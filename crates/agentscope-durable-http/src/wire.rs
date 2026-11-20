//! One wire definition for both sides of the transport.
//!
//! The client in [`crate::client`] and the axum service in [`crate::server`] must agree on every
//! path, header and status code; keeping the builders here means a route cannot drift from the
//! request that addresses it. Nothing in this module carries state or business rules.
//!
//! Routes (all under `{prefix}`, default `/v1`):
//!
//! ```text
//! POST   {prefix}/sessions                                     create
//! GET    {prefix}/sessions/{ns}/{sid}                          session
//! POST   {prefix}/sessions/{ns}/{sid}/close                    closeSession
//! POST   {prefix}/sessions/{ns}/{sid}/threads                  createThread
//! GET    {prefix}/sessions/{ns}/{sid}/threads                  threads (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}            thread
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/close      closeThread
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/state      threadState
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns      submit
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns      turns (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/history    history (after, limit)
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/control    control
//! GET    {prefix}/sessions/{ns}/{sid}/admission/{request}      admission
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/claim      claim
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}                     turn
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/signals             signal
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/lease/renew         renew
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/lease/release       release
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/frame               frame
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/control-state       controlState
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/failure             recordFailure
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/commits             commit
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/commits/{cid}/receipt receipt
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/runs                prepareRun
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/runs                runs (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/runs/{run}          run
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/runs/{run}/steps    steps (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/model-calls         modelCalls (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/journal             journal (after, limit; lease)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/journal                          readJournal (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/executions/{id}                  execution
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/dispatch            beginDispatch
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/executions/{id}/result           acceptResult
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/checkpoints                      checkpoints (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/threads/{tid}/checkpoints/{position}           checkpoint
//! GET    {prefix}/sessions/{ns}/{sid}/events                                         events (after, limit)
//! GET    {prefix}/sessions/{ns}/{sid}/events/stream                                  SSE (after, limit, intervalMs)
//! POST   {prefix}/sessions/{ns}/{sid}/events/prune                                   pruneEvents
//! GET    {prefix}/sessions/{ns}/{sid}/snapshot                                       snapshot
//! POST   {prefix}/sessions/{ns}/{sid}/resources                                      resources (lease)
//! POST   {prefix}/sessions/{ns}/{sid}/restore                                        restore
//! POST   {prefix}/sessions/{ns}/{sid}/threads/{tid}/turns/{turn}/delegate            delegate (lease)
//! POST   {prefix}/ready                                                              ready (selectors, limit)
//! ```

use agentscope_durable::{CommitReceipt, DurableError, ErrorCode, Result, SessionKey, ThreadKey};
use serde_json::Value;

/// Wire protocol version every request announces.
pub const PROTOCOL_VERSION: &str = agentscope_durable::PROTOCOL_VERSION;

/// Route prefix of the backend surface; the paths below hang off it.
pub const DEFAULT_PREFIX: &str = "/v1";

/// The page-size contract of the backend, mirrored so both sides refuse the same input.
pub const MAX_PAGE_LIMIT: usize = 1000;

pub const HEADER_VERSION: &str = "X-Durable-Version";
pub const HEADER_CONTENT_TYPE: &str = "Content-Type";
pub const HEADER_OWNER: &str = "X-Durable-Owner";
pub const HEADER_EPOCH: &str = "X-Durable-Epoch";
pub const HEADER_EXPIRES_AT: &str = "X-Durable-Expires-At";
pub const HEADER_THREAD: &str = "X-Durable-Thread";
pub const HEADER_TURN: &str = "X-Durable-Turn";
pub const HEADER_LAST_EVENT_ID: &str = "Last-Event-ID";

pub const CONTENT_TYPE_JSON: &str = "application/json";
pub const CONTENT_TYPE_SSE: &str = "text/event-stream; charset=utf-8";

// ---------------------------------------------------------------- routes

fn root(prefix: &str) -> &str {
    prefix.trim_end_matches('/')
}

pub fn sessions_path(prefix: &str) -> String {
    format!("{}/sessions", root(prefix))
}

pub fn session_path(prefix: &str, session: &SessionKey) -> String {
    format!(
        "{}/sessions/{}/{}",
        root(prefix),
        encode_segment(&session.namespace),
        encode_segment(&session.session_id)
    )
}

pub fn session_action(prefix: &str, session: &SessionKey, action: &str) -> String {
    format!("{}/{action}", session_path(prefix, session))
}

pub fn threads_path(prefix: &str, session: &SessionKey) -> String {
    format!("{}/threads", session_path(prefix, session))
}

pub fn thread_path(prefix: &str, thread: &ThreadKey) -> String {
    format!(
        "{}/threads/{}",
        session_path(prefix, &thread.session),
        encode_segment(&thread.thread_id)
    )
}

pub fn thread_action(prefix: &str, thread: &ThreadKey, action: &str) -> String {
    format!("{}/{action}", thread_path(prefix, thread))
}

pub fn turn_path(prefix: &str, thread: &ThreadKey, turn_id: &str) -> String {
    format!(
        "{}/turns/{}",
        thread_path(prefix, thread),
        encode_segment(turn_id)
    )
}

pub fn turn_action(prefix: &str, thread: &ThreadKey, turn_id: &str, action: &str) -> String {
    format!("{}/{action}", turn_path(prefix, thread, turn_id))
}

pub fn turn_sub(prefix: &str, thread: &ThreadKey, turn_id: &str, sub: &str) -> String {
    format!("{}/{sub}", turn_path(prefix, thread, turn_id))
}

pub fn commit_receipt_path(
    prefix: &str,
    thread: &ThreadKey,
    turn_id: &str,
    commit_id: &str,
) -> String {
    format!(
        "{}/commits/{}/receipt",
        turn_path(prefix, thread, turn_id),
        encode_segment(commit_id)
    )
}

pub fn execution_path(prefix: &str, thread: &ThreadKey, execution_id: &str) -> String {
    format!(
        "{}/executions/{}",
        thread_path(prefix, thread),
        encode_segment(execution_id)
    )
}

pub fn execution_action(
    prefix: &str,
    thread: &ThreadKey,
    execution_id: &str,
    action: &str,
) -> String {
    format!("{}/{action}", execution_path(prefix, thread, execution_id))
}

pub fn checkpoints_path(prefix: &str, thread: &ThreadKey) -> String {
    thread_action(prefix, thread, "checkpoints")
}

pub fn checkpoint_path(prefix: &str, thread: &ThreadKey, position: u64) -> String {
    format!("{}/{position}", checkpoints_path(prefix, thread))
}

pub fn events_path(prefix: &str, session: &SessionKey) -> String {
    session_action(prefix, session, "events")
}

pub fn events_stream_path(prefix: &str, session: &SessionKey) -> String {
    session_action(prefix, session, "events/stream")
}

pub fn events_prune_path(prefix: &str, session: &SessionKey) -> String {
    session_action(prefix, session, "events/prune")
}

pub fn snapshot_path(prefix: &str, session: &SessionKey) -> String {
    session_action(prefix, session, "snapshot")
}

pub fn resources_path(prefix: &str, thread: &ThreadKey) -> String {
    thread_action(prefix, thread, "resources")
}

pub fn restore_path(prefix: &str, session: &SessionKey) -> String {
    session_action(prefix, session, "restore")
}

pub fn admission_path(prefix: &str, session: &SessionKey, request_id: &str) -> String {
    session_action(
        prefix,
        session,
        &format!("admission/{}", encode_segment(request_id)),
    )
}

pub fn ready_path(prefix: &str) -> String {
    format!("{}/ready", root(prefix))
}

// ---------------------------------------------------------------- rules

/// One page-size rule for every paged call, identical to `backend::clamp_limit`.
pub fn page_limit(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        return Err(DurableError::invalid("Page limit must be 1..=1000"));
    }
    Ok(())
}

/// Percent-encodes one path segment or query value, keeping only RFC 3986 unreserved bytes.
///
/// Identities are arbitrary text (a session id may contain `/` or spaces); interpolating them
/// raw would silently address a different route.
pub fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn version_headers() -> Vec<(String, String)> {
    vec![(HEADER_VERSION.to_string(), PROTOCOL_VERSION.to_string())]
}

/// Everything a fenced call announces besides the path: who writes, under which epoch, until when.
///
/// `expires_at` travels with the lease because the server rebuilds a [`Lease`](agentscope_durable::Lease)
/// from the request and the backend checks it is still live; dropping it would turn every call
/// into a stale one, or worse, let a dead owner keep writing.
pub fn lease_headers(lease: &agentscope_durable::Lease) -> Vec<(String, String)> {
    let mut headers = version_headers();
    headers.extend([
        (HEADER_OWNER.to_string(), lease.owner.clone()),
        (HEADER_EPOCH.to_string(), lease.epoch.to_string()),
        (HEADER_EXPIRES_AT.to_string(), lease.expires_at.to_string()),
        (HEADER_THREAD.to_string(), lease.thread.thread_id.clone()),
        (HEADER_TURN.to_string(), lease.turn_id.clone()),
    ]);
    headers
}

pub fn parse_code(value: &str) -> Option<ErrorCode> {
    Some(match value {
        "NOT_FOUND" => ErrorCode::NotFound,
        "CONFLICT" => ErrorCode::Conflict,
        "FENCED" => ErrorCode::Fenced,
        "INVALID" => ErrorCode::Invalid,
        "INCOMPATIBLE" => ErrorCode::Incompatible,
        "CURSOR_EXPIRED" => ErrorCode::CursorExpired,
        "UNAVAILABLE" => ErrorCode::Unavailable,
        _ => return None,
    })
}

/// The status a failure answers with. `409` carries both conflict and fence: a stale epoch is a
/// conflict on the same turn, and a committed conflict still answers the applied receipt.
pub fn status_for(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::NotFound => 404,
        ErrorCode::Conflict | ErrorCode::Fenced => 409,
        ErrorCode::Invalid | ErrorCode::Incompatible => 400,
        ErrorCode::CursorExpired => 410,
        ErrorCode::Unavailable => 503,
    }
}

/// The fallback when a failure body does not name a known code.
pub fn code_for_status(status: u16) -> ErrorCode {
    match status {
        400 => ErrorCode::Invalid,
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Conflict,
        410 => ErrorCode::CursorExpired,
        412 => ErrorCode::Fenced,
        503 => ErrorCode::Unavailable,
        200..=299 => ErrorCode::Invalid,
        _ => ErrorCode::Unavailable,
    }
}

/// The failure body both sides use: code, human message and the receipt a `409` may carry.
pub fn failure_body(error: &DurableError, receipt: Option<&CommitReceipt>) -> Value {
    let mut body = serde_json::json!({
        "code": error.code().as_str(),
        "message": error.message(),
    });
    if let Some(receipt) = receipt {
        if let Ok(value) = serde_json::to_value(receipt) {
            body["receipt"] = value;
        }
    }
    body
}
