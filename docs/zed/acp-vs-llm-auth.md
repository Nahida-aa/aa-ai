# Zed 的 provider 分层、ACP 认证，与「两套认证」的区分

> 调研日期：2026-11-13
> 性质：**源码核对记录，不是设计决策**（未拍板，不进 `docs/decisions/` 的 ADR 编号）。
> 参考优先级（用户 2026-11-13）：**rig > TanStack AI > genai**。
> 关联：[`provider-mapping.md`](provider-mapping.md)、[`language_models.md`](language_models.md)、
> [`0006`](../decisions/0006-wire-request-shape.md)（wire 请求体定型）。
> 本地参考仓库：`/home/aa/repos/ide_ls/learn_ls/zed`（Zed 官方克隆）、
> `/home/aa/repos/lib_ls/learn_ls/{rig,genai,ai}`。

## 0. 这篇要回答什么

用户问：`pub trait LanguageModelProvider: LanguageModelClient`（Zed
`crates/language_model/src/language_model.rs:568`）为什么这么设计、其他库怎么做；
随后指出「认证和 UI 是 ACP 服务器必定存在的功能」，要求看 Zed Agent（内部 ACP，
不需要认证、只需要 UI）怎么写；最后确认「ACP 认证和 LM 认证是不是两种东西」。
本文把这几轮的源码核对固定下来，**不替用户做任何架构决定**。

## 1. `LanguageModelProvider: LanguageModelClient` 的动机

### 1.1 三个 trait，不是两个

`crates/language_model/src/language_model.rs`：

| trait | 位置 | 分发 | 职责 |
|---|---|---|---|
| `LanguageModelClient` | `:419` | 被继承，不单独作 dyn | 纯 I/O：`stream_completion` / `stream_completion_text` / `stream_completion_tool` / `count_input_tokens` / `compact` / `api_key` |
| `LanguageModelProvider` | `:568`，`: LanguageModelClient` | `Arc<dyn LanguageModelProvider>` | 目录/认证/UI：`id` / `name` / `icon` / `default_model` / `provided_models` / `is_authenticated` / `authenticate` / `settings_view` / `set_api_key` / 各类错误文案 / `fast_mode_confirmation` |
| `LanguageModelProviderState` | `:703` | 泛型约束 | GPUI entity 订阅：`observable_entity` / `subscribe` |

`register_provider<T: LanguageModelProvider + LanguageModelProviderState>`（`registry.rs:152`）
用泛型绑住 `State`，再 `let provider: Arc<dyn LanguageModelProvider> = provider`（`:169`）
擦成 dyn 存进 `providers: BTreeMap<_, Arc<dyn LanguageModelProvider>>`（`:58`）。

### 1.2 supertrait 解决的具体问题

- **全仓没有 `dyn LanguageModelClient`**（已 grep 整个 `crates/`）：provider 和 client 是
  **同一个 struct**（`open_ai.rs:309,383`、`anthropic.rs:301,388` 等十几处都是同一类型
  同时 impl 三个 trait）。
- **registry 只存一个 Arc**：`provider_for_model` 返回 `Arc<dyn LanguageModelProvider>`
  （`registry.rs:310-316`）。
- **agent 直接调 I/O 方法**：`crates/agent/src/thread.rs:3080`
  `provider.stream_completion(&model, request, cx).await` —— 因为 supertrait，
  `Arc<dyn Provider>` 自动有 client 的方法，无需 `provider.client().xxx()`，
  也无需 registry 存两个 trait object 并配对。

即：Zed 里「目录/认证/UI」和「发请求」本来就是同一个对象（同一份 api key / HTTP client /
state），supertrait 是让**一个 dyn 对象同时具备两组方法**的最省事写法。

**边界**：方向不可逆，无法表达「只要一个能发请求的东西」。`thread.rs:3058` 的 agent thread
明明只想发请求，却必须从 `LanguageModelRegistry` 拿完整 provider。Zed 没有这个消费方
（连 `fake_provider` 都实现全套），所以不成为问题。

## 2. 其他库怎么做（按参考优先级）

### rig（优先级最高）—— 无 supertrait、无 dyn object

本地版本已把早期 `Provider` / `ProviderBuilder` / `Capabilities` trait 重构掉：

```
OpenAI (配置 + transport，client/macros 的 http_client! 生成具体类型)
  client/openai.rs:19-37
  └─ .completion(model) -> Model<OpenAiWire>        client/openai.rs:57
       └─ .call(CompletionRequest) -> ...           driver 层
```

- 全靠**泛型 + 静态分发**：`Model<W>` / `Wire` 是具体泛型类型，编译期分发。
- 分层轴是「配置/transport → 模型/wire → 请求」，能力位在 `ModelFacts` / `Quirks`
  （`providers/openai/wire.rs:357` 的 `stream_include_usage` 等），不在 trait 继承上。
- rig 运行时切 provider 靠调用方改构造代码，不靠 registry 存 dyn，所以根本不需要
  「一个 dyn 对象同时具备两组方法」。

### TanStack AI —— 继承，但拆的是「能力/模态」

`BaseTextAdapter` / `BaseImageAdapter` / `BaseAudioAdapter` … 每个能力一个抽象类，
`implements TextAdapter<...>`（`packages/ai/src/activities/*/adapter.ts`）。
`OpenAIChatCompletionsTextAdapter extends OpenAIBaseChatCompletionsTextAdapter
extends BaseTextAdapter`（`ai-openai/src/adapters/text-chat-completions.ts:72-79`）。

- 也是继承而非组合，但拆的是能力/模态：单体 `BaseAdapter` 泛型涨到 7 个，
  拆完每类最多 3 个（官方博客《为什么我们拆分适配器》）。
- `chatStream()` / `structuredOutput()` / 认证 / client / model 全在同一个 adapter 类里
  （`chat-completions-text.ts:95` 的 `this.client`）。
- 运行时切 provider 靠工厂表 `adapters[provider]()`（`runtime-adapter-switching` 文档），
  不是 registry 存 trait object。

### genai —— 静态分发，全在一个 trait

`Adapter` trait（`adapter/adapter_types.rs:11`）同时含 `default_auth` / `default_endpoint` /
`all_model_names` / `get_service_url` / `to_web_request_data` / `to_chat_response` /
`to_chat_stream` —— **认证、endpoint、模型清单、请求编码、响应解码全在一个 trait**，
没有拆 Provider/Client。`AdapterDispatcher` 按 `AdapterKind` match 静态分发
（`adapter/mod.rs:2` 明确 "static dispatch pattern"）；所有方法无 `&self`、状态全靠参数传
（`:7` 注释 "reduces state management"）。`Client::exec_chat(...)`（`client_impl.rs:90`）
内部 resolve `AdapterKind` 再 dispatch。

### 小结

四家里**只有 Zed 的 LLM provider 用了 supertrait**；rig/genai 靠泛型/静态分发根本不需要 dyn，
TanStack 用继承但拆的是能力轴。所以「拆 Provider/Client 并继承」不是通用范式，是
Zed 特定场景（单一 struct 承担两半 + registry 只存一个 dyn Arc）的产物。

## 3. Zed Agent（内部 ACP）：「不需要认证、只需要 UI」怎么写

**它是完全独立于 `LanguageModelProvider` 的一套**，走 `agent_servers` + `acp_thread`。
`provider-mapping.md` 已记：「Zed 的 LLM 调用不经过 ACP——ACP 是外部 agent CLI 的路径」；
Zed Agent 是这条路径上的 **in-process 特例**。

```
NativeAgentServer (impl agent_servers::AgentServer)       agent/src/native_agent_server.rs:23
  ├─ agent_id() -> ZED_AGENT_ID ("Zed Agent")            :25
  ├─ logo() -> IconName::ZedAgent                        :28   ← 「只需要 UI」的那一半
  └─ connect() -> Rc<dyn acp_thread::AgentConnection>   :32-53
        └─ NativeAgentConnection (impl AgentConnection)  agent/src/agent.rs:3292
             ├─ auth_methods() -> &[]  // No auth for in-process   :3341-3343
             ├─ authenticate() -> Task::ready(Ok(()))              :3345-3347
             ├─ new_session / load_session / prompt ...            :3301+
             └─ model_selector() -> ...                           :3349
```

要点：

1. **认证是必填方法**：`AgentConnection` 的 `auth_methods(&self) -> &[AuthMethod]`
   （`acp_thread/src/connection.rs:196`）与
   `authenticate(&self, method, cx) -> Task<Result<()>>`（`:206`）**都没有 default**。
   每个 ACP agent 必须给出答案。Zed Agent 的答案是「空方法列表 + 立即成功」，
   注释直写 `// No auth for in-process`。
   → **「不需要认证」是通过实现认证的「空/成功」语义表达的，不是把认证这一半拆掉。**
2. **UI 面同样在 trait 里**：`logo()` 在 `AgentServer`（更外层的注册 trait）；
   `AgentConnection` 里还有 `model_selector`、`supports_session_history`、权限请求、
   telemetry 等能力位，全和发请求混在**同一个 `Rc<dyn AgentConnection>`** 里。
3. **两层 trait + 一个 dyn object，用工厂连接，不用 supertrait**：
   `AgentServer`（目录/图标/`connect()` 工厂）返回值是 `Rc<dyn AgentConnection>`；
   `AgentConnection`（会话 + 认证 + prompt + 能力位）是一个扁平大 trait。
   `NativeAgentConnection` 实现后者，`NativeAgentServer` 实现前者并造出前者。
4. `prompt`（`connection.rs:242`）是「发消息/跑一轮」那一半，和 `authenticate` 并列在
   同一 trait —— ACP 这条线是**单 trait 大而全**，没拆 Provider/Client。

**同一个代码库里并存两种结构**：LLM provider 线用 `Provider: Client` supertrait，
ACP 线用 `AgentServer`(工厂) + `AgentConnection`(扁平大 trait)。各自解决各自的问题。

## 4. ACP 认证 与 LM 认证 是两种东西（核实结论）

**是，两套完全独立的类型与协议，Zed 里没有共用。**

| | LM provider 认证 | ACP agent 认证 |
|---|---|---|
| 所在 trait | `LanguageModelProvider`（`language_model.rs:581-582`） | `acp_thread::AgentConnection`（`connection.rs:196,206`） |
| 方法 | `is_authenticated(&self, cx: &App) -> bool`<br>`authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>>` | `auth_methods(&self) -> &[acp_v2::AuthMethod]`<br>`authenticate(&self, method: AuthMethodId, cx: &mut App) -> Task<Result<()>>` |
| 错误类型 | `language_model_core::AuthenticateError`（`language_model_core.rs:704-711`：`ConnectionRefused` / `CredentialsNotFound` / `Other`） | `anyhow::Result<()>`（无专用错误枚举） |
| 认证方式表达 | 隐式：api key / 账户会话，由 provider 自己定（`api_key()`、`read_credentials`） | 显式：`initialize_response.auth_methods` 返回**方法列表**，调用方选一个 `AuthMethodId` 传进 `authenticate`（`agent_servers/src/acp/v2.rs:1630,1641`），可带 `terminal_auth_task` 在终端里登录（`:1633-1639`） |
| 「无需认证」怎么表达 | `is_authenticated() -> true` + `authenticate() -> Ok(())`（订阅制 `openai_subscribed.rs:104-131`、`cloud.rs:322-359` 走账户会话；`bedrock.rs:467-474` 等直接 `Task::ready(Ok(()))`） | `auth_methods() -> &[]` + `authenticate() -> Task::ready(Ok(()))`（Zed Agent `agent.rs:3341-3347`） |
| 协议来源 | Zed 自己的 LLM provider 抽象 | **ACP 协议**（`acp_v2` / `agent-client-protocol`），`AuthMethod` / `AuthMethodId` 是协议类型，跨语言 |

关键区别：

- **LM 认证是「这个 provider 有没有凭据」**，布尔 + 一个不区分方式的 `authenticate`，
  凭据形态（API key / 环境变量 / 订阅会话）由各 provider 私有。
- **ACP 认证是「这个 agent 支持哪几种登录方式」**，方法列表是协议握手的一部分
  （`initialize` 响应里带回），调用方按选中的 `method_id` 触发，甚至可以拉起终端登录。
  它是 ACP 连接生命周期里的标准能力，不是某个 LLM 厂商的凭据。
- 两者**不互相引用**：`language_model` 侧不 import `acp_v2::AuthMethod`，
  `acp_thread` 侧不 import `AuthenticateError`。Zed Agent（in-process）在 ACP 侧
  「无认证」，但它内部跑 `thread.rs` 打 LLM 时仍走 `LanguageModelProvider` 的认证——
  **两条链路串联，但认证各管各的**。

## 5. 对「要不要 supertrait」的影响（记录，不下结论）

- 「认证/UI 是 ACP 服务器固有面」这个事实成立：ACP 线里认证是必填方法，
  且「不需要认证」也照样要实现（Zed Agent 的空/成功语义）。因此不能把认证/UI
  当成「可拆出去的 UI 累赘」。
- 但 Zed **自己也没用 supertrait** 表达 ACP 这一层：用的是 `AgentServer`(工厂) +
  `AgentConnection`(扁平大 trait)。supertrait 只出现在 LLM provider 线，
  服务于「同一个 struct 承担两半 + registry 只存一个 dyn Arc」。
- 所以「要不要 supertrait」取决于**是否真存在一个需要被独立持有的纯 I/O 子集**：
  Zed LLM 线有（且被刻意并进 provider），ACP 线没有（prompt 与 auth 共用连接状态）。
- 助手曾以「core 无 UI、注册半在别处」为由建议组合/不照抄，**该论证前提不成立**
  （把当前草稿分层当成了目标约束），已向用户更正。目标若确为 ACP server，
  认证方法应是一等成员；具体 trait 结构待用户定方向后再议。

## 6. 待确认（未拍板）

- 本库的目标形态到底是不是 ACP server？若否，第 4/5 节只是用于纠正论证的通用事实；
  若是，`AgentServer` / `AgentConnection` 这层要不要建、与现有
  `LanguageModelClient` 是什么上下游关系，需用户定大方向。
- 本记录不入 ADR 编号；若将来据此拍板 trait 结构，按
  [`docs/decisions/README.md`](../decisions/README.md):11 另写新 ADR。
