//! Read-side queries: turns, events, readiness, checkpoints, snapshots.

use serde_json::json;

use super::*;
use crate::backend::compact;

pub(super) fn turn(store: &dyn RecordStore, thread: &ThreadKey, turn_id: &str) -> Result<TurnView> {
    with_session_read(store, &thread.session, |txn| {
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
    with_session_read(store, &thread.session, |txn| {
        let data = require_thread(txn, thread)?;
        let current = current_turn(txn, &data)?;
        Ok(ThreadStateView {
            definition: data.definition.clone(),
            status: data.status,
            journal_position: data.journal_position,
            epoch: data.epoch,
            owner: current.as_ref().and_then(|turn| turn.owner.clone()),
            lease_until: current.as_ref().map(|turn| turn.expires_at).unwrap_or(0),
            current_turn_id: current.as_ref().map(|turn| turn.turn_id.clone()),
            checkpoint_metadata: data.checkpoint_metadata.clone(),
            resource_position: data.resource_position,
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
    with_session_read(store, &thread.session, |txn| {
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
        let parts = txn.keys(&prefix, Some(&cursor), limit)?;
        let entries: Vec<Value> = txn.get_all(&parts)?.into_iter().flatten().collect();
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
    with_session_read(store, &thread.session, |txn| {
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

/// The thread's message transcript, rebuilt from the committed journal.
///
/// The journal is the only durable record of the conversation: `message/user`,
/// `message/assistant`, `message/system` and `tool/result` carry the messages the loop actually
/// applied, in commit order. The projection is a pure read — it writes nothing and needs no new
/// fact — and a thread whose journal holds no message (a scope written before these facts existed,
/// or one that never ran) answers an empty page instead of an invented one.
///
/// The cursor is the journal sequence of the last returned message, so the next page starts after
/// it: paging a long conversation costs the pages walked, not the history in front of them.
pub(super) fn history(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    cursor: Option<&str>,
    limit: usize,
) -> Result<HistoryPage> {
    /// A journal page is capped by the backend at 1,000 entries.
    const PAGE: usize = 1000;
    let limit = clamp_limit(limit)?;
    let mut after = match cursor {
        None | Some("") => 0,
        Some(cursor) => cursor
            .parse()
            .map_err(|_| DurableError::invalid("Invalid pagination cursor"))?,
    };
    let mut items = Vec::new();
    loop {
        let page = read_journal(store, thread, after, PAGE)?;
        if page.is_empty() {
            return Ok(HistoryPage {
                items,
                cursor: None,
            });
        }
        let full_page = page.len() == PAGE;
        for entry in page {
            let Some(sequence) = entry.get("seq").and_then(Value::as_u64) else {
                continue;
            };
            if sequence <= after {
                continue;
            }
            after = sequence;
            let Some(item) = message_item(&entry, sequence) else {
                continue;
            };
            items.push(item);
            if items.len() == limit {
                return Ok(HistoryPage {
                    items,
                    cursor: Some(sequence.to_string()),
                });
            }
        }
        // A short page is the end of the journal; a full one may have more behind it.
        if !full_page {
            return Ok(HistoryPage {
                items,
                cursor: None,
            });
        }
    }
}

/// One transcript item, or `None` for a fact that is not a message.
///
/// The payload's message is projected to what a transcript needs — who spoke and what they said —
/// instead of being handed back verbatim: the journal already holds the full [`Msg`], and a
/// reader asking for history does not need the metadata or the per-message usage again.
fn message_item(entry: &Value, sequence: u64) -> Option<Value> {
    let role = match entry.get("type").and_then(Value::as_str)? {
        "message/user" => "user",
        "message/assistant" => "assistant",
        "message/system" => "system",
        "tool/result" => "tool",
        _ => return None,
    };
    let payload = entry.get("payloadJson").and_then(Value::as_str)?;
    let payload: Value = serde_json::from_str(payload).ok()?;
    let message = payload.get("message")?;
    let turn_id = entry.get("turnId").and_then(Value::as_str);
    Some(json!({
        "seq": sequence.to_string(),
        "role": role,
        "messageId": message.get("id").cloned().unwrap_or(Value::Null),
        "content": message.get("content").cloned().unwrap_or(Value::Null),
        "timestamp": message.get("timestamp").cloned().unwrap_or(Value::Null),
        "turnId": turn_id,
    }))
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
    // The identity, not the work id, decides whether two entries are the same work: one thread is
    // listed under its harness queue and under its versioned twin, and a record written before the
    // work id carried the session kept the bare thread id. Deduplicating on the id alone would
    // hand the same thread to two ticks.
    let mut seen = std::collections::BTreeSet::new();
    let mut selected = Vec::new();
    for (_, _, payload) in candidates {
        let Some(payload) = payload else { continue };
        let definition: ThreadSpec = serde_json::from_value(payload)?;
        let identity = (
            definition.key.session.storage_key(),
            definition.key.thread_id.clone(),
        );
        if !seen.insert(identity) {
            continue;
        }
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
    with_session_read(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let prefix = checkpoint_history_prefix(&thread.thread_id);
        let mut positions = Vec::new();
        // The cursor is a position, and positions are the tail of the key in order: the walk
        // starts at the one the caller already read and stops as soon as the page is full,
        // instead of reading a fixed ten thousand keys and truncating them.
        let from = checkpoint_history_part(&thread.thread_id, after);
        scan_keys(txn, &prefix, Some(&from), |_txn, part| {
            let Some(position) = part
                .rsplit(':')
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                return Ok(true);
            };
            if position > after {
                positions.push(position);
            }
            Ok(positions.len() < limit)
        })?;
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
    with_session_read(store, &thread.session, |txn| {
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
    with_session_read(store, session, |txn| {
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
            // An explicit prune is a trim as well: let the next commit record the size it left, so
            // the automatic trim is not evaluated against a size the scope no longer has.
            state.compact_pending = true;
            save_state(txn, &state)?;
        }
        Ok(dropped)
    })
}

pub(super) fn snapshot(store: &dyn RecordStore, session: &SessionKey) -> Result<Snapshot> {
    with_session_read(store, session, |txn| {
        let state = require_state(txn)?;
        let mut turns = Vec::new();
        // Paged: a snapshot asks for every turn a session ever admitted, so the walk has no
        // limit — but it also must not be truncated at a fixed count.
        scan_keys(txn, &turn_order_prefix(), None, |txn, part| {
            let Some(turn_id) = load_json::<String>(txn, part)? else {
                return Ok(true);
            };
            let turn = require_turn(txn, &turn_id)?;
            let thread = require_thread(
                txn,
                &ThreadKey::new(session.clone(), turn.thread_id.clone())?,
            )?;
            turns.push(turn.view(&thread.definition.key));
            Ok(true)
        })?;
        Ok(Snapshot {
            session: state.session.key.clone(),
            turns,
            projection: json!({}),
            sequence: state.sequence.to_string(),
            cursor: cursor_of(&state, state.sequence),
        })
    })
}
