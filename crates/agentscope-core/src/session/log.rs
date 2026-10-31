//! Blocking-equivalent persistence SPI, mirroring `SessionLog` / `SessionLogStore`.
//!
//! Rust call sites are already async, so the seam is expressed with `async_trait` instead of
//! the Java `boundedElastic` scheduler hop.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::{session_error, SessionEvent, SessionKey};
use crate::agent::RuntimeContext;

/// The visible committed head of a journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    pub seq: u64,
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub lease_until: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
}

impl Head {
    /// The empty head of a journal with no commits.
    pub fn empty() -> Self {
        Self {
            seq: 0,
            epoch: 0,
            owner: None,
            lease_until: 0,
            commit_id: None,
        }
    }
}

/// A writer identity: an owner name plus the epoch that fences stale claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writer {
    pub owner: String,
    pub epoch: u64,
}

/// Durable session journal.
#[async_trait]
pub trait SessionLog: Send + Sync {
    /// Durable input journal, independent of the execution writer and its event sequence.
    fn inbox(&self) -> crate::error::Result<Arc<dyn SessionLog>> {
        Err(session_error(
            "This session backend does not support a durable inbox",
        ))
    }

    /// The current committed head.
    async fn head(&self) -> crate::error::Result<Head>;

    /// Acquires the single execution writer lease.
    async fn acquire(&self, owner: &str, lease: Duration) -> crate::error::Result<Writer>;

    /// Permanently fences this writer identity, including an acquisition that has not
    /// started yet. Other identities may continue this session.
    async fn seal_writer(&self, _owner: &str) -> crate::error::Result<Head> {
        Err(session_error("This session backend cannot seal a writer"))
    }

    /// Extends a writer lease.
    async fn renew(&self, writer: &Writer, lease: Duration) -> crate::error::Result<()>;

    /// Releases a writer lease.
    async fn release(&self, writer: &Writer) -> crate::error::Result<()>;

    /// Appends a contiguous batch, publishing it by one fenced CAS of the head.
    async fn commit(
        &self,
        writer: &Writer,
        batch_id: &str,
        expected_seq: u64,
        events: &[SessionEvent],
    ) -> crate::error::Result<Head>;

    /// Reads up to `limit` committed events strictly after `after`.
    async fn read_after(&self, after: u64, limit: usize)
        -> crate::error::Result<Vec<SessionEvent>>;

    /// Stable-prefix scan of `(after, through]`.
    async fn scan(&self, after: u64, through: u64) -> crate::error::Result<Vec<SessionEvent>> {
        if through < after {
            return Err(session_error("Invalid committed prefix"));
        }
        let mut collected = Vec::new();
        let mut cursor = after;
        while cursor < through {
            let limit = std::cmp::min(256, (through - cursor) as usize);
            let batch = self.read_after(cursor, limit).await?;
            if batch.is_empty() {
                return Err(session_error("Missing committed prefix"));
            }
            for event in batch {
                if event.seq > through {
                    break;
                }
                if event.seq != cursor + 1 {
                    return Err(session_error("Non-contiguous event sequence"));
                }
                cursor = event.seq;
                collected.push(event);
            }
        }
        Ok(collected)
    }

    /// Visits committed events in `(after, through]` without materializing the range.
    ///
    /// Projections use this so only one commit's payload is resident at a time. A long
    /// session can hold tens of megabytes of state checkpoints; collecting them all just to
    /// read the latest one inflated the process's peak heap.
    async fn visit(
        &self,
        after: u64,
        through: u64,
        visitor: &mut (dyn for<'a> FnMut(&'a SessionEvent) -> crate::error::Result<()> + Send),
    ) -> crate::error::Result<()> {
        for event in self.scan(after, through).await? {
            visitor(&event)?;
        }
        Ok(())
    }

    /// The durable export watermark for one sink.
    async fn export_cursor(&self, _sink: &str) -> crate::error::Result<u64> {
        Ok(0)
    }
    /// Advances the export watermark, never regressing it.
    async fn advance_export_cursor(
        &self,
        _sink: &str,
        _expected: u64,
        _next: u64,
    ) -> crate::error::Result<()> {
        Err(session_error("Export cursor persistence not implemented"))
    }

    /// The published view snapshot of this journal, if the backend keeps one.
    ///
    /// Readers may start from it instead of replaying the whole chain; the returned snapshot is
    /// only a *candidate* — callers must still check it against the chain (see
    /// [`commit_at`](SessionLog::commit_at)).
    async fn snapshot(&self) -> crate::error::Result<Option<super::SessionSnapshot>> {
        Ok(None)
    }

    /// The commit id that ends at `seq`, or `None` when this backend cannot tell.
    ///
    /// A snapshot is only usable when it was taken on this very chain: a pointer left behind by
    /// a forked or replaced journal must not seed a replay.
    async fn commit_at(&self, _seq: u64) -> crate::error::Result<Option<String>> {
        Ok(None)
    }

    /// The lowest sequence number still readable, when history below it was reclaimed.
    async fn reclaimed_floor(&self) -> crate::error::Result<Option<u64>> {
        Ok(None)
    }

    /// Publishes a snapshot of the current head and reclaims the history it covers.
    ///
    /// Only a writer holding the lease may call this: publishing from a fenced writer would race
    /// the owner. Returns the published sequence number.
    async fn publish_snapshot(
        &self,
        _writer: &Writer,
        _retention: super::SessionRetention,
    ) -> crate::error::Result<Option<u64>> {
        Ok(None)
    }

    /// Deletes the payloads of checkpoints that a newer checkpoint in the same turn supersedes.
    ///
    /// Recovery always resumes from the newest checkpoint, so an earlier checkpoint of the same
    /// turn can never be read back: it is a full `AgentState` (megabytes) written once per
    /// iteration that nothing consumes. Only payloads are reclaimed, never the commit itself —
    /// a commit also carries the messages recorded up to that point. Backends that do not spill
    /// payloads keep the no-op default.
    async fn reclaim_superseded_checkpoints(
        &self,
        _turn_id: &str,
        _keep_seq: u64,
    ) -> crate::error::Result<()> {
        Ok(())
    }
}

/// Extensible session log backend. Opening a reader must not acquire a writer or repair
/// history.
#[async_trait]
pub trait SessionLogStore: Send + Sync {
    /// Opens (never creates-and-repairs) the journal for a logical key.
    async fn open(
        &self,
        key: &SessionKey,
        context: &RuntimeContext,
    ) -> crate::error::Result<Arc<dyn SessionLog>>;

    /// Discovers native sessions in the caller's storage namespace, without acquiring
    /// writers.
    async fn list(&self, _context: &RuntimeContext) -> crate::error::Result<Vec<SessionKey>> {
        Err(session_error(
            "This session log store does not support discovery",
        ))
    }

    /// Deletes the journal for a logical key, including everything the backend keeps beside it
    /// (snapshots, inbox, export cursors).
    ///
    /// Backends that cannot delete the journal must fail visibly: a caller that believes a
    /// session is gone while its history is still readable would keep leaking it. The default
    /// therefore reports unsupported instead of pretending success.
    async fn delete(&self, _key: &SessionKey) -> crate::error::Result<()> {
        Err(session_error(
            "This session log store does not support deletion",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that only implements the mandatory `open`, so `delete`'s default is what runs.
    struct OpenOnlyStore;

    #[async_trait]
    impl SessionLogStore for OpenOnlyStore {
        async fn open(
            &self,
            _key: &SessionKey,
            _context: &RuntimeContext,
        ) -> crate::error::Result<Arc<dyn SessionLog>> {
            Err(session_error("open is not implemented by this test store"))
        }
    }

    #[tokio::test]
    async fn default_delete_reports_unsupported_instead_of_succeeding() {
        let store = OpenOnlyStore;
        let key = SessionKey::new(None::<String>, "agent", "session").unwrap();
        let error = store
            .delete(&key)
            .await
            .expect_err("an unsupported delete must not report success");
        assert!(
            matches!(error, crate::error::Error::Session(_)),
            "unexpected error kind: {error:?}"
        );
        assert!(error.to_string().contains("does not support deletion"));
    }
}
