//! OpenAI（或 OpenAI 兼容端点）provider：真连 LLM 服务器。
//! v0 最小实现：chat completions（非流式取整个 response，再映射成事件流）。
//!
//! 本 crate 只负责 **OpenAI 的 wire 形状**：
//! - [`request`]：请求 wire DTO（借用 canonical IR）+ 编码器
//!   （`to_chat_completion_request` 产出借用 DTO，`encode` 用 `to_writer` 写成 owned 字节）；
//! - 响应 DTO（本文件，私有）与事件映射；
//! - [`OpenAiLanguageModelProvider`]：HTTP 调用 + 响应 → `CompletionEvent`。
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

// ---------- 响应类型 ----------

// OpenAI 响应里一次 assistant 消息（非流式先取整个 response，再映射成事件流）。
#[derive(serde::Deserialize)]
struct OpenAiResponse {
    choices: Vec<Choice>,
}

#[derive(serde::Deserialize)]
struct Choice {
    message: MessageDelta,
}

#[derive(serde::Deserialize)]
struct MessageDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCall>,
}

#[derive(serde::Deserialize)]
struct OpenAiToolCall {
    id: String,
    function: OpenAiFunction,
}

#[derive(serde::Deserialize)]
struct OpenAiFunction {
    name: String,
    /// OpenAI 把参数当字符串（JSON 文本）回。
    arguments: String,
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
                let msg = resp.text().await.unwrap_or_default();
                let transient = matches!(status, 408 | 425 | 429 | 500..=599);
                let err = aa_language_model::provider::LlmError::ProviderRejection {
                    status,
                    message: msg,
                    transient,
                };
                return Ok(stream::once(async move { Err(err) }).boxed());
            }

            let parsed: OpenAiResponse = resp
                .json()
                .await
                .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;

            let mut events: Vec<aa_language_model::provider::CompletionEvent> = Vec::new();
            let mut wants_tools = false;
            if let Some(choice) = parsed.choices.first() {
                if let Some(text) = &choice.message.content
                    && !text.is_empty()
                {
                    events.push(aa_language_model::provider::CompletionEvent::Text(
                        text.clone(),
                    ));
                }
                for tc in &choice.message.tool_calls {
                    let input = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::String(tc.function.arguments.clone()));
                    events.push(aa_language_model::provider::CompletionEvent::ToolUse {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        input,
                        is_complete: true,
                    });
                }
                wants_tools = !choice.message.tool_calls.is_empty();
            }
            // 有工具调用时必须报 `ToolUse`：agent loop 靠它判断「还要再来一轮」。
            let stop = if wants_tools {
                aa_language_model::provider::StopReason::ToolUse
            } else {
                aa_language_model::provider::StopReason::EndTurn
            };
            events.push(aa_language_model::provider::CompletionEvent::Stop(stop));

            Ok(stream::iter(events.into_iter().map(Ok)).boxed())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
    fn maps_tool_choice_and_response_format() {
        use aa_language_model::request::{ResponseFormat, ToolChoice};

        let schema = serde_json::json!({"type": "object", "properties": {"x": {"type": "integer"}}});

        // keyword 型 tool_choice + json_schema 型 response_format
        let req = aa_language_model::request::Request {
            tool_choice: Some(ToolChoice::Required),
            response_format: Some(ResponseFormat::JsonSchema {
                name: "answer".to_owned(),
                schema: schema.clone(),
                strict: true,
            }),
            ..Default::default()
        };
        let model = make_model("gpt-5");
        let json = serde_json::to_value(
            crate::request::to_chat_completion_request(&req, &model).unwrap(),
        )
        .unwrap();
        assert_eq!(json["tool_choice"], "required");
        assert_eq!(json["response_format"]["type"], "json_schema");
        assert_eq!(json["response_format"]["json_schema"]["name"], "answer");
        assert_eq!(json["response_format"]["json_schema"]["strict"], true);
        assert_eq!(json["response_format"]["json_schema"]["schema"], schema);

        // 指定工具名 → 嵌套对象形状
        let req = aa_language_model::request::Request {
            tool_choice: Some(ToolChoice::Specific("read_file".to_owned())),
            response_format: Some(ResponseFormat::JsonObject),
            ..Default::default()
        };
        let model = make_model("gpt-4o");
        let json = serde_json::to_value(
            crate::request::to_chat_completion_request(&req, &model).unwrap(),
        )
        .unwrap();
        assert_eq!(json["tool_choice"]["type"], "function");
        assert_eq!(json["tool_choice"]["function"]["name"], "read_file");
        assert_eq!(json["response_format"]["type"], "json_object");

        // Auto / None / Text 三种简单形状
        for (choice, expected) in [(ToolChoice::Auto, "auto"), (ToolChoice::None, "none")] {
            let req = aa_language_model::request::Request {
                tool_choice: Some(choice),
                response_format: Some(ResponseFormat::Text),
                ..Default::default()
            };
            let model = make_model("gpt-4o");
            let json = serde_json::to_value(
                crate::request::to_chat_completion_request(&req, &model).unwrap(),
            )
            .unwrap();
            assert_eq!(json["tool_choice"], expected);
            assert_eq!(json["response_format"]["type"], "text");
        }
    }

    #[test]
    fn request_choices_and_model_capabilities_drive_wire_fields() {
        use aa_language_model::provider::{ReasoningEffort, ServiceTier};

        // 每请求选择放在 `Request`（对齐 Zed `LanguageModelRequest`）。
        let req = aa_language_model::request::Request {
            max_output_tokens: Some(128),
            stream: true,
            reasoning_effort: Some(ReasoningEffort::High),
            service_tier: Some(ServiceTier::Flex),
            prompt_cache_key: Some("thread-42".to_owned()),
            ..Default::default()
        };
        // 静态能力位放在 `Model`（对齐 Zed `LanguageModel`）。
        let model = aa_language_model::provider::Model {
            uses_max_completion_tokens: true,
            supports_parallel_tool_calls: true,
            supports_prompt_cache_key: true,
            ..make_model("gpt-5")
        };
        // 有 tools 时 parallel_tool_calls 才置位。
        let req_with_tool = aa_language_model::request::Request {
            tools: vec![Arc::new(
                aa_language_model::request::tools::Tool::new_function(
                    "f".to_owned(),
                    String::new(),
                    serde_json::json!({"type":"object"}),
                    false,
                ),
            )],
            ..req
        };
        let json = serde_json::to_value(
            crate::request::to_chat_completion_request(&req_with_tool, &model).unwrap(),
        )
        .unwrap();
        assert_eq!(json["model"], "gpt-5");
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["max_completion_tokens"], 128);
        assert_eq!(json["parallel_tool_calls"], true);
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["service_tier"], "flex");
        assert_eq!(json["prompt_cache_key"], "thread-42");
        // 按用户选 B：v0 wire 不纳入 Zed 没有的采样/诊断字段。
        assert!(json.get("top_p").is_none());
        assert!(json.get("seed").is_none());
    }

    #[test]
    fn prompt_cache_key_requires_model_support() {
        // 请求给了键，但模型不支持该字段 → 不发（能力位门控）。
        let req = aa_language_model::request::Request {
            prompt_cache_key: Some("thread-42".to_owned()),
            ..Default::default()
        };
        let model = make_model("gpt-4o"); // supports_prompt_cache_key = false
        let json = serde_json::to_value(
            crate::request::to_chat_completion_request(&req, &model).unwrap(),
        )
        .unwrap();
        assert!(json.get("prompt_cache_key").is_none());
    }

    #[test]
    fn encode_produces_owned_bytes_outliving_request() {
        // 0006 D1：借用 DTO 的 'a 终结在 encode 内部，产出的 Vec<u8> 不借 Request。
        let bytes = {
            let req = aa_language_model::request::Request {
                temperature: Some(0.2),
                stop: vec!["STOP".to_owned()],
                max_output_tokens: Some(128),
                ..Default::default()
            };
            let model = make_model("gpt-4o");
            crate::request::encode(&req, &model).unwrap()
        };
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert!((json["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(json["stop"][0], "STOP");
        assert_eq!(json["max_tokens"], 128);
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn single_text_part_is_borrowed_not_allocated() {
        // 单个 Text part 应走 Cow::Borrowed：DTO 里持有 &str，与 IR 的 Arc<str> 同地址。
        let text: Arc<str> = Arc::from("a fairly long user message");
        let msg = Arc::new(aa_language_model::request::message::Message::new(
            aa_language_model::request::message::Role::User,
            vec![aa_language_model::request::message::ContentPart::Text(
                text.clone(),
            )],
        ));
        let req = aa_language_model::request::Request {
            messages: vec![msg],
            ..Default::default()
        };
        let model = make_model("gpt-4o");
        let body = crate::request::to_chat_completion_request(&req, &model).unwrap();
        let borrowed = match &body.messages[0] {
            crate::request::message::ChatMessage::User { content } => content.as_ref(),
            _ => panic!("expected user message"),
        };
        assert!(std::ptr::eq(borrowed.as_ptr(), text.as_ptr()));
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

        let model = make_model("gpt-test");
        let err = crate::request::to_chat_completion_request(&req, &model).unwrap_err();
        assert!(matches!(
            err,
            aa_language_model::provider::LlmError::Unsupported(_)
        ));
    }
}
