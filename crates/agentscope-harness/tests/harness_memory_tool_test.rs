//! Tests for the memory tools (memory_save / memory_get / memory_search).
use std::sync::Arc;

use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::filesystem::LocalFilesystem;
use agentscope_harness::{MemoryGetTool, MemorySaveTool, MemorySearchTool};

fn tool_args(pairs: &[(&str, &str)]) -> JsonMap {
    let mut m = JsonMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
    }
    m
}

async fn run(tool: &dyn Tool, args: JsonMap) -> String {
    let block = tool
        .call(args, ToolExecutionContext::default())
        .await
        .expect("call ok");
    block.text_content()
}

#[tokio::test]
async fn memory_save_get_search_roundtrip() {
    let dir = std::env::temp_dir().join(format!("as_mem_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));

    let save = MemorySaveTool::new(Arc::clone(&fs));
    let get = MemoryGetTool::new(Arc::clone(&fs));
    let search = MemorySearchTool::new(Arc::clone(&fs));

    let out = run(
        &save,
        tool_args(&[
            ("path", "facts.md"),
            ("content", "Rust is memory-safe.\nAgentScope ports Java."),
        ]),
    )
    .await;
    assert!(out.contains("Saved memory"), "got: {out}");

    let content = run(&get, tool_args(&[("path", "facts.md")])).await;
    assert!(content.contains("memory-safe"), "got: {content}");

    let found = run(&search, tool_args(&[("query", "memory-safe")])).await;
    assert!(found.contains("facts.md"), "got: {found}");

    let none = run(&search, tool_args(&[("query", "nonexistent-term-xyz")])).await;
    assert!(none.contains("No matching"), "got: {none}");

    let _ = std::fs::remove_dir_all(&dir);
}
