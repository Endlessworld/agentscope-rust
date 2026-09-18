//! In-process HTTP server for the `/agentscope/*` contract, mirroring
//! `io.agentscope.extensions.aistio.transport.ContractHttpServer`.
//!
//! Routes mirror the Go `connector.ContractServer` and the Python SDK one for one:
//!
//! ```text
//!   GET  /agentscope/info
//!   GET  /agentscope/health
//!   GET  /agentscope/sessions
//!   GET  /agentscope/sessions/{id}/state
//!   GET  /agentscope/sessions/{id}/context
//!   GET  /agentscope/sessions/{id}/messages?offset=&limit=
//!   GET  /agentscope/sessions/{id}/tasks
//!   GET  /agentscope/sessions/{id}/subagent-tasks
//!   GET  /agentscope/sessions/{id}/export-transcript
//!   GET  /agentscope/subagents
//!   GET  /agentscope/workspaces
//!   POST /agentscope/sessions/{id}/compress
//!   POST /agentscope/sessions/{id}/terminate
//!   POST /agentscope/sessions/{id}/abort
//!   POST /agentscope/sessions/{id}/plan-mode
//!   POST /agentscope/sessions/{id}/messages
//!   DELETE /agentscope/sessions/{id}/subagent-tasks/{taskId}
//! ```
//!
//! Built on `axum` (the Java version uses the JDK's `com.sun.net.httpserver`) so
//! instrumenting an agent never drags in a web framework beyond the one dependency.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::error::{AistioError, AistioResult};
use crate::transport::contract_provider::ContractProvider;
use crate::transport::control_plane_http_client::INTERNAL_TOKEN_HEADER;

/// Header carrying the caller-supplied command id.
pub const COMMAND_ID_HEADER: &str = "X-Command-Id";
/// Default `limit` for `GET .../messages` when the query string omits it.
pub const DEFAULT_MESSAGE_LIMIT: i64 = 100;
/// Sessions are capped at this many entries per response (matches Java).
pub const MAX_SESSION_PAGE: usize = 500;

#[derive(Clone)]
struct ServerState {
    provider: Arc<dyn ContractProvider>,
    internal_token: String,
}

/// In-process HTTP server serving the `/agentscope/*` contract.
pub struct ContractHttpServer {
    addr: SocketAddr,
    listener: Option<TcpListener>,
    router: Router,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ContractHttpServer {
    /// Binds the contract server.
    ///
    /// `port == 0` binds an ephemeral port; use [`port`](Self::port) afterwards. A blank
    /// `host` binds all interfaces. `internal_token` may be empty (writes are then open).
    pub async fn bind(
        host: &str,
        port: u16,
        provider: Arc<dyn ContractProvider>,
        internal_token: &str,
    ) -> AistioResult<Self> {
        let bind_host = if host.trim().is_empty() { "0.0.0.0" } else { host };
        let listener = TcpListener::bind((bind_host, port))
            .await
            .map_err(|e| AistioError::Http(format!("bind {bind_host}:{port}: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| AistioError::Http(format!("local_addr: {e}")))?;
        let router = build_router(provider, internal_token);
        Ok(Self {
            addr,
            listener: Some(listener),
            router,
            shutdown: None,
            task: None,
        })
    }

    /// The actual bound port, which matters when the requested port was `0`.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The actual bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The composed router (used by tests to drive requests in-process).
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Starts accepting connections on a background task.
    pub fn start(&mut self) -> AistioResult<()> {
        if self.task.is_some() {
            return Ok(());
        }
        let listener = self
            .listener
            .take()
            .ok_or_else(|| AistioError::Http("contract server already started".into()))?;
        let (tx, rx) = oneshot::channel::<()>();
        let router = self.router.clone();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        self.shutdown = Some(tx);
        self.task = Some(handle);
        Ok(())
    }

    /// Stops the server and waits for the accept loop to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task.take() {
            let _ = handle.await;
        }
    }
}

/// Builds the `/agentscope/*` router over a provider.
fn build_router(provider: Arc<dyn ContractProvider>, internal_token: &str) -> Router {
    let state = ServerState {
        provider,
        internal_token: internal_token.to_string(),
    };
    Router::new()
        .route("/agentscope/info", get(info))
        .route("/agentscope/health", get(health))
        .route("/agentscope/sessions", get(sessions))
        .route("/agentscope/subagents", get(subagents))
        .route("/agentscope/workspaces", get(workspaces))
        .route("/agentscope/sessions/:id/state", get(session_state))
        .route("/agentscope/sessions/:id/context", get(context))
        .route("/agentscope/sessions/:id/messages", get(messages).post(post_message))
        .route("/agentscope/sessions/:id/tasks", get(tasks))
        .route("/agentscope/sessions/:id/subagent-tasks", get(subagent_tasks))
        .route(
            "/agentscope/sessions/:id/subagent-tasks/:task_id",
            delete(cancel_subagent_task),
        )
        .route(
            "/agentscope/sessions/:id/export-transcript",
            get(export_transcript),
        )
        .route("/agentscope/sessions/:id/compress", post(compress))
        .route("/agentscope/sessions/:id/terminate", post(terminate))
        .route("/agentscope/sessions/:id/abort", post(abort))
        .route("/agentscope/sessions/:id/plan-mode", post(plan_mode))
        .fallback(not_found)
        .with_state(state)
}

// ─── error mapping ───

/// HTTP status + error body for an [`AistioError`], mirroring the Java `handle` mapping.
fn error_response(err: &AistioError) -> Response {
    let (status, code, hint) = match err {
        AistioError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", None),
        AistioError::Busy(_) => (StatusCode::CONFLICT, "busy", Some("wait_idle")),
        AistioError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized", None),
        AistioError::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported", None),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "failed", None),
    };
    let mut body = Map::new();
    body.insert("error".into(), Value::String(err.to_string()));
    body.insert("code".into(), Value::String(code.to_string()));
    if let Some(hint) = hint {
        body.insert("hint".into(), Value::String(hint.to_string()));
    }
    (status, Json(Value::Object(body))).into_response()
}

async fn not_found() -> Response {
    let mut body = Map::new();
    body.insert("error".into(), Value::String("not found".into()));
    body.insert("code".into(), Value::String("not_found".into()));
    (StatusCode::NOT_FOUND, Json(Value::Object(body))).into_response()
}

fn unauthorized() -> Response {
    let mut body = Map::new();
    body.insert(
        "error".into(),
        Value::String("invalid or missing X-Builder-Internal-Token".into()),
    );
    body.insert("code".into(), Value::String("unauthorized".into()));
    (StatusCode::UNAUTHORIZED, Json(Value::Object(body))).into_response()
}

/// Checks the internal token for a write request (`true` when allowed).
fn write_token_ok(state: &ServerState, headers: &HeaderMap) -> bool {
    if state.internal_token.is_empty() {
        return true;
    }
    headers
        .get(INTERNAL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == state.internal_token)
        .unwrap_or(false)
}

/// Builds the `{"accepted":true,...}` body of a successful command request.
fn command_accepted(headers: &HeaderMap, session_id: &str, phase: &str) -> Value {
    let command_id = headers
        .get(COMMAND_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    let short: String = command_id.chars().take(12).collect();
    json!({
        "accepted": true,
        "commandId": short,
        "phase": phase,
        "result": {},
    })
}

// ─── read handlers ───

async fn info(State(state): State<ServerState>) -> Response {
    match state.provider.info().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

async fn sessions(State(state): State<ServerState>) -> Response {
    match state.provider.sessions().await {
        Ok(all) => {
            let truncated = all.len() > MAX_SESSION_PAGE;
            let page: Vec<Value> = if truncated {
                all[..MAX_SESSION_PAGE].to_vec()
            } else {
                all
            };
            Json(json!({
                "sessions": page,
                "truncated": truncated,
                "hasMore": truncated,
            }))
            .into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn subagents(State(state): State<ServerState>) -> Response {
    match state.provider.subagents().await {
        Ok(v) => Json(json!({ "subagents": v })).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn workspaces(State(state): State<ServerState>) -> Response {
    match state.provider.workspaces().await {
        Ok(v) => Json(json!({ "workspaces": v })).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn session_state(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match state.provider.session_state(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn context(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match state.provider.context(&id).await {
        Ok(v) => Json(v.to_json_value()).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn messages(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let offset = query_int(&query, "offset", 0);
    let limit = query_int(&query, "limit", DEFAULT_MESSAGE_LIMIT);
    match state.provider.messages(&id, offset, limit).await {
        Ok(v) => Json(v.to_json_value()).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn tasks(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match state.provider.tasks(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn subagent_tasks(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match state.provider.subagent_tasks(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn export_transcript(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match state.provider.export_transcript(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

// ─── write handlers ───

async fn compress(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    match state.provider.compress(&id).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            Json(command_accepted(&headers, &id, &phase)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn terminate(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    match state.provider.terminate(&id).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            Json(command_accepted(&headers, &id, &phase)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn abort(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    match state.provider.abort(&id).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            Json(command_accepted(&headers, &id, &phase)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn plan_mode(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    match state.provider.plan_mode(&id, body).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            Json(command_accepted(&headers, &id, &phase)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn post_message(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    match state.provider.post_message(&id, body).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            let mut out = command_accepted(&headers, &id, &phase);
            // Inbound injection is accepted asynchronously (HTTP 202).
            (StatusCode::ACCEPTED, Json(out)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn cancel_subagent_task(
    State(state): State<ServerState>,
    Path((id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !write_token_ok(&state, &headers) {
        return unauthorized();
    }
    match state.provider.cancel_subagent_task(&id, &task_id).await {
        Ok(()) => {
            let phase = state.provider.session_phase(&id);
            Json(command_accepted(&headers, &id, &phase)).into_response()
        }
        Err(e) => error_response(&e),
    }
}

/// Parses a non-negative integer query parameter, falling back on missing/invalid/negative.
fn query_int(query: &HashMap<String, String>, key: &str, fallback: i64) -> i64 {
    match query.get(key).and_then(|raw| raw.parse::<i64>().ok()) {
        Some(v) if v >= 0 => v,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextSnapshot, Inventory, MessagePage};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingProvider {
        compressed: AtomicUsize,
        terminated: AtomicUsize,
        aborted: AtomicUsize,
        inbound: AtomicUsize,
        plan_calls: AtomicUsize,
    }

    #[async_trait]
    impl ContractProvider for RecordingProvider {
        async fn inventory(&self) -> AistioResult<Inventory> {
            Ok(Inventory::health_only(2))
        }
        async fn context(&self, session_id: &str) -> AistioResult<ContextSnapshot> {
            Ok(ContextSnapshot::builder(session_id)
                .system_prompt("sys")
                .build())
        }
        async fn messages(
            &self,
            session_id: &str,
            offset: i64,
            limit: i64,
        ) -> AistioResult<MessagePage> {
            let all = (0..5)
                .map(|i| crate::model::MessageItem::of(i, "user", format!("m{i}")))
                .collect();
            Ok(MessagePage::of(session_id, all, offset, limit))
        }
        async fn session_ids(&self) -> AistioResult<Vec<String>> {
            Ok(vec!["s1".to_string()])
        }
        async fn info(&self) -> AistioResult<Value> {
            Ok(json!({ "agentKey": "friday" }))
        }
        async fn compress(&self, _id: &str) -> AistioResult<()> {
            self.compressed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn terminate(&self, _id: &str) -> AistioResult<()> {
            self.terminated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn abort(&self, _id: &str) -> AistioResult<()> {
            self.aborted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn post_message(&self, _id: &str, _body: Value) -> AistioResult<()> {
            self.inbound.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn plan_mode(&self, _id: &str, _body: Value) -> AistioResult<()> {
            self.plan_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct ErrorProvider;

    #[async_trait]
    impl ContractProvider for ErrorProvider {
        async fn inventory(&self) -> AistioResult<Inventory> {
            Err(AistioError::NotFound("no session".into()))
        }
        async fn context(&self, _session_id: &str) -> AistioResult<ContextSnapshot> {
            Err(AistioError::Busy("compressing".into()))
        }
        async fn messages(
            &self,
            _session_id: &str,
            _offset: i64,
            _limit: i64,
        ) -> AistioResult<MessagePage> {
            Err(AistioError::Unauthorized("bad token".into()))
        }
        async fn session_ids(&self) -> AistioResult<Vec<String>> {
            Err(AistioError::Http("boom".into()))
        }
        async fn tasks(&self, _session_id: &str) -> AistioResult<Value> {
            Err(AistioError::Unsupported("task-query".into()))
        }
    }

    async fn get_json(server: &mut ContractHttpServer, path: &str) -> (u16, Value) {
        let url = format!("http://127.0.0.1:{}{}", server.port(), path);
        let resp = reqwest::get(&url).await.expect("request");
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn write(
        server: &ContractHttpServer,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("http://127.0.0.1:{}{}", server.port(), path);
        let client = reqwest::Client::new();
        let mut req = client.request(method, &url);
        if let Some(t) = token {
            req = req.header(INTERNAL_TOKEN_HEADER, t);
        }
        req = req.header(COMMAND_ID_HEADER, "cmd-test");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request");
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn started(provider: Arc<dyn ContractProvider>, token: &str) -> ContractHttpServer {
        let mut server = ContractHttpServer::bind("127.0.0.1", 0, provider, token)
            .await
            .expect("bind");
        server.start().expect("start");
        server
    }

    #[tokio::test]
    async fn routes_read_endpoints() {
        let provider = Arc::new(RecordingProvider::default());
        let mut server = started(provider, "").await;

        let (status, body) = get_json(&mut server, "/agentscope/health").await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");

        let (status, body) = get_json(&mut server, "/agentscope/info").await;
        assert_eq!(status, 200);
        assert_eq!(body["agentKey"], "friday");

        let (status, body) = get_json(&mut server, "/agentscope/sessions").await;
        assert_eq!(status, 200);
        assert_eq!(body["sessions"][0]["sessionId"], "s1");
        assert_eq!(body["truncated"], false);

        let (status, body) = get_json(&mut server, "/agentscope/sessions/s1/context").await;
        assert_eq!(status, 200);
        assert_eq!(body["systemPrompt"], "sys");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn messages_pagination_uses_query_defaults() {
        let provider = Arc::new(RecordingProvider::default());
        let mut server = started(provider, "").await;

        let (status, body) = get_json(&mut server, "/agentscope/sessions/s1/messages").await;
        assert_eq!(status, 200);
        assert_eq!(body["total"], 5);
        assert_eq!(body["limit"], DEFAULT_MESSAGE_LIMIT);
        assert_eq!(body["messages"].as_array().unwrap().len(), 5);

        let (status, body) = get_json(
            &mut server,
            "/agentscope/sessions/s1/messages?offset=1&limit=2",
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["offset"], 1);
        assert_eq!(body["limit"], 2);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"], "m1");

        // Invalid / negative query values fall back to defaults.
        let (_, body) = get_json(
            &mut server,
            "/agentscope/sessions/s1/messages?offset=-3&limit=abc",
        )
        .await;
        assert_eq!(body["offset"], 0);
        assert_eq!(body["limit"], DEFAULT_MESSAGE_LIMIT);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn write_routes_return_accepted_command() {
        let provider = Arc::new(RecordingProvider::default());
        let server = started(provider.clone(), "").await;

        let (status, body) = write(
            &server,
            reqwest::Method::POST,
            "/agentscope/sessions/s1/compress",
            None,
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["accepted"], true);
        assert_eq!(body["commandId"], "cmd-test");
        assert_eq!(body["phase"], "running");
        assert_eq!(provider.compressed.load(Ordering::SeqCst), 1);

        let (status, _) = write(
            &server,
            reqwest::Method::POST,
            "/agentscope/sessions/s1/abort",
            None,
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(provider.aborted.load(Ordering::SeqCst), 1);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn inbound_message_returns_202() {
        let provider = Arc::new(RecordingProvider::default());
        let server = started(provider.clone(), "").await;
        let (status, body) = write(
            &server,
            reqwest::Method::POST,
            "/agentscope/sessions/s1/messages",
            None,
            Some(json!({ "content": "hi" })),
        )
        .await;
        assert_eq!(status, 202);
        assert_eq!(body["accepted"], true);
        assert_eq!(provider.inbound.load(Ordering::SeqCst), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn plan_mode_writes_are_token_guarded() {
        let provider = Arc::new(RecordingProvider::default());
        let server = started(provider.clone(), "secret").await;

        let (status, body) = write(
            &server,
            reqwest::Method::POST,
            "/agentscope/sessions/s1/plan-mode",
            None,
            Some(json!({ "active": true })),
        )
        .await;
        assert_eq!(status, 401);
        assert_eq!(body["code"], "unauthorized");
        assert_eq!(provider.plan_calls.load(Ordering::SeqCst), 0);

        let (status, _) = write(
            &server,
            reqwest::Method::POST,
            "/agentscope/sessions/s1/plan-mode",
            Some("secret"),
            Some(json!({ "active": true })),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(provider.plan_calls.load(Ordering::SeqCst), 1);

        // Reads stay open even with a token configured.
        let url = format!("http://127.0.0.1:{}/agentscope/health", server.port());
        let status = reqwest::get(&url).await.unwrap().status().as_u16();
        assert_eq!(status, 200);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn errors_map_to_status_codes() {
        let mut server = started(Arc::new(ErrorProvider), "").await;

        let (status, body) = get_json(&mut server, "/agentscope/sessions/s1/context").await;
        assert_eq!(status, 409);
        assert_eq!(body["code"], "busy");
        assert_eq!(body["hint"], "wait_idle");

        let (status, body) = get_json(&mut server, "/agentscope/sessions/s1/messages").await;
        assert_eq!(status, 401);
        assert_eq!(body["code"], "unauthorized");

        let (status, body) = get_json(&mut server, "/agentscope/subagents").await;
        assert_eq!(status, 200);
        assert!(body["subagents"].as_array().unwrap().is_empty());

        let (status, body) = get_json(&mut server, "/agentscope/sessions").await;
        assert_eq!(status, 500);
        assert_eq!(body["code"], "failed");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let server = started(Arc::new(RecordingProvider::default()), "").await;
        let url = format!("http://127.0.0.1:{}/agentscope/nope", server.port());
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "not_found");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn binds_ephemeral_port_and_reports_it() {
        let server = started(Arc::new(RecordingProvider::default()), "").await;
        assert_ne!(server.port(), 0);
        assert_eq!(server.addr().ip().to_string(), "127.0.0.1");
        server.shutdown().await;
    }
}
