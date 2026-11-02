//! Host-authorized storage for immutable pre-compaction snapshots
//! (`JevContextArchive`). Never re-executes a tool.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::JevError;

use super::message::ContextMessage;

/// The only envelope version this port reads and writes.
pub const ARCHIVE_VERSION: u32 = 1;

/// The error reported by the fail-closed placeholder, i.e. when the host supplied no store.
///
/// The store-backed implementation is
/// [`StoreJevContextArchive`](crate::jev::runtime::archive::StoreJevContextArchive).
pub const HARNESS_STORE_ARCHIVE_UNAVAILABLE: &str =
    "context archive unavailable: no harness BaseStore was injected; use jev::runtime::archive::StoreJevContextArchive";

/// The owner triple a snapshot belongs to (`JevContextArchive.Scope`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveScope {
    /// The user the snapshot belongs to.
    pub user_id: String,
    /// The agent the snapshot belongs to.
    pub agent_id: String,
    /// The session the snapshot belongs to.
    pub session_id: String,
}

impl ArchiveScope {
    /// Creates and validates a scope (`Scope` compact constructor).
    pub fn new(
        user_id: impl Into<String>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, JevError> {
        let scope = Self {
            user_id: user_id.into(),
            agent_id: agent_id.into(),
            session_id: session_id.into(),
        };
        for value in [&scope.user_id, &scope.agent_id, &scope.session_id] {
            if value.trim().is_empty() || value.chars().count() > 512 || value.contains('\u{0}') {
                return Err(JevError::invalid("user, agent and session scope required"));
            }
        }
        Ok(scope)
    }
}

/// The stored snapshot envelope (`{version, scope, messages}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveEnvelope {
    /// The envelope version; only [`ARCHIVE_VERSION`] is accepted.
    pub version: u32,
    /// The scope the snapshot was saved under.
    pub scope: ArchiveScope,
    /// The messages to restore.
    pub messages: Vec<ContextMessage>,
}

impl ArchiveEnvelope {
    /// Creates a version-1 envelope.
    pub fn new(scope: ArchiveScope, messages: Vec<ContextMessage>) -> Self {
        Self {
            version: ARCHIVE_VERSION,
            scope,
            messages,
        }
    }
}

/// Whether `reference` looks like the SHA-256 hex digest Java requires.
pub fn is_valid_reference(reference: &str) -> bool {
    reference.len() == 64
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Requires a valid archive reference (`invalid archive reference`).
pub fn require_reference(reference: &str) -> Result<(), JevError> {
    if !is_valid_reference(reference) {
        return Err(JevError::invalid("invalid archive reference"));
    }
    Ok(())
}

/// Decodes and validates an envelope read from storage.
///
/// A missing version or a scope mismatch is rejected with the Java message; this is the guard
/// against reading another tenant's or another format's snapshot.
pub fn decode_envelope(data: &[u8], scope: &ArchiveScope) -> Result<Vec<ContextMessage>, JevError> {
    let envelope: ArchiveEnvelope = serde_json::from_slice(data)
        .map_err(|_error| JevError::invalid("missing or invalid snapshot"))?;
    if envelope.version != ARCHIVE_VERSION || envelope.scope != *scope {
        return Err(JevError::invalid("archive scope or version mismatch"));
    }
    Ok(envelope.messages)
}

/// Host-authorized storage for immutable pre-compaction snapshots.
#[async_trait]
pub trait JevContextArchive: Send + Sync {
    /// Stores the snapshot and returns its content-addressed reference.
    async fn save(
        &self,
        scope: &ArchiveScope,
        messages: &[ContextMessage],
    ) -> Result<String, JevError>;

    /// Returns a prior snapshot for inspection. The host must not overwrite newer active
    /// history.
    async fn restore(
        &self,
        scope: &ArchiveScope,
        reference: &str,
    ) -> Result<Vec<ContextMessage>, JevError>;
}

/// An archive that always reports that the host supplied no store.
///
/// The store-backed implementation is
/// [`StoreJevContextArchive`](crate::jev::runtime::archive::StoreJevContextArchive). Using this
/// type keeps `ENFORCE` fail-closed instead of silently dropping the recovery snapshot.
pub struct UnavailableContextArchive {
    reason: String,
}

impl UnavailableContextArchive {
    /// Creates the archive with an explicit reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The archive Java ships when no store is wired.
    pub fn harness_store_missing() -> Self {
        Self::new(HARNESS_STORE_ARCHIVE_UNAVAILABLE)
    }
}

impl std::fmt::Debug for UnavailableContextArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnavailableContextArchive")
            .field("reason", &self.reason)
            .finish()
    }
}

#[async_trait]
impl JevContextArchive for UnavailableContextArchive {
    async fn save(
        &self,
        _scope: &ArchiveScope,
        _messages: &[ContextMessage],
    ) -> Result<String, JevError> {
        Err(JevError::protocol(self.reason.clone()))
    }

    async fn restore(
        &self,
        _scope: &ArchiveScope,
        _reference: &str,
    ) -> Result<Vec<ContextMessage>, JevError> {
        Err(JevError::protocol(self.reason.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ArchiveScope {
        ArchiveScope::new("u", "a", "s").unwrap()
    }

    #[test]
    fn scopes_are_bounded_and_non_blank() {
        for (user, agent, session) in [("", "a", "s"), ("u", " ", "s"), ("u", "a", "")] {
            assert_eq!(
                ArchiveScope::new(user, agent, session)
                    .unwrap_err()
                    .message(),
                "user, agent and session scope required"
            );
        }
        assert_eq!(
            ArchiveScope::new("u", "a", "x".repeat(513))
                .unwrap_err()
                .message(),
            "user, agent and session scope required"
        );
        assert_eq!(scope().user_id, "u");
    }

    #[test]
    fn references_are_lowercase_sha256_hex() {
        assert!(is_valid_reference(&"a".repeat(64)));
        assert!(is_valid_reference(&"0123456789abcdef".repeat(4)));
        assert!(!is_valid_reference(&"A".repeat(64)));
        assert!(!is_valid_reference(&"a".repeat(63)));
        assert!(!is_valid_reference(&"g".repeat(64)));
        assert_eq!(
            require_reference("nope").unwrap_err().message(),
            "invalid archive reference"
        );
    }

    #[test]
    fn envelopes_round_trip_and_check_version_and_scope() {
        let envelope = ArchiveEnvelope::new(scope(), vec![ContextMessage::user("m1", "hello")]);
        let data = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(decode_envelope(&data, &scope()).unwrap().len(), 1);

        let other = ArchiveScope::new("u", "a", "other").unwrap();
        assert_eq!(
            decode_envelope(&data, &other).unwrap_err().message(),
            "archive scope or version mismatch"
        );

        let mut wrong = envelope.clone();
        wrong.version = 2;
        assert_eq!(
            decode_envelope(&serde_json::to_vec(&wrong).unwrap(), &scope())
                .unwrap_err()
                .message(),
            "archive scope or version mismatch"
        );
        assert_eq!(
            decode_envelope(b"not json", &scope())
                .unwrap_err()
                .message(),
            "missing or invalid snapshot"
        );
    }

    #[tokio::test]
    async fn the_unavailable_archive_fails_closed_with_its_reason() {
        let archive = UnavailableContextArchive::harness_store_missing();
        let error = archive.save(&scope(), &[]).await.unwrap_err();
        assert!(error.message().contains("StoreJevContextArchive"));
        let restore = archive
            .restore(&scope(), &"a".repeat(64))
            .await
            .unwrap_err();
        assert_eq!(restore.message(), error.message());
        assert!(format!("{archive:?}").contains("UnavailableContextArchive"));
        assert!(UnavailableContextArchive::new("custom")
            .save(&scope(), &[])
            .await
            .is_err());
    }
}
