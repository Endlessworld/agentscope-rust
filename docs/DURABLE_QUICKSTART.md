# Durable quickstart

The shortest path from an empty directory to "the process died and the turn continued".
Everything below is in [`examples/durable-quickstart`](../examples/durable-quickstart).

## Run it twice

```bash
cargo run -p durable-quickstart -- ./durable-quickstart
cargo run -p durable-quickstart -- ./durable-quickstart
```

The first run prints the turn it admitted (`run-1`), its `Succeeded` ending and the committed
events. The second run prints that `run-1` was already admitted, admits `run-2` and settles it
on the same state. Nothing is restored in memory: the backend reads the session from disk.

## The same code on SQLite

```bash
cargo run -p durable-quickstart -- ./durable-sqlite --sqlite
```

Only the `LocalBackend::open(root)` line changes:

```rust
let backend: Arc<dyn SessionBackend> = agentscope_durable_sqlite::open_backend(path)?;
```

## What the example does

1. Opens one backend (`LocalBackend` or `open_backend`) and ensures the session and thread
   exist — the same calls on the first and every later run.
2. Builds `DurableRuntime` with one adapter. `HarnessAdapter::execute` is where a real host
   drives the harness; here it commits one answer and ends the turn.
3. Admits turns through `DurableSession::submit` (the request id makes a retry idempotent) and
   advances them with `DurableSession::tick` until `Idle`.
4. Prints `turns` and the committed `events` page.

## Where to go next

- `crates/agentscope-durable-adapter-agentscope` — the adapter that runs a `HarnessAgent`.
- `agentscope_durable_worker::ResidentRunner` — a host loop that ticks a set of threads.
- `agentscope_durable_http` — the same backend over HTTP, with committed SSE streaming.
- `docs/DURABLE_GAP_CLOSURE_PLAN.md` — the closure plan this quickstart belongs to.
