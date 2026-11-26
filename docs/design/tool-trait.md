# 工具接口（AgentTool）放哪层 —— 设计决策

> 日期：2026-11-12
> 背景：fork Zed `language_model_core` 时，发现 Zed 把**工具执行接口** `AgentTool` 定义在
> `crates/agent/src/thread.rs:5510`（10470 行的巨型文件），而非独立的 tool 模块。
> 本文记录「为什么 Zed 这么做」+「我们库该怎么做」，用于指导 `aa_agent` 的 tool 接口设计。

## 1. Zed 把 AgentTool 放 thread.rs 的原因（坏味道，非设计）

- `thread.rs` 是 Zed 最大的单文件（10470 行），`AgentTool` 被「就近塞」在工具调用编排逻辑旁。
- `crates/agent/src/tools.rs`（单文件，`built_in_tools` 宏）和 `crates/agent/src/tools/`（目录，每个工具一个文件 `impl AgentTool`）**并存**——trait 定义和 trait 实现物理分离。
- 根因：`AgentTool::run` 的签名依赖 **thread 内部类型**（`ToolCallEventStream`）和 `acp_thread`（`acp_v2::ToolKind`），所以 trait 被焊死在 thread 概念上，没法下移到底层 crate。

```rust
// Zed: AgentTool::run 依赖调用方内部类型 → 这是它无法移到 core 的根因
fn run(self: Arc<Self>, input: ToolInput<Self::Input>,
       event_stream: ToolCallEventStream,   // ← thread 内部类型
       cx: &mut App) -> Task<Result<Self::Output, Self::Output>>;
```

## 2. 我们要避免的

**工具定义（数据）和工具执行（能力）要分层，且执行接口签名只依赖本库暴露的类型。**

Zed 的坏味道 = 把执行接口和线程编排耦合 + 签名依赖调用方内部类型 + 定义/实现物理分离 + 巨型文件。

## 3. 我们的分层（结论）

```
aa_language_model        ← 工具「定义」（数据）：request::tools::Tool
                          纯数据 + 序列化，喂 LLM 用，无执行、无网络依赖
        │ 定义里引用
        ▼
aa_agent                 ← 工具「执行接口」：tool::AgentTool
                          运行时抽象（跑工具/回报/取消/事件）
        │ 实现
        ▼
aa-agent（应用）/ 各工具  ← 具体工具（read/write/edit/bash…）impl tool::AgentTool
```

**执行接口放 `aa_agent`，不放 `aa_language_model`**，因为：
1. 执行需要「工具运行时」概念（事件/取消/结果回填），属 agent 域，不属"请求体"域
2. `aa_language_model` 保持**纯数据 + 序列化**，不引入运行时 trait

## 4. AgentTool 签名（关键：不依赖调用方内部类型）

学 TanStack 而非 Zed：`run` 的签名只依赖**本库暴露的类型**，不依赖"会话/线程"内部类型。
Zed 的 `run(event_stream: ToolCallEventStream, cx: &mut App)` 里 `ToolCallEventStream` 是 thread 内部类型——这是它下移不到 core 的根因。我们用一个本库暴露的小 trait 解耦：

```rust
// aa_agent::tool
pub trait AgentTool {
    type Input: serde::Deserialize + serde::Serialize + schemars::JsonSchema;
    type Output: Into<aa_language_model::request::message::ToolResultPart>;

    const NAME: &'static str;
    fn input_schema() -> serde_json::Value;

    /// 是否在参数流式生成期间就启动（默认 false）。见 §5。
    fn supports_input_streaming() -> bool { false }

    /// events 是本库暴露的 trait（emit 进度/取消/参数快照），
    /// 由调用方（会话/线程）实现并传入——工具接口不依赖任何具体线程类型。
    /// input 是本库的入参通道（非流式工具 `recv()` 立刻拿到完整参数）。
    fn run(
        self: std::sync::Arc<Self>,
        input: ToolInput<Self::Input>,
        events: &dyn ToolEvents,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Output>> + Send;
}

pub trait ToolEvents {
    fn progress(&self, msg: &str);
    fn cancelled(&self) -> bool;   // 调用方是否要求取消
    fn tool_input(&self, name: &str, raw: &str) {}  // 参数快照（默认空实现）
}
```

效果：
- 工具接口可放 core，被任意执行环境复用
- 具体工具（应用层）只 `impl AgentTool`，事件由执行方注入
- 和 `aa_language_model` 的工具定义（数据）通过 `Input`/`Output` 的 schema 关联，但物理分层

## 5. 流式入参（`ToolInput` + 修复式解析）

工具参数是逐 token 生成的。为了不等到流结束才开工，链路分两段：

1. **provider** 的流解码把每个参数增量喂给
   `aa_language_model::provider::parse_streamed_json`（`fix_streamed_json` 补齐
   `}`/`]`/`"`、丢掉悬空 `\`）；能解析才发
   `ToolUse { is_complete: false, input: Json(…) }`，解析不出来就继续等。
2. **agent loop** 遇到第一份快照就调用 `AgentToolRegistry::start` 启动声明了
   `supports_input_streaming` 的工具，用 `ToolInputStream` 持续投喂快照；完整
   `ToolUse` 到达时投喂 `Full`，参数不可解析时投喂 `InvalidJson`。

工具侧看到的是 `ToolInput<Input>`：

- `next()`：逐条读负载（`Partial` / `Full` / `InvalidJson`），适合提前显示标题、
  预热、做无副作用的准备；
- `recv()`：跳过快照等完整入参，坏参数返回 `ToolInputError`（框架统一文案），
  由工具转成自己的错误输出。

约束：
- 快照的值可能被截断，**只能**用于预览 / 可回滚的准备，最终执行以 `Full` 为准；
- 提前结束合法：工具在拿到第一份快照后直接返回，loop 会接住结果，后续投喂
  落入已关闭的通道并被静默丢弃；
- 非流式工具（默认）签名不变：`recv()` 立刻返回 `Ok`，坏 JSON 在进入工具前就
  被 registry 转成框架错误结果（副作用安全）；
- 取消时 loop 丢弃仍在跑的流式 future，并按调用顺序为未完成的调用写「已取消」
  哨兵结果；已经完成的工具结果保留。

## 6. 命名（接上一条「官方默认」决策）

按现行约定（命名空间 + 短类型名，前缀不省略，见 [`crate-layout.md`](crate-layout.md) §3）：
- 执行接口：`aa_agent::tool::AgentTool`（模块 `tool::agent_tool`）
- 数据定义：`aa_language_model::request::tools::Tool`
- 关联：`tool::AgentTool::Input` 的 schema → 生成 `request::tools::Tool`（定义）喂 LLM
