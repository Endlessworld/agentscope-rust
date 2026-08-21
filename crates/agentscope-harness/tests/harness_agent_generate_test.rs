//! Tests for `AgentGenerateTool`.
use std::sync::Arc;

use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::filesystem::LocalFilesystem;
use agentscope_harness::{AgentGenerateTool, load_from_directory};

fn tool_args(pairs: &[(&str, &str)]) -> JsonMap {
    let mut m = JsonMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
    }
    m
}

#[tokio::test]
async fn generates_spec_that_loader_parses() {
    let dir = std::env::temp_dir().join(format!("as_gen_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));

    let tool = AgentGenerateTool::new(Arc::clone(&fs));
    let block = tool
        .call(
            tool_args(&[
                ("name", "Code Reviewer!"),
                ("description", "Reviews code for bugs and style"),
                ("tools", "read_file, grep_files"),
            ]),
            ToolExecutionContext::default(),
        )
        .await
        .unwrap();
    let out = block.text_content();
    assert!(out.contains("code-reviewer"), "got: {out}");

    // The generated file must be parseable by the standard subagent loader.
    let decls = load_from_directory(dir.join("subagents").to_str().unwrap(), None);
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0].name, "code-reviewer");
    assert!(decls[0].description.contains("Reviews code"));
    assert_eq!(decls[0].tools.len(), 2);

    // Duplicate name must be rejected.
    let dup = tool
        .call(
            tool_args(&[("name", "code-reviewer"), ("description", "again")]),
            ToolExecutionContext::default(),
        )
        .await
        .unwrap();
    assert!(dup.text_content().contains("already exists"));

    let _ = std::fs::remove_dir_all(&dir);
}
