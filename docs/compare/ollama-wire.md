# Ollama `/api/chat` 与 OpenAI chat completions 的 wire 差异（事实记录）

> 只记事实：字段名、形状、出处。「信谁」「怎么映射」不在本文讨论。
> 出处：
> - Ollama 官方 API 文档 `docs/api.md`（GitHub `ollama/ollama` main，
>   抓取于 2026-11-21）：「Generate a chat completion」一节；
> - 本机 Ollama `0.40.16` 的 `/api/show`、`/api/tags` 实测响应；
> - Zed `crates/ollama/src/ollama.rs`（commit `4ed72e0135`）与
>   `crates/language_models/src/provider/ollama.rs`。

## 1. 端点与分帧

| | OpenAI 兼容 | Ollama 原生 |
|---|---|---|
| 对话路径 | `POST {base}/chat/completions`（`base` 通常带 `/v1`） | `POST {root}/api/chat`（`root` 不带 `/v1`） |
| 模型列表 | `GET {base}/models` | `GET {root}/api/tags` |
| 流式分帧 | SSE：`data: {...}\n\n`，以 `data: [DONE]` 收尾 | NDJSON：一行一个完整 JSON 对象，**没有** `[DONE]` |
| 终止信号 | `[DONE]` 或连接关闭 | 一帧 `"done": true` |
| `stream: false` | 返回单个 body（有 `choices[]`） | 返回**同一帧形状**的单个对象（有 `message`、`done: true`） |

Ollama 文档：「Certain endpoints stream responses as JSON objects. Streaming can be
disabled by providing `{"stream": false}`」。

## 2. 请求字段

| canonical | OpenAI | Ollama |
|---|---|---|
| 消息角色 | `system` / `developer` / `user` / `assistant` / `tool` | `system` / `user` / `assistant` / **`tool`**（没有 `developer`） |
| 文本 | `content` 可以是字符串或 part 数组 | `content` **只能是字符串** |
| 图片 | part：`{"type":"image_url","image_url":{"url":...}}`（支持 http URL 与 `data:` URL） | 消息级 `images: [base64]`，**只接受字节的 base64**，不带 `data:` 前缀 |
| 工具定义 | `tools[].function.{name,description,parameters}` | 同形状（文档示例一致） |
| 工具入参 | `function.arguments` 是 **JSON 字符串** | `function.arguments` 是 **对象** |
| 工具结果 | `role:"tool"` + `tool_call_id` | `role:"tool"` + **`tool_name`**（没有 id 字段） |
| 工具调用 id | 响应必带 `id` | 文档示例里响应 `tool_calls[]` **没有 `id`** |
| 输出上限 | `max_tokens` 或 `max_completion_tokens` | `options.num_predict` |
| 温度 / stop | `temperature` / `stop` | `options.temperature` / `options.stop` |
| 结构化输出 | `response_format: {"type":"json_schema",...}` | `format: "json"` 或**裸 JSON Schema** |
| 思考开关 | `reasoning_effort`（o 系列）/ 无 | `think: bool` 或 `"low"`/`"medium"`/`"high"`/`"max"` |
| 模型保活 | 无 | `keep_alive`（默认 `5m`） |
| 上下文续传 | 无 | `context`（`/api/generate`，已 deprecated） |

Ollama 文档对 `format` 的措辞：「Format can be `json` or a JSON schema」——没有
OpenAI 的 `json_schema` / `name` / `strict` 信封。

`tools` 的文档示例（`docs/api.md`「Chat request (Streaming with tools)」）：

```json
{"type":"function","function":{"name":"get_weather","description":"...","parameters":{...}}}
```

## 3. 响应字段

| | OpenAI | Ollama |
|---|---|---|
| 增量容器 | `choices[0].delta.{content,tool_calls,...}` | 顶层 `message.{content,thinking,tool_calls}` |
| 思考 | 官方无；兼容端点私塞 `reasoning_content` / `reasoning` / `reasoning_text` | **`message.thinking`**（官方字段） |
| 结束原因 | 每帧 `choices[].finish_reason` | 末帧 `done_reason`（`stop` / `length` / `load` / `unload`） |
| 用量 | `usage.{prompt_tokens,completion_tokens}` | `prompt_eval_count` / `eval_count`（末帧） |
| 错误 | HTTP 非 2xx + `{"error":{"message"}}` | HTTP 非 2xx **或** 200 流内 `{"error":"..."}`（字符串） |

### `done_reason` 与工具调用

Ollama 文档的「Streaming with tools」示例里，**带 `tool_calls` 的那一帧 `done: false`，
末帧 `done_reason` 是 `"stop"`**：

```json
{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"get_weather","arguments":{"city":"Tokyo"}}}]},"done":false}
...
{"message":{"role":"assistant","content":""},"done_reason":"stop","done":true,...}
```

也就是说 `done_reason` **不区分「工具调用」和「正常结束」**。

Zed 的处理（`crates/language_models/src/provider/ollama.rs:662-670`）：完全忽略
`done_reason`，用自己记的 `used_tools` 布尔量决定发 `StopReason::ToolUse` 还是
`StopReason::EndTurn`。

## 4. 模型列表形状

OpenAI（`GET /models`）：

```json
{"object":"list","data":[{"id":"gpt-4o","object":"model","owned_by":"openai"}]}
```

Ollama（`GET /api/tags`，本机 0.40.16 实测）：

```json
{"models":[{"name":"qwen2.5:0.5b","model":"qwen2.5:0.5b","size":398000000,
            "digest":"...","details":{"format":"gguf","family":"qwen2",
            "parameter_size":"0.49403M","quantization_level":"Q4_K_M"},
            "capabilities":["completion","tools"]}]}
```

云端模型条目额外有 `remote_host` / `remote_model` / `context_length`；
`capabilities` 只在条目上出现（`/api/show` 里也有）。

## 5. 模型能力

`POST /api/show` 返回 `capabilities: Vec<String>`（本机 `qwen2.5:0.5b` →
`["completion","tools"]`），以及 `details.{family,parameter_size,quantization_level}`、
`model_info."<arch>.context_length"`、`parameters` 里的 `num_ctx`。

Zed 的 `ModelShow`（`crates/ollama/src/ollama.rs:190`）自己写了个 `Deserialize`
来同时检查 `parameters`（解析 `num_ctx` 行）与 `model_info`（`<arch>.context_length`），
`context_length = num_ctx.or(...)`。

## 6. 本仓（aa-ai）对应

`aa_ai_ollama`：

- `chat::request`：`ChatRequest{model,messages,stream,think,format,options,tools}`；
  工具结果走独立 `role:"tool"` 消息，用 `tool_name` 关联（从历史里的
  `ToolCall{id,name}` 反查）。
- `chat::response::StreamDecoder`：一行一个对象；`message.thinking` →
  `CompletionEvent::Reasoning`；`done_reason` 与「本响应是否出现过 tool_calls」
  共同决定 `StopReason`（`length` 优先）。
- 工具调用没有 `id` 时合成 `call_N`，因为 canonical `ContentPart::ToolCall.id`
  是必填，agent loop 也靠它把结果对回去。