//! `AgentTool`：工具执行接口。

use std::sync::Arc;

use aa_language_model::request::message;
use serde::{Deserialize, Serialize};

/// 工具执行时可用的运行时能力（进度/取消）。
/// 由调用方（会话/线程）实现并传入 —— 工具接口因此**不依赖任何具体线程类型**。
///
/// Zed 对应物是 thread 内部的 `ToolCallEventStream`（耦合在 thread.rs），
/// 我们用本 trait 解耦，使 `AgentTool` 能下沉到 core 被任意执行环境复用。
///
/// 用 `&dyn ToolEvents` 传递（不是 `&impl`）：注册表把工具调用类型擦除后，
/// 需要在 `dyn` 边界上转发它，`impl Trait` 参数会挡住这一点。
pub trait ToolEvents: Send + Sync {
    /// 上报一段进度（给 UI / 日志）。
    fn progress(&self, msg: &str);

    /// 调用方是否已要求取消当前工具执行。工具应周期性检查以支持取消。
    fn cancelled(&self) -> bool;
}

/// 工具执行接口。
///
/// - `Input`：工具入参，需可序列化 + 生成 JSON Schema（喂 LLM 的 `Tool` 定义用）。
/// - `Output`：工具结果，需可转成 LLM 工具结果的一段内容（`message::ToolResultPart`）。
///
/// 错误用 `Result<Output, Output>` 而非 `Result<Output, Error>`：
/// 工具错误也要**回给 LLM 当结果**（结构化、可读），不是任意的 `anyhow::Error`。
pub trait AgentTool: Send + Sync + 'static {
    type Input: for<'de> Deserialize<'de> + Serialize + schemars::JsonSchema + Send;
    type Output: Into<message::ToolResultPart> + Send;

    /// 工具名（给 LLM 看的 token，业界趋同：`read`/`write`/`edit`/`bash`/`grep`/`glob`）。
    const NAME: &'static str;

    /// 入参的 JSON Schema（喂 LLM）。
    fn input_schema() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Self::Input)).unwrap()
    }

    /// 执行工具。`events` 由调用方注入（进度/取消），工具不依赖具体线程。
    fn run(
        self: Arc<Self>,
        input: Self::Input,
        events: &dyn ToolEvents,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Output>> + Send;
}

/// 一次工具执行的结果（已类型擦除）。
///
/// 注册表统一返回它，agent loop 再把 `part` + 调用 id 组装成
/// `ContentPart::ToolResult`。`is_error` 决定 provider wire 上是普通结果还是错误结果。
pub struct ToolOutcome {
    pub part: message::ToolResultPart,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(part: impl Into<message::ToolResultPart>) -> Self {
        Self {
            part: part.into(),
            is_error: false,
        }
    }

    pub fn error(part: impl Into<message::ToolResultPart>) -> Self {
        Self {
            part: part.into(),
            is_error: true,
        }
    }
}
