//! Two prompts of one durable session, in two processes.
//!
//! The first prompt finishes: its run commits the finalization facts the execution host uses to
//! never replay a reply. The *second* prompt is then sent by another process, which reads that
//! journal before it can run anything: the adapter settles the tool calls the previous attempt
//! left unanswered, and that settlement replays the committed prefix.
//!
//! The finalization facts are `required` extension events, so a reader that does not know their
//! codec stops recovery with `Unknown required event: durable/finalization_started`. Registering
//! that codec is therefore the duty of whoever opens the journal, not of whoever opens the
//! execution host: the settlement reads the journal first.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_core::journal::JournalEvent;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::Toolkit;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::store::file::FileStore;
use agentscope_durable::store::RecordStore;
use agentscope_durable::types::*;
use agentscope_durable::{DurableRuntime, SessionBackend};
use agentscope_durable_adapter_agentscope::{AgentScopeHarnessAdapter, STATE_VERSION};
use agentscope_harness::HarnessAgent;
use futures::stream::BoxStream;
use serde_json::{json, Value};

const HARNESS: &str = "durable-prompt";
const SESSION: &str = "two-prompts";
const LEASE: Duration = Duration::from_secs(30);

/// Store root the child process works on.
const ROOT_ENV: &str = "DURABLE_PROMPT_ROOT";
/// Which prompt the child process sends (`1` or `2`).
const ROUND_ENV: &str = "DURABLE_PROMPT_ROUND";

/// Answers with one fixed text, so a round needs exactly one tick.
struct Reply(&'static str);

impl Model for Reply {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let reply = self.0;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new(reply))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "reply"
    }
}

/// The factory the adapter calls once per tick.
fn factory(
    model: Arc<dyn Model>,
    root: PathBuf,
) -> impl Fn() -> Arc<HarnessAgent> + Send + Sync + 'static {
    move || {
        Arc::new(
            HarnessAgent::builder()
                .name(HARNESS)
                .model(Arc::clone(&model))
                .session_id(SESSION)
                .user_id("alice")
                .max_iters(4)
                .cwd(root.clone())
                .workspace(root.join("workspace"))
                .toolkit(Toolkit::new())
                .disable_compaction()
                .disable_workspace_context()
                .disable_at_path_expansion()
                .disable_memory_hooks()
                .disable_memory_tools()
                .disable_web_tools()
                .disable_shell_tool()
                .disable_subagents()
                .disable_session_persistence()
                .build(),
        )
    }
}

async fn run_round(round: u32, root: PathBuf) {
    let text = if round == 1 { "first" } else { "second" };
    let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&root).expect("file store"));
    let backend = Arc::new(TxnBackend::with_store(store));
    let model: Arc<dyn Model> = Arc::new(Reply(text));
    let runtime = Arc::new(
        DurableRuntime::new(
            Arc::clone(&backend) as Arc<dyn SessionBackend>,
            format!("prompt-process-{round}"),
        )
        .with_adapter(
            HARNESS,
            Arc::new(AgentScopeHarnessAdapter::new(factory(
                model,
                root.join(format!("round-{round}")),
            ))),
        )
        .with_lease_ttl(LEASE),
    );
    let key = SessionKey::new("ns", SESSION).expect("session key");
    runtime
        .ensure_session(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new(HARNESS, "def-1", STATE_VERSION).expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let receipt = runtime
        .submit(&thread, &format!("req-{round}"), json!({ "text": text }))
        .await
        .expect("submit");

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        runtime
            .tick(&thread)
            .await
            .unwrap_or_else(|error| panic!("round {round} tick failed: {error}"));
        let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
        if turn.status.terminal() {
            assert_eq!(
                turn.status,
                TurnStatus::Succeeded,
                "round {round} must commit, not fail: {:?}",
                turn.error
            );
            assert_eq!(
                reply_text(turn.output.clone()).as_deref(),
                Some(text),
                "round {round} commits its reply"
            );
            return;
        }
        assert!(Instant::now() < deadline, "round {round} never settled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn reply_text(value: Value) -> Option<String> {
    serde_json::from_value::<Msg>(value)
        .ok()
        .map(|message| message.text_content())
}

/// Child role: sends one prompt of the session and exits. The parent always spawns a fresh
/// process, so the codec registry this process starts with is the one a real host has.
#[test]
fn one_prompt_of_a_session_in_a_fresh_process() {
    let Ok(root) = std::env::var(ROOT_ENV) else {
        return;
    };
    let round = std::env::var(ROUND_ENV)
        .expect("the round to send")
        .parse::<u32>()
        .expect("a numeric round");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(run_round(round, PathBuf::from(root)));
}

/// The user's scenario: a first prompt finishes, then a second prompt arrives in a process that
/// reads the journal the first one left.
///
/// Before the codec was registered by the journal itself, the second process died in
/// `settle_uncertain_tool_outcomes` with `Unknown required event: durable/finalization_started`
/// and the session could never be continued.
#[test]
fn a_second_prompt_runs_after_a_finalized_first_prompt_in_another_process() {
    let root = scratch_dir();
    run_child(&root, 1);
    run_child(&root, 2);

    // Both prompts are committed facts, read by a process that ran neither.
    let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&root).expect("file store"));
    let backend = TxnBackend::with_store(store);
    let key = SessionKey::new("ns", SESSION).expect("session key");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let turns = block_on(backend.turns(&thread, None, 10)).expect("turns");
    let replies: Vec<Option<String>> = turns
        .items
        .iter()
        .map(|turn| reply_text(turn.output.clone()))
        .collect();
    assert_eq!(turns.items.len(), 2, "one turn per prompt: {replies:?}");
    for (turn, expected) in turns.items.iter().zip(["first", "second"]) {
        assert_eq!(
            turn.status,
            TurnStatus::Succeeded,
            "the prompt committed: {:?}",
            turn.error
        );
        assert_eq!(
            reply_text(turn.output.clone()).as_deref(),
            Some(expected),
            "the committed reply"
        );
    }

    // The failure was a reader problem, not a missing fact: every finalization a prompt wrote
    // has its opening frame in the journal, in order, before its completion.
    let state = block_on(backend.thread_state(&thread)).expect("thread state");
    let frames: Vec<String> = block_on(backend.read_journal(&thread, 0, 1000))
        .expect("journal")
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<JournalEvent>(entry).ok())
        .filter(|event| event.event_type.starts_with("durable/"))
        .map(|event| event.event_type)
        .collect();
    assert_eq!(
        frames,
        [
            "durable/finalization_started",
            "durable/finalization_completed",
            "durable/finalization_started",
            "durable/finalization_completed",
        ],
        "one opening frame per finalization, {} journal entries",
        state.journal_position
    );
}

fn run_child(root: &PathBuf, round: u32) {
    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "one_prompt_of_a_session_in_a_fresh_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT_ENV, root)
        .env(ROUND_ENV, round.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn the prompt process");
    assert!(
        child.status.success(),
        "prompt {round} failed in its own process:\n{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn scratch_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "agentscope-durable-prompts-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}
