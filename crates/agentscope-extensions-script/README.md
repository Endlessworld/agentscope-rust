# agentscope-extensions-script

Embedded JavaScript (via [rquickjs](https://crates.io/crates/rquickjs)) scripting for
agent tool orchestration.

It exposes every tool registered on an `agentscope_core::tool::Toolkit` as `tools.<name>(args)`
inside a sandboxed JS snippet, so an LLM or user can write loops/conditions that call tools.

## What is in here

- `ScriptEngine` — runs a JS source string against a `Toolkit`; the script calls `result(v)` to
  return. Memory, stack, and wall-clock limits are enforced.
- `FsSandbox` — registers `read_file` / `write_file` / `list_dir` / `grep` / `glob` tools rooted at
  a sandbox directory (path traversal is rejected).
- `HttpRequestTool` — a guarded `http_request` tool (scheme allowlist, SSRF private-IP rejection,
  timeout, response-size cap).
- `RunScriptTool` — exposes the engine itself as a `run_script` tool so a model can trigger it.

> **Fact:** rquickjs/QuickJS ships **no** filesystem or network APIs. All host capabilities come
> from tools the host registers (see `FsSandbox` / `HttpRequestTool`).

## Minimal use

```rust,ignore
use std::sync::Arc;
use agentscope_core::tool::Toolkit;
use agentscope_extensions_script::{FsSandbox, RunScriptTool, ScriptEngine, ScriptLimits};

let toolkit = Toolkit::new();
FsSandbox::new("./").register(&toolkit);
let engine = Arc::new(ScriptEngine::new(toolkit.clone(), ScriptLimits::default()));
toolkit.register_tool(Arc::new(RunScriptTool::new(engine.clone())));

let out = engine.run(r#"
  const r = tools.read_file({ path: "Cargo.toml" });
  result({ len: r.output[0].text.length });
"#).await.unwrap();
```

## Limits

`ScriptLimits` caps wall-clock time (interrupt handler + outer timeout), memory, and stack.
rquickjs `Runtime`/`Context` are `!Send`, so execution runs in `spawn_blocking`; async tools are
bridged with `Handle::block_on` on that blocking thread.
