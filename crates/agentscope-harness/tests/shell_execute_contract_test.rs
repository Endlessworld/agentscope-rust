//! External-behaviour contract for the `execute` tool on Windows.
//!
//! This suite is the gate for the planned warm-process pool: the process lifecycle may change,
//! but everything below — cwd, clean-process semantics, stdout/stderr, Unicode, exit codes,
//! timeout, cancellation, output caps and process events — must stay identical.
//!
//! It deliberately talks only to the public `SandboxFilesystem` / `Tool` APIs, so the same
//! tests run against both the current spawn-per-call implementation and a pooled one.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::ToolUseBlock;
use agentscope_core::tool::{
    ProcessEvents, ProcessRegistry, Tool, ToolExecutionContext, TOOL_CALL_ID_CONTEXT_KEY,
};
use agentscope_harness::filesystem::{
    LocalFilesystem, PwshProcessPool, SandboxBackedFilesystem, SandboxFilesystem,
};
use agentscope_harness::tool::ShellExecuteTool;

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_shell_contract_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn sandbox_at(cwd: &Path) -> Arc<SandboxBackedFilesystem> {
    let inner = LocalFilesystem::sandboxed(cwd);
    Arc::new(SandboxBackedFilesystem::new(Arc::new(inner), cwd))
}

fn sandbox_with_pool(
    cwd: &Path,
    size: usize,
) -> (Arc<SandboxBackedFilesystem>, Arc<PwshProcessPool>) {
    let pool = Arc::new(PwshProcessPool::new(size));
    let inner = LocalFilesystem::sandboxed(cwd);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), cwd).with_pwsh_pool(Arc::clone(&pool)),
    );
    (sandbox, pool)
}

async fn wait_until_warm(pool: &PwshProcessPool, minimum: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while pool.ready_count() < minimum {
        assert!(
            Instant::now() < deadline,
            "pool never warmed {minimum} process(es): ready={} warming={}",
            pool.ready_count(),
            pool.warming_count()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn ctx() -> RuntimeContext {
    RuntimeContext::default().with_session_id("shell-contract")
}

fn tool_args(value: serde_json::Value) -> agentscope_core::message::JsonMap {
    value
        .as_object()
        .cloned()
        .expect("tool arguments must be an object")
}

fn lower(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

#[tokio::test]
async fn every_call_gets_a_clean_process_working_directory_and_environment() {
    // A pool may keep processes alive between calls, but not their state: the first command
    // mutates env/cwd, the next call must still start in the configured cwd with a clean env.
    let root = temp_root("clean_process");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = sandbox_at(&work);

    let first = sandbox
        .execute(
            &ctx(),
            "$env:AGENTSCOPE_POOL_LEAK = 'yes'; Set-Location ..; Write-Output 'first-ok'",
            15,
        )
        .await
        .expect("first command runs");
    assert!(first.is_success(), "{first:?}");
    assert!(first.output.contains("first-ok"), "{first:?}");

    let second_command = format!(
        "if ($env:AGENTSCOPE_POOL_LEAK) {{ 'env=leaked' }} else {{ 'env=clean' }}; \
         if ((Get-Location).Path -eq '{cwd}') {{ 'cwd=ok' }} else {{ 'cwd=' + (Get-Location).Path }}",
        cwd = work.to_string_lossy()
    );
    let second = sandbox
        .execute(&ctx(), &second_command, 15)
        .await
        .expect("second command runs");

    assert!(second.is_success(), "{second:?}");
    assert!(
        second.output.contains("env=clean"),
        "environment leaked across calls: {second:?}"
    );
    assert!(
        second.output.contains("cwd=ok"),
        "working directory leaked across calls: {second:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stdout_stderr_unicode_and_literal_dollar_are_preserved() {
    let root = temp_root("encodings");
    let sandbox = sandbox_at(&root);
    let result = sandbox
        .execute(
            &ctx(),
            "Write-Output '中文-✓'; [Console]::Error.WriteLine('stderr-line'); \
             Write-Output 'price is $5'",
            15,
        )
        .await
        .expect("command runs");

    assert!(result.is_success(), "{result:?}");
    assert!(result.output.contains("中文-✓"), "{result:?}");
    assert!(result.output.contains("stderr-line"), "{result:?}");
    assert!(
        result.output.contains("price is $5"),
        "single-quoted dollar text was expanded: {result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn exit_codes_are_propagated_and_an_abnormal_exit_does_not_poison_the_next_call() {
    let root = temp_root("exit_code");
    let sandbox = sandbox_at(&root);

    let failed = sandbox
        .execute(&ctx(), "Write-Output 'before-exit'; exit 7", 15)
        .await
        .expect("command runs");
    assert_eq!(failed.exit_code, 7, "{failed:?}");
    assert!(!failed.is_success());
    assert!(failed.output.contains("before-exit"), "{failed:?}");

    let next = sandbox
        .execute(&ctx(), "Write-Output 'after-exit'", 15)
        .await
        .expect("next command runs");
    assert!(next.is_success(), "{next:?}");
    assert!(next.output.contains("after-exit"), "{next:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn native_failures_keep_the_same_exit_code_on_the_cold_and_warm_paths() {
    let root = temp_root("exit_parity");
    let cold = sandbox_at(&root);
    let (warm, pool) = sandbox_with_pool(&root, 1);
    wait_until_warm(&pool, 1).await;

    for command in [
        "Write-Output 'ok'",
        "cmd /c exit 7",
        "Write-Error 'boom'",
        "throw 'boom'",
        "exit 7",
    ] {
        let cold_result = cold
            .execute(&ctx(), command, 30)
            .await
            .expect("cold command runs");
        let warm_result = warm
            .execute(&ctx(), command, 30)
            .await
            .expect("warm command runs");
        assert_eq!(
            cold_result.exit_code, warm_result.exit_code,
            "exit-code drift for {command:?}: cold={cold_result:?} warm={warm_result:?}"
        );
        assert_eq!(
            cold_result.is_success(),
            warm_result.is_success(),
            "success drift for {command:?}: cold={cold_result:?} warm={warm_result:?}"
        );
    }
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn large_output_is_drained_capped_and_marked() {
    // The old pipe-deadlock test: >100KB must not hang, and the caller has to be told the
    // result is incomplete.
    let root = temp_root("large_output");
    let sandbox = sandbox_at(&root);
    let result = sandbox
        .execute(
            &ctx(),
            "1..15000 | ForEach-Object { '0123456789abcdefghij' }",
            30,
        )
        .await
        .expect("large command runs");

    assert!(result.is_success(), "{result:?}");
    assert!(
        result.truncated,
        "the output cap was not reported: {result:?}"
    );
    assert!(
        result.output.contains("... Output truncated at"),
        "the truncation marker is missing: {}",
        result.output.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn timeout_kills_the_process_and_reports_the_timeout_flag() {
    let root = temp_root("timeout");
    let sandbox = sandbox_at(&root);
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        sandbox.execute(&ctx(), "Start-Sleep -Seconds 30", 1),
    )
    .await
    .expect("a timed-out command must not hang the caller")
    .expect("timeout is a normal ExecuteResponse");

    assert!(result.timed_out(), "{result:?}");
    assert_eq!(result.exit_code, 124, "{result:?}");
    assert!(!result.cancelled(), "{result:?}");
    assert!(result.output.contains("timed out"), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "timeout returned too late: {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Records the ACP v2 terminal event sequence.
#[derive(Default)]
struct EventRecorder {
    started: Mutex<Vec<String>>,
    output: Mutex<Vec<u8>>,
    exited: Mutex<Vec<i32>>,
}

impl ProcessEvents for EventRecorder {
    fn started(&self, key: &str, command: &str, _cwd: &str) {
        self.started
            .lock()
            .unwrap()
            .push(format!("{key}:{command}"));
    }

    fn output(&self, _key: &str, bytes: &[u8]) {
        self.output.lock().unwrap().extend_from_slice(bytes);
    }

    fn exited(&self, _key: &str, exit_code: i32) {
        self.exited.lock().unwrap().push(exit_code);
    }
}

#[tokio::test]
async fn process_events_are_still_emitted_for_a_completed_command() {
    let root = temp_root("events");
    let registry = ProcessRegistry::new();
    let recorder = Arc::new(EventRecorder::default());
    registry.set_events(Some(recorder.clone()));
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root).with_processes(Arc::clone(&registry)),
    );
    let mut ctx = ctx();
    ctx.set_extension(TOOL_CALL_ID_CONTEXT_KEY, "call-events".to_string());

    let result = sandbox
        .execute(&ctx, "Write-Output 'streamed-event-line'", 15)
        .await
        .expect("command runs");

    assert!(result.is_success(), "{result:?}");
    assert_eq!(recorder.started.lock().unwrap().len(), 1);
    assert!(recorder.started.lock().unwrap()[0].contains("call-events"));
    assert!(
        String::from_utf8_lossy(&recorder.output.lock().unwrap()).contains("streamed-event-line"),
        "the output stream was not forwarded"
    );
    assert_eq!(*recorder.exited.lock().unwrap(), vec![0]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn timeout_emits_the_expected_terminal_events() {
    let root = temp_root("timeout_events");
    let registry = ProcessRegistry::new();
    let recorder = Arc::new(EventRecorder::default());
    registry.set_events(Some(recorder.clone()));
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root).with_processes(Arc::clone(&registry)),
    );
    let mut ctx = ctx();
    ctx.set_extension(TOOL_CALL_ID_CONTEXT_KEY, "call-timeout".to_string());

    let result = sandbox
        .execute(&ctx, "Start-Sleep -Seconds 30", 1)
        .await
        .expect("timeout returns");
    assert!(result.timed_out(), "{result:?}");
    assert_eq!(*recorder.exited.lock().unwrap(), vec![124]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_tool_reports_a_client_cancel_in_its_result_metadata() {
    let root = temp_root("tool_cancel");
    let registry = ProcessRegistry::new();
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox: Arc<dyn SandboxFilesystem> = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root).with_processes(Arc::clone(&registry)),
    );
    let tool = Arc::new(ShellExecuteTool::new(sandbox));
    let call = ToolUseBlock::new(
        "call-cancel-tool",
        "execute",
        tool_args(serde_json::json!({"command": "Start-Sleep -Seconds 30", "timeout": 60})),
    );
    let ctx = ToolExecutionContext {
        tool_call: Some(call),
        ..ToolExecutionContext::default()
    };

    let result = async {
        let execute = tool.call(
            tool_args(serde_json::json!({"command": "Start-Sleep -Seconds 30", "timeout": 60})),
            ctx,
        );
        let cancel = async {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !registry.is_running("call-cancel-tool") {
                assert!(
                    Instant::now() < deadline,
                    "the command never registered its process"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(registry.cancel("call-cancel-tool"));
        };
        let (result, ()) = tokio::join!(execute, cancel);
        result
    }
    .await
    .expect("the tool returns a cancellation result");

    assert!(
        result.text_content().contains("cancelled by the client"),
        "{}",
        result.text_content()
    );
    assert_eq!(
        result.metadata.get("cancelled"),
        Some(&serde_json::Value::Bool(true))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_client_cancel_still_kills_the_running_command() {
    let root = temp_root("cancel");
    let registry = ProcessRegistry::new();
    let recorder = Arc::new(EventRecorder::default());
    registry.set_events(Some(recorder.clone()));
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root).with_processes(Arc::clone(&registry)),
    );
    let mut ctx = ctx();
    ctx.set_extension(TOOL_CALL_ID_CONTEXT_KEY, "call-cancel".to_string());

    let result = async {
        let execute = sandbox.execute(&ctx, "Start-Sleep -Seconds 30", 60);
        let cancel = async {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !registry.is_running("call-cancel") {
                assert!(
                    Instant::now() < deadline,
                    "the command never registered its process"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                registry.cancel("call-cancel"),
                "cancel must reach the command"
            );
        };
        let (result, ()) = tokio::join!(execute, cancel);
        result
    }
    .await
    .expect("cancelled command returns a normal ExecuteResponse");

    assert!(result.cancelled(), "{result:?}");
    assert!(!result.timed_out(), "{result:?}");
    assert!(!registry.is_running("call-cancel"));
    assert_eq!(recorder.started.lock().unwrap().len(), 1);
    assert_eq!(recorder.exited.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn concurrent_calls_keep_their_output_separate() {
    let root = temp_root("concurrent");
    let sandbox = sandbox_at(&root);
    let ctx = ctx();
    let one = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 150; Write-Output 'result-one'",
        30,
    );
    let two = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 100; Write-Output 'result-two'",
        30,
    );
    let three = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 50; Write-Output 'result-three'",
        30,
    );
    let (one, two, three) = tokio::join!(one, two, three);
    let one = one.expect("one");
    let two = two.expect("two");
    let three = three.expect("three");

    for (result, own, others) in [
        (&one, "result-one", ["result-two", "result-three"]),
        (&two, "result-two", ["result-one", "result-three"]),
        (&three, "result-three", ["result-one", "result-two"]),
    ] {
        assert!(result.is_success(), "{result:?}");
        assert!(result.output.contains(own), "{result:?}");
        for other in others {
            assert!(
                !result.output.contains(other),
                "{own} received {other}'s output: {result:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn tool_level_validation_and_relative_working_directory_are_unchanged() {
    let root = temp_root("tool_contract");
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let sandbox: Arc<dyn SandboxFilesystem> = sandbox_at(&root);
    let tool = ShellExecuteTool::new(sandbox);

    let dangerous = tool
        .call(
            tool_args(serde_json::json!({"command": "rm -rf /"})),
            ToolExecutionContext::default(),
        )
        .await
        .expect("dangerous command is rejected as a tool result");
    assert!(
        dangerous.text_content().contains("dangerous fragment"),
        "{}",
        dangerous.text_content()
    );

    let bad_timeout = tool
        .call(
            tool_args(serde_json::json!({"command": "Write-Output nope", "timeout": 1801})),
            ToolExecutionContext::default(),
        )
        .await
        .expect("invalid timeout is rejected");
    assert!(
        bad_timeout.text_content().contains("timeout must be"),
        "{}",
        bad_timeout.text_content()
    );

    let bad_cwd = tool
        .call(
            tool_args(serde_json::json!({
                "command": "Write-Output nope",
                "working_directory": ".."
            })),
            ToolExecutionContext::default(),
        )
        .await
        .expect("invalid working directory is rejected");
    assert!(
        bad_cwd.text_content().contains("relative path"),
        "{}",
        bad_cwd.text_content()
    );

    let relative = tool
        .call(
            tool_args(serde_json::json!({
                "command": "Write-Output ((Get-Location).Path)",
                "working_directory": "sub"
            })),
            ToolExecutionContext::default(),
        )
        .await
        .expect("relative working directory runs");
    let text = relative.text_content();
    assert!(text.contains("Exit code: 0"), "{text}");
    let expected_dir = lower(&sub);
    assert!(
        text.to_lowercase().contains(&expected_dir),
        "command did not run in the requested directory: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_warm_process_runs_one_command_then_a_replacement_is_warmed() {
    let root = temp_root("pool_refill");
    let (sandbox, pool) = sandbox_with_pool(&root, 2);
    wait_until_warm(&pool, 1).await;

    let first = sandbox
        .execute(&ctx(), "Write-Output ('PID=' + $PID)", 30)
        .await
        .expect("warm command runs");
    assert!(first.is_success(), "{first:?}");
    let first_pid = first
        .output
        .lines()
        .find_map(|line| line.trim().strip_prefix("PID="))
        .expect("PID line")
        .to_string();

    // Checkout starts the replacement before the command runs; it must be ready for the next
    // call without a cold-spawn delay.
    wait_until_warm(&pool, 1).await;
    let second = sandbox
        .execute(&ctx(), "Write-Output ('PID=' + $PID)", 30)
        .await
        .expect("replacement command runs");
    assert!(second.is_success(), "{second:?}");
    let second_pid = second
        .output
        .lines()
        .find_map(|line| line.trim().strip_prefix("PID="))
        .expect("PID line")
        .to_string();

    assert_ne!(
        first_pid, second_pid,
        "a one-shot pool must not reuse the shell process"
    );
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn warm_commands_keep_the_same_clean_process_semantics() {
    let root = temp_root("pool_clean");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (sandbox, pool) = sandbox_with_pool(&work, 2);
    wait_until_warm(&pool, 1).await;

    let first = sandbox
        .execute(
            &ctx(),
            "$env:AGENTSCOPE_POOL_LEAK = 'yes'; Set-Location ..; Write-Output 'first-ok'",
            30,
        )
        .await
        .expect("first warm command");
    assert!(first.is_success(), "{first:?}");

    wait_until_warm(&pool, 1).await;
    let second_command = format!(
        "if ($env:AGENTSCOPE_POOL_LEAK) {{ 'env=leaked' }} else {{ 'env=clean' }}; \
         if ((Get-Location).Path -eq '{cwd}') {{ 'cwd=ok' }} else {{ 'cwd=' + (Get-Location).Path }}",
        cwd = work.to_string_lossy()
    );
    let second = sandbox
        .execute(&ctx(), &second_command, 30)
        .await
        .expect("second warm command");
    assert!(second.is_success(), "{second:?}");
    assert!(second.output.contains("env=clean"), "{second:?}");
    assert!(second.output.contains("cwd=ok"), "{second:?}");
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn warm_commands_still_emit_the_terminal_event_sequence() {
    let root = temp_root("pool_events");
    let registry = ProcessRegistry::new();
    let recorder = Arc::new(EventRecorder::default());
    registry.set_events(Some(recorder.clone()));
    let pool = Arc::new(PwshProcessPool::new(1));
    wait_until_warm(&pool, 1).await;
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root)
            .with_processes(Arc::clone(&registry))
            .with_pwsh_pool(Arc::clone(&pool)),
    );
    let mut ctx = ctx();
    ctx.set_extension(TOOL_CALL_ID_CONTEXT_KEY, "call-pool-events".to_string());

    let result = sandbox
        .execute(&ctx, "Write-Output 'warm-event-line'", 30)
        .await
        .expect("warm command runs");
    assert!(result.is_success(), "{result:?}");
    assert_eq!(recorder.started.lock().unwrap().len(), 1);
    assert!(
        String::from_utf8_lossy(&recorder.output.lock().unwrap()).contains("warm-event-line"),
        "warm output was not forwarded"
    );
    assert_eq!(*recorder.exited.lock().unwrap(), vec![0]);
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_warm_command_times_out_like_the_cold_path() {
    let root = temp_root("pool_timeout");
    let (sandbox, pool) = sandbox_with_pool(&root, 1);
    wait_until_warm(&pool, 1).await;
    let result = sandbox
        .execute(&ctx(), "Start-Sleep -Seconds 30", 1)
        .await
        .expect("warm timeout returns");
    assert!(result.timed_out(), "{result:?}");
    assert_eq!(result.exit_code, 124, "{result:?}");
    assert!(result.output.contains("timed out"), "{result:?}");
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_warm_command_can_be_cancelled_by_the_client() {
    let root = temp_root("pool_cancel");
    let registry = ProcessRegistry::new();
    let pool = Arc::new(PwshProcessPool::new(1));
    wait_until_warm(&pool, 1).await;
    let inner = LocalFilesystem::sandboxed(&root);
    let sandbox = Arc::new(
        SandboxBackedFilesystem::new(Arc::new(inner), &root)
            .with_processes(Arc::clone(&registry))
            .with_pwsh_pool(Arc::clone(&pool)),
    );
    let mut ctx = ctx();
    ctx.set_extension(TOOL_CALL_ID_CONTEXT_KEY, "call-pool-cancel".to_string());

    let result = async {
        let execute = sandbox.execute(&ctx, "Start-Sleep -Seconds 30", 60);
        let cancel = async {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !registry.is_running("call-pool-cancel") {
                assert!(Instant::now() < deadline, "warm command did not register");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(registry.cancel("call-pool-cancel"));
        };
        let (result, ()) = tokio::join!(execute, cancel);
        result
    }
    .await
    .expect("cancelled warm command returns");
    assert!(result.cancelled(), "{result:?}");
    assert!(!result.timed_out(), "{result:?}");
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_empty_pool_falls_back_to_cold_spawn_for_parallel_calls() {
    let root = temp_root("pool_fallback");
    let (sandbox, pool) = sandbox_with_pool(&root, 1);
    wait_until_warm(&pool, 1).await;

    let ctx = ctx();
    let one = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 150; Write-Output 'pool-one'",
        30,
    );
    let two = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 150; Write-Output 'pool-two'",
        30,
    );
    let three = sandbox.execute(
        &ctx,
        "Start-Sleep -Milliseconds 150; Write-Output 'pool-three'",
        30,
    );
    let (one, two, three) = tokio::join!(one, two, three);
    for (result, marker) in [
        (one.expect("one"), "pool-one"),
        (two.expect("two"), "pool-two"),
        (three.expect("three"), "pool-three"),
    ] {
        assert!(result.is_success(), "{result:?}");
        assert!(result.output.contains(marker), "{result:?}");
    }
    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}
