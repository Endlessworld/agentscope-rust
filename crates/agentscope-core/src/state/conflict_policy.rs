//! Conflict policy for `agent_state` saves, mirroring `io.agentscope.core.state.ConflictPolicy`.

/// How a state save resolves an optimistic-concurrency conflict.
///
/// A conflict is observed when [`AgentStateStore::save_if_version`] reports
/// [`UNVERSIONED`](super::store::UNVERSIONED) while the caller held a known version —
/// i.e. another writer advanced the slot first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConflictPolicy {
    /// Unconditional overwrite (opt-in; the legacy last-writer-wins semantics).
    ///
    /// Defaulting to this meant a second writer silently clobbered the first turn's messages:
    /// on the non-durable path `SessionTurnGate` only serialises in-process turns, so any other
    /// writer (a second host, a restarted process, a background wakeup) was invisible.
    Overwrite,
    /// Fail the save and surface a [`Error::ConcurrentSessionModification`] (default).
    ///
    /// [`Error::ConcurrentSessionModification`]: crate::error::Error::ConcurrentSessionModification
    #[default]
    Fail,
    /// Reload the latest baseline, append this turn's newly added messages, and retry.
    ///
    /// Opt-in only — not safe for compaction / summary turns that rewrite history.
    AppendMerge,
}

impl ConflictPolicy {
    /// The Java enum constant name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictPolicy::Overwrite => "OVERWRITE",
            ConflictPolicy::Fail => "FAIL",
            ConflictPolicy::AppendMerge => "APPEND_MERGE",
        }
    }

    /// Parses a Java enum constant name (case-insensitive).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "OVERWRITE" => Some(ConflictPolicy::Overwrite),
            "FAIL" => Some(ConflictPolicy::Fail),
            "APPEND_MERGE" => Some(ConflictPolicy::AppendMerge),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fail() {
        // Last-writer-wins is now opt-in: a conflict must be reported, not silently applied.
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Fail);
    }

    #[test]
    fn wire_names_round_trip() {
        for policy in [
            ConflictPolicy::Overwrite,
            ConflictPolicy::Fail,
            ConflictPolicy::AppendMerge,
        ] {
            assert_eq!(ConflictPolicy::from_wire(policy.as_str()), Some(policy));
        }
        assert_eq!(
            ConflictPolicy::from_wire("append_merge"),
            Some(ConflictPolicy::AppendMerge)
        );
        assert_eq!(ConflictPolicy::from_wire("NOPE"), None);
    }
}
