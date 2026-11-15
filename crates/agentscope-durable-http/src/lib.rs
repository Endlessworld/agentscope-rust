//! `agentscope-durable-http`:
//!
//! 对齐 Java `agentscope-extensions-durable-http`：只做协议/传输映射，
//! 不依赖 `agentscope-extensions-*`，不持有业务规则。传输由调用方注入（本 crate 不选 HTTP 客户端），
//! 这里只拥有线上形状与状态映射。
//!
//! The surface is the five calls a durable host needs, each one a pure request shape plus a
//! response decoder:
//!
//! | call | request | response |
//! | --- | --- | --- |
//! | `claim` | `POST {prefix}/sessions/{ns}/{session}/threads/{thread}/claim` | `Lease` or `null` |
//! | `commit` | `POST …/threads/{thread}/turns/{turn}/commits` | [`CommitReceipt`] |
//! | `frame` | `GET …/threads/{thread}/turns/{turn}/frame` | [`Frame`] |
//! | `events` | `GET {prefix}/sessions/{ns}/{session}/events?after=&limit=` | [`EventPage`] |
//! | `receipt` | `GET …/turns/{turn}/commits/{commit}/receipt` | `CommitReceipt` or `null` |
//!
//! Fenced calls carry `X-Durable-Owner` and `X-Durable-Epoch`; every call carries
//! `X-Durable-Version`. Page sizes follow the backend contract exactly: `1..=1000`, rejected —
//! never clamped — outside it. `409` is a conflict that still answers the committed receipt, so
//! [`HttpError::receipt`] keeps it.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::{
    Commit, CommitReceipt, DurableError, ErrorCode, EventPage, Frame, Lease, Result, SessionKey,
    ThreadKey,
};
use async_trait::async_trait;
use serde::de::DeserializeOwned;

/// Wire protocol version every request announces.
pub const PROTOCOL_VERSION: &str = agentscope_durable::PROTOCOL_VERSION;

/// Route prefix of the backend surface; the paths below hang off it.
pub const DEFAULT_PREFIX: &str = "/v1";

/// The page-size contract of the backend, mirrored so the client refuses the same input.
pub const MAX_PAGE_LIMIT: usize = 1000;

const HEADER_VERSION: &str = "X-Durable-Version";
const HEADER_CONTENT_TYPE: &str = "Content-Type";
const HEADER_OWNER: &str = "X-Durable-Owner";
const HEADER_EPOCH: &str = "X-Durable-Epoch";

/// A decoded failure: the backend error plus the receipt a `409` carried, if any.
///
/// A conflicting commit is not a lost commit: the server answers the receipt it already applied
/// so the caller can adopt the outcome instead of retrying blindly, and that receipt has to
/// survive the mapping to [`DurableError`].
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    error: DurableError,
    receipt: Option<CommitReceipt>,
}

impl HttpError {
    pub fn error(&self) -> &DurableError {
        &self.error
    }

    pub fn code(&self) -> ErrorCode {
        self.error.code()
    }

    pub fn message(&self) -> &str {
        self.error.message()
    }

    /// The receipt the committed conflict carried, when the response was a `409` with one.
    pub fn receipt(&self) -> Option<&CommitReceipt> {
        self.receipt.as_ref()
    }

    pub fn into_parts(self) -> (DurableError, Option<CommitReceipt>) {
        (self.error, self.receipt)
    }
}

impl From<DurableError> for HttpError {
    fn from(error: DurableError) -> Self {
        Self {
            error,
            receipt: None,
        }
    }
}

impl From<HttpError> for DurableError {
    fn from(error: HttpError) -> Self {
        error.error
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.error)
    }
}

impl std::error::Error for HttpError {}

pub type HttpResult<T> = std::result::Result<T, HttpError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One request in wire form: the transport serializes it, the router never sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The only thing this crate does not own: how the bytes get to the server.
///
/// A failure here is an *unconfirmed* outcome, never a `404`: the caller must not conclude that
/// the commit did not land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    message: String,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: HttpRequest)
        -> std::result::Result<HttpResponse, TransportError>;
}

// ---------------------------------------------------------------- request shapes

/// Claims the writer lease of a thread. `200` answers the lease, or `null` when no turn is ready.
pub fn claim_request(
    prefix: &str,
    thread: &ThreadKey,
    owner: &str,
    ttl: Duration,
) -> Result<HttpRequest> {
    if owner.trim().is_empty() {
        return Err(DurableError::invalid("Lease owner must not be blank"));
    }
    let mut headers = version_headers();
    headers.push((
        HEADER_CONTENT_TYPE.to_string(),
        "application/json".to_string(),
    ));
    Ok(HttpRequest {
        method: Method::Post,
        path: format!("{}/claim", thread_scope(prefix, thread)),
        query: Vec::new(),
        headers,
        body: serde_json::to_vec(&serde_json::json!({
            "owner": owner,
            "ttlMillis": ttl.as_millis() as u64,
        }))?,
    })
}

/// Commits one fenced transition. `200` answers the [`CommitReceipt`]; `409` answers the receipt
/// that already holds the commit.
pub fn commit_request(prefix: &str, lease: &Lease, commit: &Commit) -> Result<HttpRequest> {
    let mut headers = version_headers();
    headers.push((
        HEADER_CONTENT_TYPE.to_string(),
        "application/json".to_string(),
    ));
    headers.extend(lease_headers(lease));
    Ok(HttpRequest {
        method: Method::Post,
        path: format!(
            "{}/commits",
            turn_scope(prefix, &lease.thread, &lease.turn_id)
        ),
        query: Vec::new(),
        headers,
        body: serde_json::to_vec(commit)?,
    })
}

/// Reads everything a running invocation needs about its turn, under the same fence.
pub fn frame_request(prefix: &str, lease: &Lease) -> Result<HttpRequest> {
    let mut headers = version_headers();
    headers.extend(lease_headers(lease));
    Ok(HttpRequest {
        method: Method::Get,
        path: format!(
            "{}/frame",
            turn_scope(prefix, &lease.thread, &lease.turn_id)
        ),
        query: Vec::new(),
        headers,
        body: Vec::new(),
    })
}

/// Reads one page of a session's public events after a cursor.
pub fn events_request(
    prefix: &str,
    session: &SessionKey,
    after: Option<&str>,
    limit: usize,
) -> Result<HttpRequest> {
    page_limit(limit)?;
    let mut query = Vec::with_capacity(2);
    if let Some(after) = after {
        query.push(("after".to_string(), encode_segment(after)));
    }
    query.push(("limit".to_string(), limit.to_string()));
    Ok(HttpRequest {
        method: Method::Get,
        path: format!("{}/events", session_scope(prefix, session)),
        query,
        headers: version_headers(),
        body: Vec::new(),
    })
}

/// Reads the receipt of one commit; `200` answers it or `null` when the commit never landed.
pub fn receipt_request(
    prefix: &str,
    thread: &ThreadKey,
    turn_id: &str,
    commit_id: &str,
) -> Result<HttpRequest> {
    Ok(HttpRequest {
        method: Method::Get,
        path: format!(
            "{}/commits/{}/receipt",
            turn_scope(prefix, thread, turn_id),
            encode_segment(commit_id)
        ),
        query: Vec::new(),
        headers: version_headers(),
        body: Vec::new(),
    })
}

/// One page-size rule for every paged call, identical to `backend::clamp_limit`.
fn page_limit(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        return Err(DurableError::invalid("Page limit must be 1..=1000"));
    }
    Ok(())
}

fn session_scope(prefix: &str, session: &SessionKey) -> String {
    format!(
        "{}/sessions/{}/{}",
        prefix.trim_end_matches('/'),
        encode_segment(&session.namespace),
        encode_segment(&session.session_id)
    )
}

fn thread_scope(prefix: &str, thread: &ThreadKey) -> String {
    format!(
        "{}/threads/{}",
        session_scope(prefix, &thread.session),
        encode_segment(&thread.thread_id)
    )
}

fn turn_scope(prefix: &str, thread: &ThreadKey, turn_id: &str) -> String {
    format!(
        "{}/turns/{}",
        thread_scope(prefix, thread),
        encode_segment(turn_id)
    )
}

fn version_headers() -> Vec<(String, String)> {
    vec![(HEADER_VERSION.to_string(), PROTOCOL_VERSION.to_string())]
}

fn lease_headers(lease: &Lease) -> Vec<(String, String)> {
    vec![
        (HEADER_OWNER.to_string(), lease.owner.clone()),
        (HEADER_EPOCH.to_string(), lease.epoch.to_string()),
    ]
}

/// Percent-encodes one path segment or query value, keeping only RFC 3986 unreserved bytes.
///
/// Identities are arbitrary text (a session id may contain `/` or spaces); interpolating them
/// raw would silently address a different route.
fn encode_segment(value: &str) -> String {
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

// ---------------------------------------------------------------- responses

fn decode<T: DeserializeOwned>(response: HttpResponse) -> HttpResult<T> {
    if response.status != 200 {
        return Err(failure(&response));
    }
    serde_json::from_slice(&response.body).map_err(|error| {
        HttpError::from(DurableError::invalid(format!(
            "Malformed durable HTTP response: {error}"
        )))
    })
}

/// The same, for calls whose `200` body may be `null` or empty ("nothing yet").
fn decode_optional<T: DeserializeOwned>(response: HttpResponse) -> HttpResult<Option<T>> {
    if response.status == 200 && response.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    decode(response)
}

fn failure(response: &HttpResponse) -> HttpError {
    let code = match response.status {
        400 => ErrorCode::Invalid,
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Conflict,
        412 => ErrorCode::Fenced,
        // Only `200` carries a body on this surface.
        200..=299 => ErrorCode::Invalid,
        _ => ErrorCode::Unavailable,
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&response.body).ok();
    let object = parsed.as_ref().filter(|value| value.is_object());
    let message = match object {
        Some(object) => object
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| default_message(response.status)),
        None => {
            plain_text_message(&response.body).unwrap_or_else(|| default_message(response.status))
        }
    };
    let receipt = object
        .and_then(|object| object.get("receipt"))
        .and_then(|receipt| serde_json::from_value(receipt.clone()).ok());
    HttpError {
        error: DurableError::new(code, message),
        receipt,
    }
}

fn default_message(status: u16) -> String {
    format!("durable HTTP request failed with status {status}")
}

/// Keeps a plain-text error body, bounded: it is diagnostics, not a payload.
fn plain_text_message(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= 512).then(|| text.to_owned())
}

// ---------------------------------------------------------------- backend

/// The five-call client: it builds, sends and decodes, and holds no state of its own.
pub struct HttpBackend {
    transport: Arc<dyn Transport>,
    prefix: String,
}

impl HttpBackend {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self::with_prefix(transport, DEFAULT_PREFIX)
    }

    pub fn with_prefix(transport: Arc<dyn Transport>, prefix: impl Into<String>) -> Self {
        Self {
            transport,
            prefix: prefix.into(),
        }
    }

    async fn send(&self, request: HttpRequest) -> HttpResult<HttpResponse> {
        self.transport.send(request).await.map_err(|error| {
            HttpError::from(DurableError::unavailable(format!(
                "durable HTTP outcome is unconfirmed: {error}"
            )))
        })
    }

    pub async fn claim(
        &self,
        thread: &ThreadKey,
        owner: &str,
        ttl: Duration,
    ) -> HttpResult<Option<Lease>> {
        let request = claim_request(&self.prefix, thread, owner, ttl)?;
        decode_optional(self.send(request).await?)
    }

    pub async fn commit(&self, lease: &Lease, commit: &Commit) -> HttpResult<CommitReceipt> {
        let request = commit_request(&self.prefix, lease, commit)?;
        decode(self.send(request).await?)
    }

    pub async fn frame(&self, lease: &Lease) -> HttpResult<Frame> {
        let request = frame_request(&self.prefix, lease)?;
        decode(self.send(request).await?)
    }

    pub async fn events(
        &self,
        session: &SessionKey,
        after: Option<&str>,
        limit: usize,
    ) -> HttpResult<EventPage> {
        let request = events_request(&self.prefix, session, after, limit)?;
        decode(self.send(request).await?)
    }

    pub async fn receipt(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        commit_id: &str,
    ) -> HttpResult<Option<CommitReceipt>> {
        let request = receipt_request(&self.prefix, thread, turn_id, commit_id)?;
        decode_optional(self.send(request).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use agentscope_durable::TurnStatus;

    use super::*;

    /// A scripted transport: it records what it was asked and answers in order.
    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<HttpResponse>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(
            &self,
            request: HttpRequest,
        ) -> std::result::Result<HttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::new("fake transport has no scripted response"))
        }
    }

    fn session() -> SessionKey {
        SessionKey::new("ns", "s-1").unwrap()
    }

    fn thread() -> ThreadKey {
        ThreadKey::new(session(), "t-1").unwrap()
    }

    fn lease() -> Lease {
        Lease {
            thread: thread(),
            turn_id: "turn-1".to_string(),
            owner: "host-a".to_string(),
            epoch: 7,
            expires_at: 1_000,
        }
    }

    fn commit() -> Commit {
        Commit::new("c-1", 3, TurnStatus::Running)
    }

    fn receipt() -> CommitReceipt {
        CommitReceipt {
            commit_id: "c-1".to_string(),
            digest: "abc123".to_string(),
            revision: 4,
            cursor: "c-4".to_string(),
        }
    }

    fn receipt_json() -> serde_json::Value {
        serde_json::json!({
            "commitId": "c-1",
            "digest": "abc123",
            "revision": 4,
            "cursor": "c-4",
        })
    }

    fn response(status: u16, body: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn body_of(request: &HttpRequest) -> serde_json::Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    #[test]
    fn claim_request_targets_the_thread_claim_route() {
        let request =
            claim_request(DEFAULT_PREFIX, &thread(), "host-a", Duration::from_secs(30)).unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.path, "/v1/sessions/ns/s-1/threads/t-1/claim");
        assert!(request.query.is_empty());
        assert_eq!(request.header(HEADER_VERSION), Some(PROTOCOL_VERSION));
        assert_eq!(
            request.header(HEADER_CONTENT_TYPE),
            Some("application/json")
        );
        assert_eq!(
            body_of(&request),
            serde_json::json!({ "owner": "host-a", "ttlMillis": 30_000 })
        );
    }

    #[test]
    fn claim_request_rejects_a_blank_owner() {
        let error = claim_request(DEFAULT_PREFIX, &thread(), "  ", Duration::from_secs(1))
            .expect_err("a blank owner cannot fence anything");
        assert_eq!(error.code(), ErrorCode::Invalid);
    }

    #[test]
    fn commit_request_carries_the_lease_and_the_commit_body() {
        let commit = commit();
        let request = commit_request(DEFAULT_PREFIX, &lease(), &commit).unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.path,
            "/v1/sessions/ns/s-1/threads/t-1/turns/turn-1/commits"
        );
        assert!(request.query.is_empty());
        assert_eq!(request.header(HEADER_OWNER), Some("host-a"));
        assert_eq!(request.header(HEADER_EPOCH), Some("7"));
        assert_eq!(body_of(&request), serde_json::to_value(&commit).unwrap());
    }

    #[test]
    fn frame_request_is_a_lease_fenced_get() {
        let request = frame_request(DEFAULT_PREFIX, &lease()).unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(
            request.path,
            "/v1/sessions/ns/s-1/threads/t-1/turns/turn-1/frame"
        );
        assert_eq!(request.header(HEADER_OWNER), Some("host-a"));
        assert_eq!(request.header(HEADER_EPOCH), Some("7"));
        assert!(request.body.is_empty());
    }

    #[test]
    fn events_request_pages_with_after_and_limit() {
        let request = events_request(DEFAULT_PREFIX, &session(), Some("c-9"), 50).unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.path, "/v1/sessions/ns/s-1/events");
        assert_eq!(
            request.query,
            vec![
                ("after".to_string(), "c-9".to_string()),
                ("limit".to_string(), "50".to_string()),
            ]
        );
        assert_eq!(request.header(HEADER_VERSION), Some(PROTOCOL_VERSION));
    }

    #[test]
    fn events_request_omits_an_absent_cursor() {
        let request = events_request(DEFAULT_PREFIX, &session(), None, 10).unwrap();
        assert_eq!(request.query, vec![("limit".to_string(), "10".to_string())]);
    }

    #[test]
    fn events_request_rejects_limits_outside_the_page_contract() {
        for limit in [0, 1_001] {
            let error = events_request(DEFAULT_PREFIX, &session(), None, limit)
                .expect_err("the contract rejects the page size instead of clamping it");
            assert_eq!(error.code(), ErrorCode::Invalid);
            assert!(error.message().contains("1..=1000"), "{}", error.message());
        }
        let boundary = events_request(DEFAULT_PREFIX, &session(), None, MAX_PAGE_LIMIT).unwrap();
        assert_eq!(
            boundary.query,
            vec![("limit".to_string(), "1000".to_string())]
        );
    }

    #[test]
    fn receipt_request_targets_the_commit_receipt_route() {
        let request = receipt_request(DEFAULT_PREFIX, &thread(), "turn-1", "c-1").unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(
            request.path,
            "/v1/sessions/ns/s-1/threads/t-1/turns/turn-1/commits/c-1/receipt"
        );
        assert!(request.query.is_empty());
    }

    #[tokio::test]
    async fn a_commit_round_trip_sends_the_expected_request_and_decodes_the_receipt() {
        let transport = FakeTransport::new(vec![response(200, receipt_json())]);
        let backend = HttpBackend::new(transport.clone());

        let answered = backend.commit(&lease(), &commit()).await.unwrap();

        assert_eq!(answered, receipt());
        let sent = transport.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0],
            commit_request(DEFAULT_PREFIX, &lease(), &commit()).unwrap()
        );
        assert_eq!(sent[0].method, Method::Post);
        assert_eq!(
            sent[0].path,
            "/v1/sessions/ns/s-1/threads/t-1/turns/turn-1/commits"
        );
        assert!(sent[0].query.is_empty());
        assert_eq!(body_of(&sent[0]), serde_json::to_value(commit()).unwrap());
    }

    #[tokio::test]
    async fn each_status_maps_to_its_durable_error_code() {
        let cases = [
            (400, ErrorCode::Invalid),
            (404, ErrorCode::NotFound),
            (409, ErrorCode::Conflict),
            (412, ErrorCode::Fenced),
            (500, ErrorCode::Unavailable),
            (503, ErrorCode::Unavailable),
        ];
        for (status, expected) in cases {
            let transport = FakeTransport::new(vec![response(
                status,
                serde_json::json!({ "code": "X", "message": "boom" }),
            )]);
            let backend = HttpBackend::new(transport);
            let error = backend
                .commit(&lease(), &commit())
                .await
                .expect_err("a non-200 status is a failure");
            assert_eq!(error.code(), expected, "status {status}");
            assert_eq!(error.message(), "boom", "status {status}");
            assert!(error.receipt().is_none(), "status {status}");
        }
    }

    #[tokio::test]
    async fn a_conflict_keeps_its_message_and_receipt() {
        let transport = FakeTransport::new(vec![response(
            409,
            serde_json::json!({
                "code": "CONFLICT",
                "message": "revision advanced",
                "receipt": receipt_json(),
            }),
        )]);
        let backend = HttpBackend::new(transport);

        let error = backend
            .commit(&lease(), &commit())
            .await
            .expect_err("a conflict is a failure");

        assert_eq!(error.code(), ErrorCode::Conflict);
        assert_eq!(error.message(), "revision advanced");
        assert_eq!(error.receipt(), Some(&receipt()));
        let (durable, adopted) = error.into_parts();
        assert_eq!(durable.code(), ErrorCode::Conflict);
        assert_eq!(adopted, Some(receipt()));
    }

    #[tokio::test]
    async fn absent_leases_and_receipts_decode_to_none() {
        let transport = FakeTransport::new(vec![
            response(200, serde_json::Value::Null),
            HttpResponse {
                status: 200,
                body: Vec::new(),
            },
        ]);
        let backend = HttpBackend::new(transport);

        assert_eq!(
            backend
                .claim(&thread(), "host-a", Duration::from_secs(5))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            backend.receipt(&thread(), "turn-1", "c-1").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_malformed_success_body_is_not_accepted() {
        let transport = FakeTransport::new(vec![HttpResponse {
            status: 200,
            body: b"{oops".to_vec(),
        }]);
        let backend = HttpBackend::new(transport);

        let error = backend
            .commit(&lease(), &commit())
            .await
            .expect_err("a body that is not JSON cannot be a receipt");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert!(error.message().contains("Malformed"), "{}", error.message());
    }

    #[tokio::test]
    async fn an_unconfirmed_transport_outcome_is_unavailable() {
        let backend = HttpBackend::new(Arc::new(FakeTransport::default()));

        let error = backend
            .commit(&lease(), &commit())
            .await
            .expect_err("a transport failure is never a durable answer");
        assert_eq!(error.code(), ErrorCode::Unavailable);
    }

    #[test]
    fn identities_are_percent_encoded_into_their_route_segment() {
        let odd = SessionKey::new("team a", "s/1").unwrap();
        let request = events_request(DEFAULT_PREFIX, &odd, Some("c 9"), 1).unwrap();
        assert_eq!(request.path, "/v1/sessions/team%20a/s%2F1/events");
        assert_eq!(request.query[0].1, "c%209");
    }
}
