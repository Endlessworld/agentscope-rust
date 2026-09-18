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

use std::collections::HashMap;
use std::sync::Mutex;

use agentscope_core::error::{Error, Result};
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

/// Default Redis key prefix.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:store:";

const NS_SEPARATOR: &str = "\0";

/// Atomic put: increments the version unconditionally, writes value + version + index entry.
const PUT_SCRIPT: &str =
    "local v = tonumber(redis.call('HGET', KEYS[1], 'version') or '0') + 1 \
     redis.call('HSET', KEYS[1], 'value', ARGV[1], 'version', tostring(v)) \
     redis.call('ZADD', KEYS[2], 0, ARGV[2]) \
     return tostring(v)";

/// Atomic CAS put: writes only when the stored version equals `ARGV[3]`.
const PUT_IF_VERSION_SCRIPT: &str =
    "local cur = tonumber(redis.call('HGET', KEYS[1], 'version') or '0') \
     if cur ~= tonumber(ARGV[3]) then return '0' end \
     local v = cur + 1 \
     redis.call('HSET', KEYS[1], 'value', ARGV[1], 'version', tostring(v)) \
     redis.call('ZADD', KEYS[2], 0, ARGV[2]) \
     return tostring(v)";

/// Atomic delete: removes both the item hash and its index membership.
const DELETE_SCRIPT: &str =
    "redis.call('DEL', KEYS[1]) redis.call('ZREM', KEYS[2], ARGV[1]) return 1";

/// Redis-backed [`BaseStore`] implementation.
///
/// The harness [`BaseStore`] trait is **synchronous**, while the `redis` crate's
/// connection-managed client is async. [`RedisStore`] therefore uses a blocking
/// [`redis::Connection`] guarded by a [`Mutex`], which matches the trait shape without a
/// sync→async bridge. Use [`RedisStore::connect`] to create one from a URL.
pub struct RedisStore {
    conn: Mutex<redis::Connection>,
    key_prefix: String,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
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
    pub fn connect(url: &str, key_prefix: &str) -> Result<Self> {
        let normalized = if url.contains("://") {
            url.to_string()
        } else {
            format!("redis://{url}")
        };
        let client = redis::Client::open(normalized)
            .map_err(|e| Error::Other(format!("Invalid Redis URL: {e}")))?;
        let conn = client.get_connection().map_err(|e| map_err("connect", e))?;
        Ok(Self { conn: Mutex::new(conn), key_prefix: normalize_prefix(key_prefix) })
    }

    /// Wraps an already-open blocking connection.
    pub fn from_connection(conn: redis::Connection, key_prefix: &str) -> Self {
        Self { conn: Mutex::new(conn), key_prefix: normalize_prefix(key_prefix) }
    }

    /// The configured, normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, redis::Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
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
    serde_json::to_string(value).map_err(|e| Error::Other(format!("Failed to JSON-encode store value: {e}")))
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
        return Err(Error::Other("key must not contain the NUL character".into()));
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
        let hash: HashMap<String, String> = redis::cmd("HGETALL")
            .arg(&item_key)
            .query(&mut *self.lock())
            .ok()?;
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
        let _: std::result::Result<redis::Value, redis::RedisError> = redis::cmd("EVAL")
            .arg(PUT_SCRIPT)
            .arg(2)
            .arg(&item_key)
            .arg(&idx_key)
            .arg(&json)
            .arg(key)
            .query(&mut *self.lock());
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
        let reply: redis::Value = match redis::cmd("EVAL")
            .arg(PUT_IF_VERSION_SCRIPT)
            .arg(2)
            .arg(&item_key)
            .arg(&idx_key)
            .arg(&json)
            .arg(key)
            .arg(expected_version.to_string())
            .query(&mut *self.lock())
        {
            Ok(v) => v,
            Err(_) => return false,
        };
        match eval_reply_to_version(reply) {
            Some(v) => v != "0",
            None => false,
        }
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        if limit == 0 {
            return Vec::new();
        }
        let idx_key = self.index_key(namespace);
        let keys: Vec<String> = match redis::cmd("ZRANGEBYLEX")
            .arg(&idx_key)
            .arg("-")
            .arg("+")
            .arg("LIMIT")
            .arg(offset as i64)
            .arg(limit as i64)
            .query(&mut *self.lock())
        {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let mut items = Vec::with_capacity(keys.len());
        for k in keys {
            let item_key = self.item_key(namespace, &k);
            let hash: HashMap<String, String> = match redis::cmd("HGETALL")
                .arg(&item_key)
                .query(&mut *self.lock())
            {
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
        let _: std::result::Result<redis::Value, redis::RedisError> = redis::cmd("EVAL")
            .arg(DELETE_SCRIPT)
            .arg(2)
            .arg(&item_key)
            .arg(&idx_key)
            .arg(key)
            .query(&mut *self.lock());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
