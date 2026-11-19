//! `aa_ai_gemini` — Google Gemini `generateContent` 协议适配器（**尚未实现**）。
//!
//! 计划内容（见 [ADR 0008](../../../docs/decisions/0008-provider-tiers.md)）：
//! - `POST {base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse`；
//! - `contents[].parts[]` 与 `role` 的取值是 `user` / `model`（没有 assistant）；
//! - `generationConfig` 承载采样参数，输出上限叫 `maxOutputTokens`；
//! - `thought` part 是思考（Gemini 不给签名，回放时可以省略）。
