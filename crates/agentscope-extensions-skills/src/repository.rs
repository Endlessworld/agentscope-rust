//! The [`AgentSkillRepository`] backend contract and its harness adapter.
//!
//! Mirrors `io.agentscope.core.skill.repository.AgentSkillRepository` (+ `AgentSkillRepositoryInfo`).
//! The storage model is the harness [`AgentSkill`] — no parallel skill type is introduced.

use std::sync::Arc;

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
    pub fn new(
        repo_type: impl Into<String>,
        location: impl Into<String>,
        writable: bool,
    ) -> Self {
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
/// [`tokio::task::spawn_blocking`]. This matches the harness [`SkillRepository`], which
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

    /// Whether write operations are supported.
    fn is_writeable(&self) -> bool;

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
        Self { inner, kind, id }
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
        // Best-effort: the harness loader treats a failed repository as empty.
        self.inner.get_all_skills().unwrap_or_default()
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
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::skill::SkillRepository;

    struct DummyRepo {
        skills: Vec<AgentSkill>,
        source: String,
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
        };
        let adapter = RepoBackedSkillRepository::new(Arc::new(repo), Some("id".into()));
        assert_eq!(adapter.repo_id(), "id");
        assert!(adapter.write_skill(&skill("x")).is_err());
    }
}