//! Ollama `/api/chat` 的**响应** wire 形状 + NDJSON 解码器。
//!
//! 依据 [ADR 0007](../../../../../docs/decisions/0007-response-mapping.md)：
//! - `done_reason` 是 `StopReason` 的权威来源；
//! - 用量映射成 `LanguageModelCompletionEvent::Usage`；
//! - 非 2xx / 流内 `{"error": ...}` 都抽错误文本，不把整个 body 塞进错误；
//! - 流被截断（没收到 `done: true`）报 `StreamEnded`。
//!
//! **与 OpenAI 的两处结构性差异**（见 [`crate::chat`] 的对照表）：
//!
//! 1. **分帧不是 SSE**：一行一个完整 JSON 对象（NDJSON），没有 `data:` 前缀、
//!    没有 `[DONE]`。行缓冲要能跨任意字节边界把半行拼回来。
//! 2. **`done_reason` 不区分工具调用**：Ollama 在有 `tool_calls` 时仍然回
//!    `done_reason: "stop"`（官方 API 文档的示例就是如此）。所以 `StopReason`
//!    要同时看「本帧有没有工具调用」，不能只看 `done_reason` —— 这是本协议特有
//!    的回落规则，Zed 干脆完全忽略 `done_reason`，我们保留它但加上这条回落。
//!
//! 非流式（`stream: false`）返回的是**同一个形状的单个对象**，所以它走同一个
//! 解码器：把整个 body 当一行喂进去即可。

use serde::Deserialize;

/// 一帧（流式一行 / 非流式整个 body）。
#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub done: bool,
    /// `stop` / `length` / `load` / `unload`（后两者是加载/卸载模型的空响应）。
    #[serde(default)]
    pub done_reason: Option<String>,
    /// 输入 token 数。
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    /// 输出 token 数。
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Ollama 把错误也放在 200 的流里（模型不存在、上下文超限等）。
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub content: String,
    /// 思考过程（thinking 模型）。Ollama 的官方字段，不是兼容端点的私货。
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCall {
    /// 多数模型不给 id；响应侧缺失时由解码器合成（见 [`StreamDecoder`]）。
    #[serde(default)]
    pub id: Option<String>,
    pub function: Function,
}

#[derive(Debug, Deserialize)]
pub struct Function {
    pub name: String,
    /// **对象**（不是 OpenAI 的 JSON 字符串）。万一模型给的是字符串，按文本处理。
    #[serde(default)]
    pub arguments: serde_json::Value,
}

// ---------- 错误 body ----------

/// Ollama 的错误信封：`{"error": "..."}`（HTTP 非 2xx 与流内共用这个形状）。
#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// 从错误 body 里抽 `error`；不是该形状就原样返回。
pub fn error_message(raw: &str) -> String {
    serde_json::from_str::<ErrorBody>(raw)
        .map(|body| body.error)
        .unwrap_or_else(|_| raw.to_owned())
}

// ---------- done_reason → StopReason ----------

/// `done_reason` + 「有没有工具调用」→ canonical `StopReason`。
///
/// `length` 优先（真的被输出上限截断了，即使同时有工具调用）；
/// 其余情况只要有工具调用就是 `ToolUse`（Ollama 不为此换 done_reason）；
/// `load` / `unload` 与缺失都按正常结束。
pub fn stop_reason(
    done_reason: Option<&str>,
    has_tool_calls: bool,
) -> aa_language_model::provider::StopReason {
    use aa_language_model::provider::StopReason;
    match done_reason {
        Some("length") => StopReason::MaxTokens,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    }
}

// ---------- NDJSON 分帧 ----------

/// 按行缓冲，吐出完整的 JSON 行。
///
/// 只做分帧（`\n` / `\r\n`、空白行跳过），不解析 JSON。缓冲里保留未完成的行，
/// 网络分片切在任意字节处都能拼回来。
#[derive(Debug, Default)]
struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 取下一行（去掉行尾换行）；没有完整行时返回 `None`。
    fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            let pos = self.buf.iter().position(|&b| b == b'\n')?;
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                // 空行不携带负载（Ollama 不发，但容忍）。
                continue;
            }
            return Some(line);
        }
    }

    /// 取剩下的半行（流结束时用；非流式 body 常不带结尾换行）。
    fn take_rest(&mut self) -> Option<Vec<u8>> {
        let mut line = std::mem::take(&mut self.buf);
        while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
            line.pop();
        }
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            None
        } else {
            Some(line)
        }
    }
}

/// NDJSON → `LanguageModelCompletionEvent` 的流式解码器。
///
/// 独立于 HTTP 层，便于单测：喂字节、拿事件，不需要起服务器。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    lines: LineBuffer,
    /// 合成工具调用 id 的计数器：Ollama 不给 id，但 canonical IR 的
    /// `ContentPart::ToolCall.id` 是必填，agent loop 也靠它把结果对回去。
    next_call_id: u32,
    /// 本次响应里出现过工具调用（`done_reason` 不反映这件事）。
    saw_tool_calls: bool,
    /// 已经发过 `Stop`。
    stopped: bool,
    /// 收到 `done` 或错误，之后不再产出事件。
    done: bool,
}

impl StreamDecoder {
    /// 喂一段网络字节，返回这段时间里解出的事件。
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Vec<
        Result<
            aa_language_model::provider::LanguageModelCompletionEvent,
            aa_language_model::provider::LlmError,
        >,
    > {
        self.lines.push(chunk);
        let mut out = Vec::new();
        while !self.done {
            let Some(line) = self.lines.next_line() else {
                break;
            };
            self.decode_line(&line, &mut out);
        }
        out
    }

    /// 流结束。
    ///
    /// 先处理缓冲区里的最后一行（非流式 body 通常不带结尾换行），再判断是否
    /// 收到过 `done: true`：没收到说明流被截断，报 `StreamEnded`。
    pub fn finish(
        mut self,
    ) -> Vec<
        Result<
            aa_language_model::provider::LanguageModelCompletionEvent,
            aa_language_model::provider::LlmError,
        >,
    > {
        let mut out = Vec::new();
        if !self.done
            && let Some(line) = self.lines.take_rest()
        {
            self.decode_line(&line, &mut out);
        }
        if !self.stopped && !self.done {
            self.stopped = true;
            out.push(Err(aa_language_model::provider::LlmError::StreamEnded));
        }
        out
    }

    fn decode_line(
        &mut self,
        line: &[u8],
        out: &mut Vec<
            Result<
                aa_language_model::provider::LanguageModelCompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) {
        let chunk: ChatChunk = match serde_json::from_slice(line) {
            Ok(chunk) => chunk,
            Err(err) => {
                self.done = true;
                out.push(Err(aa_language_model::provider::LlmError::Transport(
                    format!("malformed NDJSON line: {err}"),
                )));
                return;
            }
        };

        if let Some(message) = &chunk.error {
            self.done = true;
            out.push(Err(
                aa_language_model::provider::LlmError::ProviderRejection {
                    // Ollama 在 HTTP 200 的流里报错；用 200 标明「不是 HTTP 层拒绝」。
                    status: 200,
                    message: message.clone(),
                    transient: false,
                },
            ));
            return;
        }

        if let Some(message) = &chunk.message {
            // 思考先于文本：Ollama 的 thinking 也是增量文本，按到达顺序转发。
            if !message.thinking.is_empty() {
                out.push(Ok(
                    aa_language_model::provider::LanguageModelCompletionEvent::Reasoning {
                        text: message.thinking.clone(),
                        signature: None,
                    },
                ));
            }
            if !message.content.is_empty() {
                out.push(Ok(
                    aa_language_model::provider::LanguageModelCompletionEvent::Text(
                        message.content.clone(),
                    ),
                ));
            }
            for call in &message.tool_calls {
                self.saw_tool_calls = true;
                out.push(Ok(
                    aa_language_model::provider::LanguageModelCompletionEvent::ToolUse {
                        id: match &call.id {
                            Some(id) if !id.is_empty() => id.clone(),
                            // Ollama 不返回 id：合成一个稳定的（本次响应内递增）。
                            // `is_complete = true`：Ollama 一次给完整参数对象，没有增量。
                            _ => {
                                self.next_call_id += 1;
                                format!("call_{}", self.next_call_id)
                            }
                        },
                        name: call.function.name.clone(),
                        input: call.function.arguments.clone().into(),
                        is_complete: true,
                    },
                ));
            }
        }

        if chunk.done {
            if let (Some(input), Some(output)) = (chunk.prompt_eval_count, chunk.eval_count) {
                out.push(Ok(
                    aa_language_model::provider::LanguageModelCompletionEvent::Usage(
                        aa_language_model::provider::TokenUsage {
                            input_tokens: input,
                            output_tokens: output,
                        },
                    ),
                ));
            }
            self.stopped = true;
            out.push(Ok(
                aa_language_model::provider::LanguageModelCompletionEvent::Stop(stop_reason(
                    chunk.done_reason.as_deref(),
                    self.saw_tool_calls,
                )),
            ));
            self.done = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use aa_language_model::provider::LanguageModelCompletionEvent;
    use aa_language_model::provider::StopReason;

    use super::StreamDecoder;

    fn decode(
        chunks: &[&str],
    ) -> Vec<Result<LanguageModelCompletionEvent, aa_language_model::provider::LlmError>> {
        let mut decoder = StreamDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.push(chunk.as_bytes()));
        }
        out.extend(decoder.finish());
        out
    }

    #[test]
    fn text_and_thinking_deltas_become_events() {
        let events = decode(&[
            concat!(
                r#"{"message":{"role":"assistant","content":"","thinking":"Let me "},"done":false}"#,
                "\n"
            ),
            concat!(
                r#"{"message":{"role":"assistant","content":"Hel","thinking":"think."},"done":false}"#,
                "\n"
            ),
            concat!(
                r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
                "\n"
            ),
            concat!(
                r#"{"message":{"role":"assistant","content":""},"done":true,"#,
                r#""done_reason":"stop","prompt_eval_count":12,"eval_count":3}"#,
                "\n"
            ),
        ]);

        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                LanguageModelCompletionEvent::Reasoning {
                    text: "Let me ".into(),
                    signature: None
                },
                // 同一帧里既有思考又有正文时，思考在前（模型先想再答）。
                LanguageModelCompletionEvent::Reasoning {
                    text: "think.".into(),
                    signature: None
                },
                LanguageModelCompletionEvent::Text("Hel".into()),
                LanguageModelCompletionEvent::Text("lo".into()),
                LanguageModelCompletionEvent::Usage(aa_language_model::provider::TokenUsage {
                    input_tokens: 12,
                    output_tokens: 3
                }),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn lines_split_across_network_chunks_are_reassembled() {
        let json = concat!(
            r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#,
            "\n"
        );

        // 每 7 个字节切一刀：JSON 与行边界都被切开。
        let mut decoder = StreamDecoder::default();
        let mut events = Vec::new();
        for piece in json.as_bytes().chunks(7) {
            events.extend(decoder.push(piece));
        }
        events.extend(decoder.finish());

        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                LanguageModelCompletionEvent::Text("hi".into()),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn done_reason_length_is_max_tokens() {
        let events = decode(&[concat!(
            r#"{"message":{"role":"assistant","content":"x"},"done":true,"done_reason":"length"}"#,
            "\n"
        )]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            events.last(),
            Some(LanguageModelCompletionEvent::Stop(StopReason::MaxTokens))
        ));
    }

    #[test]
    fn tool_calls_force_tool_use_even_when_done_reason_is_stop() {
        // Ollama 官方示例：带工具调用时 done_reason 仍是 "stop"。
        let events = decode(&[
            concat!(
                r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"get_weather","arguments":{"city":"Tokyo"}}}]},"done":false}"#,
                "\n"
            ),
            concat!(
                r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#,
                "\n"
            ),
        ]);

        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                // Ollama 不给 id，解码器合成。
                LanguageModelCompletionEvent::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "Tokyo"}).into(),
                    is_complete: true,
                },
                LanguageModelCompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn tool_call_ids_from_the_server_are_kept() {
        let events = decode(&[concat!(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"id":"abc","function":{"name":"f","arguments":{}}}]},"done":true,"done_reason":"stop"}"#,
            "\n"
        )]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            &events[0],
            LanguageModelCompletionEvent::ToolUse { id, .. } if id == "abc"
        ));
    }

    #[test]
    fn parallel_tool_calls_get_distinct_synthesized_ids() {
        let events = decode(&[concat!(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"a","arguments":{}}},{"function":{"name":"b","arguments":{}}}]},"done":true}"#,
            "\n"
        )]);
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LanguageModelCompletionEvent::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_1", "call_2"]);
    }

    #[test]
    fn stream_without_done_reports_stream_ended() {
        let events = decode(&[concat!(
            r#"{"message":{"role":"assistant","content":"partial"},"done":false}"#,
            "\n"
        )]);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            Ok(LanguageModelCompletionEvent::Text(ref text)) if text == "partial"
        ));
        assert!(matches!(
            events[1],
            Err(aa_language_model::provider::LlmError::StreamEnded)
        ));
    }

    #[test]
    fn in_stream_error_becomes_provider_rejection() {
        let events = decode(&[r#"{"error":"model 'nope' not found"}"#, "\n"]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(aa_language_model::provider::LlmError::ProviderRejection {
                message,
                transient,
                ..
            }) => {
                assert!(message.contains("not found"), "{message}");
                assert!(!transient);
            }
            other => panic!("应该是 ProviderRejection：{other:?}"),
        }
    }

    #[test]
    fn malformed_line_is_a_transport_error() {
        let events = decode(&["not json\n"]);
        assert!(matches!(
            events[0],
            Err(aa_language_model::provider::LlmError::Transport(_))
        ));
    }

    #[test]
    fn non_streaming_body_without_trailing_newline_is_decoded_on_finish() {
        // `stream: false` 的 body 是单个对象，通常没有结尾换行。
        let mut decoder = StreamDecoder::default();
        assert!(decoder.push(br#"{"message":{"role":"assistant","content":"ok"},"done":true,"done_reason":"stop"}"#).is_empty());
        let events: Vec<_> = decoder.finish().into_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            vec![
                LanguageModelCompletionEvent::Text("ok".into()),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn blank_lines_are_ignored() {
        let events = decode(&[
            "\n\n",
            concat!(
                r#"{"message":{"role":"assistant","content":"x"},"done":true}"#,
                "\n"
            ),
        ]);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn error_body_extracts_the_message() {
        assert_eq!(
            super::error_message(r#"{"error":"model 'x' not found"}"#),
            "model 'x' not found"
        );
        // 不是该形状就原样返回（例如反代给的 HTML）。
        assert_eq!(super::error_message("<html>"), "<html>");
    }
}
