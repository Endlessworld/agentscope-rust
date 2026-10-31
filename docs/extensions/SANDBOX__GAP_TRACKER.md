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
| 当前状态 | **总判：P0/P1/P2 全部关闭（2026-10-31 三轮收口，见 history/SANDBOX__MIGRATION_HISTORY.md）**（历史：P1 清零，2026-10-30 二轮，按 §3.2 P1 行口径）——原 P1 项（PortForward / K8s 文件 API / fs_spec projection+guard）均已落地，剩余仅 P2 字段与真实端点联调—— **已完成**（build 通过；**188 passed / 0 failed**（2026-10-30 一轮；二轮 **205 passed**，`--lib` 连跑 8 次全绿，见 history/SANDBOX__MIGRATION_HISTORY.md）；`fs_spec`、`kubernetes_files`、K8s 文件 API 与 E2B 原生传输**均已接线**，见 §3.2/见 history/SANDBOX__MIGRATION_HISTORY.md）。⚠️ **保留差异（若汇总方视为 P1 则需上调，不得静默降级）**：① `Gateway` 连接策略为「一次性读 + 有界轮询等地址出现」，**无后台 watch 任务**（Java 用 fabric8 `Watch`；Rust 无宿主任务/取消句柄，长任务会泄漏；且 Rust 每次调用重解析地址，强于 Java 的 `baseUrl` 缓存，见 history/SANDBOX__MIGRATION_HISTORY.md）；② `K8sHelper` 的 **`watch=true` 流式订阅**、模板（`SandboxTemplate`）、port-forward 其余能力未迁移（list/get/delete/warmPool 已于二轮完成，见 history/SANDBOX__MIGRATION_HISTORY.md）；③ `listActiveSandboxes` 的「prune 非 active 条目」无等价存活信号（改为删除即摘除）。~~K8sHelper 的 list（`listSandboxClaims`/`listActiveSandboxes`/`deleteAll`）~~ → **已于 2026-10-30 二轮完成** |

| Java 子模块 | 主代码 文件/行 | 测试 文件/行 | 顶层类型数 |
|---|---|---|---|
| agentrun | 11 / 1626 | 4 / 375 | 11 |
| daytona | 8 / 891 | 0 / 0 | 8 |
| e2b | 13 / 2216 | 11 / 3295 | 13 |
| kubernetes | 39 / 4099 | 8 / 884 | 39 |
| **合计** | **71 / 8832** | **23 / 4554** | **71** |

> 顶层类型数 = 每个 `.java` 主源文件恰含 1 个顶层类型（另有 12 个嵌套类型：K8s 8 个异常类各 1 个 + `SandboxClaimSpec` 3 个静态内部类 + `CreateSandboxOptions.Builder`），故 71 文件 = 71 顶层类型。

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

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-sandbox` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-sandbox` / `cargo test -p agentscope-extensions-sandbox` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-sandbox/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/SANDBOX__MIGRATION_HISTORY.md`](./history/SANDBOX__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
