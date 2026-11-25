//! 离线 fake provider：不连真 LLM，按剧本回固定的 `provider::CompletionEvent`。
//!
//! 它不是 wire provider（没有任何厂商格式），只是 [`crate::provider::LanguageModelClient`]
//! 的确定性替身，所以和 trait 放在同一个 crate：它只依赖 IR + trait，不依赖任何
//! 厂商 DTO / HTTP。用 `fake` feature 门控，正常构建不会把测试替身带进二进制。
//!
//! 对照 Zed：`FakeLanguageModelProvider` 放在定义 trait 的 `language_model`、
//! 用 `test-support` feature 门控，而不是放进 `language_model_core`（纯 IR）。
//! 本仓把 IR 和 trait 合并在 `aa_language_model`，所以替身也在这里。
//!
//! 用法：`aa_language_model = { path = "...", features = ["fake"] }`；
//! 汇总 crate 通过 `aa_language_models::fake::FakeClient` 转出同一个类型。
//!
//! 命名：和 Zed 对齐叫 fake（CLI 标志也是 `--fake`）。按 Meszaros 的严格分类，
//! 这个实现更接近 scripted stub + spy——只回预置事件、记录请求，不模拟模型行为；
//! `Fake` 在这里表达的是「对调用方可用的 provider 替身」，不是「有完整行为的
//! in-memory provider」。如果以后加上 `expect_*` 之类的期望断言，它就变成 mock，
//! 那时名字要跟着改。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use futures::StreamExt;
use futures::stream;

/// 一个预编好的「剧本」：每次 `stream_completion` 消费一轮事件，并记录收到的请求。
///
/// 多轮剧本用于测 agent loop：第一轮要先返回 `ToolUse`，第二轮才返回文本 + `Stop`。
/// 剧本耗尽后回 `Stop(EndTurn)`（避免调用方卡在空流上）。
pub struct FakeClient {
    rounds: Mutex<VecDeque<Vec<crate::provider::CompletionEvent>>>,
    seen: Mutex<Vec<crate::request::Request>>,
}

impl FakeClient {
    /// 单轮剧本。
    pub fn new(script: Vec<crate::provider::CompletionEvent>) -> Self {
        Self::with_rounds([script])
    }

    /// 多轮剧本：每个元素是一轮 `stream_completion` 的事件序列。
    pub fn with_rounds(
        rounds: impl IntoIterator<Item = Vec<crate::provider::CompletionEvent>>,
    ) -> Self {
        Self {
            rounds: Mutex::new(rounds.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// 回一段固定文本。
    pub fn text(msg: impl Into<String>) -> Self {
        Self::new(vec![
            crate::provider::CompletionEvent::Text(msg.into()),
            crate::provider::CompletionEvent::Stop(crate::provider::StopReason::EndTurn),
        ])
    }

    /// 回「LLM 要调一个工具」（agent loop 会去执行并回填）。
    pub fn tool_use(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::new(vec![
            crate::provider::CompletionEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
                is_complete: true,
            },
            crate::provider::CompletionEvent::Stop(crate::provider::StopReason::ToolUse),
        ])
    }

    /// 按调用顺序返回收到的请求，用于断言历史回填。
    ///
    /// 返回克隆：`request::Request` 内部是 `Arc`，克隆不复制消息。
    pub fn requests(&self) -> Vec<crate::request::Request> {
        self.seen.lock().unwrap().clone()
    }

    /// 最后一次请求。
    pub fn last_request(&self) -> Option<crate::request::Request> {
        self.seen.lock().unwrap().last().cloned()
    }

    fn next_round(&self) -> Vec<crate::provider::CompletionEvent> {
        self.rounds.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![crate::provider::CompletionEvent::Stop(
                crate::provider::StopReason::EndTurn,
            )]
        })
    }
}

impl crate::provider::LanguageModelClient for FakeClient {
    fn stream_completion(
        &self,
        _model: &crate::provider::Model,
        request: &crate::request::Request,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::provider::EventStream, crate::provider::LlmError>>
                + Send
                + 'static,
        >,
    > {
        self.seen.lock().unwrap().push(request.clone());
        // 同步取好本轮事件，future 不借用 self（0006 D2）。
        let events = self.next_round();
        Box::pin(async move { Ok(stream::iter(events.into_iter().map(Ok)).boxed()) })
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::executor::block_on;

    use super::*;
    use crate::provider::{CompletionEvent, LanguageModelClient, Model, StopReason};
    use crate::request::{Request, message};

    fn model() -> Model {
        Model::new("fake", "fake")
    }

    fn request(text: &str) -> Request {
        let mut request = Request::default();
        request
            .messages
            .push(std::sync::Arc::new(message::Message::new(
                message::Role::User,
                vec![message::ContentPart::text(text)],
            )));
        request
    }

    fn collect(client: &FakeClient, request: &Request) -> Vec<CompletionEvent> {
        block_on(async {
            client
                .stream_completion(&model(), request)
                .await
                .unwrap()
                .map(|event| event.unwrap())
                .collect()
                .await
        })
    }

    #[test]
    fn replays_script_and_records_requests() {
        let client = FakeClient::with_rounds([
            vec![
                CompletionEvent::Text("一".to_owned()),
                CompletionEvent::Stop(StopReason::EndTurn),
            ],
            vec![
                CompletionEvent::Text("二".to_owned()),
                CompletionEvent::Stop(StopReason::EndTurn),
            ],
        ]);

        let first = collect(&client, &request("first"));
        let second = collect(&client, &request("second"));
        assert!(matches!(first[0], CompletionEvent::Text(ref text) if text == "一"));
        assert!(matches!(second[0], CompletionEvent::Text(ref text) if text == "二"));

        let seen = client.requests();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].messages.len(), 1);
        assert_eq!(client.last_request().unwrap().messages.len(), 1);
    }

    #[test]
    fn exhausted_script_ends_the_turn() {
        let client = FakeClient::text("only");
        assert_eq!(collect(&client, &request("x")).len(), 2);
        // 剧本用完：不再回文本，直接结束这一轮，调用方不会卡在空流上。
        let events = collect(&client, &request("x"));
        assert!(matches!(
            events.as_slice(),
            [CompletionEvent::Stop(StopReason::EndTurn)]
        ));
        assert_eq!(client.requests().len(), 2);
    }
}
