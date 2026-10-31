//! The [`AgentSkillRepository`] backend contract and its harness adapter.
//!
//! Mirrors `io.agentscope.core.skill.repository.AgentSkillRepository` (+ `AgentSkillRepositoryInfo`).
//! The storage model is the harness [`AgentSkill`] — no parallel skill type is introduced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_harness::agent::skill::{AgentSkill, SkillRepo, SkillRepository};

use crate::error::SkillRepositoryError;

/// Metadata about a skill repository (`AgentSkillRepositoryInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkillRepositoryInfo {
    /// Repository type: `filesystem` / `git` / `mysql` / `postgresql` / ...
    pub repo_type: String,
    /// Repository location (URL / DSN / table descriptor).
    pub location: String,
    /// Whether write operations are supported.
    pub writable: bool,
}

impl AgentSkillRepositoryInfo {
    /// Creates a new repository info descriptor.
    pub fn new(repo_type: impl Into<String>, location: impl Into<String>, writable: bool) -> Self {
        Self {
            repo_type: repo_type.into(),
            location: location.into(),
            writable,
        }
    }
}

impl std::fmt::Display for AgentSkillRepositoryInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AgentSkillRepositoryInfo{{type='{}', location='{}', writable={}}}",
            self.repo_type, self.location, self.writable
        )
    }
}

/// A source of skills with keyed CRUD, mirroring the Java `AgentSkillRepository`.
///
/// Implementations are synchronous because the underlying backends (filesystem, git,
/// blocking SQL drivers) are synchronous; callers on an async runtime can wrap them in
/// `tokio::task::spawn_blocking`. This matches the harness [`SkillRepository`], which
/// is likewise synchronous.
pub trait AgentSkillRepository: Send + Sync {
    /// Gets a skill by name, or [`SkillRepositoryError::NotFound`].
    fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError>;

    /// Lists all skill names (ordered by name).
    fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError>;

    /// Gets all skills (ordered by name).
    fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError>;

    /// Saves or updates skills.
    ///
    /// When `force` is `false` and any skill already exists, the whole save is rejected
    /// with [`SkillRepositoryError::Conflict`] (matching the Java pre-check + transaction).
    /// Returns `false` when `skills` is empty or the repository is read-only.
    fn save(&self, skills: &[AgentSkill], force: bool) -> Result<bool, SkillRepositoryError>;

    /// Deletes a skill by name. Returns `true` when something was deleted.
    fn delete(&self, skill_name: &str) -> Result<bool, SkillRepositoryError>;

    /// Checks whether a skill exists.
    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError>;

    /// Repository metadata.
    fn get_repository_info(&self) -> AgentSkillRepositoryInfo;

    /// The repository's skill `source` string.
    fn get_source(&self) -> String;

    /// The on-disk directory backing a skill, when the backend stores skills as directories.
    ///
    /// Mirrors Java `AgentSkill#getOriginDir()`: the harness `load_skill_through_path` tool reads
    /// files from this directory, so directory-backed backends (git) must expose it. In-memory
    /// backends (SQL) return `None`; [`RepoBackedSkillRepository`] materialises their resources
    /// into a cache directory instead.
    fn skill_dir(&self, skill_name: &str) -> Option<PathBuf> {
        let _ = skill_name;
        None
    }

    /// Whether write operations are supported.
    fn is_writeable(&self) -> bool;

    /// Enables or disables write operations at runtime.
    ///
    /// Mirrors Java `AgentSkillRepository#setWriteable(boolean)`. Implementations store the
    /// flag with interior mutability so callers can toggle it through `&self`; the concrete
    /// SQL backends honour it in `save` / `delete` / `clear_all`, while read-only backends
    /// (git) ignore it exactly as Java's `GitSkillRepository` does.
    fn set_writeable(&self, writeable: bool);

    /// Releases any resources held by the repository. Default: no-op.
    fn close(&self) -> Result<(), SkillRepositoryError> {
        Ok(())
    }
}

/// Adapts an [`AgentSkillRepository`] to the harness [`SkillRepository`] trait.
///
/// This is the single bridge between the extension backend abstraction and the harness
/// consumption path (`compose_skill_repositories` / `HarnessAgent`), so extension
/// repositories can be mounted without duplicating the harness loader.
pub struct RepoBackedSkillRepository {
    inner: Arc<dyn AgentSkillRepository>,
    kind: SkillRepo,
    id: String,
    /// Lazily created cache root used to materialise in-memory (SQL) skills on disk.
    cache_root: OnceLock<Option<PathBuf>>,
    /// Skill name -> materialised directory (SQL backends only).
    materialized: Mutex<HashMap<String, PathBuf>>,
}

impl RepoBackedSkillRepository {
    /// Wraps a backend repository.
    ///
    /// `repo_id` is the stable identifier reported to the harness; when `None`, a value is
    /// derived from the repository source. `Workspace`-kind is never inferred — extension
    /// repositories are always `External { source }`.
    pub fn new(inner: Arc<dyn AgentSkillRepository>, repo_id: Option<String>) -> Self {
        let source = inner.get_source();
        let kind = SkillRepo::External {
            source: source.clone(),
        };
        let id = repo_id.unwrap_or_else(|| {
            if source.is_empty() {
                "ext-repo".to_string()
            } else {
                format!("ext:{source}")
            }
        });
        Self {
            inner,
            kind,
            id,
            cache_root: OnceLock::new(),
            materialized: Mutex::new(HashMap::new()),
        }
    }

    /// Cache root for materialised in-memory skills (`None` when the temp dir is unusable).
    fn cache_root(&self) -> Option<&Path> {
        self.cache_root
            .get_or_init(|| {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let dir = std::env::temp_dir().join(format!(
                    "agentscope-skill-cache-{}-{nanos}",
                    std::process::id()
                ));
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => Some(dir),
                    Err(e) => {
                        tracing::warn!(dir = %dir.display(), error = %e, "cannot create the skill cache dir");
                        None
                    }
                }
            })
            .as_deref()
    }

    /// Writes a backend skill's resources to a cache directory so the harness load tool can read
    /// them (`AgentSkill` has no `originDir`, so in-memory skills need this hop).
    ///
    /// Two callers racing on the same skill would interleave `create_dir_all` / `File::write` and
    /// could serve a half-written tree, so the write runs under a **fixed-size stripe lock**
    /// (256 stripes, `hash(name) % 256`): the same skill always maps to the same stripe, while
    /// the lock table stays a few KB no matter how many skills are materialised. This is the
    /// Rust counterpart of Java `SkillBox#fileLockFor` (`de2dc8f3c`), which replaced an unbounded
    /// per-path `ConcurrentHashMap` of locks with the same 256-stripe table. The materialised
    /// map itself is keyed by **skill name** (bounded by the repository size) and entries are
    /// dropped on write/delete, so it cannot grow without bound.
    fn materialize_skill(&self, name: &str) -> Option<PathBuf> {
        if let Some(dir) = self.materialized.lock().unwrap().get(name) {
            if dir.is_dir() {
                return Some(dir.clone());
            }
        }
        let stripe = materialize_lock_for(name);
        // A poisoned stripe only means a previous materialisation panicked; the lock itself is
        // still usable, so recover instead of propagating the panic.
        let _guard = stripe.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Double-check: another thread may have finished while this one waited on the stripe.
        if let Some(dir) = self.materialized.lock().unwrap().get(name) {
            if dir.is_dir() {
                return Some(dir.clone());
            }
        }
        let skill = match self.inner.get_skill(name) {
            Ok(skill) => skill,
            Err(e) => {
                tracing::debug!(skill = name, error = %e, "cannot materialize an unknown skill");
                return None;
            }
        };
        let root = self.cache_root()?;
        let dir = root.join(sanitize_component(name));
        if let Err(e) = write_resources_to_dir(&dir, &skill) {
            tracing::warn!(skill = name, dir = %dir.display(), error = %e, "failed to materialize skill resources");
            return None;
        }
        self.materialized
            .lock()
            .unwrap()
            .insert(name.to_string(), dir.clone());
        Some(dir)
    }

    /// Drops a materialised cache entry (called after a write so the load tool re-reads).
    fn forget_materialized(&self, name: &str) {
        if let Some(dir) = self.materialized.lock().unwrap().remove(name) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Like the harness [`SkillRepository::load`], but keeps the backend error.
    ///
    /// The harness trait returns `Vec<AgentSkill>` (no error channel), so [`Self::load`] can only
    /// log a failure. Callers that must distinguish "this repository has no skills" from "the
    /// backend is broken" use this method instead of guessing from an empty vector.
    pub fn try_load(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        self.inner.get_all_skills()
    }
}

impl SkillRepository for RepoBackedSkillRepository {
    fn repo_id(&self) -> String {
        self.id.clone()
    }

    fn source(&self) -> String {
        self.inner.get_source()
    }

    fn is_writeable(&self) -> bool {
        self.inner.is_writeable()
    }

    fn repo_kind(&self) -> SkillRepo {
        self.kind.clone()
    }

    fn load(&self) -> Vec<AgentSkill> {
        // SKILLS-01: the harness trait has no error channel, so a backend failure used to be
        // swallowed into an empty list - indistinguishable from "this repository is empty". A
        // real agent mounting a MySQL/PostgreSQL repository then ran with zero skills and no
        // hint that anything was wrong. Warn loudly (and keep the error reachable through
        // `try_load`) instead of serving a fake empty table.
        match self.try_load() {
            Ok(skills) => skills,
            Err(e) => {
                tracing::warn!(
                    repo = %self.id,
                    source = %self.inner.get_source(),
                    error = %e,
                    "skill repository failed to load; serving no skills from this repository"
                );
                Vec::new()
            }
        }
    }

    /// Exposes the skill directory so `load_skill_through_path` works for extension backends.
    ///
    /// Directory-backed backends (git) return their real path; in-memory backends (SQL) get their
    /// resources materialised into a cache directory on first use.
    fn skill_dir(&self, name: &str) -> Option<PathBuf> {
        self.inner
            .skill_dir(name)
            .or_else(|| self.materialize_skill(name))
    }

    fn write_skill(&self, skill: &AgentSkill) -> std::io::Result<()> {
        if !self.inner.is_writeable() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "repository is read-only",
            ));
        }
        self.inner
            .save(std::slice::from_ref(skill), true)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Drop the materialised copy so the load tool re-reads the fresh resources.
        self.forget_materialized(skill.name());
        Ok(())
    }

    fn delete_skill(&self, name: &str) -> std::io::Result<()> {
        if !self.inner.is_writeable() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "repository is read-only",
            ));
        }
        self.inner
            .delete(name)
            .map(|_| ())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.forget_materialized(name);
        Ok(())
    }
}

/// Number of lock stripes guarding concurrent skill materialisation.
///
/// Mirrors Java `SkillBox.FILE_LOCK_STRIPES`: a fixed table means concurrent writers to the
/// same skill are serialised while the memory used by locks does not grow with the number of
/// materialised skills (an unrelated collision only serialises one extra write).
pub(crate) const MATERIALIZE_LOCK_STRIPES: usize = 256;

/// The stripe index a skill name maps to (`hash % 256`, stable within the process).
fn materialize_stripe(name: &str) -> usize {
    // FNV-1a: cheap, allocation-free and stable for a given name.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % MATERIALIZE_LOCK_STRIPES as u64) as usize
}

/// Returns the fixed-size stripe lock guarding writes for `name`.
fn materialize_lock_for(name: &str) -> &'static Mutex<()> {
    static STRIPES: OnceLock<Vec<Mutex<()>>> = OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..MATERIALIZE_LOCK_STRIPES)
            .map(|_| Mutex::new(()))
            .collect()
    });
    &stripes[materialize_stripe(name)]
}

/// Makes a skill name safe to use as a single path component.
fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim().to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Writes every resource of `skill` under `dir`, decoding `base64:` payloads.
fn write_resources_to_dir(dir: &Path, skill: &AgentSkill) -> std::io::Result<()> {
    use base64::Engine as _;
    std::fs::create_dir_all(dir)?;
    for (rel, content) in skill.resources() {
        let rel = rel.replace('\\', "/");
        if rel.trim().is_empty() || rel.starts_with('/') || rel.split('/').any(|s| s == "..") {
            continue;
        }
        let target = dir.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = match content.strip_prefix("base64:") {
            Some(rest) => base64::engine::general_purpose::STANDARD
                .decode(rest)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            None => content.as_bytes().to_vec(),
        };
        std::fs::write(&target, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::skill::SkillRepository;

    struct DummyRepo {
        skills: Vec<AgentSkill>,
        source: String,
        dir: Option<PathBuf>,
        /// When `true`, `get_all_skills` reports a backend failure, standing in for a broken
        /// SQL backend (SKILLS-01).
        fail_load: bool,
    }

    impl AgentSkillRepository for DummyRepo {
        fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError> {
            self.skills
                .iter()
                .find(|s| s.name() == name)
                .cloned()
                .ok_or_else(|| SkillRepositoryError::NotFound(name.to_string()))
        }
        fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
            Ok(self.skills.iter().map(|s| s.name().to_string()).collect())
        }
        fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
            if self.fail_load {
                // SKILLS-01: a driver / transport failure, not an empty repository.
                return Err(SkillRepositoryError::Database(
                    "connection lost while listing skills".to_string(),
                ));
            }
            Ok(self.skills.clone())
        }
        fn save(&self, _s: &[AgentSkill], _f: bool) -> Result<bool, SkillRepositoryError> {
            Err(SkillRepositoryError::ReadOnly)
        }
        fn delete(&self, _n: &str) -> Result<bool, SkillRepositoryError> {
            Err(SkillRepositoryError::ReadOnly)
        }
        fn skill_exists(&self, n: &str) -> Result<bool, SkillRepositoryError> {
            Ok(self.skills.iter().any(|s| s.name() == n))
        }
        fn get_repository_info(&self) -> AgentSkillRepositoryInfo {
            AgentSkillRepositoryInfo::new("dummy", "/tmp", false)
        }
        fn get_source(&self) -> String {
            self.source.clone()
        }
        fn is_writeable(&self) -> bool {
            false
        }
        fn set_writeable(&self, _writeable: bool) {}
        fn skill_dir(&self, _skill_name: &str) -> Option<PathBuf> {
            self.dir.clone()
        }
    }

    fn skill(name: &str) -> AgentSkill {
        AgentSkill::builder()
            .name(name)
            .source("git-demo")
            .description("d")
            .build()
    }

    #[test]
    fn adapter_exposes_skills_and_source() {
        let repo = DummyRepo {
            skills: vec![skill("alpha"), skill("beta")],
            source: "git-demo".to_string(),
            dir: None,
            fail_load: false,
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), None);
        assert_eq!(adapter.source(), "git-demo");
        assert_eq!(adapter.repo_id(), "ext:git-demo");
        assert!(!adapter.is_writeable());
        assert_eq!(adapter.load().len(), 2);
        assert!(matches!(adapter.repo_kind(), SkillRepo::External { .. }));
    }

    #[test]
    fn adapter_rejects_writes_when_read_only() {
        let repo = DummyRepo {
            skills: vec![],
            source: "s".to_string(),
            dir: None,
            fail_load: false,
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), Some("id".into()));
        assert_eq!(adapter.repo_id(), "id");
        assert!(adapter.write_skill(&skill("x")).is_err());
    }

    /// P1-08: a directory-backed backend's path must reach the harness load tool unchanged.
    #[test]
    fn adapter_forwards_backend_skill_dir() {
        let dir = std::env::temp_dir().join(format!(
            "asr-adapter-dir-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let repo = DummyRepo {
            skills: vec![skill("alpha")],
            source: "git-demo".to_string(),
            dir: Some(dir.clone()),
            fail_load: false,
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), None);
        // Both `AgentSkillRepository` and the harness `SkillRepository` expose `skill_dir`, so
        // disambiguate through the harness trait (the one the harness consumes).
        assert_eq!(SkillRepository::skill_dir(&adapter, "alpha"), Some(dir));
        // No cache directory is created when the backend already owns a directory.
        assert!(adapter.cache_root.get().is_none());
    }

    /// P1-08: in-memory (SQL) skills have no directory, so the adapter materialises their
    /// resources into a cache dir that `load_skill_through_path` can read.
    #[test]
    fn adapter_materialises_in_memory_skills() {
        let stored = AgentSkill::builder()
            .name("calc")
            .source("mysql_agentscope_agentscope_skills")
            .description("d")
            .add_resource("SKILL.md", "---\nname: calc\n---\nbody")
            .add_resource("refs/table.csv", "a,b\n1,2\n")
            .build();
        let repo = DummyRepo {
            skills: vec![stored],
            source: "mysql".to_string(),
            dir: None,
            fail_load: false,
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), None);
        let dir = SkillRepository::skill_dir(&adapter, "calc").expect("materialised skill dir");
        assert!(dir.is_dir());
        let md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(md.contains("name: calc"));
        let csv = std::fs::read_to_string(dir.join("refs/table.csv")).unwrap();
        assert_eq!(csv, "a,b\n1,2\n");
        // Repeated lookups are stable and unknown skills yield no directory.
        assert_eq!(
            SkillRepository::skill_dir(&adapter, "calc"),
            Some(dir.clone())
        );
        assert_eq!(SkillRepository::skill_dir(&adapter, "missing"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `de2dc8f3c`: the materialisation lock table is a fixed 256 stripes - bounded memory,
    /// and the same skill always maps to the same stripe so writers stay mutually excluded.
    #[test]
    fn materialize_locks_are_a_fixed_stripe_table() {
        assert_eq!(MATERIALIZE_LOCK_STRIPES, 256);
        assert_eq!(materialize_stripe("calc"), materialize_stripe("calc"));
        assert!(materialize_stripe("calc") < MATERIALIZE_LOCK_STRIPES);
        // The same name always yields the same lock object …
        assert!(std::ptr::eq(
            materialize_lock_for("calc"),
            materialize_lock_for("calc")
        ));
        // … and the table does not grow with the number of skills seen.
        let before = materialize_lock_for("calc") as *const Mutex<()>;
        for index in 0..1000 {
            let _ = materialize_lock_for(&format!("skill-{index}"));
        }
        assert_eq!(materialize_lock_for("calc") as *const Mutex<()>, before);
    }

    /// Concurrent first reads of the same skill must observe a complete resource tree: the
    /// stripe lock serialises the write phase (without it the two writers interleave).
    #[test]
    fn concurrent_materialisation_of_one_skill_is_serialised() {
        let stored = AgentSkill::builder()
            .name("calc")
            .source("postgresql")
            .description("d")
            .add_resource("SKILL.md", "---\nname: calc\n---\nbody that is long enough to split")
            .add_resource("refs/a.txt", &"x".repeat(4096))
            .add_resource("refs/b.txt", &"y".repeat(4096))
            .build();
        let repo = Arc::new(RepoBackedSkillRepository::new(
            Arc::new(DummyRepo {
                skills: vec![stored],
                source: "postgresql".to_string(),
                dir: None,
                fail_load: false,
            }),
            None,
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let repo = Arc::clone(&repo);
            handles.push(std::thread::spawn(move || {
                SkillRepository::skill_dir(&*repo, "calc")
            }));
        }
        let dirs: Vec<PathBuf> = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker panicked").expect("dir"))
            .collect();
        let first = dirs[0].clone();
        assert!(dirs.iter().all(|dir| dir == &first));
        assert_eq!(
            std::fs::read_to_string(first.join("refs/a.txt")).unwrap().len(),
            4096
        );
        let _ = std::fs::remove_dir_all(first);
    }

    /// `de2dc8f3c` (signature half): the Rust port computes no per-skill signature - the
    /// harness stages resources through `SkillRepo`/`stager.rs` and the adapter is keyed by
    /// `(skill name, dir)`. This pins the reason the O(n²) resource-map copy has no analogue
    /// here: the only per-skill enumeration is `AgentSkill::resources()`, iterated once.
    #[test]
    fn adapter_does_not_carry_a_per_skill_signature() {
        let adapter = RepoBackedSkillRepository::new(
            Arc::new(DummyRepo {
                skills: vec![skill("alpha")],
                source: "git-demo".to_string(),
                dir: None,
                fail_load: false,
            }),
            None,
        );
        // Identity is the harness `repo_id` + skill name; no digest is cached per skill.
        assert_eq!(adapter.repo_id(), "ext:git-demo");
        assert!(adapter.materialized.lock().unwrap().is_empty());
    }

    /// SKILLS-01: a broken backend must not masquerade as an empty repository.
    ///
    /// The harness trait `load()` has no error channel, so it returns no skills - but it warns, and
    /// the real error stays reachable through `try_load()`. Before the fix the failure was
    /// swallowed with `unwrap_or_default()`, so a misconfigured MySQL / PostgreSQL repository
    /// looked exactly like an empty one and the agent silently ran without skills.
    #[test]
    fn adapter_keeps_backend_failures_reachable() {
        let repo = DummyRepo {
            skills: vec![],
            source: "mysql".to_string(),
            dir: None,
            fail_load: true,
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), None);
        assert!(adapter.load().is_empty());
        let error = match adapter.try_load() {
            Ok(skills) => panic!("expected a backend error, got {} skills", skills.len()),
            Err(e) => e,
        };
        assert!(
            error.to_string().contains("connection lost while listing skills"),
            "unexpected error: {error}"
        );
        assert!(matches!(error, SkillRepositoryError::Database(_)));
    }
}
