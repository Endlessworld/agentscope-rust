//! Logical session identity, mirroring `io.agentscope.core.session.SessionKey`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use super::session_error;

/// Logical identity; storage namespaces remain the responsibility of the workspace backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub user_id: Option<String>,
    pub agent_id: String,
    pub session_id: String,
}

impl SessionKey {
    /// Creates a key, rejecting empty required identities.
    pub fn new(
        user_id: Option<impl Into<String>>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> crate::error::Result<Self> {
        let agent_id = agent_id.into();
        let session_id = session_id.into();
        if agent_id.trim().is_empty() || session_id.trim().is_empty() {
            return Err(session_error("Empty session identity"));
        }
        Ok(Self {
            user_id: user_id.map(Into::into),
            agent_id,
            session_id,
        })
    }

    /// Storage path: `agents/{agent}/sessions/{user}/{session}`, each component encoded so
    /// arbitrary ids cannot escape the namespace.
    pub fn storage_path(&self) -> String {
        format!(
            "agents/{}/sessions/{}/{}",
            encode(&self.agent_id),
            encode(self.user_id.as_deref().unwrap_or("")),
            encode(&self.session_id)
        )
    }
}

fn encode(value: &str) -> String {
    format!("s_{}", URL_SAFE_NO_PAD.encode(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_path_encodes_each_component() {
        let key = SessionKey::new(Some("alice"), "cost-assistant", "conversation/001").unwrap();
        // `s_YWxpY2U` is URL-safe base64 of "alice" without padding.
        assert_eq!(
            key.storage_path(),
            "agents/s_Y29zdC1hc3Npc3RhbnQ/sessions/s_YWxpY2U/s_Y29udmVyc2F0aW9uLzAwMQ"
        );
        // The encoded path never contains a raw separator from the id.
        assert!(!key.storage_path().contains("conversation/001"));
    }

    #[test]
    fn anonymous_user_encodes_empty_string() {
        let key = SessionKey::new(None::<String>, "agent", "s1").unwrap();
        assert!(key.storage_path().starts_with("agents/s_YWdlbnQ/sessions/s_/s_"));
    }

    #[test]
    fn blank_identity_is_rejected() {
        assert!(SessionKey::new(None::<String>, "  ", "s1").is_err());
        assert!(SessionKey::new(None::<String>, "agent", "").is_err());
    }

    #[test]
    fn distinct_ids_never_collide_on_path() {
        let a = SessionKey::new(Some("u"), "agent", "a/b").unwrap();
        let b = SessionKey::new(Some("u"), "agent", "a%2Fb").unwrap();
        assert_ne!(a.storage_path(), b.storage_path());
    }
}
