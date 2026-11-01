//! Skill visibility + promotion governance, mirroring
//! `SkillVisibilityFilter` and the `SkillPromotionGate` / `SkillPromoter` /
//! `SkillAuditLog` curator types.
use std::sync::Arc;

use super::curator::SkillCandidate;
use super::repository::SkillRepository;

/// Decides whether a candidate skill is visible for promotion, mirroring
/// `io.agentscope.harness.agent.skill.curator.SkillVisibilityFilter`.
pub trait SkillVisibilityFilter: Send + Sync {
    fn is_visible(&self, candidate: &SkillCandidate) -> bool;
}

/// Allow-list visibility filter (only named skills are visible).
#[derive(Debug, Clone, Default)]
pub struct AllowListFilter {
    allowed: Vec<String>,
}

impl AllowListFilter {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl SkillVisibilityFilter for AllowListFilter {
    fn is_visible(&self, candidate: &SkillCandidate) -> bool {
        self.allowed.iter().any(|n| n == &candidate.name)
    }
}

/// Deny-list visibility filter (all but named skills are visible).
#[derive(Debug, Clone, Default)]
pub struct DenyListFilter {
    denied: Vec<String>,
}

impl DenyListFilter {
    pub fn new(denied: Vec<String>) -> Self {
        Self { denied }
    }
}

impl SkillVisibilityFilter for DenyListFilter {
    fn is_visible(&self, candidate: &SkillCandidate) -> bool {
        !self.denied.iter().any(|n| n == &candidate.name)
    }
}

/// Chains several visibility filters with AND semantics: a candidate is visible only
/// when every filter admits it. Mirrors Java `curator/CompositeFilter`.
#[derive(Default)]
pub struct CompositeFilter {
    filters: Vec<Arc<dyn SkillVisibilityFilter>>,
}

impl CompositeFilter {
    pub fn new(filters: Vec<Arc<dyn SkillVisibilityFilter>>) -> Self {
        Self { filters }
    }

    pub fn filters(&self) -> &[Arc<dyn SkillVisibilityFilter>] {
        &self.filters
    }
}

impl SkillVisibilityFilter for CompositeFilter {
    fn is_visible(&self, candidate: &SkillCandidate) -> bool {
        self.filters.iter().all(|f| f.is_visible(candidate))
    }
}

/// Decision returned by a [`SkillPromotionGate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionDecision {
    /// The candidate may be promoted automatically.
    Approve,
    /// The candidate is rejected with a human-readable reason.
    Reject(String),
    /// The candidate requires interactive human review before promotion.
    NeedsReview,
}

/// Reviews a draft skill for promotion, mirroring
/// `io.agentscope.harness.agent.skill.curator.SkillPromotionGate`.
pub trait SkillPromotionGate: Send + Sync {
    fn review(&self, candidate: &SkillCandidate) -> PromotionDecision;
}

/// Gate that rejects every candidate (safe default, mirrors `RejectAllGate`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RejectAllGate;

impl SkillPromotionGate for RejectAllGate {
    fn review(&self, _candidate: &SkillCandidate) -> PromotionDecision {
        PromotionDecision::Reject("no promotion gate configured".to_string())
    }
}

/// Gate that approves only explicitly allow-listed candidate names.
#[derive(Debug, Clone, Default)]
pub struct AllowListGate {
    allowed: Vec<String>,
}

impl AllowListGate {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl SkillPromotionGate for AllowListGate {
    fn review(&self, candidate: &SkillCandidate) -> PromotionDecision {
        if self.allowed.iter().any(|n| n == &candidate.name) {
            PromotionDecision::Approve
        } else {
            PromotionDecision::Reject(format!("name '{}' not in allow-list", candidate.name))
        }
    }
}

/// Outcome of a promotion attempt, mirroring `SkillPromoter.PromotionResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionResult {
    Approved { name: String, target: String },
    Rejected { name: String, reason: String },
    Invalid { reason: String },
}

impl PromotionResult {
    pub fn invalid(reason: impl Into<String>) -> Self {
        PromotionResult::Invalid {
            reason: reason.into(),
        }
    }
}

/// A single audit-log record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub day_utc: String,
    pub name: String,
    pub action: String,
    pub reviewer: String,
    pub detail: String,
}

impl AuditEntry {
    pub fn matches(&self, day_utc: &str, predicate: &dyn Fn(&AuditEntry) -> bool) -> bool {
        self.day_utc == day_utc && predicate(self)
    }
}

/// Append-only skill audit log, mirroring `SkillAuditLog`.
///
/// Records are written as newline-delimited JSON under a workspace-relative path
/// (`skills/.audit_log.jsonl`). Querying filters by UTC day and an arbitrary predicate.
#[derive(Clone)]
pub struct SkillAuditLog {
    path: std::path::PathBuf,
}

impl SkillAuditLog {
    pub fn new(workspace: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: workspace.into().join("skills").join(".audit_log.jsonl"),
        }
    }

    /// Records an audit event. `detail` may be empty.
    pub fn record(&self, name: &str, action: &str, reviewer: &str, detail: &str) {
        let day_utc = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let entry = AuditEntry {
            day_utc: day_utc.clone(),
            name: name.to_string(),
            action: action.to_string(),
            reviewer: reviewer.to_string(),
            detail: detail.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            let _ = append_line(&self.path, &line);
        }
    }

    /// Queries audit entries for the given UTC day, filtered by `predicate`.
    pub fn query(&self, day_utc: &str, predicate: impl Fn(&AuditEntry) -> bool) -> Vec<AuditEntry> {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
            .filter(|e| e.matches(day_utc, &predicate))
            .collect()
    }
}

fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Promotes draft skills into the main repository through a [`SkillPromotionGate`],
/// mirroring `SkillPromoter`.
pub struct SkillPromoter {
    drafts_repo: Arc<dyn SkillRepository>,
    main_repo: Arc<dyn SkillRepository>,
    usage_store: Option<Arc<std::sync::Mutex<dyn super::curator::SkillUsageStore>>>,
    gate: Arc<dyn SkillPromotionGate>,
    audit_log: Option<Arc<SkillAuditLog>>,
}

impl SkillPromoter {
    pub fn new(
        drafts_repo: Arc<dyn SkillRepository>,
        main_repo: Arc<dyn SkillRepository>,
        gate: Arc<dyn SkillPromotionGate>,
    ) -> Self {
        Self {
            drafts_repo,
            main_repo,
            usage_store: None,
            gate,
            audit_log: None,
        }
    }

    pub fn with_usage_store(
        mut self,
        store: Arc<std::sync::Mutex<dyn super::curator::SkillUsageStore>>,
    ) -> Self {
        self.usage_store = Some(store);
        self
    }

    pub fn with_audit_log(mut self, log: Arc<SkillAuditLog>) -> Self {
        self.audit_log = Some(log);
        self
    }

    /// Promotes the named draft skill. Mirrors `SkillPromoter#promoteSkill`.
    pub fn promote(&self, name: &str, reviewer_id: &str) -> PromotionResult {
        let drafts = self.drafts_repo.load();
        let Some(draft) = drafts.into_iter().find(|s| s.name() == name) else {
            return PromotionResult::invalid(format!("no draft skill named '{name}'"));
        };

        let candidate = super::curator::build_candidate(
            &draft.name(),
            &draft.description(),
            draft
                .resources()
                .get("SKILL.md")
                .map(|s| s.as_str())
                .unwrap_or(""),
            draft.resources(),
            5,
        );

        match self.gate.review(&candidate) {
            PromotionDecision::Approve => {
                if let Err(e) = self.main_repo.write_skill(&draft) {
                    return PromotionResult::Rejected {
                        name: name.to_string(),
                        reason: format!("failed to write promoted skill: {e}"),
                    };
                }
                let _ = self.drafts_repo.delete_skill(name);
                if let Some(store) = &self.usage_store {
                    let rec = super::curator::SkillUsageRecord {
                        created_by: "agent".to_string(),
                        pinned: false,
                        state: super::curator::SkillState::Active,
                        created_at: chrono::Utc::now(),
                        latest_activity_at: chrono::Utc::now(),
                        environments: Vec::new(),
                    };
                    store.lock().unwrap().put(name.to_string(), rec);
                }
                if let Some(log) = &self.audit_log {
                    log.record(name, "promote", reviewer_id, "promoted to main");
                }
                PromotionResult::Approved {
                    name: name.to_string(),
                    target: self.main_repo.repo_id(),
                }
            }
            PromotionDecision::Reject(reason) => {
                if let Some(log) = &self.audit_log {
                    log.record(name, "reject", reviewer_id, &reason);
                }
                PromotionResult::Rejected {
                    name: name.to_string(),
                    reason,
                }
            }
            PromotionDecision::NeedsReview => PromotionResult::Rejected {
                name: name.to_string(),
                reason: "candidate requires interactive review".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::repository::WorkspaceSkillRepository;

    fn candidate(name: &str) -> SkillCandidate {
        SkillCandidate {
            name: name.to_string(),
            description: "d".to_string(),
            skill_md_content: "# d".to_string(),
            support_file_paths: vec![],
            script_files: vec![],
        }
    }

    #[test]
    fn composite_filter_requires_every_filter_to_admit() {
        let filter = CompositeFilter::new(vec![
            Arc::new(AllowListFilter::new(vec![
                "a".to_string(),
                "b".to_string(),
            ])),
            Arc::new(DenyListFilter::new(vec!["b".to_string()])),
        ]);
        assert!(filter.is_visible(&candidate("a")));
        assert!(!filter.is_visible(&candidate("b")));
        assert!(!filter.is_visible(&candidate("c")));
        assert!(CompositeFilter::default().is_visible(&candidate("c")));
    }

    #[test]
    fn reject_all_gate_rejects() {
        assert!(matches!(
            RejectAllGate.review(&candidate("x")),
            PromotionDecision::Reject(_)
        ));
    }

    #[test]
    fn allow_list_gate_approves_named() {
        let gate = AllowListGate::new(vec!["good".to_string()]);
        assert!(matches!(
            gate.review(&candidate("good")),
            PromotionDecision::Approve
        ));
        assert!(matches!(
            gate.review(&candidate("bad")),
            PromotionDecision::Reject(_)
        ));
    }

    #[test]
    fn audit_log_roundtrips_query() {
        let dir = std::env::temp_dir().join(format!("asr_audit_{}", std::process::id()));
        let log = SkillAuditLog::new(&dir);
        log.record("s1", "promote", "rev", "ok");
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let entries = log.query(&day, |e| e.action == "promote");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "s1");
    }

    #[test]
    fn promoter_moves_draft_to_main() {
        let drafts_root = std::env::temp_dir().join(format!("asr_prom_d_{}", std::process::id()));
        let main_root = std::env::temp_dir().join(format!("asr_prom_m_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&drafts_root);
        let _ = std::fs::remove_dir_all(&main_root);
        std::fs::create_dir_all(&drafts_root).unwrap();
        std::fs::create_dir_all(&main_root).unwrap();

        let draft = crate::agent::skill::model::AgentSkill::builder()
            .name("tool")
            .description("d")
            .add_resource("SKILL.md".to_string(), "# Tool\n".to_string())
            .build();
        let drafts = WorkspaceSkillRepository::new_writable(&drafts_root, "drafts");
        let main = WorkspaceSkillRepository::new_writable(&main_root, "main");
        drafts.write_skill(&draft).unwrap();

        let mut store = crate::agent::skill::curator::InMemorySkillUsageStore::new();
        store.put(
            "tool",
            crate::agent::skill::curator::SkillUsageRecord {
                created_by: "agent".to_string(),
                pinned: false,
                state: crate::agent::skill::curator::SkillState::Active,
                created_at: chrono::Utc::now(),
                latest_activity_at: chrono::Utc::now(),
                environments: Vec::new(),
            },
        );
        let store = Arc::new(std::sync::Mutex::new(store));
        let promoter = SkillPromoter::new(
            Arc::new(drafts.clone()),
            Arc::new(main.clone()),
            Arc::new(AllowListGate::new(vec!["tool".to_string()])),
        )
        .with_usage_store(store);
        let result = promoter.promote("tool", "rev");
        assert!(matches!(result, PromotionResult::Approved { .. }));
        assert_eq!(main.load().len(), 1);
        assert_eq!(drafts.load().len(), 0);
    }
}
