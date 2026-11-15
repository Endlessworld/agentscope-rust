//! Read-side queries: turns, events, readiness, checkpoints, snapshots.

use serde_json::json;

use super::*;
use crate::backend::compact;

pub(super) fn turn(store: &dyn RecordStore, thread: &ThreadKey, turn_id: &str) -> Result<TurnView> {
    with_session(store, &thread.session, |txn| {
        let turn = require_turn(txn, turn_id)?;
        if turn.thread_id != thread.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        Ok(turn.view(thread))
    })
}

/// Writer-side view of a thread: journal position, fence state and the active turn.
pub(super) fn thread_state(store: &dyn RecordStore, thread: &ThreadKey) -> Result<ThreadStateView> {
    with_session(store, &thread.session, |txn| {
        let data = require_thread(txn, thread)?;
        let current = current_turn(txn, &thread.thread_id)?;
        Ok(ThreadStateView {
            definition: data.definition.clone(),
            status: data.status,
            journal_position: data.journal_position,
            epoch: data.epoch,
            owner: current.as_ref().and_then(|turn| turn.owner.clone()),
            lease_until: current.as_ref().map(|turn| turn.expires_at).unwrap_or(0),
            current_turn_id: current.as_ref().map(|turn| turn.turn_id.clone()),
            checkpoint_metadata: data.checkpoint_metadata.clone(),
        })
    })
}

/// Lease-free journal read: readers must not need the writer's ownership to catch up.
pub(super) fn read_journal(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    after: u64,
    limit: usize,
) -> Result<Vec<Value>> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        let data = require_thread(txn, thread)?;
        if after > data.journal_position {
            return Ok(Vec::new());
        }
        let prefix = journal_prefix(&thread.thread_id, &data.journal_branch);
        // Journal parts end in their zero-padded position, so the lexical order *is* the sequence
        // order and the cursor can be a key: asking for "everything after the entry I already
        // read" then costs the page, not the history in front of it. Without the cursor every
        // page of a long journal re-walked the whole prefix — quadratic reads on the one path a
        // tick takes before it can decide anything.
        let cursor = journal_part(&thread.thread_id, &data.journal_branch, after);
        let mut entries = Vec::new();
        for part in txn.keys(&prefix, Some(&cursor), limit)? {
            if let Some(entry) = txn.get(&part)? {
                entries.push(entry);
            }
        }
        crate::traffic::count_journal_read(entries.len());
        Ok(entries)
    })
}

pub(super) fn turns(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    cursor: Option<&str>,
    limit: usize,
) -> Result<TurnPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let ids = queue_ids(txn, &thread.thread_id)?;
        let start = match cursor {
            Some(cursor) => {
                let index: usize = cursor
                    .parse()
                    .map_err(|_| DurableError::invalid("Invalid pagination cursor"))?;
                index + 1
            }
            None => 0,
        };
        let mut items = Vec::new();
        for (index, id) in ids.iter().enumerate().skip(start) {
            if items.len() == limit {
                return Ok(TurnPage {
                    items,
                    cursor: Some((index - 1).to_string()),
                });
            }
            let turn = require_turn(txn, id)?;
            items.push(turn.view(thread));
        }
        Ok(TurnPage {
            items,
            cursor: None,
        })
    })
}

/// The message projection is produced by the adapter; the backend only exposes what it stored.
///
/// Until an adapter publishes a transcript, the one projection the store already holds is each
/// turn's committed output, in queue order; a turn that produced none is skipped.
pub(super) fn history(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    cursor: Option<&str>,
    limit: usize,
) -> Result<HistoryPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let ids = queue_ids(txn, &thread.thread_id)?;
        let start = match cursor {
            Some(cursor) => {
                let index: usize = cursor
                    .parse()
                    .map_err(|_| DurableError::invalid("Invalid pagination cursor"))?;
                index + 1
            }
            None => 0,
        };
        let mut items = Vec::new();
        for (index, id) in ids.iter().enumerate().skip(start) {
            if items.len() == limit {
                return Ok(HistoryPage {
                    items,
                    cursor: Some((index - 1).to_string()),
                });
            }
            let turn = require_turn(txn, id)?;
            if turn.output.is_null() {
                continue;
            }
            items.push(json!({ "turnId": turn.turn_id, "output": turn.output }));
        }
        Ok(HistoryPage {
            items,
            cursor: None,
        })
    })
}

pub(super) fn ready(
    store: &dyn RecordStore,
    selectors: &[HarnessSelector],
    limit: usize,
) -> Result<Vec<ThreadSpec>> {
    let limit = clamp_limit(limit)?;
    if selectors.is_empty() {
        return Err(DurableError::invalid("At least one selector is required"));
    }
    let now = crate::now_millis();
    let mut candidates: Vec<(i64, String, Option<serde_json::Value>)> = Vec::new();
    for selector in selectors {
        selector.validate()?;
        let queue = match (&selector.definition_version, &selector.state_version) {
            (Some(_), Some(_)) => format!("thread-version:{}", digest_of(selector)?),
            _ => format!("thread:{}", selector.harness),
        };
        for item in store.ready(&queue, now, limit)? {
            candidates.push((item.ready_at, item.work_id, item.payload));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut seen = std::collections::BTreeSet::new();
    let mut selected = Vec::new();
    for (_, work_id, payload) in candidates {
        if !seen.insert(work_id.clone()) {
            continue;
        }
        let Some(payload) = payload else { continue };
        let definition: ThreadSpec = serde_json::from_value(payload)?;
        selected.push(definition);
        if selected.len() == limit {
            break;
        }
    }
    Ok(selected)
}

pub(super) fn checkpoints(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    after: u64,
    limit: usize,
) -> Result<Vec<u64>> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let prefix = checkpoint_history_prefix(&thread.thread_id);
        let mut positions = Vec::new();
        for part in txn.keys(&prefix, None, 10_000)? {
            if let Some(position) = part
                .rsplit(':')
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            {
                if position > after {
                    positions.push(position);
                }
            }
        }
        positions.sort_unstable();
        positions.truncate(limit);
        Ok(positions)
    })
}

pub(super) fn checkpoint(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    position: u64,
) -> Result<CheckpointView> {
    with_session(store, &thread.session, |txn| {
        let data = require_thread(txn, thread)?;
        let record = load_json::<CheckpointRecord>(
            txn,
            &checkpoint_history_part(&thread.thread_id, position),
        )?
        .ok_or_else(|| DurableError::not_found("Checkpoint does not exist"))?;
        Ok(CheckpointView {
            definition: data.definition,
            metadata: record.metadata,
            checkpoint: record.checkpoint,
            journal_position: record.journal_position,
            settled: record.settled,
        })
    })
}

pub(super) fn events(
    store: &dyn RecordStore,
    session: &SessionKey,
    cursor: Option<&str>,
    limit: usize,
) -> Result<EventPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, session, |txn| {
        let state = require_state(txn)?;
        let position = match cursor {
            // No cursor means "from the retained start": a session that pruned its oldest events
            // must still be readable from the beginning of what is left, instead of answering
            // `cursor_expired` to its own first read.
            None | Some("") => state.retained_after,
            Some(cursor) => parse_cursor(&state, cursor)?,
        };
        if position < state.retained_after {
            return Err(DurableError::cursor_expired(
                "Cursor expired; load a snapshot",
            ));
        }
        let mut items = Vec::new();
        let mut last = position;
        for sequence in position + 1..=state.sequence {
            if items.len() == limit {
                break;
            }
            if let Some(event) = load_json::<PublicEvent>(txn, &event_part(sequence))? {
                items.push(event);
                last = sequence;
            }
        }
        Ok(EventPage {
            events: items,
            cursor: Some(cursor_of(&state, last)),
            high_watermark: Some(cursor_of(&state, state.sequence)),
        })
    })
}

/// Drops the oldest public events, keeping the newest `keep_newest`.
///
/// The watermark is what makes this safe to expose: a client that asks for a cursor below it is
/// told to load a snapshot instead of being handed a silent gap.
pub(super) fn prune_events(
    store: &dyn RecordStore,
    session: &SessionKey,
    keep_newest: u64,
) -> Result<u64> {
    with_session(store, session, |txn| {
        let mut state = require_state(txn)?;
        let dropped = compact::prune(txn, &mut state, keep_newest)?;
        if dropped > 0 {
            save_state(txn, &state)?;
        }
        Ok(dropped)
    })
}

pub(super) fn snapshot(store: &dyn RecordStore, session: &SessionKey) -> Result<Snapshot> {
    with_session(store, session, |txn| {
        let state = require_state(txn)?;
        let mut turns = Vec::new();
        for part in txn.keys(&turn_order_prefix(), None, 100_000)? {
            let Some(turn_id) = load_json::<String>(txn, &part)? else {
                continue;
            };
            let turn = require_turn(txn, &turn_id)?;
            let thread = require_thread(
                txn,
                &ThreadKey::new(session.clone(), turn.thread_id.clone())?,
            )?;
            turns.push(turn.view(&thread.definition.key));
        }
        Ok(Snapshot {
            session: state.session.key.clone(),
            turns,
            projection: json!({}),
            sequence: state.sequence.to_string(),
            cursor: cursor_of(&state, state.sequence),
        })
    })
}
