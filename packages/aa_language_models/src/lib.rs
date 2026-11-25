//! 各 provider 适配器的汇总入口（对应 Zed 的 `language_models` crate）。
//!
//! 这里**只做命名空间转发**，不摊平类型：
//! - `openai::OpenAiLanguageModelProvider`：OpenAI / OpenAI 兼容端点；
//! - `openrouter::OpenRouterLanguageModelProvider`：OpenRouter（chat completions + 厂商差异）；
//! - `ollama::OllamaLanguageModelProvider`：Ollama 原生 `/api/chat`；
//! - `fake::FakeClient`（需 `fake` feature）：离线替身，不连网络、不花 token；
//! - `mock_server::MockServer`（需 `mock-server` feature）：真 socket 的 mock 端点，
//!   按厂商 wire 协议应答，用真实 HTTP 链路做离线验证。
//!
//! canonical IR 与 `provider::LanguageModelClient` 只有一条规范路径：`aa_language_model::...`。
//! 使用时要写完整路径，例如
//! `aa_language_models::openai::OpenAiLanguageModelProvider::new(http, api_key)`。
//!
//! 「一家一 crate」的原因（见 [`docs/design/crate-layout.md`](../../../docs/design/crate-layout.md)）：
//! wire 适配依赖（`reqwest`、厂商字段）只进入对应的 provider crate；core 保持零网络
//! 依赖，加/删 provider 不影响使用其他 provider 的人。
//!
//! 离线 fake 的类型本体在 `aa_language_model::fake`（和 trait 放在一起），这里只是
//! 转发，保持 `aa_language_models::fake` 这条路径。

/// OpenAI / OpenAI 兼容端点（`POST /chat/completions`，SSE）。
pub mod openai {
    pub use aa_ai_openai::OpenAiLanguageModelProvider;
}

/// OpenRouter（`POST /chat/completions`，SSE；额外 header / 思考回放 / 数字错误码）。
pub mod openrouter {
    pub use aa_ai_openrouter::OpenRouterLanguageModelProvider;
}

/// Ollama 原生端点（`POST /api/chat`，NDJSON）。
///
/// 和 `openai_compatible.ollama`（`/v1`）是两个协议，见
/// [`aa_ai_ollama::chat`]。
pub mod ollama {
    pub use aa_ai_ollama::OllamaLanguageModelProvider;
}

/// 离线 fake provider（`aa_language_model::fake::FakeClient` 的转发）。
///
/// 类型本体在 core：它只依赖 IR + trait，不需要任何厂商 DTO / HTTP。
#[cfg(feature = "fake")]
pub mod fake {
    pub use aa_language_model::fake::FakeClient;
}

/// 真 socket 的 mock 端点（`mock-server` feature）。
///
/// 绑 `127.0.0.1:0`，按选定厂商的 wire 协议应答；用于离线跑真实的
/// settings → reqwest → provider → agent loop 链路。
#[cfg(feature = "mock-server")]
pub mod mock_server;
