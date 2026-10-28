//! Crash-safe local outbox for Level-2 events, mirroring `EventJournal`.
//!
//! Records are length-prefixed (u32 big-endian) JSON blobs appended to a per-identity
//! file, fsync'd on every write. The Java version stores protobuf `SessionEventMsg`;
//! the Rust port stores [`SessionEvent`] JSON, which is the same wire-agnostic payload.
//! Callers serialize access.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
        std::fs::create_dir_all(&root).map_err(|e| {
            ControlPlaneError::Config(format!("create journal dir {:?}: {e}", root))
        })?;
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
        let payload =
            serde_json::to_vec(&event).map_err(|e| ControlPlaneError::Http(e.to_string()))?;
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
            self.latest_sequences
                .insert(event.session_id.clone(), event.seq);
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

    /// Drops the pending events the control plane has committed, per session.
    ///
    /// `committed` maps a session id to the last sequence the control plane persisted;
    /// only records whose `seq` is **at or below** that floor are dropped. Everything else
    /// — later records of an acknowledged session, records of sessions absent from the map,
    /// and every record when the map is empty — stays pending. Mirrors
    /// `EventJournal.acknowledge(Map<String, Integer>)`:
    ///
    /// * an empty map is a no-op (it does not even touch the checkpoint),
    /// * the sequence checkpoint is persisted **before** any record is dropped, so a crash
    ///   in between can only cause a safe, idempotent replay,
    /// * the rewrite goes through [`write_atomic`], so the journal on disk is either the old
    ///   or the trimmed record set, never a torn prefix.
    ///
    /// Unlike Java, the in-memory backlog is only updated once the rewrite succeeded, so a
    /// failed rewrite keeps the batch replayable instead of dropping it in memory.
    pub fn acknowledge(&mut self, committed: &BTreeMap<String, i64>) -> ControlPlaneResult<()> {
        if committed.is_empty() {
            return Ok(());
        }
        let survivors: Vec<SessionEvent> = self
            .pending
            .iter()
            .filter(|event| match committed.get(&event.session_id) {
                Some(floor) => event.seq > *floor,
                None => true,
            })
            .cloned()
            .collect();
        let records = encode_records(&survivors)?;
        self.persist_sequences()?;
        write_atomic(&self.path, &records)
            .map_err(|e| ControlPlaneError::Http(format!("acknowledge journal: {e}")))?;
        self.pending = survivors;
        Ok(())
    }

    /// Rewrites the journal with only the still-pending events (compaction).
    pub fn rewrite_pending(&mut self) -> ControlPlaneResult<()> {
        let records = encode_records(&self.pending)?;
        write_atomic(&self.path, &records)
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
                if let Ok(ev) = serde_json::from_slice::<SessionEvent>(&bytes[cursor..cursor + len])
                {
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
        write_atomic(&self.sequence_path, out.as_bytes())
            .map_err(|e| ControlPlaneError::Http(format!("write sequences: {e}")))
    }
}

/// Encodes events as the on-disk length-prefixed JSON record stream.
fn encode_records(events: &[SessionEvent]) -> ControlPlaneResult<Vec<u8>> {
    let mut buf = Vec::new();
    for event in events {
        let payload =
            serde_json::to_vec(event).map_err(|e| ControlPlaneError::Http(e.to_string()))?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(ControlPlaneError::Config(format!(
                "event record exceeds {MAX_RECORD_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
    }
    Ok(buf)
}

/// Writes `bytes` to `path` through a sibling `.tmp` file, fsync, then an atomic rename.
///
/// Mirrors Java `EventJournal.rewrite()` / `persistSequences()`, so a crash mid-write never
/// leaves a torn journal behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> ControlPlaneResult<()> {
    let temp = temp_sibling(path);
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)
            .map_err(|e| ControlPlaneError::Http(format!("open {:?}: {e}", temp)))?;
        file.write_all(bytes)
            .map_err(|e| ControlPlaneError::Http(format!("write {:?}: {e}", temp)))?;
        file.sync_all()
            .map_err(|e| ControlPlaneError::Http(format!("fsync {:?}: {e}", temp)))?;
    }
    std::fs::rename(&temp, path)
        .map_err(|e| ControlPlaneError::Http(format!("rename {:?}: {e}", path)))?;
    Ok(())
}

/// `dir/name` → `dir/name.tmp`, matching Java `resolveSibling(getFileName() + ".tmp")`.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp"))
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
    PathBuf::from(home)
        .join(".agentscope")
        .join("service-controlplane")
        .join("event-journal")
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
        let dir =
            std::env::temp_dir().join(format!("controlplane-journal-{}", uuid::Uuid::new_v4()));
        let cfg = temp_config(dir.to_str().unwrap());
        {
            let mut j = EventJournal::new(&cfg).unwrap();
            let mut ev = SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("hi")
                .build();
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
        let dir =
            std::env::temp_dir().join(format!("controlplane-journal-{}", uuid::Uuid::new_v4()));
        let cfg = temp_config(dir.to_str().unwrap());
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(
            SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("a")
                .build(),
        )
        .unwrap();
        j.ack_all();
        assert_eq!(j.pending_len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn event(session: &str, seq: i64) -> SessionEvent {
        let mut ev = SessionEvent::builder(session, TYPE_MESSAGE)
            .content(format!("{session}-{seq}"))
            .build();
        ev.set_seq(seq);
        ev
    }

    fn temp_journal() -> (PathBuf, ControlPlaneConfig) {
        let dir =
            std::env::temp_dir().join(format!("controlplane-journal-{}", uuid::Uuid::new_v4()));
        let cfg = temp_config(dir.to_str().unwrap());
        (dir, cfg)
    }

    fn pending_seqs(journal: &EventJournal) -> Vec<(String, i64)> {
        journal
            .pending()
            .iter()
            .map(|ev| (ev.session_id.clone(), ev.seq))
            .collect()
    }

    /// Interleaved sessions: only the acknowledged session loses records up to its floor;
    /// the later records of that same session and every other session stay pending.
    #[test]
    fn acknowledge_drops_only_records_at_or_below_the_session_floor() {
        let (dir, cfg) = temp_journal();
        let mut j = EventJournal::new(&cfg).unwrap();
        for (session, seq) in [("s1", 1), ("s2", 1), ("s1", 2), ("s2", 2), ("s1", 3)] {
            j.append(event(session, seq)).unwrap();
        }
        let committed = BTreeMap::from([("s1".to_string(), 2i64)]);
        j.acknowledge(&committed).unwrap();
        assert_eq!(
            pending_seqs(&j),
            vec![
                ("s2".to_string(), 1),
                ("s2".to_string(), 2),
                ("s1".to_string(), 3),
            ]
        );
        // The sequence checkpoint is unaffected by trimming the outbox.
        assert_eq!(j.latest_sequence("s1"), 3);
        assert_eq!(j.latest_sequence("s2"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session that is absent from the committed map must never be dropped.
    #[test]
    fn acknowledge_ignores_sessions_absent_from_the_committed_map() {
        let (dir, cfg) = temp_journal();
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(event("s1", 1)).unwrap();
        j.append(event("s2", 1)).unwrap();
        j.acknowledge(&BTreeMap::from([("ghost".to_string(), 99i64)]))
            .unwrap();
        assert_eq!(j.pending_len(), 2);
        assert_eq!(
            pending_seqs(&j),
            vec![("s1".to_string(), 1), ("s2".to_string(), 1)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty committed map is a no-op: same backlog, same bytes on disk.
    #[test]
    fn acknowledge_with_an_empty_map_is_a_noop() {
        let (dir, cfg) = temp_journal();
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(event("s1", 1)).unwrap();
        let before = std::fs::read(&j.path).unwrap();
        j.acknowledge(&BTreeMap::new()).unwrap();
        assert_eq!(j.pending_len(), 1);
        assert_eq!(std::fs::read(&j.path).unwrap(), before);
        // A no-op must not rewrite files either (Java returns before persisting).
        assert!(!temp_sibling(&j.path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Partially acknowledged journals replay exactly the un-acknowledged tail after restart.
    #[test]
    fn acknowledged_records_do_not_replay_after_reopen() {
        let (dir, cfg) = temp_journal();
        {
            let mut j = EventJournal::new(&cfg).unwrap();
            j.append(event("s1", 1)).unwrap();
            j.append(event("s1", 2)).unwrap();
            j.append(event("s1", 3)).unwrap();
            j.append(event("s2", 1)).unwrap();
            j.acknowledge(&BTreeMap::from([("s1".to_string(), 2i64)]))
                .unwrap();
        }
        let reopened = EventJournal::new(&cfg).unwrap();
        assert_eq!(
            pending_seqs(&reopened),
            vec![("s1".to_string(), 3), ("s2".to_string(), 1)]
        );
        assert_eq!(reopened.latest_sequence("s1"), 3);
        // A second ack of the same floor leaves the surviving tail alone.
        let mut reopened = reopened;
        reopened
            .acknowledge(&BTreeMap::from([("s1".to_string(), 2i64)]))
            .unwrap();
        assert_eq!(reopened.pending_len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rewrite is atomic: the temp file is renamed, never left behind, and the on-disk
    /// record stream still decodes to exactly the survivors.
    #[test]
    fn acknowledge_rewrites_the_journal_atomically() {
        let (dir, cfg) = temp_journal();
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(event("s1", 1)).unwrap();
        j.append(event("s1", 2)).unwrap();
        j.append(event("s2", 1)).unwrap();
        j.acknowledge(&BTreeMap::from([("s1".to_string(), 1i64)]))
            .unwrap();
        assert!(!temp_sibling(&j.path).exists());
        let encoded = encode_records(j.pending()).unwrap();
        assert_eq!(std::fs::read(&j.path).unwrap(), encoded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed rewrite must not drop the backlog from memory: the batch stays replayable.
    #[test]
    fn acknowledge_keeps_the_backlog_when_the_rewrite_fails() {
        let (dir, cfg) = temp_journal();
        let mut j = EventJournal::new(&cfg).unwrap();
        j.append(event("s1", 1)).unwrap();
        j.append(event("s1", 2)).unwrap();
        // Block the temp sibling with a directory so the atomic rewrite cannot open it.
        std::fs::create_dir_all(temp_sibling(&j.path)).unwrap();
        let err = j
            .acknowledge(&BTreeMap::from([("s1".to_string(), 2i64)]))
            .unwrap_err();
        assert!(err.to_string().contains("acknowledge journal"), "{err}");
        assert_eq!(j.pending_len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default outbox lives under the renamed service-controlplane directory.
    #[test]
    fn default_journal_dir_matches_the_control_plane_layout() {
        let dir = default_journal_dir();
        let tail: Vec<String> = dir
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            tail,
            vec!["event-journal", "service-controlplane", ".agentscope"]
        );
    }
}
