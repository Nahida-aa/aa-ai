//! 真 socket 的 mock 端点：用**真实 provider 客户端**（reqwest + 编码/解码）跑通。
//!
//! 这些测试和 `FakeHttpClient` 的测试互补：那边替换 `HttpClient`，这边连端口，
//! 覆盖请求行 / header / Content-Length / 连接关闭等 HTTP 细节。

#![cfg(feature = "mock-server")]

use aa_language_model::provider::{LanguageModelCompletionEvent, StopReason, TokenUsage};
use aa_language_models::mock_server::{MockReply, MockServer, Wire};

mod support {
    use std::sync::Arc;

    use aa_http_client::{HttpClient, Proxy};
    use aa_language_model::provider::LanguageModelCompletionEvent;
    use aa_language_model::request::Request;
    use aa_language_model::request::message::{ContentPart, Message, Role};
    use futures::StreamExt;

    pub fn http() -> Arc<dyn HttpClient> {
        // 测试不走环境代理：本地回环不需要，也避免 CI 上的 http_proxy 干扰。
        Arc::new(
            aa_reqwest_client::builder()
                .proxy(Proxy::Direct)
                .build()
                .expect("构造 reqwest 客户端"),
        )
    }

    pub fn model() -> aa_language_model::provider::LanguageModel {
        aa_language_model::provider::LanguageModel::new("mock", "mock")
    }

    pub fn request(stream: bool) -> Request {
        Request {
            messages: vec![Arc::new(Message::new(
                Role::User,
                vec![ContentPart::text("hi")],
            ))],
            stream,
            ..Default::default()
        }
    }

    pub async fn collect_raw(
        provider: &dyn aa_language_model::provider::LanguageModelClient,
        request: &Request,
    ) -> Vec<Result<LanguageModelCompletionEvent, aa_language_model::provider::LlmError>> {
        provider
            .stream_completion(&model(), request)
            .await
            .expect("应该建立流")
            .collect()
            .await
    }

    pub async fn collect(
        provider: &dyn aa_language_model::provider::LanguageModelClient,
        request: &Request,
    ) -> Vec<LanguageModelCompletionEvent> {
        collect_raw(provider, request)
            .await
            .into_iter()
            .map(|event| event.expect("事件不应报错"))
            .collect()
    }
}

#[tokio::test]
async fn openai_streams_over_a_real_socket() {
    let server = MockServer::start(Wire::OpenAi).unwrap();
    server.set_handler(|request| {
        assert!(request.stream());
        assert_eq!(request.last_user_text(), Some("hi"));
        MockReply::events(vec![
            LanguageModelCompletionEvent::Text("hello".into()),
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 2,
                output_tokens: 1,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ])
    });

    let provider =
        aa_language_models::openai::OpenAiLanguageModelProvider::without_auth(support::http())
            .with_base_url(server.base_url());
    let events = support::collect(&provider, &support::request(true)).await;

    assert_eq!(
        events,
        vec![
            LanguageModelCompletionEvent::Text("hello".into()),
            // OpenAI 的 usage 帧在 finish_reason 之后也会被解出来，顺序按 body 原样。
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 2,
                output_tokens: 1,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ]
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn openai_non_streaming_body_is_served_for_stream_false() {
    let server = MockServer::start(Wire::OpenAi).unwrap();
    server.set_handler(|request| {
        assert!(!request.stream());
        MockReply::text("plain")
    });

    let provider =
        aa_language_models::openai::OpenAiLanguageModelProvider::without_auth(support::http())
            .with_base_url(server.base_url());
    let events = support::collect(&provider, &support::request(false)).await;

    assert_eq!(
        events,
        vec![
            LanguageModelCompletionEvent::Text("plain".into()),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn openrouter_streams_with_deferred_stop() {
    let server = MockServer::start(Wire::OpenRouter).unwrap();
    server.set_handler(|_| {
        MockReply::events(vec![
            LanguageModelCompletionEvent::Text("hel".into()),
            LanguageModelCompletionEvent::Text("lo".into()),
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ])
    });

    let provider = aa_language_models::openrouter::OpenRouterLanguageModelProvider::without_auth(
        support::http(),
    )
    .with_base_url(server.base_url());
    let events = support::collect(&provider, &support::request(true)).await;

    assert_eq!(
        events,
        vec![
            LanguageModelCompletionEvent::Text("hel".into()),
            LanguageModelCompletionEvent::Text("lo".into()),
            // Stop 必须排在 Usage 后面（OpenRouter 解码器在 [DONE] 才收尾）。
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn ollama_serves_ndjson_over_a_real_socket() {
    let server = MockServer::start(Wire::Ollama).unwrap();
    server.set_handler(|request| {
        assert!(request.stream());
        MockReply::events(vec![
            LanguageModelCompletionEvent::Text("你好".into()),
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ])
    });

    let provider = aa_language_models::ollama::OllamaLanguageModelProvider::new(support::http())
        .with_base_url(server.base_url());
    let events = support::collect(&provider, &support::request(true)).await;

    assert_eq!(
        events,
        vec![
            LanguageModelCompletionEvent::Text("你好".into()),
            LanguageModelCompletionEvent::Usage(TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
            }),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn http_errors_keep_the_provider_rejection_shape() {
    let server = MockServer::start(Wire::OpenAi).unwrap();
    server.set_handler(|_| MockReply::HttpError {
        status: 429,
        message: "slow down".into(),
    });

    let provider =
        aa_language_models::openai::OpenAiLanguageModelProvider::without_auth(support::http())
            .with_base_url(server.base_url());
    let events = support::collect_raw(&provider, &support::request(true)).await;
    // 429 在 provider 里是「流里的一条 Err」，这里用 Debug 匹配，避免依赖内部字段。
    let error = events.first().expect("错误也应该以事件形式出现");
    assert!(format!("{error:?}").contains("slow down"), "{error:?}");
}

#[test]
fn the_server_stops_its_thread_on_drop() {
    let server = MockServer::start(Wire::OpenAi).unwrap();
    assert!(!server.base_url().is_empty());
    drop(server);
}
