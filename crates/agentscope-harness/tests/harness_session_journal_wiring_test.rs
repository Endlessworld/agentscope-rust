//! Wiring test for native session history.
//!
//! These assertions go through the **real** `HarnessAgent::build()` + call path, because the
//! bug class they guard against is a missing assembly step: a journal backend that exists but
//! is never injected, or a recorder that is only created when a host happens to build an
//! `AgentSession`. A unit test on `SessionRecorder` cannot catch either.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::{CallableAgent, RuntimeContext};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{InMemorySessionLogStore, SessionHistoryMode, SessionLogStore};

use agentscope_harness::agent::HarnessAgent;

/// A model that always answers with `reply` and records how often it ran.
struct FixedModel {
    reply: String,
    calls: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl Model for FixedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        *self.calls.lock().unwrap() += 1;
        let reply = self.reply.clone();
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(reply))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "fixed"
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_session_journal_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// The caller identity used by every assertion below.
fn ctx() -> RuntimeContext {
    RuntimeContext::new().with_user_id("alice")
}

#[tokio::test]
async fn a_bare_harness_agent_call_journals_by_default() {
    // No `session_log_store` on the builder: `build()` must fall back to the workspace
    // backend, and the call itself must open the recorder (Java `acquireCallExecution`).
    let dir = temp_dir("default");
    let calls = Arc::new(Mutex::new(0));
    let agent = HarnessAgent::builder()
        .name("journal-agent")
        .description("journal wiring")
        .model(Arc::new(FixedModel {
            reply: "acknowledged".into(),
            calls: Arc::clone(&calls),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .session_id("s-journal")
        .user_id("alice")
        .max_iters(1)
        .build();

    // The store was injected, so the agent can address a journal before running.
    let key = agent.session_key(&ctx()).expect("journal key");
    assert_eq!(key.session_id, "s-journal");

    let reply = agentscope_core::agent::CallableAgent::call(&agent, vec![Msg::user("hello")])
        .await
        .expect("agent call");
    assert_eq!(reply.text_content(), "acknowledged");
    // The assertion below is about journaling, not call counts: when no dedicated memory
    // model is configured the maintenance middleware reuses this model, so the exact number
    // of invocations is not 1. Only "the turn ran" matters here.
    assert!(*calls.lock().unwrap() >= 1, "the model never ran");

    // The call wrote committed history: the facade views read it back.
    let transcript = agent.session_transcript(&ctx()).await.expect("transcript");
    let text: Vec<String> = transcript
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect();
    assert!(
        text.iter().any(|t| t.contains("hello")),
        "input missing from journal: {text:?}"
    );
    assert!(
        text.iter().any(|t| t.contains("acknowledged")),
        "reply missing from journal: {text:?}"
    );

    // Inspection agrees, and the projection restores the committed state.
    let projection = agent.inspect_session(&ctx()).await.expect("projection");
    assert!(projection.as_of_seq > 0, "no events committed");
    let restored = projection.restore().expect("restore").expect("state");
    assert_eq!(restored.session_id, "s-journal");
}

#[tokio::test]
async fn the_request_payload_switch_decides_what_the_journal_records() {
    // The switch is only useful if it survives the whole assembly path (builder -> agent ->
    // recorder). It must not touch recovery: the call stays discoverable as dispatched.
    for record in [false, true] {
        let dir = temp_dir(if record {
            "requests-on"
        } else {
            "requests-off"
        });
        let store = Arc::new(InMemorySessionLogStore::new());
        let agent = HarnessAgent::builder()
            .name("request-payload")
            .model(Arc::new(FixedModel {
                reply: "ok".into(),
                calls: Arc::new(Mutex::new(0)),
            }))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .session_id("s-requests")
            .user_id("alice")
            .max_iters(1)
            .record_model_requests(record)
            .session_log_store(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .build();

        CallableAgent::call(&agent, vec![Msg::user("hello")])
            .await
            .expect("agent call");

        let key = agent.session_key(&ctx()).expect("journal key");
        let log = store.open(&key, &ctx()).await.expect("open");
        let events = log.read_after(0, 4096).await.expect("read");
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types.contains(&"request/prepared"),
            record,
            "record_model_requests={record}: {types:?}"
        );
        assert!(
            types.contains(&"model/dispatch"),
            "the call must stay discoverable: {types:?}"
        );
    }
}

#[tokio::test]
async fn a_caller_supplied_store_is_used_instead_of_the_workspace_default() {
    let dir = temp_dir("explicit");
    let store = Arc::new(InMemorySessionLogStore::new());
    let agent = HarnessAgent::builder()
        .name("explicit-store")
        .model(Arc::new(FixedModel {
            reply: "stored".into(),
            calls: Arc::new(Mutex::new(0)),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .session_id("s-explicit")
        .user_id("alice")
        .max_iters(1)
        .session_log_store(Arc::clone(&store) as Arc<dyn SessionLogStore>)
        .build();

    CallableAgent::call(&agent, vec![Msg::user("in memory")])
        .await
        .expect("agent call");

    // Reading through the caller's store proves the injected backend is the one written to.
    let key = agent.session_key(&ctx()).expect("journal key");
    let log = store.open(&key, &ctx()).await.expect("open");
    let events = log.read_after(0, 256).await.expect("read");
    assert!(
        events.iter().any(|e| e.event_type == "run/start"),
        "no run/start in {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    assert!(events.iter().any(|e| e.event_type.starts_with("turn/")));
}

#[tokio::test]
async fn event_log_mode_writes_no_state_snapshot() {
    // Exactly one history authority. A configured store must not be installed as a second
    // writer while the journal owns recovery: two writers that disagree after a crash would
    // leave the reader picking a winner it cannot verify (Java gates this on
    // `legacySessionHistory` for the same reason).
    let dir = temp_dir("single-authority");
    let store = Arc::new(agentscope_core::state::InMemoryAgentStateStore::new());
    let agent = HarnessAgent::builder()
        .name("single-authority")
        .model(Arc::new(FixedModel {
            reply: "done".into(),
            calls: Arc::new(Mutex::new(0)),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .session_id("s-single")
        .user_id("alice")
        .max_iters(1)
        .state_store(Arc::clone(&store) as Arc<dyn agentscope_core::state::AgentStateStore>)
        .build();

    // The default mode journals...
    assert!(agent.session_key(&ctx()).is_some());
    CallableAgent::call(&agent, vec![Msg::user("hi")])
        .await
        .expect("agent call");
    // ...and the store stayed untouched, so a restart recovers from the journal alone.
    assert_eq!(
        store.get_session_count(),
        0,
        "the snapshot store must not be written while the journal owns history"
    );
}

#[tokio::test]
async fn legacy_mode_installs_no_journal() {
    // LEGACY is the explicit opt-out: no store, so the facade cannot address a journal and
    // the tools are not registered. Nothing silently writes both.
    let dir = temp_dir("legacy");
    let agent = HarnessAgent::builder()
        .name("legacy-agent")
        .model(Arc::new(FixedModel {
            reply: "legacy".into(),
            calls: Arc::new(Mutex::new(0)),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .session_id("s-legacy")
        .user_id("alice")
        .max_iters(1)
        .session_history_mode(SessionHistoryMode::Legacy)
        .build();

    assert!(agent.session_key(&ctx()).is_none());
    assert!(agent.session_log(&ctx()).await.is_err());

    // The call still works; it simply records nothing.
    let reply = CallableAgent::call(&agent, vec![Msg::user("hi")])
        .await
        .expect("agent call");
    assert_eq!(reply.text_content(), "legacy");
}
