# 0008. provider 分三层：协议 / 常见厂商（一家一 crate）/ 长尾端点；`Model` 只放模型事实；事件流补 `Reasoning`

> 状态：accepted（分层与「一家一 crate」由用户拍板）
> 日期：2026-11-17
> 关联：[0006](0006-wire-request-shape.md)（请求 DTO 与能力位分层）、
> [0007](0007-response-mapping.md)（响应侧字段与未做清单）、
> [`docs/compare/provider-extension.md`](../compare/provider-extension.md)（四家源码事实）、
> [`docs/design/crate-layout.md`](../design/crate-layout.md)（布局现行规则）
>
> **归属：分层方式（协议 / 常见厂商 / 长尾）与「常见厂商一家一个 crate」是用户决定
> （2026-11-17）；「`Model` 只放模型事实」「补 `CompletionEvent::Reasoning`」是助手提案，
> 依据是四家源码对照与本仓现有字段的错位。**

## 背景与问题

0006/0007 之后，`aa_ai_openai` 把「OpenAI 的默认行为」硬编码成了**唯一行为**：

| 现在固定成 | 现实中不同厂商的样子 |
|---|---|
| 总是发 `response_format` | DeepSeek 只认 `json_object`，`json_schema` 会 400（rig `DEEPSEEK`：`supports_response_format: false`） |
| 总是发 `role: "system"` | o 系列 / GPT-5 用 `developer`；rig 还有 `later_system`（中途插入的 system 放哪） |
| 思考内容直接跳过 | DeepSeek 的 `delta.reasoning_content`、Ollama 的 `<think>` 块、Anthropic 的签名 thinking 都要保留，否则下一轮回放不了 |
| 只认 `[DONE]` + `finish_reason` | rig 有 `done_without_finish_reason` / `native_finish_reason` / 额外 finish 表 |
| 固定 `Authorization: Bearer` + `/chat/completions` | Azure 用 `api-key` header + 部署路由；OpenRouter 要 `HTTP-Referer` / `X-Title` |
| `usage` 只读 `prompt_tokens` | Groq 在 `chunk.x_groq.usage`；DeepSeek/OpenRouter 还有 cache token |

同时 IR 侧缺一件事：`ContentPart::Reasoning` 早就存在（0005），但 `CompletionEvent`
没有对应增量，provider 即使解析出思考也无处可去，agent loop 存不进历史。

**核心问题不是「怎么接自定义端点」，而是「常见厂商的差异写在哪」。** 之前把
Ollama / vLLM / DeepSeek / OpenRouter 和「用户自建端点」混成一档，是概念错误：
前四个是**有名字、有文档、差异可以逐条写下来的厂商**，最后一个是长尾。

## 当时的想法（选项）

### 选项 A：一家厂商一个 crate（用户选择）

`aa_ai_deepseek`、`aa_ai_openrouter`、`aa_ai_ollama` 各自独立，只共享
`aa_language_model::request::Request` 与 `provider::Model`，编码器各写一份。

- 优点：区分度最高；每家可以自由决定端点、鉴权、思考字段、`[DONE]` 语义，
  不必为了共性把 A 家的 quirk 塞进 B 家的分支；和 Zed 的布局一致
  （Zed 的 `crates/deepseek` 就是独立 wire crate）。
- 代价：OpenAI 形状的请求编码器会重复若干份（Zed 的 `deepseek.rs` 760 行、
  `open_ai.rs` 729 行就是这条路的实际体积）。

### 选项 B：协议 crate + `Dialect` 数据

一套 `aa_ai_openai` wire，厂商差异是一条 `Dialect` 常量（rig 的做法：`Quirks` 约 35 个字段，
DeepSeek 只写 5 项，其余 `..Quirks::openai()`）。

- 优点：没有重复编码器；加一家只写一条数据。
- 代价：`Quirks` 会持续膨胀，最终变成「所有厂商的并集」，每个字段都是一个
  `if dialect.x` 分支；某家需要不同控制流（不只是不同字段）时会被卡住。

### 选项 C：混合（协议 crate 内建 dialect 表，差异超过阈值才升格）

- 优点：小差异不用开 crate。
- 代价：「阈值」没有客观标准，代码会在两种形态之间来回搬。

## 验证（源码事实）

完整对照见 [`docs/compare/provider-extension.md`](../compare/provider-extension.md)，
这里只摘与决策直接相关的：

- **四家都把「长尾端点」当成独立一档，且都复用同一份 OpenAI wire**：Zed
  `open_ai_compatible.rs` 调 `open_ai` crate；rig 同一套 wire 换 `Dialect`；
  genai `CustomAdapter` 全部转调 `OpenAIAdapter::util_*`；TanStack 子类化
  `OpenAIBaseChatCompletionsTextAdapter`。
- **常见厂商的形态四家差一个数量级**：Zed 整份复制（`deepseek.rs` 760 行、
  `ollama.rs` 1311 行）；rig 一条 const；genai 一行
  `impl_pass_through_adapter!`；TanStack 一个薄子类 + 元数据表。
- **分界线是「差异条数」，不是「名气」**：TanStack 有 `ai-groq` / `ai-mistral` /
  `ai-openrouter`，却没有 `ai-deepseek`（DeepSeek 写在 `openaiCompatible` 的文档注释里）；
  genai 的 MiniMax 走同一条 pass-through 通道但 `delegate: AnthropicAdapter`。
- **能力位没有一家靠探测**：Zed 写在 settings JSON 的
  `OpenAiCompatibleModelCapabilities`，TanStack 写在 `models` 元组，rig 写在 `Quirks` +
  `Catalog` 模型事实，genai 写在 adapter 里。

## 决策

### D1. provider 分三层，三者互不替代

| 层 | 含义 | 本仓落点 |
|---|---|---|
| **协议** | 请求 / 响应 / 流式事件的形状 | 一个协议一个 crate 或一个协议目录：`aa_ai_openai/src/chat_completion/`、将来的 `aa_ai_anthropic`、`aa_ai_gemini`、`aa_ai_ollama`（原生 `/api/chat`） |
| **常见厂商** | 协议相同或近似，但有一组写下来的差异 | **一家一个 crate**：`aa_ai_deepseek`、`aa_ai_openrouter`、…… |
| **长尾端点** | 没有专门代码的第三方服务 | **运行时数据**（`base_url` / 鉴权 / header / 模型表），具体类型待定，见「未做」 |

规则：

- 常见厂商**不共享编码器**，只共享 canonical IR（`Request` / `Message` / `ContentPart`）
  与 `provider::Model`。重复是刻意付的代价，换来每家能独立演进。
- 厂商 crate 依赖协议 crate 或自己写 wire，由该家的差异量决定；**不许把厂商 quirk
  写回 `aa_language_model`**（core 至今不认识任何 JSON 字段）。
- 长尾端点不新增 crate：它是数据，不是 trait。

### D2. `Model` 只放「模型自身的事实」，厂商差异不进 `Model`

`provider::Model` 新增字段（均为模型级，同一厂商不同模型也会不同）：

```rust
pub struct Model {
    pub provider: String,
    pub id: String,
    pub uses_max_completion_tokens: bool,
    pub uses_developer_role: bool,          // 新：o 系列 / GPT-5 把 system 发成 developer
    pub supports_tools: bool,               // 新：默认 true（OpenAI 家族）
    pub supports_parallel_tool_calls: bool,
    pub supports_images: bool,              // 新
    pub supports_prompt_cache_key: bool,
    pub context_window: Option<u64>,        // 新
    pub max_output_tokens: Option<u64>,
}
```

**明确不放**：端点、鉴权方式、`response_format` 支持与否、思考字段名、`[DONE]` 语义、
`finish_reason` 别名表。这些是厂商级的，由各 `aa_ai_<vendor>` 自己持有（rig 的
`Dialect`/`Quirks` 之所以能放，是因为它一套 wire 服务所有厂商；我们选了 A，
这些字段自然跟着厂商 crate 走）。

判断标准：**换一个模型就会变** → 放 `Model`；**换一个厂商才会变** → 放厂商 crate。

### D3. `CompletionEvent` 增加 `Reasoning`

```rust
pub enum CompletionEvent {
    Text(String),
    Reasoning { text: String, signature: Option<String> },  // 新
    ToolUse { id: String, name: String, input: serde_json::Value, is_complete: bool },
    Stop(StopReason),
    Usage(TokenUsage),
}
```

- 对齐 Zed `LanguageModelCompletionEvent::Thinking { text, signature }`。
- `signature` 只有 Anthropic 这类要求回放签名的 provider 才有值，其余为 `None`；
  文本与签名可能落在不同增量，由 provider 合并。
- agent loop 把思考累加后，**按「Reasoning → Text → ToolCall」顺序**写入 assistant
  消息，思考不进入返回文本（DeepSeek 要求 `reasoning_content` 原序回放，
  Anthropic 要求签名块在文本之前）。

## 后果

- 正面：
  - 加一家常见厂商 = 新增一个 crate，不动 core、不动别家；
  - `Model` 保持小而稳定，不会变成厂商 quirk 的并集；
  - 思考终于能从任何 provider 流到历史，接 Anthropic / DeepSeek 不再是两套临时方案。
- 代价：
  - OpenAI 形状的编码器会重复若干份（预估每家 300–800 行），改 `Request` 字段时
    要逐家跟一遍；
  - 「同一条 wire 换端点」不再是一等能力，长尾端点需要单独的数据层；
  - `Model` 的能力位目前只是数据，`aa_ai_openai` 的 encoder 还没按它筛
    `tools` / `images` / `developer`。
- 触发重新评估的条件：
  - 厂商 crate 数量 > 5，且编码器重复部分开始出现「改一处漏三处」的真实 bug；
  - 出现「同一家需要两种协议」（如 DeepSeek 同时有 chat completions 与 anthropic 兼容端点）；
  - 长尾端点需求压过厂商 crate（用户群体是自建网关而不是知名厂商）。

## 未做

- 长尾层的声明式类型（可序列化的 `ProviderConfig { base_url, auth, headers, models }`）
  尚未设计；现在用 provider 的构造数据承担（见附录）。
- `aa_ai_openai` 的 encoder 尚未按 `Model::supports_tools` / `uses_developer_role` /
  `supports_images` 筛字段。
- `verbosity` / `prompt_cache_retention` / `reasoning_details` 仍按 0007「未做」处理。
- `aa_ai_ollama` 是否保留原生 `/api/chat` crate，还是并入长尾数据层，待接第二家时定。

## 附录：长尾层的第一版实现（2026-11-17）

不改上面的决策，只记录落地形状。「长尾端点」暂时由
`aa_ai_openai::OpenAiLanguageModelProvider` 的构造数据承担，没有新增类型：

```rust
OpenAiLanguageModelProvider::new("sk-...")        // Authorization: Bearer
    .with_base_url("https://api.example.com/v1")  // 去掉尾斜杠
    .with_header("HTTP-Referer", "...")           // 追加 header，同名覆盖默认
    .with_header("X-Title", "...");

OpenAiLanguageModelProvider::without_auth()        // 不发 Authorization（本地端点）
    .with_base_url("http://localhost:11434/v1");

OpenAiLanguageModelProvider::without_auth()        // 非 Bearer 鉴权（Azure 式）
    .with_auth_header("api-key", "secret");
```

- header 在进入 future 之前同步构造，非法名字/值返回 `LlmError::Transport`，不 panic；
- `new("")` 会发空的 `Bearer `，所以「无鉴权」必须用 `without_auth()`（写进文档）；
- **兼容端点的思考**：响应 DTO 同时认 `reasoning_content` / `reasoning` /
  `reasoning_text`，映射成 `CompletionEvent::Reasoning`（依据 TanStack
  `ai-openai/src/compatible/adapter.ts:52`）。这三个字段出现在 OpenAI 形状的同一位置，
  属于兼容端点的扩展，不改变 `aa_ai_openai` 的请求形状；
- **回放仍不在这一层**：把思考写回下一轮 assistant 消息（DeepSeek 要求）留给厂商 crate；
- `examples/chat.rs` 是验收入口：只靠 `AA_BASE_URL` / `AA_API_KEY` / `AA_MODEL` /
  `AA_EXTRA_HEADER_n` 就能逐个试通手里的第三方兼容 API；
- `list_models()`（`GET {base_url}/models`）返回端点声明的 `id`，`examples/models.rs`
  直接打印，用来填 `AA_MODEL`。非 2xx 复用 `error_message` 抽 `error.message`，
  例如 401 会显示端点要求的鉴权方式（`x-api-key` 或 Bearer）。

「模型表」仍是调用方自己构造 `provider::Model`（可逐字段赋值），尚未提供声明式列表类型。
