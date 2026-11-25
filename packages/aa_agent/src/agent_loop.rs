//! `AgentLoop`：带工具的 agent 循环。
//!
//! 一次调用 = 在会话上跑一轮任务：喂 `provider::LanguageModelClient` 一个
//! `request::Request`，循环「LLM 要调工具 → 执行 → 回填 → 再喂 LLM」直到 LLM
//! 不再调工具。历史存在 [`session::Session`] 里，调用方可以先 `push_user_text`
//! 再 `run`，也可以从磁盘恢复会话后继续。
//!
//! 历史是 `Vec<Arc<message::Message>>`（见 `docs/decisions/0004-message-layout.md` D7）：
//! 每轮请求只克隆指针（8 B/条），不复制消息内容。

use std::fmt;
use std::sync::Arc;

use futures::StreamExt;

use aa_language_model::provider;
use aa_language_model::request;

use crate::compaction;
use crate::session;
use crate::tool;

/// 循环里 LLM 最多连续调工具的轮数（防 LLM 死循环调工具）。
const MAX_TOOL_ROUNDS: usize = 25;

/// agent 循环的错误。
///
/// 分成两类：LLM 侧（网络 / provider 拒绝）和会话侧（内容无法写进日志）。
/// 工具执行失败**不在这里**：它是给模型看的结果，不是循环的失败。
#[derive(Debug)]
pub enum AgentError {
    Llm(provider::LlmError),
    Session(session::SessionError),
    Compaction(compaction::CompactionError),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
            Self::Compaction(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Llm(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Compaction(error) => Some(error),
        }
    }
}

impl From<provider::LlmError> for AgentError {
    fn from(error: provider::LlmError) -> Self {
        Self::Llm(error)
    }
}

impl From<session::SessionError> for AgentError {
    fn from(error: session::SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<compaction::CompactionError> for AgentError {
    fn from(error: compaction::CompactionError) -> Self {
        Self::Compaction(error)
    }
}

/// agent 循环。持有 LLM 客户端 + 工具注册表；模型和会话按调用传。
///
/// 模型不在构造时钉死（对齐 pi `Agent.streamFunction(model, ...)`）：同一轮
/// 会话里可以按需换模型（便宜的模型跑工具轮、贵的模型出最终答案；失败降级），
/// 循环本身也不必带生命周期，可以存进会话结构。
pub struct AgentLoop {
    llm: Arc<dyn provider::LanguageModelClient>,
    tools: tool::AgentToolRegistry,
    compaction: compaction::CompactionConfig,
}

impl AgentLoop {
    pub fn new(
        llm: Arc<dyn provider::LanguageModelClient>,
        tools: tool::AgentToolRegistry,
    ) -> Self {
        Self {
            llm,
            tools,
            compaction: compaction::CompactionConfig::default(),
        }
    }

    /// 替换压缩策略（默认对齐 Zed：90% 阈值、80k 最小窗口）。
    pub fn with_compaction(mut self, config: compaction::CompactionConfig) -> Self {
        self.compaction = config;
        self
    }

    /// 在 `session` 上跑一次任务：流式回文本（通过 `events` 回调），期间自动
    /// 执行 LLM 要求的工具，直到 LLM 收敛。返回累积的文本回复（不含工具中间结果）。
    ///
    /// 调用方负责把用户输入先写进会话（`session.push_user_text`）。本函数只追加
    /// 模型输出与工具结果，所以恢复会话后可以直接继续跑。
    pub async fn run(
        &self,
        model: &provider::Model,
        session: &mut session::Session,
        events: &impl tool::ToolEvents,
    ) -> Result<String, AgentError> {
        // 工具定义（数据半）从注册表派生，喂给 LLM。`Arc` 克隆，不深拷贝 schema。
        let tool_defs = self.tools.tool_definitions();

        // 超预算时先压缩旧对话。只在「最后一条是待回复的用户文本」时动手，
        // 避免把工具调用和结果拆开。
        compaction::maybe_compact(
            &*self.llm,
            model,
            session,
            &tool_defs,
            &self.compaction,
            events,
        )
        .await?;

        let mut reply = String::new();

        for _ in 0..MAX_TOOL_ROUNDS {
            let request = request::Request {
                // 压缩后 `messages()` 已经是摘要 + 保留段。
                messages: session.messages().to_vec(),
                tools: tool_defs.clone(),
                // 循环按事件流消费并转发文本增量。
                stream: true,
                ..Default::default()
            };

            let mut stream = self.llm.stream_completion(model, &request).await?;

            // 本轮模型说了什么。文本先攒着，工具调用按出现顺序记下。
            let mut round_text = String::new();
            // 思考只回填进历史（有些 provider 下一轮要求带回去），不计入返回文本。
            let mut round_reasoning = String::new();
            let mut round_signature: Option<String> = None;
            let mut pending_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut round_usage = None;
            let mut round_stop = None;

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
                    provider::CompletionEvent::Usage(usage) => {
                        round_usage = Some(usage);
                        events.progress(&format!(
                            "[usage] in={} out={}",
                            usage.input_tokens, usage.output_tokens
                        ));
                    }
                    provider::CompletionEvent::Stop(reason) => round_stop = Some(reason),
                }
            }

            // 先把「这一轮模型说了什么」写回会话，再追加工具结果。
            //
            // 漏掉这步是个真实 bug：下一轮模型看不到自己发过的 tool_call，会重发调用
            // 或答非所问。顺序也必须是 assistant（含 tool_call）→ tool（结果）。
            // 思考排在最前：DeepSeek 要求 `reasoning_content` 按原顺序回放，
            // Anthropic 要求签名思考块在文本之前。
            let had_calls = !pending_calls.is_empty();
            session.push_assistant(session::Turn {
                text: round_text,
                reasoning: (!round_reasoning.is_empty()).then_some(round_reasoning),
                signature: round_signature,
                tool_calls: pending_calls
                    .iter()
                    .map(|(id, name, input)| session::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: session::RecordedInput::Json(input.clone()),
                    })
                    .collect(),
                stop: round_stop,
                usage: round_usage,
            });

            if !had_calls {
                return Ok(reply);
            }

            // 执行本轮全部工具调用，结果合并成**一条 user 消息**。
            // canonical IR 没有 `Tool` 角色：工具结果就是「喂回模型的内容」，
            // OpenAI encoder 再把它拆成 `role:"tool"` 消息。
            // 并行工具调用的结果天然同批，分条会多占信封字节。
            let mut records = Vec::with_capacity(pending_calls.len());
            for (id, name, input) in &pending_calls {
                events.progress(&format!("→ tool: {name}"));
                let outcome = self.tools.execute(name, input, events).await;
                if outcome.is_error {
                    events.progress(&format!(
                        "   tool {name} failed: {}",
                        outcome.part.as_text().unwrap_or("<image>")
                    ));
                }
                records.push(session::ToolResultRecord::from_parts(
                    id,
                    &[outcome.part],
                    outcome.is_error,
                )?);
            }
            session.push_tool_results(records);
        }

        Err(
            provider::LlmError::Transport(format!("tool loop exceeded {MAX_TOOL_ROUNDS} rounds"))
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use aa_language_model::fake;
    use aa_language_model::request::message;

    use super::*;

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

    fn session(system: &str) -> session::Session {
        session::Session::new(session::Header::new("fake", "test", "", system)).unwrap()
    }

    fn jsonl(session: &session::Session) -> String {
        session
            .events()
            .iter()
            .map(|event| event.to_json_line().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn records_tool_call_before_results_and_feeds_them_back() {
        let client = Arc::new(fake::FakeClient::with_rounds([
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
        ]));

        let registry = tool::AgentToolRegistry::new();
        registry.register(Arc::new(Echo));

        let agent = AgentLoop::new(client.clone(), registry);
        let mut session = session("sys");
        session.push_user_text("hello");
        let reply = agent
            .run(
                &provider::Model::new("fake", "test"),
                &mut session,
                &NoEvents,
            )
            .await
            .unwrap();
        assert_eq!(reply, "done");

        let seen = client.requests();
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
        let client = Arc::new(fake::FakeClient::with_rounds([
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
        ]));

        let registry = tool::AgentToolRegistry::new();
        registry.register(Arc::new(Echo));

        let agent = AgentLoop::new(client.clone(), registry);
        let mut session = session("");
        session.push_user_text("hi");
        let reply = agent
            .run(
                &provider::Model::new("fake", "test"),
                &mut session,
                &NoEvents,
            )
            .await
            .unwrap();
        // 思考不进入返回文本。
        assert_eq!(reply, "done");

        let seen = client.requests();
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
        let client = Arc::new(fake::FakeClient::with_rounds([
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
        ]));
        let agent = AgentLoop::new(client.clone(), tool::AgentToolRegistry::new());
        let mut session = session("");
        session.push_user_text("hi");
        let reply = agent
            .run(
                &provider::Model::new("fake", "test"),
                &mut session,
                &NoEvents,
            )
            .await
            .unwrap();
        assert_eq!(reply, "recovered");

        let seen = client.requests();
        let tool_msg = &seen[1].messages[2];
        assert_eq!(tool_msg.role(), message::Role::User);
        assert!(matches!(
            tool_msg.content()[0],
            message::ContentPart::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn resumed_session_keeps_previous_turns_in_the_request() {
        let client = Arc::new(fake::FakeClient::with_rounds([
            vec![
                provider::CompletionEvent::Text("一".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ],
            vec![
                provider::CompletionEvent::Text("二".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ],
        ]));
        let agent = AgentLoop::new(client.clone(), tool::AgentToolRegistry::new());
        let model = provider::Model::new("fake", "test");

        let mut first = session("");
        first.push_user_text("one");
        agent.run(&model, &mut first, &NoEvents).await.unwrap();

        // 模拟「写盘 → 重启 → 恢复」。
        let mut restored = session::Session::from_jsonl(&jsonl(&first)).unwrap();
        assert_eq!(restored.message_count(), 2);

        restored.push_user_text("two");
        agent.run(&model, &mut restored, &NoEvents).await.unwrap();

        let seen = client.requests();
        let second = seen.last().unwrap();
        // user(one) → assistant(一) → user(two)：上一轮的文本确实回放了。
        assert_eq!(second.messages.len(), 3);
        assert_eq!(second.messages[0].role(), message::Role::User);
        assert_eq!(second.messages[1].role(), message::Role::Assistant);
        assert_eq!(second.messages[2].role(), message::Role::User);
        assert!(matches!(
            &second.messages[1].content()[0],
            message::ContentPart::Text(text) if &**text == "一"
        ));
    }

    #[tokio::test]
    async fn compacts_over_budget_and_keeps_user_wording_beside_the_summary() {
        let client = Arc::new(fake::FakeClient::with_handler(|_model, request| {
            let is_compaction = request
                .messages
                .last()
                .and_then(|message| message.content().first())
                .is_some_and(|part| {
                    matches!(
                        part,
                        message::ContentPart::Text(text) if text.starts_with("You are compacting")
                    )
                });
            if is_compaction {
                vec![
                    provider::CompletionEvent::Text("## Goal\nship it".to_owned()),
                    provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
                ]
            } else {
                vec![
                    provider::CompletionEvent::Text("ok".to_owned()),
                    provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
                ]
            }
        }));

        let mut session = session("");
        for index in 0..8 {
            session.push_user_text(format!("question {index}"));
            session.push_assistant(session::Turn {
                text: "x".repeat(2_000),
                ..session::Turn::default()
            });
        }
        session.push_user_text("latest");

        let model = provider::Model {
            context_window: Some(4_000),
            ..provider::Model::new("fake", "test")
        };
        let config = compaction::CompactionConfig {
            min_context_window: 0,
            ..Default::default()
        };
        let agent =
            AgentLoop::new(client.clone(), tool::AgentToolRegistry::new()).with_compaction(config);
        let reply = agent.run(&model, &mut session, &NoEvents).await.unwrap();
        assert_eq!(reply, "ok");

        // 生效历史 = 保留的 8 条用户原话 + 摘要 + latest + 本轮回复。
        assert_eq!(session.message_count(), 11);
        // 原始事件一条没删（审计）。
        assert!(
            session
                .events()
                .iter()
                .any(|event| matches!(event, session::SessionEvent::Compaction(_)))
        );
        assert_eq!(session.compactions().len(), 1);
        assert!(session.compactions()[0].summary.contains("## Goal"));

        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        // 摘要请求：旧历史 + 压缩指令，且不带工具。
        assert!(matches!(
            &requests[0].messages.last().unwrap().content()[0],
            message::ContentPart::Text(text) if text.starts_with("You are compacting")
        ));
        assert!(requests[0].tools.is_empty());

        // 正常回合：保留的用户原话 → 摘要 → 待回复的 latest。
        let view = &requests[1].messages;
        assert_eq!(view.len(), 10);
        for (index, message) in view[..8].iter().enumerate() {
            assert!(matches!(
                &message.content()[0],
                message::ContentPart::Text(text) if **text == format!("question {index}")
            ));
        }
        assert!(matches!(
            &view[8].content()[0],
            message::ContentPart::Text(text) if text.contains("## Goal")
        ));
        assert!(matches!(
            &view[9].content()[0],
            message::ContentPart::Text(text) if &**text == "latest"
        ));
    }

    #[tokio::test]
    async fn never_compacts_between_a_tool_call_and_its_result() {
        let client = Arc::new(fake::FakeClient::with_handler(|_model, _request| {
            vec![
                provider::CompletionEvent::Text("ok".to_owned()),
                provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
            ]
        }));

        let mut session = session("");
        session.push_user_text("hi");
        session.push_assistant(session::Turn {
            text: "x".repeat(100_000),
            tool_calls: vec![session::ToolCall {
                id: "call_1".to_owned(),
                name: "echo".to_owned(),
                input: session::RecordedInput::Json(serde_json::json!({ "text": "hi" })),
            }],
            ..session::Turn::default()
        });
        session.push_tool_results(vec![session::ToolResultRecord {
            call_id: "call_1".to_owned(),
            content: vec!["output".to_owned()],
            is_error: false,
        }]);

        let model = provider::Model {
            context_window: Some(1_000),
            ..provider::Model::new("fake", "test")
        };
        let config = compaction::CompactionConfig {
            min_context_window: 0,
            ..Default::default()
        };
        AgentLoop::new(client, tool::AgentToolRegistry::new())
            .with_compaction(config)
            .run(&model, &mut session, &NoEvents)
            .await
            .unwrap();

        assert!(session.compactions().is_empty());
    }

    #[tokio::test]
    async fn small_context_is_sent_unchanged() {
        let client = Arc::new(fake::FakeClient::with_rounds([vec![
            provider::CompletionEvent::Text("ok".to_owned()),
            provider::CompletionEvent::Stop(provider::StopReason::EndTurn),
        ]]));
        let mut session = session("");
        session.push_user_text("hi");

        let model = provider::Model {
            context_window: Some(100_000),
            ..provider::Model::new("fake", "test")
        };
        AgentLoop::new(client.clone(), tool::AgentToolRegistry::new())
            .run(&model, &mut session, &NoEvents)
            .await
            .unwrap();

        assert!(session.compactions().is_empty());
        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.len(), 1);
    }
}
