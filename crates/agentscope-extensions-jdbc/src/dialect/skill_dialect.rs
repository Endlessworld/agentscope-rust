//! Table-domain dialect interfaces for the skills / skill-resources tables, mirroring
//! Java `SkillDialect` and `SkillResourcesDialect`.
//!
//! One row per skill in the skills table; one row per resource in the resources table,
//! keyed by `(id, resource_path)` with a foreign key to the skill row. Both tables carry
//! the owning skill's `namespace`, so namespace-scoped bulk statements need no join.
//!
//! Business SQL is ANSI-standard and lives in these traits; vendors override only the
//! `CREATE TABLE` DDL where their syntax diverges. The repository
//! ([`crate::skill::JdbcAgentSkillRepository`]) contains zero inline SQL.

use super::abstract_dialect::TableNameConfigurable;
use super::bound_sql::{BoundSql, SqlParam};

/// Table-domain dialect for the skills table.
///
/// Namespace partitions names: the DDL declares `UNIQUE(namespace, name)`, and every
/// statement addresses exactly one namespace explicitly. The namespace column is
/// write-once: no statement below updates it.
pub trait SkillDialect: TableNameConfigurable + Send + Sync {
    /// Base table name (without prefix).
    fn skill_table_name(&self) -> &str {
        "skills"
    }

    /// Table-name prefix (default `agentscope_`).
    fn skill_table_prefix(&self) -> String {
        self.table_config().prefix
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn skill_table_name_override(&self) -> Option<String> {
        self.table_config().skill_table_name
    }

    /// Physical table name used by every business SQL statement in this trait:
    /// per-table override > prefix + base.
    fn resolved_skill_table_name(&self) -> String {
        match self.skill_table_name_override() {
            Some(name) => name,
            None => format!("{}{}", self.skill_table_prefix(), self.skill_table_name()),
        }
    }

    /// DDL statements (one or more) creating the skills table. Must be idempotent.
    /// `metadata_json` is part of the declared schema: a legacy table without it fails
    /// assemble-time validation instead of being silently degraded.
    fn skill_create_table_ddls(&self) -> Vec<String>;

    /// SELECT of one skill by name within a namespace. Projection:
    /// `(id, name, description, skill_content, source, metadata_json)`.
    fn skill_select_by_name(&self, namespace: &str, name: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT id, name, description, skill_content, source, metadata_json FROM {} \
                 WHERE namespace = ? AND name = ?",
                self.resolved_skill_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Text(name.to_string()),
            ],
        )
    }

    /// SELECT of all skills of one namespace ordered by name.
    fn skill_select_all(&self, namespace: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT id, name, description, skill_content, source, metadata_json FROM {} \
                 WHERE namespace = ? ORDER BY name",
                self.resolved_skill_table_name()
            ),
            vec![SqlParam::Text(namespace.to_string())],
        )
    }

    /// SELECT of all skill names of one namespace ordered by name. Projection: `(name)`.
    fn skill_select_all_names(&self, namespace: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT name FROM {} WHERE namespace = ? ORDER BY name",
                self.resolved_skill_table_name()
            ),
            vec![SqlParam::Text(namespace.to_string())],
        )
    }

    /// Existence probe for one name within a namespace. `(namespace, name)` is UNIQUE,
    /// so no `LIMIT` is needed.
    fn skill_exists(&self, namespace: &str, skill_name: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT 1 FROM {} WHERE namespace = ? AND name = ?",
                self.resolved_skill_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Text(skill_name.to_string()),
            ],
        )
    }

    /// SELECT of one skill's id by name within a namespace. Projection: `(id)`.
    fn skill_select_id_by_name(&self, namespace: &str, skill_name: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT id FROM {} WHERE namespace = ? AND name = ?",
                self.resolved_skill_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Text(skill_name.to_string()),
            ],
        )
    }

    /// INSERT of one skill row. The generated id is read back by `(namespace, name)`
    /// because [`crate::backend::JdbcExecutor`] has no portable generated-keys surface.
    fn skill_insert(
        &self,
        namespace: &str,
        name: &str,
        description: &str,
        skill_content: &str,
        source: &str,
        metadata_json: &str,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} \
                 (namespace, name, description, skill_content, source, metadata_json) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                self.resolved_skill_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Text(name.to_string()),
                SqlParam::Text(description.to_string()),
                SqlParam::Text(skill_content.to_string()),
                SqlParam::Text(source.to_string()),
                SqlParam::Text(metadata_json.to_string()),
            ],
        )
    }

    /// DELETE of one skill by name within a namespace.
    fn skill_delete_by_name(&self, namespace: &str, skill_name: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE namespace = ? AND name = ?",
                self.resolved_skill_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Text(skill_name.to_string()),
            ],
        )
    }

    /// DELETE of every skill row of one namespace.
    fn skill_delete_all(&self, namespace: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE namespace = ?",
                self.resolved_skill_table_name()
            ),
            vec![SqlParam::Text(namespace.to_string())],
        )
    }
}

/// Table-domain dialect for the skill-resources table.
///
/// Must be created after the skills table it references. Rows carry the owning skill's
/// namespace, written in the same transaction, so bulk reads/deletes scope by namespace
/// without a join. Resources are also deleted explicitly by the repository (SQLite only
/// enforces the cascading FK with `PRAGMA foreign_keys` on), the cascade remaining a
/// second line of defense.
pub trait SkillResourcesDialect: TableNameConfigurable + Send + Sync {
    /// Base table name (without prefix).
    fn skill_resources_table_name(&self) -> &str {
        "skill_resources"
    }

    /// Table-name prefix (default `agentscope_`).
    fn skill_resources_table_prefix(&self) -> String {
        self.table_config().prefix
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn skill_resources_table_name_override(&self) -> Option<String> {
        self.table_config().skill_resources_table_name
    }

    /// Physical table name used by every business SQL statement in this trait.
    fn resolved_skill_resources_table_name(&self) -> String {
        match self.skill_resources_table_name_override() {
            Some(name) => name,
            None => format!(
                "{}{}",
                self.skill_resources_table_prefix(),
                self.skill_resources_table_name()
            ),
        }
    }

    /// DDL statements (one or more) creating the skill-resources table. Must be idempotent.
    fn skill_resources_create_table_ddls(&self) -> Vec<String>;

    /// INSERT of one resource row.
    fn skill_resources_insert(
        &self,
        namespace: &str,
        skill_id: i64,
        resource_path: &str,
        resource_content: &str,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (namespace, id, resource_path, resource_content) \
                 VALUES (?, ?, ?, ?)",
                self.resolved_skill_resources_table_name()
            ),
            vec![
                SqlParam::Text(namespace.to_string()),
                SqlParam::Int(skill_id),
                SqlParam::Text(resource_path.to_string()),
                SqlParam::Text(resource_content.to_string()),
            ],
        )
    }

    /// SELECT of every resource row of one namespace. Projection:
    /// `(id, resource_path, resource_content)`.
    fn skill_resources_select_all(&self, namespace: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT id, resource_path, resource_content FROM {} WHERE namespace = ?",
                self.resolved_skill_resources_table_name()
            ),
            vec![SqlParam::Text(namespace.to_string())],
        )
    }

    /// DELETE of every resource row of one namespace.
    fn skill_resources_delete_all(&self, namespace: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE namespace = ?",
                self.resolved_skill_resources_table_name()
            ),
            vec![SqlParam::Text(namespace.to_string())],
        )
    }

    /// SELECT of one skill's resources. Projection: `(resource_path, resource_content)`.
    fn skill_resources_select_by_skill_id(&self, skill_id: i64) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT resource_path, resource_content FROM {} WHERE id = ?",
                self.resolved_skill_resources_table_name()
            ),
            vec![SqlParam::Int(skill_id)],
        )
    }

    /// DELETE of one skill's resource rows.
    fn skill_resources_delete_by_skill_id(&self, skill_id: i64) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE id = ?",
                self.resolved_skill_resources_table_name()
            ),
            vec![SqlParam::Int(skill_id)],
        )
    }
}
