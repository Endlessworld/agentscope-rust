//! Structured execution observations, mirroring `io.agentscope.core.observation`.
//!
//! An observation is a *persistence* boundary, not a best-effort telemetry callback: a failed
//! acknowledgement must propagate instead of turning into a tool result the model may retry.
//!
//! Rust has no `ToolExecutionDetails` / `EvidenceBinding` equivalents in core yet, so the
//! observation carries only identities, timing, status and error type. Raw arguments, raw
//! output and exception messages are deliberately never copied into an observation event.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::RuntimeContext;
use crate::error::Result;
use crate::message::{ToolResultBlock, ToolResultState, ToolUseBlock};
use crate::session::SessionRecorder;

/// Runtime-context key carrying a caller-supplied [`ActionObserver`].
pub const OBSERVER_CONTEXT_KEY: &str = "agentscope.action.observer";

/// Custom event name for a settled observation.
pub const EVENT_NAME: &str = "action_observation";

/// One execution attempt's lifecycle fact. `Returned` is not a business success verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStatus {
    Started,
    Returned,
    Failed,
    Denied,
    Suspended,
    Interrupted,
}

impl ActionStatus {
    /// The wire name, matching the Java enum constant.
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionStatus::Started => "STARTED",
            ActionStatus::Returned => "RETURNED",
            ActionStatus::Failed => "FAILED",
            ActionStatus::Denied => "DENIED",
            ActionStatus::Suspended => "SUSPENDED",
            ActionStatus::Interrupted => "INTERRUPTED",
        }
    }

    /// Whether this marks the beginning of an attempt.
    pub fn is_started(&self) -> bool {
        matches!(self, ActionStatus::Started)
    }
}

/// One execution attempt's lifecycle fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionObservation {
    pub action_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub agent_id: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: ActionStatus,
    pub error_type: Option<String>,
}

impl ActionObservation {
    /// Opens an attempt for `call` bound to the execution identity in `ctx`.
    pub fn start(ctx: &RuntimeContext, call: &ToolUseBlock) -> Self {
        Self {
            action_id: uuid::Uuid::new_v4().to_string(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            agent_id: ctx.agent_id().map(str::to_string),
            user_id: ctx.user_id().map(str::to_string),
            session_id: ctx.session_id().map(str::to_string),
            started_at: now_millis(),
            ended_at: None,
            status: ActionStatus::Started,
            error_type: None,
        }
    }

    /// Settles the attempt with a terminal status.
    pub fn settled(&self, status: ActionStatus, error_type: Option<String>) -> Self {
        Self {
            ended_at: Some(now_millis()),
            status,
            error_type,
            ..self.clone()
        }
    }

    /// The event payload. No arguments, raw output or exception messages are included.
    pub fn event_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::Map::new();
        payload.insert("action_id".into(), serde_json::json!(self.action_id));
        if !self.tool_call_id.is_empty() {
            payload.insert("tool_call_id".into(), serde_json::json!(self.tool_call_id));
        }
        payload.insert("tool_name".into(), serde_json::json!(self.tool_name));
        payload.insert("status".into(), serde_json::json!(self.status.as_str()));
        payload.insert("started_at".into(), serde_json::json!(self.started_at));
        if let Some(ended_at) = self.ended_at {
            payload.insert("ended_at".into(), serde_json::json!(ended_at));
        }
        if let Some(error_type) = &self.error_type {
            payload.insert("error_type".into(), serde_json::json!(error_type));
        }
        serde_json::Value::Object(payload)
    }
}

/// The status implied by a returned tool result.
pub fn status_of(result: &ToolResultBlock) -> ActionStatus {
    if result.is_suspended() {
        return ActionStatus::Suspended;
    }
    match result.state {
        ToolResultState::Denied => ActionStatus::Denied,
        ToolResultState::Interrupted => ActionStatus::Interrupted,
        ToolResultState::Error => ActionStatus::Failed,
        _ => ActionStatus::Returned,
    }
}

/// An acknowledged persistence boundary for observations.
#[async_trait::async_trait]
pub trait ActionObserver: Send + Sync {
    /// Records one observation; `result` is `None` for start, cancellation or an
    /// uncaught execution error.
    async fn record(
        &self,
        observation: &ActionObservation,
        result: Option<&ToolResultBlock>,
    ) -> Result<()>;
}

/// A caller-supplied observer boxed for storage in a runtime context.
#[derive(Clone)]
pub struct ObserverHandle(pub Arc<dyn ActionObserver>);

impl std::fmt::Debug for ObserverHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObserverHandle")
    }
}

/// An observer that records observations into the session journal.
pub struct SessionActionObserver {
    recorder: Arc<SessionRecorder>,
}

impl SessionActionObserver {
    pub fn new(recorder: Arc<SessionRecorder>) -> Self {
        Self { recorder }
    }
}

#[async_trait::async_trait]
impl ActionObserver for SessionActionObserver {
    async fn record(
        &self,
        observation: &ActionObservation,
        result: Option<&ToolResultBlock>,
    ) -> Result<()> {
        if observation.status.is_started() {
            self.recorder
                .action_started(&observation.tool_call_id, &observation.action_id);
        }
        if let Some(result) = result {
            if result.is_suspended() {
                self.recorder.append(
                    "tool/decision",
                    &serde_json::json!({
                        "toolCallId": observation.tool_call_id,
                        "decision": "suspended",
                    }),
                )?;
            } else if result.state == ToolResultState::Denied {
                self.recorder.append(
                    "tool/decision",
                    &serde_json::json!({
                        "toolCallId": observation.tool_call_id,
                        "decision": "denied",
                    }),
                )?;
            }
        }
        let payload = serde_json::json!({
            "observation": observation.event_payload(),
            "toolCallId": observation.tool_call_id,
            "result": result,
        });
        // An acknowledged boundary: the buffered facts are committed before returning.
        self.recorder
            .record(
                if observation.status.is_started() {
                    "action/start"
                } else {
                    "action/end"
                },
                &payload,
            )
            .await?;
        Ok(())
    }
}

/// Resolves the observer for this execution: the session recorder when one is attached,
/// otherwise a caller-supplied observer from the runtime context.
fn observer_for(ctx: &RuntimeContext) -> Option<Box<dyn ActionObserver>> {
    if let Some(recorder) = SessionRecorder::from_context(ctx) {
        return Some(Box::new(SessionActionObserver::new(recorder)));
    }
    ctx.extension::<ObserverHandle>(OBSERVER_CONTEXT_KEY)
        .map(|handle| Box::new(ObserverHandle(handle.0.clone())) as Box<dyn ActionObserver>)
}

#[async_trait::async_trait]
impl ActionObserver for ObserverHandle {
    async fn record(
        &self,
        observation: &ActionObservation,
        result: Option<&ToolResultBlock>,
    ) -> Result<()> {
        self.0.record(observation, result).await
    }
}

/// Runs `execution` and records its lifecycle when an observer is configured.
///
/// The start fact is acknowledged **before** the tool runs. A failure to record never becomes
/// a tool result: the caller sees the observation error, because automatically replaying an
/// action whose outcome could not be acknowledged is unsafe.
pub async fn observe<F>(ctx: &RuntimeContext, call: &ToolUseBlock, execution: F) -> Result<ToolResultBlock>
where
    F: std::future::Future<Output = Result<ToolResultBlock>>,
{
    let Some(observer) = observer_for(ctx) else {
        return execution.await;
    };
    let started = ActionObservation::start(ctx, call);
    observer
        .record(&started, None)
        .await
        .map_err(|error| crate::error::Error::ActionObservation(error.to_string()))?;

    match execution.await {
        Ok(result) => {
            // Bind the call identity so a result is never attributed to another call.
            let mut identified = result;
            identified.id = Some(call.id.clone());
            identified.name = Some(call.name.clone());
            let settled = started.settled(status_of(&identified), None);
            observer
                .record(&settled, Some(&identified))
                .await
                .map_err(|error| crate::error::Error::ActionObservation(error.to_string()))?;
            Ok(identified)
        }
        Err(error) => {
            let settled = started.settled(ActionStatus::Failed, Some(error_kind(&error)));
            // A failed acknowledgement is surfaced as an observation error so the caller
            // does not fold it into a retryable tool failure.
            observer
                .record(&settled, None)
                .await
                .map_err(|record| crate::error::Error::ActionObservation(record.to_string()))?;
            Err(error)
        }
    }
}

/// Alias exported for callers that prefer an explicit verb-object name.
pub use observe as observe_action;

/// The error category only: messages can contain source content.
fn error_kind(error: &crate::error::Error) -> String {
    match error {
        crate::error::Error::Json(_) => "Json".into(),
        crate::error::Error::Io(_) => "Io".into(),
        crate::error::Error::Http(_) => "Http".into(),
        crate::error::Error::HttpStatus { .. } => "HttpStatus".into(),
        crate::error::Error::Model(_) => "Model".into(),
        crate::error::Error::Tool(_) => "Tool".into(),
        crate::error::Error::ToolSuspended(_) => "ToolSuspended".into(),
        crate::error::Error::Agent(_) => "Agent".into(),
        crate::error::Error::Interrupted(_) => "Interrupted".into(),
        crate::error::Error::ShuttingDown(_) => "ShuttingDown".into(),
        crate::error::Error::Session(_) => "Session".into(),
        crate::error::Error::ActionObservation(_) => "ActionObservation".into(),
        crate::error::Error::ConcurrentSessionModification { .. } => {
            "ConcurrentSessionModification".into()
        }
        crate::error::Error::Other(_) => "Other".into(),
        crate::error::Error::CompositeAgent(_) => "CompositeAgent".into(),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::message::{ContentBlock, TextBlock};
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionLog,
    };

    #[derive(Default)]
    struct RecordingObserver {
        seen: Mutex<Vec<(ActionStatus, Option<ToolResultState>)>>,
    }

    #[async_trait::async_trait]
    impl ActionObserver for RecordingObserver {
        async fn record(
            &self,
            observation: &ActionObservation,
            result: Option<&ToolResultBlock>,
        ) -> Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push((observation.status.clone(), result.map(|r| r.state)));
            Ok(())
        }
    }

    struct FailingObserver;

    #[async_trait::async_trait]
    impl ActionObserver for FailingObserver {
        async fn record(
            &self,
            _observation: &ActionObservation,
            _result: Option<&ToolResultBlock>,
        ) -> Result<()> {
            Err(crate::error::Error::Session("cannot persist".into()))
        }
    }

    fn call() -> ToolUseBlock {
        ToolUseBlock::new("call-1", "read_file", Default::default())
    }

    fn ctx_with(observer: Arc<dyn ActionObserver>) -> RuntimeContext {
        RuntimeContext::new()
            .with_user_id("alice")
            .with_session_id("s1")
            .with_extension(OBSERVER_CONTEXT_KEY, ObserverHandle(observer))
    }

    #[test]
    fn status_mapping_follows_result_state() {
        let mut result = ToolResultBlock::new("c", "t", vec![]);
        assert_eq!(status_of(&result), ActionStatus::Returned);
        for (state, expected) in [
            (ToolResultState::Error, ActionStatus::Failed),
            (ToolResultState::Denied, ActionStatus::Denied),
            (ToolResultState::Interrupted, ActionStatus::Interrupted),
            (ToolResultState::Suspended, ActionStatus::Suspended),
        ] {
            result.state = state;
            assert_eq!(status_of(&result), expected, "{state:?}");
        }
    }

    #[test]
    fn payload_exposes_identity_but_never_content() {
        let ctx = RuntimeContext::new()
            .with_user_id("alice")
            .with_session_id("s1")
            .with_agent_id("agent");
        let observation = ActionObservation::start(&ctx, &call())
            .settled(ActionStatus::Returned, None);
        let payload = observation.event_payload();
        assert_eq!(payload["tool_name"], "read_file");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["status"], "RETURNED");
        assert!(payload["started_at"].as_i64().unwrap() > 0);
        assert!(payload["ended_at"].as_i64().is_some());
        // Identities are not copied into the payload.
        assert!(payload.get("user_id").is_none());
        assert!(payload.get("session_id").is_none());
    }

    #[tokio::test]
    async fn an_unobserved_execution_is_returned_unchanged() {
        let ctx = RuntimeContext::new();
        let result = observe(&ctx, &call(), async {
            Ok(ToolResultBlock::new("", "", vec![ContentBlock::Text(TextBlock::new("ok"))]))
        })
        .await
        .unwrap();
        // Without an observer the execution result passes through untouched (Java:
        // `Mono.defer(execution)`); identity binding is the acting phase's job.
        assert!(result.id.as_deref().unwrap_or("").is_empty());
        assert_eq!(result.output.len(), 1);
    }

    #[tokio::test]
    async fn a_configured_observer_sees_start_and_end() {
        let observer = Arc::new(RecordingObserver::default());
        let ctx = ctx_with(observer.clone());
        let result = observe(&ctx, &call(), async {
            Ok(ToolResultBlock::new("x", "y", vec![]).with_state(ToolResultState::Success))
        })
        .await
        .unwrap();
        assert_eq!(result.id.as_deref(), Some("call-1"));
        let seen = observer.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, ActionStatus::Started);
        assert!(seen[0].1.is_none());
        assert_eq!(seen[1].0, ActionStatus::Returned);
        assert_eq!(seen[1].1, Some(ToolResultState::Success));
    }

    #[tokio::test]
    async fn an_execution_error_settles_as_failed_without_a_result() {
        let observer = Arc::new(RecordingObserver::default());
        let ctx = ctx_with(observer.clone());
        let err = observe::<_>(&ctx, &call(), async {
            Err(crate::error::Error::Tool("boom: secret body".into()))
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("secret body"));
        let seen = observer.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].0, ActionStatus::Failed);
        assert!(seen[1].1.is_none());
    }

    #[tokio::test]
    async fn a_failed_acknowledgement_propagates_instead_of_returning_a_result() {
        let ctx = ctx_with(Arc::new(FailingObserver));
        let err = observe(&ctx, &call(), async {
            Ok(ToolResultBlock::new("x", "y", vec![]))
        })
        .await
        .unwrap_err();
        // The dedicated variant lets the agent loop propagate instead of turning the
        // failure into a retryable tool result.
        assert!(
            matches!(err, crate::error::Error::ActionObservation(_)),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_recorder_backed_observer_writes_action_and_decision_facts() {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(storage, "s1"));
        let recorder = Arc::new(
            SessionRecorder::open(Arc::clone(&log), Some("t1"))
                .await
                .unwrap(),
        );
        let ctx = RuntimeContext::new().with_extension_arc(
            crate::session::RECORDER_CONTEXT_KEY,
            Arc::clone(&recorder),
        );

        observe(&ctx, &call(), async {
            Ok(ToolResultBlock::new("x", "y", vec![]).with_state(ToolResultState::Denied))
        })
        .await
        .unwrap();
        recorder.flush().await.unwrap();

        let head = log.head().await.unwrap();
        let types: Vec<String> = log
            .scan(0, head.seq)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event_type)
            .collect();
        assert_eq!(
            types,
            vec!["action/start", "tool/decision", "action/end"],
            "{types:?}"
        );
        // The action id is correlated with the tool call.
        assert!(!recorder.action_id("call-1").is_empty());
        recorder.close().await;
    }
}
