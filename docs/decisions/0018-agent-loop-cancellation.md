# 0018. agent loop 取消：同步 token + 异步通知，取消也收尾会话

> 状态：accepted
> 日期：2026-11-26
> 关联：[`docs/compare/agent-loop-abort.md`](../compare/agent-loop-abort.md)（Zed / rig /
> TanStack / pi / dsh 的事实与设计空间）、[0013](0013-session-log.md)（会话事件）、
> [0015](0015-compaction.md)（压缩）、
> [`packages/aa_agent/src/cancel.rs`](../../packages/aa_agent/src/cancel.rs)、
> [`packages/aa_agent/src/agent_loop.rs`](../../packages/aa_agent/src/agent_loop.rs)

## 背景与问题

一次 agent run 可能持续很久：模型流式输出、连续调工具、超预算时还要先发一次
摘要请求。此前 `AgentLoop::run` 没有任何取消入参：

- 流循环只转发事件，不检查调用方是否还想继续；
- 流结束后无条件执行本轮**全部**工具，再进下一轮；
- `ToolEvents::cancelled()` 只传给工具，循环自己不读；
- 唯一的「停止」是硬编码的轮数上限（现为 `AgentLoop::max_turns`），但它返回 `LlmError::Transport`，把
  「预算用尽」伪装成网络错误。

用户按 Ctrl-C、workflow step 被取消、上层超时，都需要在**下一个检查点**停下来。
但「停下来」不能只是不再发请求：如果模型已经产出含 `tool_calls` 的 assistant 轮，
历史里就必须有对应的工具结果，否则下一轮请求会带孤儿调用（provider 直接 400）。

约束：

- 库层不读信号、不读配置：token 由应用创建并传入；
- 工具接口是同步的（`ToolEvents::cancelled()`），不能被异步取消传染；
- 取消要能中断正在读的 HTTP body，而不是等流自然结束；
- 会话日志是审计记录，取消不能留下结构非法的历史。

## 调研

五家的完整事实见 [`docs/compare/agent-loop-abort.md`](../compare/agent-loop-abort.md)。
与决策直接相关的结论：

| | 取消来源 | 传播 | 取消后会话 | 原因 |
|---|---|---|---|---|
| Zed | thread / turn | 向下到工具 | 已收到的文本 + 工具哨兵结果 | 不记录 |
| rig | hook 决策，可自下而上 | 双向 | 保留已结算工具结果 | `run.cancel_error` |
| TanStack | `AbortSignal` | 向下 | 不保证半截 turn | `AbortError` |
| pi | run 级 `AbortController` | 向下 | 半截文本 + 未派发工具写 `isError` | 不记录 |
| dsh | `Agent.cancel(cause)` | 向下，带收件箱策略 | 未派发工具写 `AbortError`；崩溃可补齐 | 写进 `turn/end` |

共同点：**只要有 tool_call，就必须有 result**。差别在「半截文本提交还是丢弃」
和「原因是否入历史」。

## 当时的想法（选项）

1. **只靠外部 drop / timeout**
   - `tokio::time::timeout` 直接 drop future，应用自己处理会话。
   - 实现最小；但循环没有机会收尾，半截 assistant / 工具结果会丢，取消后会话
     可能不合法。aa-workflow 的 step 超时就是这种硬切，只能当兜底。
2. **异步取消 trait（`async fn is_cancelled()`）贯穿到底**
   - 贴近 TanStack 的 `AbortSignal`；但 `ToolEvents` 是同步接口，工具实现会被迫
     改成 async 轮询，取消能力传染到每个工具。
3. **同步 token + 异步通知（采纳）**
   - 工具侧同步读原子；loop 侧 `select!` 等通知，取消时立即 drop 流。
   - 一个类型同时服务同步工具和异步 loop，不改变工具接口。

## 决策

### D1：`CancelReason` 是闭合集合，首个原因胜出

```rust
pub enum CancelReason { User, Timeout, Upstream, Budget }
```

- `User`：用户主动停止（Ctrl-C / UI）；
- `Timeout`：协作式超时（硬超时仍由上层 drop future 兜底）；
- `Upstream`：workflow step / 父 agent 取消（对齐 dsh 的 `parent`）；
- `Budget`：轮数预算（`AgentLoop::max_turns`）用尽（对齐 dsh 的 `hook`）。

`cancel()` 用 `compare_exchange(0, reason)`：第一个原因胜出，后续调用幂等。
不建模 dsh 的 `disposed`：进程/作用域销毁由 Rust Drop 兜底。

### D2：`CancelToken` = `Arc<AtomicU8>` + `Notify`

- 同步接口：`is_cancelled()` / `reason()` / `check()`，工具热路径只有一次原子读；
- 异步接口：`cancelled().await`，先 `Notified::enable()` 再读原子，避免
  「检查通过 → 置位 → 注册等待」的丢唤醒窗口；
- 克隆共享同一个原因；默认未取消。

### D3：`AgentLoop::run` 显式接收 token

```rust
pub async fn run(
    &self,
    model: &provider::LanguageModel,
    session: &mut session::Session,
    events: &impl tool::ToolEvents,
    cancel: &CancelToken,
) -> Result<String, AgentError>;
```

`AgentError` 增加 `Cancelled(CancelReason)`。不取消的调用方传
`&CancelToken::default()`；不提供「可选 token」的重载，避免两条路径行为分叉。

### D4：检查点

按代价从低到高：

1. **压缩前**：取消时直接返回，不读历史、不发摘要请求；
2. **摘要流内**：`select!` 等 `cancelled()` 与 `stream.next()`，取消即 drop 流；
3. **每轮请求前**：构造 `Request` 之前；
4. **主模型流内**：同样 `select!`，`biased` 让取消优先于事件；
5. **每个工具调用前 / 后**：前一个检查避免派发新工具，后一个检查让
   「工具执行期间收到取消」也能停止下一轮。

取消时显式 `drop(stream)`，让 reqwest 中断 body 读取；不 `continue` 到下一轮。

### D5：取消也收尾会话

- 流中途取消：已收到的文本 / 思考 / 工具调用照常 `push_assistant`。
  半截文本是模型真实输出，提交它（与 Zed 一致）；空轮由 `push_assistant` 忽略。
- 本轮有 `tool_calls`：
  - 已执行的调用保留真实结果；
  - 未执行的调用（包括流中途取消时全部调用）写
    `content = ["tool canceled"]`、`is_error = true` 的哨兵结果；
  - 结果仍合并成一条 user 消息，保持 `assistant → tool_result` 配对。
- 取消发生在任何内容之前：会话只留下待回复的用户消息，下一轮可以重试。

### D6：轮数预算（`max_turns`）走取消通道

命中上限返回 `AgentError::Cancelled(CancelReason::Budget)`，不再构造
`LlmError::Transport`。调用方可以区分「模型/网络失败」和「预算用尽」。

### D7：`ToolEvents` 做桥接，不改工具接口

`AgentLoop` 内部把调用方的 `events` 和本次 token 包成 `RunEvents`：

```rust
fn cancelled(&self) -> bool {
    self.cancel.is_cancelled() || self.inner.cancelled()
}
```

工具继续实现原来的同步 `cancelled()`，不需要知道 token 类型；调用方自己已有的
取消源（例如 UI 的停止状态）也继续生效。

### D8：不支持自下而上的取消

工具返回错误是**结果**，不是取消：它只影响这一条 tool_result，不终止 run。
rig 的 `ToolExit::Stop` 这类「工具升级为 run 取消」暂不做——取消源保持单一
（调用方 token），语义更容易推理。将来做子 agent 时再单独设计。

### D9：取消原因不写进会话日志

会话日志记录的是**模型看到的对话事实**；取消原因是调用方的运行时状态。
哨兵工具结果已经足够让重放保持结构合法，因此不新增 `SessionEvent` 变体、
不升会话版本。

代价：恢复后分不清「正常结束」和「被取消」。如果以后 UI 需要，可以新增
`turn_end` 事件并升 v3，而不是把原因塞进现有事件。

### D10：压缩也可取消，取消不落压缩事件

`compaction::maybe_compact` 增加 `cancel: &CancelToken`，摘要流内同样
`select!`。取消返回 `CompactionError::Cancelled(reason)`，**不**调用
`push_compaction`：会话保持压缩前的历史，下一轮可以重新压。

## 验证

`cargo test -p aa_agent`：

- `cancel::tests::*`：默认未取消、首个原因胜出、克隆共享、取消后立即返回、
  等待者被唤醒；
- `cancel_before_run_leaves_the_session_untouched`：取消发生在 run 前，
  不发请求、不写事件；
- `cancel_mid_stream_commits_the_partial_text`：流出一个增量后取消，
  半截文本进历史，loop 主动 drop 流；
- `cancel_during_tool_batch_sentinels_the_unexecuted_calls`：第一个工具执行时
  取消，第二个调用写哨兵结果，不再发下一轮请求，`ToolEvents::cancelled()`
  能看到 token；
- `exhausting_the_tool_round_budget_is_a_cancellation`：轮数预算用尽后返回
  `Cancelled(Budget)`。

`aacode` 侧：

- `CancelHub` 维护「当前 turn」槽位；常驻 Ctrl-C watcher 第一次取消，
  槽位为空或 token 已取消时再按一次硬退出（130）；
- `chat_turn` / `run_agent` 在取消后仍 `persist`，半截 turn 和哨兵结果落盘。

## 后果

**正面：**

- 用户按 Ctrl-C 能中断流式输出 / 工具循环 / 摘要请求，HTTP body 被 drop；
- 会话始终保持 `assistant(tool_calls) → tool_result` 配对，恢复后可继续；
- 工具接口零改动，现有工具通过 `cancelled()` 自动获得取消能力；
- `Budget` 与传输错误分开，调用方可以决定重试还是提示。

**代价 / 边界：**

- 取消是协作式的：`stream.next()` 与工具内部 I/O 的取消延迟取决于检查点；
  真正卡死的调用仍需上层 `timeout` 硬切；
- 每个流事件多一次 `Notify` 注册；取消的响应性优先于这点开销；
- 取消原因不持久化（D9），恢复后只能从哨兵结果推断「这一轮被取消过」；
- 不支持工具自下而上取消 run（D8）。

**触发重新评估的条件：**

- 出现子 agent / agent-as-tool，需要把父级取消传播给子 run；
- workflow 需要区分「step 完成」与「step 被取消」并做不同重放策略；
- 摘要请求需要单独的模型/超时策略，取消语义要跟着拆分。
