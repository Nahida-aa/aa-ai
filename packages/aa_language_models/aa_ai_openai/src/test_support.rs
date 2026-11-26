//! 测试支持：把 canonical 事件编成 OpenAI wire body。
//!
//! 用于两处：
//! - `FakeHttpClient` 的测试：不用手写 SSE 字符串，生成器与解码器同一套语义；
//! - `aa_language_models::mock_server`（`mock-server` feature）：真 socket 的
//!   mock 端点，让 CLI / reqwest 链路可以离线跑。
//!
//! 生成的是「形状正确」的最小 body：信封字段（`id` / `object` / `created`）
//! 固定成 mock 值，解码器本来就不读它们。所有函数只依赖 `LanguageModelCompletionEvent`，
//! 不依赖请求侧结构。
//!
//! 事件顺序会被轻微归一化，与真实端点一致：
//! - 流式 tool call 在 `finish_reason` 到达时才由解码器吐出，因此生成的
//!   `ToolUse` 事件在解码后可能排在 `Usage` 之后；
//! - `Stop` 之后补 `data: [DONE]`；没有 `Stop` 时补一个 `stop` 收尾，
//!   避免解码器报 `StreamEnded`。

use aa_language_model::provider::{LanguageModelCompletionEvent, StopReason};

const MODEL: &str = "mock";

/// 流式 body（`text/event-stream`）。
pub fn sse_body(events: &[LanguageModelCompletionEvent]) -> String {
    let mut body = String::new();
    let mut index = 0u32;
    let mut stopped = false;

    for event in events {
        match event {
            LanguageModelCompletionEvent::Text(text) => {
                body.push_str(&delta_chunk(&serde_json::json!({ "content": text }), None));
            }
            LanguageModelCompletionEvent::Reasoning { text, .. } => {
                // OpenAI 官方不返回思考；兼容端点用 `reasoning_content`。
                body.push_str(&delta_chunk(
                    &serde_json::json!({ "reasoning_content": text }),
                    None,
                ));
            }
            LanguageModelCompletionEvent::ToolUse {
                id, name, input, ..
            } => {
                let call = serde_json::json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_wire_string(),
                    },
                });
                index += 1;
                body.push_str(&delta_chunk(
                    &serde_json::json!({ "tool_calls": [call] }),
                    None,
                ));
            }
            LanguageModelCompletionEvent::Usage(usage) => {
                let frame = serde_json::json!({
                    "id": "chatcmpl-mock",
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
            LanguageModelCompletionEvent::Stop(reason) => {
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
pub fn completion_body(events: &[LanguageModelCompletionEvent]) -> String {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    let mut stop = None;

    for event in events {
        match event {
            LanguageModelCompletionEvent::Text(fragment) => text.push_str(fragment),
            LanguageModelCompletionEvent::Reasoning { text: fragment, .. } => {
                reasoning.push_str(fragment)
            }
            LanguageModelCompletionEvent::ToolUse {
                id, name, input, ..
            } => tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": input.to_string(),
                },
            })),
            LanguageModelCompletionEvent::Usage(tokens) => usage = Some(*tokens),
            LanguageModelCompletionEvent::Stop(reason) => stop = Some(*reason),
        }
    }

    let finish = match stop {
        Some(reason) => finish_reason(reason),
        None if tool_calls.is_empty() => "stop",
        None => "tool_calls",
    };
    let body = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": text,
                "reasoning_content": reasoning,
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
        "id": "chatcmpl-mock",
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

    fn decode(body: &str) -> Vec<LanguageModelCompletionEvent> {
        let mut decoder = crate::chat_completion::response::StreamDecoder::default();
        let mut events = decoder.push(body.as_bytes());
        events.extend(decoder.finish());
        events.into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn sse_body_round_trips_text_reasoning_and_stop() {
        let events = vec![
            LanguageModelCompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            LanguageModelCompletionEvent::Text("hello".into()),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ];
        assert_eq!(decode(&sse_body(&events)), events);
    }

    #[test]
    fn sse_body_round_trips_tool_calls_and_usage() {
        // 真实 OpenAI 端点在 finish_reason 之后才发 usage；生成器按事件顺序发，
        // 解码器把 ToolUse 留到 finish_reason 再吐，所以期望顺序是 usage 在前。
        let events = vec![
            LanguageModelCompletionEvent::Text("calling".into()),
            LanguageModelCompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a" }).into(),
                is_complete: true,
            },
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
        ];
        assert_eq!(
            decode(&sse_body(&events)),
            vec![
                LanguageModelCompletionEvent::Text("calling".into()),
                LanguageModelCompletionEvent::Usage(TokenUsage {
                    input_tokens: 12,
                    output_tokens: 3,
                }),
                LanguageModelCompletionEvent::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({ "path": "a" }).into(),
                    is_complete: true,
                },
                LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn completion_body_round_trips_all_parts() {
        let events = vec![
            LanguageModelCompletionEvent::Reasoning {
                text: "think".into(),
                signature: None,
            },
            LanguageModelCompletionEvent::Text("hello".into()),
            LanguageModelCompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a" }).into(),
                is_complete: true,
            },
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
        ];
        let parsed: crate::chat_completion::response::ChatCompletionResponse =
            serde_json::from_str(&completion_body(&events)).unwrap();
        let decoded: Vec<_> = crate::non_streaming_events(parsed)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(decoded, events);
    }
}
