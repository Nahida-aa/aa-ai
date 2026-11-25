//! 真 socket 的 mock 端点（`mock-server` feature）。
//!
//! 用途：让 **真实 HTTP 链路**离线可跑——settings → reqwest → provider 编码 /
//! SSE 解码 → agent loop。和 `fake::FakeClient` 的分工：
//!
//! - [`aa_language_model::fake`](https://docs.rs/aa_language_model)：进程内，测
//!   逻辑分支，不经过 wire；
//! - `MockServer`：绑 `127.0.0.1:0`，说厂商的 wire 协议，测编码 / 解码 / 装配。
//!
//! 响应 body 由各 provider crate 的 `test_support` 生成（只有它们知道自己的 wire
//! 形状）；本模块只做 HTTP 分帧、请求记录和事件 → body 的分发。生产构建不会
//! 带这段代码（feature 门控）。
//!
//! ```no_run
//! use aa_language_model::provider::CompletionEvent;
//! use aa_language_models::mock_server::{MockReply, MockServer, Wire};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let server = MockServer::start(Wire::OpenAi)?;
//! server.set_handler(|request| {
//!     if request.last_user_text().is_some_and(|text| text.starts_with("You are compacting")) {
//!         MockReply::text("## Goal\n…")
//!     } else {
//!         MockReply::text("ok")
//!     }
//! });
//! // 把 server.base_url()（OpenAI 是 `http://127.0.0.1:PORT/v1`）填进 settings。
//! println!("{}", server.base_url());
//! # Ok(())
//! # }
//! ```
//!
//! 只支持 `POST` + `Content-Length` 请求体；响应带 `Content-Length` 后立即关闭
//! 连接（不支持 keep-alive / chunked 请求）。这对测试客户端足够。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aa_language_model::provider::CompletionEvent;

/// 端点说哪家的 wire 协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// OpenAI / OpenAI 兼容端点：SSE，`POST /chat/completions`。
    OpenAi,
    /// OpenRouter：SSE，`POST /chat/completions`（`Stop` 延后到 `[DONE]`）。
    OpenRouter,
    /// Ollama 原生：NDJSON，`POST /api/chat`。
    Ollama,
}

impl Wire {
    /// 该 wire 下 provider 期望的 `base_url`：OpenAI 家族要带 `/v1`，Ollama 用根地址。
    fn base_url(self, addr: SocketAddr) -> String {
        match self {
            Self::OpenAi | Self::OpenRouter => format!("http://{addr}/v1"),
            Self::Ollama => format!("http://{addr}"),
        }
    }
}

/// 一次收到的请求（已解析成 JSON）。
#[derive(Debug, Clone)]
pub struct MockRequest {
    wire: Wire,
    json: serde_json::Value,
    stream: bool,
}

impl MockRequest {
    /// 请求 body（厂商 JSON，字段名保持 wire 原样）。
    pub fn json(&self) -> &serde_json::Value {
        &self.json
    }

    pub fn wire(&self) -> Wire {
        self.wire
    }

    /// `stream` 字段；缺失时按厂商默认（OpenAI 家族 `false`，Ollama `true`）。
    pub fn stream(&self) -> bool {
        self.stream
    }

    /// 最后一条 user 消息的纯文本。
    ///
    /// 三家的请求体都用 `messages[].{role,content}`，这里不区分 wire；`content`
    /// 是字符串或 OpenAI 的多段数组都支持（取第一段 text）。
    pub fn last_user_text(&self) -> Option<&str> {
        let messages = self.json.get("messages")?.as_array()?;
        for message in messages.iter().rev() {
            if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                continue;
            }
            return match message.get("content")? {
                serde_json::Value::String(text) => Some(text.as_str()),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .find_map(|part| part.get("text").and_then(serde_json::Value::as_str)),
                _ => None,
            };
        }
        None
    }
}

/// handler 的返回值。
#[derive(Debug, Clone)]
pub enum MockReply {
    /// 按请求的 `stream` 自动编码成 SSE / NDJSON 或非流式 JSON。
    Events(Vec<CompletionEvent>),
    /// 非 2xx：`{"error": ...}`（OpenAI 家族是对象，Ollama 是字符串）。
    HttpError { status: u16, message: String },
    /// 原样返回，用于构造畸形 body / 自定义 Content-Type。
    Raw {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
}

impl MockReply {
    /// 一段文本 + 正常结束。
    pub fn text(text: impl Into<String>) -> Self {
        Self::Events(vec![
            CompletionEvent::Text(text.into()),
            CompletionEvent::Stop(aa_language_model::provider::StopReason::EndTurn),
        ])
    }

    pub fn events(events: Vec<CompletionEvent>) -> Self {
        Self::Events(events)
    }
}

type Handler = dyn Fn(&MockRequest) -> MockReply + Send + Sync + 'static;

struct State {
    wire: Wire,
    handler: Mutex<Option<Arc<Handler>>>,
    requests: Mutex<Vec<MockRequest>>,
    shutdown: AtomicBool,
}

/// 绑本机随机端口的 mock 端点。`Drop` 时停线程。
pub struct MockServer {
    addr: SocketAddr,
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// 随机端口启动。
    pub fn start(wire: Wire) -> io::Result<Self> {
        Self::start_on(wire, 0)
    }

    /// 指定端口启动（`0` 表示随机）。
    pub fn start_on(wire: Wire, port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let state = Arc::new(State {
            wire,
            handler: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
        });
        let thread_state = state.clone();
        let thread = thread::Builder::new()
            .name("aa-mock-server".to_owned())
            .spawn(move || serve(listener, &thread_state))?;

        Ok(Self {
            addr,
            state,
            thread: Some(thread),
        })
    }

    /// 设置每个请求的应答。未设置时返回 500。
    pub fn set_handler<F>(&self, handler: F)
    where
        F: Fn(&MockRequest) -> MockReply + Send + Sync + 'static,
    {
        *self.state.handler.lock().expect("handler 锁") = Some(Arc::new(handler));
    }

    /// 填进 settings 的 `base_url`。
    pub fn base_url(&self) -> String {
        self.state.wire.base_url(self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 按到达顺序记录的请求（测试断言用）。
    pub fn requests(&self) -> Vec<MockRequest> {
        self.state.requests.lock().expect("requests 锁").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: TcpListener, state: &State) {
    while !state.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = serve_one(stream, state);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn serve_one(mut stream: TcpStream, state: &State) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let stream_flag = json.get("stream").and_then(serde_json::Value::as_bool);
    let request = MockRequest {
        wire: state.wire,
        stream: stream_flag.unwrap_or(matches!(state.wire, Wire::Ollama)),
        json,
    };
    state
        .requests
        .lock()
        .expect("requests 锁")
        .push(request.clone());

    let handler = state.handler.lock().expect("handler 锁").clone();
    let reply = match handler {
        Some(handler) => handler(&request),
        None => MockReply::HttpError {
            status: 500,
            message: "mock server has no handler".to_owned(),
        },
    };

    let (status, content_type, body) = encode(state.wire, &request, reply);
    write_response(&mut stream, status, &content_type, &body)
}

fn encode(wire: Wire, request: &MockRequest, reply: MockReply) -> (u16, String, String) {
    match reply {
        MockReply::Events(events) => {
            let (content_type, body) = match (wire, request.stream) {
                (Wire::OpenAi, true) => (
                    "text/event-stream",
                    aa_ai_openai::test_support::sse_body(&events),
                ),
                (Wire::OpenAi, false) => (
                    "application/json",
                    aa_ai_openai::test_support::completion_body(&events),
                ),
                (Wire::OpenRouter, true) => (
                    "text/event-stream",
                    aa_ai_openrouter::test_support::sse_body(&events),
                ),
                (Wire::OpenRouter, false) => (
                    "application/json",
                    aa_ai_openrouter::test_support::completion_body(&events),
                ),
                (Wire::Ollama, true) => (
                    "application/x-ndjson",
                    aa_ai_ollama::test_support::ndjson_body(&events),
                ),
                (Wire::Ollama, false) => (
                    "application/json",
                    aa_ai_ollama::test_support::chat_body(&events),
                ),
            };
            (200, content_type.to_owned(), body)
        }
        MockReply::HttpError { status, message } => {
            let body = match wire {
                Wire::Ollama => serde_json::json!({ "error": message }),
                Wire::OpenAi | Wire::OpenRouter => {
                    serde_json::json!({ "error": { "message": message, "type": "mock_error" } })
                }
            };
            (status, "application/json".to_owned(), body.to_string())
        }
        MockReply::Raw {
            status,
            content_type,
            body,
        } => (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        ),
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_user_text_handles_string_and_array_content() {
        let request = MockRequest {
            wire: Wire::OpenAi,
            json: serde_json::json!({
                "messages": [
                    { "role": "user", "content": "first" },
                    { "role": "assistant", "content": "reply" },
                    { "role": "user", "content": [{ "type": "text", "text": "latest" }] },
                ],
            }),
            stream: true,
        };
        assert_eq!(request.last_user_text(), Some("latest"));
    }

    #[test]
    fn base_url_carries_v1_for_openai_families_only() {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert_eq!(Wire::OpenAi.base_url(addr), "http://127.0.0.1:1234/v1");
        assert_eq!(Wire::OpenRouter.base_url(addr), "http://127.0.0.1:1234/v1");
        assert_eq!(Wire::Ollama.base_url(addr), "http://127.0.0.1:1234");
    }

    #[test]
    fn requests_are_recorded_and_errors_have_wire_specific_shape() {
        let server = MockServer::start(Wire::OpenAi).unwrap();
        server.set_handler(|_| MockReply::HttpError {
            status: 429,
            message: "slow down".to_owned(),
        });

        let response = post(&server, r#"{"messages":[],"stream":false}"#);
        assert!(response.starts_with("HTTP/1.1 429"), "{response}");
        assert!(response.contains("\"message\":\"slow down\""), "{response}");
        assert_eq!(server.requests().len(), 1);
    }

    fn post(server: &MockServer, body: &str) -> String {
        let mut stream = TcpStream::connect(server.addr()).unwrap();
        write!(
            stream,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
