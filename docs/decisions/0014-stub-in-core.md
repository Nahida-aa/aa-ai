# 0014. 离线 stub 跟 trait 放在 core，用 `stub` feature 门控

> 状态：accepted
> 日期：2026-11-25
> 关联：[0008](0008-provider-tiers.md)（provider 分层 / 一家一 crate）、
> [`docs/design/crate-layout.md`](../design/crate-layout.md)（布局现行规则）、
> [`packages/aa_language_model/src/stub.rs`](../../packages/aa_language_model/src/stub.rs)

## 背景与问题

离线测试替身 `StubClient` 原先放在汇总 crate `aa_language_models`，理由是「它不是
wire provider，没有任何厂商格式」。但 `aa_language_models` 依赖全部 `aa_ai_*`，
于是只想用替身的 crate 要连带拉进 reqwest + TLS + 三家厂商。

实际后果已经出现：`aa_agent` 的测试想记录请求、断言历史回填，因为不想为一个
假客户端引入整个 provider 树，又在 `agent_loop.rs` 里内联复制了一份
`ScriptedClient`。同一个东西两份实现，改一处漏一处。

## 当时的想法（选项）

1. **留在 `aa_language_models`（现状）**
   - 「core 只有 IR + trait」很纯；代价是测试方必须依赖整个 provider 汇总。
2. **放进 `aa_language_model`，无条件编译**
   - 测试方只依赖 core 就够；但生产构建也会带上一段永远不用的替身代码。
3. **放进 `aa_language_model`，用 `stub` feature 门控（采纳）**
   - 和 2 一样省依赖，且正常构建不带替身；代价是多一个 feature，feature 统一时
     仍可能被别的依赖打开。
4. **拆一个 `aa_language_model_stub` crate**
   - 隔离最彻底；为一个 90 行的确定性替身多一个 crate，不划算。

## 验证

- 源码核对（Zed）：trait `LanguageModelClient` 在 `crates/language_model/src/language_model.rs:432`，
  fake 也在同一个 crate，且 `#[cfg(any(test, feature = "test-support"))] pub mod fake_provider;`
  （同文件第 5-6 行）；纯 IR 的 `language_model_core` 里没有 fake。结论：Zed 的规则是
  「fake 跟 trait 走」，不是「fake 跟厂商走」。
- 本仓的 crate 切分把 Zed 的 `language_model_core` 与 `language_model` 的 trait 部分
  合并在 `aa_language_model`，所以对应位置就是 core。
- 移动后 `aa_agent` 用 `aa_language_model = { features = ["stub"] }` 作为 dev-dependency，
  删掉了重复的 `ScriptedClient`，历史回填的断言改为 `StubClient::requests()`。
- `cargo test -p aa_language_model -p aa_agent`：stub 自身 2 个测试 + agent loop 11 个测试通过。

## 决策

**`stub::StubClient` 放在 `aa_language_model`，由 `stub` feature 门控；类型只此一份。**

规则：

- `#[cfg(any(test, feature = "stub"))] pub mod stub;`；
- 汇总 crate 提供 `aa_language_models/stub` feature 转发（`stub = ["aa_language_model/stub"]`），
  `aa_language_models::stub::StubClient` 这条路径保持不变；
- 应用如果把它当运行时功能（aacode 的 `--stub`），在依赖里常开 `stub`；
  只在测试里用的 crate 放 `[dev-dependencies]`；
- `StubClient` 记录收到的每个 `request::Request`（克隆只复制 `Arc`），
  测试不用再自己写录制层。

## 后果

- 测试方不再为了替身引入 reqwest；`aa_agent` 少一份重复实现；
- core 多了一个 feature 和一个模块，但默认构建产物不变；
- 如果以后替身需要依赖厂商 DTO（不应该），那说明它已经不是替身，应该独立成 crate；
- 文档里「stub 放汇总 crate，和 Zed 放 `language_model` 同理」的说法是错的
  （本仓的 `aa_language_model` 才对应 Zed 的 `language_model_core`），已一并修正。
