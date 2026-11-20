//! Ollama 原生 `/api/chat` 的 wire 形状 + 编解码。
//!
//! 为什么单独一个协议目录：它**不是** OpenAI 兼容形状，尽管两者看起来相似。
//! 逐条差异（全部有 API 文档出处，见 [`docs/compare/ollama-wire.md`](../../../../../docs/compare/ollama-wire.md)）：
//!
//! | | OpenAI chat completions | Ollama `/api/chat` |
//! |---|---|---|
//! | 流式分帧 | SSE（`data: {...}\n\n`、`[DONE]`） | NDJSON（一行一个完整 JSON 对象） |
//! | 工具参数 | `arguments` 是 **JSON 字符串** | `arguments` 是 **对象** |
//! | 工具调用 id | 必须有 `id` | 多数模型不给 `id` |
//! | 工具结果消息 | `role:"tool"` + `tool_call_id` | `role:"tool"` + `tool_name`（**没有 id**） |
//! | 系统消息 | `system` / `developer` | 只有 `system` |
//! | 思考 | 无（兼容端点私下塞 `reasoning_content`） | `message.thinking`（官方字段） |
//! | 结束原因 | 每帧 `choices[].finish_reason` | 最后一帧的 `done_reason` |
//! | 用量 | `usage.prompt_tokens` / `completion_tokens` | `prompt_eval_count` / `eval_count` |
//! | 输出上限 | `max_tokens` / `max_completion_tokens` | `options.num_predict` |
//! | 结构化输出 | `response_format` | `format`（`"json"` 或裸 JSON Schema） |
//! | 模型列表 | `GET /models` | `GET /api/tags` |
//!
//! 所以「把 Ollama 当 OpenAI 兼容端点」和「接原生 `/api/chat`」是两件事，配置里也是
//! 两个 section（`openai_compatible.ollama` 走 `/v1`，`ollama` 走 `/api/chat`）。
//!
//! 与 `aa_ai_openai::chat_completion` 一样，请求侧只 `Serialize`、响应侧只
//! `Deserialize`，两边不共用结构体。

pub mod request;
pub mod response;
