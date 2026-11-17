# 四家怎么接「自定义 provider」（事实记录）

> 只记录源码事实：类型、函数、注册点、配置形状。「自定义」在本文指**长尾端点**
> （没有专门代码的第三方服务），和「常见厂商」相对；四家的分界线见 §7。
> **不做取舍、不给建议。**
> 行号以本地克隆为准：
> - Zed：`/home/aa/repos/ide_ls/learn_ls/zed`（commit `4ed72e0135`）
> - rig：`/home/aa/repos/lib_ls/learn_ls/rig`（commit `6e13e8c`）
> - genai：`/home/aa/repos/lib_ls/learn_ls/genai`（v0.7.5，commit `d852bfd`）
> - TanStack AI：`/home/aa/repos/lib_ls/learn_ls/ai`（pnpm workspace）

## 0. 三层，不是两件

各家实际都分成三层，混在一起会看不出「区分度」写在哪：

| 层 | 含义 | 例子 |
|---|---|---|
| **协议** | 请求/响应/流式事件的形状 | OpenAI Chat Completions、Responses、Anthropic Messages、Gemini generateContent、Ollama 原生 `/api/chat` |
| **常见厂商** | 协议相同或近似，但有一组**写下来的差异** | OpenAI、DeepSeek、Groq、OpenRouter、Mistral、xAI、Ollama |
| **长尾端点** | 没有专门代码的第三方服务，用户自己提供 URL/key/模型表 | 自建网关、内部代理、刚上线的小厂 |

四家对「常见厂商」的处理差别很大（整份复制 vs 一条数据），对「长尾」则都有单独的降级入口。


---

## 1. Zed

### 1.1 内置 provider：trait + 全局注册表

```rust
// crates/language_model/src/registry.rs:152
pub fn register_provider<T: LanguageModelProvider + LanguageModelProviderState>(
    &mut self,
    provider: Arc<T>,
    cx: &mut Context<Self>,
)
```

`LanguageModelProvider: LanguageModelClient`（`language_model/src/language_model.rs:568`）把
「发请求」和「id / name / icon / authenticate / set_api_key / settings_view / provided_models…」
约 20 个方法绑在同一个 trait 上；内置 provider 在
`language_models/src/language_models.rs:228` 的 `register_language_model_providers` 里逐个
`Arc::new(...)` 注册。**新增内置协议 = 新增一个 struct + 实现整个 trait。**

### 1.2 兼容端点：**设置即数据**，运行时注册

配置形状（`crates/settings_content/src/language_model.rs:26`）：

```rust
pub openai_compatible: Option<HashMap<Arc<str>, OpenAiCompatibleSettingsContent>>,
pub anthropic_compatible: Option<HashMap<Arc<str>, AnthropicCompatibleSettingsContent>>,
```

单个模型的声明（同文件 `:450`、`:463`）：

```rust
pub struct OpenAiCompatibleAvailableModel {
    pub name: String,                 // provider 侧模型名
    pub display_name: Option<String>,
    pub max_tokens: u64,              // 上下文窗口
    pub max_output_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub reasoning_effort: Option<OpenAiReasoningEffort>,
    pub capabilities: OpenAiCompatibleModelCapabilities,
}

pub struct OpenAiCompatibleModelCapabilities {
    pub tools: bool,
    pub images: bool,
    pub parallel_tool_calls: bool,
    pub prompt_cache_key: bool,
    pub chat_completions: bool,          // 默认 true
    pub interleaved_reasoning: bool,
    pub max_tokens_parameter: bool,
}
```

**能力位是用户写在 JSON 里的数据，不是代码里的常量。** 每个条目带自己的
`api_url` + `available_models` + `custom_headers`（`custom_headers` 会过滤掉 Zed 自己管理的
header，见 `language_models/src/provider.rs:32` 的 `resolve_custom_headers`）。

一个 provider 对象由 (id, api_url, models) 参数化，仍然是同一个
`OpenAiCompatibleLanguageModelProvider`（`language_models/src/provider/open_ai_compatible.rs:49`，
构造见 `:58`），wire 仍然复用 `open_ai` crate。

热重载靠 diff（`language_models/src/language_models.rs:154-227`）：

```rust
struct CompatibleProviders(HashMap<Arc<str>, CompatibleProviderKind>); // :154
fn from_settings(cx: &App) -> Self                                     // :163
fn register_compatible_providers(registry, old, new, ...)              // :188
```

- 新旧集合对比：变了的先 `unregister_provider`，再按 kind 重新注册（`:205` / `:214`）；
- 同一 id 同时出现在 `openai_compatible` 和 `anthropic_compatible` 时，**OpenAI 优先**并
  `log::warn!`（`:168-182`）——注册表只有一个 id 命名空间。

### 1.3 扩展（wasm）也能注册

`crates/language_models/src/extension.rs:39`：

```rust
fn register_language_model_provider(
    &self,
    _provider_id: Arc<str>,
    register_fn: LanguageModelProviderRegistration,   // 回调里自己构造 provider
    cx: &mut App,
)
```

扩展可以替换内置 provider：`BUILTIN_TO_EXTENSION_MAP`（`:11`）列出装了哪个扩展就隐藏哪个内置。

### 1.4 新协议怎么加

新 struct 实现 `LanguageModelProvider`（含 UI/auth 方法）+ `LanguageModelClient`，在
`register_language_model_providers` 里加一行 `Arc::new(...)`。没有运行时的「协议描述」层：
协议就是 Rust 代码。

---

## 2. rig

这一版（`6e13e8c`）已经重构成 **wire / transport 分离**，和旧版「每家有 `Client` + `CompletionModel`」
不同。

### 2.1 四个概念

```rust
// crates/rig-core/src/wire.rs:755
pub trait Wire: Clone + Send + Sync + 'static {
    type Op: Operation;
    type Payload;  type Frame;  type Event;
    type Decoder<'id>: Decoder<'id, Self::Op, Self::Frame, Event = Self::Event>;
    type Reassembler: Reassemble<Self::Event> + Serves<Self::Op>;

    fn describe(&self) -> Descriptor<'_>;
    fn encode(&self, request: Request<Self>, mode: Mode) -> Result<Self::Payload, EncodeError>;
    fn decoder<'id>(&self) -> Self::Decoder<'id>;
}
```

- `Wire`：协议即**纯数据**（encode 不碰 socket，decoder 不知道字节怎么来的，
  攒齐的和流式的走同一条解码路径）；
- `Transport<W>`（`driver.rs:214`）：字节怎么传；
- `Model<W, T>`（`driver.rs:54`）：`wire + transport` 两半都 pub，可以换任一；
- `Catalog`（`catalog/mod.rs:89`）：`vendor/model` 引用 → 该模型该用哪个协议族 + 模型事实。

### 2.2 兼容端点：`Dialect` 常量 + 覆盖 base_url

OpenAI 形状的厂商不是各写一份 wire，而是**同一套 wire + 不同的 `Dialect`**
（`providers/openai/wire.rs:524`）：

```rust
pub struct Dialect {
    pub name: &'static str,
    pub base_url: &'static str,
    pub api_key_env: &'static str,
    pub base_url_env: Option<&'static str>,
    pub request_id_header: Option<&'static str>,
    pub alternate_auth: Option<AuthAlternative>,
    pub quirks: Quirks,                 // hooks / auth / routing / completion_route / paths...
}
```

`providers/openai/wire/dialects.rs` 里是一长串 `pub const`：`OPENAI`（`:32`）、`AZURE`、
`DEEPSEEK`、`GROQ`、`OLLAMA`、`LLAMACPP`、`OPENROUTER`、`VLLM` 类网关……

公开的自定义入口（`providers/openai/wire.rs:543`）：

```rust
pub const fn gateway(name: &'static str, base_url: &'static str, api_key_env: &'static str) -> Self
pub const fn with_quirks(mut self, quirks: Quirks) -> Self
```

但 `Dialect` 的 `Serialize` 会拒绝「未注册或改过」的定义（`:571-586`）：序列化只存名字，
反序列化从**本 build 的注册表**查回常量。也就是说 dialect 必须在编译期存在。

运行期能改的是 URL（`providers/registry.rs:862`）：

```rust
pub fn with_base_url(self, base_url: impl Into<String>) -> Self
```

以及凭证/身份：`ProviderConfig`（`registry.rs:783`）是可序列化的三选一
（OpenAi / Anthropic / Gemini），`Secret` 不落盘，`with_credential` 重新注入。

### 2.3 自定义模型事实

`Catalog` 支持 `with_overrides`（`catalog/mod.rs:94` 的 `Entry` 注释），可以覆盖模型行的
规格/价格；引用不一定来自内置表。

### 2.4 新协议怎么加

实现 `Wire`（+ `Operation` + `Decoder` + `Reassembler`），再给一个 `*Config` 实现
`connect(http) -> Model<W>`，并把变体加进 `ProviderConfig` / `Catalog` 的行。
Anthropic 与 Gemini 各自走这条路（`providers/anthropic`、`providers/gemini`）。

---

## 3. genai

### 3.1 枚举 + 静态方法 trait + 宏分发

```rust
// src/adapter/adapter_types.rs:11
pub trait Adapter {
    const DEFAULT_API_KEY_ENV_NAME: Option<&'static str>;
    fn default_auth(kind: AdapterKind) -> AuthData;
    fn default_endpoint(_kind: AdapterKind) -> Endpoint;
    async fn all_model_names(kind, endpoint, auth, web_client) -> Result<Vec<String>>;
    fn get_service_url(model_iden, service_type, endpoint) -> Result<String>;
    fn to_web_request_data(target, service_type, chat_req, options) -> Result<WebRequestData>;
    fn to_chat_response(model_iden, web_response, options) -> Result<ChatResponse>;
    fn to_chat_stream(model_iden, reqwest_builder, options) -> Result<ChatStreamResponse>;
}
```

`AdapterKind`（`src/adapter/adapter_kind.rs:15` 起）是约 30 个变体的枚举；
`AdapterDispatcher`（`src/adapter/dispatcher.rs:11`）用 `dispatch_adapter!` 宏把
`kind` 分发到对应实现。trait 的方法全是**静态方法**，因为同一个 adapter 要服务多个 kind。

模型名决定 kind：`AdapterKind::from_model_namespace`（`adapter_kind.rs:312`），
`provider::model` 形式，例如 `groq::llama-3.3-70b`；无 namespace 时有 fallback（Ollama）。

### 3.2 兼容端点：复用 OpenAI adapter

大量变体在文档注释里就写着 "Mostly uses OpenAI-compatible protocol"（`Groq` / `Together` /
`Moonshot` / `Kimi` / `Mimo` …）；它们的 `Adapter` 实现直接转调 `OpenAIAdapter` 的
util 函数，只覆盖不同的部分（endpoint/auth/reasoning 归一化）。

### 3.3 自定义端点：`AdapterKind::Custom(u8)`

```rust
// src/adapter/adapter_kind.rs:167-173
/// Those are the Custom Adapter triggered by the `genai_` prefix namespaced
/// e.g. `genai_1::gemma-4-26b-a4b-it-4bit` ...
/// `GENAI_1_ENDPOINT`: required, e.g. `https://127.0.0.1:8000/v1`
/// `GENAI_1_API_KEY`: optional, e.g. `welcome`
Custom(u8),
```

实现（`src/adapter/adapters/custom/adapter_impl.rs:40`）**整个转调 OpenAI adapter**：

```rust
fn to_web_request_data(target, service_type, chat_req, options) -> Result<WebRequestData> {
    OpenAIAdapter::util_to_web_request_data(target, service_type, chat_req, options, None)
}
```

endpoint / key 从环境变量读（`CustomAdapter::get_endpoint(n)` / `get_auth(n)`，`:19`、`:30`），
没有设置就直接返回空 endpoint。文档（`docs/for-llm/api-reference-for-llm.md:710-735`）写明：
`n` 是 `u8`，多端点靠不同编号；协议目前**固定 OpenAI chat completions**；非 OpenAI 协议要
自己实现 adapter，或用 `ServiceTargetResolver`。

### 3.4 逃生口：`ServiceTargetResolver`

`src/resolver/service_target_resolver.rs:26` 允许按请求改 `ServiceTarget`（endpoint/auth/model），
不必新增 adapter —— 这是「同一协议、动态换端点/凭证」的正规入口。

### 3.5 新协议怎么加

加 `AdapterKind` 变体 + 一个 `Adapter` 实现 + 在 `adapter_kind_str_maps!`（`adapter_kind.rs:179` 起）
加两行名字映射。全部在 crate 内，编译期完成。

---

## 4. TanStack AI

### 4.1 一个 provider 一个 npm 包，adapter 是类

```typescript
// packages/ai/src/activities/chat/adapter.ts:245
export abstract class BaseTextAdapter<...> { ... }
```

每个包（`@tanstack/ai-openai` / `-anthropic` / `-gemini` …）提供若干 adapter 类，
实现 `chatStream()` / `structuredOutput()` 等（见 `docs/community-adapters/guide.md` §2）。
**没有注册表**：调用时把 adapter 传进去，`chat({ adapter, messages })`。

OpenAI 包里按接口拆文件：`adapters/text-chat-completions.ts`、`adapters/text.ts`（Responses）、
`adapters/embedding.ts`、`adapters/image.ts`……

### 4.2 兼容端点：`openaiCompatible()` 工厂

```typescript
// packages/ai-openai/src/compatible/index.ts:44
export function openaiCompatible<const TModels extends ReadonlyArray<CompatibleModelInput>>(
  config: OpenAICompatibleConfig<TModels>,
)
```

用法（同文件注释）：

```ts
const deepseek = openaiCompatible({
  name: 'deepseek',
  baseURL: 'https://api.deepseek.com/v1',
  apiKey: process.env.DEEPSEEK_KEY!,
  models: ['deepseek-chat', 'deepseek-reasoner'],
})
chat({ adapter: deepseek('deepseek-chat'), messages })
```

- `apiKey` / `baseURL` 直接作为 OpenAI SDK 的 `ClientOptions`（`docs/adapters/openai-compatible-advanced.md:19`）；
- `api: 'chat-completions' | 'responses'` 选协议；
- 模型能力是**类型层**的：`models: ['a', createModel('b', ['text','image'])]`，
  `CompatibleModelInput` / `ExtendedModelDef`（`packages/ai-openai/src/compatible/types.ts`）；
- 子类把 `maxTokensKey` 改回 `max_tokens`（`compatible/adapter.ts:29`），并额外抽取
  `delta.reasoning_content` / `reasoning` / `reasoning_text`（`adapter.ts:52`）——
  注释说明这是 DeepSeek/Qwen/GLM/Kimi/vLLM/SGLang 的共通扩展。

### 4.3 给已有 adapter 加模型：`extendAdapter`

```typescript
// packages/ai/src/extend-adapter.ts:247
export function extendAdapter(factory, customModels)
```

**纯类型层**：运行时原样返回 factory，不加校验、不改变行为（`:253-260` 注释）。用于让
`models` 元组多出新名字并推出对应 options/能力类型。

### 4.4 新协议怎么加

发布一个社区包：`BaseTextAdapter` 子类 + `chatStream()` 产出 AG-UI 事件
（`docs/community-adapters/guide.md`）。包是独立维护的，core 团队不管；core 只提供
`@tanstack/ai` 的接口和类型。

---

## 5. 横向对照

| | Zed | rig | genai | TanStack AI |
|---|---|---|---|---|
| 内置协议的载体 | trait 实现 + 全局注册表 | `Wire` 实现 + `ProviderConfig` 变体 | `AdapterKind` 变体 + `Adapter` 实现 | adapter 类（独立包） |
| 兼容端点怎么接 | 设置里的 `openai_compatible` / `anthropic_compatible` 映射 | `Dialect::gateway(...)` + `with_base_url` | `AdapterKind::Custom(n)` + 环境变量 | `openaiCompatible({baseURL, apiKey, models})` |
| 自定义端点存在哪 | 用户 settings（热重载 diff） | 代码常量；URL 可运行期覆盖 | 环境变量 `GENAI_n_*` | 调用方构造参数 |
| 能否同一 wire 多端点 | 能，每个 id 一个实例 | 能，`with_base_url` / 多 `Model` | 能，多个 `n` / resolver | 能，多次调用工厂 |
| 能力位从哪来 | **用户声明的 JSON**（tools/images/parallel/…） | 代码里的 `Quirks` + `Catalog` 模型事实 | adapter 假设 + 模型规格表 | **类型层**（models 元组 / `createModel`） |
| 凭证 | `CredentialsProvider`（系统钥匙串），id 级 | 环境变量 / `Secret`（不序列化） | 环境变量 / `AuthResolver` | 构造参数（`apiKey`），可用 `fetch` 包装 |
| 非兼容协议 | 新 struct + 实现 20 方法 trait | 实现 `Wire`（+ Operation/Decoder） | 新 enum 变体 + `Adapter` 实现 | 新 npm 包 + `BaseTextAdapter` 子类 |
| 协议是否可运行期定义 | 扩展（wasm）可以注入 provider | 否（`Dialect` 序列化只认本 build 常量） | 否（`Custom` 固定 OpenAI 协议） | 是（JS，传类即可） |

## 6. 从源码里能直接读到的事实

- 四家都**把「长尾端点」当成独立一档**处理，而且都**复用同一份 OpenAI wire/adapter**，
  不复制请求编码器：Zed `open_ai_compatible.rs` 调 `open_ai` crate；rig 是同一套
  `providers/openai` wire + 不同 `Dialect`；genai `CustomAdapter` 转调 `OpenAIAdapter`；
  TanStack 子类化 `OpenAIBaseChatCompletionsTextAdapter`。
- **能力位（tools/images/parallel/…）在长尾这一档全部由用户或类型系统提供**，
  没有一家靠探测：Zed 写在 settings JSON，TanStack 写在 `models` 元组。
- 「自定义」的落点全是**数据**（settings / 环境变量 / 工厂参数 / 常量表），不是让用户实现
  一个 provider trait（Zed 的扩展除外，那是 wasm 插件路径）。
- 需要不同协议时，四家都要求**新增代码**：Zed 新 trait 实现、rig 新 `Wire`、genai 新枚举变体、
  TanStack 新包。

## 7. 「常见厂商」怎么拿到自己的区分度

分界线不是知名度，是**差异条数**：有专门代码的厂商，差异都被逐条写下来。
四家的写法差一个数量级：

| | 常见厂商的载体 | 一个厂商要写多少 | 差异写在哪 |
|---|---|---|---|
| Zed | 独立 provider struct（+ 有的独立 wire crate） | 整份复制：`deepseek.rs` 760 行、`ollama.rs` 1311 行、`open_ai.rs` 729 行 | provider 自己的代码 + settings 的 `available_models` |
| rig | `Dialect` 常量（`providers/openai/wire/dialects.rs`） | 一条 struct literal，只写不同字段 | `Dialect` 身份 + `Quirks` 字段 |
| genai | `impl_pass_through_adapter!` 一行 | 一行宏调用 | `key_env` / `endpoint` / `delegate` / `unsupported` / `managed_body_thinking` |
| TanStack | 独立 npm 包 + 子类 | 一个薄子类 + 模型元数据表 | 子类 override + 元数据表 |

**反例说明分界线是按差异而不是按名气划的**：

- TanStack 有 `ai-groq`、`ai-mistral`、`ai-openrouter`，但**没有 `ai-deepseek`**——
  DeepSeek 在 `ai-openai/src/compatible/index.ts:35-41` 的文档注释里直接当
  `openaiCompatible` 的例子用。
- Zed 给 DeepSeek 一整份 provider，并引用独立的 `deepseek` crate 的
  `RequestMessage`/`Response`/`ResponseDelta`（`deepseek.rs:386-468`）；
  Ollama 也走独立 `ollama` crate 的原生形状（`ollama.rs:20` 的
  `ChatMessage`/`ChatOptions`/`ChatResponseDelta`/`OllamaFunctionCall`，端点
  `/api/chat`，由 `ollama` crate 定义）。
- rig 的 DeepSeek 是一条 const：`supports_response_format: false`、
  `strict_tools_prefix: Some("/beta")`、`strict_schema: StrictSchema::DeepSeek`、
  `rewrite: BodyRewrite::DeepSeek`、`reasoning_field: Some("reasoning_content")`
  （`providers/openai/wire/dialects.rs` 的 `DEEPSEEK`），其余字段全部继承
  `Quirks::openai()`。
- genai 给 MiniMax 的 `delegate` 填的是 `AnthropicAdapter` 而不是 OpenAI
  （`all_adapters.rs:163-171`）——同一条 pass-through 通道可以换协议。

## 8. 被写下来的差异面（逐项对照）

只列四家真实出现过、且会影响 wire 的差异。

| 差异 | rig 字段 | genai | TanStack | Zed |
|---|---|---|---|---|
| 端点路径 | `completion_path` / `embeddings_path` / `models_path` / `verify_path` / `root_relative_routes` | `endpoint` 字符串 | `baseURL` + 子类 | 各 provider 的 `api_url` + 代码 |
| 鉴权方式 | `Quirks::auth`、`alternate_auth`、`request_id_header` | `key_env` / `AuthResolver` | `apiKey` / `defaultHeaders` | `CredentialsProvider` + 各家 header |
| 输出上限字段 | `output_cap` | 各 adapter | `maxTokensKey`（`max_tokens` vs `max_output_tokens`） | settings 的 `max_completion_tokens` |
| 是否支持 tools | `supports_tools` | `unsupported: [...]` | 模型元数据 + `TToolCapabilities` | `LanguageModel.supports_tools` |
| strict 工具 schema | `strict_tools_prefix`、`strict_schema`、`rewrite` | — | `makeGroqStructuredOutputCompatibleWithMap` 之类 | — |
| `response_format` | `supports_response_format`、`response_format_with_tools` | — | `groqModelPrefersJsonObject` | 模型 capabilities |
| 思考字段 | `reasoning_field`、`reliable_reasoning_count` | `managed_body_thinking` | `extractReasoning` override（`reasoning_content`/`reasoning`/`reasoning_text`） | 各 provider 自己解析 `reasoning_content` |
| 流式 usage | `stream_include_usage` | — | Groq 把 `chunk.x_groq.usage` 提升到 `chunk.usage`（`ai-groq/src/adapters/text.ts` 类注释） | 各 stream mapper |
| 结束语义 | `done_without_finish_reason`、`native_finish_reason`、`finishes` | — | — | 各 stream mapper |
| system 位置 | `later_system` | — | — | — |
| 工具结果里的图片 | `supports_image_tool_results` | — | — | — |
| 模型表 | `Catalog` 行 | adapter 内置模型列表 | 模型元数据表（输入模态/推理档位/工具能力） | settings `available_models` |

**注意**：上表每一行的「差异」都不是猜测，而是厂商文档/实测写下来的。四家都没有把
「都是 OpenAI 形状」当成「所以完全一样」。

