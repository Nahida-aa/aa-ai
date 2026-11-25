//! 离线 mock 端点：用真实 provider 客户端跑 CLI（替代临时 Python 脚本）。
//!
//! ```bash
//! # OpenAI / OpenAI 兼容 wire（默认）
//! cargo run -p aa_language_models --features mock-server --example mock_endpoint -- 18099
//!
//! # Ollama 原生 wire
//! cargo run -p aa_language_models --features mock-server --example mock_endpoint -- 18099 ollama
//! ```
//!
//! 然后把打印出来的 `base_url` 填进 `settings.jsonc` 的
//! `openai_compatible.<id>`（或 `ollama`）section。
//!
//! 行为：普通请求回 `echo: <最后一条用户消息>`；压缩摘要请求（最后一条用户
//! 消息以 `You are compacting` 开头）回一个固定 checkpoint。摘要格式属于
//! agent 层的约定，所以这段判断只放在示例里，不在库里。

use std::thread;

use aa_language_models::mock_server::{MockReply, MockServer, Wire};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().map_or(Ok(18099), |raw| raw.parse())?;
    let wire = match args.next().as_deref() {
        Some("ollama") => Wire::Ollama,
        Some("openrouter") => Wire::OpenRouter,
        _ => Wire::OpenAi,
    };

    let server = MockServer::start_on(wire, port)?;
    server.set_handler(|request| {
        let text = request.last_user_text().unwrap_or_default();
        if text.starts_with("You are compacting") {
            MockReply::text("## Goal\nmock summary\n## Next Move\n1. continue")
        } else {
            MockReply::text(format!("echo: {text}"))
        }
    });

    eprintln!("# mock 端点（{wire:?}）：{}", server.base_url());
    eprintln!("# Ctrl-C 退出");
    // 服务器线程自己在跑；主线程只要不退出就行。
    loop {
        thread::park();
    }
}
