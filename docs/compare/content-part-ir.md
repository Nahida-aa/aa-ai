# 四家的 ContentPart IR（事实记录）

> 只记录源码事实：枚举定义、字段、「私有变体」在什么情况下真的被构造。
> **不做取舍、不给建议。**
> 行号以本地克隆为准：
> - Zed：`/home/aa/repos/ide_ls/learn_ls/zed`
> - genai：`/home/aa/repos/lib_ls/learn_ls/genai`（v0.7.5，commit `d852bfd`）
> - rig：`/home/aa/repos/lib_ls/learn_ls/rig`（commit `6e13e8c`）
> - TanStack AI：`/home/aa/repos/lib_ls/learn_ls/ai`（pnpm workspace）

## 1. Zed

content 叫 `MessageContent`，挂在请求消息上：

```rust
// crates/language_model_core/src/request.rs:355
pub struct LanguageModelRequestMessage {
    pub role: Role,
    pub content: Vec<MessageContent>,
    pub cache: bool,
    pub reasoning_details: Option<Arc<serde_json::Value>>,   // 消息级裸 JSON
}
```

```rust
// crates/language_model_core/src/request.rs:264
pub enum MessageContent {
    Text(String),
    Thinking { text: String, signature: Option<String> },
    RedactedThinking(String),
    Image(LanguageModelImage),
    ToolUse(LanguageModelToolUse),
    ToolResult(LanguageModelToolResult),
    Compaction(CompactedContext),
}
```

私有负载分散在四个地方：

| 位置 | 类型 | 构造点 |
|---|---|---|
| `Thinking.signature` | `Option<String>` | Anthropic thinking 签名，随流事件写入 |
| `RedactedThinking(String)` | 整块加密数据 | `crates/agent/src/thread.rs:3780` `handle_redacted_thinking_event`，来源 Anthropic `redacted_thinking` 事件 |
| 消息级 `reasoning_details` | `Option<Arc<Value>>` | OpenAI 系回放（各 `open_ai` adapter 写入） |
| `Compaction(CompactedContext)` | `ProviderState(ProviderCompactionState)` | `crates/open_ai/src/responses.rs:139`（把 `items` 序列化成 payload）、`crates/anthropic/src/completion.rs:36`（`encrypted_content`） |

```rust
// crates/language_model_core/src/request.rs:278
pub enum CompactedContext {
    Summary { content: Arc<str>, provider_state: Option<ProviderCompactionState> },
    ProviderState(ProviderCompactionState),
}

// :296
pub struct ProviderCompactionState {
    provider_id: LanguageModelProviderId,
    format: SharedString,
    payload: Arc<str>,
}
```

关键点：**`ProviderCompactionState` 只出现在压缩段**，不在普通消息内容里。
文档注释原文：*"Opaque context produced by a provider's native compaction mechanism.
Only the provider identified by `provider_id` may interpret `payload`."*

### 1.4 枚举上有哪些方法

- `impl MessageContent` **只有一个方法**：`is_empty()`（`request.rs:328`）。
- 另有 `impl From<String>` / `impl From<&str>`（`request.rs:341-353`），把裸字符串当成 `Text`。
- **没有** `MessageContent::text(..)` / `tool_call(..)` 这类具名构造器：变体字段是 `pub`，
  各 provider 直接写 `MessageContent::Text(..)` / `MessageContent::ToolUse(..)`。
- 相邻类型：`ProviderCompactionState::new(..)`（`:302`）、
  `LanguageModelRequestMessage::{string_contents, contents_empty}`（`:363` 起）。
- 注：`crates/open_ai`、`crates/deepseek`、`crates/mistral` 等 provider crate 里各自也有
  一个 `impl MessageContent`，那是它们**自己的 wire 枚举**，和 canonical 的这个同名不同型。

## 2. rig

`AssistantContent` 有 `Opaque` 变体：

```rust
// crates/rig-core/src/completion/message.rs:205
pub enum AssistantContent {
    Text(Text),
    ToolCall(ToolCall),
    Reasoning(Reasoning),
    Image(Image),
    /// A provider item with no canonical meaning.
    Opaque(Opaque),
}

// crates/rig-core/src/completion/message/native.rs:389
/// A provider item with no canonical meaning, such as a hosted-tool step or
/// a compaction record. Only the API that produced it reads it.
pub struct Opaque {
    /// The provider item.
    pub item: serde_json::Value,
    /// Whether the item goes back to the model that produced it. A
    /// client-executed call nothing answers is kept but never sent.
    pub replay: bool,
}
```

user 侧是另一个枚举，没有 opaque：

```rust
// crates/rig-core/src/completion/message.rs:184
pub enum UserContent {
    Text(Text), ToolResult(ToolResult), Image(Image),
    Audio(Audio), Video(Video), Document(DocumentContent),
}
```

### 2.1 `Opaque` 实际被构造的位置

| 位置 | 触发条件 | `replay` |
|---|---|---|
| `providers/anthropic/streaming.rs:347` | block `type == "fallback"`（模型中途 fallback，rig 表达不了） | `false`（保留但不回放） |
| `providers/anthropic/streaming.rs:383` | `_ =>` 未知 block type | `true` |
| `providers/anthropic/streaming.rs:583` | 整个 item 无法规范化（`out.whole(index, Block::Opaque{..}, item, "")`） | `true` |
| `providers/cohere/streaming.rs:141` | 未知 block | `true` |
| `providers/gemini/interactions_api/streaming.rs:208` | content type 不是 `text` / `image` | `true` |
| `crates/rig-bedrock/src/streaming.rs:127` | Bedrock 未知 block（`Block::Opaque { replay }`） | 由上层定 |

### 2.2 hosted tool 与 `Opaque` 的关系

`crates/rig-core/src/completion/history.rs:329-400` 专门处理「服务端执行的工具」
（hosted tools）：`hosted_pair()` 把 `Opaque.item` 认成 hosted tool 的 use 或 result，
`hosted_needs_tools()` 决定这次请求要不要声明 tools。注释原文：

```
/// The hosted-tool pair this opaque item belongs to: whether it is the
/// ... (client call the hosted tool made) replays alone.
```

即：**hosted tool 没有客户端 `ToolResult` 可填，整块只能存成 `Opaque`。**

### 2.3 枚举上有哪些方法

`impl AssistantContent`（`message.rs:219` 起）：

- 构造：`text(..)`、`tool_call(..)`、`reasoning(..)`
- 规范化：`canonical()`、`into_canonical()`
- 判定：`is_answer()`、`is_blank()`
- 指纹 / 原生：`fingerprint()`、`with_native(..)`、`native_item()`

`impl UserContent`：`text(..)`、`tool_result(..)`。

## 3. genai

```rust
// src/chat/content_part/common.rs:13
pub enum ContentPart {
    Text(String),
    Binary(Binary),                 // 图片 / PDF
    ToolCall(ToolCall),
    ToolResponse(ToolResponse),
    ThoughtSignature(String),
    ReasoningContent(String),
    Custom(CustomPart),
}
```

```rust
// src/chat/content_part/custom_part.rs:11
pub struct CustomPart {
    pub model_iden: Option<ModelIden>,   // 哪个 adapter / 模型产的
    pub data: Value,                     // 原始 provider 块
}
```

### 3.1 `Custom` 实际被构造的位置

| 位置 | 触发条件 |
|---|---|
| `adapter/adapters/anthropic/streamer.rs:183` | 流里出现 `redacted_thinking`，且 `capture_reasoning_content` 开启 → 整块存成 `Custom{data: block}` |
| `adapter/adapters/openai/streamer.rs:327` | OpenRouter 的 `reasoning_details` → `Custom`，下一轮再还原 |

### 3.2 回写（发送时）

- `adapter/adapters/anthropic/adapter_shared.rs:249 / 363 / 399`：直接把
  `custom_part.data` push 回请求的 `content` 数组。
- `adapter/adapters/openai/adapter_shared.rs:456`：`is_reasoning_detail(&custom)`
  → 还原成 `reasoning_details` 字段。

### 3.3 `ThoughtSignature` 的来源

- Anthropic：`adapter/adapters/anthropic/adapter_shared.rs:676`
- Bedrock：`adapter/adapters/bedrock/converse.rs:162`（redacted）、`:166`（signature）
- Gemini wire 上的 `thoughtSignature` 在 `adapter/adapters/gemini/adapter_impl.rs:338`
  读出。

### 3.4 枚举上有哪些方法

`impl ContentPart`（`common.rs:40` 起），约 30 个：

- 构造：`from_text`、`from_binary_base64`、`from_binary_url`、`from_binary_file`、`from_custom`
- 视图：每个变体一对 `as_xxx() -> Option<&T>` / `into_xxx() -> Option<T>`
  （text / tool_call / tool_response / binary / thought_signature / reasoning_content / custom）
- 判定：`is_text`、`is_binary`、`is_image`、`is_audio`、`is_pdf`、`is_video`、
  `is_tool_call`、`is_tool_response`、`is_thought_signature`、`is_reasoning_content`、`is_custom`
- 体积：`size()`
- 另有 `#[derive(From)]`（derive_more）为部分变体生成 `From`。

## 4. TanStack AI

没有 opaque 变体，`MessagePart` 是固定联合：

```ts
// packages/ai/src/types.ts:677
export type MessagePart<TData = unknown> =
  | TextPart | ImagePart | AudioPart | VideoPart | DocumentPart
  | ToolCallPart | ToolResultPart | ThinkingPart
  | ActivityPart | StructuredOutputPart<TData> | UIResourcePart | SubagentPart
```

私有负载是**字段级**的：

```ts
// packages/ai/src/types.ts:541
export interface ThinkingPart {
  type: 'thinking'
  content: string
  stepId?: string
  /** The provider's opaque reasoning artefact, sent back unchanged: an
   *  Anthropic signature, Anthropic redacted data, or OpenAI encrypted content. */
  signature?: string
  /** The provider encrypted this thinking block (Anthropic `redacted_thinking`). */
  redacted?: boolean
}
```

另外：

- 每个 part 都有 `metadata?: TMetadata`（如 `TextPart` `:491`、`ToolCallPart` `:497`）。
- 消息级 `metadata.tanstack`（`:700+`）：`source{provider,api,model}`、
  `responseItems[{id,phase}]`、`toolCallMetadata`（如 Gemini thoughtSignature）、
  `signature`。

构造点：`packages/ai-anthropic/src/adapters/text.ts:1517`（`thinking.signature`）、
`:1503-1510`（`redacted` → 发 `redacted_thinking` 块）。

### 4.1 枚举上有哪些方法

没有。`MessagePart` 是纯 TS 联合类型，part 是普通 interface，没有 per-part 方法。
相关操作是 `activities/chat/messages.ts` 里的自由函数：
`aguiContentToContentParts`、`uiMessageToModelMessages`、`modelMessageToUIMessage`、
`subagentHistoryText` 等。

## 5. 同一件事在四家里的落点

| 场景 | Zed | rig | genai | TanStack AI |
|---|---|---|---|---|
| 文本 | `Text` | `Text` | `Text` | `TextPart` |
| 思考 | `Thinking{text,signature}` | `Reasoning` | `ReasoningContent` | `ThinkingPart{content}` |
| 思考签名 | `Thinking.signature` | `Reasoning` 内（+ 块级 `Native`） | `ThoughtSignature(String)` | `ThinkingPart.signature` |
| 加密思考（redacted） | `RedactedThinking(String)` | `Reasoning{redacted:true}` | `Custom(block)` | `ThinkingPart{redacted:true}` |
| 工具调用 / 结果 | `ToolUse` / `ToolResult` | `ToolCall` / `UserContent::ToolResult` | `ToolCall` / `ToolResponse` | `ToolCallPart` / `ToolResultPart` |
| 图片 / 文件 | `Image` | `Image` / `Document` | `Binary` | `ImagePart` / `DocumentPart` |
| **未知 provider 块** | **无** | **`Opaque`** | **`Custom`** | **无** |
| hosted tool 步骤 | 无（走普通 `ToolUse` 语义） | `Opaque` + `history.rs` 配对 | `Custom` | 无对应概念 |
| provider 原生压缩 | `Compaction(ProviderState)` | `Opaque`（注释写 "compaction record"） | 无 | 无 |
| 消息级裸 JSON | `reasoning_details: Option<Arc<Value>>` | 块级 `Native` | `extra_body` | `metadata.tanstack` |

### 5.1 枚举上有没有方法

| | 具名构造函数 | 访问器 / 判定 | 其他 |
|---|---|---|---|
| Zed | 无（只有 `From<String>` / `From<&str>`） | `is_empty()` | `ProviderCompactionState::new`、消息上的 `string_contents` / `contents_empty` |
| rig | `text` / `tool_call` / `reasoning`（Assistant）、`text` / `tool_result`（User） | `is_answer()`、`is_blank()` | `canonical` / `into_canonical` / `fingerprint` / `with_native` / `native_item` |
| genai | `from_text` / `from_binary_*` / `from_custom` | `as_*` / `into_*` 各 7 对、`is_*` 11 个 | `size()`、`derive_more::From` |
| TanStack AI | 无（纯类型） | 无 | 转换逻辑是自由函数 |

## 6. 我们的现状（`request::message::ContentPart`）

```rust
pub enum ContentPart {
    Text(Arc<str>),
    Reasoning { text: Arc<str>, signature: Option<Arc<str>> },
    RedactedThinking(Arc<str>),                                  // 0005 新增
    ToolCall { id: Arc<str>, name: Arc<str>, input: ToolInput },
    ToolResult { call_id: Arc<str>, content: Box<[ToolResultPart]>, is_error: bool },
    Image(Arc<Image>),
    Opaque(Arc<ProviderState>),
}

pub struct ProviderState {
    pub provider: Arc<str>,
    pub format: Arc<str>,
    pub payload: Box<serde_json::Value>,
}
```

对位关系（只陈述事实）：

- `Opaque` 这个**变体名**只在 rig 有（`AssistantContent::Opaque`）；Zed / genai / TanStack
  的消息内容里没有同名变体。
- `{provider, format, payload}` 这个**字段三元组**来自 Zed 的 `ProviderCompactionState`
  （`:296`），但 Zed 只把它用在压缩段，不放在 `MessageContent` 里。
- genai 的 `CustomPart` 是 `{model_iden, data}`（没有 `format`）。
- rig 的 `Opaque` 是 `{item, replay}`（没有 provider / format，多了 `replay`）。
- 四家里没有任何一家用 `Arc<..>` 包这个结构。
- `RedactedThinking` 对位：Zed 有同名专用变体；rig 并进 `Reasoning { redacted }` + 块级
  `Native`；genai 是 `Custom`；TanStack 是 `ThinkingPart { redacted: true }`。
  （取舍见 [0005](../decisions/0005-content-part-variants.md)）

枚举上的方法（我们现在的）：

| 方法 | 作用 | 上游对应 |
|---|---|---|
| `ContentPart::text(..)` | 构造 `Text` | rig `AssistantContent::text` / genai `from_text` |
| `ContentPart::tool_call(..)` | 构造 `ToolCall` | rig `AssistantContent::tool_call` |
| `ContentPart::tool_result(..)` | 构造 `ToolResult` | genai 无；rig 在 `UserContent` 上 |
| `ToolResultPart::from(String / &str / Arc<str>)` | 把工具输出转成结果段 | — |
| `ToolInput::as_value` / `to_wire_string` | 取 JSON / 转 wire 字符串 | — |

Zed 的 canonical `MessageContent` **有** `impl`，但只有 `is_empty()` + 两个 `From`，
没有具名构造器（变体字段是 `pub`，provider 直接构造）。rig / genai 都有具名构造器。

## 7. `Opaque` 什么时候用得上（按源码构造点归纳）

只有五类，且都满足同一个前提：**provider 回了一块内容，你的 IR 没有对应语义，
但下一次请求必须原样带回去（或明确不带）。**

1. **hosted tool 步骤**——服务端执行的工具（web search / code interpreter / file search /
   MCP），客户端没有 `ToolResult` 可以回填。rig：`history.rs:329-400`。
2. **加密思考**——Anthropic `redacted_thinking` 的整块密文；OpenAI Responses
   `reasoning` item 的 `encrypted_content`。四家都有对应落点（见 §5）。
3. **未知 block type**——adapter 不认识、但历史里必须保留的块。rig：`anthropic/streaming.rs:383`
   的 `_ =>` 分支。
4. **provider 原生压缩记录**——加密的压缩状态。Zed：`ProviderCompactionState`。
5. **保留但不回放的块**——例如 Anthropic 中途 fallback。rig：`replay: false`。

对照我们 v0 的形状（Chat Completions + 客户端执行的工具）：

- 第 3 类不会出现：chat completions 的 `role` / `content` 是闭集，未知字段直接丢。
- 第 1 类不会出现：我们不做 hosted tools。
- 第 5 类不会出现：chat completions 没有 fallback 块。
- 第 2 类已经有专用变体 `RedactedThinking(Arc<str>)`，不再需要 `Opaque` 兜底；
  第 4 类（原生压缩）按 [0005](../decisions/0005-content-part-variants.md) 走
  Segment 层，也不进 `Opaque`。
- 也就是说：**在当前的 Chat Completions + 客户端工具路径上，`Opaque` 不会被构造。**
  它是给「接 hosted tools 或 provider 私有块」预留的槽。
