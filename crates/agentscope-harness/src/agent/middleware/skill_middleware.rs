//! Skill middlewares, mirroring `HarnessSkillMiddleware`, `SkillUsageMiddleware`
//! and `SkillCuratorMiddleware`.
//!
//! `HarnessSkillMiddleware` injects the `<available_skills>` block into the system
//! prompt (rebuilt per call from the live repositories), `SkillUsageMiddleware`
//! records skill activity, and `SkillCuratorMiddleware` periodically runs the
//! background [`SkillCurator`].
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::StreamExt;

use crate::agent::skill::curator::{SkillCurator, SkillUsageStore};
use crate::agent::skill::model::{HarnessSkillEntry, SkillCatalog, SkillFilter};
use crate::agent::skill::promoter::SkillVisibilityFilter;
use crate::agent::skill::prompt::SkillPromptBuilder;
use crate::agent::skill::repository::SkillRepository;
use crate::agent::skill::shell_path::ShellPathPolicy;
use crate::agent::skill::stager::{self, RepoBound, SkillRepo};

/// Builds the visible skill entries from an ordered repository list, applying the
/// base [`SkillFilter`] and optional [`SkillVisibilityFilter`]. Higher-priority
/// repositories (later in the list) override earlier ones by `skillId`.
pub fn build_skill_entries(
    repos: &[Arc<dyn SkillRepository>],
    filter: &SkillFilter,
    visibility: Option<&Arc<dyn SkillVisibilityFilter>>,
) -> Vec<HarnessSkillEntry> {
    let mut by_id: HashMap<String, HarnessSkillEntry> = HashMap::new();
    for repo in repos {
        for skill in repo.load() {
            let name = skill.name().to_string();
            if !filter.is_allowed(&name) {
                continue;
            }
            if let Some(vis) = visibility {
                let candidate = crate::agent::skill::curator::build_candidate(
                    &name,
                    &skill.description(),
                    skill
                        .resources()
                        .get("SKILL.md")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    skill.resources(),
                    5,
                );
                if !vis.is_visible(&candidate) {
                    continue;
                }
            }
            let id = skill.skill_id();
            by_id.insert(id, HarnessSkillEntry::new(skill));
        }
    }
    let mut entries: Vec<HarnessSkillEntry> = by_id.into_values().collect();
    entries.sort_by(|a, b| a.skill().skill_id().cmp(&b.skill().skill_id()));
    entries
}

/// Middleware injecting the available-skills prompt block, mirroring
/// `HarnessSkillMiddleware` (and its `frozen` variant).
pub struct HarnessSkillMiddleware {
    repos: Vec<Arc<dyn SkillRepository>>,
    filter: SkillFilter,
    visibility: Option<Arc<dyn SkillVisibilityFilter>>,
    shell_policy: ShellPathPolicy,
    workspace_root: Option<PathBuf>,
    frozen: bool,
    /// 惰性缓存：首次访问时才构建目录/索引，避免启动即全量读盘（嵌入式优化）。
    cache: std::sync::Mutex<SkillCache>,
}

/// 惰性构建的技能目录与索引缓存。
#[derive(Default)]
struct SkillCache {
    catalog: Option<SkillCatalog>,
    index: Option<HashMap<String, PathBuf>>,
}

impl HarnessSkillMiddleware {
    pub fn new(
        repos: Vec<Arc<dyn SkillRepository>>,
        filter: SkillFilter,
        visibility: Option<Arc<dyn SkillVisibilityFilter>>,
        shell_policy: ShellPathPolicy,
        workspace_root: Option<PathBuf>,
    ) -> Self {
        Self {
            repos,
            filter,
            visibility,
            shell_policy,
            workspace_root,
            frozen: false,
            cache: std::sync::Mutex::new(SkillCache::default()),
        }
    }

    /// Frozen variant (no dynamic skill promotion); mirrors `HarnessSkillMiddleware.frozen`.
    pub fn frozen(
        repos: Vec<Arc<dyn SkillRepository>>,
        filter: SkillFilter,
        visibility: Option<Arc<dyn SkillVisibilityFilter>>,
        shell_policy: ShellPathPolicy,
        workspace_root: Option<PathBuf>,
    ) -> Self {
        Self {
            repos,
            filter,
            visibility,
            shell_policy,
            workspace_root,
            frozen: true,
            cache: std::sync::Mutex::new(SkillCache::default()),
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Returns the current catalog (used by the builder to wire the load tool).
    pub fn catalog(&self) -> SkillCatalog {
        SkillCatalog::of(build_skill_entries(
            &self.repos,
            &self.filter,
            self.visibility.as_ref(),
        ))
    }

    /// Builds the `skillId -> on-disk dir` index for `LoadSkillTool`.
    /// 惰性构建：首次调用时全量扫描并缓存，后续直接返回缓存。
    pub fn load_index(&self) -> HashMap<String, PathBuf> {
        let mut guard = self.cache.lock().unwrap();
        if let Some(index) = &guard.index {
            return index.clone();
        }
        let mut index = HashMap::new();
        for repo in &self.repos {
            for skill in repo.load() {
                let id = skill.skill_id();
                if let Some(dir) = repo.skill_dir(&skill.name()) {
                    index.insert(id, dir);
                }
            }
        }
        guard.index = Some(index.clone());
        index
    }

    /// 惰性构建并缓存渲染目录（首次 on_system_prompt 时才扫描磁盘）。
    pub fn cached_catalog(&self) -> SkillCatalog {
        let mut guard = self.cache.lock().unwrap();
        if let Some(catalog) = &guard.catalog {
            return catalog.clone();
        }
        let catalog = self.build_rendered_catalog();
        guard.catalog = Some(catalog.clone());
        catalog
    }

    /// 使缓存失效（技能写盘/删除后调用），下次访问时重新扫描。
    pub fn invalidate(&self) {
        let mut guard = self.cache.lock().unwrap();
        guard.catalog = None;
        guard.index = None;
    }

    /// Builds the catalog with shell-reachable `filesRoot` resolved per skill.
    fn build_rendered_catalog(&self) -> SkillCatalog {
        let entries = build_skill_entries(&self.repos, &self.filter, self.visibility.as_ref());
        if entries.is_empty() {
            return SkillCatalog::of(entries);
        }
        let bound: Vec<RepoBound> = entries
            .iter()
            .map(|e| {
                let repo = if e.skill().source().is_empty() {
                    SkillRepo::Workspace
                } else {
                    SkillRepo::External {
                        source: e.skill().source().to_string(),
                    }
                };
                RepoBound::new(e.skill().clone(), repo)
            })
            .collect();
        let kinds: Vec<SkillRepo> = bound.iter().map(|b| b.repo.clone()).collect();
        let ns = stager::resolve_source_namespaces(&kinds);
        let stage_map =
            stager::stage(self.workspace_root.as_deref(), &bound, &ns).unwrap_or_default();

        let mut out = Vec::new();
        for e in &entries {
            let name = e.skill().name().to_string();
            let stage = stage_map.get(&name).cloned();
            let mut entry = e.clone();
            if let Some(root) = self.shell_policy.resolve(&name, stage.as_ref()) {
                entry = entry.with_files_root(root);
            }
            out.push(entry);
        }
        SkillCatalog::of(out)
    }
}

#[async_trait::async_trait]
impl Middleware for HarnessSkillMiddleware {
    async fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
        if self.repos.is_empty() {
            return current_prompt;
        }
        let catalog = self.cached_catalog();
        if catalog.is_empty() {
            return current_prompt;
        }
        let block = SkillPromptBuilder::new().render(&catalog, &self.filter);
        if block.is_empty() {
            current_prompt
        } else {
            format!("{current_prompt}\n\n{block}")
        }
    }
}

/// Middleware tracking skill usage, mirroring `SkillUsageMiddleware`.
pub struct SkillUsageMiddleware {
    usage_store: Arc<Mutex<dyn SkillUsageStore>>,
    skill_names: Vec<String>,
}

impl SkillUsageMiddleware {
    pub fn new(usage_store: Arc<Mutex<dyn SkillUsageStore>>, skill_names: Vec<String>) -> Self {
        Self {
            usage_store,
            skill_names,
        }
    }
}

impl Middleware for SkillUsageMiddleware {
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let names = self.skill_names.clone();
        let store = Arc::clone(&self.usage_store);
        let fut = async move {
            let text: String = input
                .messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            let mut guard = store.lock().unwrap();
            for n in &names {
                if text.contains(n) {
                    guard.touch_latest_activity(n);
                }
            }
            drop(guard);
            next(input)
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Middleware periodically running the background curator, mirroring
/// `SkillCuratorMiddleware`.
pub struct SkillCuratorMiddleware {
    curator: Arc<SkillCurator>,
}

impl SkillCuratorMiddleware {
    pub fn new(curator: Arc<SkillCurator>) -> Self {
        Self { curator }
    }

    pub fn curator(&self) -> &Arc<SkillCurator> {
        &self.curator
    }
}

impl Middleware for SkillCuratorMiddleware {
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let curator = Arc::clone(&self.curator);
        let fut = async move {
            if curator.should_run_now(chrono::Utc::now()).await {
                let _ = curator.run_once(chrono::Utc::now()).await;
            }
            next(input)
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::repository::FileSystemSkillRepository;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("asr_mw_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    /// 回归：load_index 必须把每个技能解析到磁盘目录（load_skill_through_path 的索引源）。
    /// 此前 FileSystemSkillRepository 未转发 skill_dir，导致索引为空、工具报 Unknown skillId。
    #[test]
    fn load_index_resolves_dirs_from_repos() {
        let root = temp_dir("index");
        write_skill(&root, "data", "# Data skill\n");
        let repo: Arc<dyn SkillRepository> =
            Arc::new(FileSystemSkillRepository::new(&root, "user"));
        let mw = HarnessSkillMiddleware::new(
            vec![repo],
            SkillFilter::all(),
            None,
            ShellPathPolicy::no_shell(),
            Some(root.clone()),
        );
        let index = mw.load_index();
        assert_eq!(index.len(), 1);
        let dir = index.get("data_user").expect("skill_dir must be indexed");
        assert!(dir.join("SKILL.md").is_file());
    }
}
