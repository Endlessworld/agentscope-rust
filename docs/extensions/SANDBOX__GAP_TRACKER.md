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
| Java 规模（实测行数） | 主代码 8832 / 测试 4554（见下表；2026-10-22 静态复核实测，见 history/SANDBOX__MIGRATION_HISTORY.md） |
| Rust crate | `crates/agentscope-extensions-sandbox` |
| 迁移波次 | A |
| 当前状态 | **P0 修复中**（历史口径「已完成 / P1/P2 清零」；2026-10-31 全量对齐复核新增 3 个 P0，见 §3.5；2026-10-31 实测 227 passed / 0 failed，另 1 个环境门控集成用例）—— PortForward / K8s 文件 API / fs_spec projection+guard 均已落地接线，E2B 原生传输亦已接线（见 §3.2 与 history/SANDBOX__MIGRATION_HISTORY.md）。⚠️ **保留差异（若汇总方视为 P1 则需上调，不得静默降级）**：① `Gateway` 连接策略为「一次性读 + 有界轮询等地址出现」，**无后台 watch 任务**（Java 用 fabric8 `Watch`；Rust 无宿主任务/取消句柄，长任务会泄漏；且 Rust 每次调用重解析地址，强于 Java 的 `baseUrl` 缓存，见 history/SANDBOX__MIGRATION_HISTORY.md）；② `K8sHelper` 的 **`watch=true` 流式订阅**、模板（`SandboxTemplate`）、port-forward 其余能力未迁移（list/get/delete/warmPool 已于二轮完成，见 history/SANDBOX__MIGRATION_HISTORY.md）；③ `listActiveSandboxes` 的「prune 非 active 条目」无等价存活信号（改为删除即摘除）。~~K8sHelper 的 list（`listSandboxClaims`/`listActiveSandboxes`/`deleteAll`）~~ → **已于 2026-10-30 二轮完成** |

| Java 子模块 | 主代码 文件/行 | 测试 文件/行 | 顶层类型数 |
|---|---|---|---|
| agentrun | 11 / 1626 | 4 / 375 | 11 |
| daytona | 8 / 891 | 0 / 0 | 8 |
| e2b | 13 / 2216 | 11 / 3295 | 13 |
| kubernetes | 39 / 4099 | 8 / 884 | 39 |
| **合计** | **71 / 8832** | **23 / 4554** | **71** |

> 顶层类型数 = 每个 `.java` 主源文件恰含 1 个顶层类型（另有 12 个嵌套类型：K8s 8 个异常类各 1 个 + `SandboxClaimSpec` 3 个静态内部类 + `CreateSandboxOptions.Builder`），故 71 文件 = 71 顶层类型。

> **2026-10-31 全量对齐复核**：Java 四个子模块在 Rust 侧**全部已迁移**（agentrun / daytona / e2b / kubernetes 均有真实 reqwest 后端，证据见 §3.5 首段）。本轮新发现 **3 个 P0** 与 **1 个待裁定（harness 生命周期）**，另有 6 个 P1、5 个 P2 缺口，清单见 §3.5；差异裁定归并到 §4.3。

### 1.1 Rust crate 文件清单（12 个源文件，12054 行 / 227 test，2026-10-31 静态统计）
| 文件 | 职责 | 单测 |
|---|---|---|
| `src/lib.rs` | crate 根：模块声明、re-export、架构文档 | 0 |
| `src/error.rs` | `SandboxExtensionError` / `SandboxErrorCode` / `SandboxResult` | 3 |
| `src/http.rs` | 共享 reqwest 客户端、URL 拼接、JSON/空响应收发 | 3 |
| `src/retry.rs` | `RetryPolicy`（agentrun/daytona/e2b/kubernetes）+ `with_retries` | 7 |
| `src/workspace.rs` | `WorkspaceSpec` → shell 脚本投影、路径解析、`shell_quote` | 5 |
| `src/remote.rs` | 核心 glue：`RemoteBackend` trait（含 `file_transfer` 能力位）、`RemoteSandbox`（impl harness `Sandbox`）、`RemoteSandboxClient`（impl harness `SandboxClient`）、`RemoteSandboxStateData` | 12 |
| `src/agentrun.rs` | AgentRun 后端（options/校验/MCP exec/数据面 URL） | 11 |
| `src/daytona.rs` | Daytona 后端（options/请求体/exec 解析） | 7 |
| `src/e2b.rs` | E2B 后端（options/envd Connect/JSON 编解码/快照标记/`E2bFileTransfer`/tar 经 envd 文件 API） | 63 |
| `src/kubernetes.rs` | Kubernetes 后端（SandboxClaim CRD/exec/三种连接策略/路由头/文件 API 持久化） | 22 |
| `src/fs_spec.rs` | 本 crate 自建的 `SandboxFilesystemSpec` 等价抽象（client + workspace + snapshot + isolation scope）+ 四个 `*FilesystemSpec` + `SandboxFilesystemAdapter`（impl harness `SandboxFilesystem`，原生传输优先） | 22 |
| `src/kubernetes_files.rs` | Kubernetes 文件 API（upload/download/list/exists）+ 本 crate 声明并实现的 `SandboxFileTransfer` trait（`KubernetesFileTransfer`），经 `FileApiTransport` seam 离线可测 | 21 |
| **合计** | **12054 行** | **227** |
> 2026-10-31 逐文件静态重统计（`#[test]`+`#[tokio::test]`）：agentrun 11 / daytona 7 / e2b 74 / error 3 / fs_spec 29 / http 6 / kubernetes 49 / kubernetes_files 22 / lib 0 / remote 14 / retry 7 / workspace 5 = **227**。上表逐文件列为 2026-10-28 快照，以本行为准。

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
| `E2bSandbox` | harness `Sandbox` 实现 | `remote::RemoteSandbox` + `e2b::E2bBackend` | 已完成（含 `SandboxFileTransfer` = `e2b::E2bFileTransfer`，2026-10-28） |
| `E2bSandboxClient` | harness `SandboxClient` 工厂 | `e2b::E2bSandboxClient`（别名）、`e2b::e2b_client` | 已完成 |
| `E2bSandboxClientOptions` | 客户端配置 | `e2b::E2bSandboxClientOptions` | 已完成 |
| `E2bSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `E2bEnvdProcessClient` | envd Connect/进程启动与流式事件 + 文件传输 | `e2b::{build_envd_start_json_body, encode_connect_envelope, decode_connect_frames, parse_envd_event, drain_envd_stream, EnvdEvent, EnvdStreamResult, E2bFileTransfer, upload_file, download_file, files_url, workspace_transfer_path}` | **已完成（2026-10-29 勘误）**：原记「部分完成」；实际 JSON codec + envd `/files` 上传/下载 + `workspaceTransferPath` 路径校验均已落地，并由 `E2bFileTransfer` 实现本 crate 的 `SandboxFileTransfer`。Proto codec 为**设计决定（不迁移）**。依据 §3.2「E2B 原生文件传输接线」「E2B envd Proto codec」两行 / 见 history/SANDBOX__MIGRATION_HISTORY.md ② / 用例 `workspace_transfer_path_stays_inside_the_workspace`、`file_transfer_uploads_and_downloads_through_envd_files`、`persist_tar_uses_envd_files_instead_of_base64_over_exec` |
| `E2bPlatformHttp` | 平台 HTTP（create/connect/snapshot） | `e2b::E2bBackend` + `parse_sandbox_fields`/`envd_host`/`basic_auth_header` | 已完成 |
| `E2bRetry` | 瞬态失败重试（含 408） | `retry::RetryPolicy::e2b` | 已完成 |
| `E2bSnapshotRefs` | 原生快照标记字节 | `e2b::E2bSnapshotRefs`（magic prefix + 编解码） | 已完成 |
| `E2bPersistenceMode` | 持久化模式枚举 | `e2b::E2bPersistenceMode` | 已完成 |
| `E2bCodec` | Connect 编解码枚举 | `e2b::E2bCodec`（默认 `Json`） | 已完成（默认值有意改为可用 codec，见 §3.2） |
| `E2bFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::E2bFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层，见 §3.2） |
| `E2bHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `E2bCloudBucketMountStrategy`（`mounts/`） | 云桶挂载策略 | **无 Rust 对应** | 已完成（设计决定，不迁移，见 §3.3）。**注**：Java 该接口是**空占位**（`intentionally empty`，无任何方法），迁移价值为 0 |

### 2.4 kubernetes 子模块（`io.agentscope.extensions.sandbox.kubernetes`，39 类）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `KubernetesSandbox` | harness `Sandbox` 实现（`SandboxFileTransfer`） | `remote::RemoteSandbox` + `kubernetes::KubernetesBackend` | **已完成（2026-10-29 勘误）**：原记「部分完成（`SandboxFileTransfer` 仅在 `kubernetes_files.rs` 独立声明，未挂到 `RemoteSandbox`/`KubernetesBackend`）」；实际已接通——`KubernetesBackend::native_file_transfer` → `RemoteBackend::file_transfer` → `RemoteSandbox::file_transfer()`。依据 §3.2「Kubernetes 文件 API」行 / 见 history/SANDBOX__MIGRATION_HISTORY.md ① / 用例 `remote::tests::native_file_transfer_is_published_after_the_handle_is_established`、`file_transfer_uploads_and_downloads_under_the_base_dir` |
| `KubernetesSandboxClient` | harness `SandboxClient` 工厂 | `kubernetes::KubernetesSandboxClient`（别名）、`kubernetes::kubernetes_client` | 已完成 |
| `KubernetesSandboxClientOptions` | 客户端配置 | `kubernetes::KubernetesSandboxClientOptions` | 已完成 |
| `KubernetesSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `KubernetesFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::KubernetesFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层） |
| `KubernetesHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `client/CommandExecutor` | 在 sandbox 服务器上执行命令 | `kubernetes::{build_execute_body, parse_execute_result, KubernetesExecResult}` | 已完成 |
| `client/Constants` | CRD group/version/plural、标签、端口 | `kubernetes::{CLAIM_API_GROUP, CLAIM_API_VERSION, CLAIM_PLURAL_NAME, CREATED_BY_LABEL, CREATED_BY_VALUE, DEFAULT_SERVER_PORT, GATEWAY_API_GROUP, GATEWAY_API_VERSION, GATEWAY_PLURAL, HEADER_SANDBOX_ID, HEADER_SANDBOX_NAMESPACE, HEADER_SANDBOX_PORT, HEADER_SANDBOX_POD_IP, ROUTER_SERVICE_NAME, ROUTER_PORT, DEFAULT_ROUTER_NAMESPACE}` | 已完成（2026-10-31） | 2026-10-28 补 `GATEWAY_*`/`X-Sandbox-*`，2026-10-30 补 `ROUTER_*`/`DEFAULT_ROUTER_NAMESPACE`。**未迁移两项均为设计决定**：① `SANDBOX_API_GROUP/VERSION/PLURAL_NAME` 在 Java main 中**仅声明、零引用**（`rg SANDBOX_API_GROUP src/main` 只命中 `Constants.java` 自身）⇒ 迁过去就是死常量；② `POD_NAME_ANNOTATION`/`SANDBOX_NAME_HASH_LABEL` 属读 `Sandbox` CRD 的路径（`Sandbox#getPodName/getSandboxNameHash`，仅被 `KubernetesSandboxClient:130-135` 用来回填 state），Rust 改用 **claim status** 的 `status.sandbox.name` + `podIPs`（`select_pod_ip`）达到同一目的，不需额外读 Sandbox 资源。`CREATED_BY_VALUE` 由 `agentscope-java-client` 改为 `agentscope-rust-client`，属有意的跨语言标签差异 |
| `client/CreateSandboxOptions` | 创建参数 | `kubernetes::{build_claim_body, build_claim_body_with_options, claim_lifecycle, KubernetesSandboxClientOptions}` | 已完成（2026-10-31） | warm pool / claimName（`agentscope-<session>`）已覆盖；本轮补 `claim_labels`、`shutdown_after_seconds`（→ `lifecycle`）、`pod_labels`/`pod_annotations`（→ `additionalPodMetadata`），经 `create_sandbox` 发往 `POST .../sandboxclaims`（用例 `create_sandbox_sends_the_extended_claim_spec`） |
| `client/Sandbox` | sandbox 服务器句柄（exec/files/状态） | `kubernetes::KubernetesBackend` + `remote::RemoteSandbox` | 已完成（exec/状态/`files()` 均已接入，2026-10-28） |
| `client/SandboxClient` | K8s 侧客户端（`AutoCloseable`） | `kubernetes::KubernetesBackend` + `remote::RemoteSandboxClient` | 已完成（2026-10-30/31） | `create_sandbox`/`connect_sandbox`/`kill_sandbox` + `list_sandbox_claims`/`get_sandbox_claim`/`get_sandbox_claim_warm_pool_name`/`delete_sandbox_claim`/`list_active_sandboxes`/`delete_all` 均已落地（见 history/SANDBOX__MIGRATION_HISTORY.md）；`close()` 语义由 `RemoteSandbox::shutdown` 承担（关闭原生传输 + 释放句柄） |
| `client/Filesystem` | 文件 API（读写/列目录/移动） | `kubernetes_files::KubernetesFileApi`（`write`/`read`/`list`/`exists`/`move_path`） | 已完成（2026-10-28 已接线到 `KubernetesBackend`/`RemoteSandbox`，见 §3.2/见 history/SANDBOX__MIGRATION_HISTORY.md） |
| `client/config/DirectConnectionConfig` | 直连（**预置 router API URL**） | `kubernetes::KubernetesSandboxClientOptions::sandbox_api_url`（+ `connection_strategy = Direct`） | 已完成（2026-10-28；`api_url` 仍专指 K8s API 地址，二者分离） |
| `client/config/GatewayConnectionConfig` | 网关连接（**Gateway CRD 外部地址 + watch**） | `KubernetesConnectionStrategy::Gateway` + `resolve_gateway_address`（CRD `status.addresses[0].value` 或注入的 `gateway_address`） | 已完成（无 watch：一次性读取，已记差异） |
| `client/config/InClusterConnectionConfig` | 集群内连接（`sandboxId.namespace.svc.cluster.local`） | `KubernetesConnectionStrategy::InCluster` + `resolve_connector_base_url` | 已完成（pod IP → sandbox 服务 DNS） |
| `client/config/LocalTunnelConnectionConfig` | 本地端口转发 | `KubernetesConnectionStrategy::PortForward` + `KubernetesPortForwarder` seam + `resolve_port_forward_base_url` | 已完成（2026-10-30，见 history/SANDBOX__MIGRATION_HISTORY.md；未注入 forwarder 时明确报配置错误、不降级；默认策略仍为 InCluster，差异已在 §3.2 注明） |
| `client/config/SandboxConnectionConfig` | 连接配置接口 | 无 1:1 trait；由 `KubernetesConnectionStrategy` 枚举 + `resolve_connector_base_url` 函数式覆盖 | 已完成（枚举代替 trait 分派） |
| `client/crd/SandboxClaim` | CRD 资源对象 | `kubernetes::build_claim_body` | 已完成 |
| `client/crd/SandboxClaimSpec` | CRD spec | `kubernetes::build_claim_body_with_options`（`spec.warmPoolRef` + 可选 `lifecycle`/`additionalPodMetadata`） | 已完成（2026-10-31；空则不写，旧字节形状不变） |
| `client/crd/SandboxClaimStatus` | CRD status | `kubernetes::KubernetesClaimStatus` + `parse_claim_status` | 已完成 |
| `client/crd/SandboxResource` | sandbox 资源状态 | `kubernetes::parse_claim_status`（claim status） | 已完成（等价，设计决定：不读 `Sandbox` CRD） | claim status 里的 `status.sandbox.name`/`podIPs` 已含所需字段，`connect_sandbox` 用 claim 而非 Sandbox 资源回填 handle，省一次 CRD 读取；Java 的 `Sandbox#status()` 三态见下一行 |
| `client/exceptions/*`（8 个异常类） | 细分异常 | `error::SandboxExtensionError` / `SandboxErrorCode`（按语义归并，未 1:1） | 已完成（设计决定，不迁移：归并到错误码；Java 8 类只用于 `catch` 分支区分，Rust 用 `SandboxErrorCode` 枚举区分，`as_str()` 已含 `SANDBOX_CONNECTION_CLOSED`） |
| `client/internal/K8sHelper` | K8s REST 助手 | `kubernetes::KubernetesBackend`（reqwest 直连 K8s API） | 已完成（2026-10-30/31） | claim create/get/list/delete/status/exec + `select_pod_ip` + `claim_failure` 已覆盖；`watch=true` 与「模板」为**已完成（设计决定，不迁移）**（依据见 §3.3 同名行 / 见 history/SANDBOX__MIGRATION_HISTORY.md），port-forward 由 `KubernetesPortForwarder` seam 覆盖（见 history/SANDBOX__MIGRATION_HISTORY.md） |
| `client/internal/SandboxConnector` | 连接编排 | `kubernetes::KubernetesBackend::{connector_base_url, router_headers}`（`X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP` 已注入，`InCluster` 除外） | 已完成（无 watch/重试退避与 Java 一致，但无显式 `connect()` 生命周期） |
| `client/internal/strategy/ConnectionStrategy` | 策略接口 | `kubernetes::KubernetesConnectionStrategy`（枚举，含 `should_inject_router_headers`） | 已完成 |
| `client/internal/strategy/DirectStrategy` | 直连策略（预置 API URL） | `KubernetesConnectionStrategy::Direct` | 已完成 |
| `client/internal/strategy/GatewayStrategy` | 网关策略（Gateway CRD + watch） | `KubernetesConnectionStrategy::Gateway` + `resolve_gateway_address`/`parse_gateway_address` | 已完成（无 watch） |
| `client/internal/strategy/InClusterStrategy` | 集群内策略（DNS） | `KubernetesConnectionStrategy::InCluster` | 已完成 |
| `client/internal/strategy/PortForwardStrategy` | 端口转发策略 | `KubernetesConnectionStrategy::PortForward` + `parse_endpoint_target_pod`/`resolve_router_pod` + `KubernetesPortForwarder` seam | 已完成（2026-10-30，见 history/SANDBOX__MIGRATION_HISTORY.md） |
| `client/model/ExecutionResult` | 执行结果 | `kubernetes::KubernetesExecResult` | 已完成 |
| `client/model/FileEntry` | 文件条目 | `kubernetes_files::FileEntry` | 已完成 |
| `client/model/SandboxStatus` | sandbox 状态 | `kubernetes::KubernetesClaimStatus` + `get_sandbox_claim` 的 `Ok(None)`（404） | 已完成（等价结构，设计决定：不 1:1） | Java `SandboxReady`/`SandboxNotReady`/`SandboxNotFound` 三态 record ↔ Rust `KubernetesClaimStatus{ready, reason, sandbox_name, pod_ips}` + `None`；再造一层三态枚举属投机性代码，调用方（`RemoteBackend`）只需要 ready/reason 与句柄字段 |

### 2.5 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / Java `HttpClient` | HTTP 传输 | `reqwest`（workspace） | 超时/头部/重试见 `http.rs`、`retry.rs` |
| Jackson (`ObjectMapper`/`SimpleModule`) | JSON (反)序列化 + 子类型注册 | `serde` + `serde_json`（workspace） | state 经 harness 封闭 enum 序列化；无 Jackson 模块概念 |
| `com.google.protobuf`（DynamicMessage，E2B envd） | envd Connect protobuf 帧 | **无** | 仅实现 JSON codec。**已完成（2026-10-29 勘误）**：原记「Proto 待办；`E2bCodec::Proto` 是默认值」，实际默认已改为 `E2bCodec::Json`（`#[default]` + `E2bSandboxClientOptions::default().codec` + `E2bCodec::parse` 回落三处一致），Proto 明确为**设计决定（不迁移）**，显式选择时 `exec_in` 返回 `SandboxExtensionError::Unsupported`。依据 §3.2「E2B envd Proto codec」行 / 见 history/SANDBOX__MIGRATION_HISTORY.md ④ / 用例 `default_codec_exec_works_without_opt_in`、`proto_codec_exec_is_unsupported` |
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
| `agent::sandbox::SandboxFileTransfer`（Java harness 接口） | Rust harness **无此 trait**；本 crate 在 `kubernetes_files.rs` 自建同名 `SandboxFileTransfer`，由 `KubernetesFileTransfer`（文件 API）与 `e2b::E2bFileTransfer`（envd `/files`）实现，并通过 `RemoteBackend::file_transfer` / `RemoteSandbox::file_transfer` 接入 `fs_spec::SandboxFilesystemAdapter` | 已完成（适配层；harness 仍无此 trait，见 §3.2/§4.2） |

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
| 离线单测（**176** 个；2026-10-29 勘误，原记 137） | Java 测试 4554 行 | 已完成 | 2026-10-28 实测 **176 passed / 0 failed**（§4.1 / 见 history/SANDBOX__MIGRATION_HISTORY.md），分布见 §1.1 |

### 3.2 P1（重要）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `*FilesystemSpec` 四类 | `AgentRun/Daytona/E2b/KubernetesFilesystemSpec` | **已完成（适配层）** | Java 侧均 `extends harness SandboxFilesystemSpec`；**Rust harness 无对应 Spec 类型**（仅有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`），故在 `src/fs_spec.rs` 自建等价的 `SandboxFilesystemSpec`（client + workspace 投影 + snapshot 覆盖 + isolation scope）与四类 spec（`AgentRunFilesystemSpec`/`DaytonaFilesystemSpec`/`E2bFilesystemSpec`/`KubernetesFilesystemSpec`，经 `platform_spec!` 宏生成），并以 `SandboxFilesystemAdapter` 实现 harness `SandboxFilesystem`。非 1:1 继承，语义等价。**能力差异**：Java spec 还提供 `executionGuard(...)`、`workspaceProjectionEnabled(...)`、`workspaceProjectionRoots(...)` 与 `toSandboxContext(hostWorkspaceRoot)`（带 `__workspace_projection__` 条目）；Rust 侧 2026-10-30 前无 guard 钩子、`__workspace_projection__` 仅留常量未真写入——**已补齐（见 history/SANDBOX__MIGRATION_HISTORY.md）**。见 §4.2（不改 harness）。**2026-10-30 已补齐** `execution_guard` / `workspace_projection_enabled` / `workspace_projection_roots`，`__workspace_projection__` 真正写入 harness `LayoutSpec` 条目并可生成投影 tar，见 history/SANDBOX__MIGRATION_HISTORY.md |
| Kubernetes 文件 API | `client/Filesystem`、`client/model/FileEntry`、`SandboxFileTransfer` | **已完成（2026-10-28 接线）** | `src/kubernetes_files.rs`：`KubernetesFileApi`（`write`/`write_text`/`read`/`list`/`exists`/`move_path`，路径 percent-encoded）、`FileEntry` 模型、`KubernetesFileTransfer`（实现本 crate 声明的 `SandboxFileTransfer`）、`ReqwestFileApiTransport`（reqwest 实现）+ `FileApiTransport` seam；新增 `file_api_path` / `temp_archive_rel_path`（对齐 Java `fileApiPath`/`tempArchiveName`，UUID 去重避免并发互删）。**已接线**：`KubernetesBackend::native_file_transfer(handle)` 在 `file_api_base_dir` 非空时构造文件 API（base = connector URL，带 `X-Sandbox-*` 路由头 + `KubernetesHandleExec` exec 回退供 `move` 用）；`RemoteBackend::file_transfer` 新增默认方法，`RemoteSandbox` 在 `ensure_handle`/快照重建 handle 后刷新并对外提供 `RemoteSandbox::file_transfer()`；`persist_workspace_bytes`/`hydrate_workspace_bytes` 优先走文件 API（`persist_via_file_api`/`hydrate_via_file_api`，tar 到 `.agentscope-tmp` 临时文件 → 上传/下载 → `rm -f` 清理），失败或未配置时回退 base64-over-exec；`fs_spec::SandboxFilesystemAdapter` 对支持路径优先原生传输。测试：`file_transfer_needs_the_file_api_base_dir`、`file_transfer_uploads_and_downloads_under_the_base_dir`、`persist_workspace_uses_the_file_api_when_enabled`、`persist_workspace_falls_back_to_base64_over_exec`、`hydrate_workspace_uploads_the_archive_then_extracts`、`remote::tests::native_file_transfer_is_published_after_the_handle_is_established`、`kubernetes_files::tests::file_api_path_and_temp_archive_names_are_deterministic`。harness 无 `SandboxFileTransfer` trait，故契约在本 crate 声明（见 §4.2） |
| E2B 原生文件传输接线 | `E2bSandbox implements SandboxFileTransfer`（`supportsFileTransfer`/`uploadFile`/`downloadFile` + `workspaceTransferPath` 校验） | **已完成（2026-10-28 接线）** | `e2b.rs`：`E2bFileTransfer`（`backend` + `handle`）实现本 crate 的 `SandboxFileTransfer`，`E2bBackend::file_transfer` 无条件返回（与 Java 一致）；新增 `workspace_transfer_path(workspace_root, absolute_path)` 逐条对齐 Java：路径与 root 都必须绝对、必须在 `rootPrefix` 之下（`/mnt/data-x` 不算 `/mnt/data` 的子路径）、相对部分不得含空段/`.`/`..`，否则拒绝并回退 exec。`E2bBackend` 改为 `Clone`（`known_snapshots` 改 `Arc<Mutex<_>>`）以便适配器持有快照。**另**：TAR 持久化不再 base64-over-exec，改为 Java 的「`tar -cf <tmp>` → envd `GET /files` 下载 → `rm -f`」，回灌为「`POST /files` 上传 → `tar -xf`」。测试：`workspace_transfer_path_stays_inside_the_workspace`、`file_transfer_declines_paths_outside_the_workspace`、`file_transfer_uploads_and_downloads_through_envd_files`、`persist_tar_uses_envd_files_instead_of_base64_over_exec`、`hydrate_tar_uploads_the_archive_before_extracting`、`fs_spec` 的 `adapter_*transfer*` 三条 |
| E2B envd Proto codec | `E2bEnvdProcessClient`（protobuf DynamicMessage） | **已完成（设计决定：默认改为 Json）** | 仅 JSON codec；**默认改为 `E2bCodec::Json`**（`#[default]` + `E2bSandboxClientOptions::default().codec` + `E2bCodec::parse` 回落三处一致），使**默认配置可用**。`E2bCodec::Proto` 保留枚举变体以对齐 Java，显式选择时 `exec_in` 返回 `SandboxExtensionError::Unsupported`（"set codec = E2bCodec::Json"），不静默错编码。**不迁移 Proto 的理由**：envd 的 proto 路径要求 Connect + protobuf framing（`google.protobuf.DynamicMessage` 等价物）与完整 `process.Process/Start` 描述符树，需引入 `prost`/`prost-types` 并手写/生成 envd schema，无真实端点可离线校验；envd 官方同时支持 JSON codec，语义等价。测试：`persistence_mode_and_codec_parse`（含 `E2bCodec::default()` 与 options 默认断言）、`default_codec_exec_works_without_opt_in`（httpmock 证明默认配置可直接 exec）、`proto_codec_exec_is_unsupported`（显式 Proto 仍报错） |
| E2B 快照删除与保留（`a0e89a3aa`） | `E2bPlatformHttp` 的 `deleteSnapshot` + `snapshotRetention` | **已完成（2026-10-20）** | `E2bBackend::delete_snapshot` 走 `DELETE /templates/{id}`（id 按单段 percent-encode，`team/x:v1` → `team%2Fx:v1`，否则 `/` 会被解析成额外路径段）；401/404 返回可诊断错误。`E2bSandboxClientOptions::snapshot_retention`（默认关闭，`Some(0)` 亦关闭）在创建原生快照后清理最旧的**本进程已知** id。**能力边界（代码注释与报告均已写明）**：E2B 无快照列表端点，`known_snapshots` 为进程内记录，重启后旧快照不可见、无法跨进程清理；`E2bSnapshotRefs` 只是 id 编码载体，不是列表来源 |
| K8s 连接策略 1:1 | `InClusterStrategy`/`DirectStrategy`/`GatewayStrategy`/`PortForwardStrategy`/`ConnectionStrategy` | **已完成（4/4 策略；PortForward 经注入 seam）** | `KubernetesConnectionStrategy { Direct, InCluster, Gateway }` + `KubernetesSandboxClientOptions::{sandbox_api_url, connection_strategy, gateway_address}`；默认优先级对齐 Java `KubernetesSandboxClient.toConnectionConfig`：显式 option → `sandbox_api_url`（Direct）→ `gateway_name`（Gateway）→ InCluster。`resolve_connector_base_url(options, strategy, sandbox_id, pod_ip, gateway_address)`：Direct = 预置 router URL（去尾斜杠）；InCluster = `http://<podIp>:<port>`（IPv6 加方括号），无 pod IP 时用 **sandbox 自身**的 `<sandboxId>.<ns>.svc.cluster.local:<port>`（修正旧实现误用 gateway 名的偏差）；Gateway = `scheme://<addr>`（Java `formatUrl` 不追加端口）。Gateway 地址优先取注入值 `gateway_address`，否则读 CRD `GET /apis/gateway.networking.k8s.io/v1/namespaces/{ns}/gateways/{name}` → `status.addresses[0].value`（对齐 Java `GatewayStrategy`，无 RBAC 时记 warn 并报配置错误）。路由头：`connector_router_headers` 注入 `X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP`，`InCluster` 不注入（Java `shouldInjectRouterHeaders`），同时用于 `POST /execute` 与文件 API。**残留已清零（2026-10-30）**：`PortForwardStrategy` 以 `KubernetesPortForwarder` seam 落地（未注入 forwarder 时明确报错、绝不降级），见 history/SANDBOX__MIGRATION_HISTORY.md；默认策略仍为 `InCluster`（Java 默认 port-forward），差异保留并在 `resolve_connection_strategy` 文档注明。测试：`connection_strategy_derivation_matches_java_priority`、`direct_strategy_uses_the_configured_router_url`、`in_cluster_strategy_uses_pod_ip_then_service_dns`、`gateway_strategy_uses_the_external_address_without_a_port`、`router_headers_follow_the_strategy`、`exec_sends_router_headers_for_router_strategies`（httpmock 断言出站头）、`gateway_strategy_reads_the_address_from_the_crd`、`gateway_strategy_prefers_the_injected_address`、`gateway_address_is_parsed_from_the_crd_status` |
| K8s 异常类 1:1 | `client/exceptions/*`（8 类） | 已完成（设计决定，不迁移：归并到错误码） | 归并到 `SandboxErrorCode`/`SandboxExtensionError`；未逐类建模。依据：Java 侧 `catch (SandboxTemplateNotFoundException)` 这类分支在 Rust 用 `SandboxErrorCode` 区分；`TemplateNotFound`/`WarmPoolNotFound` 的**消息文本**已逐字对齐（`ClaimFailure::message`） |
| 真实端点联调 | — | **已完成（2026-10-31，环境变量门控）** | 网络层早已实现；本轮补两条环境变量门控的真实端点冒烟用例（未设变量时 `eprintln!` skip + `return`，不用 `#[ignore]`）：`e2b::tests::live_e2b_round_trip_when_a_real_api_key_is_configured`（`AGENTSCOPE_TEST_E2B_API_KEY` / `_URL` / `_TEMPLATE`：create → exec → persist → shutdown）与 `kubernetes::tests::live_kubernetes_round_trip_when_a_cluster_is_configured`（`AGENTSCOPE_TEST_K8S_API_URL` + `AGENTSCOPE_TEST_K8S_WARM_POOL`，可选 `_TOKEN` / `_NAMESPACE`：claim → exec → delete）。**凭据缺失不再是阻塞项**：默认离线跑两用例直接返回。agentrun/daytona 未加同类用例（无真实端点联调诉求，传输层已由 httpmock 覆盖） |
| Java 测试逐条对照 | Java tests 23 文件 / 4554 行 | **已完成（设计决定，不逐条照搬）** | 口径改为「每个 Java 行为分支在 Rust 有等价用例覆盖」（纯函数或 httpmock 用例），不照搬 Java 的测试文件/类划分：Java 侧大量用例依赖 fabric8 `KubernetesServer`、OkHttp `MockWebServer` 等 JVM 基建，Rust 无等价物且不应为测试引入。分支映射见 history/SANDBOX__MIGRATION_HISTORY.md（三个增量提交逐条对齐）与 §1.1（225 个用例） |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| E2B 云桶挂载策略 | `e2b/mounts/E2bCloudBucketMountStrategy` | 已完成（设计决定，不迁移）（2026-10-29 勘误：原记「待办」；Java 该类为 `intentionally empty` 空接口，无方法可迁） | 无 Rust 对应。**注意**：Java 该接口是**空占位**（`intentionally empty`），无实际方法，迁移价值为 0 |
| E2B 沙箱创建环境变量 | `E2bSandboxClientOptions.envs`（`Map<String,Supplier<String>>`）+ `E2bPlatformHttp` 的 `envVars` 字段 | **已完成（2026-10-31）** | `E2bSandboxClientOptions.envs: BTreeMap<String,String>` + `with_env`（空名拒绝）；`build_create_sandbox_body_with_envs` 输出 `envVars`（空 map 或全空值时整字段省略，对齐 Java `resolveEnvVars` 跳过空值），创建路径统一走它；`fs_spec::E2bFilesystemSpec::env(name, value)` 对应 Java `E2bFilesystemSpec#env`。测试：`create_body_with_env_vars_matches_java`、`create_body_omits_env_vars_when_empty`、`with_env_rejects_blank_names`、`platform_create_sends_env_vars`（httpmock 断言出站请求体）、`fs_spec::tests::e2b_spec_builders_thread_envs_and_volume_mounts` |
| K8s CRD spec 扩展字段 | `SandboxClaimSpec.lifecycle`（shutdownTime/shutdownPolicy）、`SandboxClaimSpec.additionalPodMetadata`（labels/annotations）、`CreateSandboxOptions.shutdownAfterSeconds` | **已完成（2026-10-31）** | options 新增 `claim_labels` / `shutdown_after_seconds` / `pod_labels` / `pod_annotations`；`build_claim_body_with_options` 合并标签（`CREATED_BY_LABEL` 恒胜、空 key 丢弃）、仅有 TTL 时发 `lifecycle`、仅 pod label/annotation 非空时发 `additionalPodMetadata`（`build_claim_body` 保留旧签名与旧字节形状）；`claim_lifecycle` 生成秒级 `ISO_INSTANT`（`Z`）+ `shutdownPolicy=Delete`，`Some(0)` 报配置错误（对齐 Java `IllegalArgumentException`）。另补 `claim_failure`（`TemplateNotFound`/`WarmPoolNotFound` 快速失败，正文逐字对齐 Java 异常）与 `select_pod_ip`（优先 IPv4，等价 `K8sHelper#selectPodIp`）。测试：`claim_body_carries_lifecycle_and_pod_metadata`、`claim_body_keeps_the_legacy_shape_without_extras`、`claim_lifecycle_rejects_zero_and_formats_iso_instant`、`claim_failure_maps_template_and_warm_pool_conditions`、`select_pod_ip_prefers_ipv4_and_skips_blanks`、`create_sandbox_sends_the_extended_claim_spec`（httpmock 断言真实请求体）、`create_sandbox_fails_fast_when_the_template_is_missing` |
| K8s 客户端附加能力 | `SandboxClient.listActiveSandboxes`/`listAllSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName`、`K8sHelper.listSandboxClaims` | **已完成（2026-10-30，见 history/SANDBOX__MIGRATION_HISTORY.md）** | `KubernetesBackend::{list_sandbox_claims(Option<&str>), get_sandbox_claim, get_sandbox_claim_warm_pool_name, delete_sandbox_claim, list_active_sandboxes, delete_all}` + 纯函数 `parse_claim_names`/`claim_warm_pool_name`；`active_claims: Arc<Mutex<BTreeSet>>` 复刻 Java `activeSandboxes`（`namespace/claimName` 键），`create_sandbox`/`connect_sandbox` 记录、`kill_sandbox`/`delete_sandbox_claim`/`delete_all` 摘除 |
| K8s K8sHelper 细分能力 | `client/internal/K8sHelper` | **已完成（2026-10-30 list/get/delete/warmPool；2026-10-31 条件快速失败。`watch=true` 与模板见右）** | reqwest 直连覆盖 claim create/get/list/delete/status/exec，并补齐 `selectPodIp` / `checkClaimFailureConditions` 等价物（`select_pod_ip` / `claim_failure`）。**`watch=true` 流式订阅：已完成（设计决定，不迁移）**——`decode_json` 用 `resp.text()` 读完整 body，而 `?watch=true` 在 `timeoutSeconds` 前不会结束，直接接会挂死；正确实现需 `bytes_stream()` 分帧 + `resourceVersion` 续订 + 取消语义，且本 crate 无宿主后台任务/取消句柄（见 history/SANDBOX__MIGRATION_HISTORY.md 已评估）；就绪等待用轮询，语义等价且已有用例。**模板（`SandboxTemplate`）：已完成（设计决定，不迁移）**——Java HEAD 里 `SandboxTemplate` 仅出现于异常名（`K8sHelper.java:350` 的 `SandboxTemplateNotFoundException`），无任何模板 CRD 读写，对应语义已由 `claim_failure` 覆盖 |
| Jackson 模块 | `*HarnessSandboxJacksonModule` | 已完成（设计决定，不迁移） | serde 按 enum 变体序列化；`rg HarnessSandboxJacksonModule crates/` → 0 命中（无子类型注册概念），state 的向后兼容靠 `#[serde(default)]` 而非 Jackson 模块 |
| 投影哈希算法对齐 | Java `MessageDigest` | 已完成（语义等价） | Rust 用 FNV-1a；仅用于变更检测，不跨语言互操作 |
| E2B 创建请求缺 volume mount（2026-10-26 新增） | `9730e8171`/`aa66abfd0`（`E2bPlatformHttp.createSandbox` 支持 `volumeMounts`） | ✅ 已完成（2026-10-27） | `e2b.rs`：新增 `E2bVolumeMount { name, path }`——`name` 是**平台侧已存在的 E2B 卷**、`path` 是沙箱内绝对路径（对齐 Java `E2bVolumeMount`；Java 的 `E2bCloudBucketMountStrategy` 在同 commit 里是**空占位接口**，故云桶挂载双方都不实现，tracker 旧描述里的“宿主目录/云桶”与 Java 实际不符，已更正）；`normalize_mount_path`（trim / `\`→`/` / 折叠 `//` / 去尾斜杠）、`covers_workspace_root`、`require_distinct_mount_paths` 逐条对齐 Java；`E2bSandboxClientOptions.volume_mounts` + `with_volume_mount`；`build_create_sandbox_body_with_mounts` 输出 `volumeMounts: [{name, path}]`，**空挂载时整字段省略**（`build_create_sandbox_body` 签名与输出完全不变），新建 / connect 兜底重建 / 原生快照恢复三条创建路径共用。测试：`create_body_with_volume_mounts_matches_java`、`create_body_without_volume_mounts_keeps_the_legacy_shape`、`platform_create_sends_volume_mounts`（httpmock 精确断言请求体）、`volume_mount_validates_name_and_normalizes_path`、`volume_mount_covers_workspace_root`、`require_distinct_mount_paths_rejects_duplicates`、`options_with_volume_mount_validates_and_rejects_duplicates`。反向验证：关掉挂载写入分支 → 前两条 FAILED。**遗留已清零（2026-10-31）**：`workspace_on_volume(workspace_root, mounts)`（root 等于某挂载或严格位于其下）→ 卷工作区 persist 返回空 marker、hydrate 对空归档 no-op；`volume_tar_exclude_args` 只对**严格位于 root 之下**的挂载产出 `--exclude=./<rel>`（root 为 `/` 时去掉分隔符），并经 `build_e2b_tar_command` 逐个 `shell_quote`；构造时拒绝相对 `workspace_root`（Java 硬化）；Rust `kill_sandbox` 只 `DELETE /sandboxes/{id}`、本就没有 `rm -rf`，故「destroy 跳过 rm -rf」自然成立。测试：`workspace_on_volume_detects_mount_coverage`、`volume_tar_exclude_args_only_cover_nested_mounts`、`build_e2b_tar_command_quotes_excludes`、`persist_and_hydrate_are_noops_for_a_volume_backed_workspace` |
| E2B exec 无服务端 kill + 可靠超时（2026-10-26 新增） | `9730e8171`/`aa66abfd0` | ✅ 已完成（2026-10-27） | Java **没有独立 kill 端点**：服务端 kill 就是在 `POST {envd}/process.Process/Start` 上带 `Connect-Timeout-Ms: <timeoutSeconds*1000>`，envd 到 deadline 终止远端进程。`e2b.rs::exec_in` 按此实现：① `timeout_seconds > 0` 时注入该头（`= 0` 必须省略——`0` 的语义是「deadline 已过期」而不是不限时）；② 客户端用 `RequestBuilder::timeout(T + CLIENT_TIMEOUT_SLACK_SECONDS(5))` 覆盖整体时限（对齐 Java per-call `callTimeout`），外层再套 `with_exec_deadline` 兜底；③ 超时统一映射为新错误码 `SandboxErrorCode::ExecTimeout`（`Command timed out after Ns`，**不含命令文本**——命令是模型生成的，可能内联密钥）；④ envd end-stream trailer `deadline_exceeded` 同样映射为 `ExecTimeout`；⑤ deadline 之后仍无退出码的流降级为 `ExecTimeout`。kill 通道抽象为可注入的 `E2bKillDeadline`（生产实现 `EnvdKillDeadline`），便于离线单测。测试：`exec_timeout_arms_the_server_side_kill_and_reports_an_error`（httpmock 断言出站头 + 记录型 fake 断言 kill 参数）、`exec_without_deadline_does_not_arm_the_kill`、`exec_deadline_expires_and_reports_a_timeout`、`connect_timeout_ms_header_arms_the_server_side_kill`、`exec_client_deadline_adds_the_backup_slack`、`drain_envd_stream_maps_deadline_exceeded_to_an_exec_timeout`。反向验证：去掉 `Connect-Timeout-Ms` 注入 → 首条 FAILED；把超时错误改成 `Transport` → deadline 用例 FAILED。**2026-10-31 补齐 `aa66abfd0` 第二半（`readTimeout=0`）**：`E2bBackend` 新增**无 client 级 total timeout** 的 `exec_client`（仅 connect timeout），exec 全走它；`timeout_seconds > 0` 时 `RequestBuilder::timeout(T+5s)` 是唯一整体时限，`= 0` 时真无界（对齐 Java `callTimeout(0)`）。用例 `exec_is_not_bounded_by_the_base_read_timeout`（`read_timeout_seconds=1`、mock 延迟 1.5s、无 deadline → 必成功） |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`SANDBOX-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `SANDBOX-D<NN>` / `SANDBOX-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **平台覆盖首要核实（`rg` 三同义词交叉验证）**：① `agentrun|AgentRun|agent-run` 在 `src/` 命中 **100 行** —— `src/agentrun.rs` 780 行、`impl RemoteBackend for AgentRunBackend` @`agentrun.rs:539`、工厂 `agentrun_client` @`agentrun.rs:654`、`src/lib.rs` 声明 `pub mod agentrun;`，**结论：已迁移**（非占位：18 处 reqwest 调用）；② `daytona|Daytona` 命中 **65 行** —— `src/daytona.rs` 489 行、`impl RemoteBackend for DaytonaBackend` @`daytona.rs:324`、`daytona_client` @`daytona.rs:422`、`src/lib.rs` 声明 `pub mod daytona;`，**结论：已迁移**（13 处 reqwest 调用）；③ `e2b`（`e2b.rs:1644`）与 `kubernetes`（`kubernetes.rs:1545`）同样为真实实现。四个 Java 子模块在 Rust 侧**无一遗留**。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| SANDBOX-01 | P0 | AgentRun 命令执行补齐 MCP streamable-HTTP 握手：先 `initialize`（protocolVersions `2024-11-05`/`2025-03-26`），回传 `Mcp-Session-Id`，并带上 `X-Acs-Parent-Id` | `agentrun/AgentRunMcpChannel.java#connect`（`McpClientBuilder.streamableHttpTransport(url).header("X-API-Key",…).header("X-Acs-Parent-Id",…).protocolVersions(…).buildSync()` 后 `initialize()`；`exec` 用同一客户端 `callTool`） | `agentrun.rs:280` 只构造裸 JSON-RPC `tools/call`（无连接态）；`agentrun.rs:1-9` 自述「`initialize` 握手 / SSE session 管理未实现…需要 session 的服务器会拒绝」；`rg 'initialize|Mcp-Session-Id' src/agentrun.rs` 仅命中该注释 | 待办 | `AgentRunBackend` 增加 `Arc<Mutex<Option<McpSession>>>`：`POST initialize` → 解析/保存 session id → 后续 `tools/call` 带 `Mcp-Session-Id`；验收 = httpmock 断言「先 initialize 再 tools/call」两次请求的路径/头/体，及无 session 时的明确报错 |
| SANDBOX-02 | P0 | AgentRun `sandboxId` 改为 Java 的 26 位 Crockford Base32（`base32(SHA-256(sessionId))` 前 26 位），使「同 session → 同 sandboxId」可幂等重建 | `agentrun/AgentRunSandboxClient.java#deriveSandboxId`（:243，`SANDBOX_ID_LENGTH = 26`，字母表 `0123456789ABCDEFGHJKMNPQRSTVWXYZ`，注释「AgentRun-acceptable shape」） | `agentrun.rs:552-558` 直接把 session UUID（36 位、含 `-`）当 `sandboxId` 传给 `build_create_sandbox_body`；`rg 'derive_sandbox_id|Crockford|ulid' src/` **零命中** | 待办 | 新增 `derive_sandbox_id(session_id) -> String` + 固定向量单测；`create_sandbox` 用它替换 `session_id`；验收 = `POST /sandboxes` body 的 `sandboxId` 长 26 且字符集合法（若端点校验形状，当前实现会 create 直接失败） |
| SANDBOX-03 | P0 | 远程 sandboxId 写入 `SandboxState` 并在 `resume` 还原，避免「resume 必然重建」 | `e2b/E2bSandboxState.java:27`（`sandboxId` 参与序列化）；`kubernetes/KubernetesSandboxClient.java#resume`（`getSandbox(k8s.getClaimName(), k8s.getNamespace())` 复用既有 claim） | `to_harness_state`（`remote.rs:138-181`）未写 id；`from_harness_state` 硬编码 `sandbox_id: None`（`remote.rs:219`）；`resume`（`remote.rs:768-775`）因此让 `ensure_handle` 走 `create_sandbox`（`remote.rs:503-512`，handle 为空即建新沙箱）→ K8s `POST sandboxclaims` 同名冲突 / AgentRun 409 / Daytona-E2B 新建重复沙箱 | 待办 | 新增 metadata key（如 `agentscope.sandbox.sandbox_id`）双向读写；缺 key 时从 `extra["claim_name"]`（K8s）/`extra["sandbox_id"]`（E2B）兜底；验收 = 扩 `remote::tests::serialize_state_round_trips`：序列化 → resume → 断言不调用 `create_sandbox` 且 `state().metadata` 带 id |
| SANDBOX-04 | P1 | `resume` 重建 snapshot 客户端/身份（否则快照恢复在 resume 路径失效） | `kubernetes/KubernetesSandboxClient.java#deserializeState(String, SandboxSnapshotSpec)`（用 `RemoteSnapshotSpec` 重绑 `RemoteSandboxSnapshot`，并保留已持久化的 archive id） | `remote.rs:768-775` 以 `None` 作 snapshot 传给 `RemoteSandbox::new`；`deserialize_state_with_spec` 未覆写（`remote.rs:788+` 只实现 `deserialize_state`） | 待办 | `RemoteSandboxClient` 覆写 `deserialize_state_with_spec` 并对 `resume` 传入已绑定的 spec；验收 = E2B 原生快照 state → resume → `start()` 命中 `hydrate_workspace_bytes` 的快照分支（用例需覆盖，不允许只构造 happy path） |
| SANDBOX-05 | P1 | `Sandbox::exec` 默认 cwd 设为 `workspace_root` | `agentrun/AgentRunSandbox.java#relativeOrAbsoluteCwd`（始终传 workspaceRoot）；`e2b/E2bSandbox.java#doExec`（`runShell(state, getWorkspaceRoot(), …)`）；`kubernetes/KubernetesSandbox.java#doExec`（`cd <root> && (…command…)`，且空命令直接 `ExecException(2,…)`） | `remote.rs:661-675` 固定 `exec_in(handle, command, None, …)`；AgentRun 仅 `cwd.is_some()` 才写 `arguments.cwd`（`agentrun.rs:280-296`）；E2B 仅 `Some(cwd)` 才写 `process.cwd`（`e2b.rs:712-719`）；K8s 完全不包 `cd`（`kubernetes.rs:1602-1645`）。仅 Daytona 内部兜底为 `relative_cwd(workspace_root)`（`daytona.rs:396-414`） | 待办 | `RemoteSandbox::exec` 传 `Some(workspace_root)`；K8s 在 `exec_in` 内包 `cd <quoted root> && ( … )`；验收 = httpmock 断言出站 body 含 `cwd`（AgentRun/E2B）与含 `cd`（K8s），并覆盖相对路径命令可用的集成式用例 |
| SANDBOX-06 | P1 | AgentRun / Daytona exec 输出按 512 KiB 截断并置 `truncated` | `agentrun/AgentRunSandbox.java:43,123-125`；`daytona/DaytonaSandbox.java:37,83-85` | `agentrun.rs:645`、`daytona.rs:413` 恒 `truncated: false` 且不截断；`rg '512|OUTPUT_TRUNCATE' src/` **零命中**（K8s Java 本身不截断，故 K8s 不在本项） | 待办 | 在 `RemoteExecOutput::to_exec_result` 前统一截断（按字节、保留 UTF-8 边界，上限 512*1024）并置标记；验收 = 600 KiB 输出的 httpmock 用例，断言 `truncated == true` 且长度受限 |
| SANDBOX-07 | P1 | E2B 流式输出累计上限 512 KiB（内存保护） | `e2b/E2bEnvdProcessClient.java:63,106-109,794`（`OUTPUT_TRUNCATE_BYTES` + `truncateUtf8`，边收边截） | `e2b.rs:907-908` 对每帧无条件 `extend_from_slice`，无任何上限 → 大输出可 OOM | 待办 | `drain_envd_stream` 内对 `stdout/stderr` 施加 cap（超限后继续消费但丢弃并置 `truncated`）；验收 = 多帧累计 600 KiB 用例 + `truncated` 断言（反向验证：去掉 cap 必 FAILED） |
| SANDBOX-08 | P1 | AgentRun NAS/OSS 挂载工作区语义：persist 返回空标记、hydrate 空归档 no-op、destroy 跳过 | `agentrun/AgentRunSandbox.java#doPersistWorkspace`（`isWorkspaceOnNas()` → `InputStream.nullInputStream()`）、`#doDestroyWorkspace`、`#stop`（NAS 时先 `sync`） | `rg 'workspace_on_nas|workspaceOnNas|is_workspace_under' src/` **零命中**；`AgentRunBackend` 未覆写 `persist_workspace_bytes` → 走默认 `persist_tar`（`remote.rs:389-419`），会把持久挂载上的整个工作区打成 tar 上传 | 待办 | 新增 `workspace_on_nas(root, nas, oss)`（对齐 `AgentRunSandboxClient#isWorkspaceUnderMounts`：root 前缀匹配 `mountDir`，OSS 只读不计），在 `AgentRunBackend::persist_workspace_bytes` 返回 `Vec::new()`、`hydrate_workspace_bytes` 对空归档 no-op；验收 = 4 条纯函数用例 + 1 条「NAS 工作区不产生 tar 命令」后端用例 |
| SANDBOX-09 | P1 | **待裁定（harness 侧）** 沙箱生命周期收尾未接线：self-managed 沙箱 release 时不 stop/shutdown，远程 sandbox / K8s claim 永不销毁 | `agentscope-harness/.../sandbox/SandboxManager.java#release`（`sandbox.stop()` + `sandbox.shutdown()`，仅跳过 user-managed）；`KubernetesSandbox#shutdown` → `terminate()` | `crates/agentscope-harness/src/agent/sandbox/manager.rs:278-296`：`release` 只 `debug!` 记录，注释「For now, we'll just log. The actual stop/shutdown would need to be handled…」；`remote.rs:777-780` `SandboxClient::delete(&dyn Sandbox)` 为 no-op（`dyn` 不可变，无法调 `close()`） | 待裁定 | 需 harness 决策：①`Sandbox::shutdown(&self)`（内部可变）或 `SandboxManager` 持有所有权 / ②`SandboxClient::delete` 收 `&mut dyn Sandbox`。本 crate 不改 harness；裁决后本 crate 无需改（已实现 `shutdown`）。验收 = `release` 触发 `kill_sandbox` 的 manager 用例 |
| SANDBOX-10 | P2 | K8s claim 命名对齐 Java | `kubernetes/KubernetesSandboxClient.java:106-109`（`"as-sbx-" + sessionId.replace("-","").substring(0,20)`） | `kubernetes.rs:1570`：`format!("agentscope-{session_id}")`（36 位带连字符 UUID）。DSN 合法但跨语言命名不一致，运维按名清理/对账会漏 | 待办 | 抽出 `claim_name_for(session_id)` 对齐 Java 公式 + 单测；`CREATED_BY_VALUE` 的跨语言差异仍按 §2.4 保留（有意） |
| SANDBOX-11 | P2 | E2B 快照 id 持久化 + shutdown 时收敛 retention | `e2b/E2bSandboxState.java:42`（`snapshotIds` 随 state 序列化）、`E2bSandbox#shutdown`（`killSandbox` 之后 `cleanupSnapshots()`，因为运行中的沙箱会锁住模板） | `e2b.rs:998` `known_snapshots: Arc<Mutex<Vec<String>>>` 仅进程内；`prune_known_snapshots` 只在 persist 调用（`e2b.rs:1889`），`shutdown` 无 prune；快照 id 列表未进 `extra` | 待办 | 把 ids 写入 `RemoteSandboxStateData.extra`（JSON）并在 adopt/from_state 恢复；`RemoteSandbox::shutdown` 调后端 prune（可加 `RemoteBackend::on_shutdown` 默认空实现）；验收 = 「序列化 → 新进程 resume → 仍能 prune 旧 id」用例 |
| SANDBOX-12 | P2 | K8s 归档排除 bind-mount 路径 | `kubernetes/KubernetesSandbox.java#doPersistWorkspace`（`WorkspaceMountSupport.tarExcludeArgsForBindMounts(state.getWorkspaceSpec())`，与 `--exclude=./.agentscope-tmp` 一起拼进 tar） | `kubernetes.rs:1343` 只有 `--exclude=./{FILE_API_TMP_DIR_REL}`；`rg 'bind_mount|tar_exclude' src/kubernetes.rs` **零命中** | 待办 | 复用 `workspace_spec` 的 bind mounts 生成 `--exclude=./<rel>`（与 `e2b::volume_tar_exclude_args` 同形，逐项 `shell_quote`）；验收 = 纯函数用例 + 单条 tar 命令断言 |
| SANDBOX-13 | P2 | 错误分类保留（避免全部塌成 `Error::Other`） | Java `SandboxException.*` 携带 `SandboxErrorCode`，调用方按类型/码分支 | `error.rs:101-105`：`From<SandboxExtensionError> for Error` → `Error::Other(err.to_string())`，`SandboxErrorCode` 只作为文本前缀存在 | 待办 | 至少把 `Http{status,…}` 映射为 `Error::HttpStatus{status,message}`（保留可重试语义），`Configuration` 用显式前缀；验收 = 单测断言 `Error` 变体与 status |
| SANDBOX-14 | P2 | K8s 空/空白命令快速拒绝且不出站 | `kubernetes/KubernetesSandbox.java#doExec`（空白 → `SandboxException.ExecException(2, "", "empty command rejected by KubernetesSandbox")`） | `kubernetes.rs:1602-1645` 直接 `build_execute_body(command)` 发请求，无空白校验 | 待办 | 入口 `command.trim().is_empty()` → `SandboxExtensionError::Runtime(CommandExecutionError, "empty command rejected by KubernetesSandbox")`；验收 = 单测（httpmock 无任何请求） |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| SANDBOX-15 | 四后端统一接入 harness `Sandbox` / `SandboxClient`（真实 HTTP，非占位） | `AbstractBaseSandbox` + `*SandboxClient`（4 子模块） | `remote.rs:578`（`impl Sandbox for RemoteSandbox`）、`remote.rs:730`（`impl SandboxClient for RemoteSandboxClient`）、四后端 `impl RemoteBackend`（`agentrun.rs:539` / `daytona.rs:324` / `e2b.rs:1644` / `kubernetes.rs:1545`）；测试 `remote::tests::state_projects_into_remote_variant` | 已完成 |
| SANDBOX-16 | AgentRun options 校验（apiKey/template/mcpUrl、mountDir 前缀白名单、OSS ≤5）与 NAS/OSS 请求体 | `AgentRunSandboxClientOptions#validate`、`AgentRunDataPlaneHttp#createSandbox` | `agentrun.rs:139-215`（`validate_options`）、`agentrun.rs:216-259`（`build_create_sandbox_body`）；测试 `validate_rejects_missing_fields_and_bad_mounts`、`create_body_includes_mounts`、`create_body_omits_empty_mounts` | 已完成 |
| SANDBOX-17 | Daytona 创建体（snapshot 优先于 image、cpu/memory/disk）与 exec 结果解析 | `DaytonaHttp#createSandbox`/`#execute`、`DaytonaSandbox#doExec` | `daytona.rs:80-114`、`daytona.rs:396-420`；测试 `create_body_prefers_snapshot_over_image`、`parse_execute_result_reads_exit_code_and_result` | 已完成 |
| SANDBOX-18 | E2B 卷挂载 + `envVars` + 空值/空列表省略 | `E2bPlatformHttp#createSandbox`、`E2bSandbox#resolveEnvVars` | `e2b.rs`（`build_create_sandbox_body_with_envs`/`_with_mounts`）；测试 `create_body_with_env_vars_matches_java`、`create_body_omits_env_vars_when_empty`、`volume_mount_validates_name_and_normalizes_path`、`platform_create_sends_volume_mounts` | 已完成 |
| SANDBOX-19 | E2B 快照删除 + retention 窗口（历史缺口已不成立） | `E2bPlatformHttp#deleteSnapshot`、`E2bSandboxClientOptions#snapshotRetention` | `e2b.rs:1106`（`delete_snapshot`，单段 percent-encode）、`e2b.rs:1181`（`prune_known_snapshots`）、`e2b.rs:1889`（persist 后接线）；测试 `delete_snapshot_sends_delete_to_templates_endpoint`、`snapshot_retention_keeps_last_n_and_prunes_only_the_oldest`、`snapshot_retention_disabled_makes_no_delete_request`、`snapshot_retention_404_counts_as_already_pruned` | 已完成 |
| SANDBOX-20 | E2B exec 服务端 kill + 可靠超时 + 不受 base read timeout 约束 | `E2bEnvdProcessClient#runShell`（`Connect-Timeout-Ms`、`callTimeout`）、commit `aa66abfd0` | `e2b.rs:1727-1810`（deadline/`Connect-Timeout-Ms`/`exec_client`）、`e2b.rs:1830-1860`；测试 `exec_timeout_arms_the_server_side_kill_and_reports_an_error`、`connect_timeout_ms_header_arms_the_server_side_kill`、`exec_is_not_bounded_by_the_base_read_timeout` | 已完成 |
| SANDBOX-21 | E2B 原生文件传输（envd `/files`）+ `workspaceTransferPath` 校验 + tar 经文件 API | `E2bSandbox implements SandboxFileTransfer`（`supportsFileTransfer`/`uploadFile`/`downloadFile`）、`#doPersistWorkspace` | `e2b.rs:1564`（`workspace_transfer_path`）、`e2b.rs:1611-1629`（`E2bFileTransfer`）；测试 `file_transfer_uploads_and_downloads_through_envd_files`、`workspace_transfer_path_stays_inside_the_workspace`、`persist_tar_uses_envd_files_instead_of_base64_over_exec` | 已完成 |
| SANDBOX-22 | K8s 四种连接策略（Direct/InCluster/Gateway/PortForward）+ 路由头注入规则 | `KubernetesSandboxClient#toConnectionConfig`、`client/internal/strategy/*` | `kubernetes.rs:84-131`（枚举 + `should_inject_router_headers`）、`kubernetes.rs:716-793`（`resolve_connector_base_url`）、`kubernetes.rs:794-827`（port-forward）、`kubernetes.rs:881-902`（`connector_router_headers`）；测试 `connection_strategy_derivation_matches_java_priority`、`in_cluster_strategy_uses_pod_ip_then_service_dns`、`gateway_strategy_reads_the_address_from_the_crd`、`router_headers_follow_the_strategy` | 已完成 |
| SANDBOX-23 | K8s claim spec 扩展（lifecycle / additionalPodMetadata / labels）与条件快速失败 | `CreateSandboxOptions`、`SandboxClaimSpec`、`K8sHelper#checkClaimFailureConditions` | `kubernetes.rs:362-429`（`build_claim_body_with_options`）、`kubernetes.rs:430-460`（`claim_lifecycle`）、`kubernetes.rs:461-526`（`claim_failure`）；测试 `claim_body_carries_lifecycle_and_pod_metadata`、`claim_body_keeps_the_legacy_shape_without_extras`、`claim_failure_maps_template_and_warm_pool_conditions`、`create_sandbox_sends_the_extended_claim_spec` | 已完成 |
| SANDBOX-24 | K8s 文件 API（upload/download/list/exists/move）并接线为原生传输 + 归档优先走文件 API | `client/Filesystem`、`client/model/FileEntry`、`KubernetesSandbox#persistViaFileApi` | `kubernetes_files.rs`（`KubernetesFileApi`、`FileEntry`、`KubernetesFileTransfer`）、`kubernetes.rs:1300-1418`（`native_file_transfer`/`persist_via_file_api`/`hydrate_via_file_api`）；测试 `file_transfer_uploads_and_downloads_under_the_base_dir`、`persist_workspace_uses_the_file_api_when_enabled`、`file_api_path_and_temp_archive_names_are_deterministic` | 已完成 |
| SANDBOX-25 | K8s claim 列表/查询/删除/warmPool 名/deleteAll | `client/SandboxClient#listSandboxClaims`/`#listActiveSandboxes`/`#deleteAll`/`#getSandboxClaimWarmPoolName` | `kubernetes.rs:1014-1115`；测试 `list_sandbox_claims_parses_names`、`delete_all_removes_tracked_claims`、`claim_warm_pool_name_reads_the_spec_ref` | 已完成 |
| SANDBOX-26 | 本 crate 的 `SandboxFilesystemSpec` 等价层 + `SandboxFilesystemAdapter`（ls/read/write/edit/grep/glob/delete/move/exists，原生传输优先） | `*FilesystemSpec`（4 类）+ `SandboxBackedFilesystem` | `fs_spec.rs:124-334`（trait/spec）、`fs_spec.rs:639-853`（`Filesystem` 全量实现）、`fs_spec.rs:441-456`（`from_remote_sandbox`）；测试 `adapter_write_prefers_native_transfer`、`adapter_read_prefers_native_transfer_and_windows_lines`、`parse_ls_output_*`/`parse_grep_output_*` | 已完成 |
| SANDBOX-27 | 重试策略（按后端区分）与死信分类 | `AgentRunRetry`/`DaytonaRetry`/`E2bRetry`（E2B 含 408） | `retry.rs`（`RetryPolicy::{agentrun,daytona,e2b,kubernetes}` + `with_retries`，7 条单测）；`rg 'RetryPolicy::' src/` 四后端各 1 处 | 已完成 |



## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：四个远端沙箱后端（agentrun / daytona / e2b / kubernetes）全部适配到 harness `Sandbox`/`SandboxClient` trait；共享 `RemoteBackend` glue、HTTP、重试、工作区投影、状态投影、快照接入均已落地。**四后端均为真实 reqwest HTTP 实现**（非 trait 占位）：agentrun 18 处 / daytona 13 处 / e2b 34 处 / kubernetes 14 处 HTTP 调用，各含 1 个 `impl RemoteBackend`。
- **Rust 产物**：`src/` 12 个源文件（`lib/error/http/retry/workspace/remote/agentrun/daytona/e2b/kubernetes/fs_spec/kubernetes_files.rs`），**10201 行**；**176 个单测**（`#[test]` + `#[tokio::test]`，含 `#[tokio::test(start_paused = true)]`）。**已完成（2026-10-29 勘误）**：原记「6721 行 / 137 个单测」，实际以 §1.1（2026-10-28 实测）与 见 history/SANDBOX__MIGRATION_HISTORY.md（176 passed）为准。
- **验证（实测，汇总方基线）**：
  ```
  cargo build -p agentscope-extensions-sandbox   -> 通过（0 error）
  cargo test  -p agentscope-extensions-sandbox   -> 108 passed / 0 failed / 0 ignored
  cargo test  --workspace                        -> 1866 passed / 0 failed（86 test target）
  ```
  > ⚠️ 上面的 `108 passed` 是**汇总方旧基线**（对应 §1.1 旧计数）。**2026-10-22 静态复核实测 `#[test]`/`#[tokio::test]` 标注数为 137**（`rg -o '#\[(tokio::)?test' src/`），逐文件：`agentrun` 11 / `daytona` 7 / `e2b` 43 / `error` 3 / `fs_spec` 18 / `http` 3 / `kubernetes` 9 / `kubernetes_files` 20 / `remote` 11 / `retry` 7 / `workspace` 5 = 137。**下次 `cargo test` 应确认实际通过数**（本轮为静态复核，未编译、未运行测试）。
- **已知问题**：无阻塞（2026-10-31 三轮收口，见 history/SANDBOX__MIGRATION_HISTORY.md）。真实端点已有环境变量门控冒烟用例（默认 skip）；Java 侧能力差异均已裁定（§3.2/§3.3/见 history/SANDBOX__MIGRATION_HISTORY.md）。~~新增确认的接线缺口：K8s 文件 API 与 E2B 原生文件传输**均未接入沙箱生命周期**（§3.2）~~ → **已于 2026-10-28 接线并测试覆盖**（见 history/SANDBOX__MIGRATION_HISTORY.md）。
- **2026-10-27 实测**：`cargo test -p agentscope-extensions-sandbox` → **151 passed / 0 failed / 0 ignored**（含本轮新增的 `volumeMounts` 与 exec deadline/kill 用例；三处反向验证见 §3.3 两行）。
- **2026-10-28 实测**：`cargo build -p agentscope-extensions-sandbox` → 通过（0 error / 0 warning）；`cargo test -p agentscope-extensions-sandbox` → **176 passed / 0 failed / 0 ignored**（151 → +25，详见 history/SANDBOX__MIGRATION_HISTORY.md）。`cargo fmt -p agentscope-extensions-sandbox -- --check` → 通过。

### 4.2 下一步与阻塞
1. **harness 抽象缺口（不改 harness，仅记录）**：harness `agent::sandbox` 的 `state` 模块为私有（`mod state;`），`RemoteSandboxState` **未在 `sandbox/mod.rs` re-export**（`pub use state::{InMemorySandboxStateStore, LocalSandboxState, SandboxState, SandboxStateStore};` 不含 `RemoteSandboxState`）。本 crate 因此无法命名 `RemoteSandboxState`，只能经 `serde_json` 与 `SandboxState::Remote` 变体互转（见 `remote.rs` 内注释）。**已于 2026-10-29 解决**：harness `sandbox/mod.rs` 已 re-export `RemoteSandboxState` 与 `SandboxSnapshotRef`，`RemoteSandboxStateData::to_harness_state` 改为直接构造（不再靠 `serde_json` 往返），并因此能携带快照身份（见本文件末尾 2026-10-29 执行记录）。
2. **harness 缺少 `SandboxFilesystemSpec` / `SandboxFileTransfer`**：Java 扩展的 4 个 `*FilesystemSpec` 继承 harness Java `SandboxFilesystemSpec`（`io.agentscope.harness.agent.filesystem.spec`），`SandboxFileTransfer` 亦为 harness Java 类型（`io.agentscope.harness.agent.sandbox`）。**Rust harness 复核确认两者皆无**（`rg 'SandboxFilesystemSpec|SandboxFileTransfer' crates/agentscope-harness/src/` → 0 命中；harness 只有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`）。**已按裁定在本 crate 内建适配层**（`fs_spec.rs` 的 `SandboxFilesystemSpec` + `kubernetes_files.rs` 的 `SandboxFileTransfer`），**未改 harness/core**。若 harness 后续补齐同名类型，本 crate 的实现可直接切换继承/实现面，无需改调用点。**遗留**：Java `SandboxBackedFilesystem` 会在沙箱实现 `SandboxFileTransfer` 时**优先走原生传输**；Rust harness 无此机制。**已完成（2026-10-29 勘误）**：原记「`fs_spec::SandboxFilesystemAdapter` 只能全量走 exec+base64」，实际适配器已优先原生传输（用例 `fs_spec::tests::adapter_write_prefers_native_transfer`、`adapter_read_prefers_native_transfer_and_windows_lines`），仅需调用方显式 `from_remote_sandbox(...)` / `filesystem_with_transfer(...)` 传入传输适配器（harness `Sandbox` 为 `dyn` 且无 `as_any`，无法自动下转，见 history/SANDBOX__MIGRATION_HISTORY.md）。
3. **P1 剩余**（2026-10-28 更新）：① `PortForwardStrategy`（fabric8 `LocalPortForward`）无对应，Java 在该场景（无 apiUrl/gateway）为默认 — Rust 回退 `InCluster`，已记差异；② E2B `workspaceOnVolume`（工作区在卷上时的 tar no-op / destroy 跳过 / 嵌套挂载 `--exclude`）未迁移；③ E2B `envs → envVars`、K8s `SandboxClaimSpec.lifecycle`/`additionalPodMetadata` 等 P2 字段。~~E2B Proto codec、K8s 策略语义、文件 API / 原生传输接线~~ → **已完成（见 history/SANDBOX__MIGRATION_HISTORY.md）**。**2026-10-29 复核补充的完整残留清单**：(1) `PortForwardStrategy`（`LocalPortForward` 无对应）；(2) `GatewayStrategy` 无 watch（一次性读取）；(3) `K8sHelper` 的 list/watch/模板/port-forward 未迁移；(4) K8s `listSandboxClaims`/`listActiveSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName` 未迁移；(5) `SandboxFilesystemSpec` 缺 `executionGuard`/`workspaceProjectionEnabled`/`workspaceProjectionRoots`，且 `__workspace_projection__` 未真写入（仅常量保留）；(6) 原生传输需显式 `from_remote_sandbox`（`dyn Sandbox` 无 `as_any`）；(7) E2B `workspaceOnVolume` 与 `envs → envVars`；(8) K8s CRD `lifecycle`/`additionalPodMetadata`/`shutdownAfterSeconds`；(9) E2B `E2bCloudBucketMountStrategy`（Java 为空占位接口，迁移价值 0）。
   > **2026-10-30**：上述 (1) `PortForwardStrategy` 与 (5) `SandboxFilesystemSpec` 的 guard/projection 已完成（见 history/SANDBOX__MIGRATION_HISTORY.md）；(2)(3)(4)(6)(7)(8)(9) 仍待办。
   > **2026-10-30 二轮**：(2) `Gateway` 地址等待、(4) `listSandboxClaims`/`listActiveSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName` 已完成（见 history/SANDBOX__MIGRATION_HISTORY.md / 见 history/SANDBOX__MIGRATION_HISTORY.md）；(3) 仅剩 `watch=true` 流式订阅、模板与 port-forward 其余能力；(6)(7)(8)(9) 已于 2026-10-31 收口（见 history/SANDBOX__MIGRATION_HISTORY.md）：(6) 定为本 crate 的显式接线契约 + 跨模块需求（harness `Sandbox` 无 `as_any`），(7)(8) 落代码（含 httpmock 断言真实请求体），(9) 维持「已完成（设计决定，不迁移）」。另：`--lib` 高频 flake 的根因（本机系统代理劫持 loopback mock 流量）已修复，见 history/SANDBOX__MIGRATION_HISTORY.md。
4. **真实端点联调**：**2026-10-31 已落环境变量门控冒烟用例**（E2B / Kubernetes；未设变量时 skip），存凭据的机器上手工跑即可；agentrun/daytona 无此类需求。全部请求体构造/响应解析/重试仍有离线单测覆盖。
5. **跨模块需求**：本 crate 依赖 `agentscope-core`（`Msg`/`Error`/`RuntimeContext`）与 `agentscope-harness`（sandbox trait/类型、`WorkspaceSpec`/`ExecResult`/`SandboxState`/快照 trait、`filesystem`）；无 workspace 级依赖变更需求。仍需汇总方代为评估的 harness 侧需求（**本 crate 未改 harness**）：
   a. `Sandbox` 缺 `as_any`/`file_transfer()` 钩子 → 本 crate 只能靠 `SandboxFilesystemAdapter::from_remote_sandbox(...)` 显式注入原生传输（Java `SandboxBackedFilesystem` 可 `instanceof` 自动优先），建议 harness 补 `fn as_any(&self) -> &dyn Any` 或 `fn file_transfer(&self)`。
   b. harness 无 `SandboxFilesystemSpec` / `SandboxFileTransfer`（已在 §4.2-2 记录，本 crate 自建适配层）。
   c. harness 无 `PinnedSandboxFilesystem` 等价物（Java commit `19fdc9e6d` 的 SessionTree mirror 修复所在地）→ `rg PinnedSandbox crates/` 0 命中、`SessionEntry`/`SessionTree` 已删除，当前无该缺陷；若将来引入跨调用缓存沙箱的 filesystem 代理，需在同处加「已释放即报 `SANDBOX_CONNECTION_CLOSED`」守卫。
   d. `WorkspaceMountSupport.tarExcludeArgsForAbsolutePaths` 的通用能力已在 `e2b::volume_tar_exclude_args` 落地（仅 E2B 需要）；若其它后端也要挂载排除，建议上提到 harness。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| SANDBOX-D01 | Jackson 子类型注册模块（4 个 `*HarnessSandboxJacksonModule`） | `agentrun/AgentRunHarnessSandboxJacksonModule.java`、`daytona/…`、`e2b/…`、`kubernetes/…` | Rust 无 Jackson 子类型注册概念：`SandboxState` 是带 `#[serde(tag = "type")]` 的封闭 enum，state 向后兼容靠 `#[serde(default)]`（`crates/agentscope-harness/src/agent/sandbox/state.rs:143-145`）；`rg 'HarnessSandboxJacksonModule' crates/` 零命中 | 无（序列化结果由 enum 变体决定） |
| SANDBOX-D02 | E2B envd PROTO codec（`DynamicMessage` + Connect protobuf framing） | `e2b/E2bEnvdProcessClient.java`（`buildStartRequest`/`parseStartResponseFrame`/`encodeUnaryEnvelope`） | 需引入 `prost`/`prost-types` 并手写/生成 envd `process.Process/Start` 描述符树，无真实端点可离线校验；envd 同时支持 JSON codec，语义等价。显式选择 `E2bCodec::Proto` 时 `exec_in` 返回 `SandboxExtensionError::Unsupported`（`e2b.rs:1732-1736`）而非静默错编码 | 编码层性能有差异；默认 `E2bCodec::Json`（见 SANDBOX-X08） |
| SANDBOX-D03 | K8s `watch=true` 流式订阅 | `kubernetes/client/internal/K8sHelper.java`（fabric8 `Watch`）、`GatewayStrategy.java`（watch 地址） | 本 crate 无宿主后台任务/取消句柄：`decode_json` 用 `resp.text()` 读完整 body，而 `?watch=true` 在 `timeoutSeconds` 前不结束，直接接会挂死。正确实现需 `bytes_stream()` 分帧 + `resourceVersion` 续订 + 取消语义。就绪/网关地址改用有界轮询（`kubernetes.rs:1490` `wait_until_ready`、`kubernetes.rs:1139` `wait_for_gateway_address`），语义等价且已覆盖 | 事件实时性降低；无资源泄漏（轮询有 deadline） |
| SANDBOX-D04 | K8s 8 个细分异常类 | `kubernetes/client/exceptions/*`（`SandboxNotFoundException`/`SandboxNotReadyException`/`SandboxPortForwardException`/`SandboxRequestException`/`SandboxTemplateNotFoundException`/`SandboxWarmPoolNotFoundException`/`SandboxMetadataException`/`SandboxException`） | 归并到 `SandboxErrorCode`（`error.rs:11-46`，9 个变体含 `SANDBOX_CONNECTION_CLOSED`）；调用方按 `as_str()` 分支，Java 的 `TemplateNotFound`/`WarmPoolNotFound` 消息文本逐字对齐（`kubernetes.rs:461-526` `ClaimFailure::message`） | 无信息丢失（码 + 文本都在）；不保留 Java 类层级 |
| SANDBOX-D05 | 读取 `Sandbox` CRD 资源 / `SandboxTemplate` CRD | `K8sHelper#getSandbox`、`SandboxResource`、`SandboxTemplateNotFoundException` | claim status 已含 `status.sandbox.name` 与 `podIPs`（`kubernetes.rs:584-635` `parse_claim_status`），`connect_sandbox` 用 claim 回填 handle 省一次 CRD 读；Java HEAD 中 `SandboxTemplate` 只作为异常名出现（`K8sHelper.java:350`），无任何模板 CRD 读写 | 少一次 API 调用；不支持模板 CRD（Java 亦无） |
| SANDBOX-D06 | `E2bCloudBucketMountStrategy`（云桶挂载策略） | `e2b/mounts/E2bCloudBucketMountStrategy.java` | Java 该类为 `intentionally empty` 空接口（28 行、无方法），迁移价值为 0；卷挂载已由 `E2bVolumeMount`（`e2b.rs` + `build_create_sandbox_body_with_mounts`）覆盖 | 无 |
| SANDBOX-D07 | fabric8 `KubernetesClient` / OkHttp 注入点（`getKubernetesClient()` / `getHttpClient()`） | `KubernetesSandboxClientOptions#getKubernetesClient`、`*SandboxClientOptions#getHttpClient` | Rust 用 `reqwest` 直连 K8s REST（`kubernetes.rs:1420-1480` `k8s_request`）与各后端 HTTP，不引入 kube-rs/fabric8 等价依赖；离线测试用 `httpmock 0.7` | 无法注入自定义 transport；测试方式不同 |
| SANDBOX-D08 | Java 测试基建（fabric8 `KubernetesServer`、OkHttp `MockWebServer`） | `src/test/java/**`（23 文件 / 4554 行） | 不为测试引入 JVM 专属基建；口径改为「每个 Java 行为分支在 Rust 有等价用例」（纯函数或 httpmock），见 §3.2「Java 测试逐条对照」 | 测试文件数不可比；分支覆盖为判据 |
| SANDBOX-D09 | AgentRun `read_file` / `write_file` MCP 工具 | `agentrun/AgentRunMcpChannel.java:43-46,115-150`（`readFile`/`writeFile`） | Java 侧这两个方法**定义后零调用**（`rg 'readFile|writeFile' src/main/java` 只命中定义体），属死代码；本 crate 只保留 `process_exec_cmd`（`agentrun.rs:31` `TOOL_EXEC`） | 无 |
| SANDBOX-D10 | K8s `SANDBOX_API_GROUP`/`SANDBOX_API_VERSION`/`SANDBOX_PLURAL_NAME` 常量 | `kubernetes/client/Constants.java` | Java main 中仅声明、零引用（只命中 `Constants.java` 自身）；迁过去即死常量。Rust 保留真正使用的 `CLAIM_API_*`/`GATEWAY_*`/`X-Sandbox-*`/`ROUTER_*`（`kubernetes.rs:40-80`） | 无 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| SANDBOX-X01 | 远程 state 投影 | `RemoteSandboxStateData` → `SandboxState::Remote{metadata}`（`remote.rs:138-181`） | Java 每后端一个 `*SandboxState extends SandboxState` 子类，字段天然序列化 | Rust harness `SandboxState` 是封闭 enum（`state.rs:110-146`），投影是唯一可行方式；代价见 SANDBOX-03/04（需显式补 metadata key） |
| SANDBOX-X02 | `SandboxFilesystemSpec` / `SandboxFileTransfer` | 本 crate 自建等价抽象：`fs_spec.rs:124`（trait）、`kubernetes_files.rs:120-150`（`SandboxFileTransfer`） | Java 二者均为 harness 类型，扩展模块只继承/实现 | Rust harness **无**这两个类型（`rg 'SandboxFilesystemSpec|SandboxFileTransfer' crates/agentscope-harness/src/` 零命中）；按裁定在本 crate 建适配层、不改 harness（§4.2-2） |
| SANDBOX-X03 | K8s PortForward | `KubernetesConnectionStrategy::PortForward` + `KubernetesPortForwarder` seam（`kubernetes.rs:161-184`、`794-827`） | Java `PortForwardStrategy` 直接用 fabric8 `LocalPortForward` 起隧道 | Rust 无 fabric8 等价物，隧道归宿主（`kubectl port-forward`/operator）；seam 未注入时**明确报配置错误、绝不降级**，比静默回落更安全 |
| SANDBOX-X04 | Gateway 地址解析 | 有界轮询 `wait_for_gateway_address`（`kubernetes.rs:676-714`、`1139-1214`），可注入 `gateway_address` 覆盖 | Java `GatewayStrategy` 用 fabric8 `Watch` 阻塞等 `status.addresses` 出现 | 无后台任务/取消句柄；轮询有 deadline、非阻塞，语义等价（见 SANDBOX-D03） |
| SANDBOX-X05 | 非零退出码 | `Sandbox::exec` 返回 `ExecResult{exit_code}`（`remote.rs:661-675`），不抛错 | Java `doExec` 在 `!result.ok()` 时抛 `SandboxException.ExecException(exit,…)` | 遵循 Rust harness `Sandbox::exec -> Result<ExecResult>` 契约（`crates/agentscope-harness/src/agent/sandbox/mod.rs:102-107`、`ExecResult::is_success`），由 `shell_execute_tool` 统一渲染 `Exit code:` 头；异常会丢失 stdout/stderr |
| SANDBOX-X06 | 错误上抛形态 | `Error::Other(format!("sandbox … [{CODE}]: …"))`（`error.rs:101-105`） | Java 抛强类型 `SandboxException`（可按类/码 catch） | Rust core `Error` 无 sandbox 变体；码保留在文本里、`SandboxErrorCode::as_str()` 稳定。已知代价见 SANDBOX-13 |
| SANDBOX-X07 | 投影哈希 | FNV-1a（`workspace.rs:short_hash`、`remote.rs:383-388`） | Java `MessageDigest`(SHA-256) | 仅用于本地变更检测（判断是否需要重投影），不跨语言互操作；实现零依赖且更快 |
| SANDBOX-X08 | E2B 默认 codec | `E2bCodec::Json`（`#[default]` + `E2bSandboxClientOptions::default()` + `parse` 回落三处一致） | Java 默认 `E2bCodec.PROTO` | 让默认配置无需 opt-in 即可 exec（用例 `default_codec_exec_works_without_opt_in`）；Proto 见 SANDBOX-D02 |
| SANDBOX-X09 | E2B `snapshot_retention` 类型 | `Option<usize>`（`None` 与 `Some(0)` 均关闭，`e2b.rs:343,1182-1185`） | Java `int snapshotRetention = 0`（`<=0` 关闭） | Rust 用 `Option` 表达「未配置」更精确；关闭语义与 Java 一致 |
| SANDBOX-X10 | E2B 快照清理时机 | 每次 persist 后立即 best-effort prune（`e2b.rs:1885-1892`） | Java 在 `shutdown()` 里 `killSandbox` 之后 `cleanupSnapshots()` | 尽早收敛、失败不阻塞持久化；但存在「prune 因模板被占用失败后无 shutdown 重试」的残留（见 SANDBOX-11） |
| SANDBOX-X11 | K8s claim 标签/值 | `CREATED_BY_VALUE = "agentscope-rust-client"`（`kubernetes.rs:49`） | Java `"agentscope-java-client"` | 便于运维区分调用方语言；跨语言按 label selector 仍可共存。命名差异另见 SANDBOX-10 |
| SANDBOX-X12 | per-call options | `SandboxClient::create` 忽略 `_options: Option<&SandboxClientOptions>`（`remote.rs:730-752`），配置由各后端 `*SandboxClientOptions` 承载 | Java `merge(defaultOptions, callOptions)` 逐字段覆盖 per-call 选项 | Rust harness 的 `SandboxClientOptions` 是通用 struct（docker_image/ports/…），不含任何后端字段，合并无意义；后端选项经客户端构造期一次性确定 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-sandbox` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-sandbox` / `cargo test -p agentscope-extensions-sandbox` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-sandbox/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/SANDBOX__MIGRATION_HISTORY.md`](./history/SANDBOX__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
