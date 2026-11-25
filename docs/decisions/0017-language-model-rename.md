# 0017. 模型类型改名 `LanguageModel`，结构化输出能力位进模型事实

> 状态：accepted
> 日期：2026-11-26
> 关联：[0006](0006-wire-request-shape.md)（能力位在 encoder 里生效）、
> [0008](0008-provider-tiers.md)（一家厂商一个 crate）、
> [`packages/aa_language_model/src/provider/model.rs`](../../packages/aa_language_model/src/provider/model.rs)

## 背景与问题

两件事一起改，都是同一个问题：**「模型是什么」和「这次请求要什么」的边界不够清楚。**

1. `provider::Model` 与 Zed 的 `LanguageModel` 是同一个东西（模型事实），但名字不同。
   跨仓对照源码、读 Zed 文档时每次都要在脑子里做一次映射；`LanguageModelClient`
   的参数叫 `model: &Model` 也让「模型客户端」和「模型」两个词挤在一起。
2. `Request::response_format` 一直存在，但 encoder 直接把它发出去：
   - OpenAI 老模型 / 部分兼容端点只支持 `json_object`，不支持 `json_schema`；
   - Ollama 的 `format` 只接受裸 schema，没有 `strict`；
   - 模型不支持时端点回 400，错误信息不说明是哪个字段。

   `wire` 层无法从模型名猜出这些差异，而「这个模型能不能原生约束输出」是**模型事实**，
   和 `supports_tools` / `supports_images` 同一层。

## 决策

### D1：`Model` → `LanguageModel`，与 Zed 同名

```rust
pub struct LanguageModel {
    pub provider: String,
    pub id: String,
    // …
}

pub trait LanguageModelClient {
    fn stream_completion(&self, model: &LanguageModel, request: &Request) -> …;
}
```

- 只改名字和类型引用，字段与语义不变；
- `StopReason` / `TokenUsage` 本来就和 Zed 同名，保持；
- `aa_agent`、`fake`、三个 provider、汇总 crate 的类型签名一起换，
  不做 `pub use Model = LanguageModel` 之类的别名 —— 别名会让两个名字长期并存。

### D2：新增 `ResponseFormatSupport`，默认全关

```rust
pub struct LanguageModel {
    // …
    pub response_format_support: ResponseFormatSupport,
}

#[derive(Default)]
pub struct ResponseFormatSupport {
    pub json_object: bool,
    pub json_schema: bool,
    pub strict: bool,
}
```

判定放在模型上，而不是每个 encoder 各写一份：

```rust
model.supports_response_format(&ResponseFormat) -> bool
```

- `Text` 恒为真（等于不约束）；
- `JsonObject` 看 `json_object`；
- `JsonSchema { strict }` 看 `json_schema`，`strict: true` 还要求 `strict` 位。

### D3：encoder 先校验，不支持就 `Unsupported`

OpenAI / OpenRouter / Ollama 的 `to_*_request` 在编码前统一检查：

```rust
if let Some(format) = &request.response_format
    && !model.supports_response_format(format)
{
    return Err(LlmError::Unsupported(…));
}
```

与工具、图片同一套约定：**不静默丢弃**。发一个端点不认识的字段，只会换来
一条不含字段名的 400。

## 当时考虑过的选项

1. **把 `response_format` 支持留在各 vendor crate（纯构造参数）**
   - 现状；调用方每换一个模型都要自己记得改，漏了就等端点 400。
2. **用字符串模型名在 encoder 里匹配**
   - 模型名列表永远追不完（兼容端点可以叫任何名字）。
3. **能力位放模型事实 + 统一判定（采纳）**
   - 应用构造 `LanguageModel` 时逐位填写；库只回答「能不能发」。

## 后果

**正面：**

- 与 Zed 术语一致，跨仓对照成本降低；
- 结构化输出从「发了再看端点脸色」变成构造期可判定的能力；
- 三个 encoder 共用一条判定规则，不会出现「OpenAI 放行、Ollama 拒绝」的漂移。

**代价：**

- 公共类型改名，下游（aacode）需要同步一行类型引用；
- 新字段默认关闭：**不显式开启就发不出结构化输出**。这是刻意的，
  默认放行会让不支持 `strict` 的端点收到无法处理的字段；
- 应用侧的模型表要维护三个新布尔位（可先只开 `json_object`）。

**触发重新评估的条件：**

- 若某个厂商的「结构化输出」不是 JSON Schema 形状（例如只接受正则 / grammar），
  需要扩展 `ResponseFormat`，而不是往这三个位里硬塞；
- 若主流端点普遍默认支持 `json_schema`，可考虑把默认值改为开启，
  但仍需保留逐模型覆盖。
