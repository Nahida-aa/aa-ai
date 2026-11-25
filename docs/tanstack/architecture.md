# Zed ↔ TanStack AI 包对应 + 架构 / 性能分析 + v2 选型

> 调研日期：2026-11-10
> 背景：决定「库 + 应用 agent」用 TS 写实验、Rust 写生产；**只支持 ACP v2，不优先 v1**。

## 1. Zed crate ↔ TanStack AI 包对应

先说结论：**Zed 是一个 monolith 应用（agent 是 app 内的一块 UI），TanStack AI 是一组库（agent 是 library 消费者组装出来的）。** 所以对应关系是「功能块」而不是「包对包」。

**Zed 的 LLM 调用层本身就是库形态**（和 TanStack 同构）：`LanguageModelRegistry` 按 `provider_id + model_id` 解析 provider，每个 provider 实现 `stream_completion(&model, request)`（`request` 按值传入）→ 流式 `LanguageModelCompletionEvent`（`TextDelta`/`ReasoningDelta`/`ToolUse`/`FinishReason`）。这套 ≈ TanStack 的 `chat({ adapter: openaiText(...) })` + AG-UI `StreamChunk`。所以「Zed ↔ TanStack」要分两半看：

- **Zed 内置 agent 调 LLM**（`crates/agent` + `language_model_core`）≈ TanStack 的 `ai` 核心 + LLM provider adapter
- **Zed 接外部 ACP agent**（`agent_servers` + `acp_thread`）≈ TanStack 的 `ai-acp` client 侧 + harness adapter + 框架包 UI

> ⚠️ 2026-11-10 修正：原表两处想当然（`acp_tools` 实为调试面板、`agent_servers` 对应 harness 而非 provider），已按代码核对重写，详见 `docs/zed/provider-mapping.md`。

| Zed（app 内） | 职责 | 对应 TanStack AI 包 |
| --- | --- | --- |
| `crates/agent` + `language_model_core` | **内置 agent 调 LLM**：`LanguageModelRegistry` 按 provider+model 解析，`provider.stream_completion(&model, request)` 流式（`thread.rs:3080`） | `ai`（`chat()` + `toolDefinition` + 流）+ 各 **LLM provider adapter**（`ai-openai`/`ai-gemini`/…） |
| `crates/agent_servers`（`AcpConnection`/`AcpV2Connection`） | **外部 agent 接入面**：ACP 连接管理（v1/v2、stdio/WS）。驱动的是完整 coding agent（agent 内部自己调 LLM + 跑工具），**不是调模型** | `ai-acp`（client 侧）+ **harness adapter**（`ai-grok-build` 等）—— 注意：这是「驱动 agent」类，**不是 provider** |
| `crates/acp_thread`（`AcpThread` + `ToolCall`） | ACP 会话接进 agent 面板；**渲染 agent 报上来的工具调用**（`ToolCall`，`acp_thread.rs:1181`，含 `raw_input`/`raw_output`/`locations`/`authorization`） | 框架包（`ai-react`/…）+ `ai` 的工具结果消费（client 只渲染，不执行） |
| `crates/acp_tools` | **ACP 调试日志面板**（`OpenAcpLogs` dev 动作，渲染某连接的 incoming/outgoing JSON-RPC 请求） | 无直接对应（`ai-devtools` 概念近但范围不同） |
| `crates/language_model/fake_provider` | 假 LLM provider（测试用：`FakeLanguageModelProvider` + `send_text`/`end_stream`） | 手搓 mock adapter |
| （Zed 整个 app） | agent 状态、会话历史、持久化 | `ai-persistence` + `ai-memory` + `ai-compaction` |

**TanStack 的 adapter 分两层（不是「两类 provider」）：**

1. **LLM provider adapter**（`ai-openai`/`ai-gemini`/`ai-ollama`/…）：`chat()` 后端，**调模型**。Zed 内置 agent 侧（`crates/agent` + `language_model_core`）对应这一层。
2. **Harness adapter**（`ai-grok-build`、`ai-acp` client 侧）：`chat()` 后端，**驱动一个完整 coding agent**（agent 内部自己调模型 + 跑工具）。Zed 的外部 agent 接入面（`agent_servers`）对应这一层，**不叫 provider**。

**工具归属（ACP 边界）**：ACP 的 `tool_call`/`tool_call_update` 是 **agent → client 的通知**（agent 执行工具后报给 client 渲染 + 审批）。**client 不能提供工具给 agent 调用**——那是 MCP 的事（TanStack 用 `ai-mcp` 把 host tools 桥接成 ACP 内的 MCP server，即 `BRIDGED_MCP_SERVER_NAME`）。

**关键差异：Zed 把「agent 连接」和「agent 会话 UI」拆在两个 crate（`agent_servers` 和 `acp_thread`），而 TanStack AI 把「连接/会话」都放在 `ai-client`，provider 差异用 adapter 模式抹平。** 也就是说：

- **Zed** = 应用层 monolith。你只能改它的 agent UI 行为，不能拿去做自己的 agent 产品。
- **TanStack AI** = 库层 composable。`ai` 是核心，adapter 插拔，框架包可选。你写应用时按需 `pnpm add`。

### TanStack AI 更散的原因

- **树摇（tree-shake）**：adapter 按 activity 切分（`openaiText` 只含 chat，`falVideo` 只含 video），用户只 import 用得到的。
- **库职责分离**：`ai`（核心协议/工具/流）/ `ai-client`（客户端状态机）/ `ai-harness`（长会话 agent）/ `ai-persistence`（存储）/ `ai-orchestration`（durable 编排）各自独立，可单独演进。
- **多框架**：React/Solid/Vue/Svelte/Preact/Angular 各一个包，互不依赖。

## 2. 架构 / 性能分析（内存视角，不考虑语言差距）

### 2.1 TanStack AI 是「事件流 + 中间件」，内存占用取决于你组装了什么

`chat()` 的核心是一个 **async generator 流**（AG-UI `StreamChunk`），不是把整个对话驻留内存：

```
messages（只驻留当前线程需要的）
  → adapter.connect() 产出 StreamChunk（按需 yield，不缓冲整个流）
  → middleware 管道（每个中间件可 transform chunk，O(1) 内存）
  → client 端累积 thread（这个会随对话长度增长，是唯一随时间涨的内存）
```

**内存随时间涨的部分**：
- `ai-client` 的 thread（消息历史）—— 长对话会涨，`ai-compaction` 就是干这个的
- `ai-persistence` 的 session 存储 —— 看后端（memory 就吃内存，sqlite 就不吃）
- `ai-harness` 的 session index —— 看开了多少 session

**内存 O(1) 的部分**：
- 流式 chunk（yield 即释放）
- 工具执行（一次性的）
- adapter（无状态）

### 2.2 Zed 是「应用」，内存模型完全不同

Zed 的 agent 子系统驻留在 **GPU/UI 线程 + agent 状态**里，且 Zed 同时跑整个编辑器（LSP、文件、终端、…）。**Zed 的内存不是你能优化的**——它是 app，你只能选择「用不用 Zed 验证」。

### 2.3 对你「写库 + 写应用 agent」的内存建议

你要做的两件事内存画像不同：

| 产物 | 内存模型 | 关键决策 |
| --- | --- | --- |
| **库**（你的 agent 核心） | 应该 **O(1) per turn**：不驻留对话，靠 persistence 后端卸掉历史 | 把「对话状态」从库核心剥离，做成可插拔的 persistence（对标 `ai-persistence` 的接口设计，但实现可以 memory/文件/sqlite） |
| **应用 agent**（你的 CLI） | 随 session 数 + 对话长度涨 | 限制并发 session；长对话用 compaction；工具执行用流式不缓冲 |

**对标 TanStack 的取舍**：
- `ai-harness` 把 session 状态放在内存 + 可插拔 persistence —— 这是对的，**你的库也应该这么分**。
- `ai-orchestration` 引入 TanStack Workflow（durable execution）—— **不要在生产 agent 里默认开**。Workflow 的 runStore 会驻留所有 run 状态（重试、恢复、审批暂停），内存随历史 run 数线性涨。**只在需要「agent 调用要可恢复 / 跨重启」时才用**，否则普通 agent 不用它。
- `ai-compaction` 是长对话内存的解药 —— 你的 agent 迟早要接。

**一句话**：内存友好的关键是 **状态外置（persistence）+ 流式处理（不缓冲 chunk）+ 长对话压缩（compaction）**。TanStack AI 这三件都提供了，你的库学它的接口、实现按自己需要选（比如 persistence 默认用文件而不是 memory）。

## 3. `ai-harness` 是什么

位置：`packages/ai-harness`，**0.0.0 实验性**（README 明确 WARNING：minor release 可改 API）。

**一句话**：`chat()` 答一次请求；`ai-harness` 开一条**跨多 turn 的长会话**，期间可收新消息、跑 typed agent（代码里的 agent）、加载 plugin（加工具/提示词/中间件）。

核心概念（`src/index.ts` 导出）：
- `defineHarness({ name, adapter, agents })` → 一个 harness（长会话 + 一组 typed agent）
- `createHarnessHost({ persistence })` → host，管理多个 session
- `host.open(harness, { threadId })` → `HarnessSession`，可 `send()` / `agents.<name>.run()` / `cancel()`
- 内部：`session/`（会话状态机）、`waits.ts`（工具审批/打断）、`handoff.ts`（agent 间交接）、`plugins.ts`、`durable-tool.ts`、`worker.ts`

**谁在用它**（生产依赖，不是没人用）：
- `ai-acp`（peer dep，optional）—— **ACP agent 侧就架在它上面**（`createAcpAgent` 把 `HarnessSession` 映射成 ACP 会话）
- `ai-code-mode`（peer dep）—— Code Mode 的隔离执行靠 harness
- `ai-sandbox`（peer dep）
- `ai-mcp`、`ai-skills`、`ai-computer-use`
- `ai-harness-cli` —— **这就是你要的「ACP CLI」**：`runCli` 提供 line 模式 / print 模式（CI）/ NDJSON / **ACP 模式（给编辑器）** / HTTP server

**对你的意义**：`ai-harness-cli` 的 ACP 模式 = 把 TanStack harness 暴露成 ACP agent，**但它基于 v2 实验性 SDK**（`ai-acp` 的 `./agent` 入口用 `@agentclientprotocol/sdk/experimental/v2`）。既然你只支持 v2，这条链路反而是对你最相关的参考。

## 4. `ai-orchestration` 是什么

位置：`packages/ai-orchestration`，0.0.0。

**一句话**：把 TanStack AI 的 agent 调用接到 **TanStack Workflow**（durable execution，另一个独立库 `@tanstack/workflow-core`）上，得到**可恢复、可重试、可审批暂停**的 agent 调用。

核心：
- `defineAgent({ input, output, run })` → 一个 agent step（run 里可发 `chat({ stream: true })`）
- `createWorkflow({...}).middleware([agentMiddleware()]).handler(ctx => ctx.ai.agent('name', agent, input))`
- `runWorkflow({ workflow, runStore, input })` → 返回事件流
- `toAIStream(workflowEvents)` → 转成 TanStack AI 的 AG-UI `StreamChunk`

**它不做什么**（README 明确）：Workflow 拥有执行/重放/重试/审批/信号/截止时间/存储/租约；本包**只定义 agent-step 的人体工学 + 把 Workflow 事件映射成 AG-UI 流**。AI tool 审批在 agent 调用内暂不支持，要在 agent 调用之间用 `ctx.approve()`。

**生产状态**：
- 0.0.0，依赖 `@tanstack/workflow-core`（TanStack 另一个库）
- **仓库内没有任何其它包依赖它**（`grep` 0 个 consumer），docs 也没提
- **所以你的猜测对了一半**：它确实还没被 TanStack AI 用于生产，**但原因不是「被抛弃」，而是它依赖的 TanStack Workflow 还新，且 durable execution 是可选高级特性**（不是每个 agent 都要跨重启恢复）。它是「能力库」，不是「默认路径」。

**对你的意义**：你的 agent **默认不用它**。只有当你要「agent 调用本身是 durable 的（崩溃了能恢复、审批可以暂停几小时再续）」时才接。对标 TanStack：它是 `ai-harness` 的 durable 增强层，不是替代品。

## 5. 只支持 v2 的选型影响（重要）

你决定**只支持 v2，不优先 v1**，这改变前文 `acp.md` 的几条结论，需要修正：

### 5.1 Zed 的 v2 是 beta

前文说「Zed 默认认 v1 agent，v2 要开 `acp-beta`」——**对你只支持 v2 的方案，你要在 Zed 里开 `acp-beta` feature flag**。这是已知代价：Zed 的 v2 client 还在 beta，但 `AcpV2Connection` 已经完整内置（`crates/agent_servers/src/acp/v2.rs`），你写 agent 时直接对着 v2 schema 实现即可，Zed 侧验证要开 flag。

### 5.2 TanStack 的 agent 侧恰好是 v2

`ai-acp/src/agent/index.ts`（`createAcpAgent`/`serveAcp`）基于 `@agentclientprotocol/sdk/experimental/v2`。**既然你只支持 v2，TanStack 这条链路对你反而是最直接的参考**（虽然它架在 harness 上，但你只取「ACP v2 会话怎么驱动 harness session」这一段当蓝本）。

### 5.3 修正后的最小方法集（v2，写 agent 视角）

v2 比 v1 多了：`session/resume`、`session/list`、`session/delete`、`elicitation/create`、`state_update`、receipt-style prompt、`meta` patch。你写 v2 agent 的最小集：

1. `initialize` → 回 `protocolVersion: 2` + `agentCapabilities`
2. `session/new` → `{ sessionId }`
3. `session/prompt` → v2 是 **receipt 式**：prompt 先回 ack（`messageId`），turn 通过 `session/update` 流式走（`agent_message_chunk`/`tool_call_update`/`state_update`），最后 `stopReason`
4. `session/cancel`
5. （可选，按需）`session/resume`、`session/list`、`session/delete`、`elicitation/create`（form/url）

**风险**：v2 还是 draft，schema 会动。你的 agent 要 **锁死一个 v2 版本**（SDK 用 `~` 或 `=` 锁 patch），升级前跑 ACP 官方 testing 套件。

### 5.4 验证脚本要按 v2 发

前文 `verify.ts` 思路里 `initialize` 发 `protocolVersion: 1` 改成 `2`；`session/prompt` 要处理 receipt（先发一个 ack 才继续流式）。验证时 Zed 开 `acp-beta`，或先用纯脚本验协议层。

## 6. 给你的库 + 应用 agent 的最终形态建议（只 v2）

```
lib (你写的, TS 实验 / Rust 生产)
├── 核心：ACP v2 agent 协议实现
│   ├── initialize / session/* / prompt / cancel / request_permission / elicitation
│   ├── 状态：可插拔 persistence（memory 默认，文件/sqlite 可选）
│   ├── 流式：chunk yield 不缓冲
│   └── compaction：长对话压缩（可选，后期）
└── 应用：CLI agent
    ├── --acp 启动 v2 agent（stdio NDJSON）
    ├── LLM 层：fake（固定文本流）起步，后换真 provider
    └── 工具：本地直接做（读写文件/跑命令），不声明 fs/terminal client 能力
```

**不接** `ai-orchestration`（除非要 durable）。**可参考** TanStack `ai-acp` agent 侧的 v2 会话驱动模式，但库接口你自己定。

**Zed 验证**：settings.json `agent_servers` 配你的 CLI，Zed 开 `acp-beta`。
