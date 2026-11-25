//! `GET /api/tags`：Ollama 本机模型列表。
//!
//! 和 `aa_ai_openai::models` 是**两套形状**，不共用结构体：
//! OpenAI 是 `{"data":[{"id","owned_by"}]}`，Ollama 是
//! `{"models":[{"name","model","size","digest","details":{...}}]}`。
//!
//! 应用拿到 `name` 后填进 `aa_language_model::provider::LanguageModel::new("ollama", name)`。

use serde::Deserialize;

/// `/api/tags` 的响应体。
#[derive(Debug, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

/// 列表里的一项。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    /// 完整名字（含 tag），例如 `qwen2.5:0.5b`。这是要填进 `LanguageModel::id` 的值。
    pub name: String,
    /// 模型名（多数版本与 `name` 相同；老版本可能只有这个）。
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub details: Option<ModelDetails>,
}

/// `details`：家族、参数量、量化级别（只用于展示）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelDetails {
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub parameter_size: Option<String>,
    #[serde(default)]
    pub quantization_level: Option<String>,
}

impl ModelInfo {
    /// 取一个可用于 `LanguageModel::new` 的 id（优先 `name`）。
    pub fn id(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_tags_response() {
        let list: super::ModelList = serde_json::from_str(
            r#"{"models":[
                {"name":"qwen2.5:0.5b","model":"qwen2.5:0.5b","size":398000000,
                 "digest":"abc",
                 "details":{"format":"gguf","family":"qwen2","parameter_size":"0.5B",
                            "quantization_level":"Q4_K_M"}},
                {"name":"llama3.2:latest"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(list.models.len(), 2);
        assert_eq!(list.models[0].id(), "qwen2.5:0.5b");
        let details = list.models[0].details.as_ref().unwrap();
        assert_eq!(details.family.as_deref(), Some("qwen2"));
        assert_eq!(details.parameter_size.as_deref(), Some("0.5B"));
        // 缺 details 的条目也能解析。
        assert!(list.models[1].details.is_none());
    }

    #[test]
    fn tolerates_missing_models() {
        let list: super::ModelList = serde_json::from_str("{}").unwrap();
        assert!(list.models.is_empty());
    }
}
