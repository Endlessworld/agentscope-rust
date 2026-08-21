//! Session transcript persistence, mirroring `io.agentscope.harness.agent.transcript`.
//!
//! Provides an append-only JSONL store keyed by `{tenant}/{agentId}/{sessionId}/events/`
//! plus a filesystem-backed implementation. A complementary `TranscriptMiddleware`
//! (in `agent::middleware`) persists each agent turn after the call completes.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::filesystem::Filesystem;

/// Identifies a transcript stream for a (tenant, agent, session) triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRef {
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
}

impl TranscriptRef {
    pub fn new(tenant: impl Into<String>, agent_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            agent_id: agent_id.into(),
            session_id: session_id.into(),
        }
    }

    /// Directory layout: `{root}/{tenant}/{agent_id}/{session_id}/events`.
    pub fn events_dir(&self, root: &str) -> String {
        format!(
            "{}/{}/{}/{}/events",
            root, self.tenant, self.agent_id, self.session_id
        )
    }
}

/// An immutable, append-only segment of a transcript (one per agent turn).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub seq: u64,
    pub timestamp: String,
    /// The input messages observed at the start of the turn.
    pub messages: Vec<Value>,
    /// The events produced by the turn (model calls, tool calls, results, …).
    pub events: Vec<Value>,
}

/// Append-only transcript storage, mirroring `TranscriptStore`.
#[async_trait]
pub trait TranscriptStore: Send + Sync {
    /// Appends one turn segment; returns the assigned sequence number.
    async fn append_segment(
        &self,
        r#ref: &TranscriptRef,
        messages: &[Value],
        events: &[Value],
    ) -> Result<u64>;

    /// Reads all segments for a session, in append order.
    async fn read_segments(&self, r#ref: &TranscriptRef) -> Result<Vec<TranscriptSegment>>;

    /// Lists session ids recorded under a (tenant, agent) pair.
    async fn list_sessions(&self, tenant: &str, agent_id: &str) -> Result<Vec<String>>;
}

/// Filesystem-backed transcript store (mirroring `FilesystemTranscriptStore`).
///
/// Each segment is one JSONL file under the session's `events/` directory; the
/// sequence number is encoded in the filename so ordering is stable and the
/// segments stay immutable once written.
pub struct FilesystemTranscriptStore {
    fs: Arc<dyn Filesystem>,
    root: String,
}

impl FilesystemTranscriptStore {
    pub fn new(fs: Arc<dyn Filesystem>, root: impl Into<String>) -> Self {
        Self { fs, root: root.into() }
    }

    fn segment_path(&self, r#ref: &TranscriptRef, seq: u64) -> String {
        format!("{}/{:08}.jsonl", r#ref.events_dir(&self.root), seq)
    }
}

#[async_trait]
impl TranscriptStore for FilesystemTranscriptStore {
    async fn append_segment(
        &self,
        r#ref: &TranscriptRef,
        messages: &[Value],
        events: &[Value],
    ) -> Result<u64> {
        let rt = agentscope_core::agent::RuntimeContext::default();
        let dir = r#ref.events_dir(&self.root);
        let existing = self.fs.ls(&rt, &dir).await.unwrap_or_default();
        let seq = (existing.len() as u64) + 1;
        let segment = TranscriptSegment {
            seq,
            timestamp: chrono::Utc::now().to_rfc3339(),
            messages: messages.to_vec(),
            events: events.to_vec(),
        };
        let line = serde_json::to_string(&segment)
            .map_err(|e| agentscope_core::Error::Tool(format!("transcript serialize: {e}")))?;
        self.fs
            .write(&rt, &self.segment_path(r#ref, seq), &line)
            .await?;
        Ok(seq)
    }

    async fn read_segments(&self, r#ref: &TranscriptRef) -> Result<Vec<TranscriptSegment>> {
        let rt = agentscope_core::agent::RuntimeContext::default();
        let dir = r#ref.events_dir(&self.root);
        let entries = match self.fs.ls(&rt, &dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut segments = Vec::new();
        for entry in entries {
            if entry.is_directory {
                continue;
            }
            // `entry.path` is a display path (may carry a leading `/`); reconstruct a
            // root-relative path so backends resolve it consistently across platforms.
            let fname = entry.path.rsplit('/').next().unwrap_or(&entry.path);
            let read_path = format!("{dir}/{fname}");
            match self.fs.read(&rt, &read_path, 0, 0).await {
                Ok(data) => {
                    if let Ok(seg) = serde_json::from_str::<TranscriptSegment>(&data.content) {
                        segments.push(seg);
                    }
                }
                Err(_) => continue,
            }
        }
        segments.sort_by_key(|s| s.seq);
        Ok(segments)
    }

    async fn list_sessions(&self, tenant: &str, agent_id: &str) -> Result<Vec<String>> {
        let rt = agentscope_core::agent::RuntimeContext::default();
        let dir = format!("{}/{}/{}", self.root, tenant, agent_id);
        let entries = match self.fs.ls(&rt, &dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(entries
            .into_iter()
            .filter(|e| e.is_directory)
            .map(|e| {
                e.path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(&e.path)
                    .to_string()
            })
            .collect())
    }
}
