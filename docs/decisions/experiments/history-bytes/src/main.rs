//! 用计数分配器测量「整条会话历史」的真实常驻字节与分配次数。
//!
//! 运行：cargo run --release -q
//!
//! 记账口径：
//! - `req`：请求的字节数（不含分配器元数据）；
//! - `chunk`：按 glibc malloc 估算的块大小 `align_up(max(size,1)+8, 16)`，最小 32B。
//!
//! 工作负载：
//! - 均匀：受控基线。
//! - 真实：按 part 数分布建模（assistant 至少 2 个 part：思考 + 文本/工具调用；
//!   工具结果按批合并成一条多 part 消息）。这是本实验的关键假设，改动会影响结论。
//!
//! 布局候选：
//! - A / A2：OpenAI 形状（朴素 / 冷字段装箱）
//! - B2：`role + Arc<[ContentPart]>`（总是独立分配）
//! - B3：`role + Parts::One`（单 part 内联）
//! - B4：`role + Parts2`（≤2 part 内联）

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

// ---------- 计数分配器 ----------

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static CHUNK: AtomicIsize = AtomicIsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

fn chunk_of(size: usize) -> isize {
    (size.max(1) + 8).div_ceil(16).max(2) as isize * 16
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
            CHUNK.fetch_add(chunk_of(layout.size()), Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        CHUNK.fetch_sub(chunk_of(layout.size()), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
            CHUNK.fetch_add(
                chunk_of(new_size) - chunk_of(layout.size()),
                Ordering::Relaxed,
            );
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Copy, Default)]
struct Snapshot {
    live: isize,
    chunk: isize,
    allocs: usize,
}

fn snap() -> Snapshot {
    Snapshot {
        live: LIVE.load(Ordering::Relaxed),
        chunk: CHUNK.load(Ordering::Relaxed),
        allocs: ALLOCS.load(Ordering::Relaxed),
    }
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Snapshot) {
    let before = snap();
    let value = f();
    let after = snap();
    (
        value,
        Snapshot {
            live: after.live - before.live,
            chunk: after.chunk - before.chunk,
            allocs: after.allocs - before.allocs,
        },
    )
}

// ---------- 共享类型 ----------

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Tool,
}

#[allow(dead_code)]
enum ContentPart {
    Text(Arc<str>),
    Reasoning {
        text: Arc<str>,
        signature: Option<Arc<str>>,
    },
    ToolCall {
        id: Arc<str>,
        name: Arc<str>,
        arguments: Arc<str>,
    },
    ToolResult {
        call_id: Arc<str>,
        text: Arc<str>,
        is_error: bool,
    },
}

// ---------- 布局：B2 ----------

#[allow(dead_code)]
struct Message {
    role: Role,
    parts: Arc<[ContentPart]>,
}

// ---------- 布局：Z（Zed 的请求 IR：语义 enum + owned Vec/String） ----------
//
// 源码：language_model_core/src/request.rs:355
//   LanguageModelRequestMessage { role, content: Vec<MessageContent>, cache, reasoning_details }
//   MessageContent::Text(String) / Thinking { text: String, ... }  ← owned，不借用

#[allow(dead_code)]
#[derive(Clone)]
enum ContentPartOwned {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        text: String,
        is_error: bool,
    },
}

#[allow(dead_code)]
#[derive(Clone)]
struct ZedReqMsg {
    role: Role,
    content: Vec<ContentPartOwned>,
    cache: bool,
    reasoning_details: Option<Box<[u8]>>,
}

// ---------- 布局：B5（Box<[ContentPart]>，无引用计数） ----------

#[allow(dead_code)]
struct MessageBoxed {
    role: Role,
    parts: Box<[ContentPart]>,
}

// ---------- 布局：B3（单 part 内联） ----------

#[allow(dead_code)]
enum Parts {
    One(ContentPart),
    Many(Arc<[ContentPart]>),
}

#[allow(dead_code)]
struct MessageOneInline {
    role: Role,
    parts: Parts,
}

// ---------- 布局：B4（≤2 part 内联） ----------

#[allow(dead_code)]
enum Parts2 {
    One(ContentPart),
    Two(ContentPart, ContentPart),
    Many(Arc<[ContentPart]>),
}

#[allow(dead_code)]
struct MessageTwoInline {
    role: Role,
    parts: Parts2,
}

// ---------- 布局：A / A2（OpenAI 形状） ----------

#[allow(dead_code)]
#[derive(Clone)]
struct OaCall {
    id: String,
    name: String,
    arguments: String,
}

#[allow(dead_code)]
#[derive(Clone)]
struct OaMsgNaive {
    role: Role,
    content: Option<String>,
    tool_calls: Vec<OaCall>,
    tool_call_id: Option<String>,
    name: Option<String>,
    reasoning_content: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone)]
struct OaExtras {
    tool_calls: Vec<OaCall>,
    tool_call_id: Option<String>,
    name: Option<String>,
    reasoning_content: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone)]
struct OaMsgLean {
    role: Role,
    content: Option<String>,
    extras: Option<Box<OaExtras>>,
}

// ---------- 工作负载计划 ----------

#[derive(Clone, Copy)]
enum PartSpec {
    Text(usize),
    Reasoning(usize),
    ToolCall(usize),
    ToolResult(usize),
}

#[derive(Clone)]
struct MsgPlan {
    role: Role,
    parts: Vec<PartSpec>,
    seq: usize,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        debug_assert!(hi > lo);
        lo + (self.next() % (hi - lo) as u64) as usize
    }

}

/// 真实会话的 part 数分布（关键假设，改动会改结论）：
///
/// - 20% 用户：1 part（文本）
/// - 20% 助手：2 parts（思考 + 文本）
/// - 30% 助手：3 parts（思考 + 文本 + 工具调用）
/// - 30% 工具结果：1~3 parts（并行工具调用合并成一条消息）
///
/// 平均 2.1 parts/条；单 part 只占 20%。
fn realistic_plan(n: usize) -> Vec<MsgPlan> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut v = Vec::with_capacity(n);
    for seq in 0..n {
        let roll = rng.next() % 100;
        let (role, parts) = match roll {
            0..=19 => (Role::User, vec![PartSpec::Text(rng.range(20, 400))]),
            20..=39 => (
                Role::Assistant,
                vec![
                    PartSpec::Reasoning(rng.range(50, 600)),
                    PartSpec::Text(rng.range(100, 1_500)),
                ],
            ),
            40..=69 => (
                Role::Assistant,
                vec![
                    PartSpec::Reasoning(rng.range(50, 800)),
                    PartSpec::Text(rng.range(0, 200)),
                    PartSpec::ToolCall(rng.range(20, 80)),
                ],
            ),
            _ => {
                let count = rng.range(1, 4);
                (
                    Role::Tool,
                    (0..count)
                        .map(|_| PartSpec::ToolResult(rng.range(100, 4_000)))
                        .collect(),
                )
            }
        };
        v.push(MsgPlan { role, parts, seq });
    }
    v
}

fn uniform_plan(n: usize, parts: usize, len: usize) -> Vec<MsgPlan> {
    (0..n)
        .map(|seq| MsgPlan {
            role: Role::Assistant,
            parts: std::iter::repeat_n(PartSpec::Text(len), parts).collect(),
            seq,
        })
        .collect()
}

fn payload(len: usize) -> String {
    "x".repeat(len)
}

fn plan_payload(plan: &[MsgPlan]) -> usize {
    plan.iter()
        .flat_map(|m| m.parts.iter())
        .map(|p| match *p {
            PartSpec::Text(n)
            | PartSpec::Reasoning(n)
            | PartSpec::ToolCall(n)
            | PartSpec::ToolResult(n) => n,
        })
        .sum()
}

fn part_histogram(plan: &[MsgPlan]) -> Vec<(usize, usize)> {
    let mut hist = std::collections::BTreeMap::new();
    for m in plan {
        *hist.entry(m.parts.len()).or_insert(0usize) += 1;
    }
    hist.into_iter().collect()
}

// ---------- 构造：语义 IR ----------

fn build_parts(msg: &MsgPlan) -> Vec<ContentPart> {
    msg.parts
        .iter()
        .map(|spec| match *spec {
            PartSpec::Text(n) => ContentPart::Text(Arc::from(payload(n))),
            PartSpec::Reasoning(n) => ContentPart::Reasoning {
                text: Arc::from(payload(n)),
                signature: Some(Arc::from("sig")),
            },
            PartSpec::ToolCall(n) => ContentPart::ToolCall {
                id: Arc::from(format!("call_{}", msg.seq)),
                name: Arc::from("read_file"),
                arguments: Arc::from(payload(n)),
            },
            PartSpec::ToolResult(n) => ContentPart::ToolResult {
                call_id: Arc::from(format!("call_{}", msg.seq)),
                text: Arc::from(payload(n)),
                is_error: false,
            },
        })
        .collect()
}

fn semantic_from(plan: &[MsgPlan]) -> Vec<Arc<Message>> {
    plan.iter()
        .map(|msg| {
            let parts: Arc<[ContentPart]> = build_parts(msg).into();
            Arc::new(Message {
                role: msg.role,
                parts,
            })
        })
        .collect()
}

fn zed_from(plan: &[MsgPlan]) -> Vec<ZedReqMsg> {
    plan.iter()
        .map(|msg| ZedReqMsg {
            role: msg.role,
            content: msg
                .parts
                .iter()
                .map(|spec| match *spec {
                    PartSpec::Text(n) => ContentPartOwned::Text(payload(n)),
                    PartSpec::Reasoning(n) => ContentPartOwned::Reasoning {
                        text: payload(n),
                        signature: Some("sig".to_owned()),
                    },
                    PartSpec::ToolCall(n) => ContentPartOwned::ToolCall {
                        id: format!("call_{}", msg.seq),
                        name: "read_file".to_owned(),
                        arguments: payload(n),
                    },
                    PartSpec::ToolResult(n) => ContentPartOwned::ToolResult {
                        call_id: format!("call_{}", msg.seq),
                        text: payload(n),
                        is_error: false,
                    },
                })
                .collect(),
            cache: false,
            reasoning_details: None,
        })
        .collect()
}

fn boxed_from(plan: &[MsgPlan]) -> Vec<Arc<MessageBoxed>> {
    plan.iter()
        .map(|msg| {
            Arc::new(MessageBoxed {
                role: msg.role,
                parts: build_parts(msg).into_boxed_slice(),
            })
        })
        .collect()
}

fn one_inline_from(plan: &[MsgPlan]) -> Vec<Arc<MessageOneInline>> {
    plan.iter()
        .map(|msg| {
            let parts = build_parts(msg);
            let parts = if parts.len() == 1 {
                Parts::One(parts.into_iter().next().unwrap())
            } else {
                Parts::Many(parts.into())
            };
            Arc::new(MessageOneInline {
                role: msg.role,
                parts,
            })
        })
        .collect()
}

fn two_inline_from(plan: &[MsgPlan]) -> Vec<Arc<MessageTwoInline>> {
    plan.iter()
        .map(|msg| {
            let mut parts = build_parts(msg);
            let parts = if parts.len() == 1 {
                Parts2::One(parts.remove(0))
            } else if parts.len() == 2 {
                Parts2::Two(parts.remove(0), parts.remove(0))
            } else {
                Parts2::Many(parts.into())
            };
            Arc::new(MessageTwoInline {
                role: msg.role,
                parts,
            })
        })
        .collect()
}

// ---------- 构造：OpenAI 形状 ----------

fn oa_fields(msg: &MsgPlan) -> (Option<String>, Option<String>, Vec<OaCall>, Option<String>) {
    let content_len: usize = msg
        .parts
        .iter()
        .map(|spec| match *spec {
            PartSpec::Text(n) | PartSpec::ToolResult(n) => n,
            _ => 0,
        })
        .sum();
    let mut content = (content_len > 0).then(|| String::with_capacity(content_len));
    let mut reasoning: Option<String> = None;
    let mut calls: Vec<OaCall> = Vec::new();
    let mut tool_call_id: Option<String> = None;

    for spec in &msg.parts {
        match *spec {
            PartSpec::Text(n) | PartSpec::ToolResult(n) => {
                if let Some(buf) = content.as_mut() {
                    buf.extend(std::iter::repeat_n('x', n));
                }
                if matches!(spec, PartSpec::ToolResult(_)) && tool_call_id.is_none() {
                    tool_call_id = Some(format!("call_{}", msg.seq));
                }
            }
            PartSpec::Reasoning(n) => {
                if reasoning.is_none() {
                    reasoning = Some("x".repeat(n));
                }
            }
            PartSpec::ToolCall(n) => calls.push(OaCall {
                id: format!("call_{}", msg.seq),
                name: "read_file".to_owned(),
                arguments: "x".repeat(n),
            }),
        }
    }
    (content, reasoning, calls, tool_call_id)
}

fn naive_from(plan: &[MsgPlan]) -> Vec<OaMsgNaive> {
    plan.iter()
        .map(|msg| {
            let (content, reasoning_content, tool_calls, tool_call_id) = oa_fields(msg);
            OaMsgNaive {
                role: msg.role,
                content,
                tool_calls,
                tool_call_id,
                name: None,
                reasoning_content,
            }
        })
        .collect()
}

fn lean_from(plan: &[MsgPlan]) -> Vec<OaMsgLean> {
    plan.iter()
        .map(|msg| {
            let (content, reasoning_content, tool_calls, tool_call_id) = oa_fields(msg);
            let cold = tool_call_id.is_some() || !tool_calls.is_empty() || reasoning_content.is_some();
            OaMsgLean {
                role: msg.role,
                content,
                extras: cold.then(|| {
                    Box::new(OaExtras {
                        tool_calls,
                        tool_call_id,
                        name: None,
                        reasoning_content,
                    })
                }),
            }
        })
        .collect()
}

// ---------- 报告 ----------

fn row(label: &str, n: usize, s: Snapshot, payload_bytes: usize) {
    let over = if payload_bytes == 0 {
        0.0
    } else {
        (s.live as f64 / payload_bytes as f64 - 1.0) * 100.0
    };
    println!(
        "{label:<30} req {:>8.1} B/msg | chunk {:>8.1} B/msg | {:>5.2} allocs/msg | 记账开销 {:>6.1}%",
        s.live as f64 / n as f64,
        s.chunk as f64 / n as f64,
        s.allocs as f64 / n as f64,
        over
    );
}

fn run(plan: &[MsgPlan], title: &str) {
    let n = plan.len();
    let pbytes = plan_payload(plan);
    println!("{title}");
    println!(
        "   payload 合计 {} KiB，平均 {:.0} B/条，part 分布 {:?}",
        pbytes / 1024,
        pbytes as f64 / n as f64,
        part_histogram(plan)
    );

    let (v, s) = measure(|| naive_from(plan));
    row("A  朴素 OpenAI", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| lean_from(plan));
    row("A2 精简 OpenAI", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| semantic_from(plan));
    row("B2 Arc<[ContentPart]>", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| zed_from(plan));
    row("Z  Zed 请求 IR (owned)", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| boxed_from(plan));
    row("B5 Box<[ContentPart]>", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| one_inline_from(plan));
    row("B3 单 part 内联", n, s, pbytes);
    black_box(&v);
    drop(v);

    let (v, s) = measure(|| two_inline_from(plan));
    row("B4 ≤2 part 内联", n, s, pbytes);
    black_box(&v);
    drop(v);
    println!();
}

fn main() {
    println!("== size_of（仅参考，不含堆） ==");
    println!("OaMsgNaive       = {}", size_of::<OaMsgNaive>());
    println!("OaMsgLean        = {}", size_of::<OaMsgLean>());
    println!("Message (B2)     = {}", size_of::<Message>());
    println!("MessageBoxed (B5)= {}", size_of::<MessageBoxed>());
    println!("ZedReqMsg        = {}", size_of::<ZedReqMsg>());
    println!("MessageOne (B3)  = {}", size_of::<MessageOneInline>());
    println!("MessageTwo (B4)  = {}", size_of::<MessageTwoInline>());
    println!("ContentPart      = {}", size_of::<ContentPart>());
    println!("Arc<Message>     = {}", size_of::<Arc<Message>>());
    println!();

    const N: usize = 1000;

    run(&uniform_plan(N, 1, 64), "-- 均匀：1 part，payload = 64 B --");
    run(&uniform_plan(N, 2, 512), "-- 均匀：2 parts，payload = 512 B --");

    let realistic = realistic_plan(N);
    run(&realistic, "-- 真实分布：assistant ≥2 parts，工具结果按批合并 --");

    // 敏感性：把工具结果改成「每个结果一条消息」（单 part 占比上升）
    let mut per_result = Vec::new();
    for msg in &realistic {
        if msg.role == Role::Tool && msg.parts.len() > 1 {
            for (i, spec) in msg.parts.iter().enumerate() {
                per_result.push(MsgPlan {
                    role: Role::Tool,
                    parts: vec![*spec],
                    seq: msg.seq * 10 + i,
                });
            }
        } else {
            per_result.push(msg.clone());
        }
    }
    run(&per_result, "-- 敏感性：工具结果拆成单条消息（单 part 占比上升） --");

    // ---- 克隆 ----
    println!("== 构造请求时克隆整条历史（真实分布） ==");
    let a_hist = lean_from(&realistic);
    let (c, s) = measure(|| a_hist.clone());
    row("A2 clone（深拷贝）", N, s, 0);
    black_box(&c);
    drop(c);

    let z_hist = zed_from(&realistic);
    let (c, s) = measure(|| z_hist.clone());
    row("Z  Zed 请求重建（深拷贝）", N, s, 0);
    black_box(&c);
    drop(c);
    black_box(&z_hist);
    drop(z_hist);

    let b_hist = semantic_from(&realistic);
    let (c, s) = measure(|| b_hist.clone());
    row("B2 clone Vec<Arc<Message>>", N, s, 0);
    black_box(&c);
    drop(c);

    black_box(&a_hist);
    black_box(&b_hist);
}
