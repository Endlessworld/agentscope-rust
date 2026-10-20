//! GAP-07: MCP `_meta` propagation switches, mirroring Java `McpTool` /
//! `McpClientWrapper` / `Toolkit.ToolRegistration#propagateMeta`.
//!
//! The end-to-end cases spawn the Node fixture server, which echoes `params._meta`
//! back so the test can assert on what actually crossed the wire.

use std::sync::Arc;

use agentscope_core::message::{TextBlock, ToolUseBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_harness::agent::{
    try_bridge_mcp_tools_to_toolkit, McpClientManager, McpMetaPropagationOptions, McpServerConfig,
};

fn fixture_server_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.js")
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn text_of(result: &agentscope_core::message::ToolResultBlock) -> String {
    result
        .output
        .iter()
        .filter_map(|b| match b {
            agentscope_core::message::ContentBlock::Text(TextBlock { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn connect_echo() -> (Arc<McpClientManager>, String) {
    let script = fixture_server_path();
    assert!(
        script.exists(),
        "fixture MCP server not found: {}",
        script.display()
    );
    let manager = Arc::new(McpClientManager::new());
    let config = McpServerConfig::new("echo-server").command(format!("node {}", script.display()));
    let client_id = manager.create_client(config).await.expect("create client");
    manager.connect(&client_id).await.expect("connect");
    (manager, client_id)
}

#[test]
fn connection_switch_defaults_to_enabled() {
    assert!(McpServerConfig::new("srv").propagate_meta);
    assert!(
        !McpServerConfig::new("srv")
            .propagate_meta(false)
            .propagate_meta
    );
}

#[test]
fn blank_tool_override_name_is_rejected() {
    // Mirrors Java `ToolRegistration.propagateMeta(String, boolean)`.
    let err = McpMetaPropagationOptions::new()
        .propagate_meta_for("   ", false)
        .unwrap_err();
    assert!(err.to_string().contains("cannot be null or blank"), "{err}");
}

#[tokio::test]
async fn set_propagate_meta_updates_an_existing_connection() {
    let manager = Arc::new(McpClientManager::new());
    let id = manager
        .create_client(McpServerConfig::new("offline"))
        .await
        .unwrap();
    assert_eq!(manager.propagate_meta(&id).await, Some(true));
    assert!(manager.set_propagate_meta(&id, false).await);
    assert_eq!(manager.propagate_meta(&id).await, Some(false));
    assert!(!manager.set_propagate_meta("missing", false).await);
}

#[tokio::test]
async fn meta_is_propagated_by_default_and_killed_by_the_connection_switch() {
    if !node_available() {
        eprintln!("skipping: node not available on PATH");
        return;
    }
    let (manager, client_id) = connect_echo().await;
    let toolkit = Toolkit::new();
    let n = try_bridge_mcp_tools_to_toolkit(&manager, &toolkit, &McpMetaPropagationOptions::new())
        .await
        .expect("bridge");
    assert_eq!(n, 1);

    let tool = toolkit.get_tool("echo_message").expect("tool registered");
    let ctx = ToolExecutionContext {
        tool_call: Some(ToolUseBlock::new(
            "call-abc",
            "echo_message",
            Default::default(),
        )),
        ..Default::default()
    };
    let args = serde_json::json!({"message": "hi"})
        .as_object()
        .unwrap()
        .clone();

    let on = text_of(&tool.call(args.clone(), ctx.clone()).await.unwrap());
    assert!(
        on.contains("io.agentscope/toolCallId") && on.contains("call-abc"),
        "toolCallId should reach the server, got: {on}"
    );

    // The connection-level switch is read live, so disabling it must silence an
    // already-bridged tool without rebuilding anything.
    assert!(manager.set_propagate_meta(&client_id, false).await);
    let off = text_of(&tool.call(args, ctx).await.unwrap());
    assert!(
        off.contains("echo: hi"),
        "call itself must still work: {off}"
    );
    assert!(
        !off.contains("io.agentscope/toolCallId"),
        "propagation off -> no _meta on the wire, got: {off}"
    );
}

#[tokio::test]
async fn per_tool_override_silences_only_that_tool() {
    if !node_available() {
        eprintln!("skipping: node not available on PATH");
        return;
    }
    let (manager, _client_id) = connect_echo().await;
    let toolkit = Toolkit::new();
    let options = McpMetaPropagationOptions::new()
        .propagate_meta_for("echo_message", false)
        .expect("non-blank name");
    try_bridge_mcp_tools_to_toolkit(&manager, &toolkit, &options)
        .await
        .expect("bridge");

    let tool = toolkit.get_tool("echo_message").expect("tool registered");
    let ctx = ToolExecutionContext {
        tool_call: Some(ToolUseBlock::new(
            "call-xyz",
            "echo_message",
            Default::default(),
        )),
        ..Default::default()
    };
    let args = serde_json::json!({"message": "hi"})
        .as_object()
        .unwrap()
        .clone();
    let out = text_of(&tool.call(args, ctx).await.unwrap());
    assert!(
        !out.contains("io.agentscope/toolCallId"),
        "per-tool override must win over the default, got: {out}"
    );
}

#[tokio::test]
async fn unknown_override_name_fails_the_bridge() {
    if !node_available() {
        eprintln!("skipping: node not available on PATH");
        return;
    }
    let (manager, _client_id) = connect_echo().await;
    let toolkit = Toolkit::new();
    let options = McpMetaPropagationOptions::new()
        .propagate_meta_for("echo_mesage", false) // typo on purpose
        .unwrap()
        .propagate_meta_for("also_missing", false)
        .unwrap();

    let err = try_bridge_mcp_tools_to_toolkit(&manager, &toolkit, &options)
        .await
        .expect_err("a typo must fail the registration loudly");
    let message = err.to_string();
    assert!(
        message.contains("also_missing, echo_mesage"),
        "sorted names expected: {message}"
    );
}
