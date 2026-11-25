# 设计决策记录（ADR）

> 建立日期：2026-11-12
> 目的：记录「为什么这么干」——完整的 *想法 → 验证 → 结论* 流程，而不只是最终结论。
> 方便后来加入的开发者理解约束、避免重复踩坑，也方便未来推翻决策时有据可查。

## 什么时候写

满足任一条就写一个 ADR：

- 会影响**公共类型/接口形状**（IR、trait 签名、持久化格式）；
- 有**多个合理选项**，需要说明取舍；
- 决策依赖**实测/源码核对**，不写下来以后会忘；
- 可能被**推翻或演进**，需要知道当时的触发条件。

小改动、纯实现细节不写。

## 目录约定

```
docs/decisions/
  README.md                  # 本文件：约定 + 索引
  NNNN-kebab-title.md        # 决策，一位一文件，编号递增
  experiments/
    README.md                # 实验索引
    <name>/                  # 可运行的最小验证工程（独立 workspace）
      Cargo.toml
      src/main.rs
      README.md              # 怎么跑、输出快照、解读、局限
```

## ADR 模板

```md
# NNNN. 标题

> 状态：proposed | accepted | superseded by NNNN | rejected
> 日期：YYYY-MM-DD
> 关联：其他 ADR / 代码路径 / 调研文档

## 背景与问题

要解决什么，约束是什么（内存、性能、兼容性、时间）。

## 当时的想法（选项）

至少列 2 个选项，写清各自的假设，不要只写赢家。

## 验证

- 源码核对：文件:行号 + 结论
- 实验：`experiments/<name>/` + 输出
- 反例/未验证的部分也要写

## 决策

一句话结论 + 必要的规则/不变量。

## 后果

- 正面：
- 代价：
- 触发重新评估的条件：
```

## 状态规则

- **accepted** 的 ADR 不直接改结论；要改就新写一个 ADR，把旧的标成 `superseded by NNNN`。
- 补充信息（新证据、实现进展）可以追加到文末「附录」，不改原决策。
- 实验代码可以更新，但要在实验 README 里记录输出变化。

## 索引

| 编号 | 决策 | 状态 | 日期 |
|---|---|---|---|
| [0001](0001-ir-layering.md) | IR 三层分离、内容用 enum、命名 `ContentPart` | accepted | 2026-11-12 |
| [0002](0002-memory-first.md) | 内存优先；不把 canonical IR 做成 OpenAI 形状；编码器借用式序列化 | accepted，证据由 0003 修正 | 2026-11-12 |
| [0003](0003-alloc-measurement.md) | 用分配器实测 IR 形状：推翻「5 倍」证据，共享只做一层，payload 主导 | accepted | 2026-11-12 |
| [0004](0004-message-layout.md) | `Message` 布局：`Box<[ContentPart]>`、共享靠 `Arc<Message>`、冷数据装箱、压缩用 Segment | accepted（同日修正过内联方案） | 2026-11-12 |
| [0005](0005-content-part-variants.md) | 加 `RedactedThinking`；原生压缩记录不进 `ContentPart`（走 Segment） | accepted（用户决定） | 2026-11-13 |
| [0006](0006-wire-request-shape.md) | wire 请求体定型：带 `'a` 的强类型 DTO + 借用序列化；trait 改名 `LanguageModelClient` | proposed（用户方向） | 2026-11-13 |
| [0007](0007-response-mapping.md) | 响应侧：真解析 SSE、`finish_reason` 决定 `StopReason`、`usage` 与错误体归位 | accepted（方向用户决定） | 2026-11-17 |
| [0008](0008-provider-tiers.md) | provider 分三层：协议 / 常见厂商（一家一 crate）/ 长尾端点；`Model` 只放模型事实；事件流补 `Reasoning` | accepted（分层与一家一 crate 用户决定） | 2026-11-17 |
| [0009](0009-http-transport.md) | HTTP 传输：代理语义对齐 Zed；客户端级 per-read idle 超时（默认 120s）+ keep-alive 取值照 Zed | proposed（代理语义用户决定；超时值助手提案，已落地） | 2026-11-20 |
| [0010](0010-transport-object.md) | 传输独立成 `aa_http_client`（aa-stack）：一个进程一个连接池，provider 只收 `Arc<HttpClient>`；agent loop 模型按调用传 | proposed（crate 位置与破坏性重构用户决定；接口形状助手提案） | 2026-11-20 |
| [0011](0011-ollama-native.md) | Ollama 原生 `/api/chat`：NDJSON 解码、`thinking`、`tool_name` 关联、`done_reason` + 工具调用决定 `StopReason` | accepted（用户「按建议继续」；映射规则助手提案） | 2026-11-21 |
| [0012](0012-openrouter-vendor.md) | OpenRouter 独立 crate：自写 encoder/DTO、`reasoning_details` 存 `signature`、流式 `Stop` 延后、`free` 字段 | accepted（独立 crate 用户决定；映射规则助手提案） | 2026-11-21 |
| [0013](0013-session-log.md) | 会话持久化：显式 `SessionEvent` JSONL，IR 保持零 serde；追加式、版本化、图片结果明确报错 | accepted（助手提案；用户要求先做会话而不是继续加 provider） | 2026-11-25 |

## 相关文档

- `docs/zed/language_models.md` —— Zed / genai / rig 的源码核对与多提供商实现建议（本目录决策的主要证据来源）。
- `docs/zed/provider-mapping.md` —— Zed provider 架构 + 对 TanStack 映射的修正。
- `docs/zed/acp-vs-llm-auth.md` —— `LanguageModelProvider: LanguageModelClient` 动机、四家分层对照、Zed Agent（内部 ACP）的认证写法，以及 ACP 认证与 LM 认证是两套独立类型的核实（调研记录，非 ADR）。
- `docs/compare/README.md` —— 库层对比的收录规则：哪些事实归这里、哪些归应用仓库 `aacode`。
- `docs/compare/content-part-ir.md` —— 四家 ContentPart IR 的逐字对照（含各变体的真实构造点）。
- `docs/compare/provider-extension.md` —— 四家怎么接长尾端点、怎么给常见厂商区分度（`Dialect`/`Quirks` 字段表）。
- `docs/compare/http-client.md` —— 四家的 HTTP 客户端：代理、`NO_PROXY`、连接/读取超时、流式 idle 超时的实现位置。
- `docs/design/strategy.md`、`docs/design/crate-layout.md`、`docs/design/tool-trait.md` —— 布局与命名规则、工具接口分层。
- `docs/design/library-api.md` —— 库与应用的边界：配置/凭证/模型表归应用，库只接收数据（对照 TanStack AI / Zed / opencode）。

## 实验索引

| 实验 | 验证的问题 | 结果 |
|---|---|---|
| [ir-size](experiments/ir-size/) | IR 形状的**栈上信封**大小 | 参考值：24B vs 128B；**不能代表整条历史**，见 history-bytes |
| [history-bytes](experiments/history-bytes/) | 整条历史的**真实常驻字节与分配次数**（按真实 part 分布、6 种布局） | 语义 IR 只差 1.7%；克隆 8 B/条 vs 1828 B/条；单 part 内联被证伪 |
