//! 工具定义类型。fork 自 Zed `crates/language_model_core/src/request.rs` 的
//! `LanguageModelRequestTool` 一族（数据半：可序列化喂给 LLM 的工具定义，
//! 执行半另起 trait）。
//!
//! 命名约定：跨 crate 的公共类型使用完整域名，便于搜索与 AI 检索时省掉模块路径；
//! 与 Zed 同名，去掉的只是不必要的缩写。
//! Zed 原名映射（便于对照 Zed 更新）：
//! - `Tool`            ← Zed `LanguageModelRequestTool`
//! - `LanguageModelRequestToolInput` ← Zed `LanguageModelRequestToolInput`
//! - `CustomToolFormat`← Zed `LanguageModelCustomToolFormat`
//! - `CustomToolGrammarSyntax` ← Zed `LanguageModelCustomToolGrammarSyntax`
//! - `Tool::new_function` ← Zed `LanguageModelRequestTool::function`

use serde::{Deserialize, Serialize};

/// 喂给 LLM 的工具定义（数据半，不含执行闭包）。
/// Zed 原名 `LanguageModelRequestTool`。
#[derive(Debug, PartialEq, Hash, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// 工具的输入定义统称（`Function` 是 JSON Schema，`Custom` 是自定义格式）。
    /// 不直接出现在 wire 上：各 provider 序列化成自己的字段
    /// （Anthropic = `input_schema`，OpenAI = `function.parameters`）。
    /// Zed 原字段名 `input`。
    pub input: LanguageModelRequestToolInput,
}

impl Tool {
    /// 构造一个 `Function`（JSON Schema 输入）工具。
    /// Zed 原构造函数名 `function`，此处 `new_function` 以消除与
    /// 「function calling」/ `fn` 关键字的二义。
    pub fn new_function(
        name: String,
        description: String,
        input_schema: serde_json::Value,
        use_input_streaming: bool,
    ) -> Self {
        Self {
            name,
            description,
            input: LanguageModelRequestToolInput::Function {
                input_schema,
                use_input_streaming,
            },
        }
    }
}

/// 工具的输入定义。`Function` = JSON Schema；`Custom` = 自定义格式。
/// Zed 原名 `LanguageModelRequestToolInput`。
#[derive(Debug, PartialEq, Hash, Clone, Serialize, Deserialize)]
pub enum LanguageModelRequestToolInput {
    Function {
        /// 工具入参的 JSON Schema（喂给 LLM 让它按此填参）。
        input_schema: serde_json::Value,
        /// provider 侧是否流式生成工具入参。
        /// Zed 无此注释；Anthropic wire 对应 `eager_input_streaming`。
        use_input_streaming: bool,
    },
    Custom {
        // 存疑：Zed 原样为 `Option<...>`。`None` 的确切语义未确认
        // （需看 Zed 实际如何构造/消费 Custom 工具），此处不臆造解释。
        format: Option<CustomToolFormat>,
    },
}

/// Zed 原名 `LanguageModelCustomToolFormat`。
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum CustomToolFormat {
    Text,
    Grammar {
        syntax: CustomToolGrammarSyntax,
        definition: String,
    },
}

/// Zed 原名 `LanguageModelCustomToolGrammarSyntax`。
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomToolGrammarSyntax {
    Lark,
    Regex,
}
