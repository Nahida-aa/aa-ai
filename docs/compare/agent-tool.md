# agent tool 的定义方式对比：定义、执行、schema、副作用声明

> 只记事实：类型、字段、trait 方法、行号 + commit。不做取舍、不给建议。
> 源码位置用项目内相对路径；克隆根目录见 [`AGENTS.local.md`](../AGENTS.local.md)。
>
> 本文回答「工具怎么定义」。`docs/compare/tool-input-invalid.md` 回答
> 「参数不合法时怎么处理」，`docs/design/tool-trait.md` 是我们自己的分层规则。

## 源码版本

| 项目 | commit |
|---|---|
| Zed | `2b248e6072` |
| rig | `6e13e8c` |
| TanStack AI | `6059872fe` |
| pi | `406e719a6` |
| dsh（deepseek-harness） | `e8f7b93a34` |
| opencode | `d7ae4f2e3c` |
| aa-ai | 当前工作区（`packages/aa_ai_harness/src/tool.rs`） |

## 0. 事实概览

- 七家都把工具拆成**数据半**（喂给模型的 name / description / 参数 schema）和
  **执行半**（真正跑的函数/trait）；只有拆分位置不同。
- 参数 schema 的生成来源：Rust 系用 `schemars`（Zed / aa-ai）或手写 `serde_json::Value`
  （rig）；TS 系用 Standard JSON Schema / Zod（TanStack）、TypeBox（pi）、
  `Record<string, unknown>`（dsh）、effect `Schema`（opencode）。
- `description` 的来源分两种：Zed 从输入 schema 的顶层 `description` 字段派生；
  其余都显式声明一个描述字段/方法。
- **输出声明**差异最大：dsh 强制声明 `output { schema, render }`；pi / TanStack 有可选
  `outputSchema`；rig 用 `Output: IntoToolOutput`；Zed / aa-ai 用 `Into<内容类型>`。
- **副作用元信息**：rig 有 `ToolAnnotations`（read_only / destructive / idempotent /
  open_world）；TanStack 有 `needsApproval` / `approvalSchema`；pi 有 `replay` /
  `executionMode` / `terminate`；dsh 有 `presentationMeta`；Zed / aa-ai 没有同类声明。

## 1. 总表

| 维度 | Zed | rig | TanStack AI | pi | dsh | opencode | aa-ai（新实现） |
|---|---|---|---|---|---|---|---|
| 数据半类型 | `LanguageModelRequestTool`（`crates/language_model_core/src/request.rs:419`） | `ToolDefinition`（`crates/rig-core/src/completion/request.rs:70`） | `Tool`（`packages/ai/src/types.ts:1005`）+ `toolDefinition()` | `AgentTool extends Tool`（`packages/agent/src/types.ts:478`） | `ToolSchema`（`packages/llm/llm/src/types.ts:517`）+ `ToolDefinition` | `Tool.Info` + `ToolDefinition`（`packages/core/src/tool.ts`） | `aa_language_model::request::tools::Tool` |
| 执行半 | `AgentTool` trait（`crates/agent/src/thread.rs:5661`） | `Tool` / `ToolDyn` trait（`crates/rig-core/src/tool/contextual.rs:32`） | `execute` 函数（`Tool` 的字段） | `AgentTool.execute(toolCallId, params, signal, onUpdate)` | `ToolDefinition.execute(args, exec)`（`contracts.ts:27`） | effect-based 执行（`packages/core/src/tool.ts`） | `AgentTool::run(Arc<Self>, input, events)` |
| name | `const NAME` | `const NAME` | `name: string` | 继承 `Tool.name` | `ToolSchema.name` | `Tool.Info.name` | `const NAME` |
| description | `description()` 从 schema 顶层取（`thread.rs:5670`） | `fn description(&self) -> String` | `description: string` | 继承 `Tool.description` | `ToolSchema.description` | `Tool.Info.description` | `const DESCRIPTION`（默认空） |
| 参数 schema | `input_schema() -> Schema`，`root_schema_for::<Input>()`（`thread.rs:5682`） | `fn parameters(&self) -> serde_json::Value` | `inputSchema`，Standard JSON Schema 转成 provider JSON Schema | TypeBox / pi-schema（`Static<TParameters>`） | `ToolSchema.parameters: Record<string, unknown>` | effect `Schema` | `input_schema()` = `schemars::schema_for!(Self::Input)` |
| 构造时类型约束 | `Input: Deserialize + Serialize + JsonSchema` | `Args: Deserialize + Send + Sync` | `TInput extends SchemaInput` | `TParameters extends object` | 无泛型，运行时校验 | effect schema 推导 | `Input: DeserializeOwned + JsonSchema + Send` |
| 输出声明 | `Output: Into<LanguageModelToolResultContent>` | `Output: IntoToolOutput` | `outputSchema` | `outputSchema` + `structuredContent` | 强制 `output: ToolOutputDefinition`（`schema` + `render`） | `Tool.Info` 的 result 类型 | `Output: Into<ToolResultPart>` |
| 错误形态 | `Into<内容>`，无专门错误类型 | `type Error: std::error::Error`，在擦除边界归一化 | `ToolExecuteFunction` 抛错 | `AgentToolResult.isError` / 抛错 | `ToolExecutionResult` | `Tool.Error` / effect | `Result<Output, Output>`，Err 转成 `ToolOutcome { is_error: true }` |
| 副作用声明 | 无 | `ToolAnnotations`（`request.rs:129`） | `needsApproval` / `approvalSchema` | `replay: "never"｜"safe"`、`executionMode`、`terminate` | `presentationMeta` | permission / namespace | 无 |
| 流式入参 | `supports_input_streaming()` + `ToolInputPayload`（`thread.rs:5689`） | 未见 | middleware 层有 input schema 校验 | `prepareArguments` 兜底 | 参数快照校验 | 参数修复插件 | `supports_input_streaming()` + `ToolInput` / `ToolInputHandle` |
| 注册 | `Thread::add_tool`（`thread.rs:2340` 起） | registry | `tools` 数组 / `toolDefinition().server/client()` | `AgentContext.tools`（`types.ts:511`） | `ToolService.register(definition)`（`index.ts:128`） | `Editor.add(tool)`（`tool.ts`） | `AgentToolRegistry::register`（`BTreeMap`） |
| 取消通道 | `watch::Sender<bool>` | hook Stop | `AbortSignal` + `ctx.abort(reason)` | `signal?: AbortSignal` 传入 `execute` | `exec.signal` | effect 取消 | `&CancelToken` 传入 `AgentTool::run` |

## 2. Zed

- 数据半：`LanguageModelRequestTool`（`crates/language_model_core/src/request.rs:419`），
  `LanguageModelRequestToolInput::Function { input_schema, use_input_streaming }`（`:444`）。
- 执行半：`AgentTool`（`crates/agent/src/thread.rs:5661`）：
  `type Input: Deserialize + Serialize + JsonSchema`、
  `type Output: Deserialize + Serialize + Into<LanguageModelToolResultContent>`、
  `const NAME`、`description()`、`kind()`、`initial_title()`、`input_schema()`、
  `supports_input_streaming()`。
- `description()` 默认从 `schemars::schema_for!(Self::Input)` 的顶层 `description` 取
  （`:5670–:5678`），不是独立常量。
- 工具通过 `Thread::add_tool` 注册（`:2340–:2362`），`BTreeMap` 持有；
  运行时按 name 分发。

## 3. rig

- 数据半：`ToolDefinition { name, description, parameters, annotations }`
  （`crates/rig-core/src/completion/request.rs:70–:82`）。
- 执行半：`Tool` trait（`crates/rig-core/src/tool/contextual.rs:32`）：
  `const NAME`、`type Args`、`type Output: IntoToolOutput`、
  `type Error: std::error::Error`、`description()`、`parameters()`、`annotations()`。
- `ToolAnnotations`（`request.rs:129`）带 `title` / `read_only_hint` /
  `destructive_hint` / `idempotent_hint` / `open_world_hint`，注释明确：**不发给模型**，
  只给宿主的审批/计划模式/并发策略用。
- 错误有独立的 `type Error`，只在擦除分发边界归一成 `ToolExecutionError`。

## 4. TanStack AI

- 基础接口：`Tool<TInput, TOutput, TName, TContext>`（`packages/ai/src/types.ts:1005`），
  字段 `name` / `description` / `inputSchema` / `execute`。
- `inputSchema` 接受任意 Standard JSON Schema 兼容 schema（Zod / ArkType / Valibot）
  或普通 JSON Schema 对象，调用前由 `convertSchemaToJsonSchema` 转成 provider 形状
  （`packages/ai/src/activities/chat/index.ts:1984`）。
- `toolDefinition(config)`（`packages/ai/src/activities/chat/tools/tool-definition.ts:343`）
  返回带 `.server(execute)` / `.client(execute?)` 的构造器；`needsApproval` 与
  `approvalSchema` 有类型级约束（`approvalSchema` 要求 `needsApproval: true`）。
- 返回值由 `ToolResult` 相关工具函数构造；`outputSchema` 存在时用于结构化输出。

## 5. pi

- `AgentTool<TParameters extends object, TDetails>`（`packages/agent/src/types.ts:478`）
  继承 `Tool<TParameters>`，增加：
  - `label`：UI 显示名；
  - `prepareArguments?`：schema 校验前的兼容 shim；
  - `outputSchema?`：`structuredContent` 的 JSON Schema；
  - `execute(toolCallId, params, signal?, onUpdate?)`；
  - `replay: "never" | "safe"`（副作用的恢复策略）；
  - `executionMode: "sequential" | "parallel"`（整批工具的并发覆盖）。
- `AgentToolResult`（`:424`）含 `content` / `details` / `structuredContent?` /
  `usage?` / `isError?` / `terminate?`；`structuredContent` 明确**不发模型**。
- coding-agent 侧用 TypeBox schema + `wrapToolDefinition`
  （`packages/coding-agent/src/core/tools/tool-definition-wrapper.ts`）。

## 6. dsh（deepseek-harness）

- 数据半：`ToolSchema { name, description, parameters, deferLoading? }`
  （`packages/llm/llm/src/types.ts:517–:528`）；`parameters` 是普通 JSON Schema 对象。
- 执行半：`ToolDefinition extends ToolSchema`（`packages/core/tools/src/contracts.ts:27`），
  **强制**声明 `output: ToolOutputDefinition`（`schema` + `render`），
  并带 `execute(args, exec)`、`presentCall` / `presentResult`。
- 注册：`ToolService.register(definition)`（`packages/core/tools/src/index.ts:128`）；
  未声明 output 会直接 `TypeError`。
- `ToolAnnotations` 对应物在 `packages/core/tools/src/presentation.ts`：
  提供 `presentCall` / `presentResult` 的可重放展示投影。

## 7. opencode

- `packages/core/src/tool.ts` 导出 `Tool`（来自 `@opencode/schema/tool`）与
  effect 侧的 `ToolDefinition`（`@opencode/ai/effect`）。
- 注册表是 `Editor`：`list` / `get` / `add(tool: Tool.Info)` / `update` / `remove`
  （`tool.ts` 中 `Editor` 接口）；注册时把 schema 转成 `ToolDefinition` 缓存，
  注释明确「schema 不能在不重新注册的情况下改变」。
- 有 `Tool.RegistrationError`（`name` + `message`）与权限/命名空间概念。

## 8. aa-ai（当前新实现）

- 数据半在 `aa_language_model::request::tools::Tool`：
  `Tool::new_function(name, description, input_schema, use_input_streaming)`。
- 执行半在 `packages/aa_ai_harness/src/tool.rs` 的 `AgentTool`：
  - `type Input: DeserializeOwned + schemars::JsonSchema + Send`；
  - `type Output: Into<ToolResultPart> + Send`；
  - `const NAME`、`const DESCRIPTION`（默认空串）；
  - `fn input_schema()` 用 `schemars::schema_for!(Self::Input)`；
  - `fn run(self: Arc<Self>, input, events: &dyn ToolEvents) -> impl Future<Output = Result<Output, Output>> + Send`。
- `ToolOutcome { part, is_error }`；工具返回 `Err(Output)` 转成 `is_error = true`，
  不是异常。
- `AgentToolRegistry` 用 `BTreeMap<String, Arc<dyn ErasedTool>>`：
  定义顺序稳定（保护 provider 的 prompt cache 前缀），按 name 分发。
- 流式入参：`supports_input_streaming()` 声明后，loop 在参数快照
  （`is_complete: false`）阶段调用 `AgentToolRegistry::start` 提前启动工具，
  用 `ToolInputHandle::send_partial` 投喂修补后的 JSON；终值用 `finish` 送去，
  工具通过 `ToolInput::next` / `recv` 读取。未声明流式的工具仍走一次性 `execute`。
- 目前**没有**输出 schema、副作用声明（只读/破坏性/审批）、并发执行。
  取消通过 `&CancelToken` 传进工具；没有 `progress` 回调，文本增量和工具启动
  由 loop 作为结构化 `ToolLoopEvent` 输出，宿主自行渲染。

## 9. 与其他文档的分工

| 文档 | 回答的问题 |
|---|---|
| 本文 | 工具怎么定义：数据半/执行半、schema、输出、副作用声明、注册 |
| [`tool-input-invalid.md`](tool-input-invalid.md) | 参数不合法时：查找时机、是否执行、错误去向、修复与终止 |
| [`agent-loop.md`](agent-loop.md) | 工具在 loop 里的调度：串行/并发/barrier、终止条件 |
| [`../design/tool-trait.md`](../design/tool-trait.md) | 我们自己的分层规则（定义 vs 执行） |
