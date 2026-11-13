# 0002. 内存优先：canonical IR 面向语义，编码器面向 OpenAI

> 状态：accepted，**证据已修正**
> 日期：2026-11-12
> 关联：[0001](0001-ir-layering.md)、[0003](0003-alloc-measurement.md)（修正本文「验证 1」）、[ir-size 实验](experiments/ir-size/)、`docs/zed/language_models.md` §10.6
>
> **⚠ 修正（2026-11-12）：** 本文「验证 1」用 `size_of` 得出的 **「语义 IR 24B/条 vs OpenAI 128B/条，省 5 倍」不成立**。
> 那只是栈上信封，漏掉了 `ContentPart`、`Arc` 头、字符串 buffer。按分配器实测（[0003](0003-alloc-measurement.md)），
> 均匀短文本下语义 IR 是 **200 B/条**、最优的 OpenAI 形状是 **104 B/条**；
> 混合真实负载下两者只差 **2.3%**（信封是每消息固定 ~80 B，不随 payload 增长）。
> 本文 D1/D2 决策**保持不变**，但理由改为「跨 provider 可表达 + 派生零拷贝（克隆 8 B/条、零分配）」。
> 阅读顺序：先看 [0003](0003-alloc-measurement.md)，本文的「验证 1」仅作历史记录。

## 背景与问题

aa-ai 的性能优先级是：**内存占用第一，吞吐第二**。理由：吞吐主要由提供商决定（TTFT、生成速度），架构能改变的是内存；而 coding agent 的会话会持续增长，长期占用和峰值都会成为问题。

在这个前提下有一个具体决策点：

> OpenAI 兼容 API 格式更常见，那 canonical IR 是不是应该向 OpenAI 格式倾斜，以换取内存效率？

## 当时的想法（选项）

| 方案 | 做法 | 预期收益 | 风险 |
|---|---|---|---|
| A | IR 直接是 OpenAI JSON（`serde_json::Value` 或原始 body） | 转发时几乎零转换 | `Value` 节点开销大；有损；类型安全差 |
| B | IR 是 OpenAI 形状的 typed struct | 兼容提供商映射简单 | 无法表达 Anthropic/Gemini 深层语义；信封字段多 |
| C | 语义 IR + **借用式** OpenAI encoder | 只分配 body，不复制历史 | 需要写借用适配器和 `Serialize` |
| D | 语义 IR + owned OpenAI DTO 再序列化 | 实现简单 | 多一份瞬态 DTO，收益不如 C |

关键区分：**“格式常见”影响的是转换代码量，不是保留的字节数。** 内存由三件事决定：

1. 历史/大块数据被复制几份；
2. 是否保留原始响应（raw）；
3. 会话是否有上限（压缩）。

IR 字段排成什么形状，只影响每消息的信封大小和映射代码。

## 验证

### 1. 类型尺寸实验

代码：[`experiments/ir-size/`](experiments/ir-size/)（独立 Cargo 工程，`cargo run --release` 即可复现）。

在本机（x86_64, rustc 1.99）的输出：

```
size_of OaCall          = 72
size_of OaMessageOwned  = 128
size_of ContentPart     = 64
size_of SemanticMessage = 24
size_of OaMessageRef    = 88
size_of OaCallRef       = 48

1000 msgs: OpenAI owned = 128000 bytes
1000 msgs: semantic Arc = 24000 bytes
```

**结论（已被 [0003](0003-alloc-measurement.md) 修正，见文首警示）：** `size_of` 上 OpenAI 形状的 owned 消息是语义 IR 的 5 倍多（128B vs 24B），因为 `Option<String>`、`Vec<ToolCall>`、`Option<String>` 全部内联。**但这不能代表整条历史的占用**：`ContentPart`、`Arc` 头、文本 buffer 都在堆上，补全后两者持平（192 vs 200 B/条），而精简 OpenAI 形状只有 104 B/条；在混合真实负载下四种形状只差 2.3%（[0003](0003-alloc-measurement.md)）。

借用式 OpenAI DTO 是 88B/条，但它是**瞬态**的（只活在序列化栈帧），不构成常驻内存；而且用 `collect_seq` 可以连这个 `Vec` 都省掉。

> 实验的局限：只测了栈上 `size_of`，没有测堆上 payload 和分配次数。**这个局限后来被证明是致命的**，见 [history-bytes](experiments/history-bytes/)。

### 2. 三家实现的源码核对

详见 `docs/zed/language_models.md` §10.6，摘要：

| 维度 | Zed | genai | rig |
|---|---|---|---|
| 历史 | `Vec<Arc<Message>>`，工具结果 `Arc<str>`、图片 `SharedString` | 调用方拥有，文本 `String`、base64 `Arc<str>` | `Vec<Arc<Message>>` |
| 每次请求 | 深拷贝文本/thinking | 按值传入，调用方通常 clone 历史 | 只克隆 `Arc` 指针 |
| 回复期 | 默认不留 raw | capture 默认关 | **始终**重建完整 raw `Value` |
| 工具参数 | `push_str` | `format!` 每 delta，O(n²) | `push_str` |
| 压缩 | 有 | 无 | 无 |

结论：

- **内存优势来自共享/借用和压缩，不来自“IR 长得像 OpenAI”。**
- rig 的历史共享最好，但回复期固定保留完整 raw 文档，是内存优先场景的反面教材。
- genai 的 `format!` 拼接工具参数是 O(n²) 拷贝，也是反面教材。

### 3. 语义损失核对

OpenAI Chat Completions 表达不了：

- Anthropic 的有序 signed thinking block（`signature` 随 thinking 文本一起回放）；
- Anthropic 的 `cache_control` 断点；
- Gemini 每个 function call 的 `thoughtSignature`；
- 图片型 tool result、custom tool 的 grammar 格式；
- provider 的服务端压缩状态（`ProviderCompactionState`）。

如果 IR 向 OpenAI 倾斜，这些只能塞进 `extra: Value`，而 `Value` 的每节点开销和歧义会让内存与正确性同时变差。

另外，“OpenAI 格式”本身不是一个：Chat Completions 与 Responses API 的 item 模型差别很大（`previous_response_id`、encrypted reasoning、input/output items）。向其中一个倾斜会牺牲另一个。

## 决策

### D1：canonical IR 面向语义，不向任何 wire 格式倾斜

- 历史与请求都使用 [0001](0001-ir-layering.md) 定义的语义 `ContentPart`。
- OpenAI 特有但无法归一的字段（`reasoning_content`、`reasoning_details`）放 `Native`/`Opaque` 旁路，不改造 IR 主体。

### D2：OpenAI 的红利放在 encoder，用借用式序列化

- `Wire::encode` 直接从 canonical IR 借用字段，构造 `Serialize` 适配器（`OaMessageRef<'a>`），用 `serde_json::to_writer` 写入可复用 `Vec<u8>`。
- 用 `serialize_seq`/`collect_seq` 避免先 `collect` 成 `Vec<OaMessageRef>`。
- 编码完成后立刻 drop 请求，异步任务只持有 body。
- 目标：一次请求的峰值 = 历史（共享）+ body，不出现第二份历史。

### D3：内存优先的其他硬规则（与 provider 无关）

1. 历史用 `Vec<Arc<Message>>`，大块数据用 `Arc<str>` / `Arc<[u8]>` / `Bytes`。
   **共享只做一层**（见 [0003](0003-alloc-measurement.md) D3）：`Arc<Message>` 已共享消息后，`parts` 内的短字符串不要再套 `Arc`。
2. 原始 provider 文档默认不保留；需要时 opt-in、有界。
3. 流式用有界通道（~32）+ 背压，核心库不累积文本。
4. 工具参数用 `String::with_capacity` + `push_str`，禁止每 delta `format!`。
5. 第一天就设计压缩（summary 或 provider-native），压缩后丢弃不再需要的 thinking/签名/旧工具输出。
6. IR 用 typed enum，不用 `Value`；`Native` 按需 `Arc<Value>`。
7. 模型目录 lazy，只装活跃模型。

## 后果

**正面：**

- 兼容 OpenAI 的提供商能享受“接近零成本”的编码路径，同时不需要为语义牺牲正确性。
- Anthropic/Gemini 的深层语义仍能无损往返。
- 峰值内存可预测：body + 共享历史。

**代价：**

- 每个 provider 要写一个 borrow-friendly 的 `Serialize` 适配器，比 `serde_json::to_value(&struct)` 多一些代码。
- 需要测分配次数/峰值 RSS 来验证，不能只看吞吐基准。

**重新评估的触发条件：**

- 如果实测发现借用式编码的复杂度带来的收益 < 5% 峰值 RSS，可以退回 owned DTO（方案 D），但**不要**退回方案 A/B。
- 如果将来只做网关/透传（不解析语义），方案 A 可能重新变得合理；但那不是 agent 框架。
- 如果长期只用 OpenAI 且使用服务端会话状态（`previous_response_id`/`store`），可以考虑减少本地历史；这需要单独 ADR，且要接受 provider 锁定。

## 附录：待做的验证

- [x] 用全局分配计数器测「整条历史的常驻字节与分配次数」（[history-bytes](experiments/history-bytes/)，结果见 [0003](0003-alloc-measurement.md)）。
- [ ] 用全局分配计数器测「构造 owned 请求 + 序列化」 vs 「借用式序列化」的分配次数与峰值 RSS。
- [ ] 用 200k token 级历史测请求期峰值，确认没有第二份历史。
- [ ] 压缩前后的稳态内存对比（summary vs provider-native）。
- [ ] 多分支并发持有同一历史时的额外开销（`Arc<Message>` 共享深度）。
