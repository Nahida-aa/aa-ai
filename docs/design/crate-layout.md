# crate 布局与命名（现行规则）

> 状态：**生效中**（2026-11-13 落地）
> 关联：[`strategy.md`](strategy.md)（2026-11-12 的早期策略，§1/§4 已按本文修订）、
> [`docs/zed/language_models.md`](../zed/language_models.md) §9/§10、
> [`docs/zed/provider-mapping.md`](../zed/provider-mapping.md) §4
>
> **决策归属**：本文的规则来源分两类，逐条标注：
> - **用户规定**：用户明确要求的（命名方式、一家一 crate、代码位置、内存优先、允许破坏性重构）；
> - **对比结论**：从 Zed / rig / genai / TanStack AI 源码对比得出的（详见 `docs/zed/`）。
>
> 反过来，`docs/decisions/` 里的 ADR 是**助手写的提案与验证记录**，不是用户的决策。

## 1. 一句话

**IR + trait 在 core；一个协议一个 crate；常见厂商也是一家一个 crate；命名 = 命名空间 + 短类型名。**

> 三层划分见 [ADR 0008](../decisions/0008-provider-tiers.md)：
> **协议**（wire 形状）→ **常见厂商**（一家一 crate，差异逐条写）→
> **长尾端点**（运行时数据，不开 crate）。

## 2. crate 布局

| crate | 路径 | 职责 | 依赖 |
|---|---|---|---|
| `aa_language_model` | `packages/aa_language_model/` | canonical IR（`request::*`）+ `provider::LanguageModelClient` trait / 错误 / 事件 / `Model` | 无网络依赖 |
| `aa_ai_openai` | `packages/aa_language_models/aa_ai_openai/` | OpenAI Chat Completions 的 wire DTO + encoder + HTTP 客户端 | core + `reqwest` |
| `aa_ai_deepseek`（待建） | `packages/aa_language_models/aa_ai_deepseek/` | 常见厂商：自有 encoder，差异写在自己 crate 里 | core + `reqwest`（可按需复用 `aa_ai_openai` 的 DTO） |
| `aa_ai_openrouter`（待建） | `packages/aa_language_models/aa_ai_openrouter/` | 同上（额外 header、usage 字段） | 同上 |
| `aa_ai_anthropic`（待建） | `packages/aa_language_models/aa_ai_anthropic/` | 另一套协议（Messages + 签名 thinking） | core + `reqwest` |
| `aa_ai_gemini` / `aa_ai_ollama`（待建） | 同上 | 各自协议（generateContent / 原生 `/api/chat`） | 同上 |
| `aa_language_models` | `packages/aa_language_models/` | provider 汇总入口 + 离线 `stub::StubClient` | 各 `aa_ai_*` + core |
| `aa_agent` | `packages/aa_agent/` | agent loop + 工具执行接口 | core |

### 2.0 协议 crate 与厂商 crate 的差别

| | 协议 crate | 厂商 crate |
|---|---|---|
| 决定什么 | 请求 / 响应 / 流式事件的**形状** | 一家厂商在形状之上的**差异**（端点、鉴权、思考字段、`[DONE]` 语义、`finish_reason` 别名…） |
| 复用 | 可被多个厂商 crate 依赖 | 只依赖 core（必要时依赖协议 crate 的 DTO） |
| 命名 | `aa_ai_openai`（协议名） | `aa_ai_deepseek`（厂商名） |
| 何时新开 | 出现第二套 wire 形状 | 某家差异多到要独立演进 |

厂商 crate **可以**复用协议 crate 的响应 DTO，但**不共享 encoder**：请求编码各写一份，
这是 [0008](../decisions/0008-provider-tiers.md) D1 明确接受的重复。

依赖方向（箭头 = 依赖）：

```
aa_ai_openai ──┐
aa_ai_deepseek ──┼──> aa_language_model   （IR + trait）
aa_ai_*    ──┘
aa_language_models（汇总）─┘

aa_agent ──> aa_language_model
```

- **core 不认识任何厂商的 JSON 字段，也不依赖 `reqwest`。** 加一家 provider = 新增
  一个 `aa_ai_*` crate，core 一行不改。
- `aa_language_models` 只做**命名空间转发**（`openai::OpenAiLanguageModelProvider`、
  `stub::StubClient`），不摊平 IR 类型：`request::Request` 只有
  `aa_language_model::request::Request` 一条规范路径。
- 离线 stub 放在汇总 crate（不是 core、也不是 `aa_ai_*`）：和 Zed 把
  `FakeLanguageModelProvider` 放在 `language_model` 而不是 `language_model_core`
  是同一个理由——它不是 wire provider，没有任何厂商格式。

### 2.1 provider 代码必须放哪

**放 `packages/aa_language_models/aa_ai_<provider>/src`。**

反例（已删除，别再写回来）：把 OpenAI 的 DTO / encoder / HTTP 客户端塞进
`packages/aa_language_model/src/provider/openai.rs`。那样 core 会被迫依赖
`reqwest`，而且每加一家 provider 都要动 core 的 `Cargo.toml` 和模块树。

对照 Zed：`language_model_core`（IR + trait）与 `open_ai` / `anthropic` /
`google_ai`（各自一个 crate）分离，DeepSeek 这种「大体兼容但字段不同」的也照样
新开 crate，而不是塞进 core。

## 3. 命名：命名空间 + 短类型名 + **完整路径**

**规则（用户规定）：类型名短，域用模块路径表达；路径必须完整，不允许 `use` 导入后再写短路径。**

| 做法 | 例子 | 结论 |
|---|---|---|
| 写完整路径 | `aa_language_model::request::Request` | ✅ |
| 同 crate 内写完整路径 | `crate::chat_completion::request::message::ChatMessage` | ✅ |
| 导入模块，写 `模块::类型` | `use aa_language_model::request;` → `request::Request` | ❌ 前缀被截断 |
| 导入具体类型后用裸名 | `use aa_language_model::request::Request;` → `Request` | ❌ 完全丢命名空间 |
| 用长前缀把域写进类型名 | `LanguageModelRequest` / `LanguageModelRequestMessage` | ❌ |

正例：

```rust
fn encode(
    request: &aa_language_model::request::Request,
) -> Result<
    aa_language_model::provider::EventStream,
    aa_language_model::provider::LlmError,
> {
    let msg = aa_language_model::request::message::Message::new(
        aa_language_model::request::message::Role::User,
        vec![aa_language_model::request::message::ContentPart::text("hi")],
    );
    // ...
}
```

反例：

```rust
use aa_language_model::request;                    // ❌ 下文的 request::Request 前缀不全
use aa_language_model::request::message::Message;  // ❌ 下文的 Message 是裸名

fn encode(req: &request::Request) { let _ = Message::new(/* ... */); }
fn draw(req: &Request) {}                          // ❌ 看不出是哪个域
```

为什么：

- 读代码时**不必回到文件头确认 `use`**：`aa_language_model::request::message::ContentPart`
  自带全部归属信息。跨 crate 阅读（grep 结果、diff、文档片段）时尤其重要。
- 短名 + 模块路径在调用点自带上下文，又不用把域写进类型名。
- 长前缀（`LanguageModelRequestMessage`）把域固化成类型名的一部分，改名就要全仓替换，
  且和 Zed 的名字纠缠在一起。
- 裸名在同一个文件里出现两个 `ToolInput`（`message::ToolInput` ≠ `tools::ToolInput`）时
  尤其危险。
- 断言：

  ```bash
  # 不应有任何对 IR / provider 类型的 `use` 导入
  grep -rn "use aa_language_model::" packages --include=*.rs
  # 只剩 aa_agent（冻结）
  ```

**已知代价：** 完整路径比 `use` 长很多，深嵌套类型（如
`aa_language_model::request::message::ContentPart::ToolResult`）单行会超 100 列，
`rustfmt` 会把 `match` 臂折成多行。这是刻意付的代价——可读性靠路径自证，不靠跳读
`use`。

**例外 1：** `pub use` 重导出不算「导入后写短路径」，汇总 crate（`aa_language_models`）
就是靠它做命名空间转发的（`pub mod openai { pub use aa_ai_openai::OpenAiLanguageModelProvider; }`）。
调用方仍然要写 `aa_language_models::openai::OpenAiLanguageModelProvider`。

**例外 2（用户决定，2026-11-13）：** provider 的 client 类型直接沿用 Zed 的名字
`<Name>LanguageModelProvider`（`aa_ai_openai::OpenAiLanguageModelProvider`）。
这确实把域写进了类型名、和上面的规则相悖，但用户要求与 Zed 逐字对齐
（`crates/language_models/src/provider/open_ai.rs:50`），所以单独立一条例外；
其余类型（`Request`、`ChatMessage`、`ToolKind` …）仍按短名规则。

## 4. 加一家 provider 的步骤

先判断是哪一层（[0008](../decisions/0008-provider-tiers.md) D1）：

- **新协议**：照下面的步骤新建 crate。
- **常见厂商、但协议相同**：也新建 `aa_ai_<vendor>` crate（用户决定，2026-11-17），
  差异写在自己 crate 里；不往 `aa_language_model` 或别家 crate 里加分支。
- **长尾端点**：不写 crate，留给运行时配置（尚未设计，见 0008「未做」）。

步骤：

1. `packages/aa_language_models/aa_ai_<name>/`（`cargo new --lib`），`Cargo.toml`
   写 `aa_language_model.workspace = true` + `reqwest` / `futures` / `serde` /
   `serde_json`，全部 `.workspace = true`。
2. **按接口/协议分组**，一个协议一个目录，请求与响应成对：
   `src/chat_completion/mod.rs`（只声明子模块）下
   `src/chat_completion/request/mod.rs`：顶层请求体 + 借用式 encoder
   （`to_<wire>_request(&aa_language_model::request::Request, model_id)
   -> Result<Wire, aa_language_model::provider::LlmError>`）；
   `src/chat_completion/request/message.rs`：消息层 DTO（`role` 形状、工具调用片段）；
   `src/chat_completion/response/mod.rs`：响应 body / SSE chunk / 错误 body + 映射。
   参照 `aa_ai_openai`。将来接 Responses API 就是并列的 `src/responses/`，
   两套协议不共享结构体（请求侧只 `Serialize`、响应侧只 `Deserialize`）。
3. **厂商 crate 额外要写下的东西**（这些不允许上移到 core）：
   端点与路径、鉴权 header、`response_format` 支持范围、思考字段名与回放顺序、
   `[DONE]` / `finish_reason` 语义、流式 usage 的位置、额外的模型事实表。
   对照 rig `Dialect`/`Quirks` 的字段表（[`docs/compare/provider-extension.md`](../compare/provider-extension.md) §8）。
4. `src/lib.rs`：只放 client 本体 +
   `impl aa_language_model::provider::LanguageModelClient for <Name>LanguageModelProvider`。
5. 根 `Cargo.toml`：`members` 和 `[workspace.dependencies]` 各加一行。
6. `aa_language_models/src/lib.rs`：加一个
   `pub mod <name> { pub use aa_ai_<name>::<Name>LanguageModelProvider; }`。
7. core 不动。如果发现「必须改 core 才能接这家」，说明这家有 IR 表达不了的真需求，
   先写 ADR，再决定改 IR 还是走 `ContentPart::Opaque`。

## 5. 哪些是用户决定，哪些是对比结论

| 规则 | 来源 |
|---|---|
| 内存占用优先于吞吐 | 用户规定（见 [0002](../decisions/0002-memory-first.md) 的背景） |
| 命名 = 命名空间 + 短类型名 | 用户规定 |
| 一家 provider 一个 crate，代码放 `aa_ai_*` | 用户规定；与 Zed 布局一致 |
| provider 分三层：协议 / 常见厂商（一家一 crate）/ 长尾数据 | 用户规定（2026-11-17）；见 [0008](../decisions/0008-provider-tiers.md) |
| 允许对 `packages/` 破坏性重构 | 用户规定 |
| `Model` 只放模型事实，厂商差异放厂商 crate | 助手提案（0008 D2） |
| `CompletionEvent::Reasoning` | 助手提案（0008 D3），对齐 Zed `Thinking` |
| 语义 IR + per-provider wire + 统一事件流 | 对比结论（Zed / genai / rig 三者收敛） |
| enum 表达封闭语义、`dyn` 表达开放实现 | 对比结论（Zed 的穷尽匹配优势） |
| 借用式 encoder（不重建 owned 历史） | 对比结论 + 实测（`decisions/experiments/history-bytes`） |
| 错误归一化成 `provider::LlmError` | 对比结论（Zed / rig 各自都有独立错误层） |
