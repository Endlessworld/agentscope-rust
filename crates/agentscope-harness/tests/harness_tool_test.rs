//! Integration tests for harness tools dispatched through the core `Toolkit`.
use std::path::Path;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{RuntimeContext, StreamOptions};
use agentscope_core::message::{
    ContentBlock, Msg, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::permission::{PermissionBehavior, PermissionContextState, PermissionRule};
use agentscope_core::tool::Toolkit;
use agentscope_core::{ConfirmHandler, ConfirmResult};
use agentscope_harness::agent::HarnessAgent;
use agentscope_harness::{
    Filesystem, FilesystemTool, LocalFilesystem, SandboxBackedFilesystem, SandboxFilesystem,
    ShellExecuteTool,
};
use futures::StreamExt;

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

fn result_text(r: &ToolResultBlock) -> String {
    r.output
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_tool_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}
#[tokio::test]
async fn filesystem_tools_registered_and_dispatch() {
    let dir = temp_dir("fs_tools").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let toolkit = Toolkit::new();
    FilesystemTool::register(&toolkit, fs.clone());

    for name in ["read_file", "write_file", "edit_file", "grep_files", "glob_files", "list_files"] {
        assert!(toolkit.tool_names().contains(&name.to_string()), "missing {name}");
    }

    // write_file then read_file roundtrip
    let write_call = ToolUseBlock::new("1", "write_file", serde_json::json!({ "operations": [{ "path": "/a.txt", "content": "hello world" }] }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&write_call, Default::default()).await.unwrap();
    assert!(result_text(&r).contains("### /a.txt"));
    assert!(result_text(&r).contains("Written"));

    let read_call = ToolUseBlock::new("2", "read_file", serde_json::json!({ "filePath": ["/a.txt"] }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&read_call, Default::default()).await.unwrap();
    assert!(result_text(&r).contains("hello world"));
}

#[tokio::test]
async fn edit_file_via_toolkit() {
    let dir = temp_dir("edit_tool").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    fs.write(&ctx(), "/a.txt", "one two two").await.unwrap();
    let toolkit = Toolkit::new();
    FilesystemTool::register(&toolkit, fs.clone());

    let call = ToolUseBlock::new("1", "edit_file", serde_json::json!({ "operations": [{ "path": "/a.txt", "old_string": "two", "new_string": "three", "replace_all": true }] }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&call, Default::default()).await.unwrap();
    assert!(result_text(&r).contains("2 replacement"));
    let data = fs.read(&ctx(), "/a.txt", 0, 0).await.unwrap();
    assert_eq!(data.content, "one three three");
}

#[tokio::test]
async fn shell_execute_tool() {
    let dir = temp_dir("shell_tool").await;
    let inner = Arc::new(LocalFilesystem::sandboxed(&dir));
    let sandbox: Arc<dyn SandboxFilesystem> = Arc::new(SandboxBackedFilesystem::new(inner, dir.clone()));
    let toolkit = Toolkit::new();
    ShellExecuteTool::register(&toolkit, sandbox);
    assert!(toolkit.tool_names().contains(&"execute".to_string()));

    let call = ToolUseBlock::new("1", "execute", serde_json::json!({ "command": "echo tool-hello" }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&call, Default::default()).await.unwrap();
    let text = result_text(&r);
    assert!(text.contains("tool-hello"));
    assert!(text.contains("Exit code: 0"));
}

/// A scripted model: the first call asks for one `execute` tool call, the second
/// (after the tool result) ends the turn.
struct ExecuteThenAnswer {
    command: String,
    calls: Mutex<u32>,
}

#[async_trait::async_trait]
impl Model for ExecuteThenAnswer {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let first = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls == 1
        };
        let content = if first {
            let mut input = serde_json::Map::new();
            input.insert(
                "command".to_string(),
                serde_json::Value::String(self.command.clone()),
            );
            vec![ContentBlock::ToolUse(ToolUseBlock::new("call-1", "execute", input))]
        } else {
            vec![ContentBlock::Text(TextBlock::new("done"))]
        };
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder().content(content).build())
        }))
    }

    fn model_name(&self) -> &str {
        "execute-then-answer"
    }
}

/// Runs one turn in which the model asks to delete `victim/`, and returns the
/// commands the confirmation handler was shown plus whether the directory survived.
async fn run_execute_turn(dir: &Path, approve: bool) -> (Vec<String>, bool) {
    let victim = dir.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("data.txt"), "irreplaceable").unwrap();

    // The tool always runs through a sandbox rooted at the agent cwd, so a
    // relative path is what reaches the real working directory (`rm -rf` is not
    // valid PowerShell parameter syntax, hence the two spellings).
    let command = if cfg!(windows) {
        "Remove-Item -Recurse -Force victim".to_string()
    } else {
        "rm -rf victim".to_string()
    };

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let handler: ConfirmHandler = Arc::new(move |call: ToolUseBlock| {
        let recorded = Arc::clone(&recorded);
        let command = call
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Box::pin(async move {
            recorded.lock().unwrap().push(command);
            if approve {
                ConfirmResult::allowed()
            } else {
                ConfirmResult::denied("user rejected the command")
            }
        })
    });

    // Force `Ask` for the shell tool: the destructive command is gated by the
    // approval path, not by a substring block-list.
    let permission = PermissionContextState::builder()
        .add_ask_rule(
            "execute",
            PermissionRule::new("execute", None, PermissionBehavior::Ask, "test"),
        )
        .build();

    let agent = HarnessAgent::builder()
        .name("shell-approval")
        .model(Arc::new(ExecuteThenAnswer {
            command: command.clone(),
            calls: Mutex::new(0),
        }))
        .cwd(dir)
        .register_shell_tool(true)
        .session_id("s-approval")
        .permission_context(permission)
        .confirm_handler(handler)
        .build();

    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("clean up the workspace")],
            StreamOptions::defaults(),
        )
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }

    let commands = seen.lock().unwrap().clone();
    (commands, victim.exists())
}

/// A destructive `execute` is gated by the permission/approval path: the
/// confirmation handler decides whether the command runs. (`DANGEROUS_COMMANDS`
/// is intentionally empty, so no substring block-list may be relied upon.)
#[tokio::test]
async fn shell_execute_is_gated_by_the_approval_path() {
    let dir = temp_dir("shell_approval").await;

    // Rejected: the handler is asked about this exact command and nothing runs.
    let (commands, survived) = run_execute_turn(&dir, false).await;
    assert_eq!(
        commands.len(),
        1,
        "the execute call must be offered for approval, got {commands:?}"
    );
    assert!(
        commands[0].contains("victim"),
        "the handler must see the command, got {commands:?}"
    );
    assert!(survived, "a rejected command must not be executed");

    // Approved: the identical command runs, which proves the branch above is
    // not passing vacuously.
    let (commands, survived) = run_execute_turn(&dir, true).await;
    assert_eq!(commands.len(), 1, "got {commands:?}");
    assert!(!survived, "an approved command must be executed");
}
