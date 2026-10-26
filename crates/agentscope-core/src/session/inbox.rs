//! Durable command acceptance, mirroring `SessionInbox`.
//!
//! The inbox is its own journal: accepting input uses a short writer lease and never takes
//! the execution writer's lease. Both are committed through the same [`SessionLog`] seam.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{session_error, Head, SessionEvent, SessionLog, Writer};
use crate::message::Msg;

/// How many times acceptance retries a busy writer before failing.
const ACCEPT_ATTEMPTS: usize = 100;

/// The inbox lease is short: acceptance never blocks on execution progress.
const INBOX_LEASE: Duration = Duration::from_secs(120);

/// A durably accepted command.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<Msg>,
    #[serde(default)]
    pub request_ids: Vec<String>,
}

impl Command {
    /// Creates a command. `submit`/`resume`/`respond` start an execution; `steer`/`inject`
    /// add input to a running or future execution; `interrupt` stops one.
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        turn_id: Option<String>,
        messages: Vec<Msg>,
        request_ids: Vec<String>,
    ) -> crate::error::Result<Self> {
        let id = id.into();
        if id.trim().is_empty() || id.len() > 200 {
            return Err(session_error("Invalid command id"));
        }
        let kind = kind.into();
        if kind.trim().is_empty() {
            return Err(session_error("Invalid command kind"));
        }
        Ok(Self {
            id,
            kind,
            turn_id,
            messages,
            request_ids,
        })
    }

    /// Whether accepting this command starts an execution.
    pub fn execution(&self) -> bool {
        matches!(self.kind.as_str(), "submit" | "resume" | "respond")
    }

    /// The payload as accepted on the wire (timestamps stripped for idempotency comparison).
    fn comparable(&self) -> crate::error::Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)?;
        if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages {
                if let Some(object) = message.as_object_mut() {
                    object.remove("timestamp");
                }
            }
        }
        Ok(value)
    }
}

/// The folded state of the inbox at a committed prefix.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboxSnapshot {
    pub commands: Vec<Command>,
    pub turn_id: Option<String>,
    pub run_id: Option<String>,
    pub rejected: BTreeMap<String, String>,
    pub handled: BTreeSet<String>,
}

impl InboxSnapshot {
    /// The accepted command with `id`, if any.
    pub fn command(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// Whether `id` was rejected.
    pub fn is_rejected(&self, id: &str) -> bool {
        self.rejected.contains_key(id)
    }

    /// Whether `id` was marked handled.
    pub fn is_handled(&self, id: &str) -> bool {
        self.handled.contains(id)
    }
}

/// Durable command acceptance against the inbox journal.
#[derive(Clone)]
pub struct SessionInbox {
    journal: Arc<dyn SessionLog>,
    /// The folded snapshot and the committed head it was derived from.
    ///
    /// Receipt readers poll `snapshot()` every few tens of milliseconds. Re-reading the whole
    /// inbox on every poll re-decoded every accepted command (including its input messages);
    /// in a long session that allocated steadily without any new command being accepted.
    cache: Arc<Mutex<Option<(u64, Option<String>, Arc<InboxSnapshot>)>>>,
}

impl std::fmt::Debug for SessionInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionInbox")
    }
}

impl SessionInbox {
    /// Wraps a journal, addressing its `inbox` sub-journal.
    pub fn new(log: Arc<dyn SessionLog>) -> crate::error::Result<Self> {
        Ok(Self {
            journal: log.inbox()?,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    /// Folds every accepted command and its outcome.
    ///
    /// Callers that only inspect the snapshot should prefer [`snapshot_shared`](Self::snapshot_shared):
    /// this compatibility shape clones the accepted commands on every call.
    pub async fn snapshot(&self) -> crate::error::Result<InboxSnapshot> {
        Ok((*self.snapshot_shared().await?).clone())
    }

    /// Folds every accepted command and its outcome, sharing the cached value.
    pub async fn snapshot_shared(&self) -> crate::error::Result<Arc<InboxSnapshot>> {
        let head = self.journal.head().await?;
        {
            let cache = self.cache.lock().unwrap();
            if let Some((seq, commit, snapshot)) = cache.as_ref() {
                if *seq == head.seq && commit.as_deref() == head.commit_id.as_deref() {
                    return Ok(Arc::clone(snapshot));
                }
            }
        }
        let snapshot = Arc::new(self.fold(&head).await?);
        *self.cache.lock().unwrap() = Some((head.seq, head.commit_id.clone(), Arc::clone(&snapshot)));
        Ok(snapshot)
    }

    async fn fold(&self, head: &Head) -> crate::error::Result<InboxSnapshot> {
        let mut commands = Vec::new();
        let mut rejected = BTreeMap::new();
        let mut handled = BTreeSet::new();
        let mut turn_id = None;
        let mut run_id = None;

        for event in self.journal.scan(0, head.seq).await? {
            let data = event.data()?;
            match event.event_type.as_str() {
                "inbox/accepted" => commands.push(event.payload::<Command>()?),
                "inbox/opened" => {
                    turn_id = event.turn_id.clone();
                    run_id = event.execution_run_id.clone();
                }
                "inbox/closed" => {
                    if run_id == event.execution_run_id {
                        turn_id = None;
                        run_id = None;
                    }
                }
                "inbox/rejected" => {
                    if let (Some(id), Some(reason)) = (
                        data.get("commandId").and_then(|v| v.as_str()),
                        data.get("reason").and_then(|v| v.as_str()),
                    ) {
                        rejected.insert(id.to_string(), reason.to_string());
                    }
                }
                "inbox/handled" => {
                    if let Some(id) = data.get("commandId").and_then(|v| v.as_str()) {
                        handled.insert(id.to_string());
                    }
                }
                other => {
                    return Err(session_error(format!("Unknown inbox event: {other}")));
                }
            }
        }
        Ok(InboxSnapshot {
            commands,
            turn_id,
            run_id,
            rejected,
            handled,
        })
    }

    /// Accepts a command unless the same id was already accepted with different input.
    pub async fn accept(&self, command: &Command) -> crate::error::Result<Command> {
        let writer = self.acquire().await?;
        let result = async {
            let snapshot = self.snapshot().await?;
            if let Some(existing) = snapshot.command(command.id.as_str()) {
                if existing.comparable()? != command.comparable()? {
                    return Err(session_error(
                        "Idempotency key already used for different input",
                    ));
                }
                return Ok(existing.clone());
            }
            self.commit_one(
                &writer,
                "inbox/accepted",
                None,
                command.turn_id.clone(),
                command,
            )
            .await?;
            Ok(command.clone())
        }
        .await;
        let _ = self.journal.release(&writer).await;
        result
    }

    /// Records that an execution opened for `turn`/`run`.
    pub async fn open(&self, turn_id: &str, run_id: &str) -> crate::error::Result<()> {
        let writer = self.acquire().await?;
        let result = self
            .commit_one(
                &writer,
                "inbox/opened",
                Some(run_id.to_string()),
                Some(turn_id.to_string()),
                &serde_json::json!({}),
            )
            .await
            .map(|_| ());
        let _ = self.journal.release(&writer).await;
        result
    }

    /// Records that the execution for `turn`/`run` closed.
    pub async fn close(&self, turn_id: &str, run_id: &str) -> crate::error::Result<()> {
        let writer = self.acquire().await?;
        let result = self
            .commit_one(
                &writer,
                "inbox/closed",
                Some(run_id.to_string()),
                Some(turn_id.to_string()),
                &serde_json::json!({}),
            )
            .await
            .map(|_| ());
        let _ = self.journal.release(&writer).await;
        result
    }

    /// Records a rejected command and its reason.
    pub async fn reject(&self, command: &Command, reason: &str) -> crate::error::Result<()> {
        let writer = self.acquire().await?;
        let result = self
            .commit_one(
                &writer,
                "inbox/rejected",
                None,
                command.turn_id.clone(),
                &serde_json::json!({ "commandId": command.id, "reason": reason }),
            )
            .await
            .map(|_| ());
        let _ = self.journal.release(&writer).await;
        result
    }

    /// Records that a command was handled (e.g. a consumed interrupt).
    pub async fn handled(&self, command: &Command) -> crate::error::Result<()> {
        let writer = self.acquire().await?;
        let result = self
            .commit_one(
                &writer,
                "inbox/handled",
                None,
                command.turn_id.clone(),
                &serde_json::json!({ "commandId": command.id }),
            )
            .await
            .map(|_| ());
        let _ = self.journal.release(&writer).await;
        result
    }

    /// Acquires the inbox writer lease, retrying while an execution writer holds it.
    async fn acquire(&self) -> crate::error::Result<Writer> {
        for _ in 0..ACCEPT_ATTEMPTS {
            match self
                .journal
                .acquire(&uuid::Uuid::new_v4().to_string(), INBOX_LEASE)
                .await
            {
                Ok(writer) => return Ok(writer),
                Err(error) => {
                    // A busy inbox is expected under concurrent acceptance; retry briefly.
                    if !error.to_string().contains("active writer") {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Err(session_error("Interrupted while accepting session input"))
    }

    /// Commits one inbox fact under the held lease.
    async fn commit_one<T: serde::Serialize>(
        &self,
        writer: &Writer,
        event_type: &str,
        run_id: Option<String>,
        turn_id: Option<String>,
        payload: &T,
    ) -> crate::error::Result<u64> {
        let head = self.journal.head().await?;
        let seq = head.seq + 1;
        let event = SessionEvent::from_payload(
            uuid::Uuid::new_v4().to_string(),
            seq,
            chrono::Utc::now().timestamp_millis(),
            event_type,
            run_id,
            turn_id,
            true,
            payload,
        )?;
        self.journal
            .commit(writer, &uuid::Uuid::new_v4().to_string(), head.seq, &[event])
            .await?;
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Msg;
    use crate::session::{AtomicSessionStorage, JournalSessionLog, MemorySessionStorage};

    fn inbox() -> (SessionInbox, Arc<dyn SessionLog>) {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(storage, "s1"));
        (SessionInbox::new(Arc::clone(&log)).unwrap(), log)
    }

    fn command(id: &str, kind: &str, text: &str) -> Command {
        Command::new(id, kind, Some("t1".into()), vec![Msg::user(text)], vec![]).unwrap()
    }

    #[tokio::test]
    async fn accept_is_idempotent_for_identical_input() {
        let (inbox, _log) = inbox();
        let first = command("c1", "submit", "hello");
        assert_eq!(inbox.accept(&first).await.unwrap(), first);
        // The same key with identical input returns the original receipt.
        assert_eq!(inbox.accept(&first).await.unwrap(), first);
        assert_eq!(inbox.snapshot().await.unwrap().commands.len(), 1);
    }

    #[tokio::test]
    async fn accept_rejects_reused_key_with_different_input() {
        let (inbox, _log) = inbox();
        inbox.accept(&command("c1", "submit", "hello")).await.unwrap();
        let err = inbox
            .accept(&command("c1", "submit", "different"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("different input"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn opened_and_closed_track_the_active_run() {
        let (inbox, _log) = inbox();
        inbox.open("t1", "run-1").await.unwrap();
        let snapshot = inbox.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_id.as_deref(), Some("t1"));
        assert_eq!(snapshot.run_id.as_deref(), Some("run-1"));

        inbox.close("t1", "run-1").await.unwrap();
        let snapshot = inbox.snapshot().await.unwrap();
        assert!(snapshot.turn_id.is_none());
        assert!(snapshot.run_id.is_none());
    }

    #[tokio::test]
    async fn closing_a_different_run_does_not_clear_the_active_one() {
        let (inbox, _log) = inbox();
        inbox.open("t1", "run-1").await.unwrap();
        inbox.close("t2", "run-2").await.unwrap();
        assert_eq!(inbox.snapshot().await.unwrap().run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn rejection_and_handled_are_recorded() {
        let (inbox, _log) = inbox();
        let cmd = command("c1", "submit", "hello");
        inbox.accept(&cmd).await.unwrap();
        inbox.reject(&cmd, "turn_cancelled").await.unwrap();
        inbox.handled(&cmd).await.unwrap();

        let snapshot = inbox.snapshot().await.unwrap();
        assert!(snapshot.is_rejected("c1"));
        assert_eq!(snapshot.rejected["c1"], "turn_cancelled");
        assert!(snapshot.is_handled("c1"));
    }

    #[tokio::test]
    async fn command_kinds_classify_execution() {
        for kind in ["submit", "resume", "respond"] {
            assert!(command("c", kind, "x").execution(), "{kind}");
        }
        for kind in ["steer", "inject", "interrupt"] {
            assert!(!command("c", kind, "x").execution(), "{kind}");
        }
    }

    #[tokio::test]
    async fn invalid_command_ids_are_rejected() {
        assert!(Command::new("", "submit", None, vec![], vec![]).is_err());
        assert!(Command::new("c", "", None, vec![], vec![]).is_err());
        assert!(Command::new("x".repeat(201), "submit", None, vec![], vec![]).is_err());
    }

    #[tokio::test]
    async fn idempotency_comparison_ignores_timestamps() {
        let (inbox, _log) = inbox();
        let mut first = command("c1", "submit", "hello");
        first.messages[0].timestamp = "2026-01-01 00:00:00.000".into();
        inbox.accept(&first).await.unwrap();
        // A retried submission carries a fresh timestamp but must still be idempotent.
        let mut retry = first.clone();
        retry.messages[0].timestamp = "2026-10-20 00:00:00.000".into();
        assert_eq!(inbox.accept(&retry).await.unwrap(), first);
    }

    #[tokio::test]
    async fn inbox_traffic_never_takes_the_execution_writer() {
        let (inbox, log) = inbox();
        let execution = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        // Acceptance still succeeds while an execution writer holds the main journal lease.
        inbox.accept(&command("c1", "submit", "hello")).await.unwrap();
        assert!(!inbox.snapshot().await.unwrap().commands.is_empty());
        log.release(&execution).await.unwrap();
    }
}
