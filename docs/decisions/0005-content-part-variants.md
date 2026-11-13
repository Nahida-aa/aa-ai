# 0005. `ContentPart` 变体增补：加 `RedactedThinking`，不加原生压缩记录

> 状态：accepted
> 日期：2026-11-13
> 关联：[0001](0001-ir-layering.md)（enum 表达封闭语义）、[0004](0004-message-layout.md)（Message 布局与 D7 压缩）、
> [`docs/compare/content-part-ir.md`](../compare/content-part-ir.md)（四家 ContentPart 对照）
>
> **归属：两条决策都由用户拍板（2026-11-13）。** 本文的选项整理、源码核对与后果分析由助手完成。

## 背景与问题

讨论 `ContentPart` 需要哪些变体时，出现两个候选，四家实现里都能找到对应概念：

1. **加密思考块** —— Anthropic `redacted_thinking`；
2. **provider 原生压缩记录** —— Zed 的 `Compaction`、rig 的 `Opaque`（注释写作 "a compaction record"）。

## 验证（源码事实）

### 加密思考

| 库 | 处理 |
|---|---|
| Zed | 专用变体 `MessageContent::RedactedThinking(String)`（`language_model_core/src/request.rs:270`）；OpenAI encoder 跳过（`open_ai/src/completion.rs:124,466`） |
| rig | 无专用变体：`Reasoning { text, redacted: bool, native: Option<Native> }`（`completion/message.rs:374`），密文在块级 `native` 里 |
| genai | `Custom(CustomPart { data: block })`（`adapter/adapters/anthropic/streamer.rs:183`） |
| TanStack AI | `ThinkingPart { redacted?: boolean, signature?: string }`（`packages/ai/src/types.ts:541`） |

wire 形状：`{"type":"redacted_thinking","data":"<base64>"}`。genai 的注释写得最清楚：
"arrives whole, with its encrypted `data` and no deltas, and is replayed as is"。

### 原生压缩

| 库 | 处理 |
|---|---|
| Zed | `MessageContent::Compaction(CompactedContext)`，两种形状：`Summary { content, provider_state }` 与 `ProviderState(record)` |
| rig | 无类型，走 `Opaque`（注释："such as a hosted-tool step or a compaction record"） |
| genai | 无 |
| TanStack AI | 无 |

Zed 的两个 encoder 用法（决定性的证据）：

- Anthropic（`anthropic/src/completion.rs:264-281`）：`Summary` → `RequestContent::Compaction { content, encrypted_content }`；
  裸 `ProviderState(_)` → `Ok(None)`，**直接丢弃**。
- OpenAI Responses（`open_ai/src/completion.rs:467-486`）：`ProviderState(state)` →
  `content_parts.clear()` 然后 `provider_items.extend(items)`，多次压缩时后者覆盖前者。

format 串是版本化的：`anthropic.messages.encrypted-content.v1`、`openai.responses.input-items.v1`。

## 当时的想法（选项）

### 加密思考

| 方案 | 形状 | 代价 |
|---|---|---|
| A 专用变体 | `RedactedThinking(Arc<str>)` | 16B，枚举最大变体仍是 `ToolCall`（56B），尺寸不变 |
| B 并进 Reasoning | 加 `redacted: bool` | `Reasoning` 32→40B；wire 上是两种 block，编码器仍要分支 |
| C 只用 `Opaque` | `format` 里写死 | 0 新代码，但把固定形状降级成运行期字符串匹配，payload 多包一层 `Value` |

### 原生压缩

| 方案 | 形状 | 问题 |
|---|---|---|
| A 放进 `ContentPart` | `Compaction(CompactedContext)`（Zed 的做法） | 编码器要实现「清空此前累积的输入」，是跨消息副作用 |
| B 放历史 / segment 层 | `Segment::{Messages, Compaction}`（0004 D7 的提案） | 语义对，但这一层还没实现 |
| C 暂时用 `Opaque` 顶 | format 串照抄 provider | 「遇到这个 format 就清空历史」变成隐式协议 |

## 决策

### D1：加 `RedactedThinking(Arc<str>)`（用户决定）

- 位置：紧跟 `Reasoning`，保持「思考 → 文本 → 工具调用」的顺序（Anthropic 对块顺序敏感）。
- 体积：16B；`size_of::<ContentPart>()` 不变（最大变体仍是 `ToolCall`）。
- 编码器：OpenAI 跳过；Anthropic 原样发出。
- **不**采用 `Reasoning { redacted: bool }`：签名只是思考块的一个字段，而 redacted 整块
  是密文；合并只会把分支从类型系统挪回字段。
- 已知代价：在 `aa_ai_anthropic` 落地前它是死变体（只有 OpenAI 的跳过分支）。

### D2：原生压缩记录不进 `ContentPart`（用户决定）

- 理由：它不是「模型会读的内容」，而是**替换窗口**——Zed 的 OpenAI encoder 用
  `content_parts.clear()` 证明这一点。放进 per-message 的 `content` 数组，粒度是错的。
- 落点：`Segment::{Messages, Compaction}`（[0004](0004-message-layout.md) D7），等真正做
  压缩时实现，并沿用 Zed 的两个形状（`Summary` + `ProviderState`）。
- 在那之前的临时出口：`Opaque`，`format` 照抄 provider 的版本化名字。

## 后果

**正面：**

- 接 Anthropic 时不用再改 `ContentPart` 形状；加密思考有类型安全的位置，不需要运行期匹配字符串。
- 压缩没有污染内容数组，encoder 不必实现跨消息副作用。

**代价：**

- `RedactedThinking` 暂时是死变体。
- 压缩暂时没有载体；必须用时只能走 `Opaque`。

**重新评估的触发条件：**

- 若 `redacted_thinking` 的 `data` 实际是二进制而非 base64 文本 → 改成 `Arc<[u8]>`。
- 若要做压缩 → 先实现 segment 层（新 ADR），**不要**在 `ContentPart` 上加变体。
