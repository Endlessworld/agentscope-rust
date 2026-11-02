//! `McpToolSource` (harness-level MCP client port, HIGRESS-07).
//!
//! The trait replaces the per-crate MCP-client abstraction so extension crates (Higress, …)
//! can register tools from *any* MCP client. The production adapter is `McpClientSource`,
//! which fronts one server on the harness `McpClientManager`.
use std::sync::Arc;

use agentscope_harness::agent::{
    McpClientManager, McpClientSource, McpServerConfig, McpToolSource,
};

async fn manager_with_server(name: &str, config: McpServerConfig) -> Arc<McpClientManager> {
    let manager = Arc::new(McpClientManager::new());
    manager.create_client(config).await.unwrap();
    assert!(manager.find_any_client_id_by_server(name).await.is_some());
    manager
}

#[tokio::test]
async fn adapter_reports_unconfigured_servers_instead_of_faking_a_connection() {
    let manager = Arc::new(McpClientManager::new());
    let source = McpClientSource::new(Arc::clone(&manager), "missing");

    assert_eq!(source.name(), "missing");
    assert_eq!(source.server(), "missing");

    let err = source
        .initialize()
        .await
        .expect_err("a server nobody registered cannot be initialised");
    assert!(
        format!("{err}").contains("No MCP client configured"),
        "{err}"
    );

    let err = source
        .list_tools()
        .await
        .expect_err("list_tools must not read an unconnected client");
    assert!(format!("{err}").contains("not connected"), "{err}");

    let err = source
        .call_tool("search", serde_json::json!({}), None)
        .await
        .expect_err("call_tool must not read an unconnected client");
    assert!(format!("{err}").contains("not connected"), "{err}");
}

#[tokio::test]
async fn adapter_initialises_through_the_manager() {
    // A registered server with no transport at all: `initialize` must go all the way to
    // the manager's `connect`, whose planner rejects the config. Reaching *that* error is
    // what proves the adapter really drives the managed connection.
    let manager = manager_with_server("files", McpServerConfig::new("files")).await;
    let source = McpClientSource::new(Arc::clone(&manager), "files");

    let err = source
        .initialize()
        .await
        .expect_err("a server without command or url cannot connect");
    let text = format!("{err}");
    assert!(text.contains("No command or URL specified"), "{text}");
    assert!(
        manager.find_client_id_by_server("files").await.is_none(),
        "a failed connect must not leave the server marked connected"
    );
}
