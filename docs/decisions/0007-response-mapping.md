# 0007. 响应侧映射：真解析 SSE、`finish_reason` 决定 `StopReason`、`usage` 与错误体归位

> 状态：accepted
> 日期：2026-11-17
> 关联：[0006](0006-wire-request-shape.md)（请求侧的借用 DTO / `stream` 字段）、
> [0002](0002-memory-first.md)（不缓冲整个响应）、[0004](0004-message-layout.md)（wire 强类型）
>
> **归属：方向由用户拍板（2026-11-17）——「补 SSE 解析」「下一轮只补响应侧」；
> 具体映射规则（`finish_reason` 表、截断判定、错误文本）由助手提案。**
> 两条都没有改动 IR（`Request` / `Message` / `ContentPart` 一行未动）。

## 背景与问题

0006 把请求侧定型后，暴露两个不对称：

1. **请求能发 `stream: true`，客户端却仍用 `resp.json()` 解析。** 服务端返回
   `text/event-stream`，`.json()` 必然失败。默认 `stream=false`，测试发现不了。
2. **响应侧只解析了 `content` 与 `tool_calls`。** `finish_reason`、`usage`、错误
   `error.message` 全部丢弃，导致：
   - `StopReason::MaxTokens` / `Refusal` 两个变体永远不会出现——client 只按「有没有
     `tool_calls`」猜（旧 `lib.rs` 的 `wants_tools`）；被截断的回复会被当成正常结束。
   - `LanguageModelCompletionEvent::Usage` 从未发出，`TokenUsage` 永远为空，而请求侧已经会发
     `stream_options.include_usage`。
   - 非 2xx 时把整个 body（JSON 或 HTML）塞进 `LlmError::ProviderRejection.message`。

## 验证（源码 / spec 事实）

### OpenAI 官方 spec

`openai/openai-openapi`（v2.3.0）：

- `CreateChatCompletionRequest` 有 30 个属性，叠加 `ModelResponseProperties` 后共 37 个。
  我们按 0006 D4 只发其中 15 个（Zed 基线 + `response_format`），其余是明确排除，不是遗漏。
- `ChatCompletionRequestMessage` 是 `developer` / `system` / `user` / `assistant` / `tool` /
  `function` 六选一——`developer` 是 o 系列 / GPT-5 时代 `system` 的替代名。
  **本轮不改**（属消息层，需动 IR 的 `Role` 映射，见「未做」）。
- 流式的 `finish_reason` 取值：`stop` / `length` / `tool_calls` / `content_filter` /
  `function_call`（后两个是老名/审核中断）。

### 三家怎么处理响应

| | `finish_reason` | `usage` | 流式 |
|---|---|---|---|
| Zed | wire 里是 `Option<String>`（`open_ai/src/open_ai.rs:918`），**存下来但不映射**；只有 copilot 适配器映射（`copilot_chat/src/model.rs:394`） | `Response.usage: Usage` 强制非可选（`:911`） | `stream_completion` 读 SSE，`OpenAiResponseEventMapper` 逐事件映射（`completion.rs:752`） |
| rig | `StopReason` 枚举，`length` → `MaxTokens` 等（`completion/message.rs` 一族） | 流末 usage 帧 | `LanguageModelCompletionEvent` 流，工具调用按 index 归并 |
| genai | `StopReason` 映射 | `Usage` 从最后一帧取 | adapter 逐帧解析 SSE |

结论：**映射 `finish_reason` 是 rig/genai 的共识，Zed 只是没在 OpenAI 适配器里做**；
真解析 SSE 是三家一致做法。

### 截断与 `[DONE]`

OpenAI 的流以 `data: [DONE]` 收尾；`finish_reason` 在**最后一条带 choices 的帧**里，
`usage` 在**它之后的单独一帧**（`choices: []`）。因此顺序固定为：

```
…delta… → finish_reason 帧 → usage 帧（可选） → [DONE]
```

若连接在没有 `finish_reason`、也没有 `[DONE]` 的情况下断掉，就是截断。

## 决策

### D1：真解析 SSE（用户决定）

- 新增 `aa_ai_openai::chat_completion::response::StreamDecoder`：只做 SSE 分帧 + JSON 解码，
  **不碰 HTTP**，所以能单测（喂字节、拿事件）。
- 行缓冲保留未完成的行：TCP 把一帧切在任意字节处都能拼回来（有测试）。
- 支持 `\r\n`、注释行 `:`、`event:` / `id:` / `retry:` 行（忽略）；`data:` 后的单个空格
  按规范剥掉。
- 每个网络分片到货就解码，不缓冲整个响应（对齐 0002 的内存优先）。

### D2：`finish_reason` 是 `StopReason` 的唯一权威

| `finish_reason` | `StopReason` |
|---|---|
| `tool_calls` / `function_call` | `ToolUse` |
| `length` | `MaxTokens` |
| `content_filter` | `Refusal` |
| `stop` / 其他未知值 | `EndTurn` |
| 缺失 + 有 `tool_calls` | `ToolUse`（兼容不报 `finish_reason` 的端点） |
| 缺失 + 无 `tool_calls` | `EndTurn` |

- 非流式与流式共用同一张表（`response::stop_reason` / `final_stop_reason`）。
- `length` 优先于「有 tool_calls」：被截断的工具调用参数通常是半截 JSON，报 `MaxTokens`
  让上层能区分「模型想调工具」和「参数被砍断」。

### D3：`usage` 映射成事件，且排在 `Stop` 之前

- 非流式：body 的 `usage` 若存在 → `LanguageModelCompletionEvent::Usage`。
- 流式：`include_usage` 的最后一帧（`choices: []` + `usage`）→ `LanguageModelCompletionEvent::Usage`。
- 顺序固定 `Usage` 在 `Stop` 之前，上层看到 `Stop` 即可收尾。
- 兼容端点不给 `usage` 时就不发，不补零（`TokenUsage::default()` 会被误读成「用量为零」）。

### D4：错误体只取 `error.message`

- 非 2xx：尝试 `{"error":{"message":…,"code":…}}`，取 `message (code: …)`；
  不是该形状（纯文本 / HTML）就原样返回。
- 流内错误（HTTP 200 + `{"error":…}`）：`LlmError::ProviderRejection { status: 200, … }`，
  用 `200` 标明「不是 HTTP 层拒绝」。

### D5：截断要报错，不要静默收尾

- 收到 `[DONE]` 但没收到 `finish_reason` → 补一个 `EndTurn`（避免上层空等）。
- 连接结束、既没 `finish_reason` 也没 `[DONE]` → `LlmError::StreamEnded`。

### 未做（本轮范围外，记录以免遗忘）

- `Role::System` → `developer` 的映射（需动 IR 的 `Role` 或 `Model` 能力位）。
- `verbosity` / `prompt_cache_retention` / `prompt_cache_options` 等新 wire 字段。
- `assistant.reasoning_content` / `reasoning_details` 回放（Zed `open_ai.rs:805-812`）。
- `top_p` / `seed` / `logprobs` 等采样字段（0006 D4 已排除）。

## 后果

**正面：**

- `stream: true` 真的能用了，且是逐片解码，不是先收全再切。
- `MaxTokens` / `Refusal` 第一次能被上层看到；截断不再伪装成正常结束。
- `TokenUsage` 有值，成本统计有了数据源。
- 响应侧 DTO 独立于请求侧（`response` 模块 `Deserialize`，`request` 模块 `Serialize`），
  两边不共享结构体，改一边不会连带另一边。

**代价：**

- 新增 `reqwest` 的 `stream` feature（只影响 `aa_ai_openai`）。
- 一个 SSE 帧 JSON 解析失败会让整个流以 `Transport` 结束。选择「失败快」而不是
  「跳过坏帧」，因为丢帧会让工具调用参数少一段，比报错更难查。
- 工具调用按 `index` 归并，依赖 OpenAI 的 index 契约；不按 index 的兼容端点
  （极少）会把并行调用拼错。
- 多行 `data:`（SSE 规范允许同一帧拆成多个 `data:` 行）目前不拼接——OpenAI 与其兼容
  端点都是一个 JSON 一行，等真遇到再补。

**触发重新评估的条件：**

- 若某家兼容端点把同一帧拆成多行 `data:` → 按规范拼接（空行才是帧结束）。
- 若需要 `seed`/`top_p` 等采样参数 → 另写 ADR，先决定放 IR 还是放 provider 私有配置。
- 若接 Anthropic / Responses API → 它们的流是事件类型驱动（`content_block_delta` 等），
  不能复用这个 SSE 解码器，要各写各的。
