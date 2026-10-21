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
6. **`task_revision`** 取自已提交的 `TaskContextState.revision()`（每步从 `core.tasks` 折叠进发布快照，见 §5 #3）。
7. **`verification/result`** 由 `harness::agent::verification::verify` 写入、由只读投影（`SessionProjection.verifications`）回放，不直接改 `AgentState`；Java 的 `StateStoreActionObserver` 回退存储与 `CustomEvent` 发射未迁移（见 §5 #3）。

## 4. 接线点（生产路径，已验证）

- **观测**：`crates/agentscope-core/src/agent/react_agent.rs::acting_phase` 的**两处**工具调用（`Allow/Passthrough` 与 HITL 确认通过）均经 `observation::observe`。观测持久化失败抛 `Error::ActionObservation`，**不会**降级成可重试的 tool result。
- **执行级扩展**：`ReActAgentBuilder::extension/extension_arc` 挂载、`run_stream_with_extensions` / `stream_events_with_extensions` 按次**合并**（不是覆盖）进 `RuntimeContext`。
- **上下文编译**：`HarnessAgentBuilder::{context_source, instruction, context_policy, task_context}` 为**显式 opt-in**；注册了材料**或**开启任一 task 投影（`TaskContextOptions::is_enabled()`）才在 `build()` 里加入 `ContextMiddleware`（挂 `OnModelCall`），否则完全保持原路径。
- **任务快照**：`react_agent.rs::reasoning_phase` 每步把共享任务袋（`core.tasks`，`todo_write` 就地修改）折叠进发布快照后才编译请求，因此投影与 manifest 的 `task_revision` 看到的是本步的实时任务状态（Java 靠可变 `AgentState` 达到同样效果）。
- **会话运行时**：`AgentSession` → `SessionInbox`（受理）→ `SessionRecorder`（事实）→ `ReActAgent`（执行）→ `SessionTurns`（投影）。

## 5. 未落地项（不是缺陷，是待办）

> 状态核对至 2026-10-21（本轮实施记录见 `.agents/context/state/handoff-2026-10-21.md`）。#1–#8 均已接线或落地，残留限制写在各行「残留」列。

| # | 项 | 状态 | 落地位置 / 残留 |
|---|---|---|---|
| 1 | `SessionModels` / `SessionModelPolicy`（模型边界记录、辅助/回退调用） | **已落地** | `core/src/session/models.rs`（`SessionModels::wrap`、`SessionModelPolicy`、`SessionModelPolicyHandle`）+ `SessionRecorder::prepare_model`（`request/prepared` + `model/dispatch`）；包装层逐 chunk 记 `model/chunk`、结束记 `model/end`。接线点：`react_agent.rs::run_reasoning`（`REASONING`）、harness `context/model_calls.rs::auxiliary`（`SUMMARY`）。残留：`GenerateOptions` 未实现 `Serialize`，`request/prepared` 只记非敏感子集；`model/retry` 仍无独立生产者（每次重试是新的 `model/dispatch`）；core 无 fallback model 配置，Java 的 `switchOnFirst` 回退未迁移 |
| 2 | `steer` **运行中**投递 | **已落地** | `react_agent.rs::reasoning_phase` 在 `maxIters` 检查之后、模型调用之前执行 `recorder.step(iter)` + `SessionExecution::before_step`（每步折叠 steer/inject）；`agent/session/session.rs::launch` 里重复的 `before_step` 已删除（Java 只有 `reasoning` 调用）。测试：`core/tests/session_settle_loop.rs`（settle 回环）+ `core/tests/session_step_boundary.rs`（每步一个 `step/start`、summarizing 不开步、运行前 steer 在第一步折叠且只应用一次） |
| 3 | `TaskContextProjection` / task revision / verification | **已落地** | `TaskContextState` 增 `revision/requirements/scope/contractVersion/subjectVersion/verifications`（`core/src/state/context.rs`）+ `TaskRequirement`（`state/requirement.rs`）+ 纯投影 `middleware/task_context_projection.rs` + `todo_write` 写 `context.state_key/state_revision/representation`。编译器 `ContextPipeline::prepare_with_task` 在每个请求内投影并写 manifest `task_revision`；快照经 `TASK_CONTEXT_STATE_CONTEXT_KEY` 随调用上下文传入（`reasoning_phase` 发布）。`verification/result` 生产者：`harness/src/agent/verification/mod.rs::verify`。harness 接线：`HarnessAgentBuilder::task_context` + `TaskContextOptions::is_enabled()` 门禁（只开投影也注册编译器）+ `reasoning_phase` 每步把 `core.tasks` 折叠进发布快照（否则 `todo_write` 的改动只在调用结束才回到 `AgentState`，编译器看到的恒为 revision 0）。测试：`crates/agentscope-harness/tests/harness_task_context_wiring_test.rs`（manifest revision 0/1/2 + 陈旧 receipt 降级 + 未 opt-in 时编译器不注册）。残留：Java 的 `StateStoreActionObserver` 回退存储、`CustomEvent` 发射与 `load` 未迁移；核心 `ActionObservation` 无 `evidenceBinding`，绑定改由 `SettledAction` 携带；不做 userId/sessionId 比对。仍未迁移：core `RequirementTools`（`task_requirement_propose`）与 `TaskReminderMiddleware` 的 requirements 提醒位 → `requirement_proposals` 目前只能由宿主直接调 `TaskContextState::propose` 使用，模型侧没有提议入口 |
| 4 | `ToolResultEviction` 在 context 编译器内的预算 pass | **已落地** | `ContextPipelineConfig.eviction` + `ContextPipeline::with_eviction`；`builder.rs` 在测量之前 offload 历史 tool result（追加 `tool_result_offload` transform）；`harness_agent.rs::build` 把同一份 fs/config 同时交给编译器与 `ToolResultEvictionMiddleware` |
| 5 | `SqliteSessionStorage` | **已落地** | `harness/src/agent/session/sqlite.rs`（`#[cfg(feature = "rusqlite")]`）：表 `session_objects(path,version,payload)`，`INSERT ... ON CONFLICT DO NOTHING` / `UPDATE ... WHERE version = ?` 实现 CAS。残留：未做 Java 的符号链接检查 |
| 6 | `StoreSessionStorage`（→ `DistributedStore`） | **已落地** | `harness/src/agent/session/store.rs` + `BaseStore::supports_atomic_session_storage()`（默认 `false`）；`InMemoryStore` 与 jdbc/mysql/postgresql/mongodb/redis/aistio 覆盖为 `true`（与 Java 声明一致），oss/cos 保持 `false`（Java 亦不声明） |
| 7 | `ContextModelCalls::auxiliary` | **已落地** | `harness/src/agent/context/model_calls.rs::auxiliary` / `auxiliary_text`：默认 policy、无 source、以 `ContextPurpose::Summary` 准备后再流式调用，因此产生 manifest；`memory/flush.rs`、`memory/compaction.rs` 已改走该入口。残留：compaction 侧没有 `RuntimeContext`，辅助调用不带 session 记录（Java 用 `Flux.deferContextual` 读取调用上下文） |
| 8 | `SessionExecution` 的 `continue_or_close` 运行中回环 | **已落地** | `react_agent.rs::run_agent_loop` 的 settle 循环（`session_continues` → `execute_iteration(cur_iter + 1)`）。测试：`core/tests/session_settle_loop.rs` 2 例；把 `session_continues` 强制改为 `Ok(false)` 时两例均 FAILED（已反向验证） |

## 6. 验证（时点：2026-10-21 工作树，含 §5 #3 的 harness 接线）

| 命令 | 结果 |
|---|---|
| `cargo build --workspace -j 2` | EXIT 0，0 error |
| `cargo test -p agentscope-core -j 2` | 280 lib passed + 全部集成测试 passed，0 failed |
| `cargo test -p agentscope-harness -j 2 --no-fail-fast` | 44 个 target 全部 passed（其中 `--lib` 430 passed），0 failed |
| `cargo test -p agentscope-harness --test harness_task_context_wiring_test` | 2 passed |
| `cargo test -p agentscope-harness --test harness_context_wiring_test` | 3 passed |
| `cargo test -p agentscope-harness --test harness_session_test` | 9 passed |
| `cargo test -p agentscope-core --test session_step_boundary` | 2 passed |
| `cargo test --workspace -j 2 --no-fail-fast` | 112 个 target / 3397 passed / 1 ignored；唯一失败 `agentscope-extensions-training::runner::tests::run_registry_allocates_incrementing_ids`（进程级 `OnceLock` 静态注册表竞争，既有 flake；`cargo test -p agentscope-extensions-training --lib` 单跑 64 passed / 0 failed） |

反向验证（回退实现→对应测试 FAILED→恢复）：①`session_continues` 强制 `Ok(false)` → settle 回环 2 例 FAILED；②去掉 `reasoning_phase` 的 `before_step` → 「运行前 steer 折叠」FAILED；③把 `iter >= max_iters` 改成 `>` → 「每步一个 step」FAILED（3≠2）；④删除 `reasoning_phase` 的 `core.tasks` 折叠 → 任务投影测试 FAILED（revision `[0,0,0]`）；⑤`build()` 去掉 `is_enabled()` 门禁 → 任务投影测试 FAILED（manifest 为空）。

回归过程中发现并修复的**真实缺陷**：`run_stream_with_extensions` 曾以调用方传入的 `Extensions` **覆盖**构建期扩展，导致 builder 注入的 recorder 被丢弃（由「走真实装配入口」的接线测试捕获）。修复方式为 `Extensions::extend` 覆盖式合并。
