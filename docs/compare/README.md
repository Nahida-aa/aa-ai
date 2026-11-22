# 对比记录：**库层**（aa-ai 自己的）

> 只记事实：类型、函数、注册点、配置形状、行号 + commit。「不做取舍、不给建议」。
> 应用层（CLI / 权限 / 会话 / ADE）的对比在 **aacode**：`ide_ls/aacode/docs/compare/`。

## 为什么分两处

见 `aacode/docs/compare/README.md`。一句话：复制必然漂移；决定发生在哪一层，事实就写在哪一层。

### 「既是库又是应用」的项目

像 **Zed** 这种应用几乎不依赖外部 AI 库（自己写 provider、wire DTO、agent loop），
它应该**同时按库和应用**看待。所以：

- Zed 的 `language_model_core` / `open_ai` / `anthropic` / `google_ai` → 本目录（库层事实）；
- Zed 的 `agent` / `agent_servers` / `acp_thread` → `aacode:docs/compare/`（应用层 + ADE 事实）。

不要因为「Zed 是个 app」就把 IR / wire / provider 抽象的事实写进应用侧。

## 索引

| 文件 | 内容 |
|---|---|
| [`content-part-ir.md`](content-part-ir.md) | 四家的 ContentPart IR：枚举定义、字段、私有变体在什么情况下真的被构造 |
| [`provider-extension.md`](provider-extension.md) | 四家怎么接长尾端点、怎么给常见厂商做区分度（三层：协议 / 常见厂商 / 长尾） |
| [`http-client.md`](http-client.md) | 四家的 HTTP 客户端：代理解析、`NO_PROXY`、连接/读取超时、流式 idle 超时的实现位置 |
| [`ollama-wire.md`](ollama-wire.md) | Ollama 原生 `/api/chat` 与 OpenAI chat completions 的逐字段差异（NDJSON、`thinking`、`tool_name`、`done_reason`） |
| [`openrouter-wire.md`](openrouter-wire.md) | OpenRouter 与 OpenAI chat completions 的逐字段差异（`reasoning`/`reasoning_details`、数字错误码、流内错误、`pricing`、限流） |

## 源码克隆与 commit

| 项目 | 路径 | commit |
|---|---|---|
| Zed | `/home/aa/repos/ide_ls/learn_ls/zed` | `4ed72e0135` |
| rig | `/home/aa/repos/lib_ls/learn_ls/rig` | `6e13e8c` |
| genai | `/home/aa/repos/lib_ls/learn_ls/genai` | `d852bfd`（v0.7.5） |
| TanStack AI | `/home/aa/repos/lib_ls/learn_ls/ai` | `6059872f` |
| opencode | `/home/aa/repos/lib_ls/learn_ls/opencode` | `d7ae4f2e3c` |

## 待补

- 各家的「能力位」建模：哪些放模型、哪些放请求、哪些放端点（对应我们的
  [`0006`](../decisions/0006-wire-request-shape.md) D4 / [0008](../decisions/0008-provider-tiers.md)）
- 流式响应的事件粒度：文本/思考/工具调用的增量边界（对应 [`0007`](../decisions/0007-response-mapping.md)）
- agent loop 的所有权：库替你循环（TanStack `chat()`）vs 应用自己写（Zed `Thread` / 我们 `aa_agent`）