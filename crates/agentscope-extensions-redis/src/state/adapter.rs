//! Redis client adapter abstraction, mirroring
//! `io.agentscope.extensions.redis.state.RedisClientAdapter`.
//!
//! The Java module ships three adapters (Jedis / Lettuce / Redisson). In Rust the
//! ecosystem has a single primary async driver (`redis`), so this crate exposes one
//! canonical adapter ([`RedisConnection`]) plus the [`RedisClientAdapter`] trait so that
//! alternative backends (other drivers, in-memory test doubles, cluster wrappers) can be
//! plugged into [`super::RedisAgentStateStore`] without modification.

use async_trait::async_trait;

use agentscope_core::error::Result;

/// Unified abstraction over Redis client operations.
///
/// Method names mirror the Java `RedisClientAdapter` one-to-one so the migration is easy
/// to audit:
///
/// * String operations: [`set`](RedisClientAdapter::set), [`get`](RedisClientAdapter::get)
/// * List operations: [`right_push_list`](RedisClientAdapter::right_push_list),
///   [`range_list`](RedisClientAdapter::range_list),
///   [`get_list_length`](RedisClientAdapter::get_list_length)
/// * Set operations: [`add_to_set`](RedisClientAdapter::add_to_set),
///   [`get_set_members`](RedisClientAdapter::get_set_members),
///   [`get_set_size`](RedisClientAdapter::get_set_size)
/// * Key operations: [`key_exists`](RedisClientAdapter::key_exists),
///   [`delete_keys`](RedisClientAdapter::delete_keys),
///   [`find_keys_by_pattern`](RedisClientAdapter::find_keys_by_pattern)
/// * Scripting: [`eval_script`](RedisClientAdapter::eval_script) (atomic CAS primitives)
#[async_trait]
pub trait RedisClientAdapter: Send + Sync + std::fmt::Debug {
    /// Sets a string value (full replacement).
    async fn set(&self, key: &str, value: &str) -> Result<()>;

    /// Gets a string value, or `None` if the key does not exist.
    async fn get(&self, key: &str) -> Result<Option<String>>;

    /// Appends a value to the right end of a list.
    async fn right_push_list(&self, key: &str, value: &str) -> Result<()>;

    /// Gets a range of elements from a list (`end == -1` means all remaining elements).
    async fn range_list(&self, key: &str, start: i64, end: i64) -> Result<Vec<String>>;

    /// Returns the length of a list.
    async fn get_list_length(&self, key: &str) -> Result<i64>;

    /// Deletes one or more keys.
    async fn delete_keys(&self, keys: &[String]) -> Result<()>;

    /// Adds a member to a set.
    async fn add_to_set(&self, key: &str, member: &str) -> Result<()>;

    /// Gets all members of a set.
    async fn get_set_members(&self, key: &str) -> Result<Vec<String>>;

    /// Returns the cardinality of a set.
    async fn get_set_size(&self, key: &str) -> Result<i64>;

    /// Returns whether the given key exists.
    async fn key_exists(&self, key: &str) -> Result<bool>;

    /// Finds all keys matching a glob pattern using the `SCAN` cursor protocol.
    ///
    /// Mirrors the Java adapters' `findKeysByPattern`, which all drive `SCAN` to
    /// completion (`JedisClientAdapter` uses `UnifiedJedis.scan(cursor, params)`,
    /// Lettuce uses `ScanCursor.INITIAL` … `isFinished()`, Redisson uses
    /// `KeysScanOptions`) rather than `KEYS`. Implementations must:
    ///
    /// * match the glob `pattern` (wildcards supported, same syntax as `KEYS`),
    /// * loop until the server cursor returns to `0`,
    /// * return a **deduplicated** set of keys (`SCAN` may yield the same key more than
    ///   once).
    ///
    /// The return type `Vec<String>` is the ordered Rust projection of the Java
    /// `Set<String>`; callers that need determinism sort the result (see
    /// [`super::RedisAgentStateStore::list_session_ids`]).
    async fn find_keys_by_pattern(&self, pattern: &str) -> Result<Vec<String>>;

    /// Evaluates a Lua script atomically (`EVAL`).
    ///
    /// * `keys` maps to `KEYS[1..]`.
    /// * `args` maps to `ARGV[1..]`.
    ///
    /// Returns the integer reply, or `None` when the script returns a non-integer / nil.
    async fn eval_script(
        &self,
        script: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<Option<i64>>;

    /// Releases any resources held by the adapter (no-op for pooled/cloned clients).
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}
