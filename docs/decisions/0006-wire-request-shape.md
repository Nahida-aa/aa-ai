# 0006. wire 请求体定型：带生命周期的强类型 DTO + 借用序列化；`LanguageModelClient` 命名

> 状态：proposed
> 日期：2026-11-13
> 关联：[0002](0002-memory-first.md)（D2 借用式序列化）、[0004](0004-message-layout.md)（wire 强类型结构体）、
> [`docs/design/crate-layout.md`](../design/crate-layout.md) §2/§3/§4（布局、命名、借用式 encoder）、
> [`docs/zed/language_models.md`](../zed/language_models.md)、[`docs/zed/provider-mapping.md`](../zed/provider-mapping.md)
>
> **归属：本文的方向由用户拍板（2026-11-13）。** 选项整理、源码核对与后果分析由助手完成。
>
> **定位：本文不推翻 0002 / 0004。** 0002 D2「借用式序列化」与 0004「wire body 是强类型结构体」
> 此前一直停留在文档，代码曾一度写成 owned DTO + `to_value`。本文是把两者**同时兑现**的实现决定，
> 不是 supersede。0002/0004 的结论继续成立。

## 背景与问题

讨论 `aa_ai_openai::chat_completion::request::ChatCompletionRequest` 「缺字段」时暴露出三个未定型的设计点，
现有代码（`packages/` 全部尚未提交，属草稿）都不能作为约定：

1. **wire 请求体是 owned 还是借用？** 0002 D2、`crate-layout.md:135,159` 都要求「借用式
   encoder（不重建 owned 历史）」；0004 要求「wire body 是强类型结构体，字段写错编译期失败」。
   曾有一版实现把 `ChatCompletionRequest` 写成 owned，用「调用链要跨 await / 现有 trait
   契约」当理由——这两条理由都不成立：
   - 带 `'a` 的强类型 DTO **可以**借用 IR 并同时满足编译期检查，两者不对立；
   - client trait 尚未设计，用户明确「还没设计好，可以随便改」，不能拿它当约束。
2. **`ChatCompletionRequest` 的字段表**：缺的是 OpenAI Chat Completions 的 wire 字段，不是
   core `Request` 的字段。此前误把 `tool_choice` / `response_format` 往 core 加，方向偏了
   （core 的两个类型按用户决定保留，见 D3）。
3. **client trait 改名 `LlmClient` → `LanguageModelClient`？**（用户 2026-11-13 拍板要改）

参考依据的采信优先级（用户 2026-11-13）：**rig > TanStack AI > genai**。Zed 用于对照
wire 字段基线与链路，不覆盖上述优先级。

核心约定（用户 2026-11-13 强调）：**任何既有代码都不是约定，不能因为已经存在某份代码，
就论证「因此不能怎样」。允许破坏性重构。**

## 当时的想法（选项）

### 选项组 1：wire 请求体的形状

| 方案 | 做法 | 强类型？ | 零拷贝借用 IR？ | body 能否跨 await/重试 |
|---|---|---|---|---|
| A 强类型 + owned DTO | `ChatCompletionRequest` 全 `String`/`Value` clone，`to_value` | ✅ | ❌ 文本与 schema 各复制一份 | ✅ |
| B 删 DTO，手写 `impl Serialize for RequestView<'a>` + `to_writer` | 无 DTO | ❌ 字段写错不报错 | ✅ | ✅ |
| **C 带 `'a` 的强类型 DTO + `to_writer`** | `ChatCompletionRequest<'a>` 字段借 `&'a str`/`&'a Value`/`&'a [String]`，流式写入 `Vec<u8>` | ✅ | ✅ | ✅ |

- A 违背 0002 D2 / `crate-layout.md:159`。
- B 失去 0004 要的编译期检查。
- **C 同时满足 0002 与 0004**：`'a` 只是 encoder 函数内的局部生命周期，不出现在任何
  跨 await 的公开类型上；`serde_json::to_writer(&mut Vec<u8>, &body)` 产出的 `Vec<u8>` 是
  owned，序列化在发请求前完成，可暂存、可重试、可跨 `.await`。

曾提出的反例「借用 DTO 会卡住 await / 重试」只在方案 A→B 的过渡态（先构造带 `'a` 的 DTO、
又不在同一作用域内序列化）才成立；按 C 的写法不存在该中间态。

### 选项组 2：client trait 命名

| 方案 | 结论 |
|---|---|
| **改 `LanguageModelClient`（对齐 Zed）** | ✅ 用户拍板 |
| 保持 `LlmClient` 短名 | ❌ |
| 连 `LlmError` 一起改 `LanguageModelError` | 未采纳（本次只改 trait，`LlmError` 保留） |

Zed 的 trait 叫 `LanguageModelClient`（`crates/language_model/src/language_model.rs:419`）；
改名后本仓库与 Zed 逐字对齐，符合用户在 `crate-layout.md` §3 例外 2 里已表达的「与 Zed
对齐」偏好。此前助手曾援引 `crate-layout.md:69`（长前缀反例）论证不该改名，那是把一条
通用命名规则凌驾于用户明确决定之上——用户已指出「任何既有代码/文档都不是用来限制设计的
约定」。`crate-layout.md` 的 `provider::LlmClient` 写法随之同步更新。

### 选项组 3：wire 字段表怎么补

| 方案 | 做法 | 先例 |
|---|---|---|
| 全列进静态 struct | 把 OpenAI API 字段尽量列全 | Zed `crates/open_ai/src/open_ai.rs:735-762`（15 个字段，但只用得到的子集） |
| 动态 `Value` 表 + 扩展口 | `Map<String, Value>` + `additional_params`/`extra_body` | rig `completion/options/merge.rs:27`（`FinalBody`）、genai `adapter_shared.rs:128`（`json!` + `x_merge`） |
| **强类型 struct 列到实际需要的字段 + 能力/请求分层** | wire 私有类型放 `aa_ai_openai::chat_completion::request`，每请求选择放 `Request`、静态能力位放 `Model`（见 D4） | Zed |

0004 已选「强类型、编译期失败」，故不走动态 `Value` 表。

## 验证（源码事实）

### 借用序列化可行，且不与强类型冲突

- `serde_json::to_writer` 接受 `&impl Serialize`；带 `'a` 的 DTO 实现 `Serialize` 无需 `'static`。
- rig 实际用了 `to_writer`：`crates/rig-core/src/completion/options/merge.rs:79`
  `serde_json::to_writer(&mut bytes, &self)`，产出 `Body::Bytes(Vec<u8>)`（`:80`）。
  它写的是自己的 owned `FinalBody`（`merge.rs:27-28`，`Map<String, Value>`），
  说明 `to_writer` 与「强类型/owned 中间层」正交；把中间层换成借用 DTO 不改变这一步。
- `Cow` 局部借用有先例：`rig-core/src/providers/openai/wire/chat.rs:133` 的
  `fn text(part: &ToolResultContent) -> Option<Cow<'_, str>>`。
- 结论：**C 方案在 Rust 类型系统与 serde 下成立**，无生命周期泄漏。

### Zed / rig 的请求构造链路（对照，不是约束）

| | encoder 签名 | 序列化 | body 形态 | future |
|---|---|---|---|---|
| Zed | `into_open_ai(request: LanguageModelRequest, ...)` 按值消费（`open_ai/src/completion.rs:63-72`） | `serde_json::to_string(request)`（`open_ai/src/open_ai.rs:1214`） | `String` → `AsyncBody` | `BoxFuture<'static>`（`language_models/src/provider/open_ai.rs:168`） |
| rig | `fn encode(&self, request: CompletionRequest, ...)` 按值消费（`wire/chat.rs:751`） | `serde_json::to_writer` → `Vec<u8>`（`merge.rs:79`） | `Vec<u8>`（`merge.rs:80`） | — |

- 两家都是「**按值消费 IR + 立即序列化成 owned bytes，再 await**」。
  Zed 靠 move 把 `MessageContent::Text(String)`、`input_schema: Value` 直接搬进 wire DTO
  （`completion.rs:234-251`），不复制；rig 靠 `Value` 树 + `to_writer`。
- 两家**都没有** `OaMessageRef<'a>` 式的整体借用适配器：Zed 因为按值 move 已无副本可省，
  rig 因为要动态方言/`additional_params`。这是取舍，**不是证明借用不可行**。
- 两家的 wire 字段都不是「补全 OpenAI API」，而是「实际需要的子集 + 能力开关」：
  Zed `open_ai.rs:735-762` 只有 15 个字段（`response_format`/`seed`/`top_p`/`n`/
  `frequency_penalty` 等都没有），能力字段由 `into_open_ai` 的参数决定
  （`completion.rs:216-233`：`max_tokens` vs `max_completion_tokens`、`parallel_tool_calls`）；
  rig 用 `Quirks`（`wire/chat.rs:745` `supports_response_format`）与 `additional_params`
  （`completion/request.rs:1281`），另有 `GenerationOptions`（`options.rs:43-80`）承载
  `top_p`/`seed`/`parallel_tool_calls` 等可移植参数。
- **字段差异（按用户给的优先级 rig > TanStack > genai 记录）**：Zed 的 wire 没有
  `top_p`/`seed`/`response_format`，但 rig `GenerationOptions`、genai `ChatOptions`、
  TanStack 的 `modelOptions` 都有。本 ADR 按用户选 B，wire 字段表仍以 Zed 15 为基线，
  差异留到「能力模型层」而非塞进 wire struct。

### 字段值的来源

- Zed 的 `LanguageModelToolChoice` 在 **core**（`crates/language_model_core`），
  `into_open_ai` 把它映射成 wire 的 `open_ai::ToolChoice`（`open_ai.rs:780-788`）。
  所以「策略枚举放 core、wire 形状放 provider crate」有直接先例。
- genai 把采样参数集中在 `ChatOptions`（`src/chat/chat_options.rs:28-118`），
  wire 由 adapter 用 `json!` 逐字段 `x_insert`（`adapter_shared.rs:128-265`）。
- TanStack AI 直接用 OpenAI SDK 的 `ChatCompletionCreateParamsStreaming` 当 wire 类型
  （`packages/openai-base/src/adapters/chat-completions-text.ts:1446`），字段由 SDK 保证。

### `stream_options` 的写法

`stream_options` 是 OpenAI Chat Completions 流式请求的可选对象，作用是让服务端在流
结束时多回一个带 `usage` 的 chunk，标准形状 `{"stream_options":{"include_usage":true}}`；
**只在 `stream: true` 时有意义**。各家用的是不同机制：

| 库 | 做法 |
|---|---|
| Zed | 强类型 `StreamOptions { include_usage: bool }`（`open_ai.rs:721-732`，`Default` 为 `true`）；wire `Request` 里 `Option<StreamOptions>`（`:740`），`into_open_ai` 中 `if stream { Some(default()) } else { None }`（`completion.rs:205-209`） |
| rig（优先级最高） | **无该类型**：`Rewrite::StreamUsage` 在合并阶段往 `Value` 树插 `stream_options.include_usage=true`（`merge.rs:933-941`），由能力开关 `Quirks::stream_include_usage`（`wire.rs:357`）决定；`chat.rs:772-776` 注明「调用方自己的 stream_options 优先」 |
| genai | 无该类型：`if stream && capture_usage { payload.x_insert("stream_options", json!({"include_usage":true})) }`（`adapter_shared.rs:196-197`），由每请求选项 `ChatOptions.capture_usage` 驱动 |
| TanStack AI | chat-completions adapter 在 `chatStream()` spread `stream: true`、`structuredOutput()` 里 strip；`stream_options` 归属本轮未逐字核实，不臆断 |

本 ADR 采用 Zed 式强类型 `StreamOptions`，但由 **`request.stream`** 派生（而非模型能力位），
与 genai 的「每请求选项驱动」思路一致。

### 关于 `RequestOptions`（核实结论）

助手初版自造了一个 `aa_ai_openai::chat_completion::request::RequestOptions` 聚合能力参数，并写成
「对齐 Zed」。经核实**四家都没有这个类型**，该说法不成立：

| 库 | 能力/参数放在哪 |
|---|---|
| Zed | 无聚合 struct：`into_open_ai` 的 8 个散装参数（`completion.rs:63-72`），值来自 `config.*()`；能力位在 `LanguageModel`（`language_model.rs:101-159`） |
| rig | `CompletionRequest.options: GenerationOptions`（`request.rs:1312`）+ `provider_options` + `additional_params`；能力开关在 `Quirks` / `ModelFacts`，`encode` 只有 `mode` 一个额外参数（`wire/chat.rs:751`）。唯一同名 `RequestOptions` 是 `rig-test-support/src/model_session.rs:349` 的测试辅助 |
| genai | `ChatOptions`（`chat_options.rs:28`），非 `RequestOptions` |
| TanStack AI | `modelOptions` / `TextOptions`，SDK 类型的一部分 |

按用户选 **A + 后续拍板**：删掉自造的 `RequestOptions`，encoder 两参数；
静态能力位挂 `provider::Model`，每请求选择（`stream` / `reasoning_effort` /
`service_tier` / `prompt_cache_key`）挂 `request::Request`（见 D4）。
这也顺带消掉了初版为让 `prompt_cache_key: &'a str` 借用成立而给 options 加的 `&'a`
——现在 `'a` 只借用 `Request` 和 `Model` 两个已存在的对象。

## 决策

### D1：`ChatCompletionRequest<'a>` 为带生命周期的强类型 DTO（用户方向）

- 字段借用 canonical IR：模型名 `&'a str`、`stop: Option<&'a [String]>`、工具
  `name`/`description: &'a str`、`parameters: &'a serde_json::Value`、消息文本按需
  `&'a str` / `Cow<'a, str>`。**不 clone** 历史文本与 schema。
- 必须拼接/形状变换的部分才分配：消息文本、`tool_calls[].function.arguments`（OpenAI 要求
  JSON 字符串）、`role:"tool"` 的 `tool_call_id`。
- 序列化用 `serde_json::to_writer` 写入 owned `Vec<u8>`；`'a` 只活在 encoder 内部，
  **不出现在任何跨 await 的公开类型上**。`Vec<u8>` 可暂存、可重试、可跨 `.await`。
- 满足 0004：字段名/枚举变体写错仍编译期失败。

### D2：client trait 改名 `LanguageModelClient` 并重新设计（用户拍板）

- **`LlmClient` → `LanguageModelClient`**（对齐 Zed `language_model.rs:419`），`LlmError` 保留。
  全仓引用（代码 + `crate-layout.md` / `strategy.md` / 0004 的名字引用 / 索引）同步更新。
- 新签名：返回的 future 是 `'static`，方法体**先同步完成序列化**产出 owned 字节，再把
  字节与所需 owned 配置 move 进 future；不把 `Request` 的借用带进响应流。
- 不预设「现有契约堵死借用」——trait 本身是本次一并重新设计的对象。

### D3：core 的 `ToolChoice` / `ResponseFormat` 保留在 core（用户决定，Q2「留」）

- 依据 Zed 先例（策略枚举在 `language_model_core`）；wire 私有形状
  （`ChatToolChoice` / `ChatResponseFormat` 等）留在 `aa_ai_openai::chat_completion::request`。
- 注：这两个类型是「策略语义」，不是 wire 字段；本次「缺字段」指 wire DTO 的字段表，
  两者不混。

### D4：`ChatCompletionRequest` 字段表：Zed 15 字段为基线 + `response_format`（用户选 B）

- 基线严格对齐 Zed `open_ai.rs:735-762` 的 15 个字段：`model` / `messages` / `stream` /
  `stream_options` / `max_completion_tokens` / `max_tokens` / `stop` / `temperature` /
  `tool_choice` / `parallel_tool_calls` / `tools` / `prompt_cache_key` /
  `reasoning_effort` / `service_tier`。
- **额外保留 `response_format`**：结构化输出是真需求（rig `wire/chat.rs:745` 的
  `supports_response_format`、genai `ChatOptions.response_format`、TanStack 的
  `structuredOutput()` 都有对应概念），Zed 的 chat-completions wire 恰好没有。
- **不纳入 `top_p` / `seed` / `logit_bias` / `frequency_penalty` / `presence_penalty` /
  `logprobs` 等采样诊断字段**：Zed wire 没有；曾按「OpenAI API 全集」臆造加入 `top_p`/`seed`，
  已删除。注意 rig 的 `GenerationOptions` 有 `top_p`/`seed`（`completion/options.rs:61,64`），
  TanStack/genai 也有——这是**能力模型层的差异**，不是 wire 字段表的依据；按用户选 B，
  v0 wire 只取 Zed 基线 + `response_format`。将来真需要这些参数时另写 ADR 扩展。
- **字段分两层，encoder 签名 `to_chat_completion_request(&Request, &Model)` /
  `encode(&Request, &Model)` 两参数**（用户 2026-11-13 拍板「另外一个放
  `LanguageModelRequest` 上」+「按你的推荐」，对齐 Zed 的实际分层）：
  - **每请求选择 → `request::Request`**（对齐 Zed `LanguageModelRequest`，
    `language_model_core/src/request.rs:470-495`）：`stream`、`reasoning_effort`
    （Zed `thinking_effort`）、`service_tier`（Zed `speed`）、`prompt_cache_key`
    （Zed `prompt_cache_key` / `thread_id`）。这些是「这一次想怎么发」，不是模型属性。
  - **静态能力位 → `provider::Model`**（对齐 Zed `LanguageModel` 与 `into_open_ai`
    的 `config.*()`，`completion.rs:63-72`）：`uses_max_completion_tokens`、
    `supports_parallel_tool_calls`、`supports_prompt_cache_key`、`max_output_tokens`。
    `prompt_cache_key` 仅在模型 `supports_prompt_cache_key` 为真、且请求确实给了键时才发。
  - 不由 encoder 按模型名字符串猜测，也不引入独立的 options struct。
- **`StreamOptions`**：OpenAI 流式请求的可选对象 `{"include_usage": true}`，只在
  `stream: true` 时带，由 `request.stream` 派生（Zed `open_ai.rs:721-732` 的强类型
  `StreamOptions { include_usage: bool }` + `completion.rs:205-209` 的
  `if stream { Some(default()) }`）。详见「验证 / `stream_options` 的写法」。
- wire 私有类型（`ChatToolChoice`、`ChatResponseFormat`、`StreamOptions`、
  `ToolKind`、`ChatMessage` 一族）全部放 `aa_ai_openai::chat_completion::request`；
  `ReasoningEffort` / `ServiceTier` 因属于跨 provider 的能力枚举，放 core
  `provider` 并 derive `Serialize`（wire 直接复用），core 仍不认识任何厂商 JSON 字段名
  （`crate-layout.md:38`）。

## 后果

**正面：**

- 0002 D2 与 0004 同时兑现：零拷贝借用 + 编译期字段检查，二者不再二选一。
- 与 `crate-layout.md:135,159` 的「借用式 encoder」一致。
- wire DTO 生命周期被限制在 encoder 局部，不影响重试、暂存、异步发送。

**代价：**

- 每个 provider 要写 borrow-friendly 的 DTO（`impl Serialize` 或 derive + 借用字段），
  比 `to_value(&owned_struct)` 多一些代码（0002:132 已认过这笔账）。
- `ChatCompletionRequest<'a>` 的生命周期会传到 `to_chat_completion_request` 的签名；
  调用方必须在同一作用域内完成序列化，不能长期持有 body（这是有意的约束）。
- `tool_calls[].function.arguments` 仍必须转成 JSON 字符串、消息文本仍要拼接，
  这几处分配省不掉。

**触发重新评估的条件：**

- 若实测「借用 DTO + `to_writer`」相比「按值 move owned DTO + `to_string`」的峰值 RSS
  收益 < 5%（0002:137 的阈值），可退回 Zed 的按值 move 路线，但**不要**退回
  「owned DTO + `to_value` 的 `Value` 中间树」。
- 若将来只做网关/透传（不解析语义），动态 `Value` 表可能更合理，需另写 ADR。
- 若 wire 字段通过 SDK 类型（TanStack 路线）引入依赖，字段表维护方式需重评。

## 附录：待验证

- [ ] 用全局分配计数器测「借用 DTO + `to_writer`」 vs 「按值 move owned DTO + `to_string`」
  的分配次数与峰值 RSS（承接 0002 附录第 144 行、0002:137 的阈值）。
- [ ] 200k token 级历史的请求期峰值，确认没有第二份历史。
- [x] `LanguageModelClient` 新签名已落地：返回 `'static` future，同步编码在方法体内完成。
