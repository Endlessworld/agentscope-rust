use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{JsonMap, ToolUseBlock};
use agentscope_core::tool::{ToolExecutionContext, Toolkit};
use rquickjs::{CatchResultExt, Context, Function, Runtime};
use serde_json::json;

use crate::limits::ScriptLimits;

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Runs JS scripts against a [`Toolkit`].
///
/// Each registered tool is exposed as `tools.<name>(args)`; the script must call `result(v)` to
/// return. The engine never blocks async workers: QuickJS is `!Send`, so execution happens inside
/// `spawn_blocking`, and async tools are awaited with `Handle::block_on` on that blocking thread.
#[derive(Clone)]
pub struct ScriptEngine {
    toolkit: Toolkit,
    limits: ScriptLimits,
    /// Tool names scripts cannot see or call (self-recursion, subagent spawning, …).
    denied_tools: Arc<HashSet<String>>,
}

impl ScriptEngine {
    pub fn new(toolkit: Toolkit, limits: ScriptLimits) -> Self {
        Self {
            toolkit,
            limits,
            denied_tools: Arc::new(HashSet::new()),
        }
    }

    /// Hides `tools` from scripts: a denied name is neither exposed in `tools` nor callable.
    ///
    /// Build it from the tools the host must not let a script reach (e.g. the script runner
    /// itself, subagent spawning, session-mutating tools). Denying is per-name and replaces any
    /// previous list.
    pub fn with_denied_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.denied_tools = Arc::new(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn limits(&self) -> &ScriptLimits {
        &self.limits
    }

    /// Executes `source` and returns whatever the script passed to `result(...)`.
    pub async fn run(&self, source: impl Into<String>) -> Result<String> {
        let toolkit = self.toolkit.clone();
        let limits = self.limits.clone();
        let denied = Arc::clone(&self.denied_tools);
        let source = source.into();
        let handle = tokio::runtime::Handle::current();

        let join = tokio::task::spawn_blocking(move || -> Result<String> {
            let rt = Runtime::new().map_err(rq_err)?;
            rt.set_memory_limit(limits.memory_bytes);
            rt.set_max_stack_size(limits.stack_bytes);
            let deadline = Instant::now() + limits.timeout;
            let interrupt_deadline = deadline;
            rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > interrupt_deadline)));

            let ctx = Context::full(&rt).map_err(rq_err)?;
            ctx.with(|ctx| {
                let tk = Rc::new(toolkit);
                let tk_for_fn = Rc::clone(&tk);
                let handle_for_fn = handle.clone();
                let bridge =
                    Function::new(ctx.clone(), move |name: String, args: String| -> String {
                        let tk = Rc::clone(&tk_for_fn);
                        let h = handle_for_fn.clone();
                        let call = || h.block_on(call_tool_async(tk.as_ref(), &name, &args));
                        // `panic = "abort"` (release/embedded) cannot trap a panic with
                        // `catch_unwind`; call directly there instead of pretending to.
                        #[cfg(panic = "unwind")]
                        let outcome: std::result::Result<
                            serde_json::Value,
                            String,
                        > = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call))
                            .map_err(|_| "tool panicked".to_string())
                            .and_then(|r| r.map_err(|e| e.to_string()));
                        #[cfg(not(panic = "unwind"))]
                        let outcome: std::result::Result<
                            serde_json::Value,
                            String,
                        > = call().map_err(|e| e.to_string());
                        match outcome {
                            Ok(v) => json!({ "ok": true, "data": v }).to_string(),
                            Err(e) => json!({ "ok": false, "error": e }).to_string(),
                        }
                    })
                    .map_err(rq_err)?;
                ctx.globals().set("__call_tool", bridge).map_err(rq_err)?;
                let names: Vec<String> = tk
                    .tool_names()
                    .into_iter()
                    .filter(|name| !denied.contains(name))
                    .collect();
                let names = serde_json::to_string(&names).map_err(Error::from)?;
                ctx.globals().set("__tool_names", names).map_err(rq_err)?;
                ctx.eval::<(), _>(PRELUDE).map_err(rq_err)?;
                // Wrap the user script in an async IIFE so top-level `await` is legal, then drain
                // the microtask queue (`execute_pending_job`) so promise continuations actually run.
                let wrapped = format!(
                    "(async () => {{\n{source}\n}})().then(\n  (v) => {{\n    if (globalThis.__result === undefined && v !== undefined) {{\n      globalThis.__result = typeof v === 'string' ? v : JSON.stringify(v);\n    }}\n    globalThis.__settled = true;\n  }},\n  (e) => {{\n    if (e instanceof Error) {{\n      const head = (e.name || 'Error') + (e.message ? ': ' + e.message : '');\n      const st = e.stack || '';\n      globalThis.__error = st.includes(head) ? st : st ? head + '\\n' + st : head;\n    }} else if (e === null || e === undefined) {{\n      globalThis.__error = 'script aborted (out of memory or internal error)';\n    }} else {{\n      globalThis.__error = String(e);\n    }}\n    globalThis.__settled = true;\n  }}\n);"
                );
                ctx.eval::<(), _>(wrapped.as_str())
                    .catch(&ctx)
                    .map_err(|e| {
                        if Instant::now() > deadline {
                            Error::Interrupted("script exceeded time limit".into())
                        } else {
                            Error::Tool(format!("script error: {}", format_caught(&e)))
                        }
                    })?;
                loop {
                    if Instant::now() > deadline {
                        return Err(Error::Interrupted("script exceeded time limit".into()));
                    }
                    if !ctx.execute_pending_job() {
                        break;
                    }
                }
                if Instant::now() > deadline {
                    return Err(Error::Interrupted("script exceeded time limit".into()));
                }
                let err: Option<String> = ctx.globals().get("__error").map_err(rq_err)?;
                if let Some(err) = err {
                    return Err(Error::Tool(format!("script error: {err}")));
                }
                let out: Option<String> = ctx.globals().get("__result").map_err(rq_err)?;
                if out.is_none() {
                    // Fall back to console output so scripts without `result(...)` are debuggable.
                    let logs: Option<String> = ctx
                        .eval::<Option<String>, _>(
                            r#"globalThis.__logs.length ? globalThis.__logs.join('\n') : undefined"#,
                        )
                        .map_err(rq_err)?;
                    if let Some(logs) = logs {
                        return Ok(logs);
                    }
                }
                Ok(out.unwrap_or_default())
            })
        });

        match tokio::time::timeout(self.limits.timeout + Duration::from_secs(1), join).await {
            Ok(joined) => {
                joined.map_err(|e| Error::Agent(format!("script task join error: {e}")))?
            }
            Err(_) => return Err(Error::Interrupted("script exceeded time limit".into())),
        }
    }
}

fn rq_err(e: rquickjs::Error) -> Error {
    Error::Tool(format!("script engine: {e}"))
}

/// Turns a caught JS error into a diagnostic string (name + message + stack, or the thrown value).
///
/// rquickjs' default `Display` for `Error::Exception` is just "Exception generated by QuickJS",
/// which drops the message; this keeps `new Error("boom")` readable.
fn format_caught(err: &rquickjs::CaughtError<'_>) -> String {
    match err {
        rquickjs::CaughtError::Exception(e) => {
            let name = e
                .as_object()
                .get::<_, Option<String>>("name")
                .ok()
                .flatten()
                .unwrap_or_else(|| "Error".to_string());
            let message = e.message().unwrap_or_default();
            let head = if message.is_empty() {
                name
            } else {
                format!("{name}: {message}")
            };
            match e.stack() {
                Some(stack) if !stack.is_empty() && !stack.contains(&head) => {
                    format!("{head}\n{stack}")
                }
                Some(stack) if !stack.is_empty() => stack,
                _ => head,
            }
        }
        rquickjs::CaughtError::Value(v) => {
            if v.is_null() || v.is_undefined() {
                "script aborted (out of memory or internal error)".to_string()
            } else if let Some(s) = v.as_string().and_then(|s| s.to_string().ok()) {
                s
            } else {
                "non-Error value thrown".to_string()
            }
        }
        rquickjs::CaughtError::Error(e) => e.to_string(),
    }
}

async fn call_tool_async(tk: &Toolkit, name: &str, args_json: &str) -> Result<serde_json::Value> {
    let input: JsonMap = if args_json.trim().is_empty() {
        JsonMap::new()
    } else {
        serde_json::from_str(args_json).map_err(Error::from)?
    };
    let id = format!("js-{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed));
    let call = ToolUseBlock::new(id, name, input);
    let exec_ctx = ToolExecutionContext {
        tool_call: Some(call.clone()),
        ..Default::default()
    };
    let block = tk.call_tool(&call, exec_ctx).await?;
    serde_json::to_value(&block).map_err(Error::from)
}

const PRELUDE: &str = r#"
(() => {
  const call_tool = globalThis.__call_tool;
  const tool_names = JSON.parse(globalThis.__tool_names);
  delete globalThis.__call_tool;
  delete globalThis.__tool_names;

  const defineHidden = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: false,
    });
  defineHidden('__result', undefined);
  defineHidden('__settled', false);
  defineHidden('__error', null);
  defineHidden('__logs', []);

  globalThis.tools = {};
  for (const name of tool_names) {
    Object.defineProperty(globalThis.tools, name, {
      enumerable: true,
      value: (args) => {
        const r = JSON.parse(call_tool(name, JSON.stringify(args ?? {})));
        if (!r.ok) throw new Error('tool "' + name + '" failed: ' + r.error);
        const data = r.data;
        // Thenable so `await tools.x()` / `tools.x().then(...)` work while direct field access
        // (`tools.x().output[0].text`) keeps working; `then` is non-enumerable.
        if (data && typeof data === 'object' && typeof data.then !== 'function') {
          Object.defineProperty(data, 'then', {
            enumerable: false,
            configurable: true,
            value: (onFulfilled, onRejected) => {
              const plain = Object.assign({}, data);
              delete plain.then;
              return Promise.resolve(plain).then(onFulfilled, onRejected);
            },
          });
        }
        return data;
      },
    });
  }

  globalThis.text = (block) => {
    if (typeof block === 'string') return block;
    if (!block || !Array.isArray(block.output)) return '';
    return block.output
      .map((b) => (b && typeof b.text === 'string' ? b.text : JSON.stringify(b)))
      .join('\n');
  };

  const setResult = (v) => {
    globalThis.__result = typeof v === 'string' ? v : JSON.stringify(v);
  };
  globalThis.result = (v) => {
    // A thenable (Promise, or one of the tool results) is awaited so `result(p)` does not
    // silently serialize to `{}`.
    if (v && typeof v.then === 'function') {
      v.then(setResult, (e) => {
        globalThis.__error = e && e.stack ? String(e.stack) : String(e);
        globalThis.__settled = true;
      });
      return;
    }
    setResult(v);
  };

  globalThis.console = {};
  for (const level of ['log', 'debug', 'info', 'warn', 'error']) {
    globalThis.console[level] = (...args) => {
      globalThis.__logs.push(
        args
          .map((a) => {
            try {
              return typeof a === 'string' ? a : JSON.stringify(a);
            } catch (e) {
              return String(a);
            }
          })
          .join(' ')
      );
    };
  }
})();
"#;
