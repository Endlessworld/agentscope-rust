//! Skill self-learning tools, mirroring `SkillManageTool` / `ProposeSkillTool` and
//! `SkillManageConfig`.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use super::curator::{SkillState, SkillUsageRecord, SkillUsageStore};
use super::model::AgentSkill;
use super::promoter::{SkillAuditLog, SkillPromoter, SkillPromotionGate, AllowListGate};
use super::repository::SkillRepository;

/// Configuration for the `skill_manage` tool, mirroring `SkillManageConfig`.
#[derive(Debug, Clone)]
pub struct SkillManageConfig {
    /// When true, `promote` moves drafts straight to the main repository.
    pub auto_promote: bool,
    /// Resolved main skills directory (writable).
    pub main_dir: PathBuf,
    /// Resolved drafts directory (writable).
    pub drafts_dir: PathBuf,
}

impl SkillManageConfig {
    pub fn new(main_dir: impl Into<PathBuf>, drafts_dir: impl Into<PathBuf>) -> Self {
        Self {
            auto_promote: false,
            main_dir: main_dir.into(),
            drafts_dir: drafts_dir.into(),
        }
    }

    pub fn defaults() -> Self {
        Self {
            auto_promote: false,
            main_dir: PathBuf::from("skills"),
            drafts_dir: PathBuf::from("skills").join("_drafts"),
        }
    }

    pub fn with_auto_promote(mut self, on: bool) -> Self {
        self.auto_promote = on;
        self
    }
}

/// Tool that lets the agent create, update, promote, archive and list skills,
/// mirroring `SkillManageTool`.
pub struct SkillManageTool {
    main_repo: Arc<dyn SkillRepository>,
    drafts_repo: Arc<dyn SkillRepository>,
    config: SkillManageConfig,
    usage_store: Option<Arc<Mutex<dyn SkillUsageStore>>>,
    audit_log: Option<Arc<SkillAuditLog>>,
    promoter: Option<Arc<SkillPromoter>>,
}

impl SkillManageTool {
    pub fn new(
        main_repo: Arc<dyn SkillRepository>,
        drafts_repo: Arc<dyn SkillRepository>,
        config: SkillManageConfig,
    ) -> Self {
        Self {
            main_repo,
            drafts_repo,
            config,
            usage_store: None,
            audit_log: None,
            promoter: None,
        }
    }

    pub fn with_usage_store(mut self, store: Arc<Mutex<dyn SkillUsageStore>>) -> Self {
        self.usage_store = Some(store);
        self
    }

    pub fn with_audit_log(mut self, log: Arc<SkillAuditLog>) -> Self {
        self.audit_log = Some(log);
        self
    }

    pub fn with_promoter(mut self, promoter: Arc<SkillPromoter>) -> Self {
        self.promoter = Some(promoter);
        self
    }

    /// Creates (or overwrites) a draft skill in the drafts repository.
    pub fn create_draft(&self, name: &str, skill_md: &str, resources: Vec<(String, String)>) -> std::io::Result<()> {
        let mut builder = AgentSkill::builder().name(name).description(first_line(skill_md));
        builder = builder.add_resource("SKILL.md".to_string(), skill_md.to_string());
        for (rel, content) in resources {
            builder = builder.add_resource(rel, content);
        }
        let skill = builder.build();
        self.drafts_repo.write_skill(&skill)?;
        if let Some(store) = &self.usage_store {
            let now = chrono::Utc::now();
            store.lock().unwrap().put(
                name.to_string(),
                SkillUsageRecord {
                    created_by: "agent".to_string(),
                    pinned: false,
                    state: SkillState::Draft,
                    created_at: now,
                    latest_activity_at: now,
                },
            );
        }
        if let Some(log) = &self.audit_log {
            log.record(name, "create", "agent", "draft created");
        }
        Ok(())
    }

    /// Promotes a draft skill into the main repository.
    pub fn promote(&self, name: &str, reviewer: &str) -> String {
        if let Some(promoter) = &self.promoter {
            return match promoter.promote(name, reviewer) {
                super::promoter::PromotionResult::Approved { name, target } => {
                    format!("promoted '{name}' to '{target}'")
                }
                super::promoter::PromotionResult::Rejected { name, reason } => {
                    format!("rejected '{name}': {reason}")
                }
                super::promoter::PromotionResult::Invalid { reason } => reason,
            };
        }
        if !self.config.auto_promote {
            return format!("'{name}' staged as draft; promotion requires a promotion gate (auto_promote=false)");
        }
        if let Some(draft) = self.drafts_repo.load().into_iter().find(|s| s.name() == name) {
            if let Err(e) = self.main_repo.write_skill(&draft) {
                return format!("promotion failed: {e}");
            }
            let _ = self.drafts_repo.delete_skill(name);
            if let Some(log) = &self.audit_log {
                log.record(name, "promote", reviewer, "auto-promoted");
            }
            return format!("promoted '{name}' to {}", self.main_repo.repo_id());
        }
        format!("no draft skill named '{name}'")
    }
}

fn first_line(md: &str) -> String {
    for line in md.lines() {
        let l = line.trim();
        if !l.is_empty() {
            return l.trim_start_matches('#').trim().to_string();
        }
    }
    "skill".to_string()
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage agent skills: create drafts, promote them to the main skills directory, \
         archive skills, or list available skills. Use this to capture reusable knowledge."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "promote", "archive", "list"],
                    "description": "The skill-management action to perform."
                },
                "name": {"type": "string", "description": "Skill name (required for create/update/promote/archive)."},
                "skill_md": {"type": "string", "description": "SKILL.md content (create/update)."},
                "reviewer": {"type": "string", "description": "Reviewer id for promotion (default 'agent')."}
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| agentscope_core::Error::Tool("missing 'action'".into()))?;
        match action {
            "create" | "update" => {
                let name = required_str(&args, "name")?;
                let skill_md = args
                    .get("skill_md")
                    .and_then(|v| v.as_str())
                    .unwrap_or("# Skill\n")
                    .to_string();
                match self.create_draft(name, &skill_md, vec![]) {
                    Ok(()) => Ok(ToolResultBlock::text(format!("skill '{name}' {action}d"))),
                    Err(e) => Ok(ToolResultBlock::error(format!("failed: {e}"))),
                }
            }
            "promote" => {
                let name = required_str(&args, "name")?;
                let reviewer = args
                    .get("reviewer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent")
                    .to_string();
                Ok(ToolResultBlock::text(self.promote(name, &reviewer)))
            }
            "archive" => {
                let name = required_str(&args, "name")?;
                match self.main_repo.delete_skill(name) {
                    Ok(()) => Ok(ToolResultBlock::text(format!("archived '{name}'"))),
                    Err(e) => Ok(ToolResultBlock::error(format!("failed: {e}"))),
                }
            }
            "list" => {
                let mut out = String::from("drafts:\n");
                for s in self.drafts_repo.load() {
                    out.push_str(&format!("- {}\n", s.name()));
                }
                out.push_str("main:\n");
                for s in self.main_repo.load() {
                    out.push_str(&format!("- {}\n", s.name()));
                }
                Ok(ToolResultBlock::text(out))
            }
            other => Ok(ToolResultBlock::error(format!("unknown action '{other}'"))),
        }
    }
}

/// Tool that proposes a new skill (a convenience wrapper over `skill_manage` create),
/// mirroring `ProposeSkillTool`.
pub struct ProposeSkillTool {
    manage: Arc<SkillManageTool>,
}

impl ProposeSkillTool {
    pub fn new(manage: Arc<SkillManageTool>) -> Self {
        Self { manage }
    }
}

#[async_trait]
impl Tool for ProposeSkillTool {
    fn name(&self) -> &str {
        "propose_skill"
    }

    fn description(&self) -> &str {
        "Propose a new reusable skill. Writes a draft skill (SKILL.md + optional resources) \
         that can later be promoted. Provide a descriptive name and the SKILL.md body."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name (kebab-case)."},
                "skill_md": {"type": "string", "description": "Full SKILL.md markdown body."},
                "auto_promote": {"type": "boolean", "description": "Promote immediately if a gate allows."}
            },
            "required": ["name", "skill_md"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let name = required_str(&args, "name")?;
        let skill_md = args
            .get("skill_md")
            .and_then(|v| v.as_str())
            .ok_or_else(|| agentscope_core::Error::Tool("missing 'skill_md'".into()))?
            .to_string();
        match self.manage.create_draft(name, &skill_md, vec![]) {
            Ok(()) => {
                let auto = args.get("auto_promote").and_then(|v| v.as_bool()).unwrap_or(false);
                if auto {
                    let msg = self.manage.promote(name, "agent");
                    Ok(ToolResultBlock::text(format!("proposed '{name}'; {msg}")))
                } else {
                    Ok(ToolResultBlock::text(format!("proposed draft skill '{name}'")))
                }
            }
            Err(e) => Ok(ToolResultBlock::error(format!("proposal failed: {e}"))),
        }
    }
}

fn required_str<'a>(args: &'a JsonMap, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| agentscope_core::Error::Tool(format!("missing string argument: {key}").into()))
}

/// Builds a `SkillPromoter` from the writable drafts/main repositories, gate and
/// optional usage store / audit log. Mirrors the Java wiring in `build()`.
pub fn build_promoter(
    drafts_repo: Arc<dyn SkillRepository>,
    main_repo: Arc<dyn SkillRepository>,
    gate: Arc<dyn SkillPromotionGate>,
    usage_store: Option<Arc<Mutex<dyn SkillUsageStore>>>,
    audit_log: Option<Arc<SkillAuditLog>>,
) -> Arc<SkillPromoter> {
    let mut promoter = SkillPromoter::new(drafts_repo, main_repo, gate);
    if let Some(store) = usage_store {
        promoter = promoter.with_usage_store(store);
    }
    if let Some(log) = audit_log {
        promoter = promoter.with_audit_log(log);
    }
    Arc::new(promoter)
}

/// Convenience: builds the default reject-all gate.
pub fn default_gate() -> Arc<dyn SkillPromotionGate> {
    Arc::new(AllowListGate::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::repository::WorkspaceSkillRepository;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("asr_manage_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn manage_create_and_list() {
        let drafts = temp("d");
        let main = temp("m");
        let tool = SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main.clone(), drafts.clone()),
        );
        let _ = tool.create_draft("demo", "# Demo\n", vec![]);
        assert_eq!(tool.drafts_repo.load().len(), 1);

        let args = json!({"action": "list"}).as_object().unwrap().clone();
        let out = tool.call(args, ToolExecutionContext::default()).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("demo"));
    }

    #[tokio::test]
    async fn propose_creates_draft() {
        let drafts = temp("pd");
        let main = temp("pm");
        let manage = Arc::new(SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main, drafts),
        ));
        let propose = ProposeSkillTool::new(Arc::clone(&manage));
        let args = json!({"name": "probe", "skill_md": "# Probe\n"}).as_object().unwrap().clone();
        let out = propose.call(args, ToolExecutionContext::default()).await.unwrap();
        assert!(out.text_content().contains("probe"));
        assert_eq!(manage.drafts_repo.load().len(), 1);
    }
}
