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
//! snapshots/<hash>.json     SessionSnapshot (immutable fold of a prefix)
//! snapshot.json             SnapshotPointer (latest published snapshot)
//! snapshot.previous.json    SnapshotPointer (previous one: retention floor)
//! reclaimed.json            Lowest sequence number still readable, once history was deleted
//! writer-seals/<hash>.json  Permanent fence for a writer identity
//! exports/<hash>.json       Per-sink export watermark
//! ```

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::snapshot::{
    hash_hex, object_path, SessionRetention, SessionSnapshot, SnapshotPointer, SNAPSHOT_FORMAT,
    SNAPSHOT_POINTER, SNAPSHOT_PREVIOUS,
};
use super::{
    session_error, AtomicSessionStorage, Head, SessionEvent, SessionHeader, SessionLog, Writer,
};

/// Payloads above this size are spilled to an immutable blob so the commit chain stays small.
const MAX_INLINE_PAYLOAD_BYTES: usize = 65_536;

/// How many CAS attempts a contended operation makes before giving up.
const CAS_ATTEMPTS: usize = 32;

/// Name of the marker recording how far history was reclaimed.
const RECLAIMED: &str = "reclaimed.json";

/// Commits this far behind the last snapshot are eligible for reclaiming.
const PRUNE_MIN_EVENTS: u64 = 1024;

/// Upper bound on commit files one reclaim pass examines, so a huge journal never turns a
/// flush into an unbounded scan.
const PRUNE_MAX_SCAN: usize = 4096;

/// Deleted commits per reclaim pass; the rest waits for the next snapshot.
const PRUNE_BATCH: usize = 512;

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
        self.path(&format!(
            "writer-seals/{}.json",
            Self::hash(owner.as_bytes())
        ))
    }

    async fn is_sealed(&self, owner: &str) -> crate::error::Result<bool> {
        Ok(self.storage.read(&self.seal_path(owner)).await?.is_some())
    }

    /// Rejects a writer whose ownership was fenced or sealed.
    ///
    /// An expired lease is **not** by itself a loss of ownership: the same owner/epoch may
    /// renew or release after a suspend, a clock jump, a long replay or a transient storage
    /// stall. Ownership is only lost when the epoch/owner changed, or the writer was sealed.
    async fn ensure_owned(&self, head: &Head, writer: &Writer) -> crate::error::Result<()> {
        if self.is_sealed(&writer.owner).await? {
            return Err(session_error("Session writer is permanently sealed"));
        }
        if head.epoch != writer.epoch || head.owner.as_deref() != Some(writer.owner.as_str()) {
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
                            return Err(session_error("Missing or corrupt committed payload blob"));
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
    ///
    /// `tolerant` decides what an unreadable segment means. A strict caller (a scan that must
    /// return exactly `(after, through]`) fails. A replay caller gets the readable *suffix* plus
    /// `truncated = true`: one corrupt segment must not make a whole session unopenable, and the
    /// newest commits — the ones that carry the latest checkpoint — are the ones worth keeping.
    /// Only data failures are tolerated; a transient storage error still surfaces, otherwise a
    /// network hiccup would look like history loss.
    ///
    /// `floor` is where reclaiming stopped: everything at or below it was deleted once a
    /// snapshot covered it, so the walk ends there instead of demanding a chain back to seq 0.
    /// The third return value is that floor whenever it actually cut the walk short.
    async fn segments_through(
        &self,
        head: &Head,
        after: u64,
        through: u64,
        tolerant: bool,
    ) -> crate::error::Result<(Vec<String>, bool, Option<u64>)> {
        let floor = self.reclaimed_floor().await?;
        let mut ids = VecDeque::new();
        let mut seen = BTreeSet::new();
        let mut expected = head.seq;
        let mut truncated = false;
        let mut reclaimed = None;
        let mut cursor = head.commit_id.clone();
        while let Some(id) = cursor {
            if !seen.insert(id.clone()) {
                if tolerant {
                    truncated = true;
                    break;
                }
                return Err(session_error("Commit chain cycle"));
            }
            let path = self.path(&format!("commits/{id}.json"));
            let parsed = match self.storage.read(&path).await? {
                Some(stored) => serde_json::from_slice::<Commit>(&stored.bytes)
                    .map_err(crate::error::Error::from),
                None => Err(session_error("Missing committed segment")),
            };
            let commit = match parsed {
                Ok(commit) => commit,
                Err(error) if tolerant && is_unreadable(&error) => {
                    tracing::warn!(
                        session = %self.prefix,
                        segment = %id,
                        seq = expected,
                        %error,
                        "unreadable committed segment; replaying only the readable suffix"
                    );
                    truncated = true;
                    break;
                }
                Err(error) => return Err(error),
            };
            let span = commit.events.len() as u64;
            if commit.last_seq != expected
                || commit.first_seq + span - 1 != expected
                || commit.events.is_empty()
            {
                if tolerant {
                    tracing::warn!(
                        session = %self.prefix,
                        segment = %id,
                        seq = expected,
                        first_seq = commit.first_seq,
                        last_seq = commit.last_seq,
                        "broken committed prefix; replaying only the readable suffix"
                    );
                    truncated = true;
                    break;
                }
                return Err(session_error("Broken committed prefix"));
            }
            if commit.first_seq <= through {
                ids.push_front(id.clone());
            }
            expected = commit.first_seq.saturating_sub(1);
            if let Some(floor) = floor {
                if commit.last_seq <= floor {
                    // This is the oldest commit left on chain: everything below it was reclaimed
                    // once the previous snapshot covered it.
                    reclaimed = Some(floor);
                    break;
                }
            }
            if expected <= after {
                break;
            }
            cursor = commit.parent;
        }
        if expected > after {
            if reclaimed.is_some() {
                if !tolerant {
                    return Err(session_error(format!(
                        "Session history at or before seq {} was reclaimed",
                        reclaimed.unwrap_or_default()
                    )));
                }
                truncated = true;
            } else if tolerant {
                truncated = true;
            } else {
                return Err(session_error("Incomplete committed chain"));
            }
        }
        Ok((ids.into_iter().collect(), truncated, reclaimed))
    }

    // ---------------------------------------------------------------- snapshots (P0-2)

    async fn read_pointer(&self, name: &str) -> crate::error::Result<Option<SnapshotPointer>> {
        let Some(stored) = self.storage.read(&self.path(name)).await? else {
            return Ok(None);
        };
        let pointer: SnapshotPointer = serde_json::from_slice(&stored.bytes)?;
        if pointer.format != SNAPSHOT_FORMAT {
            return Ok(None);
        }
        Ok(Some(pointer))
    }

    /// Reads and verifies the immutable object a pointer names.
    ///
    /// Both the checksum and the sequence number are checked: a pointer that survived into a
    /// copied or forked journal must not seed a replay of a different history.
    async fn read_snapshot_object(
        &self,
        pointer: &SnapshotPointer,
    ) -> crate::error::Result<SessionSnapshot> {
        let path = object_path(&self.prefix, &pointer.hash);
        let stored = self
            .storage
            .read(&path)
            .await?
            .ok_or_else(|| session_error("Missing published session snapshot"))?;
        if hash_hex(&stored.bytes) != pointer.hash {
            return Err(session_error(
                "Published session snapshot checksum mismatch",
            ));
        }
        let snapshot: SessionSnapshot = serde_json::from_slice(&stored.bytes)?;
        if snapshot.format != SNAPSHOT_FORMAT || snapshot.as_of_seq != pointer.seq {
            return Err(session_error(
                "Published session snapshot does not match its pointer",
            ));
        }
        Ok(snapshot)
    }

    /// The commit id that ends exactly at `seq`, walking back from the head.
    ///
    /// Only the commit headers are read, so this stays cheap even for a large journal.
    async fn commit_id_ending_at(&self, seq: u64) -> crate::error::Result<Option<String>> {
        let head = self.head().await?;
        if seq == 0 || seq > head.seq {
            return Ok(None);
        }
        let mut seen = BTreeSet::new();
        let mut cursor = head.commit_id.clone();
        while let Some(id) = cursor {
            if !seen.insert(id.clone()) {
                return Ok(None);
            }
            let path = self.path(&format!("commits/{id}.json"));
            let Some(stored) = self.storage.read(&path).await? else {
                return Ok(None);
            };
            let header: CommitHeader = serde_json::from_slice(&stored.bytes)?;
            if header.last_seq == seq {
                return Ok(Some(id));
            }
            if header.last_seq < seq {
                return Ok(None);
            }
            cursor = header.parent;
        }
        Ok(None)
    }

    /// Publishes a snapshot of the current head, then reclaims what the previous one covers.
    async fn publish_snapshot_locked(
        &self,
        writer: &Writer,
        retention: &SessionRetention,
    ) -> crate::error::Result<Option<u64>> {
        let head = self.head().await?;
        self.ensure_owned(&head, writer).await?;
        let Some(commit_id) = head.commit_id.clone() else {
            return Ok(None);
        };
        if head.seq == 0 {
            return Ok(None);
        }
        let latest = self.read_pointer(SNAPSHOT_POINTER).await?;
        if let Some(pointer) = &latest {
            if pointer.seq == head.seq {
                return Ok(Some(pointer.seq));
            }
        }

        // Fold the head. The replay starts from the previous snapshot, so this only walks the
        // commits published since — the whole point of keeping one.
        let previous = match &latest {
            Some(pointer) => match self.read_snapshot_object(pointer).await {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    tracing::warn!(
                        session = %self.prefix,
                        %error,
                        "the previous session snapshot is unreadable; rebuilding from the chain"
                    );
                    None
                }
            },
            None => None,
        };
        let mut runs = previous
            .as_ref()
            .map(|snapshot| snapshot.runs.clone())
            .unwrap_or_default();
        let mut seen_runs: HashSet<String> = runs.iter().cloned().collect();
        let mut collect = |event: &SessionEvent| -> crate::error::Result<()> {
            if let Some(run) = &event.execution_run_id {
                if seen_runs.insert(run.clone()) {
                    runs.push(run.clone());
                }
            }
            Ok(())
        };
        let views = super::views::replay_views(
            self as &dyn SessionLog,
            super::views::ViewSelection {
                turns: true,
                projection: true,
                pending: true,
                transcript: true,
            },
            &mut collect,
        )
        .await?;
        drop(collect);
        SessionSnapshot::trim_runs(&mut runs);

        let snapshot = SessionSnapshot {
            format: SNAPSHOT_FORMAT,
            as_of_seq: head.seq,
            as_of_commit_id: commit_id.clone(),
            turns: views.turns.unwrap_or_default(),
            projection: views.projection.unwrap_or_default(),
            pending: views.pending.unwrap_or_default(),
            transcript: views.transcript.unwrap_or_default(),
            runs,
        };
        let bytes = serde_json::to_vec(&snapshot)?;
        let hash = hash_hex(&bytes);
        let object = object_path(&self.prefix, &hash);
        // Immutable and idempotent: a retried publish rewrites identical bytes.
        if !self.storage.compare_and_set(&object, 0, &bytes).await? {
            let existing = self.storage.read(&object).await?;
            match existing {
                Some(value) if value.bytes == bytes => {}
                _ => return Err(session_error("Immutable session snapshot conflict")),
            }
        }

        // Rotate first: the previous pointer is what reclaiming trusts, and a crash between the
        // two writes only leaves both pointing at the same (valid) snapshot.
        if let Some(current) = &latest {
            let body = serde_json::to_vec(current)?;
            let path = self.path(SNAPSHOT_PREVIOUS);
            for _ in 0..CAS_ATTEMPTS {
                let value = self.storage.read(&path).await?;
                if let Some(stored) = &value {
                    let existing: SnapshotPointer = serde_json::from_slice(&stored.bytes)?;
                    if existing.seq >= current.seq {
                        break;
                    }
                }
                let expected = value.map(|value| value.version).unwrap_or(0);
                if self.storage.compare_and_set(&path, expected, &body).await? {
                    break;
                }
            }
        }

        let pointer = SnapshotPointer {
            format: SNAPSHOT_FORMAT,
            seq: head.seq,
            commit_id,
            hash,
            published_at: Self::now_ms(),
        };
        let pointer_bytes = serde_json::to_vec(&pointer)?;
        let pointer_path = self.path(SNAPSHOT_POINTER);
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&pointer_path).await?;
            if let Some(stored) = &value {
                let existing: SnapshotPointer = serde_json::from_slice(&stored.bytes)?;
                if existing.seq >= head.seq {
                    return Ok(Some(existing.seq));
                }
            }
            let expected = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&pointer_path, expected, &pointer_bytes)
                .await?
            {
                tracing::debug!(
                    session = %self.prefix,
                    seq = head.seq,
                    hash = %pointer.hash,
                    "published a session snapshot"
                );
                break;
            }
        }

        if let Some(previous) = &latest {
            if let Err(error) = self.reclaim(previous.seq, retention).await {
                // Reclaiming is housekeeping: a failed pass must never fail the run that
                // produced the facts, and the next snapshot tries again.
                tracing::warn!(session = %self.prefix, %error, "reclaiming session history failed");
            }
        }
        Ok(Some(head.seq))
    }

    /// Deletes commits (and their blobs) that an older published snapshot already covers.
    async fn reclaim(&self, floor: u64, retention: &SessionRetention) -> crate::error::Result<()> {
        if !retention.prune || floor == 0 {
            return Ok(());
        }
        let head = self.head().await?;
        if head.seq.saturating_sub(floor) < PRUNE_MIN_EVENTS {
            return Ok(());
        }
        let directory = self.path("commits");
        let paths = match self.storage.list_paths(&directory).await {
            Ok(paths) => paths,
            Err(error) => {
                tracing::debug!(
                    session = %self.prefix,
                    %error,
                    "session storage cannot list commits; skipping reclaim"
                );
                return Ok(());
            }
        };
        let prefix = format!("{directory}/");
        let enumerated: Vec<&String> = paths
            .iter()
            .filter(|path| path.starts_with(&prefix))
            .collect();
        // Blob collection is only safe when every remaining commit was examined.
        let complete = enumerated.len() <= PRUNE_MAX_SCAN;
        let oldest = Self::now_ms() - retention.min_age.as_millis() as i64;

        let mut candidates: Vec<(u64, String, BTreeMap<String, String>)> = Vec::new();
        let mut referenced: HashSet<String> = HashSet::new();
        for path in &enumerated {
            let Some(id) = path.rsplit('/').next().map(str::to_string) else {
                continue;
            };
            let commit = match self.read_commit_lite(path).await {
                Ok(commit) => commit,
                Err(error) => {
                    tracing::debug!(path = %path, %error, "skipping an unreadable commit while reclaiming");
                    continue;
                }
            };
            let batch_newest = commit
                .events
                .iter()
                .map(|event| event.occurred_at)
                .max()
                .unwrap_or(0);
            if commit.last_seq < floor && batch_newest <= oldest {
                candidates.push((commit.last_seq, id, commit.payload_blobs));
            } else {
                referenced.extend(commit.payload_blobs.into_values());
            }
        }
        candidates.sort_by_key(|(seq, _, _)| *seq);

        let mut deleted = 0usize;
        let mut freed_blobs: Vec<String> = Vec::new();
        for (seq, id, blobs) in candidates.into_iter().take(PRUNE_BATCH) {
            let path = format!("{prefix}{id}");
            match self.storage.remove(&path).await {
                Ok(_) => {
                    deleted += 1;
                    for blob in blobs.into_values() {
                        if !referenced.contains(&blob) {
                            freed_blobs.push(blob);
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(path = %path, seq, %error, "session commit could not be reclaimed");
                    break;
                }
            }
        }
        if deleted == 0 {
            return Ok(());
        }
        self.mark_reclaimed(floor).await?;
        if complete {
            self.reclaim_blobs(&freed_blobs).await?;
        }
        tracing::info!(
            session = %self.prefix,
            deleted,
            floor,
            "reclaimed session history covered by a snapshot"
        );
        Ok(())
    }

    async fn reclaim_blobs(&self, candidates: &[String]) -> crate::error::Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        let unique: BTreeSet<&String> = candidates.iter().collect();
        for blob in unique.into_iter().take(PRUNE_BATCH) {
            let path = self.path(&format!("blobs/{blob}"));
            if let Err(error) = self.storage.remove(&path).await {
                tracing::debug!(path = %path, %error, "session payload blob could not be reclaimed");
            }
        }
        Ok(())
    }

    /// Records how far history was reclaimed; the marker only moves forward.
    async fn mark_reclaimed(&self, floor: u64) -> crate::error::Result<()> {
        let path = self.path(RECLAIMED);
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&path).await?;
            if let Some(stored) = &value {
                let marker: ReclaimedMarker = serde_json::from_slice(&stored.bytes)?;
                if marker.floor >= floor {
                    return Ok(());
                }
            }
            let bytes = serde_json::to_vec(&ReclaimedMarker { floor })?;
            let expected = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&path, expected, &bytes)
                .await?
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Reads a commit without hydrating payload blobs.
    async fn read_commit_lite(&self, path: &str) -> crate::error::Result<CommitLite> {
        let stored = self
            .storage
            .read(path)
            .await?
            .ok_or_else(|| session_error("Missing committed segment"))?;
        Ok(serde_json::from_slice(&stored.bytes)?)
    }
}

/// The fields a reclaim pass needs; the event payloads stay unparsed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitHeader {
    last_seq: u64,
    #[serde(default)]
    parent: Option<String>,
}

/// Reclaim bookkeeping for one commit.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitLite {
    last_seq: u64,
    #[serde(default)]
    events: Vec<CommitLiteEvent>,
    #[serde(default)]
    payload_blobs: BTreeMap<String, String>,
}

/// Only the timestamp is needed to decide whether a batch is old enough to reclaim.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitLiteEvent {
    #[serde(default)]
    occurred_at: i64,
}

/// How far history was reclaimed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReclaimedMarker {
    floor: u64,
}

/// Whether a read failure means the bytes are unusable rather than temporarily unreachable.
///
/// Corrupt/missing committed data must not brick a session; an I/O or storage failure must
/// still surface, because replaying a partial history over a transient outage loses state.
fn is_unreadable(error: &crate::error::Error) -> bool {
    matches!(
        error,
        crate::error::Error::Session(_) | crate::error::Error::Json(_)
    )
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
                            return Err(session_error("Batch ID reused with different content"));
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

    async fn read_after(
        &self,
        after: u64,
        limit: usize,
    ) -> crate::error::Result<Vec<SessionEvent>> {
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
        let (ids, _truncated, reclaimed) =
            self.segments_through(&head, after, through, true).await?;
        if let Some(floor) = reclaimed {
            if after < floor {
                // Reclaiming deletes events; a caller that wants them cannot be served from the
                // journal any more. Saying so beats returning a prefix that silently starts
                // halfway through the history.
                return Err(session_error(format!(
                    "Session events at or before seq {floor} were reclaimed and cannot be read"
                )));
            }
        }
        for id in ids {
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

    /// Replays the readable prefix of `(after, through]`.
    ///
    /// Unlike [`scan`](SessionLog::scan) this never fails on a corrupt segment: it stops at the
    /// first unreadable commit and visits what it could read (normally the newest commits, which
    /// carry the latest checkpoint). Recovery must open a session whose history is damaged; a
    /// reader that refuses to replay at all leaves the user with no way back in.
    async fn visit(
        &self,
        after: u64,
        through: u64,
        visitor: &mut (dyn for<'a> FnMut(&'a SessionEvent) -> crate::error::Result<()> + Send),
    ) -> crate::error::Result<()> {
        let head = self.head().await?;
        if through < after || through > head.seq {
            return Err(session_error("Invalid committed prefix"));
        }
        if after == through {
            return Ok(());
        }
        let mut cursor = after;
        for id in self.segments_through(&head, after, through, true).await?.0 {
            // One commit at a time: its payload (a state checkpoint can be large) is
            // dropped before the next commit is read.
            let commit = match self.read_commit(&id).await {
                Ok(commit) => commit,
                Err(error) if is_unreadable(&error) => {
                    tracing::warn!(
                        session = %self.prefix,
                        segment = %id,
                        seq = cursor,
                        %error,
                        "unreadable committed segment; replay stops here"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            for candidate in commit.events {
                if candidate.seq <= cursor {
                    continue;
                }
                // A gap means the prefix below it is unreadable, not that the facts above it are
                // invalid: replay continues with the newest facts instead of refusing to open.
                if candidate.seq != cursor + 1 {
                    tracing::warn!(
                        session = %self.prefix,
                        expected = cursor + 1,
                        found = candidate.seq,
                        "gap in the committed prefix; replaying the readable suffix"
                    );
                }
                cursor = candidate.seq;
                visitor(&candidate)?;
                if cursor >= through {
                    return Ok(());
                }
            }
        }
        Ok(())
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

    /// Reads the published snapshot, degrading to a full replay when it is unusable.
    ///
    /// Corrupt bytes must not brick a session; a storage failure still surfaces.
    async fn snapshot(&self) -> crate::error::Result<Option<SessionSnapshot>> {
        let Some(pointer) = self.read_pointer(SNAPSHOT_POINTER).await? else {
            return Ok(None);
        };
        match self.read_snapshot_object(&pointer).await {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(error) => {
                tracing::warn!(
                    session = %self.prefix,
                    seq = pointer.seq,
                    %error,
                    "ignoring an unreadable published session snapshot"
                );
                Ok(None)
            }
        }
    }

    async fn commit_at(&self, seq: u64) -> crate::error::Result<Option<String>> {
        self.commit_id_ending_at(seq).await
    }

    async fn reclaimed_floor(&self) -> crate::error::Result<Option<u64>> {
        let Some(stored) = self.storage.read(&self.path(RECLAIMED)).await? else {
            return Ok(None);
        };
        let marker: ReclaimedMarker = serde_json::from_slice(&stored.bytes)?;
        Ok(Some(marker.floor))
    }

    async fn publish_snapshot(
        &self,
        writer: &Writer,
        retention: SessionRetention,
    ) -> crate::error::Result<Option<u64>> {
        self.publish_snapshot_locked(writer, &retention).await
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
            .commit(
                &writer,
                "b1",
                0,
                &[event(1, "run/start"), event(2, "turn/start")],
            )
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
        assert!(
            err.to_string()
                .contains("Batch ID reused with different content"),
            "{err}"
        );
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
            .commit(
                &writer,
                "b2",
                0,
                &[event(1, "run/start"), event(2, "run/end")],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Stale committed prefix"), "{err}");

        // A hole in the batch is rejected before touching storage.
        let err = log
            .commit(&writer, "b3", 1, &[event(3, "run/end")])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Non-contiguous event batch"),
            "{err}"
        );
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

    // ------------------------------------------------------------ snapshots / reclaiming

    /// A batch of assistant messages: they show up in the transcript, the turns view folds the
    /// rest, and each one is a separate committed fact.
    fn history_batch(start_seq: u64, count: u64) -> Vec<SessionEvent> {
        (1..=count)
            .map(|offset| {
                let seq = start_seq + offset;
                let mut message = crate::message::Msg::assistant(format!("m{seq}"));
                message.id = format!("m{seq}");
                SessionEvent::new(
                    format!("e{seq}"),
                    seq,
                    0,
                    "message/assistant",
                    Some("run-1".into()),
                    Some("t1".into()),
                    true,
                    serde_json::json!({ "message": message }).to_string(),
                )
                .unwrap()
            })
            .collect()
    }

    async fn commit_history(log: &JournalSessionLog, writer: &Writer, batches: usize, size: u64) {
        for index in 0..batches {
            let start = log.head().await.unwrap().seq;
            log.commit(
                writer,
                &format!("batch-{start}"),
                start,
                &history_batch(start, size),
            )
            .await
            .unwrap();
        }
    }

    async fn views_of(
        log: &JournalSessionLog,
    ) -> (
        crate::session::SessionProjection,
        crate::session::Transcript,
        crate::session::SessionTurns,
    ) {
        (
            crate::session::SessionProjection::read(log).await.unwrap(),
            crate::session::transcript(log).await.unwrap(),
            crate::session::SessionTurns::read(log).await.unwrap(),
        )
    }

    #[tokio::test]
    async fn a_snapshot_reads_the_same_views_as_a_full_replay() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        commit_history(&log, &writer, 3, 40).await;
        let before = views_of(&log).await;

        let published = log
            .publish_snapshot(&writer, SessionRetention::OFF)
            .await
            .unwrap();
        assert_eq!(published, Some(120));
        let snapshot = log.snapshot().await.unwrap().expect("snapshot published");
        assert_eq!(snapshot.as_of_seq, 120);
        assert_eq!(snapshot.transcript.messages.len(), 120);

        // The tail is what a reader walks; the head of the history comes from the snapshot.
        commit_history(&log, &writer, 1, 10).await;
        let after = views_of(&log).await;
        assert_eq!(after.0.as_of_seq, 130);
        assert_eq!(
            &after.0.uncertain_tool_calls,
            &before.0.uncertain_tool_calls
        );
        assert_eq!(after.1.messages.len(), 130);
        assert_eq!(after.2.turns.len(), before.2.turns.len());
        assert_eq!(after.1.messages[0].text_content(), "m1");
        assert_eq!(after.1.messages[129].text_content(), "m130");
    }

    #[tokio::test]
    async fn a_snapshot_from_another_chain_is_ignored() {
        let (storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        commit_history(&log, &writer, 2, 20).await;
        let before = views_of(&log).await;
        log.publish_snapshot(&writer, SessionRetention::OFF)
            .await
            .unwrap();

        // A pointer that names a commit which is not on this chain must not seed a replay.
        let pointer_path = "agents/a/sessions/u/s1/snapshot.json";
        let stored = storage.read(pointer_path).await.unwrap().unwrap();
        let mut pointer: SnapshotPointer = serde_json::from_slice(&stored.bytes).unwrap();
        pointer.commit_id =
            "0000000000000000000000000000000000000000000000000000000000000000".into();
        assert!(storage
            .compare_and_set(
                pointer_path,
                stored.version,
                &serde_json::to_vec(&pointer).unwrap()
            )
            .await
            .unwrap());

        assert_eq!(views_of(&log).await, before);
    }

    #[tokio::test]
    async fn a_corrupt_snapshot_object_falls_back_to_a_full_replay() {
        let (storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        commit_history(&log, &writer, 2, 20).await;
        let before = views_of(&log).await;
        let published = log
            .publish_snapshot(&writer, SessionRetention::OFF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(published, 40);

        let pointer: SnapshotPointer = serde_json::from_slice(
            &storage
                .read("agents/a/sessions/u/s1/snapshot.json")
                .await
                .unwrap()
                .unwrap()
                .bytes,
        )
        .unwrap();
        let object = object_path("agents/a/sessions/u/s1", &pointer.hash);
        let stored = storage.read(&object).await.unwrap().unwrap();
        assert!(storage
            .compare_and_set(&object, stored.version, b"{\"format\":1,\"asOfSeq\":40}")
            .await
            .unwrap());

        assert!(log.snapshot().await.unwrap().is_none());
        assert_eq!(views_of(&log).await, before);
    }

    #[tokio::test]
    async fn reclaiming_deletes_only_history_an_older_snapshot_covers() {
        let (_storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        commit_history(&log, &writer, 6, 200).await;
        let before = views_of(&log).await;
        let retention = SessionRetention {
            prune: true,
            min_age: Duration::ZERO,
        };
        // First snapshot: no predecessor, so nothing may be deleted yet.
        log.publish_snapshot(&writer, retention).await.unwrap();
        let commits = "agents/a/sessions/u/s1/commits";
        assert_eq!(log.reclaimed_floor().await.unwrap(), None);

        commit_history(&log, &writer, 6, 200).await;
        assert_eq!(log.head().await.unwrap().seq, 2400);
        // Second snapshot: the first one becomes the reclaim floor.
        log.publish_snapshot(&writer, retention).await.unwrap();

        assert_eq!(log.reclaimed_floor().await.unwrap(), Some(1200));
        let mut remaining = log.storage.list_paths(commits).await.unwrap();
        remaining.retain(|path| path.starts_with(&format!("{commits}/")));
        // The 5 commits below 1200 are gone; the one ending at the floor is the anchor.
        assert_eq!(remaining.len(), 7, "{remaining:?}");

        // Reading still reproduces the same views: the snapshot covers what was deleted.
        let after = views_of(&log).await;
        assert_eq!(after.0.uncertain_tool_calls, before.0.uncertain_tool_calls);
        assert_eq!(after.1.messages.len(), 2400);
        assert_eq!(after.1.messages[0].text_content(), "m1");
        assert_eq!(after.2, before.2);

        // A replay from seq 0 keeps what is still readable instead of failing. The anchor batch
        // ends exactly at the floor, so its events are still on disk and still visited; the gap
        // below it is reported by `scan` (below) and covered by the snapshot.
        let mut visited: Vec<u64> = Vec::new();
        log.visit(0, 2400, &mut |event| {
            visited.push(event.seq);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(visited.first(), Some(&1001));
        assert_eq!(visited.last(), Some(&2400));

        // An event-level read of reclaimed history says so instead of returning a short prefix.
        let err = log.scan(0, 2400).await.unwrap_err();
        assert!(err.to_string().contains("were reclaimed"), "{err}");
        assert_eq!(log.scan(1200, 2400).await.unwrap().len(), 1200);
    }

    #[tokio::test]
    async fn reclaimed_history_without_a_usable_snapshot_fails_loudly() {
        let (storage, log) = journal();
        let writer = acquired(&log, "run-1").await;
        commit_history(&log, &writer, 6, 200).await;
        let retention = SessionRetention {
            prune: true,
            min_age: Duration::ZERO,
        };
        log.publish_snapshot(&writer, retention).await.unwrap();
        commit_history(&log, &writer, 6, 200).await;
        log.publish_snapshot(&writer, retention).await.unwrap();
        assert_eq!(log.reclaimed_floor().await.unwrap(), Some(1200));

        // Losing the snapshot after the history was deleted must not silently return views that
        // are missing their first 1200 events.
        let pointer_path = "agents/a/sessions/u/s1/snapshot.json";
        let stored = storage.read(pointer_path).await.unwrap().unwrap();
        storage.remove(pointer_path).await.unwrap();
        assert!(stored.version > 0);

        let err = crate::session::SessionProjection::read(&log)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reclaimed"), "{err}");
    }
}
