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
| 当前状态 | 进行中（P0 模型 + HTTP transport + 7 个 ControlPlane* store + HttpSelfRegistration 已落地；`cargo build` 通过，`cargo test` 55/55 全绿） |

## 2. Java 清单与依赖映射

### 2.0 源文件清单（53 个 .java：主 37 / 测 16）
根包 `io.agentscope.extensions.aistio`：
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `Aistio` | 入口：`instrument(target, config[, adapter])` / `newBridge` | `lib.rs::instrument`（等价 `AistioBridge::new`) | ✅ 薄封装 |
| `AistioConfig` | 连接/上报设置 record（20 字段，校验 agentKey/tenant/namespace/instanceKey） | `config::AistioConfig`（builder + 同校验） | ✅ |
| `SessionBridge` (1177L) | 核心桥：挂载 adapter、事件序列化、Level-1/2/4 上报、命令处理、start/close | `bridge::AistioBridge`（async/tokio） | 骨架（P0 子集） |
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
| `ContractHttpServer` | 进程内 `/agentscope/*` HTTP 契约服务 | 待办（P1，需 HTTP server 依赖 → §4.2） | 待办 |
| `GrpcTransport` | ASDP gRPC 上游通道（protobuf） | 设计决定（P2 暂缓：需 ASDP proto toolchain） | 设计决定 |
| `CollaborationClient` / `OrchestrationClient` | 协作/编排客户端 | 待办（P2） | 待办 |

子包 `adapter`：
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeAdapter` (928L) | AgentScope 框架适配（覆盖全部 capability） | 待办（P1，依赖 bridge） | 待办 |
| `AistioObserverMiddleware` | 观察中间件（Level-2 事件源） | 待办（P1，需 core Middleware trait 接线） | 待办 |
| `HarnessAgentTaskStarter` / `HarnessWorkspaceAgentFactory` | harness 任务启动/工作区 agent 工厂 | 待办（P1） | 待办 |
| `AgentTask*` 工具族（5 类） | AgentTask 工具/上下文/产物 | 待办（P2） | 待办 |
| `AgentRuntimeSource` / `SessionCompactor` / `SessionHistorySource` / `WorkspaceAgentFactory` | 适配器辅助 SPI | 待办（P2） | 待办 |

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
| `ContractHttpServer` | 同名类 | 待办（P1） | 需 HTTP server 依赖（§4.2） |
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
| `AistioBridge`（attach/start/close、事件→seq、Level-1/4 上报） | `SessionBridge` | 骨架（P1） | 需 core ReActAgent 事件/中间件接线 |
| `AgentScopeAdapter` | 同名类 | 待办（P1） | 依赖 bridge + core agent 查询 API |
| `AistioObserverMiddleware` | 同名类 | 待办（P1） | 需 core Middleware trait 接线 |
| `Aistio` 入口 | 同名类 | ✅ 已完成（薄封装） | `instrument` = new+attach+start |

## 4. 项目状态与下一步
### 4.1 当前状态
**状态：部分完成（P0 链路已全部落地并通过构建/测试）**

已落地文件（`crates/agentscope-extensions-aistio/src/`）：
- `lib.rs`（模块声明 + `instrument` 入口 + `FRAMEWORK_NAME`）
- `error.rs`（`AistioError` / `AistioResult`）
- `config.rs`（`AistioConfig` + builder + 校验）
- `model/`：`session_event.rs`、`context_snapshot.rs`、`context_tracker.rs`、`inventory.rs`、`message_page.rs`、`agent_task.rs`、`mod.rs`
- `event_journal.rs`（长度前缀 JSON 记录 + 序列号持久化，fsync）
- `transport/`：`control_plane_http_client.rs`（reqwest + `X-Builder-Internal-Token`）、`contract_provider.rs`（trait）、`http_self_registration.rs`（注册 + 心跳 + 身份回调）、`mod.rs`
- `store/`：`control_plane_stores.rs`（工厂）、`base_store.rs`（impl harness `BaseStore`）、`message_bus.rs`（impl `MessageBus`）、`async_tool_registry.rs`（impl `AsyncToolRegistry`）、`snapshot_spec.rs`（impl `SandboxSnapshotSpec`/`SandboxSnapshot`）、`task_repository.rs`（impl harness `TaskRepository`，43940B）、`session_turn_gate.rs`（CP 托管 turn gate + 本地降级）、`sandbox_execution_guard.rs`（CP 托管 sandbox guard）、`mod.rs`
- `adapter/`：`framework_adapter.rs`（trait + capability 常量）、`mod.rs`
- `bridge.rs`（`AistioBridge`：attach/start/record_event/close + `ContractProvider` 实现）

验证命令与结果（最近一次：2026-09-18）：
- `cargo build -p agentscope-extensions-aistio` → **成功**（Finished dev profile，无本 crate 警告）
- `cargo test -p agentscope-extensions-aistio` → **成功：55 passed; 0 failed**（含 context hash 稳定性、journal 持久化/重载、message 分页、seq 单调分配、stores 解析、task repository 本地生命周期/回调抑制、turn gate 降级等）

约束遵守：
- 仅改动本 crate + 本 tracker；未改根 `Cargo.toml` `[workspace.dependencies]`，未改 core/harness 与其它 crate。
- 新增依赖仅写入 `crates/agentscope-extensions-aistio/Cargo.toml`：`reqwest` / `sha2` / `uuid`（均复用 workspace 版本，未新增 workspace 条目）。
- 复用 harness 既有 trait（`BaseStore`/`MessageBus`/`AsyncToolRegistry`/`SandboxSnapshotSpec`/`SandboxSnapshot`）与 core `AgentStateStore`，未重建平行类型。
- 注：`base64` 未在 workspace 依赖中，故 `AgentTaskAssignment.payload` 的 base64 编解码在本 crate 内内联实现（避免改根 Cargo.toml）。
### 4.2 下一步与阻塞
1. **跨模块待办（需汇总方裁定）**：
   - harness `DistributedStore` 字段不足（缺 base_store/sandbox_snapshot_spec/sandbox_execution_guard/message_bus/async_tool_registry/task_repository）→ 见 EXTENSIONS §2.2；本模块的 `ControlPlaneStores` 已能产出各组件，但尚无组合者。
   - harness `SessionTurnGate`（`gateway/session_turn_gate.rs`）/ `SandboxExecutionGuard`（`sandbox/guard.rs`）为具体类型（非 trait）。**已用本 crate 内适配层绕过**（`ControlPlaneSessionTurnGate` 内部委托本地 `SessionTurnGate` 作为不可达降级；`ControlPlaneSandboxExecutionGuard` 为并行托管实现），但**跨模块问题仍待汇总方裁定**：是否应在 harness 定义 trait 以便直接替换。
   - 契约 HTTP server（`ContractHttpServer`）需 HTTP server 依赖（axum/hyper 等），当前 workspace 未含 → 待汇总方加 `[workspace.dependencies]`；`ContractProvider` trait 已就绪可供其实现。
   - ASDP gRPC（`GrpcTransport`）需 proto 生成工具链 + proto 文件（`agentscope-service/aistio/internal/asdp`）→ 暂缓（P2）。
   - 若需 EventJournal 的字节级兼容（Java 为 protobuf 记录），需引入 proto/人工字节布局 → 当前采用 JSON 记录（语义等价）。
2. 本模块 P1（未完成）：`AgentScopeAdapter`（928L）+ `AistioObserverMiddleware` 事件接线（依赖 core ReActAgent 事件/中间件 API）；`AistioBridge` 完整 Level-1/2/4 上报与命令处理（当前为骨架）；`ContractHttpServer`（需 HTTP server 依赖，§4.2.1）。
3. 本模块 P2（未完成）：`adapter` 子包剩余辅助类（`AgentTask*` 工具族、`AgentRuntimeSource`、`SessionCompactor`、`SessionHistorySource`、`WorkspaceAgentFactory`、`HarnessAgentTaskStarter`/`HarnessWorkspaceAgentFactory`）、`CollaborationClient`/`OrchestrationClient`、`GrpcTransport`（设计决定暂缓）。

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

