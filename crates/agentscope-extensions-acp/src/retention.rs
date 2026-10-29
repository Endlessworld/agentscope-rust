//! Defaults this host applies to the session journal.
//!
//! The event log stores a full-context checkpoint per model call, so an unpruned journal grows
//! with the square of the conversation: one ACP session reached 700 MB of payload blobs in an
//! hour, and the next cold start had to hydrate them. Core reads the window from
//! `AGENTSCOPE_SESSION_RETENTION`; the host supplies a bounded default instead of leaving core's
//! 30-day one, because nothing outside these binaries can keep it in check.

/// Environment variable core reads for the journal retention window.
pub const SESSION_RETENTION_ENV: &str = "AGENTSCOPE_SESSION_RETENTION";

/// Raw events older than this are dropped once a snapshot covers them. The snapshot keeps the
/// conversation itself (turns, projection, transcript, pending work); only the event log of old
/// turns is lost.
pub const DEFAULT_SESSION_RETENTION: &str = "12h";

/// Applies [`DEFAULT_SESSION_RETENTION`] unless the host already chose a window.
pub fn apply_session_defaults() {
    if std::env::var_os(SESSION_RETENTION_ENV).is_none() {
        // SAFETY-free: `set_var` is safe on edition 2021; this runs before any task is spawned.
        std::env::set_var(SESSION_RETENTION_ENV, DEFAULT_SESSION_RETENTION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that chose its own window must win: the default only fills a gap.
    #[test]
    fn an_explicit_window_is_left_alone() {
        std::env::set_var(SESSION_RETENTION_ENV, "off");
        apply_session_defaults();
        assert_eq!(std::env::var(SESSION_RETENTION_ENV).unwrap(), "off");
        std::env::remove_var(SESSION_RETENTION_ENV);
        apply_session_defaults();
        assert_eq!(
            std::env::var(SESSION_RETENTION_ENV).unwrap(),
            DEFAULT_SESSION_RETENTION
        );
        std::env::remove_var(SESSION_RETENTION_ENV);
    }
}
