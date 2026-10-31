//! In-memory test doubles (`#[cfg(test)]` only) so every REDIS component can be
//! exercised **offline** — no server, no sockets, no network.
//!
//! Three production seams are mirrored:
//!
//! * [`MemoryRedisAdapter`] → [`crate::state::adapter::RedisClientAdapter`] (state store)
//! * [`MemoryRedisCommands`] → [`crate::raw_client::RedisCommands`] (snapshot + sandbox guard)
//! * [`MemoryStoreCommands`] → [`crate::store::StoreCommands`] (filesystem KV store)
//!
//! The data model mirrors exactly the Redis commands the production code issues, and the
//! Lua scripts are interpreted with their real semantics, so a test that drives a double
//! exercises the genuine call sequence, key layout and script dispatch of the code under
//! test.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_core::error::{Error, Result};
use redis::Value;

use crate::raw_client::RedisCommands;
use crate::state::adapter::RedisClientAdapter;
use crate::state::connection::{ScanNodeSource, ScanPager};
use crate::store::StoreCommands;

/// Redis glob matching (`*`, `?`), the same syntax `KEYS` / `SCAN MATCH` accepts.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    fn helper(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| helper(&p[1..], &t[i..])),
            Some('?') => !t.is_empty() && helper(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && helper(&p[1..], &t[1..]),
        }
    }
    helper(
        &pattern.chars().collect::<Vec<char>>(),
        &text.chars().collect::<Vec<char>>(),
    )
}

fn as_string(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

fn push_unique(vec: &mut Vec<String>, value: &str) {
    if !vec.iter().any(|v| v == value) {
        vec.push(value.to_string());
    }
}

/// Shared in-memory Redis state plus behaviour toggles and call counters.
#[derive(Debug, Default)]
pub(crate) struct MemoryRedisData {
    pub strings: HashMap<String, Vec<u8>>,
    pub hashes: HashMap<String, HashMap<String, String>>,
    /// Namespace index (the Redis sorted set `idx:<ns>`), insertion-ordered.
    pub indexes: HashMap<String, Vec<String>>,
    pub expirations: HashMap<String, i64>,
    // --- error injection ---------------------------------------------------
    pub fail_all: bool,
    pub fail_set_nx: bool,
    pub fail_set_bytes: bool,
    pub fail_expire: bool,
    pub fail_release_script: bool,
    /// `exists` on the raw command surface returns a transport error.
    pub fail_exists: bool,
    /// `get` returns a transport error (read-path failure injection).
    pub fail_get: bool,
    /// `eval_script` returns a transport error (CAS script path failure injection).
    pub fail_eval: bool,
    // --- call counters -----------------------------------------------------
    pub adapter_eval_calls: usize,
    pub adapter_delete_calls: usize,
    /// Total number of keys passed to `delete_keys` across all calls.
    pub adapter_deleted_key_total: usize,
    pub adapter_max_delete_batch: usize,
    pub adapter_push_calls: usize,
    pub adapter_key_exists_calls: usize,
    pub adapter_set_size_calls: usize,
    pub raw_eval_calls: usize,
    pub store_eval_calls: usize,
    pub set_nx_calls: usize,
    pub set_bytes_calls: usize,
    pub expire_calls: usize,
    pub exists_calls: usize,
    pub ping_calls: usize,
}

impl MemoryRedisData {
    fn all_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self.strings.keys().cloned().collect();
        out.sort();
        out
    }

    fn string_of(&self, key: &str) -> Option<String> {
        self.strings.get(key).and_then(|b| as_string(b))
    }

    fn int_of(&self, key: &str) -> i64 {
        self.string_of(key)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }

    fn int_of_hash(&self, key: &str, field: &str) -> i64 {
        self.hashes
            .get(key)
            .and_then(|h| h.get(field))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }
}

/// Interprets [`crate::state::version_support::SAVE_SCRIPT`] with its real semantics:
/// `ARGV[2] == -1` unconditionally bumps, otherwise the write only lands when the stored
/// version equals the expected one (`-1` is returned on conflict).
///
/// `KEYS = [payload, version, session-keys]`, `ARGV = [json, expected, stateKey]`.
fn run_save_script(data: &mut MemoryRedisData, keys: &[String], args: &[String]) -> Result<i64> {
    if keys.len() != 3 || args.len() != 3 {
        return Err(Error::Other(
            "bad SAVE_SCRIPT arity (memory double)".to_string(),
        ));
    }
    let expected: i64 = args[1]
        .trim()
        .parse()
        .map_err(|_| Error::Other(format!("bad expected version: {}", args[1])))?;
    let current = data.int_of(&keys[1]);
    if expected != -1 && current != expected {
        return Ok(-1);
    }
    let new_version = current + 1;
    data.strings
        .insert(keys[0].clone(), args[0].as_bytes().to_vec());
    data.strings
        .insert(keys[1].clone(), new_version.to_string().into_bytes());
    Ok(new_version)
}

// ---------------------------------------------------------------------------
// RedisClientAdapter double — agent state store
// ---------------------------------------------------------------------------

/// In-memory [`RedisClientAdapter`] covering every method the state store uses.
#[derive(Debug, Default)]
pub(crate) struct MemoryRedisAdapter {
    data: Mutex<MemoryRedisData>,
    /// Sets (`:_keys`), session-scoped.
    sets: Mutex<HashMap<String, HashSet<String>>>,
    /// Lists (`:list`).
    lists: Mutex<HashMap<String, Vec<String>>>,
    /// Page size used by `find_keys_by_pattern`.
    ///
    /// `None` means the real `COUNT` limit decides the page size (a faithful SCAN); tests
    /// set a small value to force a genuine **multi-page cursor walk** over the key space.
    scan_page_size: Mutex<Option<usize>>,
    /// Cursors handed to the pager, in call order — proves the cursor chain.
    scan_cursors: Arc<Mutex<Vec<u64>>>,
}

impl MemoryRedisAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an adapter whose SCAN pages are capped at `page_size` keys, so key
    /// enumeration runs through several cursor round-trips.
    pub fn with_scan_page_size(page_size: usize) -> Self {
        assert!(page_size > 0, "page size must be positive");
        let adapter = Self::default();
        *adapter
            .scan_page_size
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(page_size);
        adapter
    }

    /// Cursors requested by SCAN, in call order (`[0, 2, 4, ..]` for a paged walk).
    pub fn scan_cursor_history(&self) -> Vec<u64> {
        self.scan_cursors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn data_guard(&self) -> MutexGuard<'_, MemoryRedisData> {
        self.data.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mutates the shared state (error toggles / call counters) for failure injection.
    pub fn configure(&self, f: impl FnOnce(&mut MemoryRedisData)) {
        f(&mut self.data_guard());
    }

    fn sets_guard(&self) -> MutexGuard<'_, HashMap<String, HashSet<String>>> {
        self.sets.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lists_guard(&self) -> MutexGuard<'_, HashMap<String, Vec<String>>> {
        self.lists.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Number of `eval_script` (CAS) invocations.
    pub fn eval_calls(&self) -> usize {
        self.data_guard().adapter_eval_calls
    }

    /// Number of `delete_keys` calls that carried at least one key.
    pub fn delete_calls(&self) -> usize {
        self.data_guard().adapter_delete_calls
    }

    /// Largest number of keys passed in a single `delete_keys` call.
    ///
    /// `1` proves every `DEL` was issued per key, which Redis Cluster requires when the
    /// keys may span slots.
    pub fn max_delete_batch(&self) -> usize {
        self.data_guard().adapter_max_delete_batch
    }

    /// Number of individual `right_push_list` calls.
    pub fn push_calls(&self) -> usize {
        self.data_guard().adapter_push_calls
    }

    /// Number of `key_exists` (`EXISTS`) invocations.
    pub fn key_exists_calls(&self) -> usize {
        self.data_guard().adapter_key_exists_calls
    }

    /// Number of `get_set_size` (`SCARD`) invocations.
    pub fn set_size_calls(&self) -> usize {
        self.data_guard().adapter_set_size_calls
    }

    /// Raw string value of a key (e.g. the `:_hash` change-detection marker).
    pub fn string_of(&self, key: &str) -> Option<String> {
        self.data_guard().string_of(key)
    }

    /// Seeds a raw string value (simulates legacy / externally written data).
    pub fn seed_string(&self, key: &str, value: &str) {
        self.data_guard()
            .strings
            .insert(key.to_string(), value.as_bytes().to_vec());
    }

    /// Seeds a list value directly (simulates pre-existing `:list` data).
    pub fn seed_list(&self, key: &str, items: &[&str]) {
        self.lists_guard().insert(
            key.to_string(),
            items.iter().map(|s| s.to_string()).collect(),
        );
    }

    /// Members of a simulated set key, sorted.
    pub fn set_members(&self, key: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .sets_guard()
            .get(key)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Contents of a simulated list key.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.lists_guard().get(key).cloned().unwrap_or_default()
    }

    /// Every key currently present (strings + lists + sets), sorted and deduplicated.
    ///
    /// Redis `SCAN` is type-agnostic, so this must include **all** key spaces; the state
    /// store relies on it for `list_session_ids` / `clear_all_sessions`.
    pub fn all_keys(&self) -> Vec<String> {
        let mut out = self.data_guard().all_keys();
        out.extend(self.lists_guard().keys().cloned());
        out.extend(self.sets_guard().keys().cloned());
        out.sort();
        out.dedup();
        out
    }

    fn purge(&self, key: &str) {
        self.data_guard().strings.remove(key);
        self.lists_guard().remove(key);
        self.sets_guard().remove(key);
    }
}

/// SCAN pager over a snapshot of the adapter key space.
///
/// Models a real per-node SCAN: the cursor is a position in the (sorted) key snapshot, a
/// non-zero cursor is returned while more keys remain, and only the matching subset of each
/// page is returned (so a page can be empty while the iteration still continues).
#[derive(Debug)]
pub(crate) struct MemoryScanPager {
    keys: Vec<String>,
    /// Maximum keys per page; the caller's `COUNT` can only lower it.
    page_size: usize,
    cursors: Arc<Mutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl ScanPager for MemoryScanPager {
    async fn scan_page(
        &self,
        cursor: u64,
        pattern: &str,
        count: usize,
    ) -> Result<(u64, Vec<String>)> {
        self.cursors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(cursor);
        let start = usize::try_from(cursor).unwrap_or(usize::MAX);
        if start >= self.keys.len() {
            return Ok((0, Vec::new()));
        }
        let size = self.page_size.min(count).max(1);
        let end = start.saturating_add(size).min(self.keys.len());
        let page: Vec<String> = self.keys[start..end]
            .iter()
            .filter(|key| glob_match(pattern, key))
            .cloned()
            .collect();
        let next = if end >= self.keys.len() {
            0
        } else {
            end as u64
        };
        Ok((next, page))
    }
}

/// Single-node topology over the in-memory key space (the double models no cluster).
#[async_trait::async_trait]
impl ScanNodeSource for MemoryRedisAdapter {
    async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>> {
        let page_size = *self
            .scan_page_size
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        Ok(vec![Arc::new(MemoryScanPager {
            keys: self.all_keys(),
            page_size: page_size.unwrap_or(usize::MAX),
            cursors: self.scan_cursors.clone(),
        })])
    }
}

#[async_trait::async_trait]
impl RedisClientAdapter for MemoryRedisAdapter {
    async fn set(&self, key: &str, value: &str) -> Result<()> {
        self.data_guard()
            .strings
            .insert(key.to_string(), value.as_bytes().to_vec());
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<String>> {
        if self.data_guard().fail_get {
            return Err(Error::Other("injected Redis GET failure".to_string()));
        }
        Ok(self.data_guard().string_of(key))
    }

    async fn right_push_list(&self, key: &str, value: &str) -> Result<()> {
        self.data_guard().adapter_push_calls += 1;
        self.lists_guard()
            .entry(key.to_string())
            .or_default()
            .push(value.to_string());
        Ok(())
    }

    async fn range_list(&self, key: &str, start: i64, end: i64) -> Result<Vec<String>> {
        let lists = self.lists_guard();
        let list = match lists.get(key) {
            Some(l) => l,
            None => return Ok(Vec::new()),
        };
        if list.is_empty() {
            return Ok(Vec::new());
        }
        let len = list.len() as i64;
        let start = if start < 0 {
            (len + start).max(0)
        } else {
            start.min(len)
        };
        let end = if end < 0 { len + end } else { end.min(len - 1) };
        if end < start {
            return Ok(Vec::new());
        }
        Ok(list[start as usize..=end as usize].to_vec())
    }

    async fn get_list_length(&self, key: &str) -> Result<i64> {
        Ok(self
            .lists_guard()
            .get(key)
            .map(|l| l.len() as i64)
            .unwrap_or(0))
    }

    async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        if !keys.is_empty() {
            let mut data = self.data_guard();
            data.adapter_delete_calls += 1;
            data.adapter_deleted_key_total += keys.len();
            data.adapter_max_delete_batch = data.adapter_max_delete_batch.max(keys.len());
        }
        for key in keys {
            self.purge(key);
        }
        Ok(())
    }

    async fn add_to_set(&self, key: &str, member: &str) -> Result<()> {
        self.sets_guard()
            .entry(key.to_string())
            .or_default()
            .insert(member.to_string());
        Ok(())
    }

    async fn get_set_members(&self, key: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .sets_guard()
            .get(key)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    async fn get_set_size(&self, key: &str) -> Result<i64> {
        self.data_guard().adapter_set_size_calls += 1;
        Ok(self
            .sets_guard()
            .get(key)
            .map(|s| s.len() as i64)
            .unwrap_or(0))
    }

    async fn key_exists(&self, key: &str) -> Result<bool> {
        self.data_guard().adapter_key_exists_calls += 1;
        if self.data_guard().strings.contains_key(key) {
            return Ok(true);
        }
        Ok(self.lists_guard().contains_key(key) || self.sets_guard().contains_key(key))
    }

    /// SCAN-equivalent: matches every existing key against the glob, deduplicated.
    ///
    /// Delegates to the **production** cursor loop / fan-out
    /// (`crate::state::connection::collect_keys_across_nodes`) so the double exercises the
    /// real paging code; `with_scan_page_size` makes the walk span several pages.
    async fn find_keys_by_pattern(&self, pattern: &str) -> Result<Vec<String>> {
        crate::state::connection::collect_keys_across_nodes(self, pattern).await
    }

    async fn eval_script(
        &self,
        script: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<Option<i64>> {
        if self.data_guard().fail_eval {
            return Err(Error::Other("injected Redis EVAL failure".to_string()));
        }
        let is_discard = script == crate::state::DISCARD_TRACKED_KEY_SCRIPT;
        if is_discard && keys.len() == 1 && args.len() == 1 {
            let mut sets = self.sets_guard();
            let Some(members) = sets.get_mut(&keys[0]) else {
                return Ok(Some(0));
            };
            let removed = members.remove(&args[0]);
            let now_empty = members.is_empty();
            if now_empty {
                // A real Redis drops a set key once its last member is removed (`SREM`).
                sets.remove(&keys[0]);
            }
            return Ok(Some(if removed { 1 } else { 0 }));
        }
        if script != crate::state::version_support::SAVE_SCRIPT {
            return Err(Error::Other(
                "unsupported Lua script (memory double)".to_string(),
            ));
        }
        let mut data = self.data_guard();
        data.adapter_eval_calls += 1;
        let version = run_save_script(&mut data, keys, args)?;
        if version >= 0 {
            self.sets_guard()
                .entry(keys[2].clone())
                .or_default()
                .insert(args[2].clone());
        }
        Ok(Some(version))
    }
}

// ---------------------------------------------------------------------------
// RedisCommands double — snapshot + sandbox guard
// ---------------------------------------------------------------------------

/// In-memory [`RedisCommands`] double, including a Lua interpreter for
/// [`crate::sandbox::RELEASE_SCRIPT`].
#[derive(Debug, Default)]
pub(crate) struct MemoryRedisCommands {
    data: Mutex<MemoryRedisData>,
}

impl MemoryRedisCommands {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, MemoryRedisData> {
        self.data.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mutates internal state (error injection / seeding).
    pub fn configure(&self, f: impl FnOnce(&mut MemoryRedisData)) {
        f(&mut self.guard());
    }

    /// Reads internal state.
    pub fn inspect<T>(&self, f: impl FnOnce(&MemoryRedisData) -> T) -> T {
        f(&self.guard())
    }

    pub fn stored_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.inspect(|d| d.strings.get(key).cloned())
    }

    pub fn expiration_of(&self, key: &str) -> Option<i64> {
        self.inspect(|d| d.expirations.get(key).copied())
    }

    pub fn set_nx_calls(&self) -> usize {
        self.inspect(|d| d.set_nx_calls)
    }

    pub fn release_eval_calls(&self) -> usize {
        self.inspect(|d| d.raw_eval_calls)
    }

    pub fn exists_calls(&self) -> usize {
        self.inspect(|d| d.exists_calls)
    }
}

#[async_trait::async_trait]
impl RedisCommands for MemoryRedisCommands {
    async fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        let mut data = self.guard();
        data.set_bytes_calls += 1;
        if data.fail_set_bytes {
            return Err(Error::Other("injected SET failure".to_string()));
        }
        data.strings.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.inspect(|d| d.strings.get(key).cloned()))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let mut data = self.guard();
        data.exists_calls += 1;
        if data.fail_exists {
            return Err(Error::Other("injected EXISTS failure".to_string()));
        }
        Ok(data.strings.contains_key(key))
    }

    async fn expire(&self, key: &str, seconds: i64) -> Result<()> {
        let mut data = self.guard();
        data.expire_calls += 1;
        if data.fail_expire {
            return Err(Error::Other("injected EXPIRE failure".to_string()));
        }
        data.expirations.insert(key.to_string(), seconds);
        Ok(())
    }

    async fn set_nx_px(&self, key: &str, value: &str, ttl_ms: u64) -> Result<bool> {
        let mut data = self.guard();
        data.set_nx_calls += 1;
        if data.fail_set_nx {
            return Err(Error::Other("injected SET NX failure".to_string()));
        }
        if data.strings.contains_key(key) {
            return Ok(false);
        }
        data.strings
            .insert(key.to_string(), value.as_bytes().to_vec());
        data.expirations
            .insert(key.to_string(), (ttl_ms / 1000) as i64);
        Ok(true)
    }

    async fn eval_one_key(&self, script: &str, key: &str, arg: &str) -> Result<Value> {
        let mut data = self.guard();
        data.raw_eval_calls += 1;
        if data.fail_release_script {
            return Err(Error::Other("injected EVAL failure".to_string()));
        }
        if script != crate::sandbox::RELEASE_SCRIPT {
            return Err(Error::Other(
                "unsupported Lua script (memory double)".to_string(),
            ));
        }
        let owned = data.string_of(key).map(|v| v == arg).unwrap_or(false);
        if owned {
            data.strings.remove(key);
            data.expirations.remove(key);
            Ok(Value::Int(1))
        } else {
            Ok(Value::Int(0))
        }
    }

    async fn del(&self, key: &str) -> Result<i64> {
        let mut data = self.guard();
        Ok(if data.strings.remove(key).is_some() {
            1
        } else {
            0
        })
    }
}

// ---------------------------------------------------------------------------
// StoreCommands double — filesystem KV store
// ---------------------------------------------------------------------------

/// In-memory [`StoreCommands`] double.
#[derive(Debug, Default)]
pub(crate) struct MemoryStoreCommands {
    data: Mutex<MemoryRedisData>,
    /// When `false`, every command fails (simulates an unreachable endpoint).
    reachable: bool,
}

impl MemoryStoreCommands {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(MemoryRedisData::default()),
            reachable: true,
        }
    }

    /// A store whose Redis endpoint is unreachable: every command errors.
    pub fn unreachable() -> Self {
        Self {
            data: Mutex::new(MemoryRedisData::default()),
            reachable: false,
        }
    }

    fn guard(&self) -> MutexGuard<'_, MemoryRedisData> {
        self.data.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn check(&self) -> Result<()> {
        if self.reachable {
            Ok(())
        } else {
            Err(Error::Other(
                "REDIS_UNREACHABLE (memory double)".to_string(),
            ))
        }
    }

    /// Number of Lua `EVAL` invocations (PUT / PUT_IF_VERSION / DELETE).
    pub fn eval_calls(&self) -> usize {
        self.guard().store_eval_calls
    }

    /// Raw stored hash for an item key (`value`/`version` fields).
    pub fn hash_of(&self, key: &str) -> HashMap<String, String> {
        self.guard().hashes.get(key).cloned().unwrap_or_default()
    }

    /// Members of a namespace index, in insertion order.
    pub fn index_members(&self, key: &str) -> Vec<String> {
        self.guard().indexes.get(key).cloned().unwrap_or_default()
    }

    /// Whether an item hash key is present.
    pub fn contains(&self, key: &str) -> bool {
        self.guard().hashes.contains_key(key)
    }

    /// Seeds a raw string key (used to simulate external writes).
    pub fn seed_string(&self, key: &str, value: &str) {
        self.guard()
            .strings
            .insert(key.to_string(), value.as_bytes().to_vec());
    }

    /// Simulates an externally written index entry that has no backing item hash.
    pub fn seed_index(&self, index_key: &str, member: &str) {
        push_unique(
            self.guard()
                .indexes
                .entry(index_key.to_string())
                .or_default(),
            member,
        );
    }

    /// Simulates an externally written item (bypassing the store API).
    pub fn seed_item(&self, key: &str, json: &str, version: u64) {
        let mut data = self.guard();
        let mut hash = HashMap::new();
        hash.insert("value".to_string(), json.to_string());
        hash.insert("version".to_string(), version.to_string());
        data.hashes.insert(key.to_string(), hash);
    }
}

impl StoreCommands for MemoryStoreCommands {
    fn hgetall(&self, key: &str) -> Result<HashMap<String, String>> {
        self.check()?;
        Ok(self.guard().hashes.get(key).cloned().unwrap_or_default())
    }

    fn eval_two_keys(
        &self,
        script: &str,
        key1: &str,
        key2: &str,
        args: &[String],
    ) -> Result<Value> {
        self.check()?;
        let mut data = self.guard();
        data.store_eval_calls += 1;
        if script == crate::store::PUT_SCRIPT && args.len() == 2 {
            let version = data.int_of_hash(key1, "version") + 1;
            let hash = data.hashes.entry(key1.to_string()).or_default();
            hash.insert("value".to_string(), args[0].clone());
            hash.insert("version".to_string(), version.to_string());
            push_unique(data.indexes.entry(key2.to_string()).or_default(), &args[1]);
            return Ok(Value::BulkString(version.to_string().into_bytes()));
        }
        if script == crate::store::PUT_IF_VERSION_SCRIPT && args.len() == 3 {
            let expected: i64 = args[2].parse().unwrap_or(-1);
            let stored = data.int_of_hash(key1, "version");
            if stored != expected {
                return Ok(Value::BulkString(b"0".to_vec()));
            }
            let version = stored + 1;
            let hash = data.hashes.entry(key1.to_string()).or_default();
            hash.insert("value".to_string(), args[0].clone());
            hash.insert("version".to_string(), version.to_string());
            push_unique(data.indexes.entry(key2.to_string()).or_default(), &args[1]);
            return Ok(Value::BulkString(version.to_string().into_bytes()));
        }
        if script == crate::store::DELETE_SCRIPT && args.len() == 1 {
            data.hashes.remove(key1);
            if let Some(index) = data.indexes.get_mut(key2) {
                index.retain(|member| member != &args[0]);
            }
            return Ok(Value::Int(1));
        }
        Err(Error::Other(
            "unsupported script (memory double)".to_string(),
        ))
    }

    fn zrangebylex(&self, key: &str, offset: usize, limit: usize) -> Result<Vec<String>> {
        self.check()?;
        let members = self.guard().indexes.get(key).cloned().unwrap_or_default();
        Ok(members.into_iter().skip(offset).take(limit).collect())
    }

    fn ping(&self) -> Result<()> {
        self.guard().ping_calls += 1;
        self.check()
    }

    fn client_id(&self) -> Result<i64> {
        self.check()?;
        Ok(0)
    }
}
