# 0012. OpenRouter：协议相同的「常见厂商」也单独一个 crate

> 状态：accepted
> 日期：2026-11-21
> 关联：[0008](0008-provider-tiers.md)（协议 / 厂商 / 长尾三层）、
> [0006](0006-wire-request-shape.md)（借用式请求 DTO）、
> [0007](0007-response-mapping.md)（响应映射）、
> [0010](0010-transport-object.md)（注入 `Arc<HttpClient>`）、
> [`docs/compare/openrouter-wire.md`](../compare/openrouter-wire.md)（逐字段事实）
>
> **归属：用户 2026-11-17 决定「OpenRouter 新开 `aa_ai_openrouter`，不往
> `aa_ai_openai` 里加分支」；具体映射规则由助手提案。**

## 背景与问题

OpenRouter 的对话接口是 OpenAI chat completions 的形状：同样的
`POST /chat/completions`、SSE + `[DONE]`、`choices[].message`、`tools` /
`tool_calls` / `tool_choice`、`response_format`。按 [0008](0008-provider-tiers.md)
的三层划分，它属于「**常见厂商、协议相同**」那一档。

问题在于「协议相同」不等于「可以共享实现」。差异是硬差异：

- 思考强度是 `reasoning: { effort }`，不是 OpenAI 的顶层 `reasoning_effort`；
- 思考回放：assistant 消息可带 `reasoning`（纯文本）或 `reasoning_details`
  （结构化数组），工具调用续写要求原样回放；
- 错误体的 `code` 是**数字**，并带 `metadata`（`error_type` / `provider_code`）；
- 流内可能出错：`finish_reason: "error"` + 顶层 `error`；
- 流式 usage 始终返回，`stream_options.include_usage` 已废弃；
- 模型列表带 `pricing` / `context_length` / `supported_parameters`，且是公开接口；
- 归因 header `HTTP-Referer` / `X-Title`。

约束同前：内存优先；core 不认识厂商；不读 env / 配置；不接受过渡实现。

## 当时的想法（选项）

1. **在 `aa_ai_openai` 里加 `if is_openrouter` 分支**
   - 假设：形状一样，差异用开关盖住。
   - 代价：encoder 里出现按厂商分叉的字段；`aa_ai_openai` 的测试要覆盖两套
     reasoning 语义；以后 DeepSeek 再进来就是三套开关。
2. **Zed 式：`aa_ai_openrouter` 薄封装，复用 `aa_ai_openai` 的 DTO / 流解码**
   - 假设：只差 header 和错误体。
   - 代价：Zed 能这么做是因为它**不解析** `reasoning` / `reasoning_details`
     （provider 层只转发文本），而本库要把思考回放进 canonical IR。
     复用响应 DTO 会漏掉 details 累加、`finish_reason:"error"`、200-body 错误。
3. **独立 crate，encoder / 响应 DTO 全部自写**（选定）
   - 假设：重复是 [0008](0008-provider-tiers.md) D1 已经接受的代价。
   - 收益：每家的 wire 差异在各自 crate 内闭合，测试也按厂商写。

## 验证

### wire 事实（详见 [`docs/compare/openrouter-wire.md`](../compare/openrouter-wire.md)）

- OpenRouter OpenAPI（`https://openrouter.ai/openapi.json`，抓取于 2026-11-21）：
  `ChatRequest.reasoning` 是对象；`ChatResult.choices[].finish_reason` 枚举含
  `error`；`ChatUsage` 有 `cost` / `is_byok` 等扩展；`Model.pricing.prompt` /
  `completion` 是**字符串**。
- 官方 Errors 文档：非 2xx 的 `error.code` 是数字；生成中途失败时 HTTP 已是
  `200`，错误在 body（只有顶层 `error`，或挂在 `choices[0].error` 上）。
- 官方 Reasoning Tokens 文档：`reasoning_details` 必须**原样、按序**回放；
  `reasoning` 与 `reasoning_details` 可二选一或同时给。
- Zed `crates/open_router/src/open_router.rs`：复用 `open_ai::stream_chat_completion`，
  只加 header / 错误体 / `/models/user`——因为它不做思考回放，所以可以薄。

### 编译 / 测试

- `aa_ai_openrouter`：32 个单测 + 1 个 doctest（请求编码、SSE 解码、错误体、
  `pending_stop`、`/models` 解析、header）。
- 端到端（真实 OpenRouter，经 mitmproxy，`/models` 为公开接口）：
  - `aacode providers` 正常显示 `openrouter`；
  - `aacode models openrouter --free` 返回 14 个零价模型，
    `--free` 过滤与 `[free]` 标记正常。
- 未验证：真实 `chat` / `run`（缺 `OPENROUTER_API_KEY`）。免费模型限流是
  20 req/min + 每日上限，验证时要观察 429 与 `Retry-After`。

## 决策

### D1. 独立 crate，不依赖 `aa_ai_openai`

```
aa_ai_openrouter/src/
  lib.rs                      provider（header / 鉴权 / 两条请求路径）
  chat_completion/mod.rs      与 OpenAI 的差异清单
  chat_completion/request/    自写 encoder + 借用式 DTO
  chat_completion/response/   SSE 解码 + reasoning_details 累加 + 错误映射
  models.rs                   GET /models
```

请求侧只 `Serialize`、响应侧只 `Deserialize`，两边不共享结构体。`encode` 一次
`to_writer` 成 owned 字节，`'a` 不进 future（同 [0006](0006-wire-request-shape.md)）。
传输仍收 `Arc<aa_http_client::HttpClient>`，本 crate 没有构造 `reqwest::Client` 的代码。

### D2. 思考回放：`reasoning_details` 存进 `ContentPart::Reasoning.signature`

canonical IR 没有专门的 wire 槽位。解码时把整个 `reasoning_details` 数组
`serde_json::to_string` 塞进 `Reasoning.signature`，文本照常放 `text`：

- 回放时若 `signature` 能解析成 JSON **数组**，就作为 `reasoning_details` 发出；
- 有 details 时**不再**另发纯文本 `reasoning`（details 已包含文本/签名）；
- 没有 details 时，只有该 assistant 消息**带 `tool_calls`** 才回放纯文本
  `reasoning`。

最后一条是刻意的：文档要求回放的场景是「工具调用后继续同一段回答」；每轮都回放
纯文本思考会让上下文逐轮翻倍，而 OpenRouter 对纯文本 `reasoning` 并没有「必须
保留」的要求。

`RedactedThinking` / `Opaque` 跳过——OpenRouter 的加密思考在
`reasoning_details` 里（`reasoning.encrypted`），不走这两个 canonical 变体。

### D3. 请求侧：`reasoning: { effort }`，不发 `stream_options`

`ReasoningEffort` 的四个取值（`Minimal` / `Low` / `Medium` / `High`）都是
OpenRouter 接受值。`max_tokens` / `enabled` / `exclude` 没有 canonical 字段，
不凭空发明。`stream_options.include_usage` 已废弃且无效果，省略字段。

输出上限按 `Model::uses_max_completion_tokens` 在 `max_tokens` /
`max_completion_tokens` 之间二选一（OpenRouter 已把 `max_tokens` 标为 deprecated）。

### D4. 流式 `Stop` 延后到流结束

OpenRouter 的 usage 帧在 `finish_reason` 之后。若收到 `finish_reason` 就发
`Stop`，上层会在 `Usage` 之前收尾。所以解码器把 `StopReason` 记在
`pending_stop`，等 `[DONE]`（或流正常结束）再发 `Stop`，保证顺序是
`… → Usage → Stop`。流被截断（没有 `[DONE]` / `finish_reason`）仍报
`LlmError::StreamEnded`（同 [0007](0007-response-mapping.md)）。

`finish_reason` 映射：`length` → `MaxTokens`（优先于工具调用）、
`tool_calls` / `function_call` → `ToolUse`、`content_filter` → `Refusal`、
`stop` → `EndTurn`；非空 `refusal` 文本覆盖 `stop` 为 `Refusal`。
`finish_reason: "error"` 不产生 `Stop`——同帧的 `error` 直接成为
`ProviderRejection`。

### D5. 错误统一成 `ProviderRejection`，体内错误 `status = 200`

`ApiError { code: Option<i64>, message, metadata }`：

- 非 2xx：`status` 取 HTTP status，`message` 拼上 `metadata.error_type`；
- 流内错误：`status = 200`，`transient` 由 `error_type` 与 code 判断；
- 非流式 200：只有顶层 `error` → 直接报错；错误挂在 `choices[0].error` →
  先交付已生成内容，最后报错，**不补 `Stop`**（这次响应没有正常结束）。

`transient` 的判定看 HTTP status（408 / 425 / 429 / 5xx）和 `error_type`
（`rate_limit_exceeded` / `provider_overloaded` / `provider_unavailable` /
`timeout`）；体内错误没有 HTTP status，改用 `error.code`（408 / 429 / 5xx）。

### D6. 免费模型：库给 `is_free()`，应用给 `ModelSummary.free`

`models::ModelInfo` 解析 `pricing.prompt` / `pricing.completion`（字符串十进制），
两者数值为 0 才是免费。**不用 `:free` 后缀判断**——定价会变，后缀会漂移。

应用层 `ModelSummary` 增加 `free: Option<bool>`：OpenAI / Ollama 为 `None`，
OpenRouter 由 `is_free()` 填充；CLI `models --free` 据此过滤。

### D7. 应用层：section `openrouter`，与协议一一对应

aacode 的 `ProviderClient` 增加 `OpenRouter` 分支：

- section key `language_model_providers.openrouter`（不是 Zed 的 `open_router`）；
- provider id `openrouter`，默认环境变量 `OPENROUTER_API_KEY`；
- 设置字段：`base_url` / `auth` / `headers` / `app_url` / `app_title`
  （`app_name` 作别名）/ `available_models`；
- `build_openrouter_provider` 把 `app_url` / `app_title` 传给 provider 构造参数，
  header 由 provider 统一组装；`X-Title` 可被 `headers` 同名覆盖。

## 后果

- 正面：
  - OpenRouter 的思考回放、流内错误、数字错误码、免费模型判定都在自己 crate
    内闭合，`aa_ai_openai` 保持纯净；
  - 第三个 provider 落地，再次验证「一家一 crate + 注入传输」；
  - `/models` 公开接口让 CLI 能直接列出免费模型，不需要密钥。
- 代价：
  - 请求 encoder 与 `aa_ai_openai` 有大量重复（[0008](0008-provider-tiers.md)
    D1 已接受）；
  - `reasoning_details` 借用 `Reasoning.signature` 这个字符串槽位，是「IR 没有
    结构化 wire 槽位」的妥协；若将来多家都需要，应该新开 `ContentPart` 变体或
    在 `Message` 上加 provider-private 段；
  - `Retry-After` 没有上抛：`LlmError::ProviderRejection` 只有 `transient`，
    429 的具体退避时间丢失，v0 把退避留给应用层；
  - `provider` 路由偏好 / `service_tier` / `session_id` / prompt cache 键之外的
    OpenRouter 扩展没有 canonical 槽位，v0 不发。
- 触发重新评估的条件：
  - 第二家厂商也需要结构化思考回放（DeepSeek / Anthropic）：把
    `reasoning_details` 从 `signature` 里提出来，进 `ContentPart`；
  - 应用层需要按 `Retry-After` 精确退避：给 `ProviderRejection` 加可选
    `retry_after: Option<Duration>`；
  - 需要 `provider` 路由（排序 / 过滤 / 回退）：作为 `Request` 的 per-request
    选择，或 provider 构造数据，不塞进 `Model`。
