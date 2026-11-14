//! Session runtime: journal storage backends and read views.
//!
//! A session is the continuing conversation; a turn is one logical request; a run is one
//! execution of that request. Driving a journalled turn is the durable runtime's job
//! (`agentscope-durable`); this module only provides the backends the journal is persisted to.

#[cfg(feature = "rusqlite")]
pub mod sqlite;
pub mod storage;
pub mod store;

#[cfg(feature = "rusqlite")]
pub use sqlite::SqliteJournalStorage;
pub use storage::{WorkspaceJournalStorage, WorkspaceThreadJournalStore};
pub use store::StoreJournalStorage;
