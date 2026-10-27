//! Background skill maintenance orchestrator, mirroring `SkillCurator` and its
//! supporting types (`SkillCuratorConfig`, `SkillCuratorState`, `SkillUsageRecord`,
//! `SkillUsageStore`, `SkillCandidate`).
//!
//! Pure-function transitions walk every agent-tracked skill and move
//! `ACTIVE -> STALE -> ARCHIVED` based on the latest activity timestamp. Pinned
//! and draft skills are skipped. The LLM umbrella pass is a dry-run report only.
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::filesystem::Filesystem;

use super::model::SkillResources;

/// Curator filters and usage backends live in `filters.rs`; re-exported here for
/// parity with the Java `skill.curator` package layout.
pub use super::filters::{
    CanaryFilter, EnvironmentFilter, FilesystemSkillUsageBackend, LoggingNotificationSink,
    NoopNotificationSink, NotificationSink, NotifyAndWaitGate, SkillUsageBackend,
    DEFAULT_USAGE_RELATIVE_PATH,
};
/// Security scanner lives in `security.rs`; re-exported here for parity with the
/// Java `skill.curator` package layout.
pub use super::security::{
    scan, scan_single_file, should_allow, Category as SecurityCategory, Finding as SecurityFinding,
    ScanResult as SecurityScanResult, Severity as SecuritySeverity, TrustLevel as SkillTrustLevel,
    Verdict as SecurityVerdict,
};

/// State sidecar path (workspace-relative).
pub const CURATOR_STATE_PATH: &str = "skills/.curator_state.json";
/// Reports directory (workspace-relative).
pub const CURATOR_REPORTS_DIR: &str = "skills/.curator_reports";

/// Lifecycle state of a usage-tracked skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    Active,
    Stale,
    Archived,
    Draft,
}

/// Usage / lifecycle record for a single skill.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SkillUsageRecord {
    pub created_by: String,
    pub pinned: bool,
    pub state: SkillState,
    pub created_at: DateTime<Utc>,
    pub latest_activity_at: DateTime<Utc>,
    /// Deployment environments this skill is visible in (e.g. `["prod"]`).
    ///
    /// Empty/absent means "visible everywhere". Consumed by the environment filter.
    #[serde(default)]
    pub environments: Vec<String>,
}

impl SkillUsageRecord {
    pub fn agent_created(state: SkillState, latest_activity_at: DateTime<Utc>) -> Self {
        Self {
            created_by: "agent".to_string(),
            pinned: false,
            state,
            created_at: latest_activity_at,
            latest_activity_at,
            environments: Vec::new(),
        }
    }
}

/// Stores per-skill usage/lifecycle records for the curator.
pub trait SkillUsageStore: Send + Sync {
    fn agent_created(&self) -> Vec<(String, SkillUsageRecord)>;
    /// Reads a single record by skill name.
    fn get(&self, name: &str) -> Option<SkillUsageRecord> {
        let _ = name;
        None
    }
    fn set_state(&mut self, name: &str, state: SkillState);
    /// Records (or replaces) a usage/lifecycle record for a skill.
    fn put(&mut self, name: String, record: SkillUsageRecord);
    /// Records recent activity for an agent-created skill (updates `latest_activity_at`).
    fn touch_latest_activity(&mut self, name: &str) {
        let _ = name;
    }
}

/// In-memory [`SkillUsageStore`] for tests and lightweight deployments.
#[derive(Debug, Default)]
pub struct InMemorySkillUsageStore {
    records: BTreeMap<String, SkillUsageRecord>,
}

impl InMemorySkillUsageStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&mut self, name: impl Into<String>, record: SkillUsageRecord) {
        self.records.insert(name.into(), record);
    }
}

impl SkillUsageStore for InMemorySkillUsageStore {
    fn agent_created(&self) -> Vec<(String, SkillUsageRecord)> {
        self.records
            .iter()
            .filter(|(_, r)| r.created_by == "agent" && !r.pinned)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    fn get(&self, name: &str) -> Option<SkillUsageRecord> {
        self.records.get(name).cloned()
    }
    fn set_state(&mut self, name: &str, state: SkillState) {
        if let Some(r) = self.records.get_mut(name) {
            r.state = state;
        }
    }
    fn put(&mut self, name: String, record: SkillUsageRecord) {
        self.records.insert(name, record);
    }
    fn touch_latest_activity(&mut self, name: &str) {
        if let Some(r) = self.records.get_mut(name) {
            r.latest_activity_at = chrono::Utc::now();
        }
    }
}

/// Filesystem-backed [`SkillUsageStore`] persisting usage/lifecycle records to a
/// JSON file, mirroring the default Java `new SkillUsageStore(filesystem)`.
#[derive(Debug)]
pub struct FilesystemSkillUsageStore {
    path: std::path::PathBuf,
    records: BTreeMap<String, SkillUsageRecord>,
}

impl FilesystemSkillUsageStore {
    /// Opens (or creates) a usage store backed by the JSON file at `path`.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let records = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BTreeMap<String, SkillUsageRecord>>(&s).ok())
            .unwrap_or_default();
        Self { path, records }
    }

    /// Creates a store rooted at a workspace, persisting to
    /// `skills/.usage.json`.
    pub fn at_workspace(workspace: impl Into<std::path::PathBuf>) -> Self {
        Self::new(workspace.into().join("skills").join(".usage.json"))
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.records) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

impl SkillUsageStore for FilesystemSkillUsageStore {
    fn agent_created(&self) -> Vec<(String, SkillUsageRecord)> {
        self.records
            .iter()
            .filter(|(_, r)| r.created_by == "agent" && !r.pinned)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    fn get(&self, name: &str) -> Option<SkillUsageRecord> {
        self.records.get(name).cloned()
    }
    fn set_state(&mut self, name: &str, state: SkillState) {
        if let Some(r) = self.records.get_mut(name) {
            r.state = state;
            self.persist();
        }
    }
    fn put(&mut self, name: String, record: SkillUsageRecord) {
        self.records.insert(name, record);
        self.persist();
    }
    fn touch_latest_activity(&mut self, name: &str) {
        if let Some(r) = self.records.get_mut(name) {
            r.latest_activity_at = chrono::Utc::now();
            self.persist();
        }
    }
}

/// Configuration knobs for [`SkillCurator`]. Defaults match the conservative
/// enterprise profile: 7-day cycle, 30/90 day stale/archive cutoffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UmbrellaPassMode {
    Disabled,
    DryRunOnly,
    Live,
}

/// Configuration for [`SkillCurator`].
#[derive(Debug, Clone, Copy)]
pub struct SkillCuratorConfig {
    pub enabled: bool,
    pub interval_hours: i64,
    pub min_idle_hours: i64,
    pub stale_after_days: i64,
    pub archive_after_days: i64,
    pub umbrella_pass_mode: UmbrellaPassMode,
    pub backup_retention: i64,
}

impl SkillCuratorConfig {
    pub fn defaults() -> Self {
        Self {
            enabled: true,
            interval_hours: 24 * 7,
            min_idle_hours: 2,
            stale_after_days: 30,
            archive_after_days: 90,
            umbrella_pass_mode: UmbrellaPassMode::DryRunOnly,
            backup_retention: 5,
        }
    }
}

/// Persistent state for [`SkillCurator`], kept in `skills/.curator_state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct SkillCuratorState {
    #[serde(rename = "last_run_at", default)]
    pub last_run_at: Option<String>,
    #[serde(rename = "run_count", default)]
    pub run_count: i64,
    #[serde(default)]
    pub paused: bool,
    #[serde(rename = "last_run_summary", default)]
    pub last_run_summary: Option<String>,
    #[serde(rename = "last_run_duration_seconds", default)]
    pub last_run_duration_seconds: Option<i64>,
    #[serde(rename = "last_report_path", default)]
    pub last_report_path: Option<String>,
}

impl SkillCuratorState {
    pub fn defaults() -> Self {
        Self::default()
    }
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
    pub fn from_json_str(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// Preview of a script file for reviewers (head + line count + sha256).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptFilePreview {
    pub rel_path: String,
    pub head_preview: String,
    pub total_lines: usize,
    pub sha256: String,
}

/// Material handed to a promotion gate when a draft skill is considered for
/// promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidate {
    pub name: String,
    pub description: String,
    pub skill_md_content: String,
    pub support_file_paths: Vec<String>,
    pub script_files: Vec<ScriptFilePreview>,
}

/// Counts produced by one automatic-transitions pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransitionCounts {
    pub checked: usize,
    pub marked_stale: usize,
    pub archived: usize,
    pub reactivated: usize,
}

/// Outcome of one full curator run.
#[derive(Debug, Clone)]
pub struct CuratorRunReport {
    pub transitions: TransitionCounts,
    pub dry_run_report_path: Option<String>,
    pub ran_at: DateTime<Utc>,
    pub duration_ms: i64,
}

/// Background skill maintenance orchestrator.
pub struct SkillCurator {
    filesystem: Arc<dyn Filesystem>,
    usage_store: Arc<std::sync::Mutex<dyn SkillUsageStore>>,
    config: SkillCuratorConfig,
    /// Called to archive (non-destructively delete) a skill by name.
    archive: Arc<dyn Fn(&str) + Send + Sync>,
}

impl SkillCurator {
    pub fn new(
        filesystem: Arc<dyn Filesystem>,
        usage_store: Arc<std::sync::Mutex<dyn SkillUsageStore>>,
        config: SkillCuratorConfig,
        archive: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        Self {
            filesystem,
            usage_store,
            config,
            archive,
        }
    }

    pub fn config(&self) -> SkillCuratorConfig {
        self.config
    }

    // ---- State sidecar ----

    pub async fn load_state(&self) -> SkillCuratorState {
        let ctx = agentscope_core::agent::RuntimeContext::default();
        match self.filesystem.read(&ctx, CURATOR_STATE_PATH, 0, 0).await {
            Ok(data) if !data.content.trim().is_empty() => {
                SkillCuratorState::from_json_str(&data.content).unwrap_or_default()
            }
            _ => SkillCuratorState::defaults(),
        }
    }

    pub async fn save_state(&self, state: &SkillCuratorState) {
        let ctx = agentscope_core::agent::RuntimeContext::default();
        if let Ok(json) = state.to_json() {
            let _ = self.filesystem.write(&ctx, CURATOR_STATE_PATH, &json).await;
        }
    }

    pub async fn is_paused(&self) -> bool {
        self.load_state().await.paused
    }

    pub async fn set_paused(&self, paused: bool) {
        let mut s = self.load_state().await;
        s.paused = paused;
        self.save_state(&s).await;
    }

    /// Idle-and-interval gate. Seeds `last_run_at` on first run without firing.
    pub async fn should_run_now(&self, now: DateTime<Utc>) -> bool {
        if !self.config.enabled || self.is_paused().await {
            return false;
        }
        let s = self.load_state().await;
        let Some(last_run) = parse_utc(s.last_run_at.as_deref()) else {
            let mut seeded = s;
            seeded.last_run_at = Some(now.to_rfc3339());
            seeded.last_run_summary = Some("deferred first run — curator seeded".into());
            self.save_state(&seeded).await;
            return false;
        };
        let elapsed_hours = (now - last_run).num_hours();
        elapsed_hours >= self.config.interval_hours
    }

    // ---- Automatic transitions ----

    pub async fn apply_automatic_transitions(&self, now: DateTime<Utc>) -> TransitionCounts {
        let stale_cutoff = now - chrono::Duration::days(self.config.stale_after_days);
        let archive_cutoff = now - chrono::Duration::days(self.config.archive_after_days);

        let mut counts = TransitionCounts::default();
        let report = {
            let store = self.usage_store.lock().unwrap();
            store.agent_created()
        };

        for (name, rec) in report {
            // Only auto-manage promoted (created_by == "agent") skills; drafts
            // belong to the promotion gate.
            if rec.created_by != "agent" || rec.pinned {
                continue;
            }
            counts.checked += 1;
            let anchor = rec.latest_activity_at;

            match rec.state {
                SkillState::Active => {
                    if anchor < archive_cutoff {
                        (self.archive)(&name);
                        self.usage_store
                            .lock()
                            .unwrap()
                            .set_state(&name, SkillState::Archived);
                        counts.archived += 1;
                    } else if anchor < stale_cutoff {
                        self.usage_store
                            .lock()
                            .unwrap()
                            .set_state(&name, SkillState::Stale);
                        counts.marked_stale += 1;
                    }
                }
                SkillState::Stale => {
                    if anchor < archive_cutoff {
                        (self.archive)(&name);
                        self.usage_store
                            .lock()
                            .unwrap()
                            .set_state(&name, SkillState::Archived);
                        counts.archived += 1;
                    } else if anchor >= stale_cutoff {
                        self.usage_store
                            .lock()
                            .unwrap()
                            .set_state(&name, SkillState::Active);
                        counts.reactivated += 1;
                    }
                }
                _ => {}
            }
        }
        counts
    }

    // ---- Umbrella dry-run report ----

    pub async fn run_umbrella_dry_run_report(&self, now: DateTime<Utc>) -> Option<String> {
        if self.config.umbrella_pass_mode == UmbrellaPassMode::Disabled {
            return None;
        }
        let ts = now.format("%Y%m%d-%H%M%S").to_string();
        let report_path = format!("{CURATOR_REPORTS_DIR}/{ts}/REPORT.md");

        let mut clusters: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let store = self.usage_store.lock().unwrap();
            for (name, rec) in store.agent_created() {
                if rec.created_by != "agent" || rec.pinned {
                    continue;
                }
                let prefix = name
                    .split(['-', '_', '.'])
                    .next()
                    .unwrap_or(&name)
                    .to_string();
                clusters.entry(prefix).or_default().push(name);
            }
        }

        let mut report = String::new();
        report.push_str("# Skill Curator Dry-Run Report\n\n");
        report.push_str(&format!("Generated: {now}\n"));
        report.push_str(&format!("Mode: {:?}\n\n", self.config.umbrella_pass_mode));
        report.push_str("## Prefix clusters\n\n");

        let multi: Vec<_> = clusters.iter().filter(|(_, v)| v.len() >= 2).collect();
        for (prefix, members) in &multi {
            report.push_str(&format!(
                "- **{prefix}-*** ({} members): {}\n",
                members.len(),
                members.join(", ")
            ));
        }
        if multi.is_empty() {
            report.push_str("(no clusters of 2+ skills found)\n");
        }
        report.push_str("\n## Structured summary (required)\n\n```yaml\n");
        report.push_str("consolidations: []\n");
        report.push_str("prunings: []\n");
        report.push_str("```\n");

        let ctx = agentscope_core::agent::RuntimeContext::default();
        self.filesystem
            .write(&ctx, &report_path, &report)
            .await
            .ok()?;
        Some(report_path)
    }

    /// Run a full curator pass: phase-1 transitions + phase-2 (dry-run) report.
    pub async fn run_once(&self, now: DateTime<Utc>) -> CuratorRunReport {
        let start = std::time::Instant::now();
        let transitions = self.apply_automatic_transitions(now).await;
        let report_path = self.run_umbrella_dry_run_report(now).await;
        let duration_ms = start.elapsed().as_millis() as i64;

        let mut s = self.load_state().await;
        let summary = format!(
            "checked={} markedStale={} archived={} reactivated={}",
            transitions.checked,
            transitions.marked_stale,
            transitions.archived,
            transitions.reactivated
        );
        s.last_run_at = Some(now.to_rfc3339());
        s.run_count += 1;
        s.last_run_summary = Some(summary);
        s.last_run_duration_seconds = Some(duration_ms / 1000);
        s.last_report_path = report_path.clone();
        self.save_state(&s).await;

        CuratorRunReport {
            transitions,
            dry_run_report_path: report_path,
            ran_at: now,
            duration_ms,
        }
    }
}

fn parse_utc(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// A skill candidate plus its support resources and script previews.
pub fn build_candidate(
    name: &str,
    description: &str,
    skill_md_content: &str,
    resources: &SkillResources,
    head_lines: usize,
) -> SkillCandidate {
    let mut support = Vec::new();
    let mut script_files = Vec::new();
    for (rel, content) in resources {
        if rel.ends_with("SKILL.md") {
            continue;
        }
        support.push(rel.clone());
        let preview = ScriptFilePreview {
            rel_path: rel.clone(),
            head_preview: content
                .lines()
                .take(head_lines)
                .collect::<Vec<_>>()
                .join("\n"),
            total_lines: content.lines().count(),
            sha256: super::stager::sha256_hex(content.as_bytes()),
        };
        script_files.push(preview);
    }
    SkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        skill_md_content: skill_md_content.to_string(),
        support_file_paths: support,
        script_files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn iso(secs_from_now: i64) -> DateTime<Utc> {
        Utc::now() + Duration::seconds(secs_from_now)
    }

    #[test]
    fn curator_state_json_roundtrip() {
        let s = SkillCuratorState {
            last_run_at: Some(iso(0).to_rfc3339()),
            run_count: 3,
            paused: true,
            last_run_summary: Some("ok".into()),
            last_run_duration_seconds: Some(2),
            last_report_path: Some("p".into()),
        };
        let json = s.to_json().unwrap();
        let back = SkillCuratorState::from_json_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn config_defaults_conservative() {
        let c = SkillCuratorConfig::defaults();
        assert!(c.enabled);
        assert_eq!(c.interval_hours, 168);
        assert_eq!(c.stale_after_days, 30);
        assert_eq!(c.archive_after_days, 90);
    }

    #[test]
    fn automatic_transitions_archive_oldest() {
        let mut store = InMemorySkillUsageStore::new();
        store.put(
            "old",
            SkillUsageRecord::agent_created(SkillState::Active, iso(-100 * 24 * 3600)),
        );
        store.put(
            "recent",
            SkillUsageRecord::agent_created(SkillState::Active, iso(0)),
        );
        let store = Arc::new(std::sync::Mutex::new(store));
        let archived_names = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let archive_names_clone = Arc::clone(&archived_names);
        let archive = Arc::new(move |name: &str| {
            archive_names_clone.lock().unwrap().push(name.to_string());
        });
        let curator = SkillCurator::new(
            Arc::new(crate::filesystem::LocalFilesystem::new(std::env::temp_dir())),
            store,
            SkillCuratorConfig::defaults(),
            archive,
        );
        let counts = futures::executor::block_on(curator.apply_automatic_transitions(Utc::now()));
        assert_eq!(counts.checked, 2);
        assert_eq!(counts.archived, 1);
        assert_eq!(counts.marked_stale, 0);
        assert_eq!(*archived_names.lock().unwrap(), vec!["old".to_string()]);
    }

    #[test]
    fn filesystem_store_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("asr_fsus_{}", std::process::id()));
        let path = dir.join("skills").join(".usage.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = FilesystemSkillUsageStore::new(path.clone());
        store.put(
            "tool".to_string(),
            SkillUsageRecord::agent_created(SkillState::Active, Utc::now()),
        );
        drop(store);

        let reloaded = FilesystemSkillUsageStore::new(path.clone());
        let records = reloaded.agent_created();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, "tool");
        assert_eq!(records[0].1.state, SkillState::Active);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_candidate_extracts_script_previews() {
        let mut resources = SkillResources::new();
        resources.insert("SKILL.md".into(), "# doc".into());
        resources.insert("scripts/run.py".into(), "print(1)\nprint(2)\n".into());
        let c = build_candidate("tool", "desc", "# doc", &resources, 2);
        assert_eq!(c.name, "tool");
        assert_eq!(c.support_file_paths, vec!["scripts/run.py".to_string()]);
        assert_eq!(c.script_files.len(), 1);
        assert_eq!(c.script_files[0].total_lines, 2);
    }
}
