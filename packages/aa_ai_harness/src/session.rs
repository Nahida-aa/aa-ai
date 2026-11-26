//! `AgentSession` — agent 侧的会话运行时：唯一事实源 + 用户回合队列 + 取消。
//!
//! 事实源是 `aa_ai_persistence::SessionLog`（append-only 事件日志 + 生效历史投影）。
//! 运行时状态（排队、当前用户回合、取消令牌）不写进日志——它们不是模型看到的对话事实。

use std::collections::{HashMap, VecDeque};

use aa_ai_core::{CancelReason, CancelToken};
use aa_ai_persistence::{
    AssistantMessage, Header, ModelRef, SessionError, SessionEvent, SessionLog, ToolCall,
    ToolResultRecord,
};

/// UI 列表项：由 [`SessionEvent`] 投影出来，不是第二份可变存储。
///
/// 与 Zed 的 `AgentThreadEntry` 对应的层，但工具调用与其结果在这里**配成一条**，
/// 应用层不用自己合并；`Header` 和孤立的 `ToolResult` 不会成为列表项。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntry {
    User {
        event: usize,
        text: String,
    },
    Assistant {
        event: usize,
        text: String,
        reasoning: Option<String>,
        model: Option<ModelRef>,
    },
    ToolCall {
        event: usize,
        call: ToolCall,
        result: Option<ToolResultRecord>,
    },
    Compaction {
        event: usize,
        summary: String,
        tokens_before: u64,
        model: Option<ModelRef>,
    },
}

/// 把事件流投影成 UI 列表项。
pub fn project_entries(events: &[SessionEvent]) -> Vec<SessionEntry> {
    let results: HashMap<&str, &ToolResultRecord> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResult(record) => Some((record.call_id.as_str(), record)),
            _ => None,
        })
        .collect();

    let mut entries = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            SessionEvent::Header(_) | SessionEvent::ToolResult(_) => {}
            SessionEvent::User { text } => entries.push(SessionEntry::User {
                event: index,
                text: text.clone(),
            }),
            SessionEvent::Assistant(message) => {
                if !message.text.is_empty()
                    || message.reasoning.is_some()
                    || message.signature.is_some()
                {
                    entries.push(SessionEntry::Assistant {
                        event: index,
                        text: message.text.clone(),
                        reasoning: message.reasoning.clone(),
                        model: message.model.clone(),
                    });
                }
                for call in &message.tool_calls {
                    entries.push(SessionEntry::ToolCall {
                        event: index,
                        call: call.clone(),
                        result: results.get(call.id.as_str()).copied().cloned(),
                    });
                }
            }
            SessionEvent::Compaction(record) => entries.push(SessionEntry::Compaction {
                event: index,
                summary: record.summary.clone(),
                tokens_before: record.tokens_before,
                model: record.model.clone(),
            }),
        }
    }
    entries
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurn {
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct AgentSession {
    log: SessionLog,
    queue: VecDeque<UserTurn>,
    active: Option<UserTurn>,
    cancel: CancelToken,
    next_id: u64,
}

impl AgentSession {
    pub fn new(header: Header) -> Result<Self, SessionError> {
        Ok(Self::from_log(SessionLog::new(header)?))
    }

    pub fn from_events(events: Vec<aa_ai_persistence::SessionEvent>) -> Result<Self, SessionError> {
        Ok(Self::from_log(SessionLog::from_events(events)?))
    }

    fn from_log(log: SessionLog) -> Self {
        Self {
            log,
            queue: VecDeque::new(),
            active: None,
            cancel: CancelToken::new(),
            next_id: 0,
        }
    }

    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    pub fn log_mut(&mut self) -> &mut SessionLog {
        &mut self.log
    }

    /// 生效历史：由事件现算，不常驻（内存优先）。
    pub fn messages(&self) -> Vec<std::sync::Arc<aa_language_model::request::message::Message>> {
        self.log.messages()
    }

    pub fn events(&self) -> &[SessionEvent] {
        self.log.events()
    }

    pub fn compactions(&self) -> Vec<aa_ai_persistence::CompactionRecord> {
        self.log.compactions()
    }

    pub fn summary_message(&self) -> Option<usize> {
        self.log.summary_message()
    }

    pub fn push_assistant(&mut self, message: AssistantMessage) {
        self.log.push_assistant(message);
    }

    pub fn push_tool_results(&mut self, records: Vec<ToolResultRecord>) {
        self.log.push_tool_results(records);
    }

    pub fn push_compaction(&mut self, record: aa_ai_persistence::CompactionRecord) {
        self.log.push_compaction(record);
    }

    /// UI 列表项（由事件投影，不持久化）。
    pub fn entries(&self) -> Vec<SessionEntry> {
        project_entries(self.log.events())
    }

    pub fn phase(&self) -> Phase {
        if self.active.is_some() {
            Phase::Running
        } else {
            Phase::Idle
        }
    }

    pub fn submit(&mut self, text: impl Into<String>) -> u64 {
        let user_turn = UserTurn {
            id: self.next_id,
            text: text.into(),
        };
        self.next_id += 1;
        let id = user_turn.id;
        self.queue.push_back(user_turn);
        id
    }

    pub fn active_user_turn(&self) -> Option<&UserTurn> {
        self.active.as_ref()
    }

    pub fn pending_user_turns(&self) -> impl Iterator<Item = &UserTurn> {
        self.queue.iter()
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    pub fn cancel(&self, reason: CancelReason) {
        self.cancel.cancel(reason);
    }

    /// 开始队首用户回合：把它作为 user 事件写进事实源，置 Running。
    pub fn begin(&mut self) -> Option<&UserTurn> {
        if self.active.is_some() {
            return None;
        }
        let user_turn = self.queue.pop_front()?;
        self.log.push_user_text(user_turn.text.as_str());
        self.active = Some(user_turn);
        self.cancel = CancelToken::new();
        self.active.as_ref()
    }

    /// 结束当前用户回合并归还它；回到 Idle，取消令牌换新。
    pub fn finish(&mut self) -> Option<UserTurn> {
        self.active
            .take()
            .inspect(|_| self.cancel = CancelToken::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_turns_run_in_fifo_order() {
        let mut session = AgentSession::new(Header::new("", "")).unwrap();
        let first = session.submit("one");
        let second = session.submit("two");
        assert_eq!((first, second), (0, 1));
        assert_eq!(session.phase(), Phase::Idle);

        assert_eq!(session.begin().map(|user_turn| user_turn.id), Some(first));
        assert_eq!(session.phase(), Phase::Running);
        assert!(session.begin().is_none());
        assert_eq!(session.finish().map(|user_turn| user_turn.id), Some(first));

        assert_eq!(session.begin().map(|user_turn| user_turn.id), Some(second));
        assert_eq!(session.finish().map(|user_turn| user_turn.id), Some(second));
    }

    #[test]
    fn entries_pair_a_tool_call_with_its_result() {
        let mut log = SessionLog::new(Header::new("", "")).unwrap();
        log.push_user_text("hi");
        log.push_assistant(AssistantMessage {
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "echo".to_owned(),
                input: aa_language_model::request::message::LanguageModelToolUseInput::Json(
                    Box::new(serde_json::json!({ "text": "hi" })),
                ),
            }],
            model: Some(ModelRef {
                provider: "fake".to_owned(),
                id: "test".to_owned(),
            }),
            ..Default::default()
        });
        log.push_tool_results(vec![ToolResultRecord {
            call_id: "call_1".to_owned(),
            content: vec!["ok".to_owned()],
            is_error: false,
        }]);

        let entries = project_entries(log.events());
        assert_eq!(entries.len(), 2); // user + tool call（Header/ToolResult 不成为列表项）
        match &entries[1] {
            SessionEntry::ToolCall {
                call,
                result: Some(result),
                ..
            } => {
                assert_eq!(call.id, "call_1");
                assert_eq!(result.content[0], "ok");
            }
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn entries_keep_reasoning_only_assistant_with_tool_calls() {
        let mut log = SessionLog::new(Header::new("", "")).unwrap();
        log.push_assistant(AssistantMessage {
            reasoning: Some("thinking".to_owned()),
            tool_calls: vec![ToolCall {
                id: "call_2".to_owned(),
                name: "echo".to_owned(),
                input: aa_language_model::request::message::LanguageModelToolUseInput::Json(
                    Box::new(serde_json::json!({})),
                ),
            }],
            ..Default::default()
        });

        let entries = project_entries(log.events());
        assert_eq!(entries.len(), 2); // Assistant(思考) + ToolCall
        assert!(matches!(
            &entries[0],
            SessionEntry::Assistant { text, reasoning: Some(reasoning), .. }
                if text.is_empty() && reasoning == "thinking"
        ));
        assert!(matches!(&entries[1], SessionEntry::ToolCall { .. }));
    }

    #[test]
    fn cancel_does_not_leak_into_next_user_turn() {
        let mut session = AgentSession::new(Header::new("", "")).unwrap();
        session.submit("one");
        session.begin();
        let first_token = session.cancel_token().clone();
        session.cancel(CancelReason::User);
        assert!(first_token.is_cancelled());

        session.finish();
        session.submit("two");
        session.begin();
        assert!(!session.cancel_token().is_cancelled());
    }
}
