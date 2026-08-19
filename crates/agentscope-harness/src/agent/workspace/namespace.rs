//! Runtime-context derived namespace factory, mirroring `NamespaceFactory`.
//!
//! Produces a namespace tuple (a list of path segments) from a per-call
//! `RuntimeContext`, used to scope runtime user data (sessions, tasks, memory)
//! under the workspace.

use agentscope_core::agent::RuntimeContext;

/// Produces a namespace tuple for workspace data at call time.
///
/// Unlike a static namespace, a `NamespaceFactory` is invoked on every
/// operation, allowing the namespace to vary based on the per-call
/// `RuntimeContext` (user id, session id) rather than mutable shared state.
pub trait NamespaceFactory: Send + Sync {
    /// Returns the namespace tuple for the current operation context.
    fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String>;
}

/// A `NamespaceFactory` that always returns an empty tuple (no namespacing).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoNamespace;

impl NamespaceFactory for NoNamespace {
    fn get_namespace(&self, _ctx: &RuntimeContext) -> Vec<String> {
        Vec::new()
    }
}

/// A `NamespaceFactory` derived from `user_id` / `session_id` when present.
///
/// When both ids are available the tuple is `["users", <userId>, "sessions",
/// <sessionId>]`; when only a session id is present it is `["sessions",
/// <sessionId>]`; otherwise it is empty (no namespacing).
#[derive(Debug, Clone, Copy, Default)]
pub struct UserSessionNamespace;

impl NamespaceFactory for UserSessionNamespace {
    fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String> {
        let mut out = Vec::new();
        match (ctx.user_id(), ctx.session_id()) {
            (Some(u), Some(s)) => {
                out.push("users".into());
                out.push(u.into());
                out.push("sessions".into());
                out.push(s.into());
            }
            (None, Some(s)) => {
                out.push("sessions".into());
                out.push(s.into());
            }
            (Some(u), None) => {
                out.push("users".into());
                out.push(u.into());
            }
            (None, None) => {}
        }
        out
    }
}
