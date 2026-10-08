//! JDBC-backed session state component, mirroring
//! `io.agentscope.extensions.jdbc.state`.

mod jdbc_agent_state_store;

pub use jdbc_agent_state_store::{JdbcAgentStateStore, VersionedState, UNVERSIONED};
