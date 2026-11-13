//! 流式完成事件（provider 产出，喂给 agent loop）。
//! fork 自 Zed `LanguageModelCompletionEvent`，精简到 coding agent 需要的子集。
//! Zed 原名 `LanguageModelCompletionEvent`。

use serde::{Deserialize, Serialize};

/// 流式完成事件。agent loop 消费这个流来驱动「收文本 / 收工具调用 / 结束」。
///
/// 精简自 Zed `LanguageModelCompletionEvent`：去掉 `Queued`/`Started`/
/// `StartMessage`/`RedactedThinking`/`ReasoningDetails`/`Compaction`/
/// `ToolUseJsonParseError` 等 Zed 特有/可选变体，保留 coding agent 核心。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompletionEvent {
    /// 文本增量（LLM 流式输出）。
    Text(String),
    /// LLM 决定调一个工具（agent loop 收到后去执行，结果回填成 `Message`）。
    /// `input` 是 LLM 给的 JSON 参数（可能不全，看 `is_complete`）。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        is_complete: bool,
    },
    /// 本轮结束。
    Stop(crate::provider::model::StopReason),
    /// 用量更新（可选，provider 支持时发）。
    Usage(crate::provider::model::TokenUsage),
}
