//! `aa_agent` — agent 运行时核心（工具接口 / agent loop）。

pub mod agent_loop;
pub mod tool;

pub use agent_loop::AgentLoop;
