//! PostgreSQL-backed skill repository.
//!
//! Ports `io.agentscope.core.skill.repository.postgresql.PostgresSkillRepository`. Skills and
//! resources live in `agentscope_skills` / `agentscope_skill_resources` inside a configurable
//! **schema** (`agentscope` by default). Identifiers are double-quoted and validated; the
//! length limit is 63 (the PostgreSQL `NAMEDATALEN - 1` limit), versus MySQL's 64.
//!
//! Like the MySQL port, the repository auto-creates the schema/tables when requested and
//! probes for the optional `metadata_json` column so legacy schemas keep working.

use std::sync::Arc;

use agentscope_harness::agent::skill::model::{AgentSkill, SkillResources};

use crate::error::SkillRepositoryError;
use crate::repository::{AgentSkillRepository, AgentSkillRepositoryInfo};
use crate::sql::{SqlConnection, SqlDatabase, SqlParam};
use crate::util::{
    build_skill, has_extended_metadata, resolve_identifier, serialize_metadata,
    validate_identifier, validate_resource_path, validate_skill_name,
};

/// Default schema name.
pub const DEFAULT_SCHEMA_NAME: &str = "agentscope";
/// Default skills table name.
pub const DEFAULT_SKILLS_TABLE_NAME: &str = "agentscope_skills";
/// Default resources table name.
pub const DEFAULT_RESOURCES_TABLE_NAME: &str = "agentscope_skill_resources";

/// PostgreSQL identifier length limit.
const MAX_IDENTIFIER_LENGTH: usize = 63;

/// Maximum number of resource rows sent in one batch (`RESOURCE_BATCH_SIZE` in Java).
///
/// Java chunks `executeBatch` by this size to stay below the PostgreSQL bind-parameter limit
/// (65535 parameters / 3 parameters per row); the same chunking applies to the [`SqlConnection`]
/// batch seam.
pub const RESOURCE_BATCH_SIZE: usize = 1000;

/// Column types that can store the JSON metadata payload (`isSupportedMetadataJsonColumnType`).
/// `jsonb` is deliberately absent: writing a JSON *string* into `jsonb` would fail.
const SUPPORTED_METADATA_JSON_COLUMN_TYPES: [&str; 3] = ["text", "character varying", "character"];

/// Configuration for [`PostgresSkillRepository`].
#[derive(Debug, Clone)]
pub struct PostgresSkillRepositoryConfig {
    /// Schema name (validated identifier).
    pub schema_name: String,
    /// Skills table name (validated identifier).
    pub skills_table_name: String,
    /// Resources table name (validated identifier).
    pub resources_table_name: String,
    /// Whether to auto-create the schema/tables for new deployments.
    pub create_if_not_exist: bool,
    /// Whether write operations are allowed.
    pub writeable: bool,
}

impl Default for PostgresSkillRepositoryConfig {
    fn default() -> Self {
        Self {
            schema_name: DEFAULT_SCHEMA_NAME.to_string(),
            skills_table_name: DEFAULT_SKILLS_TABLE_NAME.to_string(),
            resources_table_name: DEFAULT_RESOURCES_TABLE_NAME.to_string(),
            create_if_not_exist: true,
            writeable: true,
        }
    }
}

impl PostgresSkillRepositoryConfig {
    /// Creates a config with the default schema/table names.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the schema name.
    pub fn schema_name(mut self, name: impl Into<String>) -> Self {
        self.schema_name = name.into();
        self
    }

    /// Sets the skills table name.
    pub fn skills_table_name(mut self, name: impl Into<String>) -> Self {
        self.skills_table_name = name.into();
        self
    }

    /// Sets the resources table name.
    pub fn resources_table_name(mut self, name: impl Into<String>) -> Self {
        self.resources_table_name = name.into();
        self
    }

    /// Toggles auto-creation of the schema/tables.
    pub fn create_if_not_exist(mut self, create: bool) -> Self {
        self.create_if_not_exist = create;
        self
    }

    /// Toggles write support.
    pub fn writeable(mut self, writeable: bool) -> Self {
        self.writeable = writeable;
        self
    }
}

/// PostgreSQL-based [`AgentSkillRepository`] implementation.
///
/// **已废弃（仅兼容，勿扩展）**：Java 侧 `PostgresSkillRepository` 已标 `@Deprecated`，
/// 官方替代是按 namespace 分区的 `JdbcAgentSkillRepository`。按 `EXTENSIONS_GAP_TRACKER.md`
/// §4.5 裁定 2，Rust 侧在 `agentscope-extensions-jdbc` 实现该替代，本类型仅保留以兼容
/// 既有调用方；**不要在此新增功能**。
pub struct PostgresSkillRepository<D: SqlDatabase> {
    db: Arc<D>,
    cfg: PostgresSkillRepositoryConfig,
    metadata_json_supported: bool,
    /// Runtime write flag (Java PostgresSkillRepository#writeable), toggled by
    /// `set_writeable`.
    writeable: std::sync::atomic::AtomicBool,
}

impl<D: SqlDatabase> PostgresSkillRepository<D> {
    /// Builds a repository with the default config.
    pub fn new(db: Arc<D>) -> Result<Self, SkillRepositoryError> {
        Self::with_config(db, PostgresSkillRepositoryConfig::default())
    }

    /// Builds a repository with an explicit config, initializing the schema when requested.
    pub fn with_config(
        db: Arc<D>,
        cfg: PostgresSkillRepositoryConfig,
    ) -> Result<Self, SkillRepositoryError> {
        // Java: null / blank names fall back to the defaults, everything else is `trim()`ed
        // *before* validation (so `"  skills "` is accepted, `"bad-name"` is not). P2-02.
        let mut cfg = cfg;
        cfg.schema_name = resolve_identifier(&cfg.schema_name, DEFAULT_SCHEMA_NAME);
        cfg.skills_table_name =
            resolve_identifier(&cfg.skills_table_name, DEFAULT_SKILLS_TABLE_NAME);
        cfg.resources_table_name =
            resolve_identifier(&cfg.resources_table_name, DEFAULT_RESOURCES_TABLE_NAME);

        validate_identifier(&cfg.schema_name, "schema name", MAX_IDENTIFIER_LENGTH)?;
        validate_identifier(
            &cfg.skills_table_name,
            "skills table name",
            MAX_IDENTIFIER_LENGTH,
        )?;
        validate_identifier(
            &cfg.resources_table_name,
            "resources table name",
            MAX_IDENTIFIER_LENGTH,
        )?;

        let cfg_writeable = cfg.writeable;
        let mut repo = Self {
            db,
            cfg,
            metadata_json_supported: false,
            writeable: std::sync::atomic::AtomicBool::new(cfg_writeable),
        };
        if repo.cfg.create_if_not_exist {
            repo.create_schema_and_tables()?;
        }
        repo.metadata_json_supported = repo.detect_metadata_json_column_support();
        Ok(repo)
    }

    /// The schema name.
    pub fn schema_name(&self) -> &str {
        &self.cfg.schema_name
    }

    /// The skills table name.
    pub fn skills_table_name(&self) -> &str {
        &self.cfg.skills_table_name
    }

    /// The resources table name.
    pub fn resources_table_name(&self) -> &str {
        &self.cfg.resources_table_name
    }

    /// The live write flag; reflects any `AgentSkillRepository::set_writeable` call.
    fn writeable(&self) -> bool {
        self.writeable.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the current schema exposes a `metadata_json` column.
    pub fn metadata_json_column_supported(&self) -> bool {
        self.metadata_json_supported
    }

    /// Quotes an identifier with double quotes (after validation), mirroring `quoteIdentifier`.
    fn quote_identifier(&self, identifier: &str) -> String {
        format!("\"{identifier}\"")
    }

    /// The fully-qualified, quoted table name (`"schema"."table"`).
    fn full_table_name(&self, table: &str) -> String {
        format!(
            "{}.{}",
            self.quote_identifier(&self.cfg.schema_name),
            self.quote_identifier(table)
        )
    }

    fn create_schema_and_tables(&self) -> Result<(), SkillRepositoryError> {
        let create_schema = format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            self.quote_identifier(&self.cfg.schema_name)
        );
        let skills_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n\
             id BIGSERIAL PRIMARY KEY,\n\
             name VARCHAR(255) NOT NULL UNIQUE,\n\
             description TEXT NOT NULL,\n\
             skill_content TEXT NOT NULL,\n\
             source VARCHAR(255) NOT NULL,\n\
             metadata_json TEXT NULL,\n\
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,\n\
             updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP\n\
             )",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let resources_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n\
             id BIGINT NOT NULL,\n\
             resource_path VARCHAR(500) NOT NULL,\n\
             resource_content TEXT NOT NULL,\n\
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,\n\
             updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,\n\
             PRIMARY KEY (id, resource_path),\n\
             FOREIGN KEY (id) REFERENCES {}(id) ON DELETE CASCADE\n\
             )",
            self.full_table_name(&self.cfg.resources_table_name),
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let mut conn = self.db.connection()?;
        conn.execute(&create_schema, &[])?;
        conn.execute(&skills_sql, &[])?;
        conn.execute(&resources_sql, &[])?;
        Ok(())
    }

    /// Probes `INFORMATION_SCHEMA.COLUMNS` for a `metadata_json` column and verifies its type.
    fn detect_metadata_json_column_support(&self) -> bool {
        let sql = "SELECT DATA_TYPE FROM INFORMATION_SCHEMA.COLUMNS \
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = 'metadata_json'";
        let Ok(mut conn) = self.db.connection() else {
            return false;
        };
        let params = [
            SqlParam::Text(self.cfg.schema_name.clone()),
            SqlParam::Text(self.cfg.skills_table_name.clone()),
        ];
        let rows = match conn.query(sql, &params) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "failed to detect metadata_json support, using the legacy schema");
                return false;
            }
        };
        let Some(data_type) = rows.first().and_then(|row| {
            row.get_text("DATA_TYPE")
                .or_else(|| row.get_text("data_type"))
        }) else {
            return false;
        };
        let normalized = data_type.trim().to_ascii_lowercase();
        if SUPPORTED_METADATA_JSON_COLUMN_TYPES.contains(&normalized.as_str()) {
            return true;
        }
        tracing::warn!(
            schema = %self.cfg.schema_name,
            table = %self.cfg.skills_table_name,
            data_type = %data_type,
            "metadata_json column has an unsupported type; extended metadata will not be persisted"
        );
        false
    }

    fn resource_rows(
        &self,
        conn: &mut D::Connection,
        skill_id: i64,
    ) -> Result<SkillResources, SkillRepositoryError> {
        let sql = format!(
            "SELECT resource_path, resource_content FROM {} WHERE id = ?",
            self.full_table_name(&self.cfg.resources_table_name)
        );
        let rows = conn.query(&sql, &[SqlParam::Int(skill_id)])?;
        let mut resources = SkillResources::new();
        for row in rows {
            if let (Some(path), Some(content)) = (
                row.get_text("resource_path"),
                row.get_text("resource_content"),
            ) {
                resources.insert(path, content);
            }
        }
        Ok(resources)
    }

    fn delete_skill_internal(
        &self,
        conn: &mut D::Connection,
        skill_name: &str,
    ) -> Result<usize, SkillRepositoryError> {
        let find_sql = format!(
            "SELECT id FROM {} WHERE name = ?",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let rows = conn.query(&find_sql, &[SqlParam::Text(skill_name.to_string())])?;
        let Some(id) = rows.first().and_then(|r| r.get_int("id")) else {
            return Ok(0);
        };
        let del_res = format!(
            "DELETE FROM {} WHERE id = ?",
            self.full_table_name(&self.cfg.resources_table_name)
        );
        conn.execute(&del_res, &[SqlParam::Int(id)])?;
        let del_skill = format!(
            "DELETE FROM {} WHERE name = ?",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        conn.execute(&del_skill, &[SqlParam::Text(skill_name.to_string())])
    }

    fn skill_exists_internal(
        &self,
        conn: &mut D::Connection,
        skill_name: &str,
    ) -> Result<bool, SkillRepositoryError> {
        let sql = format!(
            "SELECT name FROM {} WHERE name = ?",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let rows = conn.query(&sql, &[SqlParam::Text(skill_name.to_string())])?;
        Ok(!rows.is_empty())
    }

    fn insert_skill(
        &self,
        conn: &mut D::Connection,
        skill: &AgentSkill,
    ) -> Result<i64, SkillRepositoryError> {
        if self.metadata_json_supported {
            let sql = format!(
                "INSERT INTO {} (name, description, skill_content, source, metadata_json) \
                 VALUES (?, ?, ?, ?, ?)",
                self.full_table_name(&self.cfg.skills_table_name)
            );
            let params = [
                SqlParam::Text(skill.name().to_string()),
                SqlParam::Text(skill.description().to_string()),
                SqlParam::Text(self.skill_content(skill)),
                SqlParam::Text(skill.source().to_string()),
                SqlParam::Text(serialize_metadata(skill.metadata())),
            ];
            conn.insert_returning_id(&sql, &params)
        } else {
            if has_extended_metadata(skill.metadata()) {
                tracing::warn!(
                    skill = skill.name(),
                    "metadata_json column absent; extended metadata will not be persisted"
                );
            }
            let sql = format!(
                "INSERT INTO {} (name, description, skill_content, source) VALUES (?, ?, ?, ?)",
                self.full_table_name(&self.cfg.skills_table_name)
            );
            let params = [
                SqlParam::Text(skill.name().to_string()),
                SqlParam::Text(skill.description().to_string()),
                SqlParam::Text(self.skill_content(skill)),
                SqlParam::Text(skill.source().to_string()),
            ];
            conn.insert_returning_id(&sql, &params)
        }
    }

    fn skill_content(&self, skill: &AgentSkill) -> String {
        skill
            .resources()
            .iter()
            .find(|(path, _)| path.ends_with("SKILL.md"))
            .map(|(_, content)| content.clone())
            .unwrap_or_default()
    }

    /// Inserts the resource rows, chunked by [`RESOURCE_BATCH_SIZE`] (`insertResources`).
    ///
    /// Java builds one `PreparedStatement` and calls `executeBatch` every 1000 paths so a skill
    /// with thousands of files never exceeds the bind-parameter limit; the same chunking is
    /// applied here on top of [`SqlConnection::execute_batch`].
    fn insert_resources(
        &self,
        conn: &mut D::Connection,
        skill_id: i64,
        resources: &SkillResources,
    ) -> Result<(), SkillRepositoryError> {
        if resources.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "INSERT INTO {} (id, resource_path, resource_content) VALUES (?, ?, ?)",
            self.full_table_name(&self.cfg.resources_table_name)
        );
        let mut batch: Vec<Vec<SqlParam>> = Vec::with_capacity(RESOURCE_BATCH_SIZE);
        let mut inserted = 0usize;
        for (path, content) in resources {
            batch.push(vec![
                SqlParam::Int(skill_id),
                SqlParam::Text(path.clone()),
                SqlParam::Text(content.clone()),
            ]);
            if batch.len() == RESOURCE_BATCH_SIZE {
                inserted += conn.execute_batch(&sql, &batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            inserted += conn.execute_batch(&sql, &batch)?;
        }
        if inserted < resources.len() {
            // Java throws when `executeBatch` reports fewer rows than the batch held.
            return Err(SkillRepositoryError::Database(format!(
                "failed to insert all resources for id '{skill_id}': expected {}, inserted {inserted}",
                resources.len()
            )));
        }
        Ok(())
    }

    /// Clears all skills (resources cascade). Returns the number of skills deleted.
    pub fn clear_all_skills(&self) -> Result<usize, SkillRepositoryError> {
        if !self.writeable() {
            tracing::warn!("cannot clear skills: repository is read-only");
            return Ok(0);
        }
        let sql = format!(
            "DELETE FROM {}",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let mut conn = self.db.connection()?;
        conn.execute(&sql, &[])
    }
}

impl<D: SqlDatabase> AgentSkillRepository for PostgresSkillRepository<D> {
    fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError> {
        validate_skill_name(name)?;
        let mut conn = self.db.connection()?;
        let cols = if self.metadata_json_supported {
            "id, name, description, skill_content, source, metadata_json"
        } else {
            "id, name, description, skill_content, source"
        };
        let sql = format!(
            "SELECT {cols} FROM {} WHERE name = ?",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let rows = conn.query(&sql, &[SqlParam::Text(name.to_string())])?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| SkillRepositoryError::NotFound(name.to_string()))?;
        let id = row.get_int("id").unwrap_or_default();
        let description = row.get_text("description").unwrap_or_default();
        let source = row.get_text("source").unwrap_or_default();
        let metadata_json = if self.metadata_json_supported {
            row.get_text("metadata_json")
        } else {
            None
        };
        let mut resources = self.resource_rows(&mut conn, id)?;
        let content = row.get_text("skill_content").unwrap_or_default();
        if !content.is_empty() && !resources.keys().any(|k| k.ends_with("SKILL.md")) {
            resources.insert("SKILL.md".to_string(), content);
        }
        Ok(build_skill(
            name,
            &description,
            resources,
            &source,
            metadata_json.as_deref(),
        ))
    }

    fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
        let sql = format!(
            "SELECT name FROM {} ORDER BY name",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let mut conn = self.db.connection()?;
        let rows = conn.query(&sql, &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.get_text("name"))
            .collect())
    }

    fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        let cols = if self.metadata_json_supported {
            "id, name, description, skill_content, source, metadata_json"
        } else {
            "id, name, description, skill_content, source"
        };
        let skills_sql = format!(
            "SELECT {cols} FROM {} ORDER BY name",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let resources_sql = format!(
            "SELECT id, resource_path, resource_content FROM {}",
            self.full_table_name(&self.cfg.resources_table_name)
        );
        let mut conn = self.db.connection()?;
        let skill_rows = conn.query(&skills_sql, &[])?;
        let resource_rows = conn.query(&resources_sql, &[])?;

        let mut by_id: std::collections::HashMap<i64, SkillResources> =
            std::collections::HashMap::new();
        for row in resource_rows {
            if let (Some(id), Some(path), Some(content)) = (
                row.get_int("id"),
                row.get_text("resource_path"),
                row.get_text("resource_content"),
            ) {
                by_id.entry(id).or_default().insert(path, content);
            }
        }

        let mut skills = Vec::with_capacity(skill_rows.len());
        for row in skill_rows {
            let id = row.get_int("id").unwrap_or_default();
            let name = row.get_text("name").unwrap_or_default();
            let description = row.get_text("description").unwrap_or_default();
            let source = row.get_text("source").unwrap_or_default();
            let metadata_json = if self.metadata_json_supported {
                row.get_text("metadata_json")
            } else {
                None
            };
            let mut resources = by_id.remove(&id).unwrap_or_default();
            let content = row.get_text("skill_content").unwrap_or_default();
            if !content.is_empty() && !resources.keys().any(|k| k.ends_with("SKILL.md")) {
                resources.insert("SKILL.md".to_string(), content);
            }
            skills.push(build_skill(
                &name,
                &description,
                resources,
                &source,
                metadata_json.as_deref(),
            ));
        }
        skills.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(skills)
    }

    fn save(&self, skills: &[AgentSkill], force: bool) -> Result<bool, SkillRepositoryError> {
        if skills.is_empty() {
            return Ok(false);
        }
        if !self.writeable() {
            tracing::warn!("cannot save skills: repository is read-only");
            return Ok(false);
        }
        for skill in skills {
            validate_skill_name(skill.name())?;
            for path in skill.resources().keys() {
                validate_resource_path(path)?;
            }
        }

        let mut conn = self.db.connection()?;
        if !force {
            let mut conflicts = Vec::new();
            for skill in skills {
                if self.skill_exists_internal(&mut conn, skill.name())? {
                    conflicts.push(skill.name().to_string());
                }
            }
            if !conflicts.is_empty() {
                return Err(SkillRepositoryError::Conflict(conflicts.join(", ")));
            }
        }

        // Java: `executeInWriteTransaction` around the whole write.
        conn.in_transaction(|conn| {
            for skill in skills {
                if self.skill_exists_internal(conn, skill.name())? {
                    self.delete_skill_internal(conn, skill.name())?;
                }
                let id = self.insert_skill(conn, skill)?;
                self.insert_resources(conn, id, skill.resources())?;
            }
            Ok(())
        })?;
        Ok(true)
    }

    fn delete(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        validate_skill_name(skill_name)?;
        if !self.writeable() {
            tracing::warn!("cannot delete skill: repository is read-only");
            return Ok(false);
        }
        let mut conn = self.db.connection()?;
        // Java: `skillExistsInternal` pre-check, then `executeInWriteTransaction(deleteSkillInternal)`.
        if !self.skill_exists_internal(&mut conn, skill_name)? {
            tracing::warn!(skill = skill_name, "skill does not exist");
            return Ok(false);
        }
        let deleted = conn.in_transaction(|conn| self.delete_skill_internal(conn, skill_name))?;
        Ok(deleted > 0)
    }

    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        // Java `skillExists`: a null / empty name is simply "not present", never an error (P2-08).
        // Java `skillExists`: a null / empty name is simply "not present", never an error (P2-08).
        if skill_name.trim().is_empty() {
            return Ok(false);
        }
        validate_skill_name(skill_name)?;
        let mut conn = self.db.connection()?;
        self.skill_exists_internal(&mut conn, skill_name)
    }

    fn get_repository_info(&self) -> AgentSkillRepositoryInfo {
        let location = format!("{}.{}", self.cfg.schema_name, self.cfg.skills_table_name);
        AgentSkillRepositoryInfo::new("postgresql", location, self.writeable())
    }

    fn get_source(&self) -> String {
        // Java: `"postgresql_" + schemaName + "_" + skillsTableName`.
        format!(
            "postgresql_{}_{}",
            self.cfg.schema_name, self.cfg.skills_table_name
        )
    }

    fn is_writeable(&self) -> bool {
        self.writeable()
    }

    fn set_writeable(&self, writeable: bool) {
        self.writeable
            .store(writeable, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::test_support::InterceptingSqlDatabase;
    use crate::sql::InMemorySqlDatabase;

    fn skill(name: &str, desc: &str) -> AgentSkill {
        AgentSkill::builder()
            .name(name)
            .source("custom")
            .description(desc)
            .add_resource("SKILL.md", "# body")
            .add_resource("r/data.json", "{}")
            .build()
    }

    fn repo() -> PostgresSkillRepository<InMemorySqlDatabase> {
        let db = Arc::new(InMemorySqlDatabase::new());
        PostgresSkillRepository::new(db).unwrap()
    }

    #[test]
    fn default_schema_and_tables() {
        let r = repo();
        assert_eq!(r.schema_name(), "agentscope");
        assert_eq!(r.skills_table_name(), "agentscope_skills");
        assert!(r.metadata_json_column_supported());
    }

    #[test]
    fn save_then_get_roundtrip() {
        let r = repo();
        r.save(&[skill("p", "pg skill")], false).unwrap();
        let loaded = r.get_skill("p").unwrap();
        assert_eq!(loaded.description(), "pg skill");
        assert_eq!(
            loaded.resources().get("r/data.json").map(|s| s.as_str()),
            Some("{}")
        );
    }

    #[test]
    fn save_without_force_conflicts() {
        let r = repo();
        r.save(&[skill("a", "1")], false).unwrap();
        assert!(matches!(
            r.save(&[skill("a", "2")], false).unwrap_err(),
            SkillRepositoryError::Conflict(_)
        ));
    }

    #[test]
    fn force_overwrite_and_delete() {
        let r = repo();
        r.save(&[skill("a", "1")], false).unwrap();
        r.save(&[skill("a", "2")], true).unwrap();
        assert_eq!(r.get_skill("a").unwrap().description(), "2");
        assert!(r.delete("a").unwrap());
        assert!(!r.skill_exists("a").unwrap());
    }

    #[test]
    fn identifier_validation_uses_pg_length_limit() {
        let long = "a".repeat(64);
        let db = Arc::new(InMemorySqlDatabase::new());
        let cfg = PostgresSkillRepositoryConfig::new().schema_name(long);
        assert!(PostgresSkillRepository::with_config(db, cfg).is_err());
    }

    #[test]
    fn repository_info_type_is_postgresql() {
        let info = repo().get_repository_info();
        assert_eq!(info.repo_type, "postgresql");
        assert_eq!(info.location, "agentscope.agentscope_skills");
    }

    // Java: getSource -> `postgresql_<schema>_<table>` (P1-10).
    #[test]
    fn get_source_includes_schema_and_table() {
        assert_eq!(
            repo().get_source(),
            "postgresql_agentscope_agentscope_skills"
        );
    }

    // Java: isSupportedMetadataJsonColumnType (P1-07).
    #[test]
    fn text_metadata_column_is_supported() {
        let db = Arc::new(InMemorySqlDatabase::new());
        db.connection()
            .unwrap()
            .execute(
                "CREATE TABLE agentscope_skills (id BIGINT, name VARCHAR(255), description TEXT, \
                 skill_content TEXT, source VARCHAR(255), metadata_json TEXT NULL)",
                &[],
            )
            .unwrap();
        let r = PostgresSkillRepository::with_config(
            db,
            PostgresSkillRepositoryConfig::new().create_if_not_exist(false),
        )
        .unwrap();
        assert!(r.metadata_json_column_supported());
    }

    #[test]
    fn jsonb_metadata_column_falls_back_to_the_legacy_schema() {
        let db = Arc::new(InMemorySqlDatabase::new());
        db.connection()
            .unwrap()
            .execute(
                "CREATE TABLE agentscope_skills (id BIGINT, name VARCHAR(255), description TEXT, \
                 skill_content TEXT, source VARCHAR(255), metadata_json jsonb)",
                &[],
            )
            .unwrap();
        let r = PostgresSkillRepository::with_config(
            db,
            PostgresSkillRepositoryConfig::new().create_if_not_exist(false),
        )
        .unwrap();
        assert!(!r.metadata_json_column_supported());
        // The legacy path still round-trips the core columns.
        r.save(&[skill("p", "pg skill")], false).unwrap();
        let loaded = r.get_skill("p").unwrap();
        assert_eq!(loaded.description(), "pg skill");
        assert_eq!(loaded.metadata().len(), 2);
    }

    #[test]
    fn read_only_repository_rejects_writes() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let r = PostgresSkillRepository::with_config(
            db,
            PostgresSkillRepositoryConfig::new().writeable(false),
        )
        .unwrap();
        assert!(!r.save(&[skill("a", "1")], true).unwrap());
        assert!(!r.delete("a").unwrap());
    }

    // ------------------------------------------------------------------
    // P2-02: blank / padded identifiers
    // ------------------------------------------------------------------

    /// Java: `(name == null || name.trim().isEmpty()) ? DEFAULT : name.trim()`.
    #[test]
    fn blank_names_fall_back_to_the_defaults() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let r = PostgresSkillRepository::with_config(
            db.clone(),
            PostgresSkillRepositoryConfig::new()
                .schema_name(" \t")
                .skills_table_name("")
                .resources_table_name("   "),
        )
        .unwrap();
        assert_eq!(r.schema_name(), DEFAULT_SCHEMA_NAME);
        assert_eq!(r.skills_table_name(), DEFAULT_SKILLS_TABLE_NAME);
        assert_eq!(r.resources_table_name(), DEFAULT_RESOURCES_TABLE_NAME);
        assert!(db.schema_exists(DEFAULT_SCHEMA_NAME));
        r.save(&[skill("a", "1")], false).unwrap();
        assert_eq!(r.get_skill("a").unwrap().description(), "1");
    }

    /// Java trims before validating, so padded-but-valid names are accepted.
    #[test]
    fn names_are_trimmed_before_validation() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let r = PostgresSkillRepository::with_config(
            db.clone(),
            PostgresSkillRepositoryConfig::new()
                .schema_name("  reporting  ")
                .skills_table_name(" skills_pg ")
                .resources_table_name("\tresources_pg\n"),
        )
        .unwrap();
        assert_eq!(r.schema_name(), "reporting");
        assert_eq!(r.skills_table_name(), "skills_pg");
        assert_eq!(r.resources_table_name(), "resources_pg");
        assert!(db.schema_exists("reporting"));
        assert!(db.table_exists("skills_pg"));
        r.save(&[skill("a", "padded")], false).unwrap();
        assert_eq!(r.get_skill("a").unwrap().description(), "padded");
    }

    /// Trimming must not turn an invalid identifier into a valid one.
    #[test]
    fn padded_invalid_names_are_still_rejected() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let err = match PostgresSkillRepository::with_config(
            db,
            PostgresSkillRepositoryConfig::new().schema_name("  bad-schema  "),
        ) {
            Ok(_) => panic!("an identifier with a dash must be rejected"),
            Err(e) => e,
        };
        assert!(matches!(err, SkillRepositoryError::InvalidArgument { .. }));
    }

    // ------------------------------------------------------------------
    // P2-03: delete() runs in a transaction
    // ------------------------------------------------------------------

    /// Java `executeInWriteTransaction`: the resource DELETE must be rolled back when the skill
    /// DELETE fails, so no orphaned resource rows are left behind.
    #[test]
    fn delete_rolls_back_the_resource_rows_when_the_skill_delete_fails() {
        let db = Arc::new(InterceptingSqlDatabase::failing("DELETE FROM", 1));
        let r = PostgresSkillRepository::new(db.clone()).unwrap();
        r.save(&[skill("a", "1")], false).unwrap();
        assert_eq!(db.inner().rows("agentscope_skill_resources").len(), 2);

        let err = r.delete("a").unwrap_err();
        assert!(
            matches!(err, SkillRepositoryError::Database(_)),
            "unexpected error: {err:?}"
        );
        assert_eq!(db.failures(), 1, "only the skill-row DELETE may fail");

        assert_eq!(db.inner().rows("agentscope_skill_resources").len(), 2);
        assert!(r.skill_exists("a").unwrap());
        let loaded = r.get_skill("a").unwrap();
        assert_eq!(
            loaded.resources().get("r/data.json").map(String::as_str),
            Some("{}")
        );
    }

    // ------------------------------------------------------------------
    // P2-04: RESOURCE_BATCH_SIZE chunking
    // ------------------------------------------------------------------

    /// Java `RESOURCE_BATCH_SIZE = 1000`: resources are sent in chunked batches, never one
    /// statement per row and never one oversized batch.
    #[test]
    fn resource_inserts_are_batched_by_resource_batch_size() {
        assert_eq!(RESOURCE_BATCH_SIZE, 1000);
        let db = Arc::new(InterceptingSqlDatabase::recording());
        let r = PostgresSkillRepository::new(db.clone()).unwrap();

        let mut builder = AgentSkill::builder()
            .name("big")
            .source("s")
            .description("many resources")
            .add_resource("SKILL.md", "# big");
        for i in 0..2500 {
            builder = builder.add_resource(format!("r/{i}.txt"), "x");
        }
        r.save(&[builder.build()], false).unwrap();

        // 2501 rows -> 1000 + 1000 + 501.
        assert_eq!(db.batch_sizes(), vec![1000, 1000, 501]);
        assert_eq!(r.get_skill("big").unwrap().resources().len(), 2501);
    }

    /// A single small skill still goes through one batch (not one statement per row).
    #[test]
    fn a_small_skill_is_inserted_in_one_batch() {
        let db = Arc::new(InterceptingSqlDatabase::recording());
        let r = PostgresSkillRepository::new(db.clone()).unwrap();
        r.save(&[skill("a", "1")], false).unwrap();
        assert_eq!(db.batch_sizes(), vec![2]);
    }

    // ------------------------------------------------------------------
    // P2-08: skill_exists on blank names
    // ------------------------------------------------------------------

    /// Java `skillExists(null / "")` returns `false` instead of raising.
    #[test]
    fn skill_exists_returns_false_for_blank_names() {
        let r = repo();
        assert!(!r.skill_exists("").unwrap());
        assert!(!r.skill_exists("   ").unwrap());
        assert!(r.skill_exists("../escape").is_err());
    }

    // ------------------------------------------------------------------
    // P1-09: namespace isolation
    // ------------------------------------------------------------------

    /// Two repositories on the same schema must not see each other's rows.
    #[test]
    fn two_repositories_with_different_tables_are_isolated() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let alpha = PostgresSkillRepository::with_config(
            db.clone(),
            PostgresSkillRepositoryConfig::new()
                .skills_table_name("skills_alpha")
                .resources_table_name("resources_alpha"),
        )
        .unwrap();
        let beta = PostgresSkillRepository::with_config(
            db,
            PostgresSkillRepositoryConfig::new()
                .skills_table_name("skills_beta")
                .resources_table_name("resources_beta"),
        )
        .unwrap();

        alpha.save(&[skill("alpha-one", "a")], false).unwrap();
        beta.save(&[skill("beta-one", "b")], false).unwrap();

        assert_eq!(
            alpha.get_all_skill_names().unwrap(),
            vec!["alpha-one".to_string()]
        );
        assert_eq!(
            beta.get_all_skill_names().unwrap(),
            vec!["beta-one".to_string()]
        );
        assert!(!beta.skill_exists("alpha-one").unwrap());
        assert_ne!(alpha.get_source(), beta.get_source());
    }
}
