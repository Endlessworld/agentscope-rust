//! Test-only in-memory [`SqlBackend`] implementation.
//!
//! Emulates a relational backend with a real composite primary key and row-level
//! versioning so the CAS semantics of [`super::MysqlStore`] can be exercised without a
//! live database. It is compiled only under `cfg(test)`.

use std::collections::HashMap;
use std::sync::Mutex;

use super::SqlBackend;
use crate::error::{MysqlError, MysqlResult};

#[derive(Debug, Default)]
struct Row {
    value_json: String,
    version: i64,
    updated_at: i64,
}

/// In-memory relational backend keyed by `(namespace_path, item_key)`.
#[derive(Default)]
pub struct InMemorySqlBackend {
    rows: Mutex<HashMap<(String, String), Row>>,
}

impl InMemorySqlBackend {
    fn row_key(ns_path: &str, key: &str) -> (String, String) {
        (ns_path.to_string(), key.to_string())
    }

    fn row_count(&self) -> usize {
        self.rows.lock().unwrap().len()
    }
}

impl SqlBackend for InMemorySqlBackend {
    fn execute_ddl(&self, _sql: &str) -> MysqlResult<()> {
        Ok(())
    }

    fn upsert(
        &self,
        _sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()> {
        let mut rows = self.rows.lock().unwrap();
        let entry = rows
            .entry(Self::row_key(ns_path, key))
            .or_insert_with(Row::default);
        if entry.version == 0 {
            entry.version = 1;
        } else {
            entry.version += 1;
        }
        entry.value_json = value_json.to_string();
        entry.updated_at = now_ms;
        Ok(())
    }

    fn insert_create(
        &self,
        _sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()> {
        let mut rows = self.rows.lock().unwrap();
        let k = Self::row_key(ns_path, key);
        if rows.contains_key(&k) {
            return Err(MysqlError::Sql(format!(
                "duplicate entry '(?, {key})' for key 'PRIMARY'"
            )));
        }
        rows.insert(
            k,
            Row {
                value_json: value_json.to_string(),
                version: 1,
                updated_at: now_ms,
            },
        );
        Ok(())
    }

    fn cas_update(
        &self,
        _sql: &str,
        value_json: &str,
        now_ms: i64,
        ns_path: &str,
        key: &str,
        expected_version: i64,
    ) -> MysqlResult<u64> {
        let mut rows = self.rows.lock().unwrap();
        match rows.get_mut(&Self::row_key(ns_path, key)) {
            Some(row) if row.version == expected_version => {
                row.value_json = value_json.to_string();
                row.version += 1;
                row.updated_at = now_ms;
                Ok(1)
            }
            _ => Ok(0),
        }
    }

    fn select(&self, _sql: &str, ns_path: &str, key: &str) -> MysqlResult<Option<(String, i64)>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::row_key(ns_path, key))
            .map(|r| (r.value_json.clone(), r.version)))
    }

    fn search(
        &self,
        _sql: &str,
        like_pattern: &str,
        limit: usize,
        offset: usize,
    ) -> MysqlResult<Vec<(String, String, i64)>> {
        // Translate the SQL LIKE pattern (using '!' as the escape char) into a plain
        // prefix match on the decoded characters, mirroring the in-memory store's
        // prefix semantics.
        let prefix = decode_like_prefix(like_pattern, '!');
        let rows = self.rows.lock().unwrap();
        let mut matches: Vec<(String, String, i64)> = rows
            .iter()
            .filter(|((ns_path, _), _)| ns_path.starts_with(&prefix) || prefix.is_empty())
            .filter(|((ns_path, _), _)| ns_path.contains(&prefix) || ns_path.starts_with(&prefix))
            .map(|((_, key), row)| (key.clone(), row.value_json.clone(), row.version))
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(matches.into_iter().skip(offset).take(limit).collect())
    }

    fn delete(&self, _sql: &str, ns_path: &str, key: &str) -> MysqlResult<()> {
        self.rows
            .lock()
            .unwrap()
            .remove(&Self::row_key(ns_path, key));
        Ok(())
    }
}

/// Decodes a `LIKE 'prefix%' ESCAPE '!'` pattern into its literal prefix.
fn decode_like_prefix(pattern: &str, escape: char) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == escape {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else if c == '%' {
            break;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_strips_escapes_and_wildcard() {
        assert_eq!(
            decode_like_prefix("a!%b\u{1F}%", '!'),
            format!("a%b{}", '\u{1F}')
        );
    }

    #[test]
    fn insert_create_conflict_is_duplicate() {
        let b = InMemorySqlBackend::default();
        b.insert_create("", "n", "k", "{}", 0).unwrap();
        let err = b.insert_create("", "n", "k", "{}", 0).unwrap_err();
        assert!(err.is_duplicate_key());
        assert_eq!(b.row_count(), 1);
    }
}
