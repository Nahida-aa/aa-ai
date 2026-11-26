# agent loop 的输出结构对比：终止返回值、终态约束、压缩产物

> 回答三个不同的问题：
> 1. loop **返回什么**（调用方的通信契约）；
> 2. loop 是否**约束最后一轮的结构**（模型行为契约）；
> 3. **压缩产物**是什么形状。
>
> 这三件事经常被混在一起叫「结构化输出」，但实现位置和保证强度完全不同。
>
> 源码版本同 [`agent-loop.md`](agent-loop.md)。

## 0. 结论

- **Zed 几乎没有做结构化终态**：turn 的输出是 `ThreadEvent` 流 + assistant 文本，
  没有 `response_format`，也没有 output tool。结构只存在于「工具参数」里。
- 真正的区别不在「text 还是 JSON」，而在**结构在哪一层强制**：
  wire（`response_format`）、loop（output tool / finalization 请求）、SDK 外层（schema 校验）。
- **压缩产物六家都是文本摘要**。dsh 额外带 shadow range 等元数据，
  aa-ai 带保留策略元数据，但摘要正文都没有 schema 约束。
- 覆盖最完整的是 rig（4 种 `OutputMode` + 校验重试）；TanStack 把结构化放在
  middleware/adapter，orchestration 层再校验一次；aa-ai 目前只有 wire 层能力，
  loop 没有消费。
- **工具参数不合法**分两个流派：执行前校验（TanStack / pi / dsh，错误变成 tool result）
  和交给 provider/工具解析（Zed / aa-ai）；只有 rig 把它提升为可终止 run 的事件，
  也可由 hook 修复重放。

## 1. 三个层面

| 层面 | 问题 | 典型实现 |
|---|---|---|
| A. 终止返回值 | 调用方拿到 String、消息、事件流，还是泛型 `T` | `PromptResponse.content`、`run() -> String` |
| B. 终态结构约束 | 模型最后一轮是否必须符合 schema | output tool、原生 `response_format`、prompted JSON |
| C. 中间产物结构 | assistant / tool / compaction 是否结构化 | content parts、compaction record |

## 2. 逐家事实

### 2.1 Zed

- **A**：`Thread::send` 启动 `run_turn`，回答以 `ThreadEvent` 流式产出
  （[thread.rs#L2793-L2811](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L2793-L2811)）。
  没有 typed final；调用方从事件里累积文本。
- **B**：没有。`LanguageModelRequest` 无 `response_format`，agent 也没有 output tool；
  需要结构时只能让模型调用某个普通工具，结构体现在工具参数上。
- **C（压缩）**：`summary()` 返回 `Task<Option<SharedString>>`
  （[thread.rs#L4295-L4296](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L4295-L4296)），
  摘要作为一条普通消息插入（[#L7849-L7850](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L7849-L7850)），
  并通过 ACP 的 `send_context_compaction_update(summary)` 上报
  （[#L6010](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L6010)）。纯文本，无 schema。

### 2.2 rig

- **A**：`PromptResponse { content, output }`，`output` 由 `content` 派生
  （[run/response.rs#L81-L100](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/response.rs#L81-L100)）。
- **B**：最完整。`OutputMode::{Auto, Tool, Native, Prompted}`
  （[run/output.rs#L13-L34](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/output.rs#L13-L34)）：
  - `Native`：每轮发原生结构化约束；
  - `Tool`：把 schema 合成成 final-answer 工具；
  - `Prompted`：schema 放 system prompt，直接返回文本；
  - `Auto`：能组合工具就原生，否则合成工具。
  **无论哪种模式，run 只在输出通过 `validate` 校验后才结束**，不合法会重试，
  超出 retry budget 才失败（[extractor.rs#L1-L60](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/extractor.rs#L1-L60)）。
- **C**：引擎内没有压缩。

### 2.3 TanStack AI

- **A**：`chat()` 是事件流；带 `outputSchema` 时最终产出经过校验的结构化结果
  （[index.ts#L1425-L1445](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/index.ts#L1425-L1445)）。
- **B**：走 `structuredOutput` 中间件：优先 adapter 的原生能力
  （combined tools+schema、`response_format`、`text.format`、`output_format`），
  不支持时再发一次 finalization 请求（[adapter.ts#L160-L176](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/adapter.ts#L160-L176)）。
  `ai-orchestration` 的 `defineAgent({ output })` 在 workflow 层再校验一次
  （[agent.ts#L21-L43](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai-orchestration/src/agent.ts#L21-L43)）。
- **C（压缩）**：独立 `ai-compaction`。`Summarizer` 返回 `string`（可选带 usage），
  包成 `<untrusted-conversation-summary>` 消息
  （[summarizer.ts#L1-L50](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai-compaction/src/summarizer.ts#L1-L50)）；
  记录本身是 head messages + token 统计，不是结构化摘要字段
  （[record.ts](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai-compaction/src/record.ts)）。

### 2.4 pi

- **A**：`prompt()` 不返回结构化结果，结果从 messages / 事件里读
  （[agent.ts#L379-L419](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent.ts#L379-L419)）。
- **B**：低层 loop 没有结构化终态；schema 只出现在工具参数上。
- **C（压缩）**：coding-agent 的 compaction entry 用 `summary: string` 存摘要，
  prompt 要求的是「结构化 checkpoint 文本」，但没有 schema 校验
  （[compaction.ts](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/coding-agent/src/core/compaction/compaction.ts)）。

### 2.5 dsh

- **A**：`turn/end` + `assistant/message` 事件，没有 typed final。
- **B**：loop 不约束终态结构；工具 schema 在 tools 层。
- **C（压缩）**：`compaction/summary` 事件，data 包含
  `summary`（text part）、`shadowedRange`、`shadowedSeqs`、`shadowedTokenCount`、
  `provider`、`model`
  （[event-script.client.ts#L142-L150](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/api/session-controller/tests/event-script.client.ts#L142-L150)）。
  摘要正文是文本，元数据是结构化的。

### 2.6 aa-ai

- **A**：`AgentLoop::run` 返回 `String`（[agent_loop.rs#L121-L127](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L121-L127)）。
- **B**：没有。`Request.response_format` 在 wire 层已经可用，但 agent loop
  从不设置它，也没有 output tool / 校验重试。
- **C（压缩）**：`CompactionRecord { summary, retained_users, first_kept_message, tokens_before }`
  （[session.rs#L63-L69](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/session.rs#L63-L69)）。
  `summary` 是文本，其余是保留策略元数据。

## 3. 对照表

| | A. 终止返回 | B. 终态约束 | 实现位置 | C. 压缩产物 |
|---|---|---|---|---|
| Zed | `ThreadEvent` / 文本 | 无 | — | 纯文本摘要 |
| rig | `PromptResponse` | 有（4 种模式） | run 层 `OutputMode` + validate | 无 |
| TanStack | 事件流 + `outputSchema` | 有 | middleware + adapter | 文本摘要 + head 重写 |
| pi | 消息 / 事件 | 无 | — | 文本摘要 |
| dsh | 事件 + `turn/end` | 无 | — | 文本摘要 + shadow 元数据 |
| aa-ai | `String` | 无（wire 有） | — | 文本摘要 + 保留元数据 |

## 4. 这算不算「agent-loop-output」

算，但要拆开看：

1. **返回值契约**：大多数 loop 返回文本/事件，只有带 output schema 的 SDK 才返回泛型 `T`；
2. **终态约束**：只有 rig 和 TanStack 在 loop 层强制，Zed/pi/dsh 都没有；
3. **中间产物**：所有实现都有结构化（content parts、tool calls、session events），
   但这是「消息结构」，不是「输出结构」。

所以更准确的说法是：**Zed 在 1 和 2 上几乎没做，在 3 上做了**（content parts 和工具参数）。
把「agent 结束时返回 text 还是指定结构」和「压缩摘要是否结构化」分开对比，
才不会把 wire 能力、loop 校验、SDK 外层校验混为一谈。

## 5. 如果要给 aa-ai 补

| 方案 | 做法 | 代价 |
|---|---|---|
| A. 泛型 run | `run<T: DeserializeOwned>(schema)`，loop 内校验 + 重试 | 改 `AgentLoop` 签名，和 `Session` 的事件模型耦合需设计 |
| B. output tool | agent 层合成 final-answer 工具，跨 provider | 依赖 `supports_tools`，多一次工具轮 |
| C. 维持现状 | 只暴露 `response_format`，调用方自己解析 | 无 loop 保证；provider 不支持时报 `Unsupported` |

压缩摘要不建议加 schema：六家都是文本，真正需要结构化的是
`retained_users` / `shadowedRange` 这类元数据，我们和 dsh 已经有对应字段。

## 6. 工具输入不符合 schema 时怎么处理

这一节已独立成文：[`tool-input-invalid.md`](tool-input-invalid.md)，覆盖
Zed / rig / TanStack / opencode / pi / dsh / aa-ai 七家，含
「行为控制力 / 修复能力 / 诊断质量 / 副作用安全 / 接入成本」的排序。

一句话结论：只有 rig 提供完整的无效调用策略面（Retry / Repair / Skip / Stop / Fail）；
TanStack 的自愈钩子最完整；opencode 的流层分类（malformed / length / incomplete）
和错误文本最可执行；aa-ai 把策略完全交给调用方。

## 7. 未决

1. `run` 是否泛型化，还是保持 `String` + 调用方解析？
2. 结构化失败的重试次数和预算归属（loop 还是调用方）？
3. 压缩摘要是否需要固定的分节结构（决定/待办/文件），还是继续自由文本？
