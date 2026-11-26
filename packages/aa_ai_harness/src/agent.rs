//! `Agent` — agent 的**能力接口**：宿主只依赖它，不关心 agent 是进程内还是外部。
//!
//! 原生实现直接跑 `aa_ai_core::ToolLoop`；外部 ACP agent 在 `aa_ai_acp` 里实现
//! 同一个 trait。两者都返回同一种事件流，宿主用相同方式渲染/落盘。

use std::fmt;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;

use aa_ai_core::{CancelToken, ToolLoop, ToolLoopEvent};
use aa_language_model::provider;
use aa_language_model::request::message::Message;

/// agent 标识（与 provider/model 无关：一个 agent 可以在会话中换模型）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum AgentError {
    Llm(provider::LlmError),
    /// 外部 agent / 协议层的错误，不假设具体协议类型。
    Protocol(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => error.fmt(formatter),
            Self::Protocol(message) => write!(formatter, "agent protocol error: {message}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<provider::LlmError> for AgentError {
    fn from(error: provider::LlmError) -> Self {
        Self::Llm(error)
    }
}

/// 所有 agent 共用的事件流类型。
pub type AgentStream = BoxStream<'static, Result<ToolLoopEvent, AgentError>>;

/// agent 的能力接口：对一段上下文跑一个用户回合。
///
/// 与 [`ToolLoop`](aa_ai_core::ToolLoop) 一样，这里**不接收会话**——会话由宿主
/// 持有并 fold 事件。原生/外部实现的区别只在于事件怎么产生。
pub trait Agent: Send + Sync {
    fn id(&self) -> &AgentId;

    /// 在 `context`（模型上下文）上跑一个回合，返回实时事件流。
    fn run(
        &self,
        model: &provider::LanguageModel,
        context: &[Arc<Message>],
        cancel: &CancelToken,
    ) -> AgentStream;
}

/// 进程内原生 agent：直接驱动 core 的 `ToolLoop`。
pub struct NativeAgent {
    id: AgentId,
    tool_loop: ToolLoop,
}

impl NativeAgent {
    pub fn new(id: AgentId, tool_loop: ToolLoop) -> Self {
        Self { id, tool_loop }
    }

    pub fn tool_loop(&self) -> &ToolLoop {
        &self.tool_loop
    }
}

impl Agent for NativeAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn run(
        &self,
        model: &provider::LanguageModel,
        context: &[Arc<Message>],
        cancel: &CancelToken,
    ) -> AgentStream {
        Box::pin(
            self.tool_loop
                .run(model, context, cancel)
                .map(|item| item.map_err(AgentError::from)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_ai_core::tool::AgentToolRegistry;
    use aa_language_model::fake;

    #[tokio::test]
    async fn native_agent_implements_the_capability_interface() {
        let agent = NativeAgent::new(
            AgentId::new("local"),
            ToolLoop::new(
                Arc::new(fake::FakeClient::text("done")),
                AgentToolRegistry::new(),
            ),
        );
        assert_eq!(agent.id(), &AgentId::new("local"));
        let events = agent
            .run(
                &provider::LanguageModel::new("fake", "test"),
                &[],
                &CancelToken::new(),
            )
            .collect::<Vec<_>>()
            .await;
        let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(matches!(
            events.last(),
            Some(ToolLoopEvent::Finished { reply, .. }) if reply == "done"
        ));
    }
}
