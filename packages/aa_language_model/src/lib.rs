//! `aa_language_model` — canonical IR + provider 通信接口。
//!
//! 这个 crate 默认只有两样东西：
//! - `request`：canonical IR（`request::Request`、`request::message::*`、`request::tools::*`）；
//! - `provider`：`provider::LanguageModelClient` trait + 错误 + 事件类型。
//!
//! 打开 `fake` feature 后多一个 [`fake::FakeClient`]：离线替身，默认按预置剧本回事件，
//! 也可以用 `with_handler` 按请求现算（随机内容、注入错误都行）。
//! 它和 trait 放在同一 crate，是因为只依赖 IR + trait、不依赖任何厂商格式；
//! 用 feature 门控，正常构建不会带上测试替身。
//!
//! 不含任何厂商的 wire 格式，也**不依赖 `reqwest`**。加一家 provider = 新增一个
//! `aa_ai_<provider>` crate，不动这里。布局与命名规则见
//! [`docs/design/crate-layout.md`](../../../docs/design/crate-layout.md)。
//!
//! 引用约定：**命名空间 + 短类型名，且写完整路径、不写 `use` 导入**。下游一律写
//! `aa_language_model::request::Request`、`aa_language_model::request::message::Message`、
//! `aa_language_model::provider::LlmError`；不要 `use aa_language_model::request;` 之后
//! 写 `request::Request`，更不要 `use aa_language_model::request::Request;` 之后写裸
//! `Request`。crate 内部同理，写 `crate::request::message::ContentPart`。
//! 理由与正反例见 [`docs/design/crate-layout.md`](../../../docs/design/crate-layout.md) §3。
//!
//! 三层结构（见 `docs/decisions/0001-ir-layering.md`）：
//! - 历史模型：调用方持有 `Vec<Arc<request::message::Message>>`，唯一真相；
//! - 请求 IR：`request::Request`，按需克隆 `Arc`；
//! - wire DTO：provider crate 内部私有类型（如 `aa_ai_openai` 的 JSON body）。

pub mod provider;
pub mod request;

/// 离线 fake provider（剧本驱动，不连网）。默认关闭，见 Cargo.toml 的 `fake` feature。
#[cfg(any(test, feature = "fake"))]
pub mod fake;
