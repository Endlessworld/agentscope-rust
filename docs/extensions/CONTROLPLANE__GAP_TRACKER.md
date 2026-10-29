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
| 当前状态 | **总判：已完成**（P1 清零，余 P2 / 待裁定）—— 历史：P0 链路 + **P1 主干 + P2 主体**已落地（模型 + HTTP transport + 7 个 ControlPlane* store + HttpSelfRegistration + `ContractHttpServer` + `AgentScopeAdapter` + `ControlPlaneObserverMiddleware` + `ControlPlaneBridge` 事件真接线 + **`AgentTask*` 工具族 + `CollaborationClient`/`OrchestrationClient`**；2026-10-08 `cargo build` 通过、`cargo test` **106/106 全绿** → 后续会话 **125 passed** → P0-1 后 **128 lib + 4 e2e** → **2026-10-28：144 lib + 4 e2e 全绿（P1-1/P1-2/P1-3/P2-2 关闭，§8）**；**P0-1（native session-log 提交链路）已于 2026-10-22 关闭（§7：128 lib + 4 e2e）**，§6.3 全部编号项至此闭环，仅余 P2 设计决定与宿主实现待定） |

## 2. Java 清单与依赖映射

### 2.0 源文件清单（controlplane 53 个 .java：主 39 / 测 14；旧 controlplane 同为 53 个：主 39 / 测 14）
> 下表 Java 类名一列同时给出**旧 controlplane 名**与**现行 controlplane 名**（`→` 后为新名）；Rust 对应列使用 `ControlPlane*` 现行命名（**2026-10-22 已改名**，见汇总 §4.5 裁定 3）。

根包 `io.agentscope.extensions.controlplane`（旧 `io.agentscope.extensions.controlplane`）：
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `ControlPlane`（旧 `ControlPlane`） | 入口：`instrument(target, config[, adapter])` / `newBridge` | `lib.rs::instrument`（等价 `ControlPlaneBridge::new`) | ✅ 薄封装 |
| `ControlPlaneConfig`（旧 `ControlPlaneConfig`） | 连接/上报设置 record（**20 字段**，校验 agentKey/tenant/namespace/instanceKey） | `config::ControlPlaneConfig`（builder + 同校验，**20 字段一一对应**） | ✅ |
| `SessionBridge` (1207L) | 核心桥：挂载 adapter、事件序列化、Level-1/2/4 上报、命令处理、start/close | `bridge::ControlPlaneBridge`（async/tokio） | ✅ 已完成（P1 主干）；`publish_committed`/`committed_event_sink()`/`export_sink_handle()` 已落地（2026-10-22 关闭 §6.3 P0-1，§7）；类级残留见 §6.2 |
| `FrameworkAdapter` (156L) | 框架适配 SPI（13 capability 常量 + attach/detach + extractContext/listMessages...） | `adapter::FrameworkAdapter`（async_trait，**13 capability 常量逐一对应**） | ✅ trait |
| `EventJournal` (250L) | Level-2 事件崩溃安全本地 outbox（长度前缀记录 + 序列号 + sha256 身份） | `event_journal::EventJournal` | ✅ 已完成（含 `source_watermark`/`track_source`/`acknowledge` + 临时文件 rename 原子重写，§6.3 P1-2 已完成） |

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
| `ControlPlaneStores` (189L) | 工厂：`fromEnv`/`create`/`from(config)` + `withAgentStateStore` | `store::ControlPlaneStores` | ✅ env 键已对齐 `CONTROL_PLANE_HTTP` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` / `CONTROL_PLANE_NAMESPACE`（§6.3 P1-1 已完成；`from_lookup` 可离线断言键名与顺序） |
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
| `CollaborationClient` (502L) | 协作/编排客户端 | ✅ 已完成 | `src/transport/collaboration_client.rs`（issue/team/approval + task-scoped + MCP `tools/call`；`X-Agent-Task-Token`）；**4 端点已补齐**：`replan_run`/`signal_run`/`upload_artifact`/`download_artifact`（§6.3 P1-3 已完成，下载走 `send_for_bytes` 二进制通道） |
| `OrchestrationClient` (137L) | 编排客户端（definition/run/attempt/runtime-policy） | `src/transport/orchestration_client.rs`（`?tenant=&namespace=` 作用域） | ✅ **19 方法与 Java 逐一对应** |

子包 `adapter`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeAdapter` (936L) | AgentScope 框架适配（覆盖全部 capability） | `adapter::AgentScopeAdapter` | ✅ 已完成；`publishCommitted` 语义由 `BridgeReporter::publish_committed` 承接（2026-10-22 关闭 §6.3 P0-1，§7）；类级残留见 §6.2 |
| `ControlPlaneObserverMiddleware`（旧 `ControlPlaneObserverMiddleware`，267L） | 观察中间件（Level-2 事件源） | `adapter::ControlPlaneObserverMiddleware` | ✅ 已完成；native session-log 分支由 `committed_export::ControlPlaneCommittedEventSink` 承接（2026-10-22 关闭 §6.3 P0-1，§7）；类级残留见 §6.2 |
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
| ASDP gRPC `GrpcTransport` | 同名类 | 设计决定（不迁移） | 需 ASDP proto 工具链，P2；§4.2 |

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
| native session-log 提交链路 | `SessionBridge.onCommittedEvent` + `AgentScopeAdapter.publishCommitted` + Observer 的 `SessionExportSink` 分支 | ✅ 已完成（2026-10-22） | `src/committed_export.rs`：`ControlPlaneCommittedEventSink` 实现 core `SessionExportSink`，`translate` 镜像 `publishNative` 白名单（message/tool_call/tool_result/compaction）；`BridgeReporter::publish_committed` 按 per-source watermark 去重后入 journal；宿主经 `export_sink_handle()` 显式接线（§7 / §7.4） |
| 适配器辅助 SPI | `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` / `AgentTaskStarter` | ✅ 已完成（trait 层） | `src/adapter/support.rs`；`AgentTaskStarter` 已由 `HarnessAgentTaskStarter` 具体实现 |
| harness ASDP 执行器 | `HarnessAgentTaskStarter` (27.5KB) | ✅ 已完成（P2） | `src/adapter/task_starter.rs`；带去重（`attemptId:generation`）、`task.ack`、`ensureRunning`（409 竞态）、workspace 定义物化、协作工具注册、`run_to_outcome` 续跑预算、`cancel_unfinished` 清理、`finish`/`fail` 上报 |
| harness 工作区工厂 | `HarnessWorkspaceAgentFactory` (11.7KB) | ✅ 已完成（P2） | `src/adapter/workspace_factory.rs`；`materialize` 拒绝绝对路径/穿越/运行态段/私有内存/符号链接；`tools`/`permissions` 纯函数与 Java 同构 |
| 运行时工具审批回环 | `CollaborationClient.awaitRuntimeToolApproval` | ✅ 已完成（P2） | `src/transport/collaboration_client.rs`：`RuntimeApprovalSource` seam + 3 个 task-scoped 端点（`runtime-approvals` / `.../{id}/decision` / `.../{id}/ack`）+ 自由函数 `await_runtime_tool_approval`（请求→轮询→ack）；适配层经 core 原生 `ConfirmHandler` 接入（Java 的 PERMISSION_ASKING 暂停/恢复在 Rust core 不存在，见 §4.1.0f） |
| `ControlPlane` 入口 | 同名类 | ✅ 已完成（薄封装） | `instrument` = new+attach+start |

## 4. 项目状态与下一步
### 4.1 当前状态
**状态：部分完成（P0 + P1 主干已全部落地并通过构建/测试——含 2026-10-22 关闭的 P0-1 native session-log 提交链路；剩余 P2「设计决定」、宿主实现待定与架构级语义差异）**

**最新验证（2026-10-28）**：`cargo test -p agentscope-extensions-controlplane` → **144 lib + 4 e2e passed / 0 failed / 0 ignored（EXIT=0）**（基线 128 + 4，本轮 **+16**）；`cargo build -p agentscope-extensions-controlplane` 成功。§6.3 的 **P1-1 / P1-2 / P1-3 / P2-2 均已关闭**，明细与反向验证见 **§8**；**P0-1 已于 2026-10-22 关闭**（§7：128 lib + 4 e2e），§6.3 六项编号缺口（P0-1 / P1-1 / P1-2 / P1-3 / P2-1 设计决定 / P2-2）至此全部闭环。

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
   - ASDP gRPC（`GrpcTransport`）需 proto 生成工具链 + proto 文件（`agentscope-service/controlplane/internal/asdp`）→ 暂缓（P2）。
   - 若需 EventJournal 的字节级兼容（Java 为 protobuf 记录），需引入 proto/人工字节布局 → 当前采用 JSON 记录（语义等价）。
2. 本模块 P1：**已完成**（`AgentScopeAdapter` + `ControlPlaneObserverMiddleware` + `ControlPlaneBridge` 事件真接线 + `ContractHttpServer`）。剩余为 P2（见下第 3 条）与架构级语义差异（见 §4.1「语义差异」）。
3. 本模块 P2：`AgentTask*` 工具族、`CollaborationClient`/`OrchestrationClient`、`HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`（harness 侧）、**运行时工具审批回环**（`RuntimeApprovalSource` + `ConfirmHandler`）均已交付（2026-10-08）。**仍未完成**：`GrpcTransport`（设计决定暂缓，需 ASDP proto 工具链）；`SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource` 的**宿主具体实现**（仅落 trait 层 —— 这三者是应用/宿主专属能力：压缩策略、`.log.jsonl`  transcript 读取、subagent/plan 视图，crate 内无法在不臆造 harness API 的前提下给出通用实现，故有意保留为注入点）；harness builder 无 MCP 服务器接线 → `mcpServers` 选择集仅计算并导出，实际建连由宿主完成。

**真实剩余（2026-10-29 复核，均已在册，非新缺口）**：
- `transport::GrpcTransport`（ASDP gRPC）：仍为「设计决定（不迁移）」——需 ASDP proto 工具链 + proto 文件；**该「设计决定」待汇总方最终确认**（§3.2 / §6.2）。
- `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource`：仅 trait 层（`adapter::support`），**无宿主具体实现**（§6.2 三处 ⚠️）。
- 宿主接线仍需显式：export sink（`export_sink_handle()`）与 builder extensions 均无 core 的可变注入 API（§7.4）。
- harness builder 无 MCP 接线；`mcpServers` 仅计算导出，建连归宿主。
- §6.2 三处类级残留：`SessionBridge.onCommittedEvent` / `AgentScopeAdapter.publishCommitted` / Observer 的 `SessionExportSink` 分支——功能已由 `committed_export.rs` + `BridgeReporter::publish_committed` 承接，仅未逐方法名镜像。
- journal 默认目录口径分歧：Rust 取 `~/.agentscope/service-controlplane/event-journal`，Java `EventJournal.java:80-88` 代码实际拼 `~/.agentscope/controlplane/event-journal`（javadoc 写前者）——是否回改待汇总方裁定（§6.3 P2-2）。
- `acknowledge` 用 `i64`（对应 Java `Integer.MIN_VALUE` 语义），非 `Option<u64>`（§8.6 第 2 条）。

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

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-controlplane` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-controlplane` / `cargo test -p agentscope-extensions-controlplane` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-controlplane/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

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
| `SessionBridge` | 1207 | `bridge::ControlPlaneBridge` | ⚠️ 部分 | **类级残留（非功能缺口）**：Java `onCommittedEvent(source, nativeSeq, event)` 在 Rust 侧由 `BridgeReporter::publish_committed` + `committed_event_sink()`/`export_sink_handle()` 承接，功能链路已落地（2026-10-22 关闭 §6.3 P0-1，§7），**仅未逐方法名镜像** |
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
| `transport::GrpcTransport` | 404 | — | ❌ 缺 | 设计决定（不迁移）：需 ASDP proto 工具链，P2 |
| `transport::CollaborationClient` | 502 | `transport::CollaborationClient` | ✅ 已对齐 | 29 个端点全部落地（含 `replanRun`/`signalRun`/`uploadArtifact`/`downloadArtifact`，2026-10-28） |
| `transport::OrchestrationClient` | 137 | `transport::OrchestrationClient` | ✅ 已对齐 | 19 方法逐一对应 |
| `adapter::AgentScopeAdapter` | 936 | `adapter::AgentScopeAdapter` | ⚠️ 部分 | **类级残留（非功能缺口）**：`publishCommitted` 转发由 `BridgeReporter::publish_committed` 承接（2026-10-22，§7），未在 adapter 上保留同名方法 |
| `adapter::ControlPlaneObserverMiddleware` | 267 | `adapter::ControlPlaneObserverMiddleware` | ⚠️ 部分 | **类级残留（非功能缺口）**：native session-log 分支由独立模块 `committed_export::ControlPlaneCommittedEventSink`（`SessionExportSink` 注入 + `translate` 镜像 `publishNative`）承接，未内嵌进 Observer 本体（2026-10-22，§7） |
| `adapter::HarnessAgentTaskStarter` | 533 | `adapter::HarnessAgentTaskStarter` | ✅ 已对齐 | 12 单测含复现 Java 5 场景 |
| `adapter::HarnessWorkspaceAgentFactory` | 229 | `adapter::HarnessWorkspaceAgentFactory` | ✅ 已对齐 | |
| `adapter::AgentTaskOutcome` | 63 | `adapter::AgentTaskOutcome` | ✅ 已对齐 | |
| `adapter::AgentTaskOutcomeTool` | 70 | `adapter::AgentTaskOutcomeTool` | ✅ 已对齐 | |
| `adapter::AgentTaskToolContext` | 22 | `adapter::AgentTaskToolContext` | ✅ 已对齐 | |
| `adapter::AgentTaskCollaborationTool` | 110 | `adapter::AgentTaskCollaborationTool` | ✅ 已对齐 | `READ_ONLY_TOOLS`/`TERMINAL_TOOLS` 一致 |
| `adapter::AgentTaskStarter` | 27 | `adapter::support::AgentTaskStarter` | ✅ 已对齐 | trait |
| `adapter::AgentRuntimeSource` | 56 | `adapter::support::AgentRuntimeSource` | ⚠️ 部分 | 仅 trait；宿主实现待定（P2，已判定非缺口） |
| `adapter::SessionCompactor` | 56 | `adapter::support::SessionCompactor` | ⚠️ 部分 | 同上 |
| `adapter::SessionHistorySource` | 35 | `adapter::support::SessionHistorySource` | ⚠️ 部分 | 同上 |
| `adapter::WorkspaceAgentFactory` | 29 | `adapter::support::WorkspaceAgentFactory` | ✅ 已对齐 | 由 `HarnessWorkspaceAgentFactory` 实现 |

**统计（2026-10-28 更新）**：39 个 Java 类 → **✅ 已对齐 32 / ⚠️ 部分 6 / ❌ 缺 1**（❌ 缺的 `GrpcTransport` 为已判定的「设计决定（不迁移）」；6 项 ⚠️ 中 3 项是「仅 trait 层、宿主实现待定」的既有 P2 判定，其余 3 项为 **P0-1 已关闭后的类级残留**（`SessionBridge` / `AgentScopeAdapter` / `ControlPlaneObserverMiddleware`：功能链路已由 `committed_export.rs` + `BridgeReporter::publish_committed` 承接，仅未按 Java 方法名逐字镜像；2026-10-22 关闭，见 §6.3 P0-1 / §7））。

### 6.3 新发现缺口清单

| 编号 | 缺口项 | Java 参考 | 优先级 | 状态 | 说明 |
|---|---|---|---|---|---|
| P0-1 | **native session-log 提交链路未迁移** | `SessionBridge.onCommittedEvent` / `EventJournal.sourceWatermark` / `ControlPlaneObserverMiddleware` 的 `SessionExportSink` 分支 / `AgentScopeAdapter.publishCommitted` | P0 | **已完成（2026-10-29 勘误）**：原记「待办」，实际已于 **2026-10-22** 落地并关闭；依据见 §7 / 用例 `committed_events_from_a_real_run_reach_the_journal_once`、`duplicate_native_watermark_exports_once`、`source_watermark_is_durable_across_bridge_reopen`、`no_sink_configured_is_a_noop`（128 lib + 4 e2e）。落地形态：`src/committed_export.rs`（`ControlPlaneCommittedEventSink` 实现 core `SessionExportSink`，`translate` 镜像 `publishNative` 白名单）+ `BridgeReporter::publish_committed`（镜像 `onCommittedEvent`/`publishCommitted`，按 per-source watermark 去重）+ `EventJournal::source_watermark` + core `SessionExportSinkHandle` / `SessionLogExporter::for_session` 注入点；**宿主接线仍显式**（`export_sink_handle()` 需宿主装进 builder/session context，§7.4）。以下为 2026-10-22 复核时的原始判断，保留备查：Java 在 `ReActAgent.sessionLogEnabled()` 时改走「核心 session log → 原生导出 sink → journal 强制落盘 → 按 source watermark 去重」的提交链路；Rust 侧 `agentscope-core` 已有 `SessionExportSink` / `SessionLogExporter`（`crates/agentscope-core/src/session/export.rs`），但**本 crate 完全没有引用**（`rg SessionExportSink crates/agentscope-extensions-controlplane` 无命中），且 `SessionLogExporter::new` 在 core 内**除测试外无生产调用方**（`rg SessionLogExporter::new crates/` 仅命中 `export.rs` 自身测试）。**已核实的 Rust 接入点**：`ReActAgent::session_log_enabled()`（`react_agent.rs:308`）、`HarnessAgent::session_log(context)`（`harness_agent.rs:214`）、`RuntimeContext::extension::<T>(key)` / `Extensions::insert_arc`（`runtime_context.rs:45/258`）、`SessionRecorder::open_with(log, turn_id, run_id, Option<Arc<SessionLogExporter>>)`（`recorder.rs:213`）——**后者是现成的 exporter 注入位**。后果：Rust 侧 Level-2 事件仍只来自 `on_agent`/`on_acting` 旁路 tap，无法消费核心已提交事件的原生流；与 Java 新基准不等价 |
| P1-1 | env 变量名未随重命名迁移 | `ControlPlaneStores.fromEnv` 用 `CONTROL_PLANE_HTTP` / `CONTROL_PLANE_AGENT_NAME` / `CONTROL_PLANE_NAMESPACE` | P1 | 已完成（2026-10-28） | 核对结论：Rust `from_env` 已使用同名键（无 `CONTROLPLANE_*` 遗留），本轮补 `from_lookup` 健壮点 + 2 个离线键名断言测试。依据：Java `ControlPlaneStores.java:65-70`（`requireEnv("CONTROL_PLANE_HTTP")` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` + `envOr("CONTROL_PLANE_NAMESPACE","default")`） |
| P1-2 | EventJournal 缺 source watermark + 选择性 ack | `EventJournal.sourceWatermark(String)` / `trackSource` / `acknowledge(Map<String,Integer>)` | P1 | 已完成（2026-10-28） | watermark/trackSource 已在 P0-1 落地（本次核对语义与 Java `sourceWatermarks.merge(..., Math::max)` + `__native__` 检查点一致）；本轮补 `acknowledge(&BTreeMap<String,i64>)`（空 map = no-op、先落序列检查点、只丢 `seq <= floor`、未知会话不误丢、临时文件 + rename 原子重写、失败时内存 backlog 不丢）+ `ControlPlaneBridge::acknowledge`。依据：Java `EventJournal.java:138-148`（acknowledge）、`:200-256`（persistSequences/rewrite 原子写）、`SessionBridge.java:611-639`（onEventAck） |
| P1-3 | `CollaborationClient` 缺 4 个端点 | `replanRun` / `signalRun` / `uploadArtifact` / `downloadArtifact` | P1 | 已完成（2026-10-28） | 4 端点逐一对照：`POST run/replan`、`POST run/signals/{name}`（`idempotencyKey` + `payload` 缺省 `{}`）、`POST /api/v1/artifacts/uploads`（multipart，`sourceTaskId`/`targetType`/`targetRef` + `file`）、`POST /api/v1/artifacts/{id}/download?taskId=`（二进制）。新增 `send_bytes_with_headers` 与 `BytesResponse.content_type`，下载返回 `ArtifactContent { content_type, bytes }`，不经 `String`。依据：Java `CollaborationClient.java:323-421` |
| P2-1 | crate 名与类型名未随 Java 重命名 | Java → `controlplane` | P2 | **已完成（2026-10-22 改名）** | 建议**保留 Rust 侧 `controlplane` 命名**（crate 名、类型名），仅在文档/注释标注「对应 Java `agentscope-extensions-controlplane`」；理由：Rust 侧改名会波及 crate 名、import 路径与全部下游引用，收益仅是对齐命名，且本模块 Rust 无 controlplane crate 的对外依赖方需求；2026-10-29 勘误：原记「建议保留 Rust 侧 controlplane 命名 / Rust 无 controlplane crate」，实际 crate 名、`ControlPlane*` 类型前缀、`CONTROL_PLANE_*` env、journal 目录、sink 名均已按汇总 §4.5 裁定 3 完成改名，本条不再适用 |
| P2-2 | 默认 journal 目录名未迁移 | Java `~/.agentscope/service-controlplane/event-journal` | P2 | 已完成（2026-10-28，已核） | Rust `event_journal.rs::default_journal_dir()` 已是 `~/.agentscope/service-controlplane/event-journal`（P0-1 时已改），本轮补断言测试。**Java 侧不一致已记录**：`ControlPlaneConfig.java:43` javadoc 写 `service-controlplane`，但 `EventJournal.java:80-88` 代码实际拼 `~/.agentscope/controlplane/event-journal`——属本地 outbox，不跨语言共享，按文档化约定（= 任务口径）保留 `service-controlplane`，是否回改为 Java 字面值待汇总方裁定 |

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
3. §6.3 **P0-1 的尾部**（`SessionBridge`/`AgentScopeAdapter`/`ControlPlaneObserverMiddleware` ⚠️）与 `GrpcTransport` 不变，仍按原判定。
4. 本轮工作区中其它 crate 的改动（core/jdbc/higress/judge/mem/rag/redis/sandbox/studio 等）**非本会话产生**（并发会话），未触碰。



