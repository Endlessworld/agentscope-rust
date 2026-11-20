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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::session_backend::SessionBackend;
use agentscope_durable::store::{RecordStore, Txn, WorkItem};
use agentscope_durable::{DurableError, Result};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension, ToSql, Transaction, TransactionBehavior};
use serde_json::Value;

mod schema;

/// The engine settings a write transaction needs, matching the other SQLite users in this
/// workspace (`harness` and the JDBC backend both wait ten seconds). SQLite has one writer, so a
/// second connection that finds it busy must wait instead of failing with `SQLITE_BUSY` the way
/// the zero default does — every ACP session opens its own connection on the same file.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The `PRAGMA application_id` of a database this store owns (`"ASCP"`). A non-zero id that is
/// not this one belongs to another application and is refused rather than written into.
const APPLICATION_ID: i64 = 0x4153_4350;

/// What `PRAGMA user_version` is set to. Version 2 adds the foreign key, the key-length check
/// and the three-column readiness index; version 1 is migrated in place on open.
const SCHEMA_VERSION: i64 = 2;

/// The bound the PostgreSQL schema and mapping enforce, so a scope key one store accepts is not
/// refused by the other.
const MAX_SCOPE_CHARS: usize = 256;

/// The page bound the PostgreSQL mapping applies; without it a caller asking for a huge page
/// makes the store allocate it.
const MAX_PAGE: usize = 1000;

/// Rebuilds the three tables of a version 1 database into the version 2 shapes.
///
/// SQLite cannot add a foreign key or a check constraint with `ALTER TABLE`, so the only way to
/// keep existing rows is the documented table-rebuild procedure: create the new tables, copy,
/// drop the old ones, rename, recreate the index. The whole sequence runs inside one `IMMEDIATE`
/// transaction, so a crash leaves the version 1 database untouched, and
/// `PRAGMA foreign_key_check` runs before the commit rather than trusting the copy.
const MIGRATE_TO_V2: &str = "
DROP TABLE IF EXISTS agentscope_scope_v2;
DROP TABLE IF EXISTS agentscope_part_v2;
DROP TABLE IF EXISTS agentscope_work_v2;
CREATE TABLE agentscope_scope_v2 (
    scope_key  TEXT COLLATE BINARY NOT NULL PRIMARY KEY,
    revision   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    CONSTRAINT agentscope_scope_key_len CHECK (length(scope_key) BETWEEN 1 AND 256)
);
CREATE TABLE agentscope_part_v2 (
    scope_key TEXT COLLATE BINARY NOT NULL
        REFERENCES agentscope_scope_v2 (scope_key) ON DELETE CASCADE,
    k         TEXT COLLATE BINARY NOT NULL,
    v         TEXT NOT NULL,
    PRIMARY KEY (scope_key, k)
);
CREATE TABLE agentscope_work_v2 (
    queue    TEXT COLLATE BINARY NOT NULL,
    work_id  TEXT COLLATE BINARY NOT NULL,
    ready_at INTEGER NOT NULL,
    payload  TEXT,
    PRIMARY KEY (queue, work_id)
);
INSERT INTO agentscope_scope_v2 (scope_key, revision, updated_at)
    SELECT scope_key, revision, updated_at FROM agentscope_scope;
INSERT INTO agentscope_part_v2 (scope_key, k, v)
    SELECT scope_key, k, v FROM agentscope_part;
INSERT INTO agentscope_work_v2 (queue, work_id, ready_at, payload)
    SELECT queue, work_id, ready_at, payload FROM agentscope_work;
DROP TABLE agentscope_part;
DROP TABLE agentscope_work;
DROP TABLE agentscope_scope;
ALTER TABLE agentscope_scope_v2 RENAME TO agentscope_scope;
ALTER TABLE agentscope_part_v2 RENAME TO agentscope_part;
ALTER TABLE agentscope_work_v2 RENAME TO agentscope_work;
CREATE INDEX IF NOT EXISTS agentscope_work_due
    ON agentscope_work (queue, ready_at, work_id);
";

/// The four key-page statements.
///
/// A cursor and an upper bound are range constraints here, not filters applied after the index
/// scan: `(?3 IS NULL OR k > ?3)` cannot be used as a range by the query planner, so paging a
/// long journal would rescan its prefix for every page. The PostgreSQL mapping splits the same
/// way for the same reason.
const KEYS_FROM_PREFIX: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = ?1 AND k >= ?2 ORDER BY k LIMIT ?3";
const KEYS_AFTER: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = ?1 AND k > ?2 ORDER BY k LIMIT ?3";
const KEYS_FROM_PREFIX_BOUNDED: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = ?1 AND k >= ?2 AND k < ?3 ORDER BY k LIMIT ?4";
const KEYS_AFTER_BOUNDED: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = ?1 AND k > ?2 AND k < ?3 ORDER BY k LIMIT ?4";

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
        connection.busy_timeout(BUSY_TIMEOUT).map_err(unavailable)?;
        initialize(&connection)?;
        // `foreign_keys` is a per-connection switch and SQLite defaults it off; a pragma value
        // cannot be a bound parameter, so it is a literal statement like `auto_vacuum`.
        let _ = connection.execute_batch("PRAGMA foreign_keys = ON");
        // The commit path issues the same dozen statements over and over; compiling them once is
        // what keeps a row store's per-statement overhead from eating the read win.
        connection.set_prepared_statement_cache_capacity(64);
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
            verify_cas(updated)?;
        }
        transaction.commit().map_err(unavailable)?;
        if written {
            TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Applies the schema, refusing a database this build must not write into.
///
/// The application id says whose file this is, and `user_version` says how old it is: a newer
/// version was written by a build that knows a shape this one does not, and rewriting its rows
/// would corrupt them. Version 1 is migrated in place; an empty database is created at the
/// current version.
fn initialize(connection: &Connection) -> Result<()> {
    let application_id = pragma_i64(connection, "application_id")?;
    if application_id != 0 && application_id != APPLICATION_ID {
        return Err(DurableError::invalid(format!(
            "the database belongs to another application (application_id {application_id:#x})"
        )));
    }
    let version = pragma_i64(connection, "user_version")?;
    if version > SCHEMA_VERSION {
        return Err(DurableError::invalid(format!(
            "the database schema version {version} is newer than this build supports \
             ({SCHEMA_VERSION})"
        )));
    }
    if application_id == 0 {
        connection
            .execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID}"))
            .map_err(unavailable)?;
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if no_schema(connection) {
        for statement in schema::statements() {
            connection.execute_batch(&statement).map_err(unavailable)?;
        }
    } else if !part_is_v2(connection) {
        migrate_to_v2(connection)?;
    }
    connection
        .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
        .map_err(unavailable)?;
    Ok(())
}

/// Rebuilds a version 1 database; the whole copy and rename is one transaction, so a failure
/// leaves the old tables in place.
fn migrate_to_v2(connection: &Connection) -> Result<()> {
    // The switch cannot be flipped inside a transaction, and the rebuild drops the table the
    // foreign key points from.
    let _ = connection.execute_batch("PRAGMA foreign_keys = OFF");
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(unavailable)?;
    transaction
        .execute_batch(MIGRATE_TO_V2)
        .map_err(unavailable)?;
    let violations: i64 = transaction
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(unavailable)?;
    if violations > 0 {
        return Err(DurableError::invalid(format!(
            "the migrated database has {violations} foreign key violations"
        )));
    }
    transaction.commit().map_err(unavailable)?;
    Ok(())
}

/// Whether `agentscope_part` already has the version 2 shape.
///
/// A database whose `user_version` was never written but whose tables were rebuilt (a build that
/// committed the migration and then failed to record the version) must not be rebuilt again:
/// dropping the tables would drop its rows.
fn part_is_v2(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'agentscope_part'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .is_some_and(|sql| sql.contains("REFERENCES agentscope_scope"))
}

fn pragma_i64(connection: &Connection, name: &str) -> Result<i64> {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .map_err(unavailable)
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

/// Opens the SQLite-backed [`SessionBackend`] a host can drive without assembling the store.
pub fn open_backend(path: impl AsRef<Path>) -> Result<Arc<dyn SessionBackend>> {
    let store: Arc<dyn RecordStore> = Arc::new(SqliteStore::open(path)?);
    Ok(Arc::new(TxnBackend::with_store(store)))
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
        // One indexed statement and no transaction: a control poll runs several times a second
        // while a model call streams, and WAL already answers it from a consistent snapshot.
        // The PostgreSQL store reads these the same way.
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let scope = scope_key(scope)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| DurableError::unavailable("the sqlite connection lock was poisoned"))?;
        select_parts(&connection, scope, parts)
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
        // different record type with a different shape. The cursor and the upper bound are
        // separate statements so they are range constraints, not filters after the scan.
        let cursor = after.filter(|after| *after >= prefix);
        let bound = prefix_bound(prefix);
        let limit = page_limit(limit);
        match (cursor, bound.as_deref()) {
            (None, None) => query_keys(
                self.connection,
                KEYS_FROM_PREFIX,
                &[&self.scope, &prefix, &limit],
            ),
            (Some(cursor), None) => {
                query_keys(self.connection, KEYS_AFTER, &[&self.scope, &cursor, &limit])
            }
            (None, Some(bound)) => query_keys(
                self.connection,
                KEYS_FROM_PREFIX_BOUNDED,
                &[&self.scope, &prefix, &bound, &limit],
            ),
            (Some(cursor), Some(bound)) => query_keys(
                self.connection,
                KEYS_AFTER_BOUNDED,
                &[&self.scope, &cursor, &bound, &limit],
            ),
        }
    }

    fn get_all(&self, parts: &[String]) -> Result<Vec<Option<Value>>> {
        let keys: Vec<&str> = parts.iter().map(String::as_str).collect();
        select_parts(self.connection, self.scope, &keys)
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
    if scope.chars().count() > MAX_SCOPE_CHARS {
        return Err(DurableError::invalid(format!(
            "a scope key must be at most {MAX_SCOPE_CHARS} characters"
        )));
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

/// The page bound the PostgreSQL mapping applies.
fn page_limit(limit: usize) -> i64 {
    limit.min(MAX_PAGE) as i64
}

/// Interprets the row count of the revision bump.
///
/// `BEGIN IMMEDIATE` makes a miss impossible while the write lock is held, so zero rows means the
/// assumption the lock rests on did not hold (a statement issued outside its transaction, a
/// connection that committed early); the answer is a conflict rather than a lost update. More
/// than one row is not reachable through the primary key and would mean the schema is not what
/// this store thinks it is.
fn verify_cas(updated: usize) -> Result<()> {
    match updated {
        1 => Ok(()),
        0 => Err(DurableError::conflict(
            "the scope changed while its transaction was open",
        )),
        other => Err(DurableError::invalid(format!(
            "the scope revision compare-and-set touched {other} rows"
        ))),
    }
}

/// One page of keys, from one of the four statements.
fn query_keys(connection: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<String>> {
    let mut statement = connection.prepare_cached(sql).map_err(unavailable)?;
    let rows = statement
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(unavailable)?;
    rows.map(|row| row.map_err(unavailable)).collect()
}

/// Several records in one statement, in the order the caller asked for.
///
/// The key list crosses as a JSON array because SQLite has no array parameter; `json_each` turns
/// it back into the rows the `IN` needs, so the whole page is one indexed statement rather than
/// one per record.
fn select_parts(
    connection: &Connection,
    scope: &str,
    parts: &[&str],
) -> Result<Vec<Option<Value>>> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let keys = serde_json::to_string(parts).map_err(|error| {
        DurableError::invalid(format!("sqlite: a key list was not JSON: {error}"))
    })?;
    let mut statement = connection
        .prepare_cached(
            "SELECT k, v FROM agentscope_part WHERE scope_key = ?1 \
             AND k IN (SELECT value FROM json_each(?2))",
        )
        .map_err(unavailable)?;
    let rows = statement
        .query_map(params![scope, keys], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(unavailable)?;
    let mut found = HashMap::new();
    for row in rows {
        let (key, text) = row.map_err(unavailable)?;
        found.insert(key, decode(&text)?);
    }
    Ok(parts.iter().map(|part| found.remove(*part)).collect())
}

fn encode(value: &Value) -> Result<String> {
    check_storable(value)?;
    serde_json::to_string(value)
        .map_err(|error| DurableError::invalid(format!("sqlite: a record is not JSON: {error}")))
}

/// Refuses a value the PostgreSQL store cannot hold either.
///
/// `jsonb` cannot store `U+0000` inside a string, and a record that one store accepts and another
/// refuses turns a deployment's choice of store into a data loss. The same value is refused here
/// so a commit succeeds or fails the same way on every store.
fn check_storable(value: &Value) -> Result<()> {
    match value {
        Value::String(text) if text.contains('\0') => Err(unstorable()),
        Value::Array(items) => items.iter().try_for_each(check_storable),
        Value::Object(fields) => {
            for (name, value) in fields {
                if name.contains('\0') {
                    return Err(unstorable());
                }
                check_storable(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn unstorable() -> DurableError {
    DurableError::invalid(
        "a value contains U+0000, which the durable record format cannot store; the record was \
         refused rather than rewritten",
    )
}

fn decode(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| {
        DurableError::invalid(format!("sqlite: a stored record is not JSON: {error}"))
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

    #[tokio::test]
    async fn open_backend_answers_the_baseline_views() {
        use agentscope_durable::types::{
            SessionKey, SessionSpec, ThreadBinding, ThreadKey, ThreadSpec,
        };

        let path = temp_path("backend");
        let backend = open_backend(&path).expect("backend");
        let session = SessionKey::new("ns", "s1").expect("key");
        let binding = ThreadBinding::new("stub", "definition-1", "state-1").expect("binding");
        backend
            .create(SessionSpec {
                key: session.clone(),
                primary_binding: binding.clone(),
                metadata: json!({}),
            })
            .await
            .expect("create session");
        let thread = ThreadKey::new(session, "main").expect("thread");
        backend
            .create_thread(ThreadSpec {
                key: thread.clone(),
                binding,
            })
            .await
            .expect("create thread");
        let receipt = backend
            .submit(&thread, "request-1", json!({"text": "hi"}))
            .await
            .expect("submit");
        assert!(!receipt.turn_id.is_empty());
        let state = backend.thread_state(&thread).await.expect("state");
        assert_eq!(state.definition.key, thread);

        drop(backend);
        std::fs::remove_file(&path).ok();
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

    /// A version 1 database keeps its rows: the rebuild copies all three tables into the version 2
    /// shapes, records the version and leaves the foreign key switch on.
    #[test]
    fn a_v1_database_is_migrated_in_place() {
        let path = temp_path("migrate");
        {
            let connection = Connection::open(&path).expect("v1 database");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE agentscope_scope (
                        scope_key TEXT PRIMARY KEY,
                        revision INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL
                    );
                    CREATE TABLE agentscope_part (
                        scope_key TEXT NOT NULL,
                        k TEXT NOT NULL,
                        v TEXT NOT NULL,
                        PRIMARY KEY (scope_key, k)
                    );
                    CREATE TABLE agentscope_work (
                        queue TEXT NOT NULL,
                        work_id TEXT NOT NULL,
                        ready_at INTEGER NOT NULL,
                        payload TEXT,
                        PRIMARY KEY (queue, work_id)
                    );
                    CREATE INDEX agentscope_work_due ON agentscope_work (queue, ready_at);
                    INSERT INTO agentscope_scope VALUES ('s:1', 7, 1000);
                    INSERT INTO agentscope_part VALUES ('s:1', 'a', '{"n":1}');
                    INSERT INTO agentscope_work VALUES ('q', 'w', 5, NULL);
                    "#,
                )
                .expect("version 1 schema");
        }
        let store = SqliteStore::open(&path).expect("migrated store");
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("a")).expect("read"),
            Some(json!({ "n": 1 }))
        );
        assert_eq!(store.ready("q", 10, 10).expect("ready").len(), 1);
        let connection = store.connection.lock().expect("lock");
        assert_eq!(
            pragma_i64(&connection, "user_version").expect("version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            pragma_i64(&connection, "application_id").expect("application id"),
            APPLICATION_ID
        );
        assert_eq!(pragma_i64(&connection, "foreign_keys").expect("fk"), 1);
        let revision: i64 = connection
            .query_row(
                "SELECT revision FROM agentscope_scope WHERE scope_key = 's:1'",
                [],
                |row| row.get(0),
            )
            .expect("revision");
        assert_eq!(revision, 7, "the migration keeps the revision");
        let indexed: i64 = connection
            .query_row(
                "SELECT count(*) FROM pragma_index_info('agentscope_work_due')",
                [],
                |row| row.get(0),
            )
            .expect("index columns");
        assert_eq!(indexed, 3, "the readiness index has its third column");
        drop(connection);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_newer_schema_version_is_refused() {
        let path = temp_path("future");
        {
            let connection = Connection::open(&path).expect("database");
            connection
                .execute_batch("PRAGMA user_version = 99")
                .expect("version");
        }
        let error = match SqliteStore::open(&path) {
            Ok(_) => panic!("a newer schema is refused"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Invalid);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_database_of_another_application_is_refused() {
        let path = temp_path("foreign");
        {
            let connection = Connection::open(&path).expect("database");
            connection
                .execute_batch("PRAGMA application_id = 12345")
                .expect("application id");
        }
        let error = match SqliteStore::open(&path) {
            Ok(_) => panic!("a foreign database is refused"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Invalid);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_scope_key_over_the_bound_is_refused() {
        let store = SqliteStore::in_memory().expect("store");
        let key = "x".repeat(MAX_SCOPE_CHARS + 1);
        let error = txn(&store, &key, |txn| txn.get("k")).expect_err("the key is too long");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(
            txn(&store, &"x".repeat(MAX_SCOPE_CHARS), |txn| txn.get("k"))
                .expect("the bound itself"),
            None
        );
    }

    #[test]
    fn a_value_with_a_nul_is_refused() {
        let store = SqliteStore::in_memory().expect("store");
        let error = txn(&store, "s:1", |txn| txn.put("k", Some(json!("a\u{0}b"))))
            .expect_err("U+0000 is not storable");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("k")).expect("read"),
            None,
            "the refused write did not land"
        );
    }

    #[test]
    fn foreign_keys_are_enforced_and_delete_cascades() {
        let store = SqliteStore::in_memory().expect("store");
        txn(&store, "s:1", |txn| {
            txn.put("a", Some(json!(1)))?;
            txn.put("b", Some(json!(2)))
        })
        .expect("write");
        let connection = store.connection.lock().expect("lock");
        let error = connection
            .execute(
                "INSERT INTO agentscope_part (scope_key, k, v) VALUES ('missing', 'k', '1')",
                [],
            )
            .expect_err("an orphan record is refused");
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
        connection
            .execute("DELETE FROM agentscope_scope WHERE scope_key = 's:1'", [])
            .expect("delete");
        let remaining: i64 = connection
            .query_row(
                "SELECT count(*) FROM agentscope_part WHERE scope_key = 's:1'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(remaining, 0, "the delete cascaded");
    }

    #[test]
    fn a_page_is_capped_at_the_postgres_limit() {
        let store = SqliteStore::in_memory().expect("store");
        let count = MAX_PAGE + 1;
        txn(&store, "s:1", |txn| {
            for index in 0..count {
                txn.put(&format!("row:{index:05}"), Some(json!(index)))?;
            }
            Ok(())
        })
        .expect("write");
        let keys = txn(&store, "s:1", |txn| txn.keys("row:", None, count * 2)).expect("page");
        assert_eq!(keys.len(), MAX_PAGE);
    }
}
