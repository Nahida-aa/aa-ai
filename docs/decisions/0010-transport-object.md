# 0010. HTTP 传输独立成对象：一个进程一个连接池

> 状态：proposed（用户 2026-11-20 决定「全做」这次破坏性重构；
> 具体形状是助手提案）
> 日期：2026-11-20
> 关联：[`0009`](0009-http-transport.md)（超时与代理取值，本文只改**归属**）、
> [`docs/compare/http-client.md`](../compare/http-client.md)（四家对照）、
> [`docs/design/library-api.md`](../design/library-api.md)（库与应用边界）、
> [`docs/design/crate-layout.md`](../design/crate-layout.md)（crate 布局）
>
> **归属：用户决定「传输不该属于 aa-ai，放 `aa-stack/packages/util/aa_http_client`，
> 以后可独立到 aa-util」；`Arc<HttpClient>` 的接口形状、`Proxy` 枚举、`BuildError`
> 是助手提案。**

## 背景与问题

`aa_ai_openai::OpenAiLanguageModelProvider` 之前自己持有 `reqwest::Client`，
并且用 `with_proxy` / `with_read_timeout` / `with_direct_connection` 三个 setter
重建它。三个具体问题：

1. **N 个连接池**：每构造一个 provider 实例就 `reqwest::Client::builder().build()`。
   一个应用同时连 4 个端点 = 4 套连接池、4 套 DNS 缓存、4 份 TLS 会话。
   Zed 是 `Arc<dyn HttpClient>` 全进程共享（`crates/reqwest_client`），pi 是注入
   `fetch`，rig 是 `DynHttpClient`——只有我们在每个 provider 里各建一个。
2. **传输配置被卷进 provider 构造链**：`with_proxy` 必须重建客户端，返回
   `Result`，于是「代理 URL 非法」这个**传输构造错误**看起来像 provider 的配置
   校验（2026-11-20 用户就这一点质疑过）。上一版把 `with_direct_connection`
   从公共 API 删掉，只是把症状按住了：根因是 provider **拥有**传输。
3. **每个新 provider crate 都要抄一遍**：`aa_ai_ollama` / `aa_ai_anthropic` 若各自
   实现超时、keep-alive、代理、错误文案，就是四份会漂移的副本。

约束：内存优先（连接池数量直接影响常驻内存与握手次数）；core 不依赖网络；
库层不读配置/env。

## 当时的想法（选项）

### 选项 A：传输留在 provider crate，各建各的（现状）

- 优点：零新 crate；provider 自包含。
- 代价：上面的三个问题；超时/代理代码只能靠复制保持一致。

### 选项 B：放进 `aa_language_model`（core），做 `HttpClient` trait

- 优点：所有 provider 自然共享。
- 代价：core 从此依赖 `reqwest`（或被迫抽象一层 trait + dyn），与
  [0008](0008-provider-tiers.md)「core 不认识任何厂商、也不依赖网络」冲突；
  `aa_agent` 只是用 IR，却被拖着引入网络栈。

### 选项 C：独立 crate，放 `aa-ai/packages/util/`（本仓内）

- 优点：core 保持零网络；provider 共享一个对象。
- 代价：「HTTP 传输」和 LLM 没有任何关系。放在 `aa-ai` 里会让库的边界看起来
  包含基础设施，将来别的应用（非 LLM）想复用还得反向依赖 `aa-ai`。

### 选项 D：独立 crate，放 `aa-stack/packages/util/aa_http_client`（采用）

- 优点：位置表达归属——它是**通用基础设施**，不依赖任何 LLM 类型；
  `aa-stack` 的 `packages/util/*` 已经是这类 crate 的家（`aa_shared_string`、
  `util_macros` 等），以后整体独立到 `aa-util` 只是搬目录。
- 代价：`aa-ai` 与 `aacode` 都多一条**跨仓库 path 依赖**
  （`../aa-stack/packages/util/aa_http_client`），写 `Cargo.toml` 时要记得相对路径。

## 验证

- 对照（详见 [`docs/compare/http-client.md`](../compare/http-client.md)）：
  - Zed：`ReqwestClient` 是一个 crate，`Arc<dyn HttpClient>` 注入每个 provider
    （`crates/reqwest_client/src/reqwest_client.rs`）；
  - pi：协议模块只接 `options.fetch`，传输完全由调用方注入
    （`packages/ai/src/types.ts:166-171`）；
  - rig：`Model<W, T = DynHttpClient>`，wire 与 transport 是两个参数
    （`crates/rig-core/src/driver.rs:54`）；
  - genai：`Client` 内含一个 `WebClient`，所有 adapter 共用。
  四家没有一家让 provider 自己建连接池。
- 编译与测试：`aa_http_client` 5 个单元测试（坏代理 URL / 直连 / 环境变量 /
  超时默认值与关闭 / 原因链展开）；`aa_ai_openai` 31 个测试（含原离线 TCP
  假端点的 idle 超时用例，改用 `Proxy::Direct` 的共享客户端）；`aa_agent`
  4 个；`aacode_settings` 32 个（新增「`http://` 在解析期报错」）。
- 真实链路：经本机 mitmproxy（`https://127.0.0.1:8080`）跑
  `providers` / `models agnes` / `chat agnes agnes-3.0-flash` /
  `run agnes agnes-3.0-flash`（含 glob 工具调用），全部正常。

## 决策

### D1. `aa_http_client` 是通用基础设施 crate

- 位置：`aa-stack/packages/util/aa_http_client`；不依赖任何 LLM 类型。
- 内容：`HttpClient`（内部一个 `reqwest::Client`）、`Builder`、`Proxy`、
  `BuildError`、`describe_error` / `describe_transport`、超时常量。
- 不读配置、不读 env：代理由调用方给成 `Proxy` 值。

### D2. provider 只接收 `Arc<HttpClient>`

```rust
pub struct OpenAiLanguageModelProvider {
    http: Arc<dyn aa_http_client::HttpClient>,
    auth: Auth,
    base_url: String,
    headers: Vec<(String, String)>,
}
```

- 删掉 `with_proxy` / `with_read_timeout` / `without_read_timeout` /
  `with_direct_connection`：传输是构造参数，不是 provider 的可变状态。
- 代理/超时改由一个**共享对象**表达，改配置 = 换一个 `HttpClient`，
  不存在「重建客户端时丢了超时」这类 setter 顺序问题。

### D3. `Proxy` 是机制，策略仍在应用层

```rust
pub enum Proxy { Environment, Direct, Explicit(String) }
```

- `Direct` 是**机制**（忽略环境变量代理），这次终于可以作为公共 API 存在：
  它属于 `HttpClient` 构造，而不是 provider 的策略勾子。应用若要暴露
  「这个端点必须直连」，那是应用自己的设置语义。
- 「哪个键叫 `proxy`」「空串 = 环境变量」「拒绝 socks」「`http(s)://` 前缀」
  仍在 aacode 的 `normalize_proxy`（`packages/settings/src/lib.rs`）。
- `reqwest::Proxy::all` 解析失败 → `BuildError::InvalidProxy`；应用层映射成
  自己的错误（aacode 在**解析期**就构造传输，所以报 `SettingsError::Invalid`
  并带上文件路径）。

### D4. agent loop 不再钉死模型

`AgentLoop` 去掉了生命周期与固定 `Model`：

```rust
pub struct AgentLoop {
    llm: Arc<dyn provider::LanguageModelClient>,
    tools: tool::AgentToolRegistry,
}
impl AgentLoop {
    pub async fn run(&self, model: &provider::Model, system: &str, user: &str, events: &impl ToolEvents)
        -> Result<String, provider::LlmError>;
}
```

对应 pi 的 `StreamFn(model, context, options)`：模型按调用传，loop 可存进会话
结构，也能在同一轮里换模型（便宜的模型跑工具轮、贵的出最终答案；失败降级）。

**不做**：不把 `base_url` / `headers` 搬进 `Model`（pi / rig 是那么做的）。
理由：[0006](0006-wire-request-shape.md) D4 已把「模型事实」与「端点数据」分开，
而 aacode 的 `ResolvedProvider` 已经把 client 与 models 绑在同一个对象上；
类型层再绑一次是重复。`Model.provider` 保留为目录/展示 key，编码器不读它。

### D5. 应用层怎么用

aacode：

- `Settings::transport() -> Result<Arc<HttpClient>, SettingsError>`，
  内部 `OnceLock` 缓存；`load_with_defaults` 里**预先构造一次**，让代理 URL
  的语法错误在解析期暴露并带上文件路径。
- `resolve` 把同一个 `Arc` 传给 `build_provider`。
- CLI 里 `AgentLoop::new(Arc::new(resolved.client), registry)` + `run(&model, ...)`。

## 后果

- 正面：
  - 全进程一个连接池：N 个端点共享 DNS 缓存、TLS 会话、连接复用；
  - 传输配置与 provider 数据彻底分开，「代理错误算不算库层策略」不再是问题；
  - 新 provider crate 直接 `Arc<HttpClient>`，不用抄超时/代理代码；
  - `AgentLoop` 无生命周期、模型可换。
- 代价：
  - `aa-ai` / `aacode` 各多一条跨仓库 path 依赖；`aa-stack` 的 `Cargo.toml`
    增加了 `reqwest` workspace 依赖（之前只有 `aa-bundle` 用同步 `ureq`）。
  - provider 构造签名变了（破坏性）：下游必须显式给 `Arc<HttpClient>`。
  - 想给**单个端点**配不同超时，得单独构造一个 `HttpClient`（不再是 provider
    setter）。这是有意的：不同超时=不同传输，本就该是两个对象。
- 触发重新评估的条件：
  - 需要 per-request 超时（同一端点、不同请求不同 idle）：届时在
    `LlmError` 之外加 `Request.timeout` 之类的每请求字段，而不是回到 setter；
  - `aa_http_client` 需要 TLS/证书策略（mTLS、自定义 CA）：先看是否只加
    `Builder` 选项就够，不要动 `Proxy` 语义；
  - 独立到 `aa-util` 仓库：只搬目录 + 改两条 path 依赖，接口不变。

## 附录：与 0009 的分工

0009 决定**取值**（10s 建连、120s per-read idle、keep-alive、代理语义）；
0010 只决定**归属**（谁拥有 `reqwest::Client`、谁构造、谁传）。0009 的 D2 表格
仍然有效，只是这些值现在写在 `aa_http_client` 的 `Builder::default()` 里。