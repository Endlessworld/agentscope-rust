//! Integration tests for the core tool execution & governance layer:
//! ToolGroupManager, ToolValidator, ToolkitConfig, ToolExecutor (timeout/retry)
//! and preset parameters.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use agentscope_core::{
    Error, ExecutionConfig, FunctionTool, JsonMap, Result, Tool, ToolExecutionContext,
    ToolResultBlock, ToolUseBlock, Toolkit, ToolkitConfig,
};

fn parse_args(json: &str) -> JsonMap {
    serde_json::from_str(json).unwrap()
}

fn text_tool(name: &str) -> Arc<dyn Tool> {
    let name = name.to_string();
    Arc::new(FunctionTool::new(
        name.clone(),
        format!("tool {name}"),
        serde_json::json!({"type": "object"}),
        Box::new(move |_, _| Ok(ToolResultBlock::text(name.clone()))),
    ))
}

#[test]
fn group_activation_filters_schemas() {
    let tk = Toolkit::new();
    tk.register_tool(text_tool("a"));
    tk.register_tool(text_tool("b"));
    tk.create_tool_group("g1", "group 1", true);
    tk.add_tool_to_group("g1", "a");

    // b is ungrouped -> always active; a belongs to the active group g1.
    let names: Vec<String> = tk.tool_schemas().iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"a".to_string()));
    assert!(names.contains(&"b".to_string()));

    // Deactivate g1 -> a disappears, b stays.
    tk.update_tool_groups(&["g1".to_string()], false);
    let names2: Vec<String> = tk.tool_schemas().iter().map(|s| s.name.clone()).collect();
    assert!(!names2.contains(&"a".to_string()));
    assert!(names2.contains(&"b".to_string()));
}

#[tokio::test]
async fn inactive_group_blocks_call() {
    let tk = Toolkit::new();
    tk.register_tool(text_tool("c"));
    tk.create_tool_group("g2", "group 2", true);
    tk.add_tool_to_group("g2", "c");
    tk.update_tool_groups(&["g2".to_string()], false);

    let call = ToolUseBlock::new("call-1", "c", JsonMap::new());
    let err = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap_err();
    assert!(err.to_string().contains("not available"), "got: {err}");
}

#[tokio::test]
async fn validation_rejects_missing_required() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(FunctionTool::new(
        "need",
        "requires x",
        serde_json::json!({"type": "object", "properties": {"x": {"type": "number"}}, "required": ["x"]}),
        Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
    )));
    let call = ToolUseBlock::new("call-1", "need", JsonMap::new());
    let err = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap_err();
    assert!(err.to_string().contains("Parameter validation failed"), "got: {err}");
}

#[tokio::test]
async fn preset_parameters_override_input() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(FunctionTool::new(
        "greet",
        "greet",
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        Box::new(|args, _| {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            Ok(ToolResultBlock::text(format!("name={name}")))
        }),
    )));
    tk.set_preset_parameters("greet", parse_args(r#"{"name": "preset"}"#));
    let call = ToolUseBlock::new("call-1", "greet", parse_args(r#"{"name": "llm"}"#));
    let res = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap();
    assert_eq!(res.text_content(), "name=preset");
}

#[tokio::test]
async fn deletion_protection_disabled() {
    let tk = Toolkit::with_config(ToolkitConfig::builder().allow_tool_deletion(false).build());
    tk.register_tool(text_tool("x"));
    tk.remove_tool("x");
    assert!(tk.get_tool("x").is_some(), "tool must survive when deletion is disabled");
}

/// A tool whose call sleeps, used to exercise the timeout path.
struct SleepyTool;

#[async_trait]
impl Tool for SleepyTool {
    fn name(&self) -> &str { "sleepy" }
    fn description(&self) -> &str { "sleeps" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(ToolResultBlock::text("done"))
    }
}

#[tokio::test]
async fn timeout_aborts_slow_tool() {
    let cfg = ExecutionConfig::builder()
        .timeout(Duration::from_millis(50))
        .max_attempts(1)
        .build();
    let tk = Toolkit::with_config(ToolkitConfig::builder().execution_config(cfg).build());
    tk.register_tool(Arc::new(SleepyTool));
    let call = ToolUseBlock::new("call-1", "sleepy", JsonMap::new());
    let err = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap_err();
    assert!(err.to_string().contains("timeout"), "got: {err}");
}

/// A tool that fails once with a retryable HTTP 429, then succeeds.
struct FlakyTool {
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for FlakyTool {
    fn name(&self) -> &str { "flaky" }
    fn description(&self) -> &str { "flaky" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(Error::http_status(429, "rate limited"))
        } else {
            Ok(ToolResultBlock::text("ok"))
        }
    }
}

#[tokio::test]
async fn retry_succeeds_after_transient_error() {
    let cfg = ExecutionConfig::builder()
        .max_attempts(3)
        .initial_backoff(Duration::from_millis(10))
        .max_backoff(Duration::from_millis(20))
        .build();
    let tk = Toolkit::with_config(ToolkitConfig::builder().execution_config(cfg).build());
    let calls = Arc::new(AtomicU32::new(0));
    tk.register_tool(Arc::new(FlakyTool { calls: Arc::clone(&calls) }));
    let call = ToolUseBlock::new("call-1", "flaky", JsonMap::new());
    let res = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap();
    assert_eq!(res.text_content(), "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A concurrency-unsafe tool that tracks overlap: it bumps the in-flight counter
/// on entry and asserts it never exceeds one, so parallel overlap would fail.
struct UnsafeTool {
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for UnsafeTool {
    fn name(&self) -> &str { "unsafe" }
    fn description(&self) -> &str { "unsafe" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
    fn is_concurrency_safe(&self) -> bool { false }
    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak.load(Ordering::SeqCst);
        while cur > peak && self.peak.compare_exchange(peak, cur, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            peak = self.peak.load(Ordering::SeqCst);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolResultBlock::text("unsafe-ok"))
    }
}

/// A concurrency-safe tool that records concurrent overlap of safe invocations
/// (safe tools may overlap, so peak > 1 is allowed).
struct SafeTool {
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for SafeTool {
    fn name(&self) -> &str { "safe" }
    fn description(&self) -> &str { "safe" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
    fn is_concurrency_safe(&self) -> bool { true }
    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak.load(Ordering::SeqCst);
        while cur > peak && self.peak.compare_exchange(peak, cur, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            peak = self.peak.load(Ordering::SeqCst);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolResultBlock::text("safe-ok"))
    }
}

fn call(name: &str, i: u32) -> ToolUseBlock {
    ToolUseBlock::new(format!("call-{i}"), name, JsonMap::new())
}

#[tokio::test]
async fn parallel_partitions_unsafe_tools_into_serial_slots() {
    let in_flight = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(UnsafeTool { in_flight: Arc::clone(&in_flight), peak: Arc::clone(&peak) }));

    let calls = vec![call("unsafe", 1), call("unsafe", 2), call("unsafe", 3)];
    let results = tk
        .call_tools_with_config(&calls, ToolExecutionContext::default(), None, true)
        .await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.is_ok()), "got: {results:?}");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "unsafe tools must be serialized");
}

#[tokio::test]
async fn parallel_safe_tools_overlap_but_keep_order() {
    let in_flight = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(SafeTool { in_flight: Arc::clone(&in_flight), peak: Arc::clone(&peak) }));

    let calls = vec![call("safe", 1), call("safe", 2), call("safe", 3)];
    let results = tk
        .call_tools_with_config(&calls, ToolExecutionContext::default(), None, true)
        .await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.is_ok()), "got: {results:?}");
    assert!(peak.load(Ordering::SeqCst) >= 2, "safe tools should overlap");
    let texts: Vec<String> = results
        .iter()
        .map(|r| r.as_ref().unwrap().text_content())
        .collect();
    assert_eq!(texts, vec!["safe-ok"; 3]);
}

#[tokio::test]
async fn parallel_mixed_partitions_respect_declared_order() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(UnsafeTool { in_flight: Arc::new(AtomicU32::new(0)), peak: Arc::new(AtomicU32::new(0)) }));
    let safe = FunctionTool::new(
        "safe",
        "safe",
        serde_json::json!({"type": "object"}),
        Box::new(|args, _| {
            let tag = args.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            Ok(ToolResultBlock::text(format!("safe-{tag}")))
        }),
    );
    tk.register_tool(Arc::new(safe));

    let mut a = JsonMap::new();
    a.insert("tag".into(), serde_json::json!("a"));
    let mut b = JsonMap::new();
    b.insert("tag".into(), serde_json::json!("b"));
    let calls = vec![
        ToolUseBlock::new("c1", "safe", a.clone()),
        ToolUseBlock::new("c2", "unsafe", JsonMap::new()),
        ToolUseBlock::new("c3", "safe", b.clone()),
    ];
    let results = tk
        .call_tools_with_config(&calls, ToolExecutionContext::default(), None, true)
        .await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.is_ok()), "got: {results:?}");
    let texts: Vec<String> = results
        .iter()
        .map(|r| r.as_ref().unwrap().text_content())
        .collect();
    assert_eq!(texts, vec!["safe-a", "unsafe-ok", "safe-b"]);
}

#[tokio::test]
async fn sequential_mode_runs_in_order() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(UnsafeTool { in_flight: Arc::new(AtomicU32::new(0)), peak: Arc::new(AtomicU32::new(0)) }));
    let calls = vec![call("unsafe", 1), call("unsafe", 2)];
    let results = tk
        .call_tools_with_config(&calls, ToolExecutionContext::default(), None, false)
        .await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.is_ok()));
}
