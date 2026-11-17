//! 消息 / 角色 / 内容片段 —— canonical IR 的会话层。
//!
//! 布局依据 [`docs/decisions/0004-message-layout.md`](../../../../../docs/decisions/0004-message-layout.md)：
//!
//! - **历史是唯一真相**：外面存 `Vec<Arc<Message>>`，克隆一条历史只加引用计数。
//! - **片段用 `Box<[ContentPart]>`**：共享单位是 `Arc<Message>`，`content` 不需要第二层
//!   `Arc`（实测省 16 B/条 + 一次原子操作）。
//! - **冷数据装箱**：id / origin / stop / usage 放 `MessageMeta`，热结构保持 32 B。
//! - **尺寸有编译期上限**：`ContentPart ≤ 64B`、`Message ≤ 32B`。加字段时如果超限，
//!   编译失败 —— 大字段必须 `Arc` 或装箱。
//!
//! 为什么内容片段是穷尽 enum 而不是 `struct { text: Option<_>, .. }`：见
//! [`docs/decisions/0001-ir-layering.md`](../../../../../docs/decisions/0001-ir-layering.md)。
//! 简言之：封闭语义用 enum（编译器逼所有 provider 处理新变体），开放实现用 `dyn`。
//!
//! 命名对齐 Zed 的 `LanguageModelRequestMessage`：字段叫 `content`（不是 `parts`），
//! 因为它装的是「这条消息的内容」，而不是某个 wire 厂商的 message part。
//!
//! 注意：canonical IR 目前**不实现 serde**。wire 编码是各 provider 的借用式 encoder
//! （见 `aa_ai_openai`），持久化格式尚未定，先不把 serde 形状变成事实契约。

use std::mem::size_of;
use std::sync::Arc;

/// 消息角色。
///
/// **故意没有 `Tool`。** 工具结果在语义上是「喂回模型的内容」，不是一种说话的人：
/// - Anthropic / Gemini 把它放在 user 轮的内容块里；
/// - OpenAI 用独立的 `role: "tool"` 消息。
///
/// 后者是 wire 形状，由 `aa_ai_openai` 的 encoder 派生，不进入 canonical IR。
/// 在 IR 里加 `Tool` 只会让「消息角色」这个概念被 OpenAI 牵着走，并逼其他 provider
/// 反向翻译。Zed 的 `Role` 同样只有这三个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// 一条消息：角色 + 有序的内容片段 + 可选冷数据。
///
/// 历史里存 `Arc<Message>`；克隆整条历史等于克隆 `Vec<Arc<Message>>`
/// （8 B/条、零分配）。不要给 `Message` 实现 `Clone`：那会诱导调用方深拷贝 payload。
///
/// 字段是 `pub`（对齐 Zed 的 `LanguageModelRequestMessage`）：可以直接读，也可以
/// `Message { .. }` 字面量构造。`new` 只是省掉 `content.into()` 的便利构造器。
#[derive(Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Box<[ContentPart]>,
    pub meta: Option<Box<MessageMeta>>,
}

impl Message {
    /// 用角色 + 任意可转成 `Box<[ContentPart]>` 的内容构造（`Vec<ContentPart>` 可直接传）。
    ///
    /// 只有这一个构造函数：工具结果、思考、图片都是 [`ContentPart`] 的变体，
    /// 不需要为每种组合再开一个 `Message::xxx`。
    pub fn new(role: Role, content: impl Into<Box<[ContentPart]>>) -> Self {
        Self {
            role,
            content: content.into(),
            meta: None,
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn content(&self) -> &[ContentPart] {
        &self.content
    }

    pub fn meta(&self) -> Option<&MessageMeta> {
        self.meta.as_deref()
    }

    /// 附加冷数据（id / origin / stop / usage）。消费 self，便于 `Arc::new(msg.with_meta(..))`。
    pub fn with_meta(mut self, meta: MessageMeta) -> Self {
        self.meta = Some(Box::new(meta));
        self
    }
}

/// 内容片段。
///
/// `size_of` 上限 64 B（编译期断言）。新增变体时如果塞进大字段（例如三个内联
/// `String`），会超出上限并编译失败 —— 请改用 `Arc` / `Box`。
#[derive(Debug, PartialEq)]
pub enum ContentPart {
    /// 普通文本。
    Text(Arc<str>),
    /// 模型思考。`signature` 是 Anthropic 等要求的签名，回放时必须一并带上。
    Reasoning {
        text: Arc<str>,
        signature: Option<Arc<str>>,
    },
    /// 被 provider 加密的思考块（Anthropic `redacted_thinking`）。
    ///
    /// wire 上是 `{"type":"redacted_thinking","data":"<base64>"}`：整块到达、没有增量、
    /// 必须**原样、按原位置**回传，否则 provider 会拒绝或丢掉思考上下文。
    /// 与 [`ContentPart::Reasoning`] 的区别：签名只是思考块的一个字段，而这里整个块
    /// 都是密文，没有可读文本。
    ///
    /// OpenAI chat completions 没有对应概念，encoder 直接跳过（与 Zed 的
    /// `open_ai` 一致）。由 `aa_ai_anthropic` 产出与回放。
    RedactedThinking(Arc<str>),
    /// 模型请求调用工具。
    ToolCall {
        id: Arc<str>,
        name: Arc<str>,
        input: ToolInput,
    },
    /// 工具执行结果，回填给模型。一条消息可以带多个结果（并行工具调用）。
    ///
    /// 约定：工具结果**独占一条 `Role::User` 消息**，不和用户文本混排
    /// （Zed 的 `AgentMessage::to_request()` 同样把工具结果拆成独立的 user 消息）。
    /// 对 Anthropic / Gemini 它就是一个普通的 user 轮；对 OpenAI 由 encoder
    /// 进一步展开成多条 `role: "tool"` 消息。出现「文本 + 工具结果」同条属于构造
    /// bug（除非将来显式支持「工具执行中用户插话合并成同一轮」）。
    ToolResult {
        call_id: Arc<str>,
        /// 结果的各段内容（文本 / 图片）。字段名对齐 Zed 的 `LanguageModelToolResult.content`。
        content: Box<[ToolResultPart]>,
        is_error: bool,
    },
    /// 图片（输入或工具产出）。
    Image(Arc<Image>),
    /// provider 私有、canonical 无法表达的内容。只对产生它的 provider 有效。
    Opaque(Arc<ProviderState>),
}

impl ContentPart {
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        Self::Text(text.into())
    }

    pub fn tool_call(
        id: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
        input: impl Into<ToolInput>,
    ) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        }
    }

    pub fn tool_result(
        call_id: impl Into<Arc<str>>,
        content: impl Into<Box<[ToolResultPart]>>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
        }
    }
}

/// 工具调用入参。
///
/// 正常是 JSON；某些模型（或解析失败时）给的是纯文本 / 未完成的 JSON 片段。
/// `Value` 装箱是为了把 `ContentPart` 压在上限内。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolInput {
    Json(Box<serde_json::Value>),
    /// 原始文本（非 JSON，或 provider 流式给到一半的片段）。
    Text(Arc<str>),
}

impl ToolInput {
    pub fn as_value(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Json(value) => Some(value),
            Self::Text(_) => None,
        }
    }

    /// 序列化成 wire 需要的形式（OpenAI 的 `arguments` 是 JSON 字符串）。
    pub fn to_wire_string(&self) -> String {
        match self {
            Self::Json(value) => value.to_string(),
            Self::Text(text) => text.to_string(),
        }
    }
}

impl From<serde_json::Value> for ToolInput {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(Box::new(value))
    }
}

impl From<&str> for ToolInput {
    fn from(value: &str) -> Self {
        Self::Text(Arc::from(value))
    }
}

/// 工具结果的一段内容。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultPart {
    Text(Arc<str>),
    Image(Arc<Image>),
}

impl From<String> for ToolResultPart {
    fn from(value: String) -> Self {
        Self::Text(Arc::from(value))
    }
}

impl From<&str> for ToolResultPart {
    fn from(value: &str) -> Self {
        Self::Text(Arc::from(value))
    }
}

impl From<Arc<str>> for ToolResultPart {
    fn from(value: Arc<str>) -> Self {
        Self::Text(value)
    }
}

impl ToolResultPart {
    /// 文本视图（图片返回 `None`）。给日志、错误提示和只收文本的 wire 用。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Image(_) => None,
        }
    }
}

/// 图片。字节/URL 都是引用计数，克隆只加引用。
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: Arc<str>,
    pub source: ImageSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    Url(Arc<str>),
    Base64(Arc<str>),
    Bytes(Arc<[u8]>),
}

/// provider 私有负载：只有 `provider` 自己知道怎么解释 `payload`。
///
/// 压缩（Anthropic `encrypted_content` 等）、thinking signature、服务端会话状态
/// 这类东西走这里，不要污染 canonical 字段。
#[derive(Debug, PartialEq)]
pub struct ProviderState {
    pub provider: Arc<str>,
    /// 负载格式，便于 provider 自己演进而不暴露给 canonical 层。
    pub format: Arc<str>,
    pub payload: Box<serde_json::Value>,
}

/// 冷数据。绝大多数热路径（encode / 回放 / 压缩）不读它，所以装箱、只占 8 B。
#[derive(Debug, Clone, PartialEq)]
pub struct MessageMeta {
    /// 会话内 id，给 UI / 事件流定位。
    pub id: Option<Arc<str>>,
    /// 产出这条消息的 provider / model（决定 provider 私有块能否回放）。
    pub origin: Option<Origin>,
    /// assistant 消息的结束原因。
    pub stop: Option<crate::provider::StopReason>,
    /// assistant 消息的用量。
    pub usage: Option<crate::provider::TokenUsage>,
    /// 创建时间（Unix 毫秒）。
    pub created_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub provider: Arc<str>,
    pub model: Arc<str>,
}

// ---- 布局护栏：任何让热结构变胖的改动都在编译期失败 ----
const _: () = assert!(size_of::<ContentPart>() <= 64);
const _: () = assert!(size_of::<Message>() <= 32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_stays_small() {
        assert!(
            size_of::<ContentPart>() <= 64,
            "{}",
            size_of::<ContentPart>()
        );
        assert!(size_of::<Message>() <= 32, "{}", size_of::<Message>());
    }

    #[test]
    fn user_message_is_one_text_part() {
        let msg = Message::new(Role::User, vec![ContentPart::text("hello")]);
        assert_eq!(msg.role(), Role::User);
        assert_eq!(msg.content().len(), 1);
        assert!(matches!(msg.content()[0], ContentPart::Text(_)));
    }

    #[test]
    fn assistant_message_keeps_part_order() {
        // 思考必须在工具调用之前：Anthropic / Gemini 对这个顺序敏感。
        let msg = Message::new(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: Arc::from("thinking"),
                    signature: Some(Arc::from("sig")),
                },
                ContentPart::text("答案"),
                ContentPart::tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({ "path": "a.rs" }),
                ),
            ],
        );
        assert_eq!(msg.content().len(), 3);
        assert!(matches!(msg.content()[0], ContentPart::Reasoning { .. }));
        assert!(matches!(msg.content()[2], ContentPart::ToolCall { .. }));
    }

    #[test]
    fn redacted_thinking_keeps_ciphertext_opaque() {
        // Anthropic `redacted_thinking` 整块是密文、没有可读文本，所以不能塞进
        // `Reasoning`（那里 text 是必填的）。见 ADR 0005。
        let msg = Message::new(
            Role::Assistant,
            vec![
                ContentPart::RedactedThinking(Arc::from("base64-ciphertext")),
                ContentPart::text("answer"),
            ],
        );
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(msg.content[0], ContentPart::RedactedThinking(_)));
        assert!(matches!(msg.content[1], ContentPart::Text(_)));
    }

    #[test]
    fn tool_results_live_in_a_user_turn() {
        // canonical IR 没有 `Role::Tool`；工具结果是 user 轮的 content。
        let msg = Message::new(
            Role::User,
            vec![
                ContentPart::tool_result("call_1", vec![ToolResultPart::from("a")], false),
                ContentPart::tool_result("call_2", vec![ToolResultPart::from("b")], true),
            ],
        );
        assert_eq!(msg.role(), Role::User);
        assert_eq!(msg.content().len(), 2);
    }

    #[test]
    fn tool_input_wire_string() {
        let json = ToolInput::from(serde_json::json!({"path": "a.rs"}));
        assert_eq!(json.to_wire_string(), "{\"path\":\"a.rs\"}");
        assert_eq!(json.as_value().unwrap()["path"], "a.rs");

        let raw = ToolInput::from("{not json");
        assert_eq!(raw.to_wire_string(), "{not json");
        assert!(raw.as_value().is_none());
    }
}
