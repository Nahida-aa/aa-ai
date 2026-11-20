# 0009. HTTP 传输：代理语义对齐 Zed，流式读取加 idle 超时

> 状态：proposed（代理语义是用户 2026-11-20 的决定；超时默认值与实现是助手提案，已落地）
> 日期：2026-11-20
> 关联：[`docs/compare/http-client.md`](../compare/http-client.md)（Zed / genai / rig /
> TanStack 的源码事实）、[`docs/design/library-api.md`](../design/library-api.md)
> （配置/凭证归应用层）、[`0007`](0007-response-mapping.md)（流式响应边界）
>
> **归属：顶层 `proxy`、空串=读环境变量、只接受 http/https 是用户决定
> （2026-11-20）；「客户端级 `read_timeout`」「120s 默认值」「keep-alive 取值照
> Zed」是助手提案，依据是源码对照与真实链路里出现过的静默卡死。**

## 背景与问题

`with_proxy` 落地后有两个未验证点：

1. 代理行为是否真的和 Zed 一致（尤其 `NO_PROXY`、空串、非法 URL）；
2. 流式请求没有 idle 超时。真实链路里出现过 120s / 180s 没有任何输出、
   重跑又正常的情况——连接可能已经被代理或中间设备静默断开，而 TCP 还没报错。

约束：超时必须是**「多久没收到数据」**，不能是整请求总时长。LLM 在推理阶段
可能静默数十秒，健康的流也会在 token 之间停顿；总时长上限会杀掉正常请求。

## 当时的想法（选项）

### 选项 A：客户端级 `reqwest::ClientBuilder::read_timeout`（采用）

reqwest 的语义是「每次 read 的上限，成功读到字节就重置」。一次配置，所有请求
（流式 / 非流式 / `list_models`）共用。

- 优点：和 Zed `ReqwestClient::builder` 同一形状；不需要在流式解码器里再包一层
  `tokio::time::timeout`；错误类型仍是 `reqwest::Error`，`is_timeout()` 可判。
- 代价：非流式请求在生成完成前收不到任何 body 字节，所以对非流式而言它等价于
  「首字节上限」；HTTP/2 PING 帧不算 body 字节，不会重置计时。

### 选项 B：在 `bytes_stream()` 上逐 chunk 包 `tokio::time::timeout`（genai）

genai 的做法（`src/webc/stream_transport.rs:69-87`）：每次 `next()` 都套一个
timeout，header 阶段另有一个 `stream_header_timeout`。

- 优点：只影响流式路径；header 与 read 可以分开配。
- 代价：要自己引入 tokio timer 依赖、自己定义错误类型；和选项 A 在「PING 不算
  body 字节」这一点上没有区别；代码多一层。

### 选项 C：整请求 `timeout`（总时长）

- 优点：实现最简单，能兜住所有卡死。
- 代价：会杀掉合法的长回答 / 长推理，与「idle 而非总时长」的目标冲突。Zed 的
  `RequestTimeout` 也只用于 GitHub releases 这类非流式请求。

### 选项 D：不设超时，交给应用取消

- 优点：库不替应用做策略。
- 代价：默认行为就是「可能永远挂着」；CLI 只能靠用户 Ctrl-C，无法区分
  「模型慢」和「连接死了」。

## 验证（源码事实）

完整对照见 [`docs/compare/http-client.md`](../compare/http-client.md)，关键结论：

- Zed 的共享 builder 已经设了 `connect_timeout(10s)`、`tcp_keepalive(30s)`、
  `pool_idle_timeout(30s)`、HTTP/2 keep-alive 15s/10s/while-idle；`read_timeout`
  是**可选**参数，语义注释明确是 per-read idle（`reqwest_client.rs:24-40,70-80`）。
- Zed 的 LLM 实际入口 `proxy_and_user_agent` 目前传 `None`（`zed/src/main.rs:509`）；
  `proxy_user_agent_and_read_timeout` 由 #61753 引入但还没有调用方。
- genai 把 `stream_header_timeout` / `stream_read_timeout` 做成 per-call 选项，
  默认 `None`；实现是逐 chunk 的 `tokio::time::timeout`。
- rig 的共享客户端不设任何超时；超时由调用方传入的 `reqwest::Client` 决定。
- reqwest 0.12 的 `read_timeout` 同时约束「等响应头」和「读 body」；body 阶段
  每次成功读取都会重置。
- 本仓离线测试：本地原始 TCP 假端点先发一帧合法 SSE，然后不写 `[DONE]`、不关
  连接；`read_timeout=150ms` 时先收到 `Text("he")`，随后收到
  `Transport("读取超时：连续 150 毫秒没有收到任何数据 ...")`，总耗时约 150ms。

## 决策

### D1. 代理语义对齐 Zed

- 显式代理用 `reqwest::Proxy::all`，并挂 `reqwest::NoProxy::from_env()`；
  `NO_PROXY` 仍然生效。
- 不调用 `with_proxy` 时跟随环境变量（reqwest 默认）；应用层的空串语义保持
  「读环境变量」。
- 只接受 `http://` / `https://`；socks 在应用层解析期明确报未启用。
- **有意不同**：代理 URL 非法时我们返回 `LlmError::Unsupported`，不是像 Zed
  那样 `log::error!` 后继续（设置层还会退回环境变量代理/直连）。这是
  `reqwest::Proxy::all` 构造失败的上报，不是库在替应用做「代理设置校验」：
  `proxy` 键、空串语义、socks 拒绝、`http(s)://` 前缀校验仍在 aacode 的
  `normalize_proxy`。
- 不提供 `proxy_no_verify`（那是 Copilot 专用），也不替子进程注入代理环境变量
  （应用层的事）。
- 不提供「强制直连」的公共入口：是否允许绕过环境变量代理是应用层的设置语义。
  测试里用私有的 `ProxyMode::Direct`，不进公共 API。

### D2. 超时与 keep-alive 取值

| 配置 | 值 | 说明 |
|---|---|---|
| `connect_timeout` | 10s | DNS + TCP + TLS + 代理握手 |
| `tcp_keepalive` | 30s | 对齐 Zed |
| `pool_idle_timeout` | 30s | 尽早丢掉静默坏掉的复用连接 |
| `http2_keep_alive_interval` | 15s | 需要 reqwest `http2` feature |
| `http2_keep_alive_timeout` | 10s | |
| `http2_keep_alive_while_idle` | `true` | |
| `read_timeout` | 默认 120s | 每次 body read 的 idle；`with_read_timeout` / `without_read_timeout` 可改 |

120s 的理由：常见 provider 的 SSE keep-alive 间隔是 15–30s，推理静默按「数十秒」
估算；120s 给它留出数倍余量，同时能把「已经死掉的连接」在用户失去耐心前打断。
它不是总时长，健康的流只要持续有增量就不会被中断。

### D3. 错误文案

`describe_transport` 展开 `source()` 链，并区分：

- `连接超时` / `连接失败（DNS/TCP/TLS/代理握手）`：`error.is_connect()`；
- `读取超时：连续 N 秒/毫秒没有收到任何数据（连接可能已被代理或中间设备断开）`：
  `error.is_timeout()` 且不是连接阶段。

### D4. 库不设总时长上限

`LlmError` 不新增 `Timeout` 变体，超时仍走 `Transport`；应用若要「整轮最多 N 秒」
（agent 预算、CLI 超时）自己在 `stream_completion` 外层包。

## 后果

- 正面：
  - 默认行为从「可能永远挂着」变成「静默 120s 后报可读错误」；
  - 代理/连接错误能定位到原因链，不再只有 `error sending request for url`；
  - 所有构造入口共用一个 `build_client`，改超时不会在某个 setter 里丢失。
- 代价：
  - 非流式请求的首字节等待也被 120s 限制；生成长文本的非流式调用可能被误杀
    （需要时用 `with_read_timeout` 放宽）；
  - 只靠 HTTP/2 PING（不产生 body 字节）保活的 provider，不会重置 idle 计时；
  - 为了 HTTP/2 keep-alive 打开了 reqwest `http2` feature，依赖里多了 `h2`。
- 触发重新评估的条件：
  - 出现推理静默 > 120s 的正常请求；
  - 某个 provider 只发 HTTP/2 PING、不发 SSE 注释；
  - 需要把「header 超时」和「body idle 超时」分开配置（届时参考 genai 的两段式）。
