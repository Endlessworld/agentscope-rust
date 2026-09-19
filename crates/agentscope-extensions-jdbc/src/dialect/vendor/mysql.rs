//! MySQL / MariaDB dialect, mirroring `MysqlDialect`.

use crate::dialect::abstract_dialect::{
    AbstractJdbcDialect, DialectKind, TableNameConfigurable, TableNames,
};
use crate::dialect::bound_sql::{BoundSql, SqlParam};
use crate::dialect::{SessionStateDialect, SnapshotDialect, StoreDialect};

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
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             namespace_path VARCHAR(512) NOT NULL, \
             item_key VARCHAR(255) NOT NULL, value_json LONGTEXT NOT NULL, \
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
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             session_id VARCHAR(255) NOT NULL, state_key VARCHAR(255) NOT NULL, \
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

    fn session_state_upsert(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> BoundSql {
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
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             snapshot_id VARCHAR(512) NOT NULL PRIMARY KEY, \
             data LONGBLOB NOT NULL, \
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
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

impl AbstractJdbcDialect for MysqlDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Mysql
    }

    fn supports_native_locks(&self) -> bool {
        true
    }
}
