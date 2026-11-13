# history-bytes：整条历史的真实常驻字节

> 关联 ADR：[0003-alloc-measurement](../0003-alloc-measurement.md)、[0004-message-layout](../0004-message-layout.md)
> 问题：IR / `Message` 的布局到底省不省内存？

## 这个实验推翻过三次自己的结论

| 版本 | 假设 | 结果 |
|---|---|---|
| v1 | 语义 IR 比 OpenAI 形状省 5 倍（`size_of` 128 vs 24） | **错**：漏掉全部堆分配 |
| v2 | 补全堆后，语义 IR 仍比 OpenAI 形状省 | **错**：短文本下反而贵 80 B/条 |
| v3 | 单 part 内联（`Parts::One`）能补回这笔开销 | **错**：真实 part 分布下单 part 只占 31%，内联槽位是浪费 |

保留三次修正的过程，因为**错在方法上，不在数字上**——每次都是度量口径漏了一个维度。

## 方法

用 `GlobalAlloc` 包装 `System`，统计整条历史构造完成后的常驻增量。两个口径：

| 口径 | 含义 |
|---|---|
| `req` | 请求的字节数，不含分配器元数据 |
| `chunk` | 按 glibc malloc 估算的块大小 `align_up(max(size,1)+8, 16)`，最小 32B |

`chunk` 惩罚分配次数多的方案，更接近真实 RSS。

**公平性处理**：OpenAI 形状把多个 part 合并进 `content` 时预分配 `capacity`，避免 `realloc` 噪音；所有候选都使用同一份计划、同样的 payload。

## 关键假设：part 数分布

这是本实验里**最影响结论**的输入。真实会话中：

- assistant 消息**至少 2 个 part**（思考 + 文本，或思考 + 工具调用）；
- 只有用户消息通常是单 part；
- 并行工具调用的结果会合并成一条多 part 消息。

采用的分布（`realistic_plan`）：

| 占比 | 消息 | parts |
|---|---|---|
| 20% | 用户 | 1 |
| 20% | 助手（纯回答） | 2（思考 + 文本） |
| 30% | 助手（调工具） | 3（思考 + 文本 + 工具调用） |
| 30% | 工具结果（批量合并） | 1~3 |

即 **1 part : 31%、2 parts : 29%、3 parts : 40%**，平均 2.1 parts/条。另有一组敏感性负载（工具结果拆成单条消息 → 单 part 占 80%）。

## 候选布局

| 标记 | 布局 | `size_of` |
|---|---|---|
| A | 朴素 OpenAI（冷字段全内联） | 128 |
| A2 | 精简 OpenAI（冷字段装箱） | 40 |
| B2 | `role + Arc<[ContentPart]>` | 24 |
| B3 | `role + Parts::One(ContentPart) \| Many` | 64 |
| B4 | `role + Parts2`（≤2 part 内联） | 120 |
| **B5** | **`role + Box<[ContentPart]>`** | **24** |

`ContentPart`（含 Text / Reasoning / ToolCall / ToolResult）为 56B。

## 运行

```sh
cd docs/decisions/experiments/history-bytes
cargo run --release -q
```

## 输出快照（2026-11-12，x86_64，rustc 1.99，1000 条消息）

```
-- 均匀：1 part，payload = 64 B --         req B/msg | chunk B/msg | allocs/msg
A  朴素 OpenAI                              192.0 |  208.0 | 1.00
A2 精简 OpenAI                              104.0 |  120.0 | 1.00
B2 Arc<[ContentPart]>                       200.0 |  232.0 | 5.00
B5 Box<[ContentPart]>                       184.0 |  216.0 | 4.00
B3 单 part 内联                              168.0 |  200.0 | 4.00
B4 ≤2 part 内联                              224.0 |  248.0 | 4.00

-- 均匀：2 parts，payload = 512 B --
A  朴素 OpenAI                             1152.0 | 1168.0 | 1.00
A2 精简 OpenAI                             1064.0 | 1080.0 | 1.00
B2 Arc<[ContentPart]>                      1232.0 | 1288.0 | 7.00
B5 Box<[ContentPart]>                      1216.0 | 1272.0 | 6.00
B3 单 part 内联                             1272.0 | 1336.0 | 7.00
B4 ≤2 part 内联                             1200.0 | 1240.0 | 6.00

-- 真实分布（1 part 31% / 2 parts 29% / 3 parts 40%，平均 payload 1619 B）
A  朴素 OpenAI                             1841.4 | 1893.9 | 2.98
A2 精简 OpenAI                             1829.1 | 1894.3 | 3.77
B2 Arc<[ContentPart]>                      1883.0 | 1942.4 | 9.73
B5 Box<[ContentPart]>                      1867.0 | 1926.4 | 8.73
B3 单 part 内联                             1900.5 | 1965.4 | 9.42
B4 ≤2 part 内联                             1919.3 | 1971.5 | 9.12

-- 敏感性：工具结果拆成单条消息（1 part 80%）
A  朴素 OpenAI                             1455.4 | 1504.7 | 2.76
A2 精简 OpenAI                             1447.7 | 1510.4 | 3.59
B2 Arc<[ContentPart]>                      1473.5 | 1525.0 | 8.20
B5 Box<[ContentPart]>                      1457.5 | 1509.0 | 7.20
B3 单 part 内联                             1468.9 | 1523.6 | 7.59
B4 ≤2 part 内联                             1505.5 | 1549.7 | 7.43

-- 克隆整条历史（真实分布）
A2 clone（深拷贝）                          1763.7 | 1827.8 | 3.77
B5 clone Vec<Arc<Message>>                    8.0 |    8.0 | 0.00
```

## 解读

1. **`Box<[ContentPart]>`（B5）在所有负载下都是最优的语义布局**：真实分布 1926 vs B2 1942、B3 1965、B4 1972。因为 `Arc<Message>` 已经共享整条消息，parts 数组再带引用计数是纯开销（每消息 +16B + 一次原子操作）。

2. **`Parts::One` 内联应当放弃。** 它只在「单 part 占绝对多数」时成立：均匀 1 part 下 200 vs B5 216（省 16B），但真实分布下 1965 vs 1926（**贵 39B**）——内联槽位对 69% 的多 part 消息是纯浪费。即使把工具结果全拆成单条（单 part 80%），也只是打平（1523.6 vs 1509.0）。

3. **`≤2 part 内联`（B4）更差。** 120B 的内联 `Message` 只在恰好 2 part 时省一次分配，3 part 时仍要堆分配，还多背了 96B。只在专门为 2-part 设计的负载里（均匀 2 parts：1240 vs 1272）才勉强赢。

4. **真实分布下，语义 IR 与最优 OpenAI 形状几乎打平**：B5 1926 vs A2 1894，差 **1.7%**。此前「语义 IR 更省」和「OpenAI 形状更省」两种说法都只在特定负载下成立。

5. **克隆差距与布局无关、与历史长度成正比**：8 B/条 vs 1828 B/条（**228 倍**），且零分配。这是语义 IR 唯一稳定的优势。

6. **单 part 场景仍有一笔固定开销**：B5 216 vs A2 120（均匀 64B）。来源是 `ContentPart` 枚举 56B + 每个 `Arc<str>` 的 16B 头和一次分配。这笔开销在长消息下被摊薄，在短消息密集的历史里最明显。

## 局限

- `chunk` 是 glibc 公式估算，不是真实 RSS；jemalloc/mimalloc 的尺码类不同。
- part 分布是**建模值**，不是采样值。改变分布会改变结论——这是本实验最敏感的输入，代码里可调。
- 只测 x86_64；`size_of`/对齐在 aarch64 可能不同（结论方向不变）。
- 没有测压缩后历史、多分支并发、持久化快照、序列化峰值。

## 下一步

1. 从真实会话采样 part 分布（权重比任何布局优化都影响结论）。
2. 压缩前后稳态字节（summary 替换 N 条）。
3. 多分支并发持有同一历史。
4. `/proc/self/statm` 交叉验证 `chunk` 估算。
