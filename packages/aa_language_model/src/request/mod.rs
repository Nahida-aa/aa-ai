//! 请求体类型（发给 LLM provider 的请求结构）。
//!
//! 子模块：
//! - `message`：canonical IR 的会话层（`Role` / `Message` / `ContentPart` 一族）；
//! - `tools`：工具**定义**（数据半，喂给 LLM），执行半在 `aa_agent`。
//!
//! 注意 `Message` **不实现 `serde`**：canonical IR 是内存中的唯一真相，wire 形状由各
//! provider 的借用式 encoder 现场生成（见 `aa_ai_openai`）。这样 provider 的字段
//! 差异不会倒逼 IR 变形，也不会把某个版本的 JSON 形状变成事实契约。
//! 需要持久化时再单独加版本化的 snapshot 格式。

pub mod message;
pub mod tools;

use std::sync::Arc;

/// 发给 LLM 的请求体。fork 自 Zed `LanguageModelRequest`，精简到 coding agent 需要的字段。
///
/// 内存约定（见 `docs/decisions/0004-message-layout.md` D7）：
/// - `messages` 是 `Vec<Arc<Message>>`。克隆请求只复制指针（8 B/条），**不复制 payload**；
///   历史本体始终只有一份。
/// - `tools` 同样是 `Arc`：工具 schema 是 JSON，深拷贝它比拷贝指针贵几个数量级。
///
/// 这里没有做成 `Request<'a> { messages: &'a [Arc<Message>] }`：那能省掉一次
/// `Vec` 分配（8 B × 条数），但会让请求类型带生命周期，无法跨重试/队列持有。
/// 8 B/条 + 一次小分配是可以接受的代价；真正的深拷贝（旧 `Vec<Message>`）才是问题。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub messages: Vec<Arc<crate::request::message::Message>>,
    /// 本次可用工具（喂给 LLM 的 `tools::Tool` 定义，数据半）。
    pub tools: Vec<Arc<crate::request::tools::Tool>>,
    pub temperature: Option<f32>,
    /// 遇到这些字符串就停止生成。
    pub stop: Vec<String>,
    /// 输出 token 上限（provider 可忽略）。
    pub max_output_tokens: Option<u64>,
    /// 工具选择策略。provider 无法表达时忽略，不因此报错。
    pub tool_choice: Option<ToolChoice>,
    /// 期望的输出格式。provider 无法表达时忽略，不因此报错。
    pub response_format: Option<ResponseFormat>,
    /// 本次是否发起流式请求（决定 wire 的 `stream` / `stream_options`）。
    /// 对齐 Zed `LanguageModelRequest` 的每请求语义（`request.rs:484` 一族）。
    pub stream: bool,
    /// 本次的推理强度（Zed `thinking_effort`，`request.rs:485`）。
    pub reasoning_effort: Option<crate::provider::ReasoningEffort>,
    /// 本次的处理层级（Zed `speed`，`request.rs:486`）。
    pub service_tier: Option<crate::provider::ServiceTier>,
    /// prompt cache 路由键（Zed `prompt_cache_key` / `thread_id`，`request.rs:471,476`）。
    pub prompt_cache_key: Option<String>,
}

/// 工具选择策略（provider 无关）。
///
/// wire 映射由各 provider 负责：OpenAI chat completions 用 `tool_choice`，
/// Anthropic 用 `tool_choice`，Gemini 用 `tool_config`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// 模型自行决定是否调用工具（默认）。
    Auto,
    /// 禁止调用工具。
    None,
    /// 必须调用至少一个工具。
    Required,
    /// 必须调用指定名字的工具。
    Specific(String),
}

/// 期望的输出格式（provider 无关）。
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    /// 普通文本（默认）。
    Text,
    /// 合法的 JSON 对象，但不保证符合某个 schema。
    JsonObject,
    /// 符合给定 JSON Schema 的输出。
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        /// 是否要求 provider 严格遵循 schema（部分 provider 支持）。
        strict: bool,
    },
}
