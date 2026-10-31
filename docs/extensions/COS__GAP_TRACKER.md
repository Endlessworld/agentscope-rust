# COS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **COS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-cos\`
> **Java 子模块**：单模块
> **Rust 目标**：`crates/agentscope-extensions-cos/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-cos` |
| Java 子模块 | 单模块 |
| Java 规模（实测 2026-10-22） | 主代码 **882 非空行**（5 个类 / 971 总行 / 709 去空去注释行）；测试 **760 非空行**（4 个测试类 / 898 总行 / 62 个 `@Test`） |
| Rust 规模（实测 2026-10-28 第二轮） | 13 个 `src/*.rs` / **5241 非空行**（5823 总行）；无 `tests/` 目录；**105** 内联 test（58 个 `#[test]` + 47 个 `#[tokio::test]`，其中 2 个为 multi-thread flavor）+ 3 个 `no_run` doctest（**只编译、不执行**） |
| Rust crate | `crates/agentscope-extensions-cos` |
| 迁移波次 | B |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 105 passed / 0 failed，另 3 个 `no_run` doc）—— 真实 COS HTTP 适配器已落地：`CosHttpObjectStore` + 自建 `q-sign-algorithm=sha1` 签名（金标向量钉住）+ 分段上传 / 预签名 URL / STS token；P2 余项为 core 跨模块（`AgentStateStore::delete`）与实例隔离性说明（见 §4.2 / history/COS__MIGRATION_HISTORY.md） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `CosAgentStateStore` | COS 对象存储上的 `AgentStateStore` | `CosAgentStateStore`（implements `agentscope_core::state::AgentStateStore`） | key 布局对齐：`{prefix}{user}/{session}/{key}.json` / `.list.json` / `.list.hash`；anonymous 用户段 `__anon__`；前缀归一化对齐。**缺口**：Java 额外覆写了 `delete(userId, sessionId, key)`（删 `.json`+`.list.json`+`.list.hash`），Rust core `AgentStateStore` trait **无该方法** → 无法实现（core 层缺口，见 history/COS__MIGRATION_HISTORY.md P2-①）；Java `close()` 关闭 `COSClient`，Rust 无 `close`（trait 无） |
| `CosBaseStore` | COS 上的 `BaseStore`（远程 KV） | `CosStore`（implements `agentscope_harness::filesystem::remote::store::BaseStore`） | key 布局：`{prefix}{ns0}/{ns1}/.../{key}.json` / `.version`；version 成功写入 +1；CAS 语义以 harness 为准。**对齐**：Java `CosBaseStore` **未**覆写 `supportsAtomicSessionStorage()`（默认 `false`），Rust `CosStore` 亦未覆写 `supports_atomic_session_storage()`（默认 `false`）→ 一致。**强化**：Rust `put`/`put_if_version`/`delete` 走进程内 `tokio::Mutex` 串行化（Java 无锁，read-check-write 有竞态） |
| `CosDistributedStore` | 组合 agent state / base store / snapshot spec | `CosDistributedStore` | Java 含 `agent_state_store`/`base_store`/`sandbox_snapshot_spec`；Rust 版额外含 `sandbox_execution_guard`，并可 `to_harness_distributed_store()` 投影（harness 结构体字段不足，见汇总 §2.2）。**关键差异**：Java `CosDistributedStore` **未**覆写 `sandboxExecutionGuard()`（继承默认 `noop()`），Rust 版提供**真实** `CosSandboxExecutionGuard`（harness 进程内计数 guard）→ Rust 语义**强于** Java，非缺失。**缺口**：Java `agentStateStore()`/`baseStore()`/`sandboxSnapshotSpec()` 每次调用**新建**实例；Rust 版在 `create()` 时**一次性构造并缓存**（`Arc`）→ 语义等价但少了「每次新实例」的隔离性（无害，见 history/COS__MIGRATION_HISTORY.md P2-③） |
| `CosRemoteSnapshotClient` | COS 上的 `RemoteSnapshotClient` | `CosRemoteSnapshotClient`（implements `...snapshot::RemoteSnapshotClient`） | 对象 key：`{prefix}{snapshotId}.tar`；snapshot id 安全校验复用 harness `validate_snapshot_id` 语义。**差异（Rust 更严）**：Java 仅拒绝 blank/null `snapshotId`，Rust 额外拒绝 `/`、`\`、`..`、`\0`；Java `RemoteSnapshotSpec.build` 不校验，Rust 经 harness `RemoteSnapshotSpec::build` 校验 → Rust 是**安全加固**，非缺失（见 history/COS__MIGRATION_HISTORY.md P2-④） |
| `CosSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `CosSnapshotSpec`（implements `...snapshot::SandboxSnapshotSpec`） | 复用 harness `RemoteSnapshotSpec` 包装，与 Java 继承 `RemoteSnapshotSpec` 等价 |
| （Java 无独立类） | 沙箱执行互斥 | `CosSandboxExecutionGuard` | 包装 harness `SandboxExecutionGuard`，保留共享 object store 作为（未来）分布式协调接缝 |
| （Java 依赖 `com.qcloud.cos.COSClient`） | 对象存储后端 | `CosObjectStore`（async trait）+ `CosHttpObjectStore`（**真实后端**）+ `InMemoryCosObjectStore`（测试） | 见 §2.1、§3.2、§4.3 |
| `com.qcloud.cos.transfer.TransferManager`（SDK；Java 模块本身**未使用**，只用 `COSClient.putObject`） | 大对象分块上传 | `CosHttpObjectStore::put_multipart` + `put` 阈值自动切换；分片规划在 `multipart.rs` | 规则**逐条对齐 SDK 5.6.245**（`TransferManagerUtils.shouldUseMultipartUpload` / `calculateOptimalPartSize` / `createDefaultExecutorService`）+ COS 服务端限制；见 §3.3、§4.4 |
| `COSClient.generatePresignedUrl`（SDK；Java 模块**未使用**） | 预签名 URL | `presign_get` / `presign_put`（`presign.rs`，复用 `signature::sign_request`） | 同一签名算法，`q-sign-time` 控制有效期；只签 `host`；见 §3.3 |
| STS 临时密钥（`x-cos-security-token`） | 临时凭证 | `CosCredentials::with_security_token` / builder `.security_token()` / `with_credentials()` | header 与预签名 query 双通道；**token 刷新由调用方负责**（不自动刷新），见 §3.3 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.qcloud.cos:COSClient`（腾讯云 COS SDK） | `getObjectMetadata`/`getObject`/`putObject`/`deleteObject`/`deleteObjects`/`listObjects` | `CosHttpObjectStore`：`reqwest` + 自建 COS v5 签名（`signature.rs`） | **Java 用官方 SDK、Rust 手写签名**（差异已记录 §4.3）。`get`=GET、`put`=PUT（**超过阈值转 multipart**）、`exists`=HEAD、`delete`=DELETE、`list_page`=`GET /?list-type=2&prefix=&continuation-token=&max-keys=`、`delete_many`=`POST /?delete`（1000/批 + `Content-MD5`）。测试用 `InMemoryCosObjectStore`，HTTP 层用 `httpmock` 离线断言。**2026-10-22 的「`q-sign-algorithm` 零命中」结论已失效**（当时属实，现已实现）。**Rust 额外（Java 模块未用）**：`POST ?uploads`/`PUT ?partNumber&uploadId`/`POST ?uploadId`/`DELETE ?uploadId` 分段上传、预签名 URL —— 参考 Java **SDK** 的 `TransferManager`/`generatePresignedUrl`，不引新依赖 |
| `io.agentscope.core.state.AgentStateStore` | 状态存储 trait | `agentscope_core::state::AgentStateStore` | §2.1 裁定：实现它，不重建 |
| `io.agentscope.core.state.ListHashUtil` | list 增量 hash | `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`） | **已收敛（2026-09-19，2026-10-22 静态复核确认）**：删除本 crate 平行实现，`save_list` 直接委托 core；`sha2` 依赖已从 `Cargo.toml` 移除。详见 §2.2 / §3.2 / §4.1。**注意**：core 的 `compute_hash` 用 `std::collections::hash_map::DefaultHasher`（SipHash-1-3）而非 Java `String.hashCode()`，故与 Java 侧 hash 值**永不相等**（见 §2.2 新增行） |
| `io.agentscope.core.util.JsonUtils` | JSON 编解码 | `serde_json` | |
| `harness...remote.store.BaseStore` / `StoreItem` | 远程 KV trait | `agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue}` | 实现既有 trait |
| `harness...sandbox.snapshot.{RemoteSnapshotClient,RemoteSnapshotSpec,SandboxSnapshotSpec}` | 快照抽象 | 同名 harness trait | 实现/复用，不重建 |
| `harness...sandbox.SandboxExecutionGuard` | 执行互斥 | 同名 harness 类型 + 本 crate `CosSandboxExecutionGuard` 包装 | |
| `agentscope.harness.agent.DistributedStore` | 组合包 | `agentscope_harness::agent::DistributedStore` | 字段不足（仅 `agent_state_store`+`isolation_scope`）；本 crate 用自有组合 + 投影函数规避。**2026-10-22 复核**：Java `DistributedStore` 是 **interface**（含 `agentStateStore`/`baseStore`/`sandboxSnapshotSpec`/`sandboxExecutionGuard`/`messageBus`/… 9 个方法），Rust harness 侧是 **struct**（仅 2 字段）→ 缺口属实，需汇总方扩展 harness |

### 2.2 list 哈希与增量追加（core 收敛后的事实）

| 项 | 内容 |
|---|---|
| 唯一实现 | `agentscope_core::state::list_hash` — `compute_hash` / `has_changed` / `needs_full_rewrite` / `EMPTY_HASH` / `SAMPLING_THRESHOLD`，`agentscope_core::state` 已 `pub use`（`state/mod.rs:19`） |
| 本 crate 用法 | `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite};`（`agent_state_store.rs:15`） |
| 已删除 | 本地 `fn compute_list_hash`（全量 SHA-256，length-prefixed JSON）与本地 `fn needs_full_rewrite`（**已由 `git show e01809d:.../agent_state_store.rs` 复核确认历史实现确为 `!compute_list_hash(values).starts_with(h)`**） |
| 已删除依赖 | `sha2`（经 `rg` 确认仅旧 `compute_list_hash` 使用，无其他签名/加密用途；`crates/agentscope-extensions-cos/Cargo.toml` 现无 `sha2`；全 workspace 仅 core/controlplane/channel/jdbc/mongodb/rag/harness 仍用 `sha2`，与 COS 无关） |
| 存储布局（不变） | `{prefix}{user}/{session}/{key}.list.json` 存列表实体；`.list.hash` 存**全量**列表的 core 采样哈希（用于下次的「前缀比较」） |
| `existing_count` 语义 | = 已存列表**实体**的元素数，**与是否存在 hash 对象解耦**（Java `existingCount` 同义）；旧代码仅在 `stored_hash.is_some()` 时才读列表长度，会令 core 的 Case 2 永久不可达 |
| 跨语言 hash 值 | **不兼容（确认）**：Java `computeHash` = `"size:N;"` + `"i:item.hashCode(),"` → `Integer.toHexString(sb.hashCode())`（**Java `String.hashCode`**，32 位）；Rust core `compute_hash` = 同构采样串 → `DefaultHasher`（**SipHash-1-3，64 位**）。两者算法骨架同源、**哈希函数不同** → 同一列表两侧 hash 值**永不相等**。存量自愈路径见 §4.2 第 5 条 |
| ⚠️ 语义偏离（残留 P1） | core `needs_full_rewrite` Case 3 比较的是「**当前列表 `existing_count` 前缀**的哈希」与「**存储的整表哈希**」。首次写入时 `stored_hash` 存的是**整表**哈希，故第 2 次写入（`existing_count` = 整表长）时前缀 = 整表 → 相等 → 追加判定正确；但**纯追加后**若再追加一次，`stored_hash` 是上一次的整表哈希，而前缀只取到旧长度 → **必然不等 → 判全量重写**。即 **core 与 Java 同款「第二次追加仍全量重写」的行为问题**（Java `ListHashUtil.needsFullRewrite` 同样如此）。本 crate 已忠实委托 core，故与 Java 行为**一致**，非本 crate 缺陷（core 层议题，见 history/COS__MIGRATION_HISTORY.md P1-①） |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + 依赖 | pom.xml | 已完成 | 仅改本 crate `Cargo.toml`；新增 `sha2`（workspace 已有）——**已于 2026-09-19 随 list_hash 收敛移除**；`tokio` 增 `rt-multi-thread` feature；未改根 `[workspace.dependencies]` |
| list 增量哈希收敛到 core | `ListHashUtil.needsFullRewrite` | 已完成 | **2026-09-19**：删除本地 `compute_list_hash`（全量 SHA-256）+ 本地 `needs_full_rewrite`，改 `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite}`；`save_list` 调用点同步收敛；`existing_count` 改为「已存列表实体长度」，与 hash 对象解耦；`sha2` 依赖移除（grep 确认无其他用途）。验证（2026-09-19 当轮）：**31 unit test passed / 0 failed**（19 × `#[test]` + 12 × `#[tokio::test]`）+ 2 个 `no_run` doctest（只编译不执行）。**已完成（2026-10-29 勘误）**：原记「31 unit test」，实际现为 **105 passed / 0 failed**（58 `#[test]` + 47 `#[tokio::test]`）+ 3 个 `no_run` doctest；依据 §1 / §4.1 / §4.4（2026-10-28 第二轮 `cargo test` 实测）。**2026-10-22 静态复核确认**：`rg 'fn compute_list_hash\|fn needs_full_rewrite'` 全 crate → 零命中；`Cargo.toml` 无 `sha2`（`git log -S "sha2"` → 提交 `daa700d`）→ 收敛属实 |
| 修复：纯追加不再恒判全量重写 | `ListHashUtil.needsFullRewrite` Case 3 | 已完成 | 旧条件 `!compute_list_hash(values).starts_with(stored_hash)` 依赖「全量 SHA-256 具有前缀性质」——**实测该假设错误**（`sha256([a])` 与 `sha256([a,b])` 互不为前缀），故条件恒 `true`，纯追加永远判需全量重写（数据无损，追加优化失效）。已实测旧实现 `old_needs_full_rewrite([a,b], old_hash([a]), 1) == true`；新实现 `needs_full_rewrite == false`（core 取 `existing_count` 前缀哈希比较）。**2026-10-22 静态复核确认**：该回归断言现以 `agent_state_store.rs::pure_append_does_not_trigger_full_rewrite` 内联形式保留（同时断言旧 `!whole.starts_with(stored_hash)` 恒为 `true`）。**注意**：core 只修到「单次追加不判重写」，**连续第二次追加**仍会因「存整表哈希 vs 比前缀哈希」而判全量重写（与 Java 同款，见 §2.2 P1-①） |
| 修复：`stored_hash == None` 误判 | `ListHashUtil.needsFullRewrite` Case 2 | 已完成 | 旧实现 `None => true`；core 语义为 `stored_hash.is_none() && existing_count > 0` 才 `true`，即无既有数据（`existing_count == 0`）时**不**重写 |
| 对象存储后端抽象 | `COSClient` | 已完成 | `CosObjectStore` async trait（`get/put/delete/exists/len/list_prefix/list_page/delete_many/close` + UTF-8 便捷方法）；`InMemoryCosObjectStore`（测试）+ **`CosHttpObjectStore`（真实，2026-10-28）** |
| key 布局 / 前缀归一化 | `CosAgentStateStore`/`CosBaseStore` 私有方法 | 已完成 | `layout.rs`：`normalize_prefix` / `session_prefix` / `namespace_prefix` / `normalize_user` / `validate_snapshot_id` |
| `AgentStateStore` 实现 | `CosAgentStateStore` | 已完成 | `CosAgentStateStore`（builder + `Arc<dyn>` 便捷函数）；`exists`/`delete`/`list_session_ids` 基于 prefix list |
| `BaseStore` 实现（CAS/版本） | `CosBaseStore` | 已完成 | `CosStore`；version 从 0 起、成功写入 +1；`put_if_version(ns,key,v,0)`=仅当不存在时写入；写路径 async mutex 串行化 |
| 快照 client + spec | `CosRemoteSnapshotClient`/`CosSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`；spec 包裹 harness `RemoteSnapshotSpec` |
| 沙箱执行 guard | （Java 无） | 已完成 | `CosSandboxExecutionGuard` 包装 harness guard |
| 组合者 | `CosDistributedStore` | 已完成 | `CosDistributedStore::create` + 子前缀 `state/`/`store/`/`snapshot/` + `to_harness_distributed_store()` |
| · 单元测试 | 760 行测试 | 已完成 | **105** lib unit tests（58 × `#[test]` + 47 × `#[tokio::test]`）+ 3 个 `no_run` doctest（只编译不执行）。分布：`agent_state_store` 17、`base_store` 9、`http_object_store` 33、`distributed_store` 6、`snapshot` 5、`guard` 3、`object_store` 4、`layout` 5、`signature` 9、`multipart` 8、`presign` 6。**对照 Java 62 个 `@Test` → 数量已超（105 vs 62）**，且新增了 Java 测试里没有的 HTTP/签名/分段/预签名层断言 |

### 3.2 后续（P1/P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实 COS 适配器 | `COSClient` 构造 | **已完成（2026-10-28）** | `CosHttpObjectStore`（`src/http_object_store.rs`）+ `src/signature.rs`（纯函数签名）。**Java 用官方 SDK `com.qcloud:cos_api:5.6.245`、Rust 手写 COS v5 签名**（见 §4.3）。依赖：`reqwest`（workspace）、`hmac 0.12`、`sha1 0.10`、`quick-xml 0.39`、`md5 0.8`、`base64`（workspace）；dev-dep `httpmock 0.7`。**能力边界**：`get`/`put`/`exists`/`delete`/`list_page`（`list-type=2` + `continuation-token` + `max-keys`，1000 上限）/`list_prefix`（自动翻页）/`delete_many`（`POST /?delete`，1000/批 + `Content-MD5`）/`close`；404→`Ok(None)`（`get`）/`false`（`exists`）/`Ok(())`（`delete`），403→`CosError::Service{status:403}` 可经 `is_signature_error()` 与 404 区分。**分段上传 / 预签名 URL / STS 已于 2026-10-28 第二轮落地**（§3.3 / §4.4）。**仍未实现**：`select`/`inventory` 等高级 API；断点续传（`ListParts` 恢复）、上传进度回调、跨进程分布式锁 |
| ~~`ListHashUtil` 精确对齐~~ | `ListHashUtil` | **已完成（2026-09-19）** | 已收敛到 `agentscope_core::state::list_hash`，本地平行实现删除，详细见 §2.2 / §3.1 / §4.1。**残留 PS2**：`.list.hash` 不再与 Java 旧值二进制兼容（core 采样策略 vs 旧 SHA-256；且 core 用 `DefaultHasher`（SipHash-1-3）而 Java 用 `String.hashCode()` → 两侧值**永不相等**，见 §2.2「跨语言 hash 值」行）；跨语言共读同一 bucket 时首次写入会自愈（见 §4.2 第 5 条） |
| 分布式跨进程执行互斥 | （Java 亦为进程内） | 设计决定（不迁移） | harness guard 为进程内；跨进程协调未在 Java 实现，Rust 亦不做 |

### 3.3 2026-10-28 第二轮：分段上传 / 预签名 URL / STS

> Java 模块本身**只用** `COSClient.putObject`/`getObject`/…（单次 PUT 上限 5 GiB），既无分段也无预签名代码。下列规则取自 **Java 侧依赖的官方 SDK 5.6.245**（`~/.m2/.../cos_api-5.6.245.jar` 字节码）与 COS 官方 API 文档，不引新依赖。

#### 3.3.1 常量与规则来源

| 常量（`multipart.rs`） | 值 | 来源 |
|---|---|---|
| `DEFAULT_MULTIPART_THRESHOLD` | **5 MiB**（5 242 880） | `TransferManagerConfiguration.<init>` → `multipartUploadThreshold = 5242880l` |
| `DEFAULT_MULTIPART_PART_SIZE` | **5 MiB** | 同上 → `minimumUploadPartSize = 5242880l` |
| `MIN_MULTIPART_PART_SIZE` | **1 MiB** | COS《Upload Part》：“每个分块大小为 1MB - 5GB，最后一个分块可以小于 1MB” |
| `MAX_MULTIPART_PART_SIZE` | **5 GiB** | 同上 |
| `MAX_MULTIPART_PARTS` | **10 000** | 同上（“最多支持 10000 分块”） |
| `DEFAULT_MULTIPART_CONCURRENCY` | **1** | `TransferManagerUtils.createDefaultExecutorService` → `newFixedThreadPool(1, ThreadFactory)` |
| `MAX_PRESIGN_EXPIRE_SECS` | **604 800**（7 天） | COS 签名有效期上限；客户端默认仍为 `DEFAULT_SIGN_EXPIRE_SECS = 900` |

| 行为规则 | Java SDK 参考 | Rust 实现 |
|---|---|---|
| 何时走分片 | `TransferManagerUtils.shouldUseMultipartUpload`：`len >= threshold`（`lcmp; iflt`） | `put`：`len as u64 >= multipart_threshold` |
| 分片大小 | `TransferManagerUtils.calculateOptimalPartSize`：`max(ceil(len / 10000), minimumUploadPartSize)` | `multipart::plan_parts`：`configured.max(len.div_ceil(10000))` |
| 分片编号 / 乱序 | COS《Upload Part》：`partNumber ∈ 1..=10000`，可乱序上传 | `MultipartPlan::range` 按 1-based 编号切 `[start, end)` |
| 完成顺序 | COS《Complete》：乱序报 `400 InvalidPartOrder` | 上传完成后 `etags.sort_by_key(part_number)` 再拼 XML |
| 失败回滚 | COS《Complete》：“建议及时完成或舍弃” | 任一分片/完成失败 → `DELETE ?uploadId=..`；仅记录 abort 失败不掩盖原错误 |
| 发号失败不回滚 | —（无 `UploadId` 可发号） | `initiate` 失败直接返回，不发 abort |

#### 3.3.2 API 清单

| 项 | 签名 | 说明 |
|---|---|---|
| `put_multipart` | `async fn(&self, key: &str, data: &[u8]) -> Result<()>` | 显式分片（绕过阈值）；空体 / 分片尺寸非法 → `CosError::Config`，不发起任何请求 |
| `put` | trait 方法（`CosObjectStore`，HTTP 后端覆写） | 大于等于阈值转 `put_multipart`；空体 / 小于阈值走单次 `PUT` |
| `presign_get` / `presign_put` | `fn(&self, key: &str, expire_secs: Option<u64>) -> Result<String>` | **不发网络请求**；`None` 用 `sign_expire_secs`；只签 `host` |
| `with_credentials` | `fn(&self, CosCredentials) -> Self` | STS 轮换：共享 `reqwest::Client` 与 `closed` 标志 |
| builder | `multipart_threshold` / `multipart_part_size` / `multipart_concurrency` / `security_token` | 构建时校验（分片尺寸区间、并发 ≥ 1） |

#### 3.3.3 URL 形态与签名复用

- 分段上传四步：`POST /{key}?uploads` → `PUT /{key}?partNumber=N&uploadId=..`（响应头 `ETag`，去引号后重引用写回完成 XML）→ `POST /{key}?uploadId=..`（`<CompleteMultipartUpload><Part><PartNumber>n</PartNumber><ETag>".."</ETag></Part>…`）→ 失败时 `DELETE /{key}?uploadId=..`（`404 NoSuchUpload` 视为已舍弃）。
- 预签名：`{base}/{encoded key}?[业务参数&]q-sign-algorithm=sha1&q-ak=..&q-sign-time=..&q-key-time=..&q-header-list=host&q-url-param-list=..&q-signature=..`；**直接复用** `signature::sign_request` 返回的 `authorization`（不复制算法），`presign.rs` 只拼 URL。路径“签原文 / 发编码”，与请求签名一致。
- STS：普通请求 `x-cos-security-token` 进签名头列表；预签名把它当 **query 参数**并进入 `q-url-param-list`（接收方无需额外 header）；token 空串/全空白视为无 token。
- **能力边界**：分片上传的 `Content-MD5` 未发送（COS 文档标可选）；不支持 `ListParts` 断点续传、上传进度回调、分片级重试（失败即整体回滚）、流式上传（内存峰值约 2× 对象大小：整体 `Arc<Vec<u8>>` + 单片副本；单次 `PUT` 路径为 1×）；预签名不支持 `response-*` 以外的自定义头签名（只签 `host`）。快照 `.tar` 上传走 `CosObjectStore::put`，因此自动获得阈值切换。

## 4. 项目状态与下一步
### 4.1 当前状态
**已完成**：COS 模块全部产出契约落地并复用既有 trait；list 增量哈希已收敛到 core（无平行实现）。**2026-10-29 复核**：测试数 **105**（§1 / §4.4 一致）；§3.1 旧记「31」已就地勘误（历史原文保留）；真实剩余见 §4.2（core `needs_full_rewrite` Case 3 与 Java 同款、分片上传无 `ListParts` 断点续传/分片级重试/进度回调、STS 不自动刷新、预签名只签 host、根 `object_store` 依赖无引用）。**2026-10-31 收口**：原列的「core trait 无 per-key `delete`」已不成立——core `AgentStateStore` 已有 `delete_key` 默认实现且本 crate 覆写之（`close()` 仍无 trait 对应，属设计决定）；`CosError::NotFound` 已接线（§5）。
- 文件（**13 个**，与 §1 规模行一致）：`src/{lib,error,layout,object_store,http_object_store,signature,multipart,presign,agent_state_store,base_store,snapshot,guard,distributed_store}.rs`
  - 非空行分布（2026-10-28 第二轮）：`http_object_store` 2166、`agent_state_store` 686、`signature` 479、`base_store` 382、`object_store` 257、`distributed_store` 234、`multipart` 230、`presign` 217、`layout` 170、`snapshot` 150、`guard` 134、`lib` 76、`error` 60 → **合计 5241**（总行 5823）
- 验证命令与结果（**2026-10-28 第二轮重跑**）：
  - `cargo test -p agentscope-extensions-cos --no-run` → **通过**（exit 0；本 crate 无错误/无告警）
  - `cargo test -p agentscope-extensions-cos` → **通过**：**105 lib unit tests + 3 doctests / 0 failed / 0 ignored**
    - 105 = **58 个 `#[test]` + 47 个 `#[tokio::test]`**（逐文件 `plain+tokio`：`agent_state_store` 7+10、`base_store` 7+2、`http_object_store` 5+28、`object_store` 1+3、`distributed_store` 5+1、`snapshot` 2+3、`signature` 9+0、`layout` 5+0、`guard` 3+0、`multipart` 8+0、`presign` 6+0）；`base_store` 的 2 个为 `#[tokio::test(flavor = "multi_thread")]`；3 个 doctest 均为 **`no_run`**（只编译不执行）
  - `cargo clippy -p agentscope-extensions-cos --all-targets` → 本 crate **零告警**（仅 core/model/harness 既有告警）；`cargo fmt -p agentscope-extensions-cos -- --check` → **通过**（exit 0）
    - 顺带清掉一处本轮之前遗留的 test 模块 unused import（`distributed_store.rs` 的 `SandboxGuard`；`dyn SandboxGuard` 上的方法无需导入 trait 即可调用）
  - `rg "fn compute_list_hash|fn needs_full_rewrite|starts_with\\(h\\)" crates/agentscope-extensions-cos` → **无结果**（exit 1，平行实现已清空）——**2026-10-22 复核重跑仍为 exit 1，确认属实**
    - **2026-10-22 静态复核修正**：31 个 unit test 中 **19 个 `#[test]` + 12 个 `#[tokio::test]`**（按 `#[test]` / `#[tokio::test]` 逐文件统计：`agent_state_store` 5+7、`base_store` 5+0、`distributed_store` 4+1、`guard` 3+0、`snapshot` 2+3、`object_store` 0+1 → 19+12=31）；2 个 doctest 均为 **`no_run`**（`lib.rs:27`、`agent_state_store.rs:32`）→ **只编译不执行**，故「33 passed」的表述应为「31 实际执行 + 2 仅编译」
  - `rg "fn compute_list_hash|fn needs_full_rewrite|starts_with\(h\)" crates/agentscope-extensions-cos` → **无结果**（exit 1，平行实现已清空）——**2026-10-22 复核重跑仍为 exit 1，确认属实**
- 本次新增的回归测试（`agent_state_store::tests`）：
  | 测试 | 断言 |
  |---|---|
  | `pure_append_does_not_trigger_full_rewrite` | `[a]` → `[a,b]`：`needs_full_rewrite == false`；同时断言旧的 `!whole.starts_with(stored_hash)` **恒为 true**（直接证明旧假设错误）；并验证追加后数据与 `.list.hash` 均正确 |
  | `identical_list_is_not_rewritten` | 同长且未变：`CountingStore` 计数证明仅写 `.list.hash`（2 次 → 3 次），列表实体不重写 |
  | `appended_prefix_hash_stays_valid` | `[a,b]` → `[a,b,c]` 纯追加不重写 |
  | `modified_element_triggers_full_rewrite` | 前缀内元素被改 → 重写 |
  | `shrink_triggers_full_rewrite` | 列表缩短 → 重写 |
  | `empty_existing_state_does_not_force_rewrite` | 无既有数据 + 无 hash → 不重写；有既有数据 + 无 hash → 重写（Case 2） |
  | `legacy_state_without_hash_self_heals` | 旧 SHA-256 hash 遗留 + 新写入 → 列表重写、hash 被 core 值覆盖，数据无损 |
- 旧行为实测（一次性探针，已删除）：旧 `needs_full_rewrite([a,b], old_hash([a]), 1) == true`（纯追加被误判为全量重写）；新实现同输入为 `false`。
- 已知问题：无编译错误。**2026-10-28 更新**：真实 COS 后端已落地（`CosHttpObjectStore`），下列 2026-10-22 记录的 3 项静态缺口均已处理：
  - ~~`serde` / `futures` / `chrono` 冗余依赖~~ → **已删除**（`rg` 复核仍零使用点；`chrono` 的 2 处命中确为注释里的 "synchronous" 子串）
  - ~~`CosError::Backend` / `NotFound` 从未被构造~~ → `Backend` 现由 HTTP 传输/响应体读取路径构造；新增 `Service{status,code,message}`（403/404 区分）与 `Closed`（close 后请求）；**`NotFound` 也已接线（2026-10-31）**：`CosRemoteSnapshotClient::download` 在 `get` 返回 `Ok(None)` 时改为 `Err(CosError::NotFound(key).into())`（对齐 Java 侧 SDK 的 object-not-found 异常），用例 `download_missing_snapshot_reports_not_found`
  - `object_store`（根 `[workspace.dependencies]`，`0.14`）仍**全 workspace 无引用且不在 `Cargo.lock`** → 属汇总方 P2，本 crate 未动根 `Cargo.toml`

### 4.2 下一步与阻塞
1. ~~**真实 COS SDK 适配器**~~ → **已完成（2026-10-28）**：`CosHttpObjectStore` + `signature`；能力边界见 §3.2，执行记录见 §4.3。
2. ~~**分段上传 / 预签名 URL / STS token**~~ → **已完成（2026-10-28 第二轮）**：见 §3.3 与 §4.4。剩余非阻塞增强：`ListParts` 断点续传、分片级重试、上传进度回调、预签名自定义头（`response-content-disposition` 等以参数形式已可扩展）、STS **自动刷新**（按任务要求不做，由调用方重建）。
2. ~~`ListHashUtil` 精确对齐~~ → **已完成（2026-09-19）**，见 §2.2 / §3.1。
3. **跨模块依赖（需汇总方处理）**：
   - harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`）——**已解决（2026-10-28）**：harness 已增 `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard` 三槽 + `SandboxGuard` trait，`CosDistributedStore::to_harness_distributed_store()` 已填满 5 槽，COS 的 `base_store` 可经 `HarnessAgent::builder().distributed_store(...)` 自动接线（详见文末执行记录）。**2026-10-22 复核留存**：Java `DistributedStore` 是 **interface**，`HarnessAgent` 会用 `distributedStore.baseStore()` 自动注入 `RemoteFilesystemSpec`/`StoreBackedPeriodicGate`/`SkillUsageStore`
   - 根 `Cargo.toml` 无需新增 workspace 依赖；本次反而**移除了**本 crate 的 `sha2` 声明（根 `[workspace.dependencies]` 的 `sha2` 未动，也无其他 crate 受影响）。**2026-10-22 复核**：根 `[workspace.dependencies]` 中的 `object_store = "0.14"` **全 workspace 无任何 crate 引用**且不在 `Cargo.lock` → 建议汇总方清理
   - **无需汇总方回填的跨模块拆分**：COS 本次仅删除本 crate 内部平行实现，与 MONGODB/REDIS/JDBC/OSS 各自的 list_hash 收敛互不影响（各自 crate 自洽）。
4. `CosSandboxExecutionGuard` 目前仅进程内语义；如需跨进程分布式互斥，需汇总方评估存储层协调方案。**2026-10-22 复核补充**：Java `CosDistributedStore` **未**覆写 `sandboxExecutionGuard()`（用 harness 默认 `noop()`），故 Rust 提供真实 guard 属**增强**；但 Java `SandboxExecutionGuard` 是 `@FunctionalInterface`（`tryEnter` **阻塞**至获得槽位），Rust harness 是**具体 struct**（仅可重入计数、不阻塞、不跨进程）→ 即便接上 Rust guard 也**达不到 Java 的阻塞语义**（汇总 tracker 已裁定「harness guard 缺口确认」，此处仅作 COS 侧记录）
5. **存量数据自愈性说明（无数据迁移脚本）**：存量 COS bucket 中 `.list.hash` 为旧 SHA-256 值。收敛后首次 `save_list` 时，core `needs_full_rewrite` 的 Case 3 会拿「新列表 `existing_count` 前缀的 core 哈希」与「旧 SHA-256 值」比较 → 必然不等 → 判定需全量重写 → 列表实体被当前 `values` 覆盖（数据以调用方传入为准，**无损**），`.list.hash` 随即被 core 值覆盖，后续写入恢复正常增量语义。若 `.list.json` 存在但 `.list.hash` 丢失：新代码读到 `existing_count > 0` 且 `stored_hash == None` → Case 2 命中（需重写），同样一步自愈。

### 4.3 2026-10-28 执行记录（真实 COS HTTP 适配器）

**新增文件**
| 文件 | 内容 |
|---|---|
| `src/signature.rs`（479 非空行 / 9 test） | COS v5 签名纯函数：`sha1_hex` / `hmac_sha1_hex` / `sign_key` / `uri_encode(_path)` / `canonical_pairs` / `http_string` / `string_to_sign` / `sign_request` → `CosSignature`（含 `authorization`）；`CosCredentials` 的 `Debug` 对 SecretKey / SessionToken 打码 |
| `src/http_object_store.rs`（1209 非空行 / 16 test） | `CosHttpObjectStore`（`reqwest`）+ `CosHttpConfig` + builder；`get`/`put`/`exists`/`delete`/`list_page`/`list_prefix`/`delete_many`/`close` + 状态码映射 |

**trait 扩展（`object_store.rs`，全部带默认实现，不破坏既有后端）**：`ObjectListPage{keys, next_continuation_token}`、`list_page`（默认基于 `list_prefix` 分页）、`delete_many`（默认逐键删除；HTTP 后端覆盖为 `POST /?delete`）、`len`（默认全量列表）、`close`（默认 no-op）。`CosAgentStateStore::close()` 对齐 Java `close()`（Java 关 `COSClient`），其 `delete()` 改走 `delete_many`（对齐 Java `deleteKeys` 1000/批）。

**签名实现要点**
- `SignKey = hex(HMAC-SHA1(SecretKey, KeyTime))`；`StringToSign = "sha1\n{KeyTime}\n{hex(SHA1(HttpString))}\n"`；`Signature = hex(HMAC-SHA1(SignKey, StringToSign))`——SignKey 以**十六进制字符串**作为 HMAC key（不是原始字节）。
- `HttpString = lower(method)\n<raw path>\n<canonical params>\n<canonical headers>\n`；params / headers 的 key 转小写、value 全量 percent-encode（`A-Za-z0-9-._~` 以外全部编码，大写十六进制），按编码后 key 升序；列表用 `;` 连接，键值串用 `&`。
- **path 用未编码原文**（`uri_encode_path` 仅用于请求行）：文档 `HttpString` 里是 `/exampleobject(腾讯云)`，实测其 SHA1 与文档 `8b2751e7…` 一致；若对 path 再编码则对不上。
- `q-sign-time = q-key-time = "<start>;<end>"`（默认 900s，与 COS SDK 一致），时钟可注入（`ClockFn`，测试固定 `1_700_000_000`）。
- 临时密钥：`x-cos-security-token` 同时进入签名 header 列表与实际请求头。

**金标向量来源（离线钉住，无网络/无凭证）**
来源：腾讯云 COS 官方文档《请求签名》 <https://cloud.tencent.com/document/product/436/7778>（中文版「示例一 上传对象」/「示例二 下载对象」，2025-07-03 版；国际版同值）。

| 断言 | 文档值 | 测试 |
|---|---|---|
| PUT `SHA1(HttpString)` | `8b2751e77f43a0995d6e9eb9477f4b685cca4172` | `official_put_example_golden_vector` |
| PUT `Signature` | `3b8851a11a569213c17ba8fa7dcf2abe…1234`（文档把后 6 位打码为 `1234`）→ 实测 `3b8851a11a569213c17ba8fa7dcf2abec6935172` | 同上（同时断言与文档公开的 34 位前缀一致） |
| GET `SHA1(HttpString)` | `54ecfe22f59d3514fdc764b87a32d8133ea611e6` | `official_get_example_golden_vector` |
| GET `Signature` | `01681b8c9d798a678e43b685a9f1bba0f6c0…1234` → 实测 `01681b8c9d798a678e43b685a9f1bba0f6c0e012` | 同上 |
| `HeaderList` / `HttpHeaders`（步骤 4） | `date;host;x-cos-acl;x-cos-grant-read` / `date=Thu%2C%2016%20May…&x-cos-grant-read=uin%3D%22100000000011%22` | `header_canonicalization_sorts_and_lowercases` |
| `UrlParamList` / `HttpParameters`（步骤 3） | `delimiter;max-keys;prefix` / `delimiter=%2F&max-keys=10&prefix=example-folder%2F` | `parameter_canonicalization_is_sorted_and_encoded_once` |
| HMAC-SHA1 / SHA1 原语 | RFC 2202（`Jefe`）/ RFC 3174（`abc`） | `hmac_sha1_matches_rfc2202_vectors`、`sha1_matches_rfc3174_vectors` |

> 说明：文档对两条示例的 `Signature` 尾部统一打码为 `…1234`，故测试既断言完整重算值，又断言与文档公开的 34 位前缀一致；`SignKey` 由文档给出（`eb2519b4…` / `937914bf…`），SecretKey 未公开，因此 `SignKey` 派生链靠「RFC 2202 钉 HMAC 原语 + 文档 SignKey 钉 Signature」两端闭合。

**逐条对照 Java（`COSClient` → COS REST）**
| Java | Rust |
|---|---|
| `getObjectMetadata` → 404 判不存在 | `exists` = `HEAD`，404→`false`；`get` = `GET`，404→`Ok(None)` |
| `getObject` + `readAllBytes` | `get` → `resp.bytes()` |
| `putObject(PutObjectRequest)` | `PUT`（签名含 `content-type`） |
| `deleteObject` / `deleteObjects`（1000/批） | `delete`（404→`Ok(())`）/ `delete_many`（`POST /?delete` + `Content-MD5`，1000/批） |
| `listObjects` marker 循环（`maxKeys=1000`） | `GET /?list-type=2&continuation-token=` + `list_prefix` 自动翻页 |
| `CosServiceException.statusCode` | `CosError::Service{status,code,message}` + `status()` / `is_not_found()` / `is_signature_error()` |
| `cosClient.shutdown()` | `close()`（幂等；close 后请求返回 `CosError::Closed`；内存后端默认 no-op） |

**反向验证（逐个回退实现 → 观察 FAILED → 恢复）**
1. 注释 `canonical_pairs` 的 `sort_by` → `cargo test -p agentscope-extensions-cos signature::` → **5 passed / 4 failed**（`official_put_example_golden_vector`、`official_get_example_golden_vector`、`header_canonicalization_sorts_and_lowercases`、`parameter_canonicalization_is_sorted_and_encoded_once`）→ 已恢复。
2. `get` 的 `status == 404` 改为 `status.is_client_error()` → `cargo test -p agentscope-extensions-cos forbidden` → **0 passed / 1 failed**（`forbidden_is_an_error_not_a_missing_object`：403 被误判为「对象不存在」）→ 已恢复并重跑全量 73 passed。（首次尝试时被并发会话打断：`agentscope-extensions-model` 正被另一会话改写导致编译失败，恢复后重试成功。）

**依赖与文件层面的副作用**
- `crates/agentscope-extensions-cos/Cargo.toml` 新增：`reqwest`（workspace）、`hmac 0.12`、`sha1 0.10`、`quick-xml 0.39`、`md5 0.8`、`base64`（workspace）；dev-dep `httpmock 0.7`。**未改根 `Cargo.toml`**（workspace 无 `hmac`/`sha1`/`quick-xml`/`md5` 条目，参照 `agentscope-extensions-channel` 先例写具体版本）。
- `Cargo.lock` 由 cargo 自动更新（新增上述条目），属依赖引入的必然结果。
- 新增 2 个源文件 + 3 个 doctest（均 `no_run`）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-cos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-cos` / `cargo test -p agentscope-extensions-cos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-cos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/COS__MIGRATION_HISTORY.md`](./history/COS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
