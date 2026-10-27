//! MySQL-backed skill repository.
//!
//! Ports `io.agentscope.core.skill.repository.mysql.MysqlSkillRepository`. Skills live in a
//! `agentscope_skills` table; resources in `agentscope_skill_resources` keyed by the skill
//! id (with `ON DELETE CASCADE`). New schemas carry a `metadata_json` column; existing
//! (legacy) schemas without it only round-trip `name` / `description` — the repository probes
//! for the column at construction time, exactly like the Java `detectMetadataJsonColumnSupport`.

use std::sync::Arc;

use agentscope_harness::agent::skill::model::{AgentSkill, SkillResources};

use crate::error::SkillRepositoryError;
use crate::repository::{AgentSkillRepository, AgentSkillRepositoryInfo};
use crate::sql::{SqlConnection, SqlDatabase, SqlParam};
use crate::util::{
    build_skill, has_extended_metadata, serialize_metadata, validate_identifier,
    validate_resource_path, validate_skill_name,
};

/// Default database name.
pub const DEFAULT_DATABASE_NAME: &str = "agentscope";
/// Default skills table name.
pub const DEFAULT_SKILLS_TABLE_NAME: &str = "agentscope_skills";
/// Default resources table name.
pub const DEFAULT_RESOURCES_TABLE_NAME: &str = "agentscope_skill_resources";

const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Configuration for [`MysqlSkillRepository`].
#[derive(Debug, Clone)]
pub struct MysqlSkillRepositoryConfig {
    /// Database name (validated identifier).
    pub database_name: String,
    /// Skills table name (validated identifier).
    pub skills_table_name: String,
    /// Resources table name (validated identifier).
    pub resources_table_name: String,
    /// Whether to auto-create the database/tables for new deployments.
    pub create_if_not_exist: bool,
    /// Whether write operations are allowed.
    pub writeable: bool,
}

impl Default for MysqlSkillRepositoryConfig {
    fn default() -> Self {
        Self {
            database_name: DEFAULT_DATABASE_NAME.to_string(),
            skills_table_name: DEFAULT_SKILLS_TABLE_NAME.to_string(),
            resources_table_name: DEFAULT_RESOURCES_TABLE_NAME.to_string(),
            create_if_not_exist: true,
            writeable: true,
        }
    }
}

impl MysqlSkillRepositoryConfig {
    /// Creates a config with the default database/table names.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the database name.
    pub fn database_name(mut self, name: impl Into<String>) -> Self {
        self.database_name = name.into();
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

    /// Toggles auto-creation of the database/tables.
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

/// MySQL-based [`AgentSkillRepository`] implementation.
///
/// **已废弃（仅兼容，勿扩展）**：Java 侧 `MysqlSkillRepository` 已标 `@Deprecated`，
/// 官方替代是按 namespace 分区的 `JdbcAgentSkillRepository`。按 `EXTENSIONS_GAP_TRACKER.md`
/// §4.5 裁定 2，Rust 侧在 `agentscope-extensions-jdbc` 实现该替代，本类型仅保留以兼容
/// 既有调用方；**不要在此新增功能**。
pub struct MysqlSkillRepository<D: SqlDatabase> {
    db: Arc<D>,
    cfg: MysqlSkillRepositoryConfig,
    metadata_json_supported: bool,
    /// Runtime write flag (Java MysqlSkillRepository#writeable), toggled by
    /// `set_writeable`.
    writeable: std::sync::atomic::AtomicBool,
}

impl<D: SqlDatabase> MysqlSkillRepository<D> {
    /// Builds a repository with the default config.
    pub fn new(db: Arc<D>) -> Result<Self, SkillRepositoryError> {
        Self::with_config(db, MysqlSkillRepositoryConfig::default())
    }

    /// Builds a repository with an explicit config, initializing the schema when requested.
    pub fn with_config(
        db: Arc<D>,
        cfg: MysqlSkillRepositoryConfig,
    ) -> Result<Self, SkillRepositoryError> {
        validate_identifier(&cfg.database_name, "database name", MAX_IDENTIFIER_LENGTH)?;
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
            repo.create_database_if_not_exist()?;
            repo.create_tables_if_not_exist()?;
        } else {
            // Java: `verifyDatabaseExists()` + `verifyTablesExist()` -> IllegalStateException.
            repo.verify_database_exists()?;
            repo.verify_tables_exist()?;
        }
        repo.metadata_json_supported = repo.detect_metadata_json_column_support();
        Ok(repo)
    }

    /// The fully-qualified table name (`<database>.<table>`), mirroring `getFullTableName`.
    /// Identifiers are validated in the constructor, so plain concatenation is safe here.
    fn full_table_name(&self, table: &str) -> String {
        format!("{}.{}", self.cfg.database_name, table)
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

    /// Creates the database if it does not exist (`createDatabaseIfNotExist`), utf8mb4.
    fn create_database_if_not_exist(&self) -> Result<(), SkillRepositoryError> {
        let sql = format!(
            "CREATE DATABASE IF NOT EXISTS {} DEFAULT CHARACTER SET utf8mb4 COLLATE \
             utf8mb4_unicode_ci",
            self.cfg.database_name
        );
        let mut conn = self.db.connection()?;
        conn.execute(&sql, &[])?;
        Ok(())
    }

    /// Verifies the database exists (`verifyDatabaseExists`), utf8mb4.
    fn verify_database_exists(&self) -> Result<(), SkillRepositoryError> {
        let sql = "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = ?";
        let mut conn = self.db.connection()?;
        let rows = conn.query(sql, &[SqlParam::Text(self.cfg.database_name.clone())])?;
        if rows.is_empty() {
            return Err(SkillRepositoryError::Database(format!(
                "database does not exist: {} (use createIfNotExist=true to auto-create)",
                self.cfg.database_name
            )));
        }
        Ok(())
    }

    /// Verifies both tables exist (`verifyTablesExist`).
    fn verify_tables_exist(&self) -> Result<(), SkillRepositoryError> {
        self.verify_table_exists(&self.cfg.skills_table_name)?;
        self.verify_table_exists(&self.cfg.resources_table_name)
    }

    fn verify_table_exists(&self, table: &str) -> Result<(), SkillRepositoryError> {
        let sql =
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
        let params = [
            SqlParam::Text(self.cfg.database_name.clone()),
            SqlParam::Text(table.to_string()),
        ];
        let mut conn = self.db.connection()?;
        if conn.query(sql, &params)?.is_empty() {
            return Err(SkillRepositoryError::Database(format!(
                "table does not exist: {}.{} (use createIfNotExist=true to auto-create)",
                self.cfg.database_name, table
            )));
        }
        Ok(())
    }

    /// Creates the tables if they do not exist (`createTablesIfNotExist`).
    fn create_tables_if_not_exist(&self) -> Result<(), SkillRepositoryError> {
        let skills_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n\
             id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,\n\
             name VARCHAR(255) NOT NULL UNIQUE,\n\
             description TEXT NOT NULL,\n\
             skill_content LONGTEXT NOT NULL,\n\
             source VARCHAR(255) NOT NULL,\n\
             metadata_json LONGTEXT NULL,\n\
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,\n\
             updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP\n\
             ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let resources_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n\
             id BIGINT NOT NULL,\n\
             resource_path VARCHAR(500) NOT NULL,\n\
             resource_content LONGTEXT NOT NULL,\n\
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,\n\
             updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,\n\
             PRIMARY KEY (id, resource_path),\n\
             FOREIGN KEY (id) REFERENCES {}(id) ON DELETE CASCADE\n\
             ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            self.full_table_name(&self.cfg.resources_table_name),
            self.full_table_name(&self.cfg.skills_table_name)
        );
        let mut conn = self.db.connection()?;
        conn.execute(&skills_sql, &[])?;
        conn.execute(&resources_sql, &[])?;
        Ok(())
    }

    /// Probes `INFORMATION_SCHEMA.COLUMNS` for the optional `metadata_json` column.
    fn detect_metadata_json_column_support(&self) -> bool {
        let sql = "SELECT DATA_TYPE FROM INFORMATION_SCHEMA.COLUMNS \
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = 'metadata_json'";
        let Ok(mut conn) = self.db.connection() else {
            return false;
        };
        let params = [
            SqlParam::Text(self.cfg.database_name.clone()),
            SqlParam::Text(self.cfg.skills_table_name.clone()),
        ];
        match conn.query(sql, &params) {
            Ok(rows) => !rows.is_empty(),
            Err(_) => false,
        }
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

    /// Deletes a skill and (cascaded) its resources within an existing connection.
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

    /// Inserts a skill row and returns the generated id (`insertSkill`).
    fn insert_skill(
        &self,
        conn: &mut D::Connection,
        skill: &AgentSkill,
    ) -> Result<i64, SkillRepositoryError> {
        let metadata_json = serialize_metadata(skill.metadata());
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
                SqlParam::Text(metadata_json),
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

    /// The skill body persisted in the `skill_content` column.
    ///
    /// The harness `AgentSkill` folds the body into the `SKILL.md` resource; the column keeps
    /// that body for Java schema interoperability.
    fn skill_content(&self, skill: &AgentSkill) -> String {
        skill
            .resources()
            .iter()
            .find(|(path, _)| path.ends_with("SKILL.md"))
            .map(|(_, content)| content.clone())
            .unwrap_or_default()
    }

    fn insert_resources(
        &self,
        conn: &mut D::Connection,
        skill_id: i64,
        resources: &SkillResources,
    ) -> Result<(), SkillRepositoryError> {
        let sql = format!(
            "INSERT INTO {} (id, resource_path, resource_content) VALUES (?, ?, ?)",
            self.full_table_name(&self.cfg.resources_table_name)
        );
        for (path, content) in resources {
            let params = [
                SqlParam::Int(skill_id),
                SqlParam::Text(path.clone()),
                SqlParam::Text(content.clone()),
            ];
            conn.execute(&sql, &params)?;
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

impl<D: SqlDatabase> AgentSkillRepository for MysqlSkillRepository<D> {
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
        // Restore the skill body into the resource map so the harness sees a normal skill.
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

        // Group resources by skill id.
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
        // Pre-check: validate names / resource paths before opening a transaction.
        for skill in skills {
            validate_skill_name(skill.name())?;
            for path in skill.resources().keys() {
                validate_resource_path(path)?;
            }
        }

        let mut conn = self.db.connection()?;
        // Pre-check: with force=false, reject when any skill already exists.
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

        conn.set_auto_commit(false)?;
        let result = (|| -> Result<(), SkillRepositoryError> {
            for skill in skills {
                if self.skill_exists_internal(&mut conn, skill.name())? {
                    self.delete_skill_internal(&mut conn, skill.name())?;
                }
                let id = self.insert_skill(&mut conn, skill)?;
                self.insert_resources(&mut conn, id, skill.resources())?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.commit()?;
                conn.set_auto_commit(true)?;
                Ok(true)
            }
            Err(e) => {
                let _ = conn.rollback();
                let _ = conn.set_auto_commit(true);
                Err(e)
            }
        }
    }

    fn delete(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        validate_skill_name(skill_name)?;
        if !self.writeable() {
            tracing::warn!("cannot delete skill: repository is read-only");
            return Ok(false);
        }
        let mut conn = self.db.connection()?;
        let deleted = self.delete_skill_internal(&mut conn, skill_name)?;
        Ok(deleted > 0)
    }

    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        validate_skill_name(skill_name)?;
        let mut conn = self.db.connection()?;
        self.skill_exists_internal(&mut conn, skill_name)
    }

    fn get_repository_info(&self) -> AgentSkillRepositoryInfo {
        let location = format!("{}.{}", self.cfg.database_name, self.cfg.skills_table_name);
        AgentSkillRepositoryInfo::new("mysql", location, self.writeable())
    }

    fn get_source(&self) -> String {
        // Java: `"mysql_" + databaseName + "_" + skillsTableName`.
        format!(
            "mysql_{}_{}",
            self.cfg.database_name, self.cfg.skills_table_name
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
    use crate::sql::InMemorySqlDatabase;

    fn skill(name: &str, desc: &str) -> AgentSkill {
        AgentSkill::builder()
            .name(name)
            .source("custom")
            .description(desc)
            .add_resource("SKILL.md", "# body")
            .add_resource("a.txt", "data")
            .build()
    }

    fn repo() -> MysqlSkillRepository<InMemorySqlDatabase> {
        let db = Arc::new(InMemorySqlDatabase::new());
        MysqlSkillRepository::new(db).unwrap()
    }

    #[test]
    fn create_tables_and_detect_metadata_column() {
        let r = repo();
        assert!(r.metadata_json_column_supported());
    }

    #[test]
    fn save_then_get_roundtrip_preserves_resources() {
        let r = repo();
        assert!(r.save(&[skill("calc", "calc skill")], false).unwrap());
        let loaded = r.get_skill("calc").unwrap();
        assert_eq!(loaded.name(), "calc");
        assert_eq!(loaded.description(), "calc skill");
        assert_eq!(
            loaded.resources().get("a.txt").map(|s| s.as_str()),
            Some("data")
        );
        assert!(loaded.metadata().contains_key("name"));
    }

    #[test]
    fn empty_save_returns_false() {
        assert!(!repo().save(&[], false).unwrap());
    }

    // Java: MysqlSkillRepository#setWriteable / isWriteable (mutable flag)
    #[test]
    fn set_writeable_toggles_runtime_flag() {
        let r = repo();
        assert!(r.is_writeable(), "builder default is writeable");
        r.set_writeable(false);
        assert!(!r.is_writeable());
        // Writes are refused while the flag is off.
        assert!(!r.save(&[skill("calc", "calc skill")], false).unwrap());
        r.set_writeable(true);
        assert!(r.is_writeable());
        assert!(r.save(&[skill("calc", "calc skill")], false).unwrap());
    }

    #[test]
    fn save_without_force_rejects_existing() {
        let r = repo();
        r.save(&[skill("a", "1")], false).unwrap();
        let err = r.save(&[skill("a", "2")], false).unwrap_err();
        assert!(matches!(err, SkillRepositoryError::Conflict(_)));
    }

    #[test]
    fn save_with_force_overwrites() {
        let r = repo();
        r.save(&[skill("a", "1")], false).unwrap();
        assert!(r.save(&[skill("a", "2")], true).unwrap());
        assert_eq!(r.get_skill("a").unwrap().description(), "2");
    }

    #[test]
    fn delete_returns_true_then_false() {
        let r = repo();
        r.save(&[skill("a", "1")], false).unwrap();
        assert!(r.delete("a").unwrap());
        assert!(!r.delete("a").unwrap());
    }

    #[test]
    fn get_missing_skill_is_not_found() {
        assert!(matches!(
            repo().get_skill("nope").unwrap_err(),
            SkillRepositoryError::NotFound(_)
        ));
    }

    #[test]
    fn read_only_repository_rejects_writes() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let r = MysqlSkillRepository::with_config(
            db,
            MysqlSkillRepositoryConfig::new().writeable(false),
        )
        .unwrap();
        assert!(!r.save(&[skill("a", "1")], true).unwrap());
        assert!(!r.is_writeable());
    }

    #[test]
    fn rejects_bad_identifiers() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let cfg = MysqlSkillRepositoryConfig::new().skills_table_name("bad-name");
        assert!(MysqlSkillRepository::with_config(db, cfg).is_err());
    }

    #[test]
    fn rejects_path_traversal_skill_names() {
        let r = repo();
        let bad = AgentSkill::builder()
            .name("../x")
            .source("s")
            .description("d")
            .build();
        assert!(r.save(&[bad], false).is_err());
    }

    #[test]
    fn clear_all_skills_empties_store() {
        let r = repo();
        r.save(&[skill("a", "1"), skill("b", "2")], false).unwrap();
        assert_eq!(r.clear_all_skills().unwrap(), 2);
        assert!(r.get_all_skills().unwrap().is_empty());
    }

    // Java: MysqlSkillRepository#getSource -> `mysql_<db>_<table>` (P1-10).
    #[test]
    fn get_source_includes_database_and_table() {
        assert_eq!(repo().get_source(), "mysql_agentscope_agentscope_skills");
    }

    // Java: createDatabaseIfNotExist + getFullTableName + utf8mb4 DDL (P1-05).
    #[test]
    fn creates_database_and_qualified_tables() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let r = MysqlSkillRepository::new(db.clone()).unwrap();
        assert!(db.schema_exists("agentscope"));
        assert!(db.table_exists("agentscope_skills"));
        assert!(db.table_exists("agentscope_skill_resources"));
        assert_eq!(
            db.metadata_column_type("agentscope_skills").as_deref(),
            Some("longtext")
        );
        // Every statement goes through `<db>.<table>`, so a round trip proves the prefixing.
        assert!(r.save(&[skill("a", "1")], false).unwrap());
        assert_eq!(r.get_skill("a").unwrap().description(), "1");
        assert_eq!(r.get_all_skill_names().unwrap(), vec!["a".to_string()]);
        assert!(r.delete("a").unwrap());
    }

    // Java: createIfNotExist=false -> verifyDatabaseExists / verifyTablesExist (P1-06).
    #[test]
    fn create_if_not_exist_false_requires_the_database() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let err = match MysqlSkillRepository::with_config(
            db,
            MysqlSkillRepositoryConfig::new().create_if_not_exist(false),
        ) {
            Ok(_) => panic!("an empty database must be rejected"),
            Err(e) => e,
        };
        assert!(
            matches!(err, SkillRepositoryError::Database(_)),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains("database does not exist"));
    }

    #[test]
    fn create_if_not_exist_false_accepts_an_initialised_schema() {
        let db = Arc::new(InMemorySqlDatabase::new());
        let _owner = MysqlSkillRepository::new(db.clone()).unwrap();
        let second = MysqlSkillRepository::with_config(
            db,
            MysqlSkillRepositoryConfig::new().create_if_not_exist(false),
        )
        .expect("the schema created by the first repository must be detected");
        assert!(second.metadata_json_column_supported());
    }

    #[test]
    fn legacy_schema_without_metadata_column_still_works() {
        let db = Arc::new(InMemorySqlDatabase::new());
        // Pre-create the skills table WITHOUT metadata_json so support detection is false.
        // Both tables are created because `createIfNotExist=false` now verifies them (Java
        // `verifyTablesExist`).
        {
            let mut conn = db.connection().unwrap();
            conn.execute(
                "CREATE TABLE agentscope_skills (id BIGINT, name VARCHAR(255), description TEXT, \
                 skill_content LONGTEXT, source VARCHAR(255))",
                &[],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE agentscope_skill_resources (id BIGINT, resource_path VARCHAR(500), \
                 resource_content LONGTEXT)",
                &[],
            )
            .unwrap();
        }
        let r = MysqlSkillRepository::with_config(
            db,
            MysqlSkillRepositoryConfig::new().create_if_not_exist(false),
        )
        .unwrap();
        assert!(!r.metadata_json_column_supported());
        r.save(&[skill("a", "legacy")], false).unwrap();
        let loaded = r.get_skill("a").unwrap();
        assert_eq!(loaded.description(), "legacy");
        assert_eq!(loaded.metadata().len(), 2); // name + description only
    }
}
