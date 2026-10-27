//! Sandbox isolation keys.
use super::super::isolation_scope::IsolationScope;
use agentscope_core::agent::RuntimeContext;
use serde::{Deserialize, Serialize};

/// Key for sandbox isolation.
///
/// Used by `SandboxExecutionGuard` to prevent concurrent execution
/// for the same isolation key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxIsolationKey {
    /// The scope type (e.g., "user", "session", "agent").
    pub scope: String,

    /// The scope value (e.g., user ID, session ID, agent ID).
    pub value: String,

    /// Optional sub-key for finer isolation.
    pub sub_key: Option<String>,
}

impl SandboxIsolationKey {
    /// Creates a new isolation key.
    pub fn new(scope: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            value: value.into(),
            sub_key: None,
        }
    }

    /// Creates a new isolation key with sub-key.
    pub fn with_sub_key(
        scope: impl Into<String>,
        value: impl Into<String>,
        sub_key: impl Into<String>,
    ) -> Self {
        Self {
            scope: scope.into(),
            value: value.into(),
            sub_key: Some(sub_key.into()),
        }
    }

    /// Resolves an isolation key from scope, runtime context, and agent ID.
    pub fn resolve(
        isolation_scope: Option<&IsolationScope>,
        runtime_context: Option<&RuntimeContext>,
        agent_id: &str,
    ) -> Option<Self> {
        match isolation_scope {
            Some(IsolationScope::User) => {
                let user_id = runtime_context
                    .and_then(|ctx| ctx.user_id.as_deref())
                    .unwrap_or("anonymous");
                Some(Self::new("user", user_id))
            }
            Some(IsolationScope::Session) => {
                let session_id = runtime_context
                    .and_then(|ctx| ctx.session_id.as_deref())
                    .unwrap_or("anonymous");
                Some(Self::new("session", session_id))
            }
            Some(IsolationScope::Agent) => Some(Self::new("agent", agent_id)),
            Some(IsolationScope::Global) => Some(Self::new("global", "global")),
            None => None,
        }
    }

    /// Returns a string representation suitable for use as a file/directory name.
    pub fn to_path_component(&self) -> String {
        let base = format!("{}_{}", self.scope, self.value);
        match &self.sub_key {
            Some(sub) => format!("{}_{}", base, sub),
            None => base,
        }
    }
}

impl std::fmt::Display for SandboxIsolationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.scope, self.value)?;
        if let Some(sub_key) = &self.sub_key {
            write!(f, ":{}", sub_key)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_isolation_key_creation() {
        let key = SandboxIsolationKey::new("user", "user123");
        assert_eq!(key.scope, "user");
        assert_eq!(key.value, "user123");
        assert!(key.sub_key.is_none());
    }

    #[test]
    fn test_sandbox_isolation_key_with_sub_key() {
        let key = SandboxIsolationKey::with_sub_key("user_agent", "user123", "agent456");
        assert_eq!(key.scope, "user_agent");
        assert_eq!(key.value, "user123");
        assert_eq!(key.sub_key.as_deref(), Some("agent456"));
    }

    #[test]
    fn test_sandbox_isolation_key_display() {
        let key = SandboxIsolationKey::new("user", "user123");
        assert_eq!(key.to_string(), "user:user123");

        let key = SandboxIsolationKey::with_sub_key("user_agent", "user123", "agent456");
        assert_eq!(key.to_string(), "user_agent:user123:agent456");
    }

    #[test]
    fn test_sandbox_isolation_key_path_component() {
        let key = SandboxIsolationKey::new("user", "user123");
        assert_eq!(key.to_path_component(), "user_user123");

        let key = SandboxIsolationKey::with_sub_key("user_agent", "user123", "agent456");
        assert_eq!(key.to_path_component(), "user_agent_user123_agent456");
    }
}
