//! Tests for the memory tools (memory_save / memory_get / memory_search).
use std::sync::Arc;

use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::filesystem::{Filesystem, LocalFilesystem};
use agentscope_harness::{MemoryGetTool, MemorySaveTool, MemorySearchTool, WorkspaceManager};

fn tool_args(pairs: &[(&str, serde_json::Value)]) -> JsonMap {
    let mut m = JsonMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

fn str_arg(v: &str) -> serde_json::Value {
    serde_json::Value::String(v.to_string())
}

async fn run(tool: &dyn Tool, args: JsonMap) -> String {
    let block = tool
        .call(args, ToolExecutionContext::default())
        .await
        .expect("call ok");
    block.text_content()
}

fn make_workspace(tag: &str) -> (std::path::PathBuf, Arc<WorkspaceManager>) {
    let dir = std::env::temp_dir().join(format!("as_mem_test_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
    let wm = Arc::new(WorkspaceManager::with_fs(dir.clone(), Some(fs)));
    (dir, wm)
}

/// `memory_save` appends to MEMORY.md AND today's daily ledger, mirroring Java.
#[tokio::test]
async fn memory_save_writes_memory_md_and_daily_ledger() {
    let (dir, wm) = make_workspace("save");
    let save = MemorySaveTool::new(Arc::clone(&wm));

    let out = run(
        &save,
        tool_args(&[(
            "content",
            str_arg("- User prefers dark mode\n- Project deadline is 2026-07-01"),
        )]),
    )
    .await;
    assert_eq!(out, "Saved 2 memories to MEMORY.md");

    let memory_md = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(memory_md.contains("User prefers dark mode"), "got: {memory_md}");
    assert!(memory_md.contains("Project deadline is 2026-07-01"));

    // Daily ledger: memory/YYYY-MM-DD.md
    let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
    let daily_path = dir.join("memory").join(format!("{today}.md"));
    let daily = std::fs::read_to_string(&daily_path).unwrap();
    assert!(daily.contains("## Memory Save"), "got: {daily}");
    assert!(daily.contains("User prefers dark mode"));

    // Second save appends rather than overwrites.
    let out2 = run(&save, tool_args(&[("content", str_arg("- Likes tea"))])).await;
    assert_eq!(out2, "Saved 1 memory to MEMORY.md");
    let memory_md2 = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(memory_md2.contains("User prefers dark mode"));
    assert!(memory_md2.contains("Likes tea"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `memory_get` returns only the requested 1-based line range, `N|line` formatted.
#[tokio::test]
async fn memory_get_reads_line_range() {
    let (dir, wm) = make_workspace("get");
    let save = MemorySaveTool::new(Arc::clone(&wm));
    let get = MemoryGetTool::new(Arc::clone(&wm));

    run(&save, tool_args(&[("content", str_arg("- alpha\n- beta\n- gamma"))])).await;

    // MEMORY.md content is "\n- alpha\n- beta\n- gamma\n" → lines: ["", "- alpha", "- beta", "- gamma", ""]
    let out = run(
        &get,
        tool_args(&[
            ("path", str_arg("MEMORY.md")),
            ("startLine", serde_json::json!(2)),
            ("endLine", serde_json::json!(4)),
        ]),
    )
    .await;
    assert_eq!(out, "2|- alpha\n3|- beta\n4|- gamma\n");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn memory_get_reports_missing_file_and_traversal() {
    let (dir, wm) = make_workspace("get_err");
    let get = MemoryGetTool::new(Arc::clone(&wm));

    let missing = run(
        &get,
        tool_args(&[
            ("path", str_arg("nope.md")),
            ("startLine", serde_json::json!(1)),
            ("endLine", serde_json::json!(2)),
        ]),
    )
    .await;
    assert!(missing.contains("file not found"), "got: {missing}");

    let traversal = run(
        &get,
        tool_args(&[
            ("path", str_arg("../secret.md")),
            ("startLine", serde_json::json!(1)),
            ("endLine", serde_json::json!(2)),
        ]),
    )
    .await;
    assert!(traversal.contains("path traversal not allowed"), "got: {traversal}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `memory_search` is case-insensitive substring search over MEMORY.md + memory/*.md.
#[tokio::test]
async fn memory_search_finds_across_memory_files() {
    let (dir, wm) = make_workspace("search");
    let save = MemorySaveTool::new(Arc::clone(&wm));
    let search = MemorySearchTool::new(Arc::clone(&wm));

    run(&save, tool_args(&[("content", str_arg("- alpha\n- beta\n- gamma"))])).await;

    let found = run(&search, tool_args(&[("query", str_arg("BETA"))])).await;
    assert!(found.contains("Source: MEMORY.md#3: - beta"), "got: {found}");
    assert!(found.starts_with("Found "), "got: {found}");

    let none = run(&search, tool_args(&[("query", str_arg("zzz-no-such-term"))])).await;
    assert_eq!(none, "No matching memories found for: zzz-no-such-term");

    let empty = run(&search, tool_args(&[("query", str_arg("  "))])).await;
    assert_eq!(empty, "No query provided");

    let _ = std::fs::remove_dir_all(&dir);
}
