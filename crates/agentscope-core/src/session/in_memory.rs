//! Explicit ephemeral backend, mirroring `InMemorySessionLogStore`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    AtomicSessionStorage, JournalSessionLog, SessionKey, SessionLog, SessionLogStore, StorageValue,
};
use crate::agent::RuntimeContext;

/// Process-local atomic storage for tests and non-persistent embedded applications.
#[derive(Debug, Default)]
pub struct MemorySessionStorage {
    values: Mutex<HashMap<String, StorageValue>>,
}

impl MemorySessionStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AtomicSessionStorage for MemorySessionStorage {
    async fn read(&self, path: &str) -> crate::error::Result<Option<StorageValue>> {
        Ok(self
            .values
            .lock()
            .expect("memory session storage poisoned")
            .get(path)
            .cloned())
    }

    async fn compare_and_set(
        &self,
        path: &str,
        expected_version: u64,
        bytes: &[u8],
    ) -> crate::error::Result<bool> {
        let mut guard = self.values.lock().expect("memory session storage poisoned");
        let current = guard.get(path).map(|v| v.version).unwrap_or(0);
        if current != expected_version {
            return Ok(false);
        }
        guard.insert(
            path.to_string(),
            StorageValue::new(expected_version + 1, bytes),
        );
        Ok(true)
    }

    async fn list_paths(&self, prefix: &str) -> crate::error::Result<Vec<String>> {
        let mut paths: Vec<String> = self
            .values
            .lock()
            .expect("memory session storage poisoned")
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// In-memory session log store keyed by logical identity.
#[derive(Debug, Default)]
pub struct InMemorySessionLogStore {
    stores: Mutex<HashMap<SessionKey, Arc<MemorySessionStorage>>>,
}

impl InMemorySessionLogStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionLogStore for InMemorySessionLogStore {
    async fn open(
        &self,
        key: &SessionKey,
        _context: &RuntimeContext,
    ) -> crate::error::Result<Arc<dyn SessionLog>> {
        let storage = {
            let mut guard = self.stores.lock().expect("session store poisoned");
            Arc::clone(
                guard
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(MemorySessionStorage::new())),
            )
        };
        Ok(Arc::new(JournalSessionLog::new(
            storage,
            key.storage_path(),
        )))
    }

    async fn list(&self, context: &RuntimeContext) -> crate::error::Result<Vec<SessionKey>> {
        let user_id = context.user_id().map(str::to_string);
        let entries: Vec<(SessionKey, Arc<MemorySessionStorage>)> = {
            let guard = self.stores.lock().expect("session store poisoned");
            guard
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        let mut discovered = Vec::new();
        for (key, storage) in entries {
            if key.user_id != user_id {
                continue;
            }
            let base = key.storage_path();
            let has_main = storage
                .read(&format!("{base}/session.json"))
                .await?
                .is_some();
            let has_inbox = storage
                .read(&format!("{base}/inbox/session.json"))
                .await?
                .is_some();
            if has_main || has_inbox {
                discovered.push(key);
            }
        }
        discovered.sort_by(|a, b| {
            a.agent_id
                .cmp(&b.agent_id)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(discovered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionEvent, SessionLogStore};

    #[tokio::test]
    async fn store_creates_journals_and_lists_only_committed_sessions() {
        let store = InMemorySessionLogStore::new();
        let ctx = RuntimeContext::new().with_user_id("alice");
        let key = SessionKey::new(Some("alice"), "agent", "s1").unwrap();
        let log = store.open(&key, &ctx).await.unwrap();
        // Opening a reader never creates durable state.
        assert!(store.list(&ctx).await.unwrap().is_empty());

        let writer = log
            .acquire("run-1", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let event = SessionEvent::new(
            "e1",
            1,
            0,
            "run/start",
            Some("run-1".into()),
            Some("t1".into()),
            true,
            "{}",
        )
        .unwrap();
        log.commit(&writer, "b1", 0, std::slice::from_ref(&event))
            .await
            .unwrap();
        log.release(&writer).await.unwrap();

        let listed = store.list(&ctx).await.unwrap();
        assert_eq!(listed, vec![key.clone()]);

        // A different user identity does not discover it.
        let other = RuntimeContext::new().with_user_id("bob");
        assert!(store.list(&other).await.unwrap().is_empty());
    }
}
