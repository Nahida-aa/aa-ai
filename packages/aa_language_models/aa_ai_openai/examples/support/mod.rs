//! 例子共用的环境变量读取：把「一个第三方兼容端点」变成 provider 实例。
//!
//! 不放进库本体：库只提供构造数据，环境变量是应用侧约定。

use aa_ai_openai::OpenAiLanguageModelProvider;

/// `AA_BASE_URL`（默认 OpenAI 官方）。
pub fn base_url() -> String {
    std::env::var("AA_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_owned())
}

fn split_header(raw: &str) -> Option<(&str, &str)> {
    let (name, value) = raw.split_once(':')?;
    Some((name.trim(), value.trim()))
}

/// 按环境变量构造 provider：
/// - `AA_API_KEY`：Bearer 鉴权；为空/缺失则不带鉴权；
/// - `AA_AUTH_HEADER`：`Name: value`，替换 Bearer（如 `x-api-key: ...`）；
/// - `AA_EXTRA_HEADER_n` / `AA_EXTRA_HEADER`：追加 header。
pub fn provider() -> OpenAiLanguageModelProvider {
    let mut provider = match std::env::var("AA_API_KEY") {
        Ok(key) if !key.is_empty() => OpenAiLanguageModelProvider::new(key),
        _ => OpenAiLanguageModelProvider::without_auth(),
    }
    .with_base_url(base_url());

    if let Ok(raw) = std::env::var("AA_AUTH_HEADER") {
        let (name, value) = split_header(&raw).expect("AA_AUTH_HEADER 必须是 `Name: value`");
        provider = provider.with_auth_header(name, value);
    }
    // 同一个变量不能重复设置，所以编号形式用于多个 header。
    for index in 0..8 {
        if let Ok(raw) = std::env::var(format!("AA_EXTRA_HEADER_{index}")) {
            let (name, value) = split_header(&raw).expect("AA_EXTRA_HEADER_n 必须是 `Name: value`");
            provider = provider.with_header(name, value);
        }
    }
    if let Ok(raw) = std::env::var("AA_EXTRA_HEADER") {
        let (name, value) = split_header(&raw).expect("AA_EXTRA_HEADER 必须是 `Name: value`");
        provider = provider.with_header(name, value);
    }
    provider
}
