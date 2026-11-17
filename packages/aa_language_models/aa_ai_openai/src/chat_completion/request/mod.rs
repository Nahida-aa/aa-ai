//! OpenAI chat completions 的**请求** wire 形状 + canonical → wire 的编码器。
//!
//! 只记 OpenAI 的说法，不试图兼容其他厂商：加一家 = 新增一个 `aa_ai_<name>` crate，
//! 各自写自己的 `chat_completion/request`（按协议分组，见 [`crate::chat_completion`]）。
//!
//! 模块划分：
//! - [`message`]：`role` 的四种形状、`tool_calls` 片段；
//! - 本文件：顶层请求体（`model` / `messages` / `tools` / 采样参数）和编码器。
//!
//! 设计依据 [`docs/decisions/0006-wire-request-shape.md`](../../../../../docs/decisions/0006-wire-request-shape.md)：
//! wire DTO 是**带 `'a` 的强类型结构体**，字段借用 canonical IR（`&'a str` /
//! `&'a serde_json::Value` / `&'a [String]`），只有必须形状变换的才分配
//! （拼接后的消息文本用 `Cow`，`arguments` 必须是 JSON 字符串）。
//! 序列化用 [`encode`] 一次 `to_writer` 写入 owned `Vec<u8>`：`'a` 只活在编码器
//! 内部，不出现在任何跨 `.await` 的类型上；产出的字节可暂存、可重试、可跨 await。
//! 字段名/形状写错在编译期失败，而不是等 JSON 被服务端拒绝。

pub mod message;

use std::borrow::Cow;

use serde::Serialize;

/// chat completions 的请求体（借用 canonical IR）。
///
/// `'a` 覆盖 encoder 调用期间的 `Request`；序列化完成后不再持有它（见 [`encode`]）。
#[derive(Debug, Serialize)]
pub struct ChatCompletionRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<crate::chat_completion::request::message::ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 旧模型的输出上限。与 `max_completion_tokens` 二选一，由模型能力决定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// 新模型（o 系列 / GPT-5+）要求的输出上限键名。由 `Model` 能力位决定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatToolChoice<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ChatResponseFormat<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<aa_language_model::provider::ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<aa_language_model::provider::ServiceTier>,
    /// prompt cache 路由键（Zed `open_ai.rs:757` 的 `prompt_cache_key`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool<'a>>,
}

/// `stream_options`（仅流式时）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
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

/// `type` 字段的取值。OpenAI 目前只有 `function`；`custom` 工具是另一套 wire
/// （`{"type":"custom","custom":{...}}`），v0 不支持。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Function,
}

/// OpenAI `tool_choice` 的 wire 形状（provider 私有）。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ChatToolChoice<'a> {
    /// `"auto"` / `"none"` / `"required"`。
    Keyword(ChatToolChoiceKeyword),
    /// `{"type":"function","function":{"name":"..."}}`。
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

/// OpenAI `response_format` 的 wire 形状（provider 私有）。
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

/// 把 canonical `Request` 翻成 OpenAI chat completions 的 wire body（借用）。
///
/// 映射规则（语义 → OpenAI 形状）：
/// - `Text` → 单 part 直接借 `&'a str`，多 part 才拼成 `Cow::Owned`；
/// - `Reasoning` / `RedactedThinking` → **跳过**（chat completions 不回放思考；
///   加密思考块只有 Anthropic 有对应槽位）；
/// - `ToolCall` → `assistant.tool_calls[]`（`id`/`name` 借用，`arguments` 分配 JSON 字符串）；
/// - `ToolResult` → 独立的 `role: "tool"` 消息（一条结果一条消息），**即使它在
///   canonical IR 里位于 user 轮**——这正是 encoder 该做的事，IR 不为此变形；
/// - `Image` / `Opaque` → v0 不进 wire。
///
/// 约定：**工具结果独占一条 user 消息**，不和用户文本混排。Zed 的
/// `AgentMessage::to_request()` 也是把一条 agent 消息拆成「assistant（文本/思考/
/// 工具调用）」+「user（只有 ToolResult）」两条请求消息。因此这里只需按 part 逐条
/// 翻译，不存在「文本与工具结果混在一条消息里」需要拆分的场景；真出现就是构造 bug。
pub fn to_chat_completion_request<'a>(
    request: &'a aa_language_model::request::Request,
    model: &'a aa_language_model::provider::Model,
) -> Result<ChatCompletionRequest<'a>, aa_language_model::provider::LlmError> {
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
                        content: tool_result_text(content),
                    },
                );
            }
        }

        // 文本：单 part 借用，多 part 才分配拼接。
        let mut text: Option<Cow<'a, str>> = None;
        let mut calls: Vec<crate::chat_completion::request::message::ChatToolCall<'a>> = Vec::new();
        for part in msg.content() {
            match part {
                aa_language_model::request::message::ContentPart::Text(t) => {
                    text = Some(match text.take() {
                        None => Cow::Borrowed(t.as_ref()),
                        Some(mut acc) => {
                            acc.to_mut().push_str(t);
                            acc
                        }
                    });
                }
                aa_language_model::request::message::ContentPart::ToolCall { id, name, input } => {
                    calls.push(crate::chat_completion::request::message::ChatToolCall {
                        id: Cow::Borrowed(id),
                        kind: ToolKind::Function,
                        function: crate::chat_completion::request::message::ChatFunctionCall {
                            name: Cow::Borrowed(name),
                            // OpenAI 要求参数是 JSON 字符串：这一处必须分配。
                            arguments: Cow::Owned(input.to_wire_string()),
                        },
                    });
                }
                aa_language_model::request::message::ContentPart::Reasoning { .. } => {}
                // OpenAI chat completions 没有加密思考块的概念：跳过（与 Zed 一致）。
                aa_language_model::request::message::ContentPart::RedactedThinking(_) => {}
                aa_language_model::request::message::ContentPart::Image(_)
                | aa_language_model::request::message::ContentPart::Opaque(_) => {}
                // 已在上面展开成独立的 role:"tool" 消息。
                aa_language_model::request::message::ContentPart::ToolResult { .. } => {}
            }
        }

        // 纯工具结果消息已经全部展开成 `role:"tool"`，不该再补一条空 user/assistant。
        if text.is_none() && calls.is_empty() {
            continue;
        }

        messages.push(match msg.role() {
            aa_language_model::request::message::Role::System => {
                crate::chat_completion::request::message::ChatMessage::System {
                    content: text.unwrap_or(Cow::Borrowed("")),
                }
            }
            aa_language_model::request::message::Role::User => {
                crate::chat_completion::request::message::ChatMessage::User {
                    content: text.unwrap_or(Cow::Borrowed("")),
                }
            }
            aa_language_model::request::message::Role::Assistant => {
                crate::chat_completion::request::message::ChatMessage::Assistant {
                    content: text,
                    tool_calls: calls,
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
                "OpenAI Chat Completions does not support custom tools".to_owned(),
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

    // 每请求的选择从 `request` 读；静态能力位从 `model` 读（0006 D4 / A 方案）。
    // 输出上限：请求可覆盖模型默认（对齐 Zed `effective_max_output_tokens`）。
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
        // `stream_options` 只在流式时带，且仅为一个 `include_usage: true`
        // （对齐 Zed `completion.rs:205-209` 的 `StreamOptions::default()`）。
        stream_options: request.stream.then_some(StreamOptions {
            include_usage: true,
        }),
        stop: (!request.stop.is_empty()).then_some(request.stop.as_slice()),
        tool_choice: request.tool_choice.as_ref().map(to_wire_tool_choice),
        parallel_tool_calls: (model.supports_parallel_tool_calls && !request.tools.is_empty())
            .then_some(true),
        response_format: request
            .response_format
            .as_ref()
            .map(to_wire_response_format),
        reasoning_effort: request.reasoning_effort,
        service_tier: request.service_tier,
        // 仅当模型支持该字段、且本次请求确实给了键时才发。
        prompt_cache_key: model
            .supports_prompt_cache_key
            .then_some(request.prompt_cache_key.as_deref())
            .flatten(),
        tools,
    })
}

/// 把借用 DTO 直接 `to_writer` 成 owned 字节。
///
/// 这是 DTO 唯一该被消费的方式：`'a` 在本函数返回时即终结，产出的 `Vec<u8>`
/// 不借用 `Request` / `Model`，可暂存、可重试、可跨 `.await`（0006 D1）。
pub fn encode<'a>(
    request: &'a aa_language_model::request::Request,
    model: &'a aa_language_model::provider::Model,
) -> Result<Vec<u8>, aa_language_model::provider::LlmError> {
    let body = to_chat_completion_request(request, model)?;
    let mut bytes = Vec::new();
    // `Serialize` 到 `Vec<u8>` 不会失败（DTO 只含 JSON 安全类型）。
    serde_json::to_writer(&mut bytes, &body)
        .map_err(|e| aa_language_model::provider::LlmError::Transport(e.to_string()))?;
    Ok(bytes)
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

/// OpenAI 的 tool 消息只收文本；图片结果 v0 丢弃（改用多模态 content 数组时再补）。
/// 单段文本直接借用，多段才拼接。
fn tool_result_text<'a>(
    content: &'a [aa_language_model::request::message::ToolResultPart],
) -> Cow<'a, str> {
    let mut out: Option<Cow<'a, str>> = None;
    for part in content {
        if let Some(text) = part.as_text() {
            out = Some(match out.take() {
                None => Cow::Borrowed(text),
                Some(mut acc) => {
                    acc.to_mut().push_str(text);
                    acc
                }
            });
        }
    }
    out.unwrap_or(Cow::Borrowed(""))
}
