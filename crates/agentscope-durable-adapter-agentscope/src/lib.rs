//! Binds the durable runtime to the AgentScope core/harness contracts.
//!
//! The bridge owns the *same* transaction on both sides: a native journal batch and the turn
//! revision it belongs to are committed by one durable commit, so a reader never sees one
//! without the other.

pub mod backend_thread_journal;
mod checkpoint;
pub mod execution_host;
mod tool_facts;

pub use backend_thread_journal::BackendThreadJournal;
pub use execution_host::{
    pin, AgentScopeExecutionHost, FINALIZATION_COMPLETED, FINALIZATION_STARTED,
};
