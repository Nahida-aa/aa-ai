//! 工具执行接口与注册表。
//!
//! 定义（JSON schema）在 `aa_language_model::request::tools`；这里只管执行，
//! 包括**流式入参**：支持 `supports_input_streaming()` 的工具可以在参数还在
//! 生成时提前启动，通过 [`ToolInput`] 边收边看，最终以 [`ToolInputPayload::Full`]
//! 为准执行。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use aa_language_model::request::message::{LanguageModelToolUseInput, ToolResultPart};

use crate::CancelToken;
use aa_language_model::request::tools;

/// 入参通道上的负载。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolInputPayload<T> {
    /// 已修补但可能不完整的参数快照，只能用于预览 / 无副作用准备。
    Partial(serde_json::Value),
    /// 完整参数（已反序列化）。
    Full(T),
    /// 参数生成结束但无法解析。
    InvalidJson { error_message: String },
}

/// 工具看到的入参通道（接收端）。
pub struct ToolInput<T> {
    receiver: mpsc::UnboundedReceiver<ToolInputPayload<T>>,
    finished: bool,
}

impl<T> ToolInput<T> {
    pub(crate) fn from_receiver(receiver: mpsc::UnboundedReceiver<ToolInputPayload<T>>) -> Self {
        Self {
            receiver,
            finished: false,
        }
    }

    /// 一次性入参（非流式路径 / 测试）：读到即 `Full`。
    pub fn ready(input: T) -> Self {
        let (sender, receiver) = ToolInputSender::channel();
        sender.finish(input);
        receiver
    }

    /// 下一条负载；读到终值后永远是 `None`。
    pub async fn next(&mut self) -> Option<ToolInputPayload<T>> {
        if self.finished {
            return None;
        }
        let payload = self.receiver.recv().await;
        if matches!(
            payload,
            Some(ToolInputPayload::Full(_) | ToolInputPayload::InvalidJson { .. })
        ) {
            self.finished = true;
        }
        payload
    }

    /// 等最终入参：跳过 `Partial`，坏 JSON 返回错误。
    pub async fn recv(&mut self) -> Result<T, ToolInputError> {
        loop {
            match self.next().await {
                Some(ToolInputPayload::Partial(_)) => continue,
                Some(ToolInputPayload::Full(input)) => return Ok(input),
                Some(ToolInputPayload::InvalidJson { error_message }) => {
                    return Err(ToolInputError {
                        message: error_message,
                    });
                }
                None => {
                    return Err(ToolInputError {
                        message: "tool input stream closed before completion".to_owned(),
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInputError {
    message: String,
}

impl ToolInputError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ToolInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolInputError {}

/// 入参通道发送端（loop / registry 持有，按具体 `T` 类型化）。
pub struct ToolInputSender<T> {
    sender: mpsc::UnboundedSender<ToolInputPayload<T>>,
}

impl<T> ToolInputSender<T> {
    pub fn channel() -> (Self, ToolInput<T>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, ToolInput::from_receiver(receiver))
    }

    /// 投喂快照；工具已结束时返回 `false`，调用方可据此停止投喂。
    pub fn send_partial(&self, value: serde_json::Value) -> bool {
        self.sender.send(ToolInputPayload::Partial(value)).is_ok()
    }

    pub fn finish(&self, input: T) -> bool {
        self.sender.send(ToolInputPayload::Full(input)).is_ok()
    }

    pub fn fail(&self, error_message: impl Into<String>) -> bool {
        self.sender
            .send(ToolInputPayload::InvalidJson {
                error_message: error_message.into(),
            })
            .is_ok()
    }
}

/// 擦除类型后的投喂句柄：loop 不需要知道工具的具体 `Input` 类型。
pub struct ToolInputHandle {
    send: Box<dyn Fn(RawToolInput) -> bool + Send + Sync>,
}

enum RawToolInput {
    Partial(serde_json::Value),
    Full(serde_json::Value),
    Invalid(String),
}

impl ToolInputHandle {
    fn new(send: impl Fn(RawToolInput) -> bool + Send + Sync + 'static) -> Self {
        Self {
            send: Box::new(send),
        }
    }

    pub fn send_partial(&self, value: serde_json::Value) -> bool {
        (self.send)(RawToolInput::Partial(value))
    }

    pub fn finish(&self, input: LanguageModelToolUseInput) -> bool {
        match input {
            LanguageModelToolUseInput::Json(value) => {
                (self.send)(RawToolInput::Full((*value).clone()))
            }
            LanguageModelToolUseInput::Text(text) => self.fail(text.to_string()),
        }
    }

    pub fn fail(&self, message: impl Into<String>) -> bool {
        (self.send)(RawToolInput::Invalid(message.into()))
    }
}

pub trait AgentTool: Send + Sync + 'static {
    type Input: DeserializeOwned + schemars::JsonSchema + Send;
    type Output: Into<ToolResultPart> + Send;
    const NAME: &'static str;
    const DESCRIPTION: &'static str = "";

    fn input_schema() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Self::Input)).unwrap()
    }

    /// 是否支持在参数生成期间提前启动工具。
    fn supports_input_streaming() -> bool {
        false
    }

    /// 普通执行：拿到完整入参后运行。
    fn run(
        self: Arc<Self>,
        input: Self::Input,
        cancel: &CancelToken,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Output>> + Send;

    /// 流式执行：默认等完整入参再走 [`AgentTool::run`]。
    ///
    /// 真正支持流式的工具覆写它，用 [`ToolInput::next`] 读快照做无副作用准备。
    fn run_streaming(
        self: Arc<Self>,
        mut input: ToolInput<Self::Input>,
        cancel: &CancelToken,
    ) -> impl std::future::Future<Output = ToolOutcome> + Send {
        async move {
            match input.recv().await {
                Ok(value) => match self.run(value, cancel).await {
                    Ok(output) => ToolOutcome::ok(output),
                    Err(output) => ToolOutcome::error(output),
                },
                Err(error) => ToolOutcome::error(error.to_string()),
            }
        }
    }
}

#[derive(Debug)]
pub struct ToolOutcome {
    pub part: ToolResultPart,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(part: impl Into<ToolResultPart>) -> Self {
        Self {
            part: part.into(),
            is_error: false,
        }
    }

    pub fn error(part: impl Into<ToolResultPart>) -> Self {
        Self {
            part: part.into(),
            is_error: true,
        }
    }
}

trait ErasedTool: Send + Sync {
    fn definition(&self) -> Arc<tools::Tool>;
    fn execute<'a>(
        &'a self,
        input: &'a LanguageModelToolUseInput,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolOutcome>;
    fn start<'a>(
        &'a self,
        cancel: &'a CancelToken,
    ) -> Option<(ToolInputHandle, BoxFuture<'a, ToolOutcome>)>;
}

struct Adapter<T: AgentTool>(Arc<T>);

impl<T: AgentTool> ErasedTool for Adapter<T> {
    fn definition(&self) -> Arc<tools::Tool> {
        Arc::new(tools::Tool::new_function(
            T::NAME.to_owned(),
            T::DESCRIPTION.to_owned(),
            T::input_schema(),
            T::supports_input_streaming(),
        ))
    }

    fn execute<'a>(
        &'a self,
        input: &'a LanguageModelToolUseInput,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let value = match input {
                LanguageModelToolUseInput::Json(value) => (**value).clone(),
                LanguageModelToolUseInput::Text(text) => {
                    return ToolOutcome::error(text.to_string());
                }
            };
            let input = match serde_json::from_value::<T::Input>(value) {
                Ok(input) => input,
                Err(error) => return ToolOutcome::error(error.to_string()),
            };
            match T::run(self.0.clone(), input, cancel).await {
                Ok(output) => ToolOutcome::ok(output),
                Err(output) => ToolOutcome::error(output),
            }
        })
    }

    fn start<'a>(
        &'a self,
        cancel: &'a CancelToken,
    ) -> Option<(ToolInputHandle, BoxFuture<'a, ToolOutcome>)> {
        if !T::supports_input_streaming() {
            return None;
        }
        let (typed_sender, typed_receiver) = ToolInputSender::<T::Input>::channel();
        let handle = ToolInputHandle::new(move |raw| match raw {
            RawToolInput::Partial(value) => typed_sender.send_partial(value),
            RawToolInput::Full(value) => match serde_json::from_value::<T::Input>(value) {
                Ok(input) => typed_sender.finish(input),
                Err(error) => {
                    typed_sender.fail(format!("tool `{}` input parse error: {error}", T::NAME))
                }
            },
            RawToolInput::Invalid(message) => typed_sender.fail(message),
        });
        let tool = self.0.clone();
        let future = Box::pin(async move { T::run_streaming(tool, typed_receiver, cancel).await });
        Some((handle, future))
    }
}

/// 工具注册表：按名字分发，定义顺序稳定（BTreeMap，保护 provider 的 cache 前缀）。
#[derive(Clone, Default)]
pub struct AgentToolRegistry {
    tools: BTreeMap<String, Arc<dyn ErasedTool>>,
}

impl AgentToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: AgentTool>(&mut self, tool: Arc<T>) {
        self.tools
            .insert(T::NAME.to_owned(), Arc::new(Adapter(tool)));
    }

    pub fn tool_definitions(&self) -> Vec<Arc<tools::Tool>> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 提前启动一个支持流式入参的工具，返回投喂句柄和它的执行 future。
    pub fn start<'a>(
        &'a self,
        name: &str,
        cancel: &'a CancelToken,
    ) -> Option<(ToolInputHandle, BoxFuture<'a, ToolOutcome>)> {
        self.tools.get(name)?.start(cancel)
    }

    pub async fn execute(
        &self,
        name: &str,
        input: &LanguageModelToolUseInput,
        cancel: &CancelToken,
    ) -> ToolOutcome {
        match self.tools.get(name) {
            Some(tool) => tool.execute(input, cancel).await,
            None => ToolOutcome::error(format!("unknown tool: {name}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct EchoInput {
        text: String,
    }

    struct Echo;

    impl AgentTool for Echo {
        type Input = EchoInput;
        type Output = String;
        const NAME: &'static str = "echo";

        async fn run(
            self: Arc<Self>,
            input: EchoInput,
            _cancel: &CancelToken,
        ) -> Result<String, String> {
            Ok(input.text)
        }
    }

    struct StreamEcho {
        seen: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl AgentTool for StreamEcho {
        type Input = EchoInput;
        type Output = String;
        const NAME: &'static str = "stream_echo";

        fn supports_input_streaming() -> bool {
            true
        }

        async fn run(
            self: Arc<Self>,
            input: EchoInput,
            _cancel: &CancelToken,
        ) -> Result<String, String> {
            Ok(input.text)
        }

        async fn run_streaming(
            self: Arc<Self>,
            mut input: ToolInput<EchoInput>,
            _cancel: &CancelToken,
        ) -> ToolOutcome {
            while let Some(payload) = input.next().await {
                if matches!(payload, ToolInputPayload::Partial(_)) {
                    self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                } else if let ToolInputPayload::Full(value) = payload {
                    return ToolOutcome::ok(value.text);
                }
            }
            ToolOutcome::error("input closed")
        }
    }

    #[tokio::test]
    async fn dispatches_by_name() {
        let mut registry = AgentToolRegistry::new();
        registry.register(Arc::new(Echo));
        assert_eq!(registry.tool_definitions().len(), 1);
        let input = LanguageModelToolUseInput::Json(Box::new(serde_json::json!({ "text": "hi" })));
        let outcome = registry.execute("echo", &input, &CancelToken::new()).await;
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn streaming_tool_receives_partials_then_full() {
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = AgentToolRegistry::new();
        registry.register(Arc::new(StreamEcho { seen: seen.clone() }));
        let cancel = CancelToken::new();
        let (handle, future) = registry.start("stream_echo", &cancel).unwrap();
        assert!(handle.send_partial(serde_json::json!({ "text": "he" })));
        assert!(handle.send_partial(serde_json::json!({ "text": "hel" })));
        assert!(handle.finish(LanguageModelToolUseInput::Json(Box::new(
            serde_json::json!({ "text": "hello" })
        ))));
        let outcome = future.await;
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!outcome.is_error);
    }
}
