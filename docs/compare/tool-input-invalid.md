# 工具输入不合法时各家怎么处理

> 模型产出的 tool call 参数不合法（JSON 坏、字段缺失、类型不符）时：
> 在哪里发现？会不会执行？错误交给谁？能否修复或终止 run？
>
> 源码版本：
> Zed `4ed72e0135` / rig `6e13e8c` / TanStack `6059872f` /
> opencode `d7ae4f2e3c` / pi `406e719a6` / dsh `e8f7b93a34` / aa-ai 本仓 HEAD。

## 0. 结论

三派，控制力从强到弱：

1. **流组装阶段拦截**（rig / opencode 的 JSON 层）：调用还没变成可执行调用就被判无效，
   可以决定修复、丢弃或终止。rig 提供最完整的策略面（Retry / Repair / Skip / Stop / Fail）。
2. **执行前校验**（TanStack / pi / dsh / opencode 的 schema 层）：调用合法但参数不符时，
   在执行入口前拒绝，错误变成 `is_error` 工具结果，循环继续。
3. **交给工具自己反序列化**（Zed / aa-ai）：循环不做 schema 校验，JSON 解析失败后
   返回错误结果；aa-ai 在 registry 层先解析，坏 JSON 不进入工具。

opencode 比较特殊：**两层都有**——流层区分 malformed / incomplete / length，
schema 层用 Effect Schema 在执行入口校验，并且给出的错误文本最“可执行”
（明确告诉模型“重新发一次”）。

## 1. 先定义「可控」

| 维度 | 含义 |
|---|---|
| 发现时机 | 流组装 / 执行前 / 工具内部 |
| 副作用安全 | 参数错时是否保证工具一行都没执行 |
| 走向控制 | 能否选择修复、丢弃、继续或终止 run |
| 修复能力 | 是否有内建修复钩子，还是只能等模型重试 |
| 诊断质量 | 错误里是否包含违规列表、截断原因、可执行建议 |

同一个实现在这五个维度上的排名并不一致，所以不能只说“谁最可控”。

## 2. 逐家事实

### 2.1 rig：完整体现在「无效调用的决策面」

- 流组装阶段就把参数无法解析的调用标成 `InvalidToolCall`，**该调用永远不会执行**
  （[engine.rs#L456-L469](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/agent/engine.rs#L456-L469)）。
- 原因分三类：`UnknownTool`、`DisallowedByToolChoice`、`MalformedArguments`
  （[policy.rs#L24-L35](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/policy.rs#L24-L35)）。
- `on_invalid_tool_call` 可返回五种动作
  （[policy.rs#L320-L343](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/policy.rs#L320-L343)）：

  | 动作 | 效果 |
  |---|---|
  | `Retry { feedback }` | 带着纠正反馈重试模型，消耗 invalid-call 重试预算 |
  | `Repair { tool_name }` | 改写工具名后重放（`MalformedArguments` 时被拒绝） |
  | `Skip { reason }` | 跳过该调用，合成一条反馈继续 |
  | `Stop { reason }` | 结束 run（带原因） |
  | `Fail` | 整个 run 失败（fail-fast） |

- hook 还能看到：原始参数文本、可用工具、当前 `tool_choice`、完整 chat history、
  是否来自流式路径、被拒绝的输出（[policy.rs#L69-L91](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/policy.rs#L69-L91)）。
- 没有 hook 时按 run 策略 `UnhandledInvalidToolCall::{Fail, Ignore}`（默认 `Fail`）
  处理，并受 `max_invalid_tool_call_retries`、
  `max_consecutive_malformed_tool_calls` 两个预算约束
  （[spec.rs#L86-L92](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/spec.rs#L86-L92)、
  [#L109-L116](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/spec.rs#L109-L116)）。
  前者限制 hook 的 `Retry` 次数；后者统计**连续的工具 step**——只要某个 step 里有一个
  调用参数不是 JSON 对象就 +1，某个 step 全部解析成功就清零，超过上限直接以
  `ProviderError::Response` 失败并报告工具名与解析错误
  （[run/mod.rs#L1193-L1224](file:///home/aa/repos/lib_ls/learn_ls/rig/crates/rig-agent/src/run/mod.rs#L1193-L1224)）。
  它不要求是“同一个无效输入”，也不覆盖 schema 类型不符。
- 边界：这一套主要覆盖「无法解析成合法调用」；**合法 JSON 但字段类型不符**
  仍由具体 `Tool` 的输入反序列化报错，走普通工具错误路径。

所以 rig 多出来的概念不止换来“能终止”，而是**一整个无效调用策略面**：
重试/改名/跳过/停止/失败五种走向 + 三类原因 + 两份预算 + 决策上下文。
默认配置下它是 fail-fast，这些能力需要显式启用。

### 2.2 opencode：流层分类 + 执行入口 schema 校验

- **JSON 层**：无法解析的输入不会执行，而是回放一条 `input: {}` 的合成调用，
  错误文本明确要求重发：
  `"The tool input could not be parsed as JSON, so the call was not executed.
  Reissue the call with valid JSON arguments."`
  （[loop.ts#L21-L37](file:///home/aa/repos/ai_ls/learn_ls/opencode/packages/ai/src/loop.ts#L21-L37)、
  [publish-llm-event.ts#L47-L62](file:///home/aa/repos/ai_ls/learn_ls/opencode/packages/core/src/session/runner/publish-llm-event.ts#L47-L62)）。
  它还区分 `malformed` / `length` / `incomplete`：因输出上限被截断的调用会提示
  “拆小 payload 重发”，而不是笼统报错。
- **Schema 层**：`runtime.execute` 先 `decodeInput`，Effect Schema / Standard Schema
  校验失败抛 `Tool.Error`，附带格式化后的违规信息
  （[runtime.ts#L47-L85](file:///home/aa/repos/ai_ls/learn_ls/opencode/packages/core/src/tool/runtime.ts#L47-L85)）。
- `Tool.Error` 在 step 层被捕获并 `failTool`，作为工具结果回给模型
  （[step.ts#L150-L156](file:///home/aa/repos/ai_ls/learn_ls/opencode/packages/core/src/session/runner/step.ts#L150-L156)）。
- 插件 `execute.before` 可以在执行前拒绝（`Tool.Error`），但没有参数修复钩子；
  无效参数不会终止 run，靠模型下一轮重试。
- 参数不合法时该调用不会进入执行，因此副作用安全。

### 2.3 TanStack：校验最靠后，修复钩子最完整

- `validateToolInput(schema, args)` 是审批编辑、middleware 之后的最后一道检查；
  失败构造 `{ error: message }`、`state: 'output-error'` 的结果
  （[tool-calls.ts#L1255-L1300](file:///home/aa/repos/lib_ls/learn_ls/ai/packages/ai/src/activities/chat/tools/tool-calls.ts#L1255-L1300)）。
- `repairToolInput(failed, error)` 可以在校验前尝试修复；
  如果参数被审批编辑或 middleware 改过，错误里会明确告诉模型“改过之后才失败”，
  避免模型重复发同样的合法调用。
- 不会执行非法调用；不会终止 run；模型可以通过工具结果自愈。

### 2.4 dsh：确定性最强

- `defineTool` 在 `execute` 入口校验，违规抛 `ToolArgsError`（`code: INVALID_ARGS` + 违规列表）
  （[schema.ts#L598-L601](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/tools/src/schema.ts#L598-L601)、
  [#L460-L470](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/tools/src/schema.ts#L460-L470)）。
- pipeline 统一归一成 `isError` 结果（[pipeline.ts#L210-L225](file:///home/aa/repos/ai_ls/learn_ls/deepseek-harness/packages/core/tools/src/pipeline.ts#L210-L225)）。
- 非法 JSON 不丢弃，原样交给工具自己的诊断；
  `isConcurrencySafe` 校验失败回退 `false`，坏参数不会进入并行池。
- 没有 repair、没有终止；执行前拒绝保证副作用安全。

### 2.5 pi：执行前校验，最小实现

- `validateToolArguments` 抛错即返回 `isError: true` 的 immediate 结果
  （[agent-loop.ts#L728-L790](file:///home/aa/repos/ai_ls/learn_ls/pi/packages/agent/src/agent-loop.ts#L728-L790)）。
- 未知工具返回 `Tool xxx not found`。
- 没有 repair；不会终止 run；保证非法调用不执行。

### 2.6 Zed：流中只发可解析的快照，结束才判无效

- `input_schema()` 只发给模型；循环不做 JSON Schema 校验。
- 非法 JSON：`ToolInput::invalid_json(error_message)`，工具 `recv()` 返回 `Err`
  （[thread.rs#L4205-L4225](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L4205-L4225)、
  [#L5577-L5585](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L5577-L5585)）。
- **流中不判错**：每个参数增量先经 `fix_streamed_json` 修补（补未闭合括号），修补后能解析
  才发 `ToolUse { is_input_complete: false }`；失败就继续等。真实判定在流结束时做一次，
  失败才发 `ToolUseJsonParseError`（[openai completion.rs#L920-L961](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/open_ai/src/completion.rs#L920-L961)）。
  因此快照的**值可能被截断**，只供 UI 预览；不完整的事件永不执行
  （[thread.rs#L4001-L4007](file:///home/aa/repos/ide_ls/learn_ls/zed/crates/agent/src/thread.rs#L4001-L4007)）。
- 错误变成 tool result 文本，循环继续；没有 repair / 终止。
- 拦截点在 `T::run` 的第一行：工具必须 `input.recv().await?` 才拿得到 typed input，
  坏 JSON 在读入时立即返回 Err（`write_file` 的 `process_streaming_writes` 收到
  `InvalidJson` 就直接 `Failed`），所以同样在产生副作用之前失败。

### 2.7 aa-ai：零校验，错误即结果（原文不丢）

- provider adapter 自己解析 `arguments` 文本（wire 形状属于 adapter，core 不提供共享解析函数）：
  能解析就是 `LanguageModelToolUseInput::Json`，解析失败保留原文
  `LanguageModelToolUseInput::Text`（
  [message.rs#L172](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_language_model/src/request/message.rs#L172)）。
  整个响应不会因为一条坏参数失败。
- 循环把 `LanguageModelToolUseInput` 原样写进会话（`session::ToolCall.input` 直接用同一类型，
  serde 形状即日志形状），再交给
  `AgentToolRegistry::execute`（[agent_loop.rs#L272-L304](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L272-L304)）。
- registry 对 `Text` 返回 `Error parsing input JSON: <serde 错误>` 的错误结果（与 Zed 同格式，
  serde 会给出行列；原文已经在 assistant 的 tool_call 里，不重复）；
  对 `Json` 做 `serde_json::from_value`，类型不符返回 `input parse error`（
  [agent_registry.rs#L43-L73](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/tool/agent_registry.rs#L43-L73)）。
- 空参数串按 Zed 的约定当作 `{}`（无参数工具），不会误报解析失败。
- 两种错误都进入上下文，下一轮模型能看到错误并自纠；没有 repair、重试、fail-fast。
- 参数生成过程中，provider 用 `fix_streamed_json` 修补每个参数增量（与 Zed 同一算法，
  见 `aa_language_model::provider::fix_streamed_json`：补 `}`/`]`/`"`、丢掉悬空 `\`），
  修补后能解析才发 `ToolUse { is_complete: false, input: Json(<已修补值>) }`，
  解析不出来就继续等（
  [openai response#L467](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_language_models/aa_ai_openai/src/chat_completion/response/mod.rs#L467)）。
- agent loop 收到快照后做两件事：
  - 转发给 `ToolEvents::tool_input(name, raw)`（UI / 日志可用）；
  - 若工具声明了 `supports_input_streaming`，**第一份快照就提前启动**
    `AgentToolRegistry::start`，用 `ToolInput` 持续投喂快照；完整调用到达时投喂
    `Full`，参数最终不可解析时投喂 `InvalidJson`
    （[agent_loop.rs#L231-L290](file:///home/aa/repos/lib_ls/aa-ai/packages/aa_agent/src/agent_loop.rs#L231-L290)）。
  未声明流式的工具不入队、不执行，只在轮末按顺序跑一次。
- 因此**流式工具可能在参数完整之前产生副作用**，边界由工具自己保证（快照只用于
  无副作用 / 可回滚的准备，真正执行等 `Full`）。非流式工具仍保证“坏输入不执行”。
- 无钩子时，非流式路径与 Zed **行为等价**：assistant 轮保留 → 写 `is_error` tool result
  → 下一轮请求。差别只在栈上的拦截点（registry 比 `T::run` 早一帧）：坏 JSON 不会
  进入工具，工具拿到的 `ToolInput` 已经是完整入参。
- 错误文案也是同一格式：两边都由框架写 `Error parsing input JSON: ...`，
  工具只是把框架给的文本转成 Err/结果。

## 3. 对照表

| | 发现时机 | 非法调用会执行吗 | 错误去向 | 修复/重试 | 可终止 run | 副作用安全 |
|---|---|---|---|---|---|---|
| rig | 流组装 | 不会 | hook 决定 | Retry / Repair / Skip / Stop / Fail + 预算 | **是**（`Stop` / `Fail`） | 保证 |
| opencode | 流解析 + 执行入口 | 不会 | tool result（含 `reissue` 建议、违规列表） | 无，模型重试 | 否 | 保证 |
| TanStack | 执行前（最后一道） | 不会 | `is_error` tool result | `repairToolInput` + 模型重试 | 否 | 保证 |
| dsh | `execute` 入口 | 不会 | `INVALID_ARGS` → `isError` | 无 | 否 | 保证 |
| pi | 执行前 | 不会 | `isError` tool result | 无 | 否 | 保证 |
| Zed | 工具 `recv()` | 已进入 `T::run`，首行读输入即失败 | tool result 文本 | 无 | 否 | 保证（副作用前失败） |
| aa-ai | provider 解析 + registry 反序列化 | 非流式不会（registry 拦截）；流式工具已提前启动 | `is_error` tool result（`Error parsing input JSON: <serde 错误>`） | 无 | 否 | 非流式保证；流式由工具声明保证 |

## 4. 排序

| 维度 | 排序 |
|---|---|
| 行为控制力 | rig > opencode ≈ TanStack > dsh > pi > Zed > aa-ai |
| 修复成功率 | TanStack ≈ rig（需自写 hook）> opencode（错误最可执行）> dsh > pi > Zed/aa-ai |
| 诊断质量 | opencode > dsh ≈ TanStack > rig > pi > Zed > aa-ai |
| 副作用安全 | rig/opencode/TanStack/dsh/pi（执行前拒绝）≈ aa-ai 非流式（registry 拦截）；aa-ai 流式 / Zed 靠工具保证 |
| 接入成本（低→高） | aa-ai < Zed < pi ≈ dsh < opencode < TanStack < rig |

所以“谁最可控”取决于你要控制什么：

- 要**终止或改写流程**：rig；
- 要**最高自愈率**：TanStack；
- 要**错误信息最能让模型一次改对**：opencode；
- 要**确定性且零魔法**：dsh；
- 要**最小内核、调用方完全掌握策略**：aa-ai / Zed。

## 5. aa-ai 的优势不只是“接入成本低”

对这一个维度，aa-ai 的取舍是：

- **没有隐藏策略**：不修复、不丢弃、不 fail-fast，所有失败都变成工具结果，
  调用方看到的和模型看到的是同一份事实；
- **策略完全在调用方**：需要校验就在 `ToolRegistry` 加，
  需要修复就在工具里做，不需要和库的默认行为对抗；
- **不引入 schema 依赖**：`aa_language_model` 不需要 JSON Schema 引擎，
  `input_schema()` 只是给模型看的 `serde_json::Value`；
- **会话一致性简单**：错误也是 tool result，天然保持 call/result 配对。

代价同样明确：

- 非流式工具保证坏输入不执行；声明了 `supports_input_streaming` 的工具会被**提前启动**，
  副作用边界由工具自己保证（快照只做可回滚准备）；
- 诊断只有 serde 解析错误（带行列），没有违规列表或可执行建议；原文在上一轮 tool_call 里；
- 无法终止 run，也不能自动修复。

所以不是“只有接入成本低”，而是**把可控性从库转移给了调用方**。
如果要在 aa-ai 里补，剩下的是两件互不冲突的小事：
在 `ToolRegistry::execute` 前用 `input_schema()` 做 JSON Schema 校验（把“约定”
升级成“保证”），以及给连续无效调用加预算（rig 的
`max_consecutive_malformed_tool_calls` 那一档）。两者都不改变 `AgentLoop`。

## 6. 未决

1. aa-ai 是否引入 JSON Schema 校验？若引入，选哪个 crate，是否只对声明了
   `input_schema` 的工具生效？
2. 是否需要 opencode 式的 `malformed / length / incomplete` 分类？
   其中 `length` 需要 provider 暴露 finish reason。
3. 是否允许调用方注入 repair 钩子？如果允许，位置在 registry 还是 loop？
4. 流式工具提前开工后，副作用边界要不要在 registry 加约束（例如快照阶段禁止写操作），
   还是维持“由声明流式的工具自己保证”？
