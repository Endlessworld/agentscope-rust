# Migration Gap Tracker — Rust `agentscope-harness`

Tracks the porting of `io.agentscope.harness` (Java) to Rust, driven by
`Feature_Gap_Analysis.md`. Status legend:

- ❌ **Missing** — no Rust equivalent at all
- 🟡 **Stub** — present but returns `not implemented` / no-op
- 🟠 **Partial** — exists but incomplete vs Java
- ✅ **Done** — functionally equivalent

Each row lists the target Rust module path to create/extend.

## 🔴 Missing Modules

| # | Java module | Rust target | Java size | Status | Priority |
|---|---|---|---|---|---|
| M1 | `agent/team/` (TeamClient, LocalTeamClient, TeamContext, specs, TeamTask, TeamMessage…) | `src/agent/team/` | ~31KB | ❌ | P1 |
| M2 | `agent/coordination/` (PeriodicGate, LocalPeriodicGate, StoreBackedPeriodicGate) | `src/agent/coordination/` | — | ❌ | P2 |
| M3 | `agent/transcript/` (TranscriptStore, Filesystem/ObjectStore impls, TranscriptRef) | `src/agent/transcript/` | — | ✅ | P1 |
| M4 | `agent/middleware/TranscriptMiddleware` | `src/agent/middleware/transcript.rs` | — | ✅ | P1 |
| M5 | `agent/middleware/TeamsMiddleware` | `src/agent/middleware/teams.rs` | 31KB | ❌ | P1 |
| M6 | `agent/middleware/SandboxLifecycleMiddleware` | `src/agent/middleware/sandbox_lifecycle.rs` | — | ❌ | P1 |

## 🔴 Missing Tools (12)

| # | Java Tool | Rust target | Java size | Status | Priority |
|---|---|---|---|---|---|
| T1 | `AgentSpawnTool` | `src/tool/agent_spawn.rs` | 84KB | ❌ | P0 |
| T2 | `AgentGenerateTool` | `src/tool/agent_generate.rs` | 7.5KB | ✅ | P2 |
| T3 | `TeamTool` | `src/tool/team.rs` | 26KB | ❌ | P1 |
| T4 | `MemorySearchTool` | `src/tool/memory_search.rs` | 3.5KB | ✅ | P0 |
| T5 | `MemoryGetTool` | `src/tool/memory_get.rs` | 3.3KB | ✅ | P0 |
| T6 | `MemorySaveTool` | `src/tool/memory_save.rs` | 3.9KB | ✅ | P0 |
| T7 | `SessionSearchTool` | `src/tool/session_search.rs` | 14.6KB | ❌ | P2 |
| T8 | `PlanModeTools` (plan_enter/plan_write/plan_exit) | `src/tool/plan_mode.rs` | 10.3KB | ❌ | P1 |
| T9 | `WaitAsyncResultsTool` | `src/tool/wait_async.rs` | 21.8KB | ❌ | P1 |
| T10 | `ProposeSkillTool` | `src/agent/skill/load_tool.rs` (already present) | 9.8KB | ✅ | P2 |
| T11 | `SkillManageTool` | `src/agent/skill/manage.rs` (already present) | 32.8KB | ✅ | P2 |
| T12 | `WebTools` (web_fetch/web_search) | `src/tool/web_tools.rs` | 7KB | ❌ | P2 |

## 🟡 Stub / Incomplete Files

| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| S1 | `filesystem/remote.rs` (RemoteFilesystem) | `src/filesystem/remote.rs` | 🟡 Stub | P2 |
| S2 | `remote/store/` (BaseStore, InMemoryStore, NamespaceFactory, StoreItem) | `src/filesystem/remote/store.rs` | ❌ | P2 |
| S3 | `BakedContextFilesystem` | `src/filesystem/baked.rs` | ❌ | P2 |
| S4 | `ProjectAwareOverlay` | `src/filesystem/overlay.rs` | ❌ | P2 |
| S5 | `RoutedSandboxFilesystem` | `src/filesystem/sandbox.rs` | ❌ | P2 |
| S6 | `LocalFilesystemWithShell` | `src/filesystem/local.rs` | 🟠 Partial | P2 |
| S7 | Filesystem result types (EditResult/WriteResult/ReadResult/LsResult/GlobResult/GrepResult/FileDownload/Upload) | `src/filesystem/result.rs` | ✅ | P1 |
| S8 | `FilesystemUtils` | `src/filesystem/util.rs` | ❌ | P2 |
| S9 | `SkillSecurityScanner` (6 pattern classes) | `src/agent/skill/curator.rs` | ❌ | P2 |
| S10 | Curator filters (CanaryFilter/EnvironmentFilter/NotifyAndWaitGate/NotificationSink/FilesystemSkillUsageBackend) | `src/agent/skill/curator.rs` | ❌ | P2 |

## 🟡 Sandbox

| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| B1 | Docker Sandbox backend (DockerSandbox, Client, Options, State) | `src/agent/sandbox/docker.rs` | ❌ | P0 |
| B2 | Sandbox Snapshot (Local/Remote/Noop) | `src/agent/sandbox/snapshot.rs` | ❌ | P2 |
| B3 | Sandbox Layout entries (Workspace/Dir/File/Git/Bind entries) | `src/agent/sandbox/layout.rs` | ❌ | P2 |
| B4 | Sandbox helpers (MountSupport/ArchiveExtractor/ProjectionApplier/SessionStateStore) | `src/agent/sandbox/` | ❌ | P2 |

## 🟡 Gateway

| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| G1 | `ChannelFactory` | `src/agent/gateway/channel/factory.rs` | ❌ | P2 |
| G2 | `SubagentGatewayBridge` | `src/agent/gateway/subagent_bridge.rs` | ❌ | P2 |
| G3 | `SessionIdUtils` (SHA-256 12-hex) | `src/agent/gateway/` | ❌ | P2 |
| G4 | `TurnBusyException` | `src/agent/gateway/session_turn_gate.rs` | ❌ | P2 |
| G5 | `LocalSessionTurnGate` (CAS) | `src/agent/gateway/session_turn_gate.rs` | 🟠 Partial | P2 |
| G6 | `StoreBackedSubagentRegistry` | `src/agent/subagent/registry.rs` | ❌ | P2 |

## 🟡 Subagent Protocol / Remote

| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| R1 | `RemoteEventCodec` | `src/agent/subagent/protocol/codec.rs` | ❌ | P2 |
| R2 | `RemoteAgentEvent` / `RemoteEventType` | `src/agent/subagent/protocol/event.rs` | ❌ | P2 |
| R3 | `AgentProtocolTaskClient` | `src/agent/subagent/task/protocol_client.rs` | ❌ | P2 |

## 🟡 Other Details

| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| D1 | `AtPathExpansionMiddleware` (real `@path` expansion) | `src/agent/middleware/at_path.rs` | ✅ | P2 |
| D2 | `HarnessAgentBuilderSupport` (auto-wiring) | `src/agent/harness_agent.rs` | 🟠 Partial | P2 |
| D3 | `SkillRuntime` completeness | `src/agent/skill/runtime.rs` | 🟠 Partial | P2 |
| D4 | `WorkspaceSpecApplier` (+ProjectionApplier/MountSupport/ArchiveExtractor) | `src/agent/sandbox/` | 🟠 Partial | P2 |
| D5 | `SubagentDeclaration` complexity | `src/agent/subagent/loader.rs` | 🟠 Partial | P2 |
| D6 | `WorkspaceTaskRepository` complexity | `src/agent/subagent/task/repository.rs` | 🟠 Partial | P2 |

## Progress Summary

| Phase | Items | Done |
|---|---|---|
| P0 | T1, T4, T5, T6, B1 | 3/5 |
| P1 | M1, M3, M4, M5, M6, T3, T8, T9, S7 | 4/9 |
| P2 | M2, T2, T7, T10, T11, T12, S1–S10, B2–B4, G1–G6, R1–R3, D1–D6 | 4/30 |
