//! `GET /models` 的响应形状（OpenAI 与多数兼容端点）。
//!
//! 和 `chat_completion/` 并列：这是**模型列表**接口，不是对话接口。
//! 只解析用得到的字段；`object` / `created` 之类的信封字段不进语义。
//!
//! 用户填模型名的最可靠来源就是这个接口：
//!
//! ```bash
//! curl -H "Authorization: Bearer $KEY" https://api.example.com/v1/models
//! ```
//!
//! 有些网关同时接受 `x-api-key`（见 [`crate::OpenAiLanguageModelProvider::with_auth_header`]）。

use serde::Deserialize;

/// `GET /models` 的响应体。
#[derive(Debug, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub data: Vec<ModelInfo>,
}

/// 列表里的一项。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    /// 要填进 `provider::Model::new(_, id)` 的名字。
    pub id: String,
    /// 有些网关给 `owned_by`，纯 OpenAI 形状不一定有。
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_openai_model_list() {
        let list: super::ModelList = serde_json::from_str(
            r#"{"object":"list","data":[
                {"id":"gpt-4o","object":"model","owned_by":"openai"},
                {"id":"deepseek-chat","object":"model"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(list.data.len(), 2);
        assert_eq!(list.data[0].id, "gpt-4o");
        assert_eq!(list.data[0].owned_by.as_deref(), Some("openai"));
        assert_eq!(list.data[1].owned_by, None);
    }

    #[test]
    fn tolerates_missing_data() {
        let list: super::ModelList = serde_json::from_str("{}").unwrap();
        assert!(list.data.is_empty());
    }
}
