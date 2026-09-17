//! Skill curator filters and backends, mirroring the remaining
//! `io.agentscope.harness.agent.skill.curator.*` components:
//! `SkillUsageBackend` / `FilesystemSkillUsageBackend`, `CanaryFilter`,
//! `EnvironmentFilter`, `NotificationSink`, and `NotifyAndWaitGate`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::curator::{SkillCandidate, SkillUsageRecord};
use super::promoter::{PromotionDecision, SkillPromotionGate};

// ---------------------------------------------------------------------------
// SkillUsageBackend
// ---------------------------------------------------------------------------

/// Persistence backend for skill usage records, mirroring `SkillUsageBackend`.
///
/// Implementations may use a workspace file (single process) or a distributed store.
pub trait SkillUsageBackend: Send + Sync {
    /// Loads all skill usage records keyed by skill name.
    fn load_all(&self) -> BTreeMap<String, SkillUsageRecord>;

    /// Reads a single record by skill name.
    fn get(&self, name: &str) -> Option<SkillUsageRecord> {
        if name.trim().is_empty() {
            return None;
        }
        self.load_all().get(name).cloned()
    }

    /// Applies `mutator` to the record for `name`.
    ///
    /// `mutator` returns `Some(updated)` to create/replace, or `None` to delete.
    fn mutate(
        &self,
        name: &str,
        mutator: &mut dyn FnMut(SkillUsageRecord) -> Option<SkillUsageRecord>,
    );

    /// Replaces the entire usage map (best-effort for multi-key stores).
    fn replace_all(&self, data: BTreeMap<String, SkillUsageRecord>);
}

/// Default sidecar path for skill usage records.
pub const DEFAULT_USAGE_RELATIVE_PATH: &str = "skills/.usage.json";

/// Filesystem-backed [`SkillUsageBackend`] storing all records in a single JSON sidecar.
pub struct FilesystemSkillUsageBackend {
    path: PathBuf,
    lock: std::sync::Mutex<()>,
}

impl FilesystemSkillUsageBackend {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), lock: std::sync::Mutex::new(()) }
    }

    /// Creates a backend rooted at a workspace, using [`DEFAULT_USAGE_RELATIVE_PATH`].
    pub fn at_workspace(workspace: impl Into<PathBuf>) -> Self {
        Self::new(workspace.into().join(DEFAULT_USAGE_RELATIVE_PATH))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn save_all(&self, data: &BTreeMap<String, SkillUsageRecord>) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(data) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

impl SkillUsageBackend for FilesystemSkillUsageBackend {
    fn load_all(&self) -> BTreeMap<String, SkillUsageRecord> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| {
                if s.trim().is_empty() {
                    None
                } else {
                    serde_json::from_str::<BTreeMap<String, SkillUsageRecord>>(&s).ok()
                }
            })
            .unwrap_or_default()
    }

    fn mutate(
        &self,
        name: &str,
        mutator: &mut dyn FnMut(SkillUsageRecord) -> Option<SkillUsageRecord>,
    ) {
        if name.trim().is_empty() {
            return;
        }
        let _guard = self.lock.lock().unwrap();
        let mut all = self.load_all();
        let existed = all.contains_key(name);
        let current = all
            .get(name)
            .cloned()
            .unwrap_or_else(|| SkillUsageRecord::agent_created(super::curator::SkillState::Active, chrono::Utc::now()));
        match mutator(current) {
            Some(updated) => {
                all.insert(name.to_string(), updated);
            }
            None => {
                if !existed {
                    return;
                }
                all.remove(name);
            }
        }
        self.save_all(&all);
    }

    fn replace_all(&self, data: BTreeMap<String, SkillUsageRecord>) {
        let _guard = self.lock.lock().unwrap();
        self.save_all(&data);
    }
}

// ---------------------------------------------------------------------------
// CanaryFilter
// ---------------------------------------------------------------------------

/// Percentage-based rollout for agent-authored skills.
///
/// Hashes `userId × skillName` and admits the skill iff `hash mod 100 < percent`.
/// Only applies to agent-authored skills; hand-authored / hub-installed skills
/// always pass through.
///
/// `ramp_up_days` is accepted for parity but not yet used (always the static percent).
#[derive(Clone)]
pub struct CanaryFilter {
    percent: i32,
    ramp_up_days: u32,
    store: Option<Arc<dyn SkillUsageBackend>>,
}

impl CanaryFilter {
    pub fn new(percent: i32, store: Option<Arc<dyn SkillUsageBackend>>) -> Self {
        Self::with_ramp_up(percent, 0, store)
    }

    pub fn with_ramp_up(
        percent: i32,
        ramp_up_days: u32,
        store: Option<Arc<dyn SkillUsageBackend>>,
    ) -> Self {
        Self {
            percent: percent.clamp(0, 100),
            ramp_up_days,
            store,
        }
    }

    pub fn percent(&self) -> i32 {
        self.percent
    }

    pub fn ramp_up_days(&self) -> u32 {
        self.ramp_up_days
    }

    /// Stable hash → bucket. Uses Java's `String.hashCode` formula over UTF-8 bytes.
    pub fn stable_bucket(key: &str, modulus: u32) -> u32 {
        let mut h: i32 = 0;
        for b in key.as_bytes() {
            h = h.wrapping_mul(31).wrapping_add(*b as i32);
        }
        if h < 0 {
            h = -(h + 1);
        }
        (h as u32) % modulus
    }

    fn should_pass_for_agent_created(&self, skill_name: &str, user_id: Option<&str>) -> bool {
        if self.percent >= 100 {
            return true;
        }
        if self.percent <= 0 {
            return false;
        }
        let key = format!("{}|{}", user_id.unwrap_or("anonymous"), skill_name);
        (Self::stable_bucket(&key, 100) as i32) < self.percent
    }

    /// Whether `skill_name` is visible to `user_id`. Records that are missing or not
    /// agent-authored pass through unconditionally.
    pub fn is_visible(&self, skill_name: &str, user_id: Option<&str>) -> bool {
        let rec = self.store.as_ref().and_then(|s| s.get(skill_name));
        match rec {
            Some(r) if r.created_by == "agent" => {
                self.should_pass_for_agent_created(skill_name, user_id)
            }
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// EnvironmentFilter
// ---------------------------------------------------------------------------

/// Admits a skill only when its sidecar's environments list contains the configured
/// environment. Skills without a sidecar entry (pre-existing/hand-crafted) pass through.
#[derive(Clone)]
pub struct EnvironmentFilter {
    env: String,
    store: Option<Arc<dyn SkillUsageBackend>>,
}

impl EnvironmentFilter {
    pub fn new(env: impl Into<String>, store: Option<Arc<dyn SkillUsageBackend>>) -> Self {
        let env = env.into();
        Self {
            env: if env.trim().is_empty() { "prod".to_string() } else { env },
            store,
        }
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// Whether the record passes for this environment. `None` passes through.
    pub fn passes(&self, rec: Option<&SkillUsageRecord>) -> bool {
        match rec {
            None => true,
            Some(r) => r.environments.is_empty() || r.environments.iter().any(|e| e == &self.env),
        }
    }

    /// Whether `skill_name` is visible under this environment.
    pub fn is_visible(&self, skill_name: &str) -> bool {
        let rec = self.store.as_ref().and_then(|s| s.get(skill_name));
        self.passes(rec.as_ref())
    }
}

// ---------------------------------------------------------------------------
// NotificationSink
// ---------------------------------------------------------------------------

/// Pushes [`SkillCandidate`] metadata to an external system (Slack webhook, ticket
/// service, config item, …). Sinks are best-effort and never block the gate.
pub trait NotificationSink: Send + Sync {
    fn notify(&self, candidate: &SkillCandidate) -> Result<(), String>;
}

/// No-op sink — the "wire my own later" default.
#[derive(Debug, Clone, Default)]
pub struct NoopNotificationSink;

impl NotificationSink for NoopNotificationSink {
    fn notify(&self, _candidate: &SkillCandidate) -> Result<(), String> {
        Ok(())
    }
}

/// Sink that logs the candidate at INFO level.
#[derive(Debug, Clone, Default)]
pub struct LoggingNotificationSink;

impl NotificationSink for LoggingNotificationSink {
    fn notify(&self, candidate: &SkillCandidate) -> Result<(), String> {
        tracing::info!(
            target: "skill_notification_sink",
            name = %candidate.name,
            description = %candidate.description,
            scripts = candidate.script_files.len(),
            "[skill-promote-request]"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// NotifyAndWaitGate
// ---------------------------------------------------------------------------

/// Multi-replica promotion gate: writes `.review_request.json` next to the draft and
/// fires every configured [`NotificationSink`], then always returns
/// [`PromotionDecision::NeedsReview`] (the Java `Defer`) — approval comes from an
/// external `promoteSkill` call on any replica.
pub struct NotifyAndWaitGate {
    sinks: Vec<Arc<dyn NotificationSink>>,
    drafts_dir: PathBuf,
    retry_after_hours: u64,
}

impl NotifyAndWaitGate {
    pub fn new(sinks: Vec<Arc<dyn NotificationSink>>, drafts_dir: impl Into<PathBuf>) -> Self {
        Self {
            sinks,
            drafts_dir: drafts_dir.into(),
            retry_after_hours: 24,
        }
    }

    pub fn with_retry_after_hours(mut self, hours: u64) -> Self {
        self.retry_after_hours = hours;
        self
    }

    pub fn retry_after_hours(&self) -> u64 {
        self.retry_after_hours
    }

    /// Writes `{drafts_dir}/{name}/.review_request.json`. Returns the file path on success.
    pub fn write_review_request(&self, candidate: &SkillCandidate) -> Option<PathBuf> {
        let dir = self.drafts_dir.join(&candidate.name);
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let payload = serde_json::json!({
            "name": candidate.name,
            "description": candidate.description,
            "skillMdContent": candidate.skill_md_content,
            "supportFilePaths": candidate.support_file_paths,
            "scriptFiles": candidate.script_files.iter().map(|s| serde_json::json!({
                "relPath": s.rel_path,
                "headPreview": s.head_preview,
                "totalLines": s.total_lines,
                "sha256": s.sha256,
            })).collect::<Vec<_>>(),
            "retryAfterHours": self.retry_after_hours,
        });
        let path = dir.join(".review_request.json");
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => std::fs::write(&path, json).ok().map(|_| path),
            Err(_) => None,
        }
    }
}

impl SkillPromotionGate for NotifyAndWaitGate {
    fn review(&self, candidate: &SkillCandidate) -> PromotionDecision {
        let _ = self.write_review_request(candidate);
        for sink in &self.sinks {
            if let Err(e) = sink.notify(candidate) {
                tracing::warn!(error = %e, "skill notification sink failed");
            }
        }
        PromotionDecision::NeedsReview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::curator::SkillState;

    fn rec(created_by: &str, environments: Vec<&str>) -> SkillUsageRecord {
        let mut r = SkillUsageRecord::agent_created(SkillState::Active, chrono::Utc::now());
        r.created_by = created_by.to_string();
        r.environments = environments.into_iter().map(str::to_string).collect();
        r
    }

    fn candidate(name: &str) -> SkillCandidate {
        SkillCandidate {
            name: name.to_string(),
            description: "d".into(),
            skill_md_content: "# d".into(),
            support_file_paths: vec![],
            script_files: vec![],
        }
    }

    #[test]
    fn backend_roundtrip_and_mutate() {
        let dir = std::env::temp_dir().join(format!("asr_backend_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let backend = FilesystemSkillUsageBackend::at_workspace(&dir);

        backend.replace_all(BTreeMap::from([("a".to_string(), rec("agent", vec![]))]));
        assert_eq!(backend.get("a").unwrap().created_by, "agent");

        backend.mutate("b", &mut |mut r| {
            r.pinned = true;
            Some(r)
        });
        assert!(backend.get("b").unwrap().pinned);

        backend.mutate("b", &mut |_| None);
        assert!(backend.get("b").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canary_bucket_is_stable_and_in_range() {
        assert_eq!(CanaryFilter::stable_bucket("alice|skill", 100), CanaryFilter::stable_bucket("alice|skill", 100));
        let b = CanaryFilter::stable_bucket("bob|skill", 100);
        assert!(b < 100);
    }

    #[test]
    fn canary_full_percent_admits_agent_skill() {
        let backend: Arc<dyn SkillUsageBackend> = Arc::new(FakeBackend::new());
        let full = CanaryFilter::new(100, Some(backend));
        assert!(full.is_visible("agent-skill", Some("u")));
    }

    #[test]
    fn canary_passes_non_agent_skills() {
        let backend: Arc<dyn SkillUsageBackend> = Arc::new(FakeBackend::new());
        let filter = CanaryFilter::new(0, Some(backend));
        assert!(filter.is_visible("handmade", Some("u"))); // no record → pass-through
    }

    struct FakeBackend {
        records: BTreeMap<String, SkillUsageRecord>,
    }
    impl FakeBackend {
        fn new() -> Self {
            let mut records = BTreeMap::new();
            records.insert("agent-skill".to_string(), rec("agent", vec![]));
            records.insert("handmade".to_string(), rec("human", vec![]));
            Self { records }
        }
    }
    impl SkillUsageBackend for FakeBackend {
        fn load_all(&self) -> BTreeMap<String, SkillUsageRecord> {
            self.records.clone()
        }
        fn mutate(&self, _name: &str, _m: &mut dyn FnMut(SkillUsageRecord) -> Option<SkillUsageRecord>) {}
        fn replace_all(&self, _data: BTreeMap<String, SkillUsageRecord>) {}
    }

    #[test]
    fn canary_gates_agent_skills_at_zero_percent() {
        let backend: Arc<dyn SkillUsageBackend> = Arc::new(FakeBackend::new());
        let filter = CanaryFilter::new(0, Some(backend));
        assert!(!filter.is_visible("agent-skill", Some("u")));
        assert!(filter.is_visible("handmade", Some("u")));
    }

    #[test]
    fn environment_filter_matches() {
        let backend: Arc<dyn SkillUsageBackend> = Arc::new(FakeBackend::new());
        let filter = EnvironmentFilter::new("prod", Some(backend));
        assert!(filter.passes(None)); // no sidecar → pass
        assert!(filter.passes(Some(&rec("agent", vec![])))); // unrestricted
        assert!(filter.passes(Some(&rec("agent", vec!["prod"]))));
        assert!(!filter.passes(Some(&rec("agent", vec!["staging"]))));
    }

    #[test]
    fn notify_and_wait_gate_defers_and_writes_request() {
        let dir = std::env::temp_dir().join(format!("asr_gate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gate = NotifyAndWaitGate::new(
            vec![Arc::new(NoopNotificationSink), Arc::new(LoggingNotificationSink)],
            &dir,
        );
        assert_eq!(gate.review(&candidate("tool")), PromotionDecision::NeedsReview);
        assert!(dir.join("tool").join(".review_request.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
