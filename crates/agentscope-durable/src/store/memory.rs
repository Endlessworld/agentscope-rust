//! In-memory store: the default for tests and single-process deployments.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde_json::Value;

use super::{RecordStore, Txn, WorkItem};
use crate::{DurableError, Result};

#[derive(Default)]
pub struct InMemoryStore {
    scopes: Mutex<HashMap<String, BTreeMap<String, Value>>>,
    work: Mutex<HashMap<(String, String), WorkItem>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scope_count(&self) -> usize {
        self.scopes.lock().expect("store lock").len()
    }
}

struct StagedTxn<'a> {
    base: &'a BTreeMap<String, Value>,
    staged: BTreeMap<String, Option<Value>>,
    now: i64,
}

impl StagedTxn<'_> {
    fn read(&self, part: &str) -> Option<Value> {
        match self.staged.get(part) {
            Some(value) => value.clone(),
            None => self.base.get(part).cloned(),
        }
    }
}

impl Txn for StagedTxn<'_> {
    fn now(&self) -> i64 {
        self.now
    }

    fn get(&self, part: &str) -> Result<Option<Value>> {
        Ok(self.read(part))
    }

    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()> {
        self.staged.insert(part.to_string(), value);
        Ok(())
    }

    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let mut keys: Vec<String> = self
            .base
            .keys()
            .chain(self.staged.keys())
            .filter(|key| key.starts_with(prefix))
            .filter(|key| after.is_none_or(|after| key.as_str() > after))
            .filter(|key| self.read(key).is_some())
            .cloned()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys.truncate(limit);
        Ok(keys)
    }

    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()> {
        self.staged.insert(
            schedule_key(queue, work_id),
            Some(serde_json::json!({
                "queue": queue,
                "workId": work_id,
                "readyAt": ready_at,
                "payload": payload,
            })),
        );
        Ok(())
    }
}

/// Readiness entries live in the same scope map under a reserved prefix so that a
/// failed transaction rolls them back with everything else.
fn schedule_key(queue: &str, work_id: &str) -> String {
    format!("\u{1}work:{queue}\u{0}{work_id}")
}

impl RecordStore for InMemoryStore {
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        let mut scopes = self.scopes.lock().expect("store lock");
        let base = scopes.remove(scope).unwrap_or_default();
        let now = crate::now_millis();
        let mut staged = StagedTxn {
            base: &base,
            staged: BTreeMap::new(),
            now,
        };
        let outcome = work(&mut staged);
        let StagedTxn { staged, .. } = staged;
        if let Err(error) = outcome {
            // Discard: the committed map is put back untouched.
            scopes.insert(scope.to_string(), base);
            return Err(error);
        }
        let mut committed = base;
        let mut work_index = self.work.lock().expect("work lock");
        for (part, value) in staged {
            if let Some((queue, work_id)) = part.strip_prefix('\u{1}').and_then(|rest| {
                rest.strip_prefix("work:").and_then(|rest| {
                    rest.split_once('\u{0}')
                        .map(|(q, w)| (q.to_string(), w.to_string()))
                })
            }) {
                match value {
                    Some(value) => {
                        let ready_at =
                            value
                                .get("readyAt")
                                .and_then(Value::as_i64)
                                .ok_or_else(|| {
                                    DurableError::invalid("readiness entry needs readyAt")
                                })?;
                        if ready_at < 0 {
                            work_index.remove(&(queue, work_id));
                        } else {
                            work_index.insert(
                                (queue.clone(), work_id.clone()),
                                WorkItem {
                                    queue,
                                    work_id,
                                    ready_at,
                                    payload: value.get("payload").cloned().filter(|v| !v.is_null()),
                                },
                            );
                        }
                    }
                    None => {
                        work_index.remove(&(queue, work_id));
                    }
                }
                continue;
            }
            match value {
                Some(value) => {
                    committed.insert(part, value);
                }
                None => {
                    committed.remove(&part);
                }
            }
        }
        scopes.insert(scope.to_string(), committed);
        Ok(())
    }

    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let work_index = self.work.lock().expect("work lock");
        let mut due: Vec<WorkItem> = work_index
            .values()
            .filter(|item| item.ready_at <= now && item.queue == queue)
            .cloned()
            .collect();
        due.sort_by(|left, right| {
            left.ready_at
                .cmp(&right.ready_at)
                .then_with(|| left.work_id.cmp(&right.work_id))
        });
        due.dedup_by(|left, right| left.work_id == right.work_id);
        due.truncate(limit);
        Ok(due)
    }

    fn shared_across_hosts(&self) -> bool {
        false
    }
}
