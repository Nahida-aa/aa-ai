//! 测量不同 IR 形状的栈上开销（size_of）。
//!
//! 运行：cargo run --release -q
//!
//! 注意：只测栈上信封大小，不测堆上 payload 和分配次数。
//! 后者见 docs/decisions/0002-memory-first.md 的「待做的验证」。

use std::mem::size_of;
use std::sync::Arc;

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum Role {
    System,
    User,
    Assistant,
    Tool,
}

// ---------- A. OpenAI 形状（owned） ----------

#[allow(dead_code)]
struct OaCall {
    id: String,
    name: String,
    arguments: String,
}

#[allow(dead_code)]
struct OaMessageOwned {
    role: Role,
    content: Option<String>,
    tool_calls: Vec<OaCall>,
    tool_call_id: Option<String>,
    name: Option<String>,
    reasoning_content: Option<String>,
}

// ---------- B. 语义 IR（Arc + enum） ----------

#[allow(dead_code)]
enum ToolInput {
    Json(serde_json::Value),
    Text(Arc<str>),
}

#[allow(dead_code)]
enum ContentPart {
    Text(Arc<str>),
    Reasoning {
        text: Arc<str>,
        signature: Option<Arc<str>>,
        native: Option<Arc<str>>,
    },
    Image {
        data: Arc<[u8]>,
        mime: &'static str,
    },
    ToolCall {
        id: Arc<str>,
        name: Arc<str>,
        input: ToolInput,
    },
    ToolResult {
        call_id: Arc<str>,
        parts: Arc<[u8]>,
        is_error: bool,
    },
    Opaque(Arc<str>),
}

#[allow(dead_code)]
struct SemanticMessage {
    role: Role,
    parts: Arc<[ContentPart]>,
}

// ---------- C. OpenAI 形状但借用（瞬态序列化适配器） ----------

#[allow(dead_code)]
struct OaCallRef<'a> {
    id: &'a str,
    name: &'a str,
    arguments: &'a str,
}

#[allow(dead_code)]
struct OaMessageRef<'a> {
    role: Role,
    content: Option<&'a str>,
    tool_calls: &'a [OaCallRef<'a>],
    tool_call_id: Option<&'a str>,
    name: Option<&'a str>,
    reasoning_content: Option<&'a str>,
}

fn main() {
    println!("size_of OaCall          = {}", size_of::<OaCall>());
    println!(
        "size_of OaMessageOwned  = {}",
        size_of::<OaMessageOwned>()
    );
    println!("size_of ContentPart     = {}", size_of::<ContentPart>());
    println!(
        "size_of SemanticMessage = {}",
        size_of::<SemanticMessage>()
    );
    println!("size_of OaMessageRef    = {}", size_of::<OaMessageRef>());
    println!("size_of OaCallRef       = {}", size_of::<OaCallRef>());

    println!();
    println!(
        "1000 msgs: OpenAI owned = {} bytes",
        1000 * size_of::<OaMessageOwned>()
    );
    println!(
        "1000 msgs: semantic Arc = {} bytes",
        1000 * size_of::<SemanticMessage>()
    );
}
