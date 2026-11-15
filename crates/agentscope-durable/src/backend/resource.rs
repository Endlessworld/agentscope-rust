//! Session-private resources: one record per value, one receipt per mutation.
//!
//! A resource write is small and never touches the conversation: the value lives in its own part
//! and the only document a mutation rewrites is the thread record, for the generation counter.
//! Reads write nothing at all — `GET`/`SEARCH` take the fence they are authenticated by and no
//! more, so polling a resource costs no bytes.

use serde::{Deserialize, Serialize};

use super::*;

/// How many matching values one search answers when the request sets no limit.
const RESOURCE_PAGE: usize = 1_000;

/// The answer one mutation produced, recorded so a replayed request costs a read and no write.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResourceReceipt {
    digest: String,
    values: Vec<ResourceValue>,
}

pub(super) fn apply(
    store: &dyn RecordStore,
    lease: &Lease,
    request: ResourceRequest,
) -> Result<Vec<ResourceValue>> {
    request.validate()?;
    with_session(store, &lease.thread.session, |txn| {
        let turn = require_turn(txn, &lease.turn_id)?;
        if turn.thread_id != lease.thread.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        // Reads are authenticated by the lease exactly like writes: a resource answer is part of
        // what the holder of the turn observed, and the fence is what says it still holds.
        assert_owned(&turn, lease, txn.now())?;
        match request.operation {
            ResourceOperation::Get => Ok(read(txn, &request)?.into_iter().collect()),
            ResourceOperation::Search => search(txn, &request),
            _ => mutate(txn, lease, &request),
        }
    })
}

fn key_of(request: &ResourceRequest) -> String {
    request.key.clone().unwrap_or_default()
}

fn read(txn: &dyn Txn, request: &ResourceRequest) -> Result<Option<ResourceValue>> {
    let key = key_of(request);
    load_json::<ResourceValue>(txn, &resource_part(&request.namespace, &key))
}

/// Every value whose namespace starts with the request's, ordered by namespace then key.
///
/// Offset paging needs a stable order, so the matching values of the session are read and sorted
/// before the window is taken. The scan is over resource records only — small values kept apart
/// from the conversation — never over turns or events.
fn search(txn: &dyn Txn, request: &ResourceRequest) -> Result<Vec<ResourceValue>> {
    let limit = match request.limit {
        0 => RESOURCE_PAGE,
        limit => (limit as usize).min(RESOURCE_PAGE),
    };
    let mut matching = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = txn.keys(&resource_prefix(), cursor.as_deref(), RESOURCE_PAGE)?;
        let full = page.len() == RESOURCE_PAGE;
        for part in &page {
            let Some(value) = load_json::<ResourceValue>(txn, part)? else {
                continue;
            };
            if value.namespace.starts_with(&request.namespace) {
                matching.push(value);
            }
        }
        cursor = page.last().cloned();
        if !full || cursor.is_none() {
            break;
        }
    }
    matching
        .sort_by(|left, right| (&left.namespace, &left.key).cmp(&(&right.namespace, &right.key)));
    let offset = request.offset as usize;
    Ok(matching.into_iter().skip(offset).take(limit).collect())
}

/// Applies a `PUT`/`CAS`/`DELETE` once and answers what the first application produced.
fn mutate(
    txn: &mut dyn Txn,
    lease: &Lease,
    request: &ResourceRequest,
) -> Result<Vec<ResourceValue>> {
    let digest = digest_of(request)?;
    if let Some(receipt) =
        load_json::<ResourceReceipt>(txn, &resource_receipt_part(&request.request_id))?
    {
        if !digest_matches(&receipt.digest, request)? {
            return Err(DurableError::conflict("Resource request changed"));
        }
        return Ok(receipt.values);
    }
    let mut thread = require_thread(txn, &lease.thread)?;
    let existing = read(txn, request)?;
    let current = existing.as_ref().map(|value| value.version).unwrap_or(0);
    let values = match request.operation {
        ResourceOperation::Put | ResourceOperation::Cas => {
            if request.operation == ResourceOperation::Cas && request.expected_version != current {
                return Err(DurableError::conflict("Resource version changed"));
            }
            let value = ResourceValue {
                namespace: request.namespace.clone(),
                key: key_of(request),
                value: request.value.clone(),
                version: current + 1,
            };
            value.validate()?;
            store_json(txn, &resource_part(&value.namespace, &value.key), &value)?;
            vec![value]
        }
        ResourceOperation::Delete => {
            let existing =
                existing.ok_or_else(|| DurableError::not_found("Resource does not exist"))?;
            txn.put(&resource_part(&existing.namespace, &existing.key), None)?;
            Vec::new()
        }
        ResourceOperation::Get | ResourceOperation::Search => {
            return Err(DurableError::invalid("Not a resource mutation"));
        }
    };
    // The generation is what a checkpoint records: a mutation that changed a value makes every
    // older baseline a different baseline, and this counter is how that is said in one number.
    thread.resource_position += 1;
    save_thread(txn, &thread)?;
    store_json(
        txn,
        &resource_receipt_part(&request.request_id),
        &ResourceReceipt {
            digest,
            values: values.clone(),
        },
    )?;
    Ok(values)
}
