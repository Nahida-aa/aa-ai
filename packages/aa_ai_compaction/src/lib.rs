//! `aa_ai_compaction` — 上下文压缩策略。
//!
//! 输入是 [`aa_ai_persistence::SessionLog`]（唯一事实源），输出是一条
//! [`CompactionRecord`] 事件写回日志。本 crate 不持有会话、不跑 agent loop。

use std::fmt;
use std::sync::Arc;

use futures::StreamExt;

use aa_ai_core::{CancelReason, CancelToken};
use aa_ai_persistence::{CompactionRecord, SessionLog, retained_user_texts};
use aa_language_model::provider::{self, LanguageModel, LanguageModelCompletionEvent};
use aa_language_model::request::message::{ContentPart, Message, Role};
use aa_language_model::request::tools;

/// 摘要指令。结构对齐我们现有的 checkpoint 字段；合并旧摘要的规则来自
/// dsh / TanStack / pi / opencode 的做法（见 `docs/compare/context-compaction.md`）。
pub const COMPACTION_PROMPT: &str = "\
You are compacting the conversation above into a handoff summary for another agent that will resume the work.

Include:
- Goal: what the user is ultimately trying to achieve
- Requirements: constraints, preferences, and scope boundaries stated by the user
- Decisions: decisions already made and why
- Work State: what is completed, active, and blocked
- Next Move: the specific steps that remain, in order
- Relevant Files: paths another agent would need to open, with a one-line reason
- Important Context: facts that cannot be recovered from the code or files

Rules:
- Use the exact section headings below. Use terse single-line bullets, not prose.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user requests that remain unresolved.
- If the conversation already contains a previous checkpoint summary, do NOT copy it verbatim:
  keep facts that are still true, drop stale ones, and merge newer information into
  one consolidated summary under the same headings.
- Do not continue the task or call tools.
- Do not mention the summary process or that context was compacted.

## Goal
## Requirements
## Decisions
## Work State
### Completed
### Active
### Blocked
## Next Move
## Relevant Files
## Important Context";

const IMAGE_TOKENS: u64 = 1_500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Threshold {
    Fraction(f64),
    TokensUsed(u64),
    TokensRemaining(u64),
}

impl Threshold {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if let Some(percent) = trimmed.strip_suffix('%') {
            let value: f64 = percent
                .trim_end()
                .parse()
                .map_err(|_| format!("invalid threshold {raw:?}"))?;
            if !(value > 0.0 && value <= 100.0) {
                return Err(format!("threshold out of range: {raw:?}"));
            }
            return Ok(Self::Fraction(value / 100.0));
        }
        if let Some(remaining) = trimmed.strip_prefix('-') {
            let value: u64 = remaining
                .trim()
                .parse()
                .map_err(|_| format!("invalid threshold {raw:?}"))?;
            return Ok(Self::TokensRemaining(value));
        }
        let value: u64 = trimmed
            .parse()
            .map_err(|_| format!("invalid threshold {raw:?}"))?;
        if value == 0 {
            return Err("threshold of 0 is invalid".to_owned());
        }
        Ok(Self::TokensUsed(value))
    }

    fn tokens(self, capacity: u64) -> u64 {
        match self {
            Self::Fraction(fraction) => (capacity as f64 * fraction).ceil() as u64,
            Self::TokensUsed(tokens) => tokens,
            Self::TokensRemaining(tokens) => capacity.saturating_sub(tokens).saturating_add(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub threshold: Threshold,
    pub min_context_window: u64,
    pub summary_max_output_tokens: Option<u64>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: Threshold::Fraction(0.9),
            min_context_window: 80_000,
            summary_max_output_tokens: None,
        }
    }
}

/// 保守估算：ASCII 4 字符 / token，非 ASCII 1 字符 / token。
pub fn estimate_text(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + other
}

pub fn input_capacity(model: &LanguageModel) -> Option<u64> {
    let window = model.context_window?;
    Some(window.saturating_sub(model.max_output_tokens.unwrap_or(0)))
}

pub fn estimate_tokens(messages: &[Arc<Message>], tools: &[Arc<tools::Tool>]) -> u64 {
    let mut total = 0;
    for message in messages {
        for part in &message.content {
            total += match part {
                ContentPart::Text(text) => estimate_text(text),
                ContentPart::Reasoning { text, .. } => estimate_text(text),
                ContentPart::RedactedThinking(data) => estimate_text(data),
                ContentPart::ToolCall { name, input, .. } => {
                    estimate_text(name) + estimate_text(&input.to_wire_string())
                }
                ContentPart::ToolResult { content, .. } => content
                    .iter()
                    .map(|part| match part {
                        aa_language_model::request::message::ToolResultPart::Text(text) => {
                            estimate_text(text)
                        }
                        aa_language_model::request::message::ToolResultPart::Image(_) => {
                            IMAGE_TOKENS
                        }
                    })
                    .sum(),
                ContentPart::Image(_) => IMAGE_TOKENS,
                ContentPart::Opaque(_) => 0,
            };
        }
    }
    for tool in tools {
        if let Ok(json) = serde_json::to_string(tool.as_ref()) {
            total += estimate_text(&json);
        }
    }
    total
}

pub fn should_compact(
    model: &LanguageModel,
    messages: &[Arc<Message>],
    tools: &[Arc<tools::Tool>],
    config: &CompactionConfig,
) -> Option<u64> {
    let capacity = input_capacity(model)?;
    if !config.enabled || capacity < config.min_context_window {
        return None;
    }
    let tokens = estimate_tokens(messages, tools);
    (tokens >= config.threshold.tokens(capacity)).then_some(tokens)
}

#[derive(Debug)]
pub enum CompactionError {
    Llm(provider::LlmError),
    EmptySummary,
    Cancelled(CancelReason),
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => write!(formatter, "compaction failed: {error}"),
            Self::EmptySummary => write!(formatter, "compaction produced an empty summary"),
            Self::Cancelled(reason) => write!(formatter, "compaction cancelled ({reason})"),
        }
    }
}

impl std::error::Error for CompactionError {}

impl From<provider::LlmError> for CompactionError {
    fn from(error: provider::LlmError) -> Self {
        Self::Llm(error)
    }
}

/// 超预算时压缩旧对话；取消时不写任何事件，会话保持原样。
pub async fn maybe_compact(
    log: &mut SessionLog,
    llm: &dyn provider::LanguageModelClient,
    model: &LanguageModel,
    tools: &[Arc<tools::Tool>],
    config: &CompactionConfig,
    cancel: &CancelToken,
) -> Result<Option<CompactionRecord>, CompactionError> {
    cancel.check().map_err(CompactionError::Cancelled)?;

    let tokens_before = {
        let view = log.messages();
        match should_compact(model, &view, tools, config) {
            Some(tokens) => tokens,
            None => return Ok(None),
        }
    };

    let first_kept_message = match log.messages().last() {
        Some(message) if is_plain_user_text(message) => log.messages().len() - 1,
        _ => return Ok(None),
    };

    let view = log.messages();
    let retained_users = retained_user_texts(&view, first_kept_message, log.summary_message());

    let mut messages = view[..first_kept_message].to_vec();
    messages.push(Arc::new(Message::new(
        Role::User,
        vec![ContentPart::text(COMPACTION_PROMPT)],
    )));

    let request = aa_language_model::request::Request {
        messages,
        max_output_tokens: config.summary_max_output_tokens.or(model.max_output_tokens),
        stream: true,
        ..Default::default()
    };

    let mut stream = llm.stream_completion(model, &request).await?;
    let mut summary = String::new();
    let mut cancelled = None;
    loop {
        tokio::select! {
            biased;
            reason = cancel.cancelled() => { cancelled = Some(reason); break; }
            event = stream.next() => match event {
                Some(Ok(LanguageModelCompletionEvent::Text(delta))) => summary.push_str(&delta),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
        }
    }
    drop(stream);
    if let Some(reason) = cancelled {
        return Err(CompactionError::Cancelled(reason));
    }

    let summary = summary.trim();
    if summary.is_empty() {
        return Err(CompactionError::EmptySummary);
    }
    cancel.check().map_err(CompactionError::Cancelled)?;

    let record = CompactionRecord {
        summary: summary.to_owned(),
        retained_users,
        first_kept_message,
        tokens_before,
        model: Some(model.into()),
    };
    log.push_compaction(record.clone());
    Ok(Some(record))
}

fn is_plain_user_text(message: &Message) -> bool {
    message.role == Role::User
        && !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|part| matches!(part, ContentPart::Text(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thresholds() {
        assert_eq!(Threshold::parse("90%"), Ok(Threshold::Fraction(0.9)));
        assert_eq!(
            Threshold::parse("-20000"),
            Ok(Threshold::TokensRemaining(20_000))
        );
        assert!(Threshold::parse("0").is_err());
    }

    #[test]
    fn small_window_is_left_alone() {
        let model = LanguageModel {
            context_window: Some(40_000),
            ..LanguageModel::new("fake", "test")
        };
        let message = Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text("x".repeat(10_000))],
        ));
        assert_eq!(
            should_compact(
                &model,
                std::slice::from_ref(&message),
                &[],
                &CompactionConfig::default()
            ),
            None
        );
    }
}
