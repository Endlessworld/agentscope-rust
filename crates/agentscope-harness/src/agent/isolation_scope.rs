//! Controls how agent state is isolated and shared across calls, mirroring `IsolationScope`.
//!
//! The scope determines the namespace prefix used by remote/sandbox filesystems and the
//! `(userId, sessionId)` isolation key used for state persistence.

/// Canonical isolation-scope definition used by the filesystem and state backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationScope {
    /// Isolated per session identifier. Falls back to no namespace when absent.
    Session,
    /// Shared across all sessions of the same user; the default. Falls back to `Session`
    /// when `userId` is absent.
    #[default]
    User,
    /// Shared across all users and sessions of the same agent (agent name always available).
    Agent,
    /// Globally shared within the same workspace/store instance.
    Global,
}

impl IsolationScope {
    /// Derives the filesystem/store namespace prefix from the given identifiers.
    ///
    /// * `User` — prefix with `userId`, falling back to `sessionId` when absent.
    /// * `Session` — prefix with `sessionId`.
    /// * `Agent` / `Global` — no prefix (workspace is already per-agent).
    pub fn namespace(&self, user_id: Option<&str>, session_id: Option<&str>) -> Vec<String> {
        match self {
            IsolationScope::User => {
                if let Some(uid) = user_id.filter(|s| !s.trim().is_empty()) {
                    vec![uid.to_string()]
                } else if let Some(sid) = session_id.filter(|s| !s.trim().is_empty()) {
                    vec![sid.to_string()]
                } else {
                    Vec::new()
                }
            }
            IsolationScope::Session => session_id
                .filter(|s| !s.trim().is_empty())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
            IsolationScope::Agent | IsolationScope::Global => Vec::new(),
        }
    }
}
