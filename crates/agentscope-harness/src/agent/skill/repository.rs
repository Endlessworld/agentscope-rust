//! Skill repositories, mirroring `AgentSkillRepository` / `WorkspaceSkillRepository`
//! / `FileSystemSkillRepository` and `HarnessAgentBuilderSupport#composeSkillRepositories`.
//!
//! A repository is a synchronous, directory-backed source of [`AgentSkill`]s. The
//! harness composes an ordered list (low-to-high priority) and, when the skill
//! self-learning loop is enabled, swaps the read-only workspace layer for a writable
//! one so `skill_manage` can persist drafts and promoted skills.
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;

use super::model::AgentSkill;
use super::stager::SkillRepo;

/// A source of skills, mirroring `io.agentscope.core.skill.repository.AgentSkillRepository`.
pub trait SkillRepository: Send + Sync {
    /// Stable identifier for this repository (mirrors `getRepositoryId()`).
    fn repo_id(&self) -> String;

    /// The repository's skill `source` string (`""` for the workspace namespace).
    fn source(&self) -> String;

    /// Whether agent-created skills can be written back through this repository.
    fn is_writeable(&self) -> bool {
        false
    }

    /// The repository classification used by the [`super::stager`].
    fn repo_kind(&self) -> SkillRepo;

    /// Loads every skill currently available from this repository.
    fn load(&self) -> Vec<AgentSkill>;

    /// Persists a skill (only meaningful for writable repositories).
    fn write_skill(&self, skill: &AgentSkill) -> std::io::Result<()> {
        let _ = skill;
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "repository is read-only",
        ))
    }

    /// Deletes a skill by name (only meaningful for writable repositories).
    fn delete_skill(&self, name: &str) -> std::io::Result<()> {
        let _ = name;
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "repository is read-only",
        ))
    }

    /// Returns the on-disk directory for a skill managed by this repository, or
    /// `None` when the repository has no stable directory (used by the
    /// `load_skill_through_path` tool to resolve file reads).
    fn skill_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        let _ = name;
        None
    }
}

/// Directory-backed repository shared implementation.
struct DirSkillRepository {
    root: PathBuf,
    source: String,
    writeable: bool,
    kind: SkillRepo,
    id: String,
}

impl DirSkillRepository {
    fn new(root: PathBuf, source: String, writeable: bool, kind: SkillRepo, id: String) -> Self {
        Self { root, source, writeable, kind, id }
    }
}

impl SkillRepository for DirSkillRepository {
    fn repo_id(&self) -> String {
        self.id.clone()
    }
    fn source(&self) -> String {
        self.source.clone()
    }
    fn is_writeable(&self) -> bool {
        self.writeable
    }
    fn repo_kind(&self) -> SkillRepo {
        self.kind.clone()
    }
    fn load(&self) -> Vec<AgentSkill> {
        load_skills_from_dir(&self.root, &self.source)
    }
    fn write_skill(&self, skill: &AgentSkill) -> std::io::Result<()> {
        if !self.writeable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "repository is read-only",
            ));
        }
        write_skill_to_dir(&self.root, skill)
    }
    fn delete_skill(&self, name: &str) -> std::io::Result<()> {
        if !self.writeable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "repository is read-only",
            ));
        }
        let dir = self.root.join(name);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)
        } else {
            Ok(())
        }
    }
    fn skill_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        Some(self.root.join(name))
    }
}

/// Reads every skill subdirectory under `root` that contains a `SKILL.md`.
pub fn load_skills_from_dir(root: &Path, source: &str) -> Vec<AgentSkill> {
    let mut skills = Vec::new();
    if !root.is_dir() {
        return skills;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return skills;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "_drafts" {
            continue;
        }
        if !dir.join("SKILL.md").is_file() {
            continue;
        }
        let resources = collect_resources(&dir);
        let description = skill_description(&resources).unwrap_or_else(|| format!("Skill: {name}"));
        let mut builder = AgentSkill::builder().name(&name).source(source).description(description);
        for (rel, content) in &resources {
            builder = builder.add_resource(rel.clone(), content.clone());
        }
        skills.push(builder.build());
    }
    skills.sort_by(|a, b| a.name().cmp(&b.name()));
    skills
}

fn collect_resources(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let abs = dir.join(&rel);
        let Ok(entries) = std::fs::read_dir(&abs) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_rel = rel.join(entry.file_name());
            let entry_abs = dir.join(&entry_rel);
            if entry_abs.is_dir() {
                stack.push(entry_rel);
            } else if entry_abs.is_file() {
                if let Ok(content) = std::fs::read_to_string(&entry_abs) {
                    let rel_str = entry_rel.to_string_lossy().replace('\\', "/");
                    out.insert(rel_str, content);
                } else if let Ok(bytes) = std::fs::read(&entry_abs) {
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    let rel_str = entry_rel.to_string_lossy().replace('\\', "/");
                    out.insert(rel_str, format!("base64:{b64}"));
                }
            }
        }
    }
    out
}

fn skill_description(resources: &BTreeMap<String, String>) -> Option<String> {
    for (rel, content) in resources {
        if rel.ends_with("SKILL.md") {
            for line in content.lines() {
                let l = line.trim();
                if !l.is_empty() {
                    return Some(l.trim_start_matches('#').trim().to_string());
                }
            }
        }
    }
    None
}

fn write_skill_to_dir(root: &Path, skill: &AgentSkill) -> std::io::Result<()> {
    let dir = root.join(skill.name());
    std::fs::create_dir_all(&dir)?;
    for (rel, content) in skill.resources() {
        if rel.trim().is_empty() || rel.starts_with('/') || rel.contains("..") {
            continue;
        }
        let target = dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = if let Some(rest) = content.strip_prefix("base64:") {
            base64::engine::general_purpose::STANDARD
                .decode(rest)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        } else {
            content.as_bytes().to_vec()
        };
        let mut f = std::fs::File::create(&target)?;
        f.write_all(&bytes)?;
    }
    Ok(())
}

/// A read-only project-global / marketplace repository, mirroring
/// `FileSystemSkillRepository` (Layer 1 / Layer 2 / Layer 3).
#[derive(Clone)]
pub struct FileSystemSkillRepository {
    inner: Arc<DirSkillRepository>,
}

impl FileSystemSkillRepository {
    pub fn new(root: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        let root = root.into();
        let source = source.into();
        let id = if source.is_empty() {
            "fs".to_string()
        } else {
            format!("fs:{source}")
        };
        let kind_source = source.clone();
        Self {
            inner: Arc::new(DirSkillRepository::new(
                root,
                source,
                false,
                SkillRepo::External { source: kind_source },
                id,
            )),
        }
    }
}

impl SkillRepository for FileSystemSkillRepository {
    fn repo_id(&self) -> String {
        self.inner.repo_id()
    }
    fn source(&self) -> String {
        self.inner.source()
    }
    fn is_writeable(&self) -> bool {
        self.inner.is_writeable()
    }
    fn repo_kind(&self) -> SkillRepo {
        self.inner.repo_kind()
    }
    fn load(&self) -> Vec<AgentSkill> {
        self.inner.load()
    }
    fn write_skill(&self, skill: &AgentSkill) -> std::io::Result<()> {
        self.inner.write_skill(skill)
    }
    fn delete_skill(&self, name: &str) -> std::io::Result<()> {
        self.inner.delete_skill(name)
    }
}

/// Workspace namespaced repository, mirroring `WorkspaceSkillRepository`.
///
/// The default layer-4 instance is read-only; when the skill self-learning loop is
/// enabled the harness swaps it for a writable instance pointed at the configured
/// main/drafts directories.
#[derive(Clone)]
pub struct WorkspaceSkillRepository {
    inner: Arc<DirSkillRepository>,
}

impl WorkspaceSkillRepository {
    /// Read-only workspace repository over the given skills directory (Layer 4 default).
    pub fn new_read_only(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            inner: Arc::new(DirSkillRepository::new(
                root,
                String::new(),
                false,
                SkillRepo::Workspace,
                "workspace-namespaced".to_string(),
            )),
        }
    }

    /// Writable workspace repository (used by `skill_manage` / `propose_skill`).
    pub fn new_writable(root: impl Into<PathBuf>, id: &str) -> Self {
        let root = root.into();
        Self {
            inner: Arc::new(DirSkillRepository::new(
                root,
                String::new(),
                true,
                SkillRepo::Workspace,
                id.to_string(),
            )),
        }
    }
}

impl SkillRepository for WorkspaceSkillRepository {
    fn repo_id(&self) -> String {
        self.inner.repo_id()
    }
    fn source(&self) -> String {
        self.inner.source()
    }
    fn is_writeable(&self) -> bool {
        self.inner.is_writeable()
    }
    fn repo_kind(&self) -> SkillRepo {
        self.inner.repo_kind()
    }
    fn load(&self) -> Vec<AgentSkill> {
        self.inner.load()
    }
    fn write_skill(&self, skill: &AgentSkill) -> std::io::Result<()> {
        self.inner.write_skill(skill)
    }
    fn delete_skill(&self, name: &str) -> std::io::Result<()> {
        self.inner.delete_skill(name)
    }
}

/// Composes the ordered list of skill repositories (low-to-high priority), mirroring
/// `HarnessAgentBuilderSupport#composeSkillRepositories`.
///
/// * Layer 1: project-global skills directory (if configured and present).
/// * Layer 2: user-supplied marketplace repositories.
/// * Layer 3: workspace agent-shared skills directory (if present).
/// * Layer 4: per-user workspace-namespaced repository (read-only unless opted out).
pub fn compose_skill_repositories(
    project_global_skills_dir: Option<PathBuf>,
    user_repositories: &[Arc<dyn SkillRepository>],
    workspace_skills_dir: Option<PathBuf>,
    workspace_namespace_dir: PathBuf,
    disable_default_workspace_skills: bool,
) -> Vec<Arc<dyn SkillRepository>> {
    let mut ordered: Vec<Arc<dyn SkillRepository>> = Vec::new();

    if let Some(dir) = project_global_skills_dir {
        if dir.is_dir() {
            ordered.push(Arc::new(FileSystemSkillRepository::new(dir, "project-global")));
        }
    }

    for repo in user_repositories {
        ordered.push(Arc::clone(repo));
    }

    if let Some(dir) = workspace_skills_dir {
        if dir.is_dir() {
            ordered.push(Arc::new(FileSystemSkillRepository::new(dir, "workspace-shared")));
        }
    }

    if !disable_default_workspace_skills {
        ordered.push(Arc::new(WorkspaceSkillRepository::new_read_only(workspace_namespace_dir)));
    }

    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("asr_repo_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn load_reads_subdirs_with_skill_md() {
        let root = temp_dir("load");
        write_skill(&root, "alpha", "# Alpha skill\n");
        write_skill(&root, "beta", "# Beta skill\n");
        let repo = FileSystemSkillRepository::new(&root, "market");
        let skills = repo.load();
        assert_eq!(skills.len(), 2);
        assert!(skills.iter().all(|s| s.source() == "market"));
        assert!(skills.iter().any(|s| s.name() == "alpha" && s.description().starts_with("Alpha")));
    }

    #[test]
    fn read_only_repo_rejects_write() {
        let root = temp_dir("ro");
        let repo = WorkspaceSkillRepository::new_read_only(&root);
        assert!(!repo.is_writeable());
        let skill = AgentSkill::builder().name("x").description("d").build();
        assert!(repo.write_skill(&skill).is_err());
    }

    #[test]
    fn writable_repo_persists_and_reloads() {
        let root = temp_dir("wo");
        let repo = WorkspaceSkillRepository::new_writable(&root, "ws-writable");
        assert!(repo.is_writeable());
        let skill = AgentSkill::builder()
            .name("tool")
            .description("d")
            .add_resource("SKILL.md".to_string(), "# Tool\n".to_string())
            .build();
        repo.write_skill(&skill).unwrap();
        let reloaded = repo.load();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name(), "tool");
    }
}
