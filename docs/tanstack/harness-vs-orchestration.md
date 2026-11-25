# TanStack AI：harness 与 orchestration 的边界、恢复语义与 step 粒度

> 调研日期：2026-11-20
> 仓库：`/home/aa/repos/lib_ls/learn_ls/ai`（本地克隆，分支 `taren/workflow-orchestration-rfc`）
> 关联：[`overview.md`](overview.md)、[`architecture.md`](architecture.md)

## 0. 结论

1. **命名即定位**：`ai-harness` 是会话/宿主层，`ai-orchestration` 是工作流/编排层。
2. **harness 把 `chat()` 当 agent loop 用**，默认 `maxIterations(50)`；子 agent 就是工具，支持并发、后台运行和崩溃恢复。
3. **orchestration 当前把一次 `ctx.ai.agent()` 当一个 workflow step**；整个 `chat()` 循环是该 step 内部的一个原子单元，workflow 看不到其中的 model turn / tool call，也无法为单个工具指定 step 或在它之前暂停。RFC 的意图是改变这一点。
4. 因此 orchestration 的跳过与重跑都**始终**以 **step** 为单位：已完成的 step 重放不重跑，server 崩溃时整个 step 重跑（at-least-once）。
5. **粒度原则**：step 的粒度必须与副作用的粒度对齐。
6. 对 aa-ai：先抄 harness 的 model/tool boundary checkpoint + tool result recorder + transcript repair；暂不引入 `workflow-core`。
7. **一句话区分**：harness 是「以会话为容器的完整 agent loop」，orchestration 是「以流程为容器、由 workflow 拥有执行」。流程可含循环；当前实现把一个完整 agent loop 放进一个 step（`ctx.ai.agent()`），RFC 的设计意图是把 model turn / tool 也展开成 workflow operation（06 是原型）。

## 1. 两个包的定位

| | `ai-harness` | `ai-orchestration` |
| --- | --- | --- |
| 一句话 | 长会话 agent 宿主 | 用 workflow 编排多个 agent step |
| 基本单元 | session / turn / 子 agent run | workflow step |
| 持久化 | `ai-persistence`（thread、message、run、log） | `workflow-core`（checkpoint、lease、timer） |
| 审批 | 会话内 interrupt（工具审批、提问、登录） | `ctx.approve()`，位于两个 step 之间 |
| 成熟度 | 实验性，README 提示 minor 版本可能改 API | 实验性，`workflow-core` 为 `^0.0.4` |
| 典型场景 | 交互式编码助手、长对话、子 agent 调研 | 草稿→审批→发布等固定流水线 |

补充：

- orchestration 不控制模型怎么推理，也不决定模型选择哪个工具。当前它能控制的是：**哪些 agent 调用成为 step、step 的顺序、step 之间的审批与重试**。单个工具调用能否成为独立 step、由谁执行、能否在它之前暂停，取决于 `chat()` 是否自己执行工具：默认（工具带 `execute`）不可控；只有 client tool / 06 模式下才由 handler 控制。
- 作为**顶层驱动**，二者互斥：一次 run 要么由会话（harness）拥有外层 loop，要么由 workflow（orchestration）拥有外层 loop，不能同时拥有。
- 作为**组件**，二者可以嵌套：agent loop 可以是 workflow 的节点，workflow 也可以作为工具/子流程被调用。互斥的是顶层所有者，不是能力。
- 控制流、嵌套和 step 粒度的完整对照见第 2 节。

## 2. 四个互补视角

单看「谁控制流程」容易失真：两边都使用工具调用、都能调用子 agent、都具备完整 agent loop。下面四个视角合起来才准确。

### 2.1 容器：会话 vs 流程

| | harness | orchestration |
| --- | --- | --- |
| 容器 | 会话（thread） | 流程（workflow run） |
| 容器里是什么 | 一个长生命周期 agent loop | 多个步骤；当前实现每步内部可以是完整 agent loop，RFC 意图是 loop 本身由 workflow 展开 |
| 生命周期 | 可持续很久，用户随时插话 | 单次流程，可暂停、恢复、跨时间 |
| 恢复单位 | model / tool checkpoint | step；RFC 只是把 step 细化到 model turn / tool，恢复仍以 step 为单位 |

`host.open(agent, { threadId })` 可以同时开多个会话；「单会话」指一次 agent run 的作用域，不是进程只能有一个会话。

### 2.2 决策点：每个 model turn vs 每个 step 边界

- harness：每个 model turn 后都是一个决策点，模型决定下一步（继续调工具、调子 agent、还是结束）。
- orchestration（当前）：每个 step 边界是一个决策点，handler 决定下一步进入哪个 step；step 内部仍由模型决定工具。RFC 意图是让 model turn / tool 也成为决策点。

共同点：**两边都是模型选择工具和参数**。差别只在「谁决定何时进入下一个决策点」。

### 2.3 嵌套：模型套模型 vs 代码套模型

```
harness：
  session（模型驱动的外层 loop）
    └─ 子 agent 工具 → 内层 loop（仍由模型决定）

orchestration（当前实现）：
  handler（代码写的外层流程，可含循环）
    ├─ ctx.ai.agent(A) → step 内部是完整 agent loop
    │     └─ 子 agent 工具 → 更内层 loop
    └─ 审批 / 暂停 / 下一个 step

orchestration（RFC 意图）：
  workflow 拥有执行，model turn / tool call 都是 operation
    └─ agent loop 由 workflow 展开，而不是藏在单个 step 里
```

- harness：模型驱动的 loop 里涌现模型驱动的 loop，嵌套深度由模型决定。
- orchestration（当前）：代码定义的流程里嵌入模型驱动的 loop，嵌套深度由代码决定。
- orchestration（意图）：loop 本身被 workflow 展开，model turn / tool 成为独立 operation，而不是「一个 step 套一个完整 loop」。

### 2.4 流程与粒度：自定义的是流程，不是 loop

- 自定义的是**流程**：阶段顺序、循环、分支、并行、暂停、审批。
- 循环只是流程里的一种结构，不是 harness / orchestration 的分界线。
- orchestration 的 step 粒度可配置：
  - 当前默认：一个 agent loop = 一个 step（`ctx.ai.agent()` 的语义）；
  - 当前可选：每次 model 通信、每次工具调用各一个 step（06 实验的语义）；
  - RFC 意图：workflow 拥有 loop，model turn / tool 都是 workflow operation，而不是把 loop 包进 step。

| 粒度 | 恢复行为 | 代价 |
| --- | --- | --- |
| 整个 loop 一个 step | 崩溃重跑整个 loop | 简单；副作用需幂等 |
| 每次通信/工具一个 step | 已完成的不重跑 | step 多，调度和日志开销大 |

### 2.5 一句话

> **harness 是「模型自己走完整个会话」；orchestration 是「代码定义流程，由 workflow 拥有执行」。**

当前实现里，orchestration 把一个完整 agent loop 放进一个 step（`ctx.ai.agent()`），也可以拆到每次通信/工具一个 step（06）。但这不是作者的设计意图：RFC 明确要把 model turn 和 side-effecting tool 暴露成 Workflow operation，让 loop 由 workflow 展开。因此「完整 agent loop 在 step 内部」是**当前适配器的状态**，不是 orchestration 的定义。

两者都具备完整 agent loop 能力；区别不在能力，而在**容器、决策点、嵌套方式，以及 loop 由谁拥有执行**。作为顶层驱动，二者互斥；作为组件，二者可以嵌套。

## 3. `chat()` 的两种用法

### 3.1 harness：`chat()` 就是 agent loop

- `packages/ai-harness/src/session/base.ts:1355`：`agentLoopStrategy` 默认 `maxIterations(50)`。
- `packages/ai-harness/src/session/turn.ts:1237`：把该策略传给 chat 调用。
- `packages/ai-harness/src/define.ts:301-305`：公开配置项，注释为「When a turn stops calling the model」。
- `packages/ai-harness/src/define.ts:308-312`：`toolExecution: 'parallel' | 'sequential'`，默认并行。

因此 harness 的一个 "turn" = 一次 `chat()` 调用，内部最多 50 个 model turn，并执行其中的工具。harness 自己管的是会话层：输入、waits、子 agent、持久化。

### 3.2 06 实验：`chat()` 只跑一个 model turn

`packages/ai-orchestration/experiments/src/06-handler-agent-loop.ts`（本地未提交实验）：

- `chat()` 只负责一次模型通信；
- 循环、工具执行、审批都在 workflow handler 里；
- 每个 model turn、每个 tool call 各是一个 step。

这是 RFC 所说的 "later integration"：由 orchestration 拥有 loop，`chat()` 退化为单次调用。

## 4. 崩溃恢复

### 4.1 harness：model / tool boundary checkpoint

关键事实：

- `packages/ai/src/activities/chat/middleware/run-store.ts:202-227`：
  `RunCheckpoint` 的注释是 "Crash-resume data saved at the last **model or tool boundary**"；
  字段包含 `pendingTools`，以及 `leaseOwner` / `leaseExpiresAt` / `driverEpoch`（多主机接管与 fencing）。
- `packages/ai-harness/src/resume.ts:73-128`：
  `checkpointMiddleware` 在每个工具阶段前后保存 transcript，并记录「已开始但无结果」的工具调用。
- `packages/ai-harness/src/session/harness-session.ts:663-667`：
  `toolResultRecorder` 把每个完成的工具结果写成 `harness.tool.result` 日志记录，避免「一批工具里后面的崩了，前面的结果丢失」。
- `packages/ai-harness/src/session/agents.ts:1535-1563`：
  `continueAgentThread` 把残留的 `status: 'running'` 改为 `failed`，再调用 `repairTranscript`。
- `packages/ai-harness/src/define.ts:174-242`：
  `maxAttempts`（默认 10）、`recover` 钩子、`interruptedToolResult`、`truncatedToolResult`、`continueCutOff`、`resumeBackgroundAgents`。
- `packages/ai-harness/src/host.ts:948-951`：公开的 `host.recover(threadId)`。

恢复行为：

| 崩溃位置 | 恢复行为 |
| --- | --- |
| 已完成的 model turn | transcript 已有，不重调 |
| 已完成的 tool call | 日志已有结果，不重跑 |
| 正在生成中的 model turn | 重新调用；可用 `continueCutOff` 续写部分输出 |
| 正在执行中的 tool call | `replay: 'never'` 补中断结果；`replay: 'safe'` 重跑 |
| 等待中的审批/输入 | interrupt 持久化，重连后重新提示 |

结论：只有「崩溃瞬间在飞的那一次模型调用」会重复，不是整轮重跑。

### 4.2 orchestration：一个 agent 调用 = 一个 step（当前实现）

- `packages/ai-orchestration/src/middleware.ts:20-50`：
  `ctx.ai.agent(id, definition, input, options)` 的实现就是 `ctx.step(id, (step) => executeAgent(...), options)`。
- `packages/ai-orchestration/src/execute-agent.ts:12-51`：
  `executeAgent` 只做输入校验、调用 `definition.run`、输出校验；内部没有子 step。
- `rfcs/0001-workflow-backed-orchestration.md:136-145`：
  「The whole `chat()` loop inside that call is currently one atomic durable unit. Workflow does not checkpoint between model turns or tools owned by that loop.」
- 同文件 `:120-133`：
  校验后的最终输出成为 step checkpoint；replay 返回 checkpoint，不再调模型；若 worker 在 checkpoint 前死亡，可能重新调模型。语义是 at-least-once。
- 同文件 `:179-196`：
  workflow approval 与 tool approval 目前不是同一个对象；agent 调用内部出现审批会失败（`execute-agent.ts` 抛 `AgentApprovalUnsupportedError`）。

- 源码验证（`@tanstack/workflow-core@0.0.4`）：`src/engine/run-workflow.ts:663-699` 的 `engineStep` 先按 `stepId` 在事件日志里查找；命中就标记 `workflow.step.replayed` 并返回缓存结果，未命中才执行 `fn` 并追加事件。`src/types.ts:55,196` 把 `STEP_FINISHED` 等事件定义为 completion checkpoint。`wait` / `approve` / `sleep` / `now` / `uuid` 也都以 `stepId` 做同样的 replay 检查。
- 结构性不变式：**workflow 的恢复边界始终是 step**，不是「当前才是 step」。RFC 想让 model turn / tool 成为 workflow operation，本质是把 step 细化，而不是取消 step 边界。agent loop 内部怎么恢复，目前没有官方实现；即使将来有，也是 step 内部的事，workflow 不感知。

| 场景 | 结果 |
| --- | --- |
| step 已完成 | 重放返回 checkpoint，不重跑 |
| step 执行中崩溃 | 整个 step 重跑，包括已完成的 model turn 和已执行的工具 |
| 工具已产生副作用 | 会再次执行，除非幂等 |
| step 内出现审批 | 不支持，直接报错 |

### 4.3 工具副作用的崩溃窗口（harness 的处理）

问题：工具副作用已经发生，但结果尚未写进日志，进程崩溃。恢复时既不知道它执行没执行，也无法安全决定跳过还是重跑。

harness 不消除这个窗口，而是把它缩小到「`fn` 返回后、值写入日志前」，并提供三样东西：

1. **执行前后各写一条日志**
   - `checkpointMiddleware` 在工具开始前写 `harness.tool.started`（`session/harness-session.ts:655-661`）；
   - `toolResultRecorder` 在完成后写 `harness.tool.result`（`:663-667`）。
   恢复时因此能区分「没开始 / 开始了没结果 / 已完成」。
2. **稳定的幂等键**
   - `durable-tool.ts:42-57`：`idempotencyKey = ${logId}:${toolCallId}:${name}`，不含 attempt，重跑时相同；
   - 由外部系统（支付、邮件、CI）负责拒绝第二次相同 key 的调用。
3. **重放策略**
   - `replay: 'never'`：不重跑，`repairTranscript` 补 `INTERRUPTED_TOOL_RESULT`（「可能执行过也可能没有」），见 `resume.ts:322-327`；
   - `replay: 'safe'`：重跑，要求工具幂等；
   - durable `step.do`：返回值先存日志再返回；重跑命中记录则不执行 `fn`。

| 崩溃位置 | harness 的行为 |
| --- | --- |
| 工具开始前 | 直接重跑 |
| 工具执行中/执行后、结果未落日志 | `never` 标中断；`safe` 重跑；durable 查记录后重跑 |
| 结果已落日志 | 不重跑 |
| `step.do` 的 `fn` 已返回、值未存储 | 仍会重跑 `fn`，依靠 `idempotencyKey` 让外部系统拒绝第二次 |

限制（`durable-tool.ts:69-72` 原文）：

> A step runs at least once, and is stored once: a crash after `fn` ends but before the value is stored runs `fn` again.

所以 harness 的语义是 **at-least-once，不是 exactly-once**；exactly-once 只能由副作用所在系统提供。

## 5. 06 实验逐步说明

文件：`packages/ai-orchestration/experiments/src/06-handler-agent-loop.ts`

### 5.1 三个角色

1. `oneModelTurn(history)`：假模型。没有 tool 结果时返回一个 `TOOL_CALL_REQUESTED`；有 tool 结果时返回最终文本。
2. `turnAgent`：`defineAgent` 包装 `oneModelTurn`，输入是 `history`。`defineAgent` 只描述 schema 和 `run`，**不产生 step**。
3. `toolAgent`：包装 `send_email` 这个副作用；它的副作用是「执行一次就计数一次」。
4. workflow handler：真正拥有循环的人。

### 5.2 Phase 1：模型要求工具，流程暂停等审批

1. `runWorkflow` 启动，handler 进入 `for turn < 3`。
2. `ctx.ai.agent('turn-0', turnAgent, { history })` 创建 step `turn-0`，执行假模型。
3. 假模型返回 `kind === 'tool-call'`，step `turn-0` 的 checkpoint 记录这个输出。
4. handler 调用 `ctx.approve({ id: 'approve-send_email-0', ... })`，workflow 发出 `APPROVAL_REQUESTED` 并暂停。
5. `runWorkflow` 返回事件流；`runStore` 保存 run 状态和 step `turn-0` 的 checkpoint。

此时 `modelCalls = 1`，`toolRuns = 0`。

### 5.3 Phase 2：批准后，工具和下一次模型调用各自成为 step

1. `runWorkflow({ runId, approval: { approved: true } })` 恢复同一个 run。
2. workflow 从头 replay handler。
3. 再次执行到 `ctx.ai.agent('turn-0', ...)` 时，`turn-0` 已完成，直接返回 checkpoint，**不调用模型**（`modelCalls` 仍为 1）。
4. `ctx.approve` 返回已批准。
5. `ctx.ai.agent('tool-send_email-0', toolAgent, args)` 创建 step，执行副作用（`toolRuns = 1`）。
6. handler 把 assistant tool_call 和 tool result 写进 `history`。
7. 下一轮 `ctx.ai.agent('turn-1', turnAgent, { history })` 调用模型，这次 history 里有 tool 结果，返回最终文本。
8. workflow 返回 `{ answer: ... }`。

### 5.4 Phase 3：拒绝时，工具 step 永远不会执行

1. 换一个新的 `runStore`，再跑一次 Phase 1。
2. 恢复时传 `approved: false`。
3. handler 直接返回 `cancelled by user`，不会执行 `tool-send_email-0`，`toolRuns` 不增加。

### 5.5 这个实验证明了什么

- **step 粒度 = 副作用粒度**：每个 turn、每个 tool 都是独立 step，崩溃或恢复时只影响「正在飞的那一个」。
- **已完成的 step 不会重复计费**：Phase 2 的 `modelCalls` 不增长，说明 `turn-0` 是 checkpoint 重放，不是重新调用。
- **审批可以跨进程持久化**：暂停点是一个 step 边界，恢复时从同一个 runId 继续。

### 5.6 这个实验没有证明什么

- 假模型不代表真实 `chat()` 的行为。真实接入时，必须保证 `chat()` 只跑一次模型且**不执行工具**，否则工具副作用会发生在 workflow 审批之外。
- `agentLoopStrategy` 控制的是 model turn 数，不控制工具执行。循环顺序是 `processText → executeToolCalls → shouldContinue`（`packages/ai/src/activities/chat/index.ts:991`、`:1834`、`:4512`），工具在 strategy 判断之前就执行了。所以 `maxIterations(1)` 不够。
- 让 `chat()` 只产出 tool call 的可行做法：把工具定义为**没有 `execute` 的 client tool**。
  - `packages/ai/src/activities/chat/tools/tool-definition.ts:135`：`execute` 是可选字段；
  - `packages/ai/src/activities/chat/index.ts:4067-4086`：`!tool.execute` 的调用进入 pending client 列表；
  - `packages/ai/src/activities/chat/index.ts:5332-5345`：无 `execute` 的工具生成 `clientRequests`，服务端不执行。
  这样 `turnAgent.run` 里调用 `chat({ tools: 无 execute 的工具 })`，一次调用只产生 tool call，审批与执行交给 handler。
- provider 内置的 server-side tool（`isProviderExecutedToolCall`，`index.ts:4068-4069`）不会作为 client work 暴露，这条路线里要避免使用。

## 6. 对 aa-ai 的落地结论

1. **Session 事件模型**（不引入 workflow 也能做）：

   - `run.started` / `run.ended(status)`
   - `turn.started` / `turn.ended`
   - `tool.started` / `tool.result`
   - `approval.requested` / `approval.resolved`

2. **工具 replay 策略**：`never` / `safe` / `durable`；durable 的结果先落日志，再继续循环。

   - `ToolContext` 带稳定 `idempotency_key`（run id + tool call id + step name，不含 attempt）；
   - 执行前写 `tool.started`，执行后写 `tool.result`；
   - `never`：不重跑，补「可能执行过」的中断结果；
   - `safe`：重跑，要求幂等；
   - `durable`：先查已存结果，没有才执行。

3. **崩溃恢复**：

   - 启动时把残留的 `running` 标记为 `failed`；
   - 用事件日志修复 transcript；
   - 已完成的工具结果不重跑；
   - 只重跑崩溃瞬间在飞的 model turn；
   - 用 `attempts` 和 `recover` 钩子决定是否继续。

4. **暂不引入 `workflow-core`**：当前 `^0.0.4`，且 orchestration 的 step 粒度会掩盖 agent loop 内部的重复执行问题。等 aa-ai 有了稳定的事件日志，再把 turn/tool 事件映射成 step 也不迟。

## 7. 证据索引

| 主题 | 位置 |
| --- | --- |
| harness 默认 loop 50 | `packages/ai-harness/src/session/base.ts:1355`、`session/turn.ts:1237`、`define.ts:301-305` |
| 子 agent 是工具 | `packages/ai-harness/src/first-party/agents.ts:238-246` |
| 子 agent 并发 | `packages/ai-harness/src/session/agents.ts:1566-1581` |
| 工具并行 | `packages/ai-harness/src/define.ts:308-312` |
| RunCheckpoint | `packages/ai/src/activities/chat/middleware/run-store.ts:202-227` |
| checkpointMiddleware | `packages/ai-harness/src/resume.ts:73-128` |
| toolResultRecorder | `packages/ai-harness/src/session/harness-session.ts:663-667` |
| transcript 修复 | `packages/ai-harness/src/session/agents.ts:1535-1563` |
| 恢复配置 | `packages/ai-harness/src/define.ts:174-242` |
| 工具 started / result | `packages/ai-harness/src/session/harness-session.ts:655-667` |
| 幂等键 | `packages/ai-harness/src/durable-tool.ts:42-57` |
| at-least-once 原文 | `packages/ai-harness/src/durable-tool.ts:69-72` |
| 中断结果修复 | `packages/ai-harness/src/resume.ts:299-343` |
| host.recover | `packages/ai-harness/src/host.ts:948-951` |
| ctx.ai.agent = step | `packages/ai-orchestration/src/middleware.ts:20-50` |
| executeAgent | `packages/ai-orchestration/src/execute-agent.ts:12-51` |
| workflow-core replay | `@tanstack/workflow-core/src/engine/run-workflow.ts:663-699` |
| completion checkpoint | `@tanstack/workflow-core/src/types.ts:55,196` |
| workflow-core 版本 | `packages/ai-orchestration/package.json:58` |
| RFC 原子单元 | `rfcs/0001-workflow-backed-orchestration.md:136-145` |
| RFC at-least-once | `rfcs/0001-workflow-backed-orchestration.md:120-133` |
| RFC 审批边界 | `rfcs/0001-workflow-backed-orchestration.md:179-196` |
| 06 实验 | `packages/ai-orchestration/experiments/src/06-handler-agent-loop.ts` |
| tool 的 execute 可选 | `packages/ai/src/activities/chat/tools/tool-definition.ts:135` |
| client tool 分流 | `packages/ai/src/activities/chat/index.ts:4067-4086`、`:5332-5345` |

## 8. 未决问题

- 真实 `chat()` 如何做到「只跑一个 model turn 且不执行工具」：已确认可用无 `execute` 的 client tool 实现（见 5.6）；仍需在真实 provider 上验证 resume 与回填 tool result 的完整流程。
- `workflow-core` 的成熟时间，以及它是否会改变 step 内部的可恢复粒度。
- RFC 明确未决：`Durable boundaries inside long chat() model/tool loops`、`The interrupt envelope and approval mapping`。
