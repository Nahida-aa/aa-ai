//! 宿主层的用户回合编排：压缩 → 取上下文 → 调 `ToolLoop` → 落盘事件。
//!
//! loop 在 `aa_ai_core`，对会话不可知；本模块才是把 loop 接到会话上的地方。

use aa_ai_compaction::{CompactionConfig, CompactionError};
use aa_ai_core::{CancelReason, ToolLoop, ToolLoopEvent};
use aa_language_model::provider;
use futures::StreamExt;

use crate::session::AgentSession;

#[derive(Debug)]
pub struct TurnOutcome {
    pub reply: String,
    pub cancelled: Option<CancelReason>,
    /// 本回合产生的全部事件，宿主可用来渲染流式 UI（不写入会话）。
    pub events: Vec<ToolLoopEvent>,
}

#[derive(Debug)]
pub enum TurnError {
    Llm(provider::LlmError),
    Compaction(CompactionError),
}

impl From<provider::LlmError> for TurnError {
    fn from(error: provider::LlmError) -> Self {
        Self::Llm(error)
    }
}

/// 跑一个用户回合，并把 loop 产生的事件写进会话的唯一事实源。
pub async fn run_turn(
    agent: &ToolLoop,
    model: &provider::LanguageModel,
    session: &mut AgentSession,
    compaction: &CompactionConfig,
) -> Result<TurnOutcome, TurnError> {
    let cancel = session.cancel_token().clone();

    match aa_ai_compaction::maybe_compact(
        session.log_mut(),
        agent.client(),
        model,
        &agent.tool_definitions(),
        compaction,
        &cancel,
    )
    .await
    {
        Ok(_) => {}
        Err(CompactionError::Cancelled(reason)) => {
            return Ok(TurnOutcome {
                reply: String::new(),
                cancelled: Some(reason),
                events: Vec::new(),
            });
        }
        Err(error) => return Err(TurnError::Compaction(error)),
    }

    // 上下文（模型可见消息序列），不是会话本身。
    let context = session.log().messages();
    let mut stream = agent.run(model, &context, &cancel);
    let mut rendered = Vec::new();
    let mut final_reply = String::new();
    let mut final_cancelled = None;

    while let Some(event) = stream.next().await {
        let event = event?;
        rendered.push(event.clone());
        match event {
            ToolLoopEvent::Assistant(message) => session.log_mut().push_assistant(message),
            ToolLoopEvent::ToolResults(records) => session.log_mut().push_tool_results(records),
            ToolLoopEvent::TextDelta(_) | ToolLoopEvent::ToolStarted { .. } => {}
            ToolLoopEvent::Finished { reply, cancelled } => {
                final_reply = reply;
                final_cancelled = cancelled;
            }
        }
    }

    Ok(TurnOutcome {
        reply: final_reply,
        cancelled: final_cancelled,
        events: rendered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_ai_core::tool::AgentToolRegistry;
    use aa_ai_persistence::Header;
    use aa_language_model::fake;
    use std::sync::Arc;

    #[tokio::test]
    async fn host_folds_loop_events_into_the_session() {
        let agent = ToolLoop::new(
            Arc::new(fake::FakeClient::text("done")),
            AgentToolRegistry::new(),
        );
        let mut session = AgentSession::new(Header::new("", "")).unwrap();
        session.submit("hi");
        session.begin();

        let outcome = run_turn(
            &agent,
            &provider::LanguageModel::new("fake", "test"),
            &mut session,
            &CompactionConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.reply, "done");
        assert!(
            outcome
                .events
                .iter()
                .any(|event| matches!(event, ToolLoopEvent::TextDelta(_)))
        );
        assert_eq!(session.entries().len(), 2); // user + assistant（没有工具项）
        let events = session.events();
        assert!(events.iter().any(|event| matches!(
            event,
            aa_ai_persistence::SessionEvent::Assistant(message)
                if message.text == "done"
                    && message.model.as_ref().is_some_and(|m| m.id == "test")
        )));
    }
}
