//! SQLite as the durable runtime's `RecordStore`.
//!
//! One row per record, exactly like the Postgres store: a commit writes the rows it changed, never
//! the scope. What serialises writers is `BEGIN IMMEDIATE` on the database file instead of a row
//! lock, and the revision compare-and-set is the same assertion that the read set was current when
//! the writes landed.
//!
//! This is the store the file backend should have been: the cost of a commit is the number of rows
//! it touches, not the size of the conversation.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_durable::store::{RecordStore, Txn, WorkItem};
use agentscope_durable::{DurableError, Result};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agentscope_scope (
    scope_key TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS agentscope_part (
    scope_key TEXT NOT NULL,
    k TEXT NOT NULL,
    v TEXT NOT NULL,
    PRIMARY KEY (scope_key, k)
);
CREATE TABLE IF NOT EXISTS agentscope_work (
    queue TEXT NOT NULL,
    work_id TEXT NOT NULL,
    ready_at INTEGER NOT NULL,
    payload TEXT,
    PRIMARY KEY (queue, work_id)
);
CREATE INDEX IF NOT EXISTS agentscope_work_due ON agentscope_work (queue, ready_at);
";

// ----------------------------------------------------------------------------
// Write-traffic counters
// ----------------------------------------------------------------------------
//
// Two relaxed atomic adds per write and nothing else: no disk, no lock, no allocation. They answer
// "how many transactions and how many bytes does one turn cost" the same way the file store's
// counters do, so a turn can be measured rather than inferred.

static TRANSACTIONS: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// What this process wrote through [`SqliteStore`] since the last reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Committed write transactions.
    pub transactions: u64,
    /// Bytes of record text those transactions wrote.
    pub bytes_written: u64,
}

/// The counters above, as of now.
pub fn write_stats() -> WriteStats {
    WriteStats {
        transactions: TRANSACTIONS.load(Ordering::Relaxed),
        bytes_written: BYTES_WRITTEN.load(Ordering::Relaxed),
    }
}

/// Zeroes the counters; a measurement takes one before and one after the turn it costs.
pub fn reset_write_stats() {
    TRANSACTIONS.store(0, Ordering::Relaxed);
    BYTES_WRITTEN.store(0, Ordering::Relaxed);
}

/// A [`RecordStore`] over one SQLite database.
///
/// The connection sits behind a mutex because SQLite has one writer at a time and
/// [`RecordStore`] is synchronous: a pool of readers would only move the same queue.
pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (or creates) a database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path).map_err(unavailable)?;
        Self::from_connection(connection)
    }

    /// A database that lives only for the life of the process, for tests.
    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().map_err(unavailable)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        // `incremental_vacuum` only frees pages when the database was created with
        // `auto_vacuum=INCREMENTAL`, and the pragma is a no-op once the file has any page (even
        // the header `journal_mode` writes), so it goes first of all; the schema comes after.
        // An existing root keeps its `NONE` mode: changing it needs a `VACUUM`, which is not
        // worth doing on every open.
        if auto_vacuum_none(&connection) && no_schema(&connection) {
            // A pragma value cannot be a bound parameter, so `pragma_update` silently fails
            // here; the setting is issued as a literal statement instead.
            let _ = connection.execute_batch("PRAGMA auto_vacuum = INCREMENTAL");
        }
        // WAL lets a reader see the last committed state without waiting for a writer; an
        // in-memory database ignores the request and stays in its own journal mode.
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        let _ = connection.pragma_update(None, "synchronous", "NORMAL");
        // The commit path issues the same dozen statements over and over; compiling them once is
        // what keeps a row store's per-statement overhead from eating the read win.
        connection.set_prepared_statement_cache_capacity(64);
        connection.execute_batch(SCHEMA).map_err(unavailable)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn run(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
        writable: bool,
    ) -> Result<()> {
        let scope = scope_key(scope)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| DurableError::unavailable("the sqlite connection lock was poisoned"))?;
        let behavior = if writable {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let transaction = connection
            .transaction_with_behavior(behavior)
            .map_err(unavailable)?;
        let now = now_millis();
        let revision = scope_revision(&transaction, scope, now, writable)?;
        let written = {
            let mut records = SqliteTxn {
                connection: &*transaction,
                scope,
                now,
                writable,
                written: Cell::new(false),
            };
            work(&mut records)?;
            records.written.get()
        };
        if written {
            let updated = transaction
                .prepare_cached(
                    "UPDATE agentscope_scope SET revision = revision + 1, updated_at = ?3 \
                     WHERE scope_key = ?1 AND revision = ?2",
                )
                .map_err(unavailable)?
                .execute(params![scope, revision, now])
                .map_err(unavailable)?;
            if updated == 0 {
                return Err(DurableError::conflict(
                    "the scope changed while its transaction was open",
                ));
            }
        }
        transaction.commit().map_err(unavailable)?;
        if written {
            TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl SqliteStore {
    fn reclaim_freelist(&self) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| DurableError::unavailable("the sqlite connection lock was poisoned"))?;
        // Outside a transaction (the commit has returned). A trim drops rows; whether the pages
        // they occupied are worth a rebuild is a separate question, because `VACUUM` rewrites
        // every page of the file — including sessions that have nothing to do with this trim.
        // The trim that produced this call may have freed next to nothing (a scope whose live
        // facts exceed the compaction ceiling cannot shrink), and rebuilding the file for it
        // turns every commit into a whole-file write. The freelist is what a rebuild returns to
        // the filesystem; below a real share of the file, page reuse already handles it.
        let (page_size, pages, free) = freelist(&connection)?;
        let freed = free * page_size;
        if freed < RECLAIM_MIN_FREED_BYTES || freed * RECLAIM_MIN_FREED_SHARE < pages * page_size {
            return Ok(());
        }
        connection.execute_batch("VACUUM").map_err(unavailable)?;
        Ok(())
    }
}

/// The share of the file a trim has to have released before `VACUUM` pays for its rewrite.
const RECLAIM_MIN_FREED_SHARE: u64 = 4;

/// The absolute floor a rebuild must return, so a small database is not rewritten for a few pages.
const RECLAIM_MIN_FREED_BYTES: u64 = 2 << 20;

/// Page size, page count and free pages of the database.
fn freelist(connection: &Connection) -> Result<(u64, u64, u64)> {
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(unavailable)?;
    let pages: i64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(unavailable)?;
    let free: i64 = connection
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .map_err(unavailable)?;
    Ok((
        page_size.max(0) as u64,
        pages.max(0) as u64,
        free.max(0) as u64,
    ))
}

/// Whether the database was created without automatic vacuuming.
fn auto_vacuum_none(connection: &Connection) -> bool {
    connection
        .query_row("PRAGMA auto_vacuum", [], |row| row.get::<_, i64>(0))
        .map(|mode| mode == 0)
        .unwrap_or(false)
}

/// Whether the store schema has not been created yet.
fn no_schema(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'agentscope_part'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count == 0)
        .unwrap_or(false)
}

impl RecordStore for SqliteStore {
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        self.run(scope, work, true)
    }

    fn read_transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        self.run(scope, work, false)
    }

    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| DurableError::unavailable("the sqlite connection lock was poisoned"))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection
            .prepare_cached(
                "SELECT work_id, ready_at, payload FROM agentscope_work \
                 WHERE queue = ?1 AND ready_at >= 0 AND ready_at <= ?2 \
                 ORDER BY ready_at, work_id LIMIT ?3",
            )
            .map_err(unavailable)?;
        let rows = statement
            .query_map(params![queue, now, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(unavailable)?;
        let mut due = Vec::new();
        for row in rows {
            let (work_id, ready_at, payload) = row.map_err(unavailable)?;
            due.push(WorkItem {
                queue: queue.to_string(),
                work_id,
                ready_at,
                payload: payload.map(|text| decode(&text)).transpose()?,
            });
        }
        Ok(due)
    }

    fn read_parts(&self, scope: &str, parts: &[&str]) -> Result<Vec<Option<Value>>> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = parts.iter().map(|part| (*part).to_string()).collect();
        let mut values = Vec::new();
        self.read_transaction(scope, &mut |txn: &mut dyn Txn| {
            values = txn.get_all(&keys)?;
            Ok(())
        })?;
        Ok(values)
    }

    fn scope_bytes(&self, scope: &str) -> Result<Option<u64>> {
        let scope = scope_key(scope)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| DurableError::unavailable("the sqlite connection lock was poisoned"))?;
        let bytes: i64 = connection
            .prepare_cached(
                "SELECT COALESCE(SUM(length(CAST(v AS BLOB))), 0) \
                 FROM agentscope_part WHERE scope_key = ?1",
            )
            .map_err(unavailable)?
            .query_row(params![scope], |row| row.get(0))
            .map_err(unavailable)?;
        Ok(Some(bytes.max(0) as u64))
    }

    fn shared_across_hosts(&self) -> bool {
        // WAL is not safe on a network filesystem, so the database file must stay on the host
        // that opened it. Cross-host leases and shared roots are therefore not advertised.
        false
    }

    fn reclaim_space(&self) -> Result<()> {
        self.reclaim_freelist()
    }
}

/// One transaction's [`Txn`].
///
/// Writes go to the database immediately rather than being staged here: the transaction is what
/// rolls them back, and a staged copy would be a second representation of every value in memory.
struct SqliteTxn<'a> {
    connection: &'a Connection,
    scope: &'a str,
    now: i64,
    writable: bool,
    written: Cell<bool>,
}

impl Txn for SqliteTxn<'_> {
    fn now(&self) -> i64 {
        self.now
    }

    fn get(&self, part: &str) -> Result<Option<Value>> {
        let part = part_key(part)?;
        let text: Option<String> = self
            .connection
            .prepare_cached("SELECT v FROM agentscope_part WHERE scope_key = ?1 AND k = ?2")
            .map_err(unavailable)?
            .query_row(params![self.scope, part], |row| row.get(0))
            .optional()
            .map_err(unavailable)?;
        text.map(|text| decode(&text)).transpose()
    }

    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()> {
        if !self.writable {
            return Err(DurableError::invalid(
                "a read-only transaction cannot write",
            ));
        }
        let part = part_key(part)?;
        self.written.set(true);
        match value {
            Some(value) => {
                let text = encode(&value)?;
                BYTES_WRITTEN.fetch_add(text.len() as u64, Ordering::Relaxed);
                self.connection
                    .prepare_cached(
                        "INSERT INTO agentscope_part (scope_key, k, v) VALUES (?1, ?2, ?3) \
                         ON CONFLICT (scope_key, k) DO UPDATE SET v = excluded.v",
                    )
                    .map_err(unavailable)?
                    .execute(params![self.scope, part, text])
                    .map_err(unavailable)?;
            }
            None => {
                self.connection
                    .prepare_cached("DELETE FROM agentscope_part WHERE scope_key = ?1 AND k = ?2")
                    .map_err(unavailable)?
                    .execute(params![self.scope, part])
                    .map_err(unavailable)?;
            }
        }
        Ok(())
    }

    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        // A page is the keys under `prefix`, optionally starting after a cursor. The prefix is a
        // range, not just a lower bound: `turn:` must never page into `thread:`, which is a
        // different record type with a different shape.
        let cursor = after.filter(|after| *after >= prefix);
        let bound = prefix_bound(prefix);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT k FROM agentscope_part WHERE scope_key = ?1 AND k >= ?2 \
                 AND (?3 IS NULL OR k > ?3) AND (?4 IS NULL OR k < ?4) \
                 ORDER BY k LIMIT ?5",
            )
            .map_err(unavailable)?;
        let rows = statement
            .query_map(
                params![self.scope, prefix, cursor, bound.as_deref(), limit],
                |row| row.get::<_, String>(0),
            )
            .map_err(unavailable)?;
        rows.map(|row| row.map_err(unavailable)).collect()
    }

    fn get_all(&self, parts: &[String]) -> Result<Vec<Option<Value>>> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let keys = serde_json::to_string(parts).map_err(|error| {
            DurableError::unavailable(format!("sqlite: a key list was not JSON: {error}"))
        })?;
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT k, v FROM agentscope_part WHERE scope_key = ?1 \
                 AND k IN (SELECT value FROM json_each(?2))",
            )
            .map_err(unavailable)?;
        let rows = statement
            .query_map(params![self.scope, keys], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(unavailable)?;
        let mut found = HashMap::new();
        for row in rows {
            let (key, text) = row.map_err(unavailable)?;
            found.insert(key, decode(&text)?);
        }
        Ok(parts
            .iter()
            .map(|part| found.remove(part.as_str()))
            .collect())
    }

    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()> {
        if !self.writable {
            return Err(DurableError::invalid(
                "a read-only transaction cannot write",
            ));
        }
        self.written.set(true);
        if ready_at < 0 {
            self.connection
                .prepare_cached("DELETE FROM agentscope_work WHERE queue = ?1 AND work_id = ?2")
                .map_err(unavailable)?
                .execute(params![queue, work_id])
                .map_err(unavailable)?;
            return Ok(());
        }
        let payload = payload.map(|value| encode(&value)).transpose()?;
        if let Some(text) = &payload {
            BYTES_WRITTEN.fetch_add(text.len() as u64, Ordering::Relaxed);
        }
        self.connection
            .prepare_cached(
                "INSERT INTO agentscope_work (queue, work_id, ready_at, payload) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (queue, work_id) \
                 DO UPDATE SET ready_at = excluded.ready_at, payload = excluded.payload",
            )
            .map_err(unavailable)?
            .execute(params![queue, work_id, ready_at, payload])
            .map_err(unavailable)?;
        Ok(())
    }
}

/// The revision the writes of this transaction are checked against.
///
/// The first write to a scope creates its row; `BEGIN IMMEDIATE` is what makes the creator and a
/// concurrent creator agree on one row. A read-only transaction on a scope that does not exist yet
/// answers `0` without creating it.
fn scope_revision(
    transaction: &Transaction<'_>,
    scope: &str,
    now: i64,
    writable: bool,
) -> Result<i64> {
    let existing: Option<i64> = transaction
        .prepare_cached("SELECT revision FROM agentscope_scope WHERE scope_key = ?1")
        .map_err(unavailable)?
        .query_row(params![scope], |row| row.get(0))
        .optional()
        .map_err(unavailable)?;
    if let Some(revision) = existing {
        return Ok(revision);
    }
    if !writable {
        return Ok(0);
    }
    transaction
        .prepare_cached(
            "INSERT INTO agentscope_scope (scope_key, revision, updated_at) VALUES (?1, 0, ?2) \
             ON CONFLICT (scope_key) DO NOTHING",
        )
        .map_err(unavailable)?
        .execute(params![scope, now])
        .map_err(unavailable)?;
    transaction
        .prepare_cached("SELECT revision FROM agentscope_scope WHERE scope_key = ?1")
        .map_err(unavailable)?
        .query_row(params![scope], |row| row.get(0))
        .map_err(unavailable)
}

/// SQLite has no server clock; a durable decision is dated by the host that owns the file.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn scope_key(scope: &str) -> Result<&str> {
    if scope.is_empty() || scope.contains('\0') {
        return Err(DurableError::invalid(
            "a scope key must be non-empty and contain no NUL",
        ));
    }
    Ok(scope)
}

/// The exclusive upper bound of the keys under `prefix`, or `None` when the range is unbounded.
///
/// Same rule the Postgres store uses: bump the last code point and drop the tail, so `turn:`
/// becomes `turn;` and a page cannot walk into the next record type.
fn prefix_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        if let Some(next) = successor(last) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// The next code point, skipping the surrogate range a `char` cannot enter.
fn successor(last: char) -> Option<char> {
    let code = last as u32;
    let next = if code == 0xD7FF { 0xE000 } else { code + 1 };
    char::from_u32(next)
}

fn part_key(part: &str) -> Result<&str> {
    // An empty key is a stored record like any other; only NUL is refused, for the same reason
    // the Postgres store refuses it: the key is text and cannot carry it.
    if part.contains('\0') {
        return Err(DurableError::invalid("a part key must not contain NUL"));
    }
    Ok(part)
}

fn encode(value: &Value) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| DurableError::invalid(format!("sqlite: a record is not JSON: {error}")))
}

fn decode(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| {
        DurableError::unavailable(format!("sqlite: a stored record is not JSON: {error}"))
    })
}

fn unavailable(error: rusqlite::Error) -> DurableError {
    DurableError::unavailable(format!("sqlite: {error}"))
}

#[cfg(test)]
mod tests {
    use agentscope_durable::store::{txn, txn_read};
    use agentscope_durable::ErrorCode;
    use serde_json::json;

    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agentscope-sqlite-{name}-{}-{id}.db",
            std::process::id()
        ))
    }

    /// Enough rows (≈ 5 MB) that dropping them is a slice of the file a rebuild can pay for.
    const ROWS: usize = 1200;

    #[test]
    fn reclaim_space_frees_the_pages_a_trim_released() {
        let path = temp_path("reclaim");
        let store = SqliteStore::open(&path).expect("store");
        let auto_vacuum: i64 = {
            let connection = store.connection.lock().expect("lock");
            connection
                .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
                .expect("auto_vacuum")
        };
        assert_eq!(auto_vacuum, 2, "a new root uses incremental auto-vacuum");
        let payload = "x".repeat(4096);
        let write = |value: Option<Value>| {
            store
                .transaction("ns/reclaim", &mut |txn: &mut dyn Txn| {
                    for index in 0..ROWS {
                        txn.put(&format!("row:{index:04}"), value.clone())?;
                    }
                    Ok(())
                })
                .expect("write");
        };
        let freelist = || -> i64 {
            let connection = store.connection.lock().expect("lock");
            connection
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))
                .expect("freelist_count")
        };
        write(Some(Value::String(payload)));
        assert_eq!(freelist(), 0, "an insert leaves no free pages");
        write(None);
        assert!(
            freelist() > 0,
            "deleting rows puts their pages on the freelist"
        );
        store.reclaim_space().expect("reclaim");
        assert_eq!(freelist(), 0, "the reclaim empties the freelist");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_trim_that_freed_little_does_not_rebuild_the_file() {
        let path = temp_path("reclaim-small");
        let store = SqliteStore::open(&path).expect("store");
        let payload = "x".repeat(4096);
        store
            .transaction("ns/small", &mut |txn: &mut dyn Txn| {
                for index in 0..ROWS {
                    txn.put(
                        &format!("row:{index:04}"),
                        Some(Value::String(payload.clone())),
                    )?;
                }
                Ok(())
            })
            .expect("write");
        // A trim of a scope that cannot shrink drops a handful of rows, not a share of the file.
        store
            .transaction("ns/small", &mut |txn: &mut dyn Txn| {
                for index in 0..32 {
                    txn.put(&format!("row:{index:04}"), None)?;
                }
                Ok(())
            })
            .expect("trim");
        let before = {
            let connection = store.connection.lock().expect("lock");
            connection
                .query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))
                .expect("freelist_count")
        };
        assert!(before > 0, "the trim did free pages");
        store.reclaim_space().expect("reclaim");
        let after = {
            let connection = store.connection.lock().expect("lock");
            connection
                .query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))
                .expect("freelist_count")
        };
        assert_eq!(
            after, before,
            "a rebuild that would return a few pages is not worth rewriting the file"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn two_connections_on_one_file_see_each_others_commits() {
        let path = temp_path("two-connections");
        let first = SqliteStore::open(&path).expect("first store");
        let second = SqliteStore::open(&path).expect("second store");
        txn(&first, "s:1", |txn| txn.put("k", Some(json!({ "n": 1 })))).expect("write");
        assert_eq!(
            txn(&second, "s:1", |txn| txn.get("k")).expect("read"),
            Some(json!({ "n": 1 }))
        );
    }

    #[test]
    fn a_read_only_transaction_cannot_write() {
        let store = SqliteStore::in_memory().expect("store");
        txn(&store, "s:1", |txn| txn.put("k", Some(json!(1)))).expect("write");
        let error = txn_read(&store, "s:1", |txn| txn.put("k", Some(json!(2))))
            .expect_err("a read-only transaction must refuse a write");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("k")).expect("read"),
            Some(json!(1))
        );
    }

    #[test]
    fn a_keys_page_never_walks_out_of_its_prefix() {
        let store = SqliteStore::in_memory().expect("store");
        txn(&store, "s:1", |txn| {
            txn.put("turn:1", Some(json!(1)))?;
            txn.put("turn:2", Some(json!(2)))?;
            txn.put("turn:3", Some(json!(3)))?;
            txn.put("thread:1", Some(json!({ "definition": true })))
        })
        .expect("write");
        let keys = txn(&store, "s:1", |txn| txn.keys("turn:", None, 10)).expect("page");
        assert_eq!(keys, vec!["turn:1", "turn:2", "turn:3"]);
        let page = txn(&store, "s:1", |txn| txn.keys("turn:", Some("turn:1"), 10)).expect("page");
        assert_eq!(page, vec!["turn:2", "turn:3"]);
    }

    #[test]
    fn scope_bytes_follows_inserts_updates_and_deletes() {
        let store = SqliteStore::in_memory().expect("store");
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(0));
        txn(&store, "s:1", |txn| txn.put("a", Some(json!("hello")))).expect("insert");
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(7));
        txn(&store, "s:1", |txn| txn.put("a", Some(json!("hi")))).expect("update");
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(4));
        txn(&store, "s:1", |txn| txn.put("b", Some(json!({ "n": 1 })))).expect("second insert");
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(4 + 7));
        txn(&store, "s:1", |txn| txn.put("a", None)).expect("delete");
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(7));
    }

    #[test]
    fn a_failed_transaction_does_not_change_the_scope_size() {
        let store = SqliteStore::in_memory().expect("store");
        txn(&store, "s:1", |txn| txn.put("a", Some(json!("hello")))).expect("insert");
        let error = txn(&store, "s:1", |txn| {
            txn.put("a", Some(json!("a much longer value")))?;
            Err::<(), _>(DurableError::invalid("boom"))
        })
        .expect_err("the closure failed");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(store.scope_bytes("s:1").expect("measure"), Some(7));
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("a")).expect("read"),
            Some(json!("hello"))
        );
    }
}
