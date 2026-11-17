//! 消息层 wire DTO：`role` 的四种形状与工具调用片段。
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
#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage<'a> {
    System {
        content: Cow<'a, str>,
    },
    User {
        content: Cow<'a, str>,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall<'a>>,
    },
    Tool {
        tool_call_id: Cow<'a, str>,
        content: Cow<'a, str>,
    },
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
