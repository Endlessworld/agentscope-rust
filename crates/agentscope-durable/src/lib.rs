//! Recoverable, harness-neutral execution runtime.
//!
//! A turn is a durable state machine, not a stack frame: every state change is a fenced
//! commit against a single-writer lease, and every external tool effect carries an
//! execution identity so a restart can tell `applied`, `accepted` and `unknown` apart
//! instead of guessing.
//!
//! This crate is deliberately free of any harness or model dependency; the binding to a
//! concrete agent implementation lives in `agentscope-durable-adapter-agentscope`.

pub mod backend;
pub mod delegation;
pub mod error;
pub mod event_filter;
pub mod execution_store;
pub mod executor;
pub mod harness_adapter;
mod json;
pub mod runtime;
pub mod session;
pub mod session_backend;
pub mod store;
pub mod tool_binding;
pub mod traffic;
pub mod types;
pub mod wakeups;

pub use backend::local::LocalBackend;
pub use delegation::{child_thread_id, fork_thread, DelegationExecutor};
pub use error::{DurableError, ErrorCode, Result};
pub use event_filter::{is_terminal_event, DeltaFilter, EventStreamOptions};
pub use execution_store::{
    DispatchRecord, DispatchState, ExecutionStore, MemoryExecutionStore, RecordStoreExecutionStore,
};
pub use executor::{Executor, ExecutorCapabilities, ExecutorRegistry, DEFAULT_EXECUTOR};
pub use harness_adapter::{AdapterCapabilities, EventSink, HarnessAdapter, HarnessContext};
pub use runtime::{DurableRuntime, TickOutcome};
pub use session::DurableSession;
pub use session_backend::{BackendCapabilities, SessionBackend};
pub use tool_binding::{MapCatalog, ToolBinding, ToolCatalog};
pub use types::*;
pub use wakeups::{LocalWakeups, SubscriptionId, Wakeup, WakeupReason, WakeupSink, Wakeups};

/// The only clock a durable decision may use outside a store transaction.
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
