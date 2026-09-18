//! Test-only in-memory [`StateBackend`] implementation.
//!
//! Models the `(session_id, state_key, item_index)` primary key of the MySQL schema so
//! the incremental list semantics and version helpers of [`super::MysqlAgentStateStore`]
//! can be exercised without a live database. Compiled only under `cfg(test)`.

use std::collections::{BTreeMap, HashMap, BTreeSet};
use std::sync::Mutex;

use super::StateBackend;
use crate::error::MysqlResult;

/// Anonymous-user sentinel, mirroring `agentscope_core::state::store`.
const ANON_USER: &str = "__anon__";

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Identity of a session row group.
type SessionKey = (String, String);

/// In-memory model of `agentscope_sessions`.
#[derive(Default)]
pub struct InMemoryStateBackend {
    /// `(user, session) -> state_key -> (item_index -> json)`.
    rows: Mutex<HashMap<SessionKey, HashMap<String, BTreeMap<i64, String>>>>,
}

impl InMemoryStateBackend {
    fn key(user_id: Option<&str>, session_id: &str) -> SessionKey {
        (normalize_user(user_id), session_id.to_string())
    }
}

impl StateBackend for InMemoryStateBackend {
    fn ensure_schema(&self) -> MysqlResult<()> {
        Ok(())
    }

    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()> {
        let mut rows = self.rows.lock().unwrap();
        let session = rows.entry(Self::key(user_id, session_id)).or_default();
        session
            .entry(state_key.to_string())
            .or_default()
            .insert(super::SINGLE_STATE_INDEX, value_json.to_string());
        Ok(())
    }

    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .and_then(|s| s.get(state_key))
            .and_then(|m| m.get(&super::SINGLE_STATE_INDEX))
            .cloned())
    }

    fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        values_json: &[String],
        from_index: usize,
    ) -> MysqlResult<()> {
        let mut rows = self.rows.lock().unwrap();
        let session = rows.entry(Self::key(user_id, session_id)).or_default();
        let items = session.entry(state_key.to_string()).or_default();
        // Overwrite any positions below the previous length (they may have changed once
        // the list shrank), then append the genuinely new trailing items.
        let overwrite_upto = from_index.min(values_json.len());
        for (offset, json) in values_json.iter().enumerate() {
            if offset < overwrite_upto || offset >= from_index {
                items.insert(offset as i64, json.clone());
            }
        }
        // Trim stale trailing rows so shrinking removes them.
        let total = values_json.len() as i64;
        let stale: Vec<i64> = items.keys().copied().filter(|i| *i >= total).collect();
        for idx in stale {
            items.remove(&idx);
        }
        Ok(())
    }

    fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Vec<String>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .and_then(|s| s.get(state_key))
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default())
    }

    fn list_len(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<usize> {
        Ok(self.get_list(user_id, session_id, state_key)?.len())
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<bool> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .map(|s| !s.is_empty())
            .unwrap_or(false))
    }

    fn delete(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<()> {
        self.rows
            .lock()
            .unwrap()
            .remove(&Self::key(user_id, session_id));
        Ok(())
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> MysqlResult<Vec<String>> {
        let user = normalize_user(user_id);
        let rows = self.rows.lock().unwrap();
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((u, s), session) in rows.iter() {
            if *u == user && !session.is_empty() {
                ids.insert(s.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anon_and_named_users_are_separated() {
        let b = InMemoryStateBackend::default();
        b.save_single(None, "s", "k", "1").unwrap();
        assert!(b.get_single(Some("u"), "s", "k").unwrap().is_none());
        assert_eq!(b.get_single(None, "s", "k").unwrap().as_deref(), Some("1"));
    }
}
