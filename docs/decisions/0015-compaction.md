# 0015. 上下文预算与压缩：Zed 的触发/保留 + pi 的日志化

> 状态：accepted
> 日期：2026-11-25
> 关联：[0004](0004-message-layout.md) D7（压缩是容器级概念）、
> [0005](0005-content-part-variants.md) D2（原生压缩不进 `ContentPart`）、
> [0013](0013-session-log.md)（会话 JSONL）、
> [`packages/aa_agent/src/compaction.rs`](../../packages/aa_agent/src/compaction.rs)、
> [`packages/aa_agent/src/session.rs`](../../packages/aa_agent/src/session.rs)

## 背景与问题

会话可以无限长，但模型的上下文窗口有限。没有压缩时只有两种结局：请求被
provider 拒绝，或者调用方自己粗暴截断（丢掉工具调用/结果的配对关系）。

需求约束：

- 库层不读环境变量/配置：阈值、预算由 `aacode` 传进来；
- 会话日志是审计记录，不能因为压缩就丢掉原始事实；
- 压缩后 `--continue` 不能每次重新付一次摘要请求；
- 工具调用与工具结果不能被切点拆开。

## 调研（2026-11-25）

| | Zed | opencode | pi |
|---|---|---|---|
| 触发 | 输入容量的 90%（可配）；容量 < 80k 不自动压 | 窗口 − max(16k, 15%)；溢出再收缩重试 | 窗口 − 16k |
| 保留原文 | 最近 80KB **用户文本** | 最近 15k token 整段 | 最近 20k token 整段，不切 tool result |
| 摘要请求 | 完整请求 + prompt（保留工具定义以命中 cache） | 完整对话 + prompt；超长时四级裁剪 | 序列化成文本；工具结果截 2k 字符 |
| 持久化 | 插入 `Message::Compaction` | 内存 checkpoint，后续更新 | 日志追加 `CompactionEntry` |
| 摘要模型 | 可单独配 | 可单独配 | 默认同模型 |

源码位置：

- Zed `crates/agent/src/thread.rs:126`（`MIN_COMPACTION_CONTEXT_WINDOW = 80_000`）、
  `:129`（`COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET = 80_000`）、
  `:2696`（`input_token_capacity`）、`:4855`（触发点）、
  `:4918`（`build_compaction_request`）、`:5436`（保留用户消息）；
  摘要提示词在 `crates/agent_settings/src/prompts/compaction_prompt.txt`。
- opencode `packages/core/src/session/compaction.ts`（dev 分支）：
  `RESERVE_MIN = 16_000`、`RESERVE_SHARE = 0.15`、默认 `keep: 15_000`、
  `SHRINK_STEPS`、`PRECOMPUTE_LEAD`；摘要模板就是本会话 checkpoint 的字段。
- pi `packages/coding-agent/src/core/compaction/compaction.ts` 与
  `docs/compaction.md`：`reserveTokens = 16384`、`keepRecentTokens = 20000`、
  切点规则、`CompactionEntry { summary, firstKeptEntryId, tokensBefore }`。
- `deepseek-ai/dsh-superagent-prototype` 没有压缩实现（只有上下文装配），
  公开仓库无法作为参考。

结论：三家都用「LLM 摘要旧对话 + 保留最近一段原文」，差别在保留什么、
摘要请求怎么发、压缩事实怎么落盘。

## 当时的想法（选项）

1. **纯本地折叠**（把旧轮拼成结构化文本，不发额外请求）
   - 确定性、可离线测；但摘要质量差，长任务会丢关键决策。
2. **只压请求视图**（日志永远保留全文，每次 `--continue` 重新摘要）
   - 实现最小；代价是每次续聊多一次摘要请求，且摘要不落盘。
3. **Zed 式 + pi 式持久化（采纳）**
   - 触发/保留照 Zed；压缩作为日志事件，重放时直接得到压缩后的历史。

## 决策

### D1：预算与触发照 Zed

- `input_capacity = context_window - max_output_tokens`；
- 默认阈值 90%，支持三种写法（对齐 Zed）：`90%` / `100000` / `-20000`；
- 输入容量 < `min_context_window`（默认 80_000）不做自动压缩。Zed 把这个
  80k 写死；我们把它放进 `CompactionConfig`，默认值保持一致，`aacode` 可以调。

### D2：保留最近 80KB 用户文本

- 摘要覆盖「待回复用户消息之前」的全部历史；
- 其中最近 80KB 的**纯用户文本**（不含工具结果）原样保留，放在摘要之前；
- 摘要以一条 user 消息发出：
  `The previous conversation was compacted. Use this summary as context:\n\n{summary}`
  （Zed 的措辞），provider 不需要新增消息类型。

### D3：摘要请求用当前生效历史，不带工具

- 请求 = 当前生效历史去掉待回复的那条用户消息 + `COMPACTION_PROMPT`；
- 不传工具定义：我们的 provider 还没有接 prompt cache，保留工具定义只会
  增加 token；opencode/pi 的摘要请求同样不带工具；
- 用当前会话模型（`aacode` 以后可以换成更便宜的模型）。

### D4：压缩落成会话事件，版本升到 2

```rust
SessionEvent::Compaction(CompactionRecord {
    summary: String,
    retained_users: Vec<String>,
    first_kept_message: usize,
    tokens_before: u64,
})
```

- 日志仍是追加式、原始事件一条不删（审计/导出）；
- 内存里的 `Session::messages` 是**生效历史**：重放时遇到 compaction 事件就
  替换成「system → retained_users → 摘要 → 保留段」，被覆盖的 `Arc<Message>`
  直接 drop（0004 D7 的目标）；
- `retained_users` 存文本而不是下标：重放不依赖「保留规则」的实现细节；
- `first_kept_message` 是压缩时生效历史里的下标（含 system）；日志只追加，
  所以重放时下标稳定；
- 旧 v1 会话明确拒绝（`UnsupportedVersion`），不做猜测式兼容。

### D5：只在「最后一条是待回复的用户文本」时压缩

- `AgentLoop::run` 开头、`aacode chat` 每轮发送前各检查一次；
- 工具循环中间不压缩：那会把 assistant 的 `tool_call` 和它的 `tool_result`
  拆开，下一轮模型会看到孤儿结果；
- 没有 `context_window` 的模型（例如 `fake`）不压缩。

### D6：估算不引入 tokenizer

- ASCII 4 字符 / token，非 ASCII 1 字符 / token；图片 1500 token；
- 工具定义的 JSON schema 也计入；
- pi/opencode 用纯 `chars / 4`，对中文低估约 4 倍；这里宁可早压缩。

## 后果

**正面：**

- 压缩后的历史真正释放内存，不是「留着全文只改发送内容」；
- `--continue` 直接重放摘要，不重复付摘要请求；
- 原始日志完整，审计和后续换算法都不受影响；
- 触发/保留行为与 Zed 一致，出问题可以直接对照上游。

**代价：**

- 会话格式升到 v2，v1 文件读不了；
- 单条工具结果本身就超过窗口时，摘要请求仍可能被 provider 拒绝
  （Zed 也不处理；opencode 的四级裁剪、pi 的 2k 截断留待后续）；
- 没有后台预计算摘要、没有溢出后的「压缩重试」；这两项等实际需要再加；
- 摘要质量依赖模型；空摘要按错误处理，不静默接受。

**触发重新评估的条件：**

- 需要在摘要请求里复用 prompt cache（届时把工具定义/思考配置带回去）；
- 小窗口模型（< 80k）成为主要使用场景（调低 `min_context_window` 或按窗口比例缩放）；
- 需要「压缩后重试」来兜住 provider 的 context-overflow 错误。
