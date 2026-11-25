//! 会话：历史 + 可落盘的事件日志。
//!
//! 为什么不让 canonical IR 直接 serde：`aa_language_model::request::message`
//! 明确不实现 serde（IR 是内存契约，不是磁盘契约）。这里用一层显式的
//! [`SessionEvent`]：只记录**重放所需的事实**（文本 / 思考 + 签名 / 工具调用 /
//! 工具结果），加载时再还原成 `Message`。`version` 不匹配就明确拒绝，
//! 不做猜测式兼容。
//!
//! 文件路径、什么时候追加、崩溃后怎么处理，都是应用的事；本模块只管
//! 「一行一个事件」的格式和内存里的历史。事件是**追加式**的：应用按
//! [`Session::events`] 的顺序写，恢复时按同样顺序读。

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aa_language_model::provider::{StopReason, TokenUsage};
use aa_language_model::request::message::{ContentPart, Message, Role, ToolInput, ToolResultPart};
use serde::{Deserialize, Serialize};

/// 磁盘格式版本。改事件形状时必须递增，并在 [`Session::from_events`] 里明确拒绝旧版本。
///
/// v2：增加 [`SessionEvent::Compaction`]。
pub const VERSION: u32 = 2;

/// 压缩时从旧历史里原样保留的用户文本预算（字节）。
///
/// 对齐 Zed 的 `COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET`（约 20k token）：
/// 摘要之外再留一批最近的用户消息，避免摘要丢掉用户的原话。
pub const RETAINED_USER_BYTES: usize = 80_000;

/// 会话日志的一行。
///
/// 序列化后是内部标记的 JSON：`{"type":"user","text":"..."}`。
/// `Header` 必须是第一条，且只有一条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// 会话事实：版本、创建时间、provider / model、工作目录、系统提示。
    Header(Header),
    /// 用户输入。
    User { text: String },
    /// 模型这一轮的完整输出（文本 + 思考 + 工具调用 + 用量 / 结束原因）。
    Assistant(Turn),
    /// 一条工具结果。同一次模型调用产生的多条结果在恢复时会合并成一条 user 消息。
    ToolResult(ToolResultRecord),
    /// 旧对话已被摘要替代。原始事件仍保留在日志里（审计 / 导出），
    /// 但内存里的生效历史只保留摘要与保留段。
    Compaction(CompactionRecord),
}

/// 一次压缩的事实。
///
/// 存的是「重放需要什么」，不是压缩算法的全部细节：
/// - `summary`：模型生成的摘要原文；
/// - `retained_users`：压缩时挑出来原样保留的用户文本（按时间顺序）；
/// - `first_kept_message`：摘要之后第一条保留消息在**压缩时生效历史**里的下标
///   （含 system 那条）；
/// - `tokens_before`：压缩前的估算值，只用于展示和调参。
///
/// 原始事件不会被删除：日志仍是完整审计记录，压缩只替换内存里的生效历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_users: Vec<String>,
    pub first_kept_message: usize,
    pub tokens_before: u64,
}

/// 会话头。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    /// Unix 秒。只用于展示和排序，不参与历史重放。
    pub created_at: u64,
    pub provider: String,
    pub model: String,
    /// 会话创建时的工作目录；恢复时用来提示（工具权限仍由调用方决定）。
    #[serde(default)]
    pub cwd: String,
    /// 系统提示；空串表示没有。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
}

impl Header {
    /// 用当前时间构造一个当前版本（v2）的会话头。
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        cwd: impl Into<String>,
        system: impl Into<String>,
    ) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        Self {
            version: VERSION,
            created_at,
            provider: provider.into(),
            model: model.into(),
            cwd: cwd.into(),
            system: system.into(),
        }
    }
}

/// 模型一轮的输出。
///
/// 字段顺序和 [`ContentPart`] 的回放顺序一致：思考 → 文本 → 工具调用。
/// 思考的 `signature` 必须原样保留（Anthropic 等要求回放签名块）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
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
}

impl Turn {
    /// 没有文本、没有思考、没有工具调用：不写历史，避免产生空 assistant 消息。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.is_none()
            && self.signature.is_none()
            && self.tool_calls.is_empty()
    }
}

/// 一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: RecordedInput,
}

/// 工具入参的持久化形状。
///
/// 正常是 JSON；模型给纯文本（或流式半截 JSON）时是文本。两种都保留，
/// 不能把文本硬塞进 `Value`（会改变回放给模型的内容）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RecordedInput {
    Json(serde_json::Value),
    Text(String),
}

impl From<&ToolInput> for RecordedInput {
    fn from(input: &ToolInput) -> Self {
        match input {
            ToolInput::Json(value) => Self::Json((**value).clone()),
            ToolInput::Text(text) => Self::Text(text.to_string()),
        }
    }
}

impl RecordedInput {
    /// 还原成 canonical 的工具入参。
    pub fn to_tool_input(&self) -> ToolInput {
        match self {
            Self::Json(value) => ToolInput::Json(Box::new(value.clone())),
            Self::Text(text) => ToolInput::Text(Arc::from(text.as_str())),
        }
    }
}

/// 一条工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub call_id: String,
    /// 日志只记录文本段。图片结果没有稳定编码，构造时明确报错，不静默丢内容。
    pub content: Vec<String>,
    pub is_error: bool,
}

impl ToolResultRecord {
    /// 从 canonical 的工具结果片段构造。
    ///
    /// 图片段返回 [`SessionError::UnsupportedContent`]：会话日志是回放用的，
    /// 丢掉图片会让恢复后的历史与当时发给模型的不一致。
    pub fn from_parts(
        call_id: &str,
        parts: &[ToolResultPart],
        is_error: bool,
    ) -> Result<Self, SessionError> {
        let mut content = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                ToolResultPart::Text(text) => content.push(text.to_string()),
                ToolResultPart::Image(_) => {
                    return Err(SessionError::UnsupportedContent("tool result image"));
                }
            }
        }
        Ok(Self {
            call_id: call_id.to_owned(),
            content,
            is_error,
        })
    }
}

/// 内存里的会话：头 + 事件日志 + 由事件还原的历史。
///
/// 历史始终是 [`Arc<Message>`]（对齐 `0004-message-layout.md` D7）：克隆整条
/// 历史只加引用计数。`events` 与 `messages` 同步增长，应用可以按
/// `events().len()` 判断还需要追加哪些行。
#[derive(Debug, Clone)]
pub struct Session {
    header: Header,
    events: Vec<SessionEvent>,
    messages: Vec<Arc<Message>>,
    /// 日志里出现过的压缩记录。生效历史只看最后一条。
    compactions: Vec<CompactionRecord>,
    /// 摘要消息在 `messages` 里的下标。摘要也是 user 消息（provider 无关），
    /// 保留用户原话时要把它排除掉。
    summary_message: Option<usize>,
}

impl Session {
    /// 新会话。系统提示非空时，历史的第一条就是它。
    pub fn new(header: Header) -> Result<Self, SessionError> {
        if header.version != VERSION {
            return Err(SessionError::UnsupportedVersion {
                found: header.version,
                supported: VERSION,
            });
        }
        let mut messages = Vec::new();
        push_system(&mut messages, &header.system);
        Ok(Self {
            events: vec![SessionEvent::Header(header.clone())],
            header,
            messages,
            compactions: Vec::new(),
            summary_message: None,
        })
    }

    /// 从事件日志还原（`events[0]` 必须是 [`SessionEvent::Header`]）。
    ///
    /// 连续的 `ToolResult` 会合并成一条 user 消息：agent loop 把同一轮并行
    /// 工具结果写在一条消息里，恢复时必须保持同样的形状。
    pub fn from_events(events: Vec<SessionEvent>) -> Result<Self, SessionError> {
        let header = match events.first() {
            Some(SessionEvent::Header(header)) => header.clone(),
            _ => return Err(SessionError::MissingHeader),
        };
        if header.version != VERSION {
            return Err(SessionError::UnsupportedVersion {
                found: header.version,
                supported: VERSION,
            });
        }

        let mut messages = Vec::new();
        push_system(&mut messages, &header.system);
        let mut pending_results: Vec<ContentPart> = Vec::new();
        let mut compactions = Vec::new();
        let mut summary_message = None;

        for event in &events[1..] {
            match event {
                SessionEvent::Header(_) => return Err(SessionError::DuplicateHeader),
                SessionEvent::User { text } => {
                    flush_results(&mut messages, &mut pending_results);
                    messages.push(Arc::new(Message::new(
                        Role::User,
                        vec![ContentPart::text(text.as_str())],
                    )));
                }
                SessionEvent::Assistant(turn) => {
                    flush_results(&mut messages, &mut pending_results);
                    if let Some(message) = turn_message(turn) {
                        messages.push(Arc::new(message));
                    }
                }
                SessionEvent::ToolResult(record) => {
                    pending_results.push(tool_result_part(record));
                }
                // 压缩：替换生效历史，但原始事件保留（审计）。
                SessionEvent::Compaction(record) => {
                    compactions.push(record.clone());
                    summary_message = apply_compaction(&mut messages, record);
                }
            }
        }
        flush_results(&mut messages, &mut pending_results);

        Ok(Self {
            header,
            events,
            messages,
            compactions,
            summary_message,
        })
    }

    /// 从 JSONL 文本解析。空行忽略；其他行必须是合法的 [`SessionEvent`]。
    pub fn from_jsonl(text: &str) -> Result<Self, SessionError> {
        let mut events = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: SessionEvent =
                serde_json::from_str(line).map_err(|error| SessionError::Json {
                    line: index + 1,
                    message: error.to_string(),
                })?;
            events.push(event);
        }
        Self::from_events(events)
    }

    /// 追加一条用户消息。
    pub fn push_user_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.events.push(SessionEvent::User { text: text.clone() });
        self.messages.push(Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text(text)],
        )));
    }

    /// 追加模型这一轮的输出。空轮（没有任何内容）不写历史。
    pub fn push_assistant(&mut self, turn: Turn) {
        if turn.is_empty() {
            return;
        }
        if let Some(message) = turn_message(&turn) {
            self.messages.push(Arc::new(message));
        }
        self.events.push(SessionEvent::Assistant(turn));
    }

    /// 追加一批工具结果（同一次模型调用产生的），合并成一条 user 消息。
    pub fn push_tool_results(&mut self, records: Vec<ToolResultRecord>) {
        if records.is_empty() {
            return;
        }
        let parts = records
            .iter()
            .map(tool_result_part)
            .collect::<Vec<ContentPart>>();
        self.messages
            .push(Arc::new(Message::new(Role::User, parts)));
        self.events
            .extend(records.into_iter().map(SessionEvent::ToolResult));
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn system(&self) -> &str {
        &self.header.system
    }

    /// 当前生效的历史（`Arc` 克隆，不复制内容）。
    ///
    /// 压缩之后这里**只剩摘要 + 保留段**；被摘要掉的原始消息不再常驻内存
    /// （审计全文仍在 [`Session::events`] 里）。发给模型的就是它。
    pub fn messages(&self) -> &[Arc<Message>] {
        &self.messages
    }

    /// 日志里出现过的压缩记录（按出现顺序）。
    pub fn compactions(&self) -> &[CompactionRecord] {
        &self.compactions
    }

    /// 摘要消息在生效历史里的下标（没有压缩时为 `None`）。
    pub fn summary_message(&self) -> Option<usize> {
        self.summary_message
    }

    /// 记录一次压缩，并把生效历史替换成「保留的用户消息 → 摘要 → 保留段」。
    ///
    /// 原始事件不删除：日志仍是完整审计记录。
    pub fn push_compaction(&mut self, record: CompactionRecord) {
        self.events.push(SessionEvent::Compaction(record.clone()));
        self.compactions.push(record.clone());
        self.summary_message = apply_compaction(&mut self.messages, &record);
    }

    /// 完整事件日志（第 0 条是 header）。应用按这个顺序追加到文件。
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// 事件数量（不是消息数量）。
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// 历史消息数量（含 system）。
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

impl SessionEvent {
    /// 序列化成一行（不含换行符）。
    pub fn to_json_line(&self) -> Result<String, SessionError> {
        serde_json::to_string(self).map_err(|error| SessionError::Encode(error.to_string()))
    }
}

impl fmt::Display for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} / {} · {} 条消息 · {} 个事件",
            self.header.provider,
            self.header.model,
            self.messages.len(),
            self.events.len()
        )
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

/// 把生效历史替换成 `system → 保留的用户消息 → 摘要 → 压缩点之后的消息`。
///
/// `first_kept_message` 是压缩时生效历史里的下标；被覆盖的 `Arc<Message>`
/// 在这里直接 drop（0004 D7 的目标）。返回摘要消息的新下标。
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

/// 从 `messages[..before]` 里挑出最近的用户文本，总量不超过 [`RETAINED_USER_BYTES`]。
///
/// 只挑「纯用户输入」：带工具结果的消息不算（它属于模型回合，不是用户原话）；
/// `skip` 用来排除上一条压缩摘要（它也是 user 消息，但不是用户说的话）。
/// 放不下的那条按字节截断（保证落在 UTF-8 字符边界上），对齐 Zed 的
/// `truncate_user_message_to_byte_budget`。
pub fn retained_user_texts(
    messages: &[Arc<Message>],
    before: usize,
    skip: Option<usize>,
) -> Vec<String> {
    let mut remaining = RETAINED_USER_BYTES;
    let mut retained: Vec<String> = Vec::new();

    for (index, message) in messages[..before.min(messages.len())]
        .iter()
        .enumerate()
        .rev()
    {
        if Some(index) == skip {
            continue;
        }
        let Some(text) = user_text(message) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if text.len() <= remaining {
            remaining -= text.len();
            retained.push(text.to_owned());
        } else {
            if remaining > 0 {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                retained.push(text[..end].to_owned());
            }
            break;
        }
    }

    retained.reverse();
    retained
}

/// 纯用户文本：角色是 user、有文本、且不是工具结果。
pub(crate) fn user_text(message: &Message) -> Option<&str> {
    if message.role != Role::User {
        return None;
    }
    let mut text = None;
    for part in &message.content {
        match part {
            ContentPart::Text(value) if text.is_none() => text = Some(&**value),
            ContentPart::ToolResult { .. } => return None,
            _ => {}
        }
    }
    text
}

fn flush_results(messages: &mut Vec<Arc<Message>>, pending: &mut Vec<ContentPart>) {
    if !pending.is_empty() {
        messages.push(Arc::new(Message::new(Role::User, std::mem::take(pending))));
    }
}

fn turn_message(turn: &Turn) -> Option<Message> {
    if turn.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(2 + turn.tool_calls.len());
    if turn.reasoning.is_some() || turn.signature.is_some() {
        parts.push(ContentPart::Reasoning {
            text: Arc::from(turn.reasoning.as_deref().unwrap_or_default()),
            signature: turn.signature.as_deref().map(Arc::from),
        });
    }
    if !turn.text.is_empty() {
        parts.push(ContentPart::text(turn.text.as_str()));
    }
    for call in &turn.tool_calls {
        parts.push(ContentPart::tool_call(
            call.id.as_str(),
            call.name.as_str(),
            call.input.to_tool_input(),
        ));
    }
    Some(Message::new(Role::Assistant, parts))
}

fn tool_result_part(record: &ToolResultRecord) -> ContentPart {
    let content = record
        .content
        .iter()
        .map(|text| ToolResultPart::from(text.as_str()))
        .collect::<Vec<ToolResultPart>>();
    ContentPart::tool_result(record.call_id.as_str(), content, record.is_error)
}

/// 会话格式错误。全部是「明确拒绝」，没有猜测式兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// 第一行不是 header。
    MissingHeader,
    /// header 出现了不止一次。
    DuplicateHeader,
    /// 版本不认识。
    UnsupportedVersion { found: u32, supported: u32 },
    /// 某一行不是合法的 JSON 事件。
    Json { line: usize, message: String },
    /// 序列化失败（理论上不会发生）。
    Encode(String),
    /// 该内容无法写进会话日志（目前只有工具结果里的图片）。
    UnsupportedContent(&'static str),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => write!(formatter, "会话日志缺少 header（第一行必须是 header）"),
            Self::DuplicateHeader => write!(formatter, "会话日志里出现了多个 header"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "会话日志版本 {found} 不受支持（当前支持 {supported}）"
            ),
            Self::Json { line, message } => {
                write!(formatter, "会话日志第 {line} 行不是合法事件：{message}")
            }
            Self::Encode(message) => write!(formatter, "会话事件序列化失败：{message}"),
            Self::UnsupportedContent(what) => {
                write!(formatter, "会话日志无法记录：{what}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(system: &str) -> Header {
        Header::new("fake", "test", "/tmp", system)
    }

    #[test]
    fn user_turn_roundtrips_through_jsonl() {
        let mut session = Session::new(header("sys")).unwrap();
        session.push_user_text("你好");
        session.push_assistant(Turn {
            text: "在".to_owned(),
            stop: Some(StopReason::EndTurn),
            usage: Some(TokenUsage {
                input_tokens: 3,
                output_tokens: 1,
            }),
            ..Turn::default()
        });

        let jsonl = session
            .events()
            .iter()
            .map(|event| event.to_json_line().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let restored = Session::from_jsonl(&jsonl).unwrap();

        assert_eq!(restored.header(), session.header());
        assert_eq!(restored.events(), session.events());
        assert_eq!(restored.messages().len(), 3);
        assert_eq!(restored.messages()[0].role(), Role::System);
        assert_eq!(restored.messages()[1].role(), Role::User);
        assert_eq!(restored.messages()[2].role(), Role::Assistant);
    }

    #[test]
    fn tool_calls_and_results_keep_their_order_and_grouping() {
        let mut session = Session::new(header("")).unwrap();
        session.push_user_text("查一下");
        session.push_assistant(Turn {
            reasoning: Some("想".to_owned()),
            signature: Some("sig".to_owned()),
            tool_calls: vec![
                ToolCall {
                    id: "a".to_owned(),
                    name: "read".to_owned(),
                    input: RecordedInput::Json(serde_json::json!({ "path": "a.rs" })),
                },
                ToolCall {
                    id: "b".to_owned(),
                    name: "read".to_owned(),
                    input: RecordedInput::Text("not json".to_owned()),
                },
            ],
            ..Turn::default()
        });
        session.push_tool_results(vec![
            ToolResultRecord {
                call_id: "a".to_owned(),
                content: vec!["file a".to_owned()],
                is_error: false,
            },
            ToolResultRecord {
                call_id: "b".to_owned(),
                content: vec!["boom".to_owned()],
                is_error: true,
            },
        ]);

        let jsonl = session
            .events()
            .iter()
            .map(|event| event.to_json_line().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let restored = Session::from_jsonl(&jsonl).unwrap();

        // user → assistant(思考 + 2 个调用) → user(2 条结果合并成一条消息)
        assert_eq!(restored.messages().len(), 3);
        let assistant = &restored.messages()[1];
        assert_eq!(assistant.role(), Role::Assistant);
        assert!(matches!(
            &assistant.content()[0],
            ContentPart::Reasoning { text, signature }
                if &**text == "想" && signature.as_deref() == Some("sig")
        ));
        assert!(matches!(
            assistant.content()[1],
            ContentPart::ToolCall { ref id, .. } if &**id == "a"
        ));
        assert!(matches!(
            assistant.content()[2],
            ContentPart::ToolCall { ref id, .. } if &**id == "b"
        ));

        let results = &restored.messages()[2];
        assert_eq!(results.role(), Role::User);
        assert_eq!(results.content().len(), 2);
        assert!(matches!(
            results.content()[1],
            ContentPart::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn empty_turn_is_not_recorded() {
        let mut session = Session::new(header("")).unwrap();
        session.push_assistant(Turn::default());
        assert_eq!(session.event_count(), 1); // 只有 header
        assert_eq!(session.message_count(), 0);
    }

    #[test]
    fn wrong_version_is_rejected_explicitly() {
        let mut header = header("");
        header.version = VERSION + 1;
        let error = Session::new(header).unwrap_err();
        assert_eq!(
            error,
            SessionError::UnsupportedVersion {
                found: VERSION + 1,
                supported: VERSION
            }
        );
    }

    #[test]
    fn missing_or_duplicate_header_is_rejected() {
        assert_eq!(
            Session::from_events(vec![]).unwrap_err(),
            SessionError::MissingHeader
        );
        assert_eq!(
            Session::from_events(vec![SessionEvent::User {
                text: "hi".to_owned()
            }])
            .unwrap_err(),
            SessionError::MissingHeader
        );

        let events = vec![
            SessionEvent::Header(header("")),
            SessionEvent::Header(header("")),
        ];
        assert_eq!(
            Session::from_events(events).unwrap_err(),
            SessionError::DuplicateHeader
        );
    }

    #[test]
    fn image_tool_result_is_an_explicit_error() {
        let image = ToolResultPart::Image(Arc::new(aa_language_model::request::message::Image {
            mime: Arc::from("image/png"),
            source: aa_language_model::request::message::ImageSource::Bytes(Arc::from(&b"png"[..])),
        }));
        let error = ToolResultRecord::from_parts("a", &[image], false).unwrap_err();
        assert_eq!(error, SessionError::UnsupportedContent("tool result image"));
    }

    #[test]
    fn compaction_replaces_the_live_history_but_not_the_log() {
        let mut session = Session::new(header("sys")).unwrap();
        session.push_user_text("first");
        session.push_assistant(Turn {
            text: "a1".to_owned(),
            ..Turn::default()
        });
        session.push_user_text("second");

        let first_kept_message = session.message_count() - 1;
        session.push_compaction(CompactionRecord {
            summary: "summary".to_owned(),
            retained_users: vec!["first".to_owned()],
            first_kept_message,
            tokens_before: 123,
        });

        // 生效历史被替换；完整审计仍在 events 里。
        assert_eq!(session.message_count(), 4);
        assert_eq!(session.compactions().len(), 1);

        let view = session.messages();
        // system → 保留的用户消息 → 摘要 → 压缩点之后的待回复消息。
        assert_eq!(view.len(), 4);
        assert_eq!(view[0].role(), Role::System);
        assert!(matches!(
            &view[1].content()[0],
            ContentPart::Text(text) if &**text == "first"
        ));
        assert!(matches!(
            &view[2].content()[0],
            ContentPart::Text(text)
                if text.contains("The previous conversation was compacted")
                    && text.contains("summary")
        ));
        assert!(matches!(
            &view[3].content()[0],
            ContentPart::Text(text) if &**text == "second"
        ));

        // 重放后生效历史一致，原始事件一条不少。
        let jsonl = session
            .events()
            .iter()
            .map(|event| event.to_json_line().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let restored = Session::from_jsonl(&jsonl).unwrap();
        assert_eq!(restored.message_count(), 4);
        assert_eq!(restored.compactions(), session.compactions());
        assert_eq!(restored.messages(), session.messages());
    }

    #[test]
    fn retained_user_texts_respect_the_byte_budget() {
        let mut session = Session::new(header("")).unwrap();
        session.push_user_text("old");
        session.push_assistant(Turn {
            text: "a".to_owned(),
            ..Turn::default()
        });
        session.push_tool_results(vec![ToolResultRecord {
            call_id: "c".to_owned(),
            content: vec!["tool output".to_owned()],
            is_error: false,
        }]);
        session.push_user_text("new");

        // 工具结果不是「用户原话」，不参与保留。
        let retained = retained_user_texts(session.messages(), session.message_count(), None);
        assert_eq!(retained, vec!["old".to_owned(), "new".to_owned()]);
    }

    #[test]
    fn the_previous_summary_is_not_retained_as_user_wording() {
        let mut session = Session::new(header("")).unwrap();
        session.push_user_text("first");
        session.push_assistant(Turn {
            text: "a1".to_owned(),
            ..Turn::default()
        });
        session.push_user_text("second");
        session.push_compaction(CompactionRecord {
            summary: "summary one".to_owned(),
            retained_users: vec!["first".to_owned()],
            first_kept_message: 2,
            tokens_before: 100,
        });

        session.push_assistant(Turn {
            text: "a2".to_owned(),
            ..Turn::default()
        });
        session.push_user_text("third");

        let retained = retained_user_texts(
            session.messages(),
            session.message_count() - 1,
            session.summary_message(),
        );
        assert_eq!(retained, vec!["first".to_owned(), "second".to_owned()]);
    }
}
