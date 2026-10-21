# Rust / Java Harness GAP Tracker

Last updated: 2026-10-21

Batch 1: GAP-101 through GAP-112.
Batch 2: GAP-201 through GAP-205, GAP-301 through GAP-303.

This file tracks framework-level differences between the Rust harness and the Java
`agentscope-harness` implementation. It is intentionally implementation-oriented:
each item names the observable behaviour, the Java reference, and the Rust status.

## Status legend

| Status | Meaning |
|---|---|
| `DONE` | Implemented and verified in Rust. |
| `IN_PROGRESS` | Being changed in the current work. |
| `TODO` | Confirmed gap, not yet implemented. |
| `BLOCKED` | Depends on another gap or a missing capability. |
| `MATCHES_JAVA` | Rust deliberately matches Java (including Java's own asymmetry). |
| `WONTFIX` | Reviewed and intentionally not changed. |

---

## Summary

| ID | Area | Status |
|---|---|---|
| GAP-001 | Workspace memory prompt flags (`disable_memory_tools` / `disable_memory_hooks`) | `DONE` |
| GAP-002 | Request-scoped system prompt lifecycle | `DONE` |
| GAP-101 | Main-agent compaction API semantics | `DONE` |
| GAP-102 | Declared subagent `workspace_path` + `AGENTS.md` system prompt | `DONE` |
| GAP-103 | General-purpose subagent `disable_workspace_context` | `DONE` |
| GAP-104 | General-purpose subagent `environment_memory` | `DONE` |
| GAP-105 | General-purpose subagent `additional_context_files` | `DONE` |
| GAP-106 | General-purpose subagent `max_context_tokens` | `DONE` |
| GAP-107 | Subagent `memory_config` inheritance | `DONE` |
| GAP-108 | General-purpose subagent `agent_tracing_log_enabled` | `DONE` |
| GAP-109 | Subagent plan-mode inheritance | `DONE` |
| GAP-110 | General-purpose subagent compaction inheritance | `DONE` |
| GAP-111 | General-purpose subagent tool-result eviction inheritance | `DONE` |
| GAP-112 | `use_legacy_xml_workspace_context` propagation | `DONE` |
| GAP-113 | At-path expansion inheritance | `MATCHES_JAVA` |
| GAP-114 | Declared-subagent compaction/eviction/workspace-context asymmetry | `MATCHES_JAVA` |
| GAP-115 | Subagent `disable_session_persistence` inheritance | `TODO` |
| GAP-201 | Async system-prompt hook | `DONE` |
| GAP-202 | Middleware `order()` / priority | `DONE` |
| GAP-203 | Full restored-state snapshot in pre-call hook | `DONE` |
| GAP-204 | Filesystem type reflection (`kind` / `as_any`) | `DONE` |
| GAP-205 | Artifact delivery (`deliver_artifact`) | `DONE` |
| GAP-301 | Effective workspace for local overlay | `DONE` |
| GAP-302 | Filesystem-specific workspace paragraph | `DONE` |
| GAP-303 | Artifact-delivery workspace notice | `DONE` |
| GAP-304 | Session-context OS/date/temp formatting parity | `TODO` |
| GAP-305 | Consume or retire `use_legacy_xml_workspace_context` | `TODO` |

---

## Completed baseline

### GAP-001 — Workspace memory prompt flags

Status: `DONE`

- `WorkspaceContextMiddleware` accepts `disable_memory_tools` / `disable_memory_hooks`.
- `MEMORY.md` is omitted only when both flags are set.
- Memory Recall, `memory_save`, hooks-only persistence, auto-extract, truncation notice,
  and workspace-files notice all follow Java's branch logic.
- `HarnessAgent` passes both flags.
- Subagent blueprint propagates both flags.

Verified by:

- `agentscope-harness/src/agent/middleware/workspace_context.rs` tests
- `harness_context_wiring_test::workspace_context_honors_memory_flags_through_the_real_build_path`

### GAP-002 — Request-scoped system prompt lifecycle

Status: `DONE`

- `AgentInput.system_prompt` carries request-scoped system material.
- `run_agent_loop` merges legacy SYSTEM messages into that channel.
- The prompt is frozen once per call after `on_agent`.
- `on_system_prompt` runs once per call, not once per reasoning iteration.
- Every reasoning request receives a fresh copy of the frozen system message.
- `AgentState.context` remains system-free; stale persisted SYSTEM entries are purged.
- Multiple SYSTEM messages produced by reasoning middleware are normalized to one.

Verified by:

- `agentscope-core/src/agent/react_agent.rs` tests:
  - `legacy_injected_system_prompts_are_request_scoped`
  - `persisted_system_messages_are_purged_from_restored_state`
  - `system_prompt_is_frozen_once_per_call_and_prepended_to_every_request`
  - `reasoning_middleware_cannot_leave_multiple_system_messages`

---

## P0 — Behavioural / API correctness

### GAP-101 — Main-agent compaction API semantics

Status: `DONE`

Previous Rust state:

- `HarnessAgentBuilder::compaction(CompactionMiddlewareConfig)` installed a prompt-only
  marker middleware.
- The real implementation lived behind `conversation_compaction(CompactionConfig)`.
- `disable_compaction()` only gated the marker, not the real compaction path.

Current Rust state:

- `HarnessAgentBuilder::compaction(CompactionConfig)` now installs the real
  `ConversationCompactionMiddleware`.
- `conversation_compaction(CompactionConfig)` remains as an alias.
- `disable_compaction()` gates the real compaction middleware.
- ACP and the coding-agent factory no longer install the prompt-only marker.

Java reference:

- `HarnessAgent.Builder.compaction(CompactionConfig)`
- `HarnessAgent.Builder.disableCompaction()`
- `CompactionMiddleware` performs flush/offload/summarize/replace.

### GAP-102 — Declared subagent `workspace_path` + `AGENTS.md`

Status: `DONE`

Previous Rust state:

- A declaration with `workspace_path` had its inline body cleared by the loader.
- `LeafFactory::declared_leaf` still used `decl.prompt`, so the child received only
  `SUBAGENT_CONTEXT_SECTION` and lost the workspace `AGENTS.md`.

Current Rust state:

- `resolve_declared_sys_prompt_base` reads `<workspace_path>/AGENTS.md` when present.
- Inline prompt remains the fallback when no workspace path is set.

Java reference:

- `HarnessAgentBuilderSupport.resolveDeclaredSysPromptBase`

Tests:

- `declared_workspace_reads_agents_md_for_the_system_prompt`
- `declared_inline_prompt_is_used_without_a_workspace`

---

## P1 — Subagent configuration inheritance

The Rust `SubagentBlueprint` now carries the parent configuration needed by the
materializer. The propagation rules mirror Java's current asymmetry:

- Both child kinds: plan mode, memory config, memory flags, `use_legacy_xml`.
- General-purpose only: workspace-context disable, environment memory, additional
  context files, max context tokens, tracing log, compaction, tool-result eviction.
- Declared children keep the parent defaults for the general-purpose-only fields,
  matching Java's current `buildDeclaredFactory`.

### GAP-103 — `disable_workspace_context`

Status: `DONE` (general-purpose)

- Java general-purpose propagates it.
- Rust general-purpose now propagates it.
- Declared children do not inherit it in either implementation.

### GAP-104 — `environment_memory`

Status: `DONE` (general-purpose)

- Java general-purpose propagates `capturedEnvMemory`.
- Rust general-purpose now propagates `environment_memory`.

### GAP-105 — `additional_context_files`

Status: `DONE` (general-purpose)

- Java general-purpose copies every `additionalContextFile`.
- Rust general-purpose now does the same.

### GAP-106 — `max_context_tokens`

Status: `DONE` (general-purpose)

- Java general-purpose propagates `maxContextTokens`.
- Rust general-purpose now propagates it instead of falling back to 8000.

### GAP-107 — `memory_config`

Status: `DONE` (both child kinds)

- Java propagates `MemoryConfig` to general-purpose and declared children.
- Rust `SubagentBlueprint` now carries it and the leaf builder applies it.

### GAP-108 — `agent_tracing_log_enabled`

Status: `DONE` (general-purpose)

- Java general-purpose propagates `capturedAgentTracingLogEnabled`.
- Rust general-purpose now applies the parent value.

### GAP-109 — Plan mode

Status: `DONE` (both child kinds)

- Java `configurePlanMode` propagates enabled / allow-shell / plan directory.
- Rust now applies the same three values to both child kinds.

### GAP-110 — Compaction

Status: `DONE` (general-purpose)

- Java general-purpose propagates `disableCompaction` / `compactionConfig`.
- Rust general-purpose now applies the parent `CompactionConfig` or disable flag.
- Declared children do not inherit compaction in either implementation.

### GAP-111 — Tool-result eviction

Status: `DONE` (general-purpose)

- Java general-purpose propagates `disableToolResultEviction` / config.
- Rust general-purpose now applies both.

### GAP-112 — `use_legacy_xml_workspace_context`

Status: `DONE` (propagation only)

- Java propagates the flag to both child kinds.
- Rust now does the same.
- The flag is still not consumed by the workspace-context implementation; see GAP-305.

### GAP-113 — At-path expansion inheritance

Status: `MATCHES_JAVA`

- Java does not propagate `disableAtPathExpansion` to general-purpose or declared
  children.
- Rust also does not propagate it.
- This is a deliberate match with Java's current behaviour. Revisit only if the
  desired product behaviour is "children inherit the parent's at-path switch".

### GAP-114 — Declared-subagent asymmetry

Status: `MATCHES_JAVA`

Java's `buildDeclaredFactory` does not propagate:

- `disableWorkspaceContext`
- compaction
- tool-result eviction
- additional context files
- max context tokens
- environment memory
- agent tracing log

Rust currently matches that asymmetry. If Java later makes declared children inherit
these values, update the Rust blueprint and this tracker together.

### GAP-115 — `disable_session_persistence`

Status: `TODO`

Java propagates `disableSessionPersistence` to both general-purpose and declared
children. Rust's blueprint currently carries `state_store` but not a dedicated
session-persistence disable flag. Needs a Rust equivalent before this can be closed.

---

## P2 — Framework-level gaps

### GAP-201 — Async system-prompt hook

Status: `DONE`

- `Middleware::on_system_prompt` is now async (`#[async_trait]`).
- `MiddlewareChain::apply_system_prompt` awaits the chain once per call.
- `WorkspaceContextMiddleware` builds its section directly in `on_system_prompt`;
  `on_agent` no longer needs to precompute it.

### GAP-202 — Middleware `order()` / priority

Status: `DONE`

- `Middleware::order()` defaults to 1.
- `MiddlewareChain` keeps a stable descending-order sort after every `add`.
- Higher priorities run earlier; ties retain registration order.
- Added `middleware_chain_sorts_by_descending_order`.

### GAP-203 — Full restored-state snapshot in pre-call hook

Status: `DONE`

- Added async `Middleware::on_pre_call(ctx, state, input_messages, system_prompt)`.
- `run_agent_loop` invokes it after the call-scoped state is bound and after the
  system-prompt transformer chain, before the prompt is frozen.
- The hook sees the restored history plus call input and may append prompt material.
- Added `pre_call_hook_sees_the_restored_state_snapshot`.

### GAP-204 — Filesystem type reflection

Status: `DONE`

- Added `Filesystem::as_any()` and `Filesystem::kind()`.
- Implemented `FilesystemKind` for local, overlay, sandbox, composite, remote, and
  baked wrappers (plus the extension sandbox adapter).
- The workspace prompt can now use concrete filesystem metadata.

### GAP-205 — Artifact delivery

Status: `DONE`

- Added `ArtifactDeliveryTarget`, `ArtifactDeliveryRequest`, and
  `ArtifactDeliveryResult` in `agent/artifact.rs`.
- Added `ArtifactDeliveryTool` (`deliver_artifact`) in `tool/artifact_delivery.rs`.
- Added `HarnessAgentBuilder::artifact_delivery_target(...)`; setting it registers the
  tool and enables the sandbox File Isolation Notice.
- Added tool and registration tests.

---

## P2 — Workspace prompt content

### GAP-301 — Effective workspace for local overlay

Status: `DONE`

- Local overlays now use `WorkspaceManager::resolve_runtime_data_path(rc, "")` as the
  effective workspace, matching Java.
- Detection uses `FilesystemKind::Overlay` plus the upper layer's `FilesystemKind`.

### GAP-302 — Filesystem-specific workspace paragraph

Status: `DONE`

Java-style branches are now rendered for:

- local overlay: project / workspace / path policy / overlay semantics
- sandbox: sandbox root / isolation wording
- composite / remote: distributed workspace wording
- legacy: single-line working directory

Additional allow-list roots are not yet available through Rust's `LocalFilesystem`
metadata; that is a remaining content-level refinement.

### GAP-303 — Artifact-delivery workspace notice

Status: `DONE`

- Sandbox prompts add the File Isolation Notice when an artifact delivery target is
  configured.
- Without a target, the prompt keeps the "no mechanism to move files" wording.

### GAP-304 — Session-context OS/date/temp formatting parity

Status: `TODO`

Java uses `os.name + os.version`, `EEEE MMM d, yyyy`, and `java.io.tmpdir`.
Rust uses `OS + ARCH`, `%A %b %-d, %Y`, and `std::env::temp_dir()`. This is cosmetic
but still a textual difference.

### GAP-305 — `use_legacy_xml_workspace_context` consumption

Status: `TODO`

Both Java and Rust currently store/propagate the flag but do not appear to consume it
in the workspace-context implementation. Decide whether to:

1. implement the legacy XML rendering path,
2. mark the flag deprecated, or
3. remove it.

---

## Verification log

| Check | Result |
|---|---|
| `cargo test -p agentscope-core --lib` | 289 passed |
| `cargo test -p agentscope-harness --lib -- --test-threads=1` | 437 passed |
| `cargo test -p agentscope-harness --test harness_context_wiring_test` | 4 passed |
| `cargo test -p agentscope-harness --test harness_agent_test` | 14 passed |
| ACP event / session resume integration subset | all passed |
| `cargo check --workspace` | passed |

Re-run these after each batch of changes and update the statuses above.
