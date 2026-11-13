//! 离线 stub provider：不连真 LLM，按规则回固定/可编排的 `aa_language_model::provider::CompletionEvent`。
//!
//! 它不是 wire provider（没有任何厂商格式），只是 `aa_language_model::provider::LanguageModelClient` 的确定性替身，
//! 所以放在汇总 crate 而不是 `aa_ai_*` 里——和 Zed 把 `FakeLanguageModelProvider`
//! 放在 `language_model` 而不是 `language_model_core` 是同一个理由。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use futures::StreamExt;
use futures::stream;

/// 一个预编好的「剧本」：每次 `stream_completion` 消费一轮事件。
///
/// 多轮剧本用于测 agent loop：第一轮要先返回 `ToolUse`，第二轮才返回文本 + `Stop`。
/// 剧本耗尽后回 `Stop(EndTurn)`（避免调用方卡在空流上）。
pub struct StubClient {
    rounds: Mutex<VecDeque<Vec<aa_language_model::provider::CompletionEvent>>>,
}

impl StubClient {
    /// 单轮剧本。
    pub fn new(script: Vec<aa_language_model::provider::CompletionEvent>) -> Self {
        Self::with_rounds([script])
    }

    /// 多轮剧本：每个元素是一轮 `stream_completion` 的事件序列。
    pub fn with_rounds(
        rounds: impl IntoIterator<Item = Vec<aa_language_model::provider::CompletionEvent>>,
    ) -> Self {
        Self {
            rounds: Mutex::new(rounds.into_iter().collect()),
        }
    }

    /// 回一段固定文本。
    pub fn text(msg: impl Into<String>) -> Self {
        Self::new(vec![
            aa_language_model::provider::CompletionEvent::Text(msg.into()),
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::EndTurn,
            ),
        ])
    }

    /// 回「LLM 要调一个工具」（agent loop 会去执行并回填）。
    pub fn tool_use(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::new(vec![
            aa_language_model::provider::CompletionEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
                is_complete: true,
            },
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::ToolUse,
            ),
        ])
    }

    fn next_round(&self) -> Vec<aa_language_model::provider::CompletionEvent> {
        self.rounds.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::EndTurn,
            )]
        })
    }
}

impl aa_language_model::provider::LanguageModelClient for StubClient {
    fn stream_completion(
        &self,
        _model: &aa_language_model::provider::Model,
        _request: &aa_language_model::request::Request,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        aa_language_model::provider::EventStream,
                        aa_language_model::provider::LlmError,
                    >,
                > + Send
                + 'static,
        >,
    > {
        // 同步取好本轮事件，future 不借用 self（0006 D2）。
        let events = self.next_round();
        Box::pin(async move { Ok(stream::iter(events.into_iter().map(Ok)).boxed()) })
    }
}
