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
    async fn read_after(&self, after: u64, limit: usize) -> crate::error::Result<Vec<SessionEvent>>;

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
        Err(session_error(
            "Export cursor persistence not implemented",
        ))
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
}
