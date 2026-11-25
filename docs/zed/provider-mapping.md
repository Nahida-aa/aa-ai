# Zed 的 provider 架构 + 对 TanStack 映射的修正

> 调研日期：2026-11-10
> 修正前文 `architecture.md` 里两处想当然的映射。

## 1. Zed 有没有 `chat({ adapter: openaiText(...) })` 的等价物？

**有，而且就是库形态的 provider adapter 架构，和 TanStack 同构。**

Zed 的 LLM 调用不经过 ACP——ACP 是「外部 agent CLI」的路径。Zed 内置 agent（gemini/cursor…）走的是 `crates/language_model_core`：

```rust
// Zed 内置 agent 实际调 LLM 的方式（crates/agent/src/thread.rs:3080）
let provider = ...; // LanguageModelProvider
let stream = provider.stream_completion(&model, request, cx).await?; // request 按值传入
for await (chunk in stream) { ... }   // 流式 LanguageModelCompletionEvent
```

- `LanguageModelRegistry::global(cx)`（GPUI 全局）= provider 注册中心，按 `provider_id` + `model_id` 解析出 `(provider, model)`
- 每个内置 provider（gemini、openai、anthropic…）实现 `stream_completion(&model, request)`（`request: LanguageModelRequest` 按值传入）→ `Stream<LanguageModelCompletionEvent>`
- 事件映射：`TextDelta` / `ReasoningDelta` / `ToolUse` / `FinishReason` —— 和 TanStack AG-UI 的 `TEXT_MESSAGE_CONTENT` / `REASONING_*` / `TOOL_CALL_*` / `finishReason` 一一对应

**等价表（修正版）**：

> ⚠️ 2026-11-12 修正：之前把 `stream_completion` 直接类比 `chat({ adapter })` 是**粒度错了一格**。
> `stream_completion` 只是「一次 LLM 调用」，**不是 agent loop**；agent loop（工具循环）在 Zed 的 `Thread` 里，
> 在 TanStack 里整个封装进 `chat()`。

| Zed | TanStack AI | 说明 |
|---|---|---|
| `stream_completion(&model, req)` | `chat()` **内部那次** `adapter.connect()` | 一次 LLM 调用（数据），不含循环 |
| `Thread`（`crates/agent` 的工具循环） | `chat({ adapter, tools })` **整个** | agent loop（循环） |
| `LanguageModelRegistry`（按 provider+model 解析） | `adapter` 工厂（`openaiText` / `geminiText` …） | 一样 |
| `LanguageModelCompletionEvent` 流 | AG-UI `StreamChunk` 流 | 一样 |
| `fake_provider` | 手搓 mock adapter | 一样 |

**关键差异（封装粒度）**：
- **TanStack 把「一次 LLM 调用 + 工具循环」封装进 `chat()` 一次**（库替你循环）
- **Zed 拆开**：底层 `stream_completion` 只管一次 LLM（数据层，fork 我们的对象），上层 `Thread` 自己写工具循环（agent 层）

**所以「Zed 对应 TanStack 哪几个包」的准确答案**：

- **Zed 内置 agent 调 LLM 的底层**（`language_model_core` 的 `stream_completion`）≈ TanStack `chat()` 里那次 LLM 调用，**不是整个 `chat()`**
- **Zed 的 agent loop**（`crates/agent` 的 `Thread`）≈ TanStack `chat({ tools })` 整个
- **Zed 接外部 ACP agent**（`agent_servers` + `acp_thread`）≈ TanStack `ai-acp`（client 侧）+ 框架包 UI

**对 fork 的含义**：我们 fork Zed 的 `language_model_core` = 拿到「一次 LLM 调用」数据层，**agent loop 要在 `aa_agent` 自己写**（这是和 TanStack `chat()` 的关键区别）。

## 2. 修正前文两处错误映射

前文 `architecture.md` §1 的表格有两处想当然，现在用代码核对后修正：

### 2.1 `crates/acp_tools` ≠ 「ACP 工具调用」

**`crates/acp_tools` 实际是一个 ACP 调试日志面板**（看 `crates/acp_tools/src/acp_tools.rs`）：

- `actions!(dev, [OpenAcpLogs])` —— 开发者动作，打开 ACP 连接日志
- `AcpTools` 结构体持 `watched_connections` / `incoming_request_methods` / `outgoing_request_methods` —— 监视某条 ACP 连接，渲染收发的 JSON-RPC 请求/响应
- `AcpToolsToolbarItemView` —— 在 agent 面板工具栏里显示 ACP 连接状态

**它不执行工具调用，不是 ACP tool 的对应物。** 真正渲染 ACP 工具调用的是 `crates/acp_thread/src/acp_thread.rs` 的 `ToolCall`（`id` / `raw_input` / `raw_output` / `locations` / `authorization`，`acp_thread.rs:1181` 起），在 agent 线程时间线里展示 agent 报上来的工具调用。

### 2.2 工具归属：ACP 里工具由 agent 侧执行，client 只收到结果

你指出的完全正确——**ACP 不能让 client 提供工具给 agent 调用**。我核对代码后确认：

- ACP 的 `tool_call` / `tool_call_update` 是 **agent → client 的通知**（agent 执行工具后把「调了什么、参数、结果」报给 client 渲染）
- client（Zed）只能做：(a) 渲染工具调用 UI；(b) `session/request_permission` 审批；(c) 对 agent 的终端/文件操作做沙箱兜底
- 要 client 反向提供工具给 agent，那是 **MCP** 的事，不是 ACP 的事（TanStack 里是 `ai-mcp` 把 host tools 桥接成 ACP 内的 MCP server，见 `ai-acp` 的 `matchBridgedToolName` / `BRIDGED_MCP_SERVER_NAME`）

**所以正确映射**：

| Zed | TanStack AI |
|---|---|
| `crates/acp_thread` 的 `ToolCall`（渲染 agent 报上来的工具调用） | `ai` 的 `toolDefinition` 输出 + 工具结果（client 侧消费，不执行） |
| `crates/acp_tools`（ACP 调试日志面板） | （TanStack 无直接对应，`ai-devtools` 是观测面板，概念近但范围不同） |

## 3. 修正后的完整对应表（Zed ↔ TanStack AI）

> 「功能块对包」，不是包对包。Zed 是 app（monolith），TanStack 是库（composable）。

| Zed（app 内） | 职责 | 对应 TanStack AI 包 |
|---|---|---|
| `crates/agent` + `language_model_core` | 内置 agent：按 provider+model 解析，`stream_completion` 流式调 LLM | `ai`（`chat()` + `toolDefinition` + 流）+ 各 provider adapter（`ai-openai`/`ai-gemini`/…） |
| `crates/agent_servers`（`AcpConnection`/`AcpV2Connection`） | 外部 agent 接入面：ACP 连接管理（v1/v2、stdio/WS） | `ai-acp`（client 侧，把 ACP CLI 变成 chat 后端） |
| `crates/acp_thread`（`AcpThread` + `ToolCall`） | ACP 会话接进 agent 面板；渲染 agent 报的工具调用 | 框架包（`ai-react`/…）+ `ai` 的工具结果消费 |
| `crates/acp_tools` | ACP 连接调试日志面板 | 无直接对应（`ai-devtools` 概念近） |
| Zed app 的会话历史/持久化/压缩 | 长对话状态管理 | `ai-persistence` + `ai-memory` + `ai-compaction` |
| `crates/language_model/fake_provider` | 假 LLM provider（测试） | 手搓 mock adapter |

### 关于「TanStack 有两类提供商」

**不完全是**。准确说法：

- TanStack 的 **provider adapter 都指向 LLM**（`ai-openai`/`ai-gemini`/`ai-ollama`/`ai-cloudflare`…）——这是「调模型」的一类
- 「接外部 agent CLI」是 **另一类 adapter**，但 TanStack 把它归到 **harness adapter**（`ai-grok-build` 等）+ `ai-acp` 的 client 侧，不是「provider」——它是「驱动一个完整 coding agent」，不是「调一个模型」
- 所以 TanStack 的 adapter 分两层：
  1. **LLM provider adapter**（`ai-openai`、`ai-gemini`…）：`chat()` 的后端，调模型
  2. **Harness adapter**（`ai-grok-build`、`ai-acp` client 侧）：`chat()` 的后端，驱动一个外部 agent（agent 内部自己调模型）

  Zed 的 `agent_servers`（接外部 ACP agent）对应的是第 2 类，不是「provider」。前文写「gemini/cursor 等内置 provider 对应各 provider adapter」是对的（内置 agent 确实是调 LLM），但 Zed 接外部 agent 的那一侧（`agent_servers`）对应 TanStack 的 harness/`ai-acp`，不是 provider。

## 4. 对你「库 + 应用 agent」的含义

- Zed 证明了「库形态的 provider adapter」是可行的：`LanguageModelRegistry` + `stream_completion` 就是 Zed 自己的 `chat({ adapter })`。你写库时，**LLM 层用 provider adapter 接口（对标 Zed 的 `stream_completion` / TanStack 的 `chat({ adapter })`）**，ACP 层单独做（对接外部 agent）。
- Zed 的 `fake_provider` 说明**假 LLM provider 是标配**（测试里直接 `FakeLanguageModelProvider` + `send_text`/`end_stream`）。你的库也应内置一个 fake provider（本仓：`aa_language_model::fake`，`fake` feature 门控；汇总 crate 转发同一类型），比 TanStack 的「手搓 mock adapter」更省事。
- 工具执行归属：你的 agent（应用侧）**自己执行工具**（读文件/写文件/跑命令），通过 ACP `tool_call` 报给 client（Zed 渲染 + 审批）。client 不提供工具——这正符合 ACP 的边界，也符合你「只支持 v2 agent」的决策。
