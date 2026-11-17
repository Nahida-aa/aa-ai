//! 列出 OpenAI 兼容端点的模型名（`GET {base_url}/models`）。
//!
//! ```bash
//! AA_BASE_URL="https://api.example.com/v1" \
//! AA_API_KEY="sk-..." \
//! cargo run -p aa_ai_openai --example models
//! ```
//!
//! 拿到 id 后填进 `AA_MODEL` 跑 `examples/chat.rs`。有些网关用 `x-api-key` 而不是
//! Bearer：`AA_AUTH_HEADER="x-api-key: ..."`。

mod support;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = support::provider();
    for model in provider.list_models().await? {
        match model.owned_by {
            Some(owner) => println!("{}\t{owner}", model.id),
            None => println!("{}", model.id),
        }
    }
    Ok(())
}
