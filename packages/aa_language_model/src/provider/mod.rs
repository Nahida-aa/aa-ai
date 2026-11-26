//! LLM 通信**接口**（trait + 错误 + 事件类型）。
//!
//! crate 布局（见 `docs/design/crate-layout.md`）：
//! - `aa_language_model`（本 crate）：canonical IR + [`LanguageModelClient`] trait；
//! - `aa_ai_openai` / `aa_ai_anthropic` / …：**一家一个 crate** 的 wire 适配；
//! - `aa_language_models`：provider 汇总入口；
//! - `fake`（本 crate，`fake` feature）：离线替身，不依赖任何厂商格式。
//!
//! core 不依赖 `reqwest`，也不认识任何厂商的 JSON 字段：加一家 provider = 新增一个
//! crate，不动这里。对应 Zed 的 `language_model_core` + `open_ai` / `anthropic` /
//! `google_ai` 布局。
//!
//! 关键：`LanguageModelClient` 是**库形态**（纯 async，不依赖任何 UI 运行时），
//! 区别于 Zed 的 `LanguageModelClient`（耦合 GPUI 的 `cx: &AsyncApp`）。
//! Zed 的 `stream_completion(&model, &request, cx)` 在这里变成
//! `stream_completion(&model, &request) -> EventStream`，可被任意 runtime 调用。

mod event;
mod model;
mod stream_json;

pub use event::LanguageModelCompletionEvent;
pub use model::{
    LanguageModel, ReasoningEffort, ResponseFormatSupport, ServiceTier, StopReason, TokenUsage,
};
/// 流式工具参数的修复式解析（provider 流解码用，工具层也可复用）。
pub use stream_json::{fix_streamed_json, parse_streamed_json};

use std::future::Future;
use std::pin::Pin;

/// 流式完成事件流（provider 产出，agent loop 消费）。
///
/// `'static` 是有意的：流一旦返回就自成一体，不借用调用栈上的 `Request`。
/// 需要借用历史的部分在 encoder 里当场写完 body，不把引用带出方法。
pub type EventStream =
    futures::stream::BoxStream<'static, Result<LanguageModelCompletionEvent, LlmError>>;

/// 与 LLM 服务器通信的接口（一次 LLM 调用，数据层）。
///
/// **不含 agent loop**：它只发一个 `Request`、收回流式 `LanguageModelCompletionEvent`。
/// 工具循环（收 `ToolUse` → 执行工具 → 回填 → 再发）由 `aa_agent` 的 loop 做。
///
/// 签名按 [0006](../../../../docs/decisions/0006-wire-request-shape.md) D2 重新设计：
/// 返回的 future 是 `'static`，**不借用 `self` / `model` / `request`**。实现者必须
/// 在构造 future **之前**同步把 `request` 编码成 owned 字节（`to_writer`），
/// 只把字节与所需的 owned 配置 move 进 future。这样借用式 wire DTO 的 `'a`
/// 终结在方法体内部，既不阻塞跨 `.await` 发送，也不限制暂存/重试。
pub trait LanguageModelClient: Send + Sync + 'static {
    /// 流式完成 `request`。先同步编码，再返回 `'static` future。
    ///
    /// 返回 `Pin<Box<dyn Future>>` 而不是 `impl Future`，是为了 trait 能当
    /// `&dyn LanguageModelClient` 用（`impl Trait` 返回值会让 trait 失去 dyn 兼容性，
    /// 而 agent loop 需要按运行时切换 provider）。装箱的是**一次 future**，
    /// 不是每次 stream 事件。
    fn stream_completion(
        &self,
        model: &LanguageModel,
        request: &crate::request::Request,
    ) -> Pin<Box<dyn Future<Output = Result<EventStream, LlmError>> + Send + 'static>>;
}

/// LLM 通信错误（库形态，不依赖 Zed 的 `anyhow` 重型错误分类）。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 没有该 provider 的 API key。
    #[error("missing api key for provider `{0}`")]
    NoApiKey(String),

    /// provider 拒绝请求（非 2xx）。保留原始 status + message，供上层判断是否重试。
    #[error("provider rejection ({status}): {message}")]
    ProviderRejection {
        status: u16,
        message: String,
        /// 是否可重试（429/5xx/超时类）。
        transient: bool,
    },

    /// 网络 / 序列化等底层错误。
    #[error("llm transport error: {0}")]
    Transport(String),

    /// 该 provider 无法表达这次请求（例如 OpenAI chat completions 不支持 Custom 工具）。
    #[error("unsupported by provider: {0}")]
    Unsupported(String),

    /// 流意外结束。
    #[error("llm stream ended unexpectedly")]
    StreamEnded,
}
