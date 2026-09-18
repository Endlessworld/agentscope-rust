//! Default database and collection names for MongoDB distributed store components,
//! mirroring `io.agentscope.extensions.mongodb.MongoConstants`.
//!
//! Centralised here so that all component classes reference the same source of
//! truth, preventing drift when the same collection name is referenced from
//! multiple construction paths.

/// Default database name used when no explicit name is supplied.
pub const DEFAULT_DATABASE: &str = "agentscope";

/// Collection holding agent session state documents ([`crate::MongodbAgentStateStore`]).
pub const SESSIONS_COLLECTION: &str = "agentscope_sessions";

/// Collection holding workspace filesystem key-value items ([`crate::MongodbStore`]).
pub const BASE_STORE_COLLECTION: &str = "agentscope_base";

/// GridFS bucket (and legacy single-document collection) holding sandbox
/// snapshots ([`crate::MongodbRemoteSnapshotClient`]).
pub const SNAPSHOTS_COLLECTION: &str = "agentscope_snapshots";

/// Collection holding sandbox execution lease locks
/// ([`crate::MongodbSandboxExecutionGuard`]).
pub const SANDBOX_LOCKS_COLLECTION: &str = "agentscope_sandbox_locks";
