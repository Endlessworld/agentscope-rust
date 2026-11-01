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
//! Carrier adjudication (SKILLS-10): **keep the CLI**, and verify it up front with
//! `git --version` ([`GitSkillRepository::check_git_available`], invoked from
//! [`GitSkillRepository::from_config`]) so a missing or broken binary fails at construction with
//! an actionable message instead of surfacing as an opaque error on the first read. The binary is
//! injectable through [`GitSkillRepositoryConfig::git_binary`] for hosts that install git outside
//! `PATH`.
//!
//! Each read triggers a lightweight remote-ref check (filtered to `refs/heads/<branch>`, like
//! Java's `setHeads(true)` + `matchesBranch`) and only pulls when the remote ref changed, exactly
//! like the Java `ensureRepositorySynced` / `hasRemoteUpdates` pair. An unreachable remote is a
//! hard error rather than a silent no-update (Java assumes updates and then fails inside the
//! pull; we surface the same observable failure one step earlier).
//!
//! Skill identity follows `SkillFileSystemHelper`: the skill **name** and **description** come
//! from the `SKILL.md` YAML frontmatter (`name` / `description`), falling back to the directory
//! name / first body line only when the frontmatter omits them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_harness::agent::skill::model::AgentSkill;
use agentscope_harness::agent::skill::repository::{load_skill_from_dir, load_skills_from_dir};

use crate::error::SkillRepositoryError;
use crate::repository::{AgentSkillRepository, AgentSkillRepositoryInfo};
use crate::util::split_frontmatter;

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
    /// The `git` program to invoke; `None` uses `git` from `PATH`.
    ///
    /// SKILLS-10: the backend keeps the system `git` CLI (no `gix`/`git2` dependency, and Java
    /// documents the `~/.gitconfig` / credential-helper / `~/.ssh` auth model), so hosts that
    /// install git outside `PATH` - and tests that need a deterministic failure - can point at an
    /// explicit binary.
    pub git_binary: Option<String>,
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
            git_binary: None,
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

    /// Overrides the `git` program (default: `git` from `PATH`), see [`Self::git_binary`].
    pub fn git_binary(mut self, binary: impl Into<String>) -> Self {
        self.git_binary = Some(binary.into());
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
    /// The `git` program every invocation uses (normalised at construction, see SKILLS-10).
    git_binary: String,
    /// Serialises whole synchronization passes (SKILLS-04).
    ///
    /// Kept apart from [`Self::state`] so the pass can still lock the state fields inside: a
    /// second thread must wait for the clone / pull to finish instead of racing it and cloning on
    /// top of a half-written work tree. Java gets this from
    /// `GitSkillRepository#ensureRepositorySynced` being `synchronized`.
    sync_lock: Mutex<()>,
    /// How many `git clone` invocations this instance performed (see [`Self::clone_count`]).
    clone_count: AtomicUsize,
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
        // Java stores `remoteUrl.trim()`; keep an untrimmed value out of every shell call below.
        let mut cfg = cfg;
        cfg.remote_url = cfg.remote_url.trim().to_string();
        // SKILLS-10: the CLI carrier is a runtime dependency, so verify it here - before a temp
        // clone dir is created - and report one actionable error instead of an opaque failure on
        // the first read.
        let git_binary = normalize_git_binary(cfg.git_binary.as_deref());
        Self::check_git_available(&git_binary)?;
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
            git_binary,
            sync_lock: Mutex::new(()),
            clone_count: AtomicUsize::new(0),
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

    /// Number of `git clone` invocations this instance performed.
    ///
    /// Observability for the single-flight guarantee of [`Self::ensure_repository_synced`]
    /// (SKILLS-04): concurrent first reads must clone exactly once.
    pub fn clone_count(&self) -> usize {
        self.clone_count.load(Ordering::Relaxed)
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

    /// Runs `<program> <args>` in the given working directory, mapping failures to
    /// [`SkillRepositoryError::Git`].
    ///
    /// `program` comes from [`GitSkillRepositoryConfig::git_binary`] (normalised once at
    /// construction), so the error message names the binary that was actually invoked.
    fn run_git(
        program: &str,
        cwd: Option<&Path>,
        args: &[&str],
    ) -> Result<std::process::Output, SkillRepositoryError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let output = cmd.output().map_err(|e| {
            SkillRepositoryError::Git(format!(
                "failed to run `{program} {}`: {e}. The git skill backend runs the system git CLI; \
                 install git, put it on PATH, or set `GitSkillRepositoryConfig::git_binary`.",
                args.join(" ")
            ))
        })?;
        Ok(output)
    }

    /// Clones the remote repository into the local path.
    fn clone_repository(&self) -> Result<(), SkillRepositoryError> {
        tracing::info!(url = %self.cfg.remote_url, path = %self.local_path.display(), "cloning git skill repository");
        self.clone_count.fetch_add(1, Ordering::Relaxed);
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
        let output = Self::run_git(&self.git_binary, None, &arg_refs)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SkillRepositoryError::Git(format!(
                "clone failed for {}: {}",
                self.cfg.remote_url,
                stderr.trim()
            )));
        }
        if let Ok(head) = Self::rev_parse(&self.git_binary, &self.local_path, "HEAD") {
            self.state.lock().unwrap().last_remote_ref = Some(head);
        }
        Ok(())
    }

    /// `<program> rev-parse <rev>` in `dir`.
    fn rev_parse(program: &str, dir: &Path, rev: &str) -> Result<String, SkillRepositoryError> {
        let output = Self::run_git(program, Some(dir), &["rev-parse", rev])?;
        if !output.status.success() {
            return Err(SkillRepositoryError::Git(format!("rev-parse {rev} failed")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Resolves the remote ref used for the update check, mirroring Java `resolveRemoteHead`.
    ///
    /// Java uses `lsRemote.setHeads(true)` plus `matchesBranch` (`refs/heads/<branch>`); with the
    /// system `git` binary the same filter is expressed by passing the refspec to `ls-remote`.
    /// When no branch is configured the remote `HEAD` (the ref that was cloned) is used.
    ///
    /// * `Ok(Some(sha))` - resolved;
    /// * `Ok(None)` - the remote is reachable but has no matching head;
    /// * `Err(..)` - `git` could not run, or the remote is unreachable.
    fn remote_head(&self) -> Result<Option<String>, SkillRepositoryError> {
        let branch = self
            .cfg
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty());
        let spec = branch.map(|b| format!("refs/heads/{b}"));
        let mut args: Vec<&str> = vec!["ls-remote", self.cfg.remote_url.as_str()];
        args.push(spec.as_deref().unwrap_or("HEAD"));
        let output = Self::run_git(&self.git_binary, None, &args)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SkillRepositoryError::Git(format!(
                "ls-remote failed for {}: {}",
                self.cfg.remote_url,
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?.to_string();
            match (parts.next(), spec.as_deref()) {
                // Guard against a server that ignores the refspec filter.
                (Some(reference), Some(expected)) if reference != expected => None,
                (None, Some(_)) => None,
                _ => Some(sha),
            }
        }))
    }

    /// Whether the remote ref differs from the last synced ref.
    ///
    /// Mirrors Java `hasRemoteUpdates`: an unresolvable remote head means "assume there are
    /// updates" (`resolveRemoteHead` returns null -> true -> pull -> error). Here the unreachable
    /// case is reported directly, so reads fail instead of silently serving stale data; the
    /// observable outcome is the same as Java's "pull then blow up", minus the pointless pull.
    fn has_remote_updates(&self) -> Result<bool, SkillRepositoryError> {
        let remote = match self.remote_head() {
            Ok(Some(remote)) => remote,
            Ok(None) => return Ok(true),
            Err(e) => {
                tracing::warn!(
                    url = %self.cfg.remote_url,
                    error = %e,
                    "cannot resolve the remote git ref; refusing to read possibly stale skills"
                );
                return Err(e);
            }
        };
        let mut state = self.state.lock().unwrap();
        if state.last_remote_ref.is_none() {
            state.last_remote_ref = Self::rev_parse(&self.git_binary, &self.local_path, "HEAD").ok();
        }
        Ok(state.last_remote_ref.as_deref() != Some(remote.as_str()))
    }

    /// Pulls the latest changes into an existing clone.
    fn pull_repository(&self) -> Result<(), SkillRepositoryError> {
        let output =
            Self::run_git(&self.git_binary, Some(&self.local_path), &["pull", "--ff-only"])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SkillRepositoryError::Git(format!(
                "pull failed for {}: {}",
                self.cfg.remote_url,
                stderr.trim()
            )));
        }
        self.state.lock().unwrap().last_remote_ref = Self::rev_parse(&self.git_binary, &self.local_path, "HEAD").ok();
        Ok(())
    }

    /// Ensures the clone exists (cloning or pulling as needed) and resolves the skills path.
    ///
    /// The whole pass is single-flight (SKILLS-04): Java marks `ensureRepositorySynced`
    /// `synchronized`, and without the same guarantee two threads reading a fresh repository
    /// both see an empty directory and race `remove_dir_all` + `git clone` into the same path,
    /// leaving a failed or half-written work tree. The lock is separate from [`Self::state`] so
    /// it can be held across the git invocations (and so it never re-enters `state`).
    fn ensure_repository_synced(&self) -> Result<(), SkillRepositoryError> {
        let _guard = self
            .sync_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let empty = !self.local_path.exists() || is_empty_dir(&self.local_path);
        if empty {
            // A previously created temp dir must be removed before `git clone`.
            if self.local_path.exists() {
                let _ = std::fs::remove_dir_all(&self.local_path);
            }
            self.clone_repository()?;
        } else if self.local_path.join(".git").exists() {
            if self.has_remote_updates()? {
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

    /// Verifies that the `git` binary is runnable and reports a version (`git --version`).
    ///
    /// SKILLS-10 adjudication: the backend keeps shelling out to the system `git` CLI rather than
    /// taking a `gix` / `git2` dependency (zero new crates, and the user's credential model -
    /// `~/.gitconfig`, credential helpers, `~/.ssh` - keeps working exactly as Java documents).
    /// The price is a runtime dependency on the binary, so the check runs in [`Self::from_config`]:
    /// a missing `git` becomes one clear, actionable error at construction time.
    ///
    /// Returns the raw version line (`git version 2.43.0`).
    pub fn check_git_available(git_binary: &str) -> Result<String, SkillRepositoryError> {
        let program = normalize_git_binary(Some(git_binary));
        let output = Command::new(&program)
            .arg("--version")
            .output()
            .map_err(|e| {
                SkillRepositoryError::Git(format!(
                    "`{program} --version` could not be executed: {e}. The git skill backend runs \
                     the system git CLI; install git, put it on PATH, or set \
                     `GitSkillRepositoryConfig::git_binary` to its path."
                ))
            })?;
        if !output.status.success() {
            return Err(SkillRepositoryError::Git(format!(
                "`{program} --version` failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !version.contains("git version") {
            return Err(SkillRepositoryError::Git(format!(
                "`{program} --version` did not report a git version (got {version:?}); \
                 `GitSkillRepositoryConfig::git_binary` must point at a git executable"
            )));
        }
        Ok(version)
    }

    /// Forces a synchronization pass (mirrors the Java `sync()`).
    pub fn sync(&self) -> Result<(), SkillRepositoryError> {
        self.ensure_repository_synced()
    }

    /// The resolved on-disk skills directory (after a sync pass).
    fn skills_path(&self) -> Result<PathBuf, SkillRepositoryError> {
        self.ensure_auto_synced()?;
        self.state
            .lock()
            .unwrap()
            .skills_path
            .clone()
            .ok_or_else(|| SkillRepositoryError::Git("skills path not initialized".into()))
    }

    /// Loads **every** skill (all `SKILL.md` files and all resources) from the resolved skills
    /// directory, applying the Java frontmatter identity (`SkillFileSystemHelper`:
    /// name/description from the `SKILL.md` frontmatter).
    ///
    /// SKILLS-06: this is the bulk read and costs O(repository bytes). Name-only and
    /// single-skill reads must not go through it - [`Self::get_skill`] loads one directory
    /// ([`load_skill_from_dir`]) and [`Self::get_all_skill_names`] parses frontmatter only
    /// ([`skill_names_in_dir`]), matching Java `SkillFileSystemHelper.loadSkill` /
    /// `getAllSkillNames`. Only whole-repository reads (`get_all_skills`) and the existence probe
    /// that keeps `SkillFileSystemHelper#skillExists` semantics (`skill_exists`) use it.
    fn load_all(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        let path = self.skills_path()?;
        Ok(load_skills_with_frontmatter(&path, &self.get_source()))
    }

    /// Names of every skill in the resolved skills directory.
    ///
    /// Reads only each skill's `SKILL.md` frontmatter - no resource is loaded - so listing names
    /// is O(#skills) small reads instead of O(repository bytes).
    fn load_all_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
        let path = self.skills_path()?;
        Ok(skill_names_in_dir(&path))
    }

    /// Resolves the on-disk directory of a skill, matching on the frontmatter `name` (the same
    /// identity `get_skill` uses) and falling back to the directory name.
    fn resolve_skill_dir(&self, skill_name: &str) -> Option<PathBuf> {
        // The harness calls this once per skill while building its index, so only synchronize when
        // the repository has never been synced (avoids one `ls-remote` per skill).
        let mut root = self.state.lock().unwrap().skills_path.clone();
        if root.is_none() {
            self.ensure_auto_synced().ok()?;
            root = self.state.lock().unwrap().skills_path.clone();
        }
        let root = root?;
        for entry in std::fs::read_dir(&root).ok()?.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("SKILL.md").is_file() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if !is_reportable_skill_dir(&dir_name) {
                continue;
            }
            let name = match std::fs::read_to_string(dir.join("SKILL.md")) {
                Ok(md) => skill_identity(&md, &dir_name).0,
                Err(_) => dir_name,
            };
            if name == skill_name {
                return Some(absolute_path(&dir));
            }
        }
        None
    }
}

impl AgentSkillRepository for GitSkillRepository {
    /// Loads a single skill by its frontmatter `name`.
    ///
    /// SKILLS-06: mirrors `SkillFileSystemHelper.loadSkill` - the directory is resolved first
    /// (frontmatter `name`, directory-name fallback) and only that directory is loaded, so the
    /// cost is O(target skill bytes) instead of O(repository bytes).
    fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError> {
        crate::util::validate_skill_name(name)?;
        // Surface sync failures as errors (as the old whole-repository load did) instead of
        // letting `resolve_skill_dir` turn them into a `NotFound`.
        self.ensure_auto_synced()?;
        let dir = self
            .resolve_skill_dir(name)
            .ok_or_else(|| SkillRepositoryError::NotFound(name.to_string()))?;
        load_skill_from_dir(&dir, &self.get_source())
            .map(apply_frontmatter_identity)
            .filter(|skill| skill.name() == name)
            .ok_or_else(|| SkillRepositoryError::NotFound(name.to_string()))
    }

    fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
        self.load_all_names()
    }

    fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        self.load_all()
    }

    fn save(&self, _skills: &[AgentSkill], _force: bool) -> Result<bool, SkillRepositoryError> {
        // Java `GitSkillRepository#save`: log a warning and return `false`, never throw (P2-06).
        tracing::warn!("GitSkillRepository is read-only, save operation ignored");
        Ok(false)
    }

    fn delete(&self, _skill_name: &str) -> Result<bool, SkillRepositoryError> {
        // Java silently ignores writes on the read-only git repository and returns false.
        Ok(false)
    }

    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        // Java `SkillFileSystemHelper#skillExists` reports a blank name as "absent" without
        // touching the repository, so this must not trigger a sync (P2-08).
        if skill_name.trim().is_empty() {
            return Ok(false);
        }
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

    /// P1-08: exposes the on-disk skill directory so `load_skill_through_path` works.
    fn skill_dir(&self, skill_name: &str) -> Option<PathBuf> {
        self.resolve_skill_dir(skill_name)
    }

    /// Read-only backend: the flag is ignored, exactly as Java's `GitSkillRepository`
    /// logs a warning and keeps `isWriteable()` false.
    fn set_writeable(&self, writeable: bool) {
        if writeable {
            tracing::warn!("GitSkillRepository is read-only, set writeable operation ignored");
        }
    }

    fn close(&self) -> Result<(), SkillRepositoryError> {
        if self.owns_temp_dir && self.local_path.exists() {
            // Java `GitSkillRepository#close` only logs a failed cleanup (`LOGGER.warn`) and
            // returns normally: a leaked temp directory must not fail the caller's shutdown
            // path. The trait keeps `Result` for backends that *can* fail (a driver that must
            // flush), but a filesystem cleanup failure is logged and swallowed here so the
            // semantics match Java byte for byte.
            if let Err(e) = std::fs::remove_dir_all(&self.local_path) {
                tracing::warn!(
                    path = %self.local_path.display(),
                    error = %e,
                    "failed to cleanup local repository directory"
                );
            }
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

/// The `git` program to invoke: the configured override, or `git` from `PATH`.
///
/// A blank / whitespace-only override is treated as "unspecified" so an empty config value never
/// turns into a confusing `Command::new("")` failure (SKILLS-10).
fn normalize_git_binary(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|program| !program.is_empty())
        .unwrap_or("git")
        .to_string()
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

/// Loads every skill directory under `root`, then replaces the harness identity (directory name +
/// first body line) with the Java `SkillFileSystemHelper` identity (frontmatter `name` /
/// `description`). Resources and scanning come from the harness loader - no parallel directory
/// walk is implemented here.
fn load_skills_with_frontmatter(root: &Path, source: &str) -> Vec<AgentSkill> {
    let mut skills: Vec<AgentSkill> = load_skills_from_dir(root, source)
        .into_iter()
        .map(apply_frontmatter_identity)
        .collect();
    skills.sort_by(|a, b| a.name().cmp(b.name()));
    skills
}

/// Names of the skills under `root`, parsed from each `SKILL.md` frontmatter only.
///
/// SKILLS-06: mirrors Java `SkillFileSystemHelper.getAllSkillNames` - a directory contributes its
/// frontmatter `name` (directory-name fallback, same identity as [`load_skills_with_frontmatter`])
/// and nothing else is read. Directory skipping matches [`load_skills_from_dir`] (hidden entries
/// and `_drafts`), and a directory without a readable `SKILL.md` has no skill, so it is skipped.
fn skill_names_in_dir(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if !dir.is_dir() || !is_reportable_skill_dir(&dir_name) {
            continue;
        }
        if let Ok(skill_md) = std::fs::read_to_string(dir.join("SKILL.md")) {
            names.push(skill_identity(&skill_md, &dir_name).0);
        }
    }
    names.sort();
    names
}

/// Rewrites a harness-loaded skill with the frontmatter-derived name / description.
fn apply_frontmatter_identity(skill: AgentSkill) -> AgentSkill {
    let dir_name = skill.name().to_string();
    let resources = skill.resources().clone();
    let metadata = skill.metadata().clone();
    let Some(skill_md) = resources.get("SKILL.md") else {
        return skill;
    };
    let (name, description) = skill_identity(skill_md, &dir_name);
    if name == dir_name && description == skill.description() {
        return skill;
    }
    AgentSkill::builder()
        .name(name)
        .source(skill.source())
        .description(description)
        .metadata(metadata)
        .resources(resources)
        .build()
}

/// Derives `(name, description)` from `SKILL.md`, mirroring `SkillFileSystemHelper` (frontmatter
/// `name` / `description`) and falling back to the directory name / first body line when the
/// frontmatter omits them (Java skips such skills; the harness always reported them, so the
/// fallback keeps that leniency).
fn skill_identity(skill_md: &str, dir_name: &str) -> (String, String) {
    let front = frontmatter_scalars(skill_md);
    let name = match front.get("name").map(|v| v.trim()) {
        Some(candidate) if candidate.is_empty() => dir_name.to_string(),
        Some(candidate) if is_safe_skill_name(candidate) => candidate.to_string(),
        Some(candidate) => {
            tracing::warn!(
                name = candidate,
                dir = dir_name,
                "ignoring unsafe frontmatter skill name (path separators / '..')"
            );
            dir_name.to_string()
        }
        None => dir_name.to_string(),
    };
    let description = front
        .get("description")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| first_body_line(skill_md).unwrap_or_else(|| format!("Skill: {name}")));
    (name, description)
}

fn is_safe_skill_name(name: &str) -> bool {
    !name.contains("..") && !name.contains('/') && !name.contains('\\')
}

/// Whether a directory under the skills root can hold a skill: hidden entries and `_drafts` are
/// skipped (the harness `load_skills_from_dir` scan skips both), so name listing, single-skill
/// loads and bulk loads all report the same set (SKILLS-06).
fn is_reportable_skill_dir(dir_name: &str) -> bool {
    !dir_name.starts_with('.') && dir_name != "_drafts"
}

/// First non-empty body line with leading `#` stripped (harness `skill_description` semantics,
/// applied to the body so a frontmatter file does not yield `---`).
fn first_body_line(skill_md: &str) -> Option<String> {
    split_frontmatter(skill_md)
        .1
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim().to_string())
        .filter(|line| !line.is_empty())
}

/// Extracts the top-level scalar keys of the YAML frontmatter (the subset the skill identity
/// needs: `name` / `description`). Mirrors the Java fallback parser: nested maps and top-level
/// lists are skipped, `|` / `>` block scalars are folded, quoted scalars are unquoted.
fn frontmatter_scalars(markdown: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(front) = split_frontmatter(markdown).0 else {
        return out;
    };
    let lines: Vec<&str> = front.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('-') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() || key.contains(' ') || key.contains('\t') {
            continue;
        }
        let value = raw_value.trim();
        if value.starts_with('|') || value.starts_with('>') {
            let folded = value.starts_with('>');
            let mut parts: Vec<&str> = Vec::new();
            while i < lines.len() {
                let next = lines[i];
                if !(next.starts_with(' ') || next.starts_with('\t')) {
                    break;
                }
                i += 1;
                parts.push(next.trim());
            }
            let joined = if folded {
                parts.join(" ")
            } else {
                parts.join("\n")
            };
            out.insert(key.to_string(), joined.trim().to_string());
            continue;
        }
        if value.is_empty() {
            // Nested mapping or an empty value: not a usable scalar.
            continue;
        }
        out.insert(key.to_string(), scalar_value(value));
    }
    out
}

fn scalar_value(raw: &str) -> String {
    let raw = raw.trim();
    let quoted = raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')));
    if quoted {
        return raw[1..raw.len() - 1].to_string();
    }
    match raw.find(" #") {
        Some(idx) => raw[..idx].trim_end().to_string(),
        None => raw.to_string(),
    }
}

/// Makes a path absolute without requiring it to exist (`Path::canonicalize` would fail).
fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
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
            GitSkillRepository::extract_repository_identifier("https://github.com/owner/repo.git"),
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
            GitSkillRepositoryConfig::new("https://github.com/owner/repo.git").branch("feature/x"),
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

    // ------------------------------------------------------------------
    // P1-01 / P1-02: frontmatter identity (unit level)
    // ------------------------------------------------------------------

    #[test]
    fn frontmatter_scalars_reads_plain_quoted_and_block_values() {
        let md = "\u{feff}---\nname: \"calc\"\ndescription: Adds numbers # note\nnested:\n  child: x\nlist:\n  - a\nlong: |\n  line1\n  line2\nfolded: >-\n  one\n  two\n---\n# body\n";
        let fm = frontmatter_scalars(md);
        assert_eq!(fm.get("name").map(String::as_str), Some("calc"));
        assert_eq!(
            fm.get("description").map(String::as_str),
            Some("Adds numbers")
        );
        assert_eq!(fm.get("long").map(String::as_str), Some("line1\nline2"));
        assert_eq!(fm.get("folded").map(String::as_str), Some("one two"));
        assert!(!fm.contains_key("nested"));
        assert!(!fm.contains_key("child"));
        assert!(!fm.contains_key("list"));
    }

    #[test]
    fn skill_identity_prefers_frontmatter_and_falls_back() {
        let (name, desc) = skill_identity(
            "---\nname: calc\ndescription: Adds numbers\n---\n# Calc\n",
            "dir-name",
        );
        assert_eq!(name, "calc");
        assert_eq!(desc, "Adds numbers");

        // No frontmatter -> directory name + first body line.
        let (name, desc) = skill_identity("# Calc skill\nbody\n", "dir-name");
        assert_eq!(name, "dir-name");
        assert_eq!(desc, "Calc skill");

        // Frontmatter without a description -> first body line, not the `---` fence.
        let (name, desc) = skill_identity("---\nname: calc\n---\n# Calc skill\n", "dir-name");
        assert_eq!(name, "calc");
        assert_eq!(desc, "Calc skill");

        // Path traversal in the frontmatter name is ignored.
        let (name, _) = skill_identity("---\nname: ../escape\n---\nbody\n", "dir-name");
        assert_eq!(name, "dir-name");
    }

    #[test]
    fn split_frontmatter_splits_a_standard_fence() {
        let md = "---\nname: calc\ndescription: Adds numbers\n---\n# Body\n";
        assert_eq!(
            split_frontmatter(md),
            (Some("name: calc\ndescription: Adds numbers\n"), "# Body\n")
        );

        // An optional BOM and CRLF line endings behave identically.
        let crlf = "\u{feff}---\r\nname: calc\r\n---\r\nbody\r\n";
        assert_eq!(
            split_frontmatter(crlf),
            (Some("name: calc\r\n"), "body\r\n")
        );
    }

    /// Java parity: the closing fence must occupy a line of its own, so an indented `---`
    /// (typically inside a `|` / `>` block scalar) stays body content.
    #[test]
    fn split_frontmatter_requires_the_closing_fence_to_be_unindented() {
        let md = "---\nname: calc\nnote: |\n  ---\n  still body\n---\nreal body\n";
        assert_eq!(
            split_frontmatter(md),
            (
                Some("name: calc\nnote: |\n  ---\n  still body\n"),
                "real body\n"
            )
        );

        // Nothing but indented fences -> the frontmatter is never closed.
        let md = "---\nname: calc\nblock: |\n  ---\n  ...\nbody\n";
        assert_eq!(split_frontmatter(md), (None, md));
    }

    #[test]
    fn split_frontmatter_rejects_a_fence_with_a_suffix() {
        let md = "---\nname: calc\ndescription: d\n---suffix\nbody\n";
        assert_eq!(split_frontmatter(md), (None, md));
    }

    #[test]
    fn split_frontmatter_accepts_dots_and_trailing_blanks() {
        assert_eq!(
            split_frontmatter("---\nname: calc\n...\nbody\n"),
            (Some("name: calc\n"), "body\n")
        );
        // Java allows blank space between the closing fence and the line break.
        assert_eq!(
            split_frontmatter("---\nname: calc\n---  \t\nbody\n"),
            (Some("name: calc\n"), "body\n")
        );
    }

    /// SKILLS-05: Java's pattern allows blank space between the opening fence and the line break.
    #[test]
    fn split_frontmatter_accepts_trailing_blanks_after_the_opening_fence() {
        assert_eq!(
            split_frontmatter("---   \t\nname: calc\n---\nbody\n"),
            (Some("name: calc\n"), "body\n")
        );
        assert_eq!(
            split_frontmatter("---  \r\nname: calc\r\n---\r\nbody\r\n"),
            (Some("name: calc\r\n"), "body\r\n")
        );
        // A fence followed by blanks but no line break is still not a fence.
        assert_eq!(split_frontmatter("---  x\nbody\n"), (None, "---  x\nbody\n"));
    }

    /// SKILLS-05 end-to-end: a skill whose opening fence carries blanks keeps its frontmatter
    /// identity instead of degrading to the directory name / a `---` description.
    #[test]
    fn skill_identity_reads_a_frontmatter_after_a_blank_padded_fence() {
        let (name, description) = skill_identity(
            "--- \t\nname: calc\ndescription: Adds numbers\n---\n# Calc\n",
            "dir-name",
        );
        assert_eq!(name, "calc");
        assert_eq!(description, "Adds numbers");

        // Without a description the fallback must skip the fence and read the body heading.
        let (name, description) = skill_identity("--- \nname: calc\n---\n# Calc skill\n", "dir");
        assert_eq!(name, "calc");
        assert_ne!(description, "---");
        assert_eq!(description, "Calc skill");
    }

    #[test]
    fn split_frontmatter_returns_the_whole_text_without_a_closing_fence() {
        let md = "---\nname: calc\nbody without a fence\n";
        assert_eq!(split_frontmatter(md), (None, md));
        assert_eq!(split_frontmatter("# Calc\n"), (None, "# Calc\n"));
        // The opening fence needs a line break right behind it.
        assert_eq!(split_frontmatter("---x\nbody\n"), (None, "---x\nbody\n"));
    }

    /// End-to-end guard: an indented fence inside a block scalar keeps the real body intact,
    /// so the description still falls back to the first body heading (not the `---` fence).
    #[test]
    fn skill_identity_treats_an_indented_fence_as_body() {
        let (name, desc) = skill_identity(
            "---\nname: calc\nnote: |\n  ---\n---\n# Heading\n",
            "dir-name",
        );
        assert_eq!(name, "calc");
        assert_eq!(desc, "Heading");
    }

    // ------------------------------------------------------------------
    // Parser edge cases (P1-09): missing / extra separators, CRLF, malformed keys
    // ------------------------------------------------------------------

    /// A second `---` line belongs to the body: only the first closing fence counts.
    #[test]
    fn split_frontmatter_handles_extra_separators() {
        let md = "---\nname: a\n---\n---\nbody\n";
        assert_eq!(split_frontmatter(md), (Some("name: a\n"), "---\nbody\n"));
        // A `---` inside the body without an opening fence stays body content.
        assert_eq!(
            split_frontmatter("body\n---\nmore\n"),
            (None, "body\n---\nmore\n")
        );
    }

    /// Windows line endings work end to end, with and without frontmatter.
    #[test]
    fn split_frontmatter_handles_crlf_without_frontmatter() {
        assert_eq!(
            split_frontmatter("# Calc\r\nbody\r\n"),
            (None, "# Calc\r\nbody\r\n")
        );
        let (name, desc) = skill_identity(
            "---\r\nname: crlf\r\ndescription: windows\r\n---\r\n# Head\r\n",
            "dir-name",
        );
        assert_eq!(name, "crlf");
        assert_eq!(desc, "windows");
    }

    /// Frontmatter without a fence pair is ignored wholesale.
    #[test]
    fn frontmatter_scalars_ignores_malformed_lines() {
        let md = "---\nno-colon-line\nkey with space: v\n: empty-key\nvalid: yes\n---\nbody\n";
        let fm = frontmatter_scalars(md);
        assert_eq!(fm.get("valid").map(String::as_str), Some("yes"));
        assert_eq!(fm.len(), 1, "unexpected keys: {fm:?}");
        // No frontmatter at all -> nothing parsed, even if key/value lines exist.
        assert!(frontmatter_scalars("name: nope\n").is_empty());
    }

    /// An empty frontmatter block falls back to the directory name + first body heading.
    #[test]
    fn skill_identity_with_empty_frontmatter() {
        let (name, desc) = skill_identity("---\n---\n# Heading\nbody\n", "dir-name");
        assert_eq!(name, "dir-name");
        assert_eq!(desc, "Heading");
        // Frontmatter with only an unrelated key behaves the same way.
        let (name, desc) = skill_identity("---\nlicense: MIT\n---\nPlain first line\n", "dir");
        assert_eq!(name, "dir");
        assert_eq!(desc, "Plain first line");
    }

    #[test]
    fn read_only_write_paths_are_ignored() {
        let repo = GitSkillRepository::new("https://github.com/test/repo.git").unwrap();
        // Java `GitSkillRepository#save` logs and returns `false` - no error, and consistent with
        // `delete` (P2-06).
        assert!(!repo.save(&[], true).unwrap());
        let skill = AgentSkill::builder()
            .name("x")
            .source("s")
            .description("d")
            .build();
        assert!(!repo.save(std::slice::from_ref(&skill), true).unwrap());
        assert!(!repo.delete("anything").unwrap());
        repo.set_writeable(true);
        assert!(!repo.is_writeable());
    }

    /// P2-08: a blank name is "absent" and must not trigger a sync (the URL is unreachable).
    #[test]
    fn skill_exists_is_false_for_a_blank_name_without_syncing() {
        let repo = GitSkillRepository::new("https://example.invalid/owner/repo.git").unwrap();
        assert!(!repo.skill_exists("").unwrap());
        assert!(!repo.skill_exists("   ").unwrap());
        // Path traversal still fails loudly.
        assert!(repo.skill_exists("../escape").is_err());
    }

    // ------------------------------------------------------------------
    // SKILLS-10: the git CLI carrier is verified up front
    // ------------------------------------------------------------------

    /// The startup self-check the CLI carrier relies on: `git --version` must run and report a
    /// git version (offline, no clone).
    #[test]
    fn git_self_check_reports_the_system_git_version() {
        let version = GitSkillRepository::check_git_available("git").expect("git --version");
        assert!(
            version.starts_with("git version"),
            "unexpected output: {version}"
        );
    }

    /// A missing / unusable binary fails at construction time with a message that says what to do,
    /// instead of surfacing as an opaque failure on the first read.
    #[test]
    fn git_self_check_rejects_an_unusable_binary_at_construction_time() {
        let result = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new("file:///nonexistent/agentscope-skills")
                .git_binary("agentscope-no-such-git-binary-2026"),
        );
        let error = match result {
            Ok(_) => panic!("a missing git binary must fail at construction time"),
            Err(e) => e,
        };
        let message = error.to_string();
        assert!(
            message.contains("agentscope-no-such-git-binary-2026"),
            "{message}"
        );
        assert!(message.contains("--version"), "{message}");
        // The message must name the configuration knob that fixes it.
        assert!(message.contains("git_binary"), "{message}");
        assert!(matches!(error, SkillRepositoryError::Git(_)));
    }

    /// The configured binary drives *every* invocation (not just the self-check), and a padded
    /// override is normalised instead of being passed to `Command::new` verbatim.
    #[test]
    fn git_binary_override_is_normalised_and_used() {
        let root = unique_dir("git-binary");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "s1", "s1", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .local_path(root.join("clone"))
                .git_binary("  git  "),
        )
        .expect("an explicit git path must work");
        assert_eq!(
            repo.get_all_skill_names().unwrap(),
            vec!["s1".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_url_is_trimmed() {
        let repo = GitSkillRepository::new("  https://github.com/test/repo.git \n").unwrap();
        assert_eq!(
            repo.get_repository_info().location,
            "https://github.com/test/repo.git"
        );
    }

    // ------------------------------------------------------------------
    // Real `file://` repositories (Java parity: 23 offline JGit cases)
    // ------------------------------------------------------------------

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "agentscope-git-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "agentscope-tests")
            .env("GIT_AUTHOR_EMAIL", "tests@example.com")
            .env("GIT_COMMITTER_NAME", "agentscope-tests")
            .env("GIT_COMMITTER_EMAIL", "tests@example.com")
            .output()
            .expect("the git binary is required by the git repository tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q"]);
        // Version independent `git init -b main`.
        git_in(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    fn commit_all(dir: &Path, message: &str) {
        git_in(dir, &["add", "-A"]);
        git_in(
            dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message],
        );
    }

    fn write_skill(root: &Path, dir_name: &str, name: &str, description: &str) {
        write_skill_body(root, dir_name, name, description, "");
    }

    /// Like [`write_skill`], but appends extra markdown to the body so the two branches can
    /// carry files whose *content* differs while the frontmatter identity stays the same.
    fn write_skill_body(root: &Path, dir_name: &str, name: &str, description: &str, body: &str) {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n---\n\n# Heading\nbody text\n{body}\n"
            ),
        )
        .unwrap();
    }

    fn file_url(path: &Path) -> String {
        format!("file:///{}", path.display().to_string().replace('\\', "/"))
    }

    fn repo_for(url: &str, local: &Path) -> GitSkillRepository {
        GitSkillRepository::from_config(GitSkillRepositoryConfig::new(url).local_path(local))
            .unwrap()
    }

    #[test]
    fn clones_local_repository_and_uses_frontmatter_identity() {
        let root = unique_dir("clone");
        let remote = root.join("remote");
        init_repo(&remote);
        // Directory name deliberately differs from the frontmatter name (P1-01).
        write_skill(&remote, "dir-name", "calc-skill", "A calculator");
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(
            repo.get_all_skill_names().unwrap(),
            vec!["calc-skill".to_string()]
        );
        let skill = repo.get_skill("calc-skill").unwrap();
        assert_eq!(skill.name(), "calc-skill");
        assert_eq!(skill.description(), "A calculator"); // not `---` (P1-02)
        assert!(repo.skill_exists("calc-skill").unwrap());
        assert!(!repo.skill_exists("dir-name").unwrap());

        // P1-08: the load tool needs the real checkout directory.
        let dir = repo.skill_dir("calc-skill").expect("skill dir");
        assert_eq!(dir.file_name().unwrap().to_string_lossy(), "dir-name");
        assert!(dir.join("SKILL.md").is_file());
        assert!(repo.skill_dir("missing").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_skills_from_the_skills_subdirectory_convention() {
        let root = unique_dir("subdir");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(
            &remote,
            "skills/subdir-skill",
            "subdir-skill",
            "A skill in skills/",
        );
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(
            repo.get_skill("subdir-skill").unwrap().description(),
            "A skill in skills/"
        );
        let dir = repo.skill_dir("subdir-skill").unwrap();
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "skills");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pulls_new_remote_commits() {
        let root = unique_dir("pull");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(repo.get_all_skill_names().unwrap().len(), 1);

        write_skill(&remote, "skills/two", "two", "second");
        commit_all(&remote, "add two");

        assert_eq!(
            repo.get_all_skill_names().unwrap(),
            vec!["one".to_string(), "two".to_string()]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// P1-03: the remote probe must look at `refs/heads/<branch>`, not the remote `HEAD`.
    #[test]
    fn remote_head_is_filtered_by_branch() {
        let root = unique_dir("branch");
        let remote = root.join("remote");
        init_repo(&remote);
        // The same skill directory exists on both branches; only its body differs, so the
        // branch filter is observable through the checked-out content.
        //
        // Regression guard: this test used to assert that `main-skill` was *absent* from the
        // `develop` clone. That only held because `develop` was branched after `main-skill`
        // was committed — branching copies history, so the file (and its identity) is present
        // on `develop` by construction.
        write_skill_body(
            &remote,
            "skills/main-skill",
            "main-skill",
            "on main",
            "on main",
        );
        commit_all(&remote, "main");

        git_in(&remote, &["checkout", "-q", "-b", "develop"]);
        write_skill_body(
            &remote,
            "skills/main-skill",
            "main-skill",
            "on develop",
            "on develop",
        );
        commit_all(&remote, "develop");
        // Put HEAD back on `main` so a plain `ls-remote <url> HEAD` would return the wrong ref.
        git_in(&remote, &["checkout", "-q", "main"]);

        let main_head = git_in(&remote, &["rev-parse", "refs/heads/main"]);
        let dev_head = git_in(&remote, &["rev-parse", "refs/heads/develop"]);
        assert_ne!(main_head, dev_head);

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .branch("develop")
                .local_path(root.join("clone")),
        )
        .unwrap();
        assert_eq!(repo.remote_head().unwrap(), Some(dev_head.clone()));

        let skill = repo.get_skill("main-skill").unwrap();
        assert_eq!(skill.description(), "on develop");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// P1-04: an unreachable remote must surface an error instead of silently serving stale data.
    #[test]
    fn unreachable_remote_fails_instead_of_serving_stale_skills() {
        let root = unique_dir("unreachable");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(repo.get_all_skill_names().unwrap().len(), 1);

        // Drop the remote: the clone still exists, but `ls-remote` now fails.
        std::fs::remove_dir_all(&remote).unwrap();
        let err = repo.get_all_skill_names().unwrap_err();
        assert!(
            matches!(err, SkillRepositoryError::Git(_)),
            "unexpected error: {err:?}"
        );
        assert!(repo.skill_exists("one").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Java parity: `skillsRoot` pins a nested skills directory.
    #[test]
    fn custom_skills_root_loads_nested_skills() {
        let root = unique_dir("root");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(
            &remote,
            "packs/marketing/greeter",
            "greeter",
            "A skill nested under packs/marketing",
        );
        commit_all(&remote, "nested");

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .skills_root("packs/marketing")
                .local_path(root.join("clone")),
        )
        .unwrap();
        let skill = repo.get_skill("greeter").unwrap();
        assert_eq!(skill.name(), "greeter");
        assert_eq!(skill.description(), "A skill nested under packs/marketing");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Java parity: `autoSync=false` requires an explicit `sync()` before reads.
    #[test]
    fn manual_sync_is_required_when_auto_sync_is_disabled() {
        let root = unique_dir("manual");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .auto_sync(false)
                .local_path(root.join("clone")),
        )
        .unwrap();
        assert!(repo.get_all_skill_names().is_err());
        repo.sync().unwrap();
        assert_eq!(repo.get_all_skill_names().unwrap(), vec!["one".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Java `hasRemoteUpdates == false` -> no pull: local checkout changes survive a read.
    #[test]
    fn an_unchanged_remote_does_not_reclone_or_pull() {
        let root = unique_dir("no-pull");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let clone = root.join("clone");
        let repo = repo_for(&file_url(&remote), &clone);
        assert_eq!(repo.get_all_skill_names().unwrap(), vec!["one".to_string()]);

        // Dirty the working tree: a re-clone or a pull would revert it.
        if let Some(skill_dir) = repo.skill_dir("one") {
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: one\ndescription: local edit\n---\nbody\n",
            )
            .unwrap();
        }
        // The remote did not move, so the read must not fetch.
        assert_eq!(repo.get_skill("one").unwrap().description(), "local edit");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Java parity: every skill directory resource is exposed on the loaded skill.
    #[test]
    fn loads_skill_resources_from_the_checkout() {
        let root = unique_dir("resources");
        let remote = root.join("remote");
        init_repo(&remote);
        let skill_dir = remote.join("skills/tool");
        std::fs::create_dir_all(skill_dir.join("ref")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tool\ndescription: A tool\n---\n\n# Tool\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("ref/notes.txt"), "notes").unwrap();
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        let skill = repo.get_skill("tool").unwrap();
        assert_eq!(skill.description(), "A tool");
        assert_eq!(
            skill.resources().get("ref/notes.txt").map(String::as_str),
            Some("notes")
        );
        assert!(skill.resources().contains_key("SKILL.md"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Skill names are reported sorted, matching Java's `TreeMap`-backed listing.
    #[test]
    fn skill_names_are_sorted() {
        let root = unique_dir("sorted");
        let remote = root.join("remote");
        init_repo(&remote);
        for name in ["zulu", "alpha", "mike"] {
            write_skill(&remote, &format!("skills/{name}"), name, "d");
        }
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(
            repo.get_all_skill_names().unwrap(),
            vec!["alpha".to_string(), "mike".to_string(), "zulu".to_string()]
        );
        let skills = repo.get_all_skills().unwrap();
        assert_eq!(
            skills.iter().map(|s| s.name()).collect::<Vec<_>>(),
            vec!["alpha", "mike", "zulu"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A configured branch that does not exist must surface the clone failure.
    #[test]
    fn an_unknown_branch_is_reported() {
        let root = unique_dir("bad-branch");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .branch("does-not-exist")
                .local_path(root.join("clone")),
        )
        .unwrap();
        let err = repo.get_all_skill_names().unwrap_err();
        assert!(
            matches!(err, SkillRepositoryError::Git(_)),
            "unexpected error: {err:?}"
        );
        assert!(
            err.to_string().contains("clone failed"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A configured `skillsRoot` that is missing from the checkout is an error, not "no skills".
    #[test]
    fn a_missing_skills_root_is_reported() {
        let root = unique_dir("bad-root");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::from_config(
            GitSkillRepositoryConfig::new(file_url(&remote))
                .skills_root("packs/missing")
                .local_path(root.join("clone")),
        )
        .unwrap();
        let err = repo.get_all_skill_names().unwrap_err();
        assert!(
            err.to_string().contains("skillsRoot does not exist"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A non-empty local path that is not a git repository must not be clobbered.
    #[test]
    fn a_local_path_that_is_not_a_repository_is_rejected() {
        let root = unique_dir("not-a-repo");
        let local = root.join("local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("stray.txt"), "keep me").unwrap();

        let repo = repo_for("https://example.invalid/owner/repo.git", &local);
        let err = repo.get_all_skill_names().unwrap_err();
        assert!(
            err.to_string().contains("not a valid git repository"),
            "unexpected error: {err}"
        );
        assert!(local.join("stray.txt").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `close()` only removes a clone the repository created itself (Java `tempDirectory` flag).
    #[test]
    fn close_keeps_an_explicit_local_path() {
        let root = unique_dir("close");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let local = root.join("clone");
        let repo = repo_for(&file_url(&remote), &local);
        assert_eq!(repo.get_all_skill_names().unwrap().len(), 1);
        repo.close().unwrap();
        assert!(local.join(".git").is_dir(), "an explicit path must survive");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// P2-09: a temp clone owned by the repository is removed by `close()`, and a second call
    /// is a no-op (Java `close()` is idempotent).
    #[test]
    fn close_removes_an_owned_temp_dir_and_is_idempotent() {
        let root = unique_dir("close-owned");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::new(file_url(&remote)).unwrap();
        assert_eq!(repo.get_all_skill_names().unwrap(), vec!["one".to_string()]);
        let local = repo.local_path().to_path_buf();
        assert!(local.is_dir());
        repo.close().unwrap();
        assert!(!local.exists(), "an owned temp clone must be removed");
        // Second call is a no-op, not an error.
        repo.close().unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    /// P2-09 (adjudicated): Java `GitSkillRepository#close` only logs a failed cleanup
    /// (`LOGGER.warn`) and returns normally, so a leaked temp directory can never fail the
    /// caller's shutdown path. Rust keeps the `Result` return type (the trait serves backends
    /// that *can* fail) but swallows the filesystem failure the same way. On Windows an open
    /// handle inside the tree makes `remove_dir_all` fail, which pins the swallow branch.
    #[test]
    fn close_never_returns_a_cleanup_failure() {
        let root = unique_dir("close-failing");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = GitSkillRepository::new(file_url(&remote)).unwrap();
        assert_eq!(repo.get_all_skill_names().unwrap().len(), 1);
        let local = repo.local_path().to_path_buf();
        // Keep a handle open so the directory cannot be unlinked on Windows.
        let held = std::fs::File::open(local.join(".git").join("HEAD")).ok();
        assert!(repo.close().is_ok(), "a failed cleanup must not surface as Err");
        drop(held);

        let _ = std::fs::remove_dir_all(&local);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `sync()` is idempotent and safe to call after an automatic sync.
    #[test]
    fn sync_is_idempotent() {
        let root = unique_dir("sync");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        repo.sync().unwrap();
        repo.sync().unwrap();
        assert_eq!(repo.get_all_skill_names().unwrap(), vec!["one".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// SKILLS-04: `ensureRepositorySynced` is single-flight, like Java's `synchronized`.
    ///
    /// Eight threads open the same never-synced repository at once; without the sync lock they
    /// all see an empty directory and race `remove_dir_all` + `git clone` into it (failed or
    /// half-written work tree), and the clone counter goes above one.
    #[test]
    fn concurrent_first_reads_synchronise_once() {
        let root = unique_dir("concurrent");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/one", "one", "first");
        commit_all(&remote, "initial");

        let repo = std::sync::Arc::new(repo_for(&file_url(&remote), &root.join("clone")));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let repo = std::sync::Arc::clone(&repo);
                std::thread::spawn(move || repo.get_all_skill_names())
            })
            .collect();
        for handle in handles {
            let names = handle
                .join()
                .expect("worker thread")
                .expect("read succeeds");
            assert_eq!(names, vec!["one".to_string()]);
        }
        assert_eq!(repo.clone_count(), 1, "the clone must run once");

        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------
    // SKILLS-06: per-name reads must not load the whole repository
    // ------------------------------------------------------------------

    /// SKILLS-06: `get_skill` loads only the resolved skill directory, so a sibling skill's
    /// resources neither leak into the result nor have to be read.
    #[test]
    fn get_skill_loads_only_the_target_skill_directory() {
        let root = unique_dir("single-skill");
        let remote = root.join("remote");
        init_repo(&remote);

        let target = remote.join("skills/target");
        std::fs::create_dir_all(target.join("ref")).unwrap();
        std::fs::write(
            target.join("SKILL.md"),
            "---\nname: target\ndescription: Target\n---\n\n# Target\n",
        )
        .unwrap();
        std::fs::write(target.join("ref/own.md"), "own").unwrap();

        // The sibling carries resources a whole-repository load would have read.
        let sibling = remote.join("skills/sibling");
        std::fs::create_dir_all(sibling.join("ref")).unwrap();
        std::fs::write(
            sibling.join("SKILL.md"),
            "---\nname: sibling\ndescription: Sibling\n---\n\n# Sibling\n",
        )
        .unwrap();
        std::fs::write(sibling.join("ref/sibling-only.md"), "sibling").unwrap();
        std::fs::write(sibling.join("bulk.txt"), "x".repeat(256 * 1024)).unwrap();
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        let skill = repo.get_skill("target").unwrap();
        assert_eq!(skill.name(), "target");
        let keys: Vec<&str> = skill.resources().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["SKILL.md", "ref/own.md"]);
        assert!(!skill.resources().contains_key("ref/sibling-only.md"));
        assert!(!skill.resources().contains_key("bulk.txt"));

        // The sibling is still reachable on its own.
        let sibling_skill = repo.get_skill("sibling").unwrap();
        assert!(sibling_skill
            .resources()
            .contains_key("ref/sibling-only.md"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// SKILLS-06: `get_all_skill_names` parses each `SKILL.md` frontmatter and never loads a
    /// resource, so a huge or non-UTF8 resource changes nothing - and the cheap path reports
    /// exactly the set the bulk path reports.
    #[test]
    fn get_all_skill_names_reads_only_frontmatter() {
        let root = unique_dir("names-only");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/plain-dir", "plain-name", "P");

        let heavy = remote.join("skills/heavy-dir");
        std::fs::create_dir_all(heavy.join("assets")).unwrap();
        std::fs::write(
            heavy.join("SKILL.md"),
            "---\nname: heavy-name\n---\n# Heavy\n",
        )
        .unwrap();
        std::fs::write(heavy.join("assets/huge.txt"), "x".repeat(4 * 1024 * 1024)).unwrap();
        std::fs::write(heavy.join("assets/blob.bin"), [0xff_u8, 0xfe, 0x00, 0x01]).unwrap();
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        let names = repo.get_all_skill_names().unwrap();
        assert_eq!(
            names,
            vec!["heavy-name".to_string(), "plain-name".to_string()]
        );
        // Parity with the bulk read: the name-only path must not lose or invent a skill.
        assert_eq!(
            names,
            repo.get_all_skills()
                .unwrap()
                .iter()
                .map(|s| s.name().to_string())
                .collect::<Vec<_>>()
        );
        // The single-skill path still sees the resources it owns.
        let heavy_skill = repo.get_skill("heavy-name").unwrap();
        assert!(heavy_skill.resources().contains_key("assets/huge.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// SKILLS-06: the name-only scan applies the same directory filter and frontmatter identity
    /// as the bulk scan.
    #[test]
    fn skill_names_in_dir_skips_hidden_drafts_and_entries_without_skill_md() {
        let root = unique_dir("names-unit");
        write_skill(&root, "dir-alpha", "alpha-skill", "a");
        write_skill(&root, "brvo", "zed-skill", "z");
        write_skill(&root, ".hidden", "hidden-skill", "h");
        write_skill(&root, "_drafts", "draft-skill", "d");
        std::fs::create_dir_all(root.join("no-skill")).unwrap();
        std::fs::write(root.join("no-skill/README.md"), "no SKILL.md").unwrap();
        std::fs::write(root.join("stray.md"), "a file, not a directory").unwrap();

        assert_eq!(
            skill_names_in_dir(&root),
            vec!["alpha-skill".to_string(), "zed-skill".to_string()]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// SKILLS-06 parity: hidden directories are not skills for the bulk scan, so the per-name
    /// reads must not report them either (and a draft nested under `_drafts` is never a skill).
    #[test]
    fn hidden_and_draft_directories_are_not_reported_as_skills() {
        let root = unique_dir("drafts");
        let remote = root.join("remote");
        init_repo(&remote);
        write_skill(&remote, "skills/visible", "visible", "v");
        write_skill(&remote, "skills/_drafts/pending", "pending", "p");
        write_skill(&remote, "skills/.hidden-skill", "hidden-skill", "h");
        commit_all(&remote, "initial");

        let repo = repo_for(&file_url(&remote), &root.join("clone"));
        assert_eq!(
            repo.get_all_skill_names().unwrap(),
            vec!["visible".to_string()]
        );
        assert_eq!(repo.get_all_skills().unwrap().len(), 1);
        for name in ["pending", "hidden-skill"] {
            assert!(
                matches!(repo.get_skill(name), Err(SkillRepositoryError::NotFound(_))),
                "`{name}` must not be loadable by name"
            );
            assert!(!repo.skill_exists(name).unwrap());
        }
        assert!(repo.skill_dir("hidden-skill").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------
    // SKILLS-11: real assembly path (`HarnessAgentBuilder::skill_repository`)
    // ------------------------------------------------------------------

    /// The adapter was unit-tested in isolation only: no case proved that a repository handed to
    /// `HarnessAgentBuilder::skill_repository` survives `build()` and still answers `load()` /
    /// `skill_dir()` there ("tests green != production path correct"). This walks the real entry
    /// point: a `file://` git repository -> `RepoBackedSkillRepository` -> builder -> composed
    /// repository list.
    #[test]
    fn adapter_is_mounted_through_the_harness_assembly_path() {
        use std::sync::Arc;

        use agentscope_harness::agent::skill::SkillRepository;
        use agentscope_harness::agent::HarnessAgent;

        let root = unique_dir("assembly");
        let remote = root.join("remote");
        init_repo(&remote);
        // Directory name differs from the frontmatter name so the identity is observable.
        write_skill(&remote, "calc-dir", "calc-skill", "A calculator");
        commit_all(&remote, "initial");

        let git = Arc::new(repo_for(&file_url(&remote), &root.join("clone")));
        let adapter: Arc<dyn SkillRepository> =
            Arc::new(crate::repository::RepoBackedSkillRepository::new(git, None));
        let repo_id = adapter.repo_id();

        let cwd = root.join("cwd");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let agent = HarnessAgent::builder()
            .cwd(cwd)
            .workspace(workspace)
            .skill_repository(Arc::clone(&adapter))
            .build();

        let mounted = agent
            .skill_repositories()
            .iter()
            .find(|repo| repo.repo_id() == repo_id)
            .expect("the builder must mount the extension repository");
        assert!(
            Arc::ptr_eq(mounted, &adapter),
            "the builder must mount the caller's instance, not a copy"
        );

        let skills = mounted.load();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "calc-skill");
        assert_eq!(skills[0].description(), "A calculator");
        let dir = mounted
            .skill_dir("calc-skill")
            .expect("git repositories expose the checkout directory");
        assert!(dir.join("SKILL.md").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }
}
