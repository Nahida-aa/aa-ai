//! `AgentToolRegistry`：工具注册中心（按 name 分发 + 类型安全执行）。
//!
//! 解决 `AgentTool` 是泛型 trait（每个工具 `Input`/`Output` 类型不同）的分发问题：
//! 注册时把具体 `T` 包进 [`ToolAdapter`]（类型擦除），运行期按 name 取出 trait object
//! 执行。调用方只看到 `execute(name, json) -> ToolOutcome`，不需要知道具体工具类型。
//!
//! 用 `BTreeMap` 而不是 `HashMap`：`tool_definitions()` 的顺序必须是稳定的，
//! 否则每轮请求的工具顺序会抖动，破坏 provider 的 prompt cache 前缀匹配。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use aa_language_model::request::tools;

use crate::tool::agent_tool;

/// 类型擦除后的工具。
trait ErasedTool: Send + Sync {
    fn definition(&self) -> &Arc<tools::Tool>;

    /// 未来借 `self` 与 `events`（不要求 `ToolEvents: 'static`）。
    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        events: &'a dyn agent_tool::ToolEvents,
    ) -> Pin<Box<dyn Future<Output = agent_tool::ToolOutcome> + Send + 'a>>;
}

/// 具体工具 `T` 的擦除包装：把「解析 JSON → 跑 → 转 `ToolOutcome`」固化在这里，
/// 运行期不需要 downcast。
struct ToolAdapter<T: agent_tool::AgentTool> {
    tool: Arc<T>,
    definition: Arc<tools::Tool>,
}

impl<T: agent_tool::AgentTool> ErasedTool for ToolAdapter<T> {
    fn definition(&self) -> &Arc<tools::Tool> {
        &self.definition
    }

    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        events: &'a dyn agent_tool::ToolEvents,
    ) -> Pin<Box<dyn Future<Output = agent_tool::ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let input: T::Input = match serde_json::from_value(input) {
                Ok(input) => input,
                Err(e) => {
                    return agent_tool::ToolOutcome::error(format!(
                        "tool `{}` input parse error: {e}",
                        T::NAME
                    ));
                }
            };
            match self.tool.clone().run(input, events).await {
                Ok(out) => agent_tool::ToolOutcome::ok(out),
                // 工具错误也作为结果回给 LLM（结构化，不是 anyhow）。
                Err(out) => agent_tool::ToolOutcome::error(out),
            }
        })
    }
}

/// 工具注册中心。
pub struct AgentToolRegistry {
    entries: RwLock<BTreeMap<String, Arc<dyn ErasedTool>>>,
}

impl AgentToolRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    /// 注册一个工具（按 `T::NAME`）。同名覆盖。
    pub fn register<T: agent_tool::AgentTool>(&self, tool: Arc<T>) {
        let name = T::NAME.to_string();
        let definition = Arc::new(tools::Tool::new_function(
            T::NAME.to_string(),
            String::new(),
            T::input_schema(),
            false,
        ));
        self.entries
            .write()
            .unwrap()
            .insert(name, Arc::new(ToolAdapter::<T> { tool, definition }));
    }

    /// 所有已注册工具的定义（喂 LLM 的 `Request.tools`）。
    ///
    /// 返回 `Arc<Tool>`：请求每轮都会重建，深拷贝 JSON schema 不值得。
    pub fn tool_definitions(&self) -> Vec<Arc<tools::Tool>> {
        self.entries
            .read()
            .unwrap()
            .values()
            .map(|entry| entry.definition().clone())
            .collect()
    }

    /// 按 name 执行工具（解析 JSON 入参 + 跑 + 返回结果）。
    ///
    /// 未知工具不返回 `Err`，而是 `ToolOutcome::error`：这也是一条要回给 LLM 的
    /// 工具结果（模型可以据此换一个工具或修正参数）。
    pub async fn execute(
        &self,
        name: &str,
        input: &serde_json::Value,
        events: &dyn agent_tool::ToolEvents,
    ) -> agent_tool::ToolOutcome {
        let entry = self.entries.read().unwrap().get(name).cloned();
        match entry {
            Some(entry) => entry.run(input.clone(), events).await,
            None => agent_tool::ToolOutcome::error(format!("unknown tool: {name}")),
        }
    }

    /// 已注册工具名列表（按名字排序）。
    pub fn names(&self) -> Vec<String> {
        self.entries.read().unwrap().keys().cloned().collect()
    }
}

impl Default for AgentToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct EchoInput {
        text: String,
    }

    struct Echo;

    impl agent_tool::AgentTool for Echo {
        type Input = EchoInput;
        type Output = String;
        const NAME: &'static str = "echo";

        async fn run(
            self: Arc<Self>,
            input: EchoInput,
            _events: &dyn agent_tool::ToolEvents,
        ) -> Result<Self::Output, Self::Output> {
            Ok(input.text)
        }
    }

    struct NoEvents;

    impl agent_tool::ToolEvents for NoEvents {
        fn progress(&self, _msg: &str) {}
        fn cancelled(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn dispatches_by_name_and_reports_unknown_tool() {
        let registry = AgentToolRegistry::new();
        registry.register(Arc::new(Echo));

        assert_eq!(registry.names(), vec!["echo".to_owned()]);
        assert_eq!(registry.tool_definitions().len(), 1);

        let outcome = registry
            .execute("echo", &serde_json::json!({ "text": "hi" }), &NoEvents)
            .await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.part.as_text(), Some("hi"));

        let missing = registry
            .execute("nope", &serde_json::json!({}), &NoEvents)
            .await;
        assert!(missing.is_error);
    }
}
