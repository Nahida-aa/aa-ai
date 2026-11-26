//! `aa_ai_persistence` — 会话的**唯一事实源**：append-only 事件日志 + 由它派生的生效历史。
//!
//! canonical IR（`aa_language_model::request::message`）不实现 serde；这里用显式的
//! [`SessionEvent`] 作为磁盘契约。版本不匹配明确拒绝，不猜测兼容。
//!
//! 本 crate 不跑 loop、不做压缩、不读文件路径。

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aa_language_model::provider::{StopReason, TokenUsage};
use aa_language_model::request::message::{
    ContentPart, LanguageModelToolUseInput, Message, Role, ToolResultPart,
};
use serde::{Deserialize, Serialize};

/// 磁盘格式版本。改事件形状必须递增。
pub const VERSION: u32 = 3;

/// 压缩时原样保留的用户文本预算（字节）。
pub const RETAINED_USER_BYTES: usize = 80_000;

/// 会话日志的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    Header(Header),
    User { text: String },
    Assistant(AssistantMessage),
    ToolResult(ToolResultRecord),
    Compaction(CompactionRecord),
}

/// 会话头。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub created_at: u64,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
}

impl Header {
    pub fn new(cwd: impl Into<String>, system: impl Into<String>) -> Self {
        Self {
            version: VERSION,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
            cwd: cwd.into(),
            system: system.into(),
        }
    }
}

/// 产生某条事件的模型身份。会话可以在过程中切换提供商/模型，
/// 所以模型信息属于事件，不属于会话头。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

impl From<&aa_language_model::provider::LanguageModel> for ModelRef {
    fn from(model: &aa_language_model::provider::LanguageModel) -> Self {
        Self {
            provider: model.provider.clone(),
            id: model.id.clone(),
        }
    }
}

/// 模型一次回复的完整输出。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// 产生这条回复的模型；`None` 用于恢复旧日志/手工构造。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
}

impl AssistantMessage {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.is_none()
            && self.signature.is_none()
            && self.tool_calls.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: LanguageModelToolUseInput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub call_id: String,
    pub content: Vec<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_users: Vec<String>,
    pub first_kept_message: usize,
    pub tokens_before: u64,
    /// 生成摘要的模型（可与会话当前模型不同）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
}

/// 持久化格式错误。全部明确拒绝，不猜测兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    MissingHeader,
    DuplicateHeader,
    UnsupportedVersion { found: u32, supported: u32 },
    Json { line: usize, message: String },
    Encode(String),
    UnsupportedContent(&'static str),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => write!(formatter, "会话日志缺少 header"),
            Self::DuplicateHeader => write!(formatter, "会话日志里出现多个 header"),
            Self::UnsupportedVersion { found, supported } => {
                write!(formatter, "会话版本 {found} 不受支持（当前 {supported}）")
            }
            Self::Json { line, message } => {
                write!(formatter, "第 {line} 行不是合法事件：{message}")
            }
            Self::Encode(message) => write!(formatter, "事件序列化失败：{message}"),
            Self::UnsupportedContent(what) => write!(formatter, "无法记录：{what}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// 会话存储契约：库只要求「能载入事件、能追加事件」，不规定磁盘格式。
///
/// 实现方可以是 JSONL 文件（见 `examples/jsonl.rs`）、SQLite、对象存储或内存。
/// 库本身不做 I/O；`SessionLog` 只是内存中的事实源。
pub trait SessionStore {
    type Error: std::error::Error;

    /// 读回完整事件流（顺序必须与写入顺序一致）。
    fn load(&self) -> Result<Vec<SessionEvent>, Self::Error>;

    /// 追加新事件；已写入的事件不会被修改。
    fn append(&self, events: &[SessionEvent]) -> Result<(), Self::Error>;
}

/// 会话日志：**唯一事实源就是 `events`**（append-only）。
///
/// 生效历史不常驻内存：每次需要时由 [`SessionLog::project`] 从事件 fold 出来。
/// 这是「内存优先、吞吐第二」的取舍——省掉第二份 `Arc<Message>` 常驻副本，
/// 代价是每次请求要重建一次投影。
#[derive(Debug, Clone)]
pub struct SessionLog {
    header: Header,
    events: Vec<SessionEvent>,
}

/// 由事件流投影出的运行时视图（临时，用完即弃）。
#[derive(Debug, Default)]
struct Projection {
    messages: Vec<Arc<Message>>,
    compactions: Vec<CompactionRecord>,
    summary_message: Option<usize>,
}

impl SessionLog {
    pub fn new(header: Header) -> Result<Self, SessionError> {
        Self::check_version(&header)?;
        Ok(Self {
            events: vec![SessionEvent::Header(header.clone())],
            header,
        })
    }

    pub fn from_events(events: Vec<SessionEvent>) -> Result<Self, SessionError> {
        let header = match events.first() {
            Some(SessionEvent::Header(header)) => header.clone(),
            _ => return Err(SessionError::MissingHeader),
        };
        Self::check_version(&header)?;
        if events[1..]
            .iter()
            .any(|event| matches!(event, SessionEvent::Header(_)))
        {
            return Err(SessionError::DuplicateHeader);
        }
        Ok(Self { header, events })
    }

    fn check_version(header: &Header) -> Result<(), SessionError> {
        if header.version != VERSION {
            return Err(SessionError::UnsupportedVersion {
                found: header.version,
                supported: VERSION,
            });
        }
        Ok(())
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// 生效历史（模型请求看到的就是它）。按事件现算，不常驻。
    pub fn messages(&self) -> Vec<Arc<Message>> {
        self.project().messages
    }

    /// 日志里出现过的压缩记录。
    pub fn compactions(&self) -> Vec<CompactionRecord> {
        self.project().compactions
    }

    /// 摘要消息在当前生效历史里的下标。
    pub fn summary_message(&self) -> Option<usize> {
        self.project().summary_message
    }

    fn project(&self) -> Projection {
        let mut projection = Projection::default();
        push_system(&mut projection.messages, &self.header.system);
        let mut pending_results: Vec<ContentPart> = Vec::new();

        for event in &self.events[1..] {
            match event {
                SessionEvent::User { text } => {
                    flush_results(&mut projection.messages, &mut pending_results);
                    projection.messages.push(Arc::new(Message::new(
                        Role::User,
                        vec![ContentPart::text(text.as_str())],
                    )));
                }
                SessionEvent::Assistant(message) => {
                    flush_results(&mut projection.messages, &mut pending_results);
                    if let Some(message) = to_message(message) {
                        projection.messages.push(Arc::new(message));
                    }
                }
                SessionEvent::ToolResult(record) => {
                    pending_results.push(to_result_part(record));
                }
                SessionEvent::Compaction(record) => {
                    flush_results(&mut projection.messages, &mut pending_results);
                    projection.compactions.push(record.clone());
                    projection.summary_message = apply_compaction(&mut projection.messages, record);
                }
                SessionEvent::Header(_) => unreachable!(),
            }
        }
        flush_results(&mut projection.messages, &mut pending_results);
        projection
    }

    pub fn push_user_text(&mut self, text: impl Into<String>) {
        self.events.push(SessionEvent::User { text: text.into() });
    }

    pub fn push_assistant(&mut self, message: AssistantMessage) {
        if !message.is_empty() {
            self.events.push(SessionEvent::Assistant(message));
        }
    }

    pub fn push_tool_results(&mut self, records: Vec<ToolResultRecord>) {
        self.events
            .extend(records.into_iter().map(SessionEvent::ToolResult));
    }

    pub fn push_compaction(&mut self, record: CompactionRecord) {
        self.events.push(SessionEvent::Compaction(record));
    }
}

fn push_system(messages: &mut Vec<Arc<Message>>, system: &str) {
    if !system.is_empty() {
        messages.push(Arc::new(Message::new(
            Role::System,
            vec![ContentPart::text(system)],
        )));
    }
}

fn flush_results(messages: &mut Vec<Arc<Message>>, pending: &mut Vec<ContentPart>) {
    if !pending.is_empty() {
        messages.push(Arc::new(Message::new(Role::User, std::mem::take(pending))));
    }
}

fn to_message(message: &AssistantMessage) -> Option<Message> {
    if message.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if message.reasoning.is_some() || message.signature.is_some() {
        parts.push(ContentPart::Reasoning {
            text: Arc::from(message.reasoning.as_deref().unwrap_or_default()),
            signature: message.signature.as_deref().map(Arc::from),
        });
    }
    if !message.text.is_empty() {
        parts.push(ContentPart::text(message.text.as_str()));
    }
    for call in &message.tool_calls {
        parts.push(ContentPart::tool_call(
            call.id.as_str(),
            call.name.as_str(),
            call.input.clone(),
        ));
    }
    Some(Message::new(Role::Assistant, parts))
}

fn to_result_part(record: &ToolResultRecord) -> ContentPart {
    let content = record
        .content
        .iter()
        .map(|text| ToolResultPart::from(text.as_str()))
        .collect::<Vec<_>>();
    ContentPart::tool_result(record.call_id.as_str(), content, record.is_error)
}

fn apply_compaction(messages: &mut Vec<Arc<Message>>, record: &CompactionRecord) -> Option<usize> {
    let first_kept = record.first_kept_message.min(messages.len());
    let kept = messages.split_off(first_kept);
    let system = messages
        .first()
        .filter(|message| message.role == Role::System)
        .cloned();
    messages.clear();
    if let Some(system) = system {
        messages.push(system);
    }
    for text in &record.retained_users {
        messages.push(Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text(text.as_str())],
        )));
    }
    let summary_index = messages.len();
    messages.push(Arc::new(Message::new(
        Role::User,
        vec![ContentPart::text(format!(
            "The previous conversation was compacted. Use this summary as context:\n\n{}",
            record.summary
        ))],
    )));
    messages.extend(kept);
    Some(summary_index)
}

/// 从 `messages[..before]` 里挑最近的用户文本，不超过 [`RETAINED_USER_BYTES`]。
pub fn retained_user_texts(
    messages: &[Arc<Message>],
    before: usize,
    skip: Option<usize>,
) -> Vec<String> {
    let mut remaining = RETAINED_USER_BYTES;
    let mut retained = Vec::new();
    for (index, message) in messages[..before.min(messages.len())]
        .iter()
        .enumerate()
        .rev()
    {
        if Some(index) == skip {
            continue;
        }
        if message.role != Role::User {
            continue;
        }
        let Some(text) = message.content.iter().find_map(|part| match part {
            ContentPart::Text(text) => Some(&**text),
            _ => None,
        }) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if text.len() <= remaining {
            remaining -= text.len();
            retained.push(text.to_string());
        } else {
            if remaining > 0 {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                retained.push(text[..end].to_string());
            }
            break;
        }
    }
    retained.reverse();
    retained
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_roundtrips_and_projects_history() {
        let mut log = SessionLog::new(Header::new("", "sys")).unwrap();
        log.push_user_text("hi");
        log.push_assistant(AssistantMessage {
            text: "hello".into(),
            ..Default::default()
        });

        // 格式由使用者决定；这里只是用 serde_json 演示一次 round-trip。
        let lines = log
            .events()
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let events = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let restored = SessionLog::from_events(events).unwrap();
        assert_eq!(restored.events(), log.events());
        assert_eq!(restored.messages().len(), 3); // system → user → assistant
    }

    #[test]
    fn assistant_with_only_reasoning_and_tool_calls_survives() {
        let mut log = SessionLog::new(Header::new("", "")).unwrap();
        log.push_assistant(AssistantMessage {
            text: String::new(),
            reasoning: Some("thinking".to_owned()),
            signature: Some("sig".to_owned()),
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "echo".to_owned(),
                input: aa_language_model::request::message::LanguageModelToolUseInput::Json(
                    Box::new(serde_json::json!({ "text": "hi" })),
                ),
            }],
            ..Default::default()
        });

        assert_eq!(log.events().len(), 2); // Header + Assistant（没有空轮被丢掉）
        let messages = log.messages();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0].content()[0],
            ContentPart::Reasoning { text, signature }
                if text.as_ref() == "thinking" && signature.as_deref() == Some("sig")
        ));
        assert!(matches!(
            &messages[0].content()[1],
            ContentPart::ToolCall { .. }
        ));
    }

    #[test]
    fn compaction_replaces_live_history_but_keeps_events() {
        let mut log = SessionLog::new(Header::new("", "")).unwrap();
        for i in 0..4 {
            log.push_user_text(format!("q{i}"));
            log.push_assistant(AssistantMessage {
                text: format!("a{i}"),
                ..Default::default()
            });
        }
        let before = log.events().len();
        log.push_compaction(CompactionRecord {
            summary: "summary".into(),
            retained_users: vec!["q0".into()],
            first_kept_message: 5,
            tokens_before: 100,
            model: None,
        });
        assert_eq!(log.events().len(), before + 1);
        assert!(log.compactions().len() == 1);
    }
}
