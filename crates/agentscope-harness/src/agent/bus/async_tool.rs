//! Async tool execution tracking, mirroring `io.agentscope.harness.agent.bus`
//! (`AsyncToolRecord`, `AsyncToolRegistry`, `WorkspaceAsyncToolRegistry`).
//!
//! When a tool execution exceeds the configured timeout, the middleware registers
//! it here. The registry tracks the tool's lifecycle so that:
//! - on normal completion the record is updated and the result delivered via inbox;
//! - on process crash/restart, stale RUNNING records can be detected and cleaned up.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};

use crate::filesystem::Filesystem;

/// Tracks the lifecycle of an async tool execution offloaded to the background.
#[derive(Debug, Clone, PartialEq)]
pub struct AsyncToolRecord {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl AsyncToolRecord {
    pub const RUNNING: &'static str = "RUNNING";
    pub const COMPLETED: &'static str = "COMPLETED";
    pub const FAILED: &'static str = "FAILED";
    pub const TIMEOUT: &'static str = "TIMEOUT";

    /// Creates a record in `RUNNING` status.
    pub fn new(
        id: impl Into<String>,
        session_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: id.into(),
            session_id: session_id.into(),
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            status: Self::RUNNING.to_string(),
            created_at,
        }
    }
}

/// Registry for tracking async tool executions offloaded to the background.
#[async_trait]
pub trait AsyncToolRegistry: Send + Sync {
    /// Register a new async tool execution (expected status `RUNNING`).
    async fn register(&self, record: AsyncToolRecord) -> Result<()>;

    /// Mark an async tool execution as completed.
    async fn complete(&self, id: &str, _result: &str) -> Result<()>;

    /// Mark an async tool execution as failed.
    async fn fail(&self, id: &str, _error: &str) -> Result<()>;

    /// Find async tool records for `session_id` that have been in RUNNING status
    /// longer than `ttl`. These are likely orphaned due to a process crash.
    async fn find_stale(
        &self,
        session_id: &str,
        ttl: ChronoDuration,
    ) -> Result<Vec<AsyncToolRecord>>;

    /// Mark a stale async tool record as timed out, so it is no longer returned by
    /// subsequent `find_stale` calls.
    async fn mark_timeout(&self, id: &str) -> Result<()>;

    /// Lists all async tool records for `session_id`, regardless of status.
    async fn list_session(&self, session_id: &str) -> Result<Vec<AsyncToolRecord>> {
        let _ = session_id;
        Ok(Vec::new())
    }
}

/// Filesystem-backed `AsyncToolRegistry`.
///
/// Each record is persisted as a JSON file under
/// `{registryRoot}/{sessionId}/{recordId}.json`, so records survive process
/// crashes and can be recovered across restarts.
pub struct WorkspaceAsyncToolRegistry {
    fs: Arc<dyn Filesystem>,
    registry_root: String,
    /// Context for the `AsyncToolRegistry` trait entry points, which carry none.
    ///
    /// Every storage operation resolves its namespace from a context passed in explicitly (see
    /// the `*_with_context` methods); this only feeds the context-free trait shim.
    ctx: Option<RuntimeContext>,
}

impl WorkspaceAsyncToolRegistry {
    /// Creates a registry rooted at `registry_root` (an absolute path within
    /// `filesystem`, e.g. `/bus/async-tools`).
    pub fn new(filesystem: Arc<dyn Filesystem>, registry_root: impl Into<String>) -> Self {
        let mut root = registry_root.into();
        while root.ends_with('/') {
            root.pop();
        }
        Self {
            fs: filesystem,
            registry_root: root,
            ctx: None,
        }
    }

    /// Supplies the runtime context the context-free `AsyncToolRegistry` entry points use.
    ///
    /// Without it those entry points cannot know the caller's agent/user, so every record lands
    /// in the default storage namespace. Callers that can thread a context per call should use
    /// the `*_with_context` methods instead.
    pub fn with_runtime_context(mut self, ctx: RuntimeContext) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Context for the context-free trait entry points.
    ///
    /// Returns an owned value so the fallback needs no process-wide static: the trait shims hold
    /// it for the duration of one call.
    fn ctx_for_trait(&self) -> RuntimeContext {
        if let Some(ctx) = &self.ctx {
            return ctx.clone();
        }
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "WorkspaceAsyncToolRegistry was built without a runtime context; async tool \
                 records are written to the default storage namespace. Configure \
                 `with_runtime_context(..)` to scope them to the caller's agent and user."
            );
        });
        RuntimeContext::default()
    }

    fn session_dir(&self, session_id: &str) -> String {
        format!("{}/{}", self.registry_root, sanitize(session_id))
    }

    fn record_path(&self, session_id: &str, id: &str) -> String {
        format!("{}/{id}.json", self.session_dir(session_id))
    }

    async fn ensure_dir(&self, ctx: &RuntimeContext, dir: &str) -> Result<()> {
        if !self.fs.exists(ctx, dir).await? {
            self.fs.write(ctx, &format!("{dir}/.keep"), "").await?;
        }
        Ok(())
    }

    async fn read_record(&self, ctx: &RuntimeContext, path: &str) -> Option<AsyncToolRecord> {
        let data = self.fs.read(ctx, path, 0, usize::MAX).await.ok()?;
        parse_record(&data.content)
    }

    /// Locates a record by id across all session directories under the root.
    async fn find_record_path(&self, ctx: &RuntimeContext, id: &str) -> Option<String> {
        let root_ls = self.fs.ls(ctx, &self.registry_root).await.ok()?;
        for dir in root_ls {
            if !dir.is_directory {
                continue;
            }
            // `ls` returns directory entries with a trailing slash on some backends. Concatenating
            // `{dir.path}/{id}.json` then produced `...//id.json`; a remote key comparison misses
            // that path, so terminal updates silently no-op and the record stays RUNNING forever.
            // Folding is done here as well as trimming, because the registry root (or the id) may
            // itself contribute a doubled separator.
            let dir_path = dir.path.trim_end_matches('/');
            let candidate = collapse_slashes(&format!("{dir_path}/{id}.json"));
            if self.fs.exists(ctx, &candidate).await.unwrap_or(false) {
                return Some(candidate);
            }
        }
        None
    }

    async fn update_status(&self, ctx: &RuntimeContext, id: &str, new_status: &str) -> Result<()> {
        let Some(path) = self.find_record_path(ctx, id).await else {
            tracing::warn!(
                id,
                status = new_status,
                "async tool record was not found; its terminal status was not persisted"
            );
            return Ok(());
        };
        let Some(rec) = self.read_record(ctx, &path).await else {
            return Ok(());
        };
        // The local backend refuses to overwrite an existing file, so drop the old
        // record before rewriting it with the new status.
        self.fs.delete(ctx, &path).await?;
        self.write_record(ctx, &path, &rec, new_status).await
    }

    async fn write_record(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        rec: &AsyncToolRecord,
        status: &str,
    ) -> Result<()> {
        let data = json!({
            "id": rec.id.clone(),
            "sessionId": rec.session_id.clone(),
            "toolName": rec.tool_name.clone(),
            "toolCallId": rec.tool_call_id.clone(),
            "status": status,
            "createdAt": rec.created_at.to_rfc3339(),
        });
        let json = serde_json::to_string(&data)?;
        self.fs.write(ctx, path, &json).await
    }

    // ---- Context-carrying entry points ------------------------------------------------
    //
    // The `AsyncToolRegistry` trait carries no `RuntimeContext`, so the shim below has to fall
    // back to a default context. These variants let a caller that does hold the run's context
    // scope every read and write to the right agent/user namespace.

    /// Registers a new record under `ctx`.
    pub async fn register_with_context(
        &self,
        ctx: &RuntimeContext,
        record: AsyncToolRecord,
    ) -> Result<()> {
        let dir = self.session_dir(&record.session_id);
        self.ensure_dir(ctx, &dir).await?;
        let path = self.record_path(&record.session_id, &record.id);
        // Idempotent: drop any existing record with the same id before writing.
        self.fs.delete(ctx, &path).await?;
        self.write_record(ctx, &path, &record, &record.status).await
    }

    /// Marks `id` as completed under `ctx`.
    pub async fn complete_with_context(
        &self,
        ctx: &RuntimeContext,
        id: &str,
        _result: &str,
    ) -> Result<()> {
        self.update_status(ctx, id, AsyncToolRecord::COMPLETED)
            .await
    }

    /// Marks `id` as failed under `ctx`.
    pub async fn fail_with_context(
        &self,
        ctx: &RuntimeContext,
        id: &str,
        _error: &str,
    ) -> Result<()> {
        self.update_status(ctx, id, AsyncToolRecord::FAILED).await
    }

    /// Returns the records of `session_id` that have been RUNNING longer than `ttl`, under `ctx`.
    pub async fn find_stale_with_context(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        ttl: ChronoDuration,
    ) -> Result<Vec<AsyncToolRecord>> {
        let dir = self.session_dir(session_id);
        if !self.fs.exists(ctx, &dir).await? {
            return Ok(Vec::new());
        }
        let entries = self.fs.ls(ctx, &dir).await?;
        let cutoff = Utc::now() - ttl;
        let mut stale = Vec::new();
        for fi in entries {
            if fi.is_directory || !fi.path.ends_with(".json") {
                continue;
            }
            let path = collapse_slashes(&fi.path);
            if let Some(rec) = self.read_record(ctx, &path).await {
                if rec.status == AsyncToolRecord::RUNNING && rec.created_at < cutoff {
                    stale.push(rec);
                }
            }
        }
        Ok(stale)
    }

    /// Marks `id` as timed out under `ctx`.
    pub async fn mark_timeout_with_context(&self, ctx: &RuntimeContext, id: &str) -> Result<()> {
        self.update_status(ctx, id, AsyncToolRecord::TIMEOUT).await
    }

    /// Lists every record of `session_id` under `ctx`.
    pub async fn list_session_with_context(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
    ) -> Result<Vec<AsyncToolRecord>> {
        let dir = self.session_dir(session_id);
        if !self.fs.exists(ctx, &dir).await? {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in self.fs.ls(ctx, &dir).await? {
            if entry.is_directory || !entry.path.ends_with(".json") {
                continue;
            }
            let path = collapse_slashes(&entry.path);
            if let Some(rec) = self.read_record(ctx, &path).await {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }
}

/// Collapses runs of `/` into a single separator so a joined path never carries `//`.
///
/// `ls` reports directory entries with a trailing slash on some backends and a registry root may
/// itself end in one, so joining produced `.../session//id.json`. `RemoteFilesystem` compares keys
/// verbatim (`normalize_path` drops a trailing separator but does not fold interior duplicates),
/// so such a path missed the stored record and the terminal status update became a silent no-op.
fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut previous_was_slash = false;
    for c in path.chars() {
        if c == '/' {
            if previous_was_slash {
                continue;
            }
            previous_was_slash = true;
        } else {
            previous_was_slash = false;
        }
        out.push(c);
    }
    out
}

/// Sanitizes a session id for use as a directory name: non `[a-zA-Z0-9._-]` chars
/// become `_`; empty/blank values fall back to `default`.
fn sanitize(s: &str) -> String {
    if s.trim().is_empty() {
        return "default".to_string();
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Parses a persisted record; returns `None` for corrupt/unparseable content.
fn parse_record(content: &str) -> Option<AsyncToolRecord> {
    let data: Value = serde_json::from_str(content).ok()?;
    Some(AsyncToolRecord {
        id: data.get("id")?.as_str()?.to_string(),
        session_id: data.get("sessionId")?.as_str()?.to_string(),
        tool_name: data.get("toolName")?.as_str()?.to_string(),
        tool_call_id: data.get("toolCallId")?.as_str()?.to_string(),
        status: data.get("status")?.as_str()?.to_string(),
        created_at: DateTime::parse_from_rfc3339(data.get("createdAt")?.as_str()?)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()?,
    })
}

#[async_trait]
impl AsyncToolRegistry for WorkspaceAsyncToolRegistry {
    async fn register(&self, record: AsyncToolRecord) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.register_with_context(&ctx, record).await
    }

    async fn complete(&self, id: &str, result: &str) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.complete_with_context(&ctx, id, result).await
    }

    async fn fail(&self, id: &str, error: &str) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.fail_with_context(&ctx, id, error).await
    }

    async fn find_stale(
        &self,
        session_id: &str,
        ttl: ChronoDuration,
    ) -> Result<Vec<AsyncToolRecord>> {
        let ctx = self.ctx_for_trait();
        self.find_stale_with_context(&ctx, session_id, ttl).await
    }

    async fn mark_timeout(&self, id: &str) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.mark_timeout_with_context(&ctx, id).await
    }

    async fn list_session(&self, session_id: &str) -> Result<Vec<AsyncToolRecord>> {
        let ctx = self.ctx_for_trait();
        self.list_session_with_context(&ctx, session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::remote::store::{InMemoryStore, NamespaceFactory};
    use crate::filesystem::remote::RemoteFilesystem;
    use crate::filesystem::LocalFilesystem;

    #[tokio::test]
    async fn a_stale_record_is_marked_timeout() {
        // Regression: `ls` returns directory paths with a trailing slash on this backend, and
        // `format!("{dir}/{id}.json")` produced `...//id.json`. Terminal updates then missed the
        // record and it stayed RUNNING forever.
        let dir = std::env::temp_dir().join(format!("asr_async_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session_dir = dir.join("async-tools").join("session-a");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("call-1.json"),
            json!({
                "id": "call-1",
                "sessionId": "session-a",
                "toolName": "execute",
                "toolCallId": "call-1",
                "status": AsyncToolRecord::RUNNING,
                "createdAt": "2020-01-01T00:00:00+00:00",
            })
            .to_string(),
        )
        .unwrap();

        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let registry = WorkspaceAsyncToolRegistry::new(fs, "async-tools");
        let stale = registry
            .find_stale("session-a", ChronoDuration::minutes(10))
            .await
            .expect("stale scan");
        assert_eq!(stale.len(), 1, "the old record must be found");
        registry.mark_timeout("call-1").await.expect("mark timeout");

        let records = registry
            .list_session("session-a")
            .await
            .expect("list session");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, AsyncToolRecord::TIMEOUT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn terminal_updates_reach_a_record_whose_directory_listed_a_trailing_slash() {
        // `RemoteFilesystem::ls` reports sub-directories with a trailing slash. Joining that with
        // `{id}.json` yielded `.../session-a//call-1.json`, and `normalize_path` keeps an interior
        // doubled separator verbatim, so `exists` missed the record and `mark_timeout` silently
        // did nothing.
        let fs: Arc<dyn Filesystem> = Arc::new(RemoteFilesystem::with_default_namespace(Arc::new(
            InMemoryStore::new(),
        )));
        let registry = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");
        let ctx = RuntimeContext::default();
        let old = Utc::now() - ChronoDuration::hours(1);
        registry
            .register_with_context(
                &ctx,
                AsyncToolRecord::new("call-1", "session-a", "execute", "call-1", old),
            )
            .await
            .expect("register");

        let stale = registry
            .find_stale_with_context(&ctx, "session-a", ChronoDuration::minutes(10))
            .await
            .expect("stale scan");
        assert_eq!(stale.len(), 1);

        let path = registry
            .find_record_path(&ctx, "call-1")
            .await
            .expect("record path");
        assert!(!path.contains("//"), "doubled separator in {path}");

        registry
            .mark_timeout_with_context(&ctx, "call-1")
            .await
            .expect("mark timeout");
        let records = registry
            .list_session_with_context(&ctx, "session-a")
            .await
            .expect("list session");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, AsyncToolRecord::TIMEOUT);
    }

    #[test]
    fn collapse_slashes_folds_only_repeated_separators() {
        assert_eq!(
            collapse_slashes("/bus//session-a///call-1.json"),
            "/bus/session-a/call-1.json"
        );
        assert_eq!(
            collapse_slashes("/bus/session-a/call-1.json"),
            "/bus/session-a/call-1.json"
        );
    }

    #[tokio::test]
    async fn a_supplied_runtime_context_scopes_the_storage_namespace() {
        struct UserNamespace;
        impl NamespaceFactory for UserNamespace {
            fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String> {
                vec![
                    "users".to_string(),
                    ctx.user_id().unwrap_or("anon").to_string(),
                    "async-tools".to_string(),
                ]
            }
        }
        let fs: Arc<dyn Filesystem> = Arc::new(RemoteFilesystem::new(
            Arc::new(InMemoryStore::new()),
            Arc::new(UserNamespace),
        ));
        let alice = RuntimeContext::default().with_user_id("alice");
        let bob = RuntimeContext::default().with_user_id("bob");
        let registry = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");
        registry
            .register_with_context(
                &alice,
                AsyncToolRecord::new("r1", "s", "execute", "c1", Utc::now()),
            )
            .await
            .expect("register");
        assert_eq!(
            registry
                .list_session_with_context(&alice, "s")
                .await
                .expect("alice")
                .len(),
            1
        );
        assert!(
            registry
                .list_session_with_context(&bob, "s")
                .await
                .expect("bob")
                .is_empty(),
            "another user's namespace must stay empty"
        );
        // The context-free trait entry point can only see its own namespace, never Alice's.
        assert!(registry
            .list_session("s")
            .await
            .expect("default")
            .is_empty());
    }
}
