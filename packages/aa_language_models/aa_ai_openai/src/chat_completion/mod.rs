//! OpenAI **chat completions** 协议（`POST /chat/completions`）的 wire 形状。
//!
//! 一个协议一个目录，请求与响应成对出现：
//! - [`request`]：请求体 DTO（借用 canonical IR）+ 编码器 `encode`；
//! - [`response`]：响应 body / SSE chunk / 错误 body + `StreamDecoder`。
//!
//! 两边**不共用结构体**：请求侧只 `Serialize`、响应侧只 `Deserialize`，改一边不会
//! 连带另一边（`arguments` 发出时是 `&serde_json::Value`，回来时是 JSON 字符串，
//! 形状本来就不同）。
//!
//! 加另一套协议（Responses API、Anthropic messages）时就并列新开目录，
//! 不复用这里的类型——这正是「一家一个 crate、一套协议一个模块」的边界所在。

pub mod request;
pub mod response;
