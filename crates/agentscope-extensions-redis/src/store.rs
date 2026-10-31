//! Redis-backed [`BaseStore`], mirroring
//! `io.agentscope.extensions.redis.store.RedisStore`.
//!
//! ## Key layout
//!
//! For each item with namespace `[a, b, c]` and key `k`, two Redis keys are used:
//!
//! * **Item hash** `<prefix>item:<ns>\0<k>` — Redis hash with fields `value` (JSON-encoded
//!   object) and `version` (stringified integer).
//! * **Namespace index** `<prefix>idx:<ns>` — sorted set (all scores `0`) holding every `k`
//!   written under that namespace, enabling lexicographic search via `ZRANGEBYLEX` without
//!   scanning the keyspace.
//!
//! `<ns>` is the namespace components joined with `"\0"`.
//!
//! ## Concurrency
//!
//! [`RedisStore::put`] and [`BaseStore::put_if_version`] both run as a single Lua `EVAL`,
//! making version read + hash write + index update one atomic operation. This makes
//! `put_if_version` safe as a distributed compare-and-swap primitive, matching the harness
//! [`BaseStore`] contract (`expected_version == 0` ⇒ create-only-if-absent).
//!
//! ## Seams
//!
//! The harness [`BaseStore`] trait is **synchronous**, while the `redis` crate is async.
//! Instead of bridging sync→async on every call, the store talks to the crate-internal
//! synchronous [`StoreCommands`] seam: the production implementation
//! ([`BlockingRedisCommands`]) owns a blocking [`redis::Connection`], and offline tests
//! inject an in-memory double. This keeps the trait shape natural *and* makes every branch
//! of this module testable without a server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::error::{Error, Result};
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

/// Default Redis key prefix.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:store:";

const NS_SEPARATOR: &str = "\0";

/// Atomic put: increments the version unconditionally, writes value + version + index entry.
pub(crate) const PUT_SCRIPT: &str =
    "local v = tonumber(redis.call('HGET', KEYS[1], 'version') or '0') + 1 \
     redis.call('HSET', KEYS[1], 'value', ARGV[1], 'version', tostring(v)) \
     redis.call('ZADD', KEYS[2], 0, ARGV[2]) \
     return tostring(v)";

/// Atomic CAS put: writes only when the stored version equals `ARGV[3]`.
pub(crate) const PUT_IF_VERSION_SCRIPT: &str =
    "local cur = tonumber(redis.call('HGET', KEYS[1], 'version') or '0') \
     if cur ~= tonumber(ARGV[3]) then return '0' end \
     local v = cur + 1 \
     redis.call('HSET', KEYS[1], 'value', ARGV[1], 'version', tostring(v)) \
     redis.call('ZADD', KEYS[2], 0, ARGV[2]) \
     return tostring(v)";

/// Atomic delete: removes both the item hash and its index membership.
pub(crate) const DELETE_SCRIPT: &str =
    "redis.call('DEL', KEYS[1]) redis.call('ZREM', KEYS[2], ARGV[1]) return 1";

/// Synchronous command surface the [`BaseStore`] implementation requires.
///
/// This is an implementation seam (`pub(crate)`), not public API.
pub(crate) trait StoreCommands: Send + Sync + std::fmt::Debug {
    /// `HGETALL key`.
    fn hgetall(&self, key: &str) -> Result<HashMap<String, String>>;

    /// `EVAL script 2 key1 key2 argv...`.
    fn eval_two_keys(
        &self,
        script: &str,
        key1: &str,
        key2: &str,
        args: &[String],
    ) -> Result<redis::Value>;

    /// `ZRANGEBYLEX key - + LIMIT offset limit`.
    fn zrangebylex(&self, key: &str, offset: usize, limit: usize) -> Result<Vec<String>>;

    /// `PING` — validates a freshly opened connection.
    fn ping(&self) -> Result<()>;

    /// `CLIENT ID` — the server-side id of the connection currently in use.
    ///
    /// Diagnostics only: it opens the lazily wired connection, and after a reconnect the id
    /// changes, which is how the live test proves the socket was rebuilt (not reused).
    fn client_id(&self) -> Result<i64>;
}

/// Production [`StoreCommands`] backed by a blocking [`redis::Connection`].
///
/// The connection is opened **lazily** on first use, so constructing a `RedisStore`
/// performs no I/O. [`RedisStore::connect`] additionally issues `PING` to validate the
/// endpoint eagerly, while [`RedisStore::connect_lazy`] (used by
/// [`crate::RedisDistributedStore::base_store`]) stays pure wiring.
///
/// The connection is **not** permanent: a command that fails because the socket died
/// evicts it and retries once on a fresh connection (see [`run_with_reconnect`]). Java gets
/// the same behaviour for free from `JedisPooled`, which takes a pooled connection per
/// command.
pub(crate) struct BlockingRedisCommands {
    /// Normalised URL used to open the connection on first use.
    url: String,
    conn: Mutex<Option<redis::Connection>>,
}

impl std::fmt::Debug for BlockingRedisCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let connected = self.conn.lock().map(|c| c.is_some()).unwrap_or(false);
        f.debug_struct("BlockingRedisCommands")
            .field("url", &self.url)
            .field("connected", &connected)
            .finish()
    }
}

fn normalize_url(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("redis://{url}")
    }
}

impl BlockingRedisCommands {
    /// Records a URL for a connection opened on first use. Does **not** connect.
    pub fn open(url: &str) -> Result<Self> {
        let normalized = normalize_url(url);
        // Validate the URL shape without touching the network.
        redis::Client::open(normalized.clone())
            .map_err(|e| Error::Other(format!("Invalid Redis URL: {e}")))?;
        Ok(Self {
            url: normalized,
            conn: Mutex::new(None),
        })
    }

    /// Wraps an already-open blocking connection.
    pub fn new(conn: redis::Connection) -> Self {
        Self {
            url: String::new(),
            conn: Mutex::new(Some(conn)),
        }
    }

    /// Runs `f` against a live connection, opening one on first use and rebuilding it once
    /// if the command fails because the socket died.
    fn with_conn<T>(
        &self,
        context: &str,
        f: impl FnMut(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> Result<T> {
        let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        run_with_reconnect(&mut guard, context, || open_blocking_connection(&self.url), f)
    }
}

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
}

/// True for errors that mean the socket itself is unusable: server restart, failover,
/// firewall dropping an idle connection, keep-alive timeout.
///
/// Such a connection can never serve another command, so it is dropped instead of reused.
fn is_connection_failure(error: &redis::RedisError) -> bool {
    error.is_io_error() || error.is_connection_dropped()
}

/// Opens a blocking connection to an already normalised URL.
fn open_blocking_connection(url: &str) -> Result<redis::Connection> {
    if url.trim().is_empty() {
        return Err(Error::Other(
            "Redis connection was supplied pre-opened and cannot be reconnected (no URL)".into(),
        ));
    }
    let client = redis::Client::open(url.to_string())
        .map_err(|error| Error::Other(format!("Invalid Redis URL: {error}")))?;
    client
        .get_connection()
        .map_err(|error| map_err("connect", error))
}

/// Runs `op` against the connection cached in `slot`, opening one on first use and
/// **evicting it and retrying once** when the command fails for a connection-level reason.
///
/// The harness [`BaseStore`] trait is synchronous, so this store holds one blocking
/// connection instead of taking a pooled connection per command (which is what Java's
/// `JedisPooled` does, and why it recovers from a restart for free). Without the eviction a
/// single socket error would make every later workspace KV operation fail for the rest of
/// the process lifetime.
///
/// Only *connection* failures are retried ([`is_connection_failure`]): a command that
/// reached the server and failed (a Lua error, a wrong type, a CAS conflict) must not run
/// twice. The connection type is generic so the eviction/retry rule is covered offline with
/// a fake connection (see the module tests).
fn run_with_reconnect<T, C>(
    slot: &mut Option<C>,
    context: &str,
    mut connect: impl FnMut() -> Result<C>,
    mut op: impl FnMut(&mut C) -> redis::RedisResult<T>,
) -> Result<T> {
    if slot.is_none() {
        *slot = Some(connect()?);
    }
    let attempt = {
        let conn = slot.as_mut().expect("connection initialised above");
        op(conn)
    };
    match attempt {
        Ok(value) => Ok(value),
        Err(error) if is_connection_failure(&error) => {
            tracing::warn!(
                "Redis {context} hit a dead connection ({error}); reconnecting and retrying once"
            );
            // Drop the dead socket first: reconnecting over it would fail again.
            *slot = None;
            let conn = slot.insert(connect()?);
            op(conn).map_err(|error| map_err(context, error))
        }
        Err(error) => Err(map_err(context, error)),
    }
}

impl StoreCommands for BlockingRedisCommands {
    fn hgetall(&self, key: &str) -> Result<HashMap<String, String>> {
        self.with_conn("HGETALL", |conn| redis::cmd("HGETALL").arg(key).query(conn))
    }

    fn eval_two_keys(
        &self,
        script: &str,
        key1: &str,
        key2: &str,
        args: &[String],
    ) -> Result<redis::Value> {
        self.with_conn("EVAL", |conn| {
            let mut cmd = redis::cmd("EVAL");
            cmd.arg(script).arg(2).arg(key1).arg(key2);
            for a in args {
                cmd.arg(a);
            }
            cmd.query(conn)
        })
    }

    fn zrangebylex(&self, key: &str, offset: usize, limit: usize) -> Result<Vec<String>> {
        self.with_conn("ZRANGEBYLEX", |conn| {
            redis::cmd("ZRANGEBYLEX")
                .arg(key)
                .arg("-")
                .arg("+")
                .arg("LIMIT")
                .arg(offset as i64)
                .arg(limit as i64)
                .query(conn)
        })
    }

    fn ping(&self) -> Result<()> {
        self.with_conn("PING", |conn| {
            redis::cmd("PING").query::<String>(conn).map(|_| ())
        })
    }

    fn client_id(&self) -> Result<i64> {
        self.with_conn("CLIENT ID", |conn| {
            redis::cmd("CLIENT").arg("ID").query(conn)
        })
    }
}

/// Redis-backed [`BaseStore`] implementation.
///
/// The harness [`BaseStore`] trait is **synchronous**; this store therefore owns a
/// blocking connection through [`BlockingRedisCommands`]. Use [`RedisStore::connect`] to
/// create one from a URL.
pub struct RedisStore {
    commands: Arc<dyn StoreCommands>,
    key_prefix: String,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return DEFAULT_KEY_PREFIX.to_string();
    }
    if trimmed.ends_with(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:")
    }
}

impl RedisStore {
    /// Connects to Redis using a URL such as `redis://localhost:6379` or `localhost:6379`.
    ///
    /// The connection is opened eagerly and validated with `PING`.
    pub fn connect(url: &str, key_prefix: &str) -> Result<Self> {
        let store = Self::connect_lazy(url, key_prefix)?;
        store.commands.ping()?;
        Ok(store)
    }

    /// Like [`RedisStore::connect`] but does **not** touch the server: the blocking
    /// connection is created on first use. Used by
    /// [`crate::RedisDistributedStore::base_store`] so a combiner can hand out a
    /// synchronous store without performing I/O at wiring time.
    pub(crate) fn connect_lazy(url: &str, key_prefix: &str) -> Result<Self> {
        let commands = BlockingRedisCommands::open(url)?;
        Ok(Self::from_commands(Arc::new(commands), key_prefix))
    }

    /// Wraps an already-open blocking connection.
    pub fn from_connection(conn: redis::Connection, key_prefix: &str) -> Self {
        Self::from_commands(Arc::new(BlockingRedisCommands::new(conn)), key_prefix)
    }

    /// Builds a store over an arbitrary command backend (the production blocking
    /// connection, or in tests an in-memory double).
    pub(crate) fn from_commands(commands: Arc<dyn StoreCommands>, key_prefix: &str) -> Self {
        Self {
            commands,
            key_prefix: normalize_prefix(key_prefix),
        }
    }

    /// The configured, normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The server-side id (`CLIENT ID`) of the blocking connection the store currently
    /// holds.
    ///
    /// Diagnostics only: it opens the lazily wired connection if needed. After a connection
    /// failure the id changes, which is how the live reconnect test proves the dead socket
    /// was rebuilt rather than reused.
    pub fn client_id(&self) -> Result<i64> {
        self.commands.client_id()
    }

    // ---- key helpers -------------------------------------------------------

    fn item_key(&self, namespace: &[String], key: &str) -> String {
        format!(
            "{}item:{}{NS_SEPARATOR}{key}",
            self.key_prefix,
            namespace_path(namespace)
        )
    }

    fn index_key(&self, namespace: &[String]) -> String {
        format!("{}idx:{}", self.key_prefix, namespace_path(namespace))
    }

    fn to_item(key: &str, hash: &HashMap<String, String>) -> StoreItem {
        let json = hash.get("value");
        let version = hash
            .get("version")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let value = deserialize(json);
        StoreItem::new(key, value, version)
    }
}

/// Serialises a store value to JSON text.
fn serialize(value: &StoreValue) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|e| Error::Other(format!("Failed to JSON-encode store value: {e}")))
}

/// Deserialises JSON text to a store value, defaulting to an empty map.
fn deserialize(json: Option<&String>) -> StoreValue {
    match json {
        None => StoreValue::new(),
        Some(s) if s.is_empty() => StoreValue::new(),
        Some(s) => serde_json::from_str::<StoreValue>(s).unwrap_or_else(|_| StoreValue::new()),
    }
}

fn namespace_path(namespace: &[String]) -> String {
    namespace.join(NS_SEPARATOR)
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::Other("key must not be null or empty".into()));
    }
    if key.contains('\0') {
        return Err(Error::Other(
            "key must not contain the NUL character".into(),
        ));
    }
    Ok(())
}

/// Coerces a Redis `EVAL` reply into the returned version string.
fn eval_reply_to_version(reply: redis::Value) -> Option<String> {
    use redis::Value;
    match reply {
        Value::Int(i) => Some(i.to_string()),
        Value::BulkString(bytes) => String::from_utf8(bytes).ok(),
        Value::SimpleString(s) => Some(s),
        Value::Nil => None,
        _ => None,
    }
}

impl BaseStore for RedisStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        if validate_key(key).is_err() {
            return None;
        }
        let item_key = self.item_key(namespace, key);
        let hash = self.commands.hgetall(&item_key).ok()?;
        if hash.is_empty() {
            return None;
        }
        Some(Self::to_item(key, &hash))
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if validate_key(key).is_err() {
            return;
        }
        let json = match serialize(&value) {
            Ok(j) => j,
            Err(_) => return,
        };
        let item_key = self.item_key(namespace, key);
        let idx_key = self.index_key(namespace);
        let _ =
            self.commands
                .eval_two_keys(PUT_SCRIPT, &item_key, &idx_key, &[json, key.to_string()]);
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        if validate_key(key).is_err() {
            return false;
        }
        let json = match serialize(&value) {
            Ok(j) => j,
            Err(_) => return false,
        };
        let item_key = self.item_key(namespace, key);
        let idx_key = self.index_key(namespace);
        let reply = match self.commands.eval_two_keys(
            PUT_IF_VERSION_SCRIPT,
            &item_key,
            &idx_key,
            &[json, key.to_string(), expected_version.to_string()],
        ) {
            Ok(v) => v,
            Err(_) => return false,
        };
        match eval_reply_to_version(reply) {
            Some(v) => v != "0",
            None => false,
        }
    }

    /// `put_if_version` runs as one server-side script, so the compare and the write cannot
    /// interleave (Java `supportsAtomicSessionStorage`).
    fn supports_atomic_session_storage(&self) -> bool {
        true
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        if limit == 0 {
            return Vec::new();
        }
        let idx_key = self.index_key(namespace);
        let keys = match self.commands.zrangebylex(&idx_key, offset, limit) {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let mut items = Vec::with_capacity(keys.len());
        for k in keys {
            let item_key = self.item_key(namespace, &k);
            let hash = match self.commands.hgetall(&item_key) {
                Ok(h) => h,
                Err(_) => continue,
            };
            if hash.is_empty() {
                continue; // stale index entry — tolerated
            }
            items.push(Self::to_item(&k, &hash));
        }
        items
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if validate_key(key).is_err() {
            return;
        }
        let item_key = self.item_key(namespace, key);
        let idx_key = self.index_key(namespace);
        let _ = self
            .commands
            .eval_two_keys(DELETE_SCRIPT, &item_key, &idx_key, &[key.to_string()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryStoreCommands;
    use serde_json::json;

    fn ns(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn value(k: &str, v: i64) -> StoreValue {
        let mut map = StoreValue::new();
        map.insert(k.to_string(), json!(v));
        map
    }

    fn store() -> (RedisStore, Arc<MemoryStoreCommands>) {
        let commands = Arc::new(MemoryStoreCommands::new());
        (RedisStore::from_commands(commands.clone(), "t:"), commands)
    }

    #[test]
    fn normalize_prefix_rules() {
        assert_eq!(normalize_prefix(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("  "), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("app:"), "app:");
        assert_eq!(normalize_prefix("app"), "app:");
    }

    #[test]
    fn namespace_path_joins_with_nul() {
        let ns = vec!["a".to_string(), "b".to_string()];
        assert_eq!(namespace_path(&ns), "a\0b");
    }

    #[test]
    fn validate_key_rejects_empty_and_nul() {
        assert!(validate_key("").is_err());
        assert!(validate_key("a\0b").is_err());
        assert!(validate_key("ok").is_ok());
    }

    #[test]
    fn deserialize_defaults_to_empty_map() {
        assert_eq!(deserialize(None), StoreValue::new());
        assert_eq!(deserialize(Some(&String::new())), StoreValue::new());
        let mut expected = StoreValue::new();
        expected.insert("k".into(), serde_json::json!(1));
        assert_eq!(deserialize(Some(&r#"{"k":1}"#.to_string())), expected);
    }

    #[test]
    fn item_and_index_keys_use_prefix_and_separator() {
        let ns = vec!["ns1".to_string(), "ns2".to_string()];
        // Pure string-format key helpers (no live connection required).
        assert_eq!(
            format!("{}item:{}{NS_SEPARATOR}key", "p:", namespace_path(&ns)),
            format!("p:item:{}\0key", namespace_path(&ns)),
        );
        assert_eq!(
            format!("{}idx:{}", "p:", namespace_path(&ns)),
            "p:idx:ns1\0ns2",
        );
    }

    #[test]
    fn eval_reply_to_version_accepts_bulk_string() {
        // PUT_SCRIPT returns `tostring(v)`, i.e. a bulk string, not an integer.
        assert_eq!(
            eval_reply_to_version(redis::Value::BulkString(b"3".to_vec())),
            Some("3".into())
        );
        assert_eq!(
            eval_reply_to_version(redis::Value::Int(4)),
            Some("4".into())
        );
        assert_eq!(eval_reply_to_version(redis::Value::Nil), None);
    }

    // ---- BaseStore behaviour (offline, in-memory backend) -----------------

    #[test]
    fn put_get_round_trip_and_index_entry() {
        let (store, commands) = store();
        let ns = ns(&["files"]);
        store.put(&ns, "a.txt", value("size", 1));

        let item = store.get(&ns, "a.txt").expect("item present");
        assert_eq!(item.key, "a.txt");
        assert_eq!(item.value.get("size"), Some(&json!(1)));
        assert_eq!(item.version, 1, "version starts at 1 on first write");
        assert_eq!(
            commands.index_members("t:idx:files"),
            vec!["a.txt".to_string()]
        );
        assert_eq!(
            commands
                .hash_of("t:item:files\0a.txt")
                .get("version")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn put_bumps_version_for_same_key_and_keeps_single_index_entry() {
        let (store, commands) = store();
        let ns = ns(&["files"]);
        store.put(&ns, "a", value("n", 1));
        store.put(&ns, "a", value("n", 2));

        assert_eq!(store.get(&ns, "a").unwrap().version, 2);
        assert_eq!(store.get(&ns, "a").unwrap().value.get("n"), Some(&json!(2)));
        assert_eq!(
            commands.index_members("t:idx:files"),
            vec!["a".to_string()],
            "ZADD is idempotent"
        );
    }

    #[test]
    fn get_missing_key_and_invalid_key_return_none() {
        let (store, commands) = store();
        let ns = ns(&["files"]);
        assert!(store.get(&ns, "nope").is_none());
        assert!(
            store.get(&ns, "").is_none(),
            "empty key is rejected before I/O"
        );
        assert!(store.get(&ns, "a\0b").is_none(), "NUL in key is rejected");
        assert_eq!(commands.eval_calls(), 0);
    }

    #[test]
    fn put_if_version_create_only_when_absent() {
        let (store, _commands) = store();
        let ns = ns(&["files"]);
        // expected 0 on an absent key: the harness contract is "create only if absent".
        assert!(store.put_if_version(&ns, "k", value("v", 1), 0));
        assert_eq!(store.get(&ns, "k").unwrap().version, 1);
        // second attempt with expected 0 must now fail (version is 1)
        assert!(!store.put_if_version(&ns, "k", value("v", 2), 0));
        assert_eq!(store.get(&ns, "k").unwrap().value.get("v"), Some(&json!(1)));
    }

    #[test]
    fn put_if_version_matching_version_succeeds_and_bumps() {
        let (store, _commands) = store();
        let ns = ns(&["files"]);
        store.put(&ns, "k", value("v", 1));
        assert!(store.put_if_version(&ns, "k", value("v", 2), 1));
        assert_eq!(store.get(&ns, "k").unwrap().version, 2);
        assert_eq!(store.get(&ns, "k").unwrap().value.get("v"), Some(&json!(2)));
    }

    #[test]
    fn put_if_version_stale_version_is_rejected() {
        let (store, _commands) = store();
        let ns = ns(&["files"]);
        store.put(&ns, "k", value("v", 1));
        assert!(!store.put_if_version(&ns, "k", value("v", 9), 7));
        assert_eq!(store.get(&ns, "k").unwrap().version, 1, "version unchanged");
    }

    #[test]
    fn put_if_version_rejects_invalid_key_without_io() {
        let (store, commands) = store();
        let ns = ns(&["files"]);
        assert!(!store.put_if_version(&ns, "", value("v", 1), 0));
        assert_eq!(commands.eval_calls(), 0);
    }

    #[test]
    fn search_paginates_in_index_order() {
        let (store, _commands) = store();
        let ns = ns(&["ns1", "ns2"]);
        for k in ["d", "a", "c", "b"] {
            store.put(&ns, k, value("n", 1));
        }
        let page1: Vec<String> = store.search(&ns, 2, 0).into_iter().map(|i| i.key).collect();
        let page2: Vec<String> = store.search(&ns, 2, 2).into_iter().map(|i| i.key).collect();
        let page3: Vec<String> = store.search(&ns, 2, 4).into_iter().map(|i| i.key).collect();
        assert_eq!(
            page1,
            vec!["d", "a"],
            "index preserves insertion order (all scores 0)"
        );
        assert_eq!(page2, vec!["c", "b"]);
        assert!(page3.is_empty(), "offset past the end yields nothing");
    }

    #[test]
    fn search_with_zero_limit_and_unknown_namespace_is_empty() {
        let (store, _commands) = store();
        let files = ns(&["ns"]);
        store.put(&files, "a", value("n", 1));
        assert!(
            store.search(&files, 0, 0).is_empty(),
            "limit 0 performs no query"
        );
        assert!(store.search(&ns(&["other"]), 10, 0).is_empty());
    }

    #[test]
    fn search_skips_stale_index_entries() {
        let (store, commands) = store();
        let ns = ns(&["ns"]);
        // Index entry with no backing hash simulates an externally deleted item.
        commands.seed_index("t:idx:ns", "ghost");
        store.put(&ns, "real", value("n", 1));
        let found: Vec<String> = store
            .search(&ns, 10, 0)
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(found, vec!["real"]);
    }

    #[test]
    fn delete_removes_item_and_index_membership() {
        let (store, commands) = store();
        let ns = ns(&["ns"]);
        store.put(&ns, "a", value("n", 1));
        store.put(&ns, "b", value("n", 1));
        store.delete(&ns, "a");

        assert!(store.get(&ns, "a").is_none());
        assert!(!commands.contains("t:item:ns\0a"));
        assert_eq!(commands.index_members("t:idx:ns"), vec!["b".to_string()]);
        assert!(store.get(&ns, "b").is_some(), "unrelated item untouched");
    }

    #[test]
    fn delete_invalid_key_is_noop() {
        let (store, commands) = store();
        let ns = ns(&["ns"]);
        store.delete(&ns, "");
        store.delete(&ns, "a\0b");
        assert_eq!(commands.eval_calls(), 0);
    }

    #[test]
    fn unavailable_backend_degrades_to_empty_results() {
        // BaseStore is infallible: an unreachable backend must not panic.
        let commands = Arc::new(MemoryStoreCommands::unreachable());
        assert!(commands.ping().is_err());
        let store = RedisStore::from_commands(commands, "t:");
        let ns = ns(&["ns"]);
        assert!(store.get(&ns, "k").is_none());
        assert!(store.search(&ns, 10, 0).is_empty());
        assert!(!store.put_if_version(&ns, "k", value("n", 1), 0));
    }

    #[test]
    fn corrupt_stored_json_degrades_to_empty_map() {
        let (store, commands) = store();
        let ns = ns(&["ns"]);
        commands.seed_item("t:item:ns\0k", "{not json", 4);
        let item = store.get(&ns, "k").expect("hash exists");
        assert!(item.value.is_empty());
        assert_eq!(item.version, 4, "version is still surfaced");
    }

    // ---- connection eviction / reconnect (REDIS-03) ----------------------

    /// Stand-in for a blocking `redis::Connection`: only the identity matters, because the
    /// rule under test is "which socket does the next command run on".
    #[derive(Debug)]
    struct FakeConnection {
        id: u32,
    }

    /// A dead socket: `is_io_error()` is true, i.e. exactly what a Redis restart / failover
    /// makes `redis::Connection` return.
    fn dead_socket() -> redis::RedisError {
        redis::RedisError::from(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "connection reset by peer",
        ))
    }

    #[test]
    fn a_dead_connection_is_evicted_and_the_command_is_retried_once() {
        let mut slot: Option<FakeConnection> = None;
        let mut opened = 0u32;
        let mut calls = 0u32;

        let id = run_with_reconnect(
            &mut slot,
            "HGETALL",
            || {
                opened += 1;
                Ok(FakeConnection { id: opened })
            },
            |conn| {
                calls += 1;
                if calls == 1 {
                    Err(dead_socket())
                } else {
                    Ok(conn.id)
                }
            },
        )
        .expect("the retried command succeeds on the fresh connection");

        assert_eq!(id, 2, "the retry ran on the newly opened connection");
        assert_eq!(opened, 2, "the dead socket must be replaced");
        assert_eq!(calls, 2, "exactly one retry");
        assert_eq!(
            slot.as_ref().map(|conn| conn.id),
            Some(2),
            "the fresh connection is cached for later commands"
        );
    }

    #[test]
    fn a_command_error_that_is_not_a_connection_error_is_never_retried() {
        let mut slot: Option<FakeConnection> = None;
        let mut opened = 0u32;
        let mut calls = 0u32;

        let error = run_with_reconnect::<i64, FakeConnection>(
            &mut slot,
            "EVAL",
            || {
                opened += 1;
                Ok(FakeConnection { id: opened })
            },
            |_conn| {
                calls += 1;
                Err(redis::RedisError::from((
                    redis::ErrorKind::UnexpectedReturnType,
                    "expected a string reply",
                )))
            },
        )
        .expect_err("a real server error must surface");

        assert_eq!(calls, 1, "a command that reached the server is not re-sent");
        assert_eq!(opened, 1);
        assert!(slot.is_some(), "a live connection is kept");
        assert!(error.to_string().contains("Redis EVAL"), "{error}");
    }

    #[test]
    fn a_second_connection_error_gives_up_after_one_retry() {
        let mut slot: Option<FakeConnection> = None;
        let mut opened = 0u32;
        let mut calls = 0u32;

        let error = run_with_reconnect::<i64, FakeConnection>(
            &mut slot,
            "ZRANGEBYLEX",
            || {
                opened += 1;
                Ok(FakeConnection { id: opened })
            },
            |_conn| {
                calls += 1;
                Err(dead_socket())
            },
        )
        .expect_err("two dead sockets must fail instead of looping");

        assert_eq!(calls, 2, "one retry, then give up");
        assert_eq!(opened, 2, "no extra connection is opened");
        assert!(error.to_string().contains("Redis ZRANGEBYLEX"), "{error}");
    }

    #[test]
    fn a_preopened_connection_reports_that_it_cannot_be_reconnected() {
        // `RedisStore::from_connection` has no URL, so a dead socket cannot be rebuilt: the
        // failure must say so instead of surfacing an empty-URL parse error.
        // (`redis::Connection` has no `Debug`, so `expect_err` is not usable here.)
        let error = match open_blocking_connection("") {
            Ok(_) => panic!("a pre-opened connection has no URL to reconnect to"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("cannot be reconnected"),
            "{error}"
        );
    }
}
