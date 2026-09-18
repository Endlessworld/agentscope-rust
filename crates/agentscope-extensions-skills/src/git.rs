//! Git-backed, read-only skill repository.
//!
//! Ports `io.agentscope.core.skill.repository.GitSkillRepository`. The remote repository is
//! cloned into a local directory (a temp dir by default) and skills are read from disk,
//! mirroring `SkillFileSystemHelper.getAllSkills`.
//!
//! Java uses JGit; this port shells out to the system `git` binary. That matches Java's
//! documented authentication model (`~/.gitconfig` / credential helpers for HTTPS, `~/.ssh`
//! for SSH) and avoids pulling the `git2` C dependency (not present in `Cargo.lock`).
//!
//! Each read triggers a lightweight remote-ref check and only pulls when the remote HEAD
//! changed, exactly like the Java `ensureRepositorySynced` / `hasRemoteUpdates` pair.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_harness::agent::skill::model::AgentSkill;
use agentscope_harness::agent::skill::repository::load_skills_from_dir;

use crate::error::SkillRepositoryError;
use crate::repository::{AgentSkillRepository, AgentSkillRepositoryInfo};

/// Configuration for [`GitSkillRepository`].
#[derive(Debug, Clone)]
pub struct GitSkillRepositoryConfig {
    /// Remote repository URL (HTTPS / SSH / `file://`).
    pub remote_url: String,
    /// Branch to clone; `None` uses the remote default branch.
    pub branch: Option<String>,
    /// Local clone directory; `None` creates a temp dir owned by the repository.
    pub local_path: Option<PathBuf>,
    /// Custom source identifier; `None` derives `git-<identifier>[@<branch>]`.
    pub source: Option<String>,
    /// In-repo subdirectory that holds the skills; `None` auto-detects `skills/`.
    pub skills_root: Option<String>,
    /// When `true`, reads synchronize the clone first.
    pub auto_sync: bool,
}

impl GitSkillRepositoryConfig {
    /// Creates a config for a remote URL with default branch / temp clone / auto-sync.
    pub fn new(remote_url: impl Into<String>) -> Self {
        Self {
            remote_url: remote_url.into(),
            branch: None,
            local_path: None,
            source: None,
            skills_root: None,
            auto_sync: true,
        }
    }

    /// Sets the branch to clone.
    pub fn branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }

    /// Sets the local clone directory (disables temp-dir ownership).
    pub fn local_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.local_path = Some(path.into());
        self
    }

    /// Sets the source identifier.
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Sets the in-repo skills subdirectory.
    pub fn skills_root(mut self, root: impl Into<String>) -> Self {
        self.skills_root = Some(root.into());
        self
    }

    /// Toggles auto-sync on reads.
    pub fn auto_sync(mut self, auto: bool) -> Self {
        self.auto_sync = auto;
        self
    }
}

/// Internal mutable sync state, guarded by a mutex (mirrors the Java `synchronized`).
struct SyncState {
    skills_path: Option<PathBuf>,
    last_remote_ref: Option<String>,
}

/// Git-based read-only [`AgentSkillRepository`] implementation.
pub struct GitSkillRepository {
    cfg: GitSkillRepositoryConfig,
    local_path: PathBuf,
    owns_temp_dir: bool,
    state: Mutex<SyncState>,
}

impl GitSkillRepository {
    /// Creates a repository from a remote URL (temp clone, default branch, auto-sync).
    pub fn new(remote_url: impl Into<String>) -> Result<Self, SkillRepositoryError> {
        Self::from_config(GitSkillRepositoryConfig::new(remote_url))
    }

    /// Creates a repository from a full config.
    pub fn from_config(cfg: GitSkillRepositoryConfig) -> Result<Self, SkillRepositoryError> {
        if cfg.remote_url.trim().is_empty() {
            return Err(SkillRepositoryError::invalid(
                "remote URL",
                "cannot be null or empty",
            ));
        }
        let (local_path, owns_temp_dir) = match &cfg.local_path {
            Some(p) => (p.clone(), false),
            None => (create_temp_dir()?, true),
        };
        // Validate skills_root up front (rejects absolute / `..`), mirroring the Java ctor.
        normalize_skills_root(cfg.skills_root.as_deref())?;
        Ok(Self {
            cfg,
            local_path,
            owns_temp_dir,
            state: Mutex::new(SyncState {
                skills_path: None,
                last_remote_ref: None,
            }),
        })
    }

    /// The local clone directory.
    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    /// Extracts a concise `owner/repo` identifier from a git URL, mirroring
    /// `extractRepositoryIdentifier`.
    pub fn extract_repository_identifier(url: &str) -> String {
        let normalized = url.strip_suffix(".git").unwrap_or(url);
        if normalized.contains('@') && normalized.contains(':') {
            if let Some(idx) = normalized.rfind(':') {
                return normalized[idx + 1..].to_string();
            }
        }
        if let Some(scheme) = normalized.find("://") {
            let after = scheme + 3;
            if let Some(host_end) = normalized[after..].find('/').map(|i| after + i) {
                if host_end + 1 < normalized.len() {
                    return normalized[host_end + 1..].to_string();
                }
            }
        }
        match normalized.rfind('/') {
            Some(i) => normalized[i + 1..].to_string(),
            None => normalized.to_string(),
        }
    }

    /// Derives the default source string (`git-<id>[@<branch>]`), sanitised for the filesystem.
    fn build_default_source(&self) -> String {
        let id = Self::extract_repository_identifier(&self.cfg.remote_url);
        let raw = match &self.cfg.branch {
            Some(b) => format!("git-{id}@{b}"),
            None => format!("git-{id}"),
        };
        raw.chars()
            .map(|c| match c {
                '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
                other => other,
            })
            .collect()
    }

    /// Runs `git <args>` in the given working directory, mapping failures to
    /// [`SkillRepositoryError::Git`].
    fn run_git(
        cwd: Option<&Path>,
        args: &[&str],
    ) -> Result<std::process::Output, SkillRepositoryError> {
        let mut cmd = Command::new("git");
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let output = cmd.output().map_err(|e| {
            SkillRepositoryError::Git(format!(
                "failed to run `git {}`: {e}",
                args.join(" ")
            ))
        })?;
        Ok(output)
    }

    /// Clones the remote repository into the local path.
    fn clone_repository(&self) -> Result<(), SkillRepositoryError> {
        tracing::info!(url = %self.cfg.remote_url, path = %self.local_path.display(), "cloning git skill repository");
        if let Some(parent) = self.local_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
        if let Some(branch) = &self.cfg.branch {
            args.push("--branch".into());
            args.push(branch.clone());
        }
        args.push(self.cfg.remote_url.clone());
        args.push(self.local_path.to_string_lossy().to_string());
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Self::run_git(None, &arg_refs)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SkillRepositoryError::Git(format!(
                "clone failed for {}: {}",
                self.cfg.remote_url,
                stderr.trim()
            )));
        }
        if let Ok(head) = Self::rev_parse(&self.local_path, "HEAD") {
            self.state.lock().unwrap().last_remote_ref = Some(head);
        }
        Ok(())
    }

    /// `git rev-parse <rev>` in `dir`.
    fn rev_parse(dir: &Path, rev: &str) -> Result<String, SkillRepositoryError> {
        let output = Self::run_git(Some(dir), &["rev-parse", rev])?;
        if !output.status.success() {
            return Err(SkillRepositoryError::Git(format!(
                "rev-parse {rev} failed"
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Resolves the remote HEAD ref via `git ls-remote`. Returns `None` when the remote is
    /// unreachable, so a transient network failure degrades to "no update".
    fn remote_head(&self) -> Option<String> {
        let output = Self::run_git(None, &["ls-remote", &self.cfg.remote_url, "HEAD"]).ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        text.split_whitespace().next().map(|s| s.to_string())
    }

    /// Whether the remote HEAD differs from the last synced ref.
    fn has_remote_updates(&self) -> bool {
        let last = self.state.lock().unwrap().last_remote_ref.clone();
        match (self.remote_head(), last) {
            (Some(remote), Some(last)) => remote != last,
            (Some(_), None) => true,
            // Unknown remote state -> do not pull (matches the conservative Java path).
            (None, _) => false,
        }
    }

    /// Pulls the latest changes into an existing clone.
    fn pull_repository(&self) -> Result<(), SkillRepositoryError> {
        let output = Self::run_git(Some(&self.local_path), &["pull", "--ff-only"])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SkillRepositoryError::Git(format!(
                "pull failed for {}: {}",
                self.cfg.remote_url,
                stderr.trim()
            )));
        }
        self.state.lock().unwrap().last_remote_ref = self.remote_head();
        Ok(())
    }

    /// Ensures the clone exists (cloning or pulling as needed) and resolves the skills path.
    fn ensure_repository_synced(&self) -> Result<(), SkillRepositoryError> {
        let empty = !self.local_path.exists() || is_empty_dir(&self.local_path);
        if empty {
            // A previously created temp dir must be removed before `git clone`.
            if self.local_path.exists() {
                let _ = std::fs::remove_dir_all(&self.local_path);
            }
            self.clone_repository()?;
        } else if self.local_path.join(".git").exists() {
            if self.has_remote_updates() {
                self.pull_repository()?;
            } else {
                tracing::debug!("remote git repository has no updates, skipping pull");
            }
        } else {
            return Err(SkillRepositoryError::Git(format!(
                "local path exists but is not a valid git repository: {}",
                self.local_path.display()
            )));
        }

        let mut state = self.state.lock().unwrap();
        if state.skills_path.is_none() {
            state.skills_path = Some(self.resolve_skills_path()?);
        }
        Ok(())
    }

    /// Resolves the directory holding skills, honouring an explicit `skills_root`.
    fn resolve_skills_path(&self) -> Result<PathBuf, SkillRepositoryError> {
        if let Some(root) = normalize_skills_root(self.cfg.skills_root.as_deref())? {
            let candidate = self.local_path.join(&root);
            if !candidate.is_dir() {
                return Err(SkillRepositoryError::Git(format!(
                    "configured skillsRoot does not exist in repository: {root}"
                )));
            }
            return Ok(candidate);
        }
        let skills_sub = self.local_path.join("skills");
        if skills_sub.is_dir() {
            Ok(skills_sub)
        } else {
            Ok(self.local_path.clone())
        }
    }

    /// Ensures the repository is synchronized when `auto_sync` is enabled.
    fn ensure_auto_synced(&self) -> Result<(), SkillRepositoryError> {
        if self.cfg.auto_sync {
            return self.ensure_repository_synced();
        }
        if self.state.lock().unwrap().skills_path.is_none() {
            return Err(SkillRepositoryError::Git(
                "repository is not synchronized; call sync() before reading skills".into(),
            ));
        }
        Ok(())
    }

    /// Forces a synchronization pass (mirrors the Java `sync()`).
    pub fn sync(&self) -> Result<(), SkillRepositoryError> {
        self.ensure_repository_synced()
    }

    /// Loads all skills from the resolved skills directory.
    fn load_all(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        self.ensure_auto_synced()?;
        let path = self
            .state
            .lock()
            .unwrap()
            .skills_path
            .clone()
            .ok_or_else(|| SkillRepositoryError::Git("skills path not initialized".into()))?;
        Ok(load_skills_from_dir(&path, &self.get_source()))
    }
}

impl AgentSkillRepository for GitSkillRepository {
    fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError> {
        crate::util::validate_skill_name(name)?;
        self.load_all()?
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| SkillRepositoryError::NotFound(name.to_string()))
    }

    fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
        let mut names: Vec<String> = self.load_all()?.into_iter().map(|s| s.name().to_string()).collect();
        names.sort();
        Ok(names)
    }

    fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        self.load_all()
    }

    fn save(&self, _skills: &[AgentSkill], _force: bool) -> Result<bool, SkillRepositoryError> {
        tracing::warn!("GitSkillRepository is read-only; save ignored");
        Err(SkillRepositoryError::ReadOnly)
    }

    fn delete(&self, _skill_name: &str) -> Result<bool, SkillRepositoryError> {
        // Java silently ignores writes on the read-only git repository and returns false.
        Ok(false)
    }

    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        crate::util::validate_skill_name(skill_name)?;
        Ok(self.load_all()?.iter().any(|s| s.name() == skill_name))
    }

    fn get_repository_info(&self) -> AgentSkillRepositoryInfo {
        AgentSkillRepositoryInfo::new("git", self.cfg.remote_url.clone(), false)
    }

    fn get_source(&self) -> String {
        self.cfg
            .source
            .clone()
            .unwrap_or_else(|| self.build_default_source())
    }

    fn is_writeable(&self) -> bool {
        false
    }

    fn close(&self) -> Result<(), SkillRepositoryError> {
        if self.owns_temp_dir && self.local_path.exists() {
            std::fs::remove_dir_all(&self.local_path).map_err(|e| {
                SkillRepositoryError::Io(format!(
                    "failed to cleanup local repository directory {}: {e}",
                    self.local_path.display()
                ))
            })?;
        }
        Ok(())
    }
}

impl Drop for GitSkillRepository {
    fn drop(&mut self) {
        // Best-effort cleanup for temp dirs (mirrors the JVM shutdown hook).
        if self.owns_temp_dir && self.local_path.exists() {
            let _ = std::fs::remove_dir_all(&self.local_path);
        }
    }
}

/// Creates a unique temp directory (no `tempfile` dependency).
fn create_temp_dir() -> Result<PathBuf, SkillRepositoryError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::temp_dir().join(format!(
        "agentscope-git-skills-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// Returns `true` when the directory has no entries.
fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut it| it.next().is_none())
        .unwrap_or(true)
}

/// Validates and normalises a user-supplied in-repo skills root (`normalizeSkillsRoot`).
///
/// Rejects absolute paths and `..` segments; blank input means "unspecified" (`Ok(None)`).
fn normalize_skills_root(raw: Option<&str>) -> Result<Option<String>, SkillRepositoryError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = Path::new(trimmed);
    // POSIX semantics: a leading `/` (or Windows `\\` / drive prefix) is absolute. Java's
    // `Path.isAbsolute()` treats `/abs` as absolute; Rust's `Path::is_absolute()` does **not**
    // on Windows (it requires a drive/UNC prefix), so check the raw string as well.
    let starts_with_sep = trimmed.starts_with('/') || trimmed.starts_with('\\');
    if path.is_absolute() || starts_with_sep {
        return Err(SkillRepositoryError::invalid(
            "skillsRoot",
            format!("must be a path relative to the repository root, got: {raw}"),
        ));
    }
    for segment in path.components() {
        if segment.as_os_str() == ".." {
            return Err(SkillRepositoryError::invalid(
                "skillsRoot",
                format!("must not contain '..' segments, got: {raw}"),
            ));
        }
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_url() {
        assert!(GitSkillRepository::new("").is_err());
    }

    #[test]
    fn repository_info_is_read_only_git() {
        let repo = GitSkillRepository::new("https://github.com/test/repo.git").unwrap();
        assert!(!repo.is_writeable());
        let info = repo.get_repository_info();
        assert_eq!(info.repo_type, "git");
        assert_eq!(info.location, "https://github.com/test/repo.git");
        assert!(!info.writable);
    }

    #[test]
    fn extracts_identifier_for_https_and_ssh() {
        assert_eq!(
            GitSkillRepository::extract_repository_identifier(
                "https://github.com/owner/repo.git"
            ),
            "owner/repo"
        );
        assert_eq!(
            GitSkillRepository::extract_repository_identifier("git@github.com:owner/repo.git"),
            "owner/repo"
        );
        assert_eq!(
            GitSkillRepository::extract_repository_identifier(
                "https://gitlab.com/group/subgroup/project.git"
            ),
            "group/subgroup/project"
        );
    }

    #[test]
    fn default_source_includes_branch_and_sanitises() {
        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new("https://github.com/owner/repo.git")
                .branch("feature/x"),
        )
        .unwrap();
        assert_eq!(repo.get_source(), "git-owner/repo@feature/x");
    }

    #[test]
    fn skills_root_rejects_absolute_and_parent() {
        assert!(GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new("u").skills_root("/abs")
        )
        .is_err());
        assert!(GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new("u").skills_root("a/../b")
        )
        .is_err());
    }
}
