//! Portable ANSI dialect — the baseline inherited by all vendors.

use crate::dialect::abstract_dialect::{AbstractJdbcDialect, DialectKind};
use crate::dialect::bound_sql::{BoundSql, SqlParam};
use crate::dialect::{
    SessionStateDialect, SnapshotDialect, StoreDialect,
};

/// ANSI baseline dialect. Also the default for H2 and untargeted JDBC databases.
#[derive(Debug, Clone, Default)]
pub struct AnsiDialect;

impl AnsiDialect {
    pub fn new() -> Self {
        Self
    }
}

impl StoreDialect for AnsiDialect {
    fn store_table_name(&self) -> &str {
        "store"
    }

    fn store_create_table_ddls(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (namespace_path VARCHAR(512) NOT NULL, \
                 item_key VARCHAR(255) NOT NULL, value_json TEXT NOT NULL, \
                 version BIGINT NOT NULL, updated_at BIGINT NOT NULL, \
                 PRIMARY KEY (namespace_path, item_key))",
                self.resolved_store_table_name()
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_ns ON {} (namespace_path)",
                self.resolved_store_table_name(),
                self.resolved_store_table_name()
            ),
        ]
    }

    // ANSI UPSERT is not expressible everywhere; the portable default is a no-op
    // marker. Vendors override with MERGE / ON CONFLICT / ON DUPLICATE KEY.
    fn store_upsert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "MERGE INTO {} (namespace_path, item_key, value_json, version, updated_at) \
                 KEY (namespace_path, item_key) VALUES (?, ?, ?, 1, ?)",
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
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {} (session_id VARCHAR(512) NOT NULL, \
             state_key VARCHAR(255) NOT NULL, item_index INTEGER NOT NULL, \
             state_data TEXT NOT NULL, version BIGINT NOT NULL, \
             PRIMARY KEY (session_id, state_key, item_index))",
            self.resolved_session_state_table_name()
        )]
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
                "MERGE INTO {} (session_id, state_key, item_index, state_data, version) \
                 KEY (session_id, state_key, item_index) VALUES (?, ?, ?, ?, 1)",
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
            "CREATE TABLE IF NOT EXISTS {} (snapshot_id VARCHAR(255) PRIMARY KEY, \
             data BLOB NOT NULL, updated_at BIGINT NOT NULL)",
            self.resolved_snapshot_table_name()
        )]
    }

    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql {
        BoundSql::new(
            format!(
                "MERGE INTO {} (snapshot_id, data, updated_at) \
                 KEY (snapshot_id) VALUES (?, ?, ?)",
                self.resolved_snapshot_table_name()
            ),
            vec![
                SqlParam::Text(snapshot_id.to_string()),
                SqlParam::Blob(data.to_vec()),
                SqlParam::Int(crate::now_millis()),
            ],
        )
    }
}

impl AbstractJdbcDialect for AnsiDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Ansi
    }
}
