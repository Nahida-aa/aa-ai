//! Ollama `/api/chat` 的**请求** wire 形状 + canonical → wire 的编码器。
//!
//! 全部借用 canonical IR（`&'a str` / `&'a [String]` / `&'a serde_json::Value`），
//! 只有必须做形状变换的地方才分配：
//! - 多段文本拼成一条 `content`（`Cow::Owned`）；
//! - 图片字节 / 裸 base64 → base64 文本；
//! - 工具结果的 `tool_call_id` → `tool_name`（Ollama 没有 id 字段）。
//!
//! 序列化走 [`encode`] 一次 `to_writer` 写成 owned 字节，`'a` 不出现在任何跨
//! `.await` 的类型上（与 `aa_ai_openai` 同一约定，见 ADR 0006 D1）。

use std::borrow::Cow;

use serde::Serialize;

/// `/api/chat` 的请求体。
#[derive(Debug, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<ChatMessage<'a>>,
    pub stream: bool,
    /// 仅当本次请求显式给了 `reasoning_effort` 时才发。
    ///
    /// Ollama 的 `think` 是「要不要思考」的开关（`true` / `false` / 思考等级）。
    /// 不写时由模型模板自己决定，所以默认不出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<Think>,
    /// 结构化输出：`"json"` 或裸 JSON Schema（不是 OpenAI 的 `response_format` 信封）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Options<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool<'a>>,
}

/// 思考开关。Ollama 接受布尔或等级字符串，两种形状在同一个字段上。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Think {
    Bool(bool),
    Level(ThinkLevel),
}

/// Ollama 的思考等级（`low` / `medium` / `high` / `max`）。
///
/// canonical `ReasoningEffort` 有 `Minimal` 没有 `Max`：`Minimal` 落到 `low`
/// （Ollama 没有更低的档），`High` 不升级成 `max`（是否值得多花 token 由应用决定，
/// 库不替它猜）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkLevel {
    Low,
    Medium,
    High,
}

/// `format` 的两种形状。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Format<'a> {
    /// `"json"`（JSON mode，不约束 schema）。
    Keyword(&'a str),
    /// 裸 JSON Schema（结构化输出）。
    Schema(&'a serde_json::Value),
}

/// `options`（Ollama 的采样 / 生成参数，扁平字段）。
///
/// canonical `Request` 里只有这三个能对上；`seed` / `top_p` / `repeat_penalty`
/// 等没有对应字段，不在此臆造。
#[derive(Debug, Default, Serialize)]
pub struct Options<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 输出 token 上限（OpenAI 的 `max_tokens` 在这里叫 `num_predict`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<&'a [String]>,
}

impl Options<'_> {
    fn is_empty(&self) -> bool {
        self.temperature.is_none() && self.num_predict.is_none() && self.stop.is_none()
    }
}

/// 消息：**Ollama 的 `role` 有四种**，比 canonical IR 多一个 `tool`。
///
/// `Tool` 由 encoder 从 user 轮里的 `ContentPart::ToolResult` 派生，不进 IR
/// （IR 的 `Role` 只有三个，理由见 `docs/decisions/0004-message-layout.md`）。
///
/// 这里 `content` 一律出现（Ollama 文档里的每条消息都有），空内容发空串。
#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage<'a> {
    System {
        content: Cow<'a, str>,
    },
    User {
        content: Cow<'a, str>,
        /// base64 文本（不带 `data:` 前缀）。
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Cow<'a, str>>>,
    },
    Assistant {
        content: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ChatToolCall<'a>>>,
    },
    Tool {
        content: Cow<'a, str>,
        /// **Ollama 用工具名而不是 call id** 关联结果与调用。
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Cow<'a, str>>>,
    },
}

/// `tool_calls[]` 的一项。
///
/// `id` 是可选：Ollama 自己**不返回** id（多数字段缺失），但接受请求里带。我们
/// 在回放历史时带上自己合成的 id（与响应侧的事件 id 一致），缺失也不影响语义。
#[derive(Debug, Serialize)]
pub struct ChatToolCall<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    pub function: ChatFunctionCall<'a>,
}

#[derive(Debug, Serialize)]
pub struct ChatFunctionCall<'a> {
    pub name: &'a str,
    /// **对象**，不是 JSON 字符串（OpenAI 是字符串）。这是本协议最容易被抄错的地方。
    pub arguments: &'a serde_json::Value,
}

/// `tools[]` 的一项：`{"type":"function","function":{...}}`。
#[derive(Debug, Serialize)]
pub struct ChatTool<'a> {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    pub function: ChatFunctionDef<'a>,
}

#[derive(Debug, Serialize)]
pub struct ChatFunctionDef<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Function,
}

/// 把 canonical `Request` 翻成 `/api/chat` 的 wire body（借用）。
///
/// 映射规则：
/// - `Text` → 拼进 `content`（多段文本合并，Ollama 的 `content` 是单个字符串）；
/// - `Reasoning` / `RedactedThinking` → **跳过**。Ollama 的 `thinking` 只用于读取；
///   模型下一轮不需要回放思考（与 OpenAI 一致，与 DeepSeek / Anthropic 不同）；
/// - `ToolCall` → `assistant.tool_calls[].function`，`arguments` **原样发对象**；
/// - `ToolResult` → 独立的 `role:"tool"` 消息，`tool_name` 从历史里的
///   `ToolCall{id,name}` 反查（Ollama 没有 `tool_call_id`）；
/// - `Image` → `images` 里的 base64 文本；`ImageSource::Url` 无法表达（Ollama 只收
///   字节）→ [`LlmError::Unsupported`]，不静默丢图；
/// - `Opaque` → 跳过。
///
/// **能力位**（`LanguageModel`，ADR 0006 D4）：
/// - `supports_tools = false` + 请求带工具 → `Unsupported`；
/// - `supports_images = false` + 请求带图片 → `Unsupported`；
/// - `response_format_support` 未开启对应变体 + 请求带 `response_format` →
///   `Unsupported`。
///
/// `tool_choice` / `response_format` 之外的每请求字段：`tool_choice` 在 Ollama
/// 没有对应物，**忽略**（`Request` 的约定：provider 无法表达时忽略，不报错）。
pub fn to_chat_request<'a>(
    request: &'a aa_language_model::request::Request,
    model: &'a aa_language_model::provider::LanguageModel,
) -> Result<ChatRequest<'a>, aa_language_model::provider::LlmError> {
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

    // call_id → tool_name：历史是同一个 `Request`，所以只在编码期间借一份扫描表。
    // 用 Vec 而不是 HashMap：一条历史里工具调用通常个位数，线性扫描比建表便宜。
    let mut call_names: Vec<(&str, &str)> = Vec::new();
    for message in &request.messages {
        for part in message.content() {
            if let aa_language_model::request::message::ContentPart::ToolCall { id, name, .. } =
                part
            {
                call_names.push((id, name));
            }
        }
    }

    let mut messages = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        // 工具结果在 wire 上是独立消息：先把它们展开（顺序与 IR 里的顺序一致），
        // 因为 Ollama 没有 id 字段，只能靠 tool_name 关联。
        let mut tool_messages = Vec::new();
        for part in message.content() {
            if let aa_language_model::request::message::ContentPart::ToolResult {
                call_id,
                content,
                ..
            } = part
            {
                tool_messages.push(tool_message(call_id, content, &call_names, model)?);
            }
        }
        messages.extend(tool_messages);

        let mut text = Cow::Borrowed("");
        let mut has_text = false;
        let mut images: Vec<Cow<'a, str>> = Vec::new();
        let mut calls: Vec<ChatToolCall<'a>> = Vec::new();

        for part in message.content() {
            match part {
                aa_language_model::request::message::ContentPart::Text(t) => {
                    push_text(&mut text, &mut has_text, t);
                }
                aa_language_model::request::message::ContentPart::Image(image) => {
                    images.push(image_base64(image, model)?);
                }
                aa_language_model::request::message::ContentPart::ToolCall { id, name, input } => {
                    calls.push(ChatToolCall {
                        id: Some(id),
                        function: ChatFunctionCall {
                            name,
                            arguments: tool_arguments(input)?,
                        },
                    });
                }
                // Ollama 的 thinking 不回放；加密思考块没有对应槽位。
                aa_language_model::request::message::ContentPart::Reasoning { .. }
                | aa_language_model::request::message::ContentPart::RedactedThinking(_) => {}
                // provider 私有负载没有通用槽位。
                aa_language_model::request::message::ContentPart::Opaque(_) => {}
                // 已经展开成独立的 role:"tool" 消息。
                aa_language_model::request::message::ContentPart::ToolResult { .. } => {}
            }
        }

        // 整条消息只有工具结果：不再补一条空的 user/assistant。
        if !has_text && images.is_empty() && calls.is_empty() {
            continue;
        }

        messages.push(match message.role() {
            aa_language_model::request::message::Role::System => {
                ChatMessage::System { content: text }
            }
            aa_language_model::request::message::Role::User => ChatMessage::User {
                content: text,
                images: (!images.is_empty()).then_some(images),
            },
            aa_language_model::request::message::Role::Assistant => ChatMessage::Assistant {
                content: text,
                tool_calls: (!calls.is_empty()).then_some(calls),
            },
        });
    }

    let mut tools = Vec::with_capacity(request.tools.len());
    for tool in &request.tools {
        let aa_language_model::request::tools::ToolInput::Function { input_schema, .. } =
            &tool.input
        else {
            return Err(aa_language_model::provider::LlmError::Unsupported(
                "Ollama 只支持 function 工具（JSON Schema），不支持 custom 工具".to_owned(),
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

    // 输出上限：请求可覆盖模型默认（与 aa_ai_openai 同一规则）。
    let num_predict = request.max_output_tokens.or(model.max_output_tokens);
    let options = Options {
        temperature: request.temperature,
        num_predict,
        stop: (!request.stop.is_empty()).then_some(request.stop.as_slice()),
    };

    Ok(ChatRequest {
        model: &model.id,
        messages,
        stream: request.stream,
        think: request.reasoning_effort.map(|effort| {
            Think::Level(match effort {
                aa_language_model::provider::ReasoningEffort::Minimal
                | aa_language_model::provider::ReasoningEffort::Low => ThinkLevel::Low,
                aa_language_model::provider::ReasoningEffort::Medium => ThinkLevel::Medium,
                aa_language_model::provider::ReasoningEffort::High => ThinkLevel::High,
            })
        }),
        format: request.response_format.as_ref().and_then(to_wire_format),
        // 全空时不发 `options`：空对象在某些版本上会被当成「重置参数」。
        options: (!options.is_empty()).then_some(options),
        tools,
    })
}

/// 把借用 DTO 直接 `to_writer` 成 owned 字节。
///
/// `'a` 在返回时终结：产出的 `Vec<u8>` 不借用 `Request` / `LanguageModel`，可暂存、可重试、
/// 可跨 `.await`（ADR 0006 D1）。
pub fn encode(
    request: &aa_language_model::request::Request,
    model: &aa_language_model::provider::LanguageModel,
) -> Result<Vec<u8>, aa_language_model::provider::LlmError> {
    let body = to_chat_request(request, model)?;
    let mut bytes = Vec::new();
    serde_json::to_writer(&mut bytes, &body)
        .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;
    Ok(bytes)
}

/// 工具结果 → 独立的 `role:"tool"` 消息。
fn tool_message<'a>(
    call_id: &'a str,
    content: &'a [aa_language_model::request::message::ToolResultPart],
    call_names: &[(&'a str, &'a str)],
    model: &aa_language_model::provider::LanguageModel,
) -> Result<ChatMessage<'a>, aa_language_model::provider::LlmError> {
    let mut text = Cow::Borrowed("");
    let mut has_text = false;
    let mut images: Vec<Cow<'a, str>> = Vec::new();

    for part in content {
        match part {
            aa_language_model::request::message::ToolResultPart::Text(t) => {
                push_text(&mut text, &mut has_text, t);
            }
            aa_language_model::request::message::ToolResultPart::Image(image) => {
                images.push(image_base64(image, model)?);
            }
        }
    }

    Ok(ChatMessage::Tool {
        content: text,
        // 找不到对应调用时留空：字段是可选的，缺了比编一个名字好。
        tool_name: call_names
            .iter()
            .find(|(id, _)| *id == call_id)
            .map(|(_, name)| *name),
        images: (!images.is_empty()).then_some(images),
    })
}

/// 追加一段文本；多段文本合并成一条 `content`。
fn push_text<'a>(text: &mut Cow<'a, str>, has_text: &mut bool, part: &'a str) {
    *has_text = true;
    match text {
        Cow::Borrowed("") => *text = Cow::Borrowed(part),
        Cow::Borrowed(existing) => {
            let mut owned = String::with_capacity(existing.len() + part.len());
            owned.push_str(existing);
            owned.push_str(part);
            *text = Cow::Owned(owned);
        }
        Cow::Owned(owned) => owned.push_str(part),
    }
}

/// 工具入参：Ollama 要对象。非 JSON 的入参（模型给半截文本）无法表达 → 报错。
fn tool_arguments(
    input: &aa_language_model::request::message::ToolInput,
) -> Result<&serde_json::Value, aa_language_model::provider::LlmError> {
    match input {
        aa_language_model::request::message::ToolInput::Json(value) => Ok(value),
        aa_language_model::request::message::ToolInput::Text(text) => {
            Err(aa_language_model::provider::LlmError::Unsupported(format!(
                "Ollama 的工具参数必须是 JSON 对象，这条历史里是纯文本：{text:?}"
            )))
        }
    }
}

/// `ResponseFormat` → `format`。`Text` 不发（省略字段 = 普通文本）。
fn to_wire_format<'a>(
    format: &'a aa_language_model::request::ResponseFormat,
) -> Option<Format<'a>> {
    use aa_language_model::request::ResponseFormat as Canonical;
    Some(match format {
        Canonical::Text => return None,
        Canonical::JsonObject => Format::Keyword("json"),
        Canonical::JsonSchema { schema, .. } => Format::Schema(schema),
    })
}

/// 图片 → base64 文本（Ollama 的 `images` 是裸 base64，不带 `data:` 前缀）。
fn image_base64<'a>(
    image: &'a aa_language_model::request::message::Image,
    model: &aa_language_model::provider::LanguageModel,
) -> Result<Cow<'a, str>, aa_language_model::provider::LlmError> {
    use aa_language_model::request::message::ImageSource;
    use base64::Engine as _;

    if !model.supports_images {
        return Err(aa_language_model::provider::LlmError::Unsupported(format!(
            "模型 `{}` 标记为不支持图片输入（LanguageModel::supports_images = false）",
            model.id
        )));
    }

    match &image.source {
        // Ollama 只接受图片字节，不接受 URL：要 URL 支持得由应用先下载。
        ImageSource::Url(url) => Err(aa_language_model::provider::LlmError::Unsupported(format!(
            "Ollama 的 images 只接受 base64 字节，不接受 URL：{url}"
        ))),
        ImageSource::Base64(data) => Ok(Cow::Borrowed(data)),
        ImageSource::Bytes(bytes) => Ok(Cow::Owned(
            base64::engine::general_purpose::STANDARD.encode(bytes.as_ref()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aa_language_model::request::Request;
    use aa_language_model::request::message::{ContentPart, Message, Role, ToolResultPart};

    use super::to_chat_request;

    fn model(id: &str) -> aa_language_model::provider::LanguageModel {
        let mut model = aa_language_model::provider::LanguageModel::new("ollama", id);
        model.supports_tools = true;
        model
    }

    /// 开启了全部结构化输出能力位的模型。
    fn structured_model(id: &str) -> aa_language_model::provider::LanguageModel {
        let mut model = model(id);
        model.response_format_support = aa_language_model::provider::ResponseFormatSupport {
            json_object: true,
            json_schema: true,
            strict: true,
        };
        model
    }

    fn json(
        request: &Request,
        model: &aa_language_model::provider::LanguageModel,
    ) -> serde_json::Value {
        serde_json::to_value(to_chat_request(request, model).unwrap()).unwrap()
    }

    #[test]
    fn encodes_system_user_and_assistant_text() {
        let request = Request {
            messages: vec![
                Arc::new(Message::new(Role::System, vec![ContentPart::text("sys")])),
                Arc::new(Message::new(Role::User, vec![ContentPart::text("hi")])),
                Arc::new(Message::new(Role::Assistant, vec![ContentPart::text("yo")])),
            ],
            ..Default::default()
        };

        let body = json(&request, &model("llama3.2"));
        assert_eq!(body["model"], "llama3.2");
        // 默认不写 stream / think / options / tools。
        assert_eq!(body["stream"], serde_json::Value::Bool(false));
        assert!(body.get("think").is_none());
        assert!(body.get("options").is_none());
        assert!(body.get("tools").is_none());

        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role":"system","content":"sys"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"yo"},
            ])
        );
    }

    #[test]
    fn tool_calls_send_arguments_as_object_and_results_use_tool_name() {
        let request = Request {
            messages: vec![
                Arc::new(Message::new(
                    Role::User,
                    vec![ContentPart::text("weather?")],
                )),
                Arc::new(Message::new(
                    Role::Assistant,
                    vec![ContentPart::tool_call(
                        "call_1",
                        "get_weather",
                        serde_json::json!({"city": "Tokyo"}),
                    )],
                )),
                // 工具结果在 canonical IR 里是 user 轮的内容。
                Arc::new(Message::new(
                    Role::User,
                    vec![ContentPart::tool_result(
                        "call_1",
                        vec![ToolResultPart::from("11°C")],
                        false,
                    )],
                )),
            ],
            ..Default::default()
        };

        let body = json(&request, &model("llama3.2"));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        // arguments 是对象，不是 JSON 字符串。
        assert_eq!(
            messages[1]["tool_calls"],
            serde_json::json!([{
                "id": "call_1",
                "function": {"name": "get_weather", "arguments": {"city": "Tokyo"}}
            }])
        );

        // 结果消息用 tool_name 关联（Ollama 没有 tool_call_id）。
        assert_eq!(
            messages[2],
            serde_json::json!({"role": "tool", "content": "11°C", "tool_name": "get_weather"})
        );
    }

    #[test]
    fn tool_result_without_matching_call_omits_tool_name() {
        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::tool_result(
                    "unknown",
                    vec![ToolResultPart::from("x")],
                    false,
                )],
            ))],
            ..Default::default()
        };

        let body = json(&request, &model("m"));
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "tool", "content": "x"}])
        );
    }

    #[test]
    fn multiple_tool_results_become_multiple_tool_messages() {
        let request = Request {
            messages: vec![
                Arc::new(Message::new(
                    Role::Assistant,
                    vec![
                        ContentPart::tool_call("a", "read", serde_json::json!({})),
                        ContentPart::tool_call("b", "glob", serde_json::json!({})),
                    ],
                )),
                Arc::new(Message::new(
                    Role::User,
                    vec![
                        ContentPart::tool_result("a", vec![ToolResultPart::from("1")], false),
                        ContentPart::tool_result("b", vec![ToolResultPart::from("2")], false),
                    ],
                )),
            ],
            ..Default::default()
        };

        let body = json(&request, &model("m"));
        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role":"assistant","content":"","tool_calls":[
                    {"id":"a","function":{"name":"read","arguments":{}}},
                    {"id":"b","function":{"name":"glob","arguments":{}}}
                ]},
                {"role":"tool","content":"1","tool_name":"read"},
                {"role":"tool","content":"2","tool_name":"glob"},
            ])
        );
    }

    #[test]
    fn reasoning_and_redacted_thinking_are_not_replayed() {
        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::Assistant,
                vec![
                    ContentPart::Reasoning {
                        text: Arc::from("think"),
                        signature: Some(Arc::from("sig")),
                    },
                    ContentPart::RedactedThinking(Arc::from("cipher")),
                    ContentPart::Text(Arc::from("done")),
                ],
            ))],
            ..Default::default()
        };

        let body = json(&request, &model("m"));
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "assistant", "content": "done"}])
        );
    }

    #[test]
    fn options_carry_temperature_num_predict_and_stop() {
        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::text("hi")],
            ))],
            temperature: Some(0.25),
            stop: vec!["END".to_owned()],
            max_output_tokens: Some(128),
            ..Default::default()
        };

        let body = json(&request, &model("m"));
        assert_eq!(
            body["options"],
            serde_json::json!({"temperature": 0.25, "num_predict": 128, "stop": ["END"]})
        );
    }

    #[test]
    fn model_max_output_tokens_is_the_fallback() {
        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::text("hi")],
            ))],
            ..Default::default()
        };
        let mut model = model("m");
        model.max_output_tokens = Some(64);
        assert_eq!(json(&request, &model)["options"]["num_predict"], 64);

        // 请求覆盖模型默认。
        let request = Request {
            max_output_tokens: Some(32),
            ..request
        };
        assert_eq!(json(&request, &model)["options"]["num_predict"], 32);
    }

    #[test]
    fn empty_options_are_omitted() {
        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::text("hi")],
            ))],
            ..Default::default()
        };
        assert!(json(&request, &model("m")).get("options").is_none());
    }

    #[test]
    fn response_format_maps_to_format() {
        use aa_language_model::request::ResponseFormat;

        let request = Request {
            response_format: Some(ResponseFormat::JsonObject),
            ..Default::default()
        };
        assert_eq!(json(&request, &structured_model("m"))["format"], "json");

        let request = Request {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "person".to_owned(),
                schema: serde_json::json!({"type": "object"}),
                strict: true,
            }),
            ..Default::default()
        };
        // 裸 schema，不带 OpenAI 的 `json_schema` 信封。
        assert_eq!(
            json(&request, &structured_model("m"))["format"],
            serde_json::json!({"type": "object"})
        );

        let request = Request {
            response_format: Some(ResponseFormat::Text),
            ..Default::default()
        };
        assert!(json(&request, &model("m")).get("format").is_none());
    }

    #[test]
    fn unsupported_response_format_is_rejected() {
        use aa_language_model::request::ResponseFormat;

        let request = Request {
            response_format: Some(ResponseFormat::JsonObject),
            ..Default::default()
        };
        let error = to_chat_request(&request, &model("m")).unwrap_err();
        assert!(
            matches!(error, aa_language_model::provider::LlmError::Unsupported(_)),
            "{error}"
        );
    }

    #[test]
    fn reasoning_effort_maps_to_think_level() {
        use aa_language_model::provider::ReasoningEffort;

        for (effort, expected) in [
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
        ] {
            let request = Request {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            assert_eq!(json(&request, &model("m"))["think"], expected);
        }
    }

    #[test]
    fn tools_are_sent_as_function_definitions() {
        let request = Request {
            tools: vec![Arc::new(
                aa_language_model::request::tools::Tool::new_function(
                    "read".to_owned(),
                    "read a file".to_owned(),
                    serde_json::json!({"type": "object"}),
                    false,
                ),
            )],
            ..Default::default()
        };

        let body = json(&request, &model("m"));
        assert_eq!(
            body["tools"],
            serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "read a file",
                    "parameters": {"type": "object"}
                }
            }])
        );
    }

    #[test]
    fn unsupported_tools_are_an_error_before_the_request_goes_out() {
        let request = Request {
            tools: vec![Arc::new(
                aa_language_model::request::tools::Tool::new_function(
                    "read".to_owned(),
                    "r".to_owned(),
                    serde_json::json!({}),
                    false,
                ),
            )],
            ..Default::default()
        };
        let mut model = model("m");
        model.supports_tools = false;
        let error = to_chat_request(&request, &model).unwrap_err();
        assert!(
            matches!(error, aa_language_model::provider::LlmError::Unsupported(_)),
            "{error}"
        );
    }

    #[test]
    fn custom_tools_are_rejected() {
        let request = Request {
            tools: vec![Arc::new(aa_language_model::request::tools::Tool {
                name: "t".to_owned(),
                description: "d".to_owned(),
                input: aa_language_model::request::tools::ToolInput::Custom { format: None },
            })],
            ..Default::default()
        };
        let error = to_chat_request(&request, &model("m")).unwrap_err();
        assert!(error.to_string().contains("custom"), "{error}");
    }

    #[test]
    fn images_are_base64_without_data_url_prefix() {
        use aa_language_model::request::message::Image;

        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![
                    ContentPart::text("what is this?"),
                    ContentPart::Image(Arc::new(Image {
                        mime: Arc::from("image/png"),
                        source: aa_language_model::request::message::ImageSource::Base64(
                            Arc::from("AAAA"),
                        ),
                    })),
                ],
            ))],
            ..Default::default()
        };

        let mut model = model("llava");
        model.supports_images = true;
        let body = json(&request, &model);
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "what is this?", "images": ["AAAA"]}])
        );
    }

    #[test]
    fn image_url_is_rejected_because_ollama_takes_bytes_only() {
        use aa_language_model::request::message::Image;

        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::Image(Arc::new(Image {
                    mime: Arc::from("image/png"),
                    source: aa_language_model::request::message::ImageSource::Url(Arc::from(
                        "https://example.com/a.png",
                    )),
                }))],
            ))],
            ..Default::default()
        };

        let mut model = model("llava");
        model.supports_images = true;
        let error = to_chat_request(&request, &model).unwrap_err();
        assert!(error.to_string().contains("URL"), "{error}");
    }

    #[test]
    fn images_require_the_model_capability() {
        use aa_language_model::request::message::Image;

        let request = Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::Image(Arc::new(Image {
                    mime: Arc::from("image/png"),
                    source: aa_language_model::request::message::ImageSource::Base64(Arc::from(
                        "AAAA",
                    )),
                }))],
            ))],
            ..Default::default()
        };

        // 默认 supports_images = false。
        let error = to_chat_request(&request, &model("llama3.2")).unwrap_err();
        assert!(error.to_string().contains("图片"), "{error}");
    }
}
