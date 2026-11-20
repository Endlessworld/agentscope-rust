//! The durable inbox/outbox for side effects.
//!
//! An execution is recorded before it leaves the process and its outcome is recorded after. A
//! restart reads that record instead of guessing: `Settled` answers from the stored result,
//! `Dispatched` means "may have happened, do not resend", `Unknown` means "cannot be settled".

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::store::RecordStore;
use crate::types::{ExecutionRequest, ExecutionResult};
use crate::{DurableError, Result};

/// Where one dispatch is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DispatchState {
    /// Recorded, not sent yet.
    Pending,
    /// Sent, no result observed. Never resent automatically: the peer may have run it.
    Dispatched,
    /// A result is recorded.
    Settled,
    /// The effect may or may not have happened; no automatic retry.
    Unknown,
}

impl DispatchState {
    /// Whether this state still belongs in the durable readiness index.
    fn queued(self) -> bool {
        matches!(self, Self::Pending | Self::Dispatched)
    }
}

/// One execution the host dispatched and what it knows about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchRecord {
    pub execution_id: String,
    pub request: ExecutionRequest,
    pub state: DispatchState,
    pub attempts: u32,
    pub dispatched_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExecutionResult>,
}

/// The durable record of what left the process.
pub trait ExecutionStore: Send + Sync {
    fn put(&self, record: &DispatchRecord) -> Result<()>;
    fn get(&self, execution_id: &str) -> Result<Option<DispatchRecord>>;
    /// Records that are due: `Pending` (never sent) and `Dispatched` (outcome unobserved).
    fn pending(&self, limit: usize) -> Result<Vec<DispatchRecord>>;
    fn settle(&self, execution_id: &str, result: &ExecutionResult) -> Result<()>;
}

/// The in-process implementation; the durable one below is the production shape.
#[derive(Default)]
pub struct MemoryExecutionStore {
    records: Mutex<BTreeMap<String, DispatchRecord>>,
}

impl MemoryExecutionStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, DispatchRecord>>> {
        self.records
            .lock()
            .map_err(|_| DurableError::unavailable("execution store lock is poisoned"))
    }
}

impl ExecutionStore for MemoryExecutionStore {
    fn put(&self, record: &DispatchRecord) -> Result<()> {
        self.records()?
            .insert(record.execution_id.clone(), record.clone());
        Ok(())
    }

    fn get(&self, execution_id: &str) -> Result<Option<DispatchRecord>> {
        Ok(self.records()?.get(execution_id).cloned())
    }

    fn pending(&self, limit: usize) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records()?
            .values()
            .filter(|record| record.state.queued())
            .take(limit)
            .cloned()
            .collect())
    }

    fn settle(&self, execution_id: &str, result: &ExecutionResult) -> Result<()> {
        let mut records = self.records()?;
        let record = records.get_mut(execution_id).ok_or_else(|| {
            DurableError::not_found(format!("execution `{execution_id}` is not recorded"))
        })?;
        record.state = DispatchState::Settled;
        record.result = Some(result.clone());
        Ok(())
    }
}

/// An execution inbox over any [`RecordStore`].
///
/// Records live in one scope (`hand:executions`) and the due list is the store's own readiness
/// index, so a host that already runs a `RecordStore` needs no second database. The queue name
/// carries the scope because the readiness index is global to the store.
pub struct RecordStoreExecutionStore {
    store: Arc<dyn RecordStore>,
}

impl RecordStoreExecutionStore {
    pub const SCOPE: &'static str = "hand:executions";
    pub const QUEUE: &'static str = "exec:hand:executions";

    pub fn new(store: Arc<dyn RecordStore>) -> Self {
        Self { store }
    }

    fn part(execution_id: &str) -> String {
        format!("exec:{execution_id}")
    }
}

fn decode(value: serde_json::Value) -> Result<DispatchRecord> {
    serde_json::from_value(value)
        .map_err(|error| DurableError::invalid(format!("dispatch record: {error}")))
}

impl ExecutionStore for RecordStoreExecutionStore {
    fn put(&self, record: &DispatchRecord) -> Result<()> {
        let value = serde_json::to_value(record)?;
        let payload = value.clone();
        let state = record.state;
        let execution_id = record.execution_id.clone();
        self.store.transaction(Self::SCOPE, &mut |txn| {
            txn.put(&Self::part(&execution_id), Some(value.clone()))?;
            if state.queued() {
                txn.schedule(Self::QUEUE, &execution_id, txn.now(), Some(payload.clone()))
            } else {
                txn.schedule(Self::QUEUE, &execution_id, -1, None)
            }
        })
    }

    fn get(&self, execution_id: &str) -> Result<Option<DispatchRecord>> {
        let mut value = None;
        self.store.read_transaction(Self::SCOPE, &mut |txn| {
            value = txn.get(&Self::part(execution_id))?;
            Ok(())
        })?;
        value.map(decode).transpose()
    }

    fn pending(&self, limit: usize) -> Result<Vec<DispatchRecord>> {
        let due = self.store.ready(Self::QUEUE, crate::now_millis(), limit)?;
        let mut records = Vec::with_capacity(due.len());
        for item in due {
            // The scheduled payload is the record itself, so a page of due work costs one read
            // of the index, not one transaction per record.
            let record = match item.payload {
                Some(payload) => Some(decode(payload)?),
                None => self.get(&item.work_id)?,
            };
            if let Some(record) = record {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn settle(&self, execution_id: &str, result: &ExecutionResult) -> Result<()> {
        let part = Self::part(execution_id);
        let result = result.clone();
        self.store.transaction(Self::SCOPE, &mut |txn| {
            let value = txn.get(&part)?.ok_or_else(|| {
                DurableError::not_found(format!("execution `{execution_id}` is not recorded"))
            })?;
            let mut record = decode(value)?;
            record.state = DispatchState::Settled;
            record.result = Some(result.clone());
            let value = serde_json::to_value(&record)?;
            txn.put(&part, Some(value))?;
            txn.schedule(Self::QUEUE, execution_id, -1, None)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::memory::InMemoryStore;
    use crate::types::{ExecutionState, ToolCall};

    fn request() -> ExecutionRequest {
        ExecutionRequest::new(
            "exec-1",
            crate::types::ThreadKey::new(
                crate::types::SessionKey::new("ns", "s1").expect("session key"),
                "main",
            )
            .expect("thread key"),
            "turn-1",
            ToolCall::new("call-1", "local", "echo", json!({"text": "hi"})).expect("call"),
        )
        .expect("request")
    }

    fn record(state: DispatchState) -> DispatchRecord {
        DispatchRecord {
            execution_id: "exec-1".to_string(),
            request: request(),
            state,
            attempts: 1,
            dispatched_at: 1,
            result: None,
        }
    }

    fn stores() -> Vec<Box<dyn ExecutionStore>> {
        let memory = Arc::new(InMemoryStore::new());
        vec![
            Box::new(MemoryExecutionStore::new()),
            Box::new(RecordStoreExecutionStore::new(memory)),
        ]
    }

    #[test]
    fn a_settled_record_leaves_the_due_list() {
        for store in stores() {
            store.put(&record(DispatchState::Pending)).expect("put");
            assert_eq!(store.pending(10).expect("pending").len(), 1);
            store
                .settle("exec-1", &ExecutionResult::succeeded(json!({"ok": true})))
                .expect("settle");
            assert!(store.pending(10).expect("pending").is_empty());
            let stored = store.get("exec-1").expect("get").expect("record");
            assert_eq!(stored.state, DispatchState::Settled);
            assert_eq!(
                stored.result.map(|result| result.state),
                Some(ExecutionState::Succeeded)
            );
        }
    }

    #[test]
    fn a_dispatched_record_stays_due() {
        for store in stores() {
            store.put(&record(DispatchState::Dispatched)).expect("put");
            let due = store.pending(10).expect("pending");
            assert_eq!(due.len(), 1);
            assert_eq!(due[0].state, DispatchState::Dispatched);
        }
    }

    #[test]
    fn the_execution_inbox_has_a_stable_wire_shape() {
        let value = serde_json::to_value(record(DispatchState::Dispatched)).expect("serialize");
        assert_eq!(value["state"], json!("DISPATCHED"));
        assert_eq!(value["executionId"], json!("exec-1"));
        let back: DispatchRecord = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back.state, DispatchState::Dispatched);
    }

    #[test]
    fn a_settle_of_an_unrecorded_execution_is_not_found() {
        let store = MemoryExecutionStore::new();
        let error = store
            .settle("missing", &ExecutionResult::missing())
            .expect_err("must fail");
        assert_eq!(error.code(), crate::ErrorCode::NotFound);
    }

    #[test]
    fn a_dispatch_record_survives_a_new_store_handle() {
        let dir = std::env::temp_dir().join(format!(
            "durable-execution-store-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let root = dir.to_string_lossy().to_string();
        {
            let store: Arc<dyn RecordStore> =
                Arc::new(crate::store::file::FileStore::open(&dir).expect("store"));
            let inbox = RecordStoreExecutionStore::new(store);
            inbox.put(&record(DispatchState::Dispatched)).expect("put");
        }
        let store: Arc<dyn RecordStore> =
            Arc::new(crate::store::file::FileStore::open(&dir).expect("reopen"));
        let inbox = RecordStoreExecutionStore::new(store);
        let stored = inbox.get("exec-1").expect("get").expect("record");
        assert_eq!(stored.state, DispatchState::Dispatched);
        assert_eq!(inbox.pending(10).expect("pending").len(), 1);

        drop(root);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settled_record_is_not_pending_after_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "durable-execution-settled-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let store: Arc<dyn RecordStore> =
            Arc::new(crate::store::file::FileStore::open(&dir).expect("store"));
        let inbox = RecordStoreExecutionStore::new(Arc::clone(&store));
        inbox.put(&record(DispatchState::Pending)).expect("put");
        inbox
            .settle("exec-1", &ExecutionResult::succeeded(json!(null)))
            .expect("settle");
        drop(inbox);
        drop(store);
        let store: Arc<dyn RecordStore> =
            Arc::new(crate::store::file::FileStore::open(&dir).expect("reopen"));
        let inbox = RecordStoreExecutionStore::new(store);
        assert!(inbox.pending(10).expect("pending").is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pending_is_bounded_by_the_limit() {
        let store = MemoryExecutionStore::new();
        for index in 0..5 {
            let mut record = record(DispatchState::Pending);
            record.execution_id = format!("exec-{index}");
            store.put(&record).expect("put");
        }
        assert_eq!(store.pending(2).expect("pending").len(), 2);
    }
}
