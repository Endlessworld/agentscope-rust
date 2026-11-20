//! Sandboxed process execution behind the durable runtime's executor port.
//!
//! The durable runtime never performs a side effect itself: it records an execution identity,
//! hands the call to an [`Executor`](agentscope_durable::executor::Executor) and treats what that
//! executor reports as the fact of the turn. This crate is that port for a sandbox: [`Sandbox`]
//! is the three methods a runner has to answer (`spawn`, `poll`, `kill`), and [`SandboxExecutor`]
//! turns those answers into the runtime's [`ExecutionResult`](agentscope_durable::ExecutionResult).
//!
//! The mapping is the whole crate, so it is written down here:
//!
//! | sandbox answer | durable state | why |
//! | --- | --- | --- |
//! | `spawn`/`poll` error before any effect | `INVALID` failure | the call never left the host |
//! | `poll` -> [`Poll::Running`] | `PENDING` | no verdict yet; **never** concluded |
//! | `poll` -> `Exited { code: 0, .. }` | `SUCCEEDED` | with exit code and captured output |
//! | `poll` -> `Exited { code != 0, .. }` | `FAILED` | the command's own verdict, code kept |
//! | deadline passed, still running | `CANCELLED` + `outcome: TIMED_OUT` | see below |
//! | `kill` accepted | `CANCELLED` | the host stopped waiting |
//! | handle no longer visible | `UNKNOWN` | honest: never guessed either way |
//!
//! A `PENDING` result is the one that must never be mistaken for an outcome. The runtime uses
//! "concluded" to decide whether a turn may end, and `PENDING` is not concluded, so a turn whose
//! effect is still running waits instead of reporting a success nobody proved.
//!
//! `ExecutionState` has no `TIMED_OUT`: a timeout is reported as `CANCELLED` with
//! `result.outcome = "TIMED_OUT"` and `result.timeoutMs`, because a watchdog stop is not the
//! command's own failure — a `FAILED` here would read as "the program exited non-zero" and would
//! also make the effect indistinguishable from a real crash in every downstream report. Whatever
//! output the sandbox could still report after the kill is kept, and `truncated` follows
//! [`ExecSpec::max_output_bytes`].
//!
//! Output capture is byte-exact: the limit counts bytes, not characters, and when a stream is
//! longer than the limit the first half of the budget is kept from the head and the rest from the
//! tail, with `*Bytes`/`*TotalBytes` in the payload describing exactly how many bytes survived.
//!
//! [`FakeSandbox`] is a scripted sandbox for tests: no process, no clock, no I/O. It is production
//! code so a downstream crate can assert its own executor wiring without a container.

mod bindings;
mod executor;
mod fake;
mod output;
mod sandbox;

pub use bindings::{
    BindingRecord, BindingState, BindingStore, FileBindings, MemoryBindings, SandboxBindings,
};
pub use executor::{
    SandboxExecutor, OUTCOME_CANCELLED, OUTCOME_FAILED, OUTCOME_RUNNING, OUTCOME_SUCCEEDED,
    OUTCOME_TIMED_OUT,
};
pub use fake::FakeSandbox;
pub use output::{capture, Captured};
pub use sandbox::{
    ExecSpec, Handle, Poll, Sandbox, SandboxError, SandboxErrorKind, DEFAULT_MAX_OUTPUT_BYTES,
};
