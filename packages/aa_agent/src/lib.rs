//! `aa_agent` — agent 运行时核心（工具接口 / agent loop / 会话）。

pub mod agent_loop;
pub mod session;
pub mod tool;

pub use agent_loop::{AgentError, AgentLoop};
pub use session::Session;
