//! Regression tests for the cancellation paths of `AcpAgentSession` (ACP-01).
//!
//! Both cases used to leave a prompt running with no way to stop it:
//!
//! * `session/close` closed the session and bumped the cancel generation, but the running
//!   prompt could only notice a cancel *after* the run settled, so closing a session whose
//!   provider never answers waited out the whole 10s settle grace (and `session/close` with
//!   it). The turn now observes the closed flag directly and abandons the turn immediately:
//!   there is no client left to answer, so a pristine journal is worth nothing there.
//! * A `session/cancel` that arrived while a follow-up prompt was still queued behind the
//!   previous turn was silently dropped: the queued prompt captured the cancel generation
//!   *after* the wait and therefore treated the cancel as "before my turn" and ran anyway.
//!
//! Both tests use a model that never returns, so they are deterministic: no live provider, no
//! network and no timer race. The only timing bound asserts that the fix takes effect, while
//! the unfixed behaviour is either a deterministic 10s or an unbounded wait.
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{ContentBlock, SessionId, StopReason, TextContent};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpAgentSession;
use agentscope_harness::agent::HarnessAgent;

/// A model whose stream never yields, so the turn only ends when it is cancelled.
struct HangingModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Model for HangingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::pending())
    }

    fn model_name(&self) -> &str {
        "hanging"
    }
}

/// First call hangs forever, later calls answer at once. Lets a test hold the session with
/// turn 1 while observing what turn 2 (queued behind it) does.
struct HangingFirstModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Model for HangingFirstModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Box::pin(futures::stream::pending())
        } else {
            Box::pin(futures::stream::once(async {
                Ok(ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("queued turn ran"))])
                    .build())
            }))
        }
    }

    fn model_name(&self) -> &str {
        "hanging-first"
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "acp-session-cancel-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// Builds a session whose agent is journal-backed (so prompt turns are real) but isolated
/// from the developer's home directory.
fn build_session(tag: &str, model: Arc<dyn Model>) -> AcpAgentSession {
    let dir = scratch_dir(tag);
    let agent = HarnessAgent::builder()
        .name("acp-session-cancel")
        .model(model)
        .session_id(format!("s-{tag}"))
        .cwd(dir.clone())
        .workspace(dir.join(".agentscope").join("workspace"))
        .disable_memory_hooks()
        .max_iters(3)
        .build();
    AcpAgentSession::new(
        SessionId::new(format!("s-{tag}")),
        dir,
        agent,
        Vec::new(),
        Vec::new(),
    )
}

fn prompt(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(text.to_string()))]
}

/// Waits until `counter` is non-zero. The prompt's bookkeeping before the first model call is
/// fast but not instantaneous, so the tests synchronise on the model call instead of sleeping.
async fn wait_for_first_model_call(counter: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while counter.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the prompt never reached the model"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn closing_a_session_stops_a_running_turn_instead_of_stalling_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = build_session(
        "close",
        Arc::new(HangingModel {
            calls: Arc::clone(&calls),
        }),
    );
    let handle = session.clone();
    let mut running = session;
    let turn = tokio::spawn(async move { running.run_prompt(prompt("go"), |_| {}).await });

    wait_for_first_model_call(&calls).await;
    assert!(handle.is_running(), "the prompt must be running");

    // `session/close` must stop the turn, not wait for a run that never returns.
    handle.close();

    let stop = tokio::time::timeout(Duration::from_secs(3), turn)
        .await
        .expect("a closed session must stop the running turn instead of waiting out the grace")
        .expect("the prompt task must not panic");
    assert_eq!(stop, StopReason::Cancelled);
    assert!(
        !handle.is_running(),
        "the closed session must not report a running prompt"
    );
}

#[tokio::test]
async fn a_cancel_while_a_prompt_is_queued_stops_it_before_it_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = build_session(
        "queued",
        Arc::new(HangingFirstModel {
            calls: Arc::clone(&calls),
        }),
    );

    // Turn 1 holds the session (and its prompt lock) until it is cancelled.
    let mut first = session.clone();
    let first_turn = tokio::spawn(async move { first.run_prompt(prompt("first"), |_| {}).await });
    wait_for_first_model_call(&calls).await;

    // Turn 2 is a follow-up that queues behind turn 1. Poll it once so it is guaranteed to be
    // waiting on the prompt lock before the cancel lands (a fixed sleep would race the
    // admission).
    let mut second = session.clone();
    let mut queued = Box::pin(second.run_prompt(prompt("second"), |_| {}));
    std::future::poll_fn(|cx| {
        assert!(
            queued.as_mut().poll(cx).is_pending(),
            "the second prompt must wait for the first one"
        );
        std::task::Poll::Ready(())
    })
    .await;

    // The client presses Stop: the session-wide cancel must cover the queued prompt too.
    session.cancel();

    let stop = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("a queued prompt must observe the cancel instead of running the turn");
    assert_eq!(stop, StopReason::Cancelled);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the queued prompt must not reach the model after the cancel"
    );

    // Turn 1 is still winding down; closing the session ends it at once (see the test above)
    // so the journal writer is not left behind for the next test.
    session.close();
    let first_stop = tokio::time::timeout(Duration::from_secs(3), first_turn)
        .await
        .expect("closing the session must stop the first turn")
        .expect("the first prompt task must not panic");
    assert_eq!(first_stop, StopReason::Cancelled);
}