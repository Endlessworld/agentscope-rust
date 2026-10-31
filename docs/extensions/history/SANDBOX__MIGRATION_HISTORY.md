# SANDBOX 迁移执行历史（归档）

> 本文件是 [`../SANDBOX__GAP_TRACKER.md`](../SANDBOX__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 2026-10-28 执行记录（P1 接线修复） · 2026-10-29 执行记录（harness 侧沙箱状态持久化 / 快照身份） · 8. 2026-10-30 执行记录 — PortForward / FilesystemSpec guard+projection · 9. 2026-10-30 执行记录（二）— e2b httpmock flaky 稳定化 / K8s list·delete·warmPool / Gateway 等待 · 10. 2026-10-31 执行记录（三）— 三个 Java 增量提交 + 全量收口）。

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




