//! Portable ANSI dialect — the baseline inherited by all vendors.

use crate::dialect::abstract_dialect::{
    AbstractJdbcDialect, DialectKind, TableNameConfigurable, TableNames,
};
use crate::dialect::bound_sql::{BoundSql, SqlParam};
use crate::dialect::{SessionStateDialect, SkillDialect, SkillResourcesDialect, SnapshotDialect, StoreDialect};

/// ANSI baseline dialect. Also the default for H2 and untargeted JDBC databases.
#[derive(Debug, Clone, Default)]
pub struct AnsiDialect {
    table_names: TableNames,
}

impl AnsiDialect {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TableNameConfigurable for AnsiDialect {
    fn table_names(&self) -> &TableNames {
        &self.table_names
    }
}

impl StoreDialect for AnsiDialect {
    fn store_table_name(&self) -> &str {
        "store"
    }

    fn store_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 namespace_path VARCHAR(2048) NOT NULL, item_key VARCHAR(255) NOT NULL, \
                 value_json CLOB NOT NULL, version BIGINT NOT NULL, updated_at BIGINT NOT NULL, \
                 PRIMARY KEY (namespace_path, item_key))",
                self.resolved_store_table_name()
            ),
            // H2/ANSI cannot express a secondary index inside CREATE TABLE.
            format!(
                "CREATE INDEX IF NOT EXISTS {}_namespace_idx ON {} (namespace_path)",
                self.resolved_store_table_name(),
                self.resolved_store_table_name()
            ),
        ]
    }

    // ANSI/H2 UPSERT — MERGE INTO with matched/not-matched branches, mirroring H2Dialect.
    // Vendors override with ON CONFLICT / ON DUPLICATE KEY as needed.
    fn store_upsert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "MERGE INTO {} AS t USING (VALUES (?, ?, ?, ?)) AS s(np, ik, vj, ts) \
                 ON t.namespace_path = s.np AND t.item_key = s.ik \
                 WHEN MATCHED THEN UPDATE SET \
                 value_json = s.vj, version = t.version + 1, updated_at = s.ts \
                 WHEN NOT MATCHED THEN INSERT \
                 (namespace_path, item_key, value_json, version, updated_at) \
                 VALUES (s.np, s.ik, s.vj, 1, s.ts)",
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

impl SessionStateDialect for AnsiDialect {
    fn session_state_table_name(&self) -> &str {
        "sessions"
    }

    fn session_state_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 session_id VARCHAR(255) NOT NULL, state_key VARCHAR(255) NOT NULL, \
                 item_index INT NOT NULL DEFAULT 0, state_data CLOB NOT NULL, \
                 version BIGINT NOT NULL DEFAULT 0, \
                 created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 PRIMARY KEY (session_id, state_key, item_index))",
                self.resolved_session_state_table_name()
            ),
            // H2/ANSI cannot express a secondary index inside CREATE TABLE.
            format!(
                "CREATE INDEX IF NOT EXISTS {}_session_idx ON {} (session_id)",
                self.resolved_session_state_table_name(),
                self.resolved_session_state_table_name()
            ),
        ]
    }

    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME = ?",
            vec![SqlParam::Text(table_name.to_string())],
        )
    }

    fn session_state_upsert(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "MERGE INTO {} AS t USING (VALUES (?, ?, ?, ?)) AS s(sid, sk, ii, sd) \
                 ON t.session_id = s.sid AND t.state_key = s.sk AND t.item_index = s.ii \
                 WHEN MATCHED THEN UPDATE SET state_data = s.sd, version = t.version + 1 \
                 WHEN NOT MATCHED THEN INSERT \
                 (session_id, state_key, item_index, state_data, version) \
                 VALUES (s.sid, s.sk, s.ii, s.sd, 1)",
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
}

impl SnapshotDialect for AnsiDialect {
    fn snapshot_table_name(&self) -> &str {
        "snapshots"
    }

    fn snapshot_create_table_ddls(&self) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             snapshot_id VARCHAR(512) NOT NULL PRIMARY KEY, \
             data BLOB NOT NULL, \
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
            self.resolved_snapshot_table_name()
        )]
    }

    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql {
        BoundSql::new(
            format!(
                "MERGE INTO {} AS t USING (VALUES (?, CAST(? AS BLOB))) AS s(sid, dat) \
                 ON t.snapshot_id = s.sid \
                 WHEN MATCHED THEN UPDATE SET \
                 data = s.dat, created_at = CURRENT_TIMESTAMP \
                 WHEN NOT MATCHED THEN INSERT \
                 (snapshot_id, data, created_at) \
                 VALUES (s.sid, s.dat, CURRENT_TIMESTAMP)",
                self.resolved_snapshot_table_name()
            ),
            vec![
                SqlParam::Text(snapshot_id.to_string()),
                SqlParam::Blob(data.to_vec()),
            ],
        )
    }
}

impl SkillDialect for AnsiDialect {
    fn skill_create_table_ddls(&self) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             id            BIGINT AUTO_INCREMENT PRIMARY KEY, \
             namespace     VARCHAR(64) NOT NULL DEFAULT 'default', \
             name          VARCHAR(255) NOT NULL, \
             description   CLOB NOT NULL, \
             skill_content CLOB NOT NULL, \
             source        VARCHAR(255) NOT NULL, \
             metadata_json CLOB, \
             created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             CONSTRAINT uk_namespace_name UNIQUE (namespace, name))",
            self.resolved_skill_table_name()
        )]
    }
}

impl SkillResourcesDialect for AnsiDialect {
    fn skill_resources_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                 id               BIGINT NOT NULL, \
                 namespace        VARCHAR(64) NOT NULL DEFAULT 'default', \
                 resource_path    VARCHAR(500) NOT NULL, \
                 resource_content CLOB NOT NULL, \
                 created_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 updated_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
                 PRIMARY KEY (id, resource_path), \
                 FOREIGN KEY (id) REFERENCES {}(id) ON DELETE CASCADE)",
                self.resolved_skill_resources_table_name(),
                self.resolved_skill_table_name()
            ),
            // H2/ANSI cannot express a secondary index inside CREATE TABLE; without it the
            // namespace-scoped bulk resource statements full-scan the shared table.
            format!(
                "CREATE INDEX IF NOT EXISTS {}_namespace_idx ON {} (namespace)",
                self.resolved_skill_resources_table_name(),
                self.resolved_skill_resources_table_name()
            ),
        ]
    }
}

impl AbstractJdbcDialect for AnsiDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Ansi
    }
}
