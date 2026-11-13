# ir-size：IR 形状的栈上开销

> 关联 ADR：[0002-memory-first](../0002-memory-first.md)
> 问题：OpenAI 兼容格式更常见，canonical IR 是否应该做成 OpenAI 形状以省内存？
>
> **⚠ 本实验的结论已被 [history-bytes](../history-bytes/) 修正。** 这里只量 `size_of`（栈上信封），
> 不含 `ContentPart`、`Arc` 头、字符串 buffer；补全堆分配后，朴素 OpenAI 与语义 IR 几乎持平（192 vs 200 B/条）。
> 本实验保留作为「为什么 `size_of` 不够」的案例。结论以 [0003](../0003-alloc-measurement.md) 为准。

## 假设

“IR 长得像 OpenAI 请求体”能减少内存占用。

## 方法

用 `std::mem::size_of` 测量三种表示的每条消息栈上开销：

- **A. OpenAI owned**：`Option<String>` + `Vec<OaCall>` + `Option<String>` 全内联；
- **B. 语义 IR**：`enum ContentPart` + `Arc<[ContentPart]>`；
- **C. 借用式 OpenAI DTO**：字段全是 `&str`，只用于序列化，瞬态。

## 运行

```sh
cd docs/decisions/experiments/ir-size
cargo run --release -q
```

## 输出快照（2026-11-12，x86_64，rustc 1.99）

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

## 解读

- ~~**OpenAI owned 形状每条消息 128B，语义 IR 只有 24B。** 假设被否定：把 IR 做成 OpenAI 形状不会更省内存，反而多 5 倍信封开销，因为可选字段全部内联。~~
  **（已修正）** 这只说明栈上信封大小；整条历史按分配器实测持平。见 [history-bytes](../history-bytes/)。
- `ContentPart` 是 64B（最大变体决定），但消息里存的是 `Arc<[ContentPart]>`，所以大块数据在堆上，信封不随内容变大。
- 借用式 DTO 是 88B/条，但它是瞬态的；用 `serialize_seq`/`collect_seq` 可以连这个临时 `Vec` 都省掉。

## 局限

- 只测了**栈上 size_of**，没测堆上 payload 和分配次数。
- 没有测“构造 owned 请求”与“借用式序列化”的峰值 RSS。
- 没有覆盖工具调用密集、图片密集的历史。

## 下一步

1. 用 `GlobalAlloc` 计数器测两种编码路径的分配次数。
2. 用 200k token 级历史测请求期峰值 RSS，确认没有第二份历史。
3. 比较压缩前后（summary vs provider-native）的稳态内存。
