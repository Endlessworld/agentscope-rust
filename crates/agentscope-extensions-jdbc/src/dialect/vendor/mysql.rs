//! MySQL / MariaDB dialect, mirroring `MysqlDialect`.

use crate::dialect::abstract_dialect::{
    AbstractJdbcDialect, DialectKind, TableNameConfigurable, TableNames,
};
use crate::dialect::bound_sql::{BoundSql, SqlParam};
use crate::dialect::{
    SessionStateDialect, SkillDialect, SkillResourcesDialect, SnapshotDialect, StoreDialect,
};

/// MySQL dialect: `ON DUPLICATE KEY UPDATE`, `LONGBLOB` snapshots, native `GET_LOCK`.
#[derive(Debug, Clone, Default)]
pub struct MysqlDialect {
    table_names: TableNames,
}

impl MysqlDialect {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TableNameConfigurable for MysqlDialect {
    fn table_names(&self) -> &TableNames {
        &self.table_names
    }
}

impl StoreDialect for MysqlDialect {
    fn store_create_table_ddls(&self) -> Vec<String> {
        // Keep the composite PK under InnoDB's utf8mb4 3072-byte limit:
        // (512 + 255) * 4 = 3068 bytes; idx_namespace (512 * 4 = 2048 bytes) also fits.
        //
        // Key columns pin `utf8mb4_bin` for the same reason as the session-state and snapshot
        // tables: `DEFAULT CHARSET=utf8mb4` alone resolves to a case-insensitive collation,
        // so `put(ns, "README.md")` and `put(ns, "readme.md")` collide on the primary key and
        // the second silently overwrites the first. `value_json` keeps the table default.
        //
        // This dialect resolves to the same physical table name (`agentscope_store`) as
        // `agentscope-extensions-mysql`'s store dialect, so both bootstrap paths must agree.
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             namespace_path VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL, \
             item_key VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL, \
             value_json LONGTEXT NOT NULL, \
             version BIGINT NOT NULL, updated_at BIGINT NOT NULL, \
             PRIMARY KEY (namespace_path, item_key), \
             INDEX idx_namespace (namespace_path)) \
             ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            self.resolved_store_table_name()
        )]
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
                 ON DUPLICATE KEY UPDATE value_json = VALUES(value_json), \
                 version = version + 1, updated_at = VALUES(updated_at)",
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

impl SessionStateDialect for MysqlDialect {
    fn session_state_create_table_ddls(&self) -> Vec<String> {
        // `session_id`/`state_key` pin a binary collation. The table default stays
        // `utf8mb4_unicode_ci` (case-insensitive) for the payload column, but a key column
        // inheriting it makes `(session_id, state_key, item_index)` treat two keys that
        // differ only in letter case as equal — the second write then collides on the
        // primary key and silently overwrites the first. Same defect as the store table
        // (`agentscope-extensions-mysql/src/store/dialect.rs`), same fix.
        //
        // A column-level `COLLATE` is required even though the table already declares
        // `DEFAULT CHARACTER SET utf8mb4`: the table default is only used by columns with no
        // collation of their own, which is exactly the case this fixes. `CHARACTER SET` is
        // spelled out because `COLLATE` must match the column charset.
        //
        // PK size stays under InnoDB's 3072-byte limit: (255 + 255) * 4 + 4 = 2044 bytes.
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             session_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL, \
             state_key VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL, \
             item_index INT NOT NULL DEFAULT 0, state_data LONGTEXT NOT NULL, \
             version BIGINT NOT NULL DEFAULT 0, \
             created_at DATETIME DEFAULT CURRENT_TIMESTAMP, \
             updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
             PRIMARY KEY (session_id, state_key, item_index), \
             INDEX idx_session (session_id)) \
             DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            self.resolved_session_state_table_name()
        )]
    }

    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT 1 FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
            vec![SqlParam::Text(table_name.to_string())],
        )
    }

    fn session_state_upsert(&self, slot_id: &str, key: &str, index: i32, json: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
                 VALUES (?, ?, ?, ?, 1) \
                 ON DUPLICATE KEY UPDATE state_data = VALUES(state_data), \
                 version = version + 1",
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

impl SnapshotDialect for MysqlDialect {
    fn snapshot_create_table_ddls(&self) -> Vec<String> {
        // `snapshot_id` is the primary key, so it needs the same binary collation as the
        // other key columns: under a case-insensitive collation two snapshot ids differing
        // only in letter case collide, and the UPSERT silently overwrites the first archive
        // with the second. The table-level default stays `utf8mb4` (Java `MysqlDialect`
        // declares `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4` too), while the key column is
        // fully explicit (`CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`).
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             snapshot_id VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL PRIMARY KEY, \
             data LONGBLOB NOT NULL, \
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP) \
             ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            self.resolved_snapshot_table_name()
        )]
    }

    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (snapshot_id, data) VALUES (?, ?) \
                 ON DUPLICATE KEY UPDATE data = VALUES(data), \
                 created_at = CURRENT_TIMESTAMP",
                self.resolved_snapshot_table_name()
            ),
            vec![
                SqlParam::Text(snapshot_id.to_string()),
                SqlParam::Blob(data.to_vec()),
            ],
        )
    }
}

impl SkillDialect for MysqlDialect {
    fn skill_create_table_ddls(&self) -> Vec<String> {
        // `namespace` pins `utf8mb4_bin` so case-only namespace differences (Team-A vs
        // team-a) stay distinct, matching JdbcAgentSkillRepository's namespace charset.
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             id            BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
             namespace     VARCHAR(64) COLLATE utf8mb4_bin NOT NULL DEFAULT 'default', \
             name          VARCHAR(255) NOT NULL, \
             description   TEXT NOT NULL, \
             skill_content LONGTEXT NOT NULL, \
             source        VARCHAR(255) NOT NULL, \
             metadata_json LONGTEXT NULL, \
             created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP \
               ON UPDATE CURRENT_TIMESTAMP, \
             UNIQUE KEY uk_namespace_name (namespace, name)) \
             DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            self.resolved_skill_table_name()
        )]
    }
}

impl SkillResourcesDialect for MysqlDialect {
    fn skill_resources_create_table_ddls(&self) -> Vec<String> {
        // `idx_namespace` mirrors the store table's: the bulk resource statements filter
        // by namespace on a table shared by every namespace, and PRIMARY KEY
        // (id, resource_path) cannot serve that predicate (namespace is not a prefix).
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             id               BIGINT NOT NULL, \
             namespace        VARCHAR(64) COLLATE utf8mb4_bin NOT NULL DEFAULT 'default', \
             resource_path    VARCHAR(500) NOT NULL, \
             resource_content LONGTEXT NOT NULL, \
             created_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP, \
             updated_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP \
               ON UPDATE CURRENT_TIMESTAMP, \
             PRIMARY KEY (id, resource_path), \
             INDEX idx_namespace (namespace), \
             FOREIGN KEY (id) REFERENCES {}(id) ON DELETE CASCADE) \
             DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            self.resolved_skill_resources_table_name(),
            self.resolved_skill_table_name()
        )]
    }
}

impl AbstractJdbcDialect for MysqlDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Mysql
    }

    fn supports_native_locks(&self) -> bool {
        true
    }

    /// Restrict the probe to the connected database (`DATABASE()`), so a same-named
    /// table in another schema cannot mask a missing column.
    fn table_columns_query(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT column_name FROM information_schema.columns \
             WHERE LOWER(table_name) = LOWER(?) AND table_schema = DATABASE()",
            vec![SqlParam::Text(table_name.to_string())],
        )
    }
}
