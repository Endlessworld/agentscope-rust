//! Published view snapshots: a fold of the journal, written as one immutable object and
//! published by a single CAS of `snapshot.json` (P0-2, S1/S3).
//!
//! The journal itself stays "one commit per flush": a crash can never lose a committed event.
//! What snapshots fix is the *read* side — a cold start used to replay the whole commit chain,
//! and history could never be reclaimed because replay started at seq 0.
//!
//! ```text
//! snapshot.json              SnapshotPointer (latest)     <- the only publish point (CAS)
//! snapshot.previous.json     SnapshotPointer (previous)   <- retention floor + fallback
//! snapshots/<hash>.json      SessionSnapshot (immutable, named by its own checksum)
//! ```
//!
//! Crash safety: the object is written before the pointer. A crash while writing leaves an
//! orphan object that the next publish overwrites or ignores; the pointer is only advanced once
//! the object it names is durable, so a reader either sees a complete snapshot or the old one.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{SessionEvent, SessionLog, SessionProjection, SessionTurns, Transcript};

/// Snapshot layout version. A pointer or object with another version is ignored.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// Publish a snapshot once this many events have been committed since the previous one.
pub const SNAPSHOT_EVERY_EVENTS: u64 = 512;

/// How long the recorder waits before publishing an otherwise unchanged snapshot.
pub const SNAPSHOT_EVERY: Duration = Duration::from_secs(60);

/// Run ids remembered for the "run id already used" guard.
///
/// Newest-last and capped: a session with more runs than this keeps the guard only for recent
/// runs, which is where a duplicated id would actually come from.
pub const SNAPSHOT_TRACKED_RUNS: usize = 4096;

/// Name of the pointer to the latest snapshot (relative to the journal prefix).
pub(crate) const SNAPSHOT_POINTER: &str = "snapshot.json";

/// Name of the pointer to the snapshot before the latest one.
pub(crate) const SNAPSHOT_PREVIOUS: &str = "snapshot.previous.json";

/// A fold of one committed prefix, plus the sequence number it is exact as of.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub format: u32,
    pub as_of_seq: u64,
    pub as_of_commit_id: String,
    #[serde(default)]
    pub turns: SessionTurns,
    #[serde(default)]
    pub projection: SessionProjection,

    #[serde(default)]
    pub pending: BTreeMap<String, SessionEvent>,
    #[serde(default)]
    pub transcript: Transcript,
    /// Execution run ids seen up to `as_of_seq`, oldest first.
    #[serde(default)]
    pub runs: Vec<String>,
}

impl SessionSnapshot {
    /// Keeps the guard bounded: only the newest [`SNAPSHOT_TRACKED_RUNS`] ids are retained.
    pub fn trim_runs(runs: &mut Vec<String>) {
        if runs.len() > SNAPSHOT_TRACKED_RUNS {
            let drop = runs.len() - SNAPSHOT_TRACKED_RUNS;
            runs.drain(..drop);
        }
    }
}

/// Names one published snapshot: its sequence number, commit and object checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPointer {
    pub format: u32,
    pub seq: u64,
    pub commit_id: String,
    pub hash: String,
    pub published_at: i64,
}

/// How much committed history is kept once a snapshot covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRetention {
    /// Whether commits covered by an older snapshot may be deleted at all.
    pub prune: bool,
    /// Keep every commit younger than this, even when a snapshot covers it.
    pub min_age: Duration,
}

impl SessionRetention {
    /// Retention disabled: snapshots are still published, nothing is deleted.
    pub const OFF: Self = Self {
        prune: false,
        min_age: Duration::ZERO,
    };

    /// Reads `AGENTSCOPE_SESSION_RETENTION` once per publish.
    ///
    /// `off`, `0`, `false` and `no` disable deletion; anything else (including an absent value)
    /// keeps the default. A bare number is read as the minimum age in days; a `m`/`h`/`d` suffix
    /// reads minutes/hours/days, which is what bounds the journal of a long-running session (each
    /// turn appends a full-state checkpoint blob, see `journal::blobs`).
    pub fn from_env() -> Self {
        match std::env::var("AGENTSCOPE_SESSION_RETENTION") {
            Ok(value) => {
                let trimmed = value.trim().to_ascii_lowercase();
                match trimmed.as_str() {
                    "off" | "0" | "false" | "no" | "disabled" => Self::OFF,
                    "" => Self::default(),
                    other => match parse_min_age(other) {
                        Some(min_age) if min_age.is_zero() => Self::OFF,
                        Some(min_age) => Self {
                            min_age,
                            ..Self::default()
                        },
                        None => Self::default(),
                    },
                }
            }
            Err(_) => Self::default(),
        }
    }
}

/// Parses `90m` / `6h` / `2d` / `30s`; a bare number is days. Returns `None` for anything else so
/// the caller can fall back to the default instead of silently disabling retention.
fn parse_min_age(value: &str) -> Option<Duration> {
    let (number, unit) = match value.char_indices().next_back() {
        Some((index, last)) if last.is_ascii_alphabetic() => (&value[..index], Some(last)),
        _ => (value, None),
    };
    let number: u64 = number.trim().parse().ok()?;
    let seconds = match unit {
        None | Some('d') => number.checked_mul(86_400)?,
        Some('h') => number.checked_mul(3_600)?,
        Some('m') => number.checked_mul(60)?,
        Some('s') => number,
        Some(_) => return None,
    };
    Some(Duration::from_secs(seconds))
}

impl Default for SessionRetention {
    fn default() -> Self {
        Self {
            prune: true,
            min_age: Duration::from_secs(30 * 86_400),
        }
    }
}

/// Hex-encoded SHA-256, matching the journal's object naming.
pub(crate) fn hash_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Path of the immutable snapshot object named `hash`.
pub(crate) fn object_path(prefix: &str, hash: &str) -> String {
    format!("{prefix}/snapshots/{hash}.json")
}

/// Loads the snapshot a backend published, ignoring one written in an unknown format.
///
/// This only rules out a *format* mismatch. A caller must still confirm that the snapshot was
/// taken on the chain it is about to extend (see [`SessionLog::commit_at`]).
pub(crate) async fn load_verified(
    log: &dyn SessionLog,
) -> crate::error::Result<Option<SessionSnapshot>> {
    match log.snapshot().await? {
        Some(snapshot) if snapshot.format == SNAPSHOT_FORMAT => Ok(Some(snapshot)),
        Some(snapshot) => {
            tracing::warn!(
                format = snapshot.format,
                expected = SNAPSHOT_FORMAT,
                "ignoring a session snapshot written in an unknown format"
            );
            Ok(None)
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_reads_the_off_switches() {
        // The environment is process-wide, so this only checks the parsing helper indirectly:
        // `from_env` is exercised through the `off` spellings by the journal tests instead.
        assert!(SessionRetention::OFF.min_age.is_zero());
        assert!(!SessionRetention::OFF.prune);
        assert!(SessionRetention::default().prune);
        assert_eq!(
            SessionRetention::default().min_age,
            Duration::from_secs(30 * 86_400)
        );
    }

    /// A long-running session writes one full-state checkpoint blob per turn, so retention has to
    /// be expressible in hours -- days are too coarse to bound a single day's journal.
    #[test]
    fn retention_accepts_sub_day_units() {
        assert_eq!(parse_min_age("90m"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_min_age("6h"), Some(Duration::from_secs(21_600)));
        assert_eq!(parse_min_age("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_min_age("30s"), Some(Duration::from_secs(30)));
        // A bare number keeps its historical meaning: days.
        assert_eq!(parse_min_age("7"), Some(Duration::from_secs(604_800)));
        // Zero means "do not prune", which `from_env` maps onto `OFF`.
        assert_eq!(parse_min_age("0"), Some(Duration::ZERO));
        // Unknown spellings fall back to the default rather than disabling retention.
        assert_eq!(parse_min_age("soon"), None);
        assert_eq!(parse_min_age("6w"), None);
    }

    #[test]
    fn runs_are_trimmed_to_the_newest_entries() {
        let mut runs: Vec<String> = (0..SNAPSHOT_TRACKED_RUNS + 2)
            .map(|index| format!("run-{index}"))
            .collect();
        SessionSnapshot::trim_runs(&mut runs);
        assert_eq!(runs.len(), SNAPSHOT_TRACKED_RUNS);
        assert_eq!(runs.first().map(String::as_str), Some("run-2"));
    }
}
