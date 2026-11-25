# 0013. 会话持久化：显式事件日志，不是 IR 的 serde 形状

> 状态：accepted
> 日期：2026-11-25
> 关联：[0004](0004-message-layout.md)（消息布局 / 历史是 `Vec<Arc<Message>>`）、
> [0001](0001-ir-layering.md)（IR 分层）、
> [`packages/aa_agent/src/session.rs`](../../packages/aa_agent/src/session.rs)

## 背景与问题

agent 需要多轮会话：`chat` 之后还能接着聊，`run` 能恢复上次的工具历史。
这意味着历史要落盘，并且**恢复后重放给模型的内容必须和当时发给它的一致**
（尤其思考签名、工具调用 / 结果顺序）。

约束：

- `aa_language_model::request::message` 明确**不实现 serde**：IR 是内存契约，
  如果磁盘格式等于 IR 形状，IR 每次调整都会变成磁盘格式的破坏性变更；
- 内存优先：恢复时不要重新解析厂商 wire，也不要深拷贝整条历史；
- 崩溃不能毁掉已有对话：写入必须是追加式；
- 不接受猜测式兼容：读到不认识的版本 / 形状就明确报错。

## 当时的想法（选项）

1. **直接给 IR 加 serde，JSONL 存 `Message`**
   - 最省代码；但把 IR 形状钉成磁盘契约，违背 IR 的定位。
2. **存每次请求 / 响应的原始 wire（JSON）**
   - 可回放；但和厂商绑定，换 provider 就不可读，且思考签名等字段分散在各家格式里。
3. **显式事件日志（`SessionEvent`）+ 从事件重建 IR**
   - 多一层映射；但磁盘契约只包含「重放所需的事实」，IR 可以自由演进。

## 验证

- `aa_agent` 单测：文本 / 思考 + 签名 / 工具调用（JSON 与纯文本入参）/ 工具结果
  的 JSONL 往返；连续 `tool_result` 恢复后合并成一条 user 消息（和 agent loop
  发给模型的形状一致）。
- 端到端（`run --fake`）：新建会话 → 落盘 → `--continue` 恢复，第二轮请求的
  历史包含上一轮全部消息。
- 反例：版本号不匹配、缺 header、重复 header、非法 JSON 行都会明确报错。

## 决策

**磁盘格式是 `aa_agent::session::SessionEvent` 的 JSONL，一行一个事件，第一条必须是
`Header { version, created_at, provider, model, cwd, system }`。**

规则 / 不变量：

- 事件只有四种：`header` / `user` / `assistant` / `tool_result`；
- `assistant` 一轮一条，字段顺序固定为思考（含 `signature`）→ 文本 → 工具调用，
  和回放给模型的 `ContentPart` 顺序一致；
- 同一次模型调用产生的多条 `tool_result` 在恢复时合并成**一条** user 消息
  （canonical 里工具结果独占一条 user 消息）；
- v1 的工具结果只收文本；图片结果在**写入时**明确报错，不静默丢；
- `version` 不匹配直接拒绝，不做字段级兜底；
- 文件路径、追加时机、`--session` 命名规则属于应用（`aacode`），库层不读环境变量。

## 后果

- 加字段是兼容的（`#[serde(default)]`）；改语义必须递增 `version`；
- IR 仍然零 serde：以后换存储（SQLite / 二进制）只需换 `SessionEvent` 的编码；
- 应用可以按 `events().len()` 做增量追加，每轮落盘一次，崩溃最多丢当前一轮；
- 图片结果目前不支持持久化：工具要产出图片时，需要先给 v2 定义稳定的编码。
