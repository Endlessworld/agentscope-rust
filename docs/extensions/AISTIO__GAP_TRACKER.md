# AISTIO 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **AISTIO** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-aistio\`
> **Java 子模块**：单模块，主包 `io.agentscope.extensions.aistio`，子包 `adapter` / `model` / `store` / `transport`
> **Rust 目标**：`crates/agentscope-extensions-aistio/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-aistio` |
| Java 子模块 | 单模块：`adapter` / `model` / `store` / `transport`（+ 根包 6 类） |
| Java 规模（初测） | 主 37 文件 ~9352 行 / 测 16 文件 ~2841 行（共 53 .java） |
| Rust crate | `crates/agentscope-extensions-aistio` |
| 迁移波次 | C |
| 当前状态 | 进行中（P0 链路 + **P1 主干 + P2 主体**已落地：模型 + HTTP transport + 7 个 ControlPlane* store + HttpSelfRegistration + `ContractHttpServer` + `AgentScopeAdapter` + `AistioObserverMiddleware` + `AistioBridge` 事件真接线 + **`AgentTask*` 工具族 + `CollaborationClient`/`OrchestrationClient`**；`cargo build` 通过，`cargo test` **106/106 全绿**） |

## 2. Java 清单与依赖映射

### 2.0 源文件清单（53 个 .java：主 37 / 测 16）
根包 `io.agentscope.extensions.aistio`：
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `Aistio` | 入口：`instrument(target, config[, adapter])` / `newBridge` | `lib.rs::instrument`（等价 `AistioBridge::new`) | ✅ 薄封装 |
| `AistioConfig` | 连接/上报设置 record（20 字段，校验 agentKey/tenant/namespace/instanceKey） | `config::AistioConfig`（builder + 同校验） | ✅ |
| `SessionBridge` (1177L) | 核心桥：挂载 adapter、事件序列化、Level-1/2/4 上报、命令处理、start/close | `bridge::AistioBridge`（async/tokio） | ✅ 已完成（P1 主干） |
| `FrameworkAdapter` | 框架适配 SPI（capabilities + attach/detach + extractContext/listMessages...） | `adapter::FrameworkAdapter`（async_trait） | ✅ trait |
| `EventJournal` | Level-2 事件崩溃安全本地 outbox（长度前缀记录 + 序列号 + sha256 身份） | `event_journal::EventJournal` | ✅ |

子包 `model`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `SessionEvent` | Level-2 事件（seq/type/role/content/tool*/tokens/duration/meta） | `model::SessionEvent` | ✅ |
| `ContextSnapshot` | Level-4 有效上下文 + `computeHash()`（SHA-256 排序 JSON 前 16 hex） | `model::ContextSnapshot::hash()` | ✅ |
| `ContextTracker` | 从事件流增量维护有效上下文视图 + contextHash | `model::ContextTracker` | ✅ |
| `Inventory` | 实例级 inventory（subagents/workspaces/health） | `model::Inventory` | ✅ |
| `MessagePage` | Level-3 全量历史分页 | `model::MessagePage::of` | ✅ |
| `AgentTaskAssignment` | ASDP 执行信封（attemptId/agentTaskId/...） | `model::AgentTaskAssignment` | ✅ |

子包 `store`（ControlPlane 托管存储，实现 harness 既有 trait，不重建平行类型）：
| Java 类 | 职责 | Rust 对应（实现既有 trait） | 状态 |
|---|---|---|---|
| `ControlPlaneStores` | 工厂：`fromEnv`/`create`/`from(config)` + `withAgentStateStore` | `store::ControlPlaneStores` | ✅ |
| `ControlPlaneBaseStore` | `/api/v1/dp/kv/*` → harness `BaseStore` | `store::ControlPlaneBaseStore` impl `BaseStore` | ✅ |
| `ControlPlaneMessageBus` | `/api/v1/dp/bus/*` → harness `MessageBus`（Mode D 轮询降级） | `store::ControlPlaneMessageBus` impl `MessageBus` | ✅ |
| `ControlPlaneAsyncToolRegistry` | `/api/v1/dp/async-tools/*` → harness `AsyncToolRegistry` | `store::ControlPlaneAsyncToolRegistry` impl `AsyncToolRegistry` | ✅ |
| `ControlPlaneTaskRepository` (799L) | `/api/v1/dp/tasks/*` → harness `TaskRepository`（本地 handle + CP 持久化 + 心跳） | `store::ControlPlaneTaskRepository` impl `TaskRepository` | ✅ 已完成 |
| `ControlPlaneSessionTurnGate` | `/api/v1/dp/locks/*` → harness `SessionTurnGate`（20s 续租/60s TTL/30s 回退本地） | `store::ControlPlaneSessionTurnGate`（crate 内适配层；harness gate 为具体 struct 非 trait，本地 `SessionTurnGate` 作降级回退，见 §4.2） | ✅ 已完成 |
| `ControlPlaneSandboxExecutionGuard` | `/api/v1/dp/locks/*` → harness `SandboxExecutionGuard` | `store::ControlPlaneSandboxExecutionGuard`（crate 内适配层；harness guard 为具体 struct 非 trait，见 §4.2） | ✅ 已完成 |
| `ControlPlaneSnapshotSpec` | `/api/v1/dp/snapshots/*` → harness `SandboxSnapshotSpec`/`SandboxSnapshot` | `store::ControlPlaneSnapshotSpec` impl `SandboxSnapshotSpec` | ✅ |

子包 `transport`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `ControlPlaneHttpClient` | JDK HttpClient 包装，加 `X-Builder-Internal-Token`，10s 超时 | `transport::ControlPlaneHttpClient`（reqwest） | ✅ |
| `ContractProvider` | 契约服务数据提供者 SPI | `transport::ContractProvider`（trait） | ✅ |
| `HttpSelfRegistration` | `POST /api/v1/agent-registrations` + 周期心跳 + 身份回调 | `transport::HttpSelfRegistration`（+ `RegisteredIdentity`/`RegistrationOptions`/`parse_registered_identity`） | ✅ 已完成 |
| `ContractHttpServer` | 进程内 `/agentscope/*` HTTP 契约服务 | ✅ 已完成（axum） | `src/transport/contract_http_server.rs`（27KB）+ `transport/mod.rs` re-export；此前 tracker 标注滞后 |
| `GrpcTransport` | ASDP gRPC 上游通道（protobuf） | 设计决定（P2 暂缓：需 ASDP proto toolchain） | 设计决定 |
| `CollaborationClient` / `OrchestrationClient` | 协作/编排客户端 | ✅ 已完成 | `src/transport/collaboration_client.rs`（issue/team/approval + task-scoped + MCP `tools/call`；`X-Agent-Task-Token`）+ `src/transport/orchestration_client.rs`（definition/run/attempt/runtime-policy，`?tenant=&namespace=` 作用域）；均离线（无真实网络调用） |

子包 `adapter`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeAdapter` (928L) | AgentScope 框架适配（覆盖全部 capability） | `adapter::AgentScopeAdapter` | ✅ 已完成 |
| `AistioObserverMiddleware` | 观察中间件（Level-2 事件源） | `adapter::AistioObserverMiddleware` | ✅ 已完成 |
| `HarnessAgentTaskStarter` / `HarnessWorkspaceAgentFactory` | harness 任务启动/工作区 agent 工厂 | ✅ 已完成 | `src/adapter/task_starter.rs`（`HarnessAgentTaskStarter` impl `AgentTaskStarter` + `TurnRunner` seam + `run_to_outcome` 续跑循环 + `HarnessTurnRunner`）+ `src/adapter/workspace_factory.rs`（`HarnessWorkspaceAgentFactory` + `materialize`/`tools`/`permissions`/`skill_filter`/`apply_tool_selection`）；12 个新单测（含复现 Java `HarnessAgentTaskOutcomeTest` 的 5 个场景） |
| `AgentTask*` 工具族（5 类） | AgentTask 工具/上下文/产物 | ✅ 已完成 | `src/adapter/agent_task.rs`（`AgentTaskOutcome`/`State`/`ToolContext`/`AgentTaskOutcomeTool`）+ `src/adapter/collaboration_tool.rs`（`AgentTaskCollaborationTool`）；都实现 core `Tool`；`READ_ONLY_TOOLS`/`TERMINAL_TOOLS` 与 Java 一致 |
| `AgentRuntimeSource` / `SessionCompactor` / `SessionHistorySource` / `WorkspaceAgentFactory` | 适配器辅助 SPI | ✅ trait 已落（`src/adapter/support.rs`）；宿主具体实现待后续（P2） | `AgentRuntimeSource`/`SessionCompactor`/`SessionHistorySource` 已实现为 trait；`WorkspaceAgentFactory` 由 `HarnessWorkspaceAgentFactory`（`create(definition, assignment) -> Result<HarnessAgent, String>`）实现 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `agentscope-core` | 核心类型 | `agentscope-core`（path，已有） | |
| `agentscope-harness` | harness trait（DistributedStore/BaseStore/MessageBus/AsyncToolRegistry/TaskRepository/Sandbox*） | `agentscope-harness`（path，已有） | |
| grpc + protobuf-java（ASDP） | gRPC 上游 | 设计决定暂缓（P2） | 需 proto 工具链，§4.2 |
| jackson-databind | JSON | `serde_json`（workspace） | |
| JDK HttpClient | HTTP 客户端 | `reqwest`（workspace，json+rustls-tls） | 新增到本 crate 依赖 |
| JDK com.sun.net.httpserver | 契约 HTTP 服务 | 待办（需 HTTP server 依赖） | §4.2 |
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
| `AistioConfig`（builder + 校验） | `AistioConfig` | ✅ 已完成 | agentKey 必填；startGrpc 需 controlPlane |
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
| `AistioBridge`（attach/start/close、事件→seq、Level-1/4 上报） | `SessionBridge` | ✅ 已完成（P1 主干） | 事件 emit 已**真接线**（assign seq → journal → tracker）；新增 `describe_session`/`set_busy`/`is_busy`/`set_effective_system_prompt`/`effective_system_prompt`/`set_context_used_tokens`/`is_known_session`/`reporter()`；`BridgeReporter` 为无所有权发布句柄 |
| `AgentScopeAdapter` | 同名类 | ✅ 已完成（P1） | `src/adapter/agent_scope_adapter.rs`；实现 `FrameworkAdapter`（`can_handle` → ReActAgent/HarnessAgent；context/message/session-command/session-abort/task/plan/export/inbound 能力）；`extract_context`/`list_messages`/`execute_command`/`session_ids` 落地 |
| `AistioObserverMiddleware` | 同名类 | ✅ 已完成（P1） | `src/adapter/observer_middleware.rs`；`on_agent`（session_start + 输入消息 + busy + 旁路 tap）、`on_acting`（tool_call）、`on_system_prompt`（采集有效 prompt）；tool_result 由 `ToolResultTextDelta`/`ToolResultEnd` 事件重组；token 取自 `AgentResult.reply` 的 `Msg.usage` |
| 适配器辅助 SPI | `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` / `AgentTaskStarter` | ✅ 已完成（trait 层） | `src/adapter/support.rs`；`AgentTaskStarter` 已由 `HarnessAgentTaskStarter` 具体实现 |
| harness ASDP 执行器 | `HarnessAgentTaskStarter` (27.5KB) | ✅ 已完成（P2） | `src/adapter/task_starter.rs`；带去重（`attemptId:generation`）、`task.ack`、`ensureRunning`（409 竞态）、workspace 定义物化、协作工具注册、`run_to_outcome` 续跑预算、`cancel_unfinished` 清理、`finish`/`fail` 上报 |
| harness 工作区工厂 | `HarnessWorkspaceAgentFactory` (11.7KB) | ✅ 已完成（P2） | `src/adapter/workspace_factory.rs`；`materialize` 拒绝绝对路径/穿越/运行态段/私有内存/符号链接；`tools`/`permissions` 纯函数与 Java 同构 |
| 运行时工具审批回环 | `CollaborationClient.awaitRuntimeToolApproval` | ✅ 已完成（P2） | `src/transport/collaboration_client.rs`：`RuntimeApprovalSource` seam + 3 个 task-scoped 端点（`runtime-approvals` / `.../{id}/decision` / `.../{id}/ack`）+ 自由函数 `await_runtime_tool_approval`（请求→轮询→ack）；适配层经 core 原生 `ConfirmHandler` 接入（Java 的 PERMISSION_ASKING 暂停/恢复在 Rust core 不存在，见 §4.1.0f） |
| `Aistio` 入口 | 同名类 | ✅ 已完成（薄封装） | `instrument` = new+attach+start |

## 4. 项目状态与下一步
### 4.1 当前状态
**状态：部分完成（P0 + P1 主干已落地并通过构建/测试；剩余 P2 与架构级语义差异）**

本次会话（2026-10-08）交付：`AistioObserverMiddleware`（Level-2 事件源）、`AgentScopeAdapter`（框架适配）、适配器辅助 SPI、**`AgentTask*` 工具族（`AgentTaskOutcome`/`State`/`ToolContext`/`AgentTaskOutcomeTool`/`AgentTaskCollaborationTool`）**、**`CollaborationClient`/`OrchestrationClient`**、**harness 侧 `HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`**、**运行时工具审批回环（`RuntimeApprovalSource` + `ConfirmHandler`）**，以及 **`AistioBridge` 事件 emit 的真接线**（原为 no-op 闭包 → 事件全被吞掉）。

验证命令与结果（本次实测）：`cargo test -p agentscope-extensions-aistio` → **125 passed / 0 failed**（基线 55，**+70**）；全部离线（无真实网络/控制面）。

#### 4.1.0g 修复：`AgentScopeAdapter::on_bridge_attached` 未重写（装配路径真 bug，2026-10-08）
**问题（交叉审核发现，已源码复核）**：`impl FrameworkAdapter for AgentScopeAdapter` 只实现了 `framework_name`/`capabilities`/`can_handle`/`attach`/`detach`/`extract_context`，**漏了 `on_bridge_attached`**；`attach()` 又把传入的 `_emit` 直接丢弃。而 `bridge.rs:266` 是**唯一**经 `on_bridge_attached` 注 reporter 的入口（trait 默认实现为空），`AgentScopeAdapter` 内对 reporter 的写入点为零。
**后果（生产路径）**：`instrument()` → `bridge.attach()` 装配后 `AdapterShared.reporter` 恒为 `None` → `AistioObserverMiddleware` 在 `on_agent`/`on_acting` 命中 `let Some(reporter) = ... else { return next(input) }` 提前返回 → **Level-2 事件（session_start/message/tool_call/tool_result/session busy）全部丢失**；`extract_context` 的 `effective_system_prompt` 采样恒空（回落 base prompt）。
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

已落地文件（`crates/agentscope-extensions-aistio/src/`）：
- `lib.rs`（模块声明 + `instrument` 入口 + `FRAMEWORK_NAME`）
- `error.rs`（`AistioError` / `AistioResult`）
- `config.rs`（`AistioConfig` + builder + 校验）
- `model/`：`session_event.rs`、`context_snapshot.rs`、`context_tracker.rs`、`inventory.rs`、`message_page.rs`、`agent_task.rs`、`mod.rs`
- `event_journal.rs`（长度前缀 JSON 记录 + 序列号持久化，fsync）
- `transport/`：`control_plane_http_client.rs`、`contract_provider.rs`、`http_self_registration.rs`、**`contract_http_server.rs`（axum，已完成）**、`mod.rs`
- `store/`：`control_plane_stores.rs`、`base_store.rs`、`message_bus.rs`、`async_tool_registry.rs`、`snapshot_spec.rs`、`task_repository.rs`、`session_turn_gate.rs`、`sandbox_execution_guard.rs`、`mod.rs`
- `adapter/`：`framework_adapter.rs`（trait + capability 常量 + `on_bridge_attached` 钩子）、`observer_middleware.rs`、`agent_scope_adapter.rs`、`support.rs`（4 个可选 SPI）、`agent_task.rs`、`collaboration_tool.rs`、**`task_starter.rs`（新，harness ASDP 执行器）**、**`workspace_factory.rs`（新，harness 工作区工厂）**、`mod.rs`
- `bridge.rs`（`AistioBridge` + **`BridgeReporter`**：attach/start/record_event/close + `describe_session`/`set_busy`/`effective_system_prompt`/`set_context_used_tokens` + `ContractProvider` 实现）

验证命令与结果（本次实测，2026-10-08）：
- `cargo build -p agentscope-extensions-aistio` → **成功**（exit 0，新代码 0 告警）
- `cargo test -p agentscope-extensions-aistio` → **125 passed / 0 failed / 0 ignored**（基线 55，**+70**）
- `cargo build --workspace` → **BUILD_EXIT=0**
- `cargo test -p agentscope-core` → 全绿（本次对 core 仅**纯增量加 getter**）

#### 4.1.1 本次变更文件
| 文件 | 类型 | 内容 |
|---|---|---|
| `crates/agentscope-core/src/agent/react_agent.rs` | 改（**跨模块，经用户授权**） | 新增 `ReActAgent::sys_prompt()` / `max_iters()` 两个**只读 getter**；不改任何行为/架构 |
| `src/bridge.rs` | 改 | `journal`/`trackers` 移入 `Arc<Mutex<BridgeState>>`；`attach` 的 emit 由 no-op 改为真发布（assign seq → journal → tracker）；新增 `BridgeReporter` 与 6 个会话元数据方法 |
| `src/adapter/framework_adapter.rs` | 改 | 新增可选钩子 `on_bridge_attached(BridgeReporter)`（默认 no-op） |
| `src/adapter/observer_middleware.rs` | 新 | `AistioObserverMiddleware` + `AdapterShared`（reporter/started_sessions/session_users） |
| `src/adapter/agent_scope_adapter.rs` | 新 | `AgentScopeAdapter`（`FrameworkAdapter` 实现） |
| `src/adapter/support.rs` | 新 | `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` / `AgentTaskStarter` |
| `src/adapter/mod.rs` | 改 | 声明与 re-export 新模块 |

#### 4.1.2 与 Java 的语义差异（实现中明确处理，非缺口）
1. **多会话 state**：Java `ReActAgent` 按 `(userId, sessionId)` 持有多个 state 槽；Rust core `ReActAgent` 为**单槽**（`agent_state()`）。`resolve_state` 因此：`ReActAgent` 仅当 `state.session_id == 请求 session` 时返回，否则返回空（**绝不误读其它会话**）；`HarnessAgent` 额外经其 `DistributedStore.agent_state_store` 按 `(user, session)` 读持久化槽。
2. **token 来源**：Java 取 `ModelCallEndEvent.getUsage()`；Rust `ModelCallEnd` 为空 `EventData`（不含 usage），故 middleware 改从 `AgentResult` 的 `reply` 消息（`Msg.usage`）取 turn 级 token。
3. **base system prompt**：Java 有 `react.getSysPrompt()`（本次已补 Rust getter）；�**有效 prompt** 仍以 middleware 在 `on_system_prompt` 的采样为准（与 Java 一致）。
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
   - ASDP gRPC（`GrpcTransport`）需 proto 生成工具链 + proto 文件（`agentscope-service/aistio/internal/asdp`）→ 暂缓（P2）。
   - 若需 EventJournal 的字节级兼容（Java 为 protobuf 记录），需引入 proto/人工字节布局 → 当前采用 JSON 记录（语义等价）。
2. 本模块 P1：**已完成**（`AgentScopeAdapter` + `AistioObserverMiddleware` + `AistioBridge` 事件真接线 + `ContractHttpServer`）。剩余为 P2（见下第 3 条）与架构级语义差异（见 §4.1「语义差异」）。
3. 本模块 P2：`AgentTask*` 工具族、`CollaborationClient`/`OrchestrationClient`、`HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`（harness 侧）、**运行时工具审批回环**（`RuntimeApprovalSource` + `ConfirmHandler`）均已交付（2026-10-08）。**仍未完成**：`GrpcTransport`（设计决定暂缓，需 ASDP proto 工具链）；`SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource` 的**宿主具体实现**（仅落 trait 层 —— 这三者是应用/宿主专属能力：压缩策略、`.log.jsonl`  transcript 读取、subagent/plan 视图，crate 内无法在不臆造 harness API 的前提下给出通用实现，故有意保留为注入点）；harness builder 无 MCP 服务器接线 → `mcpServers` 选择集仅计算并导出，实际建连由宿主完成。

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

**验证**：`cargo test -p agentscope-extensions-aistio` → **55 passed / 0 failed**（两个原失败用例均转绿）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-aistio` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-aistio` / `cargo test -p agentscope-extensions-aistio` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-aistio/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

