//! Integration tests for harness tools dispatched through the core `Toolkit`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::tool::Toolkit;
use agentscope_harness::{
    Filesystem, FilesystemTool, LocalFilesystem, SandboxBackedFilesystem, SandboxFilesystem,
    ShellExecuteTool,
};

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
    let write_call = ToolUseBlock::new("1", "write_file", serde_json::json!({ "path": "/a.txt", "content": "hello world" }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&write_call, Default::default()).await.unwrap();
    assert!(result_text(&r).contains("Written to"));

    let read_call = ToolUseBlock::new("2", "read_file", serde_json::json!({ "path": "/a.txt" }).as_object().cloned().unwrap());
    let r = toolkit.call_tool(&read_call, Default::default()).await.unwrap();
    assert_eq!(result_text(&r), "hello world");
}

#[tokio::test]
async fn edit_file_via_toolkit() {
    let dir = temp_dir("edit_tool").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    fs.write(&ctx(), "/a.txt", "one two two").await.unwrap();
    let toolkit = Toolkit::new();
    FilesystemTool::register(&toolkit, fs.clone());

    let call = ToolUseBlock::new("1", "edit_file", serde_json::json!({ "path": "/a.txt", "old_string": "two", "new_string": "three", "replace_all": true }).as_object().cloned().unwrap());
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

#[tokio::test]
async fn shell_execute_rejects_dangerous_command() {
    let dir = temp_dir("shell_dangerous").await;
    let inner = Arc::new(LocalFilesystem::sandboxed(&dir));
    let sandbox: Arc<dyn SandboxFilesystem> = Arc::new(SandboxBackedFilesystem::new(inner, dir.clone()));
    let toolkit = Toolkit::new();
    ShellExecuteTool::register(&toolkit, sandbox);

    for cmd in ["rm -rf /tmp/x", "sudo rm -rf /", "dd if=/dev/zero of=/dev/sda", "chmod 777 /etc/passwd"] {
        let call = ToolUseBlock::new("1", "execute", serde_json::json!({ "command": cmd }).as_object().cloned().unwrap());
        let r = toolkit.call_tool(&call, Default::default()).await.unwrap();
        let text = result_text(&r);
        assert!(text.contains("dangerous"), "expected rejection for: {cmd} (got: {text})");
        assert!(!text.contains("Exit code:"), "dangerous command should not execute: {cmd}");
    }
}
