# agent loop 结构对比：Zed / rig / TanStack / pi / dsh / aa-ai

> 只记事实与结构差异，不做取舍。行号以本地克隆为准。
>
> - Zed：`/home/aa/repos/ide_ls/learn_ls/zed`（commit `4ed72e0135`）
> - rig：`/home/aa/repos/lib_ls/learn_ls/rig`（commit `6e13e8c`）
> - TanStack AI：`/home/aa/repos/lib_ls/learn_ls/ai`（commit `6059872f`）
> - pi：`/home/aa/repos/ai_ls/learn_ls/pi`（commit `406e719a6`）
> - dsh：`/home/aa/repos/ai_ls/learn_ls/deepseek-harness`（commit `e8f7b93a34`）
> - aa-ai：本仓 HEAD

## 0. 结论

六家的骨架是同一个：

```text
loop {
    请求历史 + 工具定义 → LLM 流
    收文本 / 思考 / tool_calls
    写 assistant 消息
    没有 tool_calls → 结束
    执行工具 → 写 tool 结果
}
```

差异集中在五个轴：

1. **状态所有权**：loop 对象持有历史（Zed 原生/pi/TanStack/dsh），还是由参数传入（aa-ai），还是 memory store（rig）；
2. **轮/步的状态机**：直接 `while`（Zed 原生/pi/TanStack/aa-ai），还是显式状态机（rig 的 `AgentRunStep`、dsh 的 turn/step）；
3. **工具批调度**：串行（aa-ai）、并发（Zed 原生）、可配并发（rig）、默认并行（TanStack）、按工具声明（pi）、barrier + 有界池（dsh）；
4. **终止条件**：无工具、轮数上限、finish reason、turn/end 原因；
5. **恢复与压缩**：事件日志 + 重放（dsh/aa-ai）、run 状态机 resume（rig）、独立压缩包（TanStack）。

## 1. Zed

`AcpThread::submit`（`acp_thread.rs:7545`）按 `SubmissionTransport`（枚举定义
`submission.rs:164`）分三路。**这三个名字是 submission 的确认/传输方式，
不是「原生/外部 agent」的分类**；构造时看 `connection.receipt_submissions(...)`
（`acp_thread.rs:4188`、`:4216`），有则 `Receipt`、无则 `Completion`。

- **`Completion`**：一次 prompt 请求的响应就结束回合，Zed 自己从回合推导
  前台状态。ACP v1 外部 agent 走这里：`submit` → `send_inner`（`acp_thread.rs:7630`）
  → `AcpThread::run_turn`（`:7729`）→ `connection.prompt(request, cx)`（`:7674`）
  → `AcpConnection::prompt`（`agent_servers/src/acp/v1.rs:1761`）发 `session/prompt`
  给外部进程。**没有内部 `Thread`，循环在外部进程**。
- **`Receipt`**：agent 先回执确认接受，前台状态单独上报。原生 Zed agent
  （`agent.rs:3342` → `NativeSessionSubmissions::submit`，`:3254`）和 **ACP v2
  外部 agent**（`agent_servers/src/acp/v2.rs:1684` → `V2SessionSubmissions`）都实现它。
  原生分支继续：`submit_native`（`agent.rs:2482`）→ `drive_execution`（`:2660`）→
  `execute_retry_action`（`:3415`）→ `execute_prompt`（`:3456`）→
  `NativeAgentConnection::run_turn`（`agent.rs:2759`）只是桥接器：从 `sessions`
  取 `thread` / `acp_thread` → 调用闭包 `f(thread, cx)` 拿到事件流 →
  `handle_thread_events` 把事件泵回 `AcpThread`。**启动回合的是闭包内容**：
  `execute_prompt` 传入的闭包里调 `thread.send_existing`（`agent.rs:3502` →
  `thread.rs:2846`；retry 时是 `thread.resume`，`agent.rs:3451` → `thread.rs:2781`）
  → `Thread::run_turn`（`thread.rs:3023`）spawn → `run_turn_internal` 的
  `loop`（`:3102`）。v2 外部分支则把 prompt 发给外部进程，循环仍在外部。
  这一路**不走** `AcpThread::run_turn`。
- **`AgentOwned`**：agent 自己拥有会话工作（如 ACP 子 agent），Zed 不能提交
  prompt；由 `agent_owned()` 设置（`acp_thread.rs:4252`），`submit` 直接调
  `reject_agent_owned_submission`（`:7562`）。

所以「原生还是外部」由 `connection` 的具体实现决定，不由 transport 名字决定；
「agent loop 是 Thread 的方法」只对**原生分支**成立。下面所有 `Thread` 相关条目
都只描述原生分支——外部会话根本没有 `Thread`。

- **入口与所有权（原生分支）**：`Thread::send` → `send_existing` → `run_turn`
  （[thread.rs#L2793-L2811](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2793-L2811)、[#L2846-L2859](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2846-L2859)）。
  `Thread` 持有对话；`Session` 再包一层 Thread + ACP 状态
  （[agent.rs#L229-L246](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/agent.rs#L229-L246)）。
- **一轮的结构**：外层 `loop` 每轮先做 compaction，再重新读取模型、刷新工具、构建 request
  （[thread.rs#L3114-L3160](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L3114-L3160)）。
  轮内用 `select_biased!` 同时等三类事件：LLM 流、工具结果、取消
  （[#L3247-L3340](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L3247-L3340)）。
- **工具调度**：`FuturesUnordered` 并发执行，工具结果与 ongoing stream 交错处理；
  streaming tool input 与结果可以同时在途。
- **终止**：`tool_results.is_empty() && early_tool_results.is_empty()` 即本轮结束
  （[#L3420](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L3420)）；
  **没有看到轮数上限**。
- **压缩**：`perform_compaction_if_needed`，上下文窗口低于阈值不启用
  （[#L122-L123](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L122-L123)）。
- **子 agent**：`create_subagent` / `resume_subagent`，`SubagentContext.depth`，
  达到 `MAX_SUBAGENT_DEPTH` 后不再暴露 create 工具
  （[#L1405-L1429](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L1405-L1429)、[#L2394-L2398](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2394-L2398)）。
- **辨识度**：每轮之间可以换模型、刷新工具；工具并发与流式输出深度交错。

## 2. rig

- **入口与所有权**：`drive_agent` 用 `async_stream` 驱动整个 run
  （[engine.rs#L137-L151](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L137-L151)），
  历史由 memory store 加载/保存（[#L212-L260](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L212-L260)）。
- **状态机**：显式 `AgentRunStep::{CallModel, CallTools, Done}`
  （[run/mod.rs#L75-L100](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/mod.rs#L75-L100)），
  `next_step` 处理 turn budget / `MaxTurns`
  （[#L849-L895](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/mod.rs#L849-L895)）。
- **CallModel**：run start hook → completion hook → 模型选择 → 请求 → 消费流
  （[engine.rs#L264-L381](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L264-L381)）。
- **CallTools**：`drive_tool_calls` 按 `tool_concurrency` 执行：`<=1` 严格顺序，
  否则有界并发；malformed call fail-fast，未启动的 sibling 立即 drop
  （[#L456-L469](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L456-L469)、[#L520-L521](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L520-L521)）。
  结果按调用顺序写回。
- **终止**：`max_turns`（[builder.rs#L210-L211](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/builder.rs#L210-L211)）、
  hook 的 `Stop`、无工具。
- **压缩**：引擎内没有 compaction。
- **子 agent**：`Agent::into_tool()` 把 agent 变成 `DynamicTool`
  （[agent/tool.rs#L1-L45](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/tool.rs#L1-L45)）。
- **辨识度**：hook 贯穿每个阶段；run 是显式状态机，可 resume；并发度可配。

## 3. TanStack AI

- **入口与所有权**：`chat()` 的运行时是 `TextEngine`，内部持有
  `messages` / `iterationCount` / `toolCallCount` / `lastFinishReason`
  （[index.ts#L1030-L1069](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L1030-L1069)）。
- **终止**：默认最多 5 次 model turn，且 `finishReason === 'length'` 时停止
  （[#L1241-L1247](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L1241-L1247)）；
  策略可插拔：`maxIterations()` / `untilFinishReason()`
  （[agent-loop-strategies.ts#L3-L61](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/agent-loop-strategies.ts#L3-L61)）。
- **工具调度**：`toolExecution` 默认 `'parallel'`
  （[tool-calls.ts#L1013-L1036](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/tools/tool-calls.ts#L1013-L1036)）；
  `onBeforeToolCall` / `onGuardToolCall` 提供审批与参数校验
  （[#L1241-L1398](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/tools/tool-calls.ts#L1241-L1398)）。
- **状态**：messages 在 TextEngine 内；`middleware/run-store.ts` 负责 run 级持久化。
- **压缩**：独立 `ai-compaction` 包，但以 `withCompaction()` 返回的 `ChatMiddleware`
  挂进 `chat()`，每次模型调用前运行（`packages/ai-compaction/src/index.ts:1–:4`、`:794`）。
- **子 agent**：`spawn` 工具 + `SUBAGENT_STARTED / SUBAGENT_FINISHED` 事件
  （[spawn.ts#L1443-L1487](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/agents/spawn.ts#L1443-L1487)），
  子 agent 的用户回合可从消息里恢复（[turn.ts#L130-L159](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/agents/turn.ts#L130-L159)）。
- **辨识度**：loop 策略对象化；工具审批钩子最完整。

## 4. pi

- **入口与所有权**：`Agent` 持有 `messages` / `steeringQueue` / `followUpQueue`
  （[agent.ts#L188-L257](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent.ts#L188-L257)），
  `prompt()` / `continue()` 启动或续跑循环（[#L379-L419](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent.ts#L379-L419)）。
- **双层循环**：外层处理 follow-up，内层处理 tool calls 与 steering 消息
  （[agent-loop.ts#L172-L330](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L172-L330)）。
- **工具调度**：按工具声明的 `executionMode`——任一工具是 `sequential` 就整批串行，
  否则并行（[#L527-L532](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L527-L532)）。
- **终止**：无工具调用且无 steering/follow-up；`shouldTerminateToolBatch` 可提前结束。
- **状态**：低层只发事件（`agent_start` / `turn_start` / `message_*`），
  持久化由上层（coding-agent）负责。
- **压缩**：loop 不含；注释明确 compaction 可能发生在 preparation 阶段
  （[#L210](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L210)），
  coding-agent 有自定义压缩扩展。
- **子 agent**：核心 agent 包没有；只在上层扩展里出现。
- **辨识度**：**队列驱动**——循环中途可以注入 steering / follow-up 用户消息。

## 5. dsh（deepseek-harness）

- **入口与所有权**：`ReactLoopAgent` 持有 `inbox` / `session` / `model`
  （[agent.ts#L111-L155](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/src/agent.ts#L111-L155)），
  `send()` 按 `next-turn` / `next-step` 入队
  （[#L171-L190](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/src/agent.ts#L171-L190)）。
- **结构**：`kick()` 的 `while (await this.turn())`；回合内循环 `preStep` / `step`，
  由 `inbox.nextStep` 决定是否继续，结束时写 `turn/end`
  （[#L274-L439](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/src/agent.ts#L274-L439)）。
- **工具调度**：`dsh-tools/batch` 用 exclusive barrier + 有界并发池，
  按模型顺序提交结果；abort 时为未启动调用写合成错误结果
  （[batch.ts#L1-L12](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/tools/src/batch.ts#L1-L12)）。
- **状态**：event-sourced `Session`（user / assistant / tool / turn-end / compaction 事件），
  有 checkpoint policy 与崩溃补齐。
- **压缩**：会话级 `compaction/*` 事件 + `maintenance` phase，不是 loop 内的临时步骤。
- **子 agent**：会话头 `origin: 'subagent'`，有 `subagent/catalog` 事件
  （[session/index.ts#L129-L130](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/session/src/index.ts#L129-L130)、
  [known-event-types.ts#L62](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/session/src/known-event-types.ts#L62)）。
- **辨识度**：事件溯源 + 队列 + 取消原因 + 崩溃恢复，语义最完整。

## 6. aa-ai

- **入口与所有权**：`AgentLoop::run(model, session, events, cancel)`
  （[agent_loop.rs#L121-L127](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L121-L127)）；
  历史由调用方持有并传入，loop 只追加。
- **一轮的结构**：run 前先做一次压缩（[#L138-L156](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L138-L156)），
  然后 `loop`（轮数预算 `AgentLoop::max_turns: Option<usize>`，默认 `None` 不限）：构建 `Request{stream: true}` → 流循环
  （`tokio::select!` 取消 / 事件，[#L188-L223](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L188-L223)）
  → `push_assistant` → 无工具则返回 → 执行工具 → `push_tool_results`
  （[#L234-L304](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L234-L304)）。
- **工具调度**：非流式工具轮末串行 `for` + `await`；声明 `supports_input_streaming`
  的工具在参数生成期间就提前启动，与流并发（`FuturesUnordered`），用 `ToolInput`
  持续投喂已修补快照。工具失败写 `is_error` 结果继续，不中断循环。
- **终止**：无工具、轮数预算用尽（仅当调用设置了 `max_turns`，`Cancelled(Budget)`）、取消。
- **状态**：`Session` 事件日志（`user` / `assistant` / `tool_result` / `compaction`），
  可 JSONL 恢复；`messages()` 是事件的计算视图。
- **压缩**：每次 `run` 前检查（Zed 阈值 + 保留策略），压缩本身记事件。
- **子 agent**：无。
- **辨识度**：最小内核——无队列、无 UI 耦合；只有声明流式的工具提前并发，
  其余仍串行；模型按调用传入，可中途换。

## 7. 对照表

| 维度 | Zed | rig | TanStack | pi | dsh | aa-ai |
|---|---|---|---|---|---|---|
| 历史所有权 | Thread | memory store | TextEngine | Agent | Session | 调用方 `Session` |
| 循环形态 | 外层 `loop` + 轮内 select | `AgentRunStep` 状态机 | `TextEngine` + 策略 | 双层 `while` | turn/step + inbox | `loop` + 可选 `max_turns` |
| 工具并发 | 并发（FuturesUnordered） | 可配（`tool_concurrency`） | 默认并行 | 按工具声明 | barrier + 有界池 | 串行（流式工具提前并发） |
| 工具失败 | 结果写回，继续 | fail-fast / Stop | 审批钩子 + 错误结果 | 错误结果 | 合成错误结果 | 错误结果 |
| 终止 | 无工具 | max_turns / hook | 5 轮 / finish reason | 无工具 + 队列空 | turn/end 原因 | 无工具 / 可选 max_turns / 取消 |
| 持久化 | Thread 状态 | memory store | run-store 中间件 | 上层 | event-sourced + checkpoint | JSONL 事件日志 |
| 压缩 | 轮前 | 无 | `chat()` middleware | preparation 钩子 | 会话级 compaction | run 前 |
| 子 agent | 内建 + 深度限制 | `into_tool` | spawn 工具 | 上层扩展 | subagent origin | 无 |
| 取消 | thread 级 signal | hook Stop | AbortSignal | run 级 signal | cancel(cause) | CancelToken |

## 8. 差异里值得注意的点

1. **工具并发**：除 aa-ai 外都在 loop 里处理并发；`LanguageModel` 已有
   `supports_parallel_tool_calls` 能力位，但 loop 还没消费它。
2. **队列/steering**：只有 pi 和 dsh 把「运行中注入用户消息」做进循环；
   我们的 `Session` 是纯追加日志，没有 next-step 队列概念。
3. **子 agent**：Zed / rig / TanStack 都有；我们的 `AgentTool` 目前只能包普通工具，
   还没有 agent-as-tool。
4. **状态所有权**：三种路线（loop 对象 / memory store / 事件日志）。
   我们选的是事件日志，和 dsh 最近，但缺少 dsh 的 turn/end 与 checkpoint 语义。
5. **压缩位置**：Zed 与 aa-ai 在轮前；TanStack 以 `chat()` middleware 形式在每次
   模型调用前；dsh 会话级。详见第 10 节。
6. **终止策略**：Zed 和 aa-ai 默认都没有轮数上限，靠「无工具」自然收敛；
   aa-ai 可选 `max_turns`（对齐 rig 的可空预算），用尽走取消通道
   （`Cancelled(Budget)`），有意与 provider 错误区分。

## 9. 未决

1. 是否实现并行工具执行？需要先定结果顺序、失败语义（fail-fast 还是全部执行）
   以及审批交互。
2. 是否引入 next-step 队列（steering）？这会改变 `Session` 的写入时机。
3. 子 agent 用什么形态：`AgentTool` 包 `AgentLoop`，还是 Zed 式独立 Thread？
4. 压缩是否移出 `run`，改为会话层触发？现在每次 run 前检查一次。

## 10. 术语与代码分布：会话、agent loop、Thread

> 这一节澄清本文反复用到的几个词，以及各家的代码放在哪里。
> 源码位置用项目内相对路径；克隆根目录见 [`AGENTS.local.md`](../AGENTS.local.md)。

### 10.1 四个词的定义

| 词 | 定义 | 判定标准 / 源码证据 |
|---|---|---|
| **会话 / thread** | 长生命周期容器，持有历史、持久化、压缩等会话级能力 | 跨多次模型调用存在；恢复/续聊的单位 |
| **用户回合** | 从一条用户输入开始的一次完整处理过程；一个会话可有多轮 | Zed `running_turn` 注释：「该回合跨多次请求存活，期间模型调工具、我们执行并回填结果」（`crates/agent/src/thread.rs:1342`） |
| **model turn / 迭代** | 一次模型请求 + 其响应（可能带 tool_calls） | TanStack `maxIterations` 注释：数的是 model turns，**不是工具调用**；一次迭代可发多个并行工具调用（`packages/ai/src/activities/chat/agent-loop-strategies.ts:1–:21`） |
| **agent loop** | 实现「一个用户回合」的循环：请求模型 → 收文本/工具调用 → 执行工具并回填 → 再请求……直到满足停止条件 | 它的一次运行 ≈ 一个用户回合，但不严格一对一（见下） |

一个通用的结构抽象是：

```text
一次 agent loop（一个用户回合）：
  一条用户输入
    → 一次或多次模型调用（每次调用后可并行执行工具并回填结果）
    → 直到满足停止条件：无工具调用 / 迭代上限 / 用户取消 / 错误
    → 本轮输出
```

这个过程的通用名字是 **agent loop**（也写 *agentic loop* / *agentic cycle*；
按机制也叫 *tool-use loop*，学术上对应 *ReAct loop*）。源码用语：
TanStack 用 "agent loop"（`packages/ai/src/activities/chat/index.ts:658`）、
"agentic loop"（`:753`、`:6929`）和 "Agentic Cycle"（`agent-loop-strategies.ts:10`）；
Zed 用 "main agentic loop"（`crates/agent/src/thread.rs:3818`）；
pi 的文档标题就是 "Agent loop"（`packages/coding-agent/docs/how-pi-works.md:7`）。

注意业界用 "agent loop" 同时指**这段机制**和**它的一次执行**——这正是术语混淆的根源。
本文的约定：`agent loop` 指过程/机制，`用户回合`（或 `一次执行`）指一次运行，
`model turn` / `迭代` 指一次模型调用。**不单独使用 `turn`**——它容易被读成 `model turn`。

**一个 agent loop 只是会话的一部分。** 会话里可以有多轮 loop 运行，
还有 loop 之外的能力：持久化、压缩、打断、子 agent、审批。
把「loop 等于会话」会混淆两件事：*一次任务怎么跑* 和 *跨任务的状态放哪*。

这个抽象对六家都成立，但有四个必须写明的边界：

1. **一条用户输入 ≠ 严格一个 loop**。Zed 有 `end_turn_at_next_boundary`
   （`thread.rs:1347`）、pi 有 `steeringQueue` / `followUpQueue`（`agent.ts:184`）、
   dsh 有 `steer()` → `next-step` 和 `followup()` → `next-turn`（`agent.ts:181–:185`）、
   TanStack harness 有 `steerQueue`（`turn-parts.ts:868`）——运行中可以再注入用户输入。
   反过来，resume / continue 启动的 loop 可以没有新用户输入。
2. **不保证「最终得到输出」**：可能以取消、预算耗尽、错误结束；取消时也可能
   只提交半截文本（aa-ai 的收尾规则）。
3. **名字不可靠**：dsh 的 `class AgentLoop` 是跨会话的 `Service`/`AgentFactory`，
   不是一次 loop；真正的循环是 `Agent::kick()` → `turn()`。
4. **用户回合是会话的一部分**：一个 session 可有多轮用户回合；session 还持有
   loop 之外的能力（持久化、压缩、打断、子 agent、审批）。

### 10.2 各家的 loop 到底覆盖多大范围

「名字里有 loop」不等于「一次 loop」，也不等于「一个会话」：

| 项目 | 名字 | 作用域 | 证据 |
|---|---|---|---|
| Zed | `Thread::run_turn_internal` | **仅原生 Zed agent**：一次用户回合；`Thread` 是会话容器。外部 agent 会话没有 `Thread`，循环在外部进程 | `crates/agent/src/thread.rs:3102`、`:3114`；`agent.rs:2759` |
| rig | `drive_agent` | 一次 run；历史在 memory store | `crates/rig-agent/src/agent/engine.rs:137` |
| TanStack AI | `chat()` | **一次用户回合**（一条用户消息 → 一次 `chat()` 调用）；内部默认最多 5 个 **model turn**，harness session 是容器 | `packages/ai/src/activities/chat/index.ts:6128`、`:1242`；`agent-loop-strategies.ts:1–:21`；`packages/ai-harness/src/session/turn.ts:390–:401` |
| pi | `agentLoop()` / `runAgentLoop()` 函数 | **一次 run**，context 由参数传入；`Agent.sessionId` 在外部 | `packages/agent/src/agent-loop.ts:47`、`:111`；`agent.ts:226` |
| dsh | `class AgentLoop` | **不是一次 loop**：`Service` / `AgentFactory`，跨会话的工厂；真正的循环是 `Agent::kick()` 的 `while (await this.turn())` | `packages/core/agent-loop/src/index.ts:290`；`agent.ts:111`、`:274`、`:333` |
| aa-ai | `AgentLoop::run` | **一次任务**；`Session` 由调用方传入 | `packages/aa_agent/src/agent_loop.rs` |

所以只有 dsh 把「AgentLoop」用作**服务/工厂名**，其余家要么是函数/方法，
要么是 run 级对象。判断作用域要看谁持有历史，不能看名字。

### 10.3 为什么有的叫 Thread，有的叫 Session

源码里**没有找到解释命名由来的注释**；能确认的只是结构上的对应关系：

| 项目 | 容器叫什么 | 结构事实 |
|---|---|---|
| Zed | **Thread**（仅原生分支） | 内部类型 `Thread`（`crates/agent/src/thread.rs:1328`），协议 id 却是 `acp_v2::SessionId`；`crates/acp_thread/` 做桥接；`Agent.sessions` 是 `SessionId → { thread }` 映射 |
| opencode | session | `packages/core/src/session/` |
| pi | session | `Agent.sessionId`（`packages/agent/src/agent.ts:226`） |
| TanStack AI | 运行时叫 session，持久化 id 叫 `threadId` | `packages/ai-harness/src/session/handle.ts:51`、`:56`；`base.ts:1392` 的 `loadThread` / `saveThread` |
| dsh | session | `Agent.session: Session`（`packages/core/agent-loop/src/agent.ts:141`）；`packages/session/*` |
| aa-ai | session | `aa_agent::session::Session`（`packages/aa_agent/src/session.rs`） |

结论：thread 不是比 session 多一层结构，只是同一概念的命名差异：
Zed 用内部领域词 Thread 对应协议词 session；TanStack 用 session 指运行时对象、
threadId 指持久化标识。

### 10.4 代码分布（含压缩）

| 项目 | 会话容器 | agent loop 所在 | 压缩所在 | 摘要 prompt | 压缩持久化 |
|---|---|---|---|---|---|
| Zed | `crates/agent/src/thread.rs`（`Thread`，仅原生分支） | 同文件，`run_turn_internal`（`:3102`），压缩调用 `:3118` | `crates/agent/src/thread.rs`（`:3532`、`:4864`、`:4918`、`:5440`） | `crates/agent_settings/src/prompts/compaction_prompt.txt` | 压缩消息插进 thread 历史 |
| opencode | `packages/core/src/session/` | `packages/core/src/session/runner/`、`run-coordinator.ts` | `packages/core/src/session/compaction.ts` | 同文件（`SUMMARY_TEMPLATE`，`:173`；UPDATE，`:229`） | 内存 checkpoint + completed 记录 |
| pi | `packages/agent/src/agent.ts`（`Agent`） | `packages/agent/src/agent-loop.ts` | `packages/coding-agent/src/core/compaction/`（`compaction.ts`、`branch-summarization.ts`、`utils.ts`） | `compaction.ts:507`、`:540` | `CompactionEntry` 进会话日志 |
| TanStack AI | `packages/ai-harness/src/session/`（`threadId`） | `packages/ai/src/activities/chat/index.ts:6128` 的 `chat()` | `packages/ai-compaction/src/`（`index.ts`、`summarizer.ts`、`record.ts`、`usage-count.ts`） | `summarizer.ts:108`、`:116` | `CompactionRecord` 进宿主日志 |
| dsh | `packages/session/*`，`Agent.session` 持有 | `packages/core/agent-loop/src/`（`agent.ts`、`index.ts`） | `packages/compaction/compaction-basic/src/` | `summarizer.ts:33–:66` | `compaction/*` 日志事件 + 带 `surfaceOp` 的 `user/message` |
| aa-ai | `packages/aa_agent/src/session.rs` | `packages/aa_agent/src/agent_loop.rs` | `packages/aa_agent/src/compaction.rs`（prompt 同文件 `:35–:66`） | 同左 | `SessionEvent::Compaction`（`session.rs:57–:68`） |

分布模式：压缩代码通常与会话容器同层或更外层，不在 agent loop 的核心模块里。
例外是 aa-ai：压缩函数单独成模块，但由 `AgentLoop::run` 直接调用。
原生 Zed 的压缩实现甚至和 `Thread` 在同一个文件；dsh / TanStack 把压缩做成独立包，
再通过事件钩子或 middleware 挂回 loop。
