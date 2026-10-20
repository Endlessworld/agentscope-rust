//! Acknowledged committed-event export, mirroring `SessionExportSink` and
//! `SessionLogExporter`.

use std::sync::Arc;

use super::{SessionEvent, SessionLog};

/// A committed-event sink. Implementations must deduplicate by native event id.
///
/// A lost acknowledgement re-delivers the same immutable event; it never re-executes it.
pub trait SessionExportSink: Send + Sync {
    /// Stable sink name; the durable cursor is keyed by it.
    fn name(&self) -> &str;

    /// Accepts one committed event.
    fn accept(&self, event: &SessionEvent) -> crate::error::Result<()>;
}

/// Drains committed events into a sink using a durable outbox cursor.
pub struct SessionLogExporter {
    log: Arc<dyn SessionLog>,
    sink: Arc<dyn SessionExportSink>,
}

impl SessionLogExporter {
    pub fn new(log: Arc<dyn SessionLog>, sink: Arc<dyn SessionExportSink>) -> Self {
        Self { log, sink }
    }

    /// Delivers every committed event after the sink's cursor, advancing as it goes.
    ///
    /// The cursor advances only after `accept` succeeds, so a partial drain resumes
    /// exactly where it stopped.
    pub async fn drain(&self) -> crate::error::Result<usize> {
        let name = self.sink.name().to_string();
        let mut cursor = self.log.export_cursor(&name).await?;
        let head = self.log.head().await?.seq;
        let mut delivered = 0;
        for event in self.log.scan(cursor, head).await? {
            self.sink.accept(&event)?;
            self.log
                .advance_export_cursor(&name, cursor, event.seq)
                .await?;
            cursor = event.seq;
            delivered += 1;
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionEvent, Writer,
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

    impl SessionExportSink for RecordingSink {
        fn name(&self) -> &str {
            &self.name
        }

        fn accept(&self, event: &SessionEvent) -> crate::error::Result<()> {
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

    async fn seeded() -> (Arc<dyn SessionLog>, Arc<dyn AtomicSessionStorage>) {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(Arc::clone(&storage), "s1"));
        let writer: Writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let events = vec![
            SessionEvent::new("e1", 1, 0, "run/start", Some("run-1".into()), None, true, "{}")
                .unwrap(),
            SessionEvent::new("e2", 2, 0, "turn/start", Some("run-1".into()), None, true, "{}")
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
        let exporter = SessionLogExporter::new(Arc::clone(&log), sink.clone());

        assert_eq!(exporter.drain().await.unwrap(), 2);
        assert_eq!(sink.seen(), vec!["run/start", "turn/start"]);
        // A second drain is a no-op because the cursor advanced durably.
        assert_eq!(exporter.drain().await.unwrap(), 0);
        assert_eq!(sink.seen().len(), 2);
    }

    #[tokio::test]
    async fn cursor_only_advances_after_a_successful_accept() {
        let (log, _storage) = seeded().await;
        let sink = Arc::new(RecordingSink::new("sink-b", Some(1)));
        let exporter = SessionLogExporter::new(Arc::clone(&log), sink.clone());

        // The second event fails, so the cursor stays at the first event.
        assert!(exporter.drain().await.is_err());
        assert_eq!(log.export_cursor("sink-b").await.unwrap(), 1);
        assert_eq!(sink.seen(), vec!["run/start"]);
    }

    #[tokio::test]
    async fn sinks_keep_independent_cursors() {
        let (log, _storage) = seeded().await;
        let a = Arc::new(RecordingSink::new("sink-a", None));
        SessionLogExporter::new(Arc::clone(&log), a.clone()).drain().await.unwrap();
        assert_eq!(log.export_cursor("sink-a").await.unwrap(), 2);
        assert_eq!(log.export_cursor("sink-c").await.unwrap(), 0);
    }
}
