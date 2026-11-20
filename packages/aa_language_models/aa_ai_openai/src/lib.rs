//! OpenAI（或 OpenAI 兼容端点）provider：真连 LLM 服务器。
//!
//! 本 crate 只负责 **OpenAI 的 wire 形状**：
//! - [`chat_completion::request`]：请求 wire DTO（借用 canonical IR）+ 编码器
//!   （`to_chat_completion_request` 产出借用 DTO，`encode` 用 `to_writer` 写成 owned 字节）；
//! - [`chat_completion::response`]：响应 DTO（非流式 / SSE chunk / 错误 body）+
//!   `finish_reason`、`usage` 映射 + `StreamDecoder`；
//! - [`OpenAiLanguageModelProvider`]：HTTP 调用 + 响应 → `CompletionEvent`。
//!
//! 目录按**接口**分组：`chat_completion/` 是 `/chat/completions` 的请求 + 响应；
//! 将来接 Responses API 就并列加 `responses/`，两套协议不共用结构体。
//!
//! 两种模式：`request.stream == false` 时读整个 JSON body；`true` 时读
//! `text/event-stream`，逐帧产出 `Text` / `ToolUse`（按 `index` 归并增量）/ `Usage` /
//! `Stop`。两条路径都从 `finish_reason` 得出 `StopReason`（见
//! [`docs/decisions/0007-response-mapping.md`](../../../docs/decisions/0007-response-mapping.md)）。
//!
//! # 作为库：一个实例 = 一份端点配置
//!
//! 和 Zed / opencode 不同，这里没有全局 settings / 注册表 / 热重载，也**不读环境变量
//! 或配置文件**：配置、凭证来源、`id → provider` 表都是应用的职责。库只接收**已经
//! 解好的**端点数据，调用方按自己的配置构造实例：
//!
//! ```no_run
//! use aa_ai_openai::OpenAiLanguageModelProvider;
//!
//! let provider = OpenAiLanguageModelProvider::new("sk-...")
//!     .with_base_url("https://api.example.com/v1")
//!     .with_header("HTTP-Referer", "https://app.example.com")
//!     .with_header("X-Title", "my-app");
//! // 本地无鉴权端点：
//! let local = OpenAiLanguageModelProvider::without_auth()
//!     .with_base_url("http://localhost:11434/v1");
//! // 非 Bearer 鉴权（Azure 式）：
//! let azure = OpenAiLanguageModelProvider::without_auth()
//!     .with_auth_header("api-key", "secret");
//! ```
//!
//! 这就是「长尾兼容端点」那一档（[ADR 0008]）：端点、鉴权、额外 header 都是构造数据，
//! 不需要写 crate；模型表由应用持有（库只接收 `provider::Model`）。有硬差异的常见厂商
//! （思考回放、`response_format` 范围、`[DONE]` 语义）仍各开 `aa_ai_<vendor>`。
//!
//! [ADR 0008]: ../../../docs/decisions/0008-provider-tiers.md
//!
//! canonical IR 和 `LanguageModelClient` trait 在 `aa_language_model`：core 不依赖 `reqwest`，
//! 加一家 provider = 新增一个 crate，不动 core。这与 Zed 的
//! `language_model_core` + `open_ai` / `anthropic` / `google_ai` 布局一致。
//!
//! 字段名/形状写错在编译期失败，而不是等 JSON 被服务端拒绝。
//!
//! 类型名对齐 Zed 的 `OpenAiLanguageModelProvider`
//! （`crates/language_models/src/provider/open_ai.rs:50`）：Zed 里同一个 struct
//! 既实现 `LanguageModelProvider`（注册）又实现 `LanguageModelClient`（发请求），
//! 我们这里只有后者，因为「注册」那一半属于 `aa_language_models`。
//!
//! # 传输策略
//!
//! 所有实例共用一套 HTTP 传输配置（对齐 Zed `ReqwestClient::builder`）：10s 建连、
//! 30s TCP keep-alive / 连接池空闲回收、HTTP/2 keep-alive 15s/10s；读取有 120s 的
//! per-read idle 超时（收到任何字节就重置，不是整请求总时长），可用
//! [`OpenAiLanguageModelProvider::with_read_timeout`] 调整。代理见
//! [`OpenAiLanguageModelProvider::with_proxy`]。决策记录见
//! [`docs/decisions/0009-http-transport.md`](../../../docs/decisions/0009-http-transport.md)。

pub mod chat_completion;
pub mod models;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use futures::stream;

// ---------- HTTP 传输策略 ----------
//
// 超时/keep-alive 取值对齐 Zed `crates/reqwest_client/src/reqwest_client.rs`
// 的 `ReqwestClient::builder`（commit `4ed72e0135`）。区别只在 read_timeout：
// Zed 的 LLM 入口目前传 `None`，我们默认开启（见 [`DEFAULT_READ_TIMEOUT`]）。

/// 建连超时（DNS + TCP + TLS + 代理握手）。
///
/// 没有它，坏网络或挂掉的代理会让 `.send()` 长时间悬着，用户只看到「没反应」。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// TCP keep-alive：让 NAT/中间设备看到连接仍然活跃。
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// 空闲连接在池里放多久后丢弃。
///
/// 尽早丢掉「静默坏掉」的复用连接（NAT 超时、链路重置）：stale HTTP/2 连接
/// 是长连接端点 `BadRecordMac` 一类 TLS 错误的常见来源。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP/2 PING 间隔与等待 PONG 的上限。
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// 流式读取的默认 idle 超时：连续这么久没收到**任何字节**就放弃。
///
/// 这是「两次读取之间」的上限，**不是整请求总时长**：只要 provider 还在发增量
/// （包括 SSE 注释形式的 keep-alive），计时就会重置。取 120s 是因为推理阶段
/// 可能静默数十秒，而常见 provider 的 keep-alive 间隔是 15–30s；它只用来抓
/// 「连接已经死了，但 TCP 还没报错」的情况。
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// 出站路由。改它必须重建 [`reqwest::Client`]，所以和 `http` 一起存在实例上。
enum ProxyMode {
    /// 跟随环境变量（`ALL_PROXY` / `HTTPS_PROXY` / `NO_PROXY`），reqwest 默认。
    Environment,
    /// 显式代理 URL（`http://` / `https://`，可带 `user:pass@`）。
    Explicit(String),
    /// 强制直连，忽略环境变量代理。只给测试用：是否允许「直连」是应用层的
    /// 设置语义，不是库要暴露的策略。
    #[cfg(test)]
    Direct,
}

/// 按传输策略构造客户端。所有构造入口（`new` / `with_proxy` / ...）都走这里，
/// 避免某个 setter 重建客户端时把超时配置丢掉。
fn build_client(
    proxy: &ProxyMode,
    read_timeout: Option<Duration>,
) -> Result<reqwest::Client, aa_language_model::provider::LlmError> {
    use aa_language_model::provider::LlmError;

    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true);

    if let Some(read_timeout) = read_timeout {
        builder = builder.read_timeout(read_timeout);
    }

    match proxy {
        ProxyMode::Environment => {}
        #[cfg(test)]
        ProxyMode::Direct => {
            builder = builder.no_proxy();
        }
        ProxyMode::Explicit(proxy_url) => {
            let proxy = reqwest::Proxy::all(proxy_url.as_str()).map_err(|error| {
                LlmError::Unsupported(format!("代理 `{proxy_url}` 无效：{error}"))
            })?;
            // 显式代理仍然尊重 NO_PROXY（NO_PROXY 是「哪些主机直连」，与走哪个代理无关）。
            builder = builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_env()));
        }
    }

    builder.build().map_err(|error| {
        LlmError::Transport(format!("构建 HTTP 客户端失败：{}", describe_error(&error)))
    })
}

/// 把错误展开成「原因链」。
///
/// reqwest 的 `Display` 只有最外层（`error sending request for url (...)`），真正的
/// 原因——连接被拒、代理握手失败、证书错误——藏在 `source()` 里。配置了代理之后，
/// 不带原因链的报错基本没法定位（分不清是端点挂了还是代理挂了）。
pub(crate) fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// reqwest 错误 → 面向用户的文案。
///
/// 超时单独标出来：`read_timeout` 触发时 reqwest 的 `Display` 往往只是
/// `error decoding response body`，不点明「多久没收到数据」就分不清是端点慢、
/// 代理挂了还是连接已经被中间设备断开。
fn describe_transport(error: &reqwest::Error, read_timeout: Option<Duration>) -> String {
    let detail = describe_error(error);
    // `is_connect()` 也可能同时 `is_timeout()`（建连超时），先判连接阶段。
    if error.is_connect() {
        let kind = if error.is_timeout() {
            "连接超时"
        } else {
            "连接失败"
        };
        return format!("{kind}（DNS/TCP/TLS/代理握手）: {detail}");
    }
    if error.is_timeout() {
        return match read_timeout {
            Some(timeout) => format!(
                "读取超时：连续 {} 没有收到任何数据（连接可能已被代理或中间设备断开）: {detail}",
                format_duration(timeout)
            ),
            None => format!("请求超时: {detail}"),
        };
    }
    detail
}

/// 超时文案里的时长：`120 秒` / `200 毫秒`（避免亚秒级显示成「0 秒」）。
fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{} 秒", duration.as_secs())
    } else {
        format!("{} 毫秒", duration.as_millis())
    }
}

/// 鉴权方式（wire 细节，只有本 crate 关心）。
enum Auth {
    /// `Authorization: Bearer <key>`（OpenAI 与多数兼容端点）。
    Bearer(String),
    /// 自定义鉴权 header（Azure 的 `api-key` 等）。
    Header { name: String, value: String },
    /// 不带鉴权（本地 Ollama / vLLM / LM Studio）。
    None,
}

/// OpenAI / OpenAI 兼容端点的 provider。
pub struct OpenAiLanguageModelProvider {
    auth: Auth,
    /// 默认 `https://api.openai.com/v1`；可指向 Ollama / vLLM / 第三方网关。
    base_url: String,
    /// 额外请求 header（`(name, value)`，按顺序追加，同名后者覆盖前者）。
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    /// 出站路由；改它需要重建 `http`。
    proxy: ProxyMode,
    /// 每次读取的 idle 超时；`None` = 不设。
    read_timeout: Option<Duration>,
}

impl OpenAiLanguageModelProvider {
    /// 用 Bearer 鉴权构造。
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_auth(Auth::Bearer(api_key.into()))
    }

    /// 不发送 `Authorization`（本地或网关自带鉴权时用）。
    ///
    /// 不要用 `new("")` 代替：那会发出空的 `Authorization: Bearer `，部分端点会 400。
    pub fn without_auth() -> Self {
        Self::with_auth(Auth::None)
    }

    fn with_auth(auth: Auth) -> Self {
        Self {
            auth,
            base_url: "https://api.openai.com/v1".into(),
            headers: Vec::new(),
            http: build_client(&ProxyMode::Environment, Some(DEFAULT_READ_TIMEOUT))
                .expect("默认 HTTP 客户端配置必须有效"),
            proxy: ProxyMode::Environment,
            read_timeout: Some(DEFAULT_READ_TIMEOUT),
        }
    }

    /// 指向 OpenAI 兼容端点（Ollama / vLLM / 自托管 / 第三方网关）。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// 用自定义 header 鉴权，替换默认的 `Authorization: Bearer`。
    pub fn with_auth_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth = Auth::Header {
            name: name.into(),
            value: value.into(),
        };
        self
    }

    /// 追加一个请求 header（OpenRouter 的 `HTTP-Referer` / `X-Title` 等）。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 出站代理（`http://` / `https://`，可带 `user:pass@`）。
    ///
    /// 语义对齐 Zed 的顶层 `proxy` 设置：显式设置后**不再**读环境变量代理
    /// （`ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY`）——reqwest 一旦显式加 proxy
    /// 就会关掉 `auto_sys_proxy`。`NO_PROXY` 的绕过列表仍然生效。
    ///
    /// 不调用本方法时保持 reqwest 默认：自动读环境变量代理。
    ///
    /// **只负责机制，不负责策略**：这里接收的是应用层已经解好的 URL。
    /// 「哪个配置键叫 `proxy`」「空串 = 跟随环境变量」「拒绝 socks」都是应用层
    /// 设置（aacode 的 `normalize_proxy`）的事，库不读配置也不读 env。
    ///
    /// 返回 `Result` 是因为 `reqwest::Proxy::all` 会解析 URL、构建客户端也可能
    /// 失败；这是传输构造错误，不是「代理设置校验」。调用发生在构造期，不在
    /// `.await` 之后。
    pub fn with_proxy(
        mut self,
        proxy_url: impl AsRef<str>,
    ) -> Result<Self, aa_language_model::provider::LlmError> {
        let mode = ProxyMode::Explicit(proxy_url.as_ref().to_owned());
        self.http = build_client(&mode, self.read_timeout)?;
        self.proxy = mode;
        Ok(self)
    }

    /// 覆盖流式读取的 idle 超时（默认 [`DEFAULT_READ_TIMEOUT`]）。
    ///
    /// 这是「两次收到字节之间的最长间隔」，不是整请求总时长：健康的流式响应只要
    /// 持续有增量（包括 provider 的 SSE keep-alive 注释）就不会被中断。注意它同时
    /// 约束非流式响应：非流式请求在生成完成前收不到任何 body 字节，所以等价的
    /// 是「首字节上限」。
    pub fn with_read_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Self, aa_language_model::provider::LlmError> {
        self.http = build_client(&self.proxy, Some(timeout))?;
        self.read_timeout = Some(timeout);
        Ok(self)
    }

    /// 关闭读取 idle 超时。只有调用方自己负责取消（或端点确实需要无限静默）时才用。
    pub fn without_read_timeout(mut self) -> Result<Self, aa_language_model::provider::LlmError> {
        self.http = build_client(&self.proxy, None)?;
        self.read_timeout = None;
        Ok(self)
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
            Auth::Bearer(key) => put("Authorization", &format!("Bearer {key}"))?,
            Auth::Header { name, value } => put(name, value)?,
            Auth::None => {}
        }
        // 用户 header 放最后：同名时覆盖上面的默认值。
        for (name, value) in &self.headers {
            put(name, value)?;
        }
        Ok(map)
    }

    /// 列出端点声明的模型（`GET {base_url}/models`）。
    ///
    /// 兼容端点的模型名一般只能从这里拿到；返回顺序即端点给的顺序。
    /// 拿到 `id` 后填进 `aa_language_model::provider::Model::new(provider, id)`。
    pub async fn list_models(
        &self,
    ) -> Result<Vec<crate::models::ModelInfo>, aa_language_model::provider::LlmError> {
        let headers = self.headers()?;
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                aa_language_model::provider::LlmError::Transport(crate::describe_transport(
                    &e,
                    self.read_timeout,
                ))
            })?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let raw = resp.text().await.unwrap_or_default();
            return Err(aa_language_model::provider::LlmError::ProviderRejection {
                status,
                message: crate::chat_completion::response::error_message(&raw),
                transient: matches!(status, 408 | 425 | 429 | 500..=599),
            });
        }

        let list: crate::models::ModelList = resp.json().await.map_err(|e| {
            aa_language_model::provider::LlmError::Transport(crate::describe_transport(
                &e,
                self.read_timeout,
            ))
        })?;
        Ok(list.data)
    }
}

// ---------- LanguageModelClient ----------

impl aa_language_model::provider::LanguageModelClient for OpenAiLanguageModelProvider {
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
        // 每请求选择（stream 等）来自 `Request`，静态能力位来自 `Model`（0006 D4 / A 方案）。
        let body = match crate::chat_completion::request::encode(request, model) {
            Ok(bytes) => bytes,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let wants_stream = request.stream;

        // future 需要的全部东西都先变成 owned，才能是 `'static`。
        // header 在这里同步构造：非法配置直接返回错误，不进 future、不发请求。
        let headers = match self.headers() {
            Ok(headers) => headers,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let read_timeout = self.read_timeout;

        Box::pin(async move {
            let resp = http
                .post(url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    aa_language_model::provider::LlmError::Transport(crate::describe_transport(
                        &e,
                        read_timeout,
                    ))
                })?;

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let raw = resp.text().await.unwrap_or_default();
                let err = aa_language_model::provider::LlmError::ProviderRejection {
                    status,
                    // 只取 `error.message`，不要把整段 JSON / HTML 塞给上层。
                    message: crate::chat_completion::response::error_message(&raw),
                    transient: matches!(status, 408 | 425 | 429 | 500..=599),
                };
                return Ok(stream::once(async move { Err(err) }).boxed());
            }

            if wants_stream {
                // 立即把 response 交给流式解码器：不缓冲整个 body，边到边解。
                return Ok(sse_event_stream(resp.bytes_stream(), read_timeout));
            }

            let parsed: crate::chat_completion::response::ChatCompletionResponse =
                resp.json().await.map_err(|e| {
                    aa_language_model::provider::LlmError::Transport(crate::describe_transport(
                        &e,
                        read_timeout,
                    ))
                })?;
            Ok(stream::iter(non_streaming_events(parsed)).boxed())
        })
    }
}

/// 非流式 body → 事件列表（最后一项是 `Stop`）。
fn non_streaming_events(
    parsed: crate::chat_completion::response::ChatCompletionResponse,
) -> Vec<Result<aa_language_model::provider::CompletionEvent, aa_language_model::provider::LlmError>>
{
    use aa_language_model::provider::CompletionEvent;

    let mut events = Vec::new();
    let mut has_tool_calls = false;
    let mut finish_reason = None;

    if let Some(choice) = parsed.choices.first() {
        finish_reason = choice.finish_reason.as_deref();
        // 兼容端点的非流式思考：先于文本产出（DeepSeek 等）。
        if let Some(reasoning) = choice.message.reasoning() {
            events.push(Ok(CompletionEvent::Reasoning {
                text: reasoning.to_owned(),
                signature: None,
            }));
        }
        if let Some(text) = &choice.message.content
            && !text.is_empty()
        {
            events.push(Ok(CompletionEvent::Text(text.clone())));
        }
        for call in &choice.message.tool_calls {
            has_tool_calls = true;
            events.push(Ok(CompletionEvent::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: parse_arguments(&call.function.arguments),
                is_complete: true,
            }));
        }
    }

    // `usage` 放在 `Stop` 之前：上层看到 `Stop` 就可以收尾。
    if let Some(usage) = parsed.usage {
        events.push(Ok(CompletionEvent::Usage(usage.to_canonical())));
    }
    events.push(Ok(CompletionEvent::Stop(
        crate::chat_completion::response::final_stop_reason(finish_reason, has_tool_calls),
    )));
    events
}

/// OpenAI 把参数当字符串回（`arguments: "{\"path\":\"a\"}"`）；解析失败保留原文。
fn parse_arguments(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_owned()))
}

// ---------- SSE ----------

/// 把字节流解成 `CompletionEvent` 流（`text/event-stream`）。
///
/// 泛型 `S`/`B` 是为了不引入 `bytes` 依赖：只要能给出 `AsRef<[u8]>` 的分片即可。
/// 每个网络分片到货就解码，不把整个响应缓冲起来。
///
/// `read_timeout` 只用于把 idle 超时翻译成可读文案；超时本身由 reqwest 的
/// `read_timeout` 在每个 body read 上计时。
fn sse_event_stream<S, B>(
    inner: S,
    read_timeout: Option<Duration>,
) -> aa_language_model::provider::EventStream
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    struct State<S> {
        inner: Pin<Box<S>>,
        decoder: crate::chat_completion::response::StreamDecoder,
        pending: VecDeque<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
        /// 网络流已结束（或出错），只剩 pending 要吐。
        ended: bool,
    }

    let state = State {
        inner: Box::pin(inner),
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
            match state.inner.next().await {
                Some(Ok(chunk)) => {
                    state.pending.extend(state.decoder.push(chunk.as_ref()));
                }
                Some(Err(err)) => {
                    state.ended = true;
                    state
                        .pending
                        .push_back(Err(aa_language_model::provider::LlmError::Transport(
                            crate::describe_transport(&err, read_timeout),
                        )));
                }
                None => {
                    // 流结束但没收到 `[DONE]` / `finish_reason` → 解码器报 `StreamEnded`。
                    let decoder = std::mem::take(&mut state.decoder);
                    state.pending.extend(decoder.finish());
                    state.ended = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aa_language_model::provider::LanguageModelClient as _;
    use futures::StreamExt;

    fn make_model(id: &str) -> aa_language_model::provider::Model {
        aa_language_model::provider::Model::new("openai", id)
    }

    /// 测试专用：绕过环境变量代理，保证请求打到本地假端点。
    fn force_direct(
        mut provider: crate::OpenAiLanguageModelProvider,
    ) -> crate::OpenAiLanguageModelProvider {
        provider.http =
            crate::build_client(&crate::ProxyMode::Direct, provider.read_timeout).unwrap();
        provider.proxy = crate::ProxyMode::Direct;
        provider
    }

    #[test]
    fn encodes_parts_without_panicking() {
        let history = vec![
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::System,
                vec![aa_language_model::request::message::ContentPart::text(
                    "sys",
                )],
            )),
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![aa_language_model::request::message::ContentPart::text("hi")],
            )),
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::Assistant,
                vec![
                    aa_language_model::request::message::ContentPart::Reasoning {
                        text: Arc::from("think"),
                        signature: Some(Arc::from("sig")),
                    },
                    // 加密思考块没有 OpenAI 对应物，encoder 应跳过。
                    aa_language_model::request::message::ContentPart::RedactedThinking(Arc::from(
                        "cipher",
                    )),
                    aa_language_model::request::message::ContentPart::Text(Arc::from("calling")),
                    aa_language_model::request::message::ContentPart::tool_call(
                        "call_1",
                        "read_file",
                        serde_json::json!({"path": "a"}),
                    ),
                ],
            )),
            // 工具结果在 canonical IR 里是 user 轮的内容。
            Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![
                    aa_language_model::request::message::ContentPart::tool_result(
                        "call_1",
                        vec![aa_language_model::request::message::ToolResultPart::from(
                            "contents",
                        )],
                        false,
                    ),
                ],
            )),
        ];
        let req = aa_language_model::request::Request {
            messages: history,
            ..Default::default()
        };

        let model = make_model("gpt-test");
        let body =
            crate::chat_completion::request::to_chat_completion_request(&req, &model).unwrap();
        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["model"], "gpt-test");
        assert_eq!(json["stream"], false);
        // 未设置的 Option 字段不应出现在 body 里。
        assert!(json.get("tool_choice").is_none());
        assert!(json.get("response_format").is_none());
        assert!(json.get("max_completion_tokens").is_none());
        assert!(json.get("stream_options").is_none());
        assert!(json.get("parallel_tool_calls").is_none());

        let messages = json["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
        // 思考不回放，但文本和工具调用都在。
        assert_eq!(messages[2]["content"], "calling");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        // encoder 把它拆成独立的 role:"tool" 消息（IR 里没有 Tool 角色）。
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "contents");
    }

    #[test]
    fn rejects_custom_tools() {
        let req = aa_language_model::request::Request {
            tools: vec![Arc::new(aa_language_model::request::tools::Tool {
                name: "custom".to_owned(),
                description: String::new(),
                input: aa_language_model::request::tools::ToolInput::Custom {
                    format: Some(aa_language_model::request::tools::CustomToolFormat::Text),
                },
            })],
            ..Default::default()
        };

        let err = crate::chat_completion::request::to_chat_completion_request(
            &req,
            &make_model("gpt-test"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            aa_language_model::provider::LlmError::Unsupported(_)
        ));
    }

    // ---------- 能力位 ----------

    /// 一条 user 消息 → 编码后的 wire body。
    fn encode_user(
        parts: Vec<aa_language_model::request::message::ContentPart>,
        model: &aa_language_model::provider::Model,
    ) -> serde_json::Value {
        let req = aa_language_model::request::Request {
            messages: vec![Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                parts,
            ))],
            ..Default::default()
        };
        let body =
            crate::chat_completion::request::to_chat_completion_request(&req, model).unwrap();
        serde_json::to_value(&body).unwrap()
    }

    #[test]
    fn uses_developer_role_when_the_model_asks_for_it() {
        let req = aa_language_model::request::Request {
            messages: vec![Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::System,
                vec![aa_language_model::request::message::ContentPart::text(
                    "be terse",
                )],
            ))],
            ..Default::default()
        };

        let mut model = make_model("gpt-test");
        let body =
            crate::chat_completion::request::to_chat_completion_request(&req, &model).unwrap();
        assert_eq!(
            serde_json::to_value(&body).unwrap()["messages"][0]["role"],
            "system"
        );

        // o 系列 / GPT-5：同一个 System 语义，wire 上叫 developer。
        model.uses_developer_role = true;
        let body =
            crate::chat_completion::request::to_chat_completion_request(&req, &model).unwrap();
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["messages"][0]["role"], "developer");
        assert_eq!(json["messages"][0]["content"], "be terse");
    }

    #[test]
    fn tools_are_rejected_when_the_model_does_not_support_them() {
        let req = aa_language_model::request::Request {
            tools: vec![Arc::new(
                aa_language_model::request::tools::Tool::new_function(
                    "read".to_owned(),
                    String::new(),
                    serde_json::json!({ "type": "object" }),
                    false,
                ),
            )],
            ..Default::default()
        };

        let mut model = make_model("no-tools");
        model.supports_tools = false;
        let err =
            crate::chat_completion::request::to_chat_completion_request(&req, &model).unwrap_err();
        match err {
            aa_language_model::provider::LlmError::Unsupported(msg) => {
                assert!(msg.contains("supports_tools"), "{msg}");
                assert!(msg.contains("no-tools"), "{msg}");
            }
            other => panic!("期望 Unsupported，实际 {other:?}"),
        }
    }

    #[test]
    fn images_are_rejected_when_the_model_does_not_support_them() {
        let image = aa_language_model::request::message::Image {
            mime: Arc::from("image/png"),
            source: aa_language_model::request::message::ImageSource::Url(Arc::from(
                "https://example.com/a.png",
            )),
        };

        let err = crate::chat_completion::request::to_chat_completion_request(
            &aa_language_model::request::Request {
                messages: vec![Arc::new(aa_language_model::request::message::Message::new(
                    aa_language_model::request::message::Role::User,
                    vec![aa_language_model::request::message::ContentPart::Image(
                        Arc::new(image),
                    )],
                ))],
                ..Default::default()
            },
            &make_model("text-only"),
        )
        .unwrap_err();
        match err {
            aa_language_model::provider::LlmError::Unsupported(msg) => {
                assert!(msg.contains("supports_images"), "{msg}");
            }
            other => panic!("期望 Unsupported，实际 {other:?}"),
        }
    }

    #[test]
    fn text_and_image_become_multipart_in_original_order() {
        let image = aa_language_model::request::message::Image {
            mime: Arc::from("image/png"),
            source: aa_language_model::request::message::ImageSource::Url(Arc::from(
                "https://example.com/a.png",
            )),
        };
        let mut model = make_model("vision");
        model.supports_images = true;

        let json = encode_user(
            vec![
                aa_language_model::request::message::ContentPart::text("这是什么"),
                aa_language_model::request::message::ContentPart::Image(Arc::new(image)),
            ],
            &model,
        );
        let content = &json["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "这是什么");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn image_only_message_keeps_an_array_content() {
        let image = aa_language_model::request::message::Image {
            mime: Arc::from("image/png"),
            source: aa_language_model::request::message::ImageSource::Bytes(Arc::from(
                &b"hello"[..],
            )),
        };
        let mut model = make_model("vision");
        model.supports_images = true;

        let json = encode_user(
            vec![aa_language_model::request::message::ContentPart::Image(
                Arc::new(image),
            )],
            &model,
        );
        let content = &json["messages"][0]["content"];
        // 单张图片也必须用数组（字符串形状装不下）。
        assert_eq!(content[0]["type"], "image_url");
        // 字节转 data URL：hello 的 base64 是 aGVsbG8=。
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn raw_base64_image_is_wrapped_in_a_data_url() {
        let image = aa_language_model::request::message::Image {
            mime: Arc::from("image/jpeg"),
            source: aa_language_model::request::message::ImageSource::Base64(Arc::from("AAAA")),
        };
        let mut model = make_model("vision");
        model.supports_images = true;

        let json = encode_user(
            vec![aa_language_model::request::message::ContentPart::Image(
                Arc::new(image),
            )],
            &model,
        );
        assert_eq!(
            json["messages"][0]["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
    }

    #[test]
    fn multi_text_message_stays_a_plain_string() {
        let model = make_model("gpt-test");
        let json = encode_user(
            vec![
                aa_language_model::request::message::ContentPart::text("he"),
                aa_language_model::request::message::ContentPart::text("llo"),
            ],
            &model,
        );
        // 相邻文本合并，不变成多模态数组（兼容端点只认字符串）。
        assert_eq!(json["messages"][0]["content"], "hello");
    }

    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let req = aa_language_model::request::Request {
            tool_choice: Some(aa_language_model::request::ToolChoice::Required),
            ..Default::default()
        };
        let model = make_model("m");
        let body =
            crate::chat_completion::request::to_chat_completion_request(&req, &model).unwrap();
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("tool_choice").is_none(), "{json}");
    }

    // ---------- 响应侧 ----------

    fn parse(json: serde_json::Value) -> crate::chat_completion::response::ChatCompletionResponse {
        serde_json::from_value(json).unwrap()
    }

    fn unwrap_events(
        events: Vec<
            Result<
                aa_language_model::provider::CompletionEvent,
                aa_language_model::provider::LlmError,
            >,
        >,
    ) -> Vec<aa_language_model::provider::CompletionEvent> {
        events.into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn non_streaming_reports_tool_use_and_length_stop() {
        // 被截断的一轮：finish_reason=length 必须变成 MaxTokens，而不是 EndTurn。
        let parsed = parse(serde_json::json!({
            "choices": [{
                "message": {
                    "content": "partial",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" }
                    }]
                },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 9, "completion_tokens": 4 }
        }));

        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert!(matches!(
            events[0],
            aa_language_model::provider::CompletionEvent::Text(_)
        ));
        assert!(matches!(
            events[1],
            aa_language_model::provider::CompletionEvent::ToolUse { .. }
        ));
        assert_eq!(
            events[2],
            aa_language_model::provider::CompletionEvent::Usage(
                aa_language_model::provider::TokenUsage {
                    input_tokens: 9,
                    output_tokens: 4,
                }
            )
        );
        assert_eq!(
            events[3],
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::MaxTokens
            )
        );
    }

    #[test]
    fn non_streaming_without_finish_reason_falls_back_to_tool_calls() {
        // 有些兼容端点不报 finish_reason；有 tool_calls 就当 ToolUse。
        let parsed = parse(serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "not json" }
                    }]
                }
            }]
        }));

        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert_eq!(
            events[0],
            aa_language_model::provider::CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                // 半截/非 JSON 参数保留原文，不吞掉模型输出。
                input: serde_json::Value::String("not json".into()),
                is_complete: true,
            }
        );
        assert_eq!(
            events[1],
            aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::ToolUse
            )
        );
    }

    #[test]
    fn content_filter_finish_reason_is_refusal() {
        let parsed = parse(serde_json::json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "content_filter" }]
        }));
        let events = unwrap_events(crate::non_streaming_events(parsed));
        assert_eq!(
            events.last(),
            Some(&aa_language_model::provider::CompletionEvent::Stop(
                aa_language_model::provider::StopReason::Refusal
            ))
        );
    }

    #[test]
    fn sse_stream_emits_events_without_buffering_the_body() {
        let chunks = vec![
            Ok::<_, reqwest::Error>("data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n".as_bytes().to_vec()),
            Ok("data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n".as_bytes().to_vec()),
            Ok("data: [DONE]\n\n".as_bytes().to_vec()),
        ];

        let stream = crate::sse_event_stream(futures::stream::iter(chunks), None);
        let events = futures::executor::block_on(stream.collect::<Vec<_>>());

        assert_eq!(
            unwrap_events(events),
            vec![
                aa_language_model::provider::CompletionEvent::Text("he".into()),
                aa_language_model::provider::CompletionEvent::Text("llo".into()),
                aa_language_model::provider::CompletionEvent::Stop(
                    aa_language_model::provider::StopReason::EndTurn
                ),
            ]
        );
    }

    #[test]
    fn proxy_is_configuration_not_code() {
        let provider = crate::OpenAiLanguageModelProvider::new("sk-test")
            .with_base_url("https://api.example.com/v1")
            .with_proxy("http://127.0.0.1:8080")
            .expect("合法代理 URL 应该能构造客户端");
        // 代理只影响 reqwest 客户端；鉴权/base_url 不受影响。
        assert_eq!(
            provider.headers().unwrap()["authorization"]
                .to_str()
                .unwrap(),
            "Bearer sk-test"
        );
    }

    #[test]
    fn invalid_proxy_url_is_an_error_not_a_panic() {
        // `host:port` 会被 reqwest 当成 http 代理（容错行为）；真正非法的是坏 URL。
        let error = crate::OpenAiLanguageModelProvider::new("sk-test")
            .with_proxy("http://")
            .err()
            .expect("坏代理 URL 应该报错");
        assert!(error.to_string().contains("代理"), "{error}");
    }

    // ---------- 超时 ----------

    /// 起一个只吐一帧 SSE 然后卡住的假端点：验证 idle 超时在**有数据之后**触发，
    /// 而不是把整个流当成一个总时长上限。
    ///
    /// 用原始 TCP 而不是 hyper，是为了精确控制「发完一帧后不再写、也不关连接」。
    #[tokio::test]
    async fn read_timeout_fires_when_a_stream_goes_silent() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            // 读到请求头结束就回应；body 很小，留在内核缓冲里即可。
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let read = socket.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      \r\n",
                )
                .unwrap();
            let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n";
            socket
                .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                .unwrap();
            socket.write_all(event).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.flush().unwrap();
            // 关键：不写 [DONE]，也不关连接，让读取侧静默。
            std::thread::sleep(Duration::from_millis(500));
        });

        let provider = crate::OpenAiLanguageModelProvider::without_auth()
            .with_base_url(format!("http://{addr}/v1"))
            .with_read_timeout(Duration::from_millis(150))
            .unwrap();
        // 测试机可能配了全局代理；强制直连，保证打到本地假端点。
        // 「直连」不是库的公共策略，只在测试里改私有字段。
        let provider = force_direct(provider);

        let request = aa_language_model::request::Request {
            messages: vec![Arc::new(aa_language_model::request::message::Message::new(
                aa_language_model::request::message::Role::User,
                vec![aa_language_model::request::message::ContentPart::text("hi")],
            ))],
            stream: true,
            ..Default::default()
        };

        let started = Instant::now();
        let mut stream = provider
            .stream_completion(&make_model("m"), &request)
            .await
            .expect("请求应该建立成功");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        let elapsed = started.elapsed();
        server.join().unwrap();

        // 第一帧正常到达（有数据不会触发超时）。
        assert!(
            matches!(
                events.first(),
                Some(Ok(aa_language_model::provider::CompletionEvent::Text(text)))
                    if text.as_str() == "he"
            ),
            "{events:?}"
        );

        // 静默 150ms 后报可读的 idle 超时，而不是一直挂着。
        let error = events
            .iter()
            .find_map(|event| event.as_ref().err())
            .expect("静默后应该有超时错误");
        let message = error.to_string();
        assert!(message.contains("读取超时"), "{message}");
        assert!(message.contains("150 毫秒"), "{message}");
        assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn timeout_durations_are_formatted_readably() {
        assert_eq!(crate::format_duration(Duration::from_secs(120)), "120 秒");
        assert_eq!(
            crate::format_duration(Duration::from_millis(150)),
            "150 毫秒"
        );
    }

    #[test]
    fn endpoint_configuration_is_data_not_code() {
        let provider = crate::OpenAiLanguageModelProvider::new("sk-test")
            .with_base_url("https://api.example.com/v1/")
            .with_header("HTTP-Referer", "https://app.example.com")
            .with_header("X-Title", "aa-ai");
        let headers = provider.headers().unwrap();
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-test"
        );
        assert_eq!(
            headers.get("http-referer").unwrap().to_str().unwrap(),
            "https://app.example.com"
        );
        assert_eq!(headers.get("x-title").unwrap().to_str().unwrap(), "aa-ai");
        assert_eq!(
            headers.get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );

        // 无鉴权时不发 Authorization；自定义 header 鉴权替换 Bearer。
        let provider = crate::OpenAiLanguageModelProvider::without_auth();
        assert!(provider.headers().unwrap().get("authorization").is_none());

        let provider = crate::OpenAiLanguageModelProvider::without_auth()
            .with_auth_header("api-key", "secret");
        assert_eq!(
            provider
                .headers()
                .unwrap()
                .get("api-key")
                .unwrap()
                .to_str()
                .unwrap(),
            "secret"
        );
    }

    #[test]
    fn invalid_header_is_an_error_not_a_panic() {
        let provider = crate::OpenAiLanguageModelProvider::new("k").with_header("bad header", "x");
        assert!(matches!(
            provider.headers(),
            Err(aa_language_model::provider::LlmError::Transport(_))
        ));
    }
}
