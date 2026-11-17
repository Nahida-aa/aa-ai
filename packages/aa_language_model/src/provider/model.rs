//! 模型（身份 + wire 能力位）/ 结束原因 / 用量。fork 自 Zed，按 0006 D4 / A 方案扩展。
//! Zed 原名：`Model` ← `LanguageModel`，`StopReason`/`TokenUsage` 同。

use serde::{Deserialize, Serialize};

/// 一个具体的 LLM 模型（provider + model_id + **模型自身的事实**）。
///
/// 只放「跟模型绑定、不随每次请求变化」的事实，对齐 Zed `LanguageModel`
/// （`language_model.rs:101-159`）与 `into_open_ai` 的 `config.*()` 参数
/// （`open_ai/src/completion.rs:63-72`）：`max_tokens` vs `max_completion_tokens`、
/// `parallel_tool_calls`、`prompt_cache_key` 支持与否、输出上限。
/// **每请求的选择**（`stream` / `reasoning_effort` / `service_tier` / `prompt_cache_key`）
/// 放在 `request::Request`（对齐 Zed `LanguageModelRequest`，`request.rs:470-495`），
/// 不放这里（0006 D4 / A 方案，用户 2026-11-13 拍板）。
///
/// **厂商级的差异不放这里**：端点、鉴权、`response_format` 支持、思考字段名、
/// `[DONE]` 语义等属于厂商（见 0008），由各 `aa_ai_<vendor>` crate 自己持有。
/// 这里只回答「这个模型本身能做什么」——同一个厂商的不同模型也会不一样
/// （OpenAI 的 o 系列用 `max_completion_tokens` / `developer`，gpt-4o 用
/// `max_tokens` / `system`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Model {
    /// provider 标识（`openai` / `deepseek` / `anthropic` / ...）。
    pub provider: String,
    /// provider 内的模型 id（`gpt-4o` / `claude-sonnet-4` / ...）。
    pub id: String,
    /// `true` 用 `max_completion_tokens`，`false` 用 `max_tokens`。
    pub uses_max_completion_tokens: bool,
    /// `true` 时 `system` 消息发成 `developer`（o 系列 / GPT-5）。
    pub uses_developer_role: bool,
    /// 是否接受 `tools` / `tool_choice`。
    pub supports_tools: bool,
    /// 模型是否支持 `parallel_tool_calls`。
    pub supports_parallel_tool_calls: bool,
    /// 是否接受图片输入。
    pub supports_images: bool,
    /// 模型是否接受 `prompt_cache_key`。
    pub supports_prompt_cache_key: bool,
    /// 上下文窗口（输入 + 输出），未知为 `None`。
    pub context_window: Option<u64>,
    /// 该模型的输出上限（对齐 Zed `LanguageModel::max_output_tokens`）。
    pub max_output_tokens: Option<u64>,
}

impl Model {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
            uses_max_completion_tokens: false,
            uses_developer_role: false,
            // OpenAI 家族默认支持工具；不支持工具的模型显式置 false。
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_images: false,
            supports_prompt_cache_key: false,
            context_window: None,
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
