# 0004. `Message` 的布局：`Box<[ContentPart]>`、共享靠 `Arc<Message>`、冷数据装箱

> 状态：accepted（同日修正过一次，见文首）
> 日期：2026-11-12
> 关联：[0001](0001-ir-layering.md)（enum 与命名）、[0003](0003-alloc-measurement.md)（度量口径）、[history-bytes](experiments/history-bytes/)（本 ADR 的数据）
>
> **⚠ 同日修正：** 初版主张「单 part 内联（`Parts::One`）」，依据是单 part 占多数的负载。
> 但真实会话里 assistant 消息**至少 2 个 part**（思考 + 文本/工具调用），单 part 只占约 31%。
> 实测在该分布下 `Parts::One` 比 `Box<[ContentPart]>` **贵 39 B/条**。已删除该设计，改用 `Box<[ContentPart]>`。
> 决策对象未变（仍是 `Message` 布局），且当时尚未实现，故不另开 0005，直接在此修正并保留经过。

## 背景与问题

[0003](0003-alloc-measurement.md) 确认了两件事：

- 信封成本是**每消息常数**，payload 主导总字节；
- 克隆成本决定派生开销（8 B/条 vs 1828 B/条）。

但 `Message` **内部**怎么排还没测：`parts` 是否独立分配？单 part 要不要内联？这是每消息固定成本里最大的一块，必须用数据定，而不是照着 Zed/rig 抄。

## 当时的想法（选项）

| 方案 | 布局 | 内联 `size_of` | 假设 |
|---|---|---|---|
| M1 朴素结构 | `role` + `Option<String>` + `Vec<ToolCall>` + 3 个 `Option<String>` | 128 | 冷字段全部内联 |
| M2 | `role` + `Arc<[ContentPart]>` | 24 | parts 总是堆分配，且需要独立共享 |
| M3 | `role` + `Parts::{One, Many}` | 64 | 单 part 占多数，内联能省一次分配 |
| M4 | `role` + `Parts2`（≤2 part 内联） | 120 | 2 part 占多数 |
| M5 | `role` + `Box<[ContentPart]>` | 24 | parts 堆分配，但不需要独立引用计数 |
| M6 消息 enum | `User(..)/Assistant(..)/Tool(..)` 各一套字段 | — | 各角色形状不同 |

## 验证

[`experiments/history-bytes`](experiments/history-bytes/)，1000 条消息，`chunk` 口径（含 malloc 块估算）。

**关键输入是 part 数分布**，取建模值：用户 20%（1 part）、助手纯回答 20%（2 parts）、助手调工具 30%（3 parts）、工具结果 30%（1~3 parts）。即 **1 part 31% / 2 parts 29% / 3 parts 40%**，平均 2.1 parts/条。

| 候选 | 均匀 1 part 64B | 均匀 2 parts 1KB | **真实分布** | 分配/条（真实） |
|---|---|---|---|---|
| A2 精简 OpenAI | **120** | **1080** | **1894** | 3.77 |
| M2 `Arc<[ContentPart]>` | 232 | 1288 | 1942 | 9.73 |
| **M5 `Box<[ContentPart]>`** | 216 | 1272 | **1926** | **8.73** |
| M3 单 part 内联 | 200 | 1336 | 1965 | 9.42 |
| M4 ≤2 part 内联 | 248 | 1240 | 1972 | 9.12 |

敏感性（把工具结果拆成单条消息，单 part 占 80%）：M5 1509、M3 1524、M2 1525、A2 1510。

结论：

- **`Box` 全面优于 `Arc<[..]>`**：`Arc<Message>` 已经共享整条消息，parts 数组再带引用计数是纯开销（每消息 +16B + 原子操作）。
- **单 part 内联不成立**：只在单 part 占绝对多数时略省，真实分布下**贵 39 B/条**，因为内联槽位对 69% 的多 part 消息是浪费。
- **≤2 part 内联更差**：3 part 消息仍要堆分配，却多背 96B。
- **语义 IR 与最优 OpenAI 形状差 1.7%**（1926 vs 1894），但克隆差 **228 倍**（8 B/条 vs 1828 B/条）。

## 决策

### D1：`Message` 是 struct，不是 enum

```rust
pub struct Message {
    role: Role,                       // 1B：System / User / Assistant
    content: Box<[ContentPart]>,      // 16B（胖指针），数据在堆上
    meta: Option<Box<MessageMeta>>,   // 8B：冷数据，绝大多数路径不读
}
```

- **不用消息 enum（M6）。** 各角色的「形状」相同（role + 有序 content），不同的只是元数据（stop_reason/usage/origin）。用 enum 会让每个 encoder 先 match 消息种类、再 match part，组合爆炸。
- 合法状态由**编码器 + 构造点**保证，而不是把 part × role 的矩阵铺进类型系统。例如「assistant 的 `ToolCall` 必须有一条对应的 `ToolResult`」是 agent loop 的职责，wire 适配器无法表达这个约束，类型系统也不值得为此长胖。

### D2：`content` 用 `Box<[ContentPart]>`

- 不用 `Arc<[..]>`：共享单位是 `Arc<Message>`，content 不需要独立引用计数（实测省 16 B/条 + 一次原子操作）。
- 不用内联 `Parts::One`：真实分布下更贵（见上）。
- 需要「只替换 content、复用其余字段」时，重建 `Message` 即可——它只有 24B，重建比维护第二层共享更便宜。

### D3：canonical `Role` 只有 System / User / Assistant

工具结果**不是一种角色**，而是 `ContentPart::ToolResult`，放在 user 轮里：

```rust
pub enum Role { System, User, Assistant }
```

Zed 的 `language_model_core::Role` 也只有这三个；`role: "tool"` 是 OpenAI 的
`RequestMessage::Tool` 变体（`crates/open_ai/src/open_ai.rs`），只在它的 wire 层存在。
我们在 encoder 里做同样的派生：遇到 `ToolResult` 就单独发一条 `role:"tool"` 消息。

- 若在 IR 里加 `Role::Tool`，「消息角色」就会被 OpenAI 牵着走：Anthropic / Gemini
  反而要把它反向翻译回 user 轮的内容块。
- 工具结果按约定**独占一条 user 消息**，不和用户文本混排。这不是我们发明的约束：
  Zed 的 `AgentMessage::to_request()`（`crates/agent/src/thread.rs:689`）就是把一条
  agent 消息拆成「assistant（文本/思考/工具调用）」+「user（只有 ToolResult）」两条。
- 于是 `Role::User` 在工具结果消息上的含义是「wire 上的 user 轮」，而不是「用户打的字」；
  OpenAI encoder 按 part 逐条展开即可（`ToolResult` → `role:"tool"`），不存在混合消息。
  真出现「文本 + 工具结果」同条，是构造 bug，不是需要支持的形状。
- 布局不变：`Role` 少一个成员不改变 `size_of`，但语义少了一层厂商泄漏。

### D4：尺寸上限由编译期断言守护

```rust
const _: () = assert!(size_of::<ContentPart>() <= 64);
const _: () = assert!(size_of::<Message>() <= 32);
```

- 一旦有人给某个变体塞 3 个内联 `String`，`ContentPart` 会涨到 80B，`Message` 的胖指针之外还要多背内存；让编译失败。
- 因此**大字段必须 `Arc` 或装箱**。这不是风格偏好，是布局约束。

`ContentPart` 的变体（与 [0001](0001-ir-layering.md) 一致，只补尺寸说明）：

```rust
pub enum ContentPart {
    Text(Arc<str>),                                                  // 16B
    Reasoning {                                                      // 40B
        text: Arc<str>,
        signature: Option<Arc<str>>,
        native: Option<Arc<Native>>,
    },
    ToolCall {                                                       // 48B
        id: Arc<str>,
        name: Arc<str>,
        input: ToolInput,          // enum，内含 Arc<str> 或 Box<Value>
    },
    ToolResult {                                                     // 32B
        call_id: Arc<str>,
        contents: Box<[ToolResultPart]>,
        is_error: bool,
    },
    Image(Arc<Image>),          // 图片永远装箱：data 是 Arc<[u8]>
    Opaque(Arc<Native>),        // provider 私有块，按需
}
```

### D5：字符串类型由「装在什么容器里」决定

- `ContentPart` 里的短字符串也用 `Arc<str>`（id、工具名）：`String` 会把 `ContentPart` 从 56B 撑到 80B，破坏尺寸上限。
- 已经装箱的内部结构（`ToolResultPart`、`Native`）用 `String`/`Box<str>`，不要再套引用计数。
- 一句话：**共享层数由「谁需要共享」决定，不是为了省内存而层层 `Arc`。**

### D6：冷数据进 `meta`，不进热结构

```rust
pub struct MessageMeta {
    pub id: MessageId,               // UI/事件定位
    pub created_at: Timestamp,
    pub origin: Option<Origin>,      // 产出它的 provider / api / model
    pub stop: Option<StopReason>,    // 仅 assistant
    pub usage: Option<Usage>,        // 仅 assistant
}
```

- 这些字段在 encode/回放/压缩热路径上一次都不读，内联进来会让每条消息多 40~80B。
- `Option<Box<MessageMeta>>` 只占 8B。用 `Box` 而非 `Arc`：`Message` 本身已经被 `Arc` 共享，meta 不需要第二层共享。

### D7：压缩是 `Thread` 的段，不是消息

```rust
pub enum Segment {
    Messages(Vec<Arc<Message>>),
    Compaction(Compaction),
}

pub enum Compaction {
    Summary { text: Arc<str>, covers: Range<usize> },
    ProviderNative { provider: ProviderId, items: Box<[Native]>, covers: Range<usize> },
}
```

- 压缩要表达「替换了哪一段历史」，天然是容器级概念。
- 好处：压缩后可以**直接 drop 被覆盖的 `Arc<Message>`**，内存立刻回收。
- 与 Zed 的 `Message::Compaction` 对比：Zed 把它塞进消息枚举，导致每次 `to_request()` 都要特判「压缩消息怎么变成请求消息」。

### D8：历史是 `Vec<Arc<Message>>`，请求只克隆 `Arc`

```rust
pub struct Request {
    pub messages: Vec<Arc<Message>>,   // 克隆 = 8 B/条，不复制 payload
    pub tools: Vec<Arc<Tool>>,
    // ... sampling options
}
```

- 真正要避免的是「为一次请求把历史重新构造一遍」：Zed 的 `to_request()` 会重建 owned 的
  `Vec<LanguageModelRequestMessage>`，请求期峰值约为借用式的 2 倍
  （[history-bytes](experiments/history-bytes/) 的 Z 组：常驻 1914 + 重建 1914 ≈ 3828 B/条）。
- v0 用 owned `Vec<Arc<Message>>`，而不是 `&'a [Arc<Message>]`：后者的收益只是省的这一次
  `8 B × N` 的小分配，代价是请求类型带生命周期、无法跨重试/队列持有。**最贵的那部分
  （payload）已经被 `Arc` 变成 O(1) 了。**
- encoder 借用 `&Request` 直接写 body（[0002](0002-memory-first.md) D2），请求发完即 drop。
- 需要 O(1) 历史快照时，把 `Vec<Arc<Message>>` 再包一层 `Arc<Vec<..>>`；追加 COW 复制 8B/条。**v0 不做。**

## 后果

**正面：**

- 真实分布下语义 IR 只比最优 OpenAI 形状贵 1.7%，同时保留跨 provider 表达力和 8 B/条的派生成本。
- 布局有编译期护栏（`ContentPart ≤ 64B`、`Message ≤ 32B`），冷字段或大字段被迫装箱。
- 压缩能真正释放内存（drop 被覆盖的 `Arc`），而不是留着历史只改发送内容。

**代价：**

- 每条消息多一次 parts 堆分配（相对 OpenAI 的单 `String`），短消息密集时约 +96 B/条。
- 短文本仍付 `Arc<str>` 的 16B 头 + 一次分配。
- 测试包含 `unsafe` 分配器（仅文档目录，不进产物）。

**重新评估的触发条件：**

- **采样出真实的 part 分布**，若与 31/29/40 显著不同，重跑本实验（这是最敏感的输入）。
- 若短消息（<64B）占比 >70% 且分配次数成为瓶颈 → 评估小字符串内联（类似 `SmolStr`）。
- 若需要「只换 parts、保留 meta」的高频操作 → 重新评估 `Arc<[..]>` 或 `Arc<MessageMeta>`。
- 若一条消息常见 part 数 >8（多图、并行工具）→ 重新评估 parts 装箱策略。
- 需要跨进程/跨语言传历史 → `Message` 的 serde 表示变成契约，需要版本化 ADR。

## 附录：实现状态（2026-11-12）

已按本 ADR 重构，`cargo test --workspace` 与 `cargo clippy --all-targets` 通过。

| 决策 | 落点 |
|---|---|
| D1/D2 `role + Box<[ContentPart]> + Option<Box<MessageMeta>>` | `packages/aa_language_model/src/request/message.rs` |
| D3 三个角色，工具结果是 part | 同文件 `Role`；`aa_ai_openai` 派生 `role:"tool"` |
| D4 编译期尺寸断言 | 同文件，`size_of::<ContentPart>() ≤ 64`、`size_of::<Message>() ≤ 32` |
| D5 短字符串用 `Arc<str>` | `ContentPart` 各变体 |
| D6 冷数据装箱 | `MessageMeta` / `Origin` |
| D8 历史 `Vec<Arc<Message>>` | `aa_language_model::request::Request`；`aa_agent::AgentLoop` 用它保存历史 |

实现时确认/调整的细节：

- **删掉了 `request::content::Content`**，统一用 `ToolResultPart`（文本 / 图片）。
  `AgentTool::Output: Into<ToolResultPart>`，注册表返回 `ToolOutcome { part, is_error }`。
- **`LanguageModelClient` 改成 dyn-safe**：`fn stream_completion<'a>(..) -> Pin<Box<dyn Future + Send + 'a>>`。
  用 `impl Future` 返回值会让 `&dyn LanguageModelClient` 不成立，而 agent loop 需要按运行时切 provider。
- **工具定义排序稳定**：注册表用 `BTreeMap`，否则每轮请求的工具顺序抖动会破坏 prompt cache。
- **修掉 agent loop 的一个真实 bug**：原来没有把 assistant 的 `tool_calls` 写回历史，
  只追加了工具结果；下一轮模型看不到自己发过的调用。现在顺序固定为
  `assistant（text + tool_call）→ user（批合并的 tool_result）`，并有回归测试。
- **`ToolResult` 的字段叫 `content`**（与 Zed 的 `LanguageModelToolResult.content` 对齐）；
  消息的字段/访问器也叫 `content`，不用 `parts`。
- **`Message` 只保留最小 API**：`new` / `role` / `content` / `meta` / `with_meta`。
  Zed 把字段设为 `pub`，所以只写了 `string_contents` / `contents_empty` 两个方法；
  我们的字段私有（保护 32B 布局），因此需要构造器和访问器，但不再为每种角色
  提供 `Message::user(..)` 之类的语法糖。
- `Message` / `ContentPart` **不实现 `serde`**：wire 形状是 provider 的私有契约，
  canonical IR 暂不承诺序列化格式。
- **OpenAI wire body 是强类型结构体**：`ChatCompletionRequest` / `ChatMessage` 等
  只存在于 `aa_ai_openai::request`，由
  `aa_ai_openai::request::to_chat_completion_request(&Request, model_id)`
  生成（借用而非消费；Zed 对应的 `into_open_ai` 是 `into_`，因为它的请求历史是 owned）。
  字段写错在编译期失败，而不是等 JSON 被服务端拒绝。

## 附录：实现进展

- 2026-11-13：client trait 改名 `LanguageModelClient`，签名改为返回 `'static` future
  （方法体内同步完成编码、不再借用 `&'a Request` 进 future）。见 [0006](0006-wire-request-shape.md) D2。
  本条只记录实现变化，不修改上文 D 系列决策的结论。
- 2026-11-13：wire 请求体定型为带 `'a` 的强类型 DTO + `to_writer` 借用序列化，
  兑现本文第 244-248 行的「强类型结构体、借用而非消费」，详见 [0006](0006-wire-request-shape.md) D1。

## 附录：待验证

- [ ] 真实会话的 part 分布采样（比任何布局优化都影响结论）。
- [ ] `Vec<Arc<Vec<Arc<Message>>>>`（O(1) 历史快照）的追加成本 vs 收益。
- [ ] 小字符串内联对短消息密集负载的影响。
- [ ] `Message` 的 serde 表示与版本迁移（持久化前置）。
