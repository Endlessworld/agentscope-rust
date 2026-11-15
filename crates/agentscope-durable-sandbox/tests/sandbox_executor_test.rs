//! What each sandbox answer becomes on the durable side, and what must never happen.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::executor::Executor;
use agentscope_durable::{
    ErrorCode, ExecutionRequest, ExecutionResult, ExecutionState, SessionKey, ThreadKey, ToolCall,
};
use agentscope_durable_sandbox::{ExecSpec, FakeSandbox, Poll, Sandbox, SandboxExecutor};
use serde_json::{json, Value};

fn request(execution_id: &str, arguments: Value) -> ExecutionRequest {
    let session = SessionKey::new("ns", "session").expect("session key");
    let thread = ThreadKey::new(session, "main").expect("thread key");
    let call = ToolCall::new("call-1", "sandbox", "exec", arguments).expect("tool call");
    ExecutionRequest::new(execution_id, thread, "turn-1", call).expect("execution request")
}

fn call(arguments: Value) -> ExecutionRequest {
    request("turn-1:call-1", arguments)
}

fn text(result: &ExecutionResult, key: &str) -> String {
    result.result[key]
        .as_str()
        .unwrap_or_else(|| panic!("`{key}` is not a string in {:?}", result.result))
        .to_string()
}

fn number(result: &ExecutionResult, key: &str) -> u64 {
    result.result[key]
        .as_u64()
        .unwrap_or_else(|| panic!("`{key}` is not a number in {:?}", result.result))
}

#[tokio::test]
async fn a_zero_exit_is_success_with_its_output() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(0, "hello\n", "")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "echo", "args": ["hello"] })))
        .await
        .expect("start");

    assert_eq!(result.state, ExecutionState::Succeeded);
    assert_eq!(result.error, None);
    assert_eq!(text(&result, "outcome"), "SUCCEEDED");
    assert_eq!(number(&result, "exitCode"), 0);
    assert_eq!(text(&result, "stdout"), "hello\n");
    assert_eq!(number(&result, "stdoutBytes"), 6);
    assert_eq!(result.result["truncated"], json!(false));
}

#[tokio::test]
async fn a_non_zero_exit_is_a_failure_that_keeps_its_code_and_output() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(3, "partial", "boom\n")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "make" })))
        .await
        .expect("start");

    assert_eq!(result.state, ExecutionState::Failed);
    assert_eq!(text(&result, "outcome"), "FAILED");
    assert_eq!(number(&result, "exitCode"), 3);
    assert_eq!(text(&result, "stdout"), "partial");
    assert_eq!(text(&result, "stderr"), "boom\n");
    assert_eq!(number(&result, "stderrBytes"), 5);
    assert!(
        result.error.as_deref().unwrap_or_default().contains('3'),
        "the failure names the exit code: {result:?}"
    );
}

#[tokio::test]
async fn a_running_poll_is_pending_and_never_concluded() {
    let sandbox = FakeSandbox::new().script(vec![Poll::running(42), Poll::exited(0, "done", "")]);
    let executor = SandboxExecutor::new(sandbox);
    let request = call(json!({ "command": "sleep", "args": ["1"] }));

    let first = executor.start(&request).await.expect("start");

    assert_eq!(first.state, ExecutionState::Pending);
    assert!(!first.state.terminal(), "pending must not be terminal");
    assert!(
        !first.state.concluded(),
        "a running effect must never be treated as concluded"
    );
    assert_eq!(text(&first, "outcome"), "RUNNING");
    assert_eq!(number(&first, "progress"), 42);
    assert!(
        first.handle.is_some(),
        "the handle is recorded for later polls"
    );

    let second = executor.query(&request).await.expect("query");
    assert_eq!(second.state, ExecutionState::Succeeded);
    assert_eq!(text(&second, "stdout"), "done");
}

#[tokio::test]
async fn a_timeout_is_reported_as_timed_out_and_not_as_a_failure() {
    // A script whose single answer repeats: the process never finishes by itself.
    let sandbox = Arc::new(FakeSandbox::new().script(vec![Poll::running(10)]));
    let executor = SandboxExecutor::with_shared(sandbox.clone());
    let request = call(json!({ "command": "sleep", "args": ["60"], "timeoutMs": 30 }));

    let first = executor.start(&request).await.expect("start");
    assert_eq!(first.state, ExecutionState::Pending);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = executor.query(&request).await.expect("query");

    assert_eq!(text(&result, "outcome"), "TIMED_OUT");
    assert_ne!(
        result.state,
        ExecutionState::Failed,
        "a timeout is not a failure"
    );
    assert_eq!(result.state, ExecutionState::Cancelled);
    assert_eq!(number(&result, "timeoutMs"), 30);
    assert_eq!(number(&result, "exitCode"), 137, "the kill is recorded");
    assert_eq!(sandbox.kills().len(), 1, "a timed out process is killed");
    assert_eq!(executor.live_executions(), 0, "a stopped run is forgotten");
}

#[tokio::test]
async fn a_zero_budget_times_out_on_the_first_observation() {
    let sandbox = Arc::new(FakeSandbox::new().script(vec![Poll::running(0)]));
    let executor = SandboxExecutor::with_shared(sandbox.clone());

    let result = executor
        .start(&call(json!({ "command": "sleep", "timeoutMs": 0 })))
        .await
        .expect("start");

    assert_eq!(text(&result, "outcome"), "TIMED_OUT");
    assert_eq!(result.state, ExecutionState::Cancelled);
    assert_eq!(sandbox.kills().len(), 1);
}

#[tokio::test]
async fn cancelling_kills_the_handle_and_reports_cancelled() {
    let sandbox = Arc::new(FakeSandbox::new().script(vec![Poll::running(5)]));
    let executor = SandboxExecutor::with_shared(sandbox.clone());
    let request = call(json!({ "command": "sleep", "args": ["60"] }));

    let first = executor.start(&request).await.expect("start");
    assert_eq!(first.state, ExecutionState::Pending);

    let cancelled = executor.cancel(&request).await.expect("cancel");
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(text(&cancelled, "outcome"), "CANCELLED");
    assert_eq!(sandbox.kills(), vec![first.handle.clone().expect("handle")]);
    assert_eq!(executor.live_executions(), 0);
}

#[tokio::test]
async fn cancelling_an_execution_that_already_finished_keeps_the_real_outcome() {
    let sandbox =
        Arc::new(FakeSandbox::new().script(vec![Poll::running(1), Poll::exited(7, "", "late")]));
    let executor = SandboxExecutor::with_shared(sandbox.clone());
    let request = call(json!({ "command": "make" }));

    let first = executor.start(&request).await.expect("start");
    assert_eq!(first.state, ExecutionState::Pending);

    let result = executor.cancel(&request).await.expect("cancel");
    assert_eq!(result.state, ExecutionState::Failed);
    assert_eq!(number(&result, "exitCode"), 7);
    assert!(
        sandbox.kills().is_empty(),
        "a finished process is not killed"
    );
}

#[tokio::test]
async fn output_at_the_limit_is_kept_whole() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(0, "abcdefgh", "")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "cat", "maxOutputBytes": 8 })))
        .await
        .expect("start");

    assert_eq!(text(&result, "stdout"), "abcdefgh");
    assert_eq!(number(&result, "stdoutBytes"), 8);
    assert_eq!(number(&result, "stdoutTotalBytes"), 8);
    assert_eq!(result.result["truncated"], json!(false));
}

#[tokio::test]
async fn output_one_byte_over_the_limit_keeps_head_and_tail() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(0, "0123456789", "")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "cat", "maxOutputBytes": 8 })))
        .await
        .expect("start");

    assert_eq!(
        text(&result, "stdout"),
        "01236789",
        "head and tail, nothing in between"
    );
    assert_eq!(number(&result, "stdoutBytes"), 8);
    assert_eq!(number(&result, "stdoutTotalBytes"), 10);
    assert_eq!(result.result["truncated"], json!(true));
    assert_eq!(result.result["stdoutTruncated"], json!(true));
}

#[tokio::test]
async fn the_output_budget_counts_bytes_not_characters() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(0, "中文", "")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "echo", "maxOutputBytes": 4 })))
        .await
        .expect("start");

    assert_eq!(
        number(&result, "stdoutBytes"),
        4,
        "four bytes, not four characters"
    );
    assert_eq!(number(&result, "stdoutTotalBytes"), 6);
    assert_eq!(result.result["truncated"], json!(true));
}

#[tokio::test]
async fn a_successful_call_keeps_every_byte_of_multi_byte_text() {
    let sandbox = FakeSandbox::new().script(vec![Poll::exited(0, "中文", "")]);
    let executor = SandboxExecutor::new(sandbox);

    let result = executor
        .start(&call(json!({ "command": "echo" })))
        .await
        .expect("start");

    assert_eq!(text(&result, "stdout"), "中文");
    assert_eq!(number(&result, "stdoutBytes"), 6);
    assert_eq!(result.result["truncated"], json!(false));
}

#[tokio::test]
async fn the_spec_is_passed_through_unchanged() {
    let sandbox = Arc::new(FakeSandbox::new());
    let executor = SandboxExecutor::with_shared(sandbox.clone());

    executor
        .start(&call(json!({
            "command": "printf",
            "args": ["%s", "hi there", "--flag"],
            "cwd": "/tmp/work dir",
            "env": { "A": "1", "B": "two" },
            "timeoutMs": 1234,
            "maxOutputBytes": 4096
        })))
        .await
        .expect("start");

    let spawned = sandbox.spawned_specs();
    assert_eq!(spawned.len(), 1);
    let expected = ExecSpec {
        command: "printf".to_string(),
        args: vec![
            "%s".to_string(),
            "hi there".to_string(),
            "--flag".to_string(),
        ],
        cwd: Some("/tmp/work dir".to_string()),
        env: BTreeMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "two".to_string()),
        ]),
        timeout_ms: Some(1234),
        max_output_bytes: 4096,
    };
    assert_eq!(spawned[0], expected);
}

#[tokio::test]
async fn a_repeated_start_does_not_launch_a_second_process() {
    let sandbox = Arc::new(FakeSandbox::new().script(vec![Poll::running(1), Poll::running(2)]));
    let executor = SandboxExecutor::with_shared(sandbox.clone());
    let request = call(json!({ "command": "sleep" }));

    let first = executor.start(&request).await.expect("first start");
    let second = executor.start(&request).await.expect("second start");

    assert_eq!(number(&first, "progress"), 1);
    assert_eq!(number(&second, "progress"), 2);
    assert_eq!(
        sandbox.spawned_specs().len(),
        1,
        "one execution, one process"
    );
}

#[tokio::test]
async fn another_process_reports_unknown_instead_of_guessing() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::new().script(vec![Poll::running(1)]));
    let writer = SandboxExecutor::with_shared(sandbox.clone());
    let reader = SandboxExecutor::with_shared(sandbox);
    let request = call(json!({ "command": "sleep" }));

    writer.start(&request).await.expect("start");
    let answer = reader.query(&request).await.expect("query");

    assert_eq!(answer.state, ExecutionState::Unknown);
    assert_ne!(answer.state, ExecutionState::Succeeded);
    assert!(
        answer
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no sandbox handle"),
        "the answer says why it cannot know: {answer:?}"
    );
}

#[tokio::test]
async fn an_invalid_spec_is_rejected_before_anything_runs() {
    let sandbox = Arc::new(FakeSandbox::new());
    let executor = SandboxExecutor::with_shared(sandbox.clone());

    let error = executor
        .start(&call(json!({ "args": ["no command"] })))
        .await
        .expect_err("a spec without a command is invalid");

    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(sandbox.spawned_specs().is_empty());
}

#[tokio::test]
async fn a_sandbox_that_cannot_answer_yet_leaves_the_effect_unknown() {
    struct Silent;

    #[async_trait::async_trait]
    impl Sandbox for Silent {
        async fn spawn(
            &self,
            _spec: ExecSpec,
        ) -> Result<agentscope_durable_sandbox::Handle, agentscope_durable_sandbox::SandboxError>
        {
            Err(agentscope_durable_sandbox::SandboxError::unavailable(
                "control plane did not answer",
            ))
        }

        async fn poll(
            &self,
            _handle: &agentscope_durable_sandbox::Handle,
        ) -> Result<Poll, agentscope_durable_sandbox::SandboxError> {
            unreachable!()
        }

        async fn kill(
            &self,
            _handle: &agentscope_durable_sandbox::Handle,
        ) -> Result<(), agentscope_durable_sandbox::SandboxError> {
            unreachable!()
        }
    }

    let executor = SandboxExecutor::new(Silent);
    let result = executor
        .start(&call(json!({ "command": "make" })))
        .await
        .expect("an unanswered spawn is reported, not raised");

    assert_eq!(result.state, ExecutionState::Unknown);
    assert!(!result.state.terminal(), "unknown is not a verdict");
}
