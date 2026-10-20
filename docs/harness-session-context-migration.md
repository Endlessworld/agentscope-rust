# AI Session 改造迁移（Java `a2cbea5c3` → Rust）

> 依据：Java 提交 `a2cbea5c3` — *feat: redesign Harness context and unify Service sessions with JEV*（裁决 A1=A：跟随 Java 新 session/context 设计）。
> 本文只记录**本仓已落地**的部分、与 Java 的**已知差异**、以及**未落地**项；不重复 Java 侧语义描述。

## 1. 范围与规模

Java 该提交新增 4 个包（非测试源码约 5,640 行）：

| Java 包 | 文件数 | Rust 落点 |
|---|---|---|
| `core/session` | 26 | `crates/agentscope-core/src/session/` |
| `core/observation` | 4 | `crates/agentscope-core/src/observation/` |
| `harness/agent/context` | 16 | `crates/agentscope-harness/src/agent/context/` |
| `harness/agent/session` | 6 | `crates/agentscope-harness/src/agent/session/` |

## 2. 核心类型映射

### 2.1 `core/session` → `agentscope_core::session`

| Java | Rust | 说明 |
|---|---|---|
| `SessionKey` | `SessionKey` | `storage_path()` 同构：`agents/{agent}/sessions/{user}/{session}`，每段 `s_` + base64url(无填充) |
| `SessionEvent` | `SessionEvent` | 信封同构（`schemaVersion`/`seq`/`type`/`required`/`payloadJson`） |
| `SessionEventTypes` | `SessionEventTypes::BUILTIN` | 57 个内建类型，名称逐字对齐 |
| `SessionEventCodecRegistry` | `SessionEventCodecRegistry`（trait `SessionEventCodec`） | required-unknown 停止恢复 |
| `SessionHeader` / `SessionHistoryMode` | `SessionHeader` / `SessionHistoryMode` | `authority="event_log"`，版本不符即失败 |
| `AtomicSessionStorage` | `AtomicSessionStorage`（`async_trait`） | `compare_and_set`、`expected=0` 即 create-if-absent |
| `SessionLog` / `SessionLogStore` | `SessionLog` / `SessionLogStore`（均 `async_trait`） | 租约、seal、commit、scan、export cursor |
| `JournalSessionLog` | `JournalSessionLog` | 同构：单次 head CAS 发布整批、SHA-256 校验、>64KiB payload 落 blob |
| `InMemorySessionLogStore` | `InMemorySessionLogStore` + `MemorySessionStorage` | 测试/嵌入式用 |
| `SessionTurns` / `SessionViews` / `SessionProjection` | `SessionTurns` / `transcript()` / `SessionProjection` | 只读投影，不修复历史 |
| `SessionInteractions` | `pending_interactions()` + `continuation_turn()` | pending/resolved 折叠 |
| `SessionTranscriptExport` | `transcript_entries()` / `transcript_jsonl()` | 行格式对齐（`type/id/role/content/timestamp/toolName/toolInput/toolResult`） |
| `SessionRecorder` | `SessionRecorder` | 事件按受理顺序缓冲、单次提交、失败粘滞 |
| `SessionInbox` | `SessionInbox` / `Command` / `InboxSnapshot` | 独立 inbox 日志、幂等键（比较忽略 timestamp） |
| `SessionExecution` | `SessionExecution` | 受理校验、steer/inject 应用、close |
| `SessionStateAccess` | `state_access::{read,update,replace}` | 快照/受控变更/陈旧快照拒绝 |
| `SessionRecovery` | `recovery::{cancel_pending,reconcile}` | 人工核对，绝不自动重放 |
| `SessionMigration` | `migration::{import_baseline,export_snapshot,export_state,fork}` | `baseline_only` 覆盖，不伪造历史 |
| `SessionLogExporter` / `SessionExportSink` | 同名 | cursor 仅在 sink 接受后推进 |
| `SessionModels` / `SessionModelPolicy` | **未落地**（见 §5） | 依赖模型包装层 |

### 2.2 `core/observation` → `agentscope_core::observation`

| Java | Rust |
|---|---|
| `ActionObservation` / `Status` | `ActionObservation` / `ActionStatus`（6 态同名） |
| `ActionObserver` | `ActionObserver`（`async_trait`，`record()`） |
| `ActionObservations.observe` | `observation::observe`（`observe_action` 别名） |
| `ActionObservationException` | `Error::ActionObservation`（专用变体） |

观测事件 payload 与 Java 一致地**只含身份/时标/状态/错误类别**，不复制参数、原始输出或异常消息。

### 2.3 `harness/agent/context` → `agentscope_harness::agent::context`

| Java | Rust |
|---|---|
| `ContextItem` / `Placement` | `ContextItem` / `ContextItemPlacement` |
| `ContextBlock`（`runtime`/`reference`） | `ContextBlock`（同构 fluent） |
| `ContextRequest` / `ContextSource` / `SourceFailurePolicy` / `ContextSourceOptions` | 同名（默认 5s 超时 + `Fail`） |
| `ContextSources` | `ContextSources`（指令 + source 注册，id 前缀 `source/{id}/`） |
| `ContextPolicy` / `ContextTokenEstimator` | `ContextPolicy` / `TokenEstimator` + `ApproximateEstimator`（method 串 `mixed-text-2.5chars-media-2048-v1`） |
| `ContextSelectionPolicy` | `ContextSelectionPolicy` + `DefaultSelectionPolicy`（memory→knowledge→其它） |
| `ContextManifest` / `ContextManifest.Item` | `ContextManifest` / `ContextManifestItem`（仅元数据，无正文） |
| `ContextRenderer` | `render_context_blocks` / `escape` / `material_message` |
| `HarnessContextBuilder` | `ContextPipeline` + `ContextMiddleware` |
| `TaskContextOptions` | `TaskContextOptions`（同约束：proposals ⇒ requirements） |

## 3. 与 Java 的已知差异

1. **同步 → 异步**：Java `SessionLog` 是阻塞 SPI 并调度到 `boundedElastic`；Rust 直接用 `async_trait`，不再有调度跳。
2. **心跳**：Java `SessionRecorder` 有 1s 周期性 flush 心跳线程；Rust 改为调用方显式 `flush()`（`record()` = append+flush）。
3. **`RuntimeContext` 扩展槽**：Java 是 `put(key, value)` 字符串键 map；Rust 新增 `Extensions`（`&'static str` 键 + `Any` 值，**写时复制**语义，`extend()` 做覆盖式合并）。
4. **观测器异常**：Java `catch RuntimeException` + warn；Rust 用 `catch_unwind` 包裹 observer，panic 不影响请求。
5. **`ContextTokenEstimator`** 以 `EstimateInput{messages, tools}` 入参（Java 用 `ModelCallInput`），避免把 `Arc<dyn Model>` 带进估算层。
6. **`task_revision`** 固定写 `0`：Rust `TaskContextState` 尚无 `revision` 字段（见 §5）。
7. **`verification/result`** 只做内建类型校验，不回放进 state：Rust 无 `TaskVerification`/`TaskRequirement`/`EvidenceBinding`。

## 4. 接线点（生产路径，已验证）

- **观测**：`crates/agentscope-core/src/agent/react_agent.rs::acting_phase` 的**两处**工具调用（`Allow/Passthrough` 与 HITL 确认通过）均经 `observation::observe`。观测持久化失败抛 `Error::ActionObservation`，**不会**降级成可重试的 tool result。
- **执行级扩展**：`ReActAgentBuilder::extension/extension_arc` 挂载、`run_stream_with_extensions` / `stream_events_with_extensions` 按次**合并**（不是覆盖）进 `RuntimeContext`。
- **上下文编译**：`HarnessAgentBuilder::{context_source, instruction, context_policy}` 为**显式 opt-in**；注册了材料才在 `build()` 里加入 `ContextMiddleware`（挂 `OnModelCall`），未注册则完全保持原路径。
- **会话运行时**：`AgentSession` → `SessionInbox`（受理）→ `SessionRecorder`（事实）→ `ReActAgent`（执行）→ `SessionTurns`（投影）。

## 5. 未落地项（不是缺陷，是待办）

| # | 项 | 阻塞原因 |
|---|---|---|
| 1 | `SessionModels` / `SessionModelPolicy`（模型边界记录、辅助/回退调用） | 需要在 `Model` 包装层记录 `model/chunk`；当前 `Model` 无统一包装点 |
| 2 | `steer` **运行中**投递 | `ReActAgent` 无「推理步之间」钩子；现为**运行前一次性应用**（已在 `session/mod.rs` 文档注明） |
| 3 | `TaskContextProjection` / task revision / verification | Rust `TaskContextState` 只有 `tasks`；需先补 `revision`/`TaskRequirement`/`TaskVerification` |
| 4 | `ToolResultEviction` 在 context 编译器内的预算 pass | 已有独立 `ToolResultEvictionMiddleware`，未并入 `ContextPipeline` |
| 5 | `SqliteSessionStorage` | 已用 `WorkspaceSessionLogStore`（文件系统 + 版本文件 CAS）；SQLite 后端未做 |
| 6 | `StoreSessionStorage`（→ `DistributedStore`） | harness `DistributedStore` 字段不含 session log |
| 7 | `ContextModelCalls::auxiliary` | 需模型包装；同 #1 |
| 8 | `SessionExecution` 的 `continue_or_close` 运行中回环 | 同 #2 |

## 6. 验证

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | EXIT 0，0 error |
| `cargo test -p agentscope-core -j 2` | 217 lib passed + 全部集成测试 passed，0 failed |
| `cargo test -p agentscope-harness --lib -j 2` | 406 passed，0 failed |
| `cargo test -p agentscope-harness --test harness_context_wiring_test` | 3 passed |
| `cargo test -p agentscope-harness --test harness_session_test` | 9 passed |

回归过程中发现并修复的**真实缺陷**：`run_stream_with_extensions` 曾以调用方传入的 `Extensions` **覆盖**构建期扩展，导致 builder 注入的 recorder 被丢弃（由「走真实装配入口」的接线测试捕获）。修复方式为 `Extensions::extend` 覆盖式合并。
