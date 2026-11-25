# 0016. wire 级 mock：生成器跟 provider 走，服务器放汇总 crate

> 状态：accepted
> 日期：2026-11-25
> 关联：[0014](0014-fake-in-core.md)（进程内 fake）、
> [`packages/aa_language_models/src/mock_server.rs`](../../packages/aa_language_models/src/mock_server.rs)、
> [`packages/aa_language_models/aa_ai_openai/src/test_support.rs`](../../packages/aa_language_models/aa_ai_openai/src/test_support.rs)

## 背景与问题

离线验证一直有两层，但只有一层有库支持：

1. **逻辑层**：`fake::FakeClient`（`aa_language_model/fake`）——进程内返回
   `CompletionEvent`，覆盖 agent loop / 压缩 / 会话，不碰 wire。
2. **协议层**：需要真实的 HTTP + SSE / NDJSON。此前只能手写 SSE 字符串，
   或者起一个临时 Python 脚本当端点。

第二层的成本不在 socket，而在**生成各家 wire 格式正确的响应体**：
`data:` 分帧、`[DONE]`、OpenRouter 的 `Stop` 延后、Ollama 的 `done_reason`、
思考字段的三种别名。手写字符串会和解码器悄悄漂移；Python 脚本则完全脱离
Rust 类型，改动 provider 后没人记得同步。

## 当时的想法（选项）

1. **只做 `FakeHttpClient` 的便利层**（不绑端口）
   - 覆盖 encoder / 解码；但 CLI 的真实链路（settings → reqwest）仍然测不到。
2. **单独一个 `aa_mock_server` crate**
   - 隔离干净；但按 [0014](0014-fake-in-core.md) 的标准，一个测试服务器
     不值得独立 crate，还要把全部 provider 拉进来。
3. **wire 生成器放各 provider，服务器放汇总 crate（采纳）**
   - 生成器与解码器同一个 crate，字段改名时一起编译；服务器只做 HTTP 分帧。

## 决策

### D1：wire body 生成器放各 provider，`test-support` feature 门控

```rust
// aa_ai_openai / aa_ai_openrouter
pub fn sse_body(events: &[CompletionEvent]) -> String;
pub fn completion_body(events: &[CompletionEvent]) -> String;

// aa_ai_ollama
pub fn ndjson_body(events: &[CompletionEvent]) -> String;
pub fn chat_body(events: &[CompletionEvent]) -> String;
```

- 只依赖 `CompletionEvent`，不依赖请求侧结构；
- OpenAI 与 OpenRouter **不共享**生成器：两家响应形状接近但不同
  （`reasoning` vs `reasoning_content`、`reasoning_details`、`Stop` 延后），
  共享会把差异藏回一个 crate（与 [0012](0012-openrouter-vendor.md) 同样的理由）；
- 每个生成器都有「生成 → 现有解码器 → 事件相等」的往返测试，字段漂移会在
  测试里暴露，而不是等 mock 端点行为不对才发现。

### D2：`MockServer` 放 `aa_language_models`，`mock-server` feature 门控

```rust
let server = MockServer::start(Wire::OpenAi)?;   // 绑 127.0.0.1:0
server.set_handler(|request| {
    if request.last_user_text() == Some("hi") { MockReply::text("hello") }
    else { MockReply::HttpError { status: 429, message: "slow down".into() } }
});
server.base_url();  // OpenAI 家族带 /v1，Ollama 用根地址
```

- `std::net::TcpListener` + 一个线程（5ms 轮询 shutdown），不引入 hyper / axum，
  也不要求调用方有 tokio runtime；`Drop` 时停线程；
- 只支持 `POST` + `Content-Length`，响应 `Connection: close`：测试客户端足够，
  不实现 keep-alive / chunked 请求；
- 请求按 ISO 原样记录，`last_user_text` 对三家通用（都是
  `messages[].{role,content}`）；
- 按请求里的 `stream` 自动选 SSE / NDJSON / 非流式 body，不需要 handler 关心。

### D3：示例替代临时脚本

`examples/mock_endpoint.rs`（`--features mock-server`）跑一个固定端口的端点：
普通请求回 `echo:`，压缩请求回固定 checkpoint。settings 里填它打印的
`base_url` 就能跑 `aacode chat / run`。摘要格式是 agent 层的约定，判断只写在
示例里，不进库。

## 后果

**正面：**

- 协议层可以完全离线验证：真实 reqwest + 真实编码/解码 + 真实 HTTP 分帧；
- 生成器和解码器同一 crate，字段改名编译期就能发现一侧漏改；
- 临时 Python 脚本可以删掉，`cargo test --features mock-server` 就是回归。

**代价：**

- 三个 provider 多一个 `test-support` feature（生产构建不带）；
- 汇总 crate 的 `mock-server` 会顺带打开三个 provider 的 `test-support`
  （cargo feature 是加法；只在测试/示例里开启）；
- 服务器刻意做得很小：不支持 keep-alive、chunked 请求、并发连接压测。

**触发重新评估的条件：**

- 需要模拟分片到达（真流式时序）：届时 `MockReply` 要支持多段 body + flush；
- 需要 mock `/models` 等 GET 端点：在 `MockServer` 上加路由，而不是让 provider 知道。
