//! `aa_ai_harness` — agent 侧运行时（从零实现，不依赖旧 `aa_agent`）。
//!
//! 单一事实源是 `aa_ai_persistence::SessionLog`；本 crate 负责会话运行时状态、
//! 用户回合编排、`Agent` 能力接口与 UI 投影。agent loop 与工具在 `aa_ai_core`。

pub mod agent;
pub mod session;
pub mod turn;

pub use agent::{Agent, AgentError, AgentId, AgentStream, NativeAgent};
pub use session::{AgentSession, SessionEntry};
pub use turn::{TurnError, TurnOutcome, run_turn};
