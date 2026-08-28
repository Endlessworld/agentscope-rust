# Migration Plan — Rust `agentscope-harness`

Goal: progressively port `io.agentscope.harness` (Java) into
`crates/agentscope-harness`, tracked by `MIGRATION_GAP_TRACKER.md`.

Scope confirmed against the repo on <date>: the harness crate already has
`gateway/`, `subagent/task/`, `skill/curator`, `memory/`, `sandbox/` and a rich
`set of middleware files, but is **missing** the modules listed as ❌ in the
tracker (team, coordination, transcript, 12 tools, Transcript/Teams/SandboxLifecycle
middleware, Docker backend, RemoteFilesystem impl, security scanner, etc.).

## Guiding Principles

1. **Mirror Java structure** under `src/agent/...` and `src/tool/...` so the port
   stays reviewable.
2. **Reuse existing primitives** — `Filesystem`, `RuntimeContext`, `Toolkit`,
   `Middleware`, `SubagentRegistry`, `MemoryFlushManager`, `SandboxManager`. Do not
   reinvent them.
3. **Wire new pieces into `lib.rs` / `mod.rs`** as they land; keep the crate
   compiling (`cargo build -p agentscope-harness`).
4. **Tests alongside code** in `tests/`; add a focused unit test per new module.
5. **Smallest shippable slice first** — prefer P0 small tools over the 84KB
   `AgentSpawnTool` monolith; stub `AgentSpawnTool` with the public API surface and
   grow it iteratively.

## Phase P0 — Core Functionality Gap

| Item | Deliverable | Depends on | Acceptance |
|---|---|---|---|
| T4/T5/T6 Memory tools | `memory_search.rs`, `memory_get.rs`, `memory_save.rs`; read/write `MEMORY.md` + `memory/*.md` via `Filesystem` | `Filesystem` | Tools register into `Toolkit`; unit test round-trips a note |
| B1 Docker Sandbox | `sandbox/docker.rs` implementing `Sandbox`+`SandboxClient`; add `bollard` to Cargo.toml | `Sandbox` trait | `cargo build`; integration test gated behind docker availability |
| T1 AgentSpawnTool | `agent_spawn.rs`: public API (spawn, permission scoping, plan-mode propagation, cancel/end) | subagent registry, bus | Compiles with a thin but real implementation; expanded in later passes |

## Phase P1 — Important Functionality Gap

| Item | Deliverable |
|---|---|
| M3/M4 Transcript | `transcript/` store + `transcript.rs` middleware (append JSONL after each call) |
| M1/M5 Teams | `team/` module + `teams.rs` middleware + `team.rs` tool |
| M6 SandboxLifecycle | `sandbox_lifecycle.rs` middleware orchestrating `SandboxManager` |
| T8 PlanModeTools | `plan_mode.rs` (plan_enter/plan_write/plan_exit) wired to `PlanModeManager` |
| T9 WaitAsyncResults | `wait_async.rs` barrier over async-tool registry + inbox |
| T3 TeamTool | `team.rs` (covered by M5) |
| S7 FS result types | `filesystem/result.rs` (EditResult/WriteResult/ReadResult/LsResult/GlobResult/GrepResult/…) |

## Phase P2 — Enhancements

PeriodicGate coordination, SkillSecurityScanner, RemoteFilesystem full impl,
SubagentGatewayBridge, WebTools, SessionSearchTool, AgentGenerateTool,
ProposeSkillTool, SkillManageTool, sandbox snapshot/layout/helpers, gateway
factories, subagent protocol codecs, `@path` expansion, builder support.

## Build / Verify Commands

```powershell
cargo build -p agentscope-harness
cargo test  -p agentscope-harness
cargo clippy -p agentscope-harness
```

## Sequencing Notes

- Memory tools (T4–T6) and FS result types (S7) are the cheapest, highest-value
  P0/P1 wins and unblock downstream modules — start here.
- `AgentSpawnTool` and Docker are large; deliver a compiling skeleton + tests, then
  deepen in follow-up passes rather than blocking the crate.
- Each completed item flips its tracker row to ✅ and is committed separately.

## Progress Log

- 2026-08-24: T8 PlanModeTools + T9 WaitAsyncResultsTool (barrier mode) landed; M6 SandboxLifecycleMiddleware verified already implemented; AsyncToolRegistry gained list_session. Baseline: cargo test all green (core 119, harness 244). T1 AgentSpawnTool thin slice landed (5 tests).
- Next: B1 Docker sandbox; deepen AgentSpawnTool (streaming events, remote targets).
- 2026-08-24 (2): M1 agent/team landed: TeamClient trait + data model + LocalTeamClient (versioned CAS, mailboxes, wakeups, role clipping), 7 tests. Baseline core 119 + harness 256 = 375. Next: T3 TeamTool + M5 TeamsMiddleware on top of team/, B1 Docker sandbox.
- 2026-08-24 (3): T3 TeamTool landed: 10 ops dispatched over TeamClient with TeamContext role clipping, 4 tests. Baseline core 119 + harness 260 = 379. P1 remaining: M5 TeamsMiddleware. Next: M5 on top of team/ + team.rs, then B1 Docker sandbox.
- 2026-08-24 (4): M5 TeamsMiddleware landed: <agent-teams> system-prompt section + per-turn mailbox drain as <team-message> assistant tail; 4 tests. P1 complete (9/9). Baseline core 119 + harness 264 = 383. Next: B1 Docker sandbox (last P0).
