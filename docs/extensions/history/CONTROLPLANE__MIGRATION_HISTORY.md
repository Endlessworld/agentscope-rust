# CONTROLPLANE 迁移执行历史（归档）

> 本文件是 [`../CONTROLPLANE__GAP_TRACKER.md`](../CONTROLPLANE__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 对齐基准变更：controlplane → controlplane（2026-10-22 静态复核） · 7. 静态复核记录（2026-10-22） · 7. P0 #13 修复记录（2026-10-22） · 8. 2026-10-28 执行记录：P1-1 / P1-2 / P1-3 / P2-2 · 9. 2026-10-31 收口：Java 增量提交评估 + 未关闭条目裁决 · 10. 执行记录（2026-10-31 二轮收口））。

## 6. 对齐基准变更：controlplane → controlplane（2026-10-22 静态复核）

### 6.1 改名事实（已用 git 复核，非推测）
- Java 侧 `agentscope-extensions-controlplane` 在 commit **`a2cbea5c3`**（2026-10-08，"feat: redesign Harness context and unify Service sessions with JEV" #3441）被**整体重命名**为 **`agentscope-extensions-controlplane`**。
  - 复核命令：`git -C E:\local-github\agentscope-java log --diff-filter=D -3 -- 'agentscope-extensions/agentscope-extensions-controlplane'` → 命中 `a2cbea5c3`（删除）与 `be41da404`（更早的引入）；`git show --stat --find-renames a2cbea5c3` → **54 个文件全部为 rename（`.../controlplane/ControlPlane.java}` 等），无新增/删除文件**。
  - Java HEAD（`ed8073259`）上 `agentscope-extensions/agentscope-extensions-controlplane` **目录不存在**，`agentscope-extensions-controlplane` 存在。
- **类名同步改名**：`ControlPlane`→`ControlPlane`、`ControlPlaneConfig`→`ControlPlaneConfig`、`ControlPlaneObserverMiddleware`→`ControlPlaneObserverMiddleware`；包名 `io.agentscope.extensions.controlplane`→`io.agentscope.extensions.controlplane`。**其余 36 个类保留原名**（含 `GrpcTransport` / `ContractProvider` / `ContractHttpServer` / `CollaborationClient` / `OrchestrationClient` / `ControlPlaneHttpClient` / `EventJournal` / `SessionBridge` / `FrameworkAdapter` / `ContextSnapshot` / `ContextTracker` / `SessionEvent` / `Inventory` / `MessagePage` / `AgentTaskAssignment` / 全部 `ControlPlane*` store 等）。
- **语义变更（不只是改名）**：`a2cbea5c3` 在改名之外引入了 **native session-log 提交链路**（`SessionBridge.onCommittedEvent` + `EventJournal.sourceWatermark` + `ControlPlaneObserverMiddleware` 的 `SessionExportSink` 分支 + `AgentScopeAdapter.publishCommitted`）、**`ControlPlaneBaseStore.supportsAtomicSessionStorage()`**、**env 变量改名**（`CONTROLPLANE_CONTROL_PLANE_HTTP`→`CONTROL_PLANE_HTTP`、`CONTROLPLANE_AGENT_NAME`→`CONTROL_PLANE_AGENT_NAME`、`CONTROLPLANE_NAMESPACE`→`CONTROL_PLANE_NAMESPACE`）、**默认 journal 目录改名**（`~/.agentscope/controlplane/event-journal`→`~/.agentscope/service-controlplane/event-journal`）。
- **规模变化**：旧 controlplane（`a2cbea5c3~1`）主 39 文件 9360 行 / 测 14 文件 2898 行 → 现 controlplane 主 39 文件 **9518** 行 / 测 14 文件 **3017** 行（净增 158 / 119 行，即上述语义新增）。
- **Rust 侧**：**没有 controlplane crate**。本 crate 仍叫 `agentscope-extensions-controlplane`，类型仍为 `ControlPlaneConfig` / `ControlPlaneBridge` / `ControlPlaneObserverMiddleware` / `ControlPlaneError`（仅 `ControlPlane*` store 家族与 `ControlPlaneHttpClient` 沿用 Java 同名）。
- **tracker 处理方式**：保留本文件为 CONTROLPLANE 模块单一事实来源（不改文件名、不重写正文结构），在顶部与 §2.0 标注改名事实并把「Java 清单」切换为 controlplane 现状；**§6.2 给出 controlplane 类清单 vs Rust 的缺口表**，作为本模块后续对齐的唯一待办基准。§1～§5 的历史交付记录保持原样（记录的是对**旧 controlplane** 的对齐成果，仍有效）。

### 6.2 controlplane 类清单 vs Rust 缺口表
> 「✅ 已对齐」= 该类在 Rust 侧有真实对应物且关键行为已覆盖；「⚠️ 部分」= 有对应物但缺新增语义或仅 trait 层；「❌ 缺」= Rust 侧无对应物。

| Java 类（controlplane） | 行数 | Rust 对应 | 状态 | 缺口说明 |
|---|---|---|---|---|
| `ControlPlane` | 69 | `lib.rs::instrument` | ✅ 已对齐 | 入口语义一致 |
| `ControlPlaneConfig` | 247 | `config::ControlPlaneConfig` | ✅ 已对齐 | 20 字段逐一对应，校验一致 |
| `SessionBridge` | 1207 | `bridge::ControlPlaneBridge` | 已完成（设计决定，不迁移：不逐方法名镜像） | Java `onCommittedEvent(source, nativeSeq, event)` 由 `BridgeReporter::publish_committed`（`bridge.rs:172`）+ `committed_event_sink()`（`bridge.rs:343`）/`export_sink_handle()`（`bridge.rs:351`）承接；功能链路已由 `tests/committed_export_e2e.rs` 4 个用例覆盖（§7 / §6.3 P0-1）。2026-10-31 收口：追加同名薄转发无调用方，故不迁移，依据见 §9.3 |
| `FrameworkAdapter` | 156 | `adapter::FrameworkAdapter` | ✅ 已对齐 | 13 capability 常量逐一对应 |
| `EventJournal` | 250 | `event_journal::EventJournal` | ✅ 已对齐 | `source_watermark`/`track_source`/`acknowledge` + 原子 rewrite（2026-10-28） |
| `model::SessionEvent` | 200 | `model::SessionEvent` | ✅ 已对齐 | 6 type + 4 role 常量一致 |
| `model::ContextSnapshot` | 364 | `model::ContextSnapshot` | ✅ 已对齐 | `hash()` 一致 |
| `model::ContextTracker` | 186 | `model::ContextTracker` | ✅ 已对齐 | 增量视图 + contextHash 一致 |
| `model::Inventory` | 116 | `model::Inventory` | ✅ 已对齐 | subagents/workspaces/health 一致 |
| `model::MessagePage` | 83 | `model::MessagePage` | ✅ 已对齐 | 分页一致 |
| `model::AgentTaskAssignment` | 42 | `model::AgentTaskAssignment` | ✅ 已对齐 | 13 字段一致（payload base64 编解码一致） |
| `store::ControlPlaneStores` | 189 | `store::ControlPlaneStores` | ✅ 已对齐 | env 键 `CONTROL_PLANE_*` 与 Java `fromEnv` 逐字一致（2026-10-28） |
| `store::ControlPlaneBaseStore` | 194 | `store::ControlPlaneBaseStore` | ✅ 已对齐 | 含 `supports_atomic_session_storage() = true`（Java 新增已对齐） |
| `store::ControlPlaneMessageBus` | 267 | `store::ControlPlaneMessageBus` | ✅ 已对齐 | |
| `store::ControlPlaneAsyncToolRegistry` | 216 | `store::ControlPlaneAsyncToolRegistry` | ✅ 已对齐 | |
| `store::ControlPlaneTaskRepository` | 799 | `store::ControlPlaneTaskRepository` | ✅ 已对齐 | `find_pending_deliveries`/`mark_delivered`/`is_delivered`/`shutdown` 均已实现 |
| `store::ControlPlaneSessionTurnGate` | 237 | `store::ControlPlaneSessionTurnGate` | ✅ 已对齐 | 20s 续租/60s TTL/30s 本地回退均已实现 |
| `store::ControlPlaneSandboxExecutionGuard` | 206 | `store::ControlPlaneSandboxExecutionGuard` | ✅ 已对齐 | |
| `store::ControlPlaneSnapshotSpec` | 135 | `store::ControlPlaneSnapshotSpec` | ✅ 已对齐 | |
| `transport::ControlPlaneHttpClient` | 184 | `transport::ControlPlaneHttpClient` | ✅ 已对齐 | `send`/`send_bytes`/`send_bytes_with_headers`/`send_for_bytes`（后者带回 `content_type`） |
| `transport::ContractProvider` | 89 | `transport::ContractProvider` | ✅ 已对齐 | 17 方法逐一对应 |
| `transport::HttpSelfRegistration` | 273 | `transport::HttpSelfRegistration` | ✅ 已对齐 | 注册+心跳+身份回调一致 |
| `transport::ContractHttpServer` | 333 | `transport::ContractHttpServer` | ✅ 已对齐 | 16 route path / 17 端点逐一对应 |
| `transport::GrpcTransport` | 404 | `transport::asdp` | 已完成（**wire 层设计决定，不迁移** + 语义层已落地） | **proto 契约存在（非“无契约”）**：`agentscope-service/service-controlplane/internal/asdp/asdp.proto`（`AgentDataPlaneService.Connect(stream Upstream) returns (stream Downstream)`，另有一份 `sdk/dsh/proto/asdp.proto`）；Java 由该文件生成 `io.agentscope.controlplane.proto.*`。不移植原因：需 `protoc` + `tonic`/`prost`（不在 workspace 依赖集）+ 构建期拷贝 service proto，且 bidi 流无法经 unary JSON 的 `ControlPlaneHttpClient` 表达。**语义层已于 2026-10-31 落地**：`transport::asdp` 实现 handshake、重连退避（1s 基/30s 帽/`min(attempt,5)`）、15s 心跳、ack 门控 outbox、下行四类命令分发；wire 经 `AsdpLink` seam 注入（tonic 实现归宿主）。依据见 §10 |
| `transport::CollaborationClient` | 502 | `transport::CollaborationClient` | ✅ 已对齐 | 29 个端点全部落地（含 `replanRun`/`signalRun`/`uploadArtifact`/`downloadArtifact`，2026-10-28） |
| `transport::OrchestrationClient` | 137 | `transport::OrchestrationClient` | ✅ 已对齐 | 19 方法逐一对应 |
| `adapter::AgentScopeAdapter` | 936 | `adapter::AgentScopeAdapter` | 已完成（设计决定，不迁移：不逐方法名镜像） | `publishCommitted` 转发由 `BridgeReporter::publish_committed`（`bridge.rs:172`）承接，adapter 经 `on_bridge_attached`（`agent_scope_adapter.rs:274`）拿到 reporter，不再重复暴露同名方法。依据见 §9.3 |
| `adapter::ControlPlaneObserverMiddleware` | 267 | `adapter::ControlPlaneObserverMiddleware` | 已完成（设计决定，不迁移：分支外置） | native session-log 分支由 `committed_export::ControlPlaneCommittedEventSink` 承接：`impl SessionExportSink for ControlPlaneCommittedEventSink`（`committed_export.rs:59`）+ `translate` 镜像 `publishNative` 白名单，经 core `SessionExportSinkHandle` 注入 recorder。分支外置是刻意的单一职责划分（Observer 只做旁路 tap，提交事实只走 sink）。依据见 §9.3 |
| `adapter::HarnessAgentTaskStarter` | 533 | `adapter::HarnessAgentTaskStarter` | ✅ 已对齐 | 12 单测含复现 Java 5 场景 |
| `adapter::HarnessWorkspaceAgentFactory` | 229 | `adapter::HarnessWorkspaceAgentFactory` | ✅ 已对齐 | |
| `adapter::AgentTaskOutcome` | 63 | `adapter::AgentTaskOutcome` | ✅ 已对齐 | |
| `adapter::AgentTaskOutcomeTool` | 70 | `adapter::AgentTaskOutcomeTool` | ✅ 已对齐 | |
| `adapter::AgentTaskToolContext` | 22 | `adapter::AgentTaskToolContext` | ✅ 已对齐 | |
| `adapter::AgentTaskCollaborationTool` | 110 | `adapter::AgentTaskCollaborationTool` | ✅ 已对齐 | `READ_ONLY_TOOLS`/`TERMINAL_TOOLS` 一致 |
| `adapter::AgentTaskStarter` | 27 | `adapter::support::AgentTaskStarter` | ✅ 已对齐 | trait |
| `adapter::AgentRuntimeSource` | 56 | `adapter::support::AgentRuntimeSource` + `adapter::TaskRepositoryRuntimeSource` | ✅ 已对齐（2026-10-31） | trait + **harness `TaskRepository` 宿主实现**：`list_subagent_tasks` → `list_tasks` + `subagent_task_json`；`cancel_subagent_task` → `cancel_task`。无法在 crate 内解析的能力（subagent/workspace 注册表、plan-mode handle 需绑定具体 `AgentState`）保留 Java 默认（空表 / `false`）；`setPlanMode` 等仍由宿主覆写。依据见 §10 |
| `adapter::SessionCompactor` | 56 | `adapter::support::SessionCompactor` + `adapter::HarnessSessionCompactor` | ✅ 已对齐（2026-10-31） | trait + **`ConversationCompactor` 宿主实现**：过滤 SYSTEM → `compact_if_needed`（`agent_id` + `session_id`）→ 回写 `state.context`，返回注入的摘要文本；`Model` 仍由宿主提供，无 model 时走确定性预处理（可离线测）。依据见 §10 |
| `adapter::SessionHistorySource` | 35 | `adapter::support::SessionHistorySource` + `adapter::StoreSessionHistorySource` | ✅ 已对齐（2026-10-31） | trait + **原生 session log 宿主实现**：`SessionLogStore::open(SessionKey, RuntimeContext)` → core `session::transcript()` → `MessageItem`（含已被压缩掉的历史）；无法构成 `SessionKey` 时回 `None` 让 adapter 回退 live state。依据见 §10 |
| `adapter::WorkspaceAgentFactory` | 29 | `adapter::support::WorkspaceAgentFactory` | ✅ 已对齐 | 由 `HarnessWorkspaceAgentFactory` 实现 |

**统计（2026-10-31 二轮收口后）**：39 个 Java 类 → **✅ 已对齐 38 / ⚠️ 部分 0 / ❌ 缺 1**；其中「已完成（设计决定，不迁移）」共 **4** 项：`GrpcTransport`（**wire 层**：protoc + tonic/prost + bidi 流；语义层已由 `transport::asdp` 承接）、`SessionBridge`/`AgentScopeAdapter`/`ControlPlaneObserverMiddleware`（功能已由 `committed_export.rs` + `BridgeReporter::publish_committed` 等价承接，不逐方法名镜像 / 分支外置）。三个宿主注入点 SPI（`AgentRuntimeSource`/`SessionCompactor`/`SessionHistorySource`）**已于 2026-10-31 提供默认实现**（`adapter::host_impls`）。逐条源码级依据见 **§9.3 / §10**。

### 6.3 新发现缺口清单

| 编号 | 缺口项 | Java 参考 | 优先级 | 状态 | 说明 |
|---|---|---|---|---|---|
| P0-1 | **native session-log 提交链路未迁移** | `SessionBridge.onCommittedEvent` / `EventJournal.sourceWatermark` / `ControlPlaneObserverMiddleware` 的 `SessionExportSink` 分支 / `AgentScopeAdapter.publishCommitted` | P0 | **已完成（2026-10-29 勘误）**：原记「待办」，实际已于 **2026-10-22** 落地并关闭；依据见 §7 / 用例 `committed_events_from_a_real_run_reach_the_journal_once`、`duplicate_native_watermark_exports_once`、`source_watermark_is_durable_across_bridge_reopen`、`no_sink_configured_is_a_noop`（128 lib + 4 e2e）。落地形态：`src/committed_export.rs`（`ControlPlaneCommittedEventSink` 实现 core `SessionExportSink`，`translate` 镜像 `publishNative` 白名单）+ `BridgeReporter::publish_committed`（镜像 `onCommittedEvent`/`publishCommitted`，按 per-source watermark 去重）+ `EventJournal::source_watermark` + core `SessionExportSinkHandle` / `SessionLogExporter::for_session` 注入点；**宿主接线仍显式**（`export_sink_handle()` 需宿主装进 builder/session context，§7.4）。以下为 2026-10-22 复核时的原始判断，保留备查：Java 在 `ReActAgent.sessionLogEnabled()` 时改走「核心 session log → 原生导出 sink → journal 强制落盘 → 按 source watermark 去重」的提交链路；Rust 侧 `agentscope-core` 已有 `SessionExportSink` / `SessionLogExporter`（`crates/agentscope-core/src/session/export.rs`），但**本 crate 完全没有引用**（`rg SessionExportSink crates/agentscope-extensions-controlplane` 无命中），且 `SessionLogExporter::new` 在 core 内**除测试外无生产调用方**（`rg SessionLogExporter::new crates/` 仅命中 `export.rs` 自身测试）。**已核实的 Rust 接入点**：`ReActAgent::session_log_enabled()`（`react_agent.rs:308`）、`HarnessAgent::session_log(context)`（`harness_agent.rs:214`）、`RuntimeContext::extension::<T>(key)` / `Extensions::insert_arc`（`runtime_context.rs:45/258`）、`SessionRecorder::open_with(log, turn_id, run_id, Option<Arc<SessionLogExporter>>)`（`recorder.rs:213`）——**后者是现成的 exporter 注入位**。后果：Rust 侧 Level-2 事件仍只来自 `on_agent`/`on_acting` 旁路 tap，无法消费核心已提交事件的原生流；与 Java 新基准不等价 |
| P1-1 | env 变量名未随重命名迁移 | `ControlPlaneStores.fromEnv` 用 `CONTROL_PLANE_HTTP` / `CONTROL_PLANE_AGENT_NAME` / `CONTROL_PLANE_NAMESPACE` | P1 | 已完成（2026-10-28） | 核对结论：Rust `from_env` 已使用同名键（无 `CONTROLPLANE_*` 遗留），本轮补 `from_lookup` 健壮点 + 2 个离线键名断言测试。依据：Java `ControlPlaneStores.java:65-70`（`requireEnv("CONTROL_PLANE_HTTP")` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` + `envOr("CONTROL_PLANE_NAMESPACE","default")`） |
| P1-2 | EventJournal 缺 source watermark + 选择性 ack | `EventJournal.sourceWatermark(String)` / `trackSource` / `acknowledge(Map<String,Integer>)` | P1 | 已完成（2026-10-28） | watermark/trackSource 已在 P0-1 落地（本次核对语义与 Java `sourceWatermarks.merge(..., Math::max)` + `__native__` 检查点一致）；本轮补 `acknowledge(&BTreeMap<String,i64>)`（空 map = no-op、先落序列检查点、只丢 `seq <= floor`、未知会话不误丢、临时文件 + rename 原子重写、失败时内存 backlog 不丢）+ `ControlPlaneBridge::acknowledge`。依据：Java `EventJournal.java:138-148`（acknowledge）、`:200-256`（persistSequences/rewrite 原子写）、`SessionBridge.java:611-639`（onEventAck） |
| P1-3 | `CollaborationClient` 缺 4 个端点 | `replanRun` / `signalRun` / `uploadArtifact` / `downloadArtifact` | P1 | 已完成（2026-10-28） | 4 端点逐一对照：`POST run/replan`、`POST run/signals/{name}`（`idempotencyKey` + `payload` 缺省 `{}`）、`POST /api/v1/artifacts/uploads`（multipart，`sourceTaskId`/`targetType`/`targetRef` + `file`）、`POST /api/v1/artifacts/{id}/download?taskId=`（二进制）。新增 `send_bytes_with_headers` 与 `BytesResponse.content_type`，下载返回 `ArtifactContent { content_type, bytes }`，不经 `String`。依据：Java `CollaborationClient.java:323-421` |
| P2-1 | crate 名与类型名未随 Java 重命名 | Java → `controlplane` | P2 | **已完成（2026-10-22 改名）** | 建议**保留 Rust 侧 `controlplane` 命名**（crate 名、类型名），仅在文档/注释标注「对应 Java `agentscope-extensions-controlplane`」；理由：Rust 侧改名会波及 crate 名、import 路径与全部下游引用，收益仅是对齐命名，且本模块 Rust 无 controlplane crate 的对外依赖方需求；2026-10-29 勘误：原记「建议保留 Rust 侧 controlplane 命名 / Rust 无 controlplane crate」，实际 crate 名、`ControlPlane*` 类型前缀、`CONTROL_PLANE_*` env、journal 目录、sink 名均已按汇总 §4.5 裁定 3 完成改名，本条不再适用 |
| P2-2 | 默认 journal 目录名未迁移 | Java `~/.agentscope/service-controlplane/event-journal` | P2 | 已完成（2026-10-28，已核） | Rust `event_journal.rs::default_journal_dir()` 已是 `~/.agentscope/service-controlplane/event-journal`（P0-1 时已改），本轮补断言测试。**Java 侧不一致已记录**：`ControlPlaneConfig.java:43` javadoc 写 `service-controlplane`，但 `EventJournal.java:80-88` 代码实际拼 `~/.agentscope/controlplane/event-journal`——属本地 outbox，不跨语言共享，按文档化约定（= 任务口径）保留 `service-controlplane`，是否回改为 Java 字面值待汇总方裁定 |
| P1-4 | env 契约未完全对齐 Java `requireEnv`/`envOr`（blank 未拒绝、未 trim） | `ControlPlaneStores::require_key` / `from_lookup` | P1 | 已完成（2026-10-31，§9.2） | Java `ControlPlaneStores.java:192-198` 的 `requireEnv` 对 `v == null \|\| v.isBlank()` 抛错并 `v.trim()`；`:200-206` 的 `envOr` 对 blank 回落 fallback 并 trim。Rust 原实现只判 `Option::is_none()` 且不 trim → `BUILDER_INTERNAL_TOKEN=""` 会构造出**空 token 客户端**（未认证调用，迟至 401 才暴露）。已改为 blank 视同缺失 + trim。依据与测试见 §9.2 |

## 7. 静态复核记录（2026-10-22）

### 7.1 复核方式
- **纯静态核对**：未运行 `cargo build` / `cargo test`（本会话为静态复核子任务），未修改任何 Rust 代码；仅读取源码、`rg` 符号检索、`git log/show/diff` 复核 Java 侧改名事实、PowerShell 统计行数/文件数/`#[test]` 数。
- **Java 侧基线**：`E:\local-github\agentscope-java` HEAD = `ed8073259`；改名提交 `a2cbea5c3`；旧版源码经 `git show a2cbea5c3~1:...` 导出到 `C:\Users\Administrator\AppData\Local\Temp\opencode\controlplane-old\` 供对照。
- **Rust 侧基线**：`crates/agentscope-extensions-controlplane` 工作区**干净**（`git status --porcelain` 对该 crate 无输出；`git diff --stat HEAD` 为空）—— **注：任务描述提到 `src/adapter/observer_middleware.rs` 为 `M`，实测当前工作区该文件与 HEAD 一致，未发现未提交改动**；本节一切结论以工作区实际内容为准。

### 7.2 实测统计

| 项 | 数值 | 采集方式 |
|---|---|---|
| Java controlplane 主 | **39 文件 / 9518 行** | `Get-ChildItem -Recurse -Filter *.java`（排除 `src\test\`）+ `Measure-Object -Line` |
| Java controlplane 测 | **14 文件 / 3017 行** | 同上（`src\test\`） |
| Java 旧 controlplane（`a2cbea5c3~1`） | 主 39 文件 9360 行 / 测 14 文件 2898 行 | `git show` 导出后统计 |
| Java controlplane 类总数 | **39**（根包 5 + adapter 13 + model 6 + store 8 + transport 7） | 分目录 `Get-ChildItem` 计数 |
| Rust src | **37 文件 / 10709 行** | `Get-ChildItem -Recurse -Filter *.rs` + `Measure-Object -Line` |
| Rust 测试数 | **125**（`#[test]` **84** + `#[tokio::test]` **41**） | 正则 `^\s*#\[(tokio::)?test\]` 逐文件计数 |
| Rust crate Cargo.toml | 24 行 | 文件统计 |

### 7.3 controlplane→controlplane 改名事实与影响（详见 §6.1）
- 改名提交 `a2cbea5c3`（2026-10-08）将 Java 模块整体重命名，`--find-renames` 确认 **54 个文件全部为 rename，无增删**；Java HEAD 上 controlplane 目录已不存在。
- 类名：`ControlPlane`/`ControlPlaneConfig`/`ControlPlaneObserverMiddleware` 三处改名，其余 36 个类保留原名（含 `GrpcTransport`/`ContractProvider`）。
- **除改名外还有 4 项语义变更**：native session-log 提交链路（新增）、`supportsAtomicSessionStorage()`（新增）、env 变量改名、默认 journal 目录改名。
- **对 Rust 的影响**：Rust 侧无 controlplane crate，`controlplane` 命名与 Java 旧名一致 → **Rust 当前实现对齐的是「旧 controlplane 语义」**；相对现行 controlplane 基准，存在 §6.3 的 P0-1 / P1-1 / P1-2 / P1-3 / P2-2 共 5 项实质缺口。

### 7.4 controlplane 对齐缺口表
见 **§6.2**（39 类逐项对照：✅ 已对齐 29 / ⚠️ 部分 9 / ❌ 缺 1）。

### 7.5 新发现缺口
见 **§6.3**（P0-1 native session-log 链路 / P1-1 env 变量 / P1-2 EventJournal watermark+选择性 ack / P1-3 CollaborationClient 4 端点 / P2-1 命名决策 / P2-2 journal 目录）。

### 7.6 勘误列表（原文 → 实际）

| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | 顶部路径 | `agentscope-extensions/agentscope-extensions-controlplane/`（作为现行源码路径） | 该目录在 Java HEAD 已**不存在**；现行为 `agentscope-extensions/agentscope-extensions-controlplane/` |
| 2 | 顶部子模块 | 主包 `io.agentscope.extensions.controlplane` | 主包 `io.agentscope.extensions.controlplane` |
| 3 | §1 概览 | 「Java 模块 `agentscope-extensions-controlplane`」 | 「`agentscope-extensions-controlplane` → **`agentscope-extensions-controlplane`**」 |
| 4 | §1 概览 | 「Java 规模（初测）主 37 文件 ~9352 行 / 测 16 文件 ~2841 行（共 53 .java）」 | **主 39 文件 9518 行 / 测 14 文件 3017 行**（共 53 .java）；旧 controlplane 亦为 39/14 文件、9360/2898 行。原「37/16」为早期初测值，已过时 |
| 5 | §1 概览 | 「根包 6 类」 | 根包 **5 类**（`ControlPlane`/`ControlPlaneConfig`/`SessionBridge`/`FrameworkAdapter`/`EventJournal`） |
| 6 | §1 概览 | `cargo test` **106/106 全绿** | §4.1 已自述后续为 **125 passed / 0 failed**（2026-10-08）；106 为更早基线 |
| 7 | §2.0 标题 | 「源文件清单（53 个 .java：主 37 / 测 16）」 | 「主 **39** / 测 **14**」 |
| 8 | §2.0 `SessionBridge` | `(1177L)` | 现 controlplane 为 **1207L**（旧 controlplane 1177L） |
| 9 | §2.0 `AgentScopeAdapter` | `(928L)` | 现 controlplane 为 **936L**（旧 controlplane 928L） |
| 10 | §2.0 `ContractHttpServer` | 「`src/transport/contract_http_server.rs`（**27KB**）」 | 实测 **700 行**（≈23KB）；「27KB」为旧快照值 |
| 11 | §2.1 依赖映射 | 契约 HTTP 服务 → 「**待办**（需 HTTP server 依赖）」 | 已完成：axum 落地，`ContractHttpServer` 16 route path / 17 端点齐全（§3.2 已记 ✅，本行滞后） |
| 12 | §2.0 `ControlPlaneConfig` | 「20 字段」 | **正确**（实测 Java `ControlPlaneConfig` 20 字段、Rust `ControlPlaneConfig` 20 字段，一一对应） |
| 13 | §2.0 类名 | 直接用旧 controlplane 类名 | 已补注现行 controlplane 新名（`ControlPlane`/`ControlPlaneConfig`/`ControlPlaneObserverMiddleware`） |
| 14 | §4.1.2 第 3 条 | 行内含 **1 个 U+FFFD 替换字符**（`本次已补 Rust getter` 后的顿号位置），自 commit `2816e4a` 起即损坏 | 本次复核**已修复**为「，」（全仓库其余 tracker 无同类损坏） |
| 15 | §4.2 第 1 条 | 「原硬约束为『禁改 core』」 | 与 §4.1.1/§4.1.3 已记录的「经用户明确授权改 core」自相矛盾；本次复核**未复验 core 改动是否仍存在**（`ReActAgent::sys_prompt`/`max_iters` getter 仍在 `crates/agentscope-core/src/agent/react_agent.rs`），建议汇总方统一口径 |

### 7.7 复核结论
- 本模块 **P0 主链路与 P1 主干对「旧 controlplane」基准已实质对齐**（模型/transport/store/adapter/bridge 均已落地，Rust 侧符号逐个 grep 存在且行为覆盖，125 个测试）。
- 相对 **现行 controlplane 基准**，新增 **1 项 P0**（native session-log 提交链路，Rust core 已有 `SessionExportSink` 但本 crate 未接）与 **3 项 P1**（env 变量名、EventJournal watermark+选择性 ack、CollaborationClient 4 端点），均可在本 crate 内增量补齐，**不依赖 ASDP proto 工具链**。
- 建议状态：**进行中**（保持原状态值），因 P0-1 属新基准下的核心链路缺口；`GrpcTransport` 维持「设计决定（不迁移）」。

## 7. P0 #13 修复记录（2026-10-22）

> 本轮仅接线 §6.3 的 **P0-1（native session-log 提交链路）**；未做 crate 改名 / 目录重命名，未改根 `Cargo.toml`，未触碰其他 crate 的业务代码。

### 7.1 改动清单

**controlplane crate（主改动）**
| 文件 | 改动 |
|---|---|
| `src/committed_export.rs`（新增） | `ControlPlaneCommittedEventSink` 实现 core 的 `SessionExportSink`：把已提交原生 fact 映射为 Level-2 事件（`message/user`/`message/assistant` → `message`，`tool/requested` → `tool_call`，`tool/result` → `tool_result`，`context/replaced` → `compaction`），写入 `native_source`/`native_seq`/`native_event_id`/`execution_run_id` envelope，再交给 `BridgeReporter::publish_committed`。sink 名 `controlplane-native-v1`。 |
| `src/event_journal.rs` | 新增 `source_watermarks` + `source_watermark(source)` + `track_source`；append 时从 `framework_meta` 的 `native_source`/`native_seq` 取最大值；`.sequences` 检查点以 `__native__<source>\t<seq>` 持久化，重启后仍能去重。 |
| `src/bridge.rs` | 新增 `BridgeState::publish_committed`（`watermark >= native_seq` 直接丢弃，否则分配会话 seq → journal fsync → tracker）、`BridgeReporter::publish_committed`、`ControlPlaneBridge::committed_event_sink()` / `export_sink_handle()` / `pending_event_count()` / `pending_events()` / `source_watermark()`。 |
| `src/lib.rs` | 导出 `committed_export`、`ControlPlaneCommittedEventSink`、`SINK_NAME`。 |
| `tests/committed_export_e2e.rs`（新增） | 4 条端到端/回归用例（见 §7.3）。 |

**core / harness（最小增量接线，默认关闭）**
| 文件 | 改动 |
|---|---|
| `crates/agentscope-core/src/session/export.rs` | 新增 `SESSION_EXPORT_SINK_CONTEXT_KEY`（对齐 Java `SessionExportSink.CONTEXT_KEY`）、`SessionExportSinkHandle`（让 `Arc<dyn SessionExportSink>` 能进 typed `Extensions`）、`SessionExportSink::accept_for` 默认方法、`SessionLogExporter::for_session`。未配置 sink 时 `accept`/`drain` 行为不变。 |
| `crates/agentscope-core/src/session/recorder.rs` | 新增 `SessionRecorder::open_with_exporter`；`open` 委托给它并传 `None`，行为逐字节不变。 |
| `crates/agentscope-core/src/agent/react_agent.rs` | `open_owned_recorder` 读取 run extensions 或 builder extensions 里的 `SessionExportSinkHandle`；存在则用 `for_session(..., session_id)` 构造 exporter，否则走原路径。 |
| `crates/agentscope-harness/src/agent/session/session.rs` | `launch` 从 session `RuntimeContext` 读取同一个 handle；存在则给 recorder 配 exporter，否则原路径。 |

**为什么必须动 core/harness**：`SessionRecorder` 的 `exporter: Option<Arc<SessionLogExporter>>` 注入位已存在（§6.3 已确认），但生产代码创建 recorder 的两处（`react_agent::open_owned_recorder`、`harness::launch`）都没有把宿主提供的 sink 传进去；Rust 的 `Middleware::on_agent` 只拿 `&RuntimeContext`，无法像 Java 那样在执行期 `ctx.put(...)`。因此只做“读取可选 handle → 构造 exporter”的增量改动，未配置时零行为变化。

### 7.2 与 Java 语义对照

| Java（controlplane） | Rust（本轮） | 说明 |
|---|---|---|
| `SessionExportSink.CONTEXT_KEY` + `ControlPlaneObserverMiddleware` 安装匿名 sink | `SESSION_EXPORT_SINK_CONTEXT_KEY` + `SessionExportSinkHandle`，宿主通过 builder `.extension(...)` 或 session `RuntimeContext` 安装 | 注入点语义一致；Rust 无 `ctx.put`，改用已有 builder/context 注入位 |
| `ReActAgent.acquireCallExecution` → `new SessionRecorder(..., sink)` | `open_owned_recorder` → `SessionRecorder::open_with_exporter(..., exporter)` | 提交点相同（recorder flush） |
| `SessionRecorder` 每次 commit 后 `SessionLogExporter.drain()` | `recorder.flush()` 成功后 `exporter.drain()`（既有代码，本轮未改） | 逐条已提交事件、cursor 成功后推进 |
| `publishNative` 类型映射（message/tool/compaction） | `committed_export::translate` 同一白名单映射 | `run/*`、`step/*`、`state/checkpoint` 等仍只留在 native |
| `AgentScopeAdapter.publishCommitted` → `SessionBridge.onCommittedEvent` | `BridgeReporter::publish_committed` → `BridgeState::publish_committed` | — |
| `EventJournal.sourceWatermark` + `trackSource` + `__native__` 持久化 | `EventJournal::source_watermark` + `track_source` + 同前缀检查点 | 缺失 ACK 重投同一 fact 不变更 journal |
| `onCommittedEvent` 在 journal 不可用时抛异常 | 无 journal 时 `publish_committed` 返回 `false`（不抛） | 遵循本模块“旁路失败不阻塞会话”的原则；宿主未开 `enable_events` 时安全 no-op |
| `SessionExportSink` 每个 context 绑定一个 sessionId | `accept_for(session_id, event)` 默认方法 + `for_session` exporter | 一个 agent 级 sink 可跨会话；每个 session 独立水位 |

### 7.3 测试与结果

命令（Windows PowerShell 5.1，工作区根目录）：

```
cargo test -p agentscope-extensions-controlplane        # 128 lib + 4 e2e，全部通过
cargo check -p agentscope-core -p agentscope-harness  # 通过
cargo test -p agentscope-core --lib               # 291 passed，全部通过
```

`crates/agentscope-extensions-controlplane/tests/committed_export_e2e.rs`：

| 用例 | 改前为何失败 |
|---|---|
| `committed_events_from_a_real_run_reach_the_journal_once` | `open_owned_recorder` 从不读取 sink，`SessionLogExporter::new` 无生产调用方，真实 agent 跑完后 bridge journal 为空、`source_watermark("s1") == 0`。 |
| `duplicate_native_watermark_exports_once` | `EventJournal` 没有 source watermark，同一 `native_seq` 每 accept 一次就 append 一次，pending 会变成 2/3。 |
| `source_watermark_is_durable_across_bridge_reopen` | 水位不落盘；重开 bridge 后水位归 0，重投同一 fact 会再 append。 |
| `no_sink_configured_is_a_noop` | 该场景改前也通过；作为“未配置 sink 行为完全不变”的回归护栏保留。 |

新增 core 单测 `session::export::tests::for_session_delivers_the_logical_session_id`：改前 exporter 不携带 session id，sink 无法按会话水位去重。

### 7.4 未完成 / 遗留

- **宿主接线仍显式**：`instrument(target, cfg, adapter)` 接收已构建好的 agent，无法再改其 builder extensions。宿主需先 `ControlPlaneBridge::new(cfg).start()`，把 `bridge.export_sink_handle()` 装进 builder/session context，再 `bridge.attach(...)`；或使用 `stream_events_with_extensions` 的 per-run extensions。未做 core agent 的可变注入 API（避免扩大 core 改动面）。
- harness 路径已接线，但本轮只做 `cargo check` 编译验证；端到端用例走 core `ReActAgent`（离线 fake model）。
- ~~§6.3 的 P1-1（env 变量名）、P1-2 选择性 ACK（本轮只补 source watermark，`ack_all` 仍为全量）、P1-3、P2-2 未在本轮处理。~~ → **已于 2026-10-28 全部关闭，见 §8**（P1-2 的选择性 `acknowledge` 也在 §8 补齐）。
- crate/类型命名仍为 `controlplane`（按 §6.3 P2-1 的设计决定）。

## 8. 2026-10-28 执行记录：P1-1 / P1-2 / P1-3 / P2-2

> 本轮只改 `crates/agentscope-extensions-controlplane/**` 与本 tracker；未触根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**`、其它 crate；未加依赖；未删测试、未 `#[ignore]`。

### 8.1 Java 依据（HEAD `16cb024ae`，逐行核对）

| 项 | Java 位置 | 内容（实测） |
|---|---|---|
| P1-1 | `store/ControlPlaneStores.java:65-70` | `requireEnv("CONTROL_PLANE_HTTP")` → `BUILDER_INTERNAL_TOKEN` → `CONTROL_PLANE_AGENT_NAME`，`envOr("CONTROL_PLANE_NAMESPACE", "default")`；**无 `CONTROLPLANE_*` 旧键兼容分支** |
| P1-2 | `EventJournal.java:61-63` / `:65-78` / `:138-148` / `:200-256` | `sourceWatermark` 默认 0；`trackSource` 取 `native_source`/`native_seq` 并 `merge(..., Math::max)`；`acknowledge` 空 map 直接 return → 落检查点 → 删 `seq <= committed.getOrDefault(sid, Integer.MIN_VALUE)` → `rewrite()`；`persistSequences`/`rewrite` 均为 `.tmp` + `ATOMIC_MOVE` |
| P1-2（调用方） | `SessionBridge.java:611-639` | `onEventAck` 按 session `merge(..., Math::max)` 折叠 committed，失败则保留批次重放（控制面对 `(session, seq)` 幂等） |
| P1-3 | `transport/CollaborationClient.java:323-325`（replan）、`:327-340`（signal）、`:347-395`（upload）、`:397-421`（download） | `POST run/replan`；`POST run/signals/{path(name)}` body `{idempotencyKey, payload: payload==null?{}:payload}`；`POST /api/v1/artifacts/uploads` multipart（`sourceTaskId` / `targetType` 缺省 `agent-task` / `targetRef` 缺省 taskId / `file` 文件名去引号换行 + 缺省 `application/octet-stream`）；`POST /api/v1/artifacts/{path(id)}/download?taskId={path(taskId)}` 走 `sendForBytes` |
| P2-2 | `EventJournal.java:80-88` vs `ControlPlaneConfig.java:43` | **Java 自身不一致**：代码拼 `~/.agentscope/controlplane/event-journal`，javadoc 写 `~/.agentscope/service-controlplane/event-journal`；Rust 取后者（文档化约定 + 任务口径），已在 §6.3 P2-2 标注待裁定 |

### 8.2 改动清单

| 文件 | 类型 | 内容 |
|---|---|---|
| `src/event_journal.rs` | 改 | 新增 `acknowledge(&BTreeMap<String, i64>) -> ControlPlaneResult<()>`：空 map 立即 no-op；**先** `persist_sequences()` 再写盘；`retain` 仅保留 `seq > floor` 与 map 中不存在的会话；`encode_records` + `write_atomic`（`<name>.tmp` → fsync → rename）替换原 `std::fs::write`，`rewrite_pending`/`persist_sequences` 一并原子化；**失败时内存 backlog 不变**（先写盘成功才 `self.pending = survivors`，比 Java 的「先改内存再 rewrite」更保守）；+7 测试 |
| `src/bridge.rs` | 改 | 新增 `ControlPlaneBridge::acknowledge(&BTreeMap<String, i64>)`（无 journal → no-op），对应 `SessionBridge.onEventAck` 的 ack 落盘入口 |
| `src/store/control_plane_stores.rs` | 改 | 新增 `ENV_CONTROL_PLANE_HTTP` / `ENV_BUILDER_INTERNAL_TOKEN` / `ENV_CONTROL_PLANE_AGENT_NAME` / `ENV_CONTROL_PLANE_NAMESPACE` 常量与 `from_lookup(FnMut(&str) -> Option<String>)` 健壮点（`from_env` 委托给它），删掉原 `require_env`；+2 测试（键名+顺序、缺省 namespace / 缺键报错） |
| `src/transport/control_plane_http_client.rs` | 改 | `BytesResponse.content_type: Option<String>`（`send_for_bytes` 读取 `Content-Type`）；新增 `send_bytes_with_headers`（对应 Java 5 参 `sendBytes`），`send_bytes` 委托之 |
| `src/transport/collaboration_client.rs` | 改 | 新增 `ArtifactContent { content_type, bytes }` + `replan_run` / `signal_run` / `upload_artifact` / `download_artifact`；`multipart_artifact_body` + `write_form_field`（边界 `controlplane-{uuid}`，文件名去 `"`/CR/LF）；`parse_json_body` 抽出并给 `send` 复用；`DEFAULT_ARTIFACT_CONTENT_TYPE`；+7 测试（含 loopback axum stub） |
| `src/transport/mod.rs` | 改 | re-export `ArtifactContent` / `DEFAULT_ARTIFACT_CONTENT_TYPE` |
| `docs/extensions/CONTROLPLANE__GAP_TRACKER.md` | 改 | §2.0 / §6.2 / §6.3 状态 + §7.4 收尾 + 本 §8 |

### 8.3 与 Java 的显式差异（非缺口）

1. **序列号类型统一 `i64`**：Java `Map<String,Integer>`（session seq）/ `long`（source watermark）双类型，Rust 统一 `i64`，因此 `acknowledge` 入参为 `BTreeMap<String, i64>`（任务文本写的是 `u64`；取 `i64` 以与 `SessionEvent.seq` / `latest_sequences` / `source_watermark` 保持一致，无需在有符号/无符号间转换）。
2. **`source_watermark` 返回 `i64`（0 = 无）而非 `Option<u64>`**：这是 P0-1 已落地的实现，语义与 Java `sourceWatermarks.getOrDefault(source, 0L)` 逐字等价（`bridge.source_watermark` 亦为 `i64`），本轮核对后保留未改。
3. **下载带回 `Content-Type`**：Java `BytesResponse(status, byte[])` 丢弃该头，Rust 额外返回 `content_type`（缺省 `application/octet-stream`），避免调用方猜媒体类型；Java 的 `downloadArtifact` 只返 `byte[]`，故这是**超集**。
4. **`signal_run` 的 `payload`**：`None` 与 `Some(Null)` 都发 `{}`（Java `Map.of` 本身不接受 null，等价）。
5. **`acknowledge` 失败语义**：Java 先改内存再 `rewrite`（失败则内存已丢、仅靠重启从旧文件回放）；Rust 先写盘成功再换内存，失败时本进程内也可继续重放。

### 8.4 测试与验证

```
cargo build -p agentscope-extensions-controlplane                 # EXIT=0（仅既有 3 条告警）
cargo test  -p agentscope-extensions-controlplane                 # EXIT=0  144 lib + 4 e2e passed / 0 failed / 0 ignored
cargo fmt   -p agentscope-extensions-controlplane -- --check      # 已 fmt（仅本 crate 6 个文件变更）
```

新增 16 个用例（128 → 144）：

| 模块 | 用例 |
|---|---|
| `event_journal` | `acknowledge_drops_only_records_at_or_below_the_session_floor`（多会话交错 + 部分 ack）、`acknowledged_records_do_not_replay_after_reopen`（部分 ack 后重启重放，再 ack 同一水位不误删）、`acknowledge_ignores_sessions_absent_from_the_committed_map`（未知 source/session 不丢）、`acknowledge_with_an_empty_map_is_a_noop`（空 map 不改 backlog、不改文件、不落 `.tmp`）、`acknowledge_rewrites_the_journal_atomically`（无残留 `.tmp`，落盘字节 = 存活记录编码）、`acknowledge_keeps_the_backlog_when_the_rewrite_fails`（`<name>.tmp` 被目录占位 → Err 且 pending 不变）、`default_journal_dir_matches_the_control_plane_layout` |
| `store::control_plane_stores` | `from_lookup_reads_the_renamed_control_plane_env_keys`（键名与求值顺序逐字断言）、`from_lookup_defaults_the_namespace_and_reports_missing_keys` |
| `transport::collaboration_client` | `replan_run_posts_the_node_to_run_replan`（方法/路径含 `%20`/两个头/体）、`signal_run_posts_the_idempotency_key_and_defaults_a_missing_payload`（`run/signals/pause%20now`）、`upload_artifact_posts_a_multipart_form_with_binary_content`（boundary 前缀、3 个 form 字段、文件名去引号换行、`0xFF/0xFE` 字节原样）、`upload_artifact_maps_error_statuses`（409 → `artifact.upload failed: HTTP 409 ...`）、`download_artifact_round_trips_non_utf8_bytes_with_the_content_type`（`[0xFF,0xFE,0x00,0x80,0x7F]` 完整往返 + `?taskId=t+1` + 无 body）、`download_artifact_defaults_the_content_type_and_surfaces_errors`（404 + body 透传）、`download_artifact_percent_encodes_the_artifact_id` |

全部离线：用 `axum` + `TcpListener::bind((127.0.0.1, 0))` 起 loopback stub（与 `ContractHttpServer` 测试同法），不触真实控制面。

### 8.5 反向验证（回退实现 → 必须 FAILED → 恢复）

| # | 回退 | 结果 |
|---|---|---|
| 1 | `acknowledge` 的 `None => true` 改为 `None => false`（把未提交会话也丢） | `acknowledge` 过滤后 **3 failed / 3 passed**：`acknowledge_ignores_sessions_absent_from_the_committed_map`、`acknowledge_drops_only_records_at_or_below_the_session_floor`、`acknowledged_records_do_not_replay_after_reopen` |
| 2 | `download_artifact` 改用 `String::from_utf8_lossy(...).into_bytes()` | `download_artifact_round_trips_non_utf8_bytes_with_the_content_type` **FAILED**（`left: [239,191,189,...]` vs `right: [255,254,0,128,127]`） |
| 3 | `ENV_CONTROL_PLANE_HTTP` 改回 `"CONTROLPLANE_CONTROL_PLANE_HTTP"` | 2 个 `from_lookup_*` **FAILED** |

恢复后重跑全量：**144 + 4 passed / 0 failed / EXIT=0**。

### 8.6 需裁定 / 遗留

1. **P2-2 路径字面值**：Java `EventJournal.java:80-88` 代码为 `~/.agentscope/controlplane/...`，javadoc（`ControlPlaneConfig.java:43`）为 `service-controlplane`；Rust 采用后者。若要与 Java 字节级路径一致，需回改并同步本 tracker §6.1/§6.3。
2. **`acknowledge` 类型**：任务文本为 `BTreeMap<String, u64>`，实现取 `i64`（理由见 §8.3）。如需 `u64` 公开 API，需同时改 `SessionEvent.seq` 与 `source_watermark` 的符号类型。
3. §6.3 **P0-1 的尾部**（`SessionBridge`/`AgentScopeAdapter`/`ControlPlaneObserverMiddleware`）与 `GrpcTransport`：原判 ⚠️/❌，**2026-10-31 已逐条改判为「已完成（设计决定，不迁移）」**，见 §6.2 与 §9.3。
4. 本轮工作区中其它 crate 的改动（core/jdbc/higress/judge/mem/rag/redis/sandbox/studio 等）**非本会话产生**（并发会话），未触碰。

## 9. 2026-10-31 收口：Java 增量提交评估 + 未关闭条目裁决

> 本轮改动仅限 `crates/agentscope-extensions-controlplane/src/store/control_plane_stores.rs` 与本 tracker。按主智能体「统一验证」约定，**本轮未运行 cargo**（构建/测试由主智能体统一执行），故下方测试数为「预期」而非实测。

### 9.1 Java 增量提交评估（Java HEAD `1524b3191`）

| Java 提交 | 所在包 / 类 | 本 crate 对应物 | 裁决 |
|---|---|---|---|
| `3eb1a33c4` fix(service): keep namespaced owner ids out of the agent id validation | `agentscope-service/service-dataplane` → `io.agentscope.builder.web.catalog.HarnessAgentBuildService#sessionLogAgentId` | **无** | 已完成（设计决定，不迁移） |
| `a543f993c` fix(dataagent): remove the public JWT fallback secret | `agentscope-examples/agents/agentscope-dataagent` → `io.agentscope.dataagent.web.auth.JwtService` + `application.yml` | **无** | 已完成（设计决定，不迁移） |

**`3eb1a33c4` 的依据（源码级）**
- 提交内容：`sessionLogAgentId` 改为把 `ownerId` / `agentId` / `session.id` 三段各自 `[^A-Za-z0-9._-] → -` 后再拼 `DP_AGENT_PREFIX`；原因是拼接结果被交给 `HarnessAgent.Builder#agentId`，而 `AgentBase#normalizeAgentId` **拒绝**含 `:` 的 id（namespaced owner id `namespace:default` → `Invalid agentId` → 受管会话整轮不跑）。
- Rust 侧**无该组装点**：`rg -n "dpa-|DP_AGENT_PREFIX|session_log_agent_id" crates/agentscope-extensions-controlplane` **零命中**；`crates/` 下 25 个 crate 中**无 service-dataplane / builder 层**（该提交所在包未迁移 Rust）。
- 且该**失败模式在 Rust 不存在**：`agentscope_core::agent::runtime_context::normalize_agent_id`（`runtime_context.rs:136-146`）对非法字符是**归一**（→ `_`）而不是抛错；唯一生产调用链为 `ReActAgent` 的 `with_agent_id`（`react_agent.rs:752` → `runtime_context.rs:210`）。故 `:` 只会被改写为 `_`，不会中断会话。
- 后续若 Rust 侧新增 service-dataplane 层并组装 session-log agent id，应「分段 sanitize 后再拼」并复用 core `normalize_agent_id`；本 crate 现在无需改动（无投机性代码）。

**`a543f993c` 的依据（源码级）**
- 提交内容：删掉 `dataagent.jwt.secret` 的公开默认值（`dataagent-default-dev-secret-change-in-production-32chars`），要求 ≥32 字节的部署专属密钥，并拒绝 blank / 短密钥 / 旧默认值。
- 本 crate **无 JWT 面**：`rg -in "jwt|hmac|hs256|dev-secret|change-in-production" crates/agentscope-extensions-controlplane` 仅命中测试内的 `"secret"` 字面量（`transport/contract_http_server.rs:711/729`、`adapter/collaboration_tool.rs:129`、`adapter/task_starter.rs:1237`），无签名密钥构造、无 `application.yml`/配置文件。
- 「默认不提供可用的公开 fallback secret」在本 crate 由**构造契约**保证而非默认值：`ControlPlaneStores::from_env/from_lookup` 的 `BUILDER_INTERNAL_TOKEN` 走 `require_key`（缺失或空白即错）；`from_config` 对空 `internal_token` 报错；`ControlPlaneHttpClient::new(base_url, internal_token)` **无默认 token 形参**。
- 但「拒绝 blank」原实现确有缺口（只判 `is_none`，未 trim）→ 本轮补齐，见 §9.2。

### 9.2 本轮修复：P1-4 env 契约（拒绝 blank + trim）

| 项 | 内容 |
|---|---|
| 问题 | `require_key` 只判 `Option::is_none()`：`BUILDER_INTERNAL_TOKEN=""` / `"  "` 会构造出空 token 客户端（未认证调用，迟至首个 `401` 才暴露）；且值未 trim，带空白的 token 会原样进 `X-Builder-Internal-Token`，带空白的 namespace 会进每个 `?namespace=` |
| Java 依据 | `store/ControlPlaneStores.java:192-198` `requireEnv`：`v == null \|\| v.isBlank()` → `IllegalStateException`，否则 `return v.trim()`；`:200-206` `envOr`：blank → fallback，否则 `trim()` |
| 处置 | `require_key` 改为 `Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string())`；`from_lookup` 的 namespace 改为 `map(trim).filter(!empty).unwrap_or(DEFAULT_NAMESPACE)`（等价 `envOr`）。**未额外收紧 `create`**：Java `ControlPlaneStores.create` 同样不校验 token（`ControlPlaneStores.java:77-87`），保持一致 |
| 测试 | 新增 2 个离线用例（`from_lookup` 注入、无进程 env/网络依赖）：`blank_required_env_keys_are_rejected`、`env_values_are_trimmed_like_require_env`；既有 4 个 `from_lookup_*` / `create*` 断言全部保留未改 |
| 验证 | 预期 `cargo test -p agentscope-extensions-controlplane` → **146 lib + 4 e2e / 0 failed**（基线 144 + 4，本轮 +2）。**本轮未运行**，待主智能体统一执行 |

### 9.3 未关闭条目最终裁决（逐条源码级依据）

| 条目 | 裁决 | 源码级依据 |
|---|---|---|
| `transport::GrpcTransport`（ASDP gRPC） | 已完成（设计决定，不迁移） | Rust 仓库 `.proto` 文件数 **0**；本 crate `Cargo.toml` 无 `tonic`/`prost`；`start_grpc` 默认 `false`（`config.rs:140`）；HTTP 通路已覆盖 `/agentscope/*` 17 端点与 `/api/v1/dp/*` 全部 store 契约 |
| `SessionCompactor` | 已完成（设计决定，不提供通用实现） | 需宿主提供模型与压缩策略：harness `ConversationCompactor::new(Option<Arc<dyn Model>>)`（`memory/compaction.rs:595`）与 `HarnessAgentBuilder::compaction(CompactionConfig)`（`harness_agent.rs:991`）都是宿主配置；trait 即注入点（`AgentScopeAdapter::new(..., compactor)`） |
| `SessionHistorySource` | 已完成（设计决定，不提供通用实现） | transcript 存储位置是宿主契约（Java 对应 harness `.log.jsonl`）；trait 即注入点 `AgentScopeAdapter::set_history_source`（`agent_scope_adapter.rs:89`） |
| `AgentRuntimeSource` | 已完成（设计决定，不提供通用实现） | subagent/plan/workspace 注册表属宿主；trait 即注入点 `set_runtime_source`（`agent_scope_adapter.rs:97`），未注入时**不宣告**相应 capability（`agent_scope_adapter.rs:233-240`，测试 `capabilities_baseline_without_optional_sources`），控制面不会显示必然失败的按钮 |
| `SessionBridge.onCommittedEvent` 类级残留 | 已完成（设计决定，不迁移：不逐方法名镜像） | `BridgeReporter::publish_committed`（`bridge.rs:172`）+ `committed_event_sink()`（`bridge.rs:343`）+ `export_sink_handle()`（`bridge.rs:351`）；`tests/committed_export_e2e.rs` 4 用例（`committed_events_from_a_real_run_reach_the_journal_once` 等）。追加同名薄转发无生产调用方 → 不写 |
| `AgentScopeAdapter.publishCommitted` 类级残留 | 已完成（设计决定，不迁移） | reporter 经 `on_bridge_attached` 注入（`agent_scope_adapter.rs:274`），转发语义与 `BridgeReporter::publish_committed` 完全一致；adapter 不重复暴露 |
| Observer 的 `SessionExportSink` 分支 | 已完成（设计决定，不迁移：分支外置） | `impl SessionExportSink for ControlPlaneCommittedEventSink`（`committed_export.rs:59`），`translate` 镜像 `publishNative` 白名单；Observer 只做旁路 tap（`on_agent`/`on_acting`），提交事实只走 sink —— 单一职责划分，内嵌会让 Observer 同时依赖两套事件源 |
| journal 默认目录 | 已完成 | 保留 `~/.agentscope/service-controlplane/event-journal`；Java 自身矛盾（`EventJournal.java:80-88` 代码 vs `ControlPlaneConfig.java:43` javadoc），按任务口径取文档值；本地 outbox 不跨语言共享 |
| `acknowledge` 的 `i64` | 已完成 | 语义等价：Java `Integer.MIN_VALUE` 哨兵 ↔ `-1`；`source_watermark` 同为 `i64`（§8.3） |

### 9.4 跨模块需求（本 crate 不可解，需汇总方登记）

| 需求 | 归属 | 现状 |
|---|---|---|
| export sink 的 builder 级注入 | core | 现只能经 `RuntimeContext::extension(SESSION_EXPORT_SINK_CONTEXT_KEY, ...)`；本 crate 已提供 `export_sink_handle()` |
| MCP 服务器接线 | harness | `HarnessAgentBuilder` 无 MCP 配置；`ToolsSelection.mcp_servers` 仅计算导出，建连归宿主 |
| ASDP gRPC | 仓库级 | proto 契约存在（`agentscope-service/service-controlplane/internal/asdp/asdp.proto`），但需 `protoc` + `tonic`/`prost` + 构建期拷贝 service proto。**语义层已落地**（`transport::asdp`，含 `AsdpLink` seam），wire 由宿主注入 |
| `acknowledge` 若要 `Option<u64>` 公开 API | core | 需同时改 `SessionEvent.seq` 与 `source_watermark` 的符号类型（§8.6 第 2 条） |

## 10. 执行记录（2026-10-31 二轮收口）

> 本轮为**代码执行**（未跑 cargo，由主智能体统一验证）。改动仅限 `crates/agentscope-extensions-controlplane/src/{transport/asdp.rs, transport/mod.rs, adapter/host_impls.rs, adapter/mod.rs, adapter/agent_scope_adapter.rs, error.rs}`；**未改**根 `Cargo.toml`、core、harness、其它 crate。

### 10.1 `GrpcTransport` 裁定与处置（P2，原「设计决定」细化）

**Java 侧契约确实存在**：`agentscope-java/agentscope-service/service-controlplane/internal/asdp/asdp.proto`
（`service AgentDataPlaneService { rpc Connect(stream Upstream) returns (stream Downstream); }`，
`java_package = "io.agentscope.controlplane.proto"`，`java_multiple_files = true`；另有一份
`sdk/dsh/proto/asdp.proto`）。`GrpcTransport.java:16-30` 直接 import 这些生成类。此前 tracker 记「proto 文件不存在」
**有误**（当时只搜了 Rust 仓库），已在 §6.2 / §9.4 更正。

**不能照搬的原因（逐条）**：

1. **生成链**：Java 由 Maven protobuf 插件生成；Rust 需 `prost-build`/`tonic-build` + `protoc`（本机不保证安装）+ `build.rs` 构建期拷贝 service proto（跨 crate 文件引用，超出本 crate 可写范围）。`tonic`/`prost` 均不在根 `[workspace.dependencies]`，本 crate 自行声明即违反「不新增 repository 级依赖」的既有约定（可另立任务）。
2. **传输形态**：ASDP 是**单条双向 gRPC 流**；本 crate 现有两个 transport 都是 unary JSON（`ControlPlaneHttpClient` 基于 `reqwest`，`ContractHttpServer` 基于 `axum`），无法表达 server-push 的 `Downstream` 流与 `EventReportAck` 回压。
3. **依赖面**：gRPC 栈会把 `h2`/`http2` 一类重依赖带进可选扩展，与「扩展 crate 保持轻量、HTTP 通路已全覆盖契约」的现有取舍冲突。

**因此本轮落地的是「可验证的语义层」而非 wire 层**（`src/transport/asdp.rs`，1 module + 7 用例）：

| 语义 | Java 依据 | Rust 落点 |
|---|---|---|
| handshake（`ConnectRequest` + `UpstreamMeta`） | `GrpcTransport.java:229-241, 261-274` | `AsdpTransport::connect()` + `Upstream`/`UpstreamPayload::Connect` |
| 重连退避 | `:248-259`（`min(30s, 1s << min(attempt,5))`） | `AsdpTransport::reconnect_delay(attempt)` + `on_stream_closed()`（实测序列 2/4/8/16/30/30s——Java 先自增再移位） |
| 15s 心跳 | `:62-64, 317-336` | `HEARTBEAT_INTERVAL` + `heartbeat()`（断线时不排队，同 Java） |
| ack 门控 outbox | `:286-315`（`reportEvents` 返回 bool）+ `:378-412`（`EventReportAck`） | `report_events()` 入 `pending`（按 `report_id`）+ `handle_downstream()` 收到无 `error` 的 ack 才移除 |
| 下行四类命令 | `:378-432` | `AsdpEvent::{Acked, SessionCommand, ExecutionAttempt, ConversationTurn, ConnectRejected, Ignored}` |
| 收尾 | `:333-370` | `close()`（pending 保留——durable outbox 归 bridge） |

**DTO 形状**：按 proto3 **JSON** 编码建模（结构体 `#[serde(rename_all = "camelCase")]`，`bytes` 字段为 base64 字符串）——不是 protobuf 字节；`Upstream`/`Downstream` 信封**手写编解码**（不用 `#[serde(flatten)]`，serde 对 enum flatten 支持不完整）。字段与 `asdp.proto` 的 `UpstreamMeta`/`ConnectRequest`/`Heartbeat`/`EventReport`/`EventReportAck`/`SessionEventCursor`/`SessionCommand`/`ExecutionAttemptCommand`/`ConversationTurnCommand`/`ConnectResponse`/`Upstream`/`Downstream` 一一对应。

### 10.2 三个宿主 SPI 默认实现（`src/adapter/host_impls.rs`）

| SPI | 实现 | Java 依据 | 行为要点 |
|---|---|---|---|
| `SessionHistorySource` | `StoreSessionHistorySource` | `SessionHistorySource.java:18-24`（“plug a source that reads the append-only `.log.jsonl`”） | `SessionKey::new(user, agent_id, session)` → `store.open()` → `core::session::transcript()` → `message_item()`（复用 adapter 同一映射，序号从 1）；无法构成 key → `None`（adapter 回退 live state） |
| `SessionCompactor` | `HarnessSessionCompactor` | `SessionCompactor.java:20-46`（推荐委派 `ConversationCompactor`） | 过滤 SYSTEM → `compact_if_needed(conversation, config, agent_id, session_id)` → 回写 `state.context`，返回注入摘要文本；无 model 时为确定性预处理 + `(summary unavailable - no model configured)`（可离线断言） |
| `AgentRuntimeSource` | `TaskRepositoryRuntimeSource` | `AgentRuntimeSource.java`（`listSubagentTasks`/`cancelSubagentTask`，其余 default） | `list_tasks`/`cancel_task` 经 harness `TaskRepository`；`list_subagents`/`list_workspaces` 回空表（同 Java default `List.of()`）；`set_plan_mode`/`read_plan_excerpt` 保持 trait 默认（需绑定具体 `PlanModeHandle`，本 crate 不持有） |

配套：`adapter::mod` 已 `mod host_impls;` + `pub use`；`agent_scope_adapter::message_item` 提升为 `pub(crate)`（无行为变更）；`AgentScopeAdapter` 的 `compress` 分支仅在 `summary` 非空时发布 `compaction` 事件（避免「无压缩却报压缩」）。

### 10.3 新增用例（12 个，均离线）

- `transport::asdp::tests::connect_sends_the_handshake_with_meta_and_capabilities`
- `transport::asdp::tests::reconnect_backoff_matches_the_java_cap`
- `transport::asdp::tests::heartbeat_is_skipped_while_disconnected`
- `transport::asdp::tests::event_reports_stay_pending_until_acknowledged`
- `transport::asdp::tests::downstream_commands_are_dispatched`
- `transport::asdp::tests::stream_close_schedules_the_next_reconnect_delay`
- `transport::asdp::tests::next_event_reads_and_decodes_one_frame`（共 7 个）
- `adapter::host_impls::tests::store_history_source_reads_the_native_transcript`
- `adapter::host_impls::tests::store_history_source_rejects_an_empty_session_id`
- `adapter::host_impls::tests::harness_compactor_rewrites_the_context_and_returns_the_summary`
- `adapter::host_impls::tests::harness_compactor_is_a_noop_below_the_trigger`
- `adapter::host_impls::tests::task_source_lists_and_cancels_background_tasks`

预期基线：**158 lib + 4 e2e**（上轮 146 + 12；asdp 7 + host_impls 5）。验证：`cargo test -p agentscope-extensions-controlplane`。

### 10.4 反向验证建议（由主智能体执行）

1. `reconnect_delay` 改回固定 1s → `reconnect_backoff_matches_the_java_cap` FAILED。
2. `handle_downstream` 对带 `error` 的 ack 也移除 pending → `event_reports_stay_pending_until_acknowledged` FAILED。
3. `HarnessSessionCompactor::compact` 不回写 `state.context` → `harness_compactor_rewrites_the_context_and_returns_the_summary` FAILED。




