//! 对着一个 OpenAI 兼容端点跑一次真实对话（流式或非流式）。
//!
//! 这个例子只依赖环境变量，方便把你手里的第三方兼容 API 逐个试通：
//!
//! ```bash
//! AA_BASE_URL="https://api.example.com/v1" \
//! AA_API_KEY="sk-..." \
//! AA_MODEL="some-model" \
//! cargo run -p aa_ai_openai --example chat
//! ```
//!
//! 可选变量：
//! - `AA_PROMPT`：用户消息，默认「用一句话介绍你自己」；
//! - `AA_STREAM`：`0` 走非流式（默认流式）；
//! - `AA_EXTRA_HEADER`：`Name: value`，可重复设置多个（OpenRouter 的
//!   `HTTP-Referer` / `X-Title` 等）；
//! - `AA_AUTH_HEADER`：`Name: value`，用自定义 header 鉴权（Azure 的 `api-key`），
//!   设置后不再发 `Authorization: Bearer`；
//! - 不设置 `AA_API_KEY` 且不设置 `AA_AUTH_HEADER` 时不发鉴权（本地 Ollama / vLLM）。
//!
//! 输出：思考用暗色、正文用默认色，结束时打印 `[stop] ...` 和 `[usage] ...`。

use std::sync::Arc;

use futures::StreamExt;

use aa_language_model::provider::CompletionEvent;
use aa_language_model::provider::Model;
use aa_language_model::request::Request;
use aa_language_model::request::message::ContentPart;
use aa_language_model::request::message::Message;
use aa_language_model::request::message::Role;

fn split_header(raw: &str) -> Option<(&str, &str)> {
    let (name, value) = raw.split_once(':')?;
    Some((name.trim(), value.trim()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url =
        std::env::var("AA_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
    let model_id = std::env::var("AA_MODEL").expect("必须设置 AA_MODEL");
    let prompt = std::env::var("AA_PROMPT").unwrap_or_else(|_| "用一句话介绍你自己".to_owned());

    let mut provider = match std::env::var("AA_API_KEY") {
        Ok(key) if !key.is_empty() => aa_ai_openai::OpenAiLanguageModelProvider::new(key),
        _ => aa_ai_openai::OpenAiLanguageModelProvider::without_auth(),
    }
    .with_base_url(base_url);

    if let Ok(raw) = std::env::var("AA_AUTH_HEADER") {
        let (name, value) = split_header(&raw).expect("AA_AUTH_HEADER 必须是 `Name: value`");
        provider = provider.with_auth_header(name, value);
    }
    // `AA_EXTRA_HEADER` 可以出现多次，但 std::env::vars 会去重，所以额外支持编号形式。
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

    let stream = std::env::var("AA_STREAM")
        .map(|value| value != "0")
        .unwrap_or(true);

    let request = Request {
        messages: vec![Arc::new(Message::new(
            Role::User,
            vec![ContentPart::text(prompt)],
        ))],
        stream,
        ..Default::default()
    };
    let model = Model::new("third-party", model_id.as_str());

    let mut events = aa_language_model::provider::LanguageModelClient::stream_completion(
        &provider, &model, &request,
    )
    .await?;

    while let Some(event) = events.next().await {
        match event? {
            CompletionEvent::Reasoning { text, .. } => {
                print!("\x1b[2m{text}\x1b[0m");
            }
            CompletionEvent::Text(text) => print!("{text}"),
            CompletionEvent::ToolUse { name, input, .. } => {
                eprintln!("\n[tool] {name} {input}");
            }
            CompletionEvent::Usage(usage) => {
                eprintln!(
                    "\n[usage] input={} output={}",
                    usage.input_tokens, usage.output_tokens
                );
            }
            CompletionEvent::Stop(reason) => eprintln!("\n[stop] {reason:?}"),
        }
    }
    println!();
    Ok(())
}
