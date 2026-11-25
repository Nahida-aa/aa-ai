# 库/应用 开发策略 + provider 支持 + 包命名

> 日期：2026-11-12（2026-11-13 按「一家 provider 一个 crate」修订 §1/§2/§4）
> 后续决策记录见 [`docs/decisions/`](../decisions/README.md)（ADR + 可运行验证实验）。
>
> **归属**：本文是早期对话的整理稿，混了两种内容——**用户决定**（用户在对话里明确要求的）
> 和**助手建议**（助手基于 Zed/rig/genai/TanStack 对比给出的方案）。逐条标注了来源。
> 布局与命名的现行规则以 [`crate-layout.md`](crate-layout.md) 为准。

## 1. provider（LLM 提供商）要不要提前支持多个

**要，但 v0 只接 1 家，接口按"多 provider"设计。**（用户决定）

- TanStack 为 openai/anthropic/gemini/… 各写了 adapter 包。
- Zed 的模型是 `LanguageModelClient` trait + **每个 provider 一个 crate**
  （`crates/open_ai`、`crates/anthropic`、`crates/google_ai` 都依赖 `language_model_core`）。
- v0 不需要 10 家，但**接口（trait）要按"可插拔多 provider"设计**，实现先写 1 家
  （OpenAI，最通用）。以后加 provider = 新增一个 crate，不动接口。

**分层**（2026-11-13 修订：从「单 crate + provider 模块」改为「一家一 crate」）：

```
packages/
  aa_language_model/                  IR + trait，零网络依赖
    request/    request::Request / request::message::* / request::tools::*
    provider/   provider::LanguageModelClient / LlmError / CompletionEvent
    fake.rs     离线替身（fake feature）
  aa_language_models/
    aa_ai_openai/       OpenAI wire DTO + encoder + HTTP 客户端（依赖 reqwest）
    aa_ai_anthropic/    待建
    src/                汇总入口（openai::OpenAiLanguageModelProvider 等；fake 转发 core）
  aa_agent/
    agent_loop.rs       agent loop（消费 provider::LanguageModelClient + tool::AgentTool）
```

> 之前这里写的是 `aa_language_model_core` 里带 `provider/openai.rs`、`provider/fake.rs`。
> 那份布局会让 core 依赖 `reqwest`，且每加一家 provider 都要改 core。已按
> [`crate-layout.md`](crate-layout.md) 改掉，OpenAI 实现落在
> `packages/aa_language_models/aa_ai_openai/src`。

## 2. 一边写库一边写应用

**对，但要"接口先行、实现交替"，避免应用层空等**：（用户决定）

```
顺序（被依赖的先写，写一点就能往下走）：
1. aa_language_model: request（IR）+ provider（LanguageModelClient trait）
2. aa_language_models: aa_ai_openai（第一家 wire 适配）；aa_language_model 的 fake feature（离线）
3. aa_agent: agent_loop（消费 provider::LanguageModelClient + tool::AgentTool）
4. aacode cli: 接线 + 写 2-3 个工具（read/write/edit）impl tool::AgentTool
5. 跑通：fake LLM 调 read 工具（离线）→ OpenAI 真调（在线）
```

**关键**：IR/工具定义 和 工具实现 可以**并行**（工具定义不依赖 LLM 通信），
agent loop 依赖两者。所以不是串行等，是"接口/定义先立起来，实现在两边交替填"。

## 3. 包命名：去掉 `_core` 后缀（已确认、已执行）

既然**不分两个 crate**（core/app 合成一个），`_core` 后缀就名不副实（它暗示"还有个非 core
的对应物"）。（用户决定）

| 原名 | 现名 | 理由 |
|---|---|---|
| `aa_language_model_core` | `aa_language_model` | 不分了，就是"语言模型"域，不是"核心层" |
| `aa_agent_core` | `aa_agent` | 同上，它是"agent 运行时" |

状态：已执行——crate 名、`Cargo.toml`、workspace members、所有
`use aa_language_model_core::...` 都已改完。

## 4. 一句话总结

- **provider**（用户决定）：接口按多 provider 设计，v0 实现 1 家（OpenAI）+ 1 个 fake。
  **一家 provider 一个 crate**（`aa_ai_*`），core 只有 IR + trait、零网络依赖。
- **库/应用**（用户决定）：接口先行，IR 与工具实现并行，loop 居中。
- **命名**（用户决定）：去 `_core`；类型名用「命名空间 + 短类型名」，
  前缀不省略且写完整路径（`aa_language_model::request::Request`，不是
  `use ...::request;` 之后的 `request::Request`，不是裸 `Request`，也不是
  `LanguageModelRequest`）。细则见 [`crate-layout.md`](crate-layout.md) §3。
