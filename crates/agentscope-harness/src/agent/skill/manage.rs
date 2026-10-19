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
use crate::tool::fuzzy_text_matcher;

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
    /// Called after any mutation that changes the on-disk skill set, so the
    /// skill middleware can drop its lazily-built catalog cache.
    on_write: Option<Arc<dyn Fn() + Send + Sync>>,
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
            on_write: None,
        }
    }

    /// Registers a callback invoked after every skill mutation, used to invalidate
    /// the skill middleware's catalog cache (aligns with Java rebuilding the catalog
    /// per `onSystemPrompt` pass).
    pub fn with_on_write(mut self, cb: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.on_write = Some(cb);
        self
    }

    fn notify_write(&self) {
        if let Some(cb) = &self.on_write {
            cb();
        }
    }

    /// Locates the writable repository holding `name` (drafts take precedence).
    fn locate(&self, name: &str) -> Option<Arc<dyn SkillRepository>> {
        for repo in [&self.drafts_repo, &self.main_repo] {
            if repo.load().iter().any(|s| s.name() == name) {
                return Some(Arc::clone(repo));
            }
        }
        None
    }

    fn load_skill(&self, repo: &Arc<dyn SkillRepository>, name: &str) -> Option<AgentSkill> {
        repo.load().into_iter().find(|s| s.name() == name)
    }

    /// Overwrites a single resource of `skill` (persisting the full resource map).
    fn write_resource(
        &self,
        repo: &Arc<dyn SkillRepository>,
        skill: &AgentSkill,
        rel: &str,
        content: &str,
    ) -> std::io::Result<()> {
        let mut resources = skill.resources().clone();
        resources.insert(rel.to_string(), content.to_string());
        let updated = AgentSkill::builder()
            .name(skill.name())
            .source(skill.source())
            .description(skill.description())
            .metadata(skill.metadata().clone())
            .resources(resources)
            .build();
        repo.write_skill(&updated)
    }

    /// Replaces a skill's `SKILL.md` wholesale (Java `doEdit`).
    fn do_edit(&self, name: &str, content: &str) -> ToolResultBlock {
        if content.trim().is_empty() {
            return ToolResultBlock::error("Missing 'content' parameter (full SKILL.md).");
        }
        let Some(repo) = self.locate(name) else {
            return ToolResultBlock::error(format!("Skill '{name}' not found."));
        };
        let Some(skill) = self.load_skill(&repo, name) else {
            return ToolResultBlock::error(format!("Skill '{name}' not found."));
        };
        if let Err(e) = self.write_resource(&repo, &skill, "SKILL.md", content) {
            return ToolResultBlock::error(format!("Failed to edit skill '{name}': {e}"));
        }
        self.notify_write();
        ToolResultBlock::text(format!("Skill '{name}' SKILL.md replaced."))
    }

    /// Fuzzy patch of a single skill resource (Java `doPatch`).
    fn do_patch(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        file_path: Option<&str>,
        replace_all: bool,
    ) -> ToolResultBlock {
        let rel = file_path.filter(|p| !p.is_empty()).unwrap_or("SKILL.md");
        let Some(repo) = self.locate(name) else {
            return ToolResultBlock::error(format!("Skill '{name}' not found."));
        };
        let Some(skill) = self.load_skill(&repo, name) else {
            return ToolResultBlock::error(format!("Skill '{name}' not found."));
        };
        let Some(existing) = skill.resources().get(rel).cloned() else {
            return ToolResultBlock::error(format!("File not found: {rel} (in skill '{name}')"));
        };
        let found = fuzzy_text_matcher::search(&existing, old_string);
        if found.is_empty() {
            return ToolResultBlock::error(format!(
                "old_string not found in {rel} at any fuzziness level (tried exact, \
                 trailing-whitespace, and collapsed-whitespace)."
            ));
        }
        let (updated, replacements) = if replace_all {
            // Apply right-to-left so earlier byte offsets stay valid.
            let mut s = existing.clone();
            for m in found.matches.iter().rev() {
                s.replace_range(m.start..m.end, new_string);
            }
            (s, found.matches.len())
        } else {
            if found.matches.len() > 1 {
                return ToolResultBlock::error(format!(
                    "old_string is not unique in {rel} (found {} occurrences at match level \
                     {:?}). Add more surrounding context to make it unique, or set \
                     replace_all=true.",
                    found.matches.len(),
                    found.level
                ));
            }
            let m = &found.matches[0];
            let mut s = existing.clone();
            s.replace_range(m.start..m.end, new_string);
            (s, 1)
        };
        if let Err(e) = self.write_resource(&repo, &skill, rel, &updated) {
            return ToolResultBlock::error(format!("Failed to write patched file: {e}"));
        }
        self.notify_write();
        let suffix = if found.level == fuzzy_text_matcher::Level::Exact {
            String::new()
        } else {
            format!(" [fuzzy: {:?}]", found.level)
        };
        ToolResultBlock::text(format!(
            "Patched {rel} in '{name}' ({replacements} replacement(s)){suffix}"
        ))
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
                    environments: Vec::new(),
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
                    "enum": ["create", "update", "edit", "patch", "promote", "archive", "list"],
                    "description": "The skill-management action to perform."
                },
                "name": {"type": "string", "description": "Skill name (required for create/update/edit/patch/promote/archive)."},
                "skill_md": {"type": "string", "description": "SKILL.md content (create/update)."},
                "content": {"type": "string", "description": "Full SKILL.md content (edit)."},
                "old_string": {"type": "string", "description": "Text to replace (patch)."},
                "new_string": {"type": "string", "description": "Replacement text (patch)."},
                "file_path": {"type": "string", "description": "Skill-relative file to patch (patch; default SKILL.md)."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (patch; default false)."},
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
                    Ok(()) => {
                        self.notify_write();
                        Ok(ToolResultBlock::text(format!("skill '{name}' {action}d")))
                    }
                    Err(e) => Ok(ToolResultBlock::error(format!("failed: {e}"))),
                }
            }
            "edit" => {
                let name = required_str(&args, "name")?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| agentscope_core::Error::Tool("missing 'content'".into()))?;
                Ok(self.do_edit(name, content))
            }
            "patch" => {
                let name = required_str(&args, "name")?;
                let old_string = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| agentscope_core::Error::Tool("missing 'old_string'".into()))?;
                let new_string = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| agentscope_core::Error::Tool("missing 'new_string'".into()))?;
                let file_path = args.get("file_path").and_then(|v| v.as_str());
                let replace_all = args
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(self.do_patch(name, old_string, new_string, file_path, replace_all))
            }
            "promote" => {
                let name = required_str(&args, "name")?;
                let reviewer = args
                    .get("reviewer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent")
                    .to_string();
                let out = self.promote(name, &reviewer);
                self.notify_write();
                Ok(ToolResultBlock::text(out))
            }
            "archive" => {
                let name = required_str(&args, "name")?;
                match self.main_repo.delete_skill(name) {
                    Ok(()) => {
                        self.notify_write();
                        Ok(ToolResultBlock::text(format!("archived '{name}'")))
                    }
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

    #[tokio::test]
    async fn edit_replaces_skill_md() {
        let drafts = temp("ed");
        let main = temp("em");
        let tool = SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main, drafts),
        );
        tool.create_draft("demo", "# Demo\n\nold body\n", vec![]).unwrap();
        let args = json!({"action": "edit", "name": "demo", "content": "# Demo\n\nnew body\n"})
            .as_object()
            .unwrap()
            .clone();
        let out = tool.call(args, ToolExecutionContext::default()).await.unwrap();
        assert!(out.text_content().contains("SKILL.md replaced"));
        let skill = tool.drafts_repo.load().into_iter().find(|s| s.name() == "demo").unwrap();
        assert_eq!(skill.resources().get("SKILL.md").unwrap(), "# Demo\n\nnew body\n");
    }

    #[tokio::test]
    async fn patch_applies_fuzzy_and_reports_level() {
        let drafts = temp("pa");
        let main = temp("pm2");
        let tool = SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main, drafts),
        );
        // Trailing-whitespace-normalised match: the old_string carries different
        // trailing spaces than the stored file.
        tool.create_draft("demo", "# Demo\ntarget line   \nend\n", vec![]).unwrap();
        let args = json!({
            "action": "patch",
            "name": "demo",
            "old_string": "target line\n",
            "new_string": "replaced\n"
        })
        .as_object()
        .unwrap()
        .clone();
        let out = tool.call(args, ToolExecutionContext::default()).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Patched SKILL.md"), "got: {text}");
        assert!(text.contains("fuzzy"), "expected fuzzy level suffix, got: {text}");
        let skill = tool.drafts_repo.load().into_iter().find(|s| s.name() == "demo").unwrap();
        assert!(skill.resources().get("SKILL.md").unwrap().contains("replaced"));
    }

    #[tokio::test]
    async fn patch_rejects_non_unique_without_replace_all() {
        let drafts = temp("pn");
        let main = temp("pm3");
        let tool = SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main, drafts),
        );
        tool.create_draft("demo", "dup\ndup\n", vec![]).unwrap();
        let args = json!({
            "action": "patch",
            "name": "demo",
            "old_string": "dup",
            "new_string": "x"
        })
        .as_object()
        .unwrap()
        .clone();
        let out = tool.call(args, ToolExecutionContext::default()).await.unwrap();
        assert!(out.text_content().contains("not unique"), "got: {}", out.text_content());
    }

    #[tokio::test]
    async fn on_write_callback_fires_on_mutation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let drafts = temp("owd");
        let main = temp("owm");
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&counter);
        let cb: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        });
        let tool = SkillManageTool::new(
            Arc::new(WorkspaceSkillRepository::new_writable(&main, "main")),
            Arc::new(WorkspaceSkillRepository::new_writable(&drafts, "drafts")),
            SkillManageConfig::new(main, drafts),
        )
        .with_on_write(cb);
        let args = json!({"action": "create", "name": "demo", "skill_md": "# D\n"})
            .as_object()
            .unwrap()
            .clone();
        tool.call(args, ToolExecutionContext::default()).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
