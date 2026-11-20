//! The pool reuses one transport per key and sweeps idle entries.

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable_mcp::{
    McpConnectionPool, McpExecutor, McpTransport, Request, Response, ToolCatalog, TransportError,
};
use async_trait::async_trait;
use serde_json::json;

struct NullTransport;

#[async_trait]
impl McpTransport for NullTransport {
    async fn call(&self, _request: Request) -> Result<Response, TransportError> {
        Err(TransportError::Transport(
            "not used by this test".to_string(),
        ))
    }
}

fn catalog() -> ToolCatalog {
    ToolCatalog::from_tools_list(&json!({ "tools": [] })).expect("empty catalog")
}

#[test]
fn the_same_key_returns_the_same_connection() {
    let pool = McpConnectionPool::with_defaults();
    pool.register("server-a", Arc::new(NullTransport))
        .expect("register");
    let first = pool.get("server-a").expect("first get");
    let second = pool.get("server-a").expect("second get");
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(pool.len(), 1);
}

#[test]
fn a_full_pool_refuses_a_new_key() {
    let pool = McpConnectionPool::new(1, Duration::from_secs(60));
    pool.register("server-a", Arc::new(NullTransport))
        .expect("register");
    let error = pool
        .register("server-b", Arc::new(NullTransport))
        .expect_err("a full pool must refuse");
    assert!(matches!(error, TransportError::Transport(_)));
    // Replacing an existing key is still allowed.
    pool.register("server-a", Arc::new(NullTransport))
        .expect("replace");
    assert_eq!(pool.len(), 1);
}

#[test]
fn an_idle_connection_is_swept() {
    let pool = McpConnectionPool::new(2, Duration::ZERO);
    pool.register("server-a", Arc::new(NullTransport))
        .expect("register");
    assert_eq!(pool.sweep(), 1);
    assert!(pool.is_empty());
    assert!(pool.get("server-a").is_err());
}

#[test]
fn an_unknown_key_is_an_error_not_an_empty_connection() {
    let pool = McpConnectionPool::with_defaults();
    assert!(pool.get("missing").is_err());
}

#[test]
fn an_executor_can_be_built_from_the_pool() {
    let pool = Arc::new(McpConnectionPool::with_defaults());
    pool.register("server-a", Arc::new(NullTransport))
        .expect("register");
    let executor = McpExecutor::with_pool(Arc::clone(&pool), "server-a", catalog())
        .expect("executor from pool");
    let pooled = pool.get("server-a").expect("pooled transport");
    assert!(Arc::ptr_eq(executor.transport(), &pooled));
}
