//! 测试支持：把 canonical 事件编成 OpenRouter wire body。
//!
//! 与 `aa_ai_openai` 的生成器刻意分开：两家的**响应**形状接近但不同
//! （`reasoning` 而非 `reasoning_content`、`reasoning_details`、`Stop` 延后），
//! 共享生成器会把协议差异又藏回一个 crate 里（见 ADR 0012）。
//!
//! 解码后的顺序与真实流一致：
//! - `reasoning_details` 在 `finish_reason` 时被还原成一条空文本 + signature 的
//!   `Reasoning` 事件；
//! - `Stop` 在 `[DONE]` 才发出，保证 `Usage` 排在它前面。

use aa_language_model::provider::{CompletionEvent, StopReason};

const MODEL: &str = "mock";

/// 流式 body（`text/event-stream`）。
pub fn sse_body(events: &[CompletionEvent]) -> String {
    let mut body = String::new();
    let mut index = 0u32;
    let mut stopped = false;

    for event in events {
        match event {
            CompletionEvent::Text(text) => {
                body.push_str(&delta_chunk(&serde_json::json!({ "content": text }), None));
            }
            CompletionEvent::Reasoning { text, signature } => {
                let mut delta = serde_json::Map::new();
                if !text.is_empty() {
                    delta.insert("reasoning".to_owned(), serde_json::json!(text));
                }
                if let Some(details) = signature
                    .as_deref()
                    .and_then(|signature| serde_json::from_str::<serde_json::Value>(signature).ok())
                {
                    delta.insert("reasoning_details".to_owned(), details);
                }
                if !delta.is_empty() {
                    body.push_str(&delta_chunk(&serde_json::Value::Object(delta), None));
                }
            }
            CompletionEvent::ToolUse {
                id, name, input, ..
            } => {
                let call = serde_json::json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    },
                });
                index += 1;
                body.push_str(&delta_chunk(
                    &serde_json::json!({ "tool_calls": [call] }),
                    None,
                ));
            }
            CompletionEvent::Usage(usage) => {
                let frame = serde_json::json!({
                    "id": "gen-mock",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                    },
                });
                body.push_str(&format!("data: {frame}\n\n"));
            }
            CompletionEvent::Stop(reason) => {
                body.push_str(&delta_chunk(
                    &serde_json::json!({}),
                    Some(finish_reason(*reason)),
                ));
                stopped = true;
            }
        }
    }

    if !stopped {
        body.push_str(&delta_chunk(&serde_json::json!({}), Some("stop")));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// 非流式 body（`stream: false` 的 JSON）。
pub fn completion_body(events: &[CompletionEvent]) -> String {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut details: Option<serde_json::Value> = None;
    let mut tool_calls = Vec::new();
    let mut usage = None;
    let mut stop = None;

    for event in events {
        match event {
            CompletionEvent::Text(fragment) => text.push_str(fragment),
            CompletionEvent::Reasoning {
                text: fragment,
                signature,
            } => {
                reasoning.push_str(fragment);
                if let Some(parsed) = signature
                    .as_deref()
                    .and_then(|signature| serde_json::from_str::<serde_json::Value>(signature).ok())
                {
                    details = Some(parsed);
                }
            }
            CompletionEvent::ToolUse {
                id, name, input, ..
            } => tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": input.to_string(),
                },
            })),
            CompletionEvent::Usage(tokens) => usage = Some(*tokens),
            CompletionEvent::Stop(reason) => stop = Some(*reason),
        }
    }

    let finish = match stop {
        Some(reason) => finish_reason(reason),
        None if tool_calls.is_empty() => "stop",
        None => "tool_calls",
    };
    let body = serde_json::json!({
        "id": "gen-mock",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": text,
                "reasoning": reasoning,
                "reasoning_details": details,
                "tool_calls": tool_calls,
            },
            "finish_reason": finish,
        }],
        "usage": usage.map(|usage| serde_json::json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
        })),
    });
    body.to_string()
}

fn delta_chunk(delta: &serde_json::Value, finish_reason: Option<&str>) -> String {
    let chunk = serde_json::json!({
        "id": "gen-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    format!("data: {chunk}\n\n")
}

fn finish_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::Refusal => "content_filter",
    }
}

#[cfg(test)]
mod tests {
    use aa_language_model::provider::TokenUsage;

    use super::*;

    fn decode(body: &str) -> Vec<CompletionEvent> {
        let mut decoder = crate::chat_completion::response::StreamDecoder::default();
        let mut events = decoder.push(body.as_bytes());
        events.extend(decoder.finish());
        events.into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn sse_body_round_trips_text_usage_and_stop() {
        let events = vec![
            CompletionEvent::Text("hello".into()),
            CompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            CompletionEvent::Stop(StopReason::EndTurn),
        ];
        assert_eq!(decode(&sse_body(&events)), events);
    }

    #[test]
    fn sse_body_round_trips_reasoning_details_as_signature() {
        let signature = serde_json::json!([
            { "type": "reasoning.text", "text": "think", "index": 0 }
        ])
        .to_string();
        let events = vec![
            CompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            CompletionEvent::Reasoning {
                text: String::new(),
                signature: Some(signature.clone()),
            },
            CompletionEvent::Stop(StopReason::EndTurn),
        ];
        // 纯文本思考在帧到达时就吐出；details 到 finish_reason 才还原成 signature。
        assert_eq!(
            decode(&sse_body(&events)),
            vec![
                CompletionEvent::Reasoning {
                    text: "think".into(),
                    signature: None,
                },
                CompletionEvent::Reasoning {
                    text: String::new(),
                    signature: Some(signature),
                },
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn completion_body_round_trips_all_parts() {
        let signature = serde_json::json!([
            { "type": "reasoning.text", "text": "think", "index": 0 }
        ])
        .to_string();
        let events = vec![
            CompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            CompletionEvent::Reasoning {
                text: String::new(),
                signature: Some(signature),
            },
            CompletionEvent::Text("hello".into()),
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a" }),
                is_complete: true,
            },
            CompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            CompletionEvent::Stop(StopReason::ToolUse),
        ];
        let parsed: crate::chat_completion::response::ChatCompletionResponse =
            serde_json::from_str(&completion_body(&events)).unwrap();
        let decoded: Vec<_> = crate::chat_completion::response::non_streaming_events(parsed)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(decoded, events);
    }
}
