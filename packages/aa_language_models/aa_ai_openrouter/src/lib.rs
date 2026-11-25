//! OpenRouter provider：OpenAI chat completions 形状 + OpenRouter 的厂商差异。
//!
//! 为什么单独一个 crate 而不是 `aa_ai_openai` 的一个分支：OpenRouter 是「常见厂商、
//! 协议相同」那一档（[ADR 0008](../../../docs/decisions/0008-provider-tiers.md)），
//! 差异是**硬差异**而不是可选装饰：
//!
//! - assistant 消息要能回放 `reasoning` / `reasoning_details`（思考模型 + 工具调用）；
//! - 思考强度是 `reasoning: { effort }`，不是顶层 `reasoning_effort`；
//! - 错误体的 `code` 是数字，并带 `metadata`；
//! - 模型列表带 `pricing` / `context_length` / `supported_parameters`；
//! - 应用归因用 `HTTP-Referer` / `X-Title`。
//!
//! 逐字段事实见 [`docs/compare/openrouter-wire.md`](../../../docs/compare/openrouter-wire.md)，
//! 取舍见 [ADR 0012](../../../docs/decisions/0012-openrouter-vendor.md)。
//!
//! # 作为库
//!
//! 和 `aa_ai_openai` 一样，这里没有全局 settings / 注册表 / 热重载，也**不读环境
//! 变量或配置文件**：密钥、端点、模型表都是应用的职责。一个实例 = 一份端点配置。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aa_ai_openrouter::OpenRouterLanguageModelProvider;
//!
//! let http: Arc<dyn aa_http_client::HttpClient> =
//!     Arc::new(aa_reqwest_client::builder().build()?);
//! let provider = OpenRouterLanguageModelProvider::new(http, "sk-or-...")
//!     .with_app_title("my-app")
//!     .with_app_url("https://app.example.com");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # 传输
//!
//! 不拥有 HTTP 客户端：一个进程一个连接池，由调用方构造
//! `Arc<dyn aa_http_client::HttpClient>` 后传进来
//! （[ADR 0010](../../../docs/decisions/0010-transport-object.md)）。

pub mod chat_completion;
pub mod models;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;

/// OpenRouter 的 API 根地址（含版本段；代码只拼 `/chat/completions`）。
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// 鉴权方式（wire 细节，只有本 crate 关心）。
enum Auth {
    /// `Authorization: Bearer <key>`（OpenRouter 官方端点）。
    Bearer(String),
    /// 自定义鉴权 header（兼容网关的 `x-api-key` 等）。
    Header { name: String, value: String },
    /// 不带鉴权（本地/内网 OpenRouter 兼容网关）。
    None,
}

/// OpenRouter 端点。
///
/// 只持有**端点数据**；HTTP 客户端是共享的 `Arc<dyn aa_http_client::HttpClient>`。
pub struct OpenRouterLanguageModelProvider {
    auth: Auth,
    base_url: String,
    /// `HTTP-Referer`：应用主页，OpenRouter 用于排行榜/归因（可选）。
    app_url: Option<String>,
    /// `X-Title`：应用名，OpenRouter 用于排行榜/归因（可选）。
    app_title: Option<String>,
    /// 额外请求 header（`(name, value)`，按顺序追加，同名后者覆盖前者）。
    headers: Vec<(String, String)>,
    http: Arc<dyn aa_http_client::HttpClient>,
}

impl OpenRouterLanguageModelProvider {
    /// 用 Bearer 鉴权构造。OpenRouter 的对话接口必须带密钥。
    pub fn new(http: Arc<dyn aa_http_client::HttpClient>, api_key: impl Into<String>) -> Self {
        Self::with_auth(http, Auth::Bearer(api_key.into()))
    }

    /// 不发送 `Authorization`（本地/内网 OpenRouter 兼容网关）。
    ///
    /// 不要用 `new("")` 代替：那会发出空的 `Authorization: Bearer `。
    pub fn without_auth(http: Arc<dyn aa_http_client::HttpClient>) -> Self {
        Self::with_auth(http, Auth::None)
    }

    fn with_auth(http: Arc<dyn aa_http_client::HttpClient>, auth: Auth) -> Self {
        Self {
            auth,
            base_url: DEFAULT_BASE_URL.to_owned(),
            app_url: None,
            app_title: None,
            headers: Vec::new(),
            http,
        }
    }

    /// 用自定义 header 鉴权，替换默认的 `Authorization: Bearer`。
    pub fn with_auth_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth = Auth::Header {
            name: name.into(),
            value: value.into(),
        };
        self
    }

    /// 指向别的 OpenRouter 兼容网关（代理、区域端点）。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// 设置 `HTTP-Referer`（应用主页）。
    pub fn with_app_url(mut self, app_url: impl Into<String>) -> Self {
        self.app_url = Some(app_url.into());
        self
    }

    /// 设置 `X-Title`（应用名）。
    pub fn with_app_title(mut self, app_title: impl Into<String>) -> Self {
        self.app_title = Some(app_title.into());
        self
    }

    /// 追加一个请求 header（同名时覆盖默认值）。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 把鉴权、归因与额外 header 变成 `HeaderMap`。
    ///
    /// 在进入 future 之前同步调用：非法 header 名/值直接变成错误，不 panic、不发请求。
    fn headers(
        &self,
    ) -> Result<aa_http_client::http::HeaderMap, aa_language_model::provider::LlmError> {
        use aa_http_client::http::{HeaderMap, HeaderName, HeaderValue};

        let mut map = HeaderMap::new();
        let mut put = |name: &str, value: &str| {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                aa_language_model::provider::LlmError::Transport(format!(
                    "invalid request header name {name:?}: {e}"
                ))
            })?;
            let value = HeaderValue::from_str(value).map_err(|e| {
                aa_language_model::provider::LlmError::Transport(format!(
                    "invalid request header value for {name:?}: {e}"
                ))
            })?;
            map.insert(name, value);
            Ok::<(), aa_language_model::provider::LlmError>(())
        };

        put("Content-Type", "application/json")?;
        match &self.auth {
            Auth::Bearer(key) => put("Authorization", &format!("Bearer {key}"))?,
            Auth::Header { name, value } => put(name, value)?,
            Auth::None => {}
        }
        if let Some(app_url) = &self.app_url {
            put("HTTP-Referer", app_url)?;
        }
        if let Some(app_title) = &self.app_title {
            put("X-Title", app_title)?;
        }
        // 用户 header 放最后：同名时覆盖上面的默认值。
        for (name, value) in &self.headers {
            put(name, value)?;
        }
        Ok(map)
    }

    /// 列出 OpenRouter 的模型（`GET /models`）。
    ///
    /// 公开接口，不需要密钥；返回里带 `pricing`，`ModelInfo::is_free()` 可直接
    /// 判断零价模型。
    pub async fn list_models(
        &self,
    ) -> Result<Vec<crate::models::ModelInfo>, aa_language_model::provider::LlmError> {
        let headers = self.headers()?;
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut resp = send_json(
            &self.http,
            aa_http_client::Method::GET,
            &url,
            headers,
            Vec::new(),
        )
        .await?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let raw = read_text(&mut resp).await.unwrap_or_default();
            return Err(aa_language_model::provider::LlmError::ProviderRejection {
                status,
                message: crate::chat_completion::response::error_message(&raw),
                transient: matches!(status, 408 | 425 | 429 | 500..=599),
            });
        }

        let body = aa_http_client::read_body(&mut resp)
            .await
            .map_err(|error| aa_language_model::provider::LlmError::Transport(error.to_string()))?;
        let list: crate::models::ModelList = serde_json::from_slice(&body)
            .map_err(|error| aa_language_model::provider::LlmError::Transport(error.to_string()))?;
        Ok(list.data)
    }
}

// ---------- HTTP 样板 ----------

/// 发一个请求（`body` 为空即 GET）。
async fn send_json(
    http: &Arc<dyn aa_http_client::HttpClient>,
    method: aa_http_client::Method,
    url: &str,
    headers: aa_http_client::http::HeaderMap,
    body: Vec<u8>,
) -> Result<
    aa_http_client::Response<aa_http_client::AsyncBody>,
    aa_language_model::provider::LlmError,
> {
    let mut builder = aa_http_client::Request::builder().method(method).uri(url);
    *builder
        .headers_mut()
        .expect("刚建的 builder 一定有 headers") = headers;
    let request = builder
        .body(aa_http_client::AsyncBody::from(body))
        .map_err(|error| {
            aa_language_model::provider::LlmError::Transport(format!("构造 HTTP 请求失败：{error}"))
        })?;
    http.send(request)
        .await
        .map_err(|error| aa_language_model::provider::LlmError::Transport(error.to_string()))
}

/// 把响应体读成文本（错误体用；非 UTF-8 不报错）。
async fn read_text(
    response: &mut aa_http_client::Response<aa_http_client::AsyncBody>,
) -> Result<String, aa_language_model::provider::LlmError> {
    let body = aa_http_client::read_body(response)
        .await
        .map_err(|error| aa_language_model::provider::LlmError::Transport(error.to_string()))?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

// ---------- LanguageModelClient ----------

impl aa_language_model::provider::LanguageModelClient for OpenRouterLanguageModelProvider {
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
        // 先同步把借用式 wire DTO 写成 owned 字节；`'a` 到此终结，不进 future。
        let body = match crate::chat_completion::request::encode(request, model) {
            Ok(bytes) => bytes,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let wants_stream = request.stream;

        let headers = match self.headers() {
            Ok(headers) => headers,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        Box::pin(async move {
            let mut resp =
                send_json(&http, aa_http_client::Method::POST, &url, headers, body).await?;

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let raw = read_text(&mut resp).await.unwrap_or_default();
                let err = aa_language_model::provider::LlmError::ProviderRejection {
                    status,
                    message: crate::chat_completion::response::error_message(&raw),
                    transient: matches!(status, 408 | 425 | 429 | 500..=599),
                };
                return Ok(stream::once(async move { Err(err) }).boxed());
            }

            if wants_stream {
                return Ok(sse_event_stream(resp.into_body()));
            }

            let body = aa_http_client::read_body(&mut resp)
                .await
                .map_err(|error| {
                    aa_language_model::provider::LlmError::Transport(error.to_string())
                })?;
            let parsed: crate::chat_completion::response::ChatCompletionResponse =
                serde_json::from_slice(&body).map_err(|error| {
                    aa_language_model::provider::LlmError::Transport(error.to_string())
                })?;
            Ok(
                stream::iter(crate::chat_completion::response::non_streaming_events(
                    parsed,
                ))
                .boxed(),
            )
        })
    }
}

// ---------- SSE ----------

/// 把响应体解成 `CompletionEvent` 流（`text/event-stream`）。
///
/// 每读到一块就解码，不把整个 body 缓冲起来。
fn sse_event_stream(reader: aa_http_client::AsyncBody) -> aa_language_model::provider::EventStream {
    use futures::AsyncReadExt as _;

    struct State {
        reader: aa_http_client::AsyncBody,
        decoder: crate::chat_completion::response::StreamDecoder,
        pending: VecDeque<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
        ended: bool,
    }

    let state = State {
        reader,
        decoder: crate::chat_completion::response::StreamDecoder::default(),
        pending: VecDeque::new(),
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
            let mut chunk = [0u8; 8192];
            match state.reader.read(&mut chunk).await {
                Ok(0) => {
                    // 流结束但没收到 `[DONE]` / `finish_reason` → 解码器报 `StreamEnded`。
                    let decoder = std::mem::take(&mut state.decoder);
                    state.pending.extend(decoder.finish());
                    state.ended = true;
                }
                Ok(read) => {
                    state.pending.extend(state.decoder.push(&chunk[..read]));
                }
                Err(error) => {
                    state.ended = true;
                    state
                        .pending
                        .push_back(Err(aa_language_model::provider::LlmError::Transport(
                            error.to_string(),
                        )));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    fn provider() -> super::OpenRouterLanguageModelProvider {
        let http = Arc::new(
            aa_reqwest_client::builder()
                .proxy(aa_http_client::Proxy::Direct)
                .build()
                .unwrap(),
        );
        super::OpenRouterLanguageModelProvider::new(http, "sk-or-test")
            .with_app_url("https://example.com")
            .with_app_title("aacode")
    }

    #[test]
    fn default_base_url_is_openrouter() {
        let provider = provider();
        assert_eq!(provider.base_url, super::DEFAULT_BASE_URL);
    }

    #[test]
    fn headers_include_auth_and_attribution() {
        let headers = provider().headers().unwrap();
        assert_eq!(headers["authorization"], "Bearer sk-or-test");
        assert_eq!(headers["http-referer"], "https://example.com");
        assert_eq!(headers["x-title"], "aacode");
    }

    #[test]
    fn extra_headers_override_defaults() {
        let provider = provider().with_header("X-Title", "override");
        let headers = provider.headers().unwrap();
        assert_eq!(headers["x-title"], "override");
    }

    #[test]
    fn attribution_headers_are_optional() {
        let http = Arc::new(
            aa_reqwest_client::builder()
                .proxy(aa_http_client::Proxy::Direct)
                .build()
                .unwrap(),
        );
        let provider = super::OpenRouterLanguageModelProvider::new(http, "sk-or-test");
        let headers = provider.headers().unwrap();
        assert!(!headers.contains_key("http-referer"));
        assert!(!headers.contains_key("x-title"));
    }

    #[test]
    fn invalid_header_is_an_error_not_a_panic() {
        let provider = provider().with_header("bad header", "x");
        assert!(provider.headers().is_err());
    }
}
