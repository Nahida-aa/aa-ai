# ACP v1/v2 与 Zed 使用现状 + 选型

> 调研日期：2026-11-10
> Zed 源码：`/home/aa/repos/ide_ls/learn_ls/zed`（官方克隆）
> 目标：TS 做实验 agent，Rust 做生产 agent；先用 Zed 验证。

## 1. TanStack AI 在这件事里的定位

**没有现成的「支持 ACP 的 CLI」**，只有给开发 ACP 连接用的工具库：

- `@tanstack/ai-acp` 的**主体是 client 侧**：把别人的 ACP CLI（grok、gemini --acp…）变成 TanStack `chat()` 的后端。与「写 agent」无关。
- `./agent` 子入口（`createAcpAgent` / `serveAcp`）是 **agent 侧**参考实现，但有两个限制：
  - 架在 TanStack 自有 `ai-harness` 抽象上（要先有 harness host + session），不是从零起步的模板
  - 基于 `@agentclientprotocol/sdk/experimental/v2`（draft）
- **结论**：写自己的 agent 用 **ACP 官方 TS SDK**（`@agentclientprotocol/sdk`）的 examples 起步，TanStack 这块当架构参考即可。

## 2. Zed 里的 ACP 实现（源码索引）

Rust crate：`agent-client-protocol`（`Cargo.toml` 第 538 行）：

```toml
agent-client-protocol = { version = "=5.1.0", features = ["schemars", "unstable", "unstable_protocol_v2"] }
```

**Zed 同时实现了 v1 和 v2 两套客户端**：

| 文件 | 类型 | 要点 |
| --- | --- | --- |
| `crates/agent_servers/src/acp/v1.rs` `AcpConnection` | v1 client（默认/稳定） | `stdio` spawn 子进程；`Lines::new(outgoing, incoming)` NDJSON 传输（~591 行）；`InitializeRequest::new(ProtocolVersion::V1)`（~640）；`MINIMUM_SUPPORTED_VERSION = V1`（~386），agent 回更低版本 → `UnsupportedVersion`（~653） |
| `crates/agent_servers/src/acp/v2.rs` `AcpV2Connection` | v2 client | `Client.v2()` + `with_v2_client_ingress`（~355）；`initialize` 回 `ProtocolVersion::V2` |
| `crates/agent_servers/src/acp.rs` | 公共 | `AcpDebugLog`（协议 debug 日志）、传输、session updates 归一化 |
| `crates/acp_thread/`（`AcpThread`） | UI 层 | ACP 会话接进 Zed agent 面板；`AgentConnection` trait 是所有 agent 的抽象（`crates/acp_thread/src/connection.rs` ~95 行起） |
| `crates/agent_servers/src/agent_servers.rs` / `custom.rs` | 入口 | Zed 内置 agent（gemini/cursor 等）+ 自定义 agent server 的注册 |

### 关键细节

- **v2 是 beta，opt-in**：`crates/feature_flags/src/flags.rs` 第 22–28 行 `AcpBetaFeatureFlag`（flag 名 `acp-beta`）。v1 客户端**只有开了 `acp-beta` 才会广播** unstable 的 client 能力（subagents、plan operations），见 `crates/agent_servers/src/acp/v1.rs` ~489–524 `client_capabilities_for_agent`。
- **Zed 广播的 client 能力（v1，`client_capabilities_for_agent`）**：
  - `fs.read_text_file(true)`、`fs.write_text_file(true)`
  - `terminal(true)` + `auth.terminal(true)`
  - `session.config_options(boolean)`、`session.compaction`、`session.notices`
  - `elicitation.form` + `elicitation.url`
  - `meta`: `terminal_output`、`terminal-auth`
  - beta 时额外：`subagents`、`plan`
  - **含义**：Zed 期望 agent 把「读写编辑器打开的文件」「跑终端命令」通过 ACP 回调做，而不是 agent 直接碰磁盘（不过 agent 也可以不声明这些能力，自己直接操作）。
- **v1 会话方法**（v1.rs 的 agent → client 请求处理）：`session/request_permission`、`fs/read_text_file`、`fs/write_text_file`、`terminal/*`、`elicitation/*`。
- **agent 配置入口**：`crates/project/src/agent_server_store.rs` 的 `CustomAgentServerSettings`：
  - `Custom { command }`：自定义命令（Zed spawn 它，走 stdio ACP）
  - `Registry { env }`：ACP Registry 里的 agent，可配 env
  - 对应 settings.json 里的 `agent_servers` 段。

### Zed 侧 settings.json 自定义 agent 示例

```jsonc
// ~/.config/zed/settings.json
{
  "agent_servers": {
    "my-agent": {
      "command": "my-agent",     // $PATH 里的可执行文件
      "args": ["--acp"],        // ACP 模式开关（你的 CLI 自己定）
      "env": { "OPENAI_API_KEY": "sk-..." }
    }
  }
}
```

Zed 会 spawn `my-agent --acp`，stdin/stdout 说 NDJSON JSON-RPC（v1）。

## 3. v1 vs v2 现状（写 agent 视角）

| | ACP v1 | ACP v2 |
| --- | --- | --- |
| 状态 | **stable**（SDK TS/Rust 均 1.0） | **draft**（有 RFD + 完整 schema + 迁移文档） |
| Zed 支持 | 默认（v1 client） | 内置 v2 client，但 agent 侧要 agent 协商 v2 |
| 传输 | stdio NDJSON / WebSocket（LSP 式） | 同左 + streamable HTTP 实验 |
| 核心方法 | `initialize`、`session/new`、`session/load`、`session/prompt`、`session/cancel`、`session/request_permission`（agent→client）、`fs/read_text_file`、`fs/write_text_file`、`terminal/*`、`authenticate`、`logout` | 新增/改变：`session/resume`、`session/list`、`session/delete`、`elicitation/create`（v1 已有，v2 强化 form/url 双模式）、receipt-style prompt（prompt 先 ack 再流式 `session/update`，含 `state_update`）、message update、background work、client fs/terminal 能力面、`meta` patch |
| 代表实现 | Gemini CLI（生产级）、Zed v1 client | TanStack `ai-acp` agent 侧（基于 draft） |
| 风险 | 低 | 高（draft，API 还会变） |

**v1 agent 的最小方法集**（Zed 要跑得动）：

1. `initialize` → 回 `protocolVersion: 1` + `agentCapabilities`（先不声明 fs/terminal/elicitation）
2. `session/new` → 回 `{ sessionId }`
3. `session/prompt` → 收 user 消息；`session/update` 流式回 `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `tool_call_update` / `plan`；最后 resolve `PromptResponse { stopReason: end_turn|... }`
4. `session/cancel`（notification）→ 中断当前 turn

## 4. 选型结论

> ⚠️ 2026-11-10 更新：作者决定 **只支持 ACP v2，不优先 v1**。下面 v1 的结论已按 v2 修正，见 `architecture.md` §5。

- **TS 实验 agent → 只实现 v2**（原「实现 v1」作废）：
  1. 你只支持 v2，所以 SDK 用 `@agentclientprotocol/sdk` 的 v2 入口（experimental）
  2. Zed 的 v2 client（`AcpV2Connection`）已完整内置，但要开 `acp-beta` feature flag 才会对 agent 广播 v2 能力
  3. TanStack `ai-acp` 的 agent 侧（`createAcpAgent`/`serveAcp`）恰好基于 v2，是最直接参考
- **Rust 生产 agent → 只 v2**：`agent-client-protocol` crate 开 `unstable_protocol_v2` feature（Zed 用的 5.1.0 已含）。锁死 v2 patch 版本，升级前跑 ACP 官方 testing 套件。
- **验证顺序**（按 v2）：
  1. 手写 stdio 验证脚本（不发 Zed）：`initialize`（`protocolVersion: 2`）/ `session/new` / `session/prompt`（注意 v2 是 receipt 式：prompt 先 ack 再流式），看 agent 回包
  2. Zed 里配 `agent_servers` + 开 `acp-beta`，看增量文本 / tool call UI / 取消
  3. （可选）用官方 testing 套件跑合规
- **风险**：v2 是 draft，schema 会动。锁版本，别追 latest。

## 5. 目录规划

> ⚠️ 骨架里的 `protocolVersion: 1` 已按 v2 决策作废，实际代码用 `2`。完整 v2 方法集见 `architecture.md` §5.3。

```
/home/aa/repos/ide_ls/aa-agent/          # agent 代码
├── ts/                                  # 实验：TypeScript v1 agent + 验证脚本
│   ├── package.json
│   ├── tsconfig.json
│   └── src/
│       ├── cli.ts          # bin：--acp 启动 ACP 服务（stdio NDJSON）
│       ├── agent.ts        # agent() 请求处理（initialize / session/*）
│       ├── llm.ts          # LLM 调用 + 流式
│       ├── tools.ts        # 工具定义 + 执行（读文件/写文件/跑命令，先本地直接做）
│       └── verify.ts       # 通用验证脚本：spawn 自己，手动发 JSON-RPC 断言
└── rust/                              # 生产：Rust v1 agent（后期）
    └── ...

/home/aa/repos/lib_ls/aa-ai/            # 库
└── docs/tanstack/                       # 调研记录（本目录）
    ├── overview.md                      # 项目 + 计划总览
    └── acp.md                           # 本文
```

### 5.1 TS 实验 agent 起步骨架（TODO 草案）

```ts
// src/agent.ts（示意，非最终代码）
import { agent, ndJsonStream } from '@agentclientprotocol/sdk'

export function startAcpAgent(stream?: ...) {
  const app = agent()
    .onRequest('initialize', () => ({
      protocolVersion: 1,
      agentCapabilities: {}, // v0：不声明 fs/terminal/elicitation
    }))
    .onRequest('session/new', ({ params }) => ({ sessionId: crypto.randomUUID() }))
    .onRequest('session/prompt', async ({ params, notify }) => {
      // 1. echo user message（可选）
      // 2. 调 LLM，每段增量 → notify('session/update', { update: { agent_message_chunk, ... } })
      // 3. 工具调用 → tool_call / tool_call_update
      // 4. return { stopReason: 'end_turn' }
    })
    .onNotification('session/cancel', ({ params }) => { /* abort 当前 turn */ })
  app.connect(stream ?? ndJsonStream(process.stdout, process.stdin))
}
```

验证脚本（`src/verify.ts` 思路）：

```ts
import { spawn } from 'node:child_process'
// spawn('node dist/cli.js', ['--acp'])
// stdin 依次写：initialize → session/new → session/prompt（"用一句话介绍你自己"）
// stdout 逐行 JSON.parse：断言 initialize.response.protocolVersion === 1
// 收集 session/update chunks 直到 prompt response.stopReason === 'end_turn'
// 打印每步 OK/FAIL + 总耗时
```
