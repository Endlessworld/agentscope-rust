//! `ThreadJournal` over a durable backend.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::error::Result as CoreResult;
use agentscope_core::journal::{journal_error, Head, JournalEvent, ThreadJournal, Writer};
use agentscope_durable::{
    Commit, DurableError, JournalAppend, Lease, SessionBackend, ThreadKey, TurnStatus,
};
use async_trait::async_trait;

use crate::tool_facts;

/// A durable journal scoped to one thread.
///
/// Core's `expected_seq` is the native journal position, not the turn revision: the bridge
/// appends through [`JournalAppend`] and lets the durable transaction own both.
pub struct BackendThreadJournal {
    backend: Arc<dyn SessionBackend>,
    thread: ThreadKey,
    lease: Mutex<Option<Lease>>,
    /// Whether the batch's tool facts are recorded in the same commit (see [`crate::tool_facts`]).
    tool_facts: bool,
}

impl BackendThreadJournal {
    pub fn new(backend: Arc<dyn SessionBackend>, thread: ThreadKey) -> Self {
        Self {
            backend,
            thread,
            lease: Mutex::new(None),
            tool_facts: true,
        }
    }

    /// Turns tool-fact derivation off, leaving the bridge a pure journal writer.
    ///
    /// Only for callers that own tool records themselves; a batch then commits the journal
    /// alone and no dispatch/result fact is written.
    pub fn with_tool_facts(mut self, enabled: bool) -> Self {
        self.tool_facts = enabled;
        self
    }

    pub fn thread(&self) -> &ThreadKey {
        &self.thread
    }

    fn lease_for(&self, writer: &Writer) -> CoreResult<Lease> {
        let guard = self
            .lease
            .lock()
            .map_err(|_| journal_error("Journal lease lock poisoned"))?;
        let lease = guard
            .as_ref()
            .ok_or_else(|| journal_error("No writer lease was acquired"))?;
        if lease.owner != writer.owner || lease.epoch != writer.epoch {
            return Err(journal_error("Writer lease was fenced"));
        }
        Ok(lease.clone())
    }
}

fn convert(error: DurableError) -> agentscope_core::error::Error {
    journal_error(format!("{error}"))
}

#[async_trait]
impl ThreadJournal for BackendThreadJournal {
    async fn head(&self) -> CoreResult<Head> {
        let state = self
            .backend
            .thread_state(&self.thread)
            .await
            .map_err(convert)?;
        Ok(Head {
            seq: state.journal_position,
            epoch: state.epoch,
            owner: state.owner,
            lease_until: state.lease_until,
            commit_id: None,
        })
    }

    async fn acquire(&self, owner: &str, lease: Duration) -> CoreResult<Writer> {
        let acquired = self
            .backend
            .claim(&self.thread, owner, lease)
            .await
            .map_err(convert)?
            .ok_or_else(|| journal_error("No turn is ready to execute"))?;
        let writer = Writer {
            owner: acquired.owner.clone(),
            epoch: acquired.epoch,
        };
        let mut guard = self
            .lease
            .lock()
            .map_err(|_| journal_error("Journal lease lock poisoned"))?;
        *guard = Some(acquired);
        Ok(writer)
    }

    async fn renew(&self, writer: &Writer, lease: Duration) -> CoreResult<()> {
        let current = self.lease_for(writer)?;
        self.backend.renew(&current, lease).await.map_err(convert)
    }

    async fn release(&self, writer: &Writer) -> CoreResult<()> {
        let current = self.lease_for(writer)?;
        let result = self
            .backend
            .release(&current, Duration::ZERO)
            .await
            .map_err(convert);
        if result.is_ok() {
            if let Ok(mut guard) = self.lease.lock() {
                *guard = None;
            }
        }
        result
    }

    async fn commit(
        &self,
        writer: &Writer,
        batch_id: &str,
        expected_seq: u64,
        events: &[JournalEvent],
    ) -> CoreResult<Head> {
        if events.is_empty() {
            return Err(journal_error("Empty journal batch"));
        }
        let lease = self.lease_for(writer)?;
        let mut entries = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let expected = expected_seq + 1 + index as u64;
            if event.seq != expected {
                return Err(journal_error("Non-contiguous event sequence"));
            }
            entries.push(
                serde_json::to_value(event).map_err(|error| {
                    journal_error(format!("Cannot encode journal event: {error}"))
                })?,
            );
        }
        // Idempotency is decided here, not by the durable receipt: a retry reads a new turn
        // revision, so the same batch would otherwise look like changed content.
        let existing = self.read_after(expected_seq, events.len()).await?;
        if !existing.is_empty() {
            let same_batch = existing.len() == events.len()
                && existing
                    .iter()
                    .zip(events)
                    .all(|(stored, incoming)| stored.event_id == incoming.event_id);
            if same_batch {
                return self.head().await;
            }
            return Err(journal_error("Journal position was taken by another batch"));
        }
        let state = self
            .backend
            .thread_state(&self.thread)
            .await
            .map_err(convert)?;
        if state.journal_position != expected_seq {
            return Err(journal_error("Journal position changed"));
        }
        let turn_id = state
            .current_turn_id
            .clone()
            .ok_or_else(|| journal_error("No turn to commit into"))?;
        let turn = self
            .backend
            .turn(&self.thread, &turn_id)
            .await
            .map_err(convert)?;
        if turn.status.terminal() {
            return Err(journal_error("Turn is already settled"));
        }
        let mut commit = Commit::new(batch_id, turn.revision, TurnStatus::Running);
        // The batch's tool facts land in the same transaction as the entries that imply them.
        let facts = if self.tool_facts {
            tool_facts::from_batch(&turn_id, events)?
        } else {
            tool_facts::ToolFacts::default()
        };
        commit.tools = facts.intents;
        commit.dispatches = facts.dispatches;
        commit.results = facts.results;
        commit.journal = Some(JournalAppend {
            expected_position: expected_seq,
            entries,
        });
        self.backend.commit(&lease, commit).await.map_err(convert)?;
        self.head().await
    }

    async fn read_after(&self, after: u64, limit: usize) -> CoreResult<Vec<JournalEvent>> {
        let entries = self
            .backend
            .read_journal(&self.thread, after, limit)
            .await
            .map_err(convert)?;
        entries
            .into_iter()
            .map(|entry| {
                serde_json::from_value(entry)
                    .map_err(|error| journal_error(format!("Corrupt journal entry: {error}")))
            })
            .collect()
    }
}
