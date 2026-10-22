//! JDBC-backed skill repository, mirroring the Java
//! `io.agentscope.extensions.jdbc.skill` package.
//!
//! Per the extensions gap tracker §4.5 adjudication 2, the JDBC implementation lives in
//! this crate (it reuses the crate's [`crate::backend::JdbcExecutor`] and dialect layer);
//! `agentscope-extensions-skills` only supplies the [`AgentSkillRepository`] contract.

mod repository;

pub use repository::{
    validate_namespace, validate_resource_path, validate_skill_name, JdbcAgentSkillRepository,
    DEFAULT_NAMESPACE, MAX_RESOURCE_PATH_LENGTH, MAX_SKILL_NAME_LENGTH,
};

pub use agentscope_extensions_skills::{
    AgentSkillRepository, AgentSkillRepositoryInfo, SkillRepositoryError,
};
