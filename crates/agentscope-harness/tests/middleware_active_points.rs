//! Harness middlewares declare the points their hooks implement.
//!
//! Mirrors the harness half of Java `6b9d5840f`: an implemented hook whose point is not
//! declared is never invoked, so `CompactionMiddleware` (system prompt only) and
//! `ContextConventionsMiddleware` (`WorkspaceContextMiddleware` here) must declare a narrow
//! set instead of relying on the trait default.

use std::sync::Arc;

use agentscope_core::middleware::{ActivePointsResolver, ExtensionPoint, Middleware};
use agentscope_harness::agent::middleware::{
    CompactionMiddleware, CompactionMiddlewareConfig, WorkspaceContextMiddleware,
};
use agentscope_harness::agent::WorkspaceManager;

#[test]
fn the_compaction_middleware_declares_only_the_system_prompt_point() {
    let middleware = CompactionMiddleware::new(CompactionMiddlewareConfig {
        enabled: true,
        max_tokens: 1000,
    });

    assert_eq!(
        middleware.active_points(),
        vec![ExtensionPoint::OnSystemPrompt]
    );
    assert_eq!(
        ActivePointsResolver::resolve(&middleware),
        vec![ExtensionPoint::OnSystemPrompt]
    );
}

#[test]
fn the_context_conventions_middleware_declares_only_the_system_prompt_point() {
    let workspace = std::env::temp_dir().join(format!("active-points-{}", uuid::Uuid::new_v4()));
    let middleware = WorkspaceContextMiddleware::new(Arc::new(WorkspaceManager::with_fs(
        workspace.clone(),
        None,
    )));

    assert_eq!(
        middleware.active_points(),
        vec![ExtensionPoint::OnSystemPrompt]
    );
    assert_eq!(
        ActivePointsResolver::resolve(&middleware),
        vec![ExtensionPoint::OnSystemPrompt]
    );
    let _ = std::fs::remove_dir_all(workspace);
}
