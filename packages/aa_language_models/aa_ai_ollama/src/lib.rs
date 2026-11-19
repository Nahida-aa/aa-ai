//! `aa_ai_ollama` — Ollama 原生 `/api/chat` 适配器（**尚未实现**）。
//!
//! 计划内容（见 [`docs/design/crate-layout.md`](../../../docs/design/crate-layout.md)
//! 和 [ADR 0008](../../../docs/decisions/0008-provider-tiers.md)）：
//! - `POST {base_url}/api/chat`，请求/响应 DTO 与 OpenAI 完全不同（NDJSON 行流，不是 SSE）；
//! - `message.thinking` → `provider::CompletionEvent::Reasoning`；
//! - `message.tool_calls[].function.arguments` 是**对象**，不是 JSON 字符串；
//! - `done_reason` 决定 `provider::StopReason`；`prompt_eval_count` / `eval_count` → `TokenUsage`。
//!
//! 这个 crate 存在只是为了固定「第二个协议」的位置。在它真正实现之前：
//! - **不要**把 `openai_compatible` 的 `/v1` 影子实现搬进来 —— 那是另一个协议；
//! - 应用层要连本地 Ollama，请配 `language_model_providers.openai_compatible.ollama`。
