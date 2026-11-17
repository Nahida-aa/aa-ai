//! `AgentLoop`：v0 最简带工具的 agent 循环。
//!
//! 一次调用 = 一个任务：喂 `provider::LanguageModelClient` 一个 `request::Request`，循环「LLM 要调工具 →
//! 执行 → 回填 → 再喂 LLM」直到 LLM 不再调工具。
//!
//! 历史是 `Vec<Arc<message::Message>>`（见 `docs/decisions/0004-message-layout.md` D7）：
//! 每轮请求只克隆指针（8 B/条），不复制消息内容。

use std::sync::Arc;

use futures::StreamExt;

use aa_language_model::provider;
use aa_language_model::request;
use aa_language_model::request::message;

use crate::tool;

/// 循环里 LLM 最多连续调工具的轮数（防 LLM 死循环调工具）。
const MAX_TOOL_ROUNDS: usize = 25;

/// agent 循环。持有 LLM 客户端 + 工具注册表 + 一个固定 `provider::Model`。
pub struct AgentLoop<'a> {
    llm: &'a dyn provider::LanguageModelClient,
    model: provider::Model,
    tools: tool::AgentToolRegistry,
}

impl<'a> AgentLoop<'a> {
    pub fn new(
        llm: &'a dyn provider::LanguageModelClient,
        model: provider::Model,
        tools: tool::AgentToolRegistry,
    ) -> Self {
        Self { llm, model, tools }
    }

    /// 跑一次任务：输入消息 + 系统提示，流式回文本（通过 `events` 回调），
    /// 期间自动执行 LLM 要求的工具，直到 LLM 收敛。
    ///
    /// 返回累积的文本回复（不含工具中间结果）。工具执行进度通过 `events` 上报。
    pub async fn run(
        &self,
        system: &str,
        user: &str,
        events: &impl tool::ToolEvents,
    ) -> Result<String, provider::LlmError> {
        // 工具定义（数据半）从注册表派生，喂给 LLM。`Arc` 克隆，不深拷贝 schema。
        let tool_defs = self.tools.tool_definitions();

        let mut history: Vec<Arc<message::Message>> = Vec::new();
        if !system.is_empty() {
            history.push(Arc::new(message::Message::new(
                message::Role::System,
                vec![message::ContentPart::text(system)],
            )));
        }
        history.push(Arc::new(message::Message::new(
            message::Role::User,
            vec![message::ContentPart::text(user)],
        )));

        let mut reply = String::new();

        for _ in 0..MAX_TOOL_ROUNDS {
            let request = request::Request {
                messages: history.clone(),
                tools: tool_defs.clone(),
                ..Default::default()
            };

            let mut stream = self.llm.stream_completion(&self.model, &request).await?;

            // 本轮模型说了什么。文本先攒着，工具调用按出现顺序记下。
            let mut round_text = String::new();
            // 思考只回填进历史（有些 provider 下一轮要求带回去），不计入返回文本。
            let mut round_reasoning = String::new();
            let mut round_signature: Option<String> = None;
            let mut pending_calls: Vec<(String, String, serde_json::Value)> = Vec::new();

            while let Some(event) = stream.next().await {
                match event? {
                    provider::CompletionEvent::Text(delta) => {
                        events.progress(&delta);
                        reply.push_str(&delta);
                        round_text.push_str(&delta);
                    }
                    provider::CompletionEvent::Reasoning { text, signature } => {
                        round_reasoning.push_str(&text);
                        if signature.is_some() {
                            round_signature = signature;
                        }
                    }
                    provider::CompletionEvent::ToolUse {
                        id, name, input, ..
                    } => pending_calls.push((id, name, input)),
                    provider::CompletionEvent::Usage(usage) => events.progress(&format!(
                        "[usage] in={} out={}",
                        usage.input_tokens, usage.output_tokens
                    )),
                    provider::CompletionEvent::Stop(_) => {}
                }
            }

            // 先把「这一轮模型说了什么」写回历史，再追加工具结果。
            //
            // 漏掉这步是个真实 bug：下一轮模型看不到自己发过的 tool_call，会重发调用
            // 或答非所问。顺序也必须是 assistant（含 tool_call）→ tool（结果）。
            // 思考排在最前：DeepSeek 要求 `reasoning_content` 按原顺序回放，
            // Anthropic 要求签名思考块在文本之前。
            let had_calls = !pending_calls.is_empty();
            let mut assistant_parts: Vec<message::ContentPart> =
                Vec::with_capacity(2 + pending_calls.len());
            if !round_reasoning.is_empty() || round_signature.is_some() {
                assistant_parts.push(message::ContentPart::Reasoning {
                    text: Arc::from(round_reasoning.as_str()),
                    signature: round_signature.as_deref().map(Arc::from),
                });
            }
            if !round_text.is_empty() {
                assistant_parts.push(message::ContentPart::Text(Arc::from(round_text.as_str())));
            }
            for (id, name, input) in pending_calls {
                assistant_parts.push(message::ContentPart::tool_call(id, name, input));
            }
            if !assistant_parts.is_empty() {
                history.push(Arc::new(message::Message::new(
                    message::Role::Assistant,
                    assistant_parts,
                )));
            }

            if !had_calls {
                return Ok(reply);
            }

            // 执行本轮全部工具调用，结果合并成**一条 user 消息**。
            // canonical IR 没有 `Tool` 角色：工具结果就是「喂回模型的内容」，
            // OpenAI encoder 再把它拆成 `role:"tool"` 消息。
            // 并行工具调用的结果天然同批，分条会多占信封字节。
            let mut results: Vec<message::ContentPart> = Vec::new();
            {
                let assistant = history.last().expect("assistant message just pushed");
                for part in assistant.content() {
                    let message::ContentPart::ToolCall { id, name, input } = part else {
                        continue;
                    };
                    events.progress(&format!("→ tool: {name}"));
                    let value = input
                        .as_value()
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::String(input.to_wire_string()));
                    let outcome = self.tools.execute(name, &value, events).await;
                    if outcome.is_error {
                        events.progress(&format!(
                            "   tool {name} failed: {}",
                            outcome.part.as_text().unwrap_or("<image>")
                        ));
                    }
                    results.push(message::ContentPart::tool_result(
                        id.clone(),
                        vec![outcome.part],
                        outcome.is_error,
                    ));
                }
            }
            history.push(Arc::new(message::Message::new(
                message::Role::User,
                results,
            )));
        }

        Err(provider::LlmError::Transport(format!(
            "tool loop exceeded {MAX_TOOL_ROUNDS} rounds"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use futures::stream;

    use super::*;

    /// 按轮返回剧本，并记录每次请求（用于断言历史回填顺序）。
    struct ScriptedClient {
        rounds: Mutex<VecDeque<Vec<provider::CompletionEvent>>>,
        seen: Mutex<Vec<request::Request>>,
    }

    impl ScriptedClient {
        fn new(rounds: impl IntoIterator<Item = Vec<provider::CompletionEvent>>) -> Self {
            Self {
                rounds: Mutex::new(rounds.into_iter().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl provider::LanguageModelClient for ScriptedClient {
        fn stream_completion(
            &self,
            _model: &provider::Model,
            request: &request::Request,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<provider::EventStream, provider::LlmError>>
                    + Send
                    + 'static,
            >,
        > {
            self.seen.lock().unwrap().push(request.clone());
            let events = self.rounds.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(async move { Ok(stream::iter(events.into_iter().map(Ok)).boxed()) })
        }
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct EchoInput {
        text: String,
    }

    struct Echo;

    impl tool::AgentTool for Echo {
        type Input = EchoInput;
        type Output = String;
        const NAME: &'static str = "echo";

        async fn run(
            self: Arc<Self>,
            input: EchoInput,
            _events: &dyn tool::ToolEvents,
        ) -> Result<Self::Output, Self::Output> {
            Ok(input.text)
        }
    }

    struct NoEvents;

    impl tool::ToolEvents for NoEvents {
        fn progress(&self, _msg: &str) {}
        fn cancelled(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn records_tool_call_before_results_and_feeds_them_back() {
        let client = ScriptedClient::new([
            vec![
                provider::CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "echo".to_owned(),
                    input: serde_json::json!({ "text": "hi" }),
                    is_complete: true,
                },
                provider::CompletionEvent::Stop(provider::StopReason::ToolUse),
            ],
            vec![
                provider::CompletionEvent::Text("done".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ],
        ]);

        let registry = tool::AgentToolRegistry::new();
        registry.register(Arc::new(Echo));

        let agent = AgentLoop::new(&client, provider::Model::new("stub", "test"), registry);
        let reply = agent.run("sys", "hello", &NoEvents).await.unwrap();
        assert_eq!(reply, "done");

        let seen = client.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);

        // 第二轮请求的历史必须是：system → user → assistant(tool_call) → tool(result)。
        let messages = &seen[1].messages;
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), message::Role::System);
        assert_eq!(messages[1].role(), message::Role::User);
        assert_eq!(messages[2].role(), message::Role::Assistant);
        assert!(matches!(
            messages[2].content()[0],
            message::ContentPart::ToolCall { .. }
        ));
        assert_eq!(messages[3].role(), message::Role::User);
        assert!(matches!(
            messages[3].content()[0],
            message::ContentPart::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn reasoning_is_recorded_before_text_and_never_returned() {
        // 第一轮：思考 + 调工具；第二轮：文本。看第二轮请求里的 assistant 消息。
        let client = ScriptedClient::new([
            vec![
                provider::CompletionEvent::Reasoning {
                    text: "thin".to_owned(),
                    signature: None,
                },
                provider::CompletionEvent::Reasoning {
                    text: "king".to_owned(),
                    signature: Some("sig".to_owned()),
                },
                provider::CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "echo".to_owned(),
                    input: serde_json::json!({ "text": "hi" }),
                    is_complete: true,
                },
                provider::CompletionEvent::Stop(provider::StopReason::ToolUse),
            ],
            vec![
                provider::CompletionEvent::Text("done".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ],
        ]);

        let registry = tool::AgentToolRegistry::new();
        registry.register(Arc::new(Echo));

        let agent = AgentLoop::new(&client, provider::Model::new("stub", "test"), registry);
        let reply = agent.run("", "hi", &NoEvents).await.unwrap();
        // 思考不进入返回文本。
        assert_eq!(reply, "done");

        let seen = client.seen.lock().unwrap();
        let assistant = &seen[1].messages[1];
        assert_eq!(assistant.role(), message::Role::Assistant);
        // 思考在最前，且多段增量拼成一条、签名保留。
        assert!(matches!(
            &assistant.content()[0],
            message::ContentPart::Reasoning { text, signature }
                if &**text == "thinking" && signature.as_deref() == Some("sig")
        ));
        assert!(matches!(
            assistant.content()[1],
            message::ContentPart::ToolCall { .. }
        ));
    }

    #[tokio::test]
    async fn tool_failure_is_a_result_not_a_transport_error() {
        let client = ScriptedClient::new([
            vec![
                provider::CompletionEvent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "missing".to_owned(),
                    input: serde_json::json!({}),
                    is_complete: true,
                },
                provider::CompletionEvent::Stop(provider::StopReason::ToolUse),
            ],
            vec![
                provider::CompletionEvent::Text("recovered".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ],
        ]);
        let agent = AgentLoop::new(
            &client,
            provider::Model::new("stub", "test"),
            tool::AgentToolRegistry::new(),
        );
        let reply = agent.run("", "hi", &NoEvents).await.unwrap();
        assert_eq!(reply, "recovered");

        let seen = client.seen.lock().unwrap();
        let tool_msg = &seen[1].messages[2];
        assert_eq!(tool_msg.role(), message::Role::User);
        assert!(matches!(
            tool_msg.content()[0],
            message::ContentPart::ToolResult { is_error: true, .. }
        ));
    }
}
