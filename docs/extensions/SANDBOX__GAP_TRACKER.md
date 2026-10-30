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
| 当前状态 | **总判：P0/P1/P2 全部关闭（2026-10-31 三轮收口，见 §10）**（历史：P1 清零，2026-10-30 二轮，按 §3.2 P1 行口径）——原 P1 项（PortForward / K8s 文件 API / fs_spec projection+guard）均已落地，剩余仅 P2 字段与真实端点联调—— **已完成**（build 通过；**188 passed / 0 failed**（2026-10-30 一轮；二轮 **205 passed**，`--lib` 连跑 8 次全绿，见 §9.5）；`fs_spec`、`kubernetes_files`、K8s 文件 API 与 E2B 原生传输**均已接线**，见 §3.2/§7）。⚠️ **保留差异（若汇总方视为 P1 则需上调，不得静默降级）**：① `Gateway` 连接策略为「一次性读 + 有界轮询等地址出现」，**无后台 watch 任务**（Java 用 fabric8 `Watch`；Rust 无宿主任务/取消句柄，长任务会泄漏；且 Rust 每次调用重解析地址，强于 Java 的 `baseUrl` 缓存，见 §9.4）；② `K8sHelper` 的 **`watch=true` 流式订阅**、模板（`SandboxTemplate`）、port-forward 其余能力未迁移（list/get/delete/warmPool 已于二轮完成，§9.2）；③ `listActiveSandboxes` 的「prune 非 active 条目」无等价存活信号（改为删除即摘除）。~~K8sHelper 的 list（`listSandboxClaims`/`listActiveSandboxes`/`deleteAll`）~~ → **已于 2026-10-30 二轮完成** |

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
| `E2bEnvdProcessClient` | envd Connect/进程启动与流式事件 + 文件传输 | `e2b::{build_envd_start_json_body, encode_connect_envelope, decode_connect_frames, parse_envd_event, drain_envd_stream, EnvdEvent, EnvdStreamResult, E2bFileTransfer, upload_file, download_file, files_url, workspace_transfer_path}` | **已完成（2026-10-29 勘误）**：原记「部分完成」；实际 JSON codec + envd `/files` 上传/下载 + `workspaceTransferPath` 路径校验均已落地，并由 `E2bFileTransfer` 实现本 crate 的 `SandboxFileTransfer`。Proto codec 为**设计决定（不迁移）**。依据 §3.2「E2B 原生文件传输接线」「E2B envd Proto codec」两行 / §7.2 ② / 用例 `workspace_transfer_path_stays_inside_the_workspace`、`file_transfer_uploads_and_downloads_through_envd_files`、`persist_tar_uses_envd_files_instead_of_base64_over_exec` |
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
| `KubernetesSandbox` | harness `Sandbox` 实现（`SandboxFileTransfer`） | `remote::RemoteSandbox` + `kubernetes::KubernetesBackend` | **已完成（2026-10-29 勘误）**：原记「部分完成（`SandboxFileTransfer` 仅在 `kubernetes_files.rs` 独立声明，未挂到 `RemoteSandbox`/`KubernetesBackend`）」；实际已接通——`KubernetesBackend::native_file_transfer` → `RemoteBackend::file_transfer` → `RemoteSandbox::file_transfer()`。依据 §3.2「Kubernetes 文件 API」行 / §7.2 ① / 用例 `remote::tests::native_file_transfer_is_published_after_the_handle_is_established`、`file_transfer_uploads_and_downloads_under_the_base_dir` |
| `KubernetesSandboxClient` | harness `SandboxClient` 工厂 | `kubernetes::KubernetesSandboxClient`（别名）、`kubernetes::kubernetes_client` | 已完成 |
| `KubernetesSandboxClientOptions` | 客户端配置 | `kubernetes::KubernetesSandboxClientOptions` | 已完成 |
| `KubernetesSandboxState` | 可序列化状态 | `remote::RemoteSandboxStateData` | 已完成（投影式） |
| `KubernetesFilesystemSpec` | harness `SandboxFilesystemSpec` 实现 | `fs_spec::KubernetesFilesystemSpec`（本 crate 自建等价抽象） | 已完成（适配层） |
| `KubernetesHarnessSandboxJacksonModule` | Jackson 子类型注册 | 不适用 | 设计决定（不迁移） |
| `client/CommandExecutor` | 在 sandbox 服务器上执行命令 | `kubernetes::{build_execute_body, parse_execute_result, KubernetesExecResult}` | 已完成 |
| `client/Constants` | CRD group/version/plural、标签、端口 | `kubernetes::{CLAIM_API_GROUP, CLAIM_API_VERSION, CLAIM_PLURAL_NAME, CREATED_BY_LABEL, CREATED_BY_VALUE, DEFAULT_SERVER_PORT, GATEWAY_API_GROUP, GATEWAY_API_VERSION, GATEWAY_PLURAL, HEADER_SANDBOX_ID, HEADER_SANDBOX_NAMESPACE, HEADER_SANDBOX_PORT, HEADER_SANDBOX_POD_IP, ROUTER_SERVICE_NAME, ROUTER_PORT, DEFAULT_ROUTER_NAMESPACE}` | 已完成（2026-10-31） | 2026-10-28 补 `GATEWAY_*`/`X-Sandbox-*`，2026-10-30 补 `ROUTER_*`/`DEFAULT_ROUTER_NAMESPACE`。**未迁移两项均为设计决定**：① `SANDBOX_API_GROUP/VERSION/PLURAL_NAME` 在 Java main 中**仅声明、零引用**（`rg SANDBOX_API_GROUP src/main` 只命中 `Constants.java` 自身）⇒ 迁过去就是死常量；② `POD_NAME_ANNOTATION`/`SANDBOX_NAME_HASH_LABEL` 属读 `Sandbox` CRD 的路径（`Sandbox#getPodName/getSandboxNameHash`，仅被 `KubernetesSandboxClient:130-135` 用来回填 state），Rust 改用 **claim status** 的 `status.sandbox.name` + `podIPs`（`select_pod_ip`）达到同一目的，不需额外读 Sandbox 资源。`CREATED_BY_VALUE` 由 `agentscope-java-client` 改为 `agentscope-rust-client`，属有意的跨语言标签差异 |
| `client/CreateSandboxOptions` | 创建参数 | `kubernetes::{build_claim_body, build_claim_body_with_options, claim_lifecycle, KubernetesSandboxClientOptions}` | 已完成（2026-10-31） | warm pool / claimName（`agentscope-<session>`）已覆盖；本轮补 `claim_labels`、`shutdown_after_seconds`（→ `lifecycle`）、`pod_labels`/`pod_annotations`（→ `additionalPodMetadata`），经 `create_sandbox` 发往 `POST .../sandboxclaims`（用例 `create_sandbox_sends_the_extended_claim_spec`） |
| `client/Sandbox` | sandbox 服务器句柄（exec/files/状态） | `kubernetes::KubernetesBackend` + `remote::RemoteSandbox` | 已完成（exec/状态/`files()` 均已接入，2026-10-28） |
| `client/SandboxClient` | K8s 侧客户端（`AutoCloseable`） | `kubernetes::KubernetesBackend` + `remote::RemoteSandboxClient` | 已完成（2026-10-30/31） | `create_sandbox`/`connect_sandbox`/`kill_sandbox` + `list_sandbox_claims`/`get_sandbox_claim`/`get_sandbox_claim_warm_pool_name`/`delete_sandbox_claim`/`list_active_sandboxes`/`delete_all` 均已落地（§9.2）；`close()` 语义由 `RemoteSandbox::shutdown` 承担（关闭原生传输 + 释放句柄） |
| `client/Filesystem` | 文件 API（读写/列目录/移动） | `kubernetes_files::KubernetesFileApi`（`write`/`read`/`list`/`exists`/`move_path`） | 已完成（2026-10-28 已接线到 `KubernetesBackend`/`RemoteSandbox`，见 §3.2/§7） |
| `client/config/DirectConnectionConfig` | 直连（**预置 router API URL**） | `kubernetes::KubernetesSandboxClientOptions::sandbox_api_url`（+ `connection_strategy = Direct`） | 已完成（2026-10-28；`api_url` 仍专指 K8s API 地址，二者分离） |
| `client/config/GatewayConnectionConfig` | 网关连接（**Gateway CRD 外部地址 + watch**） | `KubernetesConnectionStrategy::Gateway` + `resolve_gateway_address`（CRD `status.addresses[0].value` 或注入的 `gateway_address`） | 已完成（无 watch：一次性读取，已记差异） |
| `client/config/InClusterConnectionConfig` | 集群内连接（`sandboxId.namespace.svc.cluster.local`） | `KubernetesConnectionStrategy::InCluster` + `resolve_connector_base_url` | 已完成（pod IP → sandbox 服务 DNS） |
| `client/config/LocalTunnelConnectionConfig` | 本地端口转发 | `KubernetesConnectionStrategy::PortForward` + `KubernetesPortForwarder` seam + `resolve_port_forward_base_url` | 已完成（2026-10-30，见 §8；未注入 forwarder 时明确报配置错误、不降级；默认策略仍为 InCluster，差异已在 §3.2 注明） |
| `client/config/SandboxConnectionConfig` | 连接配置接口 | 无 1:1 trait；由 `KubernetesConnectionStrategy` 枚举 + `resolve_connector_base_url` 函数式覆盖 | 已完成（枚举代替 trait 分派） |
| `client/crd/SandboxClaim` | CRD 资源对象 | `kubernetes::build_claim_body` | 已完成 |
| `client/crd/SandboxClaimSpec` | CRD spec | `kubernetes::build_claim_body_with_options`（`spec.warmPoolRef` + 可选 `lifecycle`/`additionalPodMetadata`） | 已完成（2026-10-31；空则不写，旧字节形状不变） |
| `client/crd/SandboxClaimStatus` | CRD status | `kubernetes::KubernetesClaimStatus` + `parse_claim_status` | 已完成 |
| `client/crd/SandboxResource` | sandbox 资源状态 | `kubernetes::parse_claim_status`（claim status） | 已完成（等价，设计决定：不读 `Sandbox` CRD） | claim status 里的 `status.sandbox.name`/`podIPs` 已含所需字段，`connect_sandbox` 用 claim 而非 Sandbox 资源回填 handle，省一次 CRD 读取；Java 的 `Sandbox#status()` 三态见下一行 |
| `client/exceptions/*`（8 个异常类） | 细分异常 | `error::SandboxExtensionError` / `SandboxErrorCode`（按语义归并，未 1:1） | 已完成（设计决定，不迁移：归并到错误码；Java 8 类只用于 `catch` 分支区分，Rust 用 `SandboxErrorCode` 枚举区分，`as_str()` 已含 `SANDBOX_CONNECTION_CLOSED`） |
| `client/internal/K8sHelper` | K8s REST 助手 | `kubernetes::KubernetesBackend`（reqwest 直连 K8s API） | 已完成（2026-10-30/31） | claim create/get/list/delete/status/exec + `select_pod_ip` + `claim_failure` 已覆盖；`watch=true` 与「模板」为**已完成（设计决定，不迁移）**（依据见 §3.3 同名行 / §10.1），port-forward 由 `KubernetesPortForwarder` seam 覆盖（§8） |
| `client/internal/SandboxConnector` | 连接编排 | `kubernetes::KubernetesBackend::{connector_base_url, router_headers}`（`X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP` 已注入，`InCluster` 除外） | 已完成（无 watch/重试退避与 Java 一致，但无显式 `connect()` 生命周期） |
| `client/internal/strategy/ConnectionStrategy` | 策略接口 | `kubernetes::KubernetesConnectionStrategy`（枚举，含 `should_inject_router_headers`） | 已完成 |
| `client/internal/strategy/DirectStrategy` | 直连策略（预置 API URL） | `KubernetesConnectionStrategy::Direct` | 已完成 |
| `client/internal/strategy/GatewayStrategy` | 网关策略（Gateway CRD + watch） | `KubernetesConnectionStrategy::Gateway` + `resolve_gateway_address`/`parse_gateway_address` | 已完成（无 watch） |
| `client/internal/strategy/InClusterStrategy` | 集群内策略（DNS） | `KubernetesConnectionStrategy::InCluster` | 已完成 |
| `client/internal/strategy/PortForwardStrategy` | 端口转发策略 | `KubernetesConnectionStrategy::PortForward` + `parse_endpoint_target_pod`/`resolve_router_pod` + `KubernetesPortForwarder` seam | 已完成（2026-10-30，见 §8） |
| `client/model/ExecutionResult` | 执行结果 | `kubernetes::KubernetesExecResult` | 已完成 |
| `client/model/FileEntry` | 文件条目 | `kubernetes_files::FileEntry` | 已完成 |
| `client/model/SandboxStatus` | sandbox 状态 | `kubernetes::KubernetesClaimStatus` + `get_sandbox_claim` 的 `Ok(None)`（404） | 已完成（等价结构，设计决定：不 1:1） | Java `SandboxReady`/`SandboxNotReady`/`SandboxNotFound` 三态 record ↔ Rust `KubernetesClaimStatus{ready, reason, sandbox_name, pod_ips}` + `None`；再造一层三态枚举属投机性代码，调用方（`RemoteBackend`）只需要 ready/reason 与句柄字段 |

### 2.5 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / Java `HttpClient` | HTTP 传输 | `reqwest`（workspace） | 超时/头部/重试见 `http.rs`、`retry.rs` |
| Jackson (`ObjectMapper`/`SimpleModule`) | JSON (反)序列化 + 子类型注册 | `serde` + `serde_json`（workspace） | state 经 harness 封闭 enum 序列化；无 Jackson 模块概念 |
| `com.google.protobuf`（DynamicMessage，E2B envd） | envd Connect protobuf 帧 | **无** | 仅实现 JSON codec。**已完成（2026-10-29 勘误）**：原记「Proto 待办；`E2bCodec::Proto` 是默认值」，实际默认已改为 `E2bCodec::Json`（`#[default]` + `E2bSandboxClientOptions::default().codec` + `E2bCodec::parse` 回落三处一致），Proto 明确为**设计决定（不迁移）**，显式选择时 `exec_in` 返回 `SandboxExtensionError::Unsupported`。依据 §3.2「E2B envd Proto codec」行 / §7.2 ④ / 用例 `default_codec_exec_works_without_opt_in`、`proto_codec_exec_is_unsupported` |
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
| 离线单测（**176** 个；2026-10-29 勘误，原记 137） | Java 测试 4554 行 | 已完成 | 2026-10-28 实测 **176 passed / 0 failed**（§4.1 / §7.3），分布见 §1.1 |

### 3.2 P1（重要）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `*FilesystemSpec` 四类 | `AgentRun/Daytona/E2b/KubernetesFilesystemSpec` | **已完成（适配层）** | Java 侧均 `extends harness SandboxFilesystemSpec`；**Rust harness 无对应 Spec 类型**（仅有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`），故在 `src/fs_spec.rs` 自建等价的 `SandboxFilesystemSpec`（client + workspace 投影 + snapshot 覆盖 + isolation scope）与四类 spec（`AgentRunFilesystemSpec`/`DaytonaFilesystemSpec`/`E2bFilesystemSpec`/`KubernetesFilesystemSpec`，经 `platform_spec!` 宏生成），并以 `SandboxFilesystemAdapter` 实现 harness `SandboxFilesystem`。非 1:1 继承，语义等价。**能力差异**：Java spec 还提供 `executionGuard(...)`、`workspaceProjectionEnabled(...)`、`workspaceProjectionRoots(...)` 与 `toSandboxContext(hostWorkspaceRoot)`（带 `__workspace_projection__` 条目）；Rust 侧 2026-10-30 前无 guard 钩子、`__workspace_projection__` 仅留常量未真写入——**已补齐（见 §8）**。见 §4.2（不改 harness）。**2026-10-30 已补齐** `execution_guard` / `workspace_projection_enabled` / `workspace_projection_roots`，`__workspace_projection__` 真正写入 harness `LayoutSpec` 条目并可生成投影 tar，见 §8 |
| Kubernetes 文件 API | `client/Filesystem`、`client/model/FileEntry`、`SandboxFileTransfer` | **已完成（2026-10-28 接线）** | `src/kubernetes_files.rs`：`KubernetesFileApi`（`write`/`write_text`/`read`/`list`/`exists`/`move_path`，路径 percent-encoded）、`FileEntry` 模型、`KubernetesFileTransfer`（实现本 crate 声明的 `SandboxFileTransfer`）、`ReqwestFileApiTransport`（reqwest 实现）+ `FileApiTransport` seam；新增 `file_api_path` / `temp_archive_rel_path`（对齐 Java `fileApiPath`/`tempArchiveName`，UUID 去重避免并发互删）。**已接线**：`KubernetesBackend::native_file_transfer(handle)` 在 `file_api_base_dir` 非空时构造文件 API（base = connector URL，带 `X-Sandbox-*` 路由头 + `KubernetesHandleExec` exec 回退供 `move` 用）；`RemoteBackend::file_transfer` 新增默认方法，`RemoteSandbox` 在 `ensure_handle`/快照重建 handle 后刷新并对外提供 `RemoteSandbox::file_transfer()`；`persist_workspace_bytes`/`hydrate_workspace_bytes` 优先走文件 API（`persist_via_file_api`/`hydrate_via_file_api`，tar 到 `.agentscope-tmp` 临时文件 → 上传/下载 → `rm -f` 清理），失败或未配置时回退 base64-over-exec；`fs_spec::SandboxFilesystemAdapter` 对支持路径优先原生传输。测试：`file_transfer_needs_the_file_api_base_dir`、`file_transfer_uploads_and_downloads_under_the_base_dir`、`persist_workspace_uses_the_file_api_when_enabled`、`persist_workspace_falls_back_to_base64_over_exec`、`hydrate_workspace_uploads_the_archive_then_extracts`、`remote::tests::native_file_transfer_is_published_after_the_handle_is_established`、`kubernetes_files::tests::file_api_path_and_temp_archive_names_are_deterministic`。harness 无 `SandboxFileTransfer` trait，故契约在本 crate 声明（见 §4.2） |
| E2B 原生文件传输接线 | `E2bSandbox implements SandboxFileTransfer`（`supportsFileTransfer`/`uploadFile`/`downloadFile` + `workspaceTransferPath` 校验） | **已完成（2026-10-28 接线）** | `e2b.rs`：`E2bFileTransfer`（`backend` + `handle`）实现本 crate 的 `SandboxFileTransfer`，`E2bBackend::file_transfer` 无条件返回（与 Java 一致）；新增 `workspace_transfer_path(workspace_root, absolute_path)` 逐条对齐 Java：路径与 root 都必须绝对、必须在 `rootPrefix` 之下（`/mnt/data-x` 不算 `/mnt/data` 的子路径）、相对部分不得含空段/`.`/`..`，否则拒绝并回退 exec。`E2bBackend` 改为 `Clone`（`known_snapshots` 改 `Arc<Mutex<_>>`）以便适配器持有快照。**另**：TAR 持久化不再 base64-over-exec，改为 Java 的「`tar -cf <tmp>` → envd `GET /files` 下载 → `rm -f`」，回灌为「`POST /files` 上传 → `tar -xf`」。测试：`workspace_transfer_path_stays_inside_the_workspace`、`file_transfer_declines_paths_outside_the_workspace`、`file_transfer_uploads_and_downloads_through_envd_files`、`persist_tar_uses_envd_files_instead_of_base64_over_exec`、`hydrate_tar_uploads_the_archive_before_extracting`、`fs_spec` 的 `adapter_*transfer*` 三条 |
| E2B envd Proto codec | `E2bEnvdProcessClient`（protobuf DynamicMessage） | **已完成（设计决定：默认改为 Json）** | 仅 JSON codec；**默认改为 `E2bCodec::Json`**（`#[default]` + `E2bSandboxClientOptions::default().codec` + `E2bCodec::parse` 回落三处一致），使**默认配置可用**。`E2bCodec::Proto` 保留枚举变体以对齐 Java，显式选择时 `exec_in` 返回 `SandboxExtensionError::Unsupported`（"set codec = E2bCodec::Json"），不静默错编码。**不迁移 Proto 的理由**：envd 的 proto 路径要求 Connect + protobuf framing（`google.protobuf.DynamicMessage` 等价物）与完整 `process.Process/Start` 描述符树，需引入 `prost`/`prost-types` 并手写/生成 envd schema，无真实端点可离线校验；envd 官方同时支持 JSON codec，语义等价。测试：`persistence_mode_and_codec_parse`（含 `E2bCodec::default()` 与 options 默认断言）、`default_codec_exec_works_without_opt_in`（httpmock 证明默认配置可直接 exec）、`proto_codec_exec_is_unsupported`（显式 Proto 仍报错） |
| E2B 快照删除与保留（`a0e89a3aa`） | `E2bPlatformHttp` 的 `deleteSnapshot` + `snapshotRetention` | **已完成（2026-10-20）** | `E2bBackend::delete_snapshot` 走 `DELETE /templates/{id}`（id 按单段 percent-encode，`team/x:v1` → `team%2Fx:v1`，否则 `/` 会被解析成额外路径段）；401/404 返回可诊断错误。`E2bSandboxClientOptions::snapshot_retention`（默认关闭，`Some(0)` 亦关闭）在创建原生快照后清理最旧的**本进程已知** id。**能力边界（代码注释与报告均已写明）**：E2B 无快照列表端点，`known_snapshots` 为进程内记录，重启后旧快照不可见、无法跨进程清理；`E2bSnapshotRefs` 只是 id 编码载体，不是列表来源 |
| K8s 连接策略 1:1 | `InClusterStrategy`/`DirectStrategy`/`GatewayStrategy`/`PortForwardStrategy`/`ConnectionStrategy` | **已完成（4/4 策略；PortForward 经注入 seam）** | `KubernetesConnectionStrategy { Direct, InCluster, Gateway }` + `KubernetesSandboxClientOptions::{sandbox_api_url, connection_strategy, gateway_address}`；默认优先级对齐 Java `KubernetesSandboxClient.toConnectionConfig`：显式 option → `sandbox_api_url`（Direct）→ `gateway_name`（Gateway）→ InCluster。`resolve_connector_base_url(options, strategy, sandbox_id, pod_ip, gateway_address)`：Direct = 预置 router URL（去尾斜杠）；InCluster = `http://<podIp>:<port>`（IPv6 加方括号），无 pod IP 时用 **sandbox 自身**的 `<sandboxId>.<ns>.svc.cluster.local:<port>`（修正旧实现误用 gateway 名的偏差）；Gateway = `scheme://<addr>`（Java `formatUrl` 不追加端口）。Gateway 地址优先取注入值 `gateway_address`，否则读 CRD `GET /apis/gateway.networking.k8s.io/v1/namespaces/{ns}/gateways/{name}` → `status.addresses[0].value`（对齐 Java `GatewayStrategy`，无 RBAC 时记 warn 并报配置错误）。路由头：`connector_router_headers` 注入 `X-Sandbox-ID`/`X-Sandbox-Namespace`/`X-Sandbox-Port`/`X-Sandbox-Pod-IP`，`InCluster` 不注入（Java `shouldInjectRouterHeaders`），同时用于 `POST /execute` 与文件 API。**残留已清零（2026-10-30）**：`PortForwardStrategy` 以 `KubernetesPortForwarder` seam 落地（未注入 forwarder 时明确报错、绝不降级），见 §8；默认策略仍为 `InCluster`（Java 默认 port-forward），差异保留并在 `resolve_connection_strategy` 文档注明。测试：`connection_strategy_derivation_matches_java_priority`、`direct_strategy_uses_the_configured_router_url`、`in_cluster_strategy_uses_pod_ip_then_service_dns`、`gateway_strategy_uses_the_external_address_without_a_port`、`router_headers_follow_the_strategy`、`exec_sends_router_headers_for_router_strategies`（httpmock 断言出站头）、`gateway_strategy_reads_the_address_from_the_crd`、`gateway_strategy_prefers_the_injected_address`、`gateway_address_is_parsed_from_the_crd_status` |
| K8s 异常类 1:1 | `client/exceptions/*`（8 类） | 已完成（设计决定，不迁移：归并到错误码） | 归并到 `SandboxErrorCode`/`SandboxExtensionError`；未逐类建模。依据：Java 侧 `catch (SandboxTemplateNotFoundException)` 这类分支在 Rust 用 `SandboxErrorCode` 区分；`TemplateNotFound`/`WarmPoolNotFound` 的**消息文本**已逐字对齐（`ClaimFailure::message`） |
| 真实端点联调 | — | **已完成（2026-10-31，环境变量门控）** | 网络层早已实现；本轮补两条环境变量门控的真实端点冒烟用例（未设变量时 `eprintln!` skip + `return`，不用 `#[ignore]`）：`e2b::tests::live_e2b_round_trip_when_a_real_api_key_is_configured`（`AGENTSCOPE_TEST_E2B_API_KEY` / `_URL` / `_TEMPLATE`：create → exec → persist → shutdown）与 `kubernetes::tests::live_kubernetes_round_trip_when_a_cluster_is_configured`（`AGENTSCOPE_TEST_K8S_API_URL` + `AGENTSCOPE_TEST_K8S_WARM_POOL`，可选 `_TOKEN` / `_NAMESPACE`：claim → exec → delete）。**凭据缺失不再是阻塞项**：默认离线跑两用例直接返回。agentrun/daytona 未加同类用例（无真实端点联调诉求，传输层已由 httpmock 覆盖） |
| Java 测试逐条对照 | Java tests 23 文件 / 4554 行 | **已完成（设计决定，不逐条照搬）** | 口径改为「每个 Java 行为分支在 Rust 有等价用例覆盖」（纯函数或 httpmock 用例），不照搬 Java 的测试文件/类划分：Java 侧大量用例依赖 fabric8 `KubernetesServer`、OkHttp `MockWebServer` 等 JVM 基建，Rust 无等价物且不应为测试引入。分支映射见 §10.4（三个增量提交逐条对齐）与 §1.1（225 个用例） |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| E2B 云桶挂载策略 | `e2b/mounts/E2bCloudBucketMountStrategy` | 已完成（设计决定，不迁移）（2026-10-29 勘误：原记「待办」；Java 该类为 `intentionally empty` 空接口，无方法可迁） | 无 Rust 对应。**注意**：Java 该接口是**空占位**（`intentionally empty`），无实际方法，迁移价值为 0 |
| E2B 沙箱创建环境变量 | `E2bSandboxClientOptions.envs`（`Map<String,Supplier<String>>`）+ `E2bPlatformHttp` 的 `envVars` 字段 | **已完成（2026-10-31）** | `E2bSandboxClientOptions.envs: BTreeMap<String,String>` + `with_env`（空名拒绝）；`build_create_sandbox_body_with_envs` 输出 `envVars`（空 map 或全空值时整字段省略，对齐 Java `resolveEnvVars` 跳过空值），创建路径统一走它；`fs_spec::E2bFilesystemSpec::env(name, value)` 对应 Java `E2bFilesystemSpec#env`。测试：`create_body_with_env_vars_matches_java`、`create_body_omits_env_vars_when_empty`、`with_env_rejects_blank_names`、`platform_create_sends_env_vars`（httpmock 断言出站请求体）、`fs_spec::tests::e2b_spec_builders_thread_envs_and_volume_mounts` |
| K8s CRD spec 扩展字段 | `SandboxClaimSpec.lifecycle`（shutdownTime/shutdownPolicy）、`SandboxClaimSpec.additionalPodMetadata`（labels/annotations）、`CreateSandboxOptions.shutdownAfterSeconds` | **已完成（2026-10-31）** | options 新增 `claim_labels` / `shutdown_after_seconds` / `pod_labels` / `pod_annotations`；`build_claim_body_with_options` 合并标签（`CREATED_BY_LABEL` 恒胜、空 key 丢弃）、仅有 TTL 时发 `lifecycle`、仅 pod label/annotation 非空时发 `additionalPodMetadata`（`build_claim_body` 保留旧签名与旧字节形状）；`claim_lifecycle` 生成秒级 `ISO_INSTANT`（`Z`）+ `shutdownPolicy=Delete`，`Some(0)` 报配置错误（对齐 Java `IllegalArgumentException`）。另补 `claim_failure`（`TemplateNotFound`/`WarmPoolNotFound` 快速失败，正文逐字对齐 Java 异常）与 `select_pod_ip`（优先 IPv4，等价 `K8sHelper#selectPodIp`）。测试：`claim_body_carries_lifecycle_and_pod_metadata`、`claim_body_keeps_the_legacy_shape_without_extras`、`claim_lifecycle_rejects_zero_and_formats_iso_instant`、`claim_failure_maps_template_and_warm_pool_conditions`、`select_pod_ip_prefers_ipv4_and_skips_blanks`、`create_sandbox_sends_the_extended_claim_spec`（httpmock 断言真实请求体）、`create_sandbox_fails_fast_when_the_template_is_missing` |
| K8s 客户端附加能力 | `SandboxClient.listActiveSandboxes`/`listAllSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName`、`K8sHelper.listSandboxClaims` | **已完成（2026-10-30，见 §9.2）** | `KubernetesBackend::{list_sandbox_claims(Option<&str>), get_sandbox_claim, get_sandbox_claim_warm_pool_name, delete_sandbox_claim, list_active_sandboxes, delete_all}` + 纯函数 `parse_claim_names`/`claim_warm_pool_name`；`active_claims: Arc<Mutex<BTreeSet>>` 复刻 Java `activeSandboxes`（`namespace/claimName` 键），`create_sandbox`/`connect_sandbox` 记录、`kill_sandbox`/`delete_sandbox_claim`/`delete_all` 摘除 |
| K8s K8sHelper 细分能力 | `client/internal/K8sHelper` | **已完成（2026-10-30 list/get/delete/warmPool；2026-10-31 条件快速失败。`watch=true` 与模板见右）** | reqwest 直连覆盖 claim create/get/list/delete/status/exec，并补齐 `selectPodIp` / `checkClaimFailureConditions` 等价物（`select_pod_ip` / `claim_failure`）。**`watch=true` 流式订阅：已完成（设计决定，不迁移）**——`decode_json` 用 `resp.text()` 读完整 body，而 `?watch=true` 在 `timeoutSeconds` 前不会结束，直接接会挂死；正确实现需 `bytes_stream()` 分帧 + `resourceVersion` 续订 + 取消语义，且本 crate 无宿主后台任务/取消句柄（§9.3 已评估）；就绪等待用轮询，语义等价且已有用例。**模板（`SandboxTemplate`）：已完成（设计决定，不迁移）**——Java HEAD 里 `SandboxTemplate` 仅出现于异常名（`K8sHelper.java:350` 的 `SandboxTemplateNotFoundException`），无任何模板 CRD 读写，对应语义已由 `claim_failure` 覆盖 |
| Jackson 模块 | `*HarnessSandboxJacksonModule` | 已完成（设计决定，不迁移） | serde 按 enum 变体序列化；`rg HarnessSandboxJacksonModule crates/` → 0 命中（无子类型注册概念），state 的向后兼容靠 `#[serde(default)]` 而非 Jackson 模块 |
| 投影哈希算法对齐 | Java `MessageDigest` | 已完成（语义等价） | Rust 用 FNV-1a；仅用于变更检测，不跨语言互操作 |
| E2B 创建请求缺 volume mount（2026-10-26 新增） | `9730e8171`/`aa66abfd0`（`E2bPlatformHttp.createSandbox` 支持 `volumeMounts`） | ✅ 已完成（2026-10-27） | `e2b.rs`：新增 `E2bVolumeMount { name, path }`——`name` 是**平台侧已存在的 E2B 卷**、`path` 是沙箱内绝对路径（对齐 Java `E2bVolumeMount`；Java 的 `E2bCloudBucketMountStrategy` 在同 commit 里是**空占位接口**，故云桶挂载双方都不实现，tracker 旧描述里的“宿主目录/云桶”与 Java 实际不符，已更正）；`normalize_mount_path`（trim / `\`→`/` / 折叠 `//` / 去尾斜杠）、`covers_workspace_root`、`require_distinct_mount_paths` 逐条对齐 Java；`E2bSandboxClientOptions.volume_mounts` + `with_volume_mount`；`build_create_sandbox_body_with_mounts` 输出 `volumeMounts: [{name, path}]`，**空挂载时整字段省略**（`build_create_sandbox_body` 签名与输出完全不变），新建 / connect 兜底重建 / 原生快照恢复三条创建路径共用。测试：`create_body_with_volume_mounts_matches_java`、`create_body_without_volume_mounts_keeps_the_legacy_shape`、`platform_create_sends_volume_mounts`（httpmock 精确断言请求体）、`volume_mount_validates_name_and_normalizes_path`、`volume_mount_covers_workspace_root`、`require_distinct_mount_paths_rejects_duplicates`、`options_with_volume_mount_validates_and_rejects_duplicates`。反向验证：关掉挂载写入分支 → 前两条 FAILED。**遗留已清零（2026-10-31）**：`workspace_on_volume(workspace_root, mounts)`（root 等于某挂载或严格位于其下）→ 卷工作区 persist 返回空 marker、hydrate 对空归档 no-op；`volume_tar_exclude_args` 只对**严格位于 root 之下**的挂载产出 `--exclude=./<rel>`（root 为 `/` 时去掉分隔符），并经 `build_e2b_tar_command` 逐个 `shell_quote`；构造时拒绝相对 `workspace_root`（Java 硬化）；Rust `kill_sandbox` 只 `DELETE /sandboxes/{id}`、本就没有 `rm -rf`，故「destroy 跳过 rm -rf」自然成立。测试：`workspace_on_volume_detects_mount_coverage`、`volume_tar_exclude_args_only_cover_nested_mounts`、`build_e2b_tar_command_quotes_excludes`、`persist_and_hydrate_are_noops_for_a_volume_backed_workspace` |
| E2B exec 无服务端 kill + 可靠超时（2026-10-26 新增） | `9730e8171`/`aa66abfd0` | ✅ 已完成（2026-10-27） | Java **没有独立 kill 端点**：服务端 kill 就是在 `POST {envd}/process.Process/Start` 上带 `Connect-Timeout-Ms: <timeoutSeconds*1000>`，envd 到 deadline 终止远端进程。`e2b.rs::exec_in` 按此实现：① `timeout_seconds > 0` 时注入该头（`= 0` 必须省略——`0` 的语义是「deadline 已过期」而不是不限时）；② 客户端用 `RequestBuilder::timeout(T + CLIENT_TIMEOUT_SLACK_SECONDS(5))` 覆盖整体时限（对齐 Java per-call `callTimeout`），外层再套 `with_exec_deadline` 兜底；③ 超时统一映射为新错误码 `SandboxErrorCode::ExecTimeout`（`Command timed out after Ns`，**不含命令文本**——命令是模型生成的，可能内联密钥）；④ envd end-stream trailer `deadline_exceeded` 同样映射为 `ExecTimeout`；⑤ deadline 之后仍无退出码的流降级为 `ExecTimeout`。kill 通道抽象为可注入的 `E2bKillDeadline`（生产实现 `EnvdKillDeadline`），便于离线单测。测试：`exec_timeout_arms_the_server_side_kill_and_reports_an_error`（httpmock 断言出站头 + 记录型 fake 断言 kill 参数）、`exec_without_deadline_does_not_arm_the_kill`、`exec_deadline_expires_and_reports_a_timeout`、`connect_timeout_ms_header_arms_the_server_side_kill`、`exec_client_deadline_adds_the_backup_slack`、`drain_envd_stream_maps_deadline_exceeded_to_an_exec_timeout`。反向验证：去掉 `Connect-Timeout-Ms` 注入 → 首条 FAILED；把超时错误改成 `Transport` → deadline 用例 FAILED。**2026-10-31 补齐 `aa66abfd0` 第二半（`readTimeout=0`）**：`E2bBackend` 新增**无 client 级 total timeout** 的 `exec_client`（仅 connect timeout），exec 全走它；`timeout_seconds > 0` 时 `RequestBuilder::timeout(T+5s)` 是唯一整体时限，`= 0` 时真无界（对齐 Java `callTimeout(0)`）。用例 `exec_is_not_bounded_by_the_base_read_timeout`（`read_timeout_seconds=1`、mock 延迟 1.5s、无 deadline → 必成功） |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：四个远端沙箱后端（agentrun / daytona / e2b / kubernetes）全部适配到 harness `Sandbox`/`SandboxClient` trait；共享 `RemoteBackend` glue、HTTP、重试、工作区投影、状态投影、快照接入均已落地。**四后端均为真实 reqwest HTTP 实现**（非 trait 占位）：agentrun 18 处 / daytona 13 处 / e2b 34 处 / kubernetes 14 处 HTTP 调用，各含 1 个 `impl RemoteBackend`。
- **Rust 产物**：`src/` 12 个源文件（`lib/error/http/retry/workspace/remote/agentrun/daytona/e2b/kubernetes/fs_spec/kubernetes_files.rs`），**10201 行**；**176 个单测**（`#[test]` + `#[tokio::test]`，含 `#[tokio::test(start_paused = true)]`）。**已完成（2026-10-29 勘误）**：原记「6721 行 / 137 个单测」，实际以 §1.1（2026-10-28 实测）与 §7.3（176 passed）为准。
- **验证（实测，汇总方基线）**：
  ```
  cargo build -p agentscope-extensions-sandbox   -> 通过（0 error）
  cargo test  -p agentscope-extensions-sandbox   -> 108 passed / 0 failed / 0 ignored
  cargo test  --workspace                        -> 1866 passed / 0 failed（86 test target）
  ```
  > ⚠️ 上面的 `108 passed` 是**汇总方旧基线**（对应 §1.1 旧计数）。**2026-10-22 静态复核实测 `#[test]`/`#[tokio::test]` 标注数为 137**（`rg -o '#\[(tokio::)?test' src/`），逐文件：`agentrun` 11 / `daytona` 7 / `e2b` 43 / `error` 3 / `fs_spec` 18 / `http` 3 / `kubernetes` 9 / `kubernetes_files` 20 / `remote` 11 / `retry` 7 / `workspace` 5 = 137。**下次 `cargo test` 应确认实际通过数**（本轮为静态复核，未编译、未运行测试）。
- **已知问题**：无阻塞（2026-10-31 三轮收口，见 §10）。真实端点已有环境变量门控冒烟用例（默认 skip）；Java 侧能力差异均已裁定（§3.2/§3.3/§10.5）。~~新增确认的接线缺口：K8s 文件 API 与 E2B 原生文件传输**均未接入沙箱生命周期**（§3.2）~~ → **已于 2026-10-28 接线并测试覆盖**（见 §7）。
- **2026-10-27 实测**：`cargo test -p agentscope-extensions-sandbox` → **151 passed / 0 failed / 0 ignored**（含本轮新增的 `volumeMounts` 与 exec deadline/kill 用例；三处反向验证见 §3.3 两行）。
- **2026-10-28 实测**：`cargo build -p agentscope-extensions-sandbox` → 通过（0 error / 0 warning）；`cargo test -p agentscope-extensions-sandbox` → **176 passed / 0 failed / 0 ignored**（151 → +25，详见 §7）。`cargo fmt -p agentscope-extensions-sandbox -- --check` → 通过。

### 4.2 下一步与阻塞
1. **harness 抽象缺口（不改 harness，仅记录）**：harness `agent::sandbox` 的 `state` 模块为私有（`mod state;`），`RemoteSandboxState` **未在 `sandbox/mod.rs` re-export**（`pub use state::{InMemorySandboxStateStore, LocalSandboxState, SandboxState, SandboxStateStore};` 不含 `RemoteSandboxState`）。本 crate 因此无法命名 `RemoteSandboxState`，只能经 `serde_json` 与 `SandboxState::Remote` 变体互转（见 `remote.rs` 内注释）。**已于 2026-10-29 解决**：harness `sandbox/mod.rs` 已 re-export `RemoteSandboxState` 与 `SandboxSnapshotRef`，`RemoteSandboxStateData::to_harness_state` 改为直接构造（不再靠 `serde_json` 往返），并因此能携带快照身份（见本文件末尾 2026-10-29 执行记录）。
2. **harness 缺少 `SandboxFilesystemSpec` / `SandboxFileTransfer`**：Java 扩展的 4 个 `*FilesystemSpec` 继承 harness Java `SandboxFilesystemSpec`（`io.agentscope.harness.agent.filesystem.spec`），`SandboxFileTransfer` 亦为 harness Java 类型（`io.agentscope.harness.agent.sandbox`）。**Rust harness 复核确认两者皆无**（`rg 'SandboxFilesystemSpec|SandboxFileTransfer' crates/agentscope-harness/src/` → 0 命中；harness 只有 `SandboxFilesystem` trait + `SandboxBackedFilesystem`）。**已按裁定在本 crate 内建适配层**（`fs_spec.rs` 的 `SandboxFilesystemSpec` + `kubernetes_files.rs` 的 `SandboxFileTransfer`），**未改 harness/core**。若 harness 后续补齐同名类型，本 crate 的实现可直接切换继承/实现面，无需改调用点。**遗留**：Java `SandboxBackedFilesystem` 会在沙箱实现 `SandboxFileTransfer` 时**优先走原生传输**；Rust harness 无此机制。**已完成（2026-10-29 勘误）**：原记「`fs_spec::SandboxFilesystemAdapter` 只能全量走 exec+base64」，实际适配器已优先原生传输（用例 `fs_spec::tests::adapter_write_prefers_native_transfer`、`adapter_read_prefers_native_transfer_and_windows_lines`），仅需调用方显式 `from_remote_sandbox(...)` / `filesystem_with_transfer(...)` 传入传输适配器（harness `Sandbox` 为 `dyn` 且无 `as_any`，无法自动下转，见 §7.5-3）。
3. **P1 剩余**（2026-10-28 更新）：① `PortForwardStrategy`（fabric8 `LocalPortForward`）无对应，Java 在该场景（无 apiUrl/gateway）为默认 — Rust 回退 `InCluster`，已记差异；② E2B `workspaceOnVolume`（工作区在卷上时的 tar no-op / destroy 跳过 / 嵌套挂载 `--exclude`）未迁移；③ E2B `envs → envVars`、K8s `SandboxClaimSpec.lifecycle`/`additionalPodMetadata` 等 P2 字段。~~E2B Proto codec、K8s 策略语义、文件 API / 原生传输接线~~ → **已完成（见 §7）**。**2026-10-29 复核补充的完整残留清单**：(1) `PortForwardStrategy`（`LocalPortForward` 无对应）；(2) `GatewayStrategy` 无 watch（一次性读取）；(3) `K8sHelper` 的 list/watch/模板/port-forward 未迁移；(4) K8s `listSandboxClaims`/`listActiveSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName` 未迁移；(5) `SandboxFilesystemSpec` 缺 `executionGuard`/`workspaceProjectionEnabled`/`workspaceProjectionRoots`，且 `__workspace_projection__` 未真写入（仅常量保留）；(6) 原生传输需显式 `from_remote_sandbox`（`dyn Sandbox` 无 `as_any`）；(7) E2B `workspaceOnVolume` 与 `envs → envVars`；(8) K8s CRD `lifecycle`/`additionalPodMetadata`/`shutdownAfterSeconds`；(9) E2B `E2bCloudBucketMountStrategy`（Java 为空占位接口，迁移价值 0）。
   > **2026-10-30**：上述 (1) `PortForwardStrategy` 与 (5) `SandboxFilesystemSpec` 的 guard/projection 已完成（见 §8）；(2)(3)(4)(6)(7)(8)(9) 仍待办。
   > **2026-10-30 二轮**：(2) `Gateway` 地址等待、(4) `listSandboxClaims`/`listActiveSandboxes`/`deleteAll`/`getSandboxClaimWarmPoolName` 已完成（见 §9.2 / §9.4）；(3) 仅剩 `watch=true` 流式订阅、模板与 port-forward 其余能力；(6)(7)(8)(9) 已于 2026-10-31 收口（见 §10）：(6) 定为本 crate 的显式接线契约 + 跨模块需求（harness `Sandbox` 无 `as_any`），(7)(8) 落代码（含 httpmock 断言真实请求体），(9) 维持「已完成（设计决定，不迁移）」。另：`--lib` 高频 flake 的根因（本机系统代理劫持 loopback mock 流量）已修复，见 §9.1。
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

## 6. 静态复核记录（2026-10-22）

> ⚠️ 本节是 2026-10-22 的**历史快照**（未编译、未运行）。其中 §6.4 列出的 P1 缺口（K8s 文件 API / E2B 原生传输 / K8s 连接策略生态位 / harness 无 `SandboxFileTransfer`）与 §6.3 的 e2b/kubernetes 结论已过时；**当前状态以 §3.2 与 §7 为准**。

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
| Rust 源码行数 | ~6069 | **6721** | ❌ 过期（-652）；（2026-10-29 勘误：现为 **10201 行 / 176 单测**，见 §1.1/§7.3） |
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
| 5 | Rust「12 个源文件，~6069 行」 | 12 个源文件，**6721 行** | §1.1 已改；（2026-10-29 勘误：现为 **10201 行 / 176 单测**，见 §1.1/§7.3） |
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

## 7. 2026-10-28 执行记录（P1 接线修复）

### 7.1 范围与改动文件
只改本 crate + 本 tracker，未动根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate；**无新增依赖**（复用 `reqwest`/`httpmock`/`serde`/`base64`/`uuid`）。

| 文件 | 改动 |
|---|---|
| `src/kubernetes.rs` | 新增 `KubernetesConnectionStrategy{Direct,InCluster,Gateway}`、`GATEWAY_*`/`HEADER_SANDBOX_*` 常量、`parse_gateway_address`、`connector_router_headers`、`host_for_url`；`resolve_connector_base_url` 改为五参数显式分派（修正旧 DNS 分支误用 gateway 名）；options 新增 `sandbox_api_url`/`connection_strategy`/`gateway_address` + `resolve_connection_strategy`/`effective_gateway_namespace`/`file_api_enabled`；`KubernetesBackend` 改 `Clone`，新增 `native_file_transfer`/`connector_base_url`/`router_headers`/`resolve_gateway_address`/`persist_via_file_api`/`hydrate_via_file_api` + `KubernetesHandleExec`（`ExecTransport`）；`exec_in` 注入路由头；`persist/hydrate_workspace_bytes` 优先文件 API |
| `src/kubernetes_files.rs` | 新增 `file_api_path`、`temp_archive_rel_path`（UUID 去重） |
| `src/remote.rs` | `RemoteBackend::file_transfer`（默认 `None`）、`RemoteSandbox` 新增 `transfer` 字段 + `file_transfer()` + `refresh_file_transfer()`（`ensure_handle` 及快照重建 handle 后刷新） |
| `src/fs_spec.rs` | `SandboxFilesystemAdapter` 新增 `transfer` + `new_with_transfer`/`with_file_transfer`/`file_transfer()`/`from_remote_sandbox`；`write`/`read`（进而 `edit`）优先原生传输，否则回退 exec+base64；`read` 新增 `apply_line_window`（等价 `sed -n 'a,bp'`）/`file_data_from_bytes`；`SandboxFilesystemSpec::filesystem_with_transfer` |
| `src/e2b.rs` | 默认 codec 改 `Json`（`#[default]` + options 默认 + `parse` 回落）；`E2bBackend` 改 `Clone`（`known_snapshots: Arc<Mutex<_>>`）；新增 `workspace_transfer_path`/`new_temp_tar_path`/`E2bFileTransfer`；`file_transfer` 能力位；TAR persist/hydrate 改走 envd `/files`（`persist_tar_via_files`/`hydrate_tar_via_files`） |

### 7.2 逐项结果
| 任务 | 结果 | 关键测试 |
|---|---|---|
| ① K8s 文件 API 接线（`file_api_base_dir` 死配置） | **完成** | `persist_workspace_uses_the_file_api_when_enabled`（POST `/execute` tar → GET `/download/...`，断言归档字节）、`hydrate_workspace_uploads_the_archive_then_extracts`（POST `/upload` → `tar -xf`）、`file_transfer_uploads_and_downloads_under_the_base_dir`、`file_transfer_needs_the_file_api_base_dir`、`persist_workspace_falls_back_to_base64_over_exec`、`remote::tests::native_file_transfer_is_published_after_the_handle_is_established` |
| ② E2B 原生传输 + `workspaceRoot` 校验 | **完成** | `workspace_transfer_path_stays_inside_the_workspace`、`file_transfer_declines_paths_outside_the_workspace`、`file_transfer_uploads_and_downloads_through_envd_files`、`persist_tar_uses_envd_files_instead_of_base64_over_exec`、`hydrate_tar_uploads_the_archive_before_extracting`、`fs_spec::adapter_write_prefers_native_transfer`/`adapter_write_falls_back_to_exec_outside_the_transfer_scope`/`adapter_read_prefers_native_transfer_and_windows_lines` |
| ③ K8s 连接策略 + 路由头 | **完成（3/4 策略）** | `connection_strategy_derivation_matches_java_priority`、`direct_strategy_uses_the_configured_router_url`、`in_cluster_strategy_uses_pod_ip_then_service_dns`、`gateway_strategy_uses_the_external_address_without_a_port`、`router_headers_follow_the_strategy`、`exec_sends_router_headers_for_router_strategies`（httpmock 断言四个出站头）、`gateway_strategy_reads_the_address_from_the_crd`、`gateway_strategy_prefers_the_injected_address` |
| ④ E2B `Proto` 默认不可用 | **完成（默认改 `Json`，Proto 作为设计决定不迁移）** | `persistence_mode_and_codec_parse`、`default_codec_exec_works_without_opt_in`、`proto_codec_exec_is_unsupported` |

### 7.3 验证（实测）
```
cargo build -p agentscope-extensions-sandbox   -> 通过（0 error，本 crate 0 warning）
cargo test  -p agentscope-extensions-sandbox   -> 176 passed / 0 failed / 0 ignored（基线 151）
cargo fmt   -p agentscope-extensions-sandbox -- --check -> 通过
```

### 7.4 反向验证（逐个回退接线 → 对应用例 FAILED → 已恢复）
| # | 回退内容 | FAILED 用例（实测） |
|---|---|---|
| 1 | `KubernetesBackend::native_file_transfer` 改为恒返 `Ok(None)`（关掉文件 API 接线） | `kubernetes::tests::persist_workspace_uses_the_file_api_when_enabled`、`kubernetes::tests::hydrate_workspace_uploads_the_archive_then_extracts`、`kubernetes::tests::file_transfer_uploads_and_downloads_under_the_base_dir` |
| 2 | `exec_in` 不再注入路由头（`headers = vec![]`） | `kubernetes::tests::exec_sends_router_headers_for_router_strategies` |
| 3 | `SandboxFilesystemAdapter::read`/`write` 的原生分支改为 `if false && ...` | `fs_spec::tests::adapter_write_prefers_native_transfer`、`fs_spec::tests::adapter_read_prefers_native_transfer_and_windows_lines` |
| 4 | `E2bCodec` 的 `#[default]` 移回 `Proto`（并同步回退 `E2bSandboxClientOptions::default().codec`） | `e2b::tests::persistence_mode_and_codec_parse`（`left: Proto, right: Json`）、`e2b::tests::default_codec_exec_works_without_opt_in`（`Unsupported("E2B envd PROTO codec is not implemented…")`） |

> 恢复后重跑：**176 passed / 0 failed**（EXITCODE=0）。

### 7.5 已知差异 / 需裁定
1. **`PortForwardStrategy` 未迁移**（需 fabric8 `LocalPortForward` 等价物）。Java 在「无 apiUrl、无 gateway」时为默认策略；Rust 默认回落 `InCluster`（pod IP → 服务 DNS）。已在 §3.2 记差异。→ **已迁移（2026-10-30，`KubernetesPortForwarder` seam，见 §8）**
2. **`Gateway` 策略的地址来源**：优先注入的 `gateway_address`，否则读 Gateway CRD（`gateway.networking.k8s.io/v1` REST，需 `api_url` + RBAC）；Java 额外 watch 就绪事件，Rust 为一次性读取（无 watch）。→ **2026-10-30 二轮：已改为「一次性读 + 有界轮询等地址出现」（等价 Java `connect()` 的 watch-until-address，无后台任务），见 §9.4；变更感知靠每次调用重解析**
3. **`SandboxFilesystemSpec::filesystem` 仍不自动探测原生传输**：harness `Sandbox` 是 `dyn` 且无 `as_any`，无法从 `Arc<dyn Sandbox>` 下转；因此提供 `SandboxFilesystemAdapter::from_remote_sandbox(Arc<RemoteSandbox>, ..)` 与 `filesystem_with_transfer(...)` 显式接线（传 `RemoteSandbox::file_transfer()`）。
4. **E2B `Proto` codec 不迁**：需 envd protobuf schema + `DynamicMessage` 等价物，且无真实端点离线可验；envd 同时支持 JSON codec，语义等价。
5. **`fs_spec` 适配器捕获的传输适配器在句柄重建时不会自动刷新**（构造时快照）；`RemoteSandbox` 自身的 `file_transfer()` 会在 `ensure_handle`/快照重建后刷新。如果调用方在重建后继续用旧适配器，需重新构造。

---

## 2026-10-29 执行记录（harness 侧沙箱状态持久化 / 快照身份）

对应汇总 tracker §1.1 第 ③ 项：MONGODB 的 `cascadeDeleteSnapshots` 读 `states._sandbox_state.json` 取快照 id，而 harness 侧既无该槽位的写入实现、也无快照字段。

| 项 | 变更 | 位置 |
|---|---|---|
| `SandboxSnapshotRef` | 新增可序列化快照身份（`noop` / `local{id}` / `remote{id}`，serde `type` 判别）；三个 `*SandboxState` 增 `#[serde(default)] snapshot`，加 `SandboxState::snapshot()` / `set_snapshot()`；旧 JSON 无该字段仍可读取 | `agentscope-harness/src/agent/sandbox/state.rs` |
| `AgentStateStoreSandboxStateStore` | 实现 `SandboxStateStore`（桥接 core `AgentStateStore`）；槽位/key/值形态对齐 Java `SessionSandboxStateStore`：`sandbox/session\|user\|agent\|global` + `_sandbox_state` + `{json,deleted}`，`delete` 写墓碑；`list_keys` 由 `list_session_ids` 反推 | `agentscope-harness/src/agent/sandbox/session_state_store.rs`（新） |
| `RemoteSandboxState` 导出 | 之前未 re-export，本 crate 只能靠 JSON 往返投影；现直接构造（顺带修掉一处只好靠 serde 形状维持的隐式契约） | `agentscope-harness/src/agent/sandbox/mod.rs` |
| 本 crate `RemoteSandboxStateData` | 新增 `snapshot: Option<SandboxSnapshotRef>`，`to_harness_state`/`from_harness_state` 双向携带；`RemoteSandboxClient::create` 在 `snapshot_spec.build()` 成功后写入身份 | `src/remote.rs` |
| Docker 后端 | 创建时同样把快照身份写回 state（此前构建后丢弃） | harness `sandbox/docker.rs` |

**仍未接线（不阻塞）**：`SandboxManager` 在生产代码中无调用点（sandbox 生命周期由宿主装配），
宿主需在 `SandboxManager::new(client, Arc::new(AgentStateStoreSandboxStateStore::new(state_store, agent_id)), agent_id)`
时显式注入。`RemoteSandboxClient::resume` 仍不重新绑定 snapshot client（既有行为，由 `RemoteSnapshotSpec` 负责）。

验证：`agentscope-extensions-sandbox` **177 passed**（含新增 `create_records_snapshot_identity_in_state`）；
harness `--lib` 502 passed；`agentscope-extensions-mongodb` 82 passed（含新增真实链路用例）。

---

## 8. 2026-10-30 执行记录 — PortForward / FilesystemSpec guard+projection

对应 §4.2 残留清单的 **(1) `PortForwardStrategy`** 与 **(5) `SandboxFilesystemSpec` 缺 `executionGuard`/`workspaceProjectionEnabled`/`workspaceProjectionRoots` 且 `__workspace_projection__` 未真写入**。

### 8.1 Java 依据（逐文件读取，未用 `git show` 全量）
| Java 文件 | 关键语义 |
|---|---|
| `.../kubernetes/client/internal/strategy/PortForwardStrategy.java` | 读 `sandbox-router-svc` 的 `Endpoints` → `subsets[].addresses[].targetRef.name` 得 router pod；`portForward(Constants.ROUTER_PORT, 0)` 取本地端口；baseUrl = `http://127.0.0.1:<localPort>`；已建立则缓存复用（`isAlive()`） |
| `.../kubernetes/client/Constants.java` | `ROUTER_SERVICE_NAME="sandbox-router-svc"`、`ROUTER_PORT=8080` |
| `.../kubernetes/client/config/LocalTunnelConnectionConfig.java` | router 命名空间默认 `agent-sandbox-system` |
| `agentscope-harness/.../filesystem/spec/SandboxFilesystemSpec.java` | 字段 `executionGuard` / `workspaceProjectionEnabled`（默认 true）/ `workspaceProjectionRoots`（默认 5 项）；`toSandboxContext(hostWorkspaceRoot)` 把 `__workspace_projection__` = `{sourceRoot: 绝对化路径, includeRoots: roots}` 写入 entries map；`hostWorkspaceRoot == null` 或未启用则不写；guard 由 `HarnessAgent` 传给 `SandboxManager`（null → noop） |

### 8.2 改动文件
| 文件 | 改动 |
|---|---|
| `src/kubernetes.rs` | ① `KubernetesConnectionStrategy::PortForward`（`as_str()="port-forward"`；`should_inject_router_headers()` 仍只对 `InCluster` 返 false，隧道后面仍是 router 故注入）；② 常量 `ROUTER_SERVICE_NAME`/`ROUTER_PORT`/`DEFAULT_ROUTER_NAMESPACE`；③ `ForwardedEndpoint{local_host,local_port}` + `base_url(scheme)`（IPv6 加方括号，复用 `host_for_url`）；④ seam trait `KubernetesPortForwarder::forward(sandbox_id, namespace, pod_name, remote_port) -> SandboxResult<ForwardedEndpoint>`（要求对 `(ns,pod,port)` 幂等）；⑤ options 新增 `port_forwarder` / `port_forward_router_namespace`（默认 `agent-sandbox-system`）/ `port_forward_pod`（Endpoints 不可读时的显式逃生口，同 `gateway_address` 套路）；⑥ 新 `resolve_port_forward_base_url(options, sandbox_id, pod_name)`（async）——未注入 forwarder ⇒ `Configuration` 错误，**不降级**；⑦ 同步 `resolve_connector_base_url` 的 `PortForward` 分支返回明确 `Configuration` 错误（async 隧道不在此解析，同样不降级）；⑧ `parse_endpoint_target_pod`（对齐 `resolveRouterPod`）+ `KubernetesBackend::resolve_router_pod()`（注入 pod 优先，否则 `GET /api/v1/namespaces/{ns}/endpoints/sandbox-router-svc`，失败记 warn 返回 `None` 由调用方报配置错误）；⑨ `connector_base_url` 先处理 `PortForward`（pod 解析失败 ⇒ 配置错误；forwarder 错误原样透传） |
| `src/fs_spec.rs` | ① `SpecCommon` 增 `execution_guard: Option<Arc<dyn SandboxGuard>>`、`workspace_projection_enabled`（默认 true）、`workspace_projection_roots`（默认 `DEFAULT_WORKSPACE_PROJECTION_ROOTS`），手写 `Default`；② trait 增 `execution_guard()` / `workspace_projection_enabled()` / `workspace_projection_roots()` / `workspace_projection_entry(Option<&Path>)` / `layout_spec(Option<&Path>)` / `projection_payload(&Path)` / `build_context_with_projection(&Path) -> (SandboxContext, Option<ProjectionPayload>)`；③ 四类 spec（`platform_spec!` 宏内一次改全）增 `with_execution_guard` / `with_workspace_projection_enabled` / `with_workspace_projection_roots`（**刻意用 `with_*` 前缀**：同名无前缀方法会与 trait getter 冲突并遮蔽取值）；④ `absolute_path` = Java `toAbsolutePath().normalize()` 的等价物（纯词法折叠，不要求目录存在）；⑤ `__workspace_projection__` 真正写入 harness `LayoutSpec.entries`（`WorkspaceEntry::WorkspaceProjection`），并可经 harness `WorkspaceProjectionApplier` 生成 tar（`projection_payload`） |
| `docs/extensions/SANDBOX__GAP_TRACKER.md` | 本节 + §1 总判 / §3.2 两行 / §4.2 指针（历史章节只加指针，未做批量替换） |

无新增依赖；未改根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。

### 8.3 harness 契约差异（不改 harness）
- Java 的 `WorkspaceSpec` 是 entries map，`toSandboxContext` 直接把投影条目放进去；Rust `SandboxContext` 携带的是**扁平化** `WorkspaceSpec`（`root`/`directories`/`files`），富条目只在 `LayoutSpec` 里。故本 crate 用 `layout_spec(Some(host_root))` 承载 `__workspace_projection__`（真正写入 `LayoutSpec.entries`），`build_context_with_projection` 一次返回「上下文 + 投影 tar」，宿主在 `start` 时灌入即可（等价 Java 在 `Sandbox#start()` 的 archive hydration）。
- Java 的 guard 由 `HarnessAgent` 注入 `SandboxManager`；Rust `SandboxManager::with_shared_guard(Arc<dyn SandboxGuard>)` 已存在，spec 只负责**保留/暴露**（`execution_guard()`），装配由宿主完成。
- 默认策略差异保留：Java 无 apiUrl/gateway 时默认 port-forward；Rust 仍默认 `InCluster`，因为 port-forward 必须由宿主注入 `KubernetesPortForwarder`，静默选中会必然报错。已在 `resolve_connection_strategy` 文档与 §3.2 注明。

### 8.4 测试（177 → 188，新增 11 个离线用例）
| 文件 | 用例 |
|---|---|
| `kubernetes` | `port_forward_strategy_uses_the_forwarder_local_endpoint`（fake forwarder 的本地地址优先；断言转发参数 = `("sandbox-1","agent-sandbox-system","router-abc",8080)`；仍注入路由头）、`port_forward_strategy_without_a_forwarder_is_a_configuration_error`（无 forwarder 明确报错且错误文本不含 pod IP；同步解析器同样报错）、`port_forward_forwarder_errors_are_propagated`（forwarder 错误原样透传）、`port_forward_router_pod_is_read_from_endpoints`（httpmock 断言 `GET /api/v1/namespaces/agent-sandbox-system/endpoints/sandbox-router-svc`）、`forwarded_endpoint_renders_http_and_brackets_ipv6`（含 `parse_endpoint_target_pod` 边界） |
| `fs_spec` | `projection_entry_is_written_when_enabled`、`projection_entry_is_omitted_when_disabled`、`projection_roots_are_configurable`、`execution_guard_is_retained`、`every_platform_spec_accepts_guard_and_projection_settings`（四类 spec 全覆盖）、`projection_payload_projects_the_include_roots`（临时目录 2 文件 → tar 首段 `AGENTS.md`，`file_count=2`） |

既有三策略优先级用例（`connection_strategy_derivation_matches_java_priority` 等 9 条）未改、仍全绿。`parse_gateway_address` 的既有断言也未改。

### 8.5 验证（实测）
```
cargo test -p agentscope-extensions-sandbox -j 2                 -> 188 passed / 0 failed / 0 ignored（EXIT=0；基线 177）
cargo clippy -p agentscope-extensions-sandbox --all-targets     -> 本 crate 两文件 0 warning（其余 warning 在他处，预先存在）
cargo fmt -p agentscope-extensions-sandbox                      -> 已格式化
```
> ⚠️ 环境性 flake（**与本次改动无关，已在未改动基线上复现**）：本 crate 的 httpmock 用例（`kubernetes::{persist_workspace_uses_the_file_api_when_enabled, hydrate_workspace_uploads_the_archive_then_extracts, file_transfer_uploads_and_downloads_under_the_base_dir}`、`e2b::{persist_tar_*, hydrate_tar_*}`）在**高频连续重跑 / 机器高负载**下偶发 `Transport("error sending request for url (http://127.0.0.1:*…)")`（`RetryPolicy` 不重试 Transport，故直接失败）。实测：用 `git show HEAD:` 临时还原两文件（真基线与首轮二进制均 177 tests）后连跑 5 次失败 1 次、单用例连跑 8 次失败 0 次；本版连跑亦为同类、失败用例每次不同且包含本次**未改动**的 `e2b` 用例 ⇒ 判定为 Windows 高负载下 `httpmock` 建连竞争，非代码缺陷；重跑即绿（已取得 188/0/EXIT=0 的完整绿跑）。→ **2026-10-30 二轮已定位真因（本机系统代理劫持 loopback mock 流量），见 §9.1；修复后连跑 68 次 0 失败**

### 8.6 反向验证（回退 → FAILED → 恢复 → 全绿）
| # | 回退内容 | FAILED 用例（实测） |
|---|---|---|
| 1 | `resolve_port_forward_base_url` 在未注入 forwarder 时改为回落 `http://<id>.<ns>.svc.cluster.local:<port>` | `kubernetes::tests::port_forward_strategy_without_a_forwarder_is_a_configuration_error`（`port_forward` 过滤集 3 passed / 1 failed） |
| 2 | `workspace_projection_entry` 恒返回 `None`（不写 `__workspace_projection__`） | `fs_spec::tests::projection_entry_is_written_when_enabled`、`projection_roots_are_configurable`、`projection_payload_projects_the_include_roots`（3 failed） |
| 3 | 宏内 `execution_guard()` 改为恒 `None` | `fs_spec::tests::execution_guard_is_retained`、`every_platform_spec_accepts_guard_and_projection_settings`（2 failed） |

恢复后：`cargo test -p agentscope-extensions-sandbox -j 2` → **188 passed / 0 failed（EXIT=0）**。

### 8.7 未完成项与原因
1. **真实 port-forward 实现**：本次只落地 seam（`KubernetesPortForwarder`）与解析链路（pod 发现 + 端口常量 + 本地 base URL 推导）；真正的隧道（kube-rs port-forward / `kubectl port-forward` 子进程）需真实集群验收，与本 crate「无真实端点联调」同类，记 §4.2。
2. **`Gateway` / `Endpoints` 仍是一次性读取**（无 watch），与 §7.5-2 一致。
3. **`PortForwardStrategy#close()` 的隧道生命周期未建模**：回收归 forwarder 实现方（trait 文档已写明幂等要求与生命周期归属）。
4. `K8sHelper` 的 list/watch/模板/port-forward 其余能力、`listSandboxClaims` 等仍在 §4.2 清单 (3)(4) 待办。→ **2026-10-30 二轮：list/get/delete/warmPool 已完成，见 §9.2；仅剩 `watch=true` 流式订阅与模板**

---

## 9. 2026-10-30 执行记录（二）— e2b httpmock flaky 稳定化 / K8s list·delete·warmPool / Gateway 等待

对应 §4.2 清单 **(2)(3)(4)**、§7.5-2 与 §8.5 记录的 flake。

### 9.1 任务 1：e2b / httpmock「0 of 1 expected requests matched」根因与修复

**诊断（先复现分布，再抓真实错误链，未凭猜测改代码）**
1. 复现分布：`--lib` 连跑 15 次失败 5 次；失败用例**不限于** §8.5 记录的 4 个，`kubernetes::tests::{file_transfer_uploads_and_downloads_under_the_base_dir, hydrate_workspace_uploads_the_archive_then_extracts}` 同样命中。共同点：**一个测试内连续两次请求同一个 mock server，第二个失败**（`a hits=1 b hits=0`，请求根本没到服务端）。
2. 真实错误链（临时把 reqwest 错误以 `{e:?}` 打印，未落盘、已还原）：`hyper_util::client::legacy::Error(SendRequest, hyper::Error(Io, Os { code: 10053, kind: ConnectionAborted }))` = **WSAECONNABORTED**，keep-alive 复用连接被中止。
3. 最小复现（临时 integration test：起 mock → 同 client 顺序两个 POST → drop，400 轮）：16/400 失败且全是第二个请求；失败时响应头出现 `proxy-connection: keep-alive` / `keep-alive: timeout=60`，而 `.no_proxy()` 客户端 **300/300 全绿**且这些头消失。
4. 环境定位：`HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings` 为 `ProxyEnable=1`、`ProxyServer=127.0.0.1:10808`、`ProxyOverride` 含 `127.*`。reqwest 0.13 的 `system-proxy` feature 走 `hyper-util::client::proxy::matcher` 读该注册表项，把 `ProxyOverride` 原样塞进 `NoProxy`（只做了 `*.`→`` 的替换），于是 `127.*` 成为**字面域名**、永远不匹配 `127.0.0.1` ⇒ loopback mock 流量全部进本地系统代理，代理回收空闲连接时给客户端 RST。
5. 结论（逐条排除题面怀疑方向）：**不是** httpmock 0.7 的端口复用 / `start_async` 与注册竞态（同一用例第一个请求必成功）；**不是** mock 在 `assert_async` 前被 drop；**不是** 生产 `RetryPolicy` 重试打空（`Transport` 不可重试）；**不是** `body_contains` 与 multipart 时序；**不是** `known_snapshots` 等全局状态。是**机器系统代理劫持 loopback**，故「回退到 `50ae343` 仍失败」「`--test-threads=1` 仍失败」都能解释。

**修复（生产代码，非测试绕过；未加 `#[ignore]`、未删/放宽任何断言）**
| 位置 | 改动 |
|---|---|
| `src/http.rs` | 新增 `is_loopback_target(value)`（接受 URL 或裸 `host[:port]`；`localhost` 忽略大小写与尾点、`127.0.0.0/8`、`::1`；`127.0.0.1.example.com`、`http://[::2]`、`http://10.0.0.5` 判否）与 `bypass_proxy_for_loopback(builder, targets)`：**任一**目标为 loopback 时 `builder.no_proxy()`，否则保留 ambient（环境变量 / 系统）代理；`client_builder(opts)` 抽出共用构建器，`build_client` 行为不变 |
| `src/agentrun.rs` / `src/daytona.rs` / `src/e2b.rs` / `src/kubernetes.rs` | 4 个后端构造 reqwest client 处接线：agentrun=`base_url`；daytona=`control_plane_base_url`+`toolbox_base_url`；e2b=`api_base_url`+`envd_sandbox_url`+`E2B_SANDBOX_URL` 覆盖值；kubernetes=`api_url`+`sandbox_api_url`+`gateway_address` |
| `src/lib.rs` | re-export 两个新函数 |

依据：Java `OkHttp` 的默认 `ProxySelector` 对本地地址一律 bypass（`<local>`），故「loopback 不走代理」是**对齐 Java 的行为**，同时修掉 port-forward / 本地 router 场景下真实用户会踩的同一个坑；非 loopback 流量仍走系统代理，外网行为不变。无新增依赖。

### 9.2 任务 2：K8s 列表 / 删除 / warmPool（`K8sHelper` + `SandboxClient` 等价物）

Java 依据（逐文件读取）：`client/internal/K8sHelper.java` 的 `listSandboxClaims(namespace, labelSelector)`（`withLabelSelector` 仅在非空白时生效，取 `items[].metadata.name`，保留服务端顺序）、`getSandboxClaim`（404 返 `null`）、`deleteSandboxClaim`（名空即 no-op、404 视为已删）；`client/SandboxClient.java` 的 `listActiveSandboxes`（`activeSandboxes` 的 `namespace/claimName` 键集，prune 非 active）、`deleteAll`（逐个删除、逐个吞错打日志）、`getSandboxClaimWarmPoolName`（claim 缺失抛 `SandboxNotFoundException`，无 `spec.warmPoolRef` 返 `null`）。

| 位置 | 改动 |
|---|---|
| `src/kubernetes.rs` 纯函数 | `parse_claim_names(&Value)`（保留顺序、跳过无 `metadata.name` 或空白名）、`claim_warm_pool_name(&Value)`（`spec.warmPoolRef.name`，空白视为无） |
| `src/kubernetes.rs` 后端方法 | `list_sandbox_claims(Option<&str>)`（`?labelSelector=`，经 `RequestBuilder::query` 正确编码）、`get_sandbox_claim`（404→`Ok(None)`）、`get_sandbox_claim_warm_pool_name`（缺失→`Runtime(WorkspaceStartError)`，正文逐字对齐 Java 消息 `SandboxClaim '<name>' not found in namespace '<ns>'.`）、`delete_sandbox_claim`（空名 no-op、404 容忍）、`list_active_sandboxes()`（有序键集）、`delete_all()`（全试一遍、取首个错误、**始终**清空注册表） |
| 注册表 | 新增字段 `active_claims: Arc<Mutex<BTreeSet<String>>>`；`create_sandbox` / `connect_sandbox` 记录、`kill_sandbox` / `delete_sandbox_claim` / `delete_all` 摘除（`kill_sandbox` 现复用统一的 `delete_claim_in`，行为不变：404 容忍） |
| 内部 | `claims_collection_path_in(ns)` / `claim_path_in(ns, name)`（`delete_all` 需按各键自带 namespace 删除）；`k8s_request_query(method, path, query, body)`（`k8s_request` 委托为空 query，行为不变） |

**语义差异（已记 §9.7）**：① Java `listActiveSandboxes` 会 prune「客户端句柄已失活」的条目，claim-only 后端没有该存活信号，改为「删除即摘除」；② `deleteAll` 在 Java 里走 `Sandbox#terminate()`（含关闭连接），Rust 等价物是「删 claim」（无客户端连接可关）。

### 9.3 `watch=true` 流式订阅：评估结论（已完成：设计决定，不迁移；见 §10.5-1）

- **Java 侧**：`K8sHelper.resolveSandboxName` / `waitForSandboxReady` 用 fabric8 `Watch`（长连接事件流）；`listSandboxClaims` **本身不用 watch**（一次性 `list()`），所以 K8s `list` 系列已完整。
- **Rust 侧现状**：就绪等待已用**轮询**实现（`wait_until_ready`，`READY_POLL_INTERVAL_MS=1500ms`，语义等价且已有测试），因此 `watch` 不是 list 的前置条件。
- **不做流式 `watch` 的理由**：① `KubernetesBackend::decode_json` 用 `resp.text()` 读完整个 body，而 `?watch=true` 的响应在 `timeoutSeconds` 前**不会结束**，当前请求助手直接卡死；② 要正确实现需 `bytes_stream()` 逐行分帧 + 取消语义 + 重连 `resourceVersion` 续订，属独立子系统；③ 本 crate 无宿主后台任务/取消句柄（见 §9.4 同类评估），长连接只能挂在调用栈上，容易泄漏。
- 结论：**已完成（设计决定，不迁移）**——不实现流式 watch，就绪等待用轮询（语义等价、已有用例）；若将来宿主提供后台任务生命周期，可作为新增能力再议（登记于 §4.2-5c 同类约束）。

### 9.4 任务 3：`Gateway` 地址等待与变更感知

- **Java 依据**（`internal/strategy/GatewayStrategy.java`）：`connect()` 首次 `get` 拿不到地址时，用 fabric8 `Watch` 等 `gatewayReadyTimeoutSeconds`（`GatewayConnectionConfig` 默认 180s），`DELETED` 事件报错；拿到后 `volatile baseUrl` **缓存**，`close()` 才清空。即 Java **不监听地址变更**，只等「地址出现」。
- **原 Rust**：`resolve_gateway_address()` 一次性 `get`，拿不到就报配置错误。
- **本次落地**：新增 `KubernetesBackend::wait_for_gateway_address()`（预算复用 `sandbox_ready_timeout_seconds`，默认 180s，与 Java 默认值一致；轮询间隔复用 `READY_POLL_INTERVAL_MS`）+ 可测的 `wait_for_gateway_address_with(budget, interval, fetch)`（超时返回 `Ok(None)`、fetch 错误原样上抛、`interval` 下限钳到 1ms 防忙等）；`connector_base_url` 的 `Gateway` 分支改用它，超时报 `Runtime(ConfigurationError)`，正文 `Timed out waiting for gateway address: <name>`（对齐 Java）。**未派生任何后台任务**，因此不需要取消句柄、无泄漏。
- **变更感知**：`connector_base_url` 每次调用都重解析（无 `baseUrl` 缓存），地址变化会在下一次 exec/文件 API 请求生效 —— 比 Java 的「缓存到 `close()`」更强，故**不引入 watch 任务**。
- **保留差异**：① `resolve_gateway_address` 对**读取失败**（如 403）仍返 `Ok(None)`，于是会等满预算再报超时；Java 会把 ApiException 直接抛出（快速失败）。② 无 `DELETED` 事件处理（轮询下表现为一直 `None` → 超时）。两项记 §9.7。

### 9.5 测试与验证（实测）

**新增 17 个离线用例（188 → 205）**
| 文件 | 用例 |
|---|---|
| `http` | `loopback_targets_are_recognized`（含裸 `127.0.0.1:10000`、`localhost:49983`、`[::1]`、`localhost.`）、`non_loopback_targets_are_rejected`（含 `127.0.0.1.example.com`、`localhost.evil.test`、`10.0.0.5`、`[::2]`、空串）、`proxy_bypass_only_applies_to_loopback_targets` |
| `kubernetes`（纯函数） | `parse_claim_names_skips_missing_and_blank_names`、`claim_warm_pool_name_reads_the_reference` |
| `kubernetes`（httpmock） | `list_sandbox_claims_returns_names_in_server_order`、`list_sandbox_claims_sends_the_label_selector`、`get_sandbox_claim_returns_none_on_404`、`get_sandbox_claim_warm_pool_name_reads_the_claim`、`get_sandbox_claim_warm_pool_name_errors_when_the_claim_is_missing`、`delete_sandbox_claim_tolerates_404_and_blank_names`、`active_claims_track_attach_and_forget_deletes`、`delete_all_removes_every_tracked_claim`、`delete_all_reports_the_first_failure_but_clears_the_registry` |
| `kubernetes`（等待语义） | `gateway_address_wait_polls_until_the_address_appears`（注入 fetch 序列，断言 3 次轮询）、`gateway_address_wait_gives_up_after_the_budget`（含错误上抛）、`gateway_without_a_published_address_times_out_with_the_java_message`（httpmock 断言轮询命中 ≥2 次） |

**判据：`cargo test -p agentscope-extensions-sandbox --lib` 连续 8 次全绿**
```
run 1: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.53s)
run 2: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.53s)
run 3: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.53s)
run 4: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.54s)
run 5: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.52s)
run 6: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.52s)
run 7: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.53s)
run 8: EXIT=0 failures_segment=False | ok. 205 passed; 0 failed; 0 ignored (1.53s)
```
（修复前同一二进制连跑 15 次失败 5 次；修复后另加跑 60 次 0 失败。）

```
cargo test -p agentscope-extensions-sandbox -j 2      -> 205 passed / 0 failed / 0 ignored（EXIT=0；基线 188）
cargo build -p agentscope-extensions-sandbox          -> 0 error（本 crate 无新 warning）
cargo fmt -p agentscope-extensions-sandbox -- --check  -> 通过
```

### 9.6 反向验证（回退 → FAILED → 恢复 → 全绿）
| # | 回退内容 | 结果（实测） |
|---|---|---|
| 1 | `bypass_proxy_for_loopback` 恒返回 `builder`（等于回到修复前） | 连跑 12 次 **11 次失败**，命中原 4 个 e2b 用例 + `kubernetes::{file_transfer_uploads_and_downloads_under_the_base_dir, delete_sandbox_claim_tolerates_404_and_blank_names, active_claims_track_attach_and_forget_deletes, delete_all_*}` ⇒ 直接证明修复的就是该根因 |
| 2 | `k8s_request_query` 丢弃 `?labelSelector=` | `kubernetes::tests::list_sandbox_claims_sends_the_label_selector` FAILED（204 passed / 1 failed） |
| 3 | `delete_all` 的键集改为空（等于不删任何 claim） | `delete_all_removes_every_tracked_claim`、`delete_all_reports_the_first_failure_but_clears_the_registry` FAILED（203 / 2） |
| 4 | `wait_for_gateway_address` 回退为一次性 `resolve_gateway_address` | `gateway_without_a_published_address_times_out_with_the_java_message` FAILED（204 / 1，用时 0.04s 说明确实没轮询） |

恢复后：`cargo test -p agentscope-extensions-sandbox --lib -j 2` → **205 passed / 0 failed（EXIT=0）**。

### 9.7 未完成项与保留差异
1. **`watch=true` 流式订阅**（claims / Gateway）：已完成（设计决定，不迁移，理由见 §9.3 / §9.4）；就绪等待为轮询，语义等价。
2. **Gateway 失败快慢**：CRD 读取错误（403 等）在 Rust 侧表现为「等满预算后超时」，Java 是快速失败；`DELETED` 事件未建模。
3. **`listActiveSandboxes` 的存活 prune** 无等价信号（改为删除即摘除）；`deleteAll` 无「关闭客户端连接」对应物。
4. **`SandboxTemplate`（模板）**：已完成（设计决定，不迁移）—— Java HEAD 中该名字只出现于异常类（`K8sHelper.java:350` 的 `SandboxTemplateNotFoundException`），无模板 CRD 读写逻辑；`TemplateNotFound`/`WarmPoolNotFound` 已由 `claim_failure` 快速失败覆盖（§10.1）。
5. **flaky 的环境依赖**：本机 `ProxyEnable=1` + `ProxyOverride` 含 `127.*` 才会触发；修复后与代理开关无关（loopback 恒不走代理）。CI 若有系统代理亦同样受保护。
6. **2026-10-31 三轮收口**：本清单 1/3/4 已给出裁定或等价实现（见 §10.1 / §10.5），新增未完成项为 **0**。

---

## 10. 2026-10-31 执行记录（三）— 三个 Java 增量提交 + 全量收口

对应 Java 参考仓库 `D:/IdeaProjects/agentscope-java` HEAD `1524b3191`（2026-10-30）之后的三个 sandbox 增量提交，并把 tracker 中所有仍未关闭的条目一次性收敛。

### 10.1 提交级对齐

| Java 提交 | 内容 | Rust 处置 |
|---|---|---|
| `19fdc9e6d` fix(sandbox): avoid session mirrors after sandbox release (#3039) | ① 已释放的 pinned sandbox 跳过 best-effort SessionTree mirror（`PinnedSandboxFilesystem.java` +213、`SandboxLifecycleMiddleware` 调 `markMirrorSandboxReleased`）；② `KubernetesSandbox#requireActiveConnection` 在 exec/persist/hydrate/uploadFile/downloadFile 前检查连接；③ 新增 `SandboxErrorCode.SANDBOX_CONNECTION_CLOSED`；④ 回归覆盖 | ②③④ **已完成**：`SandboxErrorCode::ConnectionClosed`（`as_str()="SANDBOX_CONNECTION_CLOSED"`）+ `RemoteSandbox::connection_closed_error()`（消息 `<Backend> sandbox connection has been closed`，Kubernetes 大小写逐字对齐 Java）+ `SandboxFileTransfer::close()`（默认空实现）+ `KubernetesFileTransfer::ensure_active()`（`upload_file`/`download_file` 首行检查，先于路径校验，顺序同 Java）+ `RemoteSandbox::shutdown` 关闭并清空原生传输。① **已完成（设计决定，不迁移）**：Rust harness 无 `PinnedSandboxFilesystem`（`rg PinnedSandbox crates/` → 0 命中），`SessionEntry`/`SessionTree` 已随双轨 session 删除（`crates/agentscope-harness/src/agent/memory/session.rs:4` 注释「retired … deleted」，`rg SessionTree crates/` 仅 2 处注释）⇒ 该缺陷前提不存在；已在 §4.2-5c 记下「将来引入 pinned filesystem 时需加同守卫」。用例：`remote::tests::released_sandbox_reports_a_closed_connection`、`kubernetes_files::tests::closed_transfer_reports_connection_closed_without_a_request` |
| `aa66abfd0` fix(sandbox-e2b): reliable exec timeout with server-side kill (#2976) | 每次 exec 用独立 client：`readTimeout=0`，总时长只由 `callTimeout` 拥有（旧 bug：小 base `readTimeout` 抢占 `callTimeout` 误报超时）；`Connect-Timeout-Ms` 服务端 kill；`deadline_exceeded` → `ExecTimeoutException`；`timeoutSeconds=0` 无界；负值快速失败 | **已完成**（前两轮已落 `Connect-Timeout-Ms`/`ExecTimeout`/deadline 降级；本轮补 `readTimeout=0` 等价物）：`E2bBackend.exec_client`（仅 connect timeout、无 client 级 total timeout）独占 exec；`deadline>0` 时 `RequestBuilder::timeout(T+5s)` 是唯一整体时限；`=0` 时真无界（对齐 `callTimeout(0)`）。负值：Rust 入参为 `u64`，类型层面不可能 ⇒ 无需运行时校验。用例：`e2b::tests::exec_is_not_bounded_by_the_base_read_timeout`（+ 既有 6 条 deadline/kill 用例） |
| `9730e8171` feat(e2b): support mounting pre-existing volumes at sandbox creation (#3581) | `E2bVolumeMount`、`volumeMounts` attach-at-create、`workspaceOnVolume`（persist/hydrate no-op、destroy 跳过 `rm -rf`）、嵌套挂载 tar `--exclude`、绝对 root 硬化、`E2bFilesystemSpec.volumeMount`、`WorkspaceMountSupport.tarExcludeArgsForAbsolutePaths` | **已完成**：`E2bVolumeMount`/`with_volume_mount`/`build_create_sandbox_body_with_mounts`（空即省略）/`workspace_on_volume`/`volume_tar_exclude_args`/`build_e2b_tar_command`（逐项 `shell_quote`）/相对 root 拒绝/`fs_spec::E2bFilesystemSpec::volume_mount`；`kill_sandbox` 本来只 `DELETE /sandboxes/{id}`、无 `rm -rf` ⇒ destroy 语义自然对齐；`mkdir -p` 用绝对路径，等价 Java「从 `/` 执行」。`WorkspaceMountSupport` 是 harness 工具类，等价能力在 crate 内（§4.2-5d）。`E2bSandboxState` 内嵌 mounts：Rust `RemoteSandboxStateData` 不携带 backend options（由宿主装配时提供），无需迁移 |

### 10.2 改动文件（本轮，均在允许范围内）

| 文件 | 改动 |
|---|---|
| `src/error.rs` | 新增 `SandboxErrorCode::ConnectionClosed`（`SANDBOX_CONNECTION_CLOSED`） |
| `src/e2b.rs` | `envs`/`with_env`/`build_create_sandbox_body_with_envs`；`workspace_on_volume`/`volume_tar_exclude_args`/`build_e2b_tar_command`；卷工作区 persist 空 marker、hydrate 空归档 no-op；相对 root 拒绝；`exec_client`（无 client 级 total timeout）+ exec 改用它；`live_*` 门控用例 |
| `src/kubernetes.rs` | options `claim_labels`/`shutdown_after_seconds`/`pod_labels`/`pod_annotations`；`build_claim_body_with_options`/`claim_lifecycle`/`ClaimFailure`/`claim_failure`/`select_pod_ip`；`create_sandbox` 发扩展 spec；`wait_until_ready` 条件快速失败；`live_*` 门控用例 |
| `src/remote.rs` | `connection_closed_error`（后端名首字母大写）；`shutdown` 关闭并清空原生传输；四处 `Error::Other("sandbox not started")` 统一为 `SANDBOX_CONNECTION_CLOSED` |
| `src/kubernetes_files.rs` | `SandboxFileTransfer::close()`（默认空实现）；`KubernetesFileTransfer.closed`/`is_closed`/`ensure_active`；closed 用例 |
| `src/fs_spec.rs` | `E2bFilesystemSpec::env` / `E2bFilesystemSpec::volume_mount`（对应 Java `E2bFilesystemSpec`） |

无新增依赖（`chrono`/`uuid` 均为既有 workspace 依赖，`Cargo.toml` **未改**）；未改根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。

### 10.3 新增用例（静态 205 → 227）

| 文件 | 用例 |
|---|---|
| `e2b` | `create_body_with_env_vars_matches_java`、`create_body_omits_env_vars_when_empty`、`with_env_rejects_blank_names`、`platform_create_sends_env_vars`、`workspace_on_volume_detects_mount_coverage`、`volume_tar_exclude_args_only_cover_nested_mounts`、`build_e2b_tar_command_quotes_excludes`、`persist_and_hydrate_are_noops_for_a_volume_backed_workspace`、`platform_create_sends_volume_mounts`、`exec_is_not_bounded_by_the_base_read_timeout`、`live_e2b_round_trip_when_a_real_api_key_is_configured`（门控） |
| `kubernetes` | `claim_body_carries_lifecycle_and_pod_metadata`、`claim_body_keeps_the_legacy_shape_without_extras`、`claim_lifecycle_rejects_zero_and_formats_iso_instant`、`claim_failure_maps_template_and_warm_pool_conditions`、`select_pod_ip_prefers_ipv4_and_skips_blanks`、`create_sandbox_sends_the_extended_claim_spec`、`create_sandbox_fails_fast_when_the_template_is_missing`、`live_kubernetes_round_trip_when_a_cluster_is_configured`（门控） |
| `remote` | `released_sandbox_reports_a_closed_connection` |
| `kubernetes_files` | `closed_transfer_reports_connection_closed_without_a_request` |
| `fs_spec` | `e2b_spec_builders_thread_envs_and_volume_mounts` |

门控用例写法（本 crate 约定）：读环境变量，缺失时 `eprintln!("skip: …")` + `return`，**不用 `#[ignore]`**（否则默认跑会漏掉、也无法在 CI 里一键全跑）。

### 10.4 Java 测试分支映射（抽样）

| Java 用例 | Rust 等价用例 |
|---|---|
| `E2bVolumeMountTest`（校验/归一化/distinct） | `volume_mount_validates_name_and_normalizes_path`、`require_distinct_mount_paths_rejects_duplicates`、`volume_mount_covers_workspace_root` |
| `E2bVolumeMountTest`（workspaceOnVolume + tar exclude） | `workspace_on_volume_detects_mount_coverage`、`volume_tar_exclude_args_only_cover_nested_mounts`、`build_e2b_tar_command_quotes_excludes` |
| `E2bVolumeMountLiveTest`（真实端点） | `live_e2b_round_trip_when_a_real_api_key_is_configured`（门控） |
| `E2bEnvdProcessClientSocketTest`（deadline/kill/0=无界） | `exec_timeout_arms_the_server_side_kill_and_reports_an_error`、`exec_without_deadline_does_not_arm_the_kill`、`exec_is_not_bounded_by_the_base_read_timeout`、`drain_envd_stream_maps_deadline_exceeded_to_an_exec_timeout` |
| `KubernetesSandboxTest`（closed connection） | `kubernetes_files::tests::closed_transfer_reports_connection_closed_without_a_request`、`remote::tests::released_sandbox_reports_a_closed_connection` |
| `KubernetesSandboxColdRestoreTest` | `kubernetes::tests::create_sandbox_fails_fast_when_the_template_is_missing`、`create_sandbox_sends_the_extended_claim_spec` |
| `PinnedSandboxFilesystemTest`（mirror 跳过） | **无（前提不存在）** → §10.1 ① |

### 10.5 保留差异（已裁定，非未完成）

1. `watch=true` 流式订阅不实现（理由：`decode_json` 全量读 body + 无宿主后台任务/取消句柄，§9.3）；就绪等待用轮询，语义等价。
2. Gateway 读取失败（403 等）在 Rust 表现为「等满预算后超时」，Java 快速失败；无 `DELETED` 事件处理（§9.4）。
3. `listActiveSandboxes` 无存活 prune（改为删除即摘除）；`deleteAll` 无「关闭客户端连接」对应物（§9.2）。
4. E2B 快照保留仅进程内可见（E2B 无列表端点）；Proto codec 不迁移（§3.2）。
5. `SandboxFilesystemSpec`/`SandboxFileTransfer` 为本 crate 自建（harness 无对应物，§4.2-2）；原生传输需 `from_remote_sandbox(...)` 显式接线（§4.2-5a）。
6. 真实端点冒烟需人工设环境变量执行（默认 skip），不纳入必跑基线。

### 10.6 验证状态

- **本会话按任务声明未执行** `cargo build` / `cargo test`（由主智能体统一验证）；静态统计：12 个源文件 **12054 行 / 227 test 标注**（§1.1）。已用 `rustfmt --edition 2021 --check` 验证六个改动文件**语法可解析且格式干净**（EXIT=0；`rustfmt` 不能代偿类型检查）。
- 待主智能体执行：`cargo build -p agentscope-extensions-sandbox`、`cargo test -p agentscope-extensions-sandbox -j 2`、`cargo fmt -p agentscope-extensions-sandbox -- --check`，并按 §10.7 反向验证。

### 10.7 反向验证设计（待主智能体执行）

| # | 回退内容 | 预期 FAILED |
|---|---|---|
| 1 | `E2bBackend::exec_in` 改回 `self.client`（带 `read_timeout_seconds`） | `e2b::tests::exec_is_not_bounded_by_the_base_read_timeout` |
| 2 | `build_create_sandbox_body_with_envs` 不写 `envVars` | `e2b::tests::platform_create_sends_env_vars`、`create_body_with_env_vars_matches_java` |
| 3 | `workspace_on_volume` 恒 `false` | `e2b::tests::persist_and_hydrate_are_noops_for_a_volume_backed_workspace`、`workspace_on_volume_detects_mount_coverage` |
| 4 | `create_sandbox` 回到 `build_claim_body(...)`（丢扩展 spec） | `kubernetes::tests::create_sandbox_sends_the_extended_claim_spec` |
| 5 | `KubernetesFileTransfer::ensure_active` 改为恒 `Ok(())` | `kubernetes_files::tests::closed_transfer_reports_connection_closed_without_a_request` |
| 6 | `RemoteSandbox::shutdown` 不调 `transfer.close()` | `remote::tests::released_sandbox_reports_a_closed_connection` |




