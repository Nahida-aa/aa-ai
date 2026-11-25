//! 消息层 wire DTO：`role` 的形状、多模态 `content`、`tool_calls` 片段。
//!
//! **只属于 OpenAI chat completions**：canonical IR 里没有 `role: "tool"`，
//! 也没有 `tool_calls` 这种「参数是 JSON 字符串」的形状，都由
//! [`crate::chat_completion::request::to_chat_completion_request`] 现场翻译。
//!
//! 全部借用：文本用 [`Cow<'a, str>`]（单 part 借 `&'a str`，拼接才分配），
//! `tool_call_id` / `name` 借用，`arguments` 必须是 JSON **字符串**（OpenAI
//! 不接受对象）故用 `Cow::Owned`。顶层请求体同样借用，见
//! [`crate::chat_completion::request::ChatCompletionRequest`]。

use std::borrow::Cow;

use serde::Serialize;

/// OpenAI 用 `role` 区分消息形状（内部 tag）。
///
/// 注意 `Tool` 只存在于这里：canonical IR 没有这个角色，工具结果在 IR 里是
/// user 轮里的 `aa_language_model::request::message::ContentPart::ToolResult`。
///
/// `Developer` 是 `System` 的新名字（o 系列 / GPT-5）：同一个语义，由
/// `LanguageModel::uses_developer_role` 决定发哪个 `role`。
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
    },
    Tool {
        tool_call_id: Cow<'a, str>,
        content: ChatMessageContent<'a>,
    },
}

/// `content` 的两种 wire 形状（OpenAI 用 `untagged` 区分）。
///
/// 纯文本走 `Plain`（`"content": "hi"`），带图片时走 `Multipart`
/// （`"content": [{"type":"text",...},{"type":"image_url",...}]`）。纯文本保持
/// 字符串形状很重要：不少兼容端点只认字符串，见到数组就 400。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ChatMessageContent<'a> {
    Plain(Cow<'a, str>),
    Multipart(Vec<ChatContentPart<'a>>),
}

impl<'a> ChatMessageContent<'a> {
    /// 只有一段文本时就退回 `Plain`，多段/含图片才用数组。
    ///
    /// `None` = 这条消息没有可发的内容（例如只有工具调用、或内容全被跳过）。
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
    /// OpenAI 要求参数是 JSON 字符串，不是对象；这一处必须分配。
    pub arguments: Cow<'a, str>,
}
