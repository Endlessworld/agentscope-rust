//! Pins that JSON decoding in this crate rejects trailing content.
//!
//! Java's `JacksonJsonCodec` used to accept the first root value and ignore the rest, so raw
//! tool-call arguments like `{"x":1} garbage` were treated as a valid object (Java #3697).
//! Rust decodes with `serde_json`, whose `from_str` already fails on trailing tokens while
//! accepting trailing whitespace; this test keeps that invariant from silently regressing to a
//! lenient helper.

use agentscope_core::message::ToolUseBlock;

#[test]
fn trailing_content_after_the_root_value_is_rejected() {
    for json in [
        r#"{"x":1} garbage"#,
        r#"{"x":1} {}"#,
        r#"{"x":1} null"#,
        r#"{"x":1} 42"#,
    ] {
        assert!(
            serde_json::from_str::<serde_json::Value>(json).is_err(),
            "{json}"
        );
        assert!(
            serde_json::from_str::<ToolUseBlock>(json).is_err(),
            "{json}"
        );
    }
}

#[test]
fn trailing_json_whitespace_is_accepted() {
    let json = "{\"x\":1} \r\n\t";
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(json).unwrap(),
        serde_json::json!({"x": 1})
    );
}
