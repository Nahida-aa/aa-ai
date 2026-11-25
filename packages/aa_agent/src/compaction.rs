//! 上下文预算与压缩。
//!
//! 策略对齐 Zed（`crates/agent/src/thread.rs`）：
//!
//! - **预算**：`context_window - max_output_tokens`，再按阈值（默认 90%）触发；
//!   窗口太小（默认 < 80k）不做自动压缩——没有腾挪空间，摘要请求本身就会超窗。
//! - **保留**：摘要之外，最近 80KB 的**用户文本**原样保留（[`session::RETAINED_USER_BYTES`]）；
//!   工具输出、助手文本全部进摘要。
//! - **摘要请求**：当前生效历史（去掉待回复的那条用户消息）+ 一条压缩指令，
//!   不带工具；用同一个模型。
//! - **持久化**：压缩是会话日志里的一条 [`session::SessionEvent::Compaction`]，
//!   原始事件保留（审计），内存里的 [`session::Session::messages`] 只留摘要 + 保留段。
//!
//! 只在 [`AgentLoop`](crate::AgentLoop) 每轮开始、最后一条是待回复的用户文本时压缩。
//! 不在工具循环中间压缩：那会把 assistant 的 tool_call 和它的 tool_result 拆开。
//!
//! 为什么不用 tokenizer：库层不绑定具体厂商的分词器；`chars / 4` 对中文会低估
//! 约 4 倍，所以这里把非 ASCII 按 1 token/字算，宁可早压缩。

use std::fmt;
use std::sync::Arc;

use futures::StreamExt;

use aa_language_model::provider::{self, CompletionEvent, Model};
use aa_language_model::request::Request;
use aa_language_model::request::message::{ContentPart, Message, Role, ToolResultPart};
use aa_language_model::request::tools;

use crate::session::{self, CompactionRecord, Session};
use crate::tool::ToolEvents;

/// 摘要请求的指令。结构对齐 opencode 的 checkpoint 模板（长任务上验证过）。
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

/// 一张图片的 token 估算（对齐 opencode）。
const IMAGE_TOKENS: u64 = 1_500;

/// 触发条件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Threshold {
    /// 可用输入窗口用掉这个比例就压（默认 90%）。
    Fraction(f64),
    /// 用掉这么多 token 就压。
    TokensUsed(u64),
    /// 只剩这么多 token 就压。
    TokensRemaining(u64),
}

impl Threshold {
    /// 解析设置里的写法：`90%` / `100000` / `-20000`（对齐 Zed）。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if let Some(percent) = trimmed.strip_suffix('%') {
            let value: f64 = percent
                .trim_end()
                .parse()
                .map_err(|_| format!("invalid compaction threshold percentage {raw:?}"))?;
            if !(value > 0.0 && value <= 100.0) {
                return Err(format!(
                    "compaction threshold percentage must be between 0% and 100%, got {raw:?}"
                ));
            }
            return Ok(Self::Fraction(value / 100.0));
        }
        if let Some(remaining) = trimmed.strip_prefix('-') {
            let value: u64 = remaining
                .trim()
                .parse()
                .map_err(|_| format!("invalid compaction threshold {raw:?}"))?;
            return Ok(Self::TokensRemaining(value));
        }
        let value: u64 = trimmed
            .parse()
            .map_err(|_| format!("invalid compaction threshold {raw:?}"))?;
        if value == 0 {
            return Err("compaction threshold of 0 is not valid".to_owned());
        }
        Ok(Self::TokensUsed(value))
    }

    /// 在给定输入容量下的触发 token 数。
    fn tokens(self, capacity: u64) -> u64 {
        match self {
            Self::Fraction(fraction) => (capacity as f64 * fraction).ceil() as u64,
            Self::TokensUsed(tokens) => tokens,
            Self::TokensRemaining(tokens) => capacity.saturating_sub(tokens).saturating_add(1),
        }
    }
}

/// 压缩策略。默认值对齐 Zed。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub threshold: Threshold,
    /// 输入容量小于这个值就不自动压缩（Zed 写死 `80_000`）。
    pub min_context_window: u64,
    /// 摘要请求的输出上限；`None` 用模型的输出上限。
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

/// 可用的输入预算：窗口减去这次回复的输出上限（对齐 Zed `input_token_capacity`）。
pub fn input_capacity(model: &Model) -> Option<u64> {
    let window = model.context_window?;
    Some(window.saturating_sub(model.max_output_tokens.unwrap_or(0)))
}

/// 需要压缩时返回压缩前的估算 token 数。
pub fn should_compact(
    model: &Model,
    messages: &[Arc<Message>],
    tools: &[Arc<tools::Tool>],
    config: &CompactionConfig,
) -> Option<u64> {
    if !config.enabled {
        return None;
    }
    let capacity = input_capacity(model)?;
    if capacity < config.min_context_window {
        return None;
    }
    let tokens = estimate_tokens(messages, tools);
    (tokens >= config.threshold.tokens(capacity)).then_some(tokens)
}

/// 估算一组消息 + 工具定义的 token 数。
pub fn estimate_tokens(messages: &[Arc<Message>], tools: &[Arc<tools::Tool>]) -> u64 {
    let mut total = 0;
    for message in messages {
        total += message.content.iter().map(estimate_part).sum::<u64>();
    }
    for tool in tools {
        if let Ok(json) = serde_json::to_string(tool.as_ref()) {
            total += estimate_text(&json);
        }
    }
    total
}

fn estimate_part(part: &ContentPart) -> u64 {
    match part {
        ContentPart::Text(text) => estimate_text(text),
        ContentPart::Reasoning { text, .. } => estimate_text(text),
        ContentPart::RedactedThinking(data) => estimate_text(data),
        ContentPart::ToolCall { name, input, .. } => {
            estimate_text(name) + estimate_text(&input.to_wire_string())
        }
        ContentPart::ToolResult { content, .. } => content
            .iter()
            .map(|part| match part {
                ToolResultPart::Text(text) => estimate_text(text),
                ToolResultPart::Image(_) => IMAGE_TOKENS,
            })
            .sum(),
        ContentPart::Image(_) => IMAGE_TOKENS,
        // provider 私有内容无法估算；通常是短小的状态标记。
        ContentPart::Opaque(_) => 0,
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

/// 压缩失败。失败时历史保持不变，调用方可以把错误上抛给用户重试。
#[derive(Debug)]
pub enum CompactionError {
    Llm(provider::LlmError),
    /// 模型返回了空摘要。不静默接受：空摘要会把整段旧历史从请求里抹掉。
    EmptySummary,
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => write!(formatter, "compaction request failed: {error}"),
            Self::EmptySummary => write!(formatter, "compaction produced an empty summary"),
        }
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Llm(error) => Some(error),
            Self::EmptySummary => None,
        }
    }
}

impl From<provider::LlmError> for CompactionError {
    fn from(error: provider::LlmError) -> Self {
        Self::Llm(error)
    }
}

/// 超预算时压缩旧对话。返回 `None` 表示没压（未超预算 / 没有可压缩的内容）。
///
/// 只在最后一条消息是**待回复的用户文本**时动手：否则会把 assistant 的
/// `tool_call` 和它的 `tool_result` 拆开，下一轮模型会看到孤儿结果。
pub async fn maybe_compact(
    llm: &dyn provider::LanguageModelClient,
    model: &Model,
    session: &mut Session,
    tools: &[Arc<tools::Tool>],
    config: &CompactionConfig,
    events: &impl ToolEvents,
) -> Result<Option<CompactionRecord>, CompactionError> {
    let tokens_before = {
        let view = session.messages();
        let Some(tokens) = should_compact(model, view, tools, config) else {
            return Ok(None);
        };
        tokens
    };

    let Some(pending) = session.messages().last() else {
        return Ok(None);
    };
    if session::user_text(pending).is_none() {
        return Ok(None);
    }

    // 压缩点 = 待回复的那条用户消息；它之后的消息（正常情况下没有）保持原样。
    let first_kept_message = session.messages().len() - 1;
    let retained_users = session::retained_user_texts(
        session.messages(),
        first_kept_message,
        session.summary_message(),
    );

    let mut messages: Vec<Arc<Message>> = session.messages()[..first_kept_message].to_vec();
    messages.push(Arc::new(Message::new(
        Role::User,
        vec![ContentPart::text(COMPACTION_PROMPT)],
    )));

    events.progress(&format!(
        "[compact] 上下文约 {tokens_before} tokens，正在压缩旧对话…"
    ));

    let request = Request {
        messages,
        max_output_tokens: config.summary_max_output_tokens.or(model.max_output_tokens),
        // 我们消费的是事件流；provider 的 wire 形状由这个标志决定。
        stream: true,
        ..Default::default()
    };
    let mut stream = llm.stream_completion(model, &request).await?;
    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        if let CompletionEvent::Text(delta) = event? {
            summary.push_str(&delta);
        }
    }

    let summary = summary.trim();
    if summary.is_empty() {
        return Err(CompactionError::EmptySummary);
    }

    let record = CompactionRecord {
        summary: summary.to_owned(),
        retained_users,
        first_kept_message,
        tokens_before,
    };
    session.push_compaction(record.clone());
    events.progress(&format!(
        "[compact] 已压缩 {} 条消息",
        first_kept_message.saturating_sub(1)
    ));
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_ascii_and_cjk_differently() {
        assert_eq!(estimate_text("12345678"), 2);
        assert_eq!(estimate_text("你好"), 2);
        // 混排：8 个 ASCII + 2 个汉字。
        assert_eq!(estimate_text("12345678你好"), 2 + 2);
    }

    #[test]
    fn parses_thresholds_like_zed() {
        assert_eq!(Threshold::parse("90%"), Ok(Threshold::Fraction(0.9)));
        assert_eq!(Threshold::parse(" 92.5% "), Ok(Threshold::Fraction(0.925)));
        assert_eq!(
            Threshold::parse("100000"),
            Ok(Threshold::TokensUsed(100_000))
        );
        assert_eq!(
            Threshold::parse("-20000"),
            Ok(Threshold::TokensRemaining(20_000))
        );
        assert!(Threshold::parse("0").is_err());
        assert!(Threshold::parse("100.5%").is_err());
        assert!(Threshold::parse("abc").is_err());
    }

    #[test]
    fn threshold_modes_use_input_capacity() {
        assert_eq!(Threshold::Fraction(0.9).tokens(100_000), 90_000);
        assert_eq!(Threshold::TokensUsed(12_345).tokens(100_000), 12_345);
        assert_eq!(Threshold::TokensRemaining(20_000).tokens(100_000), 80_001);
    }

    #[test]
    fn input_capacity_reserves_the_output_budget() {
        let model = Model {
            context_window: Some(100_000),
            max_output_tokens: Some(20_000),
            ..Model::new("fake", "test")
        };
        assert_eq!(input_capacity(&model), Some(80_000));
        assert_eq!(input_capacity(&Model::new("fake", "test")), None);
    }

    #[test]
    fn small_windows_are_left_alone() {
        let big = Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text("x".repeat(200_000))],
        ));
        let model = Model {
            context_window: Some(40_000),
            ..Model::new("fake", "test")
        };
        // 默认 80k 下限：40k 窗口不自动压缩。
        assert_eq!(
            should_compact(
                &model,
                std::slice::from_ref(&big),
                &[],
                &CompactionConfig::default()
            ),
            None
        );
        // 显式放宽下限后才会触发。
        let config = CompactionConfig {
            min_context_window: 0,
            ..Default::default()
        };
        assert!(should_compact(&model, &[big], &[], &config).is_some());
    }

    #[test]
    fn disabled_never_compacts() {
        let big = Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text("x".repeat(200_000))],
        ));
        let model = Model {
            context_window: Some(100_000),
            ..Model::new("fake", "test")
        };
        let config = CompactionConfig {
            enabled: false,
            ..Default::default()
        };
        assert_eq!(should_compact(&model, &[big], &[], &config), None);
    }
}
