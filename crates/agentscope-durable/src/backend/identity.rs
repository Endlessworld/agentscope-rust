//! Session and thread identity: creation, lookup, closing.

use serde_json::json;

use super::*;

pub(super) const PRIMARY_THREAD: &str = "main";

pub(super) fn create(store: &dyn RecordStore, spec: SessionSpec) -> Result<SessionView> {
    let session = spec.key.clone();
    let digest = digest_of(&spec)?;
    with_session(store, &session, |txn| {
        if let Some(state) = load_state(txn)? {
            if state.creation_digest != digest {
                return Err(DurableError::conflict("Session specification changed"));
            }
            return Ok(state.session);
        }
        let now = txn.now();
        let thread = ThreadData::new(ThreadSpec {
            key: ThreadKey::new(session.clone(), PRIMARY_THREAD)?,
            binding: spec.primary_binding.clone(),
        });
        save_thread(txn, &thread)?;
        let view = SessionView {
            key: session.clone(),
            primary_thread_id: PRIMARY_THREAD.to_string(),
            status: Lifecycle::Open,
            metadata: spec.metadata.clone(),
        };
        let mut state = SessionState {
            format: CURRENT_FORMAT,
            session: view.clone(),
            creation_digest: digest,
            admitted: 0,
            sequence: 0,
            retained_after: 0,
            compacted_bytes: 0,
            compact_pending: false,
        };
        let key = thread.definition.key.clone();
        append_event(
            txn,
            &mut state,
            &key,
            "",
            None,
            None,
            "session.created",
            "session.created",
            json!({ "primaryThreadId": PRIMARY_THREAD }),
        )?;
        append_event(
            txn,
            &mut state,
            &key,
            "",
            None,
            None,
            "thread.created",
            "thread.created",
            json!({ "threadId": PRIMARY_THREAD }),
        )?;
        schedule_thread(txn, &thread, None, now)?;
        save_state(txn, &state)?;
        Ok(view)
    })
}

pub(super) fn session(store: &dyn RecordStore, key: &SessionKey) -> Result<SessionView> {
    with_session(store, key, |txn| {
        let state = require_state(txn)?;
        Ok(state.session)
    })
}

pub(super) fn close_session(store: &dyn RecordStore, key: &SessionKey) -> Result<SessionView> {
    with_session(store, key, |txn| {
        let mut state = require_state(txn)?;
        if state.session.status == Lifecycle::Closed {
            return Ok(state.session);
        }
        for thread_id in thread_ids(txn)? {
            if current_turn(txn, &thread_id)?.is_some() {
                return Err(DurableError::conflict("Session has unfinished turns"));
            }
        }
        state.session.status = Lifecycle::Closed;
        let primary = state.session.key.clone();
        let target = ThreadKey::new(primary, state.session.primary_thread_id.clone())?;
        append_event(
            txn,
            &mut state,
            &target,
            "",
            None,
            None,
            "session.closed",
            "session.closed",
            json!({}),
        )?;
        let view = state.session.clone();
        save_state(txn, &state)?;
        Ok(view)
    })
}

pub(super) fn create_thread(store: &dyn RecordStore, spec: ThreadSpec) -> Result<ThreadView> {
    let session = spec.key.session.clone();
    with_session(store, &session, |txn| {
        let mut state = require_state(txn)?;
        if state.session.status != Lifecycle::Open {
            return Err(DurableError::conflict("Session is closed"));
        }
        let now = txn.now();
        if let Some(existing) = load_thread(txn, &spec.key.thread_id)? {
            if existing.definition != spec {
                return Err(DurableError::conflict("Thread binding is immutable"));
            }
            return Ok(ThreadView {
                definition: existing.definition,
                status: existing.status,
            });
        }
        let thread = ThreadData::new(spec.clone());
        save_thread(txn, &thread)?;
        append_event(
            txn,
            &mut state,
            &spec.key,
            "",
            None,
            None,
            &format!("thread.created.{}", spec.key.thread_id),
            "thread.created",
            json!({ "threadId": spec.key.thread_id }),
        )?;
        schedule_thread(txn, &thread, None, now)?;
        save_state(txn, &state)?;
        Ok(ThreadView {
            definition: spec,
            status: Lifecycle::Open,
        })
    })
}

pub(super) fn thread(store: &dyn RecordStore, key: &ThreadKey) -> Result<ThreadView> {
    with_session(store, &key.session, |txn| {
        let thread = require_thread(txn, key)?;
        Ok(ThreadView {
            definition: thread.definition,
            status: thread.status,
        })
    })
}

pub(super) fn close_thread(store: &dyn RecordStore, key: &ThreadKey) -> Result<ThreadView> {
    with_session(store, &key.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, key)?;
        if thread.status == Lifecycle::Closed {
            return Ok(ThreadView {
                definition: thread.definition,
                status: thread.status,
            });
        }
        if current_turn(txn, &key.thread_id)?.is_some() {
            return Err(DurableError::conflict("Thread has unfinished turns"));
        }
        thread.status = Lifecycle::Closed;
        let now = txn.now();
        append_event(
            txn,
            &mut state,
            key,
            "",
            None,
            None,
            &format!("thread.closed.{}", key.thread_id),
            "thread.closed",
            json!({ "threadId": key.thread_id }),
        )?;
        schedule_thread(txn, &thread, None, now)?;
        let view = ThreadView {
            definition: thread.definition.clone(),
            status: thread.status,
        };
        save_thread(txn, &thread)?;
        save_state(txn, &state)?;
        Ok(view)
    })
}

pub(super) fn threads(
    store: &dyn RecordStore,
    session: &SessionKey,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ThreadPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, session, |txn| {
        require_state(txn)?;
        let parts = txn.keys(&thread_prefix(), cursor, limit + 1)?;
        let has_more = parts.len() > limit;
        let mut items = Vec::with_capacity(limit.min(parts.len()));
        let mut last = None;
        for part in parts.into_iter().take(limit) {
            if let Some(thread) = load_json::<ThreadData>(txn, &part)? {
                items.push(ThreadView {
                    definition: thread.definition,
                    status: thread.status,
                });
                last = Some(part);
            }
        }
        Ok(ThreadPage {
            items,
            cursor: if has_more { last } else { None },
        })
    })
}

fn thread_ids(txn: &mut dyn Txn) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    // Paged rather than capped: the caller uses this to decide whether a session may close, and a
    // truncated list silently forgets the threads past the cap.
    scan_keys(txn, &thread_prefix(), None, |txn, part| {
        if let Some(thread) = load_json::<ThreadData>(txn, part)? {
            ids.push(thread.definition.key.thread_id);
        }
        Ok(true)
    })?;
    Ok(ids)
}
