# 0011. Ollama 原生 `/api/chat`：第二个协议接入

> 状态：accepted
> 日期：2026-11-21
> 关联：[0008](0008-provider-tiers.md)（协议 / 厂商 / 长尾三层）、
> [0006](0006-wire-request-shape.md)（借用式请求 DTO）、
> [0007](0007-response-mapping.md)（响应映射：本文记录其中一处**按协议**的偏离）、
> [`docs/compare/ollama-wire.md`](../compare/ollama-wire.md)（逐字段事实）
>
> **归属：用户 2026-11-20 决定「按建议继续」，即把已经占位的 `aa_ai_ollama`
> 真正实现；具体映射规则由助手提案。**

## 背景与问题

`aa_ai_ollama` 之前是个占位 crate，只写了一句话「计划内容」。与此同时，应用层
aacode 的 `language_model_providers.ollama` section 只能解析、`resolve` 直接报
「尚未实现」，本地 Ollama 只能绕道 `openai_compatible.ollama`（`/v1`）。

这条路能用，但丢东西：`/v1` 是 Ollama 自己做的 OpenAI 兼容层，
- `thinking` 被塞进 `reasoning_content`（兼容层私货），原生字段 `message.thinking` 看不到；
- 原生 `options.num_predict` / `keep_alive` / `format` 的裸 schema 形状无处表达；
- 工具结果靠 `tool_name` 关联这件事被兼容层翻译成 `tool_call_id`，多一层猜测。

约束同前：内存优先；core 不认识厂商；不读 env / 配置；不接受过渡实现
（宁可 `todo!()` 也不偷偷换协议）。

## 验证

### wire 事实（详见 [`docs/compare/ollama-wire.md`](../compare/ollama-wire.md)）

- Ollama 官方 `docs/api.md`：`POST /api/chat`，NDJSON 行流，`stream:false` 返回
  **同一帧形状的单个对象**；`think` 接受 bool 或 `low`/`medium`/`high`/`max`；
  `format` 是 `"json"` 或**裸 JSON Schema**；工具结果的关联字段是 `tool_name`。
- 官方「Streaming with tools」示例：带 `tool_calls` 的那帧 `done:false`，
  末帧 `done_reason:"stop"` —— **`done_reason` 不区分工具调用**。
- 本机 Ollama `0.40.16` 实测：`/api/tags` 给 `name` / `details.family` /
  `capabilities`；`/api/show` 给 `capabilities:["completion","tools"]`。
- Zed `crates/ollama/src/ollama.rs`：`ChatResponseDelta{message,done,done_reason,
  prompt_eval_count,eval_count}`；provider 层完全忽略 `done_reason`，用
  `used_tools: bool` 决定 `StopReason::ToolUse`。

### 编译 / 测试

- `aa_ai_ollama`：35 个测试（请求编码 16、响应解码 13、HTTP 往返 5、模型列表 1）。
- 端到端（本机 Ollama 0.40.16，`qwen2.5:0.5b`，经 `Proxy::Direct`）：
  - `aacode models ollama` → 走 `GET /api/tags`，列出本机与 `:cloud` 模型；
  - `aacode chat ollama qwen2.5:0.5b "…"` → 流式文本 + `[stop] EndTurn` + usage；
  - `aacode run ollama qwen2.5:0.5b "…"` → 一次 `glob` 工具调用，结果回灌后第二轮收敛。

## 决策

### D1. 一个协议一个 crate，一个协议一个模块

`aa_ai_ollama` 的结构对齐 `aa_ai_openai`：

```
aa_ai_ollama/src/
  lib.rs            provider（传输 + 鉴权 + 两条请求路径）
  chat/mod.rs       协议对照表（为什么不能复用 OpenAI 的 DTO）
  chat/request.rs   借用式请求 DTO + encoder
  chat/response.rs  NDJSON 分帧 + 解码器 + done_reason 映射
  models.rs         GET /api/tags
```

请求侧只 `Serialize`、响应侧只 `Deserialize`，两边不共享结构体（同 ADR 0006 D1）。
`encode` 一次 `to_writer` 成 owned 字节，`'a` 不出现在任何跨 `.await` 的类型上。

### D2. 传输仍是注入的 `Arc<HttpClient>`

新增的 provider 没有重新实现超时/代理/解码重试，直接收
`Arc<dyn aa_http_client::HttpClient>`（[0010](0010-transport-object.md)）。这是把传输
独立出去的第一次兑现：本 crate 里没有一行 `reqwest::Client::builder()`。

### D3. `StopReason`：`done_reason` + 「本响应出现过工具调用」

ADR 0007 D2 说 `finish_reason` 是唯一权威。Ollama 的 `done_reason` 不满足这个前提
（带工具调用时仍是 `"stop"`），所以映射按协议改为：

| `done_reason` | 有 `tool_calls` | `StopReason` |
|---|---|---|
| `length` | 任意 | `MaxTokens`（真被截断，优先） |
| 其他 / 缺失 | 是 | `ToolUse` |
| 其他 / 缺失 | 否 | `EndTurn` |

`load` / `unload`（加载、卸载模型的空响应）按 `EndTurn`。
`StopReason` 的语义（「这一轮为什么停下」）没变，变的是**从哪些 wire 字段推断**。

不做 Zed 那样的纯 `used_tools` 布尔：`length` 是真实的截断信号，丢掉它会让被截断
的回复看起来正常结束（正是 0007 要修的问题）。

### D4. 工具调用 id：有则用，没有则合成

Ollama 的响应 `tool_calls[]` 通常**没有 `id`**，但 canonical
`ContentPart::ToolCall.id` 是必填，agent loop 靠它把工具结果对回去。所以解码器
在缺失时合成 `call_1`、`call_2`…（本次响应内递增），并在回放历史时把它写回去
（`tool_calls[].id` 是 Ollama 接受的可选字段）。

工具结果消息用 `tool_name` 关联：encoder 从同一份历史的 `ToolCall{id,name}` 里反查
（线性扫描 `Vec`，不建表——一条历史里工具调用通常个位数）。

### D5. 无法表达的东西明确报错，不静默降级

- `ImageSource::Url` → `LlmError::Unsupported`（Ollama 的 `images` 只收 base64 字节，
  URL 得由应用先下载）。
- `capabilities.chat_completions=false` 的模型在原生 section 不校验（那是「能不能走
  OpenAI `/v1`」，与 `/api/chat` 无关）。
- `Model::supports_tools/images = false` + 请求带工具/图 → `Unsupported`（同
  `aa_ai_openai`，ADR 0006 D4）。
- custom 工具（非 JSON Schema）→ `Unsupported`。
- `ResponseFormat::Text` → 不写 `format`（省略字段就是普通文本，不发空串）。

### D6. 应用层：section 与协议一一对应

aacode 的 `ProviderClient` 收成枚举：

```rust
pub enum ProviderClient {
    OpenAi(aa_ai_openai::OpenAiLanguageModelProvider),
    Ollama(aa_ai_ollama::OllamaLanguageModelProvider),
}
```

- `list_models` 不在 `LanguageModelClient` trait 上（那是「端点能力」，不是协议语义），
  由 `ProviderClient::list_models()` 归一化成 `ModelSummary{id, owned_by}`。
- `resolve` 先看 `openai_compatible.<id>`，再看 `ollama` section（同名覆盖，照 Zed 注册
  顺序）。两个 section 现在都有适配器，不会互相顶替。
- 示例配置里本地 Ollama 的两个视图用不同 key（`ollama` 走原生、`ollama_v1` 走 `/v1`），
  否则同名时 compatible 会覆盖原生 section，原生协议根本选不中。

## 后果

- 正面：
  - 第二个协议落地，证明「一家/一协议一 crate + 注入传输」的形状能复用；
  - 本地 Ollama 能拿到官方 `thinking`、原生 `options` / `format`，不再依赖兼容层；
  - aacode 的 `ollama` section 从「能解析不能用」变成可用，`Unimplemented` 分支消失。
- 代价：
  - `StopReason` 的推导逻辑按协议分叉（OpenAI 看 `finish_reason`，Ollama 看
    `done_reason` + 工具调用），ADR 0007 的「唯一权威」不再是全局结论；
  - 工具调用 id 是合成的，会话持久化后与 Ollama 服务端不对应（Ollama 本来也不认）；
  - `keep_alive` / `context` 没有接：前者是服务端运维参数，后者已 deprecated。
- 触发重新评估的条件：
  - Ollama 给 `tool_calls[].id` 变成常态（那时可以去掉合成，并检查 `tool_name`
    是否还有必要）；
  - 需要 `keep_alive`（例如共享 GPU 机器上限制模型驻留）：加进 `Model` 或
    provider 构造数据，不进 `Request`；
  - 原生 `/api/generate`（非对话）需求出现：那是同一 crate 的另一个模块，
    不要塞进 `chat`。