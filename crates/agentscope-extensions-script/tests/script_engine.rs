#![cfg(feature = "js")]

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::message::{ContentBlock, JsonMap, ToolResultBlock, ToolUseBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_extensions_script::{
    FsSandbox, HttpRequestTool, RunScriptTool, ScriptEngine, ScriptLimits,
};
use async_trait::async_trait;
use serde_json::json;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo back the JSON args"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }
    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> CoreResult<ToolResultBlock> {
        Ok(ToolResultBlock::text(serde_json::to_string(&args).unwrap()))
    }
}

struct FailTool;

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "always fails"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }
    async fn call(
        &self,
        _args: JsonMap,
        _ctx: ToolExecutionContext,
    ) -> CoreResult<ToolResultBlock> {
        Err(Error::Tool("boom".into()))
    }
}

fn engine_with(tools: Vec<Arc<dyn Tool>>, limits: ScriptLimits) -> ScriptEngine {
    let tk = Toolkit::new();
    tk.register_tools(tools);
    ScriptEngine::new(tk, limits)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("agentscope-script-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tool_text(block: &ToolResultBlock) -> String {
    match block.output.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_echo_script() {
    let engine = engine_with(vec![Arc::new(EchoTool)], ScriptLimits::default());
    let out = engine
        .run(r#"const r = tools.echo({a:1}); result(r.output[0].text);"#)
        .await
        .unwrap();
    assert!(out.contains("\"a\":1"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_error_is_catchable() {
    let engine = engine_with(vec![Arc::new(FailTool)], ScriptLimits::default());
    let out = engine
        .run(
            r#"
            let caught = "";
            try { tools.fail({}); } catch (e) { caught = String(e.message); }
            result(caught);
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("boom"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_stops_dead_loop() {
    let limits = ScriptLimits {
        timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let engine = engine_with(vec![], limits);
    let res = tokio::time::timeout(Duration::from_secs(10), engine.run("while(true){}")).await;
    assert!(res.is_ok(), "outer timeout should not win");
    assert!(res.unwrap().is_err(), "dead loop should be interrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_bomb_is_contained() {
    let limits = ScriptLimits {
        timeout: Duration::from_secs(3),
        memory_bytes: 4 << 20,
        ..Default::default()
    };
    let engine = engine_with(vec![], limits);
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run("let a=[]; while(true){ a.push(new Array(100000).fill(0)); }"),
    )
    .await;
    assert!(res.is_ok(), "outer timeout should not win");
    assert!(res.unwrap().is_err(), "memory bomb should be rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_read_write_roundtrip() {
    let dir = temp_dir("rw");
    let tk = Toolkit::new();
    FsSandbox::new(&dir).register(&tk);
    let engine = ScriptEngine::new(tk, ScriptLimits::default());
    let out = engine
        .run(
            r#"
            tools.write_file({path:"a.txt", content:"hello"});
            const r = tools.read_file({path:"a.txt"});
            result(r.output[0].text);
            "#,
        )
        .await
        .unwrap();
    assert_eq!(out, "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_rejects_path_traversal() {
    let dir = temp_dir("trav");
    let tk = Toolkit::new();
    FsSandbox::new(&dir).register(&tk);
    let engine = ScriptEngine::new(tk, ScriptLimits::default());
    let out = engine
        .run(
            r#"
            let msg = "";
            try { tools.read_file({path: "../../../etc/passwd"}); }
            catch (e) { msg = String(e.message); }
            result(msg);
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("escapes sandbox"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_grep_and_glob() {
    let dir = temp_dir("gg");
    std::fs::write(dir.join("x.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(dir.join("y.rs"), "fn main(){}\n").unwrap();
    let tk = Toolkit::new();
    FsSandbox::new(&dir).register(&tk);
    let engine = ScriptEngine::new(tk, ScriptLimits::default());

    let grep = engine
        .run(r#"result(tools.grep({pattern:"beta"}).output[0].text);"#)
        .await
        .unwrap();
    assert!(grep.contains("beta"), "grep={grep}");

    let glob = engine
        .run(r#"result(tools.glob({pattern:"*.rs"}).output[0].text);"#)
        .await
        .unwrap();
    assert!(glob.contains("y.rs"), "glob={glob}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_rejects_private_address() {
    let tool = HttpRequestTool::with_default();
    let args: JsonMap = serde_json::from_value(json!({ "url": "http://127.0.0.1:9/" })).unwrap();
    let err = tool
        .call(args, ToolExecutionContext::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("private"), "err={err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_script_tool_executes() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(EchoTool));
    let engine = Arc::new(ScriptEngine::new(tk.clone(), ScriptLimits::default()));
    tk.register_tool(Arc::new(RunScriptTool::new(engine)));

    let input: JsonMap =
        serde_json::from_value(json!({ "script": "result(tools.echo({x:2}).output[0].text);" }))
            .unwrap();
    let call = ToolUseBlock::new("t1", "run_script", input);
    let block = tk
        .call_tool(&call, ToolExecutionContext::default())
        .await
        .unwrap();
    let text = tool_text(&block);
    assert!(text.contains("\"x\":2"), "text={text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_rejects_symlink_escape() {
    let root = temp_dir("symroot");
    let outside = temp_dir("symout");
    std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();
    let link = root.join("link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    {
        // Directory symlinks need Developer Mode / admin; skip when unavailable.
        if std::os::windows::fs::symlink_dir(&outside, &link).is_err() {
            return;
        }
    }

    let tk = Toolkit::new();
    FsSandbox::new(&root).register(&tk);
    let engine = ScriptEngine::new(tk, ScriptLimits::default());
    let out = engine
        .run(
            r#"
            let msg = "";
            try { tools.read_file({path: "link/secret.txt"}); }
            catch (e) { msg = String(e.message); }
            result(msg);
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("escapes sandbox"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_streams_and_caps_oversized_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            // No Content-Length: this exercises the streaming cap, not the header pre-check.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await;
            let _ = sock.write_all(&vec![b'x'; 4096]).await;
        }
    });

    let tool = HttpRequestTool::new(agentscope_extensions_script::HttpConfig {
        deny_private: false,
        max_bytes: 128,
        ..Default::default()
    });
    let args: JsonMap =
        serde_json::from_value(json!({ "url": format!("http://{addr}/") })).unwrap();
    let err = tool
        .call(args, ToolExecutionContext::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large"), "err={err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_await_and_then() {
    let engine = engine_with(vec![Arc::new(EchoTool)], ScriptLimits::default());

    let awaited = engine
        .run(r#"const r = await tools.echo({a: 7}); result(r.output[0].text);"#)
        .await
        .unwrap();
    assert!(awaited.contains("\"a\":7"), "awaited={awaited}");

    let then = engine
        .run(r#"tools.echo({b: 2}).then((r) => { result(r.output[0].text); });"#)
        .await
        .unwrap();
    assert!(then.contains("\"b\":2"), "then={then}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_iife_outputs_after_microtasks_flush() {
    let engine = engine_with(vec![Arc::new(EchoTool)], ScriptLimits::default());

    let out = engine
        .run(
            r#"
            (async () => {
              const r = await tools.echo({c: 3});
              result(r.output[0].text);
            })();
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("\"c\":3"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_return_value_becomes_result() {
    let engine = engine_with(vec![Arc::new(EchoTool)], ScriptLimits::default());
    let out = engine
        .run(
            r#"
            const r = await tools.echo({d: 4});
            return r.output[0].text;
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("\"d\":4"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_rejection_is_catchable() {
    let engine = engine_with(vec![], ScriptLimits::default());
    let out = engine
        .run(
            r#"
            let msg = "";
            try { await Promise.reject(new Error("nope")); } catch (e) { msg = String(e.message); }
            result(msg);
            "#,
        )
        .await
        .unwrap();
    assert!(out.contains("nope"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_message_is_preserved() {
    let engine = engine_with(vec![], ScriptLimits::default());
    let err = engine.run(r#"throw new Error("boom");"#).await.unwrap_err();
    assert!(err.to_string().contains("boom"), "err={err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_output_falls_back_to_result() {
    let engine = engine_with(vec![], ScriptLimits::default());
    let out = engine
        .run(r#"console.log("hello", {a: 1});"#)
        .await
        .unwrap();
    assert!(out.contains("hello"), "out={out}");
    assert!(out.contains("\"a\":1"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_symbols_are_hidden() {
    let engine = engine_with(vec![], ScriptLimits::default());
    let out = engine
        .run(r#"result([typeof __call_tool, typeof __tool_names].join(","));"#)
        .await
        .unwrap();
    assert_eq!(out, "undefined,undefined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_is_reported_as_time_limit() {
    let limits = ScriptLimits {
        timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let engine = engine_with(vec![], limits);
    let err = engine.run("while(true){}").await.unwrap_err();
    assert!(err.to_string().contains("time limit"), "err={err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tools_are_hidden() {
    let tk = Toolkit::new();
    tk.register_tool(Arc::new(EchoTool));
    let engine = ScriptEngine::new(tk, ScriptLimits::default()).with_denied_tools(["echo"]);
    let out = engine.run(r#"result(typeof tools.echo);"#).await.unwrap();
    assert_eq!(out, "undefined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_awaits_thenable_tool() {
    let engine = engine_with(vec![Arc::new(EchoTool)], ScriptLimits::default());
    let out = engine.run(r#"result(tools.echo({z: 9}));"#).await.unwrap();
    assert!(out.contains("output"), "out={out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_resolves_promise() {
    let engine = engine_with(vec![], ScriptLimits::default());
    let out = engine
        .run(r#"result(Promise.resolve("ok"));"#)
        .await
        .unwrap();
    assert_eq!(out, "ok");
}
