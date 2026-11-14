//! Acknowledged committed-event export, mirroring `JournalExportSink` and
//! `JournalExporter`.

use std::sync::Arc;

use super::{JournalEvent, ThreadJournal};

/// Runtime-context key carrying the committed-event export sink.
///
/// Mirrors Java `JournalExportSink.CONTEXT_KEY`. A host that installs a
/// [`JournalExportSinkHandle`] under this key makes every recorder the agent opens export
/// its committed events through [`JournalExporter`]; without it, exporting stays off and
/// the run behaves exactly as before.
pub const JOURNAL_EXPORT_SINK_CONTEXT_KEY: &str = "agentscope.session.exportSink";

/// A committed-event sink. Implementations must deduplicate by native event id.
///
/// A lost acknowledgement re-delivers the same immutable event; it never re-executes it.
pub trait JournalExportSink: Send + Sync {
    /// Stable sink name; the durable cursor is keyed by it.
    fn name(&self) -> &str;

    /// Accepts one committed event.
    fn accept(&self, event: &JournalEvent) -> crate::error::Result<()>;

    /// Accepts one committed event belonging to `session_id`.
    ///
    /// A sink shared by several sessions (the normal case when it is installed once on an
    /// agent) uses this to key its own dedupe state per session. Sinks bound to a single
    /// session keep the default, which delegates to [`JournalExportSink::accept`].
    fn accept_for(&self, _session_id: &str, event: &JournalEvent) -> crate::error::Result<()> {
        self.accept(event)
    }
}

/// A `JournalExportSink` that can travel through the typed
/// [`Extensions`](crate::agent::Extensions) bag.
///
/// `Extensions` stores sized `Any` values, so an `Arc<dyn JournalExportSink>` cannot be
/// inserted directly. This newtype is the handle hosts put under
/// [`JOURNAL_EXPORT_SINK_CONTEXT_KEY`].
#[derive(Clone)]
pub struct JournalExportSinkHandle {
    sink: Arc<dyn JournalExportSink>,
}

impl std::fmt::Debug for JournalExportSinkHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalExportSinkHandle")
            .field("name", &self.sink.name())
            .finish()
    }
}

impl JournalExportSinkHandle {
    /// Wraps a sink for installation in a runtime context.
    pub fn new(sink: Arc<dyn JournalExportSink>) -> Self {
        Self { sink }
    }

    /// The wrapped sink.
    pub fn sink(&self) -> Arc<dyn JournalExportSink> {
        Arc::clone(&self.sink)
    }

    /// Builds an exporter for the sink, bound to a logical session.
    pub fn exporter_for_session(
        &self,
        log: Arc<dyn ThreadJournal>,
        session_id: impl Into<String>,
    ) -> JournalExporter {
        JournalExporter::for_session(log, Arc::clone(&self.sink), session_id)
    }
}

/// Drains committed events into a sink using a durable outbox cursor.
pub struct JournalExporter {
    log: Arc<dyn ThreadJournal>,
    sink: Arc<dyn JournalExportSink>,
    /// Logical session the exported facts belong to; `None` keeps the single-session
    /// [`JournalExportSink::accept`] contract.
    session_id: Option<String>,
}

impl JournalExporter {
    pub fn new(log: Arc<dyn ThreadJournal>, sink: Arc<dyn JournalExportSink>) -> Self {
        Self {
            log,
            sink,
            session_id: None,
        }
    }

    /// Builds an exporter whose sink is told which logical session the events belong to.
    ///
    /// The recorder that drains a journal knows its session; a sink installed once on an
    /// agent does not, so this is what lets one sink deduplicate per `sourceWatermark`.
    pub fn for_session(
        log: Arc<dyn ThreadJournal>,
        sink: Arc<dyn JournalExportSink>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            log,
            sink,
            session_id: Some(session_id.into()),
        }
    }

    /// Delivers every committed event after the sink's cursor, advancing as it goes.
    ///
    /// The cursor advances only after `accept` succeeds, so a partial drain resumes exactly
    /// where it stopped. Events are read in bounded batches and the cursor is advanced once per
    /// batch: a per-event watermark cost one head read plus one compare-and-set per event, which
    /// dominated a long backlog. The sink contract already requires deduplication by event id,
    /// so a batch acknowledged as a whole can never deliver twice.
    pub async fn drain(&self) -> crate::error::Result<usize> {
        const BATCH: usize = 256;
        let name = self.sink.name().to_string();
        let mut cursor = self.log.export_cursor(&name).await?;
        // History below the floor was reclaimed after a snapshot covered it, so it can no longer
        // be exported. Skipping it is the only way to keep exporting what came after; the loss
        // is logged because it is permanent.
        if let Some(floor) = self.log.reclaimed_floor().await? {
            if cursor < floor {
                tracing::warn!(
                    sink = %name,
                    cursor,
                    floor,
                    "session history below the reclaim floor can no longer be exported"
                );
                let head = self.log.head().await?;
                let next = floor.min(head.seq);
                self.log.advance_export_cursor(&name, cursor, next).await?;
                cursor = next;
            }
        }
        let mut delivered = 0;
        loop {
            let batch = self.log.read_after(cursor, BATCH).await?;
            if batch.is_empty() {
                return Ok(delivered);
            }
            for (index, event) in batch.iter().enumerate() {
                let accepted = match &self.session_id {
                    Some(session_id) => self.sink.accept_for(session_id, event),
                    None => self.sink.accept(event),
                };
                if let Err(error) = accepted {
                    // Keep the "the cursor never passes an unaccepted event" contract: on a
                    // failure, move it to the last event that was accepted, then report.
                    if index > 0 {
                        let last_ok = batch[index - 1].seq;
                        let _ = self.log.advance_export_cursor(&name, cursor, last_ok).await;
                    }
                    return Err(error);
                }
                delivered += 1;
            }
            let last = batch.last().map(|event| event.seq).unwrap_or(cursor);
            self.log.advance_export_cursor(&name, cursor, last).await?;
            cursor = last;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::journal::{
        AtomicJournalStorage, AtomicThreadJournal, JournalEvent, MemoryJournalStorage, Writer,
    };

    #[derive(Default)]
    struct RecordingSink {
        name: String,
        seen: Mutex<Vec<String>>,
        fail_after: Option<usize>,
        attempts: AtomicUsize,
    }

    impl RecordingSink {
        fn new(name: &str, fail_after: Option<usize>) -> Self {
            Self {
                name: name.into(),
                seen: Mutex::new(Vec::new()),
                fail_after,
                attempts: AtomicUsize::new(0),
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl JournalExportSink for RecordingSink {
        fn name(&self) -> &str {
            &self.name
        }

        fn accept(&self, event: &JournalEvent) -> crate::error::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if let Some(limit) = self.fail_after {
                if attempt >= limit {
                    return Err(crate::error::Error::Other("sink down".into()));
                }
            }
            self.seen.lock().unwrap().push(event.event_type.clone());
            Ok(())
        }
    }

    async fn seeded() -> (Arc<dyn ThreadJournal>, Arc<dyn AtomicJournalStorage>) {
        let storage: Arc<dyn AtomicJournalStorage> = Arc::new(MemoryJournalStorage::new());
        let log: Arc<dyn ThreadJournal> =
            Arc::new(AtomicThreadJournal::new(Arc::clone(&storage), "s1"));
        let writer: Writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let events = vec![
            JournalEvent::new(
                "e1",
                1,
                0,
                "run/start",
                Some("run-1".into()),
                None,
                true,
                "{}",
            )
            .unwrap(),
            JournalEvent::new(
                "e2",
                2,
                0,
                "turn/start",
                Some("run-1".into()),
                None,
                true,
                "{}",
            )
            .unwrap(),
        ];
        log.commit(&writer, "b1", 0, &events).await.unwrap();
        log.release(&writer).await.unwrap();
        (log, storage)
    }

    #[tokio::test]
    async fn drain_delivers_each_event_once() {
        let (log, _storage) = seeded().await;
        let sink = Arc::new(RecordingSink::new("sink-a", None));
        let exporter = JournalExporter::new(Arc::clone(&log), sink.clone());

        assert_eq!(exporter.drain().await.unwrap(), 2);
        assert_eq!(sink.seen(), vec!["run/start", "turn/start"]);
        // A second drain is a no-op because the cursor advanced durably.
        assert_eq!(exporter.drain().await.unwrap(), 0);
        assert_eq!(sink.seen().len(), 2);
    }

    #[tokio::test]
    async fn drain_advances_the_cursor_once_per_batch() {
        // A per-event watermark cost one head read plus one compare-and-set per event, which
        // dominated a long backlog. The cursor now moves once per batch, and a failure inside
        // a batch still leaves it on the last acknowledged event.
        let storage: Arc<dyn AtomicJournalStorage> = Arc::new(MemoryJournalStorage::new());
        let log: Arc<dyn ThreadJournal> =
            Arc::new(AtomicThreadJournal::new(Arc::clone(&storage), "s1"));
        let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let events: Vec<JournalEvent> = (1..=300u64)
            .map(|seq| {
                JournalEvent::new(
                    format!("e{seq}"),
                    seq,
                    0,
                    "turn/start",
                    Some("run-1".into()),
                    None,
                    true,
                    "{}",
                )
                .unwrap()
            })
            .collect();
        log.commit(&writer, "b1", 0, &events).await.unwrap();
        log.release(&writer).await.unwrap();

        let sink = Arc::new(RecordingSink::new("sink-batch", None));
        let exporter = JournalExporter::new(Arc::clone(&log), sink.clone());
        assert_eq!(exporter.drain().await.unwrap(), 300);
        assert_eq!(log.export_cursor("sink-batch").await.unwrap(), 300);
        assert_eq!(sink.seen().len(), 300);
        // A second drain is a no-op: the cursor is durable at the last event.
        assert_eq!(exporter.drain().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failure_in_a_later_batch_keeps_the_earlier_progress() {
        let storage: Arc<dyn AtomicJournalStorage> = Arc::new(MemoryJournalStorage::new());
        let log: Arc<dyn ThreadJournal> =
            Arc::new(AtomicThreadJournal::new(Arc::clone(&storage), "s1"));
        let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let events: Vec<JournalEvent> = (1..=300u64)
            .map(|seq| {
                JournalEvent::new(
                    format!("e{seq}"),
                    seq,
                    0,
                    "turn/start",
                    Some("run-1".into()),
                    None,
                    true,
                    "{}",
                )
                .unwrap()
            })
            .collect();
        log.commit(&writer, "b1", 0, &events).await.unwrap();
        log.release(&writer).await.unwrap();

        // Fail on the first event of the second batch: the first batch stays acknowledged.
        let sink = Arc::new(RecordingSink::new("sink-late", Some(256)));
        let exporter = JournalExporter::new(Arc::clone(&log), sink.clone());
        assert!(exporter.drain().await.is_err());
        assert_eq!(log.export_cursor("sink-late").await.unwrap(), 256);
    }

    #[tokio::test]
    async fn cursor_only_advances_after_a_successful_accept() {
        let (log, _storage) = seeded().await;
        let sink = Arc::new(RecordingSink::new("sink-b", Some(1)));
        let exporter = JournalExporter::new(Arc::clone(&log), sink.clone());

        // The second event fails, so the cursor stays at the first event.
        assert!(exporter.drain().await.is_err());
        assert_eq!(log.export_cursor("sink-b").await.unwrap(), 1);
        assert_eq!(sink.seen(), vec!["run/start"]);
    }

    #[tokio::test]
    async fn sinks_keep_independent_cursors() {
        let (log, _storage) = seeded().await;
        let a = Arc::new(RecordingSink::new("sink-a", None));
        JournalExporter::new(Arc::clone(&log), a.clone())
            .drain()
            .await
            .unwrap();
        assert_eq!(log.export_cursor("sink-a").await.unwrap(), 2);
        assert_eq!(log.export_cursor("sink-c").await.unwrap(), 0);
    }

    /// A sink installed once on an agent needs the logical session to key its dedupe state.
    #[derive(Default)]
    struct SessionAwareSink {
        seen: Mutex<Vec<(String, String)>>,
    }

    impl JournalExportSink for SessionAwareSink {
        fn name(&self) -> &str {
            "session-aware"
        }

        fn accept(&self, _event: &JournalEvent) -> crate::error::Result<()> {
            panic!("for_session must deliver through accept_for");
        }

        fn accept_for(&self, session_id: &str, event: &JournalEvent) -> crate::error::Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push((session_id.to_string(), event.event_type.clone()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn for_session_delivers_the_logical_session_id() {
        let (log, _storage) = seeded().await;
        let sink = Arc::new(SessionAwareSink::default());
        JournalExporter::for_session(Arc::clone(&log), sink.clone(), "s1")
            .drain()
            .await
            .unwrap();
        assert_eq!(
            *sink.seen.lock().unwrap(),
            vec![
                ("s1".to_string(), "run/start".to_string()),
                ("s1".to_string(), "turn/start".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_reclaimed_prefix_is_skipped_instead_of_stalling_the_export() {
        // Reclaimed events can never be exported again, but that must not wedge the sink: the
        // cursor jumps to the floor and everything after it is still delivered.
        let storage: Arc<dyn AtomicJournalStorage> = Arc::new(MemoryJournalStorage::new());
        let log = Arc::new(AtomicThreadJournal::new(Arc::clone(&storage), "s1"));
        let as_log: Arc<dyn ThreadJournal> = log.clone();
        let writer = as_log
            .acquire("run-1", Duration::from_secs(60))
            .await
            .unwrap();
        let retention = crate::journal::JournalRetention {
            prune: true,
            min_age: Duration::ZERO,
        };
        for batch in 0..6 {
            let start = as_log.head().await.unwrap().seq;
            let events: Vec<JournalEvent> = (1..=400u64)
                .map(|offset| {
                    let seq = start + offset;
                    JournalEvent::new(
                        format!("e{seq}"),
                        seq,
                        0,
                        "turn/start",
                        Some("run-1".into()),
                        None,
                        true,
                        "{}",
                    )
                    .unwrap()
                })
                .collect();
            as_log
                .commit(&writer, &format!("b{batch}"), start, &events)
                .await
                .unwrap();
            if batch == 2 {
                as_log.publish_snapshot(&writer, retention).await.unwrap();
            }
        }
        as_log.publish_snapshot(&writer, retention).await.unwrap();
        assert_eq!(as_log.reclaimed_floor().await.unwrap(), Some(1200));

        let sink = Arc::new(RecordingSink::new("sink-a", None));
        let exporter = JournalExporter::new(as_log.clone(), sink.clone());
        assert_eq!(exporter.drain().await.unwrap(), 1200);
        assert_eq!(as_log.export_cursor("sink-a").await.unwrap(), 2400);
        assert_eq!(exporter.drain().await.unwrap(), 0);
    }
}
