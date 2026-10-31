# CONTROLPLANE 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CONTROLPLANE** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-controlplane\`（主包 `io.agentscope.extensions.controlplane`，子包 `adapter` / `model` / `store` / `transport`）。**命名历史**：该模块原名 `agentscope-extensions-aistio`，Java 于 commit `a2cbea5c3`（2026-10-08）整体重命名（54 文件全为 rename）；旧版源码可用 `git show a2cbea5c3~1:agentscope-extensions/agentscope-extensions-aistio/...` 取出。
> **Rust 目标**：`crates/agentscope-extensions-controlplane/`（波次 C）；crate 名、类型前缀（`ControlPlane*`）、env（`CONTROL_PLANE_*`）、journal 目录与 sink 名均已于 **2026-10-22** 对齐。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | ~~`agentscope-extensions-controlplane`~~ → **`agentscope-extensions-controlplane`**（commit `a2cbea5c3` 重命名） |
| Java 子模块 | 单模块：`adapter` / `model` / `store` / `transport`（+ 根包 5 类） |
| Java 规模（2026-10-22 实测） | controlplane：主 39 文件 **9518** 行 / 测 14 文件 **3017** 行（共 53 .java）。旧 controlplane（`a2cbea5c3~1`）：主 39 文件 9360 行 / 测 14 文件 2898 行 |
| Rust crate | `crates/agentscope-extensions-controlplane`（**2026-10-22 改名已完成**，crate / 类型前缀 / env / journal 目录 / sink 名均已对齐，见汇总 §4.5 裁定 3） |
| 迁移波次 | C |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 158 passed / 0 failed（lib），另 4 个 e2e）—— 39 个 Java 类：✅ 38 / ⚠️ 0 / ❌ 1（该 1 项与 3 项类级残留、`GrpcTransport` wire 层、三个宿主 SPI 均已判「已完成（设计决定，不迁移）」）；ASDP 语义层（`src/transport/asdp.rs`）与宿主默认实现（`src/adapter/host_impls.rs`）已落地，P1-4 env 契约已修。历史见 history/CONTROLPLANE__MIGRATION_HISTORY.md |
| 2026-10-31 全量对齐复核 | 逐包核对 39 Java 类 / 40 Rust 文件，结论见 **§3.5**（`CP-01`..`CP-21`）与 **§4.4**（`CP-D01`..`CP-D05`、`CP-X01`..`CP-X09`）。测试数沿用既有实测 158 lib + 4 e2e（未重跑）。核心结论：Level-2/提交式事件链路与 `transport::asdp` 语义层已对齐；**启动装配（契约 HTTP / 自注册 / ASDP）与契约命令路由尚未接线**，生产可用性以 `CP-01`～`CP-04` 为准 |

## 2. Java 清单与依赖映射

### 2.0 源文件清单（controlplane 53 个 .java：主 39 / 测 14；旧 controlplane 同为 53 个：主 39 / 测 14）
> 下表 Java 类名一列同时给出**旧 controlplane 名**与**现行 controlplane 名**（`→` 后为新名）；Rust 对应列使用 `ControlPlane*` 现行命名（**2026-10-22 已改名**，见汇总 §4.5 裁定 3）。

根包 `io.agentscope.extensions.controlplane`（旧 `io.agentscope.extensions.controlplane`）：
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `ControlPlane`（旧 `ControlPlane`） | 入口：`instrument(target, config[, adapter])` / `newBridge` | `lib.rs::instrument`（等价 `ControlPlaneBridge::new`) | ✅ 薄封装 |
| `ControlPlaneConfig`（旧 `ControlPlaneConfig`） | 连接/上报设置 record（**20 字段**，校验 agentKey/tenant/namespace/instanceKey） | `config::ControlPlaneConfig`（builder + 同校验，**20 字段一一对应**） | ✅ |
| `SessionBridge` (1207L) | 核心桥：挂载 adapter、事件序列化、Level-1/2/4 上报、命令处理、start/close | `bridge::ControlPlaneBridge`（async/tokio） | ✅ 已完成（P1 主干）；`publish_committed`/`committed_event_sink()`/`export_sink_handle()` 已落地（2026-10-22 关闭 见 history/CONTROLPLANE__MIGRATION_HISTORY.md P0-1，见 history/CONTROLPLANE__MIGRATION_HISTORY.md）；方法名不镜像已裁定（2026-10-31，见 history/CONTROLPLANE__MIGRATION_HISTORY.md） |
| `FrameworkAdapter` (156L) | 框架适配 SPI（13 capability 常量 + attach/detach + extractContext/listMessages...） | `adapter::FrameworkAdapter`（async_trait，**13 capability 常量逐一对应**） | ✅ trait |
| `EventJournal` (250L) | Level-2 事件崩溃安全本地 outbox（长度前缀记录 + 序列号 + sha256 身份） | `event_journal::EventJournal` | ✅ 已完成（含 `source_watermark`/`track_source`/`acknowledge` + 临时文件 rename 原子重写，见 history/CONTROLPLANE__MIGRATION_HISTORY.md P1-2 已完成） |

子包 `model`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `SessionEvent` (200L) | Level-2 事件（seq/type/role/content/tool*/tokens/duration/meta；6 种 type + 4 种 role 常量） | `model::SessionEvent`（**type/role 常量逐一对应**） | ✅ |
| `ContextSnapshot` (364L) | Level-4 有效上下文 + `computeHash()`（SHA-256 排序 JSON 前 16 hex） | `model::ContextSnapshot::hash()` | ✅ |
| `ContextTracker` (186L) | 从事件流增量维护有效上下文视图 + contextHash | `model::ContextTracker` | ✅ |
| `Inventory` (116L) | 实例级 inventory（subagents/workspaces/health） | `model::Inventory` | ✅ |
| `MessagePage` (83L) | Level-3 全量历史分页 | `model::MessagePage::of` | ✅ |
| `AgentTaskAssignment` (42L) | ASDP 执行信封（attemptId/agentTaskId/...） | `model::AgentTaskAssignment` | ✅ |

子包 `store`（ControlPlane 托管存储，实现 harness 既有 trait，不重建平行类型）：
| Java 类 | 职责 | Rust 对应（实现既有 trait） | 状态 |
|---|---|---|---|
| `ControlPlaneStores` (189L) | 工厂：`fromEnv`/`create`/`from(config)` + `withAgentStateStore` | `store::ControlPlaneStores` | ✅ env 键已对齐 `CONTROL_PLANE_HTTP` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` / `CONTROL_PLANE_NAMESPACE`（见 history/CONTROLPLANE__MIGRATION_HISTORY.md P1-1 已完成；`from_lookup` 可离线断言键名与顺序） |
| `ControlPlaneBaseStore` (194L) | `/api/v1/dp/kv/*` → harness `BaseStore` | `store::ControlPlaneBaseStore` impl `BaseStore` | ✅；含 `supports_atomic_session_storage() = true`（与 Java 新增一致） |
| `ControlPlaneMessageBus` (267L) | `/api/v1/dp/bus/*` → harness `MessageBus`（Mode D 轮询降级） | `store::ControlPlaneMessageBus` impl `MessageBus` | ✅ |
| `ControlPlaneAsyncToolRegistry` (216L) | `/api/v1/dp/async-tools/*` → harness `AsyncToolRegistry` | `store::ControlPlaneAsyncToolRegistry` impl `AsyncToolRegistry` | ✅ |
| `ControlPlaneTaskRepository` (799L) | `/api/v1/dp/tasks/*` → harness `TaskRepository`（本地 handle + CP 持久化 + 心跳） | `store::ControlPlaneTaskRepository` impl `TaskRepository` | ✅ 已完成（含 `find_pending_deliveries`/`mark_delivered`/`is_delivered`/`shutdown`） |
| `ControlPlaneSessionTurnGate` (237L) | `/api/v1/dp/locks/*` → harness `SessionTurnGate`（20s 续租/60s TTL/30s 回退本地） | `store::ControlPlaneSessionTurnGate`（crate 内适配层；harness gate 为具体 struct 非 trait，本地 `SessionTurnGate` 作降级回退，见 §4.2） | ✅ 已完成 |
| `ControlPlaneSandboxExecutionGuard` (206L) | `/api/v1/dp/locks/*` → harness `SandboxExecutionGuard` | `store::ControlPlaneSandboxExecutionGuard`（crate 内适配层；harness guard 为具体 struct 非 trait，见 §4.2） | ✅ 已完成 |
| `ControlPlaneSnapshotSpec` (135L) | `/api/v1/dp/snapshots/*` → harness `SandboxSnapshotSpec`/`SandboxSnapshot` | `store::ControlPlaneSnapshotSpec` impl `SandboxSnapshotSpec` | ✅ |

子包 `transport`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `ControlPlaneHttpClient` (184L) | JDK HttpClient 包装，加 `X-Builder-Internal-Token`，10s 超时；`send`/`sendBytes`/`sendForBytes` | `transport::ControlPlaneHttpClient`（reqwest；`send`/`send_bytes`/`send_bytes_with_headers`/`send_for_bytes`；`BytesResponse.content_type`） | ✅ |
| `ContractProvider` (89L) | 契约服务数据提供者 SPI（17 方法，全部有 default） | `transport::ContractProvider`（trait，**17 方法逐一对应**） | ✅ |
| `HttpSelfRegistration` (273L) | `POST /api/v1/agent-registrations` + 周期心跳 + 身份回调 | `transport::HttpSelfRegistration`（+ `RegisteredIdentity`/`RegistrationOptions`/`parse_registered_identity`） | ✅ 已完成 |
| `ContractHttpServer` (333L) | 进程内 `/agentscope/*` HTTP 契约服务 | ✅ 已完成（axum） | `src/transport/contract_http_server.rs` + `transport/mod.rs` re-export；**16 条 route path / 17 个端点与 Java 逐一对应**（GET info/health/sessions/subagents/workspaces、GET state/context/messages/tasks/subagent-tasks/export-transcript、POST compress/terminate/abort/plan-mode/messages、DELETE subagent-tasks/{taskId}） |
| `GrpcTransport` (404L) | ASDP gRPC 上游通道（protobuf） | 设计决定（P2 暂缓：需 ASDP proto toolchain） | 设计决定 |
| `CollaborationClient` (502L) | 协作/编排客户端 | ✅ 已完成 | `src/transport/collaboration_client.rs`（issue/team/approval + task-scoped + MCP `tools/call`；`X-Agent-Task-Token`）；**4 端点已补齐**：`replan_run`/`signal_run`/`upload_artifact`/`download_artifact`（见 history/CONTROLPLANE__MIGRATION_HISTORY.md P1-3 已完成，下载走 `send_for_bytes` 二进制通道） |
| `OrchestrationClient` (137L) | 编排客户端（definition/run/attempt/runtime-policy） | `src/transport/orchestration_client.rs`（`?tenant=&namespace=` 作用域） | ✅ **19 方法与 Java 逐一对应** |

子包 `adapter`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeAdapter` (936L) | AgentScope 框架适配（覆盖全部 capability） | `adapter::AgentScopeAdapter` | ✅ 已完成；`publishCommitted` 语义由 `BridgeReporter::publish_committed` 承接（2026-10-22 关闭 见 history/CONTROLPLANE__MIGRATION_HISTORY.md P0-1，见 history/CONTROLPLANE__MIGRATION_HISTORY.md）；方法名不镜像已裁定（2026-10-31，见 history/CONTROLPLANE__MIGRATION_HISTORY.md） |
| `ControlPlaneObserverMiddleware`（旧 `ControlPlaneObserverMiddleware`，267L） | 观察中间件（Level-2 事件源） | `adapter::ControlPlaneObserverMiddleware` | ✅ 已完成；native session-log 分支由 `committed_export::ControlPlaneCommittedEventSink` 承接（2026-10-22 关闭 见 history/CONTROLPLANE__MIGRATION_HISTORY.md P0-1，见 history/CONTROLPLANE__MIGRATION_HISTORY.md）；分支外置已裁定（2026-10-31，见 history/CONTROLPLANE__MIGRATION_HISTORY.md） |
| `HarnessAgentTaskStarter` (533L) / `HarnessWorkspaceAgentFactory` (229L) | harness 任务启动/工作区 agent 工厂 | ✅ 已完成 | `src/adapter/task_starter.rs`（`HarnessAgentTaskStarter` impl `AgentTaskStarter` + `TurnRunner` seam + `run_to_outcome` 续跑循环 + `HarnessTurnRunner`）+ `src/adapter/workspace_factory.rs`（`HarnessWorkspaceAgentFactory` + `materialize`/`tools`/`permissions`/`skill_filter`/`apply_tool_selection`）；12 个新单测（含复现 Java `HarnessAgentTaskOutcomeTest` 的 5 个场景） |
| `AgentTask*` 工具族（5 类：`AgentTaskOutcome`/`AgentTaskOutcomeTool`/`AgentTaskToolContext`/`AgentTaskCollaborationTool`/`AgentTaskStarter`） | AgentTask 工具/上下文/产物 | ✅ 已完成 | `src/adapter/agent_task.rs`（`AgentTaskOutcome`/`State`/`ToolContext`/`AgentTaskOutcomeTool`）+ `src/adapter/collaboration_tool.rs`（`AgentTaskCollaborationTool`）；都实现 core `Tool`；`READ_ONLY_TOOLS`/`TERMINAL_TOOLS` 与 Java 一致 |
| `AgentRuntimeSource` (56L) / `SessionCompactor` (56L) / `SessionHistorySource` (35L) / `WorkspaceAgentFactory` (29L) | 适配器辅助 SPI | ✅ trait 已落（`src/adapter/support.rs`）；宿主具体实现待后续（P2） | `AgentRuntimeSource`/`SessionCompactor`/`SessionHistorySource` 已实现为 trait；`WorkspaceAgentFactory` 由 `HarnessWorkspaceAgentFactory`（`create(definition, assignment) -> Result<HarnessAgent, String>`）实现 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `agentscope-core` | 核心类型 | `agentscope-core`（path，已有） | |
| `agentscope-harness` | harness trait（DistributedStore/BaseStore/MessageBus/AsyncToolRegistry/TaskRepository/Sandbox*） | `agentscope-harness`（path，已有） | |
| grpc + protobuf-java（ASDP） | gRPC 上游 | 设计决定暂缓（P2） | 需 proto 工具链，§4.2 |
| jackson-databind | JSON | `serde_json`（workspace） | |
| JDK HttpClient | HTTP 客户端 | `reqwest`（workspace，json+rustls-tls） | 新增到本 crate 依赖 |
| JDK com.sun.net.httpserver | 契约 HTTP 服务 | ✅ `axum`（根 workspace deps） | 已落地，见 §3.2 |
| SHA-256（MessageDigest） | contextHash / journal 身份 | `sha2`（workspace） | 新增到本 crate 依赖 |
| UUID | lock holder / entryId | `uuid`（workspace） | 新增到本 crate 依赖 |
| FileChannel | journal 追加 | `std::fs` / `tokio::fs` | |
| reactor-core (Mono/Flux) | 异步 | `async_trait` + tokio | |
| slf4j | 日志 | `tracing` | |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib.rs 模块声明 | — | ✅ 已完成 | 拆 config/model/adapter/store/transport/event_journal/bridge/error |
| Cargo.toml 依赖（reqwest/sha2/uuid） | pom.xml | ✅ 已完成 | 仅写本 crate `[dependencies]`，未改根/其它 crate |
| `ControlPlaneConfig`（builder + 校验） | `ControlPlaneConfig` | ✅ 已完成 | agentKey 必填；startGrpc 需 controlPlane |
| model 六类 | `model/*` | ✅ 已完成 | 含 `ContextSnapshot::hash()` 与 `MessagePage::of` |
| `EventJournal`（长度前缀记录 + sequence） | `EventJournal` | ✅ 已完成 | JSON 记录（替代 protobuf）；load/append/pending/ack |

### 3.2 transport（P0/P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `ControlPlaneHttpClient` | 同名类 | ✅ 已完成 | reqwest；`X-Builder-Internal-Token`；get/post/put/head；404/409 语义 |
| `ContractProvider` trait | 同名类 | ✅ 已完成 | async_trait |
| `HttpSelfRegistration` | 同名类 | ✅ 已完成 | 注册 POST + 心跳循环 + RegisteredIdentity；`IdentityListener` 身份回调 |
| `ContractHttpServer` | 同名类 | ✅ 已完成 | 基于 `axum`（已在根 workspace deps）；`bind`/`start`/`router`/`port` 齐备；`ContractProvider` trait 已实现 |
| ASDP gRPC `GrpcTransport` | 同名类 | 已完成（设计决定，不迁移） | 需 ASDP proto 工具链 + proto 文件（Rust 仓库 `.proto` 文件数 0，本 crate 无 `tonic`/`prost` 依赖）；`start_grpc` 默认 false，HTTP 通路已全覆盖。依据见 history/CONTROLPLANE__MIGRATION_HISTORY.md |

### 3.3 store（P0/P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `ControlPlaneStores` 工厂 | 同名类 | ✅ 已完成 | create/from_config/with_agent_state_store |
| `ControlPlaneBaseStore` impl `BaseStore` | 同名类 | ✅ 已完成 | `/api/v1/dp/kv/*`；404→None；409→false |
| `ControlPlaneMessageBus` impl `MessageBus` | 同名类 | ✅ 已完成 | queue/log 走 CP；Mode D publish/subscribe 本地广播降级 |
| `ControlPlaneAsyncToolRegistry` impl `AsyncToolRegistry` | 同名类 | ✅ 已完成 | `/api/v1/dp/async-tools/*` |
| `ControlPlaneSnapshotSpec` impl `SandboxSnapshotSpec` | 同名类 | ✅ 已完成 | `/api/v1/dp/snapshots/*`（inline 模式） |
| `ControlPlaneTaskRepository` impl `TaskRepository` | 同名类 | ✅ 已完成 | 本地 `BackgroundTask` handle + CP 持久化 + 心跳；本地生命周期解耦（见 §4.3） |
| `ControlPlaneSessionTurnGate` | 同名类 | ✅ 已完成 | harness gate 为具体 struct 非 trait → 以 crate 内适配层 + 本地 `SessionTurnGate` 降级实现（§4.2） |
| `ControlPlaneSandboxExecutionGuard` | 同名类 | ✅ 已完成 | harness guard 为具体 struct 非 trait → 以 crate 内适配层实现（§4.2） |

### 3.4 adapter / bridge（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `FrameworkAdapter` trait（capabilities + 可选操作默认报错） | `FrameworkAdapter` | ✅ 已完成 | async_trait；capability 常量对齐 |
| `ControlPlaneBridge`（attach/start/close、事件→seq、Level-1/4 上报） | `SessionBridge` | ✅ 已完成（P1 主干） | 事件 emit 已**真接线**（assign seq → journal → tracker）；新增 `describe_session`/`set_busy`/`is_busy`/`set_effective_system_prompt`/`effective_system_prompt`/`set_context_used_tokens`/`is_known_session`/`reporter()`；`BridgeReporter` 为无所有权发布句柄 |
| `AgentScopeAdapter` | 同名类 | ✅ 已完成（P1） | `src/adapter/agent_scope_adapter.rs`；实现 `FrameworkAdapter`（`can_handle` → ReActAgent/HarnessAgent；context/message/session-command/session-abort/task/plan/export/inbound 能力）；`extract_context`/`list_messages`/`execute_command`/`session_ids` 落地 |
| `ControlPlaneObserverMiddleware` | 同名类 | ✅ 已完成（P1） | `src/adapter/observer_middleware.rs`；`on_agent`（session_start + 输入消息 + busy + 旁路 tap）、`on_acting`（tool_call）、`on_system_prompt`（采集有效 prompt）；tool_result 由 `ToolResultTextDelta`/`ToolResultEnd` 事件重组；token 取自 `AgentResult.reply` 的 `Msg.usage` |
| native session-log 提交链路 | `SessionBridge.onCommittedEvent` + `AgentScopeAdapter.publishCommitted` + Observer 的 `SessionExportSink` 分支 | ✅ 已完成（2026-10-22） | `src/committed_export.rs`：`ControlPlaneCommittedEventSink` 实现 core `SessionExportSink`，`translate` 镜像 `publishNative` 白名单（message/tool_call/tool_result/compaction）；`BridgeReporter::publish_committed` 按 per-source watermark 去重后入 journal；宿主经 `export_sink_handle()` 显式接线（见 history/CONTROLPLANE__MIGRATION_HISTORY.md） |
| 适配器辅助 SPI | `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` / `AgentTaskStarter` | ✅ 已完成（trait 层 + 宿主默认实现） | `src/adapter/support.rs`（trait）+ `src/adapter/host_impls.rs`（默认实现，2026-10-31）：`HarnessSessionCompactor` / `StoreSessionHistorySource` / `TaskRepositoryRuntimeSource`，均已 `mod` + `pub use` + 单测；`AgentTaskStarter` 由 `HarnessAgentTaskStarter` 具体实现 |
| harness ASDP 执行器 | `HarnessAgentTaskStarter` (27.5KB) | ✅ 已完成（P2） | `src/adapter/task_starter.rs`；带去重（`attemptId:generation`）、`task.ack`、`ensureRunning`（409 竞态）、workspace 定义物化、协作工具注册、`run_to_outcome` 续跑预算、`cancel_unfinished` 清理、`finish`/`fail` 上报 |
| harness 工作区工厂 | `HarnessWorkspaceAgentFactory` (11.7KB) | ✅ 已完成（P2） | `src/adapter/workspace_factory.rs`；`materialize` 拒绝绝对路径/穿越/运行态段/私有内存/符号链接；`tools`/`permissions` 纯函数与 Java 同构 |
| 运行时工具审批回环 | `CollaborationClient.awaitRuntimeToolApproval` | ✅ 已完成（P2） | `src/transport/collaboration_client.rs`：`RuntimeApprovalSource` seam + 3 个 task-scoped 端点（`runtime-approvals` / `.../{id}/decision` / `.../{id}/ack`）+ 自由函数 `await_runtime_tool_approval`（请求→轮询→ack）；适配层经 core 原生 `ConfirmHandler` 接入（Java 的 PERMISSION_ASKING 暂停/恢复在 Rust core 不存在，见 §4.1.0f） |
| `ControlPlane` 入口 | 同名类 | ✅ 已完成（薄封装） | `instrument` = new+attach+start |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`CP-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.4 差异另有 `CP-D<NN>` / `CP-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> 覆盖面：Java 主 39 类（9518 行）/ 测 14 类（3017 行）逐个映射；Rust 40 个 `.rs`（约 13.5k 行）逐包核对（adapter / transport / bridge / committed_export / event_journal / store / model）。优先级按**业务能力是否可达**判定，不按代码行数。
> 测试数只引用既有实测：**158 lib passed / 0 failed + 4 e2e**（`tests/committed_export_e2e.rs`），本轮未运行 `cargo`。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| CP-01 | P0 | `ControlPlaneBridge::start` 只开 journal 并置位，未按 config 装配 `/agentscope/*` 契约 HTTP 服务、`POST /api/v1/agent-registrations` 自注册、ASDP 链路；`start_http` / `start_http_register` / `start_grpc` / `contract_http_host` / `session_affinity` 五个开关在 crate 内**零消费者**，且全仓无 `ControlPlaneBridge` 外部调用点（`rg 'ControlPlaneBridge' --glob '!crates/agentscope-extensions-controlplane/**' --glob '!docs/**'` 零命中） | `SessionBridge.java#start`（L183-317：ContractHttpServer → HttpSelfRegistration(15s) → GrpcTransport + 4 个 handler → 3 个定时器）、`#getContractPort`(L177)、`#getRegisteredIdentity`(L172) | `src/bridge.rs:313-322`（仅 journal + `started=true`）；`rg '\.start_http\b|\.contract_http_host\b|\.session_affinity\b' src` 仅命中 `config.rs` | 待办 | 给 bridge 增加 `http` / `registration` / `asdp` 槽位与 `contract_port()` / `registered_identity()` 访问器，`start` 按开关装配（自注册心跳 **15s**，对齐 `SessionBridge.java:213` 的 `15_000L`；`close` 停服务并 detach）；验收：`contract_http_port(0)` + 桩 HTTP 客户端，断言 `/agentscope/health` 200 且注册请求体字段与 `HttpSelfRegistration` 一致 |
| CP-02 | P0 | `impl ContractProvider for ControlPlaneBridge` 未委托会话命令与任务/导出路由：`compress` / `terminate` / `abort` / `plan-mode` / `messages(POST)` / `tasks` / `subagent-tasks` / `DELETE subagent-tasks/{id}` / `export-transcript` 全部落到默认实现（501 或**静默成功**） | `SessionBridge.java#compress`(L1097)、`#terminate`(L1113)、`#abort`(L1119)、`#tasks`(L1125)、`#subagentTasks`(L1149)、`#cancelSubagentTask`(L1161)、`#planMode`(L1169)、`#postMessage`(L1177)、`#exportTranscript`(L1209) | `src/bridge.rs:486-543` 只覆盖 inventory/context/messages(GET)/session_ids/info/subagents/workspaces；`src/transport/contract_provider.rs:95-140` 默认实现：`terminate`/`abort` 返回 `Ok(())`（**静默成功**），其余 `Unsupported` → 501 | 待办 | 在 `impl ContractProvider for ControlPlaneBridge` 覆盖上述路由，compress/terminate/abort 委托 `adapter.execute_command`（`src/adapter/agent_scope_adapter.rs:405-455` 已完整实现）；`terminate`/`abort` 不得再静默成功；验收：未挂 compactor 时 compress 得 501，挂 `HarnessSessionCompactor` 时 200 且 journal 出现 `compaction` 事件（`agent_scope_adapter.rs:441-451`） |
| CP-03 | P0 | `FrameworkAdapter` 缺 Java 的 9 个方法，同时 `AgentScopeAdapter::capabilities` 仍声明这些未实现能力 → 控制面按能力位显示按钮，按钮必失败（trait 文档 L7-9 明确禁止过度声明） | `FrameworkAdapter.java#listWorkspaces`(L99)、`#listTasks`(L108)、`#listSubagentTasks`(L116)、`#cancelSubagentTask`(L121)、`#setPlanMode`(L128)、`#injectUserMessage`(L136)、`#runConversationTurn`(L143)、`#buildAgentConfig`(L147)、`#handleAgentTask`(L164) | `src/adapter/framework_adapter.rs:63-131` 仅 12 个方法；`rg 'list_tasks|set_plan_mode|inject_user_message|run_conversation_turn|build_agent_config|handle_agent_task' src/adapter/framework_adapter.rs` 零命中；而 `src/adapter/agent_scope_adapter.rs:221-228` 声明 `CAP_TASK_QUERY`/`CAP_PLAN_MODE`/`CAP_EXPORT_TRANSCRIPT`/`CAP_CONVERSATION_INBOUND` | 待办 | 按 Java 签名补 9 个默认方法（默认 `Unsupported(cap)`），`AgentScopeAdapter` 用 `runtime_source`（`src/adapter/support.rs:60-95` 已有 `set_plan_mode`/`list_workspaces`/`list_subagent_tasks`/`cancel_subagent_task`）与 `agent_task_starter` 实现 `handle_agent_task`；验收：`capabilities()` 中每个能力位都有可调用实现，未实现者不声明（逐位单测） |
| CP-04 | P0 | Level-2 事件出站环路缺失：无批量 flush、无 ack 超时、无定时器；`ControlPlaneBridge::acknowledge` 无生产调用点 → journal 只进不出，outbox 与磁盘无限增长 | `SessionBridge.java#flushEvents`(L582)、`#onEventAck`(L611)；常量 `EVENT_FLUSH_INTERVAL_MS=5000`(L89)、`EVENT_BATCH_SIZE=20`(L90)、`EVENT_ACK_TIMEOUT_MS=15000`(L91) | `rg 'EVENT_FLUSH|EVENT_BATCH|ACK_TIMEOUT|flush_events' src` 零命中；`BridgeReporter::publish` 只写 journal（`src/bridge.rs:58-77`）；`acknowledge`（`src/bridge.rs:381`）仅被测试引用 | 待办 | 增加 `flush_events()`（batch 20、in-flight 15s 超时后可重投）与 ASDP `EventAck` 回接：ack 成功 → `acknowledge(committed)`，`error` 非空则保留重投（Java L611-651 语义）；5s `tokio::time::interval` 驱动；验收：离线 `RecordingLink` 造 `eventAck` 帧，断言 `pending_event_count()` 递减至 0；无 link 时保持现状并在文档注明 |
| CP-05 | P1 | ASDP 下行三类命令无消费者：`sessionCmd` / `executionAttempt` / `conversationTurn` 能解码但无人分发（Java 在 `start` 里绑定 4 个 handler） | `SessionBridge.java#start`（`setSessionCommandHandler`/`setExecutionAttemptHandler`/`setConversationTurnHandler`，L261-268）、`#dispatchCommand`(L824)、`#onExecutionAttempt`(L732)、`#onConversationTurn`(L831) | `rg 'AsdpEvent' src` 仅命中 `transport/asdp.rs` 与 `transport/mod.rs` 的 re-export；bridge/adapter 无消费者 | 待办 | 增加 `ControlPlaneBridge::dispatch_downstream(AsdpEvent)`：SessionCommand → `adapter.execute_command`、ExecutionAttempt → `AgentTaskStarter::start`（`src/adapter/support.rs:110`）、ConversationTurn → `run_conversation_turn`（随 CP-03）；验收：三类命令各一条单测，断言 adapter 收到命令且错误不 panic |
| CP-06 | P1 | 契约读路由未委托 adapter：`subagents`/`workspaces` 恒空，`context` 只回事件流视图（Java 以 adapter 为权威、失败才回落 tracker） | `SessionBridge.java#subagents`(L1069)、`#workspaces`(L1083)、`#context`(L1030：先 `adapter.extractContext`，异常回落 tracker) | `src/bridge.rs:493-545`：`context` 直接 `tracker.snapshot()`；`subagents`/`workspaces` 用 `Inventory::health_only()`（恒空），未调用 `adapter.list_subagents()`（`src/adapter/agent_scope_adapter.rs:401` 已实现并合并 runtime_source） | 待办 | `context` 先 `adapter.extract_context(...).await`（成功用 adapter、错误回落 tracker）；`subagents`/`workspaces` 委托 adapter 并映射契约字段；验收：注入 `TaskRepositoryRuntimeSource` 后 `/agentscope/subagents` 非空 |
| CP-07 | P1 | `compress` 只压缩**克隆**的 `AgentState`：压缩结果仅用于发 compaction 事件，活体上下文/持久化槽未变 → 控制面显示已压缩，下一轮实际仍是全量 | `AgentScopeAdapter.java#handleCommand`(L505-531)：对 `resolveState` 返回的**活体** `AgentState` 调 compactor，结果留在活体对象 | `src/adapter/agent_scope_adapter.rs:405-455`（压缩后丢弃）；`resolve_state` 返回 owned 值（`agent_scope_adapter.rs:149-176`）；`rg 'set_agent_state|state_mut|save_state' src` 零命中 | 待办 | 用 core `ReActAgent::state_handle()`（`crates/agentscope-core/src/agent/react_agent.rs:450`）在锁内替换 `context`；`HarnessAgent` 额外 `agent_state_store.save(...)` 落盘；验收：单测断言压缩后 `agent.agent_state().context.len()` 下降（并做反向验证） |
| CP-08 | P1 | `MessageBus` 退化为进程内广播：`publish` 不上报控制面、`subscribe` 不读控制面 → 跨实例/跨进程 Pub-Sub 语义丢失；`log_read` 成为死方法 | `ControlPlaneMessageBus.java#publish`(L207：`logAppend(key, payload, 100)`)、`#subscribe`(L210-227：`Flux.interval(2s)` + `logRead(key, cursor, 50)` 游标轮询)；`PUBLISH_MAX_LEN=100`(L48)、`SUBSCRIBE_POLL=2s`(L49) | `src/store/message_bus.rs:223-245`：publish 仅 `broadcast::Sender::send`，subscribe 仅本地 `broadcast`；`log_read`（`src/store/message_bus.rs:183`）`rg 'log_read' src` 仅定义处 | 待办 | publish → `log_append(key, payload, 100)`；subscribe → 2s 轮询 `log_read` 并按 `entryId` 推进游标（可保留本地广播做低延迟快路径）；验收：桩 HTTP 断言 publish 打到 `/api/v1/dp/bus/log/append` 且 `maxLen=100`，subscribe 在两次 tick 后收到跨进程条目 |
| CP-09 | P2 | bridge 未上报 push 侧能力位：`session-reporting` / `context-reporting` / `event-reporting` 缺失，`info` 与注册能力集合只剩 adapter 能力 | `SessionBridge.java#capabilities`(L367-375) | `rg 'session-reporting|context-reporting|event-reporting' src` 零命中；`src/bridge.rs:519-535` 的 `info` 直接取 adapter 能力 | 待办 | `info` 与自注册能力集合补三件（有 journal 才加 `event-reporting`）；验收：单测断言 `info.capabilities` 含该三键，`enable_events=false` 时不含 `event-reporting` |
| CP-10 | P2 | 会话状态视图缺失：无 phase 状态机、`session_state` 恒 `busy:false`/`phase:running`、`sessions` 只回 id+phase（Java 返回 messageCount/tokenUsage/contextPressure/startedAt/lastActiveAt/isCompacted/taskSummary/framework） | `SessionBridge.java#setPhase`(L538)、`PHASE_ACTIVE/IDLE/COMPRESSING/ARCHIVED/TERMINATED`(L81-86)、`#sessionState`(L980-1027)、`#sessions`(L923) | `rg 'PHASE_|set_phase' src` 零命中；`src/transport/contract_provider.rs:52-73` 的默认 `session_phase` 恒 `running`、`session_state` 恒 `busy:false`，bridge 未覆盖（`src/bridge.rs:486-543`） | 待办 | bridge 维护 phase（busy → active/idle、compress → compressing、terminate → terminated）并输出 Java 同名字段；验收：断言 terminate 后 `phase=terminated`、压缩中 `phase=compressing`、进行中 `busy=true` |
| CP-11 | P2 | busy 泄漏：只在 `AgentEnd` 复位，流报错/被取消/被 drop 时 `busy` 永久为 true（Java 用 `doFinally`） | `ControlPlaneObserverMiddleware.java#onAgent`（`doFinally(signal -> markBusy(false))`，L94、L120） | `src/adapter/observer_middleware.rs:217-219` 仅 `AgentEvent::AgentEnd` 分支复位 | 待办 | 在流终止的所有路径复位（`Drop` guard 或 `chain(once(...))`）；验收：`next` 返回 `Err` 的用例断言 `bridge.is_busy(session)==false` |
| CP-12 | P2 | ASDP 无身份刷新入口：注册身份/凭证变化无法传播到后续帧 | `GrpcTransport.java#updateIdentity`(L178)、`SessionBridge.java#start` L229-241（`setIdentityListener` → `grpc.updateIdentity`） | `src/transport/asdp.rs:358-480` 无 `update_identity`；`UpstreamMeta` 在 `new` 时固定 | 待办 | 增加 `update_identity(credential/agent_id/binding_id/instance_key/generation)`，后续帧用新 meta；验收：单测断言更新后连接帧 `meta.agentId`/`generation` 变化 |
| CP-13 | P2 | Level-1/Level-4/inventory 的**主动推送**是否需要在 Rust 侧实现（当前只有契约 HTTP 可拉取） | `SessionBridge.java#reportLevel1`(L676, 10s)、`#pushContext`(L653, cooldown 30s)、`#reportInventory`(L716, 30s) | `rg 'report_level1|push_context|report_inventory' src` 零命中；契约拉取已具备（`src/transport/contract_http_server.rs:155-186`）；`asdp.rs` 定义了 `UpstreamPayload::{SessionReport,ContextReport,Inventory}` 但 `frame()` 私有（`asdp.rs:479`）、无发送方法 | 待裁定 | 待与控制面协议确认：若控制面只经契约 HTTP 拉取，则本项关闭并在 §4.4 记差异；若需 push，在 CP-04 的 ASDP 环路上补 `report_sessions`/`report_context`/`report_inventory` 三个发送方法 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| CP-14 | 装配点 `on_bridge_attached` 真实实现 | `AgentScopeAdapter.java#onBridgeAttached`(L204) | `src/adapter/agent_scope_adapter.rs:274-281` 覆写并写入 `AdapterShared.reporter`；`src/bridge.rs:307` 是唯一注入点（`attach` 内调用）；回归测试走真实装配入口 `crate::instrument` → `agent_scope_adapter::tests::bridge_attach_installs_reporter_for_the_observer_middleware`（`agent_scope_adapter.rs:694-726`） | 已完成 |
| CP-15 | 事件 emit 真接线 + 提交式去重 | `SessionBridge.java#publish`(L402)、`#onCommittedEvent`(L382) | `src/bridge.rs:58-96`（seq → journal → tracker）、`BridgeState::publish_committed` 按 per-source watermark 去重；`src/committed_export.rs:79-95` 翻译白名单；e2e 4 用例 `tests/committed_export_e2e.rs` | 已完成 |
| CP-16 | ASDP 语义层（handshake / 退避 / 心跳 / ack 门控 outbox / 下行分发） | `GrpcTransport.java`（L200-437） | `src/transport/asdp.rs:295-486`；测试 `reconnect_backoff_matches_the_java_cap`、`event_reports_stay_pending_until_acknowledged`、`downstream_commands_are_dispatched`、`heartbeat_is_skipped_while_disconnected`（共 7 个，`asdp.rs:586-760`） | 已完成 |
| CP-17 | 三个宿主 SPI 默认实现 | `SessionCompactor.java:20-46` 等注释指向宿主自实现 | `src/adapter/host_impls.rs`：`StoreSessionHistorySource`（`SessionLogStore` + core `transcript`）、`HarnessSessionCompactor`（`ConversationCompactor`）、`TaskRepositoryRuntimeSource`（`TaskRepository`）；测试 `store_history_source_reads_the_native_transcript`、`harness_compactor_rewrites_the_context_and_returns_the_summary`、`task_source_lists_and_cancels_background_tasks` 等 5 个 | 已完成 |
| CP-18 | 运行时审批回环（HITL 映射） | `CollaborationClient.java#awaitRuntimeToolApproval`(L97) + `HarnessAgentTaskStarter.java#callWithApprovals`(L367) | `src/transport/collaboration_client.rs:90-120`（请求 → 1s 轮询 → ack，`RUNTIME_APPROVAL_POLL_INTERVAL`）+ `src/adapter/task_starter.rs:453-486`（`ConfirmHandler`）+ 装配点 `task_starter.rs:616`；测试 6 个（`runtime_approval_polls_until_final_then_acks` 等） | 已完成 |
| CP-19 | 契约 HTTP 服务：16 条路由路径（17 个端点）/ token / 错误码 | `ContractHttpServer.java#route`(L131-333) | `src/transport/contract_http_server.rs:155-187`（16 条 route path，`messages` 同时 GET/POST）；写路由校验 `X-Builder-Internal-Token`（L357/374/391/409/428/448）；错误映射 404/409(busy+wait_idle)/401/501（L193-197）；测试断言 401/409 体（L722/749-755） | 已完成 |
| CP-20 | 协作 / 编排客户端端点对齐 | `CollaborationClient.java`（29 个 public 方法）、`OrchestrationClient.java`（19 个） | `src/transport/collaboration_client.rs`（issue/comments/add_comment/team/request_approval/await/task_context/tools/call_tool/create_child/acknowledge/start/progress/respond/workspace_applied/complete/finish/fail/run/run_graph/complete_run_node/fail_run_node/run_artifacts/replan_run/signal_run/upload_artifact/download_artifact + `X-Agent-Task-Token`）；`src/transport/orchestration_client.rs` 19 个方法逐一对应 | 已完成 |
| CP-21 | store 六组件常量 / 路径 / env 契约 | `ControlPlaneSessionTurnGate.java` L54-58、`ControlPlaneTaskRepository.java` L68-71、`ControlPlaneStores.java#fromEnv` | `src/store/session_turn_gate.rs:27-37`（60s TTL / 20s 续租 / 3s 获取超时 / 30s 回退 / 500ms 重试）、`src/store/task_repository.rs:33-39`（`agent-protocol` / 30s 心跳）、`src/store/control_plane_stores.rs:19-25`（4 个 env 键）、`src/store/base_store.rs`（404→None、409→false） | 已完成 |

## 4. 项目状态与下一步
### 4.1 当前状态
**状态：部分完成（P0 + P1 主干已全部落地并通过构建/测试——含 2026-10-22 关闭的 P0-1 native session-log 提交链路；剩余 P2「设计决定」、宿主实现待定与架构级语义差异）**

**最新验证（2026-10-28）**：`cargo test -p agentscope-extensions-controlplane` → **144 lib + 4 e2e passed / 0 failed / 0 ignored（EXIT=0）**（基线 128 + 4，上轮 **+16**）；`cargo build -p agentscope-extensions-controlplane` 成功。见 history/CONTROLPLANE__MIGRATION_HISTORY.md 的 **P1-1 / P1-2 / P1-3 / P2-2 均已关闭**，明细与反向验证见 **history/CONTROLPLANE__MIGRATION_HISTORY.md**；**P0-1 已于 2026-10-22 关闭**（见 history/CONTROLPLANE__MIGRATION_HISTORY.md：128 lib + 4 e2e），见 history/CONTROLPLANE__MIGRATION_HISTORY.md 六项编号缺口（P0-1 / P1-1 / P1-2 / P1-3 / P2-1 设计决定 / P2-2）至此全部闭环。

**2026-10-31 增量（实测）**：新增 **P1-4**（env 契约：`require_key` 拒绝 blank + trim，对齐 Java `requireEnv`/`envOr`），新增 2 个离线用例；实测 `146 lib + 4 e2e`。同时把 见 history/CONTROLPLANE__MIGRATION_HISTORY.md 三处「类级残留」与 `GrpcTransport`、三个宿主 SPI 逐条判为「已完成（设计决定，不迁移）」并给出源码级依据（见 history/CONTROLPLANE__MIGRATION_HISTORY.md）。主智能体已实测（见 §1）。

**2026-10-31 二轮收口（见 history/CONTROLPLANE__MIGRATION_HISTORY.md）**：① 新增 `src/transport/asdp.rs`——ASDP 语义层（handshake / 重连退避 / 心跳 / ack 门控 outbox / 下行分发），wire 经 `AsdpLink` seam，`GrpcTransport` 的「设计决定」降为**仅 wire 层**；② 新增 `src/adapter/host_impls.rs`——`StoreSessionHistorySource` / `HarnessSessionCompactor` / `TaskRepositoryRuntimeSource` 三个宿主默认实现（原判「不提供通用实现」可撤销）；③ `ControlPlaneError` 新增 `Internal(String)`；④ `AgentScopeAdapter` 在摘要为空时不再发布空 `compaction` 事件。新增 **12 个离线用例**（asdp 7 + host_impls 5），实测 **158 lib + 4 e2e**。

本次会话（2026-10-08）交付：`ControlPlaneObserverMiddleware`（Level-2 事件源）、`AgentScopeAdapter`（框架适配）、适配器辅助 SPI、**`AgentTask*` 工具族（`AgentTaskOutcome`/`State`/`ToolContext`/`AgentTaskOutcomeTool`/`AgentTaskCollaborationTool`）**、**`CollaborationClient`/`OrchestrationClient`**、**harness 侧 `HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`**、**运行时工具审批回环（`RuntimeApprovalSource` + `ConfirmHandler`）**，以及 **`ControlPlaneBridge` 事件 emit 的真接线**（原为 no-op 闭包 → 事件全被吞掉）。

验证命令与结果（本次实测）：`cargo test -p agentscope-extensions-controlplane` → **125 passed / 0 failed**（基线 55，**+70**）；全部离线（无真实网络/控制面）。

#### 4.1.0g 修复：`AgentScopeAdapter::on_bridge_attached` 未重写（装配路径真 bug，2026-10-08）
**问题（交叉审核发现，已源码复核）**：`impl FrameworkAdapter for AgentScopeAdapter` 只实现了 `framework_name`/`capabilities`/`can_handle`/`attach`/`detach`/`extract_context`，**漏了 `on_bridge_attached`**；`attach()` 又把传入的 `_emit` 直接丢弃。而 `bridge.rs:266` 是**唯一**经 `on_bridge_attached` 注 reporter 的入口（trait 默认实现为空），`AgentScopeAdapter` 内对 reporter 的写入点为零。
**后果（生产路径）**：`instrument()` → `bridge.attach()` 装配后 `AdapterShared.reporter` 恒为 `None` → `ControlPlaneObserverMiddleware` 在 `on_agent`/`on_acting` 命中 `let Some(reporter) = ... else { return next(input) }` 提前返回 → **Level-2 事件（session_start/message/tool_call/tool_result/session busy）全部丢失**；`extract_context` 的 `effective_system_prompt` 采样恒空（回落 base prompt）。
**根因**：旧测试直接在 `shared.reporter` 里塞 reporter（`observer_middleware.rs` 测试 helper），绕开了装配路径，所以一直“绿”。与早前「bridge emit 曾为 no-op 闭包」属同类：**测试覆盖了内部状态，却没覆盖装配**。
**修复（对齐 Java `AgentScopeAdapter.java:204` 的 `onBridgeAttached`）**：在 `impl FrameworkAdapter for AgentScopeAdapter` 新增
`fn on_bridge_attached(&mut self, reporter: crate::bridge::BridgeReporter) { *self.shared.reporter.lock()... = Some(reporter); }`（仅新增该方法，未改其它行为）。
**回归测试（新 1 个，走真实装配路径）**：`agent_scope_adapter::tests::bridge_attach_installs_reporter_for_the_observer_middleware` —— 经 `crate::instrument(target, cfg, Box::new(adapter))` 装配后，用 `adapter.middleware()` 跑 `on_agent`，断言 `bridge.is_known_session("s-attach")`。修复前失败（reporter 未装）→ 修复后通过。

#### 4.1.0d 本轮 harness 侧交付（P2 收官）
| 文件 | 类型 | 内容 |
|---|---|---|
| `src/adapter/task_starter.rs` | 新 | `AgentTaskStarter` 的 harness 实现 `HarnessAgentTaskStarter`（去重 `attemptId:generation` → `task.ack` → `ensureRunning`（409 竞态回读） → workspace 定义物化/`workspace.applied` → 注册 `task.submit_result` 与广告协作工具（跳过 `TERMINAL_TOOLS` 与未授权动作） → kickoff prompt → `run_to_outcome` → `cancel_unfinished` → `finish`/`fail`）；`TurnRunner` seam + `HarnessTurnRunner`（`stream_events` 取 `AgentResult.reply`、`TaskRepository` 读写、`SUPPRESS_COMPLETION_CALLBACK`）；纯函数 `input_ids`/`available_actions`/`strip_credentials`/`model_context`/`safe_payload`/`role_instructions`/`kickoff_prompt` |
| `src/adapter/workspace_factory.rs` | 新 | `HarnessWorkspaceAgentFactory`（`create` → 尝试目录 + 物化 + builder(workspace/agent_id/name/sys_prompt/model resolver/max_iters/permission_context/skill_filter) + 工具选择落 toolkit）；`materialize`/`tools`/`permissions`/`tool_name`/`skill_filter`/`apply_tool_selection` |
| `src/transport/collaboration_client.rs` | 改 | 补齐执行器所需端点：`workspace_applied`、`complete`、`finish`、`fail` |
| `src/adapter/mod.rs` | 改 | 接线 `task_starter`/`workspace_factory` 并 re-export |

新测试 12 个（118 − 106），含复现 Java `HarnessAgentTaskOutcomeTest` 的 5 个场景（plain waiting promise → `missing_outcome`；真实依赖等待并回放结果；不存在的依赖 → `dependency_not_found` 不轮询；过早 succeeded → `uncollected_dependencies` + 取消未完成子任务；异常 runtime stop 不得提交 succeeded）。

#### 4.1.0e harness 侧的显式差异（非缺口，已判定）
1. **`HarnessAgentBuilder` 无 MCP 服务器与 allow/deny 工具配置**（Java `ToolsConfig`/`McpServerConfig`）：`tools(definition)` **纯函数照 Java 同构实现**（可测），但只把 allow/deny 落到 `Toolkit::remove_tool`；`mcpServers` 选择集已计算并导出（`ToolsSelection.mcp_servers`），实际建连由宿主/MCP 管理器完成 → §4.2。
2. **MCP 工具名前缀**：Java 用 `server__tool`，Rust harness `McpTool` 用原始工具名 → 本 crate 的 `permissions()` 仍按 Java `server__tool` 生成（定义对齐），工具级过滤按原始名。
3. **`builder.model(String)` vs `builder.model(Arc<dyn Model>)`**：Rust 无按名字构建模型的 builder 入口，改用 `HarnessWorkspaceAgentFactory::with_model_resolver`（应用提供 `Fn(&str) -> Option<Arc<dyn Model>>`）；未注入 resolver 时沿用应用提供 builder 的模型。
4. **审批回环已落地**：Java `awaitRuntimeToolApproval`（轮询 `/runtime-approvals/{id}/decision` + ack）已实现为 `RuntimeApprovalSource` seam + 3 个 task-scoped 端点。但 **Rust core 无 PERMISSION_ASKING 暂停/恢复**（`ReActAgent` 以 `core.confirm_handler` 内联 await 解决 `PermissionBehavior::Ask`；未装 handler 时直接报错 `permission asking (HITL) is not yet supported`）。因此 Java 的 `callWithApprovals`「循环 + `Msg.METADATA_CONFIRM_RESULTS` 恢复」在 Rust 映射为**安装 `ConfirmHandler`**：语义等价（同由控制面决策驱动 allow/deny），且无 Java 的 32 轮上限。差异：控制面不可达时 **deny**（Java 抛异常）—— 失败不 panic，符合 Bypass 原则。
5. **并发模型**：Java 用 reactor `boundedElastic` 订阅使 `start()` 非阻塞；Rust `AgentTaskStarter::start` 为 async 且 `HarnessAgentTaskStarter` 内部全程 `.await`（由调用方决定是否 `tokio::spawn`），语义等价但不再自行线程切换。

#### 4.1.0f 本轮审批回环交付（P2 收官之二）
| 文件 | 类型 | 内容 |
|---|---|---|
| `src/transport/collaboration_client.rs` | 改 | `RuntimeApprovalDecision`（approvalId/decisionVersion/allow/denyMessage）+ `RuntimeApprovalSource` trait（`create_runtime_approval`/`poll_runtime_approval`/`ack_runtime_approval`）+ 自由函数 `await_runtime_tool_approval`（请求 → 轮询至 `decisionVersion>0` → ack）+ 固有方法 `CollaborationClient::await_runtime_tool_approval`；`RUNTIME_APPROVAL_POLL_INTERVAL = 1s`（对齐 Java `Thread.sleep(1000)`） |
| `src/adapter/task_starter.rs` | 改 | `runtime_approval_confirm_handler(source, task_id, token) -> ConfirmHandler`；`HarnessAgentTaskStarter::execute` 在解析出 agent 后安装该 handler |
| `src/transport/mod.rs` | 改 | re-export `await_runtime_tool_approval` / `RuntimeApprovalDecision` / `RuntimeApprovalSource` / `RUNTIME_APPROVAL_POLL_INTERVAL` |

新测试 6 个（124 − 118）：`runtime_approval_polls_until_final_then_acks`、`runtime_approval_surfaces_deny_message`、`runtime_approval_requires_an_approval_id`（transport 层，用 `FakeApprovalSource`）；`approval_handler_allows_when_the_control_plane_approves`、`approval_handler_denies_with_the_control_plane_message`、`approval_handler_denies_when_the_control_plane_is_unavailable`（适配层，直接 await handler 闭包）。对应 Java `AgentTaskCollaborationToolTest` 的 runtime-approval 场景，全部离线。

#### 4.1.0b 本轮 P2 交付文件
| 文件 | 类型 | 内容 |
|---|---|---|
| `src/adapter/agent_task.rs` | 新 | `AgentTaskOutcome`（带 Java 同构校验）+ `AgentTaskOutcomeState`（submit/take/terminal_committed）+ `AgentTaskToolContext` + `AgentTaskOutcomeTool`（impl core `Tool`，name=`task.submit_result`） |
| `src/adapter/collaboration_tool.rs` | 新 | `AgentTaskCollaborationTool`（impl core `Tool`，每 dispatch 绑定 `AgentTaskToolContext`）+ `READ_ONLY_TOOLS`/`TERMINAL_TOOLS` 常量 |
| `src/transport/collaboration_client.rs` | 新 | issue/comment/team/approval + task-scoped（context/ack/start/progress/respond/run/run-graph/run-node/artifacts）+ MCP `tools/list`\|`tools/call`；`X-Agent-Task-Token` 头 |
| `src/transport/orchestration_client.rs` | 新 | definition（create/update/validate/publish/revisions/start）+ run（list/get/graph/events/control/rerun/signal）+ attempt + runtime-policy；`?tenant=&namespace=` 作用域 |

#### 4.1.0a 与 Java 的显式差异（实现中明确处理）
1. **`AgentTool` vs core `Tool`**：Java 协作工具实现 `AgentTool`（`getName`/`getParameters`/`callAsync`）；Rust 统一实现 core `Tool`（`name`/`description`/`parameters_schema`/`call`）。
2. **Mockito 测试替身**：Java 用 Mockito mock `ControlPlaneHttpClient` 逐方法测路径/头；Rust 因不发起真实网络，改为测**纯函数层**（路径/作用域编码、校验、`is_read_only`、不可达时返回 Err 而不 panic）。
3. **`callTool` 的 `_toolCallId`**：Rust `ToolExecutionContext.tool_call` 提供 `id`，已透传为 `_toolCallId`（与 Java 一致）。

已落地文件（`crates/agentscope-extensions-controlplane/src/`）：
- `lib.rs`（模块声明 + `instrument` 入口 + `FRAMEWORK_NAME`）
- `error.rs`（`ControlPlaneError` / `ControlPlaneResult`）
- `config.rs`（`ControlPlaneConfig` + builder + 校验）
- `model/`：`session_event.rs`、`context_snapshot.rs`、`context_tracker.rs`、`inventory.rs`、`message_page.rs`、`agent_task.rs`、`mod.rs`
- `event_journal.rs`（长度前缀 JSON 记录 + 序列号持久化，fsync）
- `transport/`：`control_plane_http_client.rs`、`contract_provider.rs`、`http_self_registration.rs`、**`contract_http_server.rs`（axum，已完成）**、`mod.rs`
- `store/`：`control_plane_stores.rs`、`base_store.rs`、`message_bus.rs`、`async_tool_registry.rs`、`snapshot_spec.rs`、`task_repository.rs`、`session_turn_gate.rs`、`sandbox_execution_guard.rs`、`mod.rs`
- `adapter/`：`framework_adapter.rs`（trait + capability 常量 + `on_bridge_attached` 钩子）、`observer_middleware.rs`、`agent_scope_adapter.rs`、`support.rs`（4 个可选 SPI）、`agent_task.rs`、`collaboration_tool.rs`、**`task_starter.rs`（新，harness ASDP 执行器）**、**`workspace_factory.rs`（新，harness 工作区工厂）**、`mod.rs`
- `bridge.rs`（`ControlPlaneBridge` + **`BridgeReporter`**：attach/start/record_event/close + `describe_session`/`set_busy`/`effective_system_prompt`/`set_context_used_tokens` + `ContractProvider` 实现）

验证命令与结果（本次实测，2026-10-08）：
- `cargo build -p agentscope-extensions-controlplane` → **成功**（exit 0，新代码 0 告警）
- `cargo test -p agentscope-extensions-controlplane` → **125 passed / 0 failed / 0 ignored**（基线 55，**+70**）
- `cargo build --workspace` → **BUILD_EXIT=0**
- `cargo test -p agentscope-core` → 全绿（本次对 core 仅**纯增量加 getter**）

#### 4.1.1 本次变更文件
| 文件 | 类型 | 内容 |
|---|---|---|
| `crates/agentscope-core/src/agent/react_agent.rs` | 改（**跨模块，经用户授权**） | 新增 `ReActAgent::sys_prompt()` / `max_iters()` 两个**只读 getter**；不改任何行为/架构 |
| `src/bridge.rs` | 改 | `journal`/`trackers` 移入 `Arc<Mutex<BridgeState>>`；`attach` 的 emit 由 no-op 改为真发布（assign seq → journal → tracker）；新增 `BridgeReporter` 与 6 个会话元数据方法 |
| `src/adapter/framework_adapter.rs` | 改 | 新增可选钩子 `on_bridge_attached(BridgeReporter)`（默认 no-op） |
| `src/adapter/observer_middleware.rs` | 新 | `ControlPlaneObserverMiddleware` + `AdapterShared`（reporter/started_sessions/session_users） |
| `src/adapter/agent_scope_adapter.rs` | 新 | `AgentScopeAdapter`（`FrameworkAdapter` 实现） |
| `src/adapter/support.rs` | 新 | `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` / `AgentTaskStarter` |
| `src/adapter/mod.rs` | 改 | 声明与 re-export 新模块 |

#### 4.1.2 与 Java 的语义差异（实现中明确处理，非缺口）
1. **多会话 state**：Java `ReActAgent` 按 `(userId, sessionId)` 持有多个 state 槽；Rust core `ReActAgent` 为**单槽**（`agent_state()`）。`resolve_state` 因此：`ReActAgent` 仅当 `state.session_id == 请求 session` 时返回，否则返回空（**绝不误读其它会话**）；`HarnessAgent` 额外经其 `DistributedStore.agent_state_store` 按 `(user, session)` 读持久化槽。
2. **token 来源**：Java 取 `ModelCallEndEvent.getUsage()`；Rust `ModelCallEnd` 为空 `EventData`（不含 usage），故 middleware 改从 `AgentResult` 的 `reply` 消息（`Msg.usage`）取 turn 级 token。
3. **base system prompt**：Java 有 `react.getSysPrompt()`（本次已补 Rust getter）；即**有效 prompt** 仍以 middleware 在 `on_system_prompt` 的采样为准（与 Java 一致）。
4. **压缩摘要标识**：沿用 harness 的 `__compaction_summary__` 按值匹配（与 Java 一致，不 import harness）。

#### 4.1.3 约束遵守
- 本次经**用户明确授权**修改了 `crates/agentscope-core/src/agent/react_agent.rs`（仅新增 2 个 getter，纯增量，未改行为/架构）；其余改动仅限本 crate + 本 tracker。
- **未改** 根 `Cargo.toml` `[workspace.dependencies]`、`crates/agentscope-harness/**`、其它模块 crate、汇总 tracker。
- **未新增任何依赖**（监控 `tokio` 等均已在 `Cargo.toml` 声明）。
- 未删除任何既有功能/测试；未用 `#[ignore]`。

> 历史记录（前次会话，2026-09-18）：`cargo build` 成功 / `cargo test` **55 passed**（P0 链路：模型 + HTTP transport + 7 个 ControlPlane* store + HttpSelfRegistration）。
### 4.2 下一步与阻塞
1. **【本次新增的跨模块改动 — 需汇总方登记】** 本次经**用户明确授权**，修改了 `crates/agentscope-core/src/agent/react_agent.rs`：新增 `ReActAgent::sys_prompt() -> &str` 与 `ReActAgent::max_iters() -> i32` 两个**只读 getter**（对应 Java `getSysPrompt()`/`getMaxIters()`）。**纯增量、未改任何行为/架构**；`cargo test -p agentscope-core` 全绿。原硬约束为「禁改 core」，请汇总方在汇总 tracker §3.3 登记此项。
2. **【仍存在的跨模块架构差异（未改，仅记录）】** Java `ReActAgent` 按 `(userId, sessionId)` 持有多个 state 槽，Rust core 为**单槽**（`agent_state()`）;本次以「`session_id` 不匹配即返回空」+ `HarnessAgent.distributed_store().agent_state_store` 按 `(user, session)` 读持久化槽桥接，**未改 core 架构**。若后续需 core 提供多槽访问器 `agent_state(user_id, session_id)`，需另立任务并重评。
3. **跨模块待办（需汇总方裁定）**：
   - harness `DistributedStore` 字段不足（缺 base_store/sandbox_snapshot_spec/sandbox_execution_guard/message_bus/async_tool_registry/task_repository）→ 见 EXTENSIONS §2.2；本模块的 `ControlPlaneStores` 已能产出各组件，但尚无组合者。
   - harness `SessionTurnGate`（`gateway/session_turn_gate.rs`）/ `SandboxExecutionGuard`（`sandbox/guard.rs`）为具体类型（非 trait）。**已用本 crate 内适配层绕过**（`ControlPlaneSessionTurnGate` 内部委托本地 `SessionTurnGate` 作为不可达降级；`ControlPlaneSandboxExecutionGuard` 为并行托管实现），但**跨模块问题仍待汇总方裁定**：是否应在 harness 定义 trait 以便直接替换。
   - ~~契约 HTTP server（`ContractHttpServer`）需 HTTP server 依赖（axum/hyper 等）~~ → **已解决**：`axum`/`http`/`bytes` 已在根 `[workspace.dependencies]`，`ContractHttpServer` 已落地并测试。
   - ASDP gRPC（`GrpcTransport`）：**语义层已落地**（`transport::asdp`，2026-10-31）；**wire 层仍需 proto 工具链**（`agentscope-service/service-controlplane/internal/asdp/asdp.proto`）→ 宿主实现 `AsdpLink` 即可接入。
   - 若需 EventJournal 的字节级兼容（Java 为 protobuf 记录），需引入 proto/人工字节布局 → 当前采用 JSON 记录（语义等价）。
2. 本模块 P1：**已完成**（`AgentScopeAdapter` + `ControlPlaneObserverMiddleware` + `ControlPlaneBridge` 事件真接线 + `ContractHttpServer`）。剩余为 P2（见下第 3 条）与架构级语义差异（见 §4.1「语义差异」）。
3. 本模块 P2：`AgentTask*` 工具族、`CollaborationClient`/`OrchestrationClient`、`HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`（harness 侧）、**运行时工具审批回环**（`RuntimeApprovalSource` + `ConfirmHandler`）均已交付（2026-10-08）。**仍未完成**：`GrpcTransport`（设计决定暂缓，需 ASDP proto 工具链）；`SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource` 的**宿主具体实现**（仅落 trait 层 —— 这三者是应用/宿主专属能力：压缩策略、`.log.jsonl`  transcript 读取、subagent/plan 视图，crate 内无法在不臆造 harness API 的前提下给出通用实现，故有意保留为注入点）；harness builder 无 MCP 服务器接线 → `mcpServers` 选择集仅计算并导出，实际建连由宿主完成。

**收口结论（2026-10-31，逐条裁决见 history/CONTROLPLANE__MIGRATION_HISTORY.md）**：
- `transport::GrpcTransport`（ASDP gRPC）：**wire 层已完成（设计决定，不迁移），语义层已落地（`transport::asdp`，2026-10-31）**——proto 契约确实存在（`agentscope-service/service-controlplane/internal/asdp/asdp.proto`），但需 `protoc` + `tonic`/`prost` + 构建期拷贝 service proto，且 bidi 流无法经 unary JSON 的 `ControlPlaneHttpClient`；handshake/退避/心跳/ack 门控/下行分发已实现，wire 由宿主实现 `AsdpLink` 注入。
- `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource`：**已完成（三个宿主默认实现已于 2026-10-31 落地于 `adapter::host_impls`）**——`HarnessSessionCompactor`（ConversationCompactor）/ `StoreSessionHistorySource`（SessionLogStore + core `transcript()`）/ `TaskRepositoryRuntimeSource`（harness `TaskRepository`）；`Model`、存储位置、plan-mode handle 仍由宿主注入（trait 覆写）。
- 见 history/CONTROLPLANE__MIGRATION_HISTORY.md 三处类级残留（`SessionBridge.onCommittedEvent` / `AgentScopeAdapter.publishCommitted` / Observer 的 `SessionExportSink` 分支）：**已完成（设计决定，不迁移：不逐方法名镜像 / 分支外置）**——功能链路由 `committed_export.rs` + `BridgeReporter::publish_committed` 承接，`tests/committed_export_e2e.rs` 4 用例覆盖。
- journal 默认目录：**已完成**，保留 `~/.agentscope/service-controlplane/event-journal`（Java `EventJournal.java:80-88` 代码与 `ControlPlaneConfig.java:43` javadoc 自相矛盾，按任务口径取文档值）。
- `acknowledge` 用 `i64`（对应 Java `Integer.MIN_VALUE` 哨兵 ↔ `-1`）：**已完成**，语义等价。
- **仍然存在的跨模块需求（非本 crate 可解，见 history/CONTROLPLANE__MIGRATION_HISTORY.md）**：① export sink 需宿主把 `export_sink_handle()` 装进 runtime context（core 无 builder 级可变注入 API）；② harness builder 无 MCP 服务器接线，`mcpServers` 仅计算导出；③ ASDP **wire 层**需 tonic/prost + protoc（语义层已由 `transport::asdp` 落地）；④ `AgentRuntimeSource` 的 plan-mode hook 需花园持有 `PlanModeHandle`（本 crate 无该句柄，保留 Java 默认）。

### 4.3 测试失败根因分析（历史问题，已修复）

修复前基线：`cargo build` 通过，`cargo test` **53 passed / 2 failed**：

| 失败用例 | 位置 | 现象 |
|---|---|---|
| `store::task_repository::tests::local_task_reaches_completed_without_control_plane` | `src/store/task_repository.rs:1055` | `left: Running` / `right: Completed` |
| `store::task_repository::tests::completion_callback_fires_and_respects_suppression` | `src/store/task_repository.rs:1084` | `left: []` / `right: ["t2"]` |

**根本原因（实测确认，非推测）**：两个测试都用 `ControlPlaneHttpClient::new("http://127.0.0.1:1", ...)`（控制面不可达）。用临时探针实测：向 `http://127.0.0.1:1` 发一次请求在**本机 Windows 上约需 2.03s 才失败**（OS 级 connect 失败，并非 10s 超时）。原 `spawn_local`（约 L405）在开始处 `await update_status(Running)`，而 `update_status` 内部先 `read_record`（HTTP GET ≈2s）再 `persist_record`（HTTP PUT ≈2s），合计 ~4s 才能继续；而 `set_done` 在其后才执行。测试在 `put_task` 返回后仅轮询 2s，故 `is_completed()` 仍为 false → `Running`；回调也因未执行到 `set_done` 后分支而未触发 → `[]`。（注意：本机失败延迟是每次请求恒定 ~2s 的 OS 行为，而非 `HTTP_TIMEOUT=10s`；因此“调小全局超时”既无效也不必要。）

**修复（保持 Java「best-effort 持久化」语义，不删任何逻辑/断言）**：
- 新增 `ControlPlaneTaskRepository::persist_status_detached(after, ...)`（`task_repository.rs`）：把状态持久化放到 `tokio::spawn` 的**分离后台任务**上；传入上一个调用的 `JoinHandle` 作为 `after` 以保证 RUNNING → COMPLETED/FAILED 的写入**顺序**，但不在热路径上 `await`。
- 改造 `spawn_local`：RUNNING 的持久化改为 `persist_status_detached(None, ...)`；`spawn_blocking(execution)` 完成后立即 `set_done`，随即触发 `fire_completion_callback`（本地副作用，立即生效），终态持久化再 `persist_status_detached(Some(running), ...)` 后台按序写入。于是 `set_done` 在毫秒级完成，控制面可达性不再影响本地任务生命周期。
- 未改 `ControlPlaneHttpClient`（含 `HTTP_TIMEOUT`）、未改任何测试断言、未删除持久化逻辑。

**验证**：`cargo test -p agentscope-extensions-controlplane` → **55 passed / 0 failed**（两个原失败用例均转绿）。

### 4.4 与 Java 的已知差异（有意为之，非缺口）

> 本节取代此前散落在 §4.1.0a/§4.1.0e/§4.1.2 的差异散列（**其信息已全部归并到下表**，上述小节保留为历史进程记录）。判定口径：Java 只是基线，Rust 的修复/优化不得因「Java 不是这样」而降级为缺口；仅 JVM 专属、示例性代码、与 Rust 生态重复、已被更优机制替代者进 §4.4.1。

#### 4.4.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| CP-D01 | ASDP gRPC wire 层（protobuf stub 生成 + 双向流） | `GrpcTransport.java` L16-30（生成 `io.agentscope.controlplane.proto` 到本包）、L200-213 `ManagedChannel` + `StreamObserver` bidi | 需 `protoc` + `tonic`/`prost`：`crates/agentscope-extensions-controlplane/Cargo.toml` 依赖清单无 tonic/prost（仅在注释中被提及），且 bidi 流无法经 unary JSON 的 `ControlPlaneHttpClient` 表达。语义层已 1:1 落地（`src/transport/asdp.rs:295-486`：handshake/退避/心跳/ack 门控/下行分发），wire 由宿主实现 `AsdpLink`（`asdp.rs:332-345`）注入 | 宿主需自带 proto 工具链才能启用 ASDP；不注入 link 时事件仅落本地 journal（与 CP-04/CP-13 联动） |
| CP-D02 | JDK `com.sun.net.httpserver` | `ContractHttpServer.java:81` `createContext` | 用 `axum` 0.7 等价替换（`src/transport/contract_http_server.rs:155-187`），路由/状态码/token 校验逐条对齐；JDK 内置 HTTP server 在 Rust 无对应物且不该复刻 | 无（契约语义一致，仅实现栈不同） |
| CP-D03 | reactor `Mono`/`Flux` + `Schedulers.boundedElastic()` | `HarnessAgentTaskStarter.java#start`(L75-79)、`ControlPlaneMessageBus.java#subscribe`(L224) | 用 `async_trait` + tokio 表达；`start` 为 async 由调用方决定是否 `tokio::spawn`（`src/adapter/support.rs:108-111`、`src/adapter/task_starter.rs:717`） | 宿主需自行 spawn；无 `boundedElastic` 的线程池隔离 |
| CP-D04 | Jackson `JsonNode`/`ObjectMapper` | 全模块（如 `HarnessAgentTaskStarter.java:412` `modelContext`） | 用 `serde_json::Value`（`src/transport/collaboration_client.rs`、`src/adapter/task_starter.rs:358-372`）；字段名仍按 Java 的 camelCase 序列化 | 无 |
| CP-D05 | Mockito 逐方法打桩测试 | `src/test` 14 个类（3017 行），如 `HttpSelfRegistrationTest`、`TaskRepositoryContractTest` | Rust 以 trait 注入（`AsdpLink`/`RuntimeApprovalSource`/`SessionLogStore`）+ 纯函数分层 + 桩 HTTP 服务替代；测试数 **158 lib + 4 e2e** 已覆盖同等分支 | 测试形态不同；跨 crate 的真实驱动集成仍缺（如 JDBC 类模块的 Docker 场景） |

#### 4.4.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| CP-X01 | 会话 state 访问 | `src/adapter/agent_scope_adapter.rs:149-176`：`ReActAgent` 单槽，`session_id` 不匹配即返回空（绝不误读其它会话）；`HarnessAgent` 额外经 `distributed_store().agent_state_store` 读持久化槽 | Java `ReActAgent` 按 `(userId, sessionId)` 持多槽，任意会话可直接读 | core 为单槽；「不匹配即空」保证安全（宁缺勿错）。多槽访问器需改 core，另立任务（§4.2 第 2 条） |
| CP-X02 | turn 级 token 来源 | `src/adapter/observer_middleware.rs:190-210`：从 `AgentEvent::AgentResult` 的 `reply` 反序列化 `Msg`，取 `Msg.usage` | Java 取 `ModelCallEndEvent.getUsage()` | Rust `ModelCallEnd` 的 `EventData` 为空（不含 usage），只能从 reply 取 |
| CP-X03 | HITL / 工具审批 | `src/adapter/task_starter.rs:445-486`：`ConfirmHandler` 内联 await 控制面决策（`ok && allow` → `ConfirmResult::allowed()`，否则 `denied`），装配点 L616；控制面不可达 → **deny** | Java 以 `GenerateReason.PERMISSION_ASKING` 暂停 + `Msg.METADATA_CONFIRM_RESULTS` 恢复（`callWithApprovals`，32 轮上限）；不可达时**抛异常** | Rust core 无 PERMISSION_ASKING 暂停/恢复路径；内联 await 语义等价且无 32 轮上限；deny 而非抛错符合 bypass 原则（报告失败不得中断对话） |
| CP-X04 | 事件持久化格式与 ack 类型 | `src/event_journal.rs` 长度前缀 **JSON** 记录 + 序列号 + sha256 身份；`acknowledge(&BTreeMap<String,i64>)` 以 `seq > floor` 判定保留（`event_journal.rs:151-168`，重写前先持久化序列检查点，失败则整批保留） | Java 追加 protobuf 记录、`acknowledge(Map<String,Integer>)`（32 位）；Rust 用 `i64` 且**内存待发队列仅在重写成功后更新**（Java 先改内存） | 语义等价且崩溃更安全（`event_journal` 测试覆盖 ghost seq / 部分 ack / 空 map 为 no-op）；不承诺字节级兼容 |
| CP-X05 | 提交式事件链路的方法形态 | `BridgeReporter::publish_committed(source, native_seq, event)`（`src/bridge.rs:186`）+ `committed_export::ControlPlaneCommittedEventSink`（`src/committed_export.rs`），经宿主 `bridge.export_sink_handle()` 装入 runtime context | Java `SessionBridge.onCommittedEvent` + `AgentScopeAdapter.publishCommitted` + observer 在 `onAgent` 内按调用安装匿名 `SessionExportSink`（`ControlPlaneObserverMiddleware.java:88-115`，名字拼接 `controlplane-native-v1`） | 不逐方法名镜像（已裁定）；sink 由 builder `.extension()` 外置，避免 middleware 每次调用改写 ctx；sink 名固定为 `controlplane-native-v1`（`committed_export.rs:29`），同名不同进程不会串游标 |
| CP-X06 | compress 的返回与事件时机 | `SessionCompactor::compact(...) -> ControlPlaneResult<String>`（`src/adapter/support.rs:20-24`）；摘要 trim 后为空则**不发** `compaction` 事件（`agent_scope_adapter.rs:441-451`） | Java `Mono<Void>`，压缩后**无条件** `publish(COMPACTION)`（`AgentScopeAdapter.java:518-531`），摘要由 `summaryOf(state)` 事后取 | 空摘要发事件会让控制面误判发生过重写；返回摘要更直接（同时用于事件 content） |
| CP-X07 | 会话轮次锁 / 沙箱执行守卫 | `src/store/session_turn_gate.rs`、`src/store/sandbox_execution_guard.rs`：crate 内适配层（内部委托本地 `SessionTurnGate` 作不可达降级） | Java 直接实现 harness 的 `SessionTurnGate`/`SandboxExecutionGuard` 接口 | harness 侧两者是**具体 struct 非 trait**（§4.2），无法直接替换；常量与 Java 一致（60s TTL / 20s 续租 / 3s 获取超时 / 30s 回退 / 500ms 重试） |
| CP-X08 | 并发原语 | `Arc<Mutex<BridgeState>>`（`src/bridge.rs:44-56`）+ 休眠恢复（`unwrap_or_else(|p| p.into_inner())`） | Java `synchronized (lock)` + `ScheduledExecutorService` 定时器 | 报告失败不得使桥不可用：poison 恢复替代异常传播；定时器随 CP-04 以 tokio interval 补 |
| CP-X09 | 业务结果校验的错误类型 | `AgentTaskOutcome::new(...) -> Result<Self, String>`，消息文本与 Java 逐字一致（`src/adapter/agent_task.rs:55-78`） | Java 抛 `IllegalArgumentException` | Rust 无受检异常，返回值便于工具层转成 `ToolResultBlock` 错误而不 panic |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-controlplane` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-controlplane` / `cargo test -p agentscope-extensions-controlplane` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-controlplane/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/CONTROLPLANE__MIGRATION_HISTORY.md`](./history/CONTROLPLANE__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
