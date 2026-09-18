# SANDBOX 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **SANDBOX** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-sandbox\`
> **Java 子模块**：agentrun, daytona, e2b, kubernetes
> **Rust 目标**：`crates/agentscope-extensions-sandbox/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-sandbox` |
| Java 子模块 | agentrun, daytona, e2b, kubernetes |
| Java 规模（实测行数） | 主代码 8024 / 测试 1577（见下表） |
| Rust crate | `crates/agentscope-extensions-sandbox` |
| 迁移波次 | A |
| 当前状态 | **已完成**（build 通过；71 passed / 0 failed） |

| Java 子模块 | 主代码 文件/行 | 测试 文件/行 |
|---|---|---|
| agentrun | 11 / 1624 | 3 / 315 |
| daytona | 8 / 891 | 0 / 0 |
| e2b | 13 / 1415 | 3 / 412 |
| kubernetes | 39 / 4094 | 8 / 850 |
| **合计** | **71 / 8024** | **14 / 1577** |

### 1.1 Rust crate 文件清单（10 个源文件，~171 KB）
| 文件 | 职责 | 单测 |
|---|---|---|
| `src/lib.rs` | crate 根：模块声明、re-export、架构文档 | 0 |
| `src/error.rs` | `SandboxExtensionError` / `SandboxErrorCode` / `SandboxResult` | 3 |
| `src/http.rs` | 共享 reqwest 客户端、URL 拼接、JSON/空响应收发 | 3 |
| `src/retry.rs` | `RetryPolicy`（agentrun/daytona/e2b/kubernetes）+ `with_retries` | 7 |
| `src/workspace.rs` | `WorkspaceSpec` → shell 脚本投影、路径解析、`shell_quote` | 5 |
| `src/remote.rs` | 核心 glue：`RemoteBackend` trait、`RemoteSandbox`（impl harness `Sandbox`）、`RemoteSandboxClient`（impl harness `SandboxClient`）、`RemoteSandboxStateData` | 11 |
| `src/agentrun.rs` | AgentRun 后端（options/校验/MCP exec/数据面 URL） | 11 |
| `src/daytona.rs` | Daytona 后端（options/请求体/exec 解析） | 7 |
| `src/e2b.rs` | E2B 后端（options/envd Connect/JSON 编解码/快照标记） | 15 |
| `src/kubernetes.rs` | Kubernetes 后端（SandboxClaim CRD/exec/connector URL） | 9 |
| **合计** | | **71** |

## 2. Java 清单与依赖映射

> 说明：Java 各子模块的 `*Sandbox` / `*SandboxClient` / `*SandboxState` 均继承 harness 的 `AbstractBaseSandbox` / `SandboxClient` / `SandboxState`（Java 侧为抽象类 + 泛型），配置由 `*SandboxClientOptions` 承载。Rust 侧 harness 把 `Sandbox`/`SandboxClient` 定义为 **trait**、`SandboxState` 为**封闭 enum**，因此本 crate 不逐类复刻 Java 继承树，而是：每个后端实现一个轻量 `RemoteBackend`，再由 `remote::RemoteSandbox` / `remote::RemoteSandboxClient` 统一适配到 harness trait（见 §2.6「harness 复用契约」）。

### 2.1 agentrun 子模块（`io.agentscope.extensions.sandbox.agentrun`，11 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentRunSandbox` | harness `Sandbox` 实现（exec/快照/挂载） | `remote::RemoteSandbox` + `agentrun::AgentRunBackend`（`impl RemoteBackend`） | 已完成 |
| `AgentRunSandboxClient` | harness `SandboxClient` 工厂 | `agentrun::AgentRunSandboxClient`（=`RemoteSandboxClient` 别名）、`agentrun::agentrun_client` | 已完成 |
| `AgentRunSandboxClientOptions` | 客户端配置 + 校验 | `agentrun::AgentRunSandboxClientOptions`、`validate_options` | 已完成 |
| `AgentRunSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData`（投影到 harness `SandboxState::Remote`） | 已完成（投影式） |
| `AgentRunDataPlaneHttp` | 数据面 HTTP（create/status） | `agentrun::AgentRunBackend`（reqwest）+ `http::HttpClient`/`join_url` | 已完成 |
| `AgentRunMcpChannel` | MCP 工具调用执行命令 | `agentrun::{build_mcp_tool_call, extract_mcp_text, mcp_is_error, parse_exec_payload}` | 已完成 |
| `AgentRunNasMountConfig` | NAS 挂载配置 | `agentrun::AgentRunNasMountConfig` | 已完成 |
| `AgentRunOssMountConfig` | OSS 挂载配置（≤5） | `agentrun::AgentRunOssMountConfig`、`MAX_OSS_MOUNTS` | 已完成 |
| `AgentRunRetry` | 瞬态失败重试 | `retry::RetryPolicy::agentrun` + `with_retries` | 已完成 |
| `AgentRunFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | **无 Rust 对应** | 待办（见 §3.2） |
| `AgentRunHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用（serde 按 enum 变体序列化） | 设计决定（不迁移） |

### 2.2 daytona 子模块（`io.agentscope.extensions.sandbox.daytona`，8 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DaytonaSandbox` | harness `Sandbox` 实现 | `remote::RemoteSandbox` + `daytona::DaytonaBackend` | 已完成 |
| `DaytonaSandboxClient` | harness `SandboxClient` 工厂 | `daytona::DaytonaSandboxClient`（别名）、`daytona::daytona_client` | 已完成 |
| `DaytonaSandboxClientOptions` | 客户端配置 | `daytona::DaytonaSandboxClientOptions` | 已完成 |
| `DaytonaSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `DaytonaHttp` | 控制面/工具面 HTTP | `daytona::DaytonaBackend`（reqwest） | 已完成 |
| `DaytonaRetry` | 瞬态失败重试 | `retry::RetryPolicy::daytona` | 已完成 |
| `DaytonaFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | **无 Rust 对应** | 待办（见 §3.2） |
| `DaytonaHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |

### 2.3 e2b 子模块（`io.agentscope.extensions.sandbox.e2b`，13 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `E2bSandbox` | harness `Sandbox` 实现 | `remote::RemoteSandbox` + `e2b::E2bBackend` | 已完成 |
| `E2bSandboxClient` | harness `SandboxClient` 工厂 | `e2b::E2bSandboxClient`（别名）、`e2b::e2b_client` | 已完成 |
| `E2bSandboxClientOptions` | 客户端配置 | `e2b::E2bSandboxClientOptions` | 已完成 |
| `E2bSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `E2bEnvdProcessClient` | envd Connect/进程启动与流式事件 | `e2b::{build_envd_start_json_body, encode_connect_envelope, decode_connect_frames, parse_envd_event, drain_envd_stream, EnvdEvent, EnvdStreamResult}` | 部分完成（**JSON codec 已实现；Proto 未实现**，见 §3.2） |
| `E2bPlatformHttp` | 平台 HTTP（create/connect/snapshot） | `e2b::E2bBackend` + `parse_sandbox_fields`/`envd_host`/`basic_auth_header` | 已完成 |
| `E2bRetry` | 瞬态失败重试（含 408） | `retry::RetryPolicy::e2b` | 已完成 |
| `E2bSnapshotRefs` | 原生快照标记字节 | `e2b::E2bSnapshotRefs`（magic prefix + 编解码） | 已完成 |
| `E2bPersistenceMode` | 持久化模式枚举 | `e2b::E2bPersistenceMode` | 已完成 |
| `E2bCodec` | Connect 编解码枚举 | `e2b::E2bCodec` | 已完成 |
| `E2bFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | **无 Rust 对应** | 待办（见 §3.2） |
| `E2bHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `E2bCloudBucketMountStrategy`（`mounts/`） | 云桶挂载策略 | **无 Rust 对应** | 待办（见 §3.3） |

### 2.4 kubernetes 子模块（`io.agentscope.extensions.sandbox.kubernetes`，39 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `KubernetesSandbox` | harness `Sandbox` 实现（`SandboxFileTransfer`） | `remote::RemoteSandbox` + `kubernetes::KubernetesBackend` | 部分完成（无 `SandboxFileTransfer` 对应物，见 §3.2） |
| `KubernetesSandboxClient` | harness `SandboxClient` 工厂 | `kubernetes::KubernetesSandboxClient`（别名）、`kubernetes::kubernetes_client` | 已完成 |
| `KubernetesSandboxClientOptions` | 客户端配置 | `kubernetes::KubernetesSandboxClientOptions` | 已完成 |
| `KubernetesSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `KubernetesFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | **无 Rust 对应** | 待办 |
| `KubernetesHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `client/CommandExecutor` | 在 sandbox 服务器上执行命令 | `kubernetes::{build_execute_body, parse_execute_result, KubernetesExecResult}` | 已完成 |
| `client/Constants` | CRD group/version/plural、标签、端口 | `kubernetes::{CLAIM_API_GROUP, CLAIM_API_VERSION, CLAIM_PLURAL_NAME, CREATED_BY_LABEL, CREATED_BY_VALUE, DEFAULT_SERVER_PORT}` | 已完成 |
| `client/CreateSandboxOptions` | 创建参数 | `kubernetes::{build_claim_body, KubernetesSandboxClientOptions}` | 部分完成（warm pool/claim 已覆盖；其余字段待确认） |
| `client/Sandbox` | sandbox 服务器句柄（exec/files/状态） | `kubernetes::KubernetesBackend` + `remote::RemoteSandbox` | 部分完成（exec/状态已覆盖；files 未迁移） |
| `client/SandboxClient` | K8s 侧客户端（`AutoCloseable`） | `kubernetes::KubernetesBackend` + `remote::RemoteSandboxClient` | 部分完成 |
| `client/Filesystem` | 文件 API（读写/列目录/移动） | **无 Rust 对应** | 待办（见 §3.2） |
| `client/config/DirectConnectionConfig` | 直连（pod IP） | `kubernetes::resolve_connector_base_url`（pod IP 分支） | 已完成 |
| `client/config/GatewayConnectionConfig` | 网关连接 | `kubernetes::resolve_connector_base_url`（gateway 分支） | 已完成 |
| `client/config/InClusterConnectionConfig` | 集群内连接 | 纳入 `KubernetesSandboxClientOptions`（`api_url`/`api_token`），但**无策略选择器** | 部分完成（见 §3.2） |
| `client/config/LocalTunnelConnectionConfig` | 本地端口转发 | **无 Rust 对应** | 待办（见 §3.2） |
| `client/config/SandboxConnectionConfig` | 连接配置接口 | 无 1:1 trait；由 `resolve_connector_base_url` 函数式覆盖 | 部分完成 |
| `client/crd/SandboxClaim` | CRD 资源对象 | `kubernetes::build_claim_body` | 已完成 |
| `client/crd/SandboxClaimSpec` | CRD spec | `kubernetes::build_claim_body`（`spec.warmPoolRef`） | 部分完成 |
| `client/crd/SandboxClaimStatus` | CRD status | `kubernetes::KubernetesClaimStatus` + `parse_claim_status` | 已完成 |
| `client/crd/SandboxResource` | sandbox 资源状态 | `kubernetes::parse_claim_status` | 部分完成 |
| `client/exceptions/*`（8 个异常类） | 细分异常 | `error::SandboxExtensionError` / `SandboxErrorCode`（按语义归并，未 1:1） | 设计决定（归并） |
| `client/internal/K8sHelper` | K8s REST 助手 | `kubernetes::KubernetesBackend`（reqwest 直连 K8s API） | 部分完成 |
| `client/internal/SandboxConnector` | 连接编排 | `kubernetes::resolve_connector_base_url` + `KubernetesBackend` | 部分完成 |
| `client/internal/strategy/ConnectionStrategy` | 策略接口 | 无 1:1 trait | 部分完成 |
| `client/internal/strategy/DirectStrategy` | 直连策略 | `resolve_connector_base_url`（pod IP） | 已完成 |
| `client/internal/strategy/GatewayStrategy` | 网关策略 | `resolve_connector_base_url`（gateway） | 已完成 |
| `client/internal/strategy/InClusterStrategy` | 集群内策略 | **无策略实现** | 待办（见 §3.2） |
| `client/internal/strategy/PortForwardStrategy` | 端口转发策略 | **无 Rust 对应** | 待办（见 §3.2） |
| `client/model/ExecutionResult` | 执行结果 | `kubernetes::KubernetesExecResult` | 已完成 |
| `client/model/FileEntry` | 文件条目 | **无 Rust 对应** | 待办 |
| `client/model/SandboxStatus` | sandbox 状态 | `kubernetes::KubernetesClaimStatus` | 部分完成 |

### 2.5 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / Java `HttpClient` | HTTP 传输 | `reqwest`（workspace） | 超时/头部/重试见 `http.rs`、`retry.rs` |
| Jackson (`ObjectMapper`/`SimpleModule`) | JSON (反)序列化 + 子类型注册 | `serde` + `serde_json`（workspace） | state 经 harness 封闭 enum 序列化；无 Jackson 模块概念 |
| `com.google.protobuf`（DynamicMessage，E2B envd） | envd Connect protobuf 帧 | **无** | 仅实现 JSON codec；Proto 待办（见 §3.2） |
| Kubernetes Java client（`CustomResource`） | SandboxClaim CRD 读写 | `reqwest` 直连 K8s REST（`kubernetes.rs`） | 未引入 kube-rs 等依赖 |
| `java.util.Base64` | 工作区 tar 的 base64 | `base64`（workspace） | `remote.rs` 分块上传/解码 |
| `java.util.UUID` | session/claim id | `uuid`（workspace） | — |
| `java.security.MessageDigest` | 投影哈希 | 内置 FNV-1a（`workspace::short_hash` / `remote::workspace_projection_hash`） | 算法不同但语义一致（变更检测） |
| `java.time.Duration` | 超时 | `std::time::Duration` + `tokio::time` | — |
| slf4j | 日志 | `tracing`（workspace） | — |
| JUnit 5 / Mockito | 测试 | `#[test]` / `#[tokio::test]` + `httpmock` 0.7（dev-dependency） | 离线 mock HTTP |

> 本 crate `Cargo.toml` `[dependencies]`（均 `workspace = true`，**未改根 `Cargo.toml`**）：`agentscope-core`、`agentscope-harness`、`serde`、`serde_json`、`thiserror`、`async-trait`、`tokio`、`futures`、`tracing`、`chrono`、`uuid`、`reqwest`、`base64`；`[dev-dependencies]`：`tokio`（含 `test-util`）、`httpmock = "0.7"`。

### 2.6 harness 复用契约（关键）
本 crate **实现 harness trait，而非重建平行类型**。逐项对应：
| harness 类型（路径） | 本 crate 的使用 | 结论 |
|---|---|---|
| `agentscope_harness::agent::sandbox::Sandbox`（trait） | `impl Sandbox for remote::RemoteSandbox`（`start`/`stop`/`shutdown`/`is_running`/`state`/`exec`/`persist_workspace`/`hydrate_workspace`） | 直接实现 |
| `agentscope_harness::agent::sandbox::SandboxClient`（trait） | `impl SandboxClient for remote::RemoteSandboxClient`（`create`/`resume`/`delete`/`serialize_state`/`deserialize_state`） | 直接实现 |
| `agent::sandbox::SandboxState`（封闭 enum） | `remote::RemoteSandboxStateData::{to_harness_state, from_harness_state}` 投影到/取自 `SandboxState::Remote` | 复用（§4.2 记 re-export 缺口） |
| `agent::sandbox::snapshot::{SandboxSnapshot, SandboxSnapshotSpec}` | `RemoteSandbox` 持有 `Option<Arc<dyn SandboxSnapshot>>`；`RemoteSandboxClient` 持有 `Option<Arc<dyn SandboxSnapshotSpec>>`（`with_snapshot_spec`） | 复用，无平行快照类型 |
| `agent::sandbox::workspace_spec::{WorkspaceSpec, ExecResult}` | `workspace.rs` 据此生成 shell 投影脚本；`ExecResult` 为 exec 返回类型 | 复用 |
| `agent::sandbox::SandboxClientOptions` | `SandboxClient::create` 参数（实现当前忽略 `_options`，配置由各后端 Options 承载） | 复用签名 |
| `agent::sandbox::SandboxExecutionGuard`（具体 struct，`Clone`） | 本 crate **未使用**（属 harness `SandboxManager` 生命周期职责） | **已裁定（见汇总 §3.3）**：Java 侧为 `@FunctionalInterface` 接口，Rust 侧为具体 struct，属**确认的 harness 缺口**；本 crate 保持不重建平行类型 |
| `agent::sandbox::SandboxIsolationKey` | 本 crate **未使用** | 未重建 |
| `agent::filesystem::{SandboxFilesystem, SandboxBackedFilesystem}` | 本 crate **未实现**（Java 的 `*FilesystemSpec` 无对应） | 缺口，见 §3.2 |

## 3. 详细缺口清单

### 3.1 P0（核心链路）— 全部已完成
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 四后端接入 harness `Sandbox`/`SandboxClient` | `*Sandbox`/`*SandboxClient`（4 子模块） | 已完成 | `remote.rs`：`RemoteSandbox`/`RemoteSandboxClient` + 4 个 `RemoteBackend` 实现 |
| AgentRun 后端 | `agentrun/*` | 已完成 | `agentrun.rs`：options 校验、数据面 URL、MCP exec、NAS/OSS 挂载 |
| Daytona 后端 | `daytona/*` | 已完成 | `daytona.rs`：控制面/工具面请求体、exec 解析、`relative_cwd` |
| E2B 后端 | `e2b/*` | 已完成 | `e2b.rs`：平台 HTTP、envd Connect(JSON)、快照标记、持久化模式 |
| Kubernetes 后端 | `kubernetes/*` | 已完成 | `kubernetes.rs`：SandboxClaim CRD、exec、connector URL |
| 状态投影（封闭 enum 适配） | `*SandboxState` | 已完成 | `RemoteSandboxStateData` + `to/from_harness_state` |
| 快照接入 | `NoopSnapshotSpec`/快照链路 | 已完成 | 复用 harness `SandboxSnapshot(Spec)`；E2B 原生快照走 `E2bSnapshotRefs` |
| 工作区投影 | `WorkspaceSpec`/`WorkspaceMountSupport` | 已完成 | `workspace.rs` 生成确定性 shell 脚本（tar/base64 上传） |
| 共享 HTTP/重试 | `*Http`/`*Retry` | 已完成 | `http.rs` + `retry.rs` |
| 离线单测（71 个） | Java 测试 1577 行 | 已完成 | 分布见 §1.1 |

### 3.2 P1（重要）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `*FilesystemSpec` 四类 | `AgentRun/Daytona/E2b/KubernetesFilesystemSpec` | **待办** | Java 侧均 `extends harness SandboxFilesystemSpec`；**Rust harness 无对应 Spec 类型**（仅有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`），无法 1:1。见 §4.2（不改 harness） |
| Kubernetes 文件 API | `client/Filesystem`、`client/model/FileEntry`、`SandboxFileTransfer` | **待办** | 仅 exec + 工作区投影可用；无独立文件读写/列目录接口 |
| E2B envd Proto codec | `E2bEnvdProcessClient`（protobuf DynamicMessage） | **部分完成** | 仅 JSON codec；`E2bCodec::Proto` 为占位默认，实际仅走 JSON |
| K8s 连接策略 1:1 | `InClusterStrategy`/`PortForwardStrategy`/`ConnectionStrategy` | **部分完成** | 仅有 direct(pod IP) 与 gateway 的 URL 解析；无策略对象树 |
| K8s 异常类 1:1 | `client/exceptions/*`（8 类） | 设计决定（归并） | 归并到 `SandboxErrorCode`/`SandboxExtensionError`；未逐类建模 |
| 真实端点联调 | — | 待办 | 网络层已实现但无真实凭据；测试全离线（httpmock） |
| Java 测试逐条对照 | Java tests 14 文件 | 待办 | Rust 单测为功能等价重写，未逐条照搬 |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| E2B 云桶挂载策略 | `e2b/mounts/E2bCloudBucketMountStrategy` | 待办 | 无 Rust 对应 |
| K8s K8sHelper 细分能力 | `client/internal/K8sHelper` | 部分完成 | 以 reqwest 直连覆盖 claim/status/exec；其余（端口转发、模板）未迁移 |
| Jackson 模块 | `*HarnessSandboxJacksonModule` | 设计决定（不迁移） | serde 按 enum 变体序列化 |
| 投影哈希算法对齐 | Java `MessageDigest` | 已完成（语义等价） | Rust 用 FNV-1a；仅用于变更检测，不跨语言互操作 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：四个远端沙箱后端（agentrun / daytona / e2b / kubernetes）全部适配到 harness `Sandbox`/`SandboxClient` trait；共享 `RemoteBackend` glue、HTTP、重试、工作区投影、状态投影、快照接入均已落地。
- **Rust 产物**：`src/` 10 个源文件（`lib/error/http/retry/workspace/remote/agentrun/daytona/e2b/kubernetes.rs`），~171 KB；71 个单测。
- **验证（实测，汇总方基线）**：
  ```
  cargo build -p agentscope-extensions-sandbox   -> 通过（0 error）
  cargo test  -p agentscope-extensions-sandbox   -> 71 passed / 0 failed / 0 ignored
  cargo test  --workspace                        -> 1485 passed / 0 failed
  ```
- **已知问题**：无阻塞。真实厂商端点未联调（测试全离线）；若干 Java 侧能力未迁移（见 §3.2/§3.3）。

### 4.2 下一步与阻塞
1. **harness 抽象缺口（不改 harness，仅记录）**：harness `agent::sandbox` 的 `state` 模块为私有（`mod state;`），`RemoteSandboxState` **未在 `sandbox/mod.rs` re-export**。本 crate 因此无法命名 `RemoteSandboxState`，只能经 `serde_json` 与 `SandboxState::Remote` 变体互转（见 `remote.rs` 内注释）。**建议汇总方考虑 re-export `RemoteSandboxState`**；在裁定前本 crate 用现有公开面绕过，**未修改 harness**。
2. **harness 缺少 `SandboxFilesystemSpec`**：Java 扩展的 4 个 `*FilesystemSpec` 继承 harness Java `SandboxFilesystemSpec`，而 Rust harness 仅有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`，无 Spec 工厂类型。故该批类无法 1:1 迁移（列 §3.2 待办）。**如需，须由汇总方决定是否扩展 harness 或在本 crate 内建适配层**；本子智能体**未改 harness/core**。
3. **P1 补齐**：Kubernetes 文件 API（`client/Filesystem`/`FileEntry`）、E2B Proto codec、K8s 连接策略对象树。
4. **真实端点联调**：需真实 E2B/Daytona/AgentRun/K8s 凭据；当前全部请求体构造/响应解析/重试均有离线单测覆盖。
5. **跨模块需求**：无硬阻塞。本 crate 依赖 `agentscope-core`（`Msg`/`Error`/`RuntimeContext`）与 `agentscope-harness`（sandbox trait/类型、`WorkspaceSpec`/`ExecResult`/`SandboxState`/快照 trait、`filesystem`）。无 workspace 级依赖变更需求（所有依赖已在 workspace 定义或本 crate `[dependencies]` 声明）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-sandbox` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-sandbox` / `cargo test -p agentscope-extensions-sandbox` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-sandbox/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

