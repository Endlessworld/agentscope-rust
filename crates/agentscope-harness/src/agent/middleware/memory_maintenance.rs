//! Middleware that performs periodic memory maintenance after each agent call,
//! mirroring `io.agentscope.harness.agent.middleware.MemoryMaintenanceMiddleware`.
//!
//! Fires on the agent invocation completion (via `on_agent` tail) and is throttled by a
//! configurable minimum gap. Steps: expire daily memory files older than
//! `daily_file_retention_days` into `memory/archive/`, run LLM consolidation if a
//! consolidator is configured, and prune session log files older than
//! `session_retention_days`.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::StreamExt;

use crate::agent::isolation_scope::IsolationScope;
use crate::agent::memory::MemoryConsolidator;
use crate::agent::workspace::constants::{AGENTS_DIR, MEMORY_DIR};
use crate::filesystem::Filesystem;

/// Default minimum gap between two maintenance runs (30 minutes).
pub const DEFAULT_MIN_GAP_SECS: i64 = 1800;

/// Process-wide per-isolation-key maintenance timestamps (millis).
static SHARED_LAST_RUN_AT: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

/// Middleware that performs periodic memory maintenance after each agent call.
pub struct MemoryMaintenanceMiddleware {
    fs: Arc<dyn Filesystem>,
    consolidator: Option<Arc<MemoryConsolidator>>,
    daily_file_retention_days: i64,
    session_retention_days: i64,
    min_gap: chrono::Duration,
    isolation_scope: IsolationScope,
}

impl MemoryMaintenanceMiddleware {
    /// Creates a middleware with the given consolidator (optional) and default retention.
    pub fn new(fs: Arc<dyn Filesystem>, consolidator: Option<MemoryConsolidator>) -> Self {
        Self::new_with_config(
            fs,
            consolidator.map(Arc::new),
            MemoryMaintenanceDefaults::default_daily_retention(),
            MemoryMaintenanceDefaults::default_session_retention(),
            chrono::Duration::seconds(DEFAULT_MIN_GAP_SECS),
            IsolationScope::User,
        )
    }

    /// Creates a middleware with explicit retention and gap configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        fs: Arc<dyn Filesystem>,
        consolidator: Option<Arc<MemoryConsolidator>>,
        daily_file_retention_days: i64,
        session_retention_days: i64,
        min_gap: chrono::Duration,
        isolation_scope: IsolationScope,
    ) -> Self {
        Self {
            fs,
            consolidator,
            daily_file_retention_days,
            session_retention_days,
            min_gap,
            isolation_scope,
        }
    }

    /// Minimum gap enforced between two maintenance runs.
    pub fn min_gap(&self) -> &chrono::Duration {
        &self.min_gap
    }

    fn timer_key_for(&self, ctx: &RuntimeContext) -> String {
        match self.isolation_scope {
            IsolationScope::User => ctx
                .user_id()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_default(),
            IsolationScope::Session => ctx
                .session_id()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_default(),
            IsolationScope::Agent | IsolationScope::Global => String::new(),
        }
    }

    /// Whether maintenance should run now per the throttle gap.
    fn should_run_now(&self, ctx: &RuntimeContext) -> bool {
        let key = format!("{:?}:{}", self.isolation_scope, self.timer_key_for(ctx));
        let now = chrono::Utc::now().timestamp_millis();
        let gap_ms = self.min_gap.num_milliseconds();
        let mut map = SHARED_LAST_RUN_AT.lock().unwrap();
        let last = map
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, t)| *t)
            .unwrap_or(0);
        if now.saturating_sub(last) < gap_ms.max(0) {
            return false;
        }
        if let Some(slot) = map.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = now;
        } else {
            map.push((key, now));
        }
        true
    }

    /// Runs the maintenance steps: expire daily files, consolidate, prune sessions.
    async fn run_maintenance(&self, ctx: &RuntimeContext) {
        self.expire_daily_files(ctx).await;
        if let Some(consolidator) = &self.consolidator {
            let _ = consolidator.consolidate(ctx).await;
        }
        self.prune_old_sessions(ctx).await;
    }

    /// Moves daily memory files older than the retention window to `memory/archive/`.
    async fn expire_daily_files(&self, ctx: &RuntimeContext) {
        let Ok(files) = self.fs.glob(ctx, &format!("{MEMORY_DIR}/*.md"), "/").await else {
            return;
        };
        let cutoff = chrono::Local::now().date_naive()
            - chrono::Duration::days(self.daily_file_retention_days);
        for fi in files {
            if fi.is_directory {
                continue;
            }
            let file_name = file_name_of(&fi.path);
            if file_name.starts_with('.') {
                continue;
            }
            let base = file_name.strip_suffix(".md").unwrap_or(&file_name);
            let Ok(file_date) = chrono::NaiveDate::parse_from_str(base, "%Y-%m-%d") else {
                continue;
            };
            if file_date < cutoff {
                let from = format!("{MEMORY_DIR}/{file_name}");
                let to = format!("{MEMORY_DIR}/archive/{file_name}");
                let _ = self.fs.move_path(ctx, &from, &to).await;
            }
        }
    }

    /// Deletes session log files (`*.log.jsonl`) older than the retention window.
    async fn prune_old_sessions(&self, ctx: &RuntimeContext) {
        let Ok(files) = self
            .fs
            .glob(ctx, &format!("{AGENTS_DIR}/**/*.log.jsonl"), "/")
            .await
        else {
            return;
        };
        let cutoff = chrono::Utc::now() - chrono::Duration::days(self.session_retention_days);
        for fi in files {
            if fi.is_directory {
                continue;
            }
            let Ok(modified) = chrono::DateTime::parse_from_rfc3339(&fi.modified_at) else {
                continue;
            };
            if modified.with_timezone(&chrono::Utc) < cutoff {
                let _ = self.fs.delete(ctx, &fi.path).await;
            }
        }
    }
}

/// Holds default retention values for [`MemoryMaintenanceMiddleware`].
struct MemoryMaintenanceDefaults;
impl MemoryMaintenanceDefaults {
    fn default_daily_retention() -> i64 {
        crate::agent::memory::DEFAULT_DAILY_FILE_RETENTION_DAYS
    }
    fn default_session_retention() -> i64 {
        crate::agent::memory::DEFAULT_SESSION_RETENTION_DAYS
    }
}

fn file_name_of(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[idx + 1..].to_string(),
        None => path.to_string(),
    }
}

impl Middleware for MemoryMaintenanceMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let ctx = ctx.clone();
        let fs = self.fs.clone();
        let consolidator = self.consolidator.clone();
        let daily = self.daily_file_retention_days;
        let session = self.session_retention_days;
        let should_run = self.should_run_now(&ctx);

        let downstream = next(input);
        let tail = async move {
            if !should_run {
                return Vec::new();
            }
            let mw = MemoryMaintenanceMiddleware {
                fs,
                consolidator,
                daily_file_retention_days: daily,
                session_retention_days: session,
                min_gap: chrono::Duration::zero(),
                isolation_scope: IsolationScope::User,
            };
            mw.run_maintenance(&ctx).await;
            Vec::new()
        };
        Box::pin(
            downstream.chain(
                futures::stream::once(tail)
                    .map(futures::stream::iter)
                    .flatten(),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;

    #[tokio::test]
    async fn expire_daily_files_archives_old_dates() {
        let dir = std::env::temp_dir().join(format!("agentscope_maint_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let ctx = RuntimeContext::new();
        // An old daily file that must be archived.
        fs.write(&ctx, &format!("{MEMORY_DIR}/2020-01-01.md"), "old")
            .await
            .unwrap();
        // A fresh daily file that must be kept.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        fs.write(&ctx, &format!("{MEMORY_DIR}/{today}.md"), "fresh")
            .await
            .unwrap();

        let mw = MemoryMaintenanceMiddleware {
            fs: fs.clone(),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
        };
        mw.expire_daily_files(&ctx).await;

        // Old file moved to archive, fresh file stays.
        assert!(fs
            .exists(&ctx, &format!("{MEMORY_DIR}/archive/2020-01-01.md"))
            .await
            .unwrap());
        assert!(!fs
            .exists(&ctx, &format!("{MEMORY_DIR}/2020-01-01.md"))
            .await
            .unwrap());
        assert!(fs
            .exists(&ctx, &format!("{MEMORY_DIR}/{today}.md"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn should_run_now_throttles_within_gap() {
        let dir = std::env::temp_dir().join(format!("agentscope_maint_gap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let ctx = RuntimeContext::new().with_user_id("u1");
        let mw = MemoryMaintenanceMiddleware {
            fs,
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::seconds(3600),
            isolation_scope: IsolationScope::User,
        };
        assert!(mw.should_run_now(&ctx));
        assert!(!mw.should_run_now(&ctx)); // within gap
    }
}
