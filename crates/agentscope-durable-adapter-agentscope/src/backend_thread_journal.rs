//! `ThreadJournal` over a durable backend.
//!
//! The bridge writes under one lease, obtained in one of two ways:
//!
//! * **claim** (the default): [`ThreadJournal::acquire`] claims the thread's single-writer
//!   lease and [`ThreadJournal::release`] gives it back. This is how a host owns its own
//!   writer slot.
//! * **borrow** ([`BackendThreadJournal::attach_lease`]): the caller already holds the lease
//!   (a durable tick does), so `acquire` adopts it without claiming and `release` leaves it
//!   alone. The runtime stays the only owner: no epoch is bumped and no invariant depends on
//!   the bridge. The borrow lasts until [`BackendThreadJournal::detach_lease`], after which
//!   the bridge claims and releases again.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::error::Result as CoreResult;
use agentscope_core::journal::{journal_error, Head, JournalEvent, ThreadJournal, Writer};
use agentscope_durable::{
    Commit, DurableError, JournalAppend, Lease, SessionBackend, ThreadKey, TurnStatus,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::{checkpoint, tool_facts};

/// How this bridge came to hold its writer lease.
enum Held {
    /// Claimed by [`ThreadJournal::acquire`]; releasing gives it back.
    Claimed(Lease),
    /// Handed over by [`BackendThreadJournal::attach_lease`]; releasing must not give it back.
    Borrowed(Lease),
}

impl Held {
    fn lease(&self) -> &Lease {
        match self {
            Held::Claimed(lease) | Held::Borrowed(lease) => lease,
        }
    }
}

/// A durable journal scoped to one thread.
///
/// Core's `expected_seq` is the native journal position, not the turn revision: the bridge
/// appends through [`JournalAppend`] and lets the durable transaction own both.
pub struct BackendThreadJournal {
    backend: Arc<dyn SessionBackend>,
    thread: ThreadKey,
    lease: Mutex<Option<Held>>,
    /// Whether the batch's tool facts are recorded in the same commit (see [`crate::tool_facts`]).
    tool_facts: bool,
}

impl BackendThreadJournal {
    pub fn new(backend: Arc<dyn SessionBackend>, thread: ThreadKey) -> Self {
        // The facts this journal carries include `required` extension events (the finalization
        // frames of `execution_host`), and a reader that does not know their codec stops
        // recovery: `settle_uncertain_tool_outcomes` replays the committed prefix before any
        // execution host is opened, so registering at the journal is the only place that covers
        // every reader — a second prompt in a fresh process, a session reload, a host resume.
        crate::execution_host::register_event_codecs();
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

    /// Borrows `lease` instead of claiming one.
    ///
    /// For a writer already inside the tick that holds the lease: `acquire` then adopts it
    /// (same owner, same epoch, nothing claimed) and `release` is a no-op, because the lease
    /// belongs to the runtime. Attach before the first `acquire`.
    pub fn attach_lease(self, lease: Lease) -> Self {
        if let Ok(mut guard) = self.lease.lock() {
            *guard = Some(Held::Borrowed(lease));
        }
        self
    }

    /// Ends the borrow, restoring claim/release semantics for every later `acquire`.
    ///
    /// A lease this bridge claimed itself is kept: only a borrow is dropped.
    pub fn detach_lease(&self) {
        if let Ok(mut guard) = self.lease.lock() {
            if matches!(guard.as_ref(), Some(Held::Borrowed(_))) {
                *guard = None;
            }
        }
    }

    pub fn thread(&self) -> &ThreadKey {
        &self.thread
    }

    /// The lease this bridge writes under, for a host that must share the same writer.
    ///
    /// `None` before [`ThreadJournal::acquire`] and after [`ThreadJournal::release`]; a borrowed
    /// lease is returned until [`BackendThreadJournal::detach_lease`].
    pub fn lease(&self) -> Option<Lease> {
        self.lease
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|held| held.lease().clone()))
    }

    /// The borrowed lease, when the caller already owns the writer slot.
    fn borrowed(&self) -> Option<Lease> {
        self.lease
            .lock()
            .ok()
            .and_then(|guard| match guard.as_ref() {
                Some(Held::Borrowed(lease)) => Some(lease.clone()),
                _ => None,
            })
    }

    /// The committed native state, which a later commit must not discard.
    async fn current_checkpoint(&self, lease: &Lease) -> CoreResult<Value> {
        Ok(self.backend.frame(lease).await.map_err(convert)?.checkpoint)
    }

    fn lease_for(&self, writer: &Writer) -> CoreResult<Lease> {
        let guard = self
            .lease
            .lock()
            .map_err(|_| journal_error("Journal lease lock poisoned"))?;
        let lease = guard
            .as_ref()
            .map(Held::lease)
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
        // A borrowed lease is already held by the caller (a durable tick): adopt it instead of
        // claiming, because claiming would be refused by our own writer slot.
        if let Some(borrowed) = self.borrowed() {
            return Ok(Writer {
                owner: borrowed.owner.clone(),
                epoch: borrowed.epoch,
            });
        }
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
        *guard = Some(Held::Claimed(acquired));
        Ok(writer)
    }

    async fn renew(&self, writer: &Writer, lease: Duration) -> CoreResult<()> {
        let current = self.lease_for(writer)?;
        self.backend.renew(&current, lease).await.map_err(convert)
    }

    async fn release(&self, writer: &Writer) -> CoreResult<()> {
        let current = self.lease_for(writer)?;
        // A borrowed lease belongs to the tick that handed it over: releasing it here would
        // hand the thread to another writer while the runtime is still running it.
        if self.borrowed().is_some() {
            return Ok(());
        }
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
        //
        // A batch can be longer than one page (`read_journal` caps a page at 1000 entries), so the
        // stored events are paged in rather than requested in one call.
        const JOURNAL_PAGE: usize = 1000;
        let mut existing = Vec::with_capacity(events.len().min(JOURNAL_PAGE));
        let mut after = expected_seq;
        while existing.len() < events.len() {
            let wanted = (events.len() - existing.len()).min(JOURNAL_PAGE);
            let page = self.read_after(after, wanted).await?;
            let full_page = page.len() == wanted;
            if let Some(last) = page.last() {
                after = last.seq;
            }
            existing.extend(page);
            if !full_page {
                break;
            }
        }
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
        // The batch's native state becomes the thread's checkpoint. A batch without one carries
        // the committed checkpoint forward: the backend refuses a commit that discards it, and
        // re-committing the same value is deduplicated (no new position).
        commit.checkpoint = match checkpoint::from_batch(events)? {
            Some(snapshot) => snapshot,
            None if state.checkpoint_metadata.is_some() => self.current_checkpoint(&lease).await?,
            None => Value::Null,
        };
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
