//! Best-effort SQLite index for the local workspace, mirroring `WorkspaceIndex`.
//!
//! Tracks files materialized locally under two path prefixes: `agents/*/sessions/**`
//! and `memory/**`. The index is used to speed up `ls / glob / exists / grep` in
//! remote-backed workspace mode by avoiding full-store key scans. File *content* is
//! never stored — `grep` still fetches each candidate file authoritatively.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension};

use super::constants::{AGENTS_DIR, MEMORY_DIR};

/// Schema version stored in `index_state`; bump when the schema changes.
const SCHEMA_VERSION: i64 = 1;
const INDEX_DIR: &str = ".index";
const INDEX_DB: &str = "workspace.db";

/// Workspace-relative prefixes eligible for indexing.
fn indexed_prefixes() -> Vec<String> {
    vec![format!("{AGENTS_DIR}/"), format!("{MEMORY_DIR}/")]
}

/// Best-effort SQLite index over locally materialized workspace files.
///
/// All methods are best-effort and never propagate errors to callers: a failed
/// upsert/remove/read logs at debug/trace and degrades gracefully.
pub struct WorkspaceIndex {
    // `rusqlite::Connection` is `Send` but not `Sync`, so guard it behind a `Mutex`
    // to make `WorkspaceIndex` shareable via `Arc`.
    conn: Mutex<Connection>,
}

impl WorkspaceIndex {
    /// Opens (or creates) the workspace index under `<workspaceRoot>/.index/workspace.db`.
    ///
    /// Returns `None` when the index cannot be initialized — callers should treat a
    /// missing index as "unavailable" and fall back to a full scan.
    pub fn open(workspace_root: &Path) -> Option<Arc<Self>> {
        let index_dir = workspace_root.join(INDEX_DIR);
        if std::fs::create_dir_all(&index_dir).is_err() {
            tracing::warn!("WorkspaceIndex unavailable (non-fatal): cannot create {index_dir:?}");
            return None;
        }
        let db_file = index_dir.join(INDEX_DB);
        let conn = Connection::open(&db_file).ok()?;
        let index = WorkspaceIndex { conn: Mutex::new(conn) };
        if index.init_schema().is_err() {
            return None;
        }
        Some(Arc::new(index))
    }

    fn init_schema(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path         TEXT PRIMARY KEY,
                size_bytes   INTEGER,
                modified_at  TEXT,
                content_type TEXT,
                encoding     TEXT,
                present_local INTEGER DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS index_state (
                key   TEXT PRIMARY KEY,
                value TEXT
            );",
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO index_state(key, value) VALUES ('schema_version', ?1)",
            rusqlite::params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Whether the given workspace-relative path falls under an indexed prefix.
    pub fn is_indexable(path: &str) -> bool {
        if path.trim().is_empty() {
            return false;
        }
        indexed_prefixes().iter().any(|p| path.starts_with(p))
    }

    /// Upserts a file entry in the index. No-ops if the path is not indexable.
    pub fn upsert(&self, path: &str, size_bytes: i64, modified_at: Option<&str>) {
        if !Self::is_indexable(path) {
            return;
        }
        let ts = modified_at.map(str::to_string).unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339()
        });
        let res = self.conn.lock().unwrap().execute(
            "INSERT INTO files(path, size_bytes, modified_at, present_local)
             VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(path) DO UPDATE SET
                 size_bytes=excluded.size_bytes,
                 modified_at=excluded.modified_at,
                 present_local=1",
            rusqlite::params![path, size_bytes, ts],
        );
        if let Err(e) = res {
            tracing::debug!("Index upsert failed for '{path}' (non-fatal): {e}");
        }
    }

    /// Upserts using local file size/mtime when the file exists on disk.
    pub fn upsert_from_local_file(&self, path: &str, local_file: &Path) {
        if !Self::is_indexable(path) {
            return;
        }
        let (size, mtime) = match std::fs::metadata(local_file) {
            Ok(md) => (
                md.len() as i64,
                md.modified()
                    .ok()
                    .and_then(|t| {
                        t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| {
                            chrono::DateTime::<chrono::Utc>::from_timestamp(
                                d.as_secs() as i64,
                                d.subsec_nanos(),
                            )
                            .unwrap_or_default()
                            .to_rfc3339()
                        })
                    }),
            ),
            Err(_) => (-1, None),
        };
        self.upsert(path, size, mtime.as_deref());
    }

    /// Removes a file entry from the index. No-ops if not indexable.
    pub fn remove(&self, path: &str) {
        if !Self::is_indexable(path) {
            return;
        }
        if let Err(e) = self
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM files WHERE path=?1", rusqlite::params![path])
        {
            tracing::debug!("Index remove failed for '{path}' (non-fatal): {e}");
        }
    }

    /// Renames (moves) an index entry from `from_path` to `to_path`.
    pub fn rename(&self, from_path: &str, to_path: &str) {
        if !Self::is_indexable(from_path) && !Self::is_indexable(to_path) {
            return;
        }
        if let Err(e) = self.conn.lock().unwrap().execute(
            "UPDATE files SET path=?2 WHERE path=?1",
            rusqlite::params![from_path, to_path],
        ) {
            tracing::debug!(
                "Index rename failed '{from_path}' -> '{to_path}' (non-fatal): {e}"
            );
        }
    }

    /// Whether the index contains a locally-present entry for the path.
    pub fn exists(&self, path: &str) -> bool {
        if !Self::is_indexable(path) {
            return false;
        }
        let found: Option<bool> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM files WHERE path=?1 AND present_local=1 LIMIT 1",
                rusqlite::params![path],
                |_| Ok(true),
            )
            .optional()
            .unwrap_or(None);
        found.unwrap_or(false)
    }

    /// Returns all locally-present paths that start with the given prefix.
    pub fn list_by_prefix(&self, prefix: &str) -> Vec<String> {
        let guard = self.conn.lock().unwrap();
        let mut stmt = match guard.prepare("SELECT path FROM files WHERE path LIKE ?1 AND present_local=1")
        {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("Index listByPrefix prepare failed (non-fatal): {e}");
                return Vec::new();
            }
        };
        let pattern = format!("{prefix}%");
        let mut out = Vec::new();
        match stmt.query_map(rusqlite::params![pattern], |row| row.get::<_, String>(0)) {
            Ok(rows) => {
                for row in rows.flatten() {
                    out.push(row);
                }
            }
            Err(e) => {
                tracing::debug!("Index listByPrefix query failed (non-fatal): {e}");
            }
        }
        out
    }

    /// Whether the index has any entries under the given prefix (faster than list).
    pub fn has_prefix(&self, prefix: &str) -> bool {
        let found: Option<bool> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM files WHERE path LIKE ?1 AND present_local=1 LIMIT 1",
                rusqlite::params![format!("{prefix}%")],
                |_| Ok(true),
            )
            .optional()
            .unwrap_or(None);
        found.unwrap_or(false)
    }

    /// Rebuilds the index by walking the local workspace directories
    /// (`agents/*/sessions` and `memory`). Best-effort.
    pub fn rebuild_from_disk(&self, workspace_root: &Path) {
        let _ = self.conn.lock().unwrap().execute("DELETE FROM files", []);
        for prefix in indexed_prefixes() {
            let dir = workspace_root.join(&prefix);
            if !dir.is_dir() {
                continue;
            }
            if let Ok(walk) = walk_files(&dir) {
                for file in walk {
                    let abs = workspace_root
                        .canonicalize()
                        .unwrap_or_else(|_| workspace_root.to_path_buf());
                    let rel = file.strip_prefix(&abs).unwrap_or(&file);
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    self.upsert_from_local_file(&rel_str, &file);
                }
            }
        }
        tracing::info!("WorkspaceIndex rebuilt from local disk");
    }
}

fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("as_ws_idx_{name}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_creates_db_and_schema() {
        let root = temp_dir("open");
        let idx = WorkspaceIndex::open(&root).expect("index opens");
        assert!(root.join(".index/workspace.db").exists());
        idx.upsert("memory/2024-01-01.md", 10, None);
        assert!(idx.exists("memory/2024-01-01.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_indexable_paths_are_ignored() {
        let root = temp_dir("nonindex");
        let idx = WorkspaceIndex::open(&root).unwrap();
        idx.upsert("skills/foo.md", 5, None);
        assert!(!idx.exists("skills/foo.md"));
        assert!(!WorkspaceIndex::is_indexable("skills/foo.md"));
        assert!(WorkspaceIndex::is_indexable("agents/a1/sessions/s.jsonl"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upsert_exists_remove_roundtrip() {
        let root = temp_dir("roundtrip");
        let idx = WorkspaceIndex::open(&root).unwrap();
        let path = "agents/a1/sessions/s1.log.jsonl";
        idx.upsert(path, 42, Some("2024-01-01T00:00:00Z"));
        assert!(idx.exists(path));
        assert!(idx.has_prefix("agents/a1/"));
        assert_eq!(idx.list_by_prefix("agents/a1/sessions/"), vec![path.to_string()]);
        idx.remove(path);
        assert!(!idx.exists(path));
        assert!(!idx.has_prefix("agents/a1/"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_moves_entry() {
        let root = temp_dir("rename");
        let idx = WorkspaceIndex::open(&root).unwrap();
        let from = "agents/a1/sessions/s1.jsonl";
        let to = "agents/a1/sessions/s2.jsonl";
        idx.upsert(from, 7, None);
        idx.rename(from, to);
        assert!(!idx.exists(from));
        assert!(idx.exists(to));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuild_from_disk_walks_indexed_dirs() {
        let root = temp_dir("rebuild");
        std::fs::create_dir_all(root.join("memory")).unwrap();
        std::fs::write(root.join("memory/2024-01-01.md"), "hello").unwrap();
        std::fs::create_dir_all(root.join("agents/a1/sessions")).unwrap();
        std::fs::write(root.join("agents/a1/sessions/s1.log.jsonl"), "{}").unwrap();
        // Non-indexed file should be skipped.
        std::fs::create_dir_all(root.join("skills")).unwrap();
        std::fs::write(root.join("skills/foo.md"), "x").unwrap();

        let idx = WorkspaceIndex::open(&root).unwrap();
        idx.rebuild_from_disk(&root);
        assert!(idx.exists("memory/2024-01-01.md"));
        assert!(idx.exists("agents/a1/sessions/s1.log.jsonl"));
        assert!(!idx.exists("skills/foo.md"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
                rec(&p, out)?;
            } else if p.is_file() {
                out.push(p);
            }
        }
        Ok(())
    }
    rec(dir, &mut out)?;
    Ok(out)
}
