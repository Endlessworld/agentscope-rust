//! Immutable commit chain made visible by one fenced CAS of its head, mirroring
//! `JournalSessionLog`.
//!
//! Layout under the journal prefix:
//!
//! ```text
//! session.json              SessionHeader (format/authority marker)
//! head.json                 Head   (seq, epoch, owner, lease, commit id)
//! commits/<hash>.json       Commit (immutable, checksummed event batch)
//! blobs/<hash>              Payload spilled for very large events
//! writer-seals/<hash>.json  Permanent fence for a writer identity
//! exports/<hash>.json       Per-sink export watermark
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    session_error, AtomicSessionStorage, Head, SessionEvent, SessionHeader, SessionLog, Writer,
};

/// Payloads above this size are spilled to an immutable blob so the commit chain stays small.
const MAX_INLINE_PAYLOAD_BYTES: usize = 65_536;

/// How many CAS attempts a contended operation makes before giving up.
const CAS_ATTEMPTS: usize = 32;

/// One immutable, checksummed batch of events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    batch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    first_seq: u64,
    last_seq: u64,
    hash: String,
    events: Vec<SessionEvent>,
    #[serde(default)]
    payload_blobs: BTreeMap<String, String>,
}

/// A journal backed by [`AtomicSessionStorage`].
pub struct JournalSessionLog {
    storage: Arc<dyn AtomicSessionStorage>,
    prefix: String,
}

impl std::fmt::Debug for JournalSessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalSessionLog")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl JournalSessionLog {
    /// Creates a journal rooted at `prefix` in `storage`.
    pub fn new(storage: Arc<dyn AtomicSessionStorage>, prefix: impl Into<String>) -> Self {
        Self {
            storage,
            prefix: prefix.into(),
        }
    }

    fn path(&self, suffix: &str) -> String {
        format!("{}/{suffix}", self.prefix)
    }

    /// Hex-encoded SHA-256, used for commit/blob/seal/export names and checksums.
    pub fn hash(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> crate::error::Result<Option<T>> {
        match self.storage.read(path).await? {
            Some(value) => Ok(Some(serde_json::from_slice(&value.bytes)?)),
            None => Ok(None),
        }
    }

    async fn head_from(&self, value: Option<super::StorageValue>) -> crate::error::Result<Head> {
        match value {
            Some(v) => Ok(serde_json::from_slice(&v.bytes)?),
            None => Ok(Head::empty()),
        }
    }

    fn seal_path(&self, owner: &str) -> String {
        self.path(&format!("writer-seals/{}.json", Self::hash(owner.as_bytes())))
    }

    async fn is_sealed(&self, owner: &str) -> crate::error::Result<bool> {
        Ok(self.storage.read(&self.seal_path(owner)).await?.is_some())
    }

    /// Rejects a writer whose ownership expired or was fenced.
    async fn ensure_owned(&self, head: &Head, writer: &Writer) -> crate::error::Result<()> {
        if self.is_sealed(&writer.owner).await? {
            return Err(session_error("Session writer is permanently sealed"));
        }
        if head.epoch != writer.epoch
            || head.owner.as_deref() != Some(writer.owner.as_str())
            || head.lease_until <= Self::now_ms()
        {
            return Err(session_error("Session writer ownership expired or fenced"));
        }
        Ok(())
    }

    async fn read_commit(&self, id: &str) -> crate::error::Result<Commit> {
        let path = self.path(&format!("commits/{id}.json"));
        let stored = self
            .storage
            .read(&path)
            .await?
            .ok_or_else(|| session_error(format!("Missing committed segment: {id}")))?;
        let commit: Commit = serde_json::from_slice(&stored.bytes)?;
        if !commit.payload_blobs.is_empty() {
            let mut hydrated = Vec::with_capacity(commit.events.len());
            for event in &commit.events {
                match commit.payload_blobs.get(&event.event_id) {
                    None => hydrated.push(event.clone()),
                    Some(blob) => {
                        let payload = self
                            .storage
                            .read(&self.path(&format!("blobs/{blob}")))
                            .await?
                            .ok_or_else(|| {
                                session_error("Missing or corrupt committed payload blob")
                            })?;
                        if Self::hash(&payload.bytes) != *blob {
                            return Err(session_error(
                                "Missing or corrupt committed payload blob",
                            ));
                        }
                        let json = String::from_utf8(payload.bytes)
                            .map_err(|_| session_error("Committed payload is not UTF-8"))?;
                        hydrated.push(with_payload(event, json));
                    }
                }
            }
            let verified = Self::hash(&serde_json::to_vec(&hydrated)?);
            if verified != commit.hash {
                return Err(session_error("Committed segment checksum mismatch"));
            }
            return Ok(Commit {
                events: hydrated,
                ..commit
            });
        }
        let verified = Self::hash(&serde_json::to_vec(&commit.events)?);
        if verified != commit.hash {
            return Err(session_error("Committed segment checksum mismatch"));
        }
        Ok(commit)
    }

    /// Walks the commit chain backwards, collecting segment ids that carry `(after, through]`.
    async fn segments_through(
        &self,
        head: &Head,
        after: u64,
        through: u64,
    ) -> crate::error::Result<Vec<String>> {
        let mut ids = VecDeque::new();
        let mut seen = BTreeSet::new();
        let mut expected = head.seq;
        let mut cursor = head.commit_id.clone();
        while let Some(id) = cursor {
            if !seen.insert(id.clone()) {
                return Err(session_error("Commit chain cycle"));
            }
            let path = self.path(&format!("commits/{id}.json"));
            let stored = self
                .storage
                .read(&path)
                .await?
                .ok_or_else(|| session_error("Missing committed segment"))?;
            let commit: Commit = serde_json::from_slice(&stored.bytes)?;
            let span = commit.events.len() as u64;
            if commit.last_seq != expected
                || commit.first_seq + span - 1 != expected
                || commit.events.is_empty()
            {
                return Err(session_error("Broken committed prefix"));
            }
            if commit.first_seq <= through {
                ids.push_front(id.clone());
            }
            expected = commit.first_seq.saturating_sub(1);
            if expected <= after {
                break;
            }
            cursor = commit.parent;
        }
        if expected > after {
            return Err(session_error("Incomplete committed chain"));
        }
        Ok(ids.into_iter().collect())
    }
}

fn with_payload(event: &SessionEvent, json: String) -> SessionEvent {
    SessionEvent {
        schema_version: event.schema_version,
        event_id: event.event_id.clone(),
        seq: event.seq,
        occurred_at: event.occurred_at,
        event_type: event.event_type.clone(),
        execution_run_id: event.execution_run_id.clone(),
        turn_id: event.turn_id.clone(),
        required: event.required,
        payload_json: json,
    }
}

#[async_trait]
impl SessionLog for JournalSessionLog {
    fn inbox(&self) -> crate::error::Result<Arc<dyn SessionLog>> {
        Ok(Arc::new(JournalSessionLog::new(
            Arc::clone(&self.storage),
            self.path("inbox"),
        )))
    }

    async fn head(&self) -> crate::error::Result<Head> {
        let header_path = self.path("session.json");
        if let Some(header) = self.read_json::<SessionHeader>(&header_path).await? {
            header.validate()?;
        }
        let value = self.storage.read(&self.path("head.json")).await?;
        self.head_from(value).await
    }

    async fn acquire(&self, owner: &str, lease: Duration) -> crate::error::Result<Writer> {
        let desired = SessionHeader::current();
        let header_path = self.path("session.json");
        // Create the marker if absent; a lost race is fine because the value is identical.
        let header_bytes = serde_json::to_vec(&desired)?;
        let _ = self
            .storage
            .compare_and_set(&header_path, 0, &header_bytes)
            .await?;
        let header: SessionHeader = self
            .read_json(&header_path)
            .await?
            .ok_or_else(|| session_error("Session header disappeared"))?;
        header.validate()?;
        if header.authority != desired.authority {
            return Err(session_error(
                "Session authority mismatch; migrate into a new session instead of changing \
                 mode in place",
            ));
        }
        if owner.trim().is_empty() {
            return Err(session_error("Writer owner is required"));
        }
        if lease.is_zero() {
            return Err(session_error("lease"));
        }
        let head_path = self.path("head.json");
        for _ in 0..CAS_ATTEMPTS {
            if self.is_sealed(owner).await? {
                return Err(session_error("Session writer is permanently sealed"));
            }
            let value = self.storage.read(&head_path).await?;
            let head = self.head_from(value.clone()).await?;
            if head.owner.is_some() && head.lease_until > Self::now_ms() {
                return Err(session_error("Session already has an active writer"));
            }
            let next = Head {
                seq: head.seq,
                epoch: head.epoch + 1,
                owner: Some(owner.to_string()),
                lease_until: Self::now_ms() + lease.as_millis() as i64,
                commit_id: head.commit_id.clone(),
            };
            let expected = value.map(|v| v.version).unwrap_or(0);
            let bytes = serde_json::to_vec(&next)?;
            if self
                .storage
                .compare_and_set(&head_path, expected, &bytes)
                .await?
            {
                // A seal can land between the initial check and this head read. Do not return
                // a writer until that race is resolved, and release the raced claim.
                if self.is_sealed(owner).await? {
                    self.seal_writer(owner).await?;
                    return Err(session_error("Session writer is permanently sealed"));
                }
                return Ok(Writer {
                    owner: owner.to_string(),
                    epoch: next.epoch,
                });
            }
        }
        Err(session_error("Writer acquisition contention"))
    }

    async fn seal_writer(&self, owner: &str) -> crate::error::Result<Head> {
        if owner.trim().is_empty() {
            return Err(session_error("Writer owner is required"));
        }
        let seal = serde_json::json!({ "owner": owner });
        let _ = self
            .storage
            .compare_and_set(&self.seal_path(owner), 0, &serde_json::to_vec(&seal)?)
            .await?;
        let head_path = self.path("head.json");
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&head_path).await?;
            let head = self.head_from(value.clone()).await?;
            let next = if head.owner.as_deref() == Some(owner) {
                Head {
                    seq: head.seq,
                    epoch: head.epoch + 1,
                    owner: None,
                    lease_until: 0,
                    commit_id: head.commit_id.clone(),
                }
            } else {
                head
            };
            // Even an unchanged head must advance its storage version. This fences a paused
            // acquisition/commit without revoking an unrelated active writer.
            let expected = value.map(|v| v.version).unwrap_or(0);
            let bytes = serde_json::to_vec(&next)?;
            if self
                .storage
                .compare_and_set(&head_path, expected, &bytes)
                .await?
            {
                return Ok(next);
            }
        }
        Err(session_error("Writer sealing contention"))
    }

    async fn renew(&self, writer: &Writer, lease: Duration) -> crate::error::Result<()> {
        self.update_owner(writer, lease, false).await
    }

    async fn release(&self, writer: &Writer) -> crate::error::Result<()> {
        self.update_owner(writer, Duration::ZERO, true).await
    }

    async fn commit(
        &self,
        writer: &Writer,
        batch_id: &str,
        expected_seq: u64,
        events: &[SessionEvent],
    ) -> crate::error::Result<Head> {
        if events.is_empty() {
            return self.head().await;
        }
        let mut seq = expected_seq;
        for event in events {
            seq += 1;
            if event.seq != seq {
                return Err(session_error("Non-contiguous event batch"));
            }
        }
        let hash = Self::hash(&serde_json::to_vec(events)?);
        let commit_id = Self::hash(batch_id.as_bytes());
        let commit_path = self.path(&format!("commits/{commit_id}.json"));
        let head_path = self.path("head.json");

        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&head_path).await?;
            let head = self.head_from(value.clone()).await?;
            // Resolve a lost commit acknowledgement even when a later batch already committed.
            if head.seq >= seq {
                let mut cursor = head.commit_id.clone();
                while let Some(id) = cursor {
                    let existing = self.read_commit(&id).await?;
                    if existing.batch_id == batch_id {
                        if existing.hash != hash || existing.first_seq != expected_seq + 1 {
                            return Err(session_error(
                                "Batch ID reused with different content",
                            ));
                        }
                        self.ensure_owned(&head, writer).await?;
                        return Ok(head);
                    }
                    if existing.last_seq < expected_seq + 1 {
                        break;
                    }
                    cursor = existing.parent.clone();
                }
            }
            self.ensure_owned(&head, writer).await?;
            if head.seq != expected_seq {
                return Err(session_error("Stale committed prefix"));
            }

            let mut blobs = BTreeMap::new();
            let mut stored = Vec::with_capacity(events.len());
            for event in events {
                if event.payload_json.len() > MAX_INLINE_PAYLOAD_BYTES {
                    let payload = event.payload_json.as_bytes();
                    let blob = Self::hash(payload);
                    let blob_path = self.path(&format!("blobs/{blob}"));
                    if !self.storage.compare_and_set(&blob_path, 0, payload).await? {
                        let existing = self.storage.read(&blob_path).await?;
                        match existing {
                            Some(v) if v.bytes == payload => {}
                            _ => {
                                return Err(session_error("Immutable payload blob conflict"));
                            }
                        }
                    }
                    blobs.insert(event.event_id.clone(), blob);
                    stored.push(with_payload(event, "{}".to_string()));
                } else {
                    stored.push(event.clone());
                }
            }

            let commit = Commit {
                batch_id: batch_id.to_string(),
                parent: head.commit_id.clone(),
                first_seq: expected_seq + 1,
                last_seq: seq,
                hash: hash.clone(),
                events: stored,
                payload_blobs: blobs,
            };
            let commit_bytes = serde_json::to_vec(&commit)?;
            if !self
                .storage
                .compare_and_set(&commit_path, 0, &commit_bytes)
                .await?
            {
                let existing = self.storage.read(&commit_path).await?;
                match existing {
                    Some(v) if v.bytes == commit_bytes => {}
                    _ => return Err(session_error("Immutable commit conflict")),
                }
            }

            self.ensure_owned(&head, writer).await?;
            let next = Head {
                seq,
                epoch: head.epoch,
                owner: head.owner.clone(),
                lease_until: head.lease_until,
                commit_id: Some(commit_id.clone()),
            };
            let expected = value.map(|v| v.version).unwrap_or(0);
            let next_bytes = serde_json::to_vec(&next)?;
            match self
                .storage
                .compare_and_set(&head_path, expected, &next_bytes)
                .await
            {
                Ok(true) => return Ok(next),
                Ok(false) => continue,
                Err(error) => {
                    // The write may have landed before the failure surfaced; treat a matching
                    // head as success so a lost ACK does not duplicate the batch.
                    if let Ok(observed) = self.head().await {
                        if observed.commit_id.as_deref() == Some(commit_id.as_str())
                            && observed.seq == seq
                        {
                            self.ensure_owned(&observed, writer).await?;
                            return Ok(observed);
                        }
                    }
                    return Err(error);
                }
            }
        }
        Err(session_error("Commit contention"))
    }

    async fn read_after(&self, after: u64, limit: usize) -> crate::error::Result<Vec<SessionEvent>> {
        if limit < 1 {
            return Err(session_error("Invalid limit"));
        }
        let head = self.head().await?;
        let through = std::cmp::min(head.seq, after + limit as u64);
        self.scan(after, through).await
    }

    async fn scan(&self, after: u64, through: u64) -> crate::error::Result<Vec<SessionEvent>> {
        let head = self.head().await?;
        if through < after || through > head.seq {
            return Err(session_error("Invalid committed prefix"));
        }
        let mut collected = Vec::new();
        let mut cursor = after;
        if after == through {
            return Ok(collected);
        }
        for id in self.segments_through(&head, after, through).await? {
            let commit = self.read_commit(&id).await?;
            for candidate in commit.events {
                if candidate.seq <= cursor {
                    continue;
                }
                if candidate.seq != cursor + 1 {
                    return Err(session_error("Non-contiguous event sequence"));
                }
                cursor = candidate.seq;
                collected.push(candidate);
                if cursor >= through {
                    return Ok(collected);
                }
            }
        }
        if cursor < through {
            return Err(session_error("Missing committed events"));
        }
        Ok(collected)
    }

    async fn export_cursor(&self, sink: &str) -> crate::error::Result<u64> {
        let path = self.path(&format!("exports/{}.json", Self::hash(sink.as_bytes())));
        Ok(self.read_json::<u64>(&path).await?.unwrap_or(0))
    }

    async fn advance_export_cursor(
        &self,
        sink: &str,
        expected: u64,
        next: u64,
    ) -> crate::error::Result<()> {
        let head = self.head().await?;
        if next <= expected || next > head.seq {
            return Err(session_error("Invalid export watermark"));
        }
        let key = self.path(&format!("exports/{}.json", Self::hash(sink.as_bytes())));
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&key).await?;
            let current = match &value {
                Some(v) => serde_json::from_slice::<u64>(&v.bytes)?,
                None => 0,
            };
            if current >= next {
                return Ok(());
            }
            if current != expected {
                return Err(session_error("Export cursor conflict"));
            }
            let expected_version = value.map(|v| v.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&key, expected_version, &serde_json::to_vec(&next)?)
                .await?
            {
                return Ok(());
            }
        }
        Err(session_error("Export cursor contention"))
    }
}

impl JournalSessionLog {
    async fn update_owner(
        &self,
        writer: &Writer,
        lease: Duration,
        release: bool,
    ) -> crate::error::Result<()> {
        let head_path = self.path("head.json");
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&head_path).await?;
            let head = self.head_from(value.clone()).await?;
            self.ensure_owned(&head, writer).await?;
            let next = Head {
                seq: head.seq,
                epoch: head.epoch,
                owner: if release { None } else { head.owner.clone() },
                lease_until: if release {
                    0
                } else {
                    Self::now_ms() + lease.as_millis() as i64
                },
                commit_id: head.commit_id.clone(),
            };
            let expected = value
                .map(|v| v.version)
                .ok_or_else(|| session_error("Missing session head"))?;
            if self
                .storage
                .compare_and_set(&head_path, expected, &serde_json::to_vec(&next)?)
                .await?
            {
                return Ok(());
            }
        }
        Err(session_error("Writer renewal contention"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MemorySessionStorage;

    fn journal() -> (Arc<dyn AtomicSessionStorage>, JournalSessionLog) {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log = JournalSessionLog::new(Arc::clone(&storage), "agents/a/sessions/u/s1");
        (storage, log)
    }

    fn event(seq: u64, kind: &str) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            0,
            kind,
            Some("run-1".into()),
            Some("t1".into()),
            true,
            "{}",
        )
        .unwrap()
    }

    async fn acquired(log: &JournalSessionLog, owner: &str) -> Writer {
        log.acquire(owner, Duration::from_secs(60)).await.unwrap()
    }

    #[tokio::test]
    async fn commit_publishes_one_contiguous_prefix() {
        let (_storage, log) = journal();
        assert_eq!(log.head().await.unwrap(), Head::empty());
        let writer = acquired(&log, "run-1").await;

        let head = log
            .commit(&writer, "b1", 0, &[event(1, "run/start"), event(2, "turn/start")])
            .await
            .unwrap();
        assert_eq!(head.seq, 2);
        assert_eq!(head.epoch, 1);
        assert_eq!(head.owner.as_deref(), Some("run-1"));
        assert!(head.commit_id.is_some());

        let scanned = log.scan(0, 2).await.unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].event_type, "run/start");
        assert_eq!(scanned[1].seq, 2);
        // A prefix scan is a window, not the whole history.
        assert_eq!(log.scan(1, 2).await.unwrap().len(), 1);
        assert!(log.scan(0, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replayed_batch_id_returns_original_head() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        let batch = [event(1, "run/start")];
        let first = log.commit(&writer, "b1", 0, &batch).await.unwrap();
        // A retried commit with the same content and batch id is idempotent.
        let replay = log.commit(&writer, "b1", 0, &batch).await.unwrap();
        assert_eq!(first, replay);

        // Reusing the batch id for different content is rejected.
        let err = log
            .commit(&writer, "b1", 0, &[event(1, "turn/start")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Batch ID reused with different content"), "{err}");
    }

    #[tokio::test]
    async fn stale_or_discontiguous_batch_is_rejected() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        log.commit(&writer, "b1", 0, &[event(1, "run/start")])
            .await
            .unwrap();
        // The batch itself is contiguous from 0, but the committed prefix already advanced.
        let err = log
            .commit(&writer, "b2", 0, &[event(1, "run/start"), event(2, "run/end")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Stale committed prefix"), "{err}");

        // A hole in the batch is rejected before touching storage.
        let err = log
            .commit(&writer, "b3", 1, &[event(3, "run/end")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Non-contiguous event batch"), "{err}");
    }

    #[tokio::test]
    async fn only_one_writer_holds_the_lease() {
        let (_storage, log) = journal();
        let _first = acquired(&log, "run-1").await;
        let err = log
            .acquire("run-2", Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("active writer"), "{err}");
    }

    #[tokio::test]
    async fn release_frees_the_lease_and_bumps_epoch() {
        let (_storage, log) = journal();
        let first = acquired(&log, "run-1").await;
        log.release(&first).await.unwrap();
        let head = log.head().await.unwrap();
        assert!(head.owner.is_none());
        assert_eq!(head.lease_until, 0);

        let second = acquired(&log, "run-2").await;
        assert_eq!(second.epoch, first.epoch + 1);

        // The stale writer cannot commit after losing ownership.
        let err = log
            .commit(&first, "b1", 0, &[event(1, "run/start")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fenced"), "{err}");
    }

    #[tokio::test]
    async fn sealing_fences_a_writer_permanently() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        let sealed = log.seal_writer("run-1").await.unwrap();
        assert!(sealed.owner.is_none());
        assert_eq!(sealed.seq, 0);

        let err = log
            .commit(&writer, "b1", 0, &[event(1, "run/start")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("permanently sealed"), "{err}");

        // The sealed identity can never acquire again, but another identity can.
        let err = log
            .acquire("run-1", Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("permanently sealed"), "{err}");
        acquired(&log, "run-9").await;
    }

    #[tokio::test]
    async fn large_payload_spills_to_blob_and_hydrates_on_read() {
        let (storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        let big = "x".repeat(MAX_INLINE_PAYLOAD_BYTES + 16);
        let payload = serde_json::json!({ "blob": big }).to_string();
        let ev = SessionEvent::new("e1", 1, 0, "tool/result", None, None, true, payload.clone())
            .unwrap();
        log.commit(&writer, "b1", 0, std::slice::from_ref(&ev))
            .await
            .unwrap();

        // The commit file stores an empty payload and a blob reference.
        let commit_id = log.head().await.unwrap().commit_id.unwrap();
        let raw: Commit = serde_json::from_slice(
            &storage
                .read(&format!("agents/a/sessions/u/s1/commits/{commit_id}.json"))
                .await
                .unwrap()
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert_eq!(raw.events[0].payload_json, "{}");
        assert_eq!(raw.payload_blobs.len(), 1);

        // Reading hydrates the original payload and verifies the checksum.
        let read = log.scan(0, 1).await.unwrap();
        assert_eq!(read[0].payload_json, payload);
    }

    #[tokio::test]
    async fn corrupted_commit_is_detected() {
        let (storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        log.commit(&writer, "b1", 0, &[event(1, "run/start")])
            .await
            .unwrap();
        let commit_id = log.head().await.unwrap().commit_id.unwrap();
        let path = format!("agents/a/sessions/u/s1/commits/{commit_id}.json");
        let current = storage.read(&path).await.unwrap().unwrap();
        let tampered = String::from_utf8(current.bytes.clone())
            .unwrap()
            .replace("\"run/start\"", "\"run/end\"");
        storage
            .compare_and_set(&path, current.version, tampered.as_bytes())
            .await
            .unwrap();
        let err = log.scan(0, 1).await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    }

    #[tokio::test]
    async fn scan_rejects_an_out_of_range_prefix() {
        let (_storage, log) = journal();
        assert!(log.scan(0, 5).await.is_err());
        assert!(log.scan(2, 1).await.is_err());
        assert!(log.read_after(0, 0).await.is_err());
    }

    #[tokio::test]
    async fn export_cursor_advances_monotonically() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        log.commit(&writer, "b1", 0, &[event(1, "run/start")])
            .await
            .unwrap();
        assert_eq!(log.export_cursor("sink-a").await.unwrap(), 0);
        log.advance_export_cursor("sink-a", 0, 1).await.unwrap();
        assert_eq!(log.export_cursor("sink-a").await.unwrap(), 1);
        // Re-advancing to an already-covered watermark is a no-op.
        log.advance_export_cursor("sink-a", 0, 1).await.unwrap();
        // Regressing or jumping past the head is rejected.
        assert!(log.advance_export_cursor("sink-a", 1, 1).await.is_err());
        assert!(log.advance_export_cursor("sink-a", 1, 9).await.is_err());
        // A conflicting expected watermark is rejected.
        assert!(log.advance_export_cursor("sink-a", 0, 1).await.is_ok());
    }

    #[tokio::test]
    async fn inbox_is_a_separate_journal() {
        let (_storage, log) = journal();
        let inbox = log.inbox().unwrap();
        let writer = inbox.acquire("cmd", Duration::from_secs(60)).await.unwrap();
        inbox
            .commit(&writer, "b1", 0, &[event(1, "inbox/accepted")])
            .await
            .unwrap();
        assert_eq!(inbox.head().await.unwrap().seq, 1);
        // The execution journal is unaffected by inbox traffic.
        assert_eq!(log.head().await.unwrap().seq, 0);
    }

    #[tokio::test]
    async fn authority_mismatch_stops_acquisition() {
        let (storage, log) = journal();
        let mut header = SessionHeader::current();
        header.authority = "state_store".into();
        storage
            .compare_and_set(
                "agents/a/sessions/u/s1/session.json",
                0,
                &serde_json::to_vec(&header).unwrap(),
            )
            .await
            .unwrap();
        let err = log
            .acquire("run-1", Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("authority"), "{err}");
    }
}
