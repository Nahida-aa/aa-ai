# agent loop 的取消：层级、传播与五家实现（设计草案）

> 性质：**事实对照 + 设计提案**。第 1–2 节只记源码事实，第 3 节起是提案，
> 行号以本地克隆为准。
>
> - Zed：`/home/aa/repos/ide_ls/learn_ls/zed`（commit `4ed72e0135`）
> - rig：`/home/aa/repos/lib_ls/learn_ls/rig`（commit `6e13e8c`）
> - TanStack AI：`/home/aa/repos/lib_ls/learn_ls/ai`（commit `6059872f`）
> - pi：`/home/aa/repos/ai_ls/learn_ls/pi`（commit `406e719a6`）
> - dsh（deepseek-harness）：`/home/aa/repos/ai_ls/learn_ls/deepseek-harness`（commit `e8f7b93a34`）
> - aa-workflow：`/home/aa/repos/lib_ls/aa-workflow`（commit `81631aa`）
> - aa-ai：本仓 HEAD

## 0. 结论先行

1. **取消是 agent loop 的契约，不是某个工具的能力。** 循环是唯一持有 future、
   决定「还发不发下一轮」的地方；工具自己停下来，循环不处理，取消就不成立。
2. **agent tool call 是 loop 取消的一个子集**：它是传播目标（自上而下），
   也可以作为来源（自下而上，如 rig 的 `ToolExit::Stop`）。两者不能混为一谈。
3. 我们最初两层都缺；现已按 §4 落地为 ADR 0018（`CancelToken` + 检查点 +
   哨兵结果），实现见 §2.6。
4. 设计上必须先回答「取消后会话怎么收尾」，否则会出现
   `assistant(tool_calls)` 后面没有对应 `tool_results` 的坏历史。
5. pi 与 dsh 正好代表两个成熟度端点：pi = 一个 run 级 `AbortSignal` 就够用；
   dsh = 原因分类 + 收件箱策略 + 崩溃补齐。我们按 pi 的机制起步、按 dsh 留扩展点。

## 1. 层级模型

```text
session / run                     ← 应用或编排层（workflow step）
└─ agent loop                     ← 取消的归属层：契约、状态机、错误
   ├─ model turn  ──────────────┐ ← 检查点：轮首、每个流事件后
   │  └─ provider stream        │ ← drop 流 = 中断 HTTP body
   └─ tool call                 ┘ ← 子集：loop 取消的传播目标
      └─ tool 内部 I/O             ← 只能协作式轮询（Rust 无法抢占）
```

| 层 | 谁拥有 | 取消手段 | 能否硬中断 |
|---|---|---|---|
| L0 传输 | reqwest / fetch | drop 流 | 是（连接被取消） |
| L1 provider 流 | `stream_completion` | drop / 停止 poll | 是 |
| L2 agent loop | `aa_agent` | 检查点 + 返回错误 | 否，只能合作 |
| L3 tool call | `AgentTool` | 同步 `cancelled()` / 信号 | 否，工具自行轮询 |
| L4 step/run | aa-workflow | `StepCtx::is_cancelled` / 超时 | 超时会 drop future |

关键点：L4 的 `tokio::time::timeout` 是**硬切**（[engine/mod.rs#L1041-L1042](file:///home/aa/repos/lib_ls/aa-workflow/packages/workflow_core/src/engine/mod.rs#L1041-L1042)），
它在超时瞬间 drop 整个 step future；L2 的检查点只对「提前发出的取消」有效，
对「刚好超时」无效。所以两层都需要：协作检查负责优雅收尾，硬切负责兜底。

## 2. 五家的事实

### 2.1 Zed：thread 级取消 + 工具哨兵结果

- `Thread::cancel()` 先取消所有 `running_subagents`，再 `running_turn.cancel()`
  ——[thread.rs#L2538-L2555](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2538-L2555)；
- 每次 turn 建 `watch::channel(false)`，把 `cancellation_rx` 传给工具执行
  （[#L2911-L2933](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2911-L2933)、[#L3107-L3115](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L3107-L3115)）；
- 工具被取消后**不丢弃当前位置**，而是写入哨兵结果
  `"Tool canceled by user"`（[#L74](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L74)），
  并把它识别为 canceled（[#L1770-L1773](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L1770-L1773)）；
- 重放时用一个已完成的假 channel，保证「已完成的工具不重跑」
  （[#L1746-L1753](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L1746-L1753)）。

要点：**取消源在 thread，传播到工具；会话保持结构合法**（有 tool_call 就有 result）。

### 2.2 rig：没有外部信号，取消是「决策」

- 循环里没有 `AbortSignal` 这类入参；取消来自 hook 的显式决定：
  `RunStartAction::Stop`、`CompletionCallAction::Stop`、`ModelSelectionAction::Stop`
  （[engine.rs#L252](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L252)、[#L282](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L282)、[#L304](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L304)）；
- 工具侧有 `ToolExit::Stop(reason)`，会升级成整个 run 的取消
  （[#L605](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L605)）——这是**自下而上**的来源；
- 流式场景中，hook 说停就 drop 流：「the stream is dropped, cancelling
  the dispatch in flight」（[#L735-L737](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L735-L737)）；
- 取消统一走 `run.cancel_error(reason)`，已结算的工具结果**保留**并写入
  （[#L583](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L583)）。

### 2.3 TanStack AI：AbortSignal 贯穿到底

- `chat()` 接受 `abortController`，把 `abortSignal` 传进 adapter
  （[index.ts#L6476](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L6476)）；
- 流循环与轮次之间检查 `abortSignal?.aborted`
  （[#L6572](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L6572)、[#L6686](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L6686)）；
- 工具执行也检查（[tool-calls.ts#L1072](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/tools/tool-calls.ts#L1072)）；
- adapter 把 signal 传给 `fetch`（[ai-openai evaluate.ts#L141](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai-openai/src/adapters/evaluate.ts#L141)）；
- agent 循环用 `orAbort(iterator.next(), signal)` 等待 chunk
  （[spawn.ts#L349](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/agents/spawn.ts#L349)、[#L812](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/agents/spawn.ts#L812)）；
- `ai-orchestration` 的 `defineAgent.run` 直接拿到 workflow step 的
  `signal` / `abortController`，超时与取消一路传进来。

要点：**只有自上而下**，没有「工具说停就停整个 run」的设计；取消表现为
`AbortError`，不保证把半截 turn 写进历史。

### 2.4 pi：run 级 AbortSignal + 剩余工具写错误结果

- 每个 run 一个 `AbortController`，`Agent.abort()` 直接 abort 当前 run
  （[agent.ts#L346-L352](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent.ts#L346-L352)）；
- signal 贯穿 agent loop，工具批处理里多处检查：
  [#L585](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L585)、
  [#L623](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L623)、
  [#L630](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L630)、
  [#L651](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L651)，
  并行执行器里同样检查（[#L748](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L748)、[#L767](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L767)）；
- 已 abort 但还没派发的调用不会被执行，而是生成
  `"Operation aborted"` 的 `isError: true` 结果（[#L629-L638](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L629-L638)，并行路径在 [#L748-L770](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L748-L770)）——
  和 Zed 的哨兵一致，保持历史配对；
- signal 也传给工具执行本身（`executePreparedToolCall(preparation, signal, ...)`），
  工具可自行协作退出；
- 没有原因分类，`abort()` 不带参数。

### 2.5 dsh：取消是一等 API，带原因和收件箱策略

- `Agent.cancel(cause, { keepInbox })`（[agent.ts#L192-L202](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/src/agent.ts#L192-L202)）；
- cause 是闭合联合：`user` / `parent` / `disposed` / `hook(reason)`
  （[#L97-L110](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/src/agent.ts#L97-L110)），
  会被写进 `turn/end`；
- 默认清空 inbox（排队中的用户输入/steering），`keepInbox: true` 则保留，
  等 idle 后重新唤醒；取消只 abort 当前 phase 的 controller；
- 尚未派发的工具调用写成 `AbortError` + `TOOL_ABORTED_BEFORE_DISPATCH`
  （[cancel.spec.ts#L570](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/tests/cancel.spec.ts#L570)、
  [tool-calls.spec.ts#L534-L535](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/tests/tool-calls.spec.ts#L534-L535)），
  历史保持配对；
- 崩溃/进程退出导致的「最后 turn 未闭合」，在 store 层用
  `interruptedTurnClosers(events)` 补齐（[crash-recovery.e2e.ts#L68-L78](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/session/session-checkpoint-policy/tests/crash-recovery.e2e.ts#L68-L78)）；
- 测试覆盖每个 landing window、signal 重置、`whenIdle()` 静默和「turn 记录的原因」
  （[cancel.spec.ts#L1-L10](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/agent-loop/tests/cancel.spec.ts#L1-L10)）。

这是四家里语义最完整的一套：**原因可分类、队列可保留、工具哨兵化、崩溃可补齐**。

### 2.6 aa-ai 现状（已实现）

- `AgentLoop::run` 接收 `&CancelToken`（[agent_loop.rs#L121-L127](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L121-L127)）；
- 检查点：run 前（[#L138-L140](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L138-L140)）、
  压缩阶段（[#L141-L156](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L141-L156)）、
  每轮开头（[#L161-L163](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L161-L163)）、
  流循环用 `tokio::select!` 等取消并 drop 流（[#L188-L223](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L188-L223)）、
  每个工具前后（[#L272-L276](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L272-L276)、[#L290-L294](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L290-L294)）；
- 会话收尾：先写 assistant（含半截文本），未执行的调用写
  `"tool canceled"` 哨兵（[#L234-L260](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L234-L260)、[#L297-L303](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L297-L303)）；
- 工具看到的 `cancelled()` 由 `RunEvents` 合并 token 与调用方事件源
  （[#L312-L326](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L312-L326)）；
- 轮数预算走同一个错误通道：`Cancelled(Budget)`（`AgentLoop::max_turns`，默认 `None` 不限）；
- token 本体：`AtomicU8`（首个原因胜出）+ `Notify`（同步读、异步等），
  明确不做自下而上取消（[cancel.rs#L1-L12](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/cancel.rs#L1-L12)）；
- 测试覆盖：run 前取消、流中途取消、工具批中途取消、预算耗尽
  （[agent_loop.rs#L818-L992](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L818-L992)）。

## 3. 设计空间：必须先回答的问题

| # | 问题 | 选项 | 参考 |
|---|---|---|---|
| Q1 | 取消源有哪些 | 用户 / 超时 / 上游 step / 轮数预算 | 各家都有用户与超时 |
| Q2 | 检查点粒度 | 轮首 / 每个流事件 / 每个工具前 | TanStack 两种都查；Zed 查 channel |
| Q3 | 取消后会话怎么写 | 丢弃半截 / 提交半截 / 提交半截+哨兵结果 / 只记标记 | Zed、pi、dsh 都是哨兵；rig 保留已结算；TanStack 不保证 |
| Q4 | 传播方向 | 只自上而下 / 也允许工具向上取消 run | TanStack 单向；rig 双向；Zed、pi、dsh 以向下为主（pi 的工具批 `terminate` 不算取消） |
| Q5 | API 同步还是异步 | 同步 `fn cancelled()` / 异步 `is_cancelled().await` | 工具侧是同步；workflow 是异步 |
| Q6 | drop 流是否算取消 | 是（中断 HTTP）/ 否（等读完） | rig 明确 drop；TanStack 传 signal |
| Q7 | 超时是否复用取消 | 复用（带 reason）/ 独立错误 | 建议复用，reason 区分 |
| Q8 | 与 orchestration replay 的关系 | 取消即 step 失败 → 整步重跑 / 记录已完成的子步骤 | aa-workflow 是整步重跑（at-least-once） |
| Q9 | 取消原因是否入历史 | 不记录 / 写进 turn 结束原因 / 写进工具结果 | dsh 写 `turn/end`；pi、Zed 不记录原因 |

**Q3 是核心**。Zed、pi、dsh 三家的共同做法是：只要模型已经产出含
`tool_calls` 的 assistant 轮，就必须为每个 call 写一条结果——未执行的写
「已取消」哨兵（Zed 的 `"Tool canceled by user"`、pi 的 `"Operation aborted"`、
dsh 的 `AbortError + TOOL_ABORTED_BEFORE_DISPATCH`），否则 `Session` 的
「assistant → tool 结果」配对会被破坏
（[session.rs#L38-L50](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/session.rs#L38-L50)）。
rig 更进一步保留已结算工具的真实结果，只对未执行的调用收尾。

dsh 的 Q9 还给出了一个额外选项：把取消原因写进 `turn/end`，这样恢复时能区分
「用户主动停止」「父级取消」「进程关闭」。

## 4. 提案（已落地，见 ADR 0018）

实现与本节一致；差异只有一点：没有采用「半截文本丢弃」选项，而是固定为
提交半截文本（见 4.4）。

### 4.1 取消类型

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    User,
    Timeout,
    Upstream,   // workflow step 取消
    Budget,     // 轮数预算用尽
}

#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicU8>); // 0 = 未取消，其余 = reason

impl CancelToken {
    pub fn cancel(&self, reason: CancelReason);
    pub fn is_cancelled(&self) -> bool;          // 同步，工具可直接读
    pub fn reason(&self) -> Option<CancelReason>;
}
```

选同步的 `AtomicU8` 而不是 `tokio::sync`：`ToolEvents::cancelled()` 是同步接口，
同步 token 可以直接桥接；workflow 的异步 `is_cancelled()` 由调用方起一个
watcher 任务写进 token 即可，不需要把异步传染给工具层。

来源分类与 dsh 的 `AgentCancelCause` 对齐，但按我们的场景裁剪：

| 本提案 | dsh 对应 | 说明 |
|---|---|---|
| `User` | `user` | 用户主动停止 |
| `Timeout` | —（workflow 超时硬 drop） | 协作式超时 |
| `Upstream` | `parent` | 上层 step/父 agent 取消 |
| `Budget` | `hook` | 轮数/预算用尽 |

dsh 的 `disposed`（进程/作用域销毁）在我们这里由 Rust Drop 兜底，暂不建模。

### 4.2 API 变化

```rust
pub async fn run(
    &self,
    model: &provider::LanguageModel,
    session: &mut session::Session,
    events: &impl tool::ToolEvents,
    cancel: &CancelToken,          // 新增
) -> Result<String, AgentError>;
```

`AgentError` 增加：

```rust
Cancelled(CancelReason),
```

`max_turns` 设置后用尽即返回 `Cancelled(Budget)`，不再伪装成传输错误。

### 4.3 检查点

按代价从低到高，全部都是 `is_cancelled()`：

1. 每轮开始、构造 `Request` 之前；
2. 每个 `LanguageModelCompletionEvent` 之后（文本增量已经转发，取消只是停止继续 poll）；
3. 每个工具调用之前；
4. 工具内部：通过 `ToolEvents` 的 `cancelled()` 自行轮询。

取消时**显式 drop `stream`**，让 reqwest 中断 body 读取；不要 `continue` 到下一轮。

### 4.4 会话收尾规则（建议）

- 本轮已有 `tool_calls`：先 `push_assistant`，再为**每个** call 写结果；
  已执行的写真实结果，未执行的写哨兵（`"tool canceled"`，`is_error = true`），
  与 Zed/pi/dsh 一致。
- 本轮只有文本：提交已收到的部分文本（`Turn.stop = None`），由调用方通过
  `Err(Cancelled(reason))` 判断这次 turn 不完整；也可以选择丢弃，取决于上层是否
  需要「中断时保留半截输出」。
- 任何情况下不留下「有 call 无 result」的历史。

已拍板并实现：**提交半截文本**（与 Zed 一致），不丢弃。
判断依据是「已收到的文本是模型真实输出」，调用方通过 `Err(Cancelled(reason))`
自行决定是否使用；TanStack 不保证，rig 则保留已结算的工具结果。

### 4.5 与 aa-workflow 的桥接

```text
StepCtx::is_cancelled()  (async)
        │ watcher task
        ▼
CancelToken (AtomicU8) ──► AgentLoop 检查点 ──► ToolEvents::cancelled()
```

- workflow 取消 → token 立即置位 → 循环在下一个检查点退出；
- step 超时 → `tokio::time::timeout` 直接 drop future，token 来不及生效，
  属于兜底；工具侧副作用必须自行幂等；
- step 被取消导致整步重跑（at-least-once），与 Zed 的「已完成工具不重跑」不同，
  这点要在 orchestration 层单独处理（比如把已完成工具结果写进 step 输出）。

### 4.6 测试计划

| 测试 | 断言 |
|---|---|
| 流中途取消 | 取消后不再产生 LLM 请求；已收到的文本按收尾规则处理 |
| 工具批中途取消 | 未执行的 call 有哨兵结果；历史配对完整 |
| 工具自停（若支持 Q4） | 循环收到取消并停止，而不是继续下一轮 |
| 超时 | 返回 `Cancelled(Timeout)`；future 被 drop 时不 panic |
| 预算耗尽 | `Cancelled(Budget)`，与 provider 错误区分 |
| workflow 桥接 | `cancel_run` 后 loop 在下一个检查点退出；step 重放只重跑一次 |
| 重放短路 | 已完成 step 不触发 `AgentLoop::run`（计数为 1） |

## 5. 未决问题

1. ~~半截文本提交还是丢弃~~（已定：提交，见 4.4）。
2. 是否支持**自下而上**的取消（工具 `Stop` 升级为 run 取消）？rig 支持，
   TanStack 不支持；我们如果只做 orchestration，可以先不做。
3. 取消哨兵结果的文本与 `is_error` 语义，是否需要进 provider wire 的
   `tool_result.is_error`（影响下一轮模型解读）。
4. 子 agent（agent-as-tool）如何传播取消——Zed 用 `running_subagents` 逐个
   `request_cancel`；我们还没有子 agent 概念。
5. 取消原因是否写进 `Session`（dsh 写 `turn/end`）？需要新增 `SessionEvent`
   变体并确认不影响旧日志重放；如果不写，恢复后就分不清「完成」与「被取消」。
6. **请求建立阶段的取消**：`stream_completion(...).await` 不在 `select!` 里，
   token 置位要等响应头到达才生效。Zed 靠 `running_turn.cancel()` drop task 覆盖
   这个窗口；我们是补 `select!`，还是明确「建立阶段只能靠上层 drop future」？
