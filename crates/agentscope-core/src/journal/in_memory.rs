//! Explicit ephemeral backend, mirroring `InMemoryThreadJournalStore`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    AtomicJournalStorage, AtomicThreadJournal, StorageValue, ThreadJournal, ThreadJournalStore,
};
use crate::agent::{RuntimeContext, ThreadKey};

/// Process-local atomic storage for tests and non-persistent embedded applications.
#[derive(Debug, Default)]
pub struct MemoryJournalStorage {
    values: Mutex<HashMap<String, StorageValue>>,
}

impl MemoryJournalStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AtomicJournalStorage for MemoryJournalStorage {
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

    async fn remove(&self, path: &str) -> crate::error::Result<bool> {
        Ok(self
            .values
            .lock()
            .expect("memory session storage poisoned")
            .remove(path)
            .is_some())
    }
}

/// In-memory session log store keyed by logical identity.
#[derive(Debug, Default)]
pub struct InMemoryThreadJournalStore {
    stores: Mutex<HashMap<ThreadKey, Arc<MemoryJournalStorage>>>,
}

impl InMemoryThreadJournalStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ThreadJournalStore for InMemoryThreadJournalStore {
    async fn open(
        &self,
        key: &ThreadKey,
        _context: &RuntimeContext,
    ) -> crate::error::Result<Arc<dyn ThreadJournal>> {
        let storage = {
            let mut guard = self.stores.lock().expect("session store poisoned");
            Arc::clone(
                guard
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(MemoryJournalStorage::new())),
            )
        };
        Ok(Arc::new(AtomicThreadJournal::new(
            storage,
            key.storage_path(),
        )))
    }

    async fn list(&self, context: &RuntimeContext) -> crate::error::Result<Vec<ThreadKey>> {
        let user_id = context.user_id().map(str::to_string);
        let entries: Vec<(ThreadKey, Arc<MemoryJournalStorage>)> = {
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

    /// Drops the journal of `key`: its storage, and therefore every value under the prefix.
    async fn delete(&self, key: &ThreadKey) -> crate::error::Result<()> {
        self.stores
            .lock()
            .expect("session store poisoned")
            .remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalEvent, ThreadJournalStore};

    #[tokio::test]
    async fn store_creates_journals_and_lists_only_committed_sessions() {
        let store = InMemoryThreadJournalStore::new();
        let ctx = RuntimeContext::new().with_user_id("alice");
        let key = ThreadKey::new(Some("alice"), "agent", "s1").unwrap();
        let log = store.open(&key, &ctx).await.unwrap();
        // Opening a reader never creates durable state.
        assert!(store.list(&ctx).await.unwrap().is_empty());

        let writer = log
            .acquire("run-1", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let event = JournalEvent::new(
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
