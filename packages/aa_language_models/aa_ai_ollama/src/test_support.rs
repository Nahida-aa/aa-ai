//! 测试支持：把 canonical 事件编成 Ollama `/api/chat` 的 wire body。
//!
//! Ollama 的分帧不是 SSE：流式是一行一个完整 JSON（NDJSON），非流式是**同一个
//! 形状的单个对象**。`done: true` 的那一帧同时携带 `done_reason` 与用量。
//!
//! `Refusal` 在 Ollama 没有对应字段（没有 content_filter），生成器按 `stop`
//! 输出，解码回来会变成 `EndTurn`；测试不要用 `Refusal` 做往返断言。

use aa_language_model::provider::{LanguageModelCompletionEvent, StopReason};

const MODEL: &str = "mock";

/// 流式 body（NDJSON）。
pub fn ndjson_body(events: &[LanguageModelCompletionEvent]) -> String {
    let mut body = String::new();
    let mut usage = None;
    let mut stop = None;

    for event in events {
        match event {
            LanguageModelCompletionEvent::Text(text) => {
                body.push_str(&line(serde_json::json!({
                    "model": MODEL,
                    "created_at": "1970-01-01T00:00:00Z",
                    "message": { "role": "assistant", "content": text },
                    "done": false,
                })));
            }
            LanguageModelCompletionEvent::Reasoning { text, .. } => {
                body.push_str(&line(serde_json::json!({
                    "model": MODEL,
                    "created_at": "1970-01-01T00:00:00Z",
                    "message": { "role": "assistant", "content": "", "thinking": text },
                    "done": false,
                })));
            }
            LanguageModelCompletionEvent::ToolUse {
                id, name, input, ..
            } => {
                body.push_str(&line(serde_json::json!({
                    "model": MODEL,
                    "created_at": "1970-01-01T00:00:00Z",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": id,
                            "function": {
                                "name": name,
                                "arguments": input.as_value().cloned().unwrap_or(serde_json::Value::Null)
                            },
                        }],
                    },
                    "done": false,
                })));
            }
            LanguageModelCompletionEvent::Usage(tokens) => usage = Some(*tokens),
            LanguageModelCompletionEvent::Stop(reason) => stop = Some(*reason),
        }
    }

    body.push_str(&line(done_line(usage, stop)));
    body
}

/// 非流式 body（单个 JSON 对象，没有结尾换行）。
pub fn chat_body(events: &[LanguageModelCompletionEvent]) -> String {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    let mut stop = None;

    for event in events {
        match event {
            LanguageModelCompletionEvent::Text(fragment) => text.push_str(fragment),
            LanguageModelCompletionEvent::Reasoning { text: fragment, .. } => {
                thinking.push_str(fragment)
            }
            LanguageModelCompletionEvent::ToolUse {
                id, name, input, ..
            } => tool_calls.push(serde_json::json!({
                "id": id,
                "function": {
                    "name": name,
                    "arguments": input.as_value().cloned().unwrap_or(serde_json::Value::Null)
                },
            })),
            LanguageModelCompletionEvent::Usage(tokens) => usage = Some(*tokens),
            LanguageModelCompletionEvent::Stop(reason) => stop = Some(*reason),
        }
    }

    let mut body = done_line(usage, stop);
    let object = body.as_object_mut().expect("done_line 是对象");
    object.insert(
        "message".to_owned(),
        serde_json::json!({
            "role": "assistant",
            "content": text,
            "thinking": thinking,
            "tool_calls": tool_calls,
        }),
    );
    body.to_string()
}

fn done_line(
    usage: Option<aa_language_model::provider::TokenUsage>,
    stop: Option<StopReason>,
) -> serde_json::Value {
    let mut object = serde_json::json!({
        "model": MODEL,
        "created_at": "1970-01-01T00:00:00Z",
        "message": { "role": "assistant", "content": "" },
        "done": true,
        "done_reason": done_reason(stop),
    });
    if let Some(usage) = usage {
        object["prompt_eval_count"] = serde_json::json!(usage.input_tokens);
        object["eval_count"] = serde_json::json!(usage.output_tokens);
    }
    object
}

fn done_reason(stop: Option<StopReason>) -> &'static str {
    match stop {
        Some(StopReason::MaxTokens) => "length",
        // Ollama 在带工具调用时仍回 `stop`；解码器靠「本帧有没有 tool_calls」补 ToolUse。
        _ => "stop",
    }
}

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

#[cfg(test)]
mod tests {
    use aa_language_model::provider::TokenUsage;

    use super::*;

    fn decode(body: &str) -> Vec<LanguageModelCompletionEvent> {
        let mut decoder = crate::chat::response::StreamDecoder::default();
        let mut events = decoder.push(body.as_bytes());
        events.extend(decoder.finish());
        events.into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn ndjson_body_round_trips_text_usage_and_stop() {
        let events = vec![
            LanguageModelCompletionEvent::Text("Hel".into()),
            LanguageModelCompletionEvent::Text("lo".into()),
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ];
        assert_eq!(decode(&ndjson_body(&events)), events);
    }

    #[test]
    fn ndjson_body_round_trips_tool_calls() {
        let events = vec![
            LanguageModelCompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            LanguageModelCompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({ "city": "Tokyo" }).into(),
                is_complete: true,
            },
            LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
        ];
        assert_eq!(decode(&ndjson_body(&events)), events);
    }

    #[test]
    fn chat_body_round_trips_all_parts() {
        let events = vec![
            LanguageModelCompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            LanguageModelCompletionEvent::Text("hello".into()),
            LanguageModelCompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({ "city": "Tokyo" }).into(),
                is_complete: true,
            },
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
        ];
        assert_eq!(decode(&chat_body(&events)), events);
    }
}
