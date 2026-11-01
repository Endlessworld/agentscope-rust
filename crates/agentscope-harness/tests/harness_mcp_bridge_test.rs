//! End-to-end test for bridging MCP server tools into a core Toolkit.
//!
//! Spawns a self-contained Node stdio MCP server (see `fixtures/echo_mcp_server.js`),
//! connects through `McpClientManager`, bridges its tool into a `Toolkit`, then
//! invokes the tool through the core `McpTool` to verify the full path:
//! discover -> bridge -> call.

use std::sync::Arc;

use agentscope_core::message::TextBlock;
use agentscope_core::tool::{Tool, Toolkit};
use agentscope_harness::agent::{bridge_mcp_tools_to_toolkit, McpClientManager, McpServerConfig};

fn fixture_server_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.js")
}

/// Returns `true` when `node` is available on PATH (the test needs it).
fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn bridge_mcp_server_tool_and_call() {
    if !node_available() {
        eprintln!("skipping: node not available on PATH");
        return;
    }
    let script = fixture_server_path();
    if !script.exists() {
        panic!("fixture MCP server not found: {}", script.display());
    }

    let manager = Arc::new(McpClientManager::new());
    let config = McpServerConfig::new("echo-server").command(format!("node {}", script.display()));

    // Connect and discover tools.
    let client_id = manager.create_client(config).await.expect("create client");
    manager.connect(&client_id).await.expect("connect");

    // Bridge discovered tools into a fresh toolkit.
    let toolkit = Toolkit::new();
    let n = bridge_mcp_tools_to_toolkit(&manager, &toolkit).await;
    assert_eq!(n, 1, "expected exactly one MCP tool to be bridged");

    let tool = toolkit.get_tool("echo_message").expect("tool registered");

    // Invoke the tool with a message argument.
    let args = serde_json::json!({ "message": "hello from agentscope" });
    let result = tool
        .call(args.as_object().unwrap().clone(), Default::default())
        .await
        .expect("tool call");

    // The MCP server echoes back: "echo: hello from agentscope".
    let text = result
        .output
        .iter()
        .filter_map(|b| match b {
            agentscope_core::message::ContentBlock::Text(TextBlock { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("echo: hello from agentscope"),
        "unexpected tool output: {text}"
    );
}

#[tokio::test]
async fn prefix_tool_names_registers_server_prefixed_tool() {
    if !node_available() {
        eprintln!("skipping: node not available on PATH");
        return;
    }
    let script = fixture_server_path();
    if !script.exists() {
        panic!("fixture MCP server not found: {}", script.display());
    }

    let manager = Arc::new(McpClientManager::new());
    let config = McpServerConfig::new("echo-server")
        .command(format!("node {}", script.display()))
        .prefix_tool_names(true);
    let client_id = manager.create_client(config).await.expect("create client");
    manager.connect(&client_id).await.expect("connect");

    let toolkit = Toolkit::new();
    let n = bridge_mcp_tools_to_toolkit(&manager, &toolkit).await;
    assert_eq!(n, 1);
    assert!(
        toolkit.get_tool("echo_message").is_none(),
        "the bare remote name must not be registered when the prefix is on"
    );

    let tool = toolkit
        .get_tool("echo-server__echo_message")
        .expect("prefixed tool registered");
    let args = serde_json::json!({ "message": "prefixed" });
    let result = tool
        .call(args.as_object().unwrap().clone(), Default::default())
        .await
        .expect("tool call");
    let text = result
        .output
        .iter()
        .filter_map(|b| match b {
            agentscope_core::message::ContentBlock::Text(TextBlock { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("echo: prefixed"), "unexpected output: {text}");
}

#[tokio::test]
async fn bridge_with_no_connected_clients_returns_zero() {
    let manager = Arc::new(McpClientManager::new());
    let toolkit = Toolkit::new();
    let n = bridge_mcp_tools_to_toolkit(&manager, &toolkit).await;
    assert_eq!(n, 0);
}
