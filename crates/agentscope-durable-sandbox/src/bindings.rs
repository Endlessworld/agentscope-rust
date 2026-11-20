//! Durable identity of a sandbox execution.
//!
//! A handle is a name only the process that spawned it can poll, but the *execution* is a fact
//! of the turn: after a restart the new process must be able to adopt the handle and keep
//! polling, or report `UNKNOWN` when the sandbox no longer knows it. The store records that
//! identity; the state machine never lets an execution jump from `Attached` to `Gone` without
//! leaving a `Releasing` mark behind, so a host that died mid-kill is visible to the next sweep.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_durable::types::ThreadKey;
use agentscope_durable::{DurableError, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::{ExecSpec, Handle};

/// Where an execution is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BindingState {
    /// The handle is live; a restart may adopt it.
    Attached,
    /// The host asked the execution to stop, and the kill is not confirmed yet.
    Releasing,
    /// The execution is over; the record is kept only for idempotent answers.
    Gone,
}

/// The durable identity of one sandbox execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingRecord {
    pub execution_id: String,
    pub thread: ThreadKey,
    pub handle: Handle,
    pub spec: ExecSpec,
    /// Absolute wall-clock deadline in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    pub state: BindingState,
    pub updated_at: u64,
}

/// Where execution identities survive a process.
pub trait BindingStore: Send + Sync {
    fn put(&self, record: &BindingRecord) -> Result<()>;
    fn get(&self, execution_id: &str) -> Result<Option<BindingRecord>>;
    /// Writes `Releasing`, then `Gone`; a crash between the two leaves the barrier visible.
    fn release(&self, execution_id: &str) -> Result<()>;
    /// Records that are not `Gone`, oldest first, at most `limit`.
    fn live(&self, limit: usize) -> Result<Vec<BindingRecord>>;
}

/// An in-process store; the shape a single host (and every test) uses.
#[derive(Debug, Default)]
pub struct MemoryBindings {
    records: Mutex<BTreeMap<String, BindingRecord>>,
}

impl MemoryBindings {
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, BindingRecord>>> {
        self.records
            .lock()
            .map_err(|_| DurableError::invalid("the sandbox bindings lock is poisoned"))
    }
}

impl BindingStore for MemoryBindings {
    fn put(&self, record: &BindingRecord) -> Result<()> {
        self.records()?
            .insert(record.execution_id.clone(), record.clone());
        Ok(())
    }

    fn get(&self, execution_id: &str) -> Result<Option<BindingRecord>> {
        Ok(self.records()?.get(execution_id).cloned())
    }

    fn release(&self, execution_id: &str) -> Result<()> {
        let Some(mut record) = self.get(execution_id)? else {
            return Ok(());
        };
        if record.state == BindingState::Gone {
            return Ok(());
        }
        record.state = BindingState::Releasing;
        record.updated_at = now_millis();
        self.put(&record)?;
        record.state = BindingState::Gone;
        record.updated_at = now_millis();
        self.put(&record)
    }

    fn live(&self, limit: usize) -> Result<Vec<BindingRecord>> {
        let guard = self.records()?;
        let mut live: Vec<BindingRecord> = guard
            .values()
            .filter(|record| record.state != BindingState::Gone)
            .cloned()
            .collect();
        live.sort_by_key(|record| record.updated_at);
        live.truncate(limit);
        Ok(live)
    }
}

/// A store backed by one JSON document, replaced atomically.
#[derive(Debug)]
pub struct FileBindings {
    path: PathBuf,
    records: Mutex<BTreeMap<String, BindingRecord>>,
}

impl FileBindings {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(io)?;
        let path = root.join("bindings.json");
        let records = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                DurableError::invalid(format!("Cannot read the sandbox bindings: {error}"))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(io(error)),
        };
        Ok(Self {
            path,
            records: Mutex::new(records),
        })
    }

    fn records(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, BindingRecord>>> {
        self.records
            .lock()
            .map_err(|_| DurableError::invalid("the sandbox bindings lock is poisoned"))
    }

    fn flush(&self, records: &BTreeMap<String, BindingRecord>) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(records).map_err(|error| {
            DurableError::invalid(format!("Cannot encode the sandbox bindings: {error}"))
        })?;
        std::fs::write(&tmp, bytes).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }
}

impl BindingStore for FileBindings {
    fn put(&self, record: &BindingRecord) -> Result<()> {
        let mut records = self.records()?;
        records.insert(record.execution_id.clone(), record.clone());
        self.flush(&records)
    }

    fn get(&self, execution_id: &str) -> Result<Option<BindingRecord>> {
        Ok(self.records()?.get(execution_id).cloned())
    }

    fn release(&self, execution_id: &str) -> Result<()> {
        let mut record = match self.get(execution_id)? {
            Some(record) if record.state != BindingState::Gone => record,
            _ => return Ok(()),
        };
        record.state = BindingState::Releasing;
        record.updated_at = now_millis();
        self.put(&record)?;
        record.state = BindingState::Gone;
        record.updated_at = now_millis();
        self.put(&record)
    }

    fn live(&self, limit: usize) -> Result<Vec<BindingRecord>> {
        let guard = self.records()?;
        let mut live: Vec<BindingRecord> = guard
            .values()
            .filter(|record| record.state != BindingState::Gone)
            .cloned()
            .collect();
        live.sort_by_key(|record| record.updated_at);
        live.truncate(limit);
        Ok(live)
    }
}

/// The store plus the lifecycle rules a host should use.
#[derive(Debug)]
pub struct SandboxBindings<S: BindingStore> {
    store: S,
}

impl<S: BindingStore> SandboxBindings<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Records a fresh execution; a later write under the same id replaces it.
    pub fn attach(&self, record: BindingRecord) -> Result<()> {
        self.store.put(&BindingRecord {
            state: BindingState::Attached,
            ..record
        })
    }

    pub fn get(&self, execution_id: &str) -> Result<Option<BindingRecord>> {
        self.store.get(execution_id)
    }

    pub fn release(&self, execution_id: &str) -> Result<()> {
        self.store.release(execution_id)
    }

    pub fn live(&self, limit: usize) -> Result<Vec<BindingRecord>> {
        self.store.live(limit)
    }
}

fn io(error: std::io::Error) -> DurableError {
    DurableError::invalid(format!("the sandbox bindings cannot be stored: {error}"))
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
