//! `ToolLoop` — 对**模型上下文**跑一轮工具循环，以异步流输出结果。
//!
//! loop 不认识会话，也不做持久化：调用方给它一段 `context`（模型可见消息序列），
//! 它返回 [`ToolLoopStream`]，宿主边消费边落盘/渲染。压缩属于宿主。

use std::collections::HashMap;
use std::sync::Arc;

use async_stream::try_stream;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

use aa_ai_persistence::{AssistantMessage, ToolCall, ToolResultRecord};
use aa_language_model::provider::{self, LanguageModelCompletionEvent};
use aa_language_model::request::message::{LanguageModelToolUseInput, Message};
use aa_language_model::request::{self, tools};

use crate::tool::{AgentToolRegistry, ToolInputHandle, ToolOutcome};
use crate::{CancelReason, CancelToken};

const MAX_TOOL_ROUNDS: usize = 25;

/// loop 输出的结构化事件流。最后一个事件总是 [`ToolLoopEvent::Finished`]。
pub type ToolLoopStream = BoxStream<'static, Result<ToolLoopEvent, provider::LlmError>>;

/// loop 产生的事实与渲染事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolLoopEvent {
    /// 模型文本增量，宿主可逐段渲染。
    TextDelta(String),
    /// 一个工具调用即将执行。
    ToolStarted { id: String, name: String },
    /// 模型这一轮的完整输出。
    Assistant(AssistantMessage),
    /// 一批工具结果（一次模型调用产生的所有调用）。
    ToolResults(Vec<ToolResultRecord>),
    /// 回合结束，带累计文本和取消原因。
    Finished {
        reply: String,
        cancelled: Option<CancelReason>,
    },
}

#[derive(Clone)]
pub struct ToolLoop {
    llm: Arc<dyn provider::LanguageModelClient>,
    tools: AgentToolRegistry,
}

impl ToolLoop {
    pub fn new(llm: Arc<dyn provider::LanguageModelClient>, tools: AgentToolRegistry) -> Self {
        Self { llm, tools }
    }

    /// 给宿主用：压缩等会话级能力需要同一个客户端。
    pub fn client(&self) -> &dyn provider::LanguageModelClient {
        &*self.llm
    }

    /// 给宿主用：压缩请求需要同样的工具定义。
    pub fn tool_definitions(&self) -> Vec<Arc<tools::Tool>> {
        self.tools.tool_definitions()
    }

    /// 在 `context`（模型上下文，不是会话）上跑一个用户回合，返回流式事件。
    pub fn run(
        &self,
        model: &provider::LanguageModel,
        context: &[Arc<Message>],
        cancel: &CancelToken,
    ) -> ToolLoopStream {
        let llm = self.llm.clone();
        let tools = self.tools.clone();
        let model = model.clone();
        let context = context.to_vec();
        let cancel = cancel.clone();

        Box::pin(try_stream! {
            let tool_defs = tools.tool_definitions();
            let mut reply = String::new();

            for _round in 0..MAX_TOOL_ROUNDS {
                if cancel.is_cancelled() {
                    yield ToolLoopEvent::Finished {
                        reply,
                        cancelled: cancel.reason(),
                    };
                    return;
                }

                let request = request::Request {
                    messages: context.clone(),
                    tools: tool_defs.clone(),
                    stream: true,
                    ..Default::default()
                };
                let mut stream = llm.stream_completion(&model, &request).await?;

                let mut text = String::new();
                let mut reasoning = String::new();
                let mut signature: Option<String> = None;
                let mut calls: Vec<(String, String, LanguageModelToolUseInput)> = Vec::new();
                // 支持流式入参的工具在参数生成期间启动；快照投喂、终值 finish。
                let mut started: HashMap<String, (ToolInputHandle, BoxFuture<'_, ToolOutcome>)> =
                    HashMap::new();
                let mut stop = None;
                let mut usage = None;
                let mut cancelled = None;
                let mut stream_error: Option<provider::LlmError> = None;

                loop {
                    tokio::select! {
                        biased;
                        reason = cancel.cancelled() => { cancelled = Some(reason); break; }
                        event = stream.next() => match event {
                            Some(Ok(LanguageModelCompletionEvent::Text(delta))) => {
                                yield ToolLoopEvent::TextDelta(delta.clone());
                                reply.push_str(&delta);
                                text.push_str(&delta);
                            }
                            Some(Ok(LanguageModelCompletionEvent::Reasoning { text: delta, signature: sig })) => {
                                reasoning.push_str(&delta);
                                if sig.is_some() { signature = sig; }
                            }
                            Some(Ok(LanguageModelCompletionEvent::ToolUse { id, name, input, is_complete: true })) => {
                                if let Some((handle, _)) = started.get(&id) {
                                    handle.finish(input.clone());
                                }
                                calls.push((id, name, input));
                            }
                            Some(Ok(LanguageModelCompletionEvent::ToolUse { id, name, input, is_complete: false })) => {
                                if !started.contains_key(&id) {
                                    if let Some((handle, future)) = tools.start(&name, &cancel) {
                                        started.insert(id.clone(), (handle, future));
                                    }
                                }
                                if let Some((handle, _)) = started.get(&id) {
                                    handle.send_partial(input.as_value().cloned().unwrap_or_default());
                                }
                            }
                            Some(Ok(LanguageModelCompletionEvent::Usage(value))) => usage = Some(value),
                            Some(Ok(LanguageModelCompletionEvent::Stop(reason))) => stop = Some(reason),
                            Some(Err(error)) => { stream_error = Some(error); break; },
                            None => break,
                        },
                    }
                }
                drop(stream);
                if let Some(error) = stream_error {
                    Err(error)?;
                }

                yield ToolLoopEvent::Assistant(AssistantMessage {
                    text,
                    reasoning: (!reasoning.is_empty()).then_some(reasoning),
                    signature,
                    tool_calls: calls
                        .iter()
                        .map(|(id, name, input)| ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        })
                        .collect(),
                    stop,
                    usage,
                    model: Some((&model).into()),
                });

                if let Some(reason) = cancelled {
                    if !calls.is_empty() {
                        yield ToolLoopEvent::ToolResults(
                            calls.iter().map(|(id, _, _)| cancelled_result(id)).collect(),
                        );
                    }
                    yield ToolLoopEvent::Finished {
                        reply,
                        cancelled: Some(reason),
                    };
                    return;
                }
                if calls.is_empty() {
                    yield ToolLoopEvent::Finished {
                        reply,
                        cancelled: None,
                    };
                    return;
                }

                let mut records = Vec::with_capacity(calls.len());
                let mut cancelled = None;
                for (id, name, input) in &calls {
                    if let Some(reason) = cancel.reason() {
                        cancelled = Some(reason);
                        break;
                    }
                    yield ToolLoopEvent::ToolStarted {
                        id: id.clone(),
                        name: name.clone(),
                    };
                    let outcome: ToolOutcome = match started.remove(id) {
                        Some((_, future)) => future.await,
                        None => tools.execute(name, input, &cancel).await,
                    };
                    records.push(ToolResultRecord {
                        call_id: id.clone(),
                        content: vec![outcome.part.as_text().unwrap_or("<image>").to_owned()],
                        is_error: outcome.is_error,
                    });
                    if let Some(reason) = cancel.reason() {
                        cancelled = Some(reason);
                        break;
                    }
                }
                if let Some(reason) = cancelled {
                    for (id, _, _) in &calls[records.len()..] {
                        records.push(cancelled_result(id));
                    }
                    yield ToolLoopEvent::ToolResults(records);
                    yield ToolLoopEvent::Finished {
                        reply,
                        cancelled: Some(reason),
                    };
                    return;
                }
                yield ToolLoopEvent::ToolResults(records);
            }

            yield ToolLoopEvent::Finished {
                reply,
                cancelled: Some(CancelReason::Budget),
            };
        })
    }
}

fn cancelled_result(call_id: &str) -> ToolResultRecord {
    ToolResultRecord {
        call_id: call_id.to_owned(),
        content: vec!["tool canceled".to_owned()],
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_language_model::fake;

    fn model() -> provider::LanguageModel {
        provider::LanguageModel::new("fake", "test")
    }

    #[tokio::test]
    async fn streams_events_then_finishes() {
        let client = Arc::new(fake::FakeClient::text("done"));
        let agent = ToolLoop::new(client, AgentToolRegistry::new());
        let events = agent
            .run(&model(), &[], &CancelToken::new())
            .collect::<Vec<_>>()
            .await;
        let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(matches!(events.first(), Some(ToolLoopEvent::TextDelta(_))));
        assert!(matches!(events.get(1), Some(ToolLoopEvent::Assistant(_))));
        assert!(matches!(
            events.last(),
            Some(ToolLoopEvent::Finished { reply, cancelled: None }) if reply == "done"
        ));
    }
}
