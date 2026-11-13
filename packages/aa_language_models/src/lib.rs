//! 各 provider 适配器的汇总入口（对应 Zed 的 `language_model` crate）。
//!
//! 这里**只做命名空间转发**，不摊平类型：
//! - `openai::OpenAiLanguageModelProvider`：OpenAI / OpenAI 兼容端点；
//! - `stub::StubClient`：离线测试替身，不连网络、不花 token。
//!
//! canonical IR 与 `provider::LanguageModelClient` 只有一条规范路径：`aa_language_model::...`。
//! 使用时要写完整路径，例如
//! `aa_language_models::openai::OpenAiLanguageModelProvider::new(api_key)`。
//!
//! 「一家一 crate」的原因（见 [`docs/design/crate-layout.md`](../../../docs/design/crate-layout.md)）：
//! wire 适配依赖（`reqwest`、厂商字段）只进入对应的 provider crate；core 保持零网络
//! 依赖，加/删 provider 不影响使用其他 provider 的人。

/// OpenAI / OpenAI 兼容端点。
pub mod openai {
    pub use aa_ai_openai::OpenAiLanguageModelProvider;
}

/// 离线 stub provider。
pub mod stub;
