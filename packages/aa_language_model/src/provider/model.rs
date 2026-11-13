//! 模型（身份 + wire 能力位）/ 结束原因 / 用量。fork 自 Zed，按 0006 D4 / A 方案扩展。
//! Zed 原名：`Model` ← `LanguageModel`，`StopReason`/`TokenUsage` 同。

use serde::{Deserialize, Serialize};

/// 一个具体的 LLM 模型（provider + model_id + **静态能力位**）。
///
/// 只放「跟模型绑定、不随每次请求变化」的能力位，对齐 Zed `LanguageModel`
/// （`language_model.rs:101-159`）与 `into_open_ai` 的 `config.*()` 参数
/// （`open_ai/src/completion.rs:63-72`）：`max_tokens` vs `max_completion_tokens`、
/// `parallel_tool_calls`、`prompt_cache_key` 支持与否、输出上限。
/// **每请求的选择**（`stream` / `reasoning_effort` / `service_tier` / `prompt_cache_key`）
/// 放在 `request::Request`（对齐 Zed `LanguageModelRequest`，`request.rs:470-495`），
/// 不放这里（0006 D4 / A 方案，用户 2026-11-13 拍板）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Model {
    /// provider 标识（`openai` / `anthropic` / ...）。
    pub provider: String,
    /// provider 内的模型 id（`gpt-4o` / `claude-sonnet-4` / ...）。
    pub id: String,
    /// `true` 用 `max_completion_tokens`，`false` 用 `max_tokens`。
    pub uses_max_completion_tokens: bool,
    /// 模型是否支持 `parallel_tool_calls`。
    pub supports_parallel_tool_calls: bool,
    /// 模型是否接受 `prompt_cache_key`。
    pub supports_prompt_cache_key: bool,
    /// 该模型的输出上限（对齐 Zed `LanguageModel::max_output_tokens`）。
    pub max_output_tokens: Option<u64>,
}

impl Model {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
            uses_max_completion_tokens: false,
            supports_parallel_tool_calls: false,
            supports_prompt_cache_key: false,
            max_output_tokens: None,
        }
    }
}

/// 推理强度（wire 值，provider 无关的枚举放 core）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// 处理层级（对齐 Zed `open_ai.rs:769-778` 的 `ServiceTier`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.id)
    }
}

/// 结束原因。Zed 原名 `StopReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
}

/// token 用量。Zed 原名 `TokenUsage`（精简到 input/output）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
