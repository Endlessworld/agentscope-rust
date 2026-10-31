//! Build-time host hook (HARNESS-06) and workspace tool governance (HARNESS-01).
//!
//! Both go through the real `HarnessAgent::builder()` assembly: the callback sees the
//! finished agent, so the assertions cover the wiring itself instead of a hand-built
//! struct. That is exactly the failure mode these tests guard against — the hook and the
//! `tools.json` filter existing as code but never running during `build()`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock, ToolResultBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_harness::agent::{HarnessAgent, ToolsConfig};

/// A model that always answers with plain text; the tests here never need tool loops.
struct TextModel;

#[async_trait::async_trait]
impl Model for TextModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }
    fn model_name(&self) -> &str {
        "text"
    }
}

/// A catalogue tool registered by the host, i.e. a stand-in for the default product
/// toolset an `allow` list is supposed to narrow.
#[derive(Debug)]
struct CountingTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "counts calls"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn call(
        &self,
        _args: agentscope_core::message::JsonMap,
        _ctx: ToolExecutionContext,
    ) -> agentscope_core::error::Result<ToolResultBlock> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResultBlock::builder()
            .id("call")
            .name(self.name)
            .output(CoreBlock::Text(TextBlock::new("ok")))
            .build())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookSnapshot {
    name: String,
    agent_id: String,
    has_model: bool,
    has_state: bool,
    tools: Vec<String>,
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_harness_built_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn builder_hands_the_finished_agent_to_the_host_callback() {
    let root = temp_root("hook");
    let calls = Arc::new(AtomicUsize::new(0));

    let base = Toolkit::default();
    base.register_tool(Arc::new(CountingTool {
        name: "echo_tool",
        calls: Arc::clone(&calls),
    }));
    base.register_tool(Arc::new(CountingTool {
        name: "secret_tool",
        calls: Arc::clone(&calls),
    }));

    let seen: Arc<Mutex<Vec<HookSnapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_hook = Arc::clone(&seen);

    let agent = HarnessAgent::builder()
        .name("built-agent")
        .agent_id("built-agent-1")
        .model(Arc::new(TextModel))
        .cwd(root.clone())
        .workspace(root.join("workspace"))
        .toolkit(base)
        .tools_config(ToolsConfig {
            allow: vec!["echo_tool".to_string()],
            deny: Vec::new(),
            mcp_servers: Default::default(),
        })
        .disable_web_tools()
        .on_agent_built(move |handle| {
            let mut tools = handle.toolkit().tool_names();
            tools.sort();
            seen_hook.lock().unwrap().push(HookSnapshot {
                name: handle.name().to_string(),
                agent_id: handle.agent_id().to_string(),
                has_model: handle.model().is_some(),
                has_state: handle.state().is_some(),
                tools,
            });
        })
        .build();

    let snapshot = {
        let snapshots = seen.lock().unwrap();
        assert_eq!(
            snapshots.len(),
            1,
            "the build callback must fire exactly once per build"
        );
        snapshots[0].clone()
    };
    assert_eq!(snapshot.name, "built-agent");
    assert_eq!(snapshot.agent_id, "built-agent-1");
    assert!(snapshot.has_model, "the handle must expose the model");
    assert!(
        snapshot.has_state,
        "the handle must expose the live agent state"
    );
    assert!(
        snapshot.tools.iter().any(|t| t == "echo_tool"),
        "allow-listed tool must be kept: {:?}",
        snapshot.tools
    );
    assert!(
        !snapshot.tools.iter().any(|t| t == "secret_tool"),
        "allow list must remove catalogued tools: {:?}",
        snapshot.tools
    );
    assert!(
        snapshot.tools.iter().any(|t| t == "agent_spawn"),
        "platform tools registered after the filter must survive an allow list: {:?}",
        snapshot.tools
    );

    // The same handle is reachable after the build, and it agrees with the callback.
    let handle = agent.handle();
    assert_eq!(handle.name(), snapshot.name);
    assert_eq!(handle.agent_id(), snapshot.agent_id);
    assert!(handle.state().is_some());

    // Sanity: the agent still runs.
    let mut stream = agent.stream_events(vec![Msg::user("hi")], StreamOptions::default());
    let mut events = 0;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        event.expect("agent run must not fail");
        events += 1;
    }
    assert!(events > 0, "the built agent produced no events");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn tools_json_in_the_workspace_is_loaded_and_applied() {
    let root = temp_root("tools_json");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // Deliberately written through the workspace loader path (no explicit `tools_config`),
    // so a builder that never loads `tools.json` leaves the denied tool in place.
    std::fs::write(
        workspace.join("tools.json"),
        r#"{ "deny": ["dangerous_tool"] }"#,
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let base = Toolkit::default();
    base.register_tool(Arc::new(CountingTool {
        name: "dangerous_tool",
        calls: Arc::clone(&calls),
    }));
    base.register_tool(Arc::new(CountingTool {
        name: "safe_tool",
        calls: Arc::clone(&calls),
    }));

    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_hook = Arc::clone(&seen);

    let _agent = HarnessAgent::builder()
        .name("tools-json-agent")
        .model(Arc::new(TextModel))
        .cwd(root.clone())
        .workspace(workspace.clone())
        .toolkit(base)
        .disable_web_tools()
        .on_agent_built(move |handle| {
            seen_hook.lock().unwrap().push(handle.toolkit().tool_names());
        })
        .build();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let tools = &seen[0];
    assert!(
        !tools.iter().any(|t| t == "dangerous_tool"),
        "`tools.json` deny must be loaded from the workspace: {tools:?}"
    );
    assert!(
        tools.iter().any(|t| t == "safe_tool"),
        "unlisted tools survive a deny-only config: {tools:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
