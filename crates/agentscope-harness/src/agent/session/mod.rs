//! Session runtime: durable task operations, storage backends and read views.
//!
//! A session is the continuing conversation; a turn is one logical request; a run is one
//! execution of that request. Ordinary `call`/`stream_events` needs none of this — only an
//! application that accepts work now and runs it in the background does.

pub mod answer;
pub mod session;
#[cfg(feature = "rusqlite")]
pub mod sqlite;
pub mod storage;
pub mod store;

pub use answer::{SessionAnswer, SessionConfirm};
pub use session::{AgentSession, InputReceipt, Task};
#[cfg(feature = "rusqlite")]
pub use sqlite::SqliteSessionStorage;
pub use storage::{WorkspaceSessionLogStore, WorkspaceSessionStorage};
pub use store::StoreSessionStorage;
