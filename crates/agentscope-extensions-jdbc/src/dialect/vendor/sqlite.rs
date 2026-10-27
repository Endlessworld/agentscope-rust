//! SQLite dialect, mirroring `SqliteDialect`.

use crate::dialect::abstract_dialect::{
    AbstractJdbcDialect, DialectKind, TableNameConfigurable, TableNames,
};
use crate::dialect::bound_sql::{BoundSql, SqlParam};
use crate::dialect::{
    SessionStateDialect, SkillDialect, SkillResourcesDialect, SnapshotDialect, StoreDialect,
};

/// SQLite dialect: `ON CONFLICT DO UPDATE`, `BLOB` snapshots, portable table lock.
#[derive(Debug, Clone, Default)]
pub struct SqliteDialect {
    table_names: TableNames,
}

impl SqliteDialect {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TableNameConfigurable for SqliteDialect {
    fn table_names(&self) -> &TableNames {
        &self.table_names
    }
}

impl StoreDialect for SqliteDialect {
    fn store_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 namespace_path TEXT NOT NULL, \
                 item_key TEXT NOT NULL, value_json TEXT NOT NULL, \
                 version INTEGER NOT NULL, updated_at INTEGER NOT NULL, \
                 PRIMARY KEY (namespace_path, item_key))",
                self.resolved_store_table_name()
            ),
            // SQLite cannot express a secondary index inside CREATE TABLE; without it the
            // LIKE prefix search degrades to a full table scan.
            format!(
                "CREATE INDEX IF NOT EXISTS {}_namespace_idx ON {} (namespace_path)",
                self.resolved_store_table_name(),
                self.resolved_store_table_name()
            ),
        ]
    }

    fn store_upsert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (namespace_path, item_key, value_json, version, updated_at) \
                 VALUES (?, ?, ?, 1, ?) \
                 ON CONFLICT (namespace_path, item_key) DO UPDATE \
                 SET value_json = excluded.value_json, version = {}.version + 1, \
                 updated_at = excluded.updated_at",
                self.resolved_store_table_name(),
                self.resolved_store_table_name()
            ),
            vec![
                SqlParam::Text(namespace_path.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Text(json.to_string()),
                SqlParam::Int(timestamp),
            ],
        )
    }
}

impl SessionStateDialect for SqliteDialect {
    fn session_state_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 session_id TEXT NOT NULL, state_key TEXT NOT NULL, \
                 item_index INTEGER NOT NULL DEFAULT 0, state_data TEXT NOT NULL, \
                 version INTEGER NOT NULL DEFAULT 0, \
                 created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 PRIMARY KEY (session_id, state_key, item_index))",
                self.resolved_session_state_table_name()
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {}_session_idx ON {} (session_id)",
                self.resolved_session_state_table_name(),
                self.resolved_session_state_table_name()
            ),
        ]
    }

    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            vec![SqlParam::Text(table_name.to_string())],
        )
    }

    fn session_state_upsert(&self, slot_id: &str, key: &str, index: i32, json: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
                 VALUES (?, ?, ?, ?, 1) \
                 ON CONFLICT (session_id, state_key, item_index) DO UPDATE \
                 SET state_data = excluded.state_data, version = {}.version + 1",
                self.resolved_session_state_table_name(),
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Text(json.to_string()),
            ],
        )
    }

    /// `RETURNING version` makes the write and the version read a single statement, so the
    /// version handed back to the caller is always the one this write produced.
    fn session_state_upsert_returning_version(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> Option<BoundSql> {
        Some(BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
                 VALUES (?, ?, ?, ?, 1) \
                 ON CONFLICT (session_id, state_key, item_index) DO UPDATE \
                 SET state_data = excluded.state_data, version = {}.version + 1 \
                 RETURNING version",
                self.resolved_session_state_table_name(),
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Text(json.to_string()),
            ],
        ))
    }
}

impl SnapshotDialect for SqliteDialect {
    fn snapshot_create_table_ddls(&self) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             snapshot_id TEXT NOT NULL PRIMARY KEY, \
             data BLOB NOT NULL, \
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
            self.resolved_snapshot_table_name()
        )]
    }

    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (snapshot_id, data) VALUES (?, ?) \
                 ON CONFLICT(snapshot_id) DO UPDATE \
                 SET data = excluded.data, created_at = CURRENT_TIMESTAMP",
                self.resolved_snapshot_table_name()
            ),
            vec![
                SqlParam::Text(snapshot_id.to_string()),
                SqlParam::Blob(data.to_vec()),
            ],
        )
    }
}

impl SkillDialect for SqliteDialect {
    fn skill_create_table_ddls(&self) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             id            INTEGER PRIMARY KEY AUTOINCREMENT, \
             namespace     VARCHAR(64) NOT NULL DEFAULT 'default', \
             name          TEXT NOT NULL, \
             description   TEXT NOT NULL, \
             skill_content TEXT NOT NULL, \
             source        TEXT NOT NULL, \
             metadata_json TEXT NULL, \
             created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             CONSTRAINT uk_namespace_name UNIQUE (namespace, name))",
            self.resolved_skill_table_name()
        )]
    }
}

impl SkillResourcesDialect for SqliteDialect {
    fn skill_resources_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 id               INTEGER NOT NULL, \
                 namespace        VARCHAR(64) NOT NULL DEFAULT 'default', \
                 resource_path    TEXT NOT NULL, \
                 resource_content TEXT NOT NULL, \
                 created_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 updated_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 PRIMARY KEY (id, resource_path), \
                 FOREIGN KEY (id) REFERENCES {}(id) ON DELETE CASCADE)",
                self.resolved_skill_resources_table_name(),
                self.resolved_skill_table_name()
            ),
            // SQLite cannot express a secondary index inside CREATE TABLE; without it the
            // namespace-scoped bulk resource statements full-scan the shared table.
            format!(
                "CREATE INDEX IF NOT EXISTS {}_namespace_idx ON {} (namespace)",
                self.resolved_skill_resources_table_name(),
                self.resolved_skill_resources_table_name()
            ),
        ]
    }
}

impl AbstractJdbcDialect for SqliteDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Sqlite
    }

    /// SQLite has no `information_schema`; the pragma table-valued function accepts
    /// the table name as a bound parameter.
    fn table_columns_query(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT name FROM pragma_table_info(?)",
            vec![SqlParam::Text(table_name.to_string())],
        )
    }
}
