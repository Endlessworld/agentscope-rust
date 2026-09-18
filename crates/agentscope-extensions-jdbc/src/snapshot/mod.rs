//! JDBC-backed sandbox snapshot component, mirroring
//! `io.agentscope.extensions.jdbc.snapshot`.

mod jdbc_remote_snapshot_client;
mod jdbc_snapshot_spec;

pub use jdbc_remote_snapshot_client::JdbcRemoteSnapshotClient;
pub use jdbc_snapshot_spec::JdbcSnapshotSpec;
