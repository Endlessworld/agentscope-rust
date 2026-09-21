//! Integration tests for the core tool mid/high-level helpers added in the tool
//! migration pass: ToolResultConverter / DefaultToolResultConverter,
//! ToolResultMessageBuilder, ToolSuspendException + suspended semantics,
//! ToolDangerousPathConstants helpers, and the ToolRegistration builder.
use std::sync::Arc;

use agentscope_core::{
    DefaultToolResultConverter, Error, FunctionTool, JsonMap, MsgRole, ToolExecutionContext,
    ToolRegistration, ToolResultBlock, ToolResultConverter, ToolResultMessageBuilder,
    ToolSuspendException, ToolUseBlock, Toolkit, is_dangerous_command, is_dangerous_path,
};

#[test]
fn default_converter_handles_null_string_and_json() {
    let conv = DefaultToolResultConverter;
    // null -> "null"
    let rb = conv.convert(None);
    assert_eq!(rb.text_content(), "null");
    let rb2 = conv.convert(Some(&serde_json::Value::Null));
    assert_eq!(rb2.text_content(), "null");
    // string passes through verbatim
    let rb3 = conv.convert(Some(&serde_json::json!("hello")));
    assert_eq!(rb3.text_content(), "hello");
    // object -> compact JSON
    let rb4 = conv.convert(Some(&serde_json::json!({"a": 1, "b": [true]})));
    assert_eq!(rb4.text_content(), "{\"a\":1,\"b\":[true]}");
}

#[test]
fn tool_result_message_builder_keys_result_to_original_call() {
    let result = ToolResultBlock::text("42");
    let call = ToolUseBlock::new("call-xyz", "add", JsonMap::new());
    let msg = ToolResultMessageBuilder::build_tool_result_msg(&result, &call, "agent-1");
    assert_eq!(msg.role(), MsgRole::Tool);
    assert_eq!(msg.name(), Some("agent-1"));
    let trb = msg.tool_result_blocks().next().expect("one tool result");
    assert_eq!(trb.id.as_deref(), Some("call-xyz"));
    assert_eq!(trb.name.as_deref(), Some("add"));
    assert_eq!(trb.text_content(), "42");
}

#[tokio::test]
async fn suspended_tool_returns_suspended_block() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(FunctionTool::new(
        "external",
        "requires external call",
        serde_json::json!({"type": "object"}),
        Box::new(|_, _| Err(ToolSuspendException::new("needs human").to_error())),
    )));
    let call = ToolUseBlock::new("c1", "external", JsonMap::new());
    let rb = tk
        .call_tool(&call, ToolExecutionContext::default())
        .await
        .expect("suspended is a success result");
    assert!(rb.is_suspended(), "result should be flagged suspended");
    assert_eq!(rb.id.as_deref(), Some("c1"));
    assert_eq!(rb.text_content(), "needs human");
}

#[test]
fn suspend_exception_defaults_reason() {
    let e = ToolSuspendException::new("");
    assert_eq!(e.reason(), "Tool execution suspended");
    assert!(matches!(e.to_error(), Error::ToolSuspended(_)));
}

#[test]
fn dangerous_path_flags_sensitive_files_and_dirs() {
    assert!(is_dangerous_path(".env"));
    assert!(is_dangerous_path("config/.ssh/config"));
    assert!(is_dangerous_path("a/.git/objects"));
    assert!(is_dangerous_path("./.env.production.local"));
    assert!(!is_dangerous_path("src/main.rs"));
    assert!(!is_dangerous_path("docs/readme.md"));
}

#[test]
fn dangerous_command_flags_disruptive_fragments() {
    // DANGEROUS_COMMANDS 已置空（子串 contains 曾误判 `git add`/middleware/--format），
    // 命令安全改由工具侧审批模式保障。空表对所有常规命令放行。
    assert!(!is_dangerous_command("rm -rf /tmp/x"));
    assert!(!is_dangerous_command("sudo rm -rf /"));
    assert!(!is_dangerous_command("dd if=/dev/zero of=/dev/sda"));
    assert!(!is_dangerous_command("ls -la"));
    assert!(!is_dangerous_command("echo hello"));
}

#[test]
fn registration_builder_registers_with_group_and_presets() {
    let tk = Toolkit::new();
    let tool = Arc::new(FunctionTool::new(
        "calc",
        "calculator",
        serde_json::json!({"type": "object"}),
        Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
    ));
    let mut presets = JsonMap::new();
    presets.insert("precision".to_string(), serde_json::json!(2));
    ToolRegistration::new(&tk)
        .tool(tool)
        .group("math")
        .preset_parameters(presets)
        .apply()
        .expect("registration succeeds");

    assert!(tk.tool_names().contains(&"calc".to_string()));
    assert!(tk.get_tool_group("math").is_some());
    assert_eq!(tk.preset_parameters("calc").get("precision").unwrap(), &serde_json::json!(2));
}

#[test]
fn registration_requires_tool() {
    let tk = Toolkit::new();
    let err = ToolRegistration::new(&tk).apply().unwrap_err();
    assert!(err.to_string().contains("requires a tool"));
}
