//! `aa_agent` — agent 运行时核心（工具接口 / agent loop / 会话 / 上下文压缩）。

pub mod agent_loop;
pub mod compaction;
pub mod session;
pub mod tool;

pub use agent_loop::{AgentError, AgentLoop};
pub use compaction::CompactionConfig;
pub use session::Session;
