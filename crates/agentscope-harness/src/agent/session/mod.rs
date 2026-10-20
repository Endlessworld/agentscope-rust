//! Session runtime: durable task operations, storage backends and read views.
//!
//! A session is the continuing conversation; a turn is one logical request; a run is one
//! execution of that request. Ordinary `call`/`stream_events` needs none of this — only an
//! application that accepts work now and runs it in the background does.

pub mod answer;
pub mod session;
pub mod storage;

pub use answer::{SessionAnswer, SessionConfirm};
pub use session::{AgentSession, InputReceipt, Task};
pub use storage::{WorkspaceSessionLogStore, WorkspaceSessionStorage};
