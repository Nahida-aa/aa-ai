//! OpenRouter 的 `/chat/completions`：请求 DTO / 响应 DTO / 流式解码。
//!
//! 按**接口**分组，和 `aa_ai_openai` 一样：`chat_completion/` 是这一个接口的
//! 请求 + 响应；将来接 Responses API 就并列加 `responses/`，两套协议不共享结构体。
//!
//! OpenRouter 的 wire **大体**是 OpenAI chat completions，但有几处硬差异，
//! 全部落在本 crate：
//!
//! | 位置 | OpenAI | OpenRouter |
//! |---|---|---|
//! | 额外 header | 无 | `HTTP-Referer` / `X-Title`（应用归因，可选） |
//! | 思考强度 | 顶层 `reasoning_effort` | `reasoning: { effort }` |
//! | 思考回放 | 无 | assistant 消息可带 `reasoning` / `reasoning_details` |
//! | 流式 usage | `stream_options.include_usage` | 始终返回；`include_usage` 已废弃 |
//! | 错误体 | `{"error":{"message","type","code"}}` | `{"error":{"code":<number>,"message","metadata"}}` |
//! | 模型列表 | `/models` 只有 `id` / `owned_by` | `/models` 有 `context_length` / `pricing` / `supported_parameters` |
//!
//! 逐字段事实见 [`docs/compare/openrouter-wire.md`](../../../../docs/compare/openrouter-wire.md)。

pub mod request;
pub mod response;
