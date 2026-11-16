//! Skill repositories, mirroring `AgentSkillRepository` / `WorkspaceSkillRepository`
//! / `FileSystemSkillRepository` and `HarnessAgentBuilderSupport#composeSkillRepositories`.
//!
//! A repository is a synchronous, directory-backed source of [`AgentSkill`]s. The
//! harness composes an ordered list (low-to-high priority) and, when the skill
//! self-learning loop is enabled, swaps the read-only workspace layer for a writable
//! one so `skill_manage` can persist drafts and promoted skills.
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
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

    /// Cheap change signal for [`Self::load`]: `Some` when the repository can summarise its
    /// current state (entry count + newest top-level modification time), `None` when it cannot.
    ///
    /// The skill middleware compares this stamp before reusing its lazily-built catalog, so a
    /// `SKILL.md` written by another process (not through `skill_manage`) becomes visible
    /// without a full re-scan on every call (#2419 / #2420).
    fn freshness(&self) -> Option<u64> {
        None
    }

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
        Self {
            root,
            source,
            writeable,
            kind,
            id,
        }
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
    fn freshness(&self) -> Option<u64> {
        dir_freshness(&self.root)
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
        // Single-skill layout: the root *is* the skill directory, not its parent.
        if self.root.join("SKILL.md").is_file()
            && self
                .root
                .file_name()
                .is_some_and(|n| n.to_string_lossy().as_ref() == name)
        {
            return Some(self.root.clone());
        }
        Some(self.root.join(name))
    }
}

/// Summarises a skills directory without reading any file: entry count plus the newest
/// top-level modification time. `None` when the directory cannot be listed.
fn dir_freshness(root: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut count: u64 = 0;
    let mut newest: u128 = 0;
    for entry in entries.flatten() {
        count += 1;
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let since = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok());
        if let Some(since) = since {
            newest = newest.max(since.as_nanos());
        }
    }
    let mut hasher = DefaultHasher::new();
    count.hash(&mut hasher);
    newest.hash(&mut hasher);
    Some(hasher.finish())
}

/// Reads every skill subdirectory under `root` that contains a `SKILL.md`.
///
/// A root that holds `SKILL.md` itself is a *single-skill* layout (`<root>/SKILL.md`): it is
/// its own skill directory, so it loads as exactly one skill instead of an empty catalog (#1790).
pub fn load_skills_from_dir(root: &Path, source: &str) -> Vec<AgentSkill> {
    let mut skills = Vec::new();
    if !root.is_dir() {
        return skills;
    }
    if root.join("SKILL.md").is_file() {
        if let Some(skill) = load_skill_from_dir(root, source) {
            skills.push(skill);
            return skills;
        }
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
        if let Some(skill) = load_skill_from_dir(&dir, source) {
            skills.push(skill);
        }
    }
    skills.sort_by(|a, b| a.name().cmp(&b.name()));
    skills
}

/// Loads a single skill directory (its `SKILL.md` + resources), without scanning
/// siblings.
///
/// The skill name comes from the directory name, exactly as
/// [`load_skills_from_dir`] derives it. Returns `None` when the directory holds no
/// readable `SKILL.md`, which is the same skip semantics the directory scan applies —
/// a repository that already resolved one skill directory can load just that skill
/// instead of copying the resource scan or re-scanning the whole root.
pub fn load_skill_from_dir(dir: &Path, source: &str) -> Option<AgentSkill> {
    if !dir.is_dir() {
        return None;
    }
    let name = dir.file_name()?.to_string_lossy().to_string();
    let resources = collect_resources(dir);
    if !resources.keys().any(|rel| rel.ends_with("SKILL.md")) {
        return None;
    }
    let description = skill_description(&resources).unwrap_or_else(|| format!("Skill: {name}"));
    let mut builder = AgentSkill::builder()
        .name(&name)
        .source(source)
        .description(description);
    for (rel, content) in &resources {
        builder = builder.add_resource(rel.clone(), content.clone());
    }
    Some(builder.build())
}

/// Cap on the total resources collected for one skill (16 MiB), bounding the in-memory
/// `base64:` expansion of a binary-heavy skill directory (#1906).
///
/// A single large resource is limited only by this total: a per-file cap made a skill whose
/// largest asset exceeded it silently lose that asset, so `get_skill` no longer returned the
/// resources the directory actually holds.
const MAX_RESOURCE_TOTAL_BYTES: u64 = 16 << 20;

fn collect_resources(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    // Canonicalised directories already walked: a symlink loop (or a `..` alias) must not
    // keep the stack growing forever.
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut total: u64 = 0;
    while let Some(rel) = stack.pop() {
        let abs = dir.join(&rel);
        let canonical = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&abs) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // `.git` and any other hidden entry: never part of a skill's published resources.
            if name_str.starts_with('.') {
                continue;
            }
            let entry_rel = rel.join(&name);
            let entry_abs = dir.join(&entry_rel);
            let Ok(meta) = entry_abs.symlink_metadata() else {
                continue;
            };
            let file_type = meta.file_type();
            // Symlinks are skipped entirely: following one can leave the skill directory.
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry_rel);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if total.saturating_add(meta.len()) > MAX_RESOURCE_TOTAL_BYTES {
                continue;
            }
            let rel_str = entry_rel.to_string_lossy().replace('\\', "/");
            let Ok(bytes) = std::fs::read(&entry_abs) else {
                continue;
            };
            total = total.saturating_add(bytes.len() as u64);
            match String::from_utf8(bytes) {
                Ok(content) => {
                    out.insert(rel_str, content);
                }
                Err(e) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(e.as_bytes());
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
                SkillRepo::External {
                    source: kind_source,
                },
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
    fn freshness(&self) -> Option<u64> {
        self.inner.freshness()
    }
    fn skill_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        self.inner.skill_dir(name)
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
    fn freshness(&self) -> Option<u64> {
        self.inner.freshness()
    }
    fn skill_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        self.inner.skill_dir(name)
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
            ordered.push(Arc::new(FileSystemSkillRepository::new(
                dir,
                "project-global",
            )));
        }
    }

    for repo in user_repositories {
        ordered.push(Arc::clone(repo));
    }

    if let Some(dir) = workspace_skills_dir {
        if dir.is_dir() {
            ordered.push(Arc::new(FileSystemSkillRepository::new(
                dir,
                "workspace-shared",
            )));
        }
    }

    if !disable_default_workspace_skills {
        ordered.push(Arc::new(WorkspaceSkillRepository::new_read_only(
            workspace_namespace_dir,
        )));
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
        assert!(skills
            .iter()
            .any(|s| s.name() == "alpha" && s.description().starts_with("Alpha")));
    }

    #[test]
    fn load_skill_from_dir_reads_only_that_directory() {
        let root = temp_dir("single");
        write_skill(&root, "alpha", "# Alpha skill\n");
        write_skill(&root, "beta", "# Beta skill\n");
        std::fs::write(root.join("alpha").join("notes.md"), "alpha notes").unwrap();
        std::fs::write(root.join("beta").join("notes.md"), "beta notes").unwrap();

        let skill = load_skill_from_dir(&root.join("alpha"), "market").expect("alpha loads");
        assert_eq!(skill.name(), "alpha");
        assert_eq!(skill.source(), "market");
        let resources: Vec<&String> = skill.resources().keys().collect();
        assert!(resources.iter().any(|r| r.ends_with("SKILL.md")));
        assert!(resources.iter().any(|r| r.as_str() == "notes.md"));
        assert_eq!(
            skill.resources().get("notes.md").map(String::as_str),
            Some("alpha notes"),
            "only the target directory's resources are read"
        );

        // A directory without a readable `SKILL.md` is skipped, like the scan does.
        let empty = temp_dir("single_empty");
        std::fs::create_dir_all(empty.join("gamma")).unwrap();
        std::fs::write(empty.join("gamma").join("notes.md"), "no skill").unwrap();
        assert!(load_skill_from_dir(&empty.join("gamma"), "market").is_none());
        assert!(load_skill_from_dir(&empty.join("missing"), "market").is_none());

        // The directory scan still returns both skills.
        assert_eq!(load_skills_from_dir(&root, "market").len(), 2);
    }

    #[test]
    fn file_repo_skill_dir_resolves_subdir() {
        let root = temp_dir("filedir");
        write_skill(&root, "alpha", "# Alpha skill\n");
        let repo = FileSystemSkillRepository::new(&root, "user");
        assert_eq!(repo.skill_dir("alpha"), Some(root.join("alpha")));
        // 目录解析不分存在性：load_index 依赖它映射到磁盘路径，由加载工具负责校验。
        assert_eq!(repo.skill_dir("missing"), Some(root.join("missing")));
    }

    #[test]
    fn workspace_repo_skill_dir_resolves_subdir() {
        let root = temp_dir("wsdir");
        write_skill(&root, "beta", "# Beta skill\n");
        let repo = WorkspaceSkillRepository::new_read_only(&root);
        assert_eq!(repo.skill_dir("beta"), Some(root.join("beta")));
    }

    #[test]
    fn a_root_that_is_itself_a_skill_loads_as_one_skill() {
        // Single-skill layout: `<root>/SKILL.md` with no `skills/` layer (#1790).
        let root = temp_dir("single_root");
        std::fs::write(root.join("SKILL.md"), "# Lone skill\n").unwrap();
        std::fs::write(root.join("notes.md"), "notes").unwrap();

        let repo = FileSystemSkillRepository::new(&root, "market");
        let skills = repo.load();
        assert_eq!(
            skills.len(),
            1,
            "a single-skill root loads exactly one skill"
        );
        let name = root.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(skills[0].name(), name);
        assert!(skills[0].description().starts_with("Lone skill"));
        assert_eq!(
            skills[0].resources().get("notes.md").map(String::as_str),
            Some("notes")
        );
        // The load tool's index must point at the root itself, not `<root>/<root-name>`.
        assert_eq!(repo.skill_dir(&name), Some(root.clone()));

        // A root holding no `SKILL.md` still enumerates subdirectories as before.
        let parent = temp_dir("single_root_parent");
        write_skill(&parent, "alpha", "# Alpha skill\n");
        assert_eq!(FileSystemSkillRepository::new(&parent, "m").load().len(), 1);
    }

    #[test]
    fn collect_resources_skips_hidden_oversized_and_looping_entries() {
        let root = temp_dir("resources");
        write_skill(&root, "alpha", "# Alpha skill\n");
        let skill = root.join("alpha");
        std::fs::create_dir_all(skill.join(".git")).unwrap();
        std::fs::write(skill.join(".git").join("config"), "secret").unwrap();
        std::fs::write(skill.join(".hidden"), "hidden").unwrap();
        std::fs::write(
            skill.join("big.bin"),
            vec![b'x'; (MAX_RESOURCE_TOTAL_BYTES + 1) as usize],
        )
        .unwrap();
        // A self-referential link must be skipped, not followed (creation may need privileges
        // on Windows; the assertion below only cares that nothing loops or panics).
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&skill, skill.join("loop"));
        }
        #[cfg(windows)]
        {
            let _ = std::os::windows::fs::symlink_dir(&skill, skill.join("loop"));
        }

        let loaded = load_skill_from_dir(&skill, "market").expect("alpha loads");
        let keys: Vec<&String> = loaded.resources().keys().collect();
        assert!(keys.iter().any(|k| k.as_str() == "SKILL.md"));
        assert!(!keys.iter().any(|k| k.contains(".git")), "{keys:?}");
        assert!(!keys.iter().any(|k| k.as_str() == ".hidden"), "{keys:?}");
        assert!(!keys.iter().any(|k| k.as_str() == "big.bin"), "{keys:?}");
        assert!(keys.len() <= 2, "resource set stays bounded: {keys:?}");
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
