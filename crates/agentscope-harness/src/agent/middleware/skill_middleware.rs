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
    names: Option<HashMap<String, String>>,
    /// 构建时各仓库的轻量新鲜度指纹（目录条目数 + 顶层 mtime）；变化则整份缓存失效。
    freshness: Option<u64>,
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
        self.revalidate_locked(&mut guard);
        if let Some(index) = &guard.index {
            return index.clone();
        }
        self.scan_index(&mut guard);
        guard.index.clone().unwrap_or_default()
    }

    /// The `skillId -> skill name` map, so the load tool can apply name-based grants
    /// (`CapabilitySurface` speaks skill *names*, one entry covering every copy).
    pub fn load_name_index(&self) -> HashMap<String, String> {
        let mut guard = self.cache.lock().unwrap();
        self.revalidate_locked(&mut guard);
        if guard.names.is_none() {
            self.scan_index(&mut guard);
        }
        guard.names.clone().unwrap_or_default()
    }

    /// Combined cheap freshness stamp over every repository, or `None` when no repository can
    /// summarise itself (caches then never auto-invalidate, as before).
    fn repo_freshness(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut any = false;
        for repo in &self.repos {
            if let Some(stamp) = repo.freshness() {
                any = true;
                repo.repo_id().hash(&mut hasher);
                stamp.hash(&mut hasher);
            }
        }
        any.then(|| hasher.finish())
    }

    /// Drops the lazy cache when the repositories' freshness stamp changed.
    ///
    /// `invalidate()` alone only fires for writes made through `skill_manage`; a `SKILL.md`
    /// created or edited by another process would otherwise stay invisible for the whole agent
    /// lifetime (#2419 / #2420). The stamp is one `read_dir` per repository, never a re-read.
    fn revalidate_locked(&self, guard: &mut SkillCache) {
        let stamp = self.repo_freshness();
        if stamp.is_some() && guard.freshness.is_some() && guard.freshness != stamp {
            guard.catalog = None;
            guard.index = None;
            guard.names = None;
        }
        guard.freshness = stamp;
    }

    /// Scans every repository once and caches both the on-disk index and the name map.
    fn scan_index(&self, guard: &mut SkillCache) {
        let mut index = HashMap::new();
        let mut names = HashMap::new();
        for repo in &self.repos {
            for skill in repo.load() {
                let id = skill.skill_id();
                names.insert(id.clone(), skill.name().to_string());
                if let Some(dir) = repo.skill_dir(&skill.name()) {
                    index.insert(id, dir);
                }
            }
        }
        guard.index = Some(index);
        guard.names = Some(names);
    }

    /// 惰性构建并缓存渲染目录（首次 on_system_prompt 时才扫描磁盘）。
    pub fn cached_catalog(&self) -> SkillCatalog {
        let mut guard = self.cache.lock().unwrap();
        self.revalidate_locked(&mut guard);
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
        guard.names = None;
        guard.freshness = None;
    }

    /// Builds the catalog with shell-reachable `filesRoot` resolved per skill.
    fn build_rendered_catalog(&self) -> SkillCatalog {
        let entries = build_skill_entries(&self.repos, &self.filter, self.visibility.as_ref());
        // No early return on an empty set: staging an empty visible set is what triggers
        // `garbage_collect_orphans`, so `.skills-cache` still drops a skill that was deleted
        // from its repository (#2664).
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
        // `resolve_source_namespaces` numbers *repositories*, so it must see each repository
        // once: handing it one entry per skill gave every skill of one repository its own
        // numbered folder (`user_1`, `user_2`, ...).
        let mut unique: Vec<SkillRepo> = Vec::new();
        for kind in &kinds {
            if !unique.contains(kind) {
                unique.push(kind.clone());
            }
        }
        let repo_ns = stager::resolve_source_namespaces(&unique);
        let ns: Vec<String> = kinds
            .iter()
            .map(|kind| {
                unique
                    .iter()
                    .position(|candidate| candidate == kind)
                    .and_then(|idx| repo_ns.get(idx).cloned())
                    .unwrap_or_default()
            })
            .collect();
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
    async fn on_system_prompt(&self, ctx: &RuntimeContext, current_prompt: String) -> String {
        if self.repos.is_empty() {
            return current_prompt;
        }
        let catalog = self.cached_catalog();
        if catalog.is_empty() {
            return current_prompt;
        }
        // Capability grant (Java `DynamicSkillMiddleware`): the visible catalog is
        // `SkillFilter` ∩ grant. The grant runs after the configuration intent and can only
        // narrow — a runtime overlay may widen the filter, never the authorization surface.
        let catalog = match agentscope_core::agent::capability_surface_from_context(ctx) {
            Some(surface) if !surface.is_unrestricted() => SkillCatalog::of(
                catalog
                    .all()
                    .iter()
                    .filter(|entry| {
                        surface.grants_skill(
                            entry.skill().name(),
                            Some(entry.skill().skill_id().as_str()),
                        )
                    })
                    .cloned()
                    .collect(),
            ),
            _ => catalog,
        };
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

    fn two_skill_middleware(tag: &str) -> (HarnessSkillMiddleware, std::path::PathBuf) {
        let root = temp_dir(tag);
        write_skill(&root, "pdf", "# PDF\n");
        write_skill(&root, "docx", "# DOCX\n");
        let repo: Arc<dyn SkillRepository> =
            Arc::new(FileSystemSkillRepository::new(&root, "user"));
        let mw = HarnessSkillMiddleware::new(
            vec![repo],
            SkillFilter::all(),
            None,
            ShellPathPolicy::no_shell(),
            Some(root.clone()),
        );
        (mw, root)
    }

    fn ctx_with_skills(names: &[&str]) -> RuntimeContext {
        let mut ctx = RuntimeContext::new().with_user_id("u1");
        ctx.extensions.insert_arc(
            agentscope_core::agent::CAPABILITY_SURFACE_CONTEXT_KEY,
            Arc::new(agentscope_core::agent::CapabilitySurface::new(
                Some(names.iter().map(|n| n.to_string()).collect()),
                None,
                None,
            )),
        );
        ctx
    }

    #[test]
    fn an_external_skill_write_invalidates_the_cached_catalog() {
        let (mw, root) = two_skill_middleware("freshness");
        assert_eq!(mw.cached_catalog().all().len(), 2);

        // Another process adds a skill: no `skill_manage` write callback runs here (#2419).
        write_skill(&root, "xlsx", "# XLSX\n");
        let catalog = mw.cached_catalog();
        assert_eq!(
            catalog.all().len(),
            3,
            "a skill added outside skill_manage must become visible"
        );
        assert!(catalog.all().iter().any(|e| e.skill().name() == "xlsx"));

        // Removing one is seen just as promptly.
        std::fs::remove_dir_all(root.join("xlsx")).unwrap();
        assert_eq!(mw.cached_catalog().all().len(), 2);
    }

    #[test]
    fn deleting_a_skill_collects_its_staged_cache_on_both_entry_paths() {
        let (mw, root) = two_skill_middleware("gc");
        assert_eq!(mw.cached_catalog().all().len(), 2);
        let cache = root.join(crate::agent::skill::stager::CACHE_DIR);
        let staged_pdf = cache.join("user").join("pdf");
        assert!(staged_pdf.join("SKILL.md").is_file());

        // Non-empty entry: the remaining skill's pass drops the orphan.
        std::fs::remove_dir_all(root.join("pdf")).unwrap();
        assert_eq!(mw.cached_catalog().all().len(), 1);
        assert!(
            !staged_pdf.exists(),
            "an orphan staged directory must be collected"
        );

        // Empty entry: deleting the last skill still runs orphan GC (#2664).
        std::fs::remove_dir_all(root.join("docx")).unwrap();
        assert!(mw.cached_catalog().is_empty());
        assert!(
            !cache.join("user").exists(),
            "an empty catalog must still clear the staging cache"
        );
    }

    #[tokio::test]
    async fn the_prompt_catalog_is_trimmed_by_the_capability_grant() {
        let (mw, _root) = two_skill_middleware("grant_prompt");
        // Without a surface both skills are offered.
        let prompt = mw
            .on_system_prompt(&RuntimeContext::new(), "base".to_string())
            .await;
        assert!(
            prompt.contains("pdf") && prompt.contains("docx"),
            "{prompt}"
        );

        // A granted name covers the copy from that source; the other skill disappears.
        let prompt = mw
            .on_system_prompt(&ctx_with_skills(&["pdf"]), "base".to_string())
            .await;
        assert!(prompt.contains("pdf"), "{prompt}");
        assert!(
            !prompt.contains("docx"),
            "an ungranted skill must not reach the prompt: {prompt}"
        );

        // An explicit empty grant keeps only the base prompt.
        let prompt = mw
            .on_system_prompt(&ctx_with_skills(&[]), "base".to_string())
            .await;
        assert_eq!(prompt, "base");
    }
}
