//! Stateless accessor for workspace content using a two-layer read architecture,
//! mirroring `WorkspaceManager`.
//!
//! **Read path:** For every read (AGENTS.md, MEMORY.md, knowledge, etc.), the
//! configured `Filesystem` is queried first. If it returns non-empty content, that
//! content is used (filesystem overrides). Otherwise the local workspace disk is
//! read as a fallback.
//!
//! **Write path:** All writes (memory, sessions, tasks) go through the `Filesystem`.
//!
//! **Listing:** File listings (memory files, knowledge files, session logs) union
//! results from both the filesystem layer and local disk, deduplicating by relative
//! path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;

use crate::filesystem::{validate_path, Filesystem};

use super::constants::{
    AGENTS_DIR, AGENTS_MD, KNOWLEDGE_DIR, KNOWLEDGE_MD, MEMORY_DIR, MEMORY_MD, SESSIONS_DIR,
    SKILLS_DIR, TASKS_DIR,
};
use super::index::WorkspaceIndex;
use super::namespace::NamespaceFactory;

/// A per-path lock for workspace-relative files, keyed by relative path.
type PathLocks = std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// Stateless accessor for workspace content using a two-layer read architecture.
pub struct WorkspaceManager {
    workspace: PathBuf,
    filesystem: Option<Arc<dyn Filesystem>>,
    index: Option<Arc<WorkspaceIndex>>,
    namespace_factory: Option<Arc<dyn NamespaceFactory>>,
    owns_index: bool,
    path_locks: PathLocks,
}

impl WorkspaceManager {
    /// Creates a manager with no filesystem layer (local disk only).
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self::with_fs(workspace, None)
    }

    /// Creates a manager with the given filesystem and an internally-owned index.
    pub fn with_fs(workspace: impl Into<PathBuf>, filesystem: Option<Arc<dyn Filesystem>>) -> Self {
        let workspace = workspace.into();
        let index = WorkspaceIndex::open(&workspace);
        Self {
            workspace,
            filesystem,
            index,
            namespace_factory: None,
            owns_index: true,
            path_locks: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Creates a manager with an externally-supplied index (this manager does not own it).
    pub fn with_index(
        workspace: impl Into<PathBuf>,
        filesystem: Option<Arc<dyn Filesystem>>,
        index: Option<Arc<WorkspaceIndex>>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            filesystem,
            index,
            namespace_factory: None,
            owns_index: false,
            path_locks: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Creates a manager with an externally-supplied index and namespace factory.
    pub fn with_namespace(
        workspace: impl Into<PathBuf>,
        filesystem: Option<Arc<dyn Filesystem>>,
        index: Option<Arc<WorkspaceIndex>>,
        namespace_factory: Option<Arc<dyn NamespaceFactory>>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            filesystem,
            index,
            namespace_factory,
            owns_index: false,
            path_locks: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Releases the SQLite-backed index when this manager owns it.
    pub fn close(&self) {
        // rusqlite `Connection` is closed on drop; nothing to do here when externally owned.
        let _ = self.owns_index;
    }

    /// Returns the best-effort workspace index (may be `None` when unavailable).
    pub fn index(&self) -> Option<&WorkspaceIndex> {
        self.index.as_deref()
    }

    /// Returns the configured filesystem layer, if any.
    pub fn filesystem(&self) -> Option<&dyn Filesystem> {
        self.filesystem.as_deref()
    }

    /// Returns the workspace root directory.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Acquires the per-path lock guard for the given relative path.
    fn lock_for(&self, rel: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.path_locks.lock().unwrap();
        locks
            .entry(rel.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Validates the workspace exists and key files are present. Logs warnings for
    /// anything missing. Called once at HarnessAgent build time.
    pub async fn validate(&self) {
        if !self.workspace.is_dir() {
            tracing::warn!(
                "Workspace directory does not exist: {}. Please create it and add AGENTS.md.",
                self.workspace.display()
            );
            return;
        }
        let mut agents_md_exists = self.workspace.join(AGENTS_MD).is_file();
        if !agents_md_exists {
            if let Some(fs) = &self.filesystem {
                match fs.exists(&RuntimeContext::new(), AGENTS_MD).await {
                    Ok(true) => agents_md_exists = true,
                    Ok(false) => {}
                    Err(e) => tracing::debug!(
                        "Filesystem not available at build time, skipping exists check: {e}"
                    ),
                }
            }
        }
        if !agents_md_exists {
            tracing::warn!(
                "AGENTS.md not found in workspace: {}. AGENTS.md defines persona and local conventions for the agent.",
                self.workspace.display()
            );
        }
    }

    /// Resolves a workspace-relative path for runtime user data, applying the namespace prefix.
    pub fn resolve_runtime_data_path(&self, ctx: &RuntimeContext, relative_path: &str) -> PathBuf {
        let ns = self
            .namespace_factory
            .as_deref()
            .map(|f| f.get_namespace(ctx))
            .unwrap_or_default();
        if ns.is_empty() {
            return self.workspace.join(relative_path);
        }
        let mut base = self.workspace.clone();
        for seg in ns {
            base = base.join(seg);
        }
        base.join(relative_path)
    }

    /// Reads AGENTS.md content, returns empty string if not found.
    pub async fn read_agents_md(&self, ctx: &RuntimeContext) -> String {
        self.read_with_override(ctx, AGENTS_MD).await
    }

    /// Reads KNOWLEDGE.md content from the knowledge directory.
    pub async fn read_knowledge_md(&self, ctx: &RuntimeContext) -> String {
        let rel = format!("{KNOWLEDGE_DIR}/{KNOWLEDGE_MD}");
        self.read_with_override(ctx, &rel).await
    }

    /// Reads MEMORY.md content (two-layer: filesystem override, local fallback).
    pub async fn read_memory_md(&self, ctx: &RuntimeContext) -> String {
        self.read_with_override(ctx, MEMORY_MD).await
    }

    /// Reads a UTF-8 file under the workspace using the two-layer pattern.
    pub async fn read_managed_workspace_file_utf8(
        &self,
        ctx: &RuntimeContext,
        relative_path: &str,
    ) -> String {
        let normalized = normalize_relative_path(relative_path);
        if normalized.is_empty() {
            return String::new();
        }
        let resolved = self.workspace.join(&normalized);
        if !resolved.starts_with(&self.workspace) {
            return String::new();
        }
        self.read_with_override(ctx, &normalized).await
    }

    pub fn get_memory_dir(&self, ctx: &RuntimeContext) -> PathBuf {
        self.resolve_runtime_data_path(ctx, MEMORY_DIR)
    }

    pub fn get_skills_dir(&self) -> PathBuf {
        self.workspace.join(SKILLS_DIR)
    }

    pub fn get_knowledge_dir(&self) -> PathBuf {
        self.workspace.join(KNOWLEDGE_DIR)
    }

    /// Lists all files under the knowledge directory tree (union of filesystem + local disk).
    pub async fn list_knowledge_files(&self, ctx: &RuntimeContext) -> Vec<PathBuf> {
        let mut relative_paths: std::collections::BTreeSet<String> = Default::default();

        if let Some(fs) = &self.filesystem {
            if let Ok(matches) = fs.glob(ctx, "*", KNOWLEDGE_DIR).await {
                for fi in matches {
                    if !fi.path.trim().is_empty() {
                        let rel = normalize_relative_path(fi.path.trim());
                        if !rel.is_empty() {
                            relative_paths.insert(rel);
                        }
                    }
                }
            }
        }

        let dir = self.get_knowledge_dir();
        if dir.is_dir() {
            if let Ok(walk) = walk_files(&dir) {
                for p in walk {
                    if let Ok(rel) = p.strip_prefix(&self.workspace) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        relative_paths.insert(rel);
                    }
                }
            }
        }

        relative_paths
            .iter()
            .map(|rel| self.workspace.join(rel))
            .collect()
    }

    pub fn get_session_dir(&self, ctx: &RuntimeContext, agent_id: &str) -> PathBuf {
        let rel = format!("{AGENTS_DIR}/{agent_id}/{SESSIONS_DIR}");
        self.resolve_runtime_data_path(ctx, &rel)
    }

    /// Appends UTF-8 text to a workspace-relative file, creating parents when needed.
    /// All writes go through the `Filesystem`. A per-path lock serialises concurrent
    /// read-merge-write cycles.
    pub async fn append_utf8_workspace_relative(
        &self,
        ctx: &RuntimeContext,
        relative_path: &str,
        content: &str,
    ) {
        if relative_path.trim().is_empty() || content.is_empty() {
            return;
        }
        let normalized = match require_safe_relative_path(relative_path) {
            Ok(n) => n,
            Err(_) => return,
        };
        if normalized.is_empty() {
            return;
        }
        let lock = self.lock_for(&normalized);
        let _guard = lock.lock().await;
        match &self.filesystem {
            None => self.append_local_file(&normalized, content),
            Some(fs) => {
                let existing = match fs.read(ctx, &normalized, 0, 0).await {
                    Ok(data) => data.content,
                    Err(_) => String::new(),
                };
                let merged = format!("{existing}{content}");
                let _ = write_through_fs(fs.as_ref(), ctx, &normalized, &merged).await;
            }
        }
    }

    /// Overwrites a workspace-relative UTF-8 file. All writes go through the filesystem.
    pub async fn write_utf8_workspace_relative(
        &self,
        ctx: &RuntimeContext,
        relative_path: &str,
        content: &str,
    ) {
        if relative_path.trim().is_empty() || content.is_empty() {
            return;
        }
        let normalized = match require_safe_relative_path(relative_path) {
            Ok(n) => n,
            Err(_) => return,
        };
        if normalized.is_empty() {
            return;
        }
        match &self.filesystem {
            None => self.write_local_file(&normalized, content),
            Some(fs) => {
                if let Some(index) = &self.index {
                    index.upsert(&normalized, content.len() as i64, None);
                }
                let _ = write_through_fs(fs.as_ref(), ctx, &normalized, content).await;
            }
        }
    }

    /// Returns the workspace-relative path of a task record store.
    fn task_record_path(&self, agent_id: &str, session_id: &str) -> String {
        format!("{AGENTS_DIR}/{agent_id}/{TASKS_DIR}/{session_id}.json")
    }

    /// Reads the task record map for the given relative path (empty when absent/unreadable).
    async fn read_task_map(&self, ctx: &RuntimeContext, rel: &str) -> serde_json::Value {
        let json = self.read_writable_workspace_relative_utf8(ctx, rel).await;
        if json.trim().is_empty() {
            return serde_json::json!({});
        }
        serde_json::from_str(&json).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Persists a task record map to the given relative path.
    async fn persist_task_map(&self, ctx: &RuntimeContext, rel: &str, map: &serde_json::Value) {
        if let Ok(serialized) = serde_json::to_string_pretty(map) {
            self.write_utf8_workspace_relative(ctx, rel, &serialized)
                .await;
        }
    }

    /// Upserts a task record (given as JSON) into `agents/<agentId>/tasks/<sessionId>.json`.
    pub async fn write_task_record_json(
        &self,
        ctx: &RuntimeContext,
        agent_id: &str,
        session_id: &str,
        task_id: &str,
        record: &serde_json::Value,
    ) {
        if agent_id.trim().is_empty() || session_id.trim().is_empty() || task_id.trim().is_empty() {
            return;
        }
        let rel = self.task_record_path(agent_id, session_id);
        let lock = self.lock_for(&rel);
        let _guard = lock.lock().await;
        let mut map = self.read_task_map(ctx, &rel).await;
        if !map.is_object() {
            map = serde_json::json!({});
        }
        map.as_object_mut()
            .unwrap()
            .insert(task_id.to_string(), record.clone());
        self.persist_task_map(ctx, &rel, &map).await;
    }

    /// Reads a single task record by task id, or `None` if not found.
    pub async fn read_task_record_json(
        &self,
        ctx: &RuntimeContext,
        agent_id: &str,
        session_id: &str,
        task_id: &str,
    ) -> Option<serde_json::Value> {
        if agent_id.trim().is_empty() || session_id.trim().is_empty() || task_id.trim().is_empty() {
            return None;
        }
        let rel = self.task_record_path(agent_id, session_id);
        let map = self.read_task_map(ctx, &rel).await;
        map.get(task_id).cloned()
    }

    /// Returns all task records for the given agent and session, in insertion order.
    pub async fn list_task_records_json(
        &self,
        ctx: &RuntimeContext,
        agent_id: &str,
        session_id: &str,
    ) -> Vec<serde_json::Value> {
        if agent_id.trim().is_empty() || session_id.trim().is_empty() {
            return Vec::new();
        }
        let rel = self.task_record_path(agent_id, session_id);
        let map = self.read_task_map(ctx, &rel).await;
        map.as_object()
            .map(|o| o.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Reads the orphan-sweep marker timestamp for this agent (plain ISO-8601 string).
    pub async fn read_sweep_marker(&self, ctx: &RuntimeContext, agent_id: &str) -> Option<String> {
        if agent_id.trim().is_empty() {
            return None;
        }
        let rel = self.sweep_marker_path(agent_id);
        let content = self.read_writable_workspace_relative_utf8(ctx, &rel).await;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Records the current timestamp as the completion time of the most recent orphan-sweep.
    pub async fn write_sweep_marker(&self, ctx: &RuntimeContext, agent_id: &str) {
        if agent_id.trim().is_empty() {
            return;
        }
        let rel = self.sweep_marker_path(agent_id);
        self.write_utf8_workspace_relative(ctx, &rel, &now_rfc3339())
            .await;
    }

    fn sweep_marker_path(&self, agent_id: &str) -> String {
        format!("{AGENTS_DIR}/{agent_id}/{TASKS_DIR}/_sweep.marker")
    }

    /// Writes a single file under `skills/_drafts/`. Writes go through the standard
    /// workspace path. (Rust does not yet expose the overlay lower-layer for a
    /// cross-replica direct write, so drafts use the normal write path.)
    pub async fn write_draft_skill_file(
        &self,
        ctx: &RuntimeContext,
        relative_path: &str,
        content: &str,
    ) {
        if relative_path.trim().is_empty() || content.is_empty() {
            return;
        }
        let normalized = match require_safe_relative_path(relative_path) {
            Ok(n) => n,
            Err(_) => return,
        };
        if normalized.is_empty() {
            return;
        }
        self.write_utf8_workspace_relative(ctx, &normalized, content)
            .await;
    }

    /// Moves a directory inside the workspace (used by promotion:
    /// `skills/_drafts/<x>` → `skills/<x>`). Best-effort; returns `true` on success.
    pub async fn move_skill(
        &self,
        ctx: &RuntimeContext,
        from_relative: &str,
        to_relative: &str,
    ) -> bool {
        let fs = match &self.filesystem {
            Some(fs) => fs.clone(),
            None => return false,
        };
        let src = match require_safe_relative_path(from_relative) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let dst = match require_safe_relative_path(to_relative) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if src.is_empty() || dst.is_empty() {
            return false;
        }
        fs.move_path(ctx, &src, &dst).await.is_ok()
    }

    /// Returns workspace-relative paths of all memory files (`MEMORY.md` and `memory/*.md`).
    pub async fn list_memory_file_paths(&self, ctx: &RuntimeContext) -> Vec<String> {
        let mut paths: std::collections::BTreeSet<String> = Default::default();

        if let Some(fs) = &self.filesystem {
            if fs.read(ctx, MEMORY_MD, 0, 1).await.is_ok() {
                paths.insert(MEMORY_MD.to_string());
            }
            if let Ok(matches) = fs.glob(ctx, "*.md", MEMORY_DIR).await {
                for fi in matches {
                    if !fi.path.trim().is_empty() {
                        let rel = normalize_relative_path(fi.path.trim());
                        if !rel.is_empty() {
                            paths.insert(rel);
                        }
                    }
                }
            }
        }

        if self.resolve_runtime_data_path(ctx, MEMORY_MD).is_file() {
            paths.insert(MEMORY_MD.to_string());
        }
        let mem_dir = self.get_memory_dir(ctx);
        if mem_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&mem_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() && p.extension().map(|e| e == "md").unwrap_or(false) {
                        let name = p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        paths.insert(format!("{MEMORY_DIR}/{name}"));
                    }
                }
            }
        }
        paths.into_iter().collect()
    }

    /// Workspace-relative string for an absolute path under the workspace.
    pub fn to_workspace_relative_string(&self, absolute_under_workspace: &Path) -> String {
        absolute_under_workspace
            .strip_prefix(&self.workspace)
            .unwrap_or(absolute_under_workspace)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Two-layer read: filesystem first (namespaced), local disk fallback.
    async fn read_with_override(&self, ctx: &RuntimeContext, relative_path: &str) -> String {
        let fs_content = self.read_text_through_filesystem(ctx, relative_path).await;
        if !fs_content.is_empty() {
            return fs_content;
        }
        read_file_quietly(&self.workspace.join(relative_path))
    }

    async fn read_text_through_filesystem(&self, ctx: &RuntimeContext, file_path: &str) -> String {
        let fs = match &self.filesystem {
            Some(fs) => fs,
            None => return String::new(),
        };
        match fs.read(ctx, file_path, 0, 0).await {
            Ok(data) => data.content,
            Err(_) => String::new(),
        }
    }

    /// Reads a workspace-relative UTF-8 file via the safe-path (two-layer) pattern.
    async fn read_writable_workspace_relative_utf8(
        &self,
        ctx: &RuntimeContext,
        relative_path: &str,
    ) -> String {
        let normalized = match require_safe_relative_path(relative_path) {
            Ok(n) => n,
            Err(_) => return String::new(),
        };
        if normalized.is_empty() {
            return String::new();
        }
        self.read_with_override(ctx, &normalized).await
    }

    /// Appends UTF-8 content to a local file under the workspace, creating parents.
    fn append_local_file(&self, relative_path: &str, content: &str) {
        let local = self.workspace.join(relative_path);
        if !local.starts_with(&self.workspace) {
            tracing::warn!("Refusing to write outside workspace: {relative_path}");
            return;
        }
        if let Some(parent) = local.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&local)
        {
            use std::io::Write;
            if f.write_all(content.as_bytes()).is_ok() {
                if let Some(index) = &self.index {
                    index.upsert_from_local_file(relative_path, &local);
                }
            }
        } else {
            tracing::warn!("Failed to append {}: {}", local.display(), relative_path);
        }
    }

    /// Atomically overwrites a workspace-relative UTF-8 file on local disk
    /// (write to a temp sibling, then rename over the target).
    fn write_local_file(&self, relative_path: &str, content: &str) {
        let local = self.workspace.join(relative_path);
        if !local.starts_with(&self.workspace) {
            tracing::warn!("Refusing to write outside workspace: {relative_path}");
            return;
        }
        let temp = local.with_file_name(format!(
            "{}.tmp.{}",
            local.file_name().unwrap_or_default().to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        if let Some(parent) = local.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let write_ok = std::fs::write(&temp, content.as_bytes());
        if write_ok.is_ok() {
            match std::fs::rename(&temp, &local) {
                Ok(()) => {
                    if let Some(index) = &self.index {
                        index.upsert_from_local_file(relative_path, &local);
                    }
                }
                Err(_) => {
                    // Fall back to plain copy when rename fails across devices.
                    let _ = std::fs::copy(&temp, &local);
                    let _ = std::fs::remove_file(&temp);
                    if let Some(index) = &self.index {
                        index.upsert_from_local_file(relative_path, &local);
                    }
                }
            }
        } else {
            tracing::warn!("Failed to write {}: {}", local.display(), relative_path);
            let _ = std::fs::remove_file(&temp);
        }
    }
}

// ---------------------------------------------------------------------
//  Module-level helpers
// ---------------------------------------------------------------------

/// Writes UTF-8 content through a `Filesystem`, deleting an existing target first to
/// implement an idempotent overwrite (`Filesystem::write` refuses to overwrite).
async fn write_through_fs(
    fs: &dyn Filesystem,
    ctx: &RuntimeContext,
    path: &str,
    content: &str,
) -> Result<()> {
    if fs.exists(ctx, path).await.unwrap_or(false) {
        let _ = fs.delete(ctx, path).await;
    }
    fs.write(ctx, path, content).await
}

/// Normalizes a path to workspace-relative form (strip leading slashes, unify separators).
fn normalize_relative_path(relative_path: &str) -> String {
    relative_path
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

/// Validates and normalizes a relative path, rejecting `..` traversal.
fn require_safe_relative_path(relative_path: &str) -> Result<String> {
    let normalized = normalize_relative_path(relative_path);
    if !normalized.is_empty() {
        validate_path(&normalized)?;
    }
    Ok(normalized)
}

/// Reads a UTF-8 file quietly, returning empty string on failure.
fn read_file_quietly(path: &Path) -> String {
    if !path.is_file() {
        return String::new();
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Recursively walks a directory collecting regular files.
fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
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

/// Current UTC time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("as_ws_mgr_{name}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn local_read_and_write_roundtrip() {
        let root = temp_dir("roundtrip");
        std::fs::write(root.join("AGENTS.md"), "# Agent").unwrap();
        let mgr = WorkspaceManager::new(root.clone());
        let ctx = RuntimeContext::new();

        let agents_md = mgr.read_agents_md(&ctx).await;
        assert_eq!(agents_md, "# Agent");

        mgr.write_utf8_workspace_relative(&ctx, "memory/2024-01-01.md", "# Day")
            .await;
        let content = mgr
            .read_managed_workspace_file_utf8(&ctx, "memory/2024-01-01.md")
            .await;
        assert_eq!(content, "# Day");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn append_creates_parent_and_accumulates() {
        let root = temp_dir("append");
        let mgr = WorkspaceManager::new(root.clone());
        let ctx = RuntimeContext::new();
        let rel = "agents/a1/sessions/s1.log.jsonl";
        mgr.append_utf8_workspace_relative(&ctx, rel, "line1\n")
            .await;
        mgr.append_utf8_workspace_relative(&ctx, rel, "line2\n")
            .await;
        let content = mgr.read_managed_workspace_file_utf8(&ctx, rel).await;
        assert_eq!(content, "line1\nline2\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn task_records_roundtrip() {
        let root = temp_dir("task");
        let mgr = WorkspaceManager::new(root.clone());
        let ctx = RuntimeContext::new();
        let record = serde_json::json!({ "taskId": "t1", "status": "RUNNING" });
        mgr.write_task_record_json(&ctx, "agent-a", "session-1", "t1", &record)
            .await;
        let read = mgr
            .read_task_record_json(&ctx, "agent-a", "session-1", "t1")
            .await;
        assert_eq!(read, Some(record.clone()));
        let list = mgr
            .list_task_records_json(&ctx, "agent-a", "session-1")
            .await;
        assert_eq!(list.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn sweep_marker_roundtrip() {
        let root = temp_dir("sweep");
        let mgr = WorkspaceManager::new(root.clone());
        let ctx = RuntimeContext::new();
        mgr.write_sweep_marker(&ctx, "agent-a").await;
        let marker = mgr.read_sweep_marker(&ctx, "agent-a").await;
        assert!(marker.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn namespace_resolution_applies_prefix() {
        let root = temp_dir("ns");
        use super::super::namespace::UserSessionNamespace;
        let factory: Arc<dyn NamespaceFactory> = Arc::new(UserSessionNamespace);
        let mgr = WorkspaceManager::with_namespace(root.clone(), None, None, Some(factory.clone()));
        let ctx = RuntimeContext::new()
            .with_user_id("u1")
            .with_session_id("s1");
        let dir = mgr.get_memory_dir(&ctx);
        assert!(dir.ends_with("users/u1/sessions/s1/memory"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
