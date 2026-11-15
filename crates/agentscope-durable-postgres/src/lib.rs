//! PostgreSQL as the durable runtime's record store.
//!
//! A durable store is three primitives — a scope transaction, a record per part, and a readiness
//! index — and PostgreSQL holds them as three tables. This crate is the mapping, not a driver: it
//! decides what a scope row is, what a part row is, how a page becomes a range scan and how a
//! commit is fenced by a revision compare-and-set. A driver supplies [`SqlSession`], which is four
//! methods, and holds no decisions of its own.
//!
//! Nothing here needs a server to be tested: the mapping is pure functions plus statements, and
//! [`testing::MemoryPg`] keeps the same facts in a map, so the whole `RecordStore` contract — and
//! a whole turn through `TxnBackend` — runs offline. What only a server can answer is in
//! `tests/schema_live.rs`, which runs when `DATABASE_URL` names a database.
//!
//! ```text
//! let store = Arc::new(PostgresStore::new(session));   // session: impl SqlSession
//! store.ensure_schema()?;                             // idempotent, safe on every connect
//! let backend = TxnBackend::with_store(store);
//! // Arc<dyn SessionBackend> from here on; nothing above knows it is PostgreSQL.
//! ```

pub mod mapping;
pub mod schema;
pub mod session;
pub mod statements;
pub mod store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use agentscope_durable::{DurableError, ErrorCode, Result};
pub use mapping::{Page, Param};
pub use session::{Row, SqlSession, SqlTransaction};
pub use store::PostgresStore;
