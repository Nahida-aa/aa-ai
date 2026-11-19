//! `aa_ai_anthropic` — Anthropic Messages 协议适配器（**尚未实现**）。
//!
//! 计划内容（见 [ADR 0008](../../../docs/decisions/0008-provider-tiers.md)）：
//! - `POST {base_url}/v1/messages`，`system` 是顶层字段而不是一条消息；
//! - 真 SSE，事件类型 `message_start` / `content_block_delta` / `message_delta`…；
//! - `thinking` 块带 `signature`，必须**原样、按位置**回放；
//! - `redacted_thinking` 是整块密文 → `ContentPart::RedactedThinking`；
//! - 工具结果在 user 轮的 `tool_result` 内容块里（IR 里已经是这个形状）。
