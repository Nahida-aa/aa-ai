# 四家的 HTTP 客户端：代理与流式超时（事实记录）

> 只记录源码事实：类型、函数、常量、调用点、配置形状。「该选哪个值」「该不该
> 报错」不在本文讨论。行号以本地克隆为准：
> - Zed：`/home/aa/repos/ide_ls/learn_ls/zed`（commit `4ed72e0135`）
> - genai：`/home/aa/repos/lib_ls/learn_ls/genai`（v0.7.5，commit `d852bfd`）
> - rig：`/home/aa/repos/lib_ls/learn_ls/rig`（commit `6e13e8c`）
> - TanStack AI：`/home/aa/repos/lib_ls/learn_ls/ai`（commit `6059872f`）

## 1. Zed：全局客户端 + 每读 idle 超时

### 1.1 客户端构造

`crates/reqwest_client/src/reqwest_client.rs:24` 的 `ReqwestClient::builder(read_timeout)`
是所有客户端的共同起点：

| 配置 | 值 | 代码 |
|---|---|---|
| `connect_timeout` | 10s | `:28` |
| `tcp_keepalive` | 30s | `:32` |
| `pool_idle_timeout` | 30s | `:33` |
| `http2_keep_alive_interval` | 15s | `:34` |
| `http2_keep_alive_timeout` | 10s | `:35` |
| `http2_keep_alive_while_idle` | `true` | `:36` |
| `read_timeout` | 可选，`None` = 不设 | `:37-40` |

`read_timeout` 的注释写明语义（`:30-31`、`:70-80`）：**每次 body read 的 idle
超时，收到字节就重置**，不是整请求总时长；调用方应把它设得「comfortably above the
provider's keep-alive interval」。macOS 上该计时使用系统睡眠时暂停的单调时钟。

### 1.2 代理

`ReqwestClient::proxy_user_agent_and_read_timeout`（`:83`）：

- `reqwest::Proxy::all(proxy_url)`，然后 `.no_proxy(reqwest::NoProxy::from_env())`
  （`:99-107`）——显式代理仍然尊重 `NO_PROXY`。
- 代理 URL 解析失败**只记 `log::error!` 并继续**（`:92-97`）：这一层不把
  `proxy` 加进 builder，`auto_sys_proxy` 仍为 `true`，所以随后还可能走环境变量
  代理。再往上一层，`ProxySettings::proxy_url()` 在显式值解析失败后还有
  `.or_else(read_proxy_from_env)`（`crates/client/src/client.rs:138-150`），
  即非法显式值会退回环境变量/直连，不会报错。
- `client_has_proxy` 只在真的加上代理时为 `true`；`ReqwestClient::proxy()` 返回它
  （`:108-119`）。
- 额外 `.use_preconfigured_tls(http_client_tls::tls_config())`（`:112`）。

`crates/client/src/client.rs:138` 的 `ProxySettings::proxy_url()`：

```rust
self.proxy.as_deref()
    .map(str::trim)
    .filter(|input| !input.is_empty())   // 空串 → 走环境变量
    .and_then(|input| input.parse::<Url>() ... )
    .or_else(read_proxy_from_env)        // ALL_PROXY / HTTPS_PROXY / HTTP_PROXY
```

`read_proxy_from_env` 的变量顺序：`ALL_PROXY`、`HTTPS_PROXY`、`HTTP_PROXY`
（大小写各一遍，`crates/http_client/src/http_client.rs:365-379`）。

### 1.3 谁在用

- LLM 实际入口：`crates/zed/src/main.rs:509`
  `ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)` → `read_timeout = None`
  （`:66-68` 转调 `proxy_user_agent_and_read_timeout(..., None)`）。
- `proxy_user_agent_and_read_timeout` 由 commit `865318723e`（#61753）引入，当前除
  `proxy_and_user_agent` 外没有别的调用方。
- `RequestTimeout`（`crates/http_client/src/http_client.rs:26`）是**整请求总时长**
  扩展；`HttpRequestExt::timeout` 把它塞进 request extension（`:56-63`），由
  `ReqwestClient` 的请求执行路径消费。用于 GitHub releases 等非流式请求。
- `proxy_no_verify` 只服务 Copilot（`crates/copilot/src/copilot.rs:487,1328`；
  设置项在 `crates/settings_content/src/language.rs:264`），不是全局客户端选项。
- 子进程环境注入：`crates/agent_servers/src/agent_servers.rs:110-133` 把顶层代理
  写成 `HTTP_PROXY` / `HTTPS_PROXY`，并透传或补 `NO_PROXY`（默认
  `localhost,127.0.0.1`）；`crates/client/src/proxy.rs:22` 的
  `excluded_from_proxy` 用同一份 `NO_PROXY` 判断 RPC 直连。
- Zed 另有 `crates/http_proxy`：给 agent 子进程用的本地 HTTP 代理（上游链式代理、
  `NO_PROXY` 规则在 `proxy/upstream.rs`）。

## 2. genai：把 header 超时与逐 chunk 超时分开

- 配置字段在 `src/chat/chat_options.rs:47,53`：
  - `stream_header_timeout`：等到响应头的上限（含连接建立）；
  - `stream_read_timeout`：每个原始 body chunk 的上限，**心跳/半帧也会重置**；
    注释明确「不是 overall response deadline」。
- 两者都可以设在 `Client` 默认值上，per-call 覆盖（`src/client/client_impl.rs:232-238`）。
- 实现不在 reqwest builder 上，而是显式包一层（`src/webc/stream_transport.rs`）：
  - `StreamRequest::send` 用 `tokio::time::timeout(header_timeout, request.send())`
    （`:43-50`）；
  - `StreamResponse::bytes_stream` 用 `tokio::time::timeout(read_timeout,
    stream.next())` 包**每一次** `next()`（`:69-87`）；
  - `text()` 在设了 read_timeout 时改为逐 chunk 读并拼接（`:89-99`）。
- 默认都是 `None`（不设）。

## 3. rig：不内建超时策略

- `crates/rig-reqwest/src/lib.rs:132-139` 的 `shared_builder()` 只设
  `redirect(Policy::none())`（native），没有 `connect_timeout` / `read_timeout` /
  keep-alive。
- 超时由调用方传入自己的 `reqwest::Client` 决定；`rig-reqwest` 的测试里用
  `reqwest::Client::builder().timeout(...)` 验证「总超时后仍能保留已读到的部分
  error body」（`crates/rig-reqwest/src/tests/error_body.rs:110-146`）。

## 4. TanStack AI：配置字段 + AbortSignal

- `TextAdapterConfig.timeout?: number`（`packages/ai/src/activities/chat/adapter.ts:21`）
  是 adapter 配置字段；文本 chat 的流式路径本身用 `AbortSignal` 取消
  （`packages/ai/src/activities/chat/index.ts:995-1026,1279-1281`），没有
  「每次 read 的 idle 超时」实现。

## 5. 本仓（aa-ai）现状

传输已独立成 `aa-stack/packages/util/aa_http_client`（[ADR 0010](../decisions/0010-transport-object.md)），
`aa_ai_openai` 只接收 `Arc<aa_http_client::HttpClient>`：

- 一个进程一个连接池（`Arc` 共享），不再每个 provider 各建一个 `reqwest::Client`。
- 常量与 Zed 对齐：`connect_timeout` 10s、`tcp_keepalive` 30s、
  `pool_idle_timeout` 30s、HTTP/2 keep-alive 15s / 10s / while-idle。
- `DEFAULT_READ_TIMEOUT = 120s`，默认开启；`Builder::read_timeout` 覆盖，
  `without_read_timeout` 关闭。语义是 reqwest 的 per-read idle（收到字节就重置）。
- 代理：`Proxy::all` + `NoProxy::from_env`；`Proxy` 是三态值
  （`Environment` / `Direct` / `Explicit`），**由调用方传进来**。URL 非法返回
  `BuildError::InvalidProxy`（这是 reqwest 构造失败的上报，不是设置层校验）。
  「哪个键叫 `proxy`」「空串=环境变量」「拒绝 socks」「`http(s)://` 前缀」在
  应用层 aacode 的 `normalize_proxy`；库不读配置/env。
- 超时/代理错误经 `aa_http_client::describe_transport` 展开原因链，并区分
  「连接超时」与「读取超时：连续 N 秒没有收到任何数据」。
- reqwest feature 为 `http2` + `rustls-tls-native-roots` + `json` + `stream`；
  没有 `system-proxy`（环境变量代理仍由 hyper-util 的 `from_env` 读取，
  `system-proxy` 只额外读 macOS/Windows 系统配置）。
