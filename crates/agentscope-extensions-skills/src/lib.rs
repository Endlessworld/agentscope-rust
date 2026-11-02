//! agentscope-extensions-skills — skill repository backends (git / MySQL / PostgreSQL).
//!
//! Ports the Java `agentscope-extensions-skills` module (submodules
//! `skill-git-repository`, `skill-mysql-repository`, `skill-postgresql-repository`).
//!
//! ## Relationship with the harness `skill/` code (adjudicated)
//!
//! The harness already ships a directory-backed `agentscope_harness::agent::skill::SkillRepository`
//! plus the reusable `agentscope_harness::agent::skill::AgentSkill` model. This crate
//! **does not** re-invent either:
//!
//! * the storage model is always `AgentSkill` / `Metadata` / `SkillResources` from the harness;
//! * this crate defines the *backend* abstraction [`AgentSkillRepository`] (Java's
//!   `io.agentscope.core.skill.repository.AgentSkillRepository`): keyed CRUD over a
//!   git / SQL backend;
//! * [`RepoBackedSkillRepository`] adapts any [`AgentSkillRepository`] into the harness
//!   `SkillRepository` trait so it can be plugged into `HarnessAgent`'s ordered repository list.
//!
//! See `docs/extensions/SKILLS__GAP_TRACKER.md` for the full mapping.

pub mod error;
pub mod repository;
pub mod sql;
pub mod util;

pub mod git;
pub mod mysql;
pub mod postgresql;

/// Real (feature-gated) `SqlDatabase` drivers. Empty unless `postgres-driver` / `mysql-driver`
/// is enabled, so the default build pulls in no driver dependency.
#[cfg(any(feature = "postgres-driver", feature = "mysql-driver"))]
pub mod driver;

pub use error::SkillRepositoryError;
pub use repository::{AgentSkillRepository, AgentSkillRepositoryInfo, RepoBackedSkillRepository};
pub use sql::{InMemorySqlDatabase, SqlConnection, SqlDatabase, SqlParam, SqlRow, SqlValue};

pub use git::{GitSkillRepository, GitSkillRepositoryConfig};
pub use mysql::{MysqlSkillRepository, MysqlSkillRepositoryConfig};
pub use postgresql::{PostgresSkillRepository, PostgresSkillRepositoryConfig};

#[cfg(feature = "mysql-driver")]
pub use driver::{MysqlSqlConnection, MysqlSqlDatabase};
#[cfg(feature = "postgres-driver")]
pub use driver::{PgSqlConnection, PgSqlDatabase};
