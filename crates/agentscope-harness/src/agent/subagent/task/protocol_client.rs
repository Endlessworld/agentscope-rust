//! HTTP client for the internal AgentScope task protocol
//! (`POST/GET /tasks/...`), mirroring
//! `io.agentscope.harness.agent.subagent.task.AgentProtocolTaskClient` plus the
//! remote transport support types (`RemoteTarget`, `RemoteTaskStatus`,
//! `RemoteSubmitContext`, `RemoteSubagentTransport`, `AgentProtocolTransport`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result};

use super::super::protocol::{RemoteAgentEvent, RemoteConfirmDecision, RemotePendingConfirm};

/// Remote endpoint identity: base URL plus optional auth/routing headers.
#[derive(Debug, Clone)]
pub struct RemoteTarget {
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
}

impl RemoteTarget {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            headers: BTreeMap::new(),
        }
    }

    pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
}

/// Status snapshot returned by `GET /tasks/{taskId}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTaskStatus {
    pub status: String,
    pub error: Option<String>,
    pub pending_confirms: Vec<RemotePendingConfirm>,
}

impl RemoteTaskStatus {
    pub fn new(status: impl Into<String>, error: Option<String>) -> Self {
        Self {
            status: status.into(),
            error,
            pending_confirms: Vec::new(),
        }
    }

    pub fn is_awaiting_confirm(&self) -> bool {
        self.status.eq_ignore_ascii_case("awaiting_confirm")
    }

    pub fn is_terminal_success(&self) -> bool {
        self.status.eq_ignore_ascii_case("success")
    }

    pub fn is_terminal_failure(&self) -> bool {
        self.status.eq_ignore_ascii_case("error") || self.status.eq_ignore_ascii_case("failed")
    }

    pub fn is_cancelled(&self) -> bool {
        self.status.eq_ignore_ascii_case("cancelled")
            || self.status.eq_ignore_ascii_case("canceled")
    }
}

/// Context sent with a remote task submission (permissions, streaming preferences,
/// parent identity).
#[derive(Debug, Clone)]
pub struct RemoteSubmitContext {
    pub user_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub stream: bool,
    pub detail: String,
    pub deny_rules: Vec<BTreeMap<String, String>>,
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl Default for RemoteSubmitContext {
    fn default() -> Self {
        Self {
            user_id: None,
            parent_session_id: None,
            stream: true,
            detail: "status".to_string(),
            deny_rules: Vec::new(),
            attributes: BTreeMap::new(),
        }
    }
}

impl RemoteSubmitContext {
    pub fn builder() -> RemoteSubmitContextBuilder {
        RemoteSubmitContextBuilder::default()
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Serializes to the protocol `context` object (`snake_case`, optional keys omitted).
    pub fn to_map(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        if let Some(uid) = &self.user_id {
            m.insert("user_id".into(), serde_json::Value::String(uid.clone()));
        }
        if let Some(pid) = &self.parent_session_id {
            m.insert(
                "parent_session_id".into(),
                serde_json::Value::String(pid.clone()),
            );
        }
        m.insert("stream".into(), serde_json::Value::Bool(self.stream));
        m.insert(
            "detail".into(),
            serde_json::Value::String(self.detail.clone()),
        );
        if !self.deny_rules.is_empty() {
            m.insert(
                "deny_rules".into(),
                serde_json::to_value(&self.deny_rules).unwrap_or(serde_json::Value::Null),
            );
        }
        if !self.attributes.is_empty() {
            m.insert(
                "attributes".into(),
                serde_json::to_value(&self.attributes).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(m)
    }
}

/// Builder for [`RemoteSubmitContext`].
#[derive(Default)]
pub struct RemoteSubmitContextBuilder {
    ctx: RemoteSubmitContext,
}

impl RemoteSubmitContextBuilder {
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.ctx.user_id = Some(user_id.into());
        self
    }

    pub fn parent_session_id(mut self, id: impl Into<String>) -> Self {
        self.ctx.parent_session_id = Some(id.into());
        self
    }

    pub fn stream(mut self, stream: bool) -> Self {
        self.ctx.stream = stream;
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.ctx.detail = detail.into();
        self
    }

    pub fn deny_rules(mut self, rules: Vec<BTreeMap<String, String>>) -> Self {
        self.ctx.deny_rules = rules;
        self
    }

    pub fn attributes(mut self, attributes: BTreeMap<String, serde_json::Value>) -> Self {
        self.ctx.attributes = attributes;
        self
    }

    pub fn build(self) -> RemoteSubmitContext {
        self.ctx
    }
}

/// Handle that stops an SSE event subscription on drop / [`close`](Self::close).
pub struct EventStreamHandle {
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EventStreamHandle {
    /// Creates a no-op handle (server without streaming support).
    pub fn noop() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
            task: None,
        }
    }

    /// Signals the reader loop to stop.
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether the reader has been asked to stop.
    pub fn is_closed(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for EventStreamHandle {
    fn drop(&mut self) {
        self.close();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Minimal HTTP client for the internal AgentScope task protocol.
pub struct AgentProtocolTaskClient {
    http: reqwest::Client,
}

impl Default for AgentProtocolTaskClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProtocolTaskClient {
    pub fn new() -> Self {
        agentscope_core::tls::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// `POST /tasks` with body `{task_id, agent_id, input, context?}`.
    pub async fn submit_task(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
        agent_id: &str,
        input: &str,
        context: Option<&RemoteSubmitContext>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "task_id": task_id,
            "agent_id": agent_id,
            "input": input,
        });
        if let Some(ctx) = context {
            body["context"] = ctx.to_map();
        }
        let mut req = self
            .http
            .post(join(base_url, "/tasks"))
            .timeout(Duration::from_secs(600))
            .header("Content-Type", "application/json")
            .json(&body);
        req = apply_headers(req, headers);
        let resp = req.send().await.map_err(http_err)?;
        ensure_status_ok(resp, "submitTask").await
    }

    /// `GET /tasks/{taskId}`.
    pub async fn get_status(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
    ) -> Result<RemoteTaskStatus> {
        let req = apply_headers(
            self.http
                .get(join(base_url, &format!("/tasks/{}", encode(task_id))))
                .timeout(Duration::from_secs(120)),
            headers,
        );
        let resp = req.send().await.map_err(http_err)?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(RemoteTaskStatus::new(
                "error",
                Some("task not found".to_string()),
            ));
        }
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() >= 400 {
            return Ok(RemoteTaskStatus::new(
                "error",
                Some(format!("HTTP {}: {body}", status.as_u16())),
            ));
        }
        let value: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        let st = value
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let err = value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let pending = parse_pending_confirms(value.get("pending_confirms"));
        Ok(RemoteTaskStatus {
            status: st,
            error: err,
            pending_confirms: pending,
        })
    }

    /// `GET /tasks/{taskId}/wait?timeout_seconds=<n>`.
    pub async fn wait_for_result(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
        timeout_seconds: u64,
    ) -> Result<String> {
        let timeout = timeout_seconds.max(1);
        let path = format!(
            "/tasks/{}/wait?timeout_seconds={}",
            encode(task_id),
            timeout
        );
        let req = apply_headers(
            self.http
                .get(join(base_url, &path))
                .timeout(Duration::from_secs(timeout + 60)),
            headers,
        );
        let resp = req.send().await.map_err(http_err)?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() >= 400 {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                message: format!("waitForResult: {body}"),
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        Ok(value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// `POST /tasks/{taskId}/cancel`.
    pub async fn cancel_task(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
    ) -> Result<()> {
        let req = apply_headers(
            self.http
                .post(join(
                    base_url,
                    &format!("/tasks/{}/cancel", encode(task_id)),
                ))
                .timeout(Duration::from_secs(120)),
            headers,
        );
        let resp = req.send().await.map_err(http_err)?;
        ensure_status_ok(resp, "cancelTask").await
    }

    /// `POST /tasks/{taskId}/resume` with body `{decisions: [...]}`.
    pub async fn resume_task(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
        decisions: &[RemoteConfirmDecision],
    ) -> Result<()> {
        let body = resume_body(decisions);
        let req = apply_headers(
            self.http
                .post(join(
                    base_url,
                    &format!("/tasks/{}/resume", encode(task_id)),
                ))
                .timeout(Duration::from_secs(120))
                .header("Content-Type", "application/json")
                .json(&body),
            headers,
        );
        let resp = req.send().await.map_err(http_err)?;
        ensure_status_ok(resp, "resumeTask").await
    }

    /// Opens `GET /tasks/{taskId}/events` as an SSE stream, delivering parsed events
    /// to `consumer`. Returns a no-op handle when the server returns 404/405.
    pub fn open_event_stream(
        &self,
        base_url: &str,
        headers: &BTreeMap<String, String>,
        task_id: &str,
        from_seq: i64,
        consumer: Arc<dyn Fn(RemoteAgentEvent) + Send + Sync>,
    ) -> EventStreamHandle {
        let mut path = format!("/tasks/{}/events", encode(task_id));
        if from_seq > 0 {
            path = format!("{path}?from_seq={from_seq}");
        }
        let url = join(base_url, &path);
        let client = self.http.clone();
        let headers = headers.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_task = stop.clone();
        let task = tokio::spawn(async move {
            let mut req = client
                .get(&url)
                .timeout(Duration::from_secs(3 * 60 * 60))
                .header("Accept", "text/event-stream");
            if from_seq > 0 {
                req = req.header("Last-Event-ID", from_seq.to_string());
            }
            req = apply_headers(req, &headers);
            let resp = match req.send().await {
                Ok(r) => r,
                Err(_) => return,
            };
            let status = resp.status().as_u16();
            if status == 404 || status == 405 {
                return;
            }
            if status >= 400 {
                return;
            }
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let mut buffer = String::new();
            while !stop_for_task.load(Ordering::SeqCst) {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(idx) = buffer.find("\n\n") {
                            let raw = buffer[..idx].to_string();
                            buffer.drain(..idx + 2);
                            dispatch_sse_block(&raw, &consumer);
                        }
                    }
                    _ => break,
                }
            }
        });
        EventStreamHandle {
            stop,
            task: Some(task),
        }
    }
}

/// Transport abstraction for remote subagent execution (Agent Protocol today).
#[async_trait::async_trait]
pub trait RemoteSubagentTransport: Send + Sync {
    /// Stable transport type string persisted on `TaskRecord`.
    fn transport_type(&self) -> &str;
    async fn submit(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        agent_id: &str,
        input: &str,
        context: Option<&RemoteSubmitContext>,
    ) -> Result<()>;
    async fn get_status(&self, target: &RemoteTarget, task_id: &str) -> Result<RemoteTaskStatus>;
    async fn wait_for_result(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        timeout_seconds: u64,
    ) -> Result<String>;
    async fn cancel(&self, target: &RemoteTarget, task_id: &str) -> Result<()>;
    async fn resume(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        decisions: &[RemoteConfirmDecision],
    ) -> Result<()>;
    /// Opens an event stream. Default is a no-op so transports without streaming
    /// degrade gracefully to polling.
    fn stream_events(
        &self,
        _target: &RemoteTarget,
        _task_id: &str,
        _from_seq: i64,
        _consumer: Arc<dyn Fn(RemoteAgentEvent) + Send + Sync>,
    ) -> EventStreamHandle {
        EventStreamHandle::noop()
    }
}

/// [`RemoteSubagentTransport`] backed by the Agent Protocol HTTP client.
pub struct AgentProtocolTransport {
    client: AgentProtocolTaskClient,
}

impl Default for AgentProtocolTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProtocolTransport {
    pub const TYPE: &'static str = "agent-protocol";

    pub fn new() -> Self {
        Self {
            client: AgentProtocolTaskClient::new(),
        }
    }

    pub fn with_client(client: AgentProtocolTaskClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl RemoteSubagentTransport for AgentProtocolTransport {
    fn transport_type(&self) -> &str {
        Self::TYPE
    }

    async fn submit(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        agent_id: &str,
        input: &str,
        context: Option<&RemoteSubmitContext>,
    ) -> Result<()> {
        self.client
            .submit_task(
                &target.base_url,
                &target.headers,
                task_id,
                agent_id,
                input,
                context,
            )
            .await
    }

    async fn get_status(&self, target: &RemoteTarget, task_id: &str) -> Result<RemoteTaskStatus> {
        self.client
            .get_status(&target.base_url, &target.headers, task_id)
            .await
    }

    async fn wait_for_result(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        timeout_seconds: u64,
    ) -> Result<String> {
        self.client
            .wait_for_result(&target.base_url, &target.headers, task_id, timeout_seconds)
            .await
    }

    async fn cancel(&self, target: &RemoteTarget, task_id: &str) -> Result<()> {
        self.client
            .cancel_task(&target.base_url, &target.headers, task_id)
            .await
    }

    async fn resume(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        decisions: &[RemoteConfirmDecision],
    ) -> Result<()> {
        self.client
            .resume_task(&target.base_url, &target.headers, task_id, decisions)
            .await
    }

    fn stream_events(
        &self,
        target: &RemoteTarget,
        task_id: &str,
        from_seq: i64,
        consumer: Arc<dyn Fn(RemoteAgentEvent) + Send + Sync>,
    ) -> EventStreamHandle {
        self.client.open_event_stream(
            &target.base_url,
            &target.headers,
            task_id,
            from_seq,
            consumer,
        )
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Builds the `{decisions: [...]}` body for `POST /tasks/{taskId}/resume`.
///
/// The optional `reason` is forwarded verbatim and omitted when absent, so a peer
/// that does not know the field sees exactly the payload it always did.
fn resume_body(decisions: &[RemoteConfirmDecision]) -> serde_json::Value {
    let decisions: Vec<serde_json::Value> = decisions
        .iter()
        .map(|d| {
            let mut value = serde_json::json!({
                "toolCallId": d.tool_call_id,
                "approved": d.approved,
            });
            if let Some(reason) = &d.reason {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert(
                        "reason".to_string(),
                        serde_json::Value::String(reason.clone()),
                    );
                }
            }
            value
        })
        .collect();
    serde_json::json!({ "decisions": decisions })
}

fn http_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Other(format!("Agent Protocol request timed out: {e}"))
    } else {
        Error::Other(format!("Agent Protocol transport error: {e}"))
    }
}

async fn ensure_status_ok(resp: reqwest::Response, op: &str) -> Result<()> {
    let status = resp.status();
    if status.as_u16() >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            message: format!("{op}: {body}"),
        });
    }
    Ok(())
}

fn apply_headers(
    req: reqwest::RequestBuilder,
    headers: &BTreeMap<String, String>,
) -> reqwest::RequestBuilder {
    let mut req = req;
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req
}

fn join(base_url: &str, path: &str) -> String {
    let base = base_url.strip_suffix('/').unwrap_or(base_url);
    format!("{base}{path}")
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn parse_pending_confirms(node: Option<&serde_json::Value>) -> Vec<RemotePendingConfirm> {
    let Some(serde_json::Value::Array(items)) = node else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            let get = |a: &str, b: &str| {
                item.get(a)
                    .or_else(|| item.get(b))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            RemotePendingConfirm {
                tool_call_id: get("toolCallId", "tool_call_id"),
                tool_name: get("toolName", "tool_name"),
                tool_input_json: get("toolInputJson", "tool_input_json"),
            }
        })
        .collect()
}

fn dispatch_sse_block(raw: &str, consumer: &Arc<dyn Fn(RemoteAgentEvent) + Send + Sync>) {
    let mut data = String::new();
    let mut event_id: Option<String> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("id:") {
            event_id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(payload);
        }
    }
    if data.is_empty() {
        return;
    }
    if let Ok(mut event) = serde_json::from_str::<RemoteAgentEvent>(&data) {
        if event.seq <= 0 {
            if let Some(id) = event_id.and_then(|s| s.trim().parse::<i64>().ok()) {
                event.seq = id;
            }
        }
        consumer(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_context_omits_absent_fields() {
        let ctx = RemoteSubmitContext::empty();
        let map = ctx.to_map();
        assert_eq!(map["stream"], true);
        assert_eq!(map["detail"], "status");
        assert!(map.get("user_id").is_none());
        assert!(map.get("attributes").is_none());
    }

    #[test]
    fn submit_context_builder_serializes_all() {
        let ctx = RemoteSubmitContext::builder()
            .user_id("alice")
            .parent_session_id("p1")
            .detail("full")
            .build();
        let map = ctx.to_map();
        assert_eq!(map["user_id"], "alice");
        assert_eq!(map["parent_session_id"], "p1");
        assert_eq!(map["detail"], "full");
    }

    #[test]
    fn task_status_predicates() {
        assert!(RemoteTaskStatus::new("awaiting_confirm", None).is_awaiting_confirm());
        assert!(RemoteTaskStatus::new("success", None).is_terminal_success());
        assert!(RemoteTaskStatus::new("failed", None).is_terminal_failure());
        assert!(RemoteTaskStatus::new("cancelled", None).is_cancelled());
        assert!(!RemoteTaskStatus::new("running", None).is_terminal_success());
    }

    #[test]
    fn pending_confirms_parse_both_key_styles() {
        let node = serde_json::json!([
            {"toolCallId": "c1", "toolName": "shell", "toolInputJson": "{}"},
            {"tool_call_id": "c2", "tool_name": "read", "tool_input_json": "[]"}
        ]);
        let parsed = parse_pending_confirms(Some(&node));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(parsed[1].tool_name.as_deref(), Some("read"));
        assert!(parse_pending_confirms(None).is_empty());
    }

    #[test]
    fn resume_body_forwards_reason_and_omits_it_when_absent() {
        let body = resume_body(&[
            RemoteConfirmDecision::new("c1", false).with_reason("user denied"),
            RemoteConfirmDecision::new("c2", true),
        ]);
        let decisions = body["decisions"].as_array().unwrap();
        assert_eq!(decisions[0]["toolCallId"], "c1");
        assert_eq!(decisions[0]["approved"], false);
        assert_eq!(decisions[0]["reason"], "user denied");
        assert_eq!(decisions[1]["approved"], true);
        assert!(
            decisions[1].get("reason").is_none(),
            "absent reason must not reach the wire: {decisions:?}"
        );
    }

    #[test]
    fn url_join_and_encode() {
        assert_eq!(join("http://h:1/", "/tasks"), "http://h:1/tasks");
        assert_eq!(join("http://h:1", "/tasks"), "http://h:1/tasks");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn sse_block_dispatch_parses_event_and_seq() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Vec<RemoteAgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let consumer: Arc<dyn Fn(RemoteAgentEvent) + Send + Sync> = Arc::new(move |e| {
            sink.lock().unwrap().push(e);
        });
        let block = "id: 5\ndata: {\"type\":\"TEXT_DELTA\",\"text\":\"hi\"}";
        dispatch_sse_block(block, &consumer);
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 5);
        assert_eq!(events[0].text.as_deref(), Some("hi"));
    }
}
