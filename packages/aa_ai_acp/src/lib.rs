//! `aa_ai_acp` — ACP 协议适配层（薄）。
//!
//! [`AcpSession`] 是协议侧的会话标识与 agent 绑定；协议编解码、`session/update`
//! 到我们事件流的转换在这里做，但**不持有会话事实源**（事实源在
//! `aa_ai_persistence::SessionLog`），也不做 agent loop。
//!
//! 依赖方向：`aa_ai_acp → aa_ai_harness → aa_ai_core / aa_ai_compaction`。
//! harness 不依赖本 crate——agent 侧运行时对 ACP 不可知。

use std::sync::Arc;

use aa_ai_harness::{Agent, AgentId};

/// ACP 协议里的 session id（对应 `acp_v2::SessionId` 的字符串形态）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// 一条 ACP 会话：协议 id + 承载它的 agent 能力接口。
///
/// 外部 ACP agent 实现 [`Agent`] 后挂到这里；宿主按 `SessionId` 持有多个
/// `AcpSession`，但注册表本身由应用（aacode）管理。
pub struct AcpSession {
    id: SessionId,
    agent: Arc<dyn Agent>,
}

impl AcpSession {
    pub fn new(id: SessionId, agent: Arc<dyn Agent>) -> Self {
        Self { id, agent }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn agent_id(&self) -> &AgentId {
        self.agent.id()
    }

    pub fn agent(&self) -> Arc<dyn Agent> {
        self.agent.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_ai_core::ToolLoop;
    use aa_ai_core::tool::AgentToolRegistry;
    use aa_ai_harness::NativeAgent;
    use std::sync::Arc;

    #[test]
    fn acp_session_binds_a_protocol_id_to_an_agent() {
        let agent = NativeAgent::new(
            AgentId::new("local"),
            ToolLoop::new(
                Arc::new(aa_language_model::fake::FakeClient::text("done")),
                AgentToolRegistry::new(),
            ),
        );
        let session = AcpSession::new(SessionId::new("thread-1"), Arc::new(agent));
        assert_eq!(session.id(), &SessionId::new("thread-1"));
        assert_eq!(session.agent_id(), &AgentId::new("local"));
    }
}
