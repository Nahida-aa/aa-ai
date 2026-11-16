//! chat completions 的**响应** wire 形状：非流式 body、流式 chunk、错误 body，
//! 以及把两者都翻成 canonical `CompletionEvent` 的映射层。
//!
//! 依据 [`docs/decisions/0007-response-mapping.md`](../../../../../docs/decisions/0007-response-mapping.md)：
//! - `finish_reason` 是 `StopReason` 的唯一权威来源，不再按「有没有 tool_calls」猜；
//! - `usage` 映射成 `CompletionEvent::Usage`（流式时来自带 `include_usage` 的最后一帧）；
//! - 非 2xx / 流内 `{"error":...}` 都抽 `error.message`，不再把整个 body 塞进错误；
//! - 流式用 `text/event-stream`：`data: {...}` 逐帧解析，`[DONE]` 收尾。
//!
//! 这里只有**反序列化**用的类型（`Deserialize`），请求侧在
//! [`crate::request`]，两边不共享结构体。

use std::collections::BTreeMap;

use serde::Deserialize;

/// 非流式响应 body。
///
/// 只取用得到的字段：`id`/`object`/`created` 之类的信封字段不参与语义，不解析。
#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// 未请求 usage 时可能缺失（兼容端点常见）。
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub message: ResponseMessage,
    /// `stop` / `length` / `tool_calls` / `content_filter` / `function_call`。
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ResponseToolCall>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseToolCall {
    #[serde(default)]
    pub id: String,
    pub function: ResponseFunction,
}

#[derive(Debug, Deserialize)]
pub struct ResponseFunction {
    pub name: String,
    /// JSON 文本。模型有时给空串或半截 JSON，解析失败按纯文本处理。
    #[serde(default)]
    pub arguments: String,
}

/// token 用量（OpenAI 字段名 → canonical `TokenUsage`）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

impl Usage {
    pub fn to_canonical(self) -> aa_language_model::provider::TokenUsage {
        aa_language_model::provider::TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
        }
    }
}

// ---------- 流式 chunk ----------

/// SSE 的一帧 `data: {...}`。
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// 只在带 `stream_options.include_usage` 时出现在最后一帧。
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 少数兼容端点会在流里回 `{"error":{...}}`（HTTP 仍是 200）。
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<DeltaToolCall>,
}

/// 流式 tool call 的一个分片：`id`/`name` 只在第一片出现，`arguments` 逐片累加。
#[derive(Debug, Deserialize)]
pub struct DeltaToolCall {
    /// 同一次响应里按 index 归并（并行调用会有多个 index 交错到达）。
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---------- 错误 body ----------

/// 错误信封：非 2xx 的 body 与流内 `{"error": ...}` 共用一个形状。
#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiError,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

impl ApiError {
    /// 供 `LlmError::ProviderRejection.message` 用的人读文本。
    pub fn detail(&self) -> String {
        let mut out = if self.message.is_empty() {
            "provider returned an error without a message".to_owned()
        } else {
            self.message.clone()
        };
        if let Some(code) = &self.code {
            out.push_str(&format!(" (code: {code})"));
        }
        out
    }
}

/// 从错误 body 里抽 `error.message`；不是该形状就原样返回。
pub fn error_message(raw: &str) -> String {
    serde_json::from_str::<ApiErrorBody>(raw)
        .map(|body| body.error.detail())
        .unwrap_or_else(|_| raw.to_owned())
}

// ---------- finish_reason → StopReason ----------

/// OpenAI `finish_reason` → canonical `StopReason`。
///
/// `function_call` 是 `tool_calls` 的旧名；`None` 表示兼容端点没报，由调用方回落。
pub fn stop_reason(finish_reason: Option<&str>) -> Option<aa_language_model::provider::StopReason> {
    use aa_language_model::provider::StopReason;
    Some(match finish_reason? {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        // `stop` 与未知值都按正常结束处理（宁可少报，不误报 MaxTokens/Refusal）。
        _ => StopReason::EndTurn,
    })
}

/// 非流式：`finish_reason` 优先，缺失时回落到「有没有 tool_calls」。
pub fn final_stop_reason(
    finish_reason: Option<&str>,
    has_tool_calls: bool,
) -> aa_language_model::provider::StopReason {
    use aa_language_model::provider::StopReason;
    match stop_reason(finish_reason) {
        Some(StopReason::EndTurn) if has_tool_calls => StopReason::ToolUse,
        Some(reason) => reason,
        // 兼容端点完全不报 finish_reason：有工具调用就当 ToolUse，其余当正常结束。
        None if has_tool_calls => StopReason::ToolUse,
        None => StopReason::EndTurn,
    }
}

// ---------- 流式解码 ----------

/// `text/event-stream` 的行缓冲：喂字节，吐出完整的 `data:` 负载。
///
/// 只做 SSE 分帧（`data:` 前缀、空行分隔、`\r\n`、注释行），不做 JSON 解析。
/// 缓冲里保留未完成的行，因此网络分片切在任意字节处都能拼回来。
#[derive(Debug, Default)]
struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 取下一个 `data:` 负载；没有完整的数据行时返回 `None`。
    fn next_data(&mut self) -> Option<Vec<u8>> {
        loop {
            let pos = self.buf.iter().position(|&b| b == b'\n')?;
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            if let Some(rest) = line.strip_prefix(b"data:") {
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                return Some(rest.to_vec());
            }
            // 空行、`event:`、`id:`、`retry:`、`:` 注释都不携带负载，继续下一行。
        }
    }
}

/// 流式 tool call 的累加器（按 `index` 归并，输出按 index 升序）。
#[derive(Debug, Default)]
struct ToolCalls {
    calls: BTreeMap<u32, PendingToolCall>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCalls {
    fn absorb(&mut self, delta: &DeltaToolCall) {
        let call = self.calls.entry(delta.index).or_default();
        if let Some(id) = &delta.id
            && !id.is_empty()
        {
            call.id = id.clone();
        }
        if let Some(function) = &delta.function {
            if let Some(name) = &function.name
                && !name.is_empty()
            {
                call.name = name.clone();
            }
            if let Some(arguments) = &function.arguments {
                call.arguments.push_str(arguments);
            }
        }
    }

    /// 取出并清空全部调用（`finish_reason` 到达时调用一次）。
    fn drain(&mut self) -> Vec<aa_language_model::provider::CompletionEvent> {
        use aa_language_model::provider::CompletionEvent;
        std::mem::take(&mut self.calls)
            .into_values()
            .map(|call| CompletionEvent::ToolUse {
                id: call.id,
                name: call.name,
                // 参数是增量拼起来的 JSON 文本；解析失败保留原文，不吞掉模型输出。
                input: serde_json::from_str(&call.arguments)
                    .unwrap_or(serde_json::Value::String(call.arguments)),
                is_complete: true,
            })
            .collect()
    }
}

/// 流式解码器：`bytes` → `CompletionEvent`。
///
/// 独立于 HTTP 层，便于单测（见本模块测试）：喂字节、拿事件，不需要起服务器。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    sse: SseBuffer,
    calls: ToolCalls,
    /// 已经发过 `Stop`，之后不再重复。
    stopped: bool,
    /// 收到 `[DONE]` 或流内错误，之后不再产出事件。
    done: bool,
}

impl StreamDecoder {
    /// 喂一段网络字节，返回这段时间里解出的事件。
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Vec<
        Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>,
    > {
        self.sse.push(chunk);
        let mut out = Vec::new();
        while !self.done {
            let Some(data) = self.sse.next_data() else {
                break;
            };
            self.decode_one(&data, &mut out);
        }
        out
    }

    /// 流结束（没有 `[DONE]`）：没发过 `Stop` 说明流被截断，报 `StreamEnded`。
    pub fn finish(
        mut self,
    ) -> Vec<
        Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>,
    > {
        if self.stopped || self.done {
            Vec::new()
        } else {
            self.stopped = true;
            vec![Err(aa_language_model::provider::LlmError::StreamEnded)]
        }
    }

    fn decode_one(
        &mut self,
        data: &[u8],
        out: &mut Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) {
        use aa_language_model::provider::CompletionEvent;

        if data == b"[DONE]" {
            // 正常收尾：provider 没给 finish_reason 时补一个 EndTurn，避免上层空等。
            if !self.stopped {
                self.stopped = true;
                out.push(Ok(CompletionEvent::Stop(
                    aa_language_model::provider::StopReason::EndTurn,
                )));
            }
            self.done = true;
            return;
        }

        let chunk: StreamChunk = match serde_json::from_slice(data) {
            Ok(chunk) => chunk,
            Err(err) => {
                self.done = true;
                out.push(Err(aa_language_model::provider::LlmError::Transport(
                    format!("malformed SSE chunk: {err}"),
                )));
                return;
            }
        };

        if let Some(error) = &chunk.error {
            self.done = true;
            out.push(Err(
                aa_language_model::provider::LlmError::ProviderRejection {
                    // HTTP 是 200，错误在流里；用 200 标明「不是 HTTP 层拒绝」。
                    status: 200,
                    message: error.detail(),
                    transient: false,
                },
            ));
            return;
        }

        for choice in &chunk.choices {
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                out.push(Ok(CompletionEvent::Text(content.clone())));
            }
            for call in &choice.delta.tool_calls {
                self.calls.absorb(call);
            }
            if let Some(finish_reason) = choice.finish_reason.as_deref() {
                // 先吐工具调用，再吐 Stop：agent loop 按事件顺序攒 would-be 调用。
                for event in self.calls.drain() {
                    out.push(Ok(event));
                }
                self.stopped = true;
                out.push(Ok(CompletionEvent::Stop(
                    stop_reason(Some(finish_reason))
                        .unwrap_or(aa_language_model::provider::StopReason::EndTurn),
                )));
            }
        }

        if let Some(usage) = chunk.usage {
            out.push(Ok(CompletionEvent::Usage(usage.to_canonical())));
        }
    }
}

#[cfg(test)]
mod tests {
    use aa_language_model::provider::CompletionEvent;
    use aa_language_model::provider::StopReason;

    use super::StreamDecoder;

    fn decode(
        chunks: &[&str],
    ) -> Vec<Result<CompletionEvent, aa_language_model::provider::LlmError>> {
        let mut decoder = StreamDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.push(chunk.as_bytes()));
        }
        out.extend(decoder.finish());
        out
    }

    #[test]
    fn text_deltas_become_text_events() {
        let events = decode(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                CompletionEvent::Text("Hel".into()),
                CompletionEvent::Text("lo".into()),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn tool_call_fragments_are_accumulated_by_index() {
        // 两个并行调用交错到达，`id`/`name` 只在第一片，`arguments` 逐片拼接。
        let events = decode(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{\\\"dir\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"\\\".\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                CompletionEvent::ToolUse {
                    id: "call_a".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a"}),
                    is_complete: true,
                },
                CompletionEvent::ToolUse {
                    id: "call_b".into(),
                    name: "list_dir".into(),
                    input: serde_json::json!({"dir": "."}),
                    is_complete: true,
                },
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn usage_arrives_before_done() {
        let events = decode(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Usage(
                aa_language_model::provider::TokenUsage {
                    input_tokens: 12,
                    output_tokens: 3,
                }
            ))
        );
    }

    #[test]
    fn length_finish_reason_is_max_tokens() {
        let events = decode(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"trunc\"},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::MaxTokens))
        );
    }

    #[test]
    fn frame_split_across_network_chunks_is_reassembled() {
        // 把一帧切在 JSON 中间，模拟 TCP 分片。
        let events = decode(&[
            "data: {\"choices\":[{\"delta\":{\"cont",
            "ent\":\"split\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(events[0], CompletionEvent::Text("split".into()));
    }

    #[test]
    fn crlf_comments_and_event_lines_are_ignored() {
        let events = decode(&[
            ": keep-alive\r\n",
            "event: message\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        ]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(events[0], CompletionEvent::Text("ok".into()));
    }

    #[test]
    fn stream_error_chunk_is_a_provider_rejection() {
        let events = decode(&["data: {\"error\":{\"message\":\"boom\",\"code\":\"x\"}}\n\n"]);
        match &events[0] {
            Err(aa_language_model::provider::LlmError::ProviderRejection {
                status,
                message,
                ..
            }) => {
                assert_eq!(*status, 200);
                assert_eq!(message, "boom (code: x)");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn truncated_stream_reports_stream_ended() {
        // 没有 finish_reason、也没有 [DONE]：连接被掐断。
        let events = decode(&["data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n"]);
        assert!(matches!(
            events.last(),
            Some(Err(aa_language_model::provider::LlmError::StreamEnded))
        ));
    }

    #[test]
    fn finish_reason_maps_to_canonical_stop_reason() {
        use super::stop_reason;
        assert_eq!(stop_reason(Some("stop")), Some(StopReason::EndTurn));
        assert_eq!(stop_reason(Some("length")), Some(StopReason::MaxTokens));
        assert_eq!(stop_reason(Some("tool_calls")), Some(StopReason::ToolUse));
        assert_eq!(
            stop_reason(Some("function_call")),
            Some(StopReason::ToolUse)
        );
        assert_eq!(
            stop_reason(Some("content_filter")),
            Some(StopReason::Refusal)
        );
        assert_eq!(stop_reason(None), None);
    }

    #[test]
    fn error_body_message_is_extracted() {
        use super::error_message;
        assert_eq!(
            error_message("{\"error\":{\"message\":\"bad key\",\"type\":\"auth\"}}"),
            "bad key"
        );
        // 不是 JSON 信封就原样返回（有些兼容端点直接回纯文本）。
        assert_eq!(error_message("502 Bad Gateway"), "502 Bad Gateway");
    }
}
