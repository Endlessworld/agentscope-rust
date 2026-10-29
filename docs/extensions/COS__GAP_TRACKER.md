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
| 当前状态 | **总判：已完成**（本 crate P1 清零，唯一 P1 属 core 跨模块；余 P2）—— 已完成（**真实 COS HTTP 适配器已落地**：`CosHttpObjectStore` + 自建 `q-sign-algorithm=sha1` 签名（金标向量钉住）+ **分段上传 / 预签名 URL / STS token**（2026-10-28 第二轮）） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `CosAgentStateStore` | COS 对象存储上的 `AgentStateStore` | `CosAgentStateStore`（implements `agentscope_core::state::AgentStateStore`） | key 布局对齐：`{prefix}{user}/{session}/{key}.json` / `.list.json` / `.list.hash`；anonymous 用户段 `__anon__`；前缀归一化对齐。**缺口**：Java 额外覆写了 `delete(userId, sessionId, key)`（删 `.json`+`.list.json`+`.list.hash`），Rust core `AgentStateStore` trait **无该方法** → 无法实现（core 层缺口，见 §6 P2-①）；Java `close()` 关闭 `COSClient`，Rust 无 `close`（trait 无） |
| `CosBaseStore` | COS 上的 `BaseStore`（远程 KV） | `CosStore`（implements `agentscope_harness::filesystem::remote::store::BaseStore`） | key 布局：`{prefix}{ns0}/{ns1}/.../{key}.json` / `.version`；version 成功写入 +1；CAS 语义以 harness 为准。**对齐**：Java `CosBaseStore` **未**覆写 `supportsAtomicSessionStorage()`（默认 `false`），Rust `CosStore` 亦未覆写 `supports_atomic_session_storage()`（默认 `false`）→ 一致。**强化**：Rust `put`/`put_if_version`/`delete` 走进程内 `tokio::Mutex` 串行化（Java 无锁，read-check-write 有竞态） |
| `CosDistributedStore` | 组合 agent state / base store / snapshot spec | `CosDistributedStore` | Java 含 `agent_state_store`/`base_store`/`sandbox_snapshot_spec`；Rust 版额外含 `sandbox_execution_guard`，并可 `to_harness_distributed_store()` 投影（harness 结构体字段不足，见汇总 §2.2）。**关键差异**：Java `CosDistributedStore` **未**覆写 `sandboxExecutionGuard()`（继承默认 `noop()`），Rust 版提供**真实** `CosSandboxExecutionGuard`（harness 进程内计数 guard）→ Rust 语义**强于** Java，非缺失。**缺口**：Java `agentStateStore()`/`baseStore()`/`sandboxSnapshotSpec()` 每次调用**新建**实例；Rust 版在 `create()` 时**一次性构造并缓存**（`Arc`）→ 语义等价但少了「每次新实例」的隔离性（无害，见 §6 P2-③） |
| `CosRemoteSnapshotClient` | COS 上的 `RemoteSnapshotClient` | `CosRemoteSnapshotClient`（implements `...snapshot::RemoteSnapshotClient`） | 对象 key：`{prefix}{snapshotId}.tar`；snapshot id 安全校验复用 harness `validate_snapshot_id` 语义。**差异（Rust 更严）**：Java 仅拒绝 blank/null `snapshotId`，Rust 额外拒绝 `/`、`\`、`..`、`\0`；Java `RemoteSnapshotSpec.build` 不校验，Rust 经 harness `RemoteSnapshotSpec::build` 校验 → Rust 是**安全加固**，非缺失（见 §6 P2-④） |
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
| ⚠️ 语义偏离（残留 P1） | core `needs_full_rewrite` Case 3 比较的是「**当前列表 `existing_count` 前缀**的哈希」与「**存储的整表哈希**」。首次写入时 `stored_hash` 存的是**整表**哈希，故第 2 次写入（`existing_count` = 整表长）时前缀 = 整表 → 相等 → 追加判定正确；但**纯追加后**若再追加一次，`stored_hash` 是上一次的整表哈希，而前缀只取到旧长度 → **必然不等 → 判全量重写**。即 **core 与 Java 同款「第二次追加仍全量重写」的行为问题**（Java `ListHashUtil.needsFullRewrite` 同样如此）。本 crate 已忠实委托 core，故与 Java 行为**一致**，非本 crate 缺陷（core 层议题，见 §6 P1-①） |

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
**已完成**：COS 模块全部产出契约落地并复用既有 trait；list 增量哈希已收敛到 core（无平行实现）。**2026-10-29 复核**：测试数 **105**（§1 / §4.4 一致）；§3.1 旧记「31」已就地勘误（历史原文保留）；真实剩余见 §4.2（core `needs_full_rewrite` Case 3 与 Java 同款、core trait 无 per-key `delete`/`close`、`CosError::NotFound` 预留变体、分片上传无 `ListParts` 断点续传/分片级重试/进度回调、STS 不自动刷新、预签名只签 host、根 `object_store` 依赖无引用）。
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
  - ~~`CosError::Backend` / `NotFound` 从未被构造~~ → `Backend` 现由 HTTP 传输/响应体读取路径构造；新增 `Service{status,code,message}`（403/404 区分）与 `Closed`（close 后请求），`NotFound` 仍未被构造（保留：语义前缀 + `From<CosError> for core Error` 走 `to_string()`，删除会改变公开 API；标注为预留变体）
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

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **静态核对，零编译、零测试执行**（`cargo build` / `cargo test` 全程未运行）。
- 手段：逐文件通读 Java 5 个主类 + 4 个测试类 / Rust 9 个 `src/*.rs`；`rg` 检索符号与依赖；`git show e01809d:...` 取历史实现复核旧行为；`git log -S "sha2"` 复核依赖收敛。
- 复核者：静态复核子智能体（只读 + 仅改本 tracker）。

### 6.2 实测统计（静态口径）

| 项 | Java | Rust |
|---|---|---|
| 主代码文件 | 5 个 `.java` | 9 个 `src/*.rs`（无 `tests/` 目录） |
| 主代码总行 | 971 | 1719 |
| 主代码非空行 | **882** | **1517** |
| 主代码去空去注释行 | 709 | 1266 |
| 测试文件 | 4 个测试类 | 0（全部内联于 `src/`） |
| 测试总行 | 898 | — |
| 测试非空行 | **760** | — |
| 测试用例数 | **62 个 `@Test`**（无 `@ParameterizedTest`/`@TestFactory`） | **31**（19 × `#[test]` + 12 × `#[tokio::test]`） |
| doctest | — | **2 个，均为 `no_run`**（`lib.rs:27`、`agent_state_store.rs:32`）→ 只编译不执行 |

- Rust 31 test 分布：`agent_state_store` 12、`base_store` 5、`distributed_store` 5、`snapshot` 5、`guard` 3、`object_store` 1、`layout` 0、`error` 0、`lib` 0。
- Java 62 `@Test` 分布：`CosAgentStateStoreTest` 20、`CosRemoteSnapshotClientTest` 18、`CosBaseStoreTest` 16、`CosDistributedStoreTest` 8。
- **行数/类数核对结论**：Java 主 882 / 测 760 与 rollup 一致（**属实**）；Rust 9 文件 / 1517 非空行 / 31 test 属实（rollup 记「31+2」中的 2 为 doctest，口径需注明为 `no_run`）。

### 6.3 对齐结论表

| Java 类 → Rust 对应 | 存在？ | 行为覆盖判定 |
|---|---|---|
| `CosAgentStateStore` → `CosAgentStateStore` | ✅ | key 布局（`.json`/`.list.json`/`.list.hash`）、`__anon__`、前缀归一化（`\`→`/`、去首 `/`、补尾 `/`）、blank 默认值 **全部对齐**；`save_list` 的 hash/追加判定已委托 core。**缺 `delete(user,session,key)` 与 `close()`**（core trait 无此二方法） |
| `CosBaseStore` → `CosStore`（`BaseStore`） | ✅ | `{ns}/{key}.json` + `.version`、version 0→+1、`put_if_version(0)`=create-if-absent、`search` 排序+offset/limit、key 去前导 `/` **全部对齐**；`supports_atomic_session_storage` 两边均 `false` **一致**；Rust 额外加进程内 mutex（强化） |
| `CosDistributedStore` → `CosDistributedStore` | ✅ | 子前缀 `state/`/`store/`/`snapshot/` 对齐；默认前缀 `agentscope/` 对齐。差异：Java 每次访问新建实例，Rust 构造时缓存（无害）；Rust 多提供真实 guard（Java 为 `noop()` 默认，**增强**） |
| `CosRemoteSnapshotClient` → `CosRemoteSnapshotClient` | ✅ | `{prefix}{id}.tar` 对齐；prefix 归一化对齐；Rust 额外做 `/`、`\`、`..`、`\0` 校验（**加固**） |
| `CosSnapshotSpec` → `CosSnapshotSpec` | ✅ | 均为「包裹 harness `RemoteSnapshotSpec`」结构，语义等价 |
| （Java 无）→ `CosSandboxExecutionGuard` | ✅ | 包装 harness guard，保留 `try_enter`/`release`/`is_held`/`active_count`；**未**实现跨进程协调（已记为设计决定不迁移） |
| `com.qcloud.cos.COSClient` → `CosObjectStore` + `InMemoryCosObjectStore` | ⚠️ | 抽象层齐备，**真实后端完全缺失**（含签名） |

### 6.4 新发现缺口

| 编号 | 优先级 | 缺口 | 归属 |
|---|---|---|---|
| P1-① | P1 | **core `needs_full_rewrite` 连续第二次追加仍判全量重写**：存的是整表哈希、比的是 `existing_count` 前缀哈希 → 追加一次后 `stored_hash`=旧整表哈希，下次比前缀（较短）必不等。**Java `ListHashUtil` 同款问题**，故本 crate 与 Java 一致、非本 crate 缺陷 | core（汇总方） |
| P1-② | P1 | ~~**测试覆盖缺口**：31 test vs Java 62 `@Test`（≈50%）~~ → **已补齐（2026-10-28，第二轮后 105）**：现 **105** test（`httpmock` HTTP 层 + 官方签名金标向量 + key 字面量断言 + 分段/预签名）。Java 独有用例（`putStripsLeadingSlashFromKey`、`deleteSingleKey_removesStateListAndHash`、`builderRejectsNullClient/BlankBucket`、`close_shutsDownClient`、`emptyNamespace_usesPrefixOnly`、`nullPrefix_treatedAsEmpty`、`prefixWithLeadingSlash_isStripped`、批量删除）均有对应：`key_with_leading_slash_is_normalized`、`builder_rejects_missing_object_store`、`builder_rejects_invalid_configuration`、`close_is_idempotent_and_blocks_further_requests`、`empty_namespace_uses_the_prefix_only`、`prefix_edge_cases_normalize_like_java`、`delete_many_*` | 本 crate（**已完成**） |
| P1-③ | P1 | **`base_store` 无法经 `HarnessAgent::builder().distributed_store(...)` 自动接线**：harness `DistributedStore` 仅 2 字段，而 Java `HarnessAgent` 会用 `distributedStore.baseStore()` 自动注入 `RemoteFilesystemSpec`/`StoreBackedPeriodicGate`/`SkillUsageStore` | harness（汇总方，汇总 tracker §2.2 已裁定） |
| P0 | — | **无新增 P0**。核心链路（state / base store / snapshot / guard / 组合者）产出契约齐全且行为对齐 | — |
| P2-① | P2 | ~~Java `AgentStateStore.delete(user,session,key)`（删 3 类对象）与 `close()` 在 Rust core trait 中**不存在**~~ → **已完成（2026-10-29）**：core trait 新增 `delete_key`（默认返回错误），`CosAgentStateStore` 覆盖为一次批量删除 `.json`/`.list.json`/`.list.hash`；`close()` 定为**不迁移**（Rust 后端共享 `Arc<dyn ObjectStore>`，生命周期由 crate 管理，`CosAgentStateStore::close()` 已具备等价能力） | core + 本 crate（**已完成**） |
| P2-② | P2 | ~~`Cargo.toml` 冗余依赖：`serde` / `futures` / `chrono` 声明但**零使用**~~ → **已删除（2026-10-28）**：三者均从本 crate `Cargo.toml` 移除，复核 `rg '\bserde\b|futures|chrono'` 仅余 `synchronous` 注释子串 | 本 crate（**已完成**） |
| P2-③ | P2 | ~~`CosError::Backend` / `NotFound` 变体从未被构造~~ → `Backend` 现由 HTTP 传输层构造；新增 `Service{status,code,message}`（403/404 区分）与 `Closed`；`NotFound` 仍无构造点，**保留为预留变体**（`From<CosError> for core Error` 走 `to_string()`，删除会动公开 API） | 本 crate（**部分完成**） |
| P2-④ | P2 | snapshot id 校验 Rust 比 Java **更严**（Java 仅拒 blank）→ 加固，非缺失，仅需在文档中明示 | 已在本文件 §2 明示 |
| P2-⑤ | P2 | ~~根 `[workspace.dependencies]` 的 `object_store = "0.14"` 全 workspace 无 crate 引用且不在 `Cargo.lock`~~ → **已删除（2026-10-29）**，见汇总 tracker §4.15.2 | 汇总方（**已完成**） |
| P2-⑥ | P2 | 错误传播语义差异：Java `save`/`get`/`getList` 失败抛 `RuntimeException`；Rust `AgentStateStore` trait 返回 `()` → Rust 侧 `tracing::error!` 吞掉错误（`agent_state_store.rs:134/172/178`）。**已裁定（2026-10-29）**：不改旧签名（会动 7 个存储 crate 全部实现与调用点）；需要错误通道的路径改用新方法（如 `delete_key` 返回 `Result`） | core trait 签名决定（**已完成**） |
| P2-⑦ | P2 | ~~真实适配器还需覆盖分页 `listObjects`（marker 循环）与批量 `deleteObjects`（1000/批）~~ → **已覆盖（2026-10-28）**：trait 新增 `list_page`（`continuation-token`/`max-keys`）与 `delete_many`（HTTP 后端 `POST /?delete` 1000/批 + `Content-MD5`），`list_prefix` 自动翻页、`CosAgentStateStore::delete` 改走批量删除 | 本 crate（**已完成**） |

### 6.5 勘误列表（原文 → 实际）

| # | 原文 | 实际 | 处置 |
|---|---|---|---|
| 1 | §4.1「**31** lib unit tests + 2 doctests = **33 passed**」 | 31 = **19 × `#[test]` + 12 × `#[tokio::test]`**；2 个 doctest 均为 **`no_run`**（只编译不执行）→ 实际执行 **31**，非 33 | §4.1 已改写并注明口径 |
| 2 | §1「**31** 内联 test + 2 doctests」 | 数字正确，但未区分 `#[test]`/`#[tokio::test]`，且未说明 doctest 为 `no_run` | §1 已补明细 |
| 3 | §4.1「31 passed / 0 failed + 2 doctests」（§3.1 单元测试行） | 同上 | §3.1 已改写 |
| 4 | §2.1「`ListHashUtil` → `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`）」 | 正确，但**遗漏** core `compute_hash` 用 `DefaultHasher`（SipHash-1-3）而 Java 用 `String.hashCode()` → 两侧 hash 值**永不相等**（跨语言不兼容） | §2.1 + §2.2 已补「跨语言 hash 值」行 |
| 5 | §3.1「修复：纯追加不再恒判全量重写」表述为已彻底修复 | 属实（单次追加已修），但**连续第二次追加**仍判全量重写（core Case 3 结构所致，Java 同款） | §2.2 + §3.1 已补限定 + P1-① |
| 6 | §3.2「真实 COS SDK 适配器 → 需 … HTTP 签名（自实现或接第三方）」 | 表述偏轻：实际**既无 SDK 也无自建签名**，且 workspace 无 `sha1`/`hmac` 依赖 → **从零开始** | §3.2 + §4.2 第 1 条已改写 |
| 7 | §2 映射表未记录 Java `CosDistributedStore` 的 `sandboxExecutionGuard()` 用 harness 默认 `noop()` | 已核实（Java 该类**无** `sandboxExecutionGuard` 覆写）→ Rust 提供真实 guard 属**增强**，非缺失 | §2 映射表已补 |
| 8 | §2 映射表未记录 Java `CosBaseStore` 未覆写 `supportsAtomicSessionStorage()` | 已核实（Java `BaseStore` 默认 `false`，`CosBaseStore` 未覆写）→ Rust 亦 `false`，**一致** | §2 映射表已补 |
| 9 | §4.2 第 3 条「harness `DistributedStore` 字段不足」表述笼统 | 已核实 Java 侧 `HarnessAgent` 会用 `distributedStore.baseStore()` **自动接线**多处 → Rust 侧丧失该自动接线能力 | §4.2 第 3 条已补 + P1-③ |
| 10 | §4.1「已知问题：无编译错误；真实 COS SDK 未接入」 | 遗漏 3 项：① `serde`/`futures`/`chrono` 冗余依赖 ② `CosError::Backend`/`NotFound` 死变体 ③ 根 `object_store` 依赖全局无引用 | §4.1「已知问题」已补 |
| 11 | §3.2「`ListHashUtil` 精确对齐」残留 PS2 表述 | 「跨语言共读同一 bucket 时首次写入会自愈」**属实**（core Case 3 必不等 → 全量重写 → hash 被覆盖）；但需补充「**两侧 hash 值永不相等**」的根因说明 | §2.2 已补 |

### 6.6 复核结论

> **本节为 2026-10-22 静态复核快照**（当时零编译、零测试）。其中「真实 SDK 适配」条目已被 §4.3（v5 签名 + HTTP 后端）与 §4.4（分段/预签名/STS）取代，保留作历史对照。

- **§4.2「真实 SDK 适配」现状判定**：Rust 侧为**自建抽象 trait**（`CosObjectStore`），**非**自建签名、**非**第三方 SDK；`q-sign-algorithm` 签名**完全未实现**（静态可判定：`rg 'q-sign-algorithm|Hmac|Sha1|sha1|secretId'` 全 crate 零命中）。**→ 已于 2026-10-28 推翻：`signature.rs` + `http_object_store.rs` 已落地，见 §4.3。**
- **`list_hash` 收敛到 core**：**属实**。本 crate 无平行实现（`rg 'fn compute_list_hash|fn needs_full_rewrite'` → 零命中），`sha2` 已从 `Cargo.toml` 移除（`git log -S "sha2"` 确认提交 `daa700d`）。
- **「追加恒全量重写」偏离修复**：**属实**。历史实现经 `git show e01809d:.../agent_state_store.rs` 确认为 `!compute_list_hash(values).starts_with(h)`，全量 SHA-256 无前缀性质 → 恒 `true`；现委托 core 后单次纯追加返回 `false`（回归测试内联保留）。
- **B 波次统一产出契约**：**5/5 齐全**（`CosDistributedStore` / `CosAgentStateStore` / `CosStore` / `CosSnapshotSpec` / `CosSandboxExecutionGuard`），且均可从 `CosDistributedStore` 访问（`agent_state_store()`/`base_store()`/`sandbox_snapshot_spec()`/`sandbox_execution_guard()` + `shared_*` trait-object 便捷方法）。
- **建议 rollup 状态值**：**保持 `已完成`**（rollup 行 12 现值「已完成（build + test 通过，31+2 test；…）」）。理由：P0 核心链路无缺口、产出契约 5/5 齐备、list_hash 收敛与追加偏离修复均经静态复核属实；剩余项均为 P1/P2 且其中 P1-①/P1-③ 属 core/harness 层（汇总方）、P1-② 属测试覆盖增强而非功能缺失。**唯一需在 rollup 同步的措辞修正**：「31+2 test」应表述为「**31 unit test（19 `#[test]` + 12 `#[tokio::test]`）+ 2 `no_run` doctest（只编译）**」，并追加「**真实 COS SDK 适配（含 `q-sign-algorithm` 签名）仍未开始**」。 （2026-10-29 勘误：本段为 2026-10-22 快照，已失效——现为 **105 passed**，真实 COS v5 签名适配器 / 分段上传 / 预签名 / STS 均已落地，见 §5/§6；rollup 现值见汇总 §1.1）


## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§2.2 / P1-ⓐ 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_harness_distributed_store()` 从「只投 agent_state_store」改为全量投影。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `self.agent_state_store.clone()` | `CosAgentStateStore`，前缀 `{prefix}state/` |
| `isolation_scope` | `IsolationScope` | 未设置（builder 默认） | 由调用方在 `HarnessAgent` 侧覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `self.base_store.clone()` | `CosStore`，前缀 `{prefix}store/` |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `self.snapshot_spec.clone()` | `CosSnapshotSpec`，前缀 `{prefix}snapshot/` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.execution_guard.clone())` | `CosSandboxExecutionGuard`；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_harness_distributed_store()`：填满 5 槽（builder，不用 `..Default::default()`）。
- `src/guard.rs`：`impl SandboxGuard for CosSandboxExecutionGuard`，四方法委托固有方法（仍委托 harness `SandboxExecutionGuard`，未改行为）。
- 测试：`src/distributed_store.rs::harness_projection_fills_every_composite_slot`。

### 验证

`cargo test -p agentscope-extensions-cos` → **74 passed / 0 failed**（第一轮基线；第二轮后为 **105 passed**，见 §4.4）。

### 未完成 / 语义差异

- 无留空槽位。
- 分布式协调仍是 strategy-deferred（guard 只是 harness 进程内计数 + 保留 object store），与 Java COS `noop()` 默认相比属**增强**，语义未变。

## 2026-10-28 第二轮执行记录：分段上传 / 预签名 URL / STS

### 新增文件

| 文件 | 规模 | 内容 |
|---|---|---|
| `src/multipart.rs`（新） | 230 非空行 / 8 test | 常量（阈值/分片尺寸上下限/最大分片数/默认并发）、`MultipartPlan::range`、`plan_parts`、`normalize_etag`、`complete_multipart_body` |
| `src/presign.rs`（新） | 217 非空行 / 6 test | `presign_url`（复用 `signature::sign_request` 的 `authorization`）、`resolve_expire_secs`、`MAX_PRESIGN_EXPIRE_SECS` |

### 改动文件

| 文件 | 改动 |
|---|---|
| `src/http_object_store.rs` | `CosHttpConfig`/builder 新增 `multipart_threshold`/`multipart_part_size`/`multipart_concurrency` + 校验；`put` 阈值切换；`put_multipart` + `initiate/upload_part/complete/abort`；`presign_get`/`presign_put`；`with_credentials`；`query_string`/`escape_xml` 改 `pub(crate)`；+17 test |
| `src/lib.rs` | `pub mod multipart; pub mod presign;` + 常量/`MAX_PRESIGN_EXPIRE_SECS` 导出；文档段重写 |
| `src/distributed_store.rs` | 删除 test 模块内无用的 `SandboxGuard` 导入（清 clippy 告警，与本轮功能无关） |
| `Cargo.toml` | **未改**（无新依赖：`tokio` 的 `JoinSet` 已在 `rt-multi-thread` 内） |

### 请求序列与断言维度

| 步骤 | 请求 | 签名 `q-url-param-list` | 断言 |
|---|---|---|---|
| initiate | `POST /{key}?uploads`（`Content-Type: application/octet-stream`） | `uploads` | 响应 XML `<UploadId>`；缺失/空 → `CosError::Backend("… no UploadId")` |
| part | `PUT /{key}?partNumber=N&uploadId=..` | `partnumber;uploadid` | 请求体逐字节相等；响应头 `ETag` 去引号（`normalize_etag`）；缺失 ETag → Backend 错误 |
| complete | `POST /{key}?uploadId=..`（`application/xml`） | `uploadid` | 完成 XML 按 partNumber 升序、`<ETag>"…"</ETag>` 引号化并 XML 转义 |
| abort | `DELETE /{key}?uploadId=..` | `uploadid` | 分片/完成失败后必发；`404 NoSuchUpload` 视为成功；abort 自身失败仅 `tracing::warn!`，不掩盖原错误 |

### 测试清单（新增 31 = 8 + 6 + 17）

- `multipart::tests`（8）：`defaults_match_the_java_transfer_manager`、`configured_part_size_is_used_when_it_covers_the_object`、`part_size_grows_to_stay_within_the_part_count_cap`、`plan_rejects_illegal_part_sizes_and_bodies`、`plan_accepts_boundary_sizes`、`etags_are_unquoted`、`complete_body_lists_parts_in_order`、`complete_body_escapes_etags`。
- `presign::tests`（6）：`url_reuses_the_authorization_chain`、`only_host_is_signed`、`method_changes_the_signature`、`extra_parameters_are_signed_and_encoded`、`object_paths_are_encoded_for_the_url`、`expiry_is_validated_and_defaults_to_the_client_setting`。
- `http_object_store::tests`（17）：`put_below_threshold_uses_a_single_put`、`put_at_threshold_switches_to_multipart`、`multipart_uploads_sorted_parts_and_completes`（2.5 MiB / 3 片 / 并发 2，逐片字节相等）、`multipart_aborts_when_a_part_fails`、`multipart_aborts_when_complete_is_rejected`、`multipart_abort_tolerates_a_missing_upload`、`multipart_initiate_failure_does_not_abort`、`multipart_initiate_requires_an_upload_id`、`multipart_rejects_an_empty_body`、`multipart_settings_are_validated_and_exposed`、`with_credentials_swaps_the_session_token`、`presign_get_builds_a_complete_signed_url`、`presign_get_signature_is_pinned`（**固定时钟 + 公网域名，整条 URL 字面量钉位**：`…&q-signature=78d1df6ab1f8351625eab6f5f0ea072d382e4dc7`，已用 PowerShell/.NET HMACSHA1/SHA1 **独立重算交叉验证**）、`presign_expiry_is_honoured_and_validated`、`presign_put_signs_the_put_method`、`presign_includes_the_session_token_as_a_query_parameter`、`presign_is_rejected_after_close`。

### 反向验证（逐个回退实现 → 观察 FAILED → 恢复）

| # | 回退 | 结果 |
|---|---|---|
| 1 | `put_multipart` 失败路径删掉 `abort_multipart` | `cargo test -p agentscope-extensions-cos multipart_` → **6 passed / 2 failed**（`multipart_aborts_when_a_part_fails`、`multipart_aborts_when_complete_is_rejected`）→ 已恢复 |
| 2 | `should_use_multipart` 的 `>=` 改 `>` | `put_at_threshold_switches_to_multipart` → **0 passed / 1 failed** → 已恢复（Java `shouldUseMultipartUpload` 也是 `>=`） |
| 3 | `etags.sort_by_key(part_number)` 改 `Reverse` | `multipart_uploads_sorted_parts_and_completes` → **0 passed / 1 failed** → 已恢复 |
| 4 | 预签名 STS token 不再进 query 参数 | `cargo test -p agentscope-extensions-cos presign_` → **5 passed / 1 failed**（`presign_includes_the_session_token_as_a_query_parameter`）→ 已恢复 |

### 验证结果

- `cargo test -p agentscope-extensions-cos` → **105 passed / 0 failed / 0 ignored**（+3 个 `no_run` doctest，0 failed）
- `cargo clippy -p agentscope-extensions-cos --all-targets` → 本 crate **零告警**；`cargo fmt -p agentscope-extensions-cos -- --check` → **通过**

### 语义差异 / 未做（如实记录）

- **Java 模块自身没有 multipart / presign 代码**（只用 `COSClient` 单次 `putObject`，单对象上限 5 GiB）。因此本次不是“照搬 Java 实现”，而是**对齐 Java 侧所依赖的 SDK 默认行为**（`TransferManagerConfiguration` / `TransferManagerUtils` 字节码证据见 §3.3.1）+ COS 服务端限制。
- 并发默认 1（对齐 `createDefaultExecutorService` 的单线程池）；需要并行时显式 `multipart_concurrency(n)`（`JoinSet` 按 chunk 限并发）。
- 只签 `host`（预签名），因此不支持需要客户端发送自定义头的场景；`response-*` 类参数可经 `presign_url` 的 `params` 扩展（`presign_get/put` 目前不暴露——保持 API 最小）。
- STS **不做自动刷新**：token 过期后调用方需 `with_credentials(...)` 换新（共享连接池与 `closed` 标志）。
- 无 `ListParts` 断点续传、无分片级重试（失败即 abort 回滚）、无上传进度回调、`Upload Part`/`Complete` 不发送 `Content-MD5`（COS 文档标可选）；内存峰值约 2× 对象大小（整体副本 + 单片副本），未做真正流式上传。
