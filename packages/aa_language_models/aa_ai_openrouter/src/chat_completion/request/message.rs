//! 消息层 wire DTO：`role` 的形状、多模态 `content`、`tool_calls`、思考回放。
//!
//! 和 `aa_ai_openai` 的 message 模块相比，只有 assistant 多两个字段：
//! - `reasoning`：纯文本思考（OpenRouter 接受 `reasoning` / `reasoning_content` 别名）；
//! - `reasoning_details`：结构化思考块（`reasoning.text` / `reasoning.encrypted` /
//!   `reasoning.summary`），模型在工具调用后要求**原样**回放。
//!
//! 其余形状（`role:"tool"`、`tool_calls[].function.arguments` 是 JSON 字符串、
//! `content` 的字符串/数组两态）与 OpenAI 相同，但仍然各写一份：厂商 crate 不共享
//! encoder（ADR 0008 D1）。
//!
//! 全部借用：文本用 [`Cow<'a, str>`]，`tool_call_id` / `name` 借用，只有
//! `arguments` 必须是 JSON 字符串（这一处必须分配）。`reasoning_details` 是已解析
//! 的 JSON，按值持有（只有带签名的 assistant 消息才会走到这里）。

use std::borrow::Cow;

use serde::Serialize;

/// OpenRouter 用 `role` 区分消息形状（内部 tag）。
///
/// `Tool` 只存在于 wire：canonical IR 没有这个角色，工具结果在 IR 里是 user 轮里的
/// `aa_language_model::request::message::ContentPart::ToolResult`。
#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage<'a> {
    System {
        content: ChatMessageContent<'a>,
    },
    Developer {
        content: ChatMessageContent<'a>,
    },
    User {
        content: ChatMessageContent<'a>,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ChatMessageContent<'a>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall<'a>>,
        /// 纯文本思考回放。只在「这条 assistant 消息带工具调用」或没有结构化
        /// `reasoning_details` 时才发（见 [`crate::chat_completion::request`]）。
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<Cow<'a, str>>,
        /// 结构化思考块回放。`ContentPart::Reasoning.signature` 里保存的 JSON 数组。
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<serde_json::Value>,
    },
    Tool {
        tool_call_id: Cow<'a, str>,
        content: ChatMessageContent<'a>,
    },
}

/// `content` 的两种 wire 形状（OpenRouter 与 OpenAI 相同，`untagged` 区分）。
///
/// 纯文本保持字符串形状：不少上游模型只认字符串，见到数组会 400。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ChatMessageContent<'a> {
    Plain(Cow<'a, str>),
    Multipart(Vec<ChatContentPart<'a>>),
}

impl<'a> ChatMessageContent<'a> {
    /// 只有一段文本时退回 `Plain`，多段/含图片才用数组。
    ///
    /// `None` = 这条消息没有可发的内容（例如只有工具调用）。
    pub fn from_parts(mut parts: Vec<ChatContentPart<'a>>) -> Option<Self> {
        match parts.len() {
            0 => None,
            1 => Some(match parts.pop().expect("len == 1") {
                ChatContentPart::Text { text } => Self::Plain(text),
                other => Self::Multipart(vec![other]),
            }),
            _ => Some(Self::Multipart(parts)),
        }
    }
}

/// 多模态 `content` 数组的一项。
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContentPart<'a> {
    Text { text: Cow<'a, str> },
    ImageUrl { image_url: ChatImageUrl<'a> },
}

/// `image_url` 对象。`url` 可以是 http(s) 地址，也可以是 data URL。
#[derive(Debug, Serialize)]
pub struct ChatImageUrl<'a> {
    pub url: Cow<'a, str>,
}

/// `assistant.tool_calls[]` 的一项。
#[derive(Debug, Serialize)]
pub struct ChatToolCall<'a> {
    pub id: Cow<'a, str>,
    #[serde(rename = "type")]
    pub kind: crate::chat_completion::request::ToolKind,
    pub function: ChatFunctionCall<'a>,
}

/// `tool_calls[].function`：名字 + JSON 字符串形式的参数。
#[derive(Debug, Serialize)]
pub struct ChatFunctionCall<'a> {
    pub name: Cow<'a, str>,
    /// OpenRouter（与 OpenAI 一样）要求参数是 JSON 字符串，不是对象。
    pub arguments: Cow<'a, str>,
}
