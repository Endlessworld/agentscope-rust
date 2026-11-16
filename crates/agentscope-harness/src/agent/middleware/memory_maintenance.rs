//! Middleware that performs periodic memory maintenance after each agent call,
//! mirroring `io.agentscope.harness.agent.middleware.MemoryMaintenanceMiddleware`.
//!
//! Fires on the agent invocation completion (via `on_agent` tail) and is throttled by a
//! configurable minimum gap. Steps: expire daily memory files older than
//! `daily_file_retention_days` into `memory/archive/`, run LLM consolidation if a
//! consolidator is configured, and drop sessions idle for longer than
//! `session_retention_days` (journals included).

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::StreamExt;

use crate::agent::isolation_scope::IsolationScope;
use crate::agent::memory::MemoryConsolidator;
use crate::agent::session::storage::SESSION_ROOT;
use crate::agent::workspace::constants::MEMORY_DIR;
use crate::filesystem::Filesystem;

/// Default minimum gap between two maintenance runs (30 minutes).
pub const DEFAULT_MIN_GAP_SECS: i64 = 1800;

/// Default idle window of a session journal, in days, when the host does not configure one.
///
/// The event log carries a full-context checkpoint per model call, so a journal grows with the
/// conversation while a single-file transcript did not: an abandoned sub-agent namespace reached
/// tens of gigabytes. Journals therefore default to a shorter window than
/// `session_retention_days` (180); `AGENTSCOPE_JOURNAL_RETENTION_DAYS` overrides it, and a
/// host-configured `session_retention_days` is used as configured (never capped by this value).
pub const DEFAULT_JOURNAL_RETENTION_DAYS: i64 = 30;

/// Environment override for the idle window applied to session journals, in days.
pub const JOURNAL_RETENTION_ENV: &str = "AGENTSCOPE_JOURNAL_RETENTION_DAYS";

/// The file whose modification time says whether a session is still in use.
const HEAD_FILE: &str = "head.json";
/// Names below a session directory that hold its journal; never part of the walk.
const JOURNAL_DIRS: &[&str] = &[
    "commits",
    "blobs",
    "snapshots",
    "writer-seals",
    "inbox",
    "exports",
];
/// `<agent>/<user>/<session>` sits three levels below [`SESSION_ROOT`]; the extra level keeps a
/// host-specific nesting from being mistaken for a session.
const MAX_JOURNAL_DEPTH: usize = 4;

/// Process-wide per-isolation-key maintenance timestamps (millis).
static SHARED_LAST_RUN_AT: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

/// Per-key serial queues for the background maintenance pass.
///
/// The pass used to be chained onto the agent stream, so every subscriber waited for the
/// consolidator's model call before `onComplete`. It now runs in its own task; the queue
/// keeps two runs of the same key from consolidating concurrently.
static MAINTENANCE_QUEUES: std::sync::Mutex<Vec<(String, Arc<tokio::sync::Mutex<()>>)>> =
    std::sync::Mutex::new(Vec::new());

/// The serial queue for `key`, created on first use.
fn maintenance_queue(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut queues = MAINTENANCE_QUEUES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, queue)) = queues.iter().find(|(candidate, _)| candidate == key) {
        return Arc::clone(queue);
    }
    let queue = Arc::new(tokio::sync::Mutex::new(()));
    queues.push((key.to_string(), Arc::clone(&queue)));
    queue
}

/// Middleware that performs periodic memory maintenance after each agent call.
pub struct MemoryMaintenanceMiddleware {
    fs: Arc<dyn Filesystem>,
    consolidator: Option<Arc<MemoryConsolidator>>,
    daily_file_retention_days: i64,
    session_retention_days: i64,
    min_gap: chrono::Duration,
    isolation_scope: IsolationScope,
    /// Whether the memory steps (daily-file expiry, consolidation) run. Session pruning is
    /// storage governance and keeps running even when the memory hooks are disabled.
    memory_steps: bool,
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
            memory_steps: true,
        }
    }

    /// Keeps only the storage-governance step (session pruning).
    ///
    /// `disable_memory_hooks` used to drop this middleware entirely, so turning the memory
    /// hooks off also stopped pruning abandoned journals.
    pub fn sessions_only(mut self) -> Self {
        self.memory_steps = false;
        self.consolidator = None;
        self
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
        let mut map = SHARED_LAST_RUN_AT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(last) = map.iter().find(|(k, _)| *k == key).map(|(_, t)| *t) else {
            // Cold start: no timestamp is not the epoch. `now - 0` always exceeded the gap,
            // so the first call of every session ran a consolidation unconditionally.
            map.push((key, now));
            return false;
        };
        if now.saturating_sub(last) < gap_ms.max(0) {
            return false;
        }
        if let Some(slot) = map.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = now;
        }
        true
    }

    /// Runs the maintenance steps: expire daily files, consolidate, prune sessions.
    async fn run_maintenance(&self, ctx: &RuntimeContext) {
        if self.memory_steps {
            self.expire_daily_files(ctx).await;
            if let Some(consolidator) = &self.consolidator {
                let _ = consolidator.consolidate(ctx).await;
            }
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

    /// Deletes sessions no client opened for the retention window.
    ///
    /// The journal layout (`.agentscope/sessions/<agent>/<user>/<session>/`) is walked one level
    /// at a time because `glob` reports no modification times -- and because the event log writes
    /// every commit as a new file, so a session racing the sweep must be recognised from its
    /// `head.json`. A session idle past the window is dropped whole (commits, payload blobs and
    /// snapshots): nobody touched it, and its log is only useful to a live run.
    async fn prune_old_sessions(&self, ctx: &RuntimeContext) {
        let cutoff = chrono::Utc::now() - self.journal_retention();
        let mut queue = vec![(SESSION_ROOT.to_string(), 0usize)];
        while let Some((dir, depth)) = queue.pop() {
            let Ok(entries) = self.fs.ls(ctx, &dir).await else {
                continue;
            };
            let mut idle_head = None;
            let mut children = Vec::new();
            for entry in &entries {
                let name = entry.path.rsplit('/').next().unwrap_or_default();
                if entry.is_directory {
                    // Journal internals are never walked: a session is decided by its head file.
                    if !JOURNAL_DIRS.contains(&name) {
                        children.push(entry.path.trim_end_matches('/').to_string());
                    }
                } else if name == HEAD_FILE {
                    idle_head = Some(is_idle_since(&entry.modified_at, cutoff));
                }
            }
            match idle_head {
                Some(true) => {
                    let _ = self.fs.delete(ctx, &dir).await;
                    continue;
                }
                Some(false) => continue,
                None => {}
            }
            if depth >= MAX_JOURNAL_DEPTH {
                continue;
            }
            queue.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }

    /// Idle window applied to journals: the configured session retention, overridable by
    /// [`JOURNAL_RETENTION_ENV`].
    ///
    /// The value is taken as configured: silently capping it at the journal default made a
    /// deliberately longer window (180 days) behave like 30.
    fn journal_retention(&self) -> chrono::Duration {
        let days = std::env::var(JOURNAL_RETENTION_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .filter(|days| *days >= 0)
            .unwrap_or(self.session_retention_days.max(0));
        chrono::Duration::days(days)
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

/// Whether a file's last modification predates `cutoff`.
///
/// An unparseable timestamp keeps the file: the sweep only acts on evidence that the session is
/// idle, so a clock/format surprise must never delete live data.
fn is_idle_since(modified_at: &str, cutoff: chrono::DateTime<chrono::Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(modified_at)
        .map(|modified| modified.with_timezone(&chrono::Utc) < cutoff)
        .unwrap_or(false)
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
        let memory_steps = self.memory_steps;
        let should_run = self.should_run_now(&ctx);
        let queue_key = format!("{:?}:{}", self.isolation_scope, self.timer_key_for(&ctx));

        let fut = async move {
            let downstream = next(input);
            if !should_run {
                return downstream;
            }
            let mw = MemoryMaintenanceMiddleware {
                fs,
                consolidator,
                daily_file_retention_days: daily,
                session_retention_days: session,
                min_gap: chrono::Duration::zero(),
                isolation_scope: IsolationScope::User,
                memory_steps,
            };
            let maintenance = async move { mw.run_maintenance(&ctx).await };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    // Same rule as the flush: the sweep can call the model (consolidation), so it
                    // starts only once this turn's stream is finished. Spawning keeps the
                    // completion event immediate.
                    let tail_handle = handle.clone();
                    let queue = maintenance_queue(&queue_key);
                    let start = futures::stream::once(async move {
                        tail_handle.spawn(async move {
                            let _serial = queue.lock().await;
                            maintenance.await;
                        });
                        Vec::new()
                    })
                    .map(futures::stream::iter)
                    .flatten();
                    let out: EventStream = Box::pin(downstream.chain(start));
                    out
                }
                // No runtime: the inline path is the only way to keep the sweep.
                Err(_) => Box::pin(
                    downstream.chain(
                        futures::stream::once(async move {
                            maintenance.await;
                            Vec::new()
                        })
                        .map(futures::stream::iter)
                        .flatten(),
                    ),
                ),
            }
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;
    use std::path::Path;

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
            memory_steps: true,
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
    async fn prune_old_sessions_drops_idle_journals_and_keeps_live_ones() {
        let dir =
            std::env::temp_dir().join(format!("agentscope_maint_prune_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir.clone()));
        let ctx = RuntimeContext::new();
        let idle = format!("{SESSION_ROOT}/agent/user/s_idle");
        let live = format!("{SESSION_ROOT}/agent/user/s_live");
        for session in [&idle, &live] {
            fs.write(&ctx, &format!("{session}/head.json"), "{}")
                .await
                .unwrap();
            fs.write(&ctx, &format!("{session}/commits/c1.json"), "{}")
                .await
                .unwrap();
        }
        // Only the head file is inspected for idleness, so age it past the 180-day window.
        let head = std::fs::File::options()
            .write(true)
            .open(Path::new(&dir).join(&idle).join("head.json"))
            .unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(200 * 86_400);
        head.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(head);

        let mw = MemoryMaintenanceMiddleware {
            fs: fs.clone(),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
            memory_steps: true,
        };
        mw.prune_old_sessions(&ctx).await;

        assert!(
            !fs.exists(&ctx, &idle).await.unwrap(),
            "an idle journal is dropped whole"
        );
        assert!(
            fs.exists(&ctx, &format!("{live}/head.json")).await.unwrap(),
            "a session still being written survives"
        );
        assert!(
            fs.exists(&ctx, &format!("{live}/commits/c1.json"))
                .await
                .unwrap(),
            "the live journal keeps its commits"
        );
    }

    /// The days-window for journals follows the configured session retention, and the
    /// environment still overrides it. Capping it at the journal default made a deliberately
    /// longer window (180 days) behave like 30.
    #[tokio::test]
    async fn journal_retention_follows_the_configured_window() {
        let mw = MemoryMaintenanceMiddleware {
            fs: Arc::new(LocalFilesystem::sandboxed(std::env::temp_dir())),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
            memory_steps: true,
        };
        assert_eq!(mw.journal_retention(), chrono::Duration::days(180));
        let shorter = MemoryMaintenanceMiddleware {
            fs: Arc::clone(&mw.fs),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 7,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
            memory_steps: true,
        };
        assert_eq!(shorter.journal_retention(), chrono::Duration::days(7));
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
            memory_steps: true,
        };
        // The first call only opens the throttle window: "no timestamp" must not mean "the
        // epoch", which made every session's first call run a consolidation.
        assert!(!mw.should_run_now(&ctx));
        assert!(!mw.should_run_now(&ctx)); // within gap
        let fresh = MemoryMaintenanceMiddleware {
            fs: Arc::new(LocalFilesystem::sandboxed(std::env::temp_dir())),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
            memory_steps: true,
        };
        assert!(fresh.should_run_now(&ctx));
    }

    /// Session pruning is storage governance, not a memory hook: turning the hooks off used
    /// to stop abandoned journals being dropped entirely.
    #[tokio::test]
    async fn sessions_only_skips_the_memory_steps() {
        let dir =
            std::env::temp_dir().join(format!("agentscope_maint_only_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let ctx = RuntimeContext::new();
        fs.write(&ctx, &format!("{MEMORY_DIR}/2020-01-01.md"), "old")
            .await
            .unwrap();
        let mw = MemoryMaintenanceMiddleware {
            fs: fs.clone(),
            consolidator: None,
            daily_file_retention_days: 90,
            session_retention_days: 180,
            min_gap: chrono::Duration::zero(),
            isolation_scope: IsolationScope::User,
            memory_steps: true,
        }
        .sessions_only();

        mw.run_maintenance(&ctx).await;

        assert!(
            fs.exists(&ctx, &format!("{MEMORY_DIR}/2020-01-01.md"))
                .await
                .unwrap(),
            "disabling the memory hooks must not archive the daily ledger"
        );
    }
}
