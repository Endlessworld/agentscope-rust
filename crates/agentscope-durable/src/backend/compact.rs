//! Keeping one session scope from growing without bound.
//!
//! A scope is one document that every commit rewrites, so its size is a write multiplier: 27 MB
//! of scope turns one journal batch into 27 MB of disk traffic. Three kinds of fact can be given
//! up without weakening anything, and this module gives them up:
//!
//! * **Superseded checkpoint payloads.** Every commit that changes the native state stores it in
//!   its `state/checkpoint` entry, and a replay only ever resumes from the *newest* one. The
//!   older payloads are megabytes nothing reads. The entry itself stays: it carries sequence
//!   numbers and turn ids the transcript and the turn views still need. This is the same rule
//!   core applies to `AtomicThreadJournal` (`reclaim_superseded_checkpoints`).
//! * **Checkpoint history records.** They exist so a caller can read an older position; after a
//!   compaction only the current position is kept, which is the only one recovery ever asks for.
//! * **Consumed public events.** `prune_events` drops the oldest ones and advances
//!   `retained_after`, the watermark `events()` already refuses a cursor below.
//!
//! What is deliberately *never* dropped: journal entries that name a tool outcome (dispatch,
//! result, uncertainty), message entries a transcript is rebuilt from, baseline/restore entries,
//! and any event a client has not been told a replacement for.
//!
//! This is also a hard boundary for model prompt caching. The messages rebuilt from those
//! entries are the exact prefix sent on every later model call; rewriting a `tool/result` into a
//! summary — even one that fits the model's context — changes that prefix and invalidates the
//! provider's cached tokens. Retention must only ever reclaim payloads that never reach a model
//! request (superseded native state checkpoints, checkpoint history, consumed public events).
//!
//! A trim is not free: it walks three key ranges of the document. A scope that cannot shrink — a
//! transcript longer than the event window, a baseline it must keep — would be walked again on
//! every commit and give back almost nothing, which is a permanent fee on a session that simply
//! is big. [`worth_compacting`] therefore asks for the scope to have grown by a tenth since the
//! previous trim measured its result before it pays for another one.

use serde_json::Value;

use super::*;

/// How big one scope document may get before a commit trims it.
///
/// The number is a compromise between two costs. Below it a commit costs one write of a document
/// that still holds a handful of conversation states (a few hundred kilobytes, which is what a
/// normal session needs); above it every commit would rewrite every state the session ever had.
/// One mebibyte is a few dozen state payloads for a real conversation, so a session crosses it
/// after noticeable work rather than after every turn, and once trimmed it falls back well below
/// it — the trim itself is amortised over the commits that follow.
pub(super) const COMPACT_THRESHOLD_BYTES: u64 = 1 << 20;

/// Public events a compaction keeps.
///
/// Events are the client-facing log, not durable state: a client that reconnects reads the
/// newest window (or a snapshot, through `query::snapshot`). A thousand is several turns of
/// replay for a streaming client, and the watermark keeps an old cursor from silently reading a
/// gap.
pub(super) const EVENT_RETENTION: u64 = 1024;

/// How much a scope must have grown since its last trim, as a fraction of its current size, for
/// another trim to be worth its scans.
///
/// The growth since the last trim is an upper bound on what a new trim can free: the payloads and
/// checkpoint history it gave up are gone, and what came back is the events written since. Below
/// half the scope the three key walks would cost what they cost the first time and free next to
/// nothing, and a full-document store would rewrite the same bytes for a few new ones; the trim
/// waits until the scope has doubled. The absolute ceiling below still bounds a scope that grows
/// too slowly to ever double.
const MIN_GAIN_DIVISOR: u64 = 2;

/// A scope this large is trimmed whatever its growth, so a scope that cannot shrink (or grows
/// just under the fraction) cannot sit above this size forever.
///
/// The fractional rule alone leaves a document that the last trim could not shrink above the
/// threshold permanently: the trim it skips is the only thing that would reclaim what it already
/// dropped. Twelve mebibytes is large enough to amortise the walk over a real session and small
/// enough that one document does not become a second database.
const COMPACT_CEILING_BYTES: u64 = 12 << 20;

/// Whether a scope of `bytes` is worth compacting, given the size the last trim left behind.
///
/// `after` is the scope size recorded by the commit that followed the last trim, or `0` when no
/// trim has been measured yet (a scope written before the field existed, or one whose compaction
/// is still the newest commit): nothing is known to be gone, so the first scope over the
/// threshold is trimmed.
///
/// A store that cannot measure the scope never gets here — the caller only asks when
/// `RecordStore::scope_bytes` answered `Some`.
pub(super) fn worth_compacting(bytes: u64, after: u64) -> bool {
    if bytes <= COMPACT_THRESHOLD_BYTES {
        return false;
    }
    if bytes >= COMPACT_CEILING_BYTES {
        return true;
    }
    after == 0 || bytes.saturating_sub(after) >= bytes / MIN_GAIN_DIVISOR
}

/// The journal payload a superseded checkpoint is reduced to.
///
/// `state/checkpoint` folds only when the payload carries `stateJson`; without it the fold keeps
/// the state from the newest payload it did read, which is exactly what a replay must do.
const RECLAIMED_PAYLOAD: &str = "{\"reclaimed\":true}";

/// The journal event type whose payload holds a native state.
///
/// `migration/baseline` and `state/restored` carry one too, and are deliberately *not* listed: a
/// baseline is the only copy of the transcript it carries (the transcript fold reads messages from
/// those two types and from message entries), so its payload is not interchangeable with a later
/// checkpoint's.
const CHECKPOINT_EVENT: &str = "state/checkpoint";

/// What one compaction dropped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompactOutcome {
    /// Superseded checkpoint payloads replaced by a marker.
    pub payloads: u64,
    /// Checkpoint history records deleted.
    pub checkpoints: u64,
    /// Public events pruned.
    pub events: u64,
}

impl CompactOutcome {
    pub fn is_empty(&self) -> bool {
        self.payloads == 0 && self.checkpoints == 0 && self.events == 0
    }
}

/// Trims one scope in place. `state` is updated where it has to be (the event watermark).
pub(super) fn run(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    thread: &ThreadData,
    keep_events: u64,
) -> Result<CompactOutcome> {
    let mut outcome = CompactOutcome::default();
    reclaim_payloads(txn, thread, &mut outcome)?;
    drop_history(txn, thread, &mut outcome)?;
    outcome.events = prune(txn, state, keep_events)?;
    Ok(outcome)
}

/// Replaces every state payload but the newest with [`RECLAIMED_PAYLOAD`].
///
/// Only `state/checkpoint` entries are touched. A baseline or a restore is the *only* copy of the
/// transcript it carries (the transcript fold reads messages from those two and from message
/// entries), so its payload is not interchangeable with a later checkpoint's.
fn reclaim_payloads(
    txn: &mut dyn Txn,
    thread: &ThreadData,
    outcome: &mut CompactOutcome,
) -> Result<()> {
    let thread_id = &thread.definition.key.thread_id;
    let prefix = journal_prefix(thread_id, &thread.journal_branch);
    // Two passes rather than one: the newest payload has to be known before an older one can be
    // judged superseded, and it is the last entry of the branch. Both passes page: the walk is
    // over the whole branch, and asking for it in one call truncated a long journal silently.
    let mut newest: Option<String> = None;
    let mut stale: Vec<String> = Vec::new();
    scan_keys(txn, &prefix, None, |txn, part| {
        let Some(entry) = txn.get(part)? else {
            return Ok(true);
        };
        if entry.get("type").and_then(Value::as_str) != Some(CHECKPOINT_EVENT) {
            return Ok(true);
        }
        let Some(payload) = entry.get("payloadJson").and_then(Value::as_str) else {
            return Ok(true);
        };
        // Already reclaimed: the fold ignores it, and rewriting it again would only cost a write.
        if !carries_state(payload) {
            return Ok(true);
        }
        if let Some(previous) = newest.replace(part.to_string()) {
            stale.push(previous);
        }
        Ok(true)
    })?;
    for part in stale {
        let Some(mut entry) = txn.get(&part)? else {
            continue;
        };
        entry["payloadJson"] = Value::String(RECLAIMED_PAYLOAD.to_string());
        txn.put(&part, Some(entry))?;
        outcome.payloads += 1;
    }
    Ok(())
}

fn carries_state(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|data| data.get("stateJson").map(|_| ()))
        .is_some()
}

/// Deletes checkpoint history records older than the current position.
///
/// The current one is what `execution_host::open` and `checkpoint()` read (through
/// `checkpoint_metadata.position`); an older position could only be read by a caller asking for
/// a state the thread has already moved past.
fn drop_history(
    txn: &mut dyn Txn,
    thread: &ThreadData,
    outcome: &mut CompactOutcome,
) -> Result<()> {
    let thread_id = &thread.definition.key.thread_id;
    // Positions are zero-padded in the key, so the walk is in order and everything from the
    // current position on is kept: the scan stops at the first one instead of reading the whole
    // history it is about to leave alone.
    scan_keys(
        txn,
        &checkpoint_history_prefix(thread_id),
        None,
        |txn, part| {
            let Some(position) = part
                .rsplit(':')
                .next()
                .and_then(|raw| raw.parse::<u64>().ok())
            else {
                return Ok(true);
            };
            if position >= thread.checkpoint_position {
                return Ok(false);
            }
            txn.put(part, None)?;
            outcome.checkpoints += 1;
            Ok(true)
        },
    )?;
    Ok(())
}

/// Drops every public event outside the newest `keep` and advances the watermark.
///
/// The `event-id` marker of a dropped event goes with it. It exists to make a *replayed* commit
/// idempotent, and a marker without its event can only do the opposite: the replay would append
/// the dropped event again under a new sequence. Keeping the markers instead made the session
/// document grow by one key per event forever, which is the growth pruning exists to stop.
pub(super) fn prune(txn: &mut dyn Txn, state: &mut SessionState, keep: u64) -> Result<u64> {
    let floor = state.sequence.saturating_sub(keep);
    if floor <= state.retained_after {
        return Ok(0);
    }
    let mut dropped = 0u64;
    scan_keys(txn, &event_prefix(), None, |txn, part| {
        let Some(sequence) = part
            .rsplit(':')
            .next()
            .and_then(|raw| raw.parse::<u64>().ok())
        else {
            return Ok(true);
        };
        if sequence > floor {
            // Sequence order is key order: nothing after this one is prunable either.
            return Ok(false);
        }
        if let Some(event) = load_json::<PublicEvent>(txn, part)? {
            txn.put(&event_id_part(&event.event_id), None)?;
        }
        txn.put(part, None)?;
        dropped += 1;
        Ok(true)
    })?;
    state.retained_after = floor;
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_under_the_threshold_is_never_compacted() {
        assert!(!worth_compacting(0, 0));
        assert!(!worth_compacting(COMPACT_THRESHOLD_BYTES, 0));
    }

    #[test]
    fn the_first_scope_over_the_threshold_is_compacted() {
        assert!(worth_compacting(COMPACT_THRESHOLD_BYTES + 1, 0));
    }

    #[test]
    fn a_scope_that_hardly_grew_since_its_last_trim_is_left_alone() {
        // A scope the last trim could not shrink stays just over the threshold; walking it again
        // for a few kilobytes is the permanent fee this guard exists to stop.
        let after = COMPACT_THRESHOLD_BYTES + 300 * 1024;
        assert!(!worth_compacting(after + after / 20, after));
        assert!(!worth_compacting(after, after));
        // Shrunk since (another writer pruned, or an explicit `prune_events`): nothing to do
        // until it grows back past twice the last result.
        assert!(!worth_compacting(after - 1, after));
        // The rule is half of the scope *now*, so it fires at twice the last result.
        let grown = after * 2;
        assert!(!worth_compacting(grown - 2, after));
        assert!(worth_compacting(grown, after));
    }

    #[test]
    fn a_scope_over_the_ceiling_is_compacted_whatever_it_grew() {
        // The fractional rule alone would leave a scope that cannot shrink above the threshold
        // forever; the ceiling is the escape hatch.
        assert!(worth_compacting(
            COMPACT_CEILING_BYTES,
            COMPACT_CEILING_BYTES
        ));
        assert!(worth_compacting(
            COMPACT_CEILING_BYTES + 1,
            COMPACT_CEILING_BYTES
        ));
    }

    #[test]
    fn the_reclaimed_payload_is_valid_json_without_a_state() {
        // Both halves matter: the journal validates that a payload parses, and the projection
        // treats "no stateJson" as "keep the state folded so far".
        let data: Value = serde_json::from_str(RECLAIMED_PAYLOAD).expect("valid json");
        assert!(data.get("stateJson").is_none());
        assert!(!carries_state(RECLAIMED_PAYLOAD));
        assert!(carries_state(r#"{"stateJson":"{}"}"#));
    }

    #[test]
    fn only_a_checkpoint_payload_is_reclaimable() {
        // `message/*` and `tool/result` entries are how a transcript is rebuilt, and a baseline is
        // the only copy of the messages it carries; `reclaim_payloads` looks at checkpoints only.
        assert_eq!(CHECKPOINT_EVENT, "state/checkpoint");
        assert_ne!(CHECKPOINT_EVENT, "migration/baseline");
    }
}
