//! OpenRouter chat completions 的**请求** wire 形状 + canonical → wire 的编码器。
//!
//! 与 `aa_ai_openai` 的请求侧逐字段对照（差异全部来自 OpenRouter 自己的
//! OpenAPI / 文档，见 [`docs/compare/openrouter-wire.md`](../../../../../docs/compare/openrouter-wire.md)）：
//!
//! - **思考强度**：`reasoning: { effort }`，不是 OpenAI 的顶层 `reasoning_effort`；
//! - **思考回放**：assistant 消息可带 `reasoning`（纯文本）或
//!   `reasoning_details`（结构化块）；canonical 侧没有专门的 wire 槽位，回放数据
//!   保存在 `ContentPart::Reasoning.signature`（见 D2）；
//! - **流式 usage**：OpenRouter 始终返回，`stream_options.include_usage` 已废弃，
//!   所以本 crate 不发 `stream_options`；
//! - **`service_tier`**：OpenRouter 的取值（`fast` / `priority` / `flex` …）与
//!   OpenAI 的枚举不是同一套，v0 不发；
//! - **`provider` 路由偏好**：没有 canonical 槽位，v0 不发（`:free` 后缀本身已经
//!   决定路由）。
//!
//! 其余形状（`tools` / `tool_choice` / `response_format` / `max_tokens` 二选一 /
//! 工具结果展开成独立 `role:"tool"` 消息）与 OpenAI 相同，但仍各写一份：
//! 厂商 crate 不共享 encoder（ADR 0008 D1）。

pub mod message;

use std::borrow::Cow;

use serde::Serialize;

/// chat completions 的请求体（借用 canonical IR）。
#[derive(Debug, Serialize)]
pub struct ChatCompletionRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<crate::chat_completion::request::message::ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 旧模型的输出上限。与 `max_completion_tokens` 二选一，由模型能力决定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// 新模型的输出上限键名。由 `LanguageModel::uses_max_completion_tokens` 决定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatToolChoice<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ChatResponseFormat<'a>>,
    /// OpenRouter 的思考配置（`reasoning.effort`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool<'a>>,
}

/// `reasoning` 对象。v0 只发 `effort`；`max_tokens` / `exclude` / `enabled`
/// 没有 canonical 对应字段，不凭空发明。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReasoningConfig {
    pub effort: aa_language_model::provider::ReasoningEffort,
}

/// `tools[]` 的一项：`{"type":"function","function":{...}}`。
#[derive(Debug, Serialize)]
pub struct ChatTool<'a> {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    pub function: ChatFunctionDef<'a>,
}

/// 工具的 JSON schema（全部借用 canonical `Tool`）。
#[derive(Debug, Serialize)]
pub struct ChatFunctionDef<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a serde_json::Value,
}

/// `type` 字段的取值。OpenRouter 目前也只支持 function 工具。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Function,
}

/// `tool_choice` 的 wire 形状（provider 私有）。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ChatToolChoice<'a> {
    Keyword(ChatToolChoiceKeyword),
    Function(ChatToolChoiceFunction<'a>),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatToolChoiceKeyword {
    Auto,
    None,
    Required,
}

#[derive(Debug, Serialize)]
pub struct ChatToolChoiceFunction<'a> {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    pub function: ChatToolChoiceFunctionName<'a>,
}

#[derive(Debug, Serialize)]
pub struct ChatToolChoiceFunctionName<'a> {
    pub name: &'a str,
}

/// `response_format` 的 wire 形状（provider 私有）。
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatResponseFormat<'a> {
    Text,
    JsonObject,
    JsonSchema { json_schema: ChatJsonSchema<'a> },
}

#[derive(Debug, Serialize)]
pub struct ChatJsonSchema<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
    pub schema: &'a serde_json::Value,
}

/// 把 canonical `Request` 翻成 OpenRouter chat completions 的 wire body（借用）。
///
/// 映射规则与 `aa_ai_openai` 相同，另加思考回放：
/// - `Reasoning` → assistant 的 `reasoning_details`（若 `signature` 是 OpenRouter
///   写下的结构化 JSON）或 `reasoning`（纯文本，且这条消息带工具调用）；
/// - `RedactedThinking` / `Opaque` → 跳过（OpenRouter 的加密思考在
///   `reasoning_details` 里，不走这两个 canonical 变体）；
/// - `ToolResult` → 独立的 `role:"tool"` 消息；
/// - `Image` → `image_url`；模型不支持图片就报错，不静默丢弃。
///
/// **能力位**（`LanguageModel`，ADR 0006 D4）：`uses_developer_role`、`supports_tools`、
/// `supports_images`、`supports_parallel_tool_calls`、`uses_max_completion_tokens`、
/// `supports_prompt_cache_key`。
pub fn to_chat_completion_request<'a>(
    request: &'a aa_language_model::request::Request,
    model: &'a aa_language_model::provider::LanguageModel,
) -> Result<ChatCompletionRequest<'a>, aa_language_model::provider::LlmError> {
    if !model.supports_tools && !request.tools.is_empty() {
        return Err(aa_language_model::provider::LlmError::Unsupported(format!(
            "模型 `{}` 标记为不支持工具（LanguageModel::supports_tools = false），\
             但请求带了 {} 个工具",
            model.id,
            request.tools.len()
        )));
    }

    if let Some(format) = &request.response_format
        && !model.supports_response_format(format)
    {
        return Err(aa_language_model::provider::LlmError::Unsupported(format!(
            "模型 `{}` 标记为不支持请求的结构化输出\
             （LanguageModel::response_format_support 未开启对应变体）",
            model.id
        )));
    }

    let mut messages = Vec::with_capacity(request.messages.len());

    for msg in &request.messages {
        // 工具结果在 wire 上是独立消息，且必须紧跟触发它的 assistant tool_calls。
        for part in msg.content() {
            if let aa_language_model::request::message::ContentPart::ToolResult {
                call_id,
                content,
                ..
            } = part
            {
                messages.push(
                    crate::chat_completion::request::message::ChatMessage::Tool {
                        tool_call_id: Cow::Borrowed(call_id),
                        content: tool_result_content(content, model)?,
                    },
                );
            }
        }

        let mut parts: Vec<crate::chat_completion::request::message::ChatContentPart<'a>> =
            Vec::new();
        let mut calls: Vec<crate::chat_completion::request::message::ChatToolCall<'a>> = Vec::new();
        let mut reasoning_text = String::new();
        let mut reasoning_details: Option<serde_json::Value> = None;

        for part in msg.content() {
            match part {
                aa_language_model::request::message::ContentPart::Text(t) => {
                    push_text(&mut parts, t);
                }
                aa_language_model::request::message::ContentPart::Image(image) => {
                    parts.push(image_part(image, model)?);
                }
                aa_language_model::request::message::ContentPart::ToolCall { id, name, input } => {
                    calls.push(crate::chat_completion::request::message::ChatToolCall {
                        id: Cow::Borrowed(id),
                        kind: ToolKind::Function,
                        function: crate::chat_completion::request::message::ChatFunctionCall {
                            name: Cow::Borrowed(name),
                            // OpenRouter 要求参数是 JSON 字符串：这一处必须分配。
                            arguments: Cow::Owned(input.to_wire_string()),
                        },
                    });
                }
                aa_language_model::request::message::ContentPart::Reasoning { text, signature } => {
                    reasoning_text.push_str(text);
                    if let Some(signature) = signature
                        && let Some(details) = parse_reasoning_details(signature)
                    {
                        reasoning_details = Some(details);
                    }
                }
                // OpenRouter 的加密思考在 `reasoning_details` 里，不走这两个变体。
                aa_language_model::request::message::ContentPart::RedactedThinking(_) => {}
                aa_language_model::request::message::ContentPart::Opaque(_) => {}
                // 已在上面展开成独立的 role:"tool" 消息。
                aa_language_model::request::message::ContentPart::ToolResult { .. } => {}
            }
        }

        let content =
            crate::chat_completion::request::message::ChatMessageContent::from_parts(parts);

        // 纯工具结果消息已经展开成 `role:"tool"`，不该再补一条空 user/assistant。
        if content.is_none() && calls.is_empty() {
            continue;
        }

        let content = content.unwrap_or(
            crate::chat_completion::request::message::ChatMessageContent::Plain(Cow::Borrowed("")),
        );
        messages.push(match msg.role() {
            aa_language_model::request::message::Role::System if model.uses_developer_role => {
                crate::chat_completion::request::message::ChatMessage::Developer { content }
            }
            aa_language_model::request::message::Role::System => {
                crate::chat_completion::request::message::ChatMessage::System { content }
            }
            aa_language_model::request::message::Role::User => {
                crate::chat_completion::request::message::ChatMessage::User { content }
            }
            aa_language_model::request::message::Role::Assistant => {
                // 有结构化 details 时只回放 details（它已经包含文本/签名）；
                // 否则只在带工具调用时回放纯文本思考：每轮都回放会把上下文翻倍，
                // 而 OpenRouter 文档要求的「必须保留」场景是工具调用后的续写。
                let reasoning = if reasoning_details.is_none()
                    && !calls.is_empty()
                    && !reasoning_text.is_empty()
                {
                    Some(Cow::Owned(reasoning_text))
                } else {
                    None
                };
                crate::chat_completion::request::message::ChatMessage::Assistant {
                    content: Some(content),
                    tool_calls: calls,
                    reasoning,
                    reasoning_details,
                }
            }
        });
    }

    let mut tools = Vec::with_capacity(request.tools.len());
    for tool in &request.tools {
        let aa_language_model::request::tools::ToolInput::Function { input_schema, .. } =
            &tool.input
        else {
            return Err(aa_language_model::provider::LlmError::Unsupported(
                "OpenRouter Chat Completions does not support custom tools".to_owned(),
            ));
        };
        tools.push(ChatTool {
            kind: ToolKind::Function,
            function: ChatFunctionDef {
                name: &tool.name,
                description: &tool.description,
                parameters: input_schema,
            },
        });
    }

    let max_output_tokens = request.max_output_tokens.or(model.max_output_tokens);
    Ok(ChatCompletionRequest {
        model: &model.id,
        messages,
        temperature: request.temperature,
        max_tokens: (!model.uses_max_completion_tokens)
            .then_some(max_output_tokens)
            .flatten(),
        max_completion_tokens: model
            .uses_max_completion_tokens
            .then_some(max_output_tokens)
            .flatten(),
        stream: request.stream,
        stop: (!request.stop.is_empty()).then_some(request.stop.as_slice()),
        // 没有工具时 `tool_choice` 无意义，有些上游会因此 400。
        tool_choice: (!request.tools.is_empty())
            .then(|| request.tool_choice.as_ref().map(to_wire_tool_choice))
            .flatten(),
        parallel_tool_calls: (model.supports_parallel_tool_calls && !request.tools.is_empty())
            .then_some(true),
        response_format: request
            .response_format
            .as_ref()
            .map(to_wire_response_format),
        reasoning: request
            .reasoning_effort
            .map(|effort| ReasoningConfig { effort }),
        prompt_cache_key: model
            .supports_prompt_cache_key
            .then_some(request.prompt_cache_key.as_deref())
            .flatten(),
        tools,
    })
}

/// 把借用 DTO 直接 `to_writer` 成 owned 字节（`'a` 在返回时终结）。
pub fn encode<'a>(
    request: &'a aa_language_model::request::Request,
    model: &'a aa_language_model::provider::LanguageModel,
) -> Result<Vec<u8>, aa_language_model::provider::LlmError> {
    let body = to_chat_completion_request(request, model)?;
    let mut bytes = Vec::new();
    serde_json::to_writer(&mut bytes, &body)
        .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;
    Ok(bytes)
}

/// `ContentPart::Reasoning.signature` → `reasoning_details`。
///
/// OpenRouter 的结构化思考块是 JSON 数组，元素带 `type: "reasoning.*"`。
/// 只接受这个形状；Anthropic 的签名字符串等其它 provider 的 `signature`
/// 原样忽略，避免把别家的回放数据发给 OpenRouter。
fn parse_reasoning_details(signature: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(signature).ok()?;
    let items = value.as_array()?;
    if items.is_empty() {
        return None;
    }
    let looks_like_details = items.iter().all(|item| {
        item.get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.starts_with("reasoning."))
    });
    looks_like_details.then_some(value)
}

fn to_wire_tool_choice(choice: &aa_language_model::request::ToolChoice) -> ChatToolChoice<'_> {
    use aa_language_model::request::ToolChoice as Canonical;
    match choice {
        Canonical::Auto => ChatToolChoice::Keyword(ChatToolChoiceKeyword::Auto),
        Canonical::None => ChatToolChoice::Keyword(ChatToolChoiceKeyword::None),
        Canonical::Required => ChatToolChoice::Keyword(ChatToolChoiceKeyword::Required),
        Canonical::Specific(name) => ChatToolChoice::Function(ChatToolChoiceFunction {
            kind: ToolKind::Function,
            function: ChatToolChoiceFunctionName { name },
        }),
    }
}

fn to_wire_response_format(
    format: &aa_language_model::request::ResponseFormat,
) -> ChatResponseFormat<'_> {
    use aa_language_model::request::ResponseFormat as Canonical;
    match format {
        Canonical::Text => ChatResponseFormat::Text,
        Canonical::JsonObject => ChatResponseFormat::JsonObject,
        Canonical::JsonSchema {
            name,
            schema,
            strict,
        } => ChatResponseFormat::JsonSchema {
            json_schema: ChatJsonSchema {
                name,
                strict: *strict,
                schema,
            },
        },
    }
}

/// 工具结果的一段内容：文本借用，图片转 data URL。
fn tool_result_content<'a>(
    content: &'a [aa_language_model::request::message::ToolResultPart],
    model: &aa_language_model::provider::LanguageModel,
) -> Result<
    crate::chat_completion::request::message::ChatMessageContent<'a>,
    aa_language_model::provider::LlmError,
> {
    use crate::chat_completion::request::message::ChatMessageContent;

    let mut parts = Vec::with_capacity(content.len());
    for part in content {
        match part {
            aa_language_model::request::message::ToolResultPart::Text(text) => {
                push_text(&mut parts, text);
            }
            aa_language_model::request::message::ToolResultPart::Image(image) => {
                parts.push(image_part(image, model)?);
            }
        }
    }

    // 工具消息的 `content` 是必填字段，没有内容时给空串。
    Ok(ChatMessageContent::from_parts(parts)
        .unwrap_or(ChatMessageContent::Plain(Cow::Borrowed(""))))
}

/// 追加一段文本：相邻文本合并成一个 part，避免「多段文本」变成多模态数组。
fn push_text<'a>(
    parts: &mut Vec<crate::chat_completion::request::message::ChatContentPart<'a>>,
    text: &'a str,
) {
    use crate::chat_completion::request::message::ChatContentPart;

    if let Some(ChatContentPart::Text { text: prev }) = parts.last_mut() {
        match prev {
            Cow::Owned(owned) => owned.push_str(text),
            Cow::Borrowed(existing) => {
                let existing: &'a str = existing;
                let mut owned = String::with_capacity(existing.len() + text.len());
                owned.push_str(existing);
                owned.push_str(text);
                *prev = Cow::Owned(owned);
            }
        }
        return;
    }

    parts.push(ChatContentPart::Text {
        text: Cow::Borrowed(text),
    });
}

/// 一张图片 → `image_url` part。模型不支持图片时明确报错。
fn image_part<'a>(
    image: &'a aa_language_model::request::message::Image,
    model: &aa_language_model::provider::LanguageModel,
) -> Result<
    crate::chat_completion::request::message::ChatContentPart<'a>,
    aa_language_model::provider::LlmError,
> {
    if !model.supports_images {
        return Err(aa_language_model::provider::LlmError::Unsupported(format!(
            "模型 `{}` 标记为不支持图片输入（LanguageModel::supports_images = false）",
            model.id
        )));
    }

    Ok(
        crate::chat_completion::request::message::ChatContentPart::ImageUrl {
            image_url: crate::chat_completion::request::message::ChatImageUrl {
                url: image_url(image),
            },
        },
    )
}

/// 图片 → OpenRouter 的 `url` 字段：http(s) 直接借，字节/裸 base64 转 data URL。
fn image_url(image: &aa_language_model::request::message::Image) -> Cow<'_, str> {
    use aa_language_model::request::message::ImageSource;
    use base64::Engine as _;

    match &image.source {
        ImageSource::Url(url) => Cow::Borrowed(url),
        ImageSource::Base64(data) => Cow::Owned(format!("data:{};base64,{}", image.mime, data)),
        ImageSource::Bytes(bytes) => Cow::Owned(format!(
            "data:{};base64,{}",
            image.mime,
            base64::engine::general_purpose::STANDARD.encode(bytes.as_ref())
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use aa_language_model::request::message::{ContentPart, Message, Role};
    use aa_language_model::request::{Request, ToolChoice};

    fn model(id: &str) -> aa_language_model::provider::LanguageModel {
        aa_language_model::provider::LanguageModel::new("open_router", id)
    }

    fn request(messages: Vec<Message>) -> Request {
        Request {
            messages: messages.into_iter().map(Arc::new).collect(),
            stream: true,
            ..Request::default()
        }
    }

    fn encoded(request: &Request, model: &aa_language_model::provider::LanguageModel) -> Value {
        let bytes = super::encode(request, model).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn encodes_basic_request_without_stream_options() {
        let request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        let body = encoded(&request, &model("google/gemma-4-31b-it:free"));

        assert_eq!(body["model"], "google/gemma-4-31b-it:free");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        // OpenRouter 的 usage 始终返回，`stream_options` 已废弃：不要发。
        assert!(body.get("stream_options").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_effort_uses_openrouter_reasoning_object() {
        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request.reasoning_effort = Some(aa_language_model::provider::ReasoningEffort::Low);
        let body = encoded(&request, &model("vendor/model"));

        assert_eq!(body["reasoning"]["effort"], "low");
        // 不能同时发 OpenAI 的顶层字段。
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn replays_plaintext_reasoning_for_tool_turns() {
        let request = request(vec![Message::new(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: Arc::from("let me look"),
                    signature: None,
                },
                ContentPart::tool_call("call_1", "read", serde_json::json!({"path": "a"})),
            ],
        )]);
        let body = encoded(&request, &model("vendor/model"));

        assert_eq!(body["messages"][0]["reasoning"], "let me look");
        assert!(body["messages"][0].get("reasoning_details").is_none());
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a"}"#
        );
    }

    #[test]
    fn does_not_replay_plaintext_reasoning_without_tool_calls() {
        let request = request(vec![Message::new(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: Arc::from("thinking"),
                    signature: None,
                },
                ContentPart::text("answer"),
            ],
        )]);
        let body = encoded(&request, &model("vendor/model"));

        // 每轮回放纯文本思考会把上下文翻倍；OpenRouter 只在工具续写时要求保留。
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn replays_structured_reasoning_details_from_signature() {
        let details = serde_json::json!([
            {"type": "reasoning.text", "index": 0, "text": "step", "signature": "sig"},
            {"type": "reasoning.encrypted", "index": 1, "data": "cipher"}
        ]);
        let request = request(vec![Message::new(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: Arc::from("step"),
                    signature: Some(Arc::from(details.to_string().as_str())),
                },
                ContentPart::tool_call("call_1", "read", serde_json::json!({})),
            ],
        )]);
        let body = encoded(&request, &model("vendor/model"));

        assert_eq!(body["messages"][0]["reasoning_details"], details);
        // details 已经包含文本，不再重复发 `reasoning`。
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn ignores_foreign_signatures() {
        let request = request(vec![Message::new(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: Arc::from("thinking"),
                    // Anthropic 的签名不是 OpenRouter 的 details 数组。
                    signature: Some(Arc::from("anthropic-signature")),
                },
                ContentPart::tool_call("call_1", "read", serde_json::json!({})),
            ],
        )]);
        let body = encoded(&request, &model("vendor/model"));

        assert!(body["messages"][0].get("reasoning_details").is_none());
        // 结构化的 details 没有，纯文本思考仍然可以回放。
        assert_eq!(body["messages"][0]["reasoning"], "thinking");
    }

    #[test]
    fn expands_tool_results_into_tool_messages() {
        let request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::ToolResult {
                call_id: Arc::from("call_1"),
                content: Box::new([aa_language_model::request::message::ToolResultPart::Text(
                    Arc::from("42"),
                )]),
                is_error: false,
            }],
        )]);
        let body = encoded(&request, &model("vendor/model"));

        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][0]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][0]["content"], "42");
    }

    #[test]
    fn max_tokens_key_follows_model_capability() {
        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request.max_output_tokens = Some(64);

        let body = encoded(&request, &model("vendor/model"));
        assert_eq!(body["max_tokens"], 64);
        assert!(body.get("max_completion_tokens").is_none());

        let mut modern = model("vendor/model");
        modern.uses_max_completion_tokens = true;
        let body = encoded(&request, &modern);
        assert_eq!(body["max_completion_tokens"], 64);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn tool_choice_only_when_tools_are_present() {
        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request.tool_choice = Some(ToolChoice::Required);
        let body = encoded(&request, &model("vendor/model"));
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn rejects_tools_when_model_says_it_cannot_use_them() {
        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request
            .tools
            .push(Arc::new(aa_language_model::request::tools::Tool {
                name: "read".to_owned(),
                description: "read".to_owned(),
                input: aa_language_model::request::tools::ToolInput::Function {
                    input_schema: serde_json::json!({"type": "object"}),
                    use_input_streaming: false,
                },
            }));
        let mut model = model("vendor/model");
        model.supports_tools = false;

        let err = super::encode(&request, &model).unwrap_err();
        assert!(matches!(
            err,
            aa_language_model::provider::LlmError::Unsupported(_)
        ));
    }

    #[test]
    fn rejects_response_format_when_capability_is_off() {
        use aa_language_model::request::ResponseFormat;

        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request.response_format = Some(ResponseFormat::JsonObject);

        let err = super::encode(&request, &model("vendor/model")).unwrap_err();
        assert!(matches!(
            err,
            aa_language_model::provider::LlmError::Unsupported(_)
        ));
    }

    #[test]
    fn encodes_json_schema_response_format_when_supported() {
        use aa_language_model::request::ResponseFormat;

        let mut request = request(vec![Message::new(
            Role::User,
            vec![ContentPart::text("hi")],
        )]);
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: "person".to_owned(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });
        let mut model = model("vendor/model");
        model.response_format_support = aa_language_model::provider::ResponseFormatSupport {
            json_object: true,
            json_schema: true,
            strict: true,
        };

        let body = encoded(&request, &model);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "person");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }
}
