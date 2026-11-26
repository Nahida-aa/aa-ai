//! 流式完成事件（provider 产出，喂给 agent loop）。
//! fork 自 Zed `LanguageModelLanguageModelCompletionEvent`，精简到 coding agent 需要的子集。
//! Zed 原名 `LanguageModelLanguageModelCompletionEvent`。

use serde::{Deserialize, Serialize};

/// 流式完成事件。agent loop 消费这个流来驱动「收文本 / 收思考 / 收工具调用 / 结束」。
///
/// 精简自 Zed `LanguageModelLanguageModelCompletionEvent`：去掉 `Queued`/`Started`/
/// `StartMessage`/`RedactedThinking`/`ReasoningDetails`/`Compaction`/
/// `ToolUseJsonParseError` 等 Zed 特有/可选变体，保留 coding agent 核心。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LanguageModelCompletionEvent {
    /// 文本增量（LLM 流式输出）。
    Text(String),
    /// 思考增量。
    ///
    /// 不是所有 provider 都发：OpenAI chat completions 本身没有思考，但
    /// DeepSeek / OpenRouter / 多数 vLLM 部署在 `delta.reasoning_content` 里发，
    /// Anthropic 是 `thinking_delta`，Ollama 的思考模型先吐 `<think>` 块。
    /// provider 各自负责把自家字段翻成这个事件；不想暴露思考的可以丢掉。
    ///
    /// `signature` 只在 provider 要求回放签名时才有值（Anthropic），
    /// 其余 provider 都是 `None`。文本和签名可能分属不同增量，由 provider 合并。
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    /// LLM 决定调一个工具（agent loop 收到后去执行，结果回填成 `Message`）。
    ///
    /// `is_complete: false` 是**流式快照**：provider 用 `fix_streamed_json` 把累计
    /// 参数修补成可解析 JSON 后才发，`input` 一定是 `Json`（值可能被截断），供
    /// 支持流式的工具提前开工 / UI 预览；坏参数不发快照，继续等。
    /// `is_complete: true` 是最终判定：参数正常是 `Json`，坏掉时保留原文
    /// `Text`，由工具层转成错误结果，而不是让整个响应失败。
    ToolUse {
        id: String,
        name: String,
        input: crate::request::message::LanguageModelToolUseInput,
        is_complete: bool,
    },
    /// 本轮结束。
    Stop(crate::provider::model::StopReason),
    /// 用量更新（可选，provider 支持时发）。
    Usage(crate::provider::model::TokenUsage),
}
