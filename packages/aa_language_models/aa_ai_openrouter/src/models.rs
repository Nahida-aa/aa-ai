//! `GET /models` 的响应形状（OpenRouter 版）。
//!
//! 和 OpenAI 的 `/models` 不同：OpenRouter 返回 `context_length`、`pricing`、
//! `supported_parameters` 等模型事实，`id` 形如 `vendor/model`（免费模型带
//! `:free` 后缀，但**不要用后缀判断免费**——以 `pricing` 的数值为准）。
//!
//! 这个接口是公开的（不需要密钥），但 provider 仍然带 `Authorization`：
//! 对 `/models` 无副作用，也便于将来切到需要鉴权的 `/models/user`。

use serde::Deserialize;

/// `GET /models` 的响应体。
#[derive(Debug, Deserialize)]
pub struct ModelList {
    #[serde(default)]
    pub data: Vec<ModelInfo>,
}

/// 列表里的一项。只解析用得到的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    /// wire 上的模型 id（`vendor/model` 或 `vendor/model:free`）。
    pub id: String,
    /// 给人看的名字。
    #[serde(default)]
    pub name: Option<String>,
    /// 上下文窗口（输入 + 输出）。
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
    /// 模型接受的参数名，例如 `tools` / `reasoning` / `response_format`。
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

/// 每百万 token 的价格（USD，字符串形式的十进制数）。
#[derive(Debug, Clone, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub completion: Option<String>,
}

impl ModelInfo {
    /// 输入和输出都为零价才算免费。
    ///
    /// 用数值判断而不是 `:free` 后缀：OpenRouter 的定价会变，后缀只是路由名。
    pub fn is_free(&self) -> bool {
        let Some(pricing) = &self.pricing else {
            return false;
        };
        is_zero(&pricing.prompt) && is_zero(&pricing.completion)
    }

    /// 模型是否声明支持工具（`supported_parameters` 里有 `tools`）。
    pub fn supports_tools(&self) -> bool {
        self.supported_parameters.iter().any(|p| p == "tools")
    }

    /// `vendor/model` 里的 vendor 段。
    pub fn vendor(&self) -> Option<&str> {
        self.id.split_once('/').map(|(vendor, _)| vendor)
    }
}

fn is_zero(price: &Option<String>) -> bool {
    price
        .as_deref()
        .and_then(|price| price.parse::<f64>().ok())
        .is_some_and(|price| price == 0.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_model_list_and_free_flag() {
        let list: super::ModelList = serde_json::from_str(
            r#"{"data":[
                {"id":"google/gemma-4-31b-it:free","name":"Gemma 4 31B",
                 "context_length":262144,
                 "pricing":{"prompt":"0","completion":"0"},
                 "supported_parameters":["tools","reasoning"]},
                {"id":"openai/gpt-5","name":"GPT-5","context_length":400000,
                 "pricing":{"prompt":"0.00000125","completion":"0.00001"},
                 "supported_parameters":["tools"]},
                {"id":"no-pricing/model"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(list.data.len(), 3);
        assert!(list.data[0].is_free());
        assert!(list.data[0].supports_tools());
        assert_eq!(list.data[0].vendor(), Some("google"));
        assert!(!list.data[1].is_free());
        // 没有 pricing 不能假设免费。
        assert!(!list.data[2].is_free());
        assert!(!list.data[2].supports_tools());
    }

    #[test]
    fn tolerates_missing_data() {
        let list: super::ModelList = serde_json::from_str("{}").unwrap();
        assert!(list.data.is_empty());
    }
}
