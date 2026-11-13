//! 工具「执行接口」（agent 层的运行时抽象）。
//!
//! 分层（见 `docs/design/tool-trait.md`）：
//! - `aa_language_model::request::tools` = 工具「定义」（数据，可序列化喂 LLM）
//! - 本模块 = 工具「执行接口」（`AgentTool` trait + 事件注入 + 注册分发）
//!
//! 关键约束：`AgentTool` 的签名只依赖**本库暴露的类型**（`ToolEvents`）
//! 和 `aa_language_model`（数据），**不依赖调用方（会话/线程）的内部类型**
//! ——这是 Zed `AgentTool`（依赖 thread 内部 `ToolCallEventStream`）下移不到
//! core 的根因，我们刻意避免。

mod agent_registry;
mod agent_tool;

pub use agent_registry::AgentToolRegistry;
pub use agent_tool::{AgentTool, ToolEvents, ToolOutcome};
