//! Timelines that branch: a tool call delegating to a child thread, and a native adapter
//! installing a settled baseline into an idle one.
//!
//! Both are rare — once per delegation, once per restore — so they may cost a handful of
//! transactions; neither runs on a tick, and neither rewrites a document a commit would rewrite
//! anyway.
//!
//! A lineage label is a journal branch. It is only ever given to a timeline that holds nothing
//! yet, because the only key walk a compaction performs is over the *current* branch: renaming the
//! branch of a history that already has entries would strand every byte under the old name.

use serde_json::json;

use super::*;

/// Registers a delegation and answers its lineage.
///
/// Idempotent per `request_id`: the child thread, its first turn and the link back to the parent
/// tool call are recorded once. A replay adopts what the first registration wrote, including the
/// one case a crash can leave behind — the link written, the receipt not.
pub(super) fn delegate(
    store: &dyn RecordStore,
    lease: &Lease,
    execution_id: &str,
    request_id: &str,
    target: DelegationTarget,
) -> Result<DelegationView> {
    json::text(request_id, "requestId")?;
    if target.child.key.session != lease.thread.session {
        return Err(DurableError::invalid(
            "A delegation must stay inside its session",
        ));
    }
    if let Some(view) = recorded(store, lease, execution_id, request_id)? {
        return Ok(view);
    }

    // The parent tool call names the lineage and holds the link a replay must adopt.
    let tool = tools::execution(store, &lease.thread, execution_id)?;
    let link = match tool.delegation.clone() {
        Some(link) => {
            // One tool call delegates once: adopting the recorded child is the idempotent answer,
            // but a request that names a different child is a mistake worth reporting.
            if link.thread != target.child.key {
                return Err(DurableError::conflict(
                    "Execution already delegates to another thread",
                ));
            }
            link
        }
        None => {
            let branch = branch_label("delegate", request_id);
            identity::create_thread_in_branch(store, target.child.clone(), Some(&branch))?;
            let parent = DelegationParent {
                thread: lease.thread.clone(),
                turn_id: lease.turn_id.clone(),
                execution_id: execution_id.to_string(),
                origin: tool.request.call.origin.clone(),
            };
            let receipt = turn::submit_with_parent(
                store,
                &target.child.key,
                request_id,
                target.input.clone(),
                Some(parent),
            )?;
            DelegationLink {
                thread: target.child.key.clone(),
                turn_id: receipt.turn_id,
            }
        }
    };

    let view = DelegationView {
        parent: tool.request,
        child: Some(link.clone()),
        cancel_requested: false,
    };
    with_session(store, &lease.thread.session, |txn| {
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let current = turn
            .tools
            .get(execution_id)
            .and_then(|tool| tool.delegation.clone());
        match current {
            // An equal link is what the first registration wrote: recording the receipt again is
            // the only work left, and it needs no lease because the fact is already committed.
            Some(current) if current == link => {}
            Some(_) => return Err(DurableError::conflict("Execution already delegates")),
            None => {
                assert_owned(&turn, lease, txn.now())?;
                if let Some(tool) = turn.tools.get_mut(execution_id) {
                    tool.delegation = Some(link.clone());
                }
                save_turn(txn, &turn)?;
            }
        }
        store_json(txn, &delegation_part(request_id), &link)?;
        Ok(view)
    })
}

/// The lineage an earlier registration of this request recorded, rebuilt from the link.
///
/// Only the link is stored: the parent half of the view is the tool call that is already in the
/// turn, and copying a tool's arguments into a second record would be pure duplication.
fn recorded(
    store: &dyn RecordStore,
    lease: &Lease,
    execution_id: &str,
    request_id: &str,
) -> Result<Option<DelegationView>> {
    with_session(store, &lease.thread.session, |txn| {
        let turn = require_turn(txn, &lease.turn_id)?;
        let Some(link) = load_json::<DelegationLink>(txn, &delegation_part(request_id))? else {
            return Ok(None);
        };
        let tool = turn
            .tools
            .get(execution_id)
            .ok_or_else(|| DurableError::not_found("Execution does not exist"))?;
        Ok(Some(DelegationView {
            parent: tool.request.clone(),
            child: Some(link),
            cancel_requested: false,
        }))
    })
}

/// The lineage label one request gives a timeline.
///
/// The prefix says where the timeline came from; the digest keeps two labels from colliding
/// without carrying a whole hash into every journal key of the branch.
fn branch_label(prefix: &str, request_id: &str) -> String {
    let digest = json::hash_text(request_id);
    format!("{prefix}-{}", &digest[..32])
}

/// Installs a settled baseline into an idle thread whose journal is still where the baseline was
/// taken.
///
/// One transaction: the checkpoint, the journal entries it expects, the resources it depends on
/// and the receipt that makes a replay cheap are all or nothing. Nothing that already exists is
/// overwritten — a checkpoint is installed as a new history position, journal entries are
/// appended after `expected_position`, and resource values are written by their own key.
pub(super) fn restore(store: &dyn RecordStore, restoration: Restoration) -> Result<ThreadView> {
    json::text(&restoration.request_id, "requestId")?;
    if restoration.checkpoint.is_null() {
        return Err(DurableError::invalid("Restoration requires a checkpoint"));
    }
    for value in &restoration.resources {
        value.validate()?;
    }
    let target = restoration.target.clone();
    with_session(store, &target.key.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, &target.key)?;
        if thread.definition != target {
            return Err(DurableError::conflict("Thread binding is immutable"));
        }
        if let Some(view) = load_json::<ThreadView>(txn, &restore_part(&restoration.request_id))? {
            return Ok(view);
        }
        if thread.status != Lifecycle::Open {
            return Err(DurableError::conflict("Thread is closed"));
        }
        // A baseline is installed into an idle thread: a live turn would be written on top of the
        // state this installation replaces, and a moved journal means the baseline is stale.
        if current_turn(txn, &thread)?.is_some() {
            return Err(DurableError::conflict(
                "Restoration requires an idle thread",
            ));
        }
        if restoration.expected_position != thread.journal_position {
            return Err(DurableError::conflict("Journal position changed"));
        }
        // See the module comment: only a timeline with nothing in it may be renamed, so a restore
        // on top of existing history stays a continuation of that history.
        if thread.journal_position == 0 && thread.checkpoint_metadata.is_none() {
            thread.journal_branch = branch_label("restore", &restoration.request_id);
        }
        for entry in &restoration.journal {
            thread.journal_position += 1;
            store_json(
                txn,
                &journal_part(
                    &target.key.thread_id,
                    &thread.journal_branch,
                    thread.journal_position,
                ),
                entry,
            )?;
        }
        for value in &restoration.resources {
            store_json(txn, &resource_part(&value.namespace, &value.key), value)?;
            thread.resource_position += 1;
        }
        // The baseline is a settled checkpoint: it becomes the newest history position, so the
        // previous one stays readable and the thread resumes from the installed state.
        thread.checkpoint_position += 1;
        let metadata = CheckpointMetadata {
            schema_version: CHECKPOINT_SCHEMA_VERSION.to_string(),
            harness: target.harness().to_string(),
            definition_version: target.definition_version().to_string(),
            state_version: target.state_version().to_string(),
            position: thread.checkpoint_position.to_string(),
            digest: json::hash(&restoration.checkpoint)?,
        };
        let record = CheckpointRecord {
            metadata: metadata.clone(),
            checkpoint: restoration.checkpoint.clone(),
            branch: thread.journal_branch.clone(),
            journal_position: thread.journal_position,
            resource_position: thread.resource_position,
            settled: true,
        };
        txn.put(
            &checkpoint_part(&target.key.thread_id),
            Some(restoration.checkpoint.clone()),
        )?;
        store_json(
            txn,
            &checkpoint_history_part(&target.key.thread_id, thread.checkpoint_position),
            &record,
        )?;
        thread.checkpoint_metadata = Some(metadata);
        append_event(
            txn,
            &mut state,
            &target.key,
            "",
            None,
            None,
            &format!("thread.restored.{}", restoration.request_id),
            "thread.restored",
            json!({
                "checkpointPosition": thread.checkpoint_position,
                "journalPosition": thread.journal_position,
                "resourcePosition": thread.resource_position,
            }),
        )?;
        save_thread(txn, &thread)?;
        save_state(txn, &state)?;
        let view = ThreadView {
            definition: thread.definition.clone(),
            status: thread.status,
        };
        store_json(txn, &restore_part(&restoration.request_id), &view)?;
        Ok(view)
    })
}
