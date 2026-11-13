# Zed 的 LLM 请求体结构体与多提供商适配

> 调研日期：2026-11-12
> 源码：`/home/aa/repos/ide_ls/learn_ls/zed`
> 结论：Zed 用一套**提供商无关的内部请求模型**（语义 IR）+ 每个提供商 crate 自己定义 wire 请求体，二者通过 `into_*` 转换函数与事件 mapper 双向适配。OpenAI 兼容只是其中一种适配，不是基类。
>
> 相关决策：[0001 IR 分层与命名](../decisions/0001-ir-layering.md)、[0002 内存优先](../decisions/0002-memory-first.md)。

## 1. 分层

```
调用方（agent / thread）
   │  LanguageModelRequest（语义 IR）
   ▼
LanguageModelClient::stream_completion(model, request)   ← trait，公共契约
   │
   ├─ provider 适配器（language_models/src/provider/*.rs）
   │     into_xxx(...)          → 各厂商 wire Request
   │     xxx::stream_completion → HTTP / SDK 调用
   │     XxxEventMapper         → 统一事件流
   ▼
LanguageModelCompletionStream<LanguageModelCompletionEvent>
```

两个关键 trait：

- `LanguageModelClient` — `crates/language_model/src/language_model.rs:419`
  ```rust
  fn stream_completion(
      &self,
      model: &LanguageModel,
      request: LanguageModelRequest,
      cx: &AsyncApp,
  ) -> BoxFuture<'static, Result<LanguageModelCompletionStream, LanguageModelCompletionError>>;
  ```
  注意 `request` 是**按值传入**，不是 `&request`。
- `LanguageModelProvider: LanguageModelClient` — `crates/language_model/src/language_model.rs:568`
  负责 id / name / 模型列表 / 鉴权 / 设置视图等。

跨提供商的公共契约是「`LanguageModelRequest` → 事件流」，HTTP body 完全关在 provider 内部。

## 2. 内部请求模型（provider-neutral）

`crates/language_model_core/src/request.rs:470`

```rust
pub struct LanguageModelRequest {
    pub thread_id: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub prompt_id: Option<String>,
    pub intent: Option<CompletionIntent>,
    pub messages: Vec<LanguageModelRequestMessage>,
    pub tools: Vec<LanguageModelRequestTool>,
    pub tool_choice: Option<LanguageModelToolChoice>,
    pub stop: Vec<String>,
    pub temperature: Option<f32>,
    pub thinking_allowed: bool,
    pub thinking_effort: Option<String>,
    pub speed: Option<Speed>,
    pub compact_at_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}
```

消息内容是**语义枚举**，不是任何厂商的 JSON 形状（`request.rs:263`）：

```rust
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

要点：

- **思考内容有签名**：`Thinking { signature }` 用于把 Anthropic / Gemini 的 thinking 原文 + 签名回放给服务端，否则部分模型会拒绝。
- **压缩状态是厂商私有 payload**：`CompactedContext::ProviderState(ProviderCompactionState)`（`request.rs:277-326`），里面是 `provider_id + format + payload: Arc<str>`，只有对应 provider 能解读。这是统一模型里显式留出的「厂商私有槽」。
- **工具定义两种形态**（`request.rs:420`）：
  ```rust
  pub enum LanguageModelRequestToolInput {
      Function { input_schema: serde_json::Value, use_input_streaming: bool },
      Custom { format: Option<LanguageModelCustomToolFormat> }, // Text | Grammar { Lark | Regex }
  }
  ```
  `Custom` 用来表达 Anthropic custom tools 之类的非 JSON-schema 形式。

`LanguageModelRequest` 本身派生了 `Serialize/Deserialize`，但**不是**直接发给服务端的东西，主要用于内部记录 / 回放。

## 3. 各提供商自己的 wire 请求体

| 提供商 | wire 类型 | 位置 | 特点 |
|---|---|---|---|
| OpenAI Chat Completions | `Request` | `open_ai/src/open_ai.rs:735` | `messages/stream/stream_options/tools/reasoning_effort/prompt_cache_key/service_tier` |
| OpenAI Responses API | `responses::Request` | `open_ai/src/responses.rs` | 另一套完整协议 |
| Anthropic | `Request`、`StreamingRequest` | `anthropic/src/anthropic.rs:992`、`:1104` | `system` 是顶层；`thinking`、`cache_control`、`context_management`、`stop_sequences`；流式版用 `#[serde(flatten)] base + stream: bool` |
| Google Gemini | `GenerateContentRequest` | `google_ai/src/google_ai.rs:482` | camelCase；`contents: Vec<Content{parts}>`；`systemInstruction`；`generationConfig`；URL 是 `...:streamGenerateContent?alt=sse&key=...`（`google_ai.rs:212`） |
| Ollama | `ChatRequest` | `ollama/src/ollama.rs:130` | `keep_alive`、`options { num_ctx, num_predict, ... }`、`think` |
| DeepSeek | `Request` | `deepseek/src/deepseek.rs:121` | 看着像 OpenAI，但 `thinking: { type: enabled/disabled }`、`reasoning_effort: low/high/max` 是自家字段 |
| AWS Bedrock | `Request` | `bedrock/src/bedrock.rs:286` | **没有 JSON body**：走 AWS SDK `converse_stream(...)` builder；thinking 参数塞进 `additional_model_request_fields: Document`（`bedrock.rs:40-52`） |

Bedrock 是最能说明设计意图的例子：上层完全不关心「请求体是 HTTP JSON 还是 SDK 调用」。

### 3.1 为什么 DeepSeek 不直接复用 `open_ai::Request`？

因为它只是「大体兼容」：思考开关是 `thinking.type = enabled/disabled`，`reasoning_effort` 是 `low/high/max`，还有 `response_format` 等差异。Zed 选择复制一份本地近似结构体（`deepseek/src/deepseek.rs:121`），避免为少数差异污染通用 `open_ai::Request`。

## 4. 转换函数：语义 IR → 各家 body

- `into_open_ai` — `open_ai/src/completion.rs:63`
- `into_open_ai_response` — `open_ai/src/completion.rs:267`（Responses API）
- `into_anthropic` — `anthropic/src/completion.rs:284`
- `into_google` — `google_ai/src/completion.rs:19`
- `into_bedrock` — `language_models/src/provider/bedrock.rs:2114`

以 `into_anthropic` 为例（`anthropic/src/completion.rs:284`）：

1. 遍历 `LanguageModelRequestMessage`；
2. `MessageContent` 逐个映射成 Anthropic 的 `RequestContent`；
3. `Role::System` 抽出来按 `\n\n` 拼进顶层 `system`；
4. 相邻同角色消息合并成一条；
5. 处理 `cache` / `cache_control`（Legacy vs Automatic 两种模式）；
6. 产出 `anthropic::Request`。

Google 的转换同样把 `MessageContent::Thinking { signature }` 映射成 `TextPart { thought: true, thought_signature }`，图片映射成 `inlineData`，tool result 映射成 `functionResponse`。

转换后适配器还可以按模型能力裁剪字段，例如：

```rust
// language_models/src/provider/anthropic.rs:415
if !config.supports_speed {
    request.speed = None;
}
```

## 5. 响应侧归一化

每个 provider 自带事件 mapper，把自家流式响应映射成统一的 `LanguageModelCompletionEvent`：

- `Text` / `Thinking` / `RedactedThinking` / `ReasoningDetails`
- `ToolUse` / `ToolUseJsonParseError`
- `UsageUpdate` / `Stop` / `StartMessage` / `Compaction`

相关类型：

- `ChatCompletionEventMapper` — `language_model_core/src/chat_completion.rs`（OpenAI 兼容系共用）
- `AnthropicEventMapper` — `anthropic/src/completion.rs`
- Google / Ollama / Bedrock 同理

`language_model_core/src/chat_completion.rs` 的注释明确说明：这些 wire 类型故意写得很宽松（字段尽量 `Option` / `Default`），因为现实里的「OpenAI 兼容」提供商经常漏字段或发 `null`。

## 6. 适配器与注册

适配器都在 `crates/language_models/src/provider/`：

```
anthropic.rs            open_ai.rs
anthropic_compatible.rs open_ai_compatible.rs
api_compatible.rs       bedrock.rs
google.rs               ollama.rs
deepseek.rs             open_router.rs
llama_cpp.rs            lmstudio.rs
mistral.rs              copilot_chat.rs
x_ai.rs                 vercel_ai_gateway.rs
opencode.rs             cloud.rs
```

适配器内部流程：

```
LanguageModelRequest
  → into_xxx(...)                  // 转成 wire Request
  → xxx::stream_completion(...)    // 发请求（HTTP 或 AWS SDK）
  → XxxEventMapper::map_stream(..) // 映射成统一事件流
```

注册在 `crates/language_models/src/language_models.rs:235` 起：

```rust
registry.register_provider(Arc::new(AnthropicLanguageModelProvider::new(...)), cx);
registry.register_provider(Arc::new(OpenAiLanguageModelProvider::new(...)), cx);
registry.register_provider(Arc::new(GoogleLanguageModelProvider::new(...)), cx);
registry.register_provider(Arc::new(BedrockLanguageModelProvider::new(...)), cx);
...
```

上层只按 `LanguageModelProviderId` 从 `LanguageModelRegistry` 找 provider，不关心具体实现。

## 7. 「OpenAI 兼容」是怎么复用的

`crates/language_models/src/provider/open_ai_compatible.rs` 的 `OpenAiCompatibleLanguageModelProvider` 是**参数化**的：

- `id`、`api_url`、settings、可用模型都来自配置；
- wire 层直接复用 `open_ai` crate 的 `Request` + `into_open_ai`；
- 配置在 `settings.json` 的 `openai_compatible` 下（见 `language_models/src/settings.rs` 的 `OpenAiCompatibleModelCapabilities`）。

同理有 `anthropic_compatible.rs`。

## 8. 结论：支持非 OpenAI 格式的两条路

1. **自带适配器（主力路径）**
   为它新建 crate（如 `google_ai`、`anthropic`、`bedrock`），定义自己的 wire body + event mapper，实现 `LanguageModelClient` / `LanguageModelProvider`。
2. **复用某家兼容层**
   `open_ai_compatible` / `anthropic_compatible`，前提是 API 真的兼容。

对于「大体兼容但字段有差异」的（DeepSeek、xAI），Zed 选择复制本地结构体，而不是硬套通用 `open_ai::Request`。

## 9. 对我们的含义（fork / 自研）

- 可以直接复用这层设计：`LanguageModelRequest` 作为 `aa_ai` 的语义 IR，agent loop 只依赖它和事件流。
- 新增提供商时不需要动 agent loop，只需实现 `LanguageModelClient` + 一个 `into_*` 和事件 mapper。
- 如果要做「自定义 HTTP 协议」，参考 Bedrock 的做法：`Request` 只是普通 Rust 结构体，发送方式（HTTP / SDK / CLI）完全自由。
- `ProviderCompactionState`、`Thinking.signature`、`Custom tool` 这几个「私有槽」值得保留，否则接入 Claude / Gemini 时会丢信息。

## 10. 与 genai / rig 的代码核对（2026-11-12）

> 对照仓库（本地已克隆）：
> - `/home/aa/repos/lib_ls/learn_ls/genai` — `genai` v0.7.5，commit `d852bfd`
> - `/home/aa/repos/lib_ls/learn_ls/rig` — commit `6e13e8c`（浅克隆）

### 10.1 先修正之前的三个判断

**① 「genai 的 IR 更接近 OpenAI chat 形状」——只对了一半。**

`ChatRequest` 确实有 OpenAI 味的 `system: Option<String>`（`src/chat/chat_request.rs:11`），但消息内容是语义化的：

```rust
// src/chat/content_part/common.rs:13
pub enum ContentPart {
    Text(String),
    Binary(Binary),
    ToolCall(ToolCall),
    ToolResponse(ToolResponse),
    ThoughtSignature(String),
    ReasoningContent(String),
    Custom(CustomPart),
}
```

并且 `ChatOptions` 有 `extra_body: Option<Value>`、`raw_frame_sink`、`cache_control`、`prompt_cache_key`（`src/chat/chat_options.rs:28-121`），`CustomPart` 带 `model_iden`（`src/chat/content_part/custom_part.rs:11`）。它并不薄，只是把「系统提示」「采样参数」放在 request/options 而不是消息块里。

**② 「genai 会丢 thinking 签名」——错。**

`ContentPart::ThoughtSignature` + `ReasoningContent` 会随请求回放；`with_capture_reasoning_content` 还会捕获签名，OpenRouter 的 `reasoning_details` 被当作 `Custom` part 原样保留（`src/adapter/adapters/openai/adapter_impl.rs:105-112`）。真正的缺口是**服务端压缩状态**：genai 没有 Zed 的 `ProviderCompactionState` 这一层（grep 无对应概念），只有 `previous_response_id` / `store` 这种会话续接。

**③ 「rig 的核心是 `CompletionModel` trait」——已过时。**

当前 rig 换成了数据导向的 `Wire` + `Transport` 架构（`crates/rig-core/src/lib.rs`、`wire.rs`、`driver.rs`）：

```rust
// crates/rig-core/src/wire.rs:755
pub trait Wire: Clone + Send + Sync + 'static {
    type Op: Operation;
    type Payload;
    type Frame;
    type Event;
    type Decoder<'id>: Decoder<'id, Self::Op, Self::Frame, Event = Self::Event>;
    type Reassembler: Reassemble<Self::Event> + Serves<Self::Op>;

    fn describe(&self) -> Descriptor<'_>;
    fn encode(&self, request: Request<Self>, mode: Mode) -> Result<Self::Payload, EncodeError>;
    fn decoder<'id>(&self) -> Self::Decoder<'id>;
    fn reassembler(&self) -> Self::Reassembler;
}
```

`Model<W, T>` 把「发什么/怎么读」(`Wire`) 和「怎么传输」(`Transport`) 分开；`Operation` 声明输入/事件/结束/响应和 `Fold`，同一个 decoder 同时服务 unary 与 stream（`crates/rig-core/src/driver.rs`、`operation.rs`）。`DynModel` 只做按 operation 的类型擦除。

消息 IR 也比 Zed 更深（`crates/rig-core/src/completion/message.rs:206`）：

```rust
pub enum AssistantContent {
    Text(Text),
    ToolCall(ToolCall),
    Reasoning(Reasoning),
    Image(Image),
    Opaque(Opaque),   // 规范化不了的原生块
}
```

外加 `AssistantMessage.origin: Option<Origin>`（哪条 wire/provider/model 产的）、`Native`、`canonical()`（剥掉原生字段供跨提供商回放）。这比 Zed 的消息级 `reasoning_details: Option<Arc<Value>>` 更细。

### 10.2 三者的共同骨架

三套代码在一点上完全一致（说明这是收敛解）：

```
语义 IR（message/tool/thinking）
  → 每提供商一个 wire 层（请求体 + 解码器）
  → 统一事件流（Text/Thinking/ToolCall/Usage/Stop/Error）
```

差异只在四个维度：

| 维度 | Zed | genai | rig |
|---|---|---|---|
| 调度方式 | `Arc<dyn Provider>` + `BoxFuture`（每请求一次虚调用） | `AdapterKind` enum + `dispatch_adapter!` 宏，静态分派，adapter 全静态函数 | 泛型 `Model<W,T>`，`DynModel` 按需擦除 |
| IR 私有槽 | 消息级 `reasoning_details`、`ProviderCompactionState`、工具 `Custom{format}` | `ContentPart::{ThoughtSignature,ReasoningContent,Custom}` + `extra_body` + `raw_frame_sink` | 每块 `Native`/`Opaque` + `Origin` + `canonical()` |
| 能力/不支持项处理 | 每模型 `supports_*` 布尔 + 适配器手工裁剪 | 适配器内按 `AdapterKind` / 模型名 match，无统一策略 | `catalog::ModelSpec::refusals()` + `OnUnsupported::{Error,Ignore}` + `fit()` |
| 传输解耦 | 弱：`stream_completion` 里直接发 HTTP / AWS SDK | 中：`WebClient` 抽象，但 `to_web_request_data` 产出 HTTP | 强：`Wire::encode` 纯函数，decoder 不认识传输；`decode_reply` 可离线解码 |
| 错误归一化 | `LanguageModelCompletionError` + 语义分类 | `Error::ChatResponse{body}` 等 | `ProviderError` + `ErrorReport` |
| Agent 循环 | 在 `crates/agent` 的 `Thread`（另一个 crate） | 不含 | `rig-agent`：`Agent`/`AgentRunner`，hooks/流式/持久化 |
| 可观测/回放 | telemetry + 各 provider 字段 | `raw_frame_sink`、`capture_*` | `Reassembler` 重建原始 provider 文档、`Origin`/`Fingerprint`、`recorder` |

### 10.3 「耦合所以性能更好」成立吗？

**不成立。准确说法是：耦合换来的是控制力，不是吞吐。**

1. **延迟被网络主导。** 一次请求的 CPU 成本（IR→JSON 序列化）通常在亚毫秒到几毫秒；TTFT 和逐 token 生成是几百毫秒到几十秒。三者的调度差异（虚调用 vs enum match vs 泛型）是噪声。
2. **真正影响 CPU 的是流式解析和每 chunk 分配，而三家差不多。**
   - Zed：`serde` 直接反序列化 typed chunk，映射成事件；
   - genai：`EventSourceStream` + `serde_json::Value` + `x_take` 提取（`src/adapter/adapters/openai/streamer.rs`），每 chunk 有 `Value` 开销；
   - rig：decoder 之外还要 `Reassembler` 重建原始文档、投射观测数据（`wire.rs` 的 `Encoded`），**每 token 做的活最多**，换来回放/telemetry。
   如果只比「每 token 开销」，Zed 的薄映射大概率最低，rig 最高；差距仍是网络噪声量级。
3. **Zed 的「耦合」其实是应用内聚，不是代码耦合。** `language_model_core` / `language_model` / `language_models` / 各 provider crate 边界很清楚，Bedrock 这种非 HTTP 提供商也能接。它耦合的是**应用需求**（GPUI 的 `AsyncApp`、后台执行器、UI 需要的 `Queued/Started/Compaction` 事件）和**单一消费者假设**。
4. **应用身份带来的真实优势**（这些才是该抄的）：
   - IR 可以很深，不用为未知库用户保留扩展点和 semver 兼容；
   - Rust enum 穷尽匹配强制每个适配器处理新语义（genai/rig 也这么做，但它们是库，必须更保守）；
   - 可以按 UI 需要裁剪事件（`Queued`、`Compaction::SummaryDelta`）；
   - 可以在适配器里直接 `if !supports_speed { request.speed = None }`，不需要设计通用 `OnUnsupported` 策略。
5. **应用身份的代价**：`cx: &AsyncApp` 绑死 GPUI；`LanguageModelRequest` 里混入 `thread_id`/`prompt_id`/`intent` 这类应用概念；没有可独立复用的 `aa_ai` core。

### 10.4 实现多提供商的建议（按优先级）

1. **先定语义 IR，但只加你真正需要的变体。**
   消息块至少：`Text` / `Reasoning{text,signature}` / `Image` / `ToolCall` / `ToolResult` / 原生块。用 Rust enum，让编译器逼所有适配器处理新变体（Zed 的最大优势）。

2. **原生/私有数据挂在「块」上，不要挂在全局。**
   抄 rig 的 `Native { provider, model, api, payload }`，比 Zed 的 `reasoning_details: Option<Value>` 和 genai 的 `CustomPart` 更可诊断；回放时用 `canonical()` 剥掉原生字段给别的提供商。

3. **能力检查要显式，不要靠适配器静默丢字段。**
   rig 的 `ModelSpec::refusals()` + `OnUnsupported` 是最完整的做法：请求发出前就能列出「这个模型不支持 temperature/tools/reasoning」，并让调用方选 `Error` 或 `Ignore`。Zed 的 `supports_*` 布尔 + 手工裁剪更简单，但用户看到的是「悄悄没生效」。建议：**每个模型一份能力表，适配器只做映射，不做策略。**

4. **`Wire` 与 `Transport` 分离。**
   抄 rig 的 `encode(&self, request, mode) -> Payload` 纯函数 + `decoder() -> Decoder`，这样：
   - 同一份解码逻辑同时服务 unary / SSE；
   - 可以离线解码录制文件做测试/回放；
   - 接 Bedrock 这种 SDK 提供商时，只需换 `Transport`。
   比 Zed 把 HTTP 调用写进 `stream_completion` 更可测。

5. **统一事件流要包含「增量」和「终态」两类。**
   至少：`Start`、`TextDelta`、`ReasoningDelta{signature?}`、`ToolCallDelta`、`Usage`、`Stop{reason}`、`Error{retryable,retry_after,provider_code}`、`Heartbeat`。工具参数增量合并是最容易出 bug 的地方（genai 用 `ToolCallSlots` 处理缺 `index` 的兼容服务器，值得参考）。

6. **兼容提供商用「命名空间 + 配置」而不是复制类型。**
   genai 的 `genai_{n}::model` + `GENAI_{n}_ENDPOINT` + `extra_body`，和 Zed 的 `openai_compatible` 设置是同一思路：base_url + api_key + models + headers + capabilities。对字段差异大的（DeepSeek 的 `thinking.type`），再做专用适配器。

7. **请求与历史用 `Arc` 共享。**
   rig 的 `Vec<Arc<Message>>` 让「记录/重试/多消费者」只复制指针；Zed 用 `SharedString` 处理 id/图片。长会话下这比 `Vec<Message>` 的深拷贝省很多。

8. **保留原始帧/原始响应。**
   genai 的 `raw_frame_sink`、Zed 的 `reasoning_details`、rig 的 `raw` 都在解决同一件事：协议之外的字段不能丢，否则回放/调试无解。v1 至少留一个 `raw: Option<Value>` 出口。

9. **错误归一化单独一层。**
   把各家的 `error.type`、HTTP code、retry_after 映射成统一分类（Zed 的 `LanguageModelCompletionError` + 语义分类，rig 的 `ProviderError`）。否则 agent 循环里会塞满 provider 判断。

10. **Agent loop 不要塞进 provider 层。**
    Zed 的拆分（`stream_completion` 一次调用 / `Thread` 循环）对自研更合适；rig 的 `rig-agent` 可以作为循环/工具调度/hook 的参考，但不要因此把 provider 层做成 agent 框架。

### 10.5 一句话总结

三家都收敛到「语义 IR + per-provider wire + 统一事件流」；Zed 的独特之处不是性能，而是**敢把 IR 设计得深、把应用概念直接放进 IR**，因为它只有一个消费者。自研时应该抄它的 IR 深度和枚举穷尽，抄 rig 的 `Wire`/`Transport` 分离与能力拒绝策略，抄 genai 的兼容提供商配置与原始帧逃生口；不要抄它的 GPUI 绑定。

### 10.6 内存优先时的取舍（2026-11-12 补）

吞吐受提供商影响，不是架构重点；内存才是长期 coding agent 的硬约束。按源码核对，三家的内存画像：

| 维度 | Zed | genai | rig |
|---|---|---|---|
| 历史存储 | `Vec<Arc<Message>>`；文本 `String`，tool result 文本 `Arc<str>`，图片 `SharedString` | 调用方拥有 `Vec<ChatMessage>`；文本 `String`，base64 图片 `Arc<str>` | `Vec<Arc<Message>>`、`Vec<Arc<ToolDefinition>>`；图片 base64 是 `String`（靠 `Arc<Message>` 避免复制） |
| 每次请求 | **深拷贝**所有文本/thinking（`agent/src/thread.rs:701,707`）；tool result/图片是引用计数克隆；system prompt 每次重新渲染 | `ChatRequest` 按值传入（`client_impl.rs:90`）；调用方不 move 就要 clone 整个历史 | request 克隆只复制 `Arc` 指针 |
| 回复期峰值 | 请求副本 + JSON body；默认不留 raw | 请求 + JSON body；`capture_*` 默认关 | **始终**用 `Reassembler` 重建完整 raw `Value`（completion wire 不能用 `Unreassembled`，`wire/document.rs:67-97`），`Reply.raw` 恒在（`wire.rs:297`） |
| 流式缓冲 | `async_channel::bounded(32)`（`language_model.rs:25,41`） | 每 chunk `Value` + `pending_events: VecDeque` | `Streamed` + decoder/fold；每事件都过 reassembler |
| 工具参数拼接 | `String::push_str`（均摊） | `format!("{existing}{arguments}")` 每 delta 一次 → O(n²) 拷贝（`openai/streamer.rs`） | `push_str`（`operation/completion.rs:1270`） |
| 压缩 | 有（summary + provider-native） | 无 | 无 |
| 模型目录 | 运行时拉取，按 provider 存 | 适配器内小表 | 内嵌 1.09MB `models.json`，lazy 解析成 owned `Arc<Vec<Entry>>`（`catalog/mod.rs:75,134`） |

**结论：**

- **历史共享 rig 最好**（`Arc<Message>`），Zed 次之（存储共享、请求深拷贝），genai 最差（调用方通常要 clone 一遍）。
- **回复期内存 Zed 最好**（默认不保留 raw），genai 次之（capture 默认关），**rig 最差**（完整 raw 文档 + 每块 `Native.item` 的重复，属于为回放/可观测付的固定成本）。
- **长期稳态 Zed 最好**：只有它有上下文压缩，能真正把历史内存压住。
- **最容易被忽视的放大器**：图片/工具输出这类大块数据。Zed 和 genai 都用 `Arc<str>`/`SharedString` 让请求克隆只加引用；rig 靠 `Arc<Message>` 达到同样效果，但单条消息内的 base64 仍是 `String`。

**自研（aa-ai）内存优先清单，按收益排序：**

1. **历史是唯一真相，`Arc<Message>` 共享**；请求借用 `&[Arc<Message>]` 或只克隆 `Arc`。
2. **边编码边发送**：用 `serde_json::to_writer` 写进可复用的 `Vec<u8>`，发完立即 drop；不要同时长期持有 IR 副本 + JSON body。
3. **大块数据一律 `Arc<str>` / `Arc<[u8]>` / `Bytes`**（图片、工具输出、长文件内容），小字符串才用 `String`。
4. **原始 provider 文档默认不保留**；需要时用有界 ring buffer / 只存增量字段，别学 rig 的无条件 reassemble。
5. **流式用有界通道（~32）+ 背压**；核心库不累积文本，累积交给 UI/调用方。
6. **工具参数用 `String::with_capacity` + `push_str`**，不要每 delta `format!`（genai 的 O(n²) 是反面教材）。
7. **IR 用 typed enum，不用 `serde_json::Value`**；`Native`/私有块按需 `Arc<Value>` 或 boxed，回放时 `canonical()` 剥掉。
8. **第一天就要有压缩**：summary 或 provider-native，避免历史无限增长；压缩后丢掉不再需要的 thinking/签名/旧工具输出。
9. **内容块用 `SmallVec<[_; 2]>` 或 `Box<[T]>`**，避免每条消息一个 `Vec` 堆分配。
10. **模型目录 lazy 且只装活跃模型**；不要把 1MB 级 JSON 无条件解析进常驻内存。
11. **遥测/capture 默认关**，开启时截断/脱敏。
12. **用峰值 RSS + 分配计数验证**，不要用吞吐微基准代替内存测量。

吞吐方面只保留一个真正的架构动作：**流式解析放后台执行器**（Zed 的 `stream_in_background`），避免阻塞 UI；其余解析优化收益远小于网络噪声。
