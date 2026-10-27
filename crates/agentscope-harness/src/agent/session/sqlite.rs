//! SQLite-backed atomic session storage, mirroring `SqliteSessionStorage`.
//!
//! A local journal for single-host deployments: one row per journal object, with the version
//! living in the same row, so a compare-and-set is a single SQL statement that SQLite
//! serializes across connections and processes. `rusqlite` is synchronous; each operation
//! opens a short-lived connection inline in the async method, matching the rest of the
//! crate's SQLite usage (no thread hop, no shared connection to guard).

use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscope_core::session::{session_error, AtomicSessionStorage, StorageValue};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};

/// Journal database file created under the session root.
const DATABASE_FILE: &str = "journal.sqlite3";

/// How long a writer waits for SQLite's write lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_millis(10_000);

/// Atomic session storage on a local SQLite database.
#[derive(Debug, Clone)]
pub struct SqliteSessionStorage {
    database: PathBuf,
}

impl SqliteSessionStorage {
    /// Creates storage rooted at `root`, using `<root>/journal.sqlite3`.
    pub fn new(root: &Path) -> Self {
        Self {
            database: root.join(DATABASE_FILE),
        }
    }

    /// The database path this storage reads and writes.
    pub fn database_path(&self) -> &Path {
        &self.database
    }
}

#[async_trait]
impl AtomicSessionStorage for SqliteSessionStorage {
    async fn read(&self, path: &str) -> crate::Result<Option<StorageValue>> {
        // A missing database holds no objects; do not create it just to look.
        if !self.database.exists() {
            return Ok(None);
        }
        let database = self.database.clone();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&database)?;
            let row = connection
                .query_row(
                    "SELECT version, payload FROM session_objects WHERE path = ?1",
                    rusqlite::params![path],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(|error| session_error(format!(
                    "Journal read failed: {}: {error}",
                    database.display()
                )))?;
            let Some((version, payload)) = row else {
                return Ok(None);
            };
            // `version > 0` is a column constraint; re-check after conversion so a tampered
            // database can never surface a version the journal would treat as "unversioned".
            let version = match u64::try_from(version) {
                Ok(version) if version > 0 => version,
                _ => return Err(session_error("Journal object has an invalid version")),
            };
            Ok(Some(StorageValue::new(version, payload)))
        })
        .await
        .map_err(|error| session_error(format!("Journal read task failed: {error}")))?
    }

    async fn compare_and_set(
        &self,
        path: &str,
        expected_version: u64,
        bytes: &[u8],
    ) -> crate::Result<bool> {
        // `u64::MAX` is the "no expected version" sentinel; it can never be stored.
        if expected_version == u64::MAX {
            return Err(session_error("Invalid journal object version"));
        }
        // `rusqlite` is synchronous and a write can wait on SQLite's busy timeout. Running it
        // inline would block the async runtime's worker (and therefore every other task on that
        // thread, including the lease heartbeat), so hop to the blocking pool.
        let database = self.database.clone();
        let path = path.to_string();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&database)?;
            // One statement is the whole transaction: readers never see a partial write, and
            // SQLite serializes competing writers.
            let affected = if expected_version == 0 {
                connection.execute(
                    "INSERT INTO session_objects(path, version, payload) VALUES (?1, 1, ?2)\n                     ON CONFLICT(path) DO NOTHING",
                    rusqlite::params![path, bytes],
                )
            } else {
                let expected = i64::try_from(expected_version)
                    .map_err(|_| session_error("Invalid journal object version"))?;
                connection.execute(
                    "UPDATE session_objects SET version = version + 1, payload = ?1\n                     WHERE path = ?2 AND version = ?3",
                    rusqlite::params![bytes, path, expected],
                )
            }
            .map_err(|error| session_error(format!(
                "Journal CAS failed: {}: {error}",
                database.display()
            )))?;
            // Fewer rows means the expectation lost the race, which is not an error.
            Ok::<bool, crate::Error>(affected == 1)
        })
        .await
        .map_err(|error| session_error(format!("Journal CAS task failed: {error}")))?
    }

    async fn list_paths(&self, prefix: &str) -> crate::Result<Vec<String>> {
        if !self.database.exists() {
            return Ok(Vec::new());
        }
        let database = self.database.clone();
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&database)?;
            let mut statement = connection
                .prepare(
                    "SELECT path FROM session_objects WHERE substr(path, 1, length(?1)) = ?1\n                 ORDER BY path",
                )
                .map_err(|error| session_error(format!(
                    "Journal discovery failed: {}: {error}",
                    database.display()
                )))?;
            let rows = statement
                .query_map(rusqlite::params![prefix], |row| row.get::<_, String>(0))
                .map_err(|error| session_error(format!(
                    "Journal discovery failed: {}: {error}",
                    database.display()
                )))?;
            let mut paths = Vec::new();
            for row in rows {
                paths.push(row.map_err(|error| {
                    session_error(format!(
                        "Journal discovery failed: {}: {error}",
                        database.display()
                    ))
                })?);
            }
            Ok(paths)
        })
        .await
        .map_err(|error| session_error(format!("Journal discovery task failed: {error}")))?
    }
}

/// Opens a connection with the journal pragmas and schema applied.
///
/// Free function so blocking-pool closures do not need to borrow the storage value.
fn open_connection(database: &Path) -> crate::Result<Connection> {
    if let Some(parent) = database.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            session_error(format!(
                "Journal open failed: {}: {error}",
                database.display()
            ))
        })?;
    }
    let connection = Connection::open(database).map_err(|error| {
        session_error(format!(
            "Journal open failed: {}: {error}",
            database.display()
        ))
    })?;
    // Wait for competing writers instead of failing with SQLITE_BUSY.
    connection
        .pragma_update(None, "busy_timeout", BUSY_TIMEOUT.as_millis() as i64)
        .map_err(|error| {
            session_error(format!(
                "Journal open failed: {}: {error}",
                database.display()
            ))
        })?;
    // Rollback journal + full sync: an acknowledged commit survives a crash.
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(|error| {
            session_error(format!(
                "Journal open failed: {}: {error}",
                database.display()
            ))
        })?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(|error| {
            session_error(format!(
                "Journal open failed: {}: {error}",
                database.display()
            ))
        })?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_objects (\n                path    TEXT PRIMARY KEY,\n                version INTEGER NOT NULL CHECK(version > 0),\n                payload BLOB NOT NULL\n            )",
        )
        .map_err(|error| {
            session_error(format!(
                "Journal open failed: {}: {error}",
                database.display()
            ))
        })?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentscope_sqlite_journal_{tag}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_missing_database_reads_as_absent() {
        let dir = temp_dir("missing");
        let storage = SqliteSessionStorage::new(&dir);
        assert!(storage.read("s1/head.json").await.unwrap().is_none());
        assert!(storage.list_paths("s1/").await.unwrap().is_empty());
        // Reading never creates the database.
        assert!(!storage.database_path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn storage_round_trips_and_enforces_versions() {
        let dir = temp_dir("cas");
        let storage = SqliteSessionStorage::new(&dir);

        assert!(storage.read("s1/head.json").await.unwrap().is_none());
        // Create-if-absent succeeds and reports version 1.
        assert!(storage
            .compare_and_set("s1/head.json", 0, b"first")
            .await
            .unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"first".to_vec());
        assert_eq!(value.version, 1);

        // Creating again with the same expectation loses the race.
        assert!(!storage
            .compare_and_set("s1/head.json", 0, b"stale")
            .await
            .unwrap());
        assert_eq!(
            storage.read("s1/head.json").await.unwrap().unwrap().bytes,
            b"first".to_vec()
        );

        // A stale version loses; the current version advances it.
        assert!(!storage
            .compare_and_set("s1/head.json", 2, b"ahead")
            .await
            .unwrap());
        assert!(storage
            .compare_and_set("s1/head.json", 1, b"second")
            .await
            .unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"second".to_vec());
        assert_eq!(value.version, 2);

        // The sentinel version can never be stored.
        assert!(storage
            .compare_and_set("s1/head.json", u64::MAX, b"bad")
            .await
            .is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_journal_persists_across_instances() {
        let dir = temp_dir("reopen");
        {
            let storage = SqliteSessionStorage::new(&dir);
            storage.compare_and_set("s1/a", 0, b"one").await.unwrap();
        }
        let storage = SqliteSessionStorage::new(&dir);
        let value = storage.read("s1/a").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"one".to_vec());
        assert_eq!(value.version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_paths_filters_prefixes_and_sorts() {
        let dir = temp_dir("list");
        let storage = SqliteSessionStorage::new(&dir);
        for path in ["s2/b", "s1/b", "s1/a", "other/c"] {
            storage.compare_and_set(path, 0, b"x").await.unwrap();
        }
        assert_eq!(
            storage.list_paths("s1/").await.unwrap(),
            vec!["s1/a".to_string(), "s1/b".to_string()]
        );
        assert_eq!(storage.list_paths("").await.unwrap().len(), 4);
        assert!(storage.list_paths("missing/").await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
