//! OpenRouter chat completions 的**响应** wire 形状：非流式 body、SSE chunk、
//! 错误 body，以及把两者翻成 canonical `CompletionEvent` 的映射层。
//!
//! 与 `aa_ai_openai` 的响应侧差异：
//! - 思考字段是 `reasoning`（文档化）与 `reasoning_content`（别名），另外还有
//!   结构化的 `reasoning_details`；
//! - `usage` 始终返回，并带 `cost` / `cost_details` / `is_byok` 等扩展字段
//!   （canonical `TokenUsage` 只有 input/output，扩展字段不解析）；
//! - 错误体是 `{"error":{"code":<number>,"message","metadata"}}`，`code` 是数字
//!   而不是 OpenAI 的字符串；
//! - `finish_reason` 多一个 `"error"`（映射规则见 [`stop_reason`]）。
//!
//! **思考回放**：`reasoning_details` 是结构化数组，模型在工具调用后要求原样回放。
//! canonical `CompletionEvent` 没有专门的 details 变体，本 crate 把它序列化成
//! JSON 放进 `CompletionEvent::Reasoning.signature`（见 ADR 0012 D2）；
//! 纯文本 `reasoning` 照常产出 `Reasoning` 事件。

use std::collections::BTreeMap;

use serde::Deserialize;

/// 非流式响应 body。只取用得到的字段；`id` / `object` / `created` 不参与语义。
#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// OpenRouter 始终返回 usage；仍然用 `Option` 兼容代理裁剪过的 body。
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 生成过程中出错但 HTTP 仍是 200 的 body 形状：`{"error": {...}}`，没有 `choices`。
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub message: ResponseMessage,
    /// `stop` / `length` / `tool_calls` / `content_filter` / `error`。
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// 非流式生成中途出错时，错误会挂在 choice 上（可能已带部分内容）。
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ResponseToolCall>,
    /// 纯文本思考（OpenRouter 文档字段）。
    #[serde(default)]
    pub reasoning: Option<String>,
    /// 同一字段的别名（OpenAI 兼容端点的叫法，OpenRouter 接受并归一化）。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 结构化思考块：`reasoning.text` / `reasoning.encrypted` / `reasoning.summary`。
    #[serde(default)]
    pub reasoning_details: Option<serde_json::Value>,
}

impl ResponseMessage {
    /// 纯文本思考：`reasoning` → `reasoning_content`。
    pub fn reasoning(&self) -> Option<&str> {
        [self.reasoning.as_deref(), self.reasoning_content.as_deref()]
            .into_iter()
            .flatten()
            .find(|text| !text.is_empty())
    }
}

#[derive(Debug, Deserialize)]
pub struct ResponseToolCall {
    #[serde(default)]
    pub id: String,
    pub function: ResponseFunction,
}

#[derive(Debug, Deserialize)]
pub struct ResponseFunction {
    pub name: String,
    /// JSON 文本。模型有时给空串或半截 JSON，解析失败按纯文本处理。
    #[serde(default)]
    pub arguments: String,
}

/// token 用量。OpenRouter 的 `cost` / `cost_details` / `is_byok` 不映射：
/// canonical `TokenUsage` 只表达 input/output。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

impl Usage {
    pub fn to_canonical(self) -> aa_language_model::provider::TokenUsage {
        aa_language_model::provider::TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
        }
    }
}

// ---------- 流式 chunk ----------

/// SSE 的一帧 `data: {...}`。
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// OpenRouter 在最后一帧带完整 usage。
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 流内错误（HTTP 仍是 200）：`{"error":{"code":429,"message":...}}`。
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<DeltaToolCall>,
    /// 纯文本思考增量。
    #[serde(default)]
    pub reasoning: Option<String>,
    /// 纯文本思考的别名。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 结构化思考块增量（按 `index` / `id` 归并，`text` / `summary` / `data` 拼接）。
    #[serde(default)]
    pub reasoning_details: Option<serde_json::Value>,
    /// 拒绝文本；非空表示这次响应是 refusal（即使 `finish_reason` 是 `stop`）。
    #[serde(default)]
    pub refusal: Option<String>,
}

impl Delta {
    /// 本帧的纯文本思考增量：`reasoning` → `reasoning_content`。
    ///
    /// 网关有时把同一段思考同时塞进两个字段；只取第一个非空值，避免重复显示。
    pub fn reasoning(&self) -> Option<&str> {
        [self.reasoning.as_deref(), self.reasoning_content.as_deref()]
            .into_iter()
            .flatten()
            .find(|text| !text.is_empty())
    }
}

/// 流式 tool call 的一个分片：`id`/`name` 只在第一片出现，`arguments` 逐片累加。
#[derive(Debug, Deserialize)]
pub struct DeltaToolCall {
    /// 同一次响应里按 index 归并（并行调用会有多个 index 交错到达）。
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---------- 错误 body ----------

/// 错误信封：非 2xx 的 body 与流内 `{"error": ...}` 共用一个形状。
#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiError,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    /// OpenRouter 的 code 是数字（OpenAI 是字符串）。
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: String,
    /// 结构化元数据：`error_type` / `provider_code` 等。
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

impl ApiError {
    /// 供 `LlmError::ProviderRejection.message` 用的人读文本。
    pub fn detail(&self) -> String {
        let mut out = if self.message.is_empty() {
            "OpenRouter returned an error without a message".to_owned()
        } else {
            self.message.clone()
        };
        if let Some(code) = self.code {
            out.push_str(&format!(" (code: {code})"));
        }
        if let Some(error_type) = self.error_type() {
            out.push_str(&format!(" [{error_type}]"));
        }
        // 上游 provider 的限流/失败把真正原因放在 `metadata.raw`（`message` 只是
        // 通用的 "Provider returned error"）。不拼进来，用户只能看到 429。
        if let Some(raw) = self.metadata_field("raw")
            && raw != self.message
        {
            out.push_str(" — ");
            out.push_str(raw);
        }
        out
    }

    /// 从 `metadata` 里取一个字符串字段（`error_type` / `raw` / `limit_source` …）。
    fn metadata_field(&self, key: &str) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.get(key))
            .and_then(serde_json::Value::as_str)
    }

    /// `metadata.error_type`（OpenRouter 的稳定错误分类）。
    pub fn error_type(&self) -> Option<&str> {
        self.metadata_field("error_type")
    }

    /// 是否值得重试：限流、上游过载/不可用、超时。
    ///
    /// 流内错误拿不到 HTTP status，只能看 `code`（OpenRouter 把它填成 HTTP 状态码）
    /// 与 `error_type`；`Retry-After` header 在流内不可见，退避留给应用层。
    pub fn transient(&self) -> bool {
        self.code
            .is_some_and(|code| code == 408 || code == 429 || (500..=599).contains(&code))
            || matches!(
                self.error_type(),
                Some(
                    "rate_limit_exceeded"
                        | "provider_overloaded"
                        | "provider_unavailable"
                        | "timeout"
                )
            )
    }

    /// 转成 canonical 错误。HTTP 200 + 体内错误 → `status: 200`（不是 HTTP 层拒绝）。
    pub fn into_rejection(self) -> aa_language_model::provider::LlmError {
        aa_language_model::provider::LlmError::ProviderRejection {
            status: 200,
            message: self.detail(),
            transient: self.transient(),
        }
    }
}

/// 从错误 body 里抽 `error.message`；不是该形状就原样返回。
pub fn error_message(raw: &str) -> String {
    serde_json::from_str::<ApiErrorBody>(raw)
        .map(|body| body.error.detail())
        .unwrap_or_else(|_| raw.to_owned())
}

// ---------- finish_reason → StopReason ----------

/// OpenRouter `finish_reason` → canonical `StopReason`。
///
/// `"error"` 不在 OpenAI 的枚举里，也不足以判断是截断还是拒绝；按 ADR 0007 的
/// 「未知值宁可少报」处理成 `EndTurn`（真正的错误通常以流内 `{"error":...}` 到达，
/// 那会变成 `LlmError::ProviderRejection`，不会走到这里）。
pub fn stop_reason(finish_reason: Option<&str>) -> Option<aa_language_model::provider::StopReason> {
    use aa_language_model::provider::StopReason;
    Some(match finish_reason? {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    })
}

/// 非流式：`finish_reason` 优先，缺失时回落到「有没有 tool_calls」。
pub fn final_stop_reason(
    finish_reason: Option<&str>,
    has_tool_calls: bool,
) -> aa_language_model::provider::StopReason {
    use aa_language_model::provider::StopReason;
    match stop_reason(finish_reason) {
        Some(StopReason::EndTurn) if has_tool_calls => StopReason::ToolUse,
        Some(reason) => reason,
        None if has_tool_calls => StopReason::ToolUse,
        None => StopReason::EndTurn,
    }
}

// ---------- 结构化思考块 ----------

/// 从 `reasoning_details` 里提取可读文本。
///
/// `reasoning.text` 用 `text`，`reasoning.summary` 用 `summary`；加密块没有可读
/// 文本。非流式响应可能只给 details、不给 `message.reasoning`，用它补上思考显示。
pub fn reasoning_text(details: &serde_json::Value) -> String {
    let Some(items) = details.as_array() else {
        return String::new();
    };
    let mut text = String::new();
    for item in items {
        let field = match item.get("type").and_then(serde_json::Value::as_str) {
            Some("reasoning.text") => "text",
            Some("reasoning.summary") => "summary",
            _ => continue,
        };
        if let Some(fragment) = item.get(field).and_then(serde_json::Value::as_str) {
            text.push_str(fragment);
        }
    }
    text
}

/// `reasoning_details` 的增量累加器。
///
/// 语义照 Zed `language_model_core::chat_completion::ReasoningDetailsAccumulator`：
/// 数组元素按 `index`（其次 `id`）匹配；`text` / `summary` / `data` 是分片、拼接，
/// 其它非 null 字段覆盖。回放时必须是**原始顺序**的完整数组。
#[derive(Debug, Default)]
struct ReasoningDetails {
    accumulated: Option<serde_json::Value>,
}

impl ReasoningDetails {
    fn push(&mut self, chunk: &serde_json::Value) {
        match chunk {
            serde_json::Value::Null => {}
            serde_json::Value::Array(chunks) if chunks.is_empty() => {}
            serde_json::Value::Array(chunks) => {
                let mut details = match self.accumulated.take() {
                    Some(serde_json::Value::Array(details)) => details,
                    _ => Vec::new(),
                };
                for chunk in chunks {
                    merge_reasoning_detail(&mut details, chunk);
                }
                self.accumulated = Some(serde_json::Value::Array(details));
            }
            other => self.accumulated = Some(other.clone()),
        }
    }

    /// 序列化成回放用的 JSON 字符串（存进 `Reasoning.signature`）。
    fn signature(&self) -> Option<String> {
        let details = self.accumulated.as_ref()?;
        if details.as_array().is_some_and(Vec::is_empty) {
            return None;
        }
        serde_json::to_string(details).ok()
    }
}

fn merge_reasoning_detail(details: &mut Vec<serde_json::Value>, chunk: &serde_json::Value) {
    let index = chunk.get("index").and_then(serde_json::Value::as_u64);
    let target_index = index
        .and_then(|index| {
            details.iter().position(|detail| {
                detail.get("index").and_then(serde_json::Value::as_u64) == Some(index)
            })
        })
        .or_else(|| {
            let id = chunk.get("id").and_then(serde_json::Value::as_str)?;
            details
                .iter()
                .position(|detail| detail.get("id").and_then(serde_json::Value::as_str) == Some(id))
        });
    let Some(target_index) = target_index else {
        details.push(chunk.clone());
        return;
    };
    let (Some(target), Some(chunk)) = (details[target_index].as_object_mut(), chunk.as_object())
    else {
        return;
    };
    for (key, value) in chunk {
        if matches!(key.as_str(), "text" | "summary" | "data")
            && let Some(fragment) = value.as_str()
            && let Some(existing) = target.get(key).and_then(serde_json::Value::as_str)
        {
            target.insert(
                key.clone(),
                serde_json::Value::String(format!("{existing}{fragment}")),
            );
        } else if !value.is_null() {
            target.insert(key.clone(), value.clone());
        }
    }
}

// ---------- 流式解码 ----------

/// `text/event-stream` 的行缓冲：只做 SSE 分帧，不做 JSON 解析。
#[derive(Debug, Default)]
struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 取下一个 `data:` 负载；没有完整的数据行时返回 `None`。
    fn next_data(&mut self) -> Option<Vec<u8>> {
        loop {
            let pos = self.buf.iter().position(|&b| b == b'\n')?;
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            if let Some(rest) = line.strip_prefix(b"data:") {
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                return Some(rest.to_vec());
            }
            // 空行、`event:`、`id:`、`retry:`、`:` 注释都不携带负载。
        }
    }
}

/// 流式 tool call 的累加器（按 `index` 归并，输出按 index 升序）。
#[derive(Debug, Default)]
struct ToolCalls {
    calls: BTreeMap<u32, PendingToolCall>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCalls {
    fn absorb(&mut self, delta: &DeltaToolCall) {
        let call = self.calls.entry(delta.index).or_default();
        if let Some(id) = &delta.id
            && !id.is_empty()
        {
            call.id = id.clone();
        }
        if let Some(function) = &delta.function {
            if let Some(name) = &function.name
                && !name.is_empty()
            {
                call.name = name.clone();
            }
            if let Some(arguments) = &function.arguments {
                call.arguments.push_str(arguments);
            }
        }
    }

    fn drain(&mut self) -> Vec<aa_language_model::provider::CompletionEvent> {
        use aa_language_model::provider::CompletionEvent;
        std::mem::take(&mut self.calls)
            .into_values()
            .map(|call| CompletionEvent::ToolUse {
                id: call.id,
                name: call.name,
                // 参数是增量拼起来的 JSON 文本；解析失败保留原文，不吞掉模型输出。
                input: serde_json::from_str(&call.arguments)
                    .unwrap_or(serde_json::Value::String(call.arguments)),
                is_complete: true,
            })
            .collect()
    }
}

/// 流式解码器：`bytes` → `CompletionEvent`。
///
/// 独立于 HTTP 层，便于单测：喂字节、拿事件，不需要起服务器。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    sse: SseBuffer,
    calls: ToolCalls,
    details: ReasoningDetails,
    /// 已经发过 `Stop`，之后不再重复。
    stopped: bool,
    /// `finish_reason` 已到、但 `Stop` 要等 usage 帧之后再发。
    ///
    /// OpenRouter 的 usage 在 `finish_reason` 之后的独立帧里；如果立刻发 `Stop`，
    /// agent loop 会先收尾，usage 永远读不到。所以先记下原因，等 `[DONE]`
    /// （或流结束时）再吐 `Stop`，保证 `Usage` 在 `Stop` 之前。
    pending_stop: Option<aa_language_model::provider::StopReason>,
    /// 收到 `[DONE]` 或流内错误，之后不再产出事件。
    done: bool,
    /// 收到过非空 `refusal`：即使 `finish_reason` 是 `stop` 也算拒绝。
    refused: bool,
}

impl StreamDecoder {
    /// 喂一段网络字节，返回这段时间里解出的事件。
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Vec<
        Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>,
    > {
        self.sse.push(chunk);
        let mut out = Vec::new();
        while !self.done {
            let Some(data) = self.sse.next_data() else {
                break;
            };
            self.decode_one(&data, &mut out);
        }
        out
    }

    /// 流结束（没有 `[DONE]`）：
    ///
    /// - 已经收到 `finish_reason` → 补发 `Stop`（`finish_reason` 才是权威，
    ///   缺 `[DONE]` 不代表回复被截断）；
    /// - 否则没发过 `Stop` 说明流被截断，报 `StreamEnded`。
    pub fn finish(
        mut self,
    ) -> Vec<
        Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>,
    > {
        if self.stopped || self.done {
            return Vec::new();
        }
        if self.pending_stop.is_some() {
            let mut out = Vec::new();
            self.flush_stop(&mut out);
            return out;
        }
        self.stopped = true;
        vec![Err(aa_language_model::provider::LlmError::StreamEnded)]
    }

    fn decode_one(
        &mut self,
        data: &[u8],
        out: &mut Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) {
        if data == b"[DONE]" {
            if self.pending_stop.is_none() && !self.stopped {
                self.begin_finish(None, out);
            }
            self.flush_stop(out);
            self.done = true;
            return;
        }

        let chunk: StreamChunk = match serde_json::from_slice(data) {
            Ok(chunk) => chunk,
            Err(err) => {
                self.done = true;
                out.push(Err(aa_language_model::provider::LlmError::Transport(
                    format!("malformed SSE chunk: {err}"),
                )));
                return;
            }
        };

        if let Some(error) = &chunk.error {
            self.done = true;
            // 流内错误的 HTTP 已经是 200，错误在 body 里（`{"error":...}`，
            // 同时带一个 `finish_reason: "error"` 的 choice）。只报错误，不再发 Stop。
            out.push(Err(error.clone().into_rejection()));
            return;
        }

        for choice in &chunk.choices {
            if choice
                .delta
                .refusal
                .as_ref()
                .is_some_and(|text| !text.is_empty())
            {
                self.refused = true;
            }
            // 思考增量先于文本（与 OpenAI 兼容端点的顺序一致）。
            if let Some(reasoning) = choice.delta.reasoning() {
                out.push(Ok(
                    aa_language_model::provider::CompletionEvent::Reasoning {
                        text: reasoning.to_owned(),
                        signature: None,
                    },
                ));
            }
            if let Some(details) = &choice.delta.reasoning_details {
                self.details.push(details);
            }
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                out.push(Ok(aa_language_model::provider::CompletionEvent::Text(
                    content.clone(),
                )));
            }
            for call in &choice.delta.tool_calls {
                self.calls.absorb(call);
            }
            if let Some(finish_reason) = choice.finish_reason.as_deref() {
                self.begin_finish(Some(finish_reason), out);
            }
        }

        if let Some(usage) = chunk.usage {
            out.push(Ok(aa_language_model::provider::CompletionEvent::Usage(
                usage.to_canonical(),
            )));
        }
    }

    /// 收尾前半段：先回放结构化思考，再吐工具调用，最后**记下** `Stop` 原因。
    ///
    /// `Stop` 本身由 [`Self::flush_stop`] 在 usage 帧之后（或 `[DONE]` / 流结束时）
    /// 发出，保证 `Usage` 排在 `Stop` 前面。
    fn begin_finish(
        &mut self,
        finish_reason: Option<&str>,
        out: &mut Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) {
        use aa_language_model::provider::CompletionEvent;

        if self.stopped || self.pending_stop.is_some() {
            return;
        }

        if let Some(signature) = self.details.signature() {
            // 空 text + signature：agent loop 会把 signature 并进这条 assistant 消息的
            // Reasoning part，回放时再变回 `reasoning_details`。
            out.push(Ok(CompletionEvent::Reasoning {
                text: String::new(),
                signature: Some(signature),
            }));
        }

        let calls = self.calls.drain();
        let has_calls = !calls.is_empty();
        out.extend(calls.into_iter().map(Ok));

        let reason = match finish_reason {
            Some("length") => aa_language_model::provider::StopReason::MaxTokens,
            Some("content_filter") => aa_language_model::provider::StopReason::Refusal,
            _ if self.refused => aa_language_model::provider::StopReason::Refusal,
            Some("tool_calls" | "function_call") => {
                aa_language_model::provider::StopReason::ToolUse
            }
            Some(_) => aa_language_model::provider::StopReason::EndTurn,
            None if has_calls => aa_language_model::provider::StopReason::ToolUse,
            None => aa_language_model::provider::StopReason::EndTurn,
        };
        self.pending_stop = Some(reason);
    }

    /// 收尾后半段：发出 `Stop`（只发一次）。
    fn flush_stop(
        &mut self,
        out: &mut Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) {
        if self.stopped {
            return;
        }
        let reason = self
            .pending_stop
            .take()
            .unwrap_or(aa_language_model::provider::StopReason::EndTurn);
        out.push(Ok(aa_language_model::provider::CompletionEvent::Stop(
            reason,
        )));
        self.stopped = true;
    }
}

// ---------- 非流式 body → 事件 ----------

/// 非流式 body → 事件列表（正常时最后一项是 `Stop`）。
///
/// OpenRouter 的「生成中途失败」有两种 200 body（见官方 Errors 文档）：
/// - 只有顶层 `{"error": {...}}`、没有 `choices`：直接报错；
/// - 错误挂在 `choices[0].error` 上，可能已带部分内容：先把内容吐出来，
///   最后报错（不补 `Stop`——这次响应并没有正常结束）。
pub fn non_streaming_events(
    parsed: ChatCompletionResponse,
) -> Vec<Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>>
{
    use aa_language_model::provider::CompletionEvent;

    let mut events = Vec::new();

    // 顶层错误：没有任何内容可交付。
    if let Some(error) = parsed.error {
        events.push(Err(error.into_rejection()));
        return events;
    }

    let mut has_tool_calls = false;
    let mut finish_reason = None;
    let mut choice_error = None;

    if let Some(choice) = parsed.choices.first() {
        finish_reason = choice.finish_reason.as_deref();
        choice_error = choice.error.clone();

        // 纯文本思考优先；只有 details 时从 details 里抽文本（加密块抽不到）。
        let mut reasoning = choice.message.reasoning().map(str::to_owned);
        if reasoning.is_none()
            && let Some(details) = &choice.message.reasoning_details
        {
            let text = reasoning_text(details);
            if !text.is_empty() {
                reasoning = Some(text);
            }
        }
        if let Some(reasoning) = reasoning {
            events.push(Ok(CompletionEvent::Reasoning {
                text: reasoning,
                signature: None,
            }));
        }
        if let Some(details) = &choice.message.reasoning_details
            && let Ok(signature) = serde_json::to_string(details)
        {
            events.push(Ok(CompletionEvent::Reasoning {
                text: String::new(),
                signature: Some(signature),
            }));
        }

        if let Some(text) = &choice.message.content
            && !text.is_empty()
        {
            events.push(Ok(CompletionEvent::Text(text.clone())));
        }
        for call in &choice.message.tool_calls {
            has_tool_calls = true;
            events.push(Ok(CompletionEvent::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: parse_arguments(&call.function.arguments),
                is_complete: true,
            }));
        }
    }

    // `usage` 放在 `Stop` 之前：上层看到 `Stop` 就可以收尾。
    if let Some(usage) = parsed.usage {
        events.push(Ok(CompletionEvent::Usage(usage.to_canonical())));
    }

    // 挂在 choice 上的错误：内容已经交付，最后报错而不是假装正常结束。
    if let Some(error) = choice_error {
        events.push(Err(error.into_rejection()));
        return events;
    }

    events.push(Ok(CompletionEvent::Stop(final_stop_reason(
        finish_reason,
        has_tool_calls,
    ))));
    events
}

/// OpenRouter 把参数当字符串回（`arguments: "{\"path\":\"a\"}"`）；解析失败保留原文。
fn parse_arguments(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_owned()))
}

#[cfg(test)]
mod tests {
    use aa_language_model::provider::{CompletionEvent, LlmError, StopReason};

    fn decode(payloads: &[&str]) -> Vec<Result<CompletionEvent, LlmError>> {
        let mut decoder = super::StreamDecoder::default();
        let mut out = Vec::new();
        for payload in payloads {
            out.extend(decoder.push(format!("data: {payload}\n\n").as_bytes()));
        }
        out.extend(decoder.finish());
        out
    }

    fn ok(events: Vec<Result<CompletionEvent, LlmError>>) -> Vec<CompletionEvent> {
        events.into_iter().map(|event| event.unwrap()).collect()
    }

    #[test]
    fn streams_text_then_usage_then_stop() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events,
            vec![
                CompletionEvent::Text("Hel".to_owned()),
                CompletionEvent::Text("lo".to_owned()),
                // usage 在 finish_reason 之后到；必须在 Stop 之前吐出来，否则
                // agent loop 收尾后就读不到了。
                CompletionEvent::Usage(aa_language_model::provider::TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                }),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn accumulates_reasoning_details_into_signature() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"reasoning":"think ","reasoning_details":[{"type":"reasoning.text","index":0,"text":"think "}]}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"more"}]},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events,
            vec![
                CompletionEvent::Reasoning {
                    text: "think ".to_owned(),
                    signature: None,
                },
                // 结构化块按 index 拼接，整体存进 signature 供下一轮回放。
                CompletionEvent::Reasoning {
                    text: String::new(),
                    signature: Some(
                        r#"[{"type":"reasoning.text","index":0,"text":"think more"}]"#.to_owned()
                    ),
                },
                CompletionEvent::Usage(aa_language_model::provider::TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn accumulates_tool_calls_by_index() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events,
            vec![
                CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({ "path": "a" }),
                    is_complete: true,
                },
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn in_stream_error_becomes_provider_rejection() {
        let events = decode(&[
            r#"{"error":{"code":429,"message":"rate limited","metadata":{"error_type":"rate_limit_exceeded"}}}"#,
        ]);

        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(LlmError::ProviderRejection {
                status,
                message,
                transient,
            }) => {
                assert_eq!(*status, 200);
                assert!(message.contains("rate limited"));
                assert!(message.contains("(code: 429)"));
                assert!(message.contains("rate_limit_exceeded"));
                assert!(*transient);
            }
            other => panic!("expected ProviderRejection, got {other:?}"),
        }
    }

    #[test]
    fn upstream_error_detail_includes_metadata_raw() {
        // 真实的免费模型限流响应：`message` 只是通用的 "Provider returned error"，
        // 具体原因（哪个上游、为什么）在 `metadata.raw` 里。
        let events = decode(&[
            r#"{"error":{"code":429,"message":"Provider returned error","metadata":{"raw":"google/gemma-4-31b-it:free is temporarily rate-limited upstream","provider_name":"Google AI Studio","limit_source":"upstream_provider_shared_pool"}}}"#,
        ]);

        match &events[0] {
            Err(LlmError::ProviderRejection { message, .. }) => {
                assert!(message.contains("Provider returned error"), "{message}");
                assert!(
                    message.contains("temporarily rate-limited upstream"),
                    "{message}"
                );
            }
            other => panic!("expected ProviderRejection, got {other:?}"),
        }
    }

    #[test]
    fn truncated_stream_reports_stream_ended() {
        let events = decode(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#]);

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].as_ref().unwrap(),
            &CompletionEvent::Text("partial".to_owned())
        );
        assert!(matches!(events[1], Err(LlmError::StreamEnded)));
    }

    #[test]
    fn finish_reason_without_done_still_emits_stop() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
        ]));

        assert_eq!(
            events,
            vec![
                CompletionEvent::Text("done".to_owned()),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn done_without_finish_reason_uses_tool_calls_as_evidence() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events,
            vec![
                CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({}),
                    is_complete: true,
                },
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn length_wins_over_tool_calls() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"length"}]}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::MaxTokens))
        );
    }

    #[test]
    fn refusal_text_overrides_stop_finish_reason() {
        let events = ok(decode(&[
            r#"{"choices":[{"delta":{"refusal":"I cannot help"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]));

        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::Refusal))
        );
    }

    #[test]
    fn non_streaming_maps_reasoning_and_tool_calls() {
        let parsed: super::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "reasoning": "let me read",
                    "reasoning_details": [
                        {"type": "reasoning.text", "index": 0, "text": "let me read"}
                    ],
                    "content": "reading",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 6, "cost": 0.0}
        }))
        .unwrap();

        let events = ok(super::non_streaming_events(parsed));
        assert_eq!(
            events,
            vec![
                CompletionEvent::Reasoning {
                    text: "let me read".to_owned(),
                    signature: None,
                },
                CompletionEvent::Reasoning {
                    text: String::new(),
                    signature: Some(
                        r#"[{"type":"reasoning.text","index":0,"text":"let me read"}]"#.to_owned()
                    ),
                },
                CompletionEvent::Text("reading".to_owned()),
                CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({ "path": "a" }),
                    is_complete: true,
                },
                CompletionEvent::Usage(aa_language_model::provider::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 6,
                }),
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn non_streaming_extracts_text_from_details_when_reasoning_is_absent() {
        let parsed: super::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "ok",
                    "reasoning_details": [
                        {"type": "reasoning.summary", "index": 0, "summary": "short summary"}
                    ]
                }
            }]
        }))
        .unwrap();

        let events = ok(super::non_streaming_events(parsed));
        assert_eq!(
            events[0],
            CompletionEvent::Reasoning {
                text: "short summary".to_owned(),
                signature: None,
            }
        );
    }

    #[test]
    fn non_streaming_top_level_error_is_a_rejection() {
        // HTTP 200 + `{"error": ...}`，没有 choices（生成中途失败）。
        let parsed: super::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "error": {
                "code": 502,
                "message": "Provider disconnected mid-stream",
                "metadata": {"error_type": "provider_unavailable"}
            }
        }))
        .unwrap();

        let events = super::non_streaming_events(parsed);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(LlmError::ProviderRejection {
                status,
                message,
                transient,
            }) => {
                assert_eq!(*status, 200);
                assert!(message.contains("disconnected"), "{message}");
                assert!(message.contains("provider_unavailable"), "{message}");
                assert!(*transient);
            }
            other => panic!("expected ProviderRejection, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_choice_error_keeps_partial_content() {
        // 错误挂在 choice 上，同时已经流出了部分内容。
        let parsed: super::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "choices": [{
                "finish_reason": "error",
                "message": {"content": "partial output"},
                "error": {"code": 502, "message": "Provider disconnected"}
            }]
        }))
        .unwrap();

        let events = super::non_streaming_events(parsed);
        assert_eq!(
            events[0].as_ref().unwrap(),
            &CompletionEvent::Text("partial output".to_owned())
        );
        // 不补 Stop：这次响应没有正常结束。
        assert!(matches!(
            events.last(),
            Some(Err(LlmError::ProviderRejection { .. }))
        ));
    }

    #[test]
    fn error_message_parses_numeric_code() {
        let message =
            super::error_message(r#"{"error":{"code":402,"message":"Insufficient credits"}}"#);
        assert_eq!(message, "Insufficient credits (code: 402)");
    }

    #[test]
    fn error_message_falls_back_to_raw_body() {
        assert_eq!(super::error_message("not json"), "not json");
    }
}
