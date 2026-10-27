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
| Java 规模（实测行数） | 主代码 8832 / 测试 4554（见下表；2026-10-22 静态复核实测，见 §6） |
| Rust crate | `crates/agentscope-extensions-sandbox` |
| 迁移波次 | A |
| 当前状态 | **已完成**（build 通过；**137 passed / 0 failed**；`fs_spec` 与 `kubernetes_files` 已落地；K8s 文件 API / E2B 原生传输**尚未接线**，见 §3.2） |

| Java 子模块 | 主代码 文件/行 | 测试 文件/行 | 顶层类型数 |
|---|---|---|---|
| agentrun | 11 / 1626 | 4 / 375 | 11 |
| daytona | 8 / 891 | 0 / 0 | 8 |
| e2b | 13 / 2216 | 11 / 3295 | 13 |
| kubernetes | 39 / 4099 | 8 / 884 | 39 |
| **合计** | **71 / 8832** | **23 / 4554** | **71** |

> 顶层类型数 = 每个 `.java` 主源文件恰含 1 个顶层类型（另有 12 个嵌套类型：K8s 8 个异常类各 1 个 + `SandboxClaimSpec` 3 个静态内部类 + `CreateSandboxOptions.Builder`），故 71 文件 = 71 顶层类型。

### 1.1 Rust crate 文件清单（12 个源文件，6721 行 / 137 test）
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
| `src/e2b.rs` | E2B 后端（options/envd Connect/JSON 编解码/快照标记/文件传输） | 43 |
| `src/kubernetes.rs` | Kubernetes 后端（SandboxClaim CRD/exec/connector URL） | 9 |
| `src/fs_spec.rs` | 本 crate 自建的 `SandboxFilesystemSpec` 等价抽象（client + workspace + snapshot + isolation scope）+ 四个 `*FilesystemSpec` + `SandboxFilesystemAdapter`（impl harness `SandboxFilesystem`） | 18 |
| `src/kubernetes_files.rs` | Kubernetes 文件 API（upload/download/list/exists）+ 本 crate 声明并实现的 `SandboxFileTransfer` trait（`KubernetesFileTransfer`），经 `FileApiTransport` seam 离线可测 | 20 |
| **合计** | **6721 行** | **137** |

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
| `AgentRunFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::AgentRunFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层，见 §3.2） |
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
| `DaytonaFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::DaytonaFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层，见 §3.2） |
| `DaytonaHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |

### 2.3 e2b 子模块（`io.agentscope.extensions.sandbox.e2b`，13 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `E2bSandbox` | harness `Sandbox` 实现 | `remote::RemoteSandbox` + `e2b::E2bBackend` | 已完成 |
| `E2bSandboxClient` | harness `SandboxClient` 工厂 | `e2b::E2bSandboxClient`（别名）、`e2b::e2b_client` | 已完成 |
| `E2bSandboxClientOptions` | 客户端配置 | `e2b::E2bSandboxClientOptions` | 已完成 |
| `E2bSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `E2bEnvdProcessClient` | envd Connect/进程启动与流式事件 + 文件传输 | `e2b::{build_envd_start_json_body, encode_connect_envelope, decode_connect_frames, parse_envd_event, drain_envd_stream, EnvdEvent, EnvdStreamResult, upload_file, download_file, files_url}` | 部分完成（**JSON codec + envd `/files` 上传/下载已实现；Proto 未实现**，见 §3.2） |
| `E2bPlatformHttp` | 平台 HTTP（create/connect/snapshot） | `e2b::E2bBackend` + `parse_sandbox_fields`/`envd_host`/`basic_auth_header` | 已完成 |
| `E2bRetry` | 瞬态失败重试（含 408） | `retry::RetryPolicy::e2b` | 已完成 |
| `E2bSnapshotRefs` | 原生快照标记字节 | `e2b::E2bSnapshotRefs`（magic prefix + 编解码） | 已完成 |
| `E2bPersistenceMode` | 持久化模式枚举 | `e2b::E2bPersistenceMode` | 已完成 |
| `E2bCodec` | Connect 编解码枚举 | `e2b::E2bCodec` | 已完成 |
| `E2bFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::E2bFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层，见 §3.2） |
| `E2bHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `E2bCloudBucketMountStrategy`（`mounts/`） | 云桶挂载策略 | **无 Rust 对应** | 待办（见 §3.3）。**注**：Java 该接口是**空占位**（`intentionally empty`，无任何方法），迁移价值为 0 |

### 2.4 kubernetes 子模块（`io.agentscope.extensions.sandbox.kubernetes`，39 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `KubernetesSandbox` | harness `Sandbox` 实现（`SandboxFileTransfer`） | `remote::RemoteSandbox` + `kubernetes::KubernetesBackend` | 部分完成（`SandboxFileTransfer` 仅在 `kubernetes_files.rs` 独立声明，**未挂到 `RemoteSandbox`/`KubernetesBackend`**，见 §3.2） |
| `KubernetesSandboxClient` | harness `SandboxClient` 工厂 | `kubernetes::KubernetesSandboxClient`（别名）、`kubernetes::kubernetes_client` | 已完成 |
| `KubernetesSandboxClientOptions` | 客户端配置 | `kubernetes::KubernetesSandboxClientOptions` | 已完成 |
| `KubernetesSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `KubernetesFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::KubernetesFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层） |
| `KubernetesHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `client/CommandExecutor` | 在 sandbox 服务器上执行命令 | `kubernetes::{build_execute_body, parse_execute_result, KubernetesExecResult}` | 已完成 |
| `client/Constants` | CRD group/version/plural、标签、端口 | `kubernetes::{CLAIM_API_GROUP, CLAIM_API_VERSION, CLAIM_PLURAL_NAME, CREATED_BY_LABEL, CREATED_BY_VALUE, DEFAULT_SERVER_PORT}` | 部分完成（缺 `GATEWAY_*`/`SANDBOX_*`/`ROUTER_SERVICE_NAME`/`ROUTER_PORT`/`SANDBOX_NAME_HASH_LABEL`；`CREATED_BY_VALUE` 值由 `agentscope-java-client` 改为 `agentscope-rust-client`，属有意的跨语言标签差异） |
| `client/CreateSandboxOptions` | 创建参数 | `kubernetes::{build_claim_body, KubernetesSandboxClientOptions}` | 部分完成（warm pool/claim 已覆盖；`lifecycle`（shutdownTime/shutdownPolicy）、`additionalPodMetadata`（labels/annotations）、`claimName`、`shutdownAfterSeconds` 未迁移） |
| `client/Sandbox` | sandbox 服务器句柄（exec/files/状态） | `kubernetes::KubernetesBackend` + `remote::RemoteSandbox` | 部分完成（exec/状态已覆盖；`files()` 未接入沙箱生命周期） |
| `client/SandboxClient` | K8s 侧客户端（`AutoCloseable`） | `kubernetes::KubernetesBackend` + `remote::RemoteSandboxClient` | 部分完成（`listSandboxClaims`/`listActiveSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName` 未迁移） |
| `client/Filesystem` | 文件 API（读写/列目录/移动） | `kubernetes_files::KubernetesFileApi`（`write`/`read`/`list`/`exists`/`move_path`） | 已完成（独立组件；**未接线**到 `RemoteSandbox`，见 §3.2） |
| `client/config/DirectConnectionConfig` | 直连（**预置 router API URL**） | `kubernetes::KubernetesSandboxClientOptions::api_url`（K8s API 地址，非 sandbox-router 地址） | 部分完成（语义不同，见 §3.2） |
| `client/config/GatewayConnectionConfig` | 网关连接（**Gateway CRD 外部地址 + watch**） | `kubernetes::resolve_connector_base_url` 的 gateway 分支 | **未完成**：Rust 分支拼的是 `*.svc.cluster.local` 集群内 DNS，非 Gateway CRD 地址（见 §3.2） |
| `client/config/InClusterConnectionConfig` | 集群内连接（`sandboxId.namespace.svc.cluster.local`） | `kubernetes::resolve_connector_base_url` 的 gateway 分支（**实际语义匹配 InCluster**） | 部分完成（DNS 形态匹配，但未命名/未区分策略） |
| `client/config/LocalTunnelConnectionConfig` | 本地端口转发 | **无 Rust 对应** | 待办（见 §3.2） |
| `client/config/SandboxConnectionConfig` | 连接配置接口 | 无 1:1 trait；由 `resolve_connector_base_url` 函数式覆盖 | 部分完成 |
| `client/crd/SandboxClaim` | CRD 资源对象 | `kubernetes::build_claim_body` | 已完成 |
| `client/crd/SandboxClaimSpec` | CRD spec | `kubernetes::build_claim_body`（`spec.warmPoolRef`） | 部分完成（`lifecycle`/`additionalPodMetadata` 未迁移） |
| `client/crd/SandboxClaimStatus` | CRD status | `kubernetes::KubernetesClaimStatus` + `parse_claim_status` | 已完成 |
| `client/crd/SandboxResource` | sandbox 资源状态 | `kubernetes::parse_claim_status` | 部分完成 |
| `client/exceptions/*`（8 个异常类） | 细分异常 | `error::SandboxExtensionError` / `SandboxErrorCode`（按语义归并，未 1:1） | 设计决定（归并） |
| `client/internal/K8sHelper` | K8s REST 助手 | `kubernetes::KubernetesBackend`（reqwest 直连 K8s API） | 部分完成（claim/status/exec 已覆盖；`listSandboxClaims`/watch/模板/port-forward 未迁移） |
| `client/internal/SandboxConnector` | 连接编排 | `kubernetes::resolve_connector_base_url` + `KubernetesBackend` | 部分完成（`X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP` 路由头**未注入**） |
| `client/internal/strategy/ConnectionStrategy` | 策略接口 | 无 1:1 trait | 部分完成 |
| `client/internal/strategy/DirectStrategy` | 直连策略（预置 API URL） | **无对应**（Rust 的 pod-IP 分支 ≠ DirectStrategy） | 待办（见 §3.2） |
| `client/internal/strategy/GatewayStrategy` | 网关策略（Gateway CRD + watch） | **无对应** | 待办（见 §3.2） |
| `client/internal/strategy/InClusterStrategy` | 集群内策略（DNS） | `resolve_connector_base_url`（gateway 分支） | 已完成（形态等价，但命名错位） |
| `client/internal/strategy/PortForwardStrategy` | 端口转发策略 | **无 Rust 对应** | 待办（见 §3.2） |
| `client/model/ExecutionResult` | 执行结果 | `kubernetes::KubernetesExecResult` | 已完成 |
| `client/model/FileEntry` | 文件条目 | `kubernetes_files::FileEntry` | 已完成 |
| `client/model/SandboxStatus` | sandbox 状态 | `kubernetes::KubernetesClaimStatus` | 部分完成（Java 为 `SandboxReady`/`SandboxNotReady`/`SandboxNotFound` 三态 record，Rust 为结构体 `ready`/`reason`） |

### 2.5 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / Java `HttpClient` | HTTP 传输 | `reqwest`（workspace） | 超时/头部/重试见 `http.rs`、`retry.rs` |
| Jackson (`ObjectMapper`/`SimpleModule`) | JSON (反)序列化 + 子类型注册 | `serde` + `serde_json`（workspace） | state 经 harness 封闭 enum 序列化；无 Jackson 模块概念 |
| `com.google.protobuf`（DynamicMessage，E2B envd） | envd Connect protobuf 帧 | **无** | 仅实现 JSON codec；Proto 待办（见 §3.2）。Rust `E2bCodec::Proto` 是**默认**值，`exec_in` 遇 Proto 直接返回 `SandboxExtensionError::Unsupported` |
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
| `agent::filesystem::{SandboxFilesystem, SandboxBackedFilesystem}` | `fs_spec::SandboxFilesystemAdapter` 实现 harness `SandboxFilesystem`（经 live `Sandbox`）；`SandboxBackedFilesystem` 为 harness 本地 helper | 复用 + 适配层（`SandboxFilesystemSpec` 无 harness 对应，本 crate 自建等价抽象，见 §3.2/§4.2） |
| `agent::sandbox::SandboxFileTransfer`（Java harness 接口） | Rust harness **无此 trait**；本 crate 在 `kubernetes_files.rs` 自建同名 `SandboxFileTransfer`（仅 `KubernetesFileTransfer` 实现）；E2B 侧只有 `E2bBackend::upload_file`/`download_file` 自由函数，**未实现该 trait** | 缺口（见 §3.2/§4.2） |

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
| 离线单测（137 个） | Java 测试 4554 行 | 已完成 | 分布见 §1.1 |

### 3.2 P1（重要）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `*FilesystemSpec` 四类 | `AgentRun/Daytona/E2b/KubernetesFilesystemSpec` | **已完成（适配层）** | Java 侧均 `extends harness SandboxFilesystemSpec`；**Rust harness 无对应 Spec 类型**（仅有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`），故在 `src/fs_spec.rs` 自建等价的 `SandboxFilesystemSpec`（client + workspace 投影 + snapshot 覆盖 + isolation scope）与四类 spec（`AgentRunFilesystemSpec`/`DaytonaFilesystemSpec`/`E2bFilesystemSpec`/`KubernetesFilesystemSpec`，经 `platform_spec!` 宏生成），并以 `SandboxFilesystemAdapter` 实现 harness `SandboxFilesystem`。非 1:1 继承，语义等价。**能力差异**：Java spec 还提供 `executionGuard(...)`、`workspaceProjectionEnabled(...)`、`workspaceProjectionRoots(...)` 与 `toSandboxContext(hostWorkspaceRoot)`（带 `__workspace_projection__` 条目）；Rust spec 无 guard 钩子、投影仅以 `spec.directories` 追加表达（`WORKSPACE_PROJECTION_ENTRY_KEY` 常量仅作诊断保留，未真正写入条目）。见 §4.2（不改 harness） |
| Kubernetes 文件 API | `client/Filesystem`、`client/model/FileEntry`、`SandboxFileTransfer` | **部分完成** | `src/kubernetes_files.rs`：`KubernetesFileApi`（`write`/`write_text`/`read`/`list`/`exists`/`move_path`，路径 percent-encoded）、`FileEntry` 模型、`KubernetesFileTransfer`（实现本 crate 声明的 `SandboxFileTransfer`）、`ReqwestFileApiTransport`（reqwest 实现）+ `FileApiTransport` seam。**未接线**：`KubernetesFileApi`/`KubernetesFileTransfer` **未被 `KubernetesBackend` 或 `RemoteSandbox` 构造/引用**（`rg kubernetes_files src/` 仅命中 `lib.rs` 的 `pub mod`），Java 侧 `KubernetesSandbox.files()` 与 `KubernetesSandbox implements SandboxFileTransfer` 的**沙箱生命周期内原生传输路径缺失**；`KubernetesSandboxClientOptions::file_api_base_dir` 字段存在但**无消费者**。harness 无 `SandboxFileTransfer` trait，故契约在本 crate 声明（见 §4.2） |
| E2B 原生文件传输接线 | `E2bSandbox implements SandboxFileTransfer`（`supportsFileTransfer`/`uploadFile`/`downloadFile` + `workspaceTransferPath` 校验） | **部分完成** | `e2b.rs` 已实现 `E2bBackend::upload_file`/`download_file`（envd `/files` multipart / GET，4 个离线单测），但**未实现本 crate 的 `SandboxFileTransfer` trait**，**缺 `workspaceTransferPath` 等价校验**（Java 限制路径必须在 `workspaceRoot` 之下；Rust `upload_file` 仅检查末段非空），且 `fs_spec::SandboxFilesystemAdapter` 的读写**全部走 exec+base64**，不会优先走原生传输（Java `SandboxBackedFilesystem` 会优先 `SandboxFileTransfer`） |
| E2B envd Proto codec | `E2bEnvdProcessClient`（protobuf DynamicMessage） | **部分完成** | 仅 JSON codec；`E2bCodec::Proto` 为**默认值**（与 Java 一致），`exec_in` 遇 Proto 返回 `SandboxExtensionError::Unsupported`（"set codec = E2bCodec::Json"）。未实现 protojson |
| E2B 快照删除与保留（`a0e89a3aa`） | `E2bPlatformHttp` 的 `deleteSnapshot` + `snapshotRetention` | **已完成（2026-10-20）** | `E2bBackend::delete_snapshot` 走 `DELETE /templates/{id}`（id 按单段 percent-encode，`team/x:v1` → `team%2Fx:v1`，否则 `/` 会被解析成额外路径段）；401/404 返回可诊断错误。`E2bSandboxClientOptions::snapshot_retention`（默认关闭，`Some(0)` 亦关闭）在创建原生快照后清理最旧的**本进程已知** id。**能力边界（代码注释与报告均已写明）**：E2B 无快照列表端点，`known_snapshots` 为进程内记录，重启后旧快照不可见、无法跨进程清理；`E2bSnapshotRefs` 只是 id 编码载体，不是列表来源 |
| K8s 连接策略 1:1 | `InClusterStrategy`/`PortForwardStrategy`/`ConnectionStrategy` | **部分完成** | `resolve_connector_base_url` 仅两条 URL 分支：**pod-IP 分支**（`http://<podIp>:<port>`）与 **集群 DNS 分支**（`<scheme>://<gwName>.<gwNs>.svc.cluster.local:<port>`）。后者形态对应 Java 的 **`InClusterStrategy`（DNS）**，**不是** Java `GatewayStrategy`（读 Gateway CRD `status.addresses[0].value` 并 watch 就绪）；Java `DirectStrategy`（预置 sandbox-router API URL）与 `GatewayStrategy` **均无对应实现**。另：Java `SandboxConnector` 在 router 模式下注入 `X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP` 路由头，Rust **未注入**；`PortForwardStrategy`（fabric8 `LocalPortForward`）**无对应** |
| K8s 异常类 1:1 | `client/exceptions/*`（8 类） | 设计决定（归并） | 归并到 `SandboxErrorCode`/`SandboxExtensionError`；未逐类建模 |
| 真实端点联调 | — | 待办 | 网络层已实现但无真实凭据；测试全离线（httpmock） |
| Java 测试逐条对照 | Java tests 23 文件 / 4554 行 | 待办 | Rust 单测为功能等价重写，未逐条照搬 |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| E2B 云桶挂载策略 | `e2b/mounts/E2bCloudBucketMountStrategy` | 待办 | 无 Rust 对应。**注意**：Java 该接口是**空占位**（`intentionally empty`），无实际方法，迁移价值为 0 |
| E2B 沙箱创建环境变量 | `E2bSandboxClientOptions.envs`（`Map<String,Supplier<String>>`）+ `E2bPlatformHttp` 的 `envVars` 字段 | 待办 | Rust `E2bSandboxClientOptions` **无 `envs` 字段**，`build_create_sandbox_body` 只发 `{templateID, timeout}`，**不发送 `envVars`**。Java `E2bFilesystemSpec.env(name, value)` 亦无对应 |
| K8s CRD spec 扩展字段 | `SandboxClaimSpec.lifecycle`（shutdownTime/shutdownPolicy）、`SandboxClaimSpec.additionalPodMetadata`（labels/annotations）、`CreateSandboxOptions.shutdownAfterSeconds` | 待办 | Rust `build_claim_body` 只发 `spec.warmPoolRef` |
| K8s 客户端附加能力 | `SandboxClient.listActiveSandboxes`/`listAllSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName`、`K8sHelper.listSandboxClaims` | 待办 | 无 Rust 对应 |
| K8s K8sHelper 细分能力 | `client/internal/K8sHelper` | 部分完成 | 以 reqwest 直连覆盖 claim/status/exec；其余（列表、watch、端口转发、模板）未迁移 |
| Jackson 模块 | `*HarnessSandboxJacksonModule` | 设计决定（不迁移） | serde 按 enum 变体序列化 |
| 投影哈希算法对齐 | Java `MessageDigest` | 已完成（语义等价） | Rust 用 FNV-1a；仅用于变更检测，不跨语言互操作 |
| E2B 创建请求缺 volume mount（2026-10-26 新增） | `9730e8171`/`aa66abfd0`（`E2bPlatformHttp.createSandbox` 支持 `volumeMounts`） | 待办 | `e2b.rs::build_create_sandbox_body` 只发 `{templateID, timeout}`，**无 volume mount 字段**；`E2bSandboxClientOptions` 也无对应配置。Java 侧可把宿主目录/云桶挂进沙箱，Rust 侧只能用工作区投影（exec+base64 上传）绕开。与 §3.3「E2B 云桶挂载策略」同族，但后者 Java 是空占位接口，此项是**真实能力差** |
| E2B exec 无服务端 kill + 可靠超时（2026-10-26 新增） | `9730e8171`/`aa66abfd0` | 待办 | `e2b.rs::exec_in` 收 `timeout_seconds` 后直接 `let _ = timeout_seconds;`（`:1079`）——**超时被忽略**，长命令只能等 transport 的 `read_timeout_seconds`（默认 120 s）中断，且中断后**不会**在服务端 `kill` 进程，可能留下挂死进程。Java 侧有服务端 kill + 可靠超时。既有 SANDBOX P1 已覆盖同族「E2B 原生传输/生命周期」问题 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：四个远端沙箱后端（agentrun / daytona / e2b / kubernetes）全部适配到 harness `Sandbox`/`SandboxClient` trait；共享 `RemoteBackend` glue、HTTP、重试、工作区投影、状态投影、快照接入均已落地。**四后端均为真实 reqwest HTTP 实现**（非 trait 占位）：agentrun 18 处 / daytona 13 处 / e2b 34 处 / kubernetes 14 处 HTTP 调用，各含 1 个 `impl RemoteBackend`。
- **Rust 产物**：`src/` 12 个源文件（`lib/error/http/retry/workspace/remote/agentrun/daytona/e2b/kubernetes/fs_spec/kubernetes_files.rs`），**6721 行**；**137 个单测**（`#[test]` + `#[tokio::test]`，含 `#[tokio::test(start_paused = true)]`）。
- **验证（实测，汇总方基线）**：
  ```
  cargo build -p agentscope-extensions-sandbox   -> 通过（0 error）
  cargo test  -p agentscope-extensions-sandbox   -> 108 passed / 0 failed / 0 ignored
  cargo test  --workspace                        -> 1866 passed / 0 failed（86 test target）
  ```
  > ⚠️ 上面的 `108 passed` 是**汇总方旧基线**（对应 §1.1 旧计数）。**2026-10-22 静态复核实测 `#[test]`/`#[tokio::test]` 标注数为 137**（`rg -o '#\[(tokio::)?test' src/`），逐文件：`agentrun` 11 / `daytona` 7 / `e2b` 43 / `error` 3 / `fs_spec` 18 / `http` 3 / `kubernetes` 9 / `kubernetes_files` 20 / `remote` 11 / `retry` 7 / `workspace` 5 = 137。**下次 `cargo test` 应确认实际通过数**（本轮为静态复核，未编译、未运行测试）。
- **已知问题**：无阻塞。真实厂商端点未联调（测试全离线）；若干 Java 侧能力未迁移（见 §3.2/§3.3）。**新增确认的接线缺口**：K8s 文件 API 与 E2B 原生文件传输**均未接入沙箱生命周期**（§3.2）。

### 4.2 下一步与阻塞
1. **harness 抽象缺口（不改 harness，仅记录）**：harness `agent::sandbox` 的 `state` 模块为私有（`mod state;`），`RemoteSandboxState` **未在 `sandbox/mod.rs` re-export**（`pub use state::{InMemorySandboxStateStore, LocalSandboxState, SandboxState, SandboxStateStore};` 不含 `RemoteSandboxState`）。本 crate 因此无法命名 `RemoteSandboxState`，只能经 `serde_json` 与 `SandboxState::Remote` 变体互转（见 `remote.rs` 内注释）。**建议汇总方考虑 re-export `RemoteSandboxState`**；在裁定前本 crate 用现有公开面绕过，**未修改 harness**。
2. **harness 缺少 `SandboxFilesystemSpec` / `SandboxFileTransfer`**：Java 扩展的 4 个 `*FilesystemSpec` 继承 harness Java `SandboxFilesystemSpec`（`io.agentscope.harness.agent.filesystem.spec`），`SandboxFileTransfer` 亦为 harness Java 类型（`io.agentscope.harness.agent.sandbox`）。**Rust harness 复核确认两者皆无**（`rg 'SandboxFilesystemSpec|SandboxFileTransfer' crates/agentscope-harness/src/` → 0 命中；harness 只有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`）。**已按裁定在本 crate 内建适配层**（`fs_spec.rs` 的 `SandboxFilesystemSpec` + `kubernetes_files.rs` 的 `SandboxFileTransfer`），**未改 harness/core**。若 harness 后续补齐同名类型，本 crate 的实现可直接切换继承/实现面，无需改调用点。**遗留**：Java `SandboxBackedFilesystem` 会在沙箱实现 `SandboxFileTransfer` 时**优先走原生传输**；Rust harness 无此机制，`fs_spec::SandboxFilesystemAdapter` 只能全量走 exec+base64。
3. **P1 剩余**：① E2B Proto codec（`E2bCodec::Proto` 为默认，exec 直接报 `Unsupported`）；② K8s 连接策略对象树 + 策略语义错位（gateway 分支实为 InCluster DNS，Direct/Gateway/PortForward 三策略均无对应，见 §3.2）；③ **K8s 文件 API 与 E2B 原生文件传输的沙箱生命周期接线**（组件已实现但未挂到 `RemoteSandbox`/`KubernetesBackend`/`fs_spec` 适配器，见 §3.2）。
4. **真实端点联调**：需真实 E2B/Daytona/AgentRun/K8s 凭据；当前全部请求体构造/响应解析/重试均有离线单测覆盖。
5. **跨模块需求**：无硬阻塞。本 crate 依赖 `agentscope-core`（`Msg`/`Error`/`RuntimeContext`）与 `agentscope-harness`（sandbox trait/类型、`WorkspaceSpec`/`ExecResult`/`SandboxState`/快照 trait、`filesystem`）。无 workspace 级依赖变更需求（所有依赖已在 workspace 定义或本 crate `[dependencies]` 声明）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-sandbox` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-sandbox` / `cargo test -p agentscope-extensions-sandbox` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-sandbox/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态核对**：未执行 `cargo build` / `cargo test`，未修改任何 Rust 源码；仅改本 tracker。
- 手段：Java 侧 `rg --files` + `Get-Content | Measure-Object -Line` 逐文件行数统计、`rg` 正则统计顶层类型/测试标注；Rust 侧 `rg -o '#\[(tokio::)?test'` 统计测试标注数、逐文件行数统计、`rg` 检索符号定义与调用点；harness 侧 `rg` 复核公开面（`crates/agentscope-harness/src/`）。
- 所有"已完成"判定均以**符号真实存在 + 调用点已接线**为准，而非仅看类型定义。

### 6.2 实测统计
| 项 | 原文记录 | 实测（2026-10-22） | 结论 |
|---|---|---|---|
| Java 主代码 行/文件 | 8024 / 71 | **8832 / 71** | ❌ 行数过期（-808） |
| Java 测试 行/文件 | 1577 / 14 | **4554 / 23** | ❌ 严重过期（-2977）；e2b 测试 3→11 文件、412→3295 行 |
| Java 顶层类型数 | 未记录 | **71**（+12 嵌套类型） | 补录 |
| Rust 源文件数 | 12 | **12** | ✅ |
| Rust 源码行数 | ~6069 | **6721** | ❌ 过期（-652） |
| Rust 测试数 | 108 | **137**（`#[test]`+`#[tokio::test]`） | ❌ 过期（-29） |

Rust 逐文件（行 / 测试标注）：`agentrun` 724/11、`daytona` 451/7、`e2b` 1921/43、`error` 113/3、`fs_spec` 853/18、`http` 166/3、`kubernetes` 547/9、`kubernetes_files` 748/20、`lib` 45/0、`remote` 825/11、`retry` 172/7、`workspace` 156/5。

### 6.3 逐子模块对齐结论
| 子模块 | 结论 | 依据 |
|---|---|---|
| agentrun | ✅ 对齐 | 11 类逐一有 Rust 对应（`AgentRunBackend`/`AgentRunSandboxClientOptions`/`validate_options`/`AgentRunNasMountConfig`/`AgentRunOssMountConfig`/`MAX_OSS_MOUNTS=5`/MCP 4 函数/重试策略）；真实 HTTP：`POST/GET/DELETE /sandboxes` + readiness 轮询，18 处 HTTP 调用 |
| daytona | ✅ 对齐 | 8 类逐一有 Rust 对应（`DaytonaBackend`/`DaytonaSandboxClientOptions`/`DaytonaRetry`/`fs_spec::DaytonaFilesystemSpec`）；真实 HTTP：`POST /sandbox`、`POST /sandbox/{id}/start`、`GET/DELETE /sandbox/{id}`、`POST /toolbox/{id}/process/execute`（`relative_cwd` 已对齐），13 处 HTTP 调用 |
| e2b | ⚠️ 部分对齐 | 13 类中 12 类有 Rust 对应；`E2bEnvdProcessClient` 的 **Proto codec 未实现**（`E2bCodec::Proto` 为默认 → `Unsupported`）；`E2bCloudBucketMountStrategy` **Java 侧本身是空占位接口**（无方法），迁移价值 0；**新增缺口**：Java `E2bSandboxClientOptions.envs` → `envVars` 未迁移（Rust 创建体只有 `{templateID,timeout}`）；`E2bSandbox implements SandboxFileTransfer` 未对齐（见 §6.4 P1-3） |
| kubernetes | ⚠️ 部分对齐 | 39 类中 `client/Filesystem`、`client/model/FileEntry`、`client/config/LocalTunnelConnectionConfig`、`strategy/{Direct,Gateway,PortForward}Strategy` 及 `SandboxClaimSpec` 扩展字段无对应；`SandboxConnector` 的 `X-Sandbox-*` 路由头未注入；`K8sHelper` 列表/watch 能力未迁移 |
| `fs_spec`（四类 `*FilesystemSpec`） | ⚠️ 类型齐全 / 语义有差 | 四个 spec 类型真实存在（`platform_spec!` 宏生成）；**能力差异**：缺 `executionGuard(...)` 钩子、缺 `workspaceProjectionEnabled/Roots(...)`、缺 `toSandboxContext(hostWorkspaceRoot)` 的 `__workspace_projection__` 条目写入（`WORKSPACE_PROJECTION_ENTRY_KEY` 仅作诊断常量保留） |
| `kubernetes_files`（文件 API + `SandboxFileTransfer`） | ⚠️ 组件完成 / **未接线** | `KubernetesFileApi`（write/read/list/exists/move_path）+ `FileEntry` + `KubernetesFileTransfer` + `ReqwestFileApiTransport` + `FileApiTransport` seam 全部存在且 20 个离线单测；但 `rg 'kubernetes_files' src/` 仅命中 `lib.rs` 的 `pub mod` → **无任何调用方**；`KubernetesSandboxClientOptions::file_api_base_dir` 无消费者；Java `KubernetesSandbox.persistWorkspace` 在 `fileApiBaseDir` 启用时走文件 API，Rust `persist_tar` **恒走 base64-over-exec** |

### 6.4 新发现缺口
| 优先级 | 缺口 | 说明 |
|---|---|---|
| **P1** | K8s 文件 API 未接入沙箱生命周期 | `KubernetesFileApi`/`KubernetesFileTransfer` 无调用方；`file_api_base_dir` 无消费者；K8s 工作区持久化/回灌恒走 base64-over-exec（Java 走文件 API）。**需接线**：`KubernetesBackend` 构造 `KubernetesFileApi`（base = `resolve_connector_base_url` + `file_api_base_dir`）→ `RemoteSandbox::persist/hydrate` 分支走文件 API → `fs_spec::SandboxFilesystemAdapter` 优先原生传输 |
| **P1** | E2B 原生文件传输未对齐 `SandboxFileTransfer` | `E2bBackend::upload_file/download_file` 存在但未实现本 crate `SandboxFileTransfer` trait；缺 Java `workspaceTransferPath` 等价校验（Rust 仅检查末段非空，未限制在 `workspaceRoot` 之下） |
| **P1** | K8s 连接策略语义错位 | Rust "gateway" 分支拼 `*.svc.cluster.local` 集群 DNS → 实为 Java `InClusterStrategy`；Java `DirectStrategy`（预置 router API URL）与 `GatewayStrategy`（Gateway CRD `status.addresses[0].value` + watch）**均无对应**；`PortForwardStrategy`（fabric8 `LocalPortForward`）无对应；`X-Sandbox-ID/Namespace/Port/Pod-IP` 路由头未注入 |
| **P1** | harness 无 `SandboxFileTransfer` → 原生传输无法被 harness 优先选用 | Java `SandboxBackedFilesystem` 会 `instanceof SandboxFileTransfer` 优先原生传输；Rust harness 无此 trait，`SandboxFilesystemAdapter` 只能全量 exec+base64（大文件受 128KiB 参数长度限制） |
| **P2** | E2B 沙箱创建环境变量（`envs` → `envVars`）未迁移 | `E2bSandboxClientOptions` 无 `envs` 字段；`build_create_sandbox_body` 不发 `envVars`；`E2bFilesystemSpec.env(name, value)` 无对应 |
| **P2** | K8s CRD spec 扩展字段未迁移 | `SandboxClaimSpec.lifecycle`（shutdownTime/shutdownPolicy）、`additionalPodMetadata`（labels/annotations）、`CreateSandboxOptions.shutdownAfterSeconds`；`build_claim_body` 只发 `spec.warmPoolRef` |
| **P2** | K8s 客户端附加能力未迁移 | `SandboxClient.listActiveSandboxes/listAllSandboxes/deleteAll/getSandboxClaimWarmPoolName`、`K8sHelper.listSandboxClaims`、watch 就绪（Rust 用轮询代替） |
| **P2** | `client/Constants` 常量不完整 | 缺 `GATEWAY_API_GROUP/VERSION/PLURAL`、`SANDBOX_API_GROUP/VERSION/PLURAL_NAME`、`ROUTER_SERVICE_NAME`、`ROUTER_PORT`、`SANDBOX_NAME_HASH_LABEL`、`POD_NAME_ANNOTATION` |

### 6.5 勘误列表（原文 → 实际）
| # | 原文 | 实际 | 处理 |
|---|---|---|---|
| 1 | Java 规模「主代码 8024 / 测试 1577」 | 主 **8832** / 测试 **4554**（文件 71 / 23） | §1 已改 |
| 2 | agentrun「11 / 1624，测试 3 / 315」 | 主 11 / **1626**，测试 **4 / 375** | §1 已改 |
| 3 | e2b「13 / 1415，测试 3 / 412」 | 主 13 / **2216**，测试 **11 / 3295** | §1 已改 |
| 4 | kubernetes「39 / 4094，测试 8 / 850」 | 主 39 / **4099**，测试 8 / **884** | §1 已改 |
| 5 | Rust「12 个源文件，~6069 行」 | 12 个源文件，**6721 行** | §1.1 已改 |
| 6 | Rust 单测「108」 | **137**（`#[test]`+`#[tokio::test]`，`retry.rs` 4→7、`fs_spec.rs` 17→18、`e2b.rs` 15→43） | §1.1/§3.1/§4.1 已改 |
| 7 | §1.1 `e2b.rs` 单测 15 | **43** | §1.1 已改 |
| 8 | §1.1 `fs_spec.rs` 单测 17 | **18** | §1.1 已改 |
| 9 | §1.1 `retry.rs` 单测 7 | 7 ✅（未变） | — |
| 10 | §2.1–§2.4 四类 `*FilesystemSpec` → 「**无 Rust 对应** / 待办」 | 四类均已存在于 `fs_spec.rs`（`AgentRunFilesystemSpec`/`DaytonaFilesystemSpec`/`E2bFilesystemSpec`/`KubernetesFilesystemSpec`） | §2.1–§2.4 已改为「已完成（适配层）」 |
| 11 | §2.4 `KubernetesFilesystemSpec` → 「无 Rust 对应 / 待办」 | 同上 | §2.4 已改 |
| 12 | §2.4 `client/Filesystem` → 「**无 Rust 对应** / 待办」 | `kubernetes_files::KubernetesFileApi`（write/read/list/exists/move_path） | §2.4 已改（补注未接线） |
| 13 | §2.4 `client/model/FileEntry` → 「无 Rust 对应 / 待办」 | `kubernetes_files::FileEntry` | §2.4 已改 |
| 14 | §2.4 `client/config/DirectConnectionConfig`（直连 pod IP）→ 已完成 | Java `DirectStrategy` = **预置 router API URL**；Rust pod-IP 分支 ≠ 它；Java pod-IP/DNS 属 `InClusterStrategy` | §2.4/§3.2 已改（部分完成） |
| 15 | §2.4 `client/config/GatewayConnectionConfig` → 已完成 | Java `GatewayStrategy` 读 **Gateway CRD 外部地址**并 watch；Rust gateway 分支是**集群 DNS**（= Java InCluster） | §2.4/§3.2 已改（部分完成） |
| 16 | §2.4 `client/internal/strategy/InClusterStrategy` → 「**无策略实现** / 待办」 | Rust gateway 分支 DNS 形态**即为** InCluster | §2.4 已改（已完成，形态等价但命名错位） |
| 17 | §2.4 `client/internal/strategy/DirectStrategy` / `GatewayStrategy` → 已完成 | 二者**均无对应** | §2.4 已改（待办） |
| 18 | §2.4 `client/crd/SandboxClaimSpec` → 部分完成（`spec.warmPoolRef`） | 正确，另需补记 `lifecycle`/`additionalPodMetadata` 缺失 | §2.4 已补充说明 |
| 19 | §2.4 `client/Constants` → 已完成 | 缺 `GATEWAY_*`/`SANDBOX_*`/`ROUTER_*`/`SANDBOX_NAME_HASH_LABEL`；`CREATED_BY_VALUE` 值有意改为 `agentscope-rust-client` | §2.4 已改为部分完成 |
| 20 | §2.3 `E2bEnvdProcessClient` → 「JSON codec 已实现；Proto 未实现」 | 正确；**另已实现** envd `/files` 上传/下载（`upload_file`/`download_file`/`files_url`） | §2.3 已补充 |
| 21 | §3.2「Kubernetes 文件 API」→ **已完成** | 组件完成但**未接线**（无调用方、`file_api_base_dir` 无消费者、K8s 持久化恒走 base64-over-exec） | §3.2 已改为**部分完成** |
| 22 | §3.2「E2B envd Proto codec」→ `E2bCodec::Proto` 为"占位默认" | Proto 是**实配默认**（与 Java 一致），exec 遇 Proto 返回 `Unsupported` | §3.2 已改写 |
| 23 | §4.1「108 个单测」 | **137** | §4.1 已改并标注旧基线来源 |
| 24 | §4.2-2「harness 缺少 `SandboxFilesystemSpec`/`SandboxFileTransfer`」 | 复核**确认**（`rg` 0 命中）；补记 Java `SandboxBackedFilesystem` 优先原生传输、Rust harness 无此机制 | §4.2 已补 |
| 25 | §4.2-1「`RemoteSandboxState` 未 re-export」 | 复核**确认**（`sandbox/mod.rs:47` 的 `pub use state::{...}` 不含 `RemoteSandboxState`） | §4.2 已补行号 |
| 26 | §3.3「E2B 云桶挂载策略 → 待办（无 Rust 对应）」 | 属实，但 Java `E2bCloudBucketMountStrategy` 是**空占位接口**（`intentionally empty`，无方法） | §3.3 已补注（建议降级/可标"设计决定（不迁移）"） |
| 27 | §3.1「共享 HTTP/重试 → 已完成」 | 复核**属实**：四后端均为真实 reqwest 实现，非 trait 占位；`RetryPolicy` 4 策略与 Java 状态码集合一致（agentrun/daytona 429/502/503；e2b +408；kubernetes 429/500/502/503/504） | 维持 |

> **未发现的问题**（复核通过，予以确认）：四后端 `RemoteBackend` 实现齐备；`impl Sandbox for RemoteSandbox` / `impl SandboxClient for RemoteSandboxClient` 存在；§2.6 harness 复用契约表中除 `SandboxFileTransfer` 外全部成立；`workspace.rs`/`remote.rs` 投影哈希语义等价（FNV-1a vs MD5，仅用于变更检测）；Jackson 模块"设计决定（不迁移）"成立。


