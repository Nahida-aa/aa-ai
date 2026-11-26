# 0001. IR 三层分离、内容用 enum、命名 `ContentPart`

> 状态：accepted
> 日期：2026-11-12
> 关联：[0002](0002-memory-first.md)、`docs/zed/language_models.md` §1–§3、§10

## 背景与问题

我们打算 fork/参考 Zed 的 `language_model` 层，给 aa-ai 做多提供商 LLM 调用。设计初期有三个容易混在一起的问题：

1. **“IR”到底指什么？** 是会话历史、单次请求，还是某家提供商的 JSON body？
2. **消息内容用 enum 还是 struct + 一堆 Option？** 有人担心“enum 是库的做法，应用里不用”。
3. **内容片段叫什么？** `Block` / `Content` / `Part` / `MessageContent` 都有人用，容易和 UI 的“代码块/渲染块”混淆。

如果这三个问题不先定，后面的 provider 适配、持久化、压缩、UI 渲染都会反复改类型。

## 当时的想法（选项）

### 问题 1：IR 的分层

| 选项 | 做法 | 问题 |
|---|---|---|
| A. 只有一层 | 一个 `Message`/`Request` 既存历史又发请求 | 应用字段（id/时间/UI 状态）混进 wire；请求参数没地方放 |
| B. 两层 | 历史模型 + wire body | 每调用都要从历史现场拼 body，映射逻辑散落 |
| C. 三层 | 历史模型 → 请求 IR → wire DTO | 多一层转换，但每层职责清楚 |

**关键认知**：内存和压缩发生在**历史层**；wire 正确性发生在**wire 层**；请求 IR 只是一次调用的派生结构，改起来最便宜。之前把“IR”笼统地用来指历史层，是表述错误。

### 问题 2：内容种类用 enum 还是 struct

```rust
// enum：每个变体只带自己的字段
enum ContentPart {
    Text(Arc<str>),
    Reasoning { text: Arc<str>, signature: Option<Arc<str>>, .. },
    ToolCall { id: Arc<str>, name: Arc<str>, input: LanguageModelToolUseInput },
    ..
}

// struct：所有字段都可选
struct Content {
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    tool_call: Option<ToolCall>,
    ..
}
```

**核对结论：Zed 作为应用也大量用 enum**，并非“enum 只适合库”：

- `agent/src/thread.rs:216` `pub enum Message { User, Agent, Resume, Compaction }`
- `agent/src/thread.rs:224` `pub enum CompactionInfo { Summary, ProviderNative }`
- `language_model_core/src/request.rs:263` `pub enum MessageContent { Text, Thinking, RedactedThinking, Image, ToolUse, ToolResult, Compaction }`

而且 Zed 的内部 enum **没有** `#[non_exhaustive]`——应用身份让它可以把“新增变体导致所有 match 报错”当作优点。

**判据不是“应用 vs 库”，而是：**

> **封闭的语义集合用 enum；开放的实现集合用 trait/dyn。**

- 内容种类（文本/思考/图片/工具调用/工具结果/压缩）是封闭语义 → enum。
- 提供商是开放集合（用户随时加新 provider）→ `Arc<dyn LanguageModelProvider>`。
- 两者不冲突：Zed 就是 enum 管内容、dyn 管 provider。

### 问题 3：命名

| 候选 | 出处 | 问题 |
|---|---|---|
| `Block` | Anthropic 的 “content block” | 与 UI 的代码块/渲染块冲突 |
| `Content` | 泛称 | 容易和“整条消息内容”混 |
| `Part` | Gemini | 短，但过于通用 |
| `ContentPart` | genai | 自描述，与 wire 的 “part/block” 都能对应 |

## 验证

- **源码核对**（详见 `docs/zed/language_models.md` §3、§10）：
  - Zed 三层确实是 `agent::Message` → `LanguageModelRequestMessage` → `open_ai::Request` 等；
  - genai 把历史和请求合并为 `ChatMessage`/`ChatRequest`，响应复用 `MessageContent`；
  - rig 共享 `Message`（`CompletionRequest.chat_history: Vec<Arc<Message>>`），响应用 `Vec<AssistantContent>`。
- **类型尺寸实验**（`experiments/ir-size/`）：enum + `Arc` 的消息**栈上信封** 24B/条，OpenAI owned 形状 128B/条。
  **注意：这只量了信封**；补全堆分配后，均匀短文本下是 200 vs 104 B/条，混合真实负载下只差 2.3%（[0003](0003-alloc-measurement.md) 的 `history-bytes`）。
  enum 的价值不在省字节，而在表达能力与零拷贝派生。
- **未验证**：持久化格式的长期演进成本；这需要在实现 thread 存储时再写一个 ADR。

## 决策

### D1：三层分离

```
历史模型（长生命周期，持久化/压缩/回放）
    Thread { messages: Vec<Arc<Message>> }
        │  派生（共享，不深拷贝）
        ▼
请求 IR（一次调用，短生命周期）
    CompletionRequest { history: &[Arc<Message>], tools, options, .. }
        │  编码（借用式序列化）
        ▼
wire DTO（瞬时）
    OpenAiRequest<'a> / AnthropicRequest / ..
```

- 历史是**唯一真相**；请求 IR 从历史派生；wire DTO 发完即弃。
- 会话层可以有 id、时间、UI 状态、压缩标记；请求层只放本次调用需要的东西；wire 层只放协议字段。

### D2：内容种类用 enum

- 用 `enum ContentPart`，不用 `struct` + 多 `Option`。
- 内部代码用**穷尽 enum**（不加 `#[non_exhaustive]`），让编译器逼所有适配器处理新语义。
- 如果将来 aa-ai 要作为公开库，再在**公开边界**上加 `#[non_exhaustive]`；内部实现仍保持穷尽。
- 为 provider 私有数据留 `Opaque(Arc<Native>)` 变体，避免为每个厂商新增变体。

### D3：命名 `ContentPart`

- 消息：`Message { role, content: Box<[ContentPart]>, .. }`
  （初版叫 `parts`、用 `Arc<[..]>`；命名与布局后来在 [0004](0004-message-layout.md) 定稿。）
- 片段：`ContentPart`，不用 `Block`、不用泛化的 `Content`。
- `ToolCall` / `ToolResult` **建模成 part**，不是 message 的独立字段：
  - Anthropic / Gemini 本来就是块级/part 级；
  - OpenAI 的 message 级 `tool_calls` 在 encode 时再提升即可；
  - 反过来按 OpenAI 建模，会逼所有非 OpenAI 适配器做拆装。

### D4：共享 parts，分离 envelope

- 历史消息和请求消息可以共享 `ContentPart` 类型，但**信封分开**：
  - 历史：`Message { id, role, parts, origin, .. }`
  - 请求：`RequestMessage { role, parts: &[ContentPart] / Arc<[ContentPart]>, cache }`
- 这样既不把 UI 字段带进 wire，又不需要复制内容。
- 如果后续发现转换成本过高，再评估合并信封（像 genai），但要写新 ADR。

## 后果

**正面：**

- provider 适配器只关心 `ContentPart` → wire 的映射，不关心会话/UI。
- 新增内容语义时，所有适配器会被编译器点名，不会静默漏字段。
- 历史层可以独立做压缩、持久化、回放，不影响 wire 层。

**代价：**

- 多一层 `Message` → `RequestMessage` 的映射（内容共享，但信封要重建）。
- 内部穷尽 enum 意味着新增变体会产生一批编译错误；这是刻意选择的成本。
- 公开库化时需要额外的 `#[non_exhaustive]`/扩展点设计。

**重新评估的触发条件：**

- 内容种类开始每周新增，且多数是 provider 私有 → 检查 `Opaque` 是否够用，考虑 trait 化。
- 请求/历史信封的转换成为性能热点（实测，不是猜）→ 考虑合并或缓存。
- 需要跨进程/跨语言传输 IR → canonical 的 serde 格式变成契约，需要单独的版本化 ADR。
