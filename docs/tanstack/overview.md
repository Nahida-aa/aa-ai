# TanStack AI 仓库调研笔记

> 仓库位置：`/home/aa/repos/lib_ls/learn_ls/ai`（TanStack/ai 官方克隆）
> 调研日期：2026-11-10

## 1. 项目是什么

**TanStack AI** — 类型安全、provider-agnostic 的 TypeScript SDK。覆盖：

- 流式 chat（typed messages / tool calls / reasoning parts）
- 类型安全的 tools（`toolDefinition()`，输入输出 schema 校验）
- Structured outputs（JSON Schema / Zod / ArkType / Valibot）
- 多模态（image / audio / video / document）
- Realtime 语音、media generation
- **Code Mode**：LLM 在隔离沙箱里写并执行 TypeScript 来编排 tools
- Devtools / observability（OpenTelemetry）
- 框架集成：React / Solid / Vue / Svelte / Preact / Angular

核心架构模式：

```
chat({ adapter, messages, ... })
  → adapter = <provider><activity>(model)   // 例如 openaiText('gpt-5.2')
  → StreamChunk 流（AG-UI 事件协议：RUN_STARTED / TEXT_MESSAGE_* / TOOL_CALL_* / RUN_FINISHED）
```

- adapter 可按 activity tree-shake（`openaiText` 只含 chat，`falVideo` 只含 video）
- 中间件（middleware）钩子：chat config / chunks / tool calls / usage / errors / structured outputs
- `withSandbox` 中间件：把 chat 跑在沙箱里（docker / cloudflare / e2b / ...）

## 2. packages/ 概览（79 个包，按类别）

| 类别 | 包 |
| --- | --- |
| 核心 | `ai`（主包）、`ai-client`（headless 客户端）、`ai-utils` |
| 框架 | `ai-react` / `ai-solid` / `ai-vue` / `ai-svelte` / `ai-preact` / `ai-angular` / `ai-remix`，各自带 `/ui` 无头 UI 子入口 |
| Provider 适配器 | `ai-openai`、`ai-anthropic`、`ai-gemini`、`ai-vertex`、`ai-grok`、`ai-groq`、`ai-ollama`、`ai-cloudflare`、`ai-byteplus`、`ai-fal`、`ai-openrouter`、`ai-cohere`、`ai-mistral`、`ai-bedrock`、`ai-mcp`、`ai-elevenlabs`、`ai-worldlabs`、`ai-reactor`、`ai-lovable`、`ai-grok-build`、`ai-opencode`、`ai-codex`、`ai-claude-code`、`ai-ollaya`、`ai-octane`、`ai-harness`、`ai-llmgateway`、`ai-vercel-gateway`、`openai-base` |
| 沙箱 | `ai-sandbox`（抽象）+ 12 个后端：`docker`、`cloudflare`、`e2b`、`daytona`、`vercel`、`railway`、`sprites`、`blaxel`、`boxd`、`upstash-box`、`local-process`；`ai-isolate-*`（node / quickjs / cloudflare / daytona / e2b） |
| Agent 基础设施 | `ai-orchestration`、`ai-memory`、`ai-persistence`、`ai-compaction`、`ai-computer-use`、`ai-computer-playwright`、`ai-computer-local`、`ai-computer-sandbox` |
| 其他 | `ai-code-mode` / `ai-code-mode-snippets`、`ai-devtools` + 4 个框架 devtools、`ai-event-client`、`ai-durable-stream`、`ai-typesafe`、`ai-skills`、`ai-harness-cli` |

**与 ACP 相关**：`ai-acp`（见下节）。

## 3. `@tanstack/ai-acp`：ACP 基础设施（重点）

位置：`packages/ai-acp`，v0.3.27。依赖 `@agentclientprotocol/sdk`（JSON-RPC client）。

### 3.1 它解决什么

把「ACP 编码 agent CLI（grok、gemini --acp …）」变成 `chat()` 的后端（在沙箱内运行）。分两层：

- **harness 侧（client 角色）**：spawn CLI → 开 ACP transport（stdio NDJSON 或 WebSocket）→ `startAcpSession`（initialize → authenticate → newSession/loadSession → prompt）→ 把 ACP `sessionUpdate` 事件翻译成 AG-UI `StreamChunk`
- **agent 侧（`./agent` 子入口）**：把一个 TanStack harness **暴露为 ACP agent**（给 Zed 等编辑器当子进程驱动）

### 3.2 主要 API（导出自 `src/index.ts`）

| 层 | 导出 |
| --- | --- |
| 会话 | `startAcpSession(options)` → `{ sessionId, resumed, prompt, cancel, dispose }` |
| 流翻译 | `translateAcpStream(events, ctx)` → `AsyncIterable<StreamChunk>`；`AsyncQueue`；`matchBridgedToolName` |
| 传输 | `spawnHandleToAcpTransport`（stdio）、`connectAcpWebSocket`、`startAcpServerInSandbox`（WebSocket/serve） |
| 权限 | `resolvePermission`、`resolveInteractivePermission`（三种模式：default / acceptEdits / bypassPermissions） |
| 便捷 adapter | `acpCompatible(config)` / `acpCompatibleText` — 给「没有专门 adapter 包」的 ACP CLI 一行接入 `chat()` |
| 提示词 | `buildAcpPrompt(messages, sessionId)` |

ACP → AG-UI 事件映射（`stream/translate`）：

| ACP sessionUpdate | StreamChunk |
| --- | --- |
| `agent_message_chunk` | `TEXT_MESSAGE_*` |
| `agent_thought_chunk` | `REASONING_*` |
| `tool_call` / `tool_call_update` | `TOOL_CALL_*` + `TOOL_CALL_RESULT` |
| `plan` | `CUSTOM`（`<name>.plan`） |
| stopReason `refusal` | `RUN_ERROR` |
| 其他 stop reason | `RUN_FINISHED` + usage |

### 3.3 Agent 侧（`./agent` 子入口）— 写自己 ACP agent 的参考

`packages/ai-acp/src/agent/index.ts`，基于 `@agentclientprotocol/sdk/experimental/v2`（**ACP v2 是 draft**）：

- `createAcpAgent({ host, harness, user })` → 把 TanStack `HarnessSession` 映射到 ACP 会话
- `serveAcp(options)` → 默认走 **stdio NDJSON**（编辑器当子进程 spawn 的形态），也可传自定义 `Stream`
- 处理的请求：`initialize`、`session/new`、`session/list`、`session/resume`、`session/prompt`、`session/close`、`session/cancel`
- 打断/等待 → 编辑器侧：approval → `session/request_permission`；question/human → `elicitation/create`（form）；sign-in → `elicitation/create`（url）
- 一个 agent app 只服务一个连接（多连接要 `new AcpServer({ createAgent: ... })`）

### 3.4 参考实现

- `@tanstack/ai-grok-build`：stdio + WebSocket 双传输 + 厂商 `extNotification`
- `@agentclientprotocol/sdk` examples（TS SDK 的 `src/examples`，可被 Zed 直接驱动）
- Gemini CLI 的 ACP agent 实现（生产级参考）

## 4. ACP 协议本身（agentclientprotocol.com）

- JSON-RPC 2.0 over stdio（NDJSON）；本地 agent = 编辑器子进程
- 生命周期：`initialize`（协商 protocolVersion + capabilities）→ `authenticate`（可选）→ `session/new` / `session/load` / `session/resume` → `session/prompt`（流式 `session/update`：agent_message_chunk、agent_thought_chunk、tool_call、plan、state_update）→ `stopReason`（end_turn / max_tokens / refusal / cancelled / error）
- client 回调 agent：`session/request_permission`（allow_once / allow_always / reject_once / reject_always）、`fs/read_text_file`、`fs/write_text_file`、`terminal/*`、`elicitation/create`
- 能力协商：`clientCapabilities`（fs / terminal / elicitation 各可独立声明支持与否；不支持 = agent 不得调用）
- 库：TS / Rust / Python / Java / Kotlin；**TS 和 Rust SDK 已 1.0**
- 版本：**v1 stable**（当前主版本）；**v2 是 draft**（新 prompt lifecycle、message update、background work 等）；v2 有完整迁移文档
- 官方测试：`libraries/testing` 页有 agent 实现合规检查
- Zed 是 ACP client；注册表：`agentclientprotocol.com/get-started/registry`

## 5. 其他值得留意的包

- `ai-harness` / `ai-harness-cli`：harness 抽象（agent 编排的底层），`ai-acp` agent 侧就架在它上面
- `ai-mcp`：MCP 桥接
- `ai-orchestration` / `ai-memory` / `ai-persistence` / `ai-compaction`：agent 状态与编排
- `ai-typesafe`：per-model 类型收窄
- 贡献流程：`CONTRIBUTING.md` + `pnpm test:pr`（sherif/knip/docs/eslint/lib/types/build）

## 6. 自研 ACP Agent CLI 计划（v0）

> 目标：先写一个**支持 ACP 的 CLI agent**，在支持 ACP 的客户端（Zed 等）里验证。

### 6.1 技术选型

- 语言：**TypeScript + Node**（生态与 Zed 客户端都最顺）
- SDK：`@agentclientprotocol/sdk`
  - 起步用 **v1 stable 的 fluent API：`agent()` / `client()`**（`ClientSideConnection` / `AgentSideConnection` 已 deprecated，但新代码用 fluent API 更稳）
  - 想学新特性再看 v2 experimental（TanStack 的 agent 侧就是这么做的，但那是 draft，不推荐做 v0 基座）
- 参考：
  - SDK examples：`agentclientprotocol/typescript-sdk` 仓库 `src/examples`（终端可驱动 + Zed 可直接接）
  - 生产级 agent：Gemini CLI 的 `packages/cli/src/acp`
  - 本仓库 `packages/ai-acp/src/agent/index.ts`（harness→ACP agent 的完整映射，含权限/打断/elicitation）

### 6.2 最小实现（v0 范围）

必须实现（agent 侧请求处理）：

1. `initialize` → 返回 `protocolVersion` + `agentCapabilities`（v0 先不声明 fs / terminal / elicitation 能力，能力协商让编辑器知道不能用）
2. `session/new` → 生成 sessionId，返回
3. `session/prompt` → 收 user 消息，跑 LLM，流式发 `session/update`：
   - `agent_message_chunk`（正文，增量）
   - `agent_thought_chunk`（思考/推理，可选）
   - `tool_call` / `tool_call_update`（有工具时：in_progress → completed，带 rawInput / rawOutput）
   - 最后 resolve `stopReason: 'end_turn'`（+ 可选 usage）
4. `session/cancel`（notification）→ 中断当前 prompt

客户端会回调 agent 的（v0 先不实现，declared as unsupported）：

- `session/request_permission`（工具审批）— v0 可先本地放行或 auto-approve
- `fs/read_text_file` / `fs/write_text_file` / `terminal/*` — 不声明能力即可

传输：stdio NDJSON（`ndJsonStream`，stdin 读 / stdout 写；stderr 留给日志）。

### 6.3 建议的 CLI 骨架

```
my-agent/
├── src/
│   ├── cli.ts          # bin：解析 --acp 标志等，启动 ACP 服务
│   ├── agent.ts        # agent() 请求处理（initialize / session/*）
│   ├── llm.ts          # LLM 调用 + 流式
│   └── tools.ts        # 工具定义 + 执行（读文件/写文件/跑命令）
├── package.json        # "bin": { "my-agent": "dist/cli.js" }
└── zed-settings.json   # Zed 侧 agent 配置（command + args）
```

### 6.4 Zed 验证

Zed 的 `~/.config/zed/settings.json` 里配置 agent（Zed ≥ 0.20 内置 ACP client）：

```jsonc
{
  "agent": {
    "cli": {
      "command": "my-agent",       // 在 $PATH 里
      "args": ["--acp"],          // ACP 模式开关
      "env": { "MY_AGENT_API_KEY": "..." }
    }
  }
}
```

> 具体键名以 Zed 当前文档为准（`agent > cli`），核心就是 Zed spawn 你的 CLI、通过 stdio 说 JSON-RPC。

验证步骤：

1. Zed 开一个项目，agent 面板选你的 agent
2. 发一条消息 → 看 Zed 是否渲染增量文本
3. 触发一个工具调用 → 看 tool call UI（名称、参数、状态、结果）
4. 中途取消 → 看 stopReason `cancelled` 是否正常
5. 重新打开会话 → 看 `session/load` / `session/resume` 行为

### 6.5 后续路线

- v1：实现 `session/request_permission`（工具审批 UI 联动 Zed 的 allow/reject）
- v1：session 持久化 + `session/load` 恢复
- v2：`session/list` / `session/delete`
- v2：声明 fs 能力，让 agent 通过 client 读写编辑器打开的文件（而不是直接碰磁盘）
- 评估是否跟进 ACP v2 draft（等它 stable）
- 可选：把 agent 接到 TanStack AI 的 `ai-sandbox` / harness 体系里（即反过来当 `acpCompatible` 的 harness 用）
