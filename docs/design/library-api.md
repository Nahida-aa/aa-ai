# 库与应用的边界：配置、凭证、模型表归应用

> 状态：**生效中**（2026-11-19，用户纠正）
> 关联：[`crate-layout.md`](crate-layout.md)、[ADR 0008](../decisions/0008-provider-tiers.md)、
> [`docs/compare/provider-extension.md`](../compare/provider-extension.md)
>
> **决策归属**：用户明确要求——多提供商配置属于应用层（例如 `aacode`），
> 库层参考 TanStack AI 的形状。本文记录这条边界和两边的职责。

## 1. 一句话

**库只接收数据并发请求；配置从哪来、密钥存在哪、默认用哪个模型，全是应用的事。**

## 2. TanStack AI 的库形状（对照）

```ts
// 工厂：一份端点配置 → 一个「按模型名取 adapter」的函数
const deepseek = openaiCompatible({
  name: 'deepseek',
  baseURL: 'https://api.deepseek.com/v1',
  apiKey: process.env.DEEPSEEK_KEY!,   // 应用读 env，传给库
  models: ['deepseek-chat', 'deepseek-reasoner'],
})

// 每次调用显式传 adapter；没有全局注册表
chat({ adapter: deepseek('deepseek-chat'), messages })
```

事实（`packages/ai-openai/src/compatible/index.ts:44`、`types.ts:100`）：

- `apiKey` / `baseURL` 是**必填的构造参数**，工厂本身不读配置文件；
- `models` 是模型元组，只用于类型推导（`extendAdapter` 运行时原样返回，`ai/src/extend-adapter.ts:253`）；
- 没有 `ProviderRegistry`，也没有热重载；调用方自己持有 adapter；
- 部分包提供 `getXxxApiKeyFromEnv()` 便捷函数，但那只在调用方显式调用时生效，不是库的全局状态。

## 3. Zed / opencode 的配置在哪一层（事实）

| | 配置文件 | 凭证 | 谁读 |
|---|---|---|---|
| Zed | settings JSON：`openai_compatible: HashMap<id, {api_url, available_models, custom_headers}>`（`settings_content/src/language_model.rs:26`） | 系统钥匙串（按 api_url），env 名由 id 推导 `{ID}_API_KEY`（`provider/api_compatible.rs:31`） | Zed 应用 |
| opencode | `opencode.json` 的 `provider.<id>`（`schema/src/config/provider.ts:118`） | credential store / `auth.json`，按 provider id；`env` 名称逐条声明（`core/src/config/plugin/provider.ts:41`） | opencode 应用 |

**两家都没有把「配置文件 / 凭证 / 模型表」放进 wire 库。** 库只拿到解好的
endpoint、key、headers 和模型名。

## 4. aa-ai 的职责划分

### 库（`aa_language_model` + `aa_ai_*` + `aa_language_models`）

- `aa_ai_<vendor>::<Vendor>LanguageModelProvider`：一份**已经解好的**端点配置
  （`base_url` / 鉴权 / 额外 header）+ `LanguageModelClient` 实现；
- `aa_language_model::provider::Model`：模型事实（输出上限、tools/images、
  `max_completion_tokens`、`developer` 角色、上下文窗口）；
- `aa_language_model::request::Request` / `Message`：canonical IR；
- 可选 wire 能力：`list_models()`（`GET /models`）——只看端点返回，不读本地配置。

**不做**：配置文件解析、`{env:}` 替换、钥匙串、`id → provider` 注册表、热重载、
默认 provider、全局 API key。

### 应用（例如 `aacode`）

- 定义配置格式（JSONC / TOML / 自己的设置 UI）；
- 决定密钥来源（env / 钥匙串 / 文件），并完成替换；
- 维护 `id → (provider, Vec<Model>)` 表；
- 决定默认模型、重试、限流、模型切换 UI；
- 把解好的数据传进库：

```rust
// 应用侧（示意；不是本仓的 API 契约）
let minekuai = aa_language_models::openai::OpenAiLanguageModelProvider::without_auth()
    .with_auth_header("x-api-key", app_config.minekuai_key)
    .with_base_url("https://minekuai.cn/v1");
let agnes = aa_language_models::openai::OpenAiLanguageModelProvider::new(app_config.agnes_key)
    .with_base_url("https://apihub.agnes-ai.com/v1");
```

应用想用文件也没问题，但解析发生在应用里；库看到的只是一个已构造的 provider。

## 5. 为什么不在库里做配置

- **多实例**：同一进程要同时连多个提供商，全局 env / 单例会互相覆盖；
- **密钥生命周期**：钥匙串、OAuth 刷新、临时 token 是应用的领域，不该让 wire 库依赖；
- **可组合**：库是 `(endpoint, key, headers) → client` 的纯函数；应用可以按需从任何来源填充；
- **测试**：库的测试不该依赖环境变量或用户目录里的文件。

## 6. 已删除的库内约定

- `aa_ai_openai/examples/` 里曾用 `AA_API_KEY` / `AA_BASE_URL` 做冒烟测试，暗示
  「库有一个全局 key」。已删除；多提供商的冒烟测试放在应用层。
- `list_models()` 保留：它只发 `GET {base_url}/models`，不读本地配置，属于 wire 能力。
