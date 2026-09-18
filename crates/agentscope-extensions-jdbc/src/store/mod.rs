//! JDBC-backed KV store component, mirroring `io.agentscope.extensions.jdbc.store`.

mod jdbc_store;

pub use jdbc_store::{store_value, JdbcStore};
