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
//! - `AA_EXTRA_HEADER_n`（或 `AA_EXTRA_HEADER`）：`Name: value`，额外 header
//!   （OpenRouter 的 `HTTP-Referer` / `X-Title` 等）；
//! - `AA_AUTH_HEADER`：`Name: value`，用自定义 header 鉴权（`api-key` / `x-api-key`），
//!   设置后不再发 `Authorization: Bearer`；
//! - 不设置 `AA_API_KEY` 且不设置 `AA_AUTH_HEADER` 时不发鉴权（本地 Ollama / vLLM）。
//!
//! 不知道模型名时先跑 `--example models` 列出端点返回的 id。
//!
//! 输出：思考用暗色、正文用默认色，结束时打印 `[stop] ...` 和 `[usage] ...`。

mod support;

use std::sync::Arc;

use futures::StreamExt;

use aa_language_model::provider::CompletionEvent;
use aa_language_model::provider::Model;
use aa_language_model::request::Request;
use aa_language_model::request::message::ContentPart;
use aa_language_model::request::message::Message;
use aa_language_model::request::message::Role;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model_id = std::env::var("AA_MODEL").expect("必须设置 AA_MODEL");
    let prompt = std::env::var("AA_PROMPT").unwrap_or_else(|_| "用一句话介绍你自己".to_owned());
    let provider = support::provider();

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
