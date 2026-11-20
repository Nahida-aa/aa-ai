//! Ollama 原生 `/api/chat` provider。
//!
//! 这是第二个**协议**（不是 OpenAI 兼容的影子）：NDJSON 行流、`message.thinking`、
//! 工具参数是对象、工具结果靠 `tool_name` 关联。逐条差异见 [`chat`]。
//!
//! # 作为库
//!
//! 和 `aa_ai_openai` 同一形状：一个实例 = 一份端点配置，传输是共享的
//! `Arc<aa_http_client::HttpClient>`（[ADR 0010](../../../docs/decisions/0010-transport-object.md)），
//! 不读 env / 配置文件、没有注册表。`base_url` 是**根地址**（默认
//! `http://localhost:11434`），代码自己拼 `/api/chat`——不要传 `/v1`，那是 OpenAI
//! 兼容 section 的语义。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aa_ai_ollama::OllamaLanguageModelProvider;
//!
//! let http = Arc::new(aa_http_client::HttpClient::builder().build()?);
//! let ollama = OllamaLanguageModelProvider::new(http)
//!     .with_base_url("http://localhost:11434");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # 与 `openai_compatible.ollama` 的区别
//!
//! 同一个 Ollama 服务器同时提供两套接口：
//!
//! - `http://localhost:11434/v1`（OpenAI 兼容）→ `language_model_providers.openai_compatible.ollama`，
//!   由 `aa_ai_openai` 处理；
//! - `http://localhost:11434`（原生）→ `language_model_providers.ollama`，由本 crate 处理。
//!
//! 原生接口多出来的东西：`thinking` 字段（不是兼容层的 `reasoning_content`）、
//! `num_predict` / `keep_alive` 这类服务端参数、`format` 的裸 schema 形状。

pub mod chat;
pub mod models;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;

/// Ollama 的默认根地址（Zed 的 `OLLAMA_API_URL`）。
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// 鉴权方式。
///
/// Ollama 本身没有鉴权，但常被放在反向代理后面（网关要求 Bearer 或自定义 header）。
/// 默认 [`Auth::None`]。
enum Auth {
    None,
    Bearer(String),
    Header { name: String, value: String },
}

/// Ollama 原生 `/api/chat` 的 provider。
pub struct OllamaLanguageModelProvider {
    auth: Auth,
    /// 根地址（不带 `/v1`，代码自己拼 `/api/chat`）。
    base_url: String,
    /// 额外请求 header（同名后者覆盖前者）。
    headers: Vec<(String, String)>,
    http: Arc<aa_http_client::HttpClient>,
}

impl OllamaLanguageModelProvider {
    /// 无鉴权构造（本地 Ollama 的常态）。
    pub fn new(http: Arc<aa_http_client::HttpClient>) -> Self {
        Self {
            auth: Auth::None,
            base_url: DEFAULT_BASE_URL.to_owned(),
            headers: Vec::new(),
            http,
        }
    }

    /// 指向别的端点（远程 Ollama / 反向代理）。传**根地址**。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// 反向代理要求 `Authorization: Bearer`。
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.auth = Auth::Bearer(token.into());
        self
    }

    /// 反向代理要求自定义鉴权 header。
    pub fn with_auth_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth = Auth::Header {
            name: name.into(),
            value: value.into(),
        };
        self
    }

    /// 追加一个请求 header。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 把鉴权与额外 header 变成 `HeaderMap`。
    ///
    /// 在进入 future 之前同步调用：非法 header 名/值直接变成错误，不 panic、不发请求。
    fn headers(&self) -> Result<reqwest::header::HeaderMap, aa_language_model::provider::LlmError> {
        let mut map = reqwest::header::HeaderMap::new();
        let mut put = |name: &str, value: &str| {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                aa_language_model::provider::LlmError::Transport(format!(
                    "invalid request header name {name:?}: {e}"
                ))
            })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                aa_language_model::provider::LlmError::Transport(format!(
                    "invalid request header value for {name:?}: {e}"
                ))
            })?;
            map.insert(name, value);
            Ok::<(), aa_language_model::provider::LlmError>(())
        };

        put("Content-Type", "application/json")?;
        match &self.auth {
            Auth::None => {}
            Auth::Bearer(token) => put("Authorization", &format!("Bearer {token}"))?,
            Auth::Header { name, value } => put(name, value)?,
        }
        for (name, value) in &self.headers {
            put(name, value)?;
        }
        Ok(map)
    }

    /// 列出本机模型（`GET {base_url}/api/tags`）。
    pub async fn list_models(
        &self,
    ) -> Result<Vec<crate::models::ModelInfo>, aa_language_model::provider::LlmError> {
        let headers = self.headers()?;
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .reqwest()
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                aa_language_model::provider::LlmError::Transport(self.http.describe(&e))
            })?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let raw = resp.text().await.unwrap_or_default();
            return Err(aa_language_model::provider::LlmError::ProviderRejection {
                status,
                message: crate::chat::response::error_message(&raw),
                transient: matches!(status, 408 | 425 | 429 | 500..=599),
            });
        }

        let list: crate::models::ModelList = resp.json().await.map_err(|e| {
            aa_language_model::provider::LlmError::Transport(self.http.describe(&e))
        })?;
        Ok(list.models)
    }
}

// ---------- LanguageModelClient ----------

impl aa_language_model::provider::LanguageModelClient for OllamaLanguageModelProvider {
    fn stream_completion(
        &self,
        model: &aa_language_model::provider::Model,
        request: &aa_language_model::request::Request,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        aa_language_model::provider::EventStream,
                        aa_language_model::provider::LlmError,
                    >,
                > + Send
                + 'static,
        >,
    > {
        // 先同步编码；`'a` 到此终结，不进 future（ADR 0006 D1）。
        let body = match crate::chat::request::encode(request, model) {
            Ok(bytes) => bytes,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let wants_stream = request.stream;
        let headers = match self.headers() {
            Ok(headers) => headers,
            Err(err) => return Box::pin(async move { Err(err) }),
        };

        let http = self.http.clone();
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        Box::pin(async move {
            let resp = http
                .reqwest()
                .post(url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|e| aa_language_model::provider::LlmError::Transport(http.describe(&e)))?;

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let raw = resp.text().await.unwrap_or_default();
                let error = aa_language_model::provider::LlmError::ProviderRejection {
                    status,
                    message: crate::chat::response::error_message(&raw),
                    transient: matches!(status, 408 | 425 | 429 | 500..=599),
                };
                return Ok(stream::once(async move { Err(error) }).boxed());
            }

            if wants_stream {
                return Ok(ndjson_event_stream(resp.bytes_stream(), http));
            }

            // `stream: false` 的 body 是同一个形状的**单个对象**：喂给同一个解码器，
            // 由 `finish()` 处理「没有结尾换行」的情况。
            let raw = resp
                .text()
                .await
                .map_err(|e| aa_language_model::provider::LlmError::Transport(http.describe(&e)))?;
            let mut decoder = crate::chat::response::StreamDecoder::default();
            let mut events = decoder.push(raw.as_bytes());
            events.extend(decoder.finish());
            Ok(stream::iter(events).boxed())
        })
    }
}

// ---------- NDJSON 流 ----------

/// 把字节流解成 `CompletionEvent` 流（NDJSON：一行一个对象）。
///
/// 泛型 `S`/`B` 是为了不引入 `bytes` 依赖（与 `aa_ai_openai` 同一手法）。
/// 每个网络分片到货就解码，不把整个响应缓冲起来。
fn ndjson_event_stream<S, B>(
    inner: S,
    http: Arc<aa_http_client::HttpClient>,
) -> aa_language_model::provider::EventStream
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    let state = NdjsonState {
        inner: Box::pin(inner),
        decoder: crate::chat::response::StreamDecoder::default(),
        pending: VecDeque::new(),
        http,
        ended: false,
    };

    stream::unfold(state, move |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            if state.ended {
                return None;
            }
            match state.inner.next().await {
                Some(Ok(chunk)) => {
                    let events = state.decoder.push(chunk.as_ref());
                    state.pending.extend(events);
                }
                Some(Err(err)) => {
                    state.ended = true;
                    let message = state.http.describe(&err);
                    state
                        .pending
                        .push_back(Err(aa_language_model::provider::LlmError::Transport(
                            message,
                        )));
                }
                None => {
                    // 流结束但没收到 `done: true` → 解码器报 `StreamEnded`。
                    let decoder = std::mem::take(&mut state.decoder);
                    state.pending.extend(decoder.finish());
                    state.ended = true;
                }
            }
        }
    })
    .boxed()
}

/// [`ndjson_event_stream`] 的状态：字节流 + 解码器 + 待吐事件。
struct NdjsonState<S> {
    inner: Pin<Box<S>>,
    decoder: crate::chat::response::StreamDecoder,
    pending: VecDeque<
        Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>,
    >,
    http: Arc<aa_http_client::HttpClient>,
    /// 网络流已结束（或出错），只剩 `pending` 要吐。
    ended: bool,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aa_language_model::provider::LanguageModelClient as _;
    use futures::StreamExt;

    fn model(id: &str) -> aa_language_model::provider::Model {
        aa_language_model::provider::Model::new("ollama", id)
    }

    /// 测试专用传输：绕过环境变量代理，保证请求打到本地假端点。
    fn direct_http() -> Arc<aa_http_client::HttpClient> {
        Arc::new(
            aa_http_client::HttpClient::builder()
                .proxy(aa_http_client::Proxy::Direct)
                .build()
                .expect("测试用 HTTP 客户端必须能构造"),
        )
    }

    fn user_request(stream: bool) -> aa_language_model::request::Request {
        aa_language_model::request::Request {
            messages: vec![Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![aa_language_model::request::message::ContentPart::text("hi")],
            ))],
            stream,
            ..Default::default()
        }
    }

    /// 起一个只回固定字节的本地端点；返回 (地址, 收到的**完整**请求文本)。
    ///
    /// 用原始 TCP 而不是 hyper：要精确控制「响应发送到哪、何时关连接」。
    /// 返回完整请求（请求行 + header + body），这样路径/方法/请求体都能断言。
    fn serve_once(response: String) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            // 读到请求头结束 + body（小请求一次读完即可）。
            let header_end = loop {
                let read = socket.read(&mut buf).unwrap();
                if read == 0 {
                    break request.len();
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            // 有 Content-Length 就等 body 到齐（否则只看 header）。
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = socket.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let raw = String::from_utf8_lossy(&request).into_owned();

            socket.write_all(response.as_bytes()).unwrap();
            socket.flush().unwrap();
            // 主动关闭：让客户端看到流结束。
            drop(socket);
            raw
        });
        (addr, handle)
    }

    /// 固定长度的 JSON 响应（非流式 / 错误体）。
    fn json_response(status_line: &str, body: &str) -> String {
        format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// chunked 的 NDJSON 响应：每行一个 chunk（行边界落在 chunk 内，更接近真实端点）。
    fn chunked_ndjson(lines: &[&str]) -> String {
        let mut response = String::from(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/x-ndjson\r\n\
             Transfer-Encoding: chunked\r\n\r\n",
        );
        for line in lines {
            let payload = format!("{line}\n");
            response.push_str(&format!("{:x}\r\n{payload}\r\n", payload.len()));
        }
        response.push_str("0\r\n\r\n");
        response
    }

    #[tokio::test]
    async fn streaming_ndjson_becomes_events() {
        let (addr, server) = serve_once(chunked_ndjson(&[
            r#"{"message":{"role":"assistant","content":"Hel"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":5,"eval_count":2}"#,
        ]));

        let provider = super::OllamaLanguageModelProvider::new(direct_http())
            .with_base_url(format!("http://{addr}"));

        let mut stream = provider
            .stream_completion(&model("qwen2.5:0.5b"), &user_request(true))
            .await
            .expect("请求应该建立成功");

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        let raw_request = server.join().unwrap();

        assert_eq!(
            events,
            vec![
                aa_language_model::provider::CompletionEvent::Text("Hel".into()),
                aa_language_model::provider::CompletionEvent::Text("lo".into()),
                aa_language_model::provider::CompletionEvent::Usage(
                    aa_language_model::provider::TokenUsage {
                        input_tokens: 5,
                        output_tokens: 2
                    }
                ),
                aa_language_model::provider::CompletionEvent::Stop(
                    aa_language_model::provider::StopReason::EndTurn
                ),
            ]
        );
        // 流式请求自带 `"stream":true`。
        assert!(raw_request.contains(r#""stream":true"#), "{raw_request}");
    }

    #[tokio::test]
    async fn non_streaming_body_becomes_events() {
        let (addr, server) = serve_once(json_response(
            "HTTP/1.1 200 OK",
            r#"{"message":{"role":"assistant","content":"hello"},"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":1}"#,
        ));

        let provider = super::OllamaLanguageModelProvider::new(direct_http())
            .with_base_url(format!("http://{addr}"));

        let mut stream = provider
            .stream_completion(&model("qwen2.5:0.5b"), &user_request(false))
            .await
            .expect("请求应该建立成功");

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        let raw_request = server.join().unwrap();

        assert_eq!(
            events,
            vec![
                aa_language_model::provider::CompletionEvent::Text("hello".into()),
                aa_language_model::provider::CompletionEvent::Usage(
                    aa_language_model::provider::TokenUsage {
                        input_tokens: 3,
                        output_tokens: 1
                    }
                ),
                aa_language_model::provider::CompletionEvent::Stop(
                    aa_language_model::provider::StopReason::EndTurn
                ),
            ]
        );
        // 非流式请求把 `stream` 发成 false（同一个 DTO，字段必填）。
        assert!(raw_request.contains(r#""stream":false"#), "{raw_request}");
    }

    #[tokio::test]
    async fn non_2xx_body_becomes_provider_rejection() {
        let (addr, server) = serve_once(json_response(
            "HTTP/1.1 404 Not Found",
            r#"{"error":"model 'nope' not found"}"#,
        ));

        let provider = super::OllamaLanguageModelProvider::new(direct_http())
            .with_base_url(format!("http://{addr}"));

        let mut stream = provider
            .stream_completion(&model("nope"), &user_request(true))
            .await
            .expect("HTTP 层不是错误：错误在流里");

        // 错误作为流的第一项到达（与 aa_ai_openai 一致）。
        let event = stream.next().await.expect("应该有错误事件");
        server.join().unwrap();
        match event {
            Err(aa_language_model::provider::LlmError::ProviderRejection {
                status,
                message,
                ..
            }) => {
                assert_eq!(status, 404);
                assert!(message.contains("not found"), "{message}");
            }
            other => panic!("应该是 ProviderRejection：{other:?}"),
        }
    }

    #[tokio::test]
    async fn request_path_is_api_chat_under_the_root_url() {
        let (addr, server) = serve_once(json_response(
            "HTTP/1.1 200 OK",
            r#"{"message":{"role":"assistant","content":"ok"},"done":true,"done_reason":"stop"}"#,
        ));

        // 故意带尾部斜杠：`/api/chat` 不能被拼成 `//api/chat`。
        let provider = super::OllamaLanguageModelProvider::new(direct_http())
            .with_base_url(format!("http://{addr}/"));

        let _ = provider
            .stream_completion(&model("m"), &user_request(false))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let raw_request = server.join().unwrap();

        assert!(
            raw_request.starts_with("POST /api/chat HTTP/1.1"),
            "路径应该是 /api/chat（不是 /v1/chat/completions）：{raw_request}"
        );
        // 无鉴权时不该自发 `Authorization`。
        assert!(
            !raw_request.to_ascii_lowercase().contains("authorization:"),
            "{raw_request}"
        );
    }

    #[tokio::test]
    async fn list_models_hits_api_tags_and_returns_names() {
        let (addr, server) = serve_once(json_response(
            "HTTP/1.1 200 OK",
            r#"{"models":[{"name":"qwen2.5:0.5b"},{"name":"llama3.2:latest"}]}"#,
        ));

        let provider = super::OllamaLanguageModelProvider::new(direct_http())
            .with_base_url(format!("http://{addr}"));

        let models = provider.list_models().await.expect("应该拿到模型列表");
        let raw_request = server.join().unwrap();

        assert_eq!(
            models
                .iter()
                .map(super::models::ModelInfo::id)
                .collect::<Vec<_>>(),
            vec!["qwen2.5:0.5b", "llama3.2:latest"]
        );
        assert!(
            raw_request.starts_with("GET /api/tags HTTP/1.1"),
            "模型列表走 /api/tags，不是 /v1/models：{raw_request}"
        );
    }
}
