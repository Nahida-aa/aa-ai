# 上下文压缩跨项目对照：触发、保留、摘要请求、持久化

> 只记事实：常量、函数、事件、行号 + commit。**不做取舍、不给建议**。
>
> 本文补的是 `docs/compare/` 缺失的那一层：上下文压缩本身。`agent-loop.md` 只记
> 压缩**发生在哪一层**，`agent-loop-output.md` 只记压缩**产物的形状**；
> 决策与取舍在 [`../decisions/0015-compaction.md`](../decisions/0015-compaction.md)。

## 源码版本

克隆根目录见 [`AGENTS.local.md`](../AGENTS.local.md)；下表只记 commit，
各节的源码位置一律用**项目内相对路径**。

| 项目 | commit |
|---|---|
| Zed | `2b248e6072` |
| opencode | `d7ae4f2e3c` |
| pi | `406e719a6` |
| TanStack AI | `6059872fe` |
| dsh（deepseek-harness） | `e8f7b93a34` |
| aa-ai | 当前工作区 |

## 0. 事实概览

- 六家都采用「LLM 生成文本摘要 + 原样保留最近一段原文」；区别在于保留什么、
  切点怎么选、摘要请求怎么发、压缩事实怎么落盘。
- **保留单位**：Zed / aa-ai 用**字节**（80,000，Zed 只留用户文本）；
  opencode / pi / dsh 用**token**；TanStack 用消息条数 + `keep` token 预算。
- **切点安全**：pi / dsh / opencode 都明确不切 `tool_call` / `tool_result` 配对；
  Zed / aa-ai 只保留用户消息，天然不涉及工具对。
- **摘要请求**：dsh 显式回放 system + tools + 前缀以复用 KV cache；opencode
  有后台预计算；Zed 保留工具定义；pi 把历史序列化成文本；aa-ai 不带工具。
- **摘要 prompt**：opencode / pi / TanStack / dsh 都有「把旧 checkpoint 合并进
  新摘要」的指令；Zed / aa-ai 没有。
- **溢出处理**：opencode 有 SHRINK_STEPS 收缩重试；dsh 有 `maxOverflowRetries`；
  Zed / pi / TanStack / aa-ai 没有溢出后的自动压缩重试。
- **持久化**：Zed 插入压缩消息；pi 追加 `CompactionEntry`；TanStack 写宿主日志
  记录 `CompactionRecord`；dsh 写 `compaction/*` 日志事件 + 一条带 `surfaceOp`
  的 `user/message`；opencode 保存内存 checkpoint / completed 记录；
  aa-ai 追加 `SessionEvent::Compaction`（会话格式 v2）。

## 1. 总表

| 维度 | Zed | opencode | pi | TanStack AI | dsh | aa-ai |
|---|---|---|---|---|---|---|
| 触发阈值 | 输入容量的 90%（可配）；容量 < 80k 不自动压 | 窗口 − `max(16k, 15%)` | 窗口 − 16,384 | 独立包，由宿主决定 | `floor(min(W×0.8, 预算−headroom))`，默认 headroom 65,536 | 输入容量的 90%；容量 < 80k 不自动压 |
| 触发位置 | thread 每轮请求前（满足条件时） | session 层，支持后台预计算 | 会话层 | `chat()` middleware，每次模型调用前 | `agent/pre-step`（每步）、模型切换、`agent/request-error` 溢出 | `AgentLoop::run` 开头；最后一条必须是用户文本 |
| 保留尾部 | 最近 80KB **用户文本** | 默认 15k token，从 user 边界开始 | 最近 20k token | `keep` token 预算 + 尾部消息原样 | 按 token 定价保留尾部，再向前扩展到工具对平衡边界 | 最近 80KB **用户文本**，UTF-8 边界截断 |
| 不切工具对 | 只留纯用户文本 | user 边界切点 | 切点在 user/assistant，永不在 tool result | 按完整消息保留尾部 | `toolPairingBalancedBefore/After` 双向校验 | 只留纯用户文本 |
| 摘要请求 | 完整请求 + prompt，保留工具定义 | 完整对话 + prompt；溢出时按 SHRINK_STEPS 收缩 | 历史序列化成文本；工具结果截 2k 字符 | `Summarizer(messages, input)` 回调 | 回放 system + tools + 被遮蔽区间，指令作为最后一条 user | 当前生效历史去掉待回复 user + prompt；不带工具 |
| 摘要模型 | 可单独配 | 可单独配 | 默认同模型 | 由 summarizer 决定 | `summarizationProvider/Model`，回退最近路由 / AgentOptions | 当前会话模型 |
| 分节结构 | Goal / State / Context / Next / Pitfalls | Objective / Requirements / … | Goal / Constraints / Progress / Key Decisions / Next Steps / Critical Context | checkpoint summary 分节 | Primary Request / Key Technical Concepts / Files and Code / Errors and Fixes / Pending Jobs / Current Work / Next Step / Critical Context | Goal / Requirements / Decisions / Work State / Next Move / Relevant Files / Important Context |
| 合并旧摘要 | 无 | UPDATE prompt，新历史优先 | UPDATE instructions，保留仍为真的事实 | UPDATE_PROMPT，合并 `<previous-summary>` | prompt 明确要求合并已有 `<compacted-summary>` | 无 |
| 落盘 | `Message::Compaction` | 内存 checkpoint + completed 记录 | `CompactionEntry { summary, firstKeptEntryId, tokensBefore }` | `CompactionRecord { head, from, firstKeptId, tokensBefore, tokensAfter, reason }` | `compaction/start`/`summary`/`end` 日志事件 + `user/message` 带 `surfaceOp: replace` | `SessionEvent::Compaction { summary, retained_users, first_kept_message, tokens_before }` |
| 溢出重试 | 无 | SHRINK_STEPS `[0.85, 0.7, 0.5, 0.35]` | 文档有长度恢复顺序 | 无 | `maxOverflowRetries` 默认 1 | 无 |
| 取消语义 | — | — | — | — | 每步与错误处理路径检查 `signal.aborted` | 摘要流可取消，取消不写压缩事件 |

## 2. Zed

- 常量：`MIN_COMPACTION_CONTEXT_WINDOW = 80_000`（`crates/agent/src/thread.rs:126`）；
  `COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET = 80_000`（`:129`，注释为 4 字节/token ≈ 20k token）。
- 容量：`Thread::input_token_capacity`（`:2696` 附近）调用自由函数
  `compaction_input_capacity`（`:5152`）；阈值计算在 `:4864–:4866`，
  低于阈值直接跳过（`:4866`）。
- 触发：`perform_compaction_if_needed`（`:3532`），调用点 `:3118`；
  小窗口直接不压缩（`:4740`）。
- 摘要请求：`build_compaction_request`（`:4918`），用完整请求加压缩指令，
  保留工具定义。
- 保留：`remaining_bytes` 预算循环（`:5440`），
  `truncate_user_message_to_byte_budget`（`:5241`，调用点 `:5459`）。
- 摘要 prompt：`crates/agent_settings/src/prompts/compaction_prompt.txt`
  分节为 Goal / State / Context / Next / Pitfalls；**没有**合并已有摘要的规则。
- 持久化：压缩作为消息插入历史（见 ADR 0015 的源码核对记录）。

## 3. opencode

- 常量：`SHRINK_STEPS = [0.85, 0.7, 0.5, 0.35]`（`:127`）、
  `RESERVE_MIN = 16_000`（`:129`）、`RESERVE_SHARE = 0.15`（`:135`）、
  `PRECOMPUTE_LEAD = 60_000`（`:158`）、`PRECOMPUTED_TAIL_SHARE = 0.4`（`:170`）、
  默认 `keep = 15_000`、`precompute = false`（`:269`）。
- 阈值：窗口减去 `max(RESERVE_MIN, 窗口 × RESERVE_SHARE)`；未知窗口时用
  `:136` 的假设值。溢出触发用 `SHRINK_STEPS[0]` 收缩估算值（`:318–:320`）。
- 后台预计算：接近上限 `PRECOMPUTE_LEAD` 时启动后台摘要并持有
  （`:342–:399`）；有效期受 `CACHE_LIFETIME` / `CACHE_MARGIN` 约束（`:161–:165`）。
- 切点：`splitConversation`（`:1014`）→ `recentStart`（`:1033`）→
  `oldestToDrop`（`:1057`）；按文本 token 估算丢弃最老条目，
  回退到 user 边界以保证工具调用与结果同批（`:1043–:1045`）。
- 摘要 prompt：`SUMMARY_TEMPLATE`（`:173`）与 UPDATE prompt（`:229`）；
  `:240` 明确「新历史优先，旧 checkpoint 中仍为真的信息保留」。
- 产物：`keepID` 指向原样保留的尾部首条消息（`:114`）；
  `:460–:468` 描述下次请求开头如何展开 checkpoint。
- 失败：`compaction.unavailable` / 无法再收缩（`:665`）。

## 4. pi

- 常量：`reserveTokens: 16384`、`keepRecentTokens: 20000`
  （`packages/coding-agent/src/core/compaction/compaction.ts:128–129`）。
- 触发：`contextTokens > contextWindow - settings.reserveTokens`（`:269`）。
- 切点：`findValidCutPoints`（`:389–:405`）只取上下文可见的 user-like /
  assistant 消息，**从不在 tool result 处切**；切在带 tool_calls 的 assistant 时，
  其结果一并保留（`:390–:392`）。`findCutPoint`（`:431` 附近）累计到
  `keepRecentTokens` 后取最近的有效切点。
- 摘要 prompt：`SUMMARIZATION_PROMPT`（`:507`）固定七节；
  `UPDATE_SUMMARIZATION_INSTRUCTIONS`（`:540`）要求保留旧摘要中仍为真的信息、
  把完成项从 In Progress 移到 Done。
- 产物：`CompactionEntry { summary, firstKeptEntryId, tokensBefore }`
  （`:106–:108`），可带 usage（`:110`）。
- 消息序列化：摘要请求把消息转成文本，工具结果截断到 2k 字符
  （见 ADR 0015 的源码核对记录）。
- 参考文档：`packages/coding-agent/docs/compaction.md`，含
  `Overflow and Length Recovery Ordering`（`:85`）、`Cut Point Rules`（`:128`）、
  `CompactionEntry Structure`（`:140`）。

## 5. TanStack AI

- **位置**：独立 `ai-compaction` 包，但以 `withCompaction()` 返回的
  `ChatMiddleware` 挂进 `chat()`，每次模型调用前运行
  （`packages/ai-compaction/src/index.ts:1–:4`、`:794`）；不是 loop 内建步骤。
- 接口：`Summarizer` 返回 `string | { summary: string; usage?: TokenUsage }`
  （`packages/ai-compaction/src/summarizer.ts:38–41`）。
- 输入：`SummarizeInput` 带 `previousSummary` / `previousDetails`（`:12–:15`），
  以及 `threadId`、工具 schema、system head、被遮蔽区间（`:75–:85`）。
- prompt：`CHECKPOINT_PROMPT`（`:108`）与 `UPDATE_PROMPT`（`:116–:124`），
  后者明确「Merge the new messages into the earlier summary」。
- 摘要包装：`<untrusted-conversation-summary>`（`:43–:50`）；
  可选 `<compaction-details>`（`:52–:67`）；`readSummary`（`:77`）解析旧摘要。
- 模型：由 summarizer 实现决定调用哪个模型（`:28–:31`）。
- 产物：`CompactionRecord { type, reason, tokensBefore, tokensAfter, head, from,
  firstKeptId?, usage? }`（`record.ts:12–:25`）；`compactionRecord()`（`:35`）
  通过比对尾部相同消息计算 `from`；`projectCompaction()` 在每次打开会话时折叠记录。

## 6. dsh（deepseek-harness）

> 本地源码的完整实现；ADR 0015 中「dsh 没有压缩实现」的结论已过时。

### 触发与预算

- 默认：`thresholdRatio = 0.8`（`packages/compaction/compaction-basic/src/config.ts:20`）、
  `headroomTokens = 65_536`、`maxTokens = headroomTokens`（`:75–:78`）、
  `compactionRetries = 1`、`maxOverflowRetries = 1`（`:105–:106`）。
- 阈值：`floor(min(contextWindow × thresholdRatio, pressureBudgetTokens))`
  （`:191–:193`），其中 `pressureBudgetTokens = messageBudgetTokens − headroomTokens`
  （`:181`）。
- 保留：`retainTokens = floor(messageBudgetTokens × retainRatio)`（`:195–:197`）；
  `retainTokens >= thresholdTokens` 视为配置错误（`:198–:202`）。
- 可按 `provider/model` 覆盖（`config.ts:125–:137`）。
- 触发点：`agent/pre-step` 每步请求前检查压力（
  `compaction-basic/src/index.ts:157–:172`）；`agent/request` 在模型切换前压缩
  （`:181` 附近）；`agent/request-error` 收到 `CONTEXT_WINDOW_EXCEEDED_CODE`
  后压缩并重试（`:213` 起），次数由 `maxOverflowRetries` 控制（`:223`）。
- 多次压缩：`for (attempt = 0; attempt <= compactionRetries; attempt++)`
  （`:353`），每轮重新测量（`:367`），仍超阈值则抛错（`:368–:370`）。
- 取消：上述钩子都先检查 `signal.aborted`（`:159`、`:216`）。

### 选段

- `selectCompactableRange`（`region.ts:120–:154`）：从尾部按 token 累加直到
  `retainTokens`，再向前移动切点到 `toolPairingBalancedBefore` 成立
  （`:143–:146`）；system head（节点 0）永不在区间内（`:109–:111`）。
- 区间边界再次校验 `toolPairingBalancedBefore/After`（`:349–:354`），
  否则抛错，不拆开工具的调用/结果对。
- 摘要必须比被遮蔽内容小：`framedSummaryTokenCount >= shadowedRouteTokenCount`
  视为失败（`region.ts:420`）；触顶输出按 `MAX_TOKENS` 失败处理
  （`summarizer.ts:204`）。

### 摘要请求与 prompt

- 指令作为**最后一条 user 消息**发出，前面回放会话自身的 system、tools 和消息前缀，
  目的是复用 provider 的 KV cache（`summarizer.ts:26–:30`）。
- 请求 envelope：`toolHistory`、可选 `tools`、derived system head + 被遮蔽区间
  （`:154–:157`）；模型取 `summarizationProvider/Model`，回退最近路由或
  `AgentOptions`（`:130–:140`）。
- 分节固定为 Primary Request and Intent / Key Technical Concepts / Files and Code /
  Errors and Fixes / Pending Jobs / Current Work / Next Step / Critical Context
  （`:33–:60`）。
- **合并旧摘要规则**：`:66` 明确「如果对话中已有 `<compacted-summary>` 块，
  它是先前的 checkpoint；不要逐字照搬，保留仍为真的事实、丢弃过期信息，
  并按同一结构合并成单一摘要」。
- checkpoint framing：`<compacted-summary>` 标签（`summarizer.ts:22–:23`），
  替换消息的前导语在 `:71`。

### 落盘

- 三种**仅日志**事件：`compaction/start`（锁）、`compaction/summary`（摘要、
  遮蔽范围、seq、token、模型 envelope）、`compaction/end`（释放锁）
  （`docs/subsystems/compaction.zh.md:15–:17`）。
- 锁覆盖整个事务：先 start，再摘要 + 写 summary + surface 替换，最后 end
  （`:19`）；中途崩溃留下可检测的未闭合 start；`session/end-seed` 之后的
  未闭合 start 视为陈旧（`:21`）。
- 摘要本身承载在另一条 `user/message` 上，带
  `surfaceOp: { op: 'replace', startSeq, endSeq }`（`:11`）。这是唯一的 surface 变更。
- 未派发的工具调用写 `AbortError + TOOL_ABORTED_BEFORE_DISPATCH`；
  崩溃导致的未闭合用户回合由 store 层补齐（见 `docs/subsystems/persistence.zh.md`）。

## 7. aa-ai（当前实现）

- 预算：`input_capacity = context_window − max_output_tokens`
  （`packages/aa_agent/src/compaction.rs:144`）；默认阈值 90%，
  输入容量 < 80k 不自动压（`CompactionConfig::default`，`:132–:140`）。
- 触发：`AgentLoop::run` 开头调用 `maybe_compact`（`:264`），且最后一条消息
  必须是待回复的用户文本；工具循环中间不压。
- 保留：`RETAINED_USER_BYTES = 80_000`（`session.rs:32`），
  `retained_user_texts`（`:483`）只挑纯用户文本（排除工具结果和旧摘要），
  按 UTF-8 字符边界截断（`:499–:505`）。
- 摘要请求：当前生效历史去掉待回复 user + `COMPACTION_PROMPT`（`:35–:66`），
  不带工具定义，用当前会话模型。
- prompt：**没有**「合并已有 `<compacted-summary>`」一类的规则。
- 落盘：`SessionEvent::Compaction { summary, retained_users, first_kept_message,
  tokens_before }`（`session.rs:57–:68`），会话格式 `VERSION = 2`（`:30`）。
  原始事件保留；重放时生效历史替换为 system → 保留用户原话 → 摘要 → 保留段。
- 溢出：没有 provider 溢出识别与压缩重试。
- 取消：`maybe_compact` 接收 `CancelToken`，摘要流内检查取消；取消返回
  `CompactionError::Cancelled` 且**不**写压缩事件（`:264` 起）。

## 8. 代码分布与术语

会话 / thread 的命名差异、agent loop 的作用域、各家的 package / 模块分布
统一写在 [`agent-loop.md`](agent-loop.md) 第 10 节，这里不重复。

## 9. 与其他文档的分工

| 文档 | 回答的问题 |
|---|---|
| [`agent-loop.md`](agent-loop.md) | 压缩放在哪一层、每轮还是每步；会话 / thread 命名、agent loop 作用域与代码分布（第 10 节） |
| [`agent-loop-output.md`](agent-loop-output.md) | 压缩产物的形状（摘要是否结构化） |
| [`../decisions/0015-compaction.md`](../decisions/0015-compaction.md) | aa-ai 为什么选 Zed 触发 + pi 持久化，会话 v2 的字段来源 |
| [`../decisions/0018-agent-loop-cancellation.md`](../decisions/0018-agent-loop-cancellation.md) | 压缩请求的取消语义 |
| 本文 | 触发公式、保留策略、切点、摘要请求、prompt、落盘、溢出、取消的事实对照 |
