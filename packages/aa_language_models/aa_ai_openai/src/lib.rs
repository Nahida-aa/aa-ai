//! OpenAI（或 OpenAI 兼容端点）provider：真连 LLM 服务器。
//!
//! 本 crate 只负责 **OpenAI 的 wire 形状**：
//! - [`request`]：请求 wire DTO（借用 canonical IR）+ 编码器
//!   （`to_chat_completion_request` 产出借用 DTO，`encode` 用 `to_writer` 写成 owned 字节）；
//! - [`response`]：响应 DTO（非流式 / SSE chunk / 错误 body）+ `finish_reason`、`usage`
//!   映射 + `StreamDecoder`；
//! - [`OpenAiLanguageModelProvider`]：HTTP 调用 + 响应 → `CompletionEvent`。
//!
//! 两种模式：`request.stream == false` 时读整个 JSON body；`true` 时读
//! `text/event-stream`，逐帧产出 `Text` / `ToolUse`（按 `index` 归并增量）/ `Usage` /
//! `Stop`。两条路径都从 `finish_reason` 得出 `StopReason`（见
//! [`docs/decisions/0007-response-mapping.md`](../../../../../docs/decisions/0007-response-mapping.md)）。
//!
//! canonical IR 和 `LanguageModelClient` trait 在 `aa_language_model`：core 不依赖 `reqwest`，
//! 加一家 provider = 新增一个 crate，不动 core。这与 Zed 的
//! `language_model_core` + `open_ai` / `anthropic` / `google_ai` 布局一致。
//!
//! 字段名/形状写错在编译期失败，而不是等 JSON 被服务端拒绝。
//!
//! 类型名对齐 Zed 的 `OpenAiLanguageModelProvider`
//! （`crates/language_models/src/provider/open_ai.rs:50`）：Zed 里同一个 struct
//! 既实现 `LanguageModelProvider`（注册）又实现 `LanguageModelClient`（发请求），
//! 我们这里只有后者，因为「注册」那一半属于 `aa_language_models`。

pub mod request;
pub mod response;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use futures::StreamExt;
use futures::stream;

/// OpenAI / OpenAI 兼容端点的 provider。
pub struct OpenAiLanguageModelProvider {
    api_key: String,
    /// 默认 `https://api.openai.com/v1`；可指向 Ollama / vLLM 等兼容端点。
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiLanguageModelProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".into(),
            http: reqwest::Client::new(),
        }
    }

    /// 指向 OpenAI 兼容端点（Ollama / vLLM / 自托管）。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

// ---------- LanguageModelClient ----------

impl aa_language_model::provider::LanguageModelClient for OpenAiLanguageModelProvider {
    fn stream_completion(
        &self,
        model: &aa_language_model::provider::Model,
        request: &aa_language_model::request::Request,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        aa_language_model::provider::EventStream,
                        aa_language_model::provider::LlmError,
                    >,
                > + Send
                + 'static,
        >,
    > {
        // 先同步把借用式 wire DTO 写成 owned 字节；`'a` 到此终结，不进 future。
        // 每请求选择（stream 等）来自 `Request`，静态能力位来自 `Model`（0006 D4 / A 方案）。
        let body = match crate::request::encode(request, model) {
            Ok(bytes) => bytes,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let wants_stream = request.stream;

        // future 需要的全部东西都先变成 owned，才能是 `'static`。
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let api_key = self.api_key.clone();

        Box::pin(async move {
            let resp = http
                .post(url)
                .bearer_auth(api_key)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let raw = resp.text().await.unwrap_or_default();
                let err = aa_language_model::provider::LlmError::ProviderRejection {
                    status,
                    // 只取 `error.message`，不要把整段 JSON / HTML 塞给上层。
                    message: crate::response::error_message(&raw),
                    transient: matches!(status, 408 | 425 | 429 | 500..=599),
                };
                return Ok(stream::once(async move { Err(err) }).boxed());
            }

            if wants_stream {
                // 立即把 response 交给流式解码器：不缓冲整个 body，边到边解。
                return Ok(sse_event_stream(resp.bytes_stream()));
            }

            let parsed: crate::response::ChatCompletionResponse = resp
                .json()
                .await
                .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;
            Ok(stream::iter(non_streaming_events(parsed)).boxed())
        })
    }
}

/// 非流式 body → 事件列表（最后一项是 `Stop`）。
fn non_streaming_events(
    parsed: crate::response::ChatCompletionResponse,
) -> Vec<Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>>
{
    use aa_language_model::provider::CompletionEvent;

    let mut events = Vec::new();
    let mut has_tool_calls = false;
    let mut finish_reason = None;

    if let Some(choice) = parsed.choices.first() {
        finish_reason = choice.finish_reason.as_deref();
        if let Some(text) = &choice.message.content
            && !text.is_empty()
        {
            events.push(Ok(CompletionEvent::Text(text.clone())));
        }
        for call in &choice.message.tool_calls {
            has_tool_calls = true;
            events.push(Ok(CompletionEvent::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: parse_arguments(&call.function.arguments),
                is_complete: true,
            }));
        }
    }

    // `usage` 放在 `Stop` 之前：上层看到 `Stop` 就可以收尾。
    if let Some(usage) = parsed.usage {
        events.push(Ok(CompletionEvent::Usage(usage.to_canonical())));
    }
    events.push(Ok(CompletionEvent::Stop(
        crate::response::final_stop_reason(finish_reason, has_tool_calls),
    )));
    events
}

/// OpenAI 把参数当字符串回（`arguments: "{\"path\":\"a\"}"`）；解析失败保留原文。
fn parse_arguments(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_owned()))
}

// ---------- SSE ----------

/// 把字节流解成 `CompletionEvent` 流（`text/event-stream`）。
///
/// 泛型 `S`/`B` 是为了不引入 `bytes` 依赖：只要能给出 `AsRef<[u8]>` 的分片即可。
/// 每个网络分片到货就解码，不把整个响应缓冲起来。
fn sse_event_stream<S, B>(inner: S) -> aa_language_model::provider::EventStream
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    struct State<S> {
        inner: Pin<Box<S>>,
        decoder: crate::response::StreamDecoder,
        pending: VecDeque<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
        /// 网络流已结束（或出错），只剩 pending 要吐。
        ended: bool,
    }

    let state = State {
        inner: Box::pin(inner),
        decoder: crate::response::StreamDecoder::default(),
        pending: VecDeque::new(),
        ended: false,
    };

    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            if state.ended {
                return None;
            }
            match state.inner.next().await {
                Some(Ok(chunk)) => {
                    state.pending.extend(state.decoder.push(chunk.as_ref()));
                }
                Some(Err(err)) => {
                    state.ended = true;
                    state
                        .pending
                        .push_back(Err(aa_language_model::provider::LlmError::Transport(
                            err.to_string(),
                        )));
                }
                None => {
                    // 流结束但没收到 `[DONE]` / `finish_reason` → 解码器报 `StreamEnded`。
                    let decoder = std::mem::take(&mut state.decoder);
                    state.pending.extend(decoder.finish());
                    state.ended = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;

    fn make_model(id: &str) -> aa_language_model::provider::Model {
        aa_language_model::provider::Model::new("openai", id)
    }

    #[test]
    fn encodes_parts_without_panicking() {
        let history = vec![
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::System,
                vec![aa_language_model::request::message::ContentPart::text(
                    "sys",
                )],
            )),
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![aa_language_model::request::message::ContentPart::text("hi")],
            )),
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::Assistant,
                vec![
                    aa_language_model::request::message::ContentPart::Reasoning {
                        text: Arc::from("think"),
                        signature: Some(Arc::from("sig")),
                    },
                    // 加密思考块没有 OpenAI 对应物，encoder 应跳过。
                    aa_language_model::request::message::ContentPart::RedactedThinking(Arc::from(
                        "cipher",
                    )),
                    aa_language_model::request::message::ContentPart::Text(Arc::from("calling")),
                    aa_language_model::request::message::ContentPart::tool_call(
                        "call_1",
                        "read_file",
                        serde_json::json!({"path": "a"}),
                    ),
                ],
            )),
            // 工具结果在 canonical IR 里是 user 轮的内容。
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![
                    aa_language_model::request::message::ContentPart::tool_result(
                        "call_1",
                        vec![aa_language_model::request::message::ToolResultPart::from(
                            "contents",
                        )],
                        false,
                    ),
                ],
            )),
        ];
        let req = aa_language_model::request::Request {
            messages: history,
            ..Default::default()
        };

        let model = make_model("gpt-test");
        let body = crate::request::to_chat_completion_request(&req, &model).unwrap();
        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["model"], "gpt-test");
        assert_eq!(json["stream"], false);
        // 未设置的 Option 字段不应出现在 body 里。
        assert!(json.get("tool_choice").is_none());
        assert!(json.get("response_format").is_none());
        assert!(json.get("max_completion_tokens").is_none());
        assert!(json.get("stream_options").is_none());
        assert!(json.get("parallel_tool_calls").is_none());

        let messages = json["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
        // 思考不回放，但文本和工具调用都在。
        assert_eq!(messages[2]["content"], "calling");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        // encoder 把它拆成独立的 role:"tool" 消息（IR 里没有 Tool 角色）。
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "contents");
    }

    #[test]
    fn rejects_custom_tools() {
        let req = aa_language_model::request::Request {
            tools: vec![Arc::new(aa_language_model::request::tools::Tool {
                name: "custom".to_owned(),
                description: String::new(),
                input: aa_language_model::request::tools::ToolInput::Custom {
                    format: Some(aa_language_model::request::tools::CustomToolFormat::Text),
                },
            })],
            ..Default::default()
        };

        let err =
            crate::request::to_chat_completion_request(&req, &make_model("gpt-test")).unwrap_err();
        assert!(matches!(
            err,
            aa_language_model::provider::LlmError::Unsupported(_)
        ));
    }

    // ---------- 响应侧 ----------

    fn parse(json: serde_json::Value) -> crate::response::ChatCompletionResponse {
        serde_json::from_value(json).unwrap()
    }

    fn unwrap_events(
        events: Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) -> Vec<aa_language_model::provider::CompletionEvent> {
        events.into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn non_streaming_reports_tool_use_and_length_stop() {
        // 被截断的一轮：finish_reason=length 必须变成 MaxTokens，而不是 EndTurn。
        let parsed = parse(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "partial",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" }
                    }]
                },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 9, "completion_tokens": 4 }
        }));

        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert!(matches!(
            events[0],
            aa_language_model::provider::CompletionEvent::Text(_)
        ));
        assert!(matches!(
            events[1],
            aa_language_model::provider::CompletionEvent::ToolUse { .. }
        ));
        assert_eq!(
            events[2],
            aa_language_model::provider::CompletionEvent::Usage(
                aa_language_model::provider::TokenUsage {
                    input_tokens: 9,
                    output_tokens: 4,
                }
            )
        );
        assert_eq!(
            events[3],
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::MaxTokens
            )
        );
    }

    #[test]
    fn non_streaming_without_finish_reason_falls_back_to_tool_calls() {
        // 有些兼容端点不报 finish_reason；有 tool_calls 就当 ToolUse。
        let parsed = parse(serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "not json" }
                    }]
                }
            }]
        }));

        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert_eq!(
            events[0],
            aa_language_model::provider::CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                // 半截/非 JSON 参数保留原文，不吞掉模型输出。
                input: serde_json::Value::String("not json".into()),
                is_complete: true,
            }
        );
        assert_eq!(
            events[1],
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::ToolUse
            )
        );
    }

    #[test]
    fn content_filter_finish_reason_is_refusal() {
        let parsed = parse(serde_json::json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "content_filter" }]
        }));
        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert_eq!(
            events.last(),
            Some(&aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::Refusal
            ))
        );
    }

    #[test]
    fn sse_stream_emits_events_without_buffering_the_body() {
        let chunks = vec![
            Ok::<_, reqwest::Error>("data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n".as_bytes().to_vec()),
            Ok("data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n".as_bytes().to_vec()),
            Ok("data: [DONE]\n\n".as_bytes().to_vec()),
        ];

        let stream = crate::sse_event_stream(futures::stream::iter(chunks));
        let events = futures::executor::block_on(stream.collect::<Vec<_>>());

        assert_eq!(
            unwrap_events(events),
            vec![
                aa_language_model::provider::CompletionEvent::Text("he".into()),
                aa_language_model::provider::CompletionEvent::Text("llo".into()),
                aa_language_model::provider::CompletionEvent::Stop(
                    aa_language_model::provider::StopReason::EndTurn
                ),
            ]
        );
    }
}
