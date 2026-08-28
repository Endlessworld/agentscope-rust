//! Async tool execution tracking, mirroring `io.agentscope.harness.agent.bus`
//! (`AsyncToolRecord`, `AsyncToolRegistry`, `WorkspaceAsyncToolRegistry`).
//!
//! When a tool execution exceeds the configured timeout, the middleware registers
//! it here. The registry tracks the tool's lifecycle so that:
//! - on normal completion the record is updated and the result delivered via inbox;
//! - on process crash/restart, stale RUNNING records can be detected and cleaned up.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
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
    async fn find_stale(&self, session_id: &str, ttl: ChronoDuration) -> Result<Vec<AsyncToolRecord>>;

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
    ctx: RuntimeContext,
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
            ctx: RuntimeContext::default(),
        }
    }

    fn session_dir(&self, session_id: &str) -> String {
        format!("{}/{}", self.registry_root, sanitize(session_id))
    }

    fn record_path(&self, session_id: &str, id: &str) -> String {
        format!("{}/{id}.json", self.session_dir(session_id))
    }

    async fn ensure_dir(&self, dir: &str) -> Result<()> {
        if !self.fs.exists(&self.ctx, dir).await? {
            self.fs.write(&self.ctx, &format!("{dir}/.keep"), "").await?;
        }
        Ok(())
    }

    async fn read_record(&self, path: &str) -> Option<AsyncToolRecord> {
        let data = self.fs.read(&self.ctx, path, 0, usize::MAX).await.ok()?;
        parse_record(&data.content)
    }

    /// Locates a record by id across all session directories under the root.
    async fn find_record_path(&self, id: &str) -> Option<String> {
        let root_ls = self.fs.ls(&self.ctx, &self.registry_root).await.ok()?;
        for dir in root_ls {
            if !dir.is_directory {
                continue;
            }
            let candidate = format!("{}/{id}.json", dir.path);
            if self.fs.exists(&self.ctx, &candidate).await.unwrap_or(false) {
                return Some(candidate);
            }
        }
        None
    }

    async fn update_status(&self, id: &str, new_status: &str) -> Result<()> {
        let Some(path) = self.find_record_path(id).await else {
            return Ok(());
        };
        let Some(rec) = self.read_record(&path).await else {
            return Ok(());
        };
        // The local backend refuses to overwrite an existing file, so drop the old
        // record before rewriting it with the new status.
        self.fs.delete(&self.ctx, &path).await?;
        self.write_record(&path, &rec, new_status).await
    }

    async fn write_record(&self, path: &str, rec: &AsyncToolRecord, status: &str) -> Result<()> {
        let data = json!({
            "id": rec.id.clone(),
            "sessionId": rec.session_id.clone(),
            "toolName": rec.tool_name.clone(),
            "toolCallId": rec.tool_call_id.clone(),
            "status": status,
            "createdAt": rec.created_at.to_rfc3339(),
        });
        let json = serde_json::to_string(&data)?;
        self.fs.write(&self.ctx, path, &json).await
    }
}

/// Sanitizes a session id for use as a directory name: non `[a-zA-Z0-9._-]` chars
/// become `_`; empty/blank values fall back to `default`.
fn sanitize(s: &str) -> String {
    if s.trim().is_empty() {
        return "default".to_string();
    }
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
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
        let dir = self.session_dir(&record.session_id);
        self.ensure_dir(&dir).await?;
        let path = self.record_path(&record.session_id, &record.id);
        // Idempotent: drop any existing record with the same id before writing.
        self.fs.delete(&self.ctx, &path).await?;
        self.write_record(&path, &record, &record.status).await
    }

    async fn complete(&self, id: &str, _result: &str) -> Result<()> {
        self.update_status(id, AsyncToolRecord::COMPLETED).await
    }

    async fn fail(&self, id: &str, _error: &str) -> Result<()> {
        self.update_status(id, AsyncToolRecord::FAILED).await
    }

    async fn find_stale(&self, session_id: &str, ttl: ChronoDuration) -> Result<Vec<AsyncToolRecord>> {
        let dir = self.session_dir(session_id);
        if !self.fs.exists(&self.ctx, &dir).await? {
            return Ok(Vec::new());
        }
        let entries = self.fs.ls(&self.ctx, &dir).await?;
        let cutoff = Utc::now() - ttl;
        let mut stale = Vec::new();
        for fi in entries {
            if fi.is_directory || !fi.path.ends_with(".json") {
                continue;
            }
            if let Some(rec) = self.read_record(&fi.path).await {
                if rec.status == AsyncToolRecord::RUNNING && rec.created_at < cutoff {
                    stale.push(rec);
                }
            }
        }
        Ok(stale)
    }

    async fn mark_timeout(&self, id: &str) -> Result<()> {
        self.update_status(id, AsyncToolRecord::TIMEOUT).await
    }

    async fn list_session(&self, session_id: &str) -> Result<Vec<AsyncToolRecord>> {
        let dir = self.session_dir(session_id);
        if !self.fs.exists(&self.ctx, &dir).await? {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in self.fs.ls(&self.ctx, &dir).await? {
            if entry.is_directory || !entry.path.ends_with(".json") {
                continue;
            }
            if let Some(rec) = self.read_record(&entry.path).await {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }
}
