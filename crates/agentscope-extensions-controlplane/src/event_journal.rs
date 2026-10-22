//! Crash-safe local outbox for Level-2 events, mirroring `EventJournal`.
//!
//! Records are length-prefixed (u32 big-endian) JSON blobs appended to a per-identity
//! file, fsync'd on every write. The Java version stores protobuf `SessionEventMsg`;
//! the Rust port stores [`SessionEvent`] JSON, which is the same wire-agnostic payload.
//! Callers serialize access.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::config::ControlPlaneConfig;
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::SessionEvent;

/// Records larger than this are rejected (matches the Java 16 MiB bound).
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Prefix used for native source-watermark entries in the `.sequences` checkpoint.
///
/// Mirrors the Java `"__native__"` prefix, so the on-disk format stays comparable.
const NATIVE_WATERMARK_PREFIX: &str = "__native__";

/// Crash-safe local outbox for Level-2 events.
#[derive(Debug)]
pub struct EventJournal {
    path: PathBuf,
    sequence_path: PathBuf,
    pending: Vec<SessionEvent>,
    latest_sequences: HashMap<String, i64>,
    /// Highest `native_seq` seen per native source, so re-delivered committed facts are
    /// dropped instead of journaled twice. Mirrors Java `EventJournal.sourceWatermarks`.
    source_watermarks: HashMap<String, i64>,
}

impl EventJournal {
    /// Opens (creating if needed) the journal described by `config`.
    pub fn new(config: &ControlPlaneConfig) -> ControlPlaneResult<Self> {
        let root = if config.event_journal_dir.is_empty() {
            default_journal_dir()
        } else {
            PathBuf::from(&config.event_journal_dir)
        };
        std::fs::create_dir_all(&root)
            .map_err(|e| ControlPlaneError::Config(format!("create journal dir {:?}: {e}", root)))?;
        let identity = format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            config.tenant, config.namespace, config.agent_key, config.instance_key
        );
        let digest = sha256_hex(identity.as_bytes());
        let mut journal = Self {
            path: root.join(format!("{digest}.events")),
            sequence_path: root.join(format!("{digest}.sequences")),
            pending: Vec::new(),
            latest_sequences: HashMap::new(),
            source_watermarks: HashMap::new(),
        };
        journal.load()?;
        Ok(journal)
    }

    /// Latest sequence seen per session id.
    pub fn latest_sequences(&self) -> HashMap<String, i64> {
        self.latest_sequences.clone()
    }

    /// Latest sequence for one session (0 when none).
    pub fn latest_sequence(&self, session_id: &str) -> i64 {
        self.latest_sequences.get(session_id).copied().unwrap_or(0)
    }

    /// Highest native sequence already journaled for `source` (0 when none).
    ///
    /// The committed-event ingest path uses this as the per-session idempotency watermark,
    /// mirroring Java `EventJournal.sourceWatermark`.
    pub fn source_watermark(&self, source: &str) -> i64 {
        self.source_watermarks.get(source).copied().unwrap_or(0)
    }

    /// Number of un-acked pending events.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// All un-acked pending events.
    pub fn pending(&self) -> &[SessionEvent] {
        &self.pending
    }

    /// Appends an event: length-prefixed JSON, fsync'd, then tracked as pending.
    pub fn append(&mut self, event: SessionEvent) -> ControlPlaneResult<()> {
        let payload = serde_json::to_vec(&event).map_err(|e| ControlPlaneError::Http(e.to_string()))?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(ControlPlaneError::Config(format!(
                "event record exceeds {MAX_RECORD_BYTES} bytes"
            )));
        }
        let mut record = Vec::with_capacity(4 + payload.len());
        record.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        record.extend_from_slice(&payload);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| ControlPlaneError::Http(format!("open journal: {e}")))?;
        file.write_all(&record)
            .map_err(|e| ControlPlaneError::Http(format!("append journal: {e}")))?;
        file.sync_all()
            .map_err(|e| ControlPlaneError::Http(format!("fsync journal: {e}")))?;
        let mut checkpoint_dirty = false;
        if event.seq > 0 {
            self.latest_sequences.insert(event.session_id.clone(), event.seq);
            checkpoint_dirty = true;
        }
        if track_source(&event, &mut self.source_watermarks) {
            checkpoint_dirty = true;
        }
        if checkpoint_dirty {
            self.persist_sequences()?;
        }
        self.pending.push(event);
        Ok(())
    }

    /// Marks all pending events as delivered (drops them from the outbox).
    pub fn ack_all(&mut self) {
        self.pending.clear();
    }

    /// Rewrites the journal with only the still-pending events (compaction).
    pub fn rewrite_pending(&mut self) -> ControlPlaneResult<()> {
        let mut buf = Vec::new();
        for ev in &self.pending {
            let payload =
                serde_json::to_vec(ev).map_err(|e| ControlPlaneError::Http(e.to_string()))?;
            buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            buf.extend_from_slice(&payload);
        }
        std::fs::write(&self.path, &buf)
            .map_err(|e| ControlPlaneError::Http(format!("rewrite journal: {e}")))?;
        Ok(())
    }

    fn load(&mut self) -> ControlPlaneResult<()> {
        if let Ok(mut file) = std::fs::File::open(&self.path) {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| ControlPlaneError::Http(format!("read journal: {e}")))?;
            let mut cursor = 0usize;
            while cursor + 4 <= bytes.len() {
                let len = u32::from_be_bytes([
                    bytes[cursor],
                    bytes[cursor + 1],
                    bytes[cursor + 2],
                    bytes[cursor + 3],
                ]) as usize;
                cursor += 4;
                if cursor + len > bytes.len() || len > MAX_RECORD_BYTES {
                    break;
                }
                if let Ok(ev) = serde_json::from_slice::<SessionEvent>(&bytes[cursor..cursor + len]) {
                    if ev.seq > 0 {
                        self.latest_sequences.insert(ev.session_id.clone(), ev.seq);
                    }
                    track_source(&ev, &mut self.source_watermarks);
                    self.pending.push(ev);
                }
                cursor += len;
            }
        }
        if let Ok(text) = std::fs::read_to_string(&self.sequence_path) {
            for line in text.lines() {
                if let Some((sid, seq)) = line.rsplit_once('\t') {
                    let Ok(n) = seq.parse::<i64>() else {
                        continue;
                    };
                    if let Some(source) = sid.strip_prefix(NATIVE_WATERMARK_PREFIX) {
                        source_watermarks_merge(&mut self.source_watermarks, source, n);
                    } else {
                        self.latest_sequences.insert(sid.to_string(), n);
                    }
                }
            }
        }
        Ok(())
    }

    fn persist_sequences(&self) -> ControlPlaneResult<()> {
        let mut out = String::new();
        for (sid, seq) in &self.latest_sequences {
            out.push_str(&format!("{sid}\t{seq}\n"));
        }
        for (source, seq) in &self.source_watermarks {
            out.push_str(&format!("{NATIVE_WATERMARK_PREFIX}{source}\t{seq}\n"));
        }
        std::fs::write(&self.sequence_path, out)
            .map_err(|e| ControlPlaneError::Http(format!("write sequences: {e}")))?;
        Ok(())
    }
}

/// Merges one native source watermark, keeping the maximum.
///
/// Mirrors Java `sourceWatermarks.merge(key, seq, Math::max)`.
fn source_watermarks_merge(watermarks: &mut HashMap<String, i64>, source: &str, seq: i64) {
    let entry = watermarks.entry(source.to_string()).or_insert(0);
    if seq > *entry {
        *entry = seq;
    }
}

/// Tracks the native envelope carried in `framework_meta`; returns whether it advanced.
///
/// Mirrors Java `EventJournal.trackSource`: legacy metadata that is not a native envelope is
/// ignored, never treated as an error.
fn track_source(event: &SessionEvent, watermarks: &mut HashMap<String, i64>) -> bool {
    let Some(meta) = event.framework_meta.as_ref() else {
        return false;
    };
    let Some(source) = meta.get("native_source").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(seq) = meta.get("native_seq").and_then(|v| v.as_i64()) else {
        return false;
    };
    let before = watermarks.get(source).copied().unwrap_or(0);
    source_watermarks_merge(watermarks, source, seq);
    watermarks.get(source).copied().unwrap_or(0) > before
}

fn default_journal_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agentscope").join("service-controlplane").join("event-journal")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::session_event::{SessionEvent, TYPE_MESSAGE};

    fn temp_config(dir: &str) -> ControlPlaneConfig {
        ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .event_journal_dir(dir)
            .build()
            .unwrap()
    }

    #[test]
    fn append_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("controlplane-journal-{}", uuid::Uuid::new_v4()));
        let cfg = temp_config(dir.to_str().unwrap());
        {
            let mut j = EventJournal::new(&cfg).unwrap();
            let mut ev = SessionEvent::builder("s1", TYPE_MESSAGE).content("hi").build();
            ev.set_seq(7);
            j.append(ev).unwrap();
            assert_eq!(j.pending_len(), 1);
            assert_eq!(j.latest_sequence("s1"), 7);
        }
        {
            let j = EventJournal::new(&cfg).unwrap();
            assert_eq!(j.pending_len(), 1);
            assert_eq!(j.latest_sequence("s1"), 7);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ack_all_clears_pending() {
        let dir = std::env::temp_dir().join(format!("controlplane-journal-{}", uuid::Uuid::new_v4()));
        let cfg = temp_config(dir.to_str().unwrap());
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(SessionEvent::builder("s1", TYPE_MESSAGE).content("a").build())
            .unwrap();
        j.ack_all();
        assert_eq!(j.pending_len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
