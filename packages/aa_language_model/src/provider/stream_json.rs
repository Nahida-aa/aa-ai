//! 流式工具参数的**修复式解析**。
//!
//! LLM 的工具参数是逐 token 生成的：任何一次网络分片都可能把 JSON 切在
//! 字符串中间、括号未闭合、甚至反斜杠转义写到一半。provider 若只在流结束时
//! 解析，工具就只能等完整参数；若拿半截原文去 `serde_json`，必然失败。
//!
//! [`fix_streamed_json`] 把半截 JSON 补成**可解析**的完整 JSON（补 `}`/`]`/`"`，
//! 丢掉悬空 `\`），[`parse_streamed_json`] 再解析它。这样 provider 能在参数
//! 还没生成完时发出可用的快照（`ToolUse { is_complete: false }`），支持流式的
//! 工具据此提前开工。
//!
//! 语义与 Zed `language_model_core::util::fix_streamed_json` 一致：
//!
//! - 修补是**尽力而为**，不保证语义正确 —— 快照的值可能被截断，只用于预览 /
//!   提前准备；最终结果永远以完整事件为准。
//! - 修补后的串仍是无效 JSON（例如 `{"a":`）时，[`parse_streamed_json`] 返回
//!   `None`，调用方应继续等下一段增量，而不是报错。
//!
//! 这里只做**结构**修补，不做 schema 校验，也不认识任何厂商字段。

/// 把半截（流式生成中的）JSON 文本补成可解析的完整 JSON。
///
/// 追踪字符串状态与括号栈，只补**结构**上缺的东西：
/// - 未闭合的字符串补 `"`；末尾悬空的 `\` 直接丢掉（它后面没有字符可转义）；
/// - 未闭合的 `{` / `[` 按打开顺序反向补 `}` / `]`。
///
/// 字符串内的括号不算结构，因此内容里的 `{` / `]` 不会破坏栈。
pub fn fix_streamed_json(input: &str) -> String {
    let mut output = input.to_owned();
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for character in input.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else {
            match character {
                '"' => in_string = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    // 流式快照里可能出现多余的闭合（模型重发 / 网关拼接）。
                    // 栈空时忽略，避免修补出一个更坏的串。
                    stack.pop();
                }
                _ => {}
            }
        }
    }

    if escaped {
        // 末尾的 `\` 后面还没有字符，无法补成合法转义。
        output.pop();
    }
    if in_string {
        output.push('"');
    }
    for closing in stack.into_iter().rev() {
        output.push(closing);
    }
    output
}

/// 修补后解析。半截 JSON 里仍有结构性错误（如 `{"a":`）时返回 `None`。
///
/// 返回 `None` 不是错误：调用方（provider 流解码）应继续等下一段增量。
pub fn parse_streamed_json(input: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&fix_streamed_json(input)).ok()
}

#[cfg(test)]
mod tests {
    use super::{fix_streamed_json, parse_streamed_json};

    #[test]
    fn closed_prefix_is_left_untouched() {
        assert_eq!(
            fix_streamed_json(r#"{"path":"a.rs"}"#),
            r#"{"path":"a.rs"}"#
        );
    }

    #[test]
    fn truncated_string_is_closed() {
        assert_eq!(
            fix_streamed_json(r#"{"path":"src/ma"#),
            r#"{"path":"src/ma"}"#
        );
        assert_eq!(
            parse_streamed_json(r#"{"path":"src/ma"#),
            Some(serde_json::json!({ "path": "src/ma" }))
        );
    }

    #[test]
    fn nested_containers_are_closed_in_reverse_order() {
        assert_eq!(
            fix_streamed_json(r#"{"a":{"b":[1,2"#),
            r#"{"a":{"b":[1,2]}}"#
        );
    }

    #[test]
    fn dangling_escape_is_dropped() {
        // 反斜杠后面还没有字符，补成 `\"` 会改变语义，所以直接丢掉。
        assert_eq!(fix_streamed_json(r#"{"a":"b\"#), r#"{"a":"b"}"#);
        assert_eq!(
            parse_streamed_json(r#"{"a":"b\"#),
            Some(serde_json::json!({ "a": "b" }))
        );
    }

    #[test]
    fn escaped_quote_does_not_end_the_string() {
        assert_eq!(fix_streamed_json(r#"{"a":"x\"y"#), r#"{"a":"x\"y"}"#);
        assert_eq!(
            parse_streamed_json(r#"{"a":"x\"y"#),
            Some(serde_json::json!({ "a": "x\"y" }))
        );
    }

    #[test]
    fn brackets_inside_strings_are_not_structure() {
        assert_eq!(fix_streamed_json(r#"{"a":"{[("#), r#"{"a":"{[("}"#);
    }

    #[test]
    fn structurally_incomplete_json_is_not_parseable() {
        assert_eq!(parse_streamed_json(r#"{"a":"#), None);
        assert_eq!(parse_streamed_json(""), None);
    }

    #[test]
    fn plain_text_stays_unparseable() {
        // 模型偶尔给非 JSON 参数：修补不会把它变成合法 JSON。
        assert_eq!(parse_streamed_json("not json"), None);
    }
}
