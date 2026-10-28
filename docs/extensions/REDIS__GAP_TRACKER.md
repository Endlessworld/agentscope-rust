# REDIS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **REDIS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-redis\`（旧记 `D:\IdeaProjects\...` 已失效，2026-10-22 静态复核更正）
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state`（含 `jedis` / `lettuce` / `redisson` 子包）/ `store`）
> **Rust 目标**：`crates/agentscope-extensions-redis/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

> **当前结论（2026-10-22 静态复核更新）**：P0 组件（state / store / snapshot / sandbox-guard / combiner）均已落地并接线；上一轮补齐 **离线测试覆盖**（`RedisClientAdapter` / `RedisCommands` / `StoreCommands` 三层内存替身）、**`base_store()` 自动接线**（P1）、**`RedisSandboxLease::released` 接线**（P2，已无 warning）。本轮为**静态复核（禁编译、未跑 `cargo build` / `cargo test`）**：实测 Rust `#[test]` + `#[tokio::test]` = **113 个**（上一会话记录的 92 为旧快照，差异见 §6 勘误 E2）；`find_keys_by_pattern` **确为 SCAN 游标**语义、`base_store()` **确为 `connect_lazy` 自动接线**、`released` **确为 `AtomicBool`**、三层内存替身与 `RedisCommands` / `StoreCommands` 两条缝**均属实存在**。剩余：**真实 Redis 集成测试**（环境受限，待办，见 §4.2）+ **1 项 P1 真实语义分歧**（`save_if_version(expected_version=0)` 与 core 契约冲突，见 §3.3 / §6）。

> **2026-10-28 第一轮更新（执行会话；P1-3 结论已被下方第二轮取代）**：P1-1（`save_if_version(0)`）已由汇总方修复（§7.1）；**P1-2（SCAN 游标循环零单测）已闭合**——游标循环 + 节点扇出抽成 crate 内缝并新增 12 个离线用例（§8.1）；**P1-3（Cluster 下 SCAN 漏键）完成「可离线验证的扇出层」**，真实 Cluster 路径**阻塞于驱动 API**（`redis` 1.7 无 primary 枚举接口，且本 crate 未启用 `cluster`/`cluster-async` feature，见 §8.2）。实测 **126 passed / 0 failed** + 2 doc-tests。

> **2026-10-28 第二轮更新（执行会话，P1-3 闭合）**：**无需 `cluster` feature**——改用 `CLUSTER SLOTS` 手写拓扑发现：新增 `parse_cluster_slots`（纯函数）、`ClusterTopologySource` / `NodeConnectionFactory` 两条缝、`ClusterScanNodeSource`（每 primary 一条独立单机连接 → 既有 `collect_keys_on_node`）与生产接线 `RedisConnection::connect_cluster(seed_url)`。新增 **19** 个离线用例（拓扑解析 11 + 扇出 5 + URL/端点/种子主机 3）。实测 **148 passed / 0 failed**（lib）+ **2 passed** doc-tests，本 crate **0 warning**，`fmt --check` 干净。**剩余边界**：真实 Cluster 集成测试仍缺（§4.2 缺口 A）；本 crate 不做 slot 路由（单键命令仍只到 seed 节点）。详见 §8.5 / §8.6。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-redis` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state`（含 `jedis` / `lettuce` / `redisson` 子包）/ `store`） |
| Java 规模（实测，2026-10-22 复核，非空行） | 主代码 **3167 行**（**14 个类**）/ 测试 **1038 行**（**7 个测试类**，共 **26 个 `@Test`**；含 2 个真实 Redis 集成测试类：`RedisAgentStateClusterIntegrationTest`（testcontainers，3 用例）、`RedisSessionStorageIntegrationTest`（`@EnabledIfEnvironmentVariable`，1 用例））。**计数口径**：非空行，以 UTF-8 `ReadAllLines` 读取（`Get-Content` 口径因 2 个文件末尾空行差异得 3165，见 §6 勘误 E1） |
| Rust crate | `crates/agentscope-extensions-redis` |
| 迁移波次 | B |
| 当前状态 | **基本完成**（**2026-10-28 第二轮实测**：`cargo build -p agentscope-extensions-redis` 通过、本 crate **0 warning**；`cargo test -p agentscope-extensions-redis` = **148 passed / 0 failed**（lib）+ **2 passed** doc-tests，`cargo fmt --check` 干净）。P1-1（§7.1）、P1-2（§8.1）、**P1-3（Cluster 多主 SCAN，§8.5）均已闭合**。剩余缺口：真实 Redis / 真实 Cluster 集成测试（§4.2 缺口 A）；本 crate 不做 Cluster slot 路由（§8.6） |

### 1.1 基线（2026-09-19 收尾；2026-10-22 静态复核订正，本会话实测）
| 项 | 值 |
|---|---|
| `cargo build -p agentscope-extensions-redis` | **2026-10-28 实测**：通过，**本 crate 0 warning**（仅 `agentscope-harness` 侧 5 条 pre-existing warning） |
| `cargo test -p agentscope-extensions-redis` | **2026-10-28 第二轮实测**：**148 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。上一节实测 137；本轮 **+19**，全部在 `state/connection.rs` |
| 用例数演进 | **21 → 92 → 113（静态计数少 1，见 §8.4）→ 114（§7.1）→ 126（§8.1）→ 137（DistributedStore 投影）→ 148（§8.5 本轮，+19 = 拓扑解析 11 / 扇出 5 / 端点与 URL 3）** |
| 源码行数（实测，2026-10-28） | **12 个 `.rs` 文件 / 6457 物理行 / 5762 非空行**（含 `#[cfg(test)]`；2026-10-22 快照为 4636 物理行）；另 `Cargo.toml` 20 行（未改） |
| 未做 | 真实 Redis 集成测试（无 server 环境），见 §4.2 |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `redis.RedisDistributedStore`（116 行） | 组合 4 组件为 harness `DistributedStore`；`fromJedis(UnifiedJedis[, keyPrefix[, keyLayoutVersion]])` | `distributed_store::RedisDistributedStore`（`src/distributed_store.rs`） | 保留构造时的 **URL** + adapter，6 个组件工厂方法全部自动接线：`agent_state_store()` / **`base_store()`（已可用，无需 URL 参数）** / `sandbox_snapshot_spec()` / `sandbox_snapshot_spec_dyn()` / `snapshot_client()` / `sandbox_execution_guard()`；`to_distributed_store()` 投影到 harness（**已解决（2026-10-28）**：5 槽全量投影 —— state store + isolation scope + base store + snapshot spec + guard）。新增 `with_url(url)` 让 `from_connection` / `from_components` 路径也能重新启用 `base_store()`；`with_key_layout_version()` / `url()` / `adapter()` 访问器。降级路径见 §3.2 |
| `redis.state.RedisAgentStateStore`（525 行） | 会话状态：单值 JSON String + 版本计数（`:ver`）+ 列表（`:list`）+ 列表哈希（`:list:_hash`）+ 键集（`:_keys`）；CAS/版本；V0/V1 键布局开关 | `state::RedisAgentStateStore`（`src/state/mod.rs`，`impl agentscope_core::state::AgentStateStore`） | 覆盖 `save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`，**并已实现 core trait 的 `supports_versioning`/`get_versioned`/`save_if_version`（A2 上提后，2026-10 新增）**，另保留同名固有方法（`u64` 版本域）；`clear_all_sessions`；`RedisAgentStateStoreBuilder` 接 `Arc<dyn RedisClientAdapter>` + `key_prefix` + `key_layout_version` |
| `redis.state.RedisAgentStateKeyLayout`（268 行，包私有） | V0/V1 键布局（V1 用 Redis hash tag `{user/session}` 保证 Cluster 同 slot）、`v0KeysPattern`/`v1KeysPattern` glob 转义、`parseV0/V1SessionIdFromKeysKey`、V1 标识符校验 | `state::key_layout::RedisAgentStateKeyLayout`（`src/state/key_layout.rs`，`pub(crate)`）+ `pub use KeyLayoutVersion` | **新增对应（原 tracker 漏列）**：`KeyLayoutVersion{V0,V1}`、`normalize_user`/`ANON_USER`、`escape_redis_glob_literal`、`v0_keys_pattern`/`v1_keys_pattern`、`parse_*_session_id_from_keys_key`、`validate_v1_*`；**9 个单测**覆盖 hash tag 同 slot、V0/V1 命名空间互斥、glob 转义、非法标识符拒绝 |
| `redis.state.RedisClientAdapter`（126 行） | client-agnostic 适配接口（`set`/`get`/`rightPushList`/`rangeList`/`getListLength`/`deleteKeys`/`addToSet`/`getSetMembers`/`getSetSize`/`keyExists`/`findKeysByPattern`/`evalScript`/`close`） | `state::adapter::RedisClientAdapter`（`src/state/adapter.rs`，`#[async_trait]` trait） | 方法名与 Java 1:1；`close()` 提供默认 no-op 实现 |
| `redis.state.RedisStateVersionSupport`（90 行） | `VERSION_SUFFIX=":ver"`、`UNCONDITIONAL="-1"`、`SAVE_SCRIPT` Lua、`versionKey`/`parseVersion`/参数拼装 | `state::version_support`（`src/state/version_support.rs`） | `SAVE_SCRIPT` 逐字节移植（Java 脚本内 `current_version()` 含一段冗余分支，行为等价，见 §3.4）；`UNCONDITIONAL="-1"`、`parse_version`、`version_key` 对齐 |
| `redis.state.jedis.JedisClientAdapter`（155 行） | Jedis 适配（`UnifiedJedis`/`RedisClient`/`RedisClusterClient`/`RedisSentinelClient`），`findKeysByPattern` 用 `scanIteration(1000, pattern).collect(Set)`（cluster 全主节点遍历） | → 合并进 `state::connection::RedisConnection`（`src/state/connection.rs`） | Rust 单一 async 驱动，见 §3.4。**`find_keys_by_pattern` 已改为 `SCAN <cursor> MATCH <pattern> COUNT 100` 游标循环 + `HashSet` 去重**，与 Java 三个适配器一致（不再用 `KEYS`）。**差异**：Java Jedis 用 `scanIteration`（跨所有 master），Rust 单 `ConnectionManager` 的 `SCAN` 只覆盖所连节点 → Cluster 下会漏键（P1，见 §6）→ **2026-10-28 已补「节点扇出层」（`ScanNodeSource` + `collect_keys_across_nodes`，离线可测）；真实 Cluster 连接的 primary 枚举改由 `CLUSTER SLOTS` 手写实现（`ClusterScanNodeSource` + `RedisConnection::connect_cluster`，无需 `cluster` feature，§8.5）** |
| `redis.state.lettuce.LettuceClientAdapter`（324 行） | Lettuce 适配（standalone/sentinel + cluster 两套命令 API），`ScanCursor.INITIAL` → `isFinished()` | → 同上（合并） | 不迁移 Lettuce reactive 栈 |
| `redis.state.redisson.RedissonClientAdapter`（223 行） | Redisson 适配（Standalone/Cluster/Sentinel/Master-Slave），`KeysScanOptions.defaults().pattern(...)` | → 同上（合并） | 不迁移 Redisson 分布式对象 API |
| `redis.state.jedis.JedisAgentStateStore`（368 行，`@Deprecated`） | 基于 `JedisPool` 的旧会话实现（含 `clearAllSessions()` → `KEYS` + `DEL`，返回 `Mono<Integer>`） | 设计决定（不迁移） | Java 已弃用并指向统一的 `RedisAgentStateStore`；见 §3.4 |
| `redis.state.redisson.RedissonAgentStateStore`（356 行，`@Deprecated`） | 基于 `RedissonClient` 的旧会话实现 | 设计决定（不迁移） | 同上 |
| `redis.store.RedisStore`（261 行） | 命名空间 KV + CAS + `ZRANGEBYLEX` 前缀搜索；item hash（`value`/`version`）+ idx sorted set；Lua `PUT`/`PUT_IF_VERSION`/`DELETE`；`putIfVersion(expectedVersion<0)` 抛 `IllegalArgumentException` | `store::RedisStore`（`src/store.rs`，`impl agentscope_harness::filesystem::remote::store::BaseStore`） | `put_if_version` 对齐 harness CAS（`expected==0` ⇒ create-only-if-absent；harness 签名是 `u64`，**负值在类型层不可表达**，故 Java 的 `IllegalArgumentException` 分支自然消失）；`RedisStore::connect(url, prefix)`（**急躁连接 + `PING` 验证**）/ `connect_lazy(url, prefix)`（**懒连，构造时零 I/O**）/ `from_connection`；同步阻塞实现（`BlockingRedisCommands` 持有 `Mutex<Option<redis::Connection>>`，首次使用时建连）。底层经 crate-internal `StoreCommands` 缝，测试可注入内存替身 |
| `redis.snapshot.RedisRemoteSnapshotClient`（80 行） | 快照上传/下载/存在性（Redis binary value，key `{prefix}{snapshotId}.tar`，可选 TTL 秒） | `snapshot::RedisRemoteSnapshotClient`（`src/snapshot.rs`，`impl RemoteSnapshotClient`） | async；`download` 缺失返回 `Error::Io(NotFound)`（对应 Java `FileNotFoundException`） |
| `redis.snapshot.RedisSnapshotSpec`（34 行，`extends RemoteSnapshotSpec`） | Redis 快照便捷 spec | `snapshot::RedisSnapshotSpec`（newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`） | 快照 id 校验沿用 harness `RemoteSnapshotSpec::build` |
| `redis.sandbox.RedisSandboxExecutionGuard`（241 行） | `SET NX PX` 分布式租约 + Lua token CAS 释放；`composeKey = <prefix><scope_lower>:<value>` | `sandbox::RedisSandboxExecutionGuard` + `RedisSandboxExecutionGuardBuilder` + `RedisSandboxLease` + 别名 `SharedRedisSandboxExecutionGuard`（`src/sandbox.rs`） | harness `SandboxExecutionGuard` 是**具体 struct 非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），故为镜像 API（`try_enter`/`release`/`is_held`）的独立类型；租约 `released: AtomicBool` **已接线**（`is_released()` 访问器 + 幂等 `release()` + `Drop` 用 `swap` 占用释放权），无 dead-code warning；`Drop` 有 runtime 时 `spawn`、无 runtime 时 `block_on` 降级；常量对齐（30min TTL / 500ms retry / `agentscope:sandbox:lock:`） |
| （Java core）`io.agentscope.core.state.ListHashUtil` | 列表变更检测（size 前缀 + 采样 hash） | **已收敛到 core**：复用 `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`crates/agentscope-core/src/state/list_hash.rs`，经 `state/mod.rs` `pub use`） | 本地平行实现 `src/state/list_hash.rs` **已删除**（2026-09-19）；`serde_json::Value` 实现 `Hash`，core 泛型 `compute_hash<T: Hash>` 直接接收 `&[Value]`（与 JDBC/PostgreSQL 调用方式一致），无需本地 JSON 串中转 |
| （测试替身）Java 用 Mockito mock `RedisClientAdapter` / `UnifiedJedis` / `JedisPool` / `RedissonClient` + testcontainers 真实 Redis | 离线单测（`state/RedisAgentStateStoreTest.java` 328 行/15 用例、`state/jedis/JedisAgentStateStoreTest.java` 67 行/1、`state/redisson/RedissonAgentStateStoreTest.java` 79 行/1、`state/jedis/JedisClientAdapterTest.java` 85 行/3、`RedisDistributedStoreTest.java` 47 行/2）+ 集成测试（`state/RedisAgentStateClusterIntegrationTest.java` 322 行/3、`store/RedisSessionStorageIntegrationTest.java` 110 行/1） | crate 内 `#[cfg(test)]` + **内存替身**（`src/test_support/mod.rs`：`MemoryRedisAdapter` / `MemoryRedisCommands` / `MemoryStoreCommands`） | 静态实测 **113 个测试函数**（上一会话记录 92，差 21，见 §6 勘误 E2），全部离线、零网络。三层替身镜像真实命令语义（含 `SAVE_SCRIPT` / `PUT_SCRIPT` / `PUT_IF_VERSION_SCRIPT` / `DELETE_SCRIPT` / `RELEASE_SCRIPT` 五个 Lua 脚本的内存解释器与 `*`/`?` glob）。详见 §3.1 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `redis.clients:jedis`（`pom.xml`） | Standalone/Cluster/Sentinel 客户端 | `redis = { version = "1.7", default-features = false, features = ["script", "tokio-comp", "connection-manager"] }` + `redis::aio::ConnectionManager`（本 crate `Cargo.toml` 以 `redis = { workspace = true }` 引用） | **勘误**：版本**已**写入根 `[workspace.dependencies]`（根 `Cargo.toml:71`），本 crate 只 `workspace = true`，不再本地固定版本（原记录「未写入」已失效，见 §6 勘误 E5） |
| `org.redisson:redisson`（`pom.xml`） | 统一客户端 + 分布式对象（`RBucket`/`RList`/`RScript`/`RSet`/`RKeys`） | 同上（合并为 `redis` 驱动） | 不迁移 Redisson 专属 API 与编解码（`StringCodec`） |
| `io.lettuce:lettuce-core`（`pom.xml`） | reactive/async 客户端（standalone/sentinel/cluster 双命令 API） | 同上（合并） | Lettuce reactive 栈不对应迁移 |
| `com.fasterxml.jackson.databind.ObjectMapper` / `io.agentscope.core.util.JsonUtils` | JSON 序列化 | `serde_json` | store 侧 `StoreValue = serde_json::Map<String, serde_json::Value>`；状态值 `serde_json::Value` |
| `reactor.core.publisher.Mono` + `Schedulers.boundedElastic()` | `clearAllSessions()` 异步包装 | `async fn clear_all_sessions() -> Result<usize>`（`src/state/mod.rs`） | Rust 直接 async，无 reactive 包装 |
| `org.slf4j.Logger`（guard 有 debug/warn 埋点） | 日志 | `tracing` | state store 在 `save`/`save_list`/`delete`/`list_session_ids`/`exists` 失败路径有 `tracing::error!`；`sandbox.rs` 有 `debug!`/`warn!`；store/snapshot/connection 无埋点 |
| `RuntimeException` / `IllegalArgumentException` / `FileNotFoundException` | 错误传播 | `agentscope_core::error::Error`（主要为 `Error::Other`；快照缺失 `Error::Io(NotFound)`） | 无本模块专用 error 枚举 |
| `java.time.Duration` / `java.util.UUID` | 租约 TTL / 锁 token | `std::time::Duration` / `uuid` crate（`Uuid::new_v4()`） | 常量对齐：`DEFAULT_LEASE_TTL=30min`、`DEFAULT_RETRY_INTERVAL=500ms` |
| `java.io.InputStream` / `byte[]`（快照） | 快照数据流 | `&[u8]` / `Vec<u8>`（`RemoteSnapshotClient` 签名） | — |
| JUnit5 + Mockito + testcontainers（**7 个测试类 1038 行 / 26 个 `@Test`**） | 离线测试 + 真实 Redis/Cluster 集成测试 | Rust 内置 `#[cfg(test)]` + **三层内存替身**（**113 个测试函数**） | 覆盖 trait 路径、五个 Lua 脚本与全部组件；见 §3.1。**未覆盖**：真实 Lua 在 Redis 中执行、真实多轮 `SCAN` 的网络往返（游标语义本身自 2026-10-28 起由离线用例覆盖，§8.1）、Cluster/Sentinel 模式（Java 侧有 4 个集成用例，Rust 侧为 0） |

**harness / core 复用契约（不改 harness/core，对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1）**：
- `impl agentscope_core::state::AgentStateStore`（`src/state/mod.rs`；trait 定义 `crates/agentscope-core/src/state/store.rs:35`，签名用 `Option<&str>` 与 `serde_json::Value`；`UNVERSIONED = -1` 在 `:12`）。**A2 上提后** core trait 已含 `supports_versioning`（`:68`）/`get_versioned`（`:76`）/`save_if_version`（`:98`，`expected_version: i64`，`0` = create-if-absent、`UNVERSIONED` = 无条件覆盖），本 crate 已一并实现（`src/state/mod.rs:346,350,368`）。
- `impl agentscope_harness::filesystem::remote::store::BaseStore`（`src/store.rs`；`StoreItem`/`BaseStore` 定义 `crates/agentscope-harness/src/filesystem/remote/store.rs:21,34`；CAS 语义以 harness 为准，`StoreItem.version` 从 1 起单调递增）。
- `impl RemoteSnapshotClient`（`src/snapshot.rs`；trait 定义 `crates/agentscope-harness/src/agent/sandbox/snapshot.rs:134`）。
- `impl SandboxSnapshotSpec`（`src/snapshot.rs`；trait 定义 `crates/agentscope-harness/src/agent/sandbox/snapshot.rs:242`，包装 `RemoteSnapshotSpec` 见 `:274`）。
- `SandboxIsolationKey` 使用 harness 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`（`crates/agentscope-harness/src/agent/sandbox/key.rs:11`），**不得**用私有 `sandbox::key::`。
- `to_distributed_store()` → harness `DistributedStore`（`src/distributed_store.rs`；struct 定义 `crates/agentscope-harness/src/agent/distributed_store.rs:14`，2026-10-28 起含 `agent_state_store` + `isolation_scope` + `base_store` + `sandbox_snapshot_spec` + `sandbox_execution_guard`）。
- `RedisSandboxExecutionGuard`：harness `SandboxExecutionGuard` 是具体 struct（`.../sandbox/guard.rs:13`），但 harness 另有 `SandboxGuard` trait（`guard.rs:14`）——2026-10-28 起本 crate 已 `impl SandboxGuard`。**2026-10-28 修复（M1）**：该 trait 的 `try_enter`/`release` 已改为 `async`，本 crate 直接 `await` 内部的 `SET NX PX` 轮询（**删除 `futures::executor::block_on` 桥**，不再有 runtime 限制），`is_distributed()` 返回 `true`。

**client-agnostic 适配层设计**：`RedisClientAdapter`（`src/state/adapter.rs`）保留 Java 的抽象意图，使 `RedisAgentStateStore` 只依赖 `Arc<dyn RedisClientAdapter>`（`src/state/mod.rs` 的 `client` 字段），从而支持自定义后端/内存替身。Rust 生态单一主流 async 驱动 `redis`，故只提供一个规范化实现 `RedisConnection`（`ConnectionManager`，`Clone` 廉价、可 `Arc` 共享，`src/state/connection.rs`）；Java 的 Jedis/Lettuce/Redisson 三套适配器合并为其一。

**三条命令缝（seam）与生产实现**（均为 crate-internal，`pub(crate)`，不属于公开 API；生产实现 `ManagerRedisCommands` 对外 re-export）：

| 缝 | 覆盖组件 | 生产实现 | 测试替身 |
|---|---|---|---|
| `state::adapter::RedisClientAdapter`（async） | `RedisAgentStateStore` | `RedisConnection`（`ConnectionManager`） | `test_support::MemoryRedisAdapter` |
| `raw_client::RedisCommands`（async） | `RedisRemoteSnapshotClient` / `RedisSandboxExecutionGuard` | `raw_client::ManagerRedisCommands` | `test_support::MemoryRedisCommands` |
| `store::StoreCommands`（**同步**） | `RedisStore` | `store::BlockingRedisCommands`（懒连 `Mutex<Option<redis::Connection>>`） | `test_support::MemoryStoreCommands` |

引入缝的原因：`redis` crate 的 async 命令 future 需真实 socket/IO driver 才能 poll（经实测，lazy `ConnectionManager` 亦要求 Tokio reactor），无法离线驱动。缝将命令面收窄到各组件真实所需，使全部组件都能用内存替身跑完整分支。`RedisDemo` 的 `raw_client()` / `raw_adapter()` 是 `RedisConnection` 向外分发这两条缝的唯一入口。

**同步/异步桥接**：harness `BaseStore` 为同步 trait，而 `redis` 是 async 驱动 → `RedisStore` 走**同步** `BlockingRedisCommands`（独立阻塞连接，`connect_lazy` 构造时零 I/O；`connect` 额外 `PING` 验证），**不在同步 trait 里起 runtime**。`RedisSandboxLease::drop` 无 runtime 时用 `futures::executor::block_on` 桥接（有 runtime 时 `spawn`）。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib 导出 | `RedisDistributedStore` 等 | 已完成 | `src/lib.rs` 声明 4 个 pub 模块（`sandbox`/`snapshot`/`state`/`store`）+ 私有 `distributed_store`，re-export 全部公共类型 |
| `RedisAgentStateStore` + builder（impl core `AgentStateStore`） | `state/RedisAgentStateStore.java` | 已完成 | 键布局对齐（`:ver`/`:list`/`:_hash`/`:_keys`；默认前缀 `agentscope:session:`）；单值经 `SAVE_SCRIPT` CAS，列表走 hash 变更检测 |
| `RedisClientAdapter` 抽象 + `RedisConnection` 实现 | `state/RedisClientAdapter.java` + `jedis`/`lettuce`/`redisson` 适配器 | 已完成（三适配器合并为 1，见 §3.4） | 方法签名 1:1 对齐 |
| `RedisStateVersionSupport`（Lua CAS 原语） | `state/RedisStateVersionSupport.java` | 已完成 | `SAVE_SCRIPT` 逐字节移植；`UNCONDITIONAL="-1"`、`VERSION_SUFFIX=":ver"` 一致 |
| `RedisStore`（impl harness `BaseStore`） | `store/RedisStore.java` | 已完成 | Lua `PUT`/`PUT_IF_VERSION`/`DELETE`；`ZRANGEBYLEX` + `LIMIT` 分页搜索；`NUL` 命名空间分隔符一致 |
| `RedisRemoteSnapshotClient`（impl `RemoteSnapshotClient`）+ `RedisSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/RedisRemoteSnapshotClient.java`、`snapshot/RedisSnapshotSpec.java` | 已完成 | key `{prefix}{snapshotId}.tar`（默认前缀 `agentscope:sandbox:snapshots:`），可选 TTL |
| `RedisSandboxExecutionGuard` + lease + builder | `sandbox/RedisSandboxExecutionGuard.java` | 已完成（语义差异见 §3.2/§3.3） | `SET NX PX` 轮询 + Lua token CAS；`Drop` 释放；`compose_key` 格式一致 |
| `RedisDistributedStore` 组合者 | `RedisDistributedStore.java` | 已完成（`base_store()` 已自动接线，见 §3.2） | 保留 URL + adapter；6 个组件工厂方法 + `to_distributed_store()`；`with_url()` / `url()` / `adapter()` |
| 依赖声明（`jedis`/`redisson`/`lettuce-core`） | `pom.xml` | 已完成 | 本 crate 仅 `redis = "1.7"`（features `script`/`tokio-comp`/`connection-manager`）；`serde`/`serde_json`/`thiserror`/`async-trait`/`tokio`/`futures`/`tracing`/`chrono`/`uuid` 取 workspace |
| **离线单元测试** | Java **7 个测试类 / 1038 行 / 26 个 `@Test`**（5 个 Mockito 离线类 606 行 + 2 个真实 Redis 集成类 432 行） | **已完成** | 上一会话记录 `cargo test -p agentscope-extensions-redis` = **92 passed / 0 failed**（+ 2 doc-tests）；**2026-10-22 静态复核实测测试函数体 = 113 个**（`#[test]` 52 + `#[tokio::test]` 61），分布：`src/state/mod.rs`(**50**)、`src/store.rs`(**20**)、`src/sandbox.rs`(**13**)、`src/snapshot.rs`(**9**)、`src/distributed_store.rs`(**9**)、`src/state/key_layout.rs`(**9**)、`src/state/version_support.rs`(**2**)、`src/state/connection.rs`(**1**)，合计 **113**（原 92 快照分布表已过期，见 §6 勘误 E3）。全部离线、零网络。三层内存替身见 §2 的「三条命令缝」表；`src/state/adapter.rs` 新增 SCAN 语义契约文档注释（保留上一会话改动）。**2026-10-28 第二轮实测 = 148 passed / 0 failed**，分布（实测 `#[test]`/`#[tokio::test]`）：`src/state/mod.rs`(**53**)、**`src/state/connection.rs`(30)**、`src/store.rs`(**20**)、`src/sandbox.rs`(**14**)、`src/distributed_store.rs`(**11**)、`src/snapshot.rs`(**9**)、`src/state/key_layout.rs`(**9**)、`src/state/version_support.rs`(**2**) = **148**（2026-10-22 的 113 系静态计数，比实测少 1，见 §8.4）。

**测试替身覆盖面**：`MemoryRedisAdapter`（`set`/`get`/`rightPushList`/`rangeList`/`getListLength`/`deleteKeys`/`addToSet`/`getSetMembers`/`getSetSize`/`keyExists`/`findKeysByPattern`/`evalScript`）、`MemoryRedisCommands`（`setBytes`/`getBytes`/`exists`/`expire`/`setNxPx`/`evalOneKey`/`del`）、`MemoryStoreCommands`（`hgetall`/`evalTwoKeys`/`zrangebylex`/`ping`）。五个 Lua 脚本（`SAVE_SCRIPT`/`PUT_SCRIPT`/`PUT_IF_VERSION_SCRIPT`/`DELETE_SCRIPT`/`RELEASE_SCRIPT`）在替身内有语义等价的解释器；`*`/`?` glob 用于 `find_keys_by_pattern`。**2026-10-28**：`MemoryRedisAdapter::find_keys_by_pattern` 不再是「全量键 glob 过滤」，而是**驱动生产游标循环**（`collect_keys_across_nodes`），并新增 `with_scan_page_size(n)`（限制每页键数，强制多轮游标）与 `scan_cursor_history()`（断言游标链）：替身自身即 `ScanNodeSource`，`MemoryScanPager` 按「游标 = 键快照内位置」建模真实 SCAN 分页。

**用例清单（按行为分类）**：
- **state**：`save`/`get`/`exists`/`delete`/`list_session_ids`（含匿名 slot、前缀碰撞 `u1` vs `u10`）、`save_list` 四分分支（首次写不 DEL / 前缀未变 no-op / 追加只 push 后缀 / 前缀改写与缩短触发全量重写）、`:_hash` 持久化、legacy 无 hash 全量重写、`save_if_version` 三分支（0 / 匹配 / 冲突返回 0）、`get_versioned`、`supports_versioning`、`clear_all_sessions`。
- **store**：`put`/`get` 往返 + 版本递交 + 单一 idx 成员、`put_if_version` 三分支（absent-create / 匹配 / stale 拒绝）、`search` 前缀分页 / 零 limit / 未知 namespace / stale idx 跳过、`delete` 移除 item + idx、非法 key 零 I/O、不可达后端降级、损坏 JSON 降级。
- **snapshot**：`upload`/`download` 往返（含二进制 `\x00\xff`）、TTL 应用与缺失告警、缺失返回 `Error::Io(NotFound)`、空白 id 零 I/O 拒绝、后端失败包装上下文、`RedisSnapshotSpec` id 校验（空白 / `a/b` / `..`）与快照往返。
- **sandbox guard**：`try_enter` 写 NX+PX 与唯一 token、显式 `release` 幂等（第二次 / `Drop` 不重跑脚本）、`Drop` 释放、**竞争自旋**（第二持有者被阻塞且 `set_nx_calls > 1`，释放后可获得）、**外来 token CAS**（不删他人锁）、`set_nx` 失败传播、guard 级 `release`/`is_held`、TTL 覆盖、**无 runtime 下 `Drop` 阻塞降级**。
- **distributed_store**：`from_connection` 无 URL、`with_url` 后 `base_store()` 懒接成功、无 URL 降级错误文案、state store 前缀、`to_distributed_store()` 投影 + isolation scope、组件工厂离线可构造。

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `RedisDistributedStore::base_store()` 自动接线 | `RedisDistributedStore.baseStore()`（返回 `new RedisStore(jedis, keyPrefix+"store:")`，共享同一 `jedis`） | **已完成** | 组合者现保留构造时的 **URL + adapter**（新增字段 `url: Option<String>`、`adapter: Arc<dyn RedisClientAdapter>`）；`base_store()` 用保留的 URL 经 `RedisStore::connect_lazy(url, "{root}store:")` 自动接线，**构造时零 I/O**（懒连），不再返回 `Err`。`from_connection` / `from_components` 路径原本无 URL → 新增 `with_url(url)` 链式方法恢复自动接线；仍无 URL 时返回**文档化降级错误**（文案含 `no Redis URL retained` 与 `base_store_with_url` 指引），调用方可继续用 `base_store_with_url(url)`（后者为**急躁连接 + `PING` 验证**）。`BaseStore` 为同步 trait，无法直接复用 async `ConnectionManager`，故仍为独立阻塞连接（与 Java「同一 jedis 实例」的差别保留但已透明；跨连接仅影响连接数，不影响键布局/语义）。详见 §4.1 用例清单 |
| `supportsVersioning()` / `getVersioned()` | `state/RedisAgentStateStore.java`（`supportsVersioning`、泛型 `<T extends State> VersionedState<T> getVersioned(..., Class<T>)`） | 已完成（降为固有方法） | core `AgentStateStore` trait 无版本方法，故保留为固有 `supports_versioning() -> bool` 与 `get_versioned(...) -> Result<VersionedState>`；`VersionedState { value: Option<Value>, version: u64 }` |
| `saveIfVersion()` | `state/RedisAgentStateStore.java` | 已完成（固有方法） | `save_if_version(..., expected_version: u64)`，冲突返回 `0`；`expected==0` 时转为无条件 bump（见 §3.3 待确认） |
| Cluster 下 `SCAN` 遍历全部 primary | `JedisClientAdapter.findKeysByPattern`：`unifiedJedis.scanIteration(1000, pattern)` —— Java 注释明确「iteration traverses every master in cluster mode」 | **已完成（2026-10-28 第二轮；离线可测 + 生产接线，仅缺真实 Cluster 集成测试）** | **拓扑发现**：`CLUSTER SLOTS`（`redis::cmd("CLUSTER").arg("SLOTS")`）→ 纯函数 `parse_cluster_slots(&redis::Value, fallback_host)` 提取每个 slot range 的 primary（`parts[2]` 的 `[host, port]`），按 (host,port) 去重并保持首次出现顺序；host 为空/NIL 时回退到种子连接主机（Redis 语义「你连我用的地址」）。**扇出**：`ClusterScanNodeSource` 为每个 primary 建**独立单机连接**（`ConnectionScanPager`），逐节点跑既有 `collect_keys_on_node` 全游标循环并 union 去重；**任一 primary 连接失败/某页失败 → 整体 `Err`**（对齐 Java `scanIteration` 直接抛驱动异常，不返回静默残缺结果）。**每次枚举都重读拓扑**（不缓存）→ failover / resharding 自动生效。**生产入口**：`RedisConnection::connect_cluster(seed_url)`（`find_keys_by_pattern` 走扇出；`is_cluster_scan()` 可自检）。**无需 `cluster`/`cluster-async` feature**，未改根 `Cargo.toml`。**剩余边界**：①无真实 Cluster 集成测试；②本 crate 不做 slot 路由（MOVED/ASK），单键命令只到 seed 节点（Java `RedisClusterClient` 有路由）。`CLUSTER SLOTS` 报错 / 空拓扑的处理见 §8.6 |
| `clearAllSessions()` | **注意：Java 统一 `RedisAgentStateStore` 并无此方法**；仅两个 `@Deprecated` store 有：`state/jedis/JedisAgentStateStore.java:345`（`KEYS prefix*` + `DEL`，返 `Mono<Integer>`）、`state/redisson/RedissonAgentStateStore.java:328`（`RKeys.getKeysByPattern` + 逐键删，返 `Mono<Integer>`） | 已完成（Rust 侧扩展方法） | `clear_all_sessions() -> Result<usize>`（`src/state/mod.rs:221`），用 `prefix*` 模式经 `find_keys_by_pattern`（**SCAN**，非 `KEYS`）+ `DEL`。因统一 Java store 无对应方法，本项是 **Rust 侧扩展**而非 1:1 移植；实现方式与 Java 的 `KEYS` 路径语义等价但更安全（非阻塞） |
| `close()` | `state/RedisAgentStateStore.java`（`client.close()`） | **已完成（设计决定，不迁移）** | 依据：Rust `RedisConnection` 为 `Clone` + `ConnectionManager`，无所有权/关闭语义；`RedisClientAdapter::close()` 保留默认 no-op 实现供自定义后端覆盖 |
| `findKeysByPattern` 的 SCAN 游标语义 | `RedisClientAdapter.findKeysByPattern` 的三个实现（`state/jedis/JedisClientAdapter.java:155`、`state/lettuce/LettuceClientAdapter.java:249`、`state/redisson/RedissonClientAdapter.java:208`）**均基于 SCAN 游标**（Jedis `scanIteration(1000, pattern)`、Lettuce `ScanCursor.INITIAL → isFinished()`、Redisson `KeysScanOptions.defaults().pattern(...)`） | **已完成** | **2026-10-22 静态复核确认属实**：`RedisConnection::find_keys_by_pattern`（`src/state/connection.rs:174-195`）确为 `SCAN <cursor> MATCH <pattern> COUNT 100` 游标循环（`cursor` 初值 `0`，`next == 0` 退出，`cursor = next` 递进），累积到 `HashSet<String>` 去重（`SCAN_COUNT=100`），**无 `KEYS` 调用**；`state/adapter.rs:62-88` 的 trait 文档注释已写明该契约（glob、循环至游标回 `0`、去重）。**未覆盖**：该 SCAN 循环**无任何单测**（唯一 `state/connection.rs` 用例只测 `value_to_i64`）；`MemoryRedisAdapter::find_keys_by_pattern` 是「全量键 glob 过滤」替身，**不模拟游标**，故 SCAN 分支行为零覆盖（见 §6.5 **P1-2**）→ **2026-10-28 已闭合**：游标循环与节点扇出抽成 crate 内缝 `ScanPager` / `ScanNodeSource`（`src/state/connection.rs` 的 `collect_keys_on_node` / `collect_keys_across_nodes`），`RedisConnection::find_keys_by_pattern` 改为 `SingleNodeScanSource` → 扇出；替身改驱动生产循环且支持多页游标；新增 **10** 个 `connection.rs` 用例 + **2** 个 state 层用例（§8.1） |
| `RedisStore` 自定义 `ObjectMapper` 注入 | `store/RedisStore.java`（3 参构造器） | **已完成（设计决定，不迁移）** | 依据：Rust 固定 `serde_json`，无 DI/注入模型，`StoreValue = serde_json::Map` 已在 harness 固定 |
| 三个适配器（Jedis/Lettuce/Redisson）的部署模式矩阵 | `state/jedis`、`state/lettuce`、`state/redisson`（Standalone/Cluster/Sentinel/Master-Slave） | **已完成（设计决定，不迁移/合并）** | 依据：Rust 统一由 `redis::Client::open(url)` + `ConnectionManager` 承担，URL scheme 决定模式；**Cluster 键枚举已由 `CLUSTER SLOTS` 手写覆盖（§8.5，无需 feature）**，Sentinel 客户端与 Cluster 的 slot 路由仍需 `redis` crate 对应 feature（未启用），基础 Standalone 已覆盖。三套适配器的公共抽象（`RedisClientAdapter`）已保留为扩展缝 |
| `JedisAgentStateStore`（`@Deprecated`，368 行） / `RedissonAgentStateStore`（`@Deprecated`，356 行） | `state/jedis/`、`state/redisson/` | **已完成（设计决定，不迁移）** | 依据：Java 自身已 `@Deprecated` 并指向统一的 `RedisAgentStateStore`；功能已由本 crate 的单一实现覆盖 |
| Redisson 分布式对象（`RBucket`/`RList`/`RScript`/`RSet`/`RKeys`、`StringCodec`） / Lettuce reactive 栈 | 三个适配器内部 | **已完成（设计决定，不迁移）** | 依据：超出 `RedisClientAdapter` 抽象所需；Rust 侧由原生命令 + 本 crate 的 Lua 脚本承担 |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| `RedisSandboxLease::released` dead-code warning | `src/sandbox.rs` 字段 `released` 写入后从未读取，产生 dead-code warning | **已完成** | 已**接线**（未用 `#[allow]` 掩盖）：改为 `AtomicBool` 成为幂等释放的唯一事实源；新增 `is_released()` / `token()` 公开访问器；`release()` 先判已释放则直接返回，`Drop` 用 `swap(true, SeqCst)` 占用释放权（显式 `release()` 后 `Drop` 不重跑脚本）。`cargo build` 现 **0 warning** |
| `save_if_version(expected_version=0)` 的 CAS 语义差异 | **2026-10-22 静态复核定级为 P1 真实分歧**。三方对照：①Java `RedisAgentStateStore.saveIfVersion`：`expectedVersion == UNVERSIONED`（core `AgentStateStore.UNVERSIONED = -1L`）才无条件；`expected == 0` 走**真实 CAS**（拿 `0` 与已存版本比较，键不存在时 `current_version()` 返回 `0` 故命中）。②core trait 契约（`crates/agentscope-core/src/state/store.rs:88-97`）：`expected_version == 0` = **create-if-absent**，`UNVERSIONED`(-1) = 无条件覆盖。③Rust（`src/state/mod.rs:194-201` 固有方法 + `:368-392` trait 实现）：`expected_version == 0` 被当作**无条件 bump**（走 `UNCONDITIONAL_EXPECTED = u64::MAX`）。⇒ Rust 在 `0` 这一档上**同时偏离 Java 与 core 契约**（core 契约的 `0` 是 create-if-absent，Rust 实现是覆盖写）。**影响**：调用方若按 core 契约以 `0` 表达「仅当不存在时创建」，在 Rust 侧会**无条件覆盖**已存在的状态（丢更新）。**已定位单点**：`src/state/mod.rs:194`（`if expected_version == 0 { … UNCONDITIONAL_EXPECTED … }`）。**2026-10-22 §7.1 已修复并闭合**（`0` 走真实 CAS，`UNVERSIONED` 映射到内部哨兵） | **已完成（2026-10-22 §7.1）** |
| `find_keys_by_pattern` 使用 `KEYS` | 见 §3.2：**已完成** — `RedisConnection` 已改用 `SCAN` 游标循环 + 去重（`SCAN` 不会阻塞 Redis） | **已完成** |
| `list_hash` 每元素 hash 输入差异与收敛 | **已收敛到 core**（2026-09-19）：本地平行实现 `src/state/list_hash.rs` 已删除，改用 `agentscope_core::state::{compute_hash, needs_full_rewrite}`（泛型 `T: Hash` 直接接收 `&[Value]`）。core 亦用 `DefaultHasher`，**非** Java `hashCode`，故 hash 值仍与 Java 不互通。**存量数据影响**：收敛后算法（size 前缀 + 采样索引 + `DefaultHasher`）与旧本地实现**并非逐位一致**（旧实现 per-element hash 以 `Value::to_string()` 文本哈希后拼字符串；core 直接对 `Value` 调 `Hash`）→ 同一 Redis 实例中已存的 `:_hash` 与新算值不匹配，**首次 `save_list` 会走 Case3 全量重写一次**（`DEL` + `RPUSH` 原值），**数据无损、结果正确、自愈**；此后写入即稳定。跨语言 Java/Rust 混用同 slot 同理（仅多一次全量重写） | **已缓解（低风险，无需迁移脚本）** |
| `state::adapter::RedisClientAdapter::close()` | Java `close()` 为必需方法；Rust 提供默认 no-op 实现（未强制实现） | 已完成（语义弱化） |
| `RedisSandboxExecutionGuard::release()` 为 best-effort `DEL` | Java 无对应无 token 的 `release(key)`（仅 lease `close()` 做 token CAS）；Rust `release(&key)` 直接 `DEL`，可能误释放他人锁，仅作 parity | **待确认（待 Java 对齐裁定）** |
| store/snapshot/connection 无 `tracing` 埋点 | Java `RedisStore`/`RedisRemoteSnapshotClient` 无日志，guard 有 SLF4J debug/warn；Rust `sandbox.rs` 已对齐，store/snapshot 亦无（与 Java 一致） | 已完成（与 Java 对齐） |
| 真实 Redis 集成测试 | 当前无真实 Redis 实例测试；**113 个**单测全为**内存替身**离线测试 | **待办（环境受限，本轮不做，见 §4.2）** |

### 3.4 设计决定（不迁移）汇总
| 项 | Java 参考 | 裁定 | 理由 |
|---|---|---|---|
| `JedisAgentStateStore` | `state/jedis/JedisAgentStateStore.java`（368 行，`@Deprecated`） | **已完成（设计决定，不迁移）** | Java 自身已弃用并指向统一的 `RedisAgentStateStore`；功能已由本 crate 单一实现覆盖 |
| `RedissonAgentStateStore` | `state/redisson/RedissonAgentStateStore.java`（356 行，`@Deprecated`） | **已完成（设计决定，不迁移）** | 同上 |
| `JedisClientAdapter` / `LettuceClientAdapter` / `RedissonClientAdapter` | 三个适配器（共 712 行） | **已完成（设计决定，合并）** | Rust 生态单一 async 驱动；保留 `RedisClientAdapter` trait 作扩展缝 |
| Redisson 分布式对象 / Lettuce reactive 栈 | 上列适配器内部 | **已完成（设计决定，不迁移）** | 超出 `RedisClientAdapter` 抽象所需 |
| `Store` 自定义 `ObjectMapper`、`RedisAgentStateStore.close()` | 见 §3.2 | **已完成（设计决定，不迁移）** | Rust 无相应所有权/注入模型 |
| Java `RedisStateVersionSupport.current_version()` 内冗余分支 | `state/RedisStateVersionSupport.java`（`if EXISTS(KEYS[1]) return 0` 后仍 `return 0`） | 行为等价，Rust 略去 | 不影响 CAS 结果 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **REDIS 收尾会话（2026-09-19）**：完成三件事——**离线测试覆盖**（21 → **92**）、**`base_store()` 自动接线**（P1 闭合）、**`RedisSandboxLease::released` 接线**（P2 闭合，**0 warning**）。上一子智能体已完成的 `find_keys_by_pattern` SCAN 改造与 adapter 契约注释**已保留，未回退**。
- **REDIS 静态复核会话（2026-10-22，本会话）**：**未编译、未跑测试**（禁编译）。逐条核对声明 + 源码，结论见 **§6 静态复核记录**。本轮**未改任何 Rust 代码**，仅订正本 tracker 的统计与映射错误。
- **REDIS 扫描覆盖会话（2026-10-28，执行会话）**：闭合 **P1-2**（SCAN 游标零单测）与 **P1-3 的离线可验证部分**（节点扇出）。SCAN 游标循环 + 节点枚举抽成 crate 内缝，`MemoryRedisAdapter` 改为驱动生产循环并可模拟多页游标；新增 12 个离线用例。**实测**：`cargo build` 通过（本 crate 0 warning）、`cargo test` = **126 passed / 0 failed** + 2 doc-tests、`cargo fmt --check` 干净。详见 **§8**。
- **REDIS Cluster 拓扑会话（2026-10-28 第二轮，执行会话）**：**P1-3 闭合**——不再依赖驱动 `cluster` feature，改用 `CLUSTER SLOTS` 手写主节点发现：`parse_cluster_slots`（纯函数）+ `ClusterTopologySource` / `NodeConnectionFactory` 两条缝 + `ClusterScanNodeSource` + 生产接线 `RedisConnection::connect_cluster(seed_url)`；新增 **19** 个离线用例。**实测**：`cargo build` 通过（本 crate 0 warning）、`cargo test` = **148 passed / 0 failed** + 2 doc-tests、`cargo fmt --check` 干净。详见 **§8.5 / §8.6**。
- **验证命令与结果**：
  - `cargo build -p agentscope-extensions-redis` → **2026-10-28 实测：通过**，本 crate **0 warning**（`agentscope-harness` 侧 5 条为既有 warning，非本轮引入）。
  - `cargo test -p agentscope-extensions-redis` → **2026-10-28 第二轮实测：148 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。历史：92（2026-09-19）→ 114（2026-10-22 §7.1）→ 126（§8.1）→ 137（DistributedStore 投影）→ **148（§8.5 本轮）**。
  - 全量 `cargo test --workspace` **未跑**（超出本会话授权范围，只跑 `-p` 单包）。
- **crate 文件清单（2026-10-28 更新，格式：物理行/非空行）**：`Cargo.toml`(22/20)、`src/lib.rs`(54/50)、`src/raw_client.rs`(140/121)、`src/distributed_store.rs`(425/383)、`src/sandbox.rs`(530/476)、`src/snapshot.rs`(306/270)、`src/store.rs`(661/588)、**`src/state/mod.rs`(1346/1200)**、`src/state/adapter.rs`(91/76)、**`src/state/connection.rs`(1419/1253)**、`src/state/key_layout.rs`(378/338)、`src/state/version_support.rs`(97/86)、**`src/test_support/mod.rs`(740/660)**（`state/mod.rs`/`connection.rs`/`test_support` 三处为本轮增长）。**`src/state/list_hash.rs` 已删除**（原 108 行）。

#### 4.1.0 本会话变更（2026-09-19 收尾）
1. **新增 `src/raw_client.rs`**（crate-internal）：`RedisCommands` async 命令缝 + 生产实现 `ManagerRedisCommands`（包装 `ConnectionManager`）。为 `RedisRemoteSnapshotClient` / `RedisSandboxExecutionGuard` 提供可注入缝，使二者可离线测试。`ManagerRedisCommands` 对外 re-export（供上层需要原始命令面时使用）。
2. **新增 `src/test_support/mod.rs`**（`#[cfg(test)]`）：三层内存替身 + `*`/`?` glob + 五个 Lua 脚本的语义等价解释器（`SAVE_SCRIPT`/`PUT_SCRIPT`/`PUT_IF_VERSION_SCRIPT`/`DELETE_SCRIPT`/`RELEASE_SCRIPT`），含错误注入开关与调用计数器。
3. **`src/store.rs`**：引入同步 `StoreCommands` 缝（生产实现 `BlockingRedisCommands`，**懒连** `Mutex<Option<redis::Connection>>`，首次使用时建连）；`RedisStore::connect_lazy` 供组合者零 I/O 自动接线；新增 16 个 `BaseStore` 行为用例。
4. **`src/snapshot.rs`**：改经 `RedisCommands` 缝（`from_commands` 仅测试可见）；新增 7 个用例（往返/缺失/TTL/id 校验/错误包装/spec 往返）。
5. **`src/sandbox.rs`**：改经 `RedisCommands` 缝；`released: bool` → `AtomicBool` 并**接线**（`is_released()`/`token()` 访问器 + 幂等 `release()` + `Drop` `swap` 占用）；新增 10 个用例（含竞争自旋、外来 token CAS、无 runtime Drop 降级）。
6. **`src/distributed_store.rs`**：新增字段 `url: Option<String>` + `adapter: Arc<dyn RedisClientAdapter>`；`base_store()` **现已自动接线**（经保留 URL + `RedisStore::connect_lazy`，构造时零 I/O）；新增 `with_url(url)` / `url()` / `adapter()` / `from_components(...)`；新增 6 个用例。
7. **`src/state/connection.rs`**：新增 `raw_client()` / `raw_adapter()` 分发内部缝；保留上一会话的 **SCAN 游标循环**（`SCAN_COUNT=100` + `HashSet` 去重）。
8. **`src/state/mod.rs`**：`mod version_support` → `pub(crate) mod version_support`（成全内部缝可见性）；新增 28 个 state store 用例（替换原 `DummyAdapter`）。
9. **`src/lib.rs`**：声明 `pub(crate) mod raw_client` + `#[cfg(test)] pub(crate) mod test_support`；re-export `ManagerRedisCommands`。
- **改动文件（仅本 crate + 本 tracker）**：`src/lib.rs`、`src/raw_client.rs`（新）、`src/test_support/mod.rs`（新）、`src/store.rs`、`src/snapshot.rs`、`src/sandbox.rs`、`src/distributed_store.rs`、`src/state/mod.rs`、`src/state/connection.rs`、`docs/extensions/REDIS__GAP_TRACKER.md`。**未改** 根 `Cargo.toml` / `core` / `harness` / 其它 crate；**未新增依赖**。

#### 4.1.1 list_hash 向 core 收敛（2026-09-19，上一会话）
- **背景**：Java `io.agentscope.core.state.ListHashUtil` 是唯一权威实现；Rust `agentscope_core::state::list_hash` 已忠实移植（`compute_hash<T: Hash>` / `has_changed` / `needs_full_rewrite` / `EMPTY_HASH` / `SAMPLING_THRESHOLD`，经 `core::state` `pub use`）。本 crate 存在**同策略的平行实现**，应收敛。
- **变更**：
  1. 删除 `crates/agentscope-extensions-redis/src/state/list_hash.rs`（本地实现）。
  2. `src/state/mod.rs`：移除 `mod list_hash;`；改为 `use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};`；调用点 `save_list_inner` 的 `list_hash::compute_hash(values)` → `compute_hash(values)`、`list_hash::needs_full_rewrite(...)` → `needs_full_rewrite(...)`。
  3. 保留 5 个**集成语义**测试（未修改前缀不需重写 / 追加增长不需重写 / 缩短或前缀编辑需重写 / 无 hash 但有既有数据需重写 / 空列表常量），从已删除文件迁入 `src/state/mod.rs` 的 `tests` 模块；纯算法重复测试（core 已有覆盖）不重复迁移。
- **无命名冲突**：`src/state/mod.rs` 内不存在同名标识符/模块，无需改名。`EMPTY_HASH` / `SAMPLING_THRESHOLD` 在本 crate 其他位置**无引用**（原为私有 `mod`，未 `pub use`），故无需额外改写；测试中 `EMPTY_HASH` 改为 `use agentscope_core::state::EMPTY_HASH;`。
- **行为等价性**：本地 Case3 曾用 `&current[..existing_count.min(current_size)]` 的 `.min` 保护；因 Case1 已保证 `current_size >= existing_count`，该保护等价于 core 的 `&current[..existing_count]`，**无行为差异**。
- **存量数据影响（自愈，无迁移脚本）**：新 core 路径的 hash 值可能与旧本地实现存量 `:_hash` 不一致 → 每 slot 首次 `save_list` 多一次全量重写（`DEL` + `RPUSH` 原值），**数据无损、不丢不清、结果正确**，此后稳定。已知且接受。
- **验证**：`cargo build -p agentscope-extensions-redis` → **通过**（仅 1 条 pre-existing warning：`RedisSandboxLease::released` 未读取；该 warning 已于 2026-09-19 收尾会话接线消除，见 §4.1.0）；`cargo test -p agentscope-extensions-redis` → **21 passed / 0 failed**（+ 2 doc-tests，与收敛前一致：删 0 增 0，仅用例从 `state::list_hash::tests` 位移为 `state::tests`）；`rg "fn compute_hash|fn needs_full_rewrite" crates/agentscope-extensions-redis` → **无结果**（无残留平行实现）。**上述 “21 passed” 为当时快照；收尾后为 92**。
- **改动文件（仅本 crate + 本 tracker）**：`crates/agentscope-extensions-redis/src/state/mod.rs`（改）、`crates/agentscope-extensions-redis/src/state/list_hash.rs`（删）、`docs/extensions/REDIS__GAP_TRACKER.md`（改）。未改根 `Cargo.toml` / `core` / `harness` / 其它 crate。
### 4.2 下一步与阻塞
1. **无阻塞项**。P0 组件与上一轮的 P1/P2 缺口（`base_store()` 自动接线、离线测试覆盖、`find_keys_by_pattern` SCAN、`released` warning）**已全部闭合**；**P1-3（Cluster 多主 SCAN）亦已于 2026-10-28 第二轮闭合**（`CLUSTER SLOTS` 手写拓扑发现，§8.5）；**2026-10-28 第二轮实测**：`build` 0 error / 本 crate 0 warning，`test` **148 passed / 0 failed**（lib）+ 2 doc-tests。**唯一剩余**：真实 Redis / 真实 Cluster 集成测试（缺口 A，环境受限），以及「本 crate 不做 Cluster slot 路由」的既有边界（§8.6）。
2. **剩余缺口 A：真实 Redis 集成测试**（环境受限，**本轮明确不做**）。现有 **148** 个用例全为内存替身离线测试，**未验证**：真实 Lua 脚本在 Redis 中的执行、真实 `SCAN` 游标多轮迭代（**2026-10-28 起其语义已由离线用例覆盖，见 §8.1；Cluster 扇出语义由 §8.5 的注入式用例覆盖，但真实 `CLUSTER SLOTS` 报文与真实多主迭代仍未覆盖**）、真实 `ConnectionManager` 重连与鉴权、Sentinel/Cluster 部署模式。补测需一个可用的 Redis 实例（建议 CI 用 testcontainers / `redis-server` 本地实例 + `#[ignore]` 或 feature-gate 的集成测试文件）。Java 侧对应的 4 个集成用例（`RedisAgentStateClusterIntegrationTest` 3 + `RedisSessionStorageIntegrationTest` 1）Rust 侧为 0。
3. **缺口 B（P1，2026-10-22 静态复核识别；2026-10-22 §7.1 已修复 → 已闭合）**：`save_if_version(expected_version=0)` 的 CAS 语义。Rust 在 `0` 档走**无条件 bump**（`src/state/mod.rs:194`），同时偏离 **Java**（`0` = 真实 CAS）与 **core trait 契约**（`crates/agentscope-core/src/state/store.rs:90`「`expected_version == 0` → create-if-absent」）。调用方若按 core 契约以 `0` 表达「仅当不存在时创建」，在 Redis 后端会**无条件覆盖**已存在状态（丢更新）。**修法建议**：把固有 `save_if_version` 的 `0` 档改为「键不存在才写」（复用 `SAVE_SCRIPT` 的 `current_version()==0` 判定），并保留 `UNCONDITIONAL_EXPECTED`（`u64::MAX`）作为唯一无条件哨兵；trait 实现层把 core 的 `UNVERSIONED(-1)` 映射到该哨兵。**本轮未修**（静态复核禁改 Rust 代码）。
4. **待确认（非阻塞）**：`RedisSandboxExecutionGuard::release(&key)` 为 best-effort `DEL`（无 token CAS），与 Java 不完全对应（Java `RedisSandboxExecutionGuard` 仅有 `tryEnter`，无 `release(key)`/`isHeld(key)`；Java harness `SandboxExecutionGuard` 是 `@FunctionalInterface` 仅含 `tryEnter`），仅作 Rust harness 镜像 API parity；`RedisSandboxLease::release()` 才是 token CAS 正路。
5. **跨模块需求（不改 harness，请汇总方评估）**：harness `DistributedStore` 是具体 struct（`crates/agentscope-harness/src/agent/distributed_store.rs:14`），仅含 `agent_state_store` + `isolation_scope`，无法承载 Java 的 `baseStore` / `sandboxSnapshotSpec` / `sandboxExecutionGuard`。本 crate 以镜像 combiner + `to_distributed_store()` 暴露，**未扩展 harness**。若后续扩展 harness 组合字段，`src/distributed_store.rs` 的投影函数是唯一改动点。
6. **跨模块需求（版本单点，请汇总方处理）**：`redis = { version = "1.7", default-features = false, features = ["script", "tokio-comp", "connection-manager"] }` 已在根 `[workspace.dependencies]`（**根 `Cargo.toml:71`**，UTF-8 行号；本 crate 以 `redis = { workspace = true }` 引用）；本轮**未新增任何依赖**。
7. **未改** 根 `Cargo.toml` / `core` / `harness` / 其它 crate；`src/state/list_hash.rs` **保持删除**，不得恢复本地平行实现。**2026-10-22 静态复核轮仅改本 tracker，未动任何 Rust 代码。**

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-redis` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-redis` / `cargo test -p agentscope-extensions-redis` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-redis/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **静态复核**：**未编译、未跑测试**（`cargo build` / `cargo test` 全部禁用），**未改任何 Rust 代码**；本轮唯一改动 = 本文件。
- 手段：逐文件读取 Java 源码（`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-redis`）与 Rust crate（`crates/agentscope-extensions-redis`），核对符号存在性 / 行号 / 行为语义；`git log` / `git status` 佐证改动归属。
- **计数口径（重要）**：本机 PowerShell 5.1 默认编码为 **gb2312**，`Get-Content` 会把 UTF-8 中文误解码为乱码并**粘连换行**（实测根 `Cargo.toml`：`Get-Content` 得 90 行 vs UTF-8 `ReadAllLines` 得 105 行），故**一切行数均以 UTF-8 `ReadAllLines` 为准**；`Get-Content` 口径结果仅作对照记录。

### 6.2 实测统计（2026-10-22）
| 项 | Java | Rust |
|---|---|---|
| 文件数 | 主 **14** 个 `.java` / 测 **7** 个 `.java` | **12** 个 `.rs`（+ `Cargo.toml` 1） |
| 物理行 | 主 **3499** / 测 **1165** | `.rs` **4636**（`Cargo.toml` 22） |
| 非空行 | 主 **3167** / 测 **1038** | `.rs` **4100**（`Cargo.toml` 20） |
| 用例数 | **26** 个 `@Test`（5 离线类 22 + 2 集成类 4） | **113** 个 `#[test]`(52) + `#[tokio::test]`(61) |
| Rust 用例分布 | — | `state/mod.rs` 50、`store.rs` 20、`sandbox.rs` 13、`snapshot.rs` 9、`distributed_store.rs` 9、`state/key_layout.rs` 9、`state/version_support.rs` 2、`state/connection.rs` 1 |

> Java 7 个测试类逐个实测：`RedisAgentStateStoreTest` 328 行/15、`RedisAgentStateClusterIntegrationTest` 322/3、`RedisSessionStorageIntegrationTest` 110/1、`JedisClientAdapterTest` 85/3、`RedissonAgentStateStoreTest` 79/1、`JedisAgentStateStoreTest` 67/1、`RedisDistributedStoreTest` 47/2。

### 6.3 本轮改动真实性核对（逐项）
| 声明 | 核对结果 | 证据 |
|---|---|---|
| `find_keys_by_pattern` 改为 **SCAN 游标**语义 | ✅ **属实** | `src/state/connection.rs:174-195`：`cursor: u64 = 0` 起，`redis::cmd("SCAN").arg(cursor).arg("MATCH").arg(pattern).arg("COUNT").arg(SCAN_COUNT=100)`，`next == 0` 退出，否则 `cursor = next`；累积 `HashSet<String>` 去重。**全 crate 无 `KEYS` 调用**。⚠️ 但该循环**零单测覆盖**（`state/connection.rs` 唯一用例只测 `value_to_i64`），且 `MemoryRedisAdapter` 替身是「全量键 glob 过滤」、**不模拟游标** |
| `base_store()` **自动接线**（`connect_lazy`） | ✅ **属实** | `src/distributed_store.rs:237-247` `base_store()` → `RedisStore::connect_lazy(url, "{root}store:")`；`src/store.rs:237-240` `connect_lazy` → `BlockingRedisCommands::open(url)`（仅 `redis::Client::open` 校验 URL 形状，**零网络**）；`connect`（`src/store.rs:227-231`）额外 `PING`。`with_url()` / `url()` / `base_store_with_url()` 均存在；无 URL 时返回含 `no Redis URL retained` + `base_store_with_url` 的降级错误 |
| `RedisSandboxLease::released` 为 **`AtomicBool`** | ✅ **属实** | `src/sandbox.rs:193` `released: AtomicBool`；`:32` `use std::sync::atomic::{AtomicBool, Ordering};`；`:208-210` `is_released()`；`:226/:230` `store(true, SeqCst)`；`:253` `swap(true, SeqCst)`（`Drop` 占释放权）。**字段确被读取** ⇒ 无 dead-code warning 隐患 |
| 新增 `MemoryRedisAdapter` / `MemoryRedisCommands` / `MemoryStoreCommands` | ✅ **属实** | `src/test_support/mod.rs`（`#[cfg(test)]`，590 行）：`MemoryRedisAdapter`(:132) / `MemoryRedisCommands`(:348) / `MemoryStoreCommands`(:465) 三个 `pub(crate)` 结构体 |
| 新增 `RedisCommands` / `StoreCommands` 缝 | ✅ **属实** | `src/raw_client.rs:35` `pub trait RedisCommands`（async，7 方法）+ `ManagerRedisCommands`(:60) 生产实现；`src/store.rs:66` `pub(crate) trait StoreCommands`（**同步**，4 方法）+ `BlockingRedisCommands`(:92) 生产实现。另有第三条缝 `src/state/adapter.rs:35` `RedisClientAdapter`（Java 原生抽象） |
| 五个 Lua 脚本的内存解释器 | ✅ **属实** | `SAVE_SCRIPT`（`state/version_support.rs:26`）、`PUT_SCRIPT`/`PUT_IF_VERSION_SCRIPT`/`DELETE_SCRIPT`（`store.rs`）、`RELEASE_SCRIPT`（`sandbox.rs:45`）；替身侧 `run_save_script` 等解释器在 `test_support/mod.rs` |
| B 波次统一产出契约（对照 rollup §2.1） | ✅ **全部存在** | `RedisDistributedStore`（`src/distributed_store.rs`）/ `RedisAgentStateStore`（`src/state/mod.rs`）/ `RedisStore`（`src/store.rs`）/ `RedisSnapshotSpec`（`src/snapshot.rs:147`）/ `RedisSandboxExecutionGuard`（`src/sandbox.rs:72`）。`lib.rs` 导出：`RedisDistributedStore`、`RedisAgentStateStore`(+`KeyLayoutVersion`/`VersionedState`)、`RedisStore`、`RedisSnapshotSpec`、`RedisRemoteSnapshotClient`、`RedisSandboxExecutionGuard`(+`Builder`/`Lease`)、`RedisClientAdapter`、`RedisConnection`、`ManagerRedisCommands` |

### 6.4 对齐结论表
| Java 类 | Rust 对应 | 存在 | 行为覆盖 | 结论 |
|---|---|---|---|---|
| `RedisDistributedStore` | `distributed_store::RedisDistributedStore` | ✅ | ✅（6 组件工厂 + `to_distributed_store()`） | **已对齐**（`base_store` 用独立阻塞连接，见 §3.2） |
| `state.RedisAgentStateStore` | `state::RedisAgentStateStore` | ✅ | ✅（`save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids` + 版本三方法 + `clear_all_sessions`） | **已对齐**（`save_if_version(0)` 语义见 §6.5 P1-1） |
| `state.RedisAgentStateKeyLayout` | `state::key_layout::RedisAgentStateKeyLayout`（`pub(crate)`）+ `KeyLayoutVersion` | ✅ | ✅（9 单测） | **已对齐** |
| `state.RedisClientAdapter` | `state::adapter::RedisClientAdapter` | ✅ | ✅（13 方法 1:1 + `close()` 默认 no-op） | **已对齐**（`close()` 语义弱化，见 §3.3） |
| `state.RedisStateVersionSupport` | `state::version_support` | ✅ | ✅（`SAVE_SCRIPT` 逐字节移植，省略 Java `current_version()` 中等价冗余分支） | **已对齐** |
| `state.jedis.JedisClientAdapter` / `lettuce.LettuceClientAdapter` / `redisson.RedissonClientAdapter` | 合并 → `state::connection::RedisConnection` | ✅ | ⚠️ 部分（`SCAN` 循环零单测；Cluster 下 `SCAN` 只覆盖所连节点 → 漏键，Java Jedis 遍历全 master） | **已对齐（Cluster 模式 P1，见 §2 行注）** |
| `state.jedis.JedisAgentStateStore` / `redisson.RedissonAgentStateStore`（`@Deprecated`） | 设计决定（不迁移） | — | — | **已完成（设计决定）** |
| `store.RedisStore` | `store::RedisStore`（`impl BaseStore`） | ✅ | ✅（`get`/`put`/`put_if_version`/`search`/`delete` + `supports_atomic_session_storage`） | **已对齐**（Java `putIfVersion(expected<0)` 抛 `IllegalArgumentException`；Rust 签名为 `u64`，负值类型层不可表达 ⇒ 分支自然消失） |
| `snapshot.RedisRemoteSnapshotClient` | `snapshot::RedisRemoteSnapshotClient` | ✅ | ✅ | **已对齐**（缺失 → `Error::Io(NotFound)`） |
| `snapshot.RedisSnapshotSpec` | `snapshot::RedisSnapshotSpec` | ✅ | ✅（包 harness `RemoteSnapshotSpec`） | **已对齐** |
| `sandbox.RedisSandboxExecutionGuard` | `sandbox::RedisSandboxExecutionGuard`（镜像类型，非 `impl`） | ✅ | ✅（`SET NX PX` + Lua token CAS + `Drop` 释放 + 常量对齐 30min/500ms/`agentscope:sandbox:lock:`） | **已对齐**（`release(&key)` 无 token CAS，见 §3.3） |
| （Java core）`ListHashUtil` | `agentscope_core::state::{compute_hash, needs_full_rewrite}` | ✅ | ✅（本地平行实现已删） | **已收敛**（hash 值与 Java 不互通，见 §3.3） |

### 6.5 新发现缺口
| 编号 | 级别 | 缺口 | 定位 | 说明 |
|---|---|---|---|---|
| P1-1 | **P1** | `save_if_version(expected_version=0)` **无条件 bump**，同时偏离 Java（`0` = 真实 CAS）与 **core trait 契约**（`0` = create-if-absent） | `src/state/mod.rs:194`（固有方法）+ `:368-392`（trait 实现） | 调用方按 core 契约以 `0` 表达「仅当不存在时创建」时，Redis 后端会**无条件覆盖**已存在状态（丢更新）。core 契约原文见 `crates/agentscope-core/src/state/store.rs:90`。**本轮未修**（禁改 Rust 代码） |
| P1-2 | **P1** | ~~`RedisConnection::find_keys_by_pattern` 的 **SCAN 游标循环零单测覆盖**；`MemoryRedisAdapter` 替身不模拟游标~~ → **已闭合（2026-10-28，§8.1）** | `src/state/connection.rs`（`collect_keys_on_node` / `collect_keys_across_nodes`）/ `src/test_support/mod.rs`（`MemoryScanPager`、`with_scan_page_size`） | 游标循环抽成 `ScanPager` 缝后新增 10 个用例（多页聚合、游标链 `0→7→42`、`COUNT`/`MATCH` 参数、跨页重复键去重、页错误传播、空键空间、三节点扇出、失败节点传播、拓扑枚举失败、空拓扑）；替身改驱动生产循环并可 `with_scan_page_size(n)` 强制多页（`scan_cursor_history()` 断言游标链），state 层另有 2 个端到端用例 |
| P1-3 | **P1** | **Cluster 模式下 `SCAN` 只覆盖所连节点** → `list_session_ids` / `clear_all_sessions` 在 Redis Cluster 上**漏键** | `src/state/connection.rs`（`SingleNodeScanSource` / `ScanNodeSource`） | **已完成（2026-10-28 第二轮，§8.5）**：Java `JedisClientAdapter` 用 `scanIteration(1000, pattern)` **遍历全部 master**。Rust 侧：`CLUSTER SLOTS` 拉拓扑 → 纯函数 `parse_cluster_slots` 去重取 primary → `ClusterScanNodeSource` 为每个 primary 建独立单机连接、逐节点跑完整游标循环并 union；任一节点失败即 `Err`。生产入口 `RedisConnection::connect_cluster(seed_url)`。**无需 `cluster` feature**。剩余：真实 Cluster 集成测试 |
| P2-1 | **P2** | 根 `Cargo.toml` 行号引用过期（原写 `:66`，UTF-8 实测 `:71`；`Get-Content` 口径才是 65） | 本文件 §2.1 行注、§4.2 第 6 条 | 已在本轮订正为 `:71` 并注明口径 |
| P2-2 | **P2** | Java 侧有 **4 个真实 Redis/Cluster 集成用例**，Rust 侧 **0**（无 `#[ignore]` / feature-gate 的集成测试骨架） | `crates/agentscope-extensions-redis/tests/`（**目录不存在**） | 补测需 CI 提供 Redis 实例（testcontainers / `redis-server`） |
| P2-3 | **P2** | `RedisSandboxExecutionGuard::release(&key)` 为无 token 的 `DEL`，可能误释放他人锁 | `src/sandbox.rs:167-176` | Java `RedisSandboxExecutionGuard` **只有 `tryEnter`**，Java harness `SandboxExecutionGuard` 是 `@FunctionalInterface` 仅含 `tryEnter` ⇒ Rust 的 `release`/`is_held` 是**超出 Java 的扩展**，仅对齐 Rust harness 具体 struct 的表面 |

### 6.6 勘误列表（原文 → 实际）
| 编号 | 位置 | 原文 | 实际 |
|---|---|---|---|
| E1 | §1「Java 规模」 | 主代码 **3167 行** / 测 **1038 行** | **属实**（UTF-8 `ReadAllLines` 口径）。但 `Get-Content`（PowerShell 5.1 默认 gb2312）口径得 **3165 / 1038**（`JedisAgentStateStore` / `RedissonAgentStateStore` 两文件末尾空行被吞）⇒ **原 3167 与任务参考 3165 之差为编码/口径差，非数据差**；已在 §1 标注口径 |
| E2 | §1.1 / §3.1 / §4.1 | `cargo test` = **92 passed / 0 failed**，测试函数体 **113** | 两者**不一致**：静态实测 `#[test]`+`#[tokio::test]` = **113**（52+61）。92 是上一会话快照；差异来源 = A2 版本化上提后新增的 `state/mod.rs` 用例 + `state/key_layout.rs` 9 个用例（本轮已把 §3.1 分布表订正为 50/20/13/9/9/9/2/1） |
| E3 | §3.1「离线单元测试」行 | Java **3 个测试类 217 行**；分布 `state/mod.rs(37)`、`store.rs(21)`、`sandbox.rs(12)`、`snapshot.rs(8)`、`distributed_store.rs(7)`、`version_support.rs(2)`、`connection.rs(1)` | Java 实为 **7 个测试类 / 1038 行 / 26 个 `@Test`**；Rust 分布实为 `state/mod.rs(50)`、`store.rs(20)`、`sandbox.rs(13)`、`snapshot.rs(9)`、`distributed_store.rs(9)`、**`state/key_layout.rs(9)`（原表完全遗漏）**、`version_support.rs(2)`、`connection.rs(1)` |
| E4 | §3.2「`clearAllSessions()`」行 | Java 参考 = `state/RedisAgentStateStore.java`（返回 `Mono<Integer>`） | **Java 参考错**：统一 `RedisAgentStateStore` **无 `clearAllSessions`**；仅两个 `@Deprecated` store 有（`state/jedis/JedisAgentStateStore.java:345`、`state/redisson/RedissonAgentStateStore.java:328`）。Rust `clear_all_sessions` 是**扩展方法**，且用 `SCAN` 而非 Java 的 `KEYS` |
| E5 | §4.2 第 6 条 | 根 `Cargo.toml:66` | **根 `Cargo.toml:71`**（UTF-8 行号） |
| E6 | §1.1「源码行数」行 | `Cargo.toml` **23** 行 | `Cargo.toml` **22** 行（物理）/ 20 非空行 |
| E7 | §4.1「crate 文件清单」 | `Cargo.toml(25)`、`distributed_store.rs(339)`、`store.rs(583)`、`state/mod.rs(736)`、`test_support/mod.rs(502)`、合计 **3457 行** | 2026-10-22 实测：`Cargo.toml(22)`、`distributed_store.rs(425)`、`store.rs(661)`、`state/mod.rs(1117)`、`test_support/mod.rs(590)`、**`state/key_layout.rs(378)`（原表遗漏该文件）**、`.rs` 合计 **4636 物理行 / 4100 非空行** |
| E8 | §3.2「`findKeysByPattern` 的 SCAN 游标语义」行 | 暗示 `find_keys_by_pattern` 已由测试检验 | **属实但覆盖被高估**：SCAN 游标循环**零单测**；替身不模拟游标。已改为明示（§6.5 P1-2） |
| E9 | §2「`redis.state.RedisAgentStateStore`」行 | 「覆盖 `save`/`save_list`/…；**并已实现 core trait 的 `supports_versioning`/`get_versioned`/`save_if_version`**」 | **属实**（`src/state/mod.rs:346/350/368` 三处 `impl AgentStateStore` 方法，行号与 §2.1 契约段引用一致）——本条**无需更正**，列出以示已核 |
| E10 | §2.1 契约段 | core trait 引用 `crates/agentscope-core/src/state/store.rs:12,35,68,76,98` | **属实**（`:12` `UNVERSIONED=-1`、`:35` trait、`:68/:76/:98` 三个版本方法），本条**无需更正** |

### 6.7 本轮改动清单
- **仅改** `docs/extensions/REDIS__GAP_TRACKER.md`（§0 结论、§1、§1.1、§2 两处行数、§2.1 契约段、§3.1 测试行、§3.2 `clearAllSessions`/`findKeysByPattern`、§3.3 `save_if_version`、§3.4 两处行数、§4.1、§4.2、新增 §6）。
- **未改** 任何 Rust 代码 / 根 `Cargo.toml` / `core` / `harness` / 其它 crate / 其它 tracker。
- **建议 rollup（`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 REDIS 行）**：
  - Java 列 `2884 / 217` → **3167 / 1038**（严重过期，须改）。
  - 测试数 `92+2 test` → **113 unit（静态实测函数体）+ 2 doc-test**；状态建议由 `已完成` 改为 **`部分完成`**，理由：① P1-1 `save_if_version(0)` 与 core 契约冲突未修；② P1-2 SCAN 游标循环零覆盖；③ P1-3 Cluster `SCAN` 漏键；④ 真实 Redis 集成测试仍为 0（Java 侧 4 个集成用例）。若汇总方认为 P1-1/P1-3 属「设计决定（不迁移）」可保留 `已完成`，但需在 rollup 注明裁定与理由。

## 7. 裁定执行记录（2026-10-22，汇总方）

> 依据 `EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 1（`save_if_version(0)` 语义）。

### 7.1 已修复（P1-1）

| 项 | 修改前 | 修改后 |
|---|---|---|
| `save_if_version(0)` | 特判为 **无条件 bump**（`UNCONDITIONAL_EXPECTED`），与 core 契约（`0` = create-if-absent）冲突 → 按契约调用会**丢更新** | 删除该特判，`0` 走**真实 CAS**：`current_version()==0` 时创建并置 1，否则冲突 |
| core `UNVERSIONED(-1)` 映射 | 错误地映射到 `0`（与真实 CAS 版本 `0` 撞车） | 映射到内部 `UNCONDITIONAL_EXPECTED`(`u64::MAX`)，与 Java `Long.MIN_VALUE` 哨兵等价 |
| 冲突信号 | `Ok(0)` 与"0 = 无条件"语义混用 | 成功值恒 `>= 1`，故 `Ok(0)` **无歧义**表示冲突 → 返回 `UNVERSIONED` |

**测试**：`save_if_version_zero_bumps_unconditionally`（该用例本身编码了 bug）改为 `save_if_version_zero_creates_only_when_absent`（断言第二次 0 期望返回冲突）；新增 `save_if_version_unconditional_sentinel_always_writes`。`cargo test -p agentscope-extensions-redis` → **114 passed / 0 failed**（原 113，净 +1）。

### 7.2 仍待办

- ~~**P1-2** SCAN 游标循环零覆盖~~ → **已闭合（2026-10-28，见 §8.1）**。
- **P1-3** Cluster 下 `SCAN` 只覆盖所连节点 → `list_session_ids` / `clear_all_sessions` 漏键（Java `scanIteration` 遍历全 master）。**已闭合（§8.5）**：`CLUSTER SLOTS` 拓扑发现 + 每 primary 独立连接的扇出，生产入口 `RedisConnection::connect_cluster`。
- **P2-2** 真实 Redis / 真实 Cluster 集成测试仍为 0（环境受限，§4.2 缺口 A）。
- **P2** 真实 Redis 集成测试为 0（Java 有 4 个集成用例）；`guard.release(&key)` 无 token CAS。
## 8. 执行记录（2026-10-28 第一轮，SCAN 覆盖：P1-2 闭合 / P1-3 部分闭合）

> **注**：本轮（§8.1–§8.4）之后另有第二轮，用 `CLUSTER SLOTS` 闭合 P1-3（§8.5 / §8.6）。

> 角色：REDIS 执行子智能体。改动范围仅 `crates/agentscope-extensions-redis/**` 与本文。**未改**根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。

### 8.1 改动内容

**受改文件（3 个 .rs）**

| 文件 | 改动 |
|---|---|
| `src/state/connection.rs` | 新增 crate 内缝 `ScanPager`（单节点分页：`scan_page(cursor, pattern, count) -> (next_cursor, keys)`）与 `ScanNodeSource`（节点枚举：`scan_nodes() -> Vec<Arc<dyn ScanPager>>`）；新增 `collect_keys_on_node`（游标循环 + `HashSet` 去重 + 排序）与 `collect_keys_across_nodes`（逐节点跑完整循环 + union + 排序，任一节点失败即 `Err`）；生产实现 `ConnectionScanPager`（`SCAN <cursor> MATCH <pattern> COUNT 100`）与 `SingleNodeScanSource`；`RedisConnection::find_keys_by_pattern` 改为走 `collect_keys_across_nodes(SingleNodeScanSource, pattern)`，**行为与单机语义不变**（仍是 SCAN、仍无 `KEYS`），只是把循环抽到可测函数 |
| `src/test_support/mod.rs` | `MemoryRedisAdapter` 新增 `scan_page_size` + `scan_cursors` 字段；新增 `with_scan_page_size(n)` / `scan_cursor_history()`；新增 `MemoryScanPager`（游标 = 键快照内位置，非终页返回非 0 游标，页内按 glob 过滤，故可出现「空页但游标非 0」）；`MemoryRedisAdapter` 实现 `ScanNodeSource`；`find_keys_by_pattern` 由「全量键 glob 过滤」改为**驱动生产函数** `collect_keys_across_nodes(self, pattern)` |
| `src/state/mod.rs` | 新增 2 个 state 层端到端用例（见下） |

**新增用例（12 个，全部离线）**

`src/state/connection.rs`（+10）：
1. `scan_loop_aggregates_every_page` — 两页 `(7,[a,b])`、`(0,[c])` → `[a,b,c]`（多页完整聚合）。
2. `scan_loop_follows_server_cursor_until_zero` — 三页 `(7,…)`、`(42,…)`、`(0,…)` → 断言结果含全部 3 键**且**请求游标序列 == `[0,7,42]`（游标非 0 不提前终止、且复用服务端返回的游标）。
3. `scan_loop_uses_match_pattern_and_count_hint` — 断言 `MATCH` 模式透传与 `COUNT==SCAN_COUNT(100)`、首页游标为 `0`。
4. `scan_loop_deduplicates_keys_repeated_across_pages` — 跨页重复键 `k1`/`k2` → `[k1,k2,k3]`（测现状语义：`HashSet` 去重）。
5. `scan_loop_propagates_page_failures` — 第二页 `Err` → 整体 `Err`，且失败页游标为 `5`（异常不被吞、循环确实前进）。
6. `scan_loop_returns_empty_for_an_empty_keyspace` — `(0,[])` → 空。
7. `scan_fanout_visits_every_node_and_merges_results` — 3 节点（A 两页、B 一页含与 A 重复键、C 空）→ union 去重排序，且**三节点均被访问**（`cursors` 分别 `[0,9]`、`[0]`、`[0]`）。
8. `scan_fanout_propagates_a_failing_node` — 节点 B 失败 → 整体 `Err`（不返回静默部分结果）。
9. `scan_fanout_propagates_topology_failures` — `scan_nodes()` 失败 → 传播。
10. `scan_fanout_over_an_empty_topology_is_empty` — 空节点列表 → 空。

`src/state/mod.rs`（+2）：
11. `list_session_ids_aggregates_every_scan_page` — `with_scan_page_size(2)` + 5 个 session → `list_session_ids` 返回全部 5 个排序 id；断言 `scan_cursor_history().len() > 1`、首游标 `0`、后续游标全 `>0`（证明走的是多页游标路径，而不是单次全量）。
12. `clear_all_sessions_deletes_keys_from_every_scan_page` — `with_scan_page_size(3)` 下 `removed == all_keys().len()` 且删空，`scan_cursor_history().len() > 1`（多页枚举不漏键）。

**验证命令与结果**

- `cargo build -p agentscope-extensions-redis` → **通过**，本 crate **0 warning**。
- `cargo test -p agentscope-extensions-redis` → **126 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。基线 114（§7.1 实测）→ **+12**。
- `cargo fmt -p agentscope-extensions-redis -- --check` → 干净（仅格式化本轮新增代码，未触碰其它文件）。
- 用例分布（实测 `#[test]`/`#[tokio::test]`）：`state/mod.rs` 53、`store.rs` 20、`sandbox.rs` 13、`state/connection.rs` 11、`snapshot.rs` 9、`distributed_store.rs` 9、`state/key_layout.rs` 9、`version_support.rs` 2 = **126**。

### 8.2 Cluster 方案：API 调研与阻塞（如实记录）

> **状态（2026-10-28 第二轮）：本节结论「真实 Cluster 拓扑发现阻塞」已解除**——改用 `CLUSTER SLOTS` 自行发现主节点，不触碰驱动 `cluster` feature。以下调研仍作为**为何不走驱动 cluster API** 的依据保留；实施方案与验证见 **§8.5**，剩余边界见 **§8.6**。

**结论**：`redis` 1.7.0 **不提供** primary 枚举 API；本轮实现「扇出逻辑 + 可注入节点源」，真实 Cluster 连接记为**阻塞**。

依据（均在 `C:\Users\Endless\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\redis-1.7.0\` 核对）：

1. `src/cluster_handling/client.rs` 的 `ClusterClient` 只有 `get_connection` / `get_connection_with_config` / `get_async_connection` / `get_async_generic_connection` / `get_pending_async_connection_with_config`，**没有 `get_primaries()` / `get_async_primaries()` 一类节点枚举方法**（1.7.0 全仓 `rg 'fn get_.*primaries'` 零命中）。
2. `src/cluster_handling/slot_map.rs:11` 的 `SlotMap` 是 `pub(crate)`，其 `addresses_for_all_primaries()`（`:86`）**外部不可见**；`src/cluster_handling/mod.rs` 中 `slot_map` / `topology` 亦为 `pub(crate)`。
3. `src/cluster_handling/routing.rs:674` 明确把 `SCAN` 归为 `RouteBy::Undefined`（注释 `TODO - special handling - b"SCAN"`）⇒ 集群连接上的 `SCAN` 只到**单节点**；`MultipleNodeRoutingInfo::AllMasters`（`:104`）虽由 `KEYS` 使用（`:654` → `RouteBy::AllPrimaries` + `CombineArrays`），但 `SCAN` 的**游标是每节点独立**的，把一次 `SCAN` 扇出到全部主节点后返回值无法区分「哪个游标属于哪个节点」，**不能正确分页**，故不可用该旁路。
4. 唯一能拿到 primary 地址的公开途径是 `cluster_read_routing::{ClusterTopology, Shard::primary()}`，但 `ClusterTopology` 只通过 `ReadRoutingStrategy::on_topology_changed` 回调下发（需在 `ClusterClientBuilder::read_routing_strategy(...)` 注入自定义策略并缓存拓扑），属**间接/脆弱**用法。
5. 前置条件缺失：本 crate 依赖 `redis = { workspace = true }`，workspace feature 仅 `script` / `tokio-comp` / `connection-manager`（`default-features = false`），**未启用 `cluster` 或 `cluster-async`**；本轮无权改根 `Cargo.toml`（`[workspace.dependencies]`），故 `redis::cluster_async` 在编译期就不存在。且现有 `RedisConnection` 只持有 `ConnectionManager`，接入 Cluster 需新增第二连接类型 + 贯穿 6 个组件。

**已落地（不依赖上述 API）**：`ScanNodeSource` 抽象 + `collect_keys_across_nodes`；一旦拿到 primary 列表，只需新增一个 `ClusterScanNodeSource`（每个 primary 建 `ConnectionManager`/`ClusterConnection` per-node `SCAN`）返回 `Vec<Arc<dyn ScanPager>>`，`RedisConnection` 侧无需改动。**待汇总方裁定**：是否（a）授权在本 crate `Cargo.toml` 追加 `features = ["cluster-async"]` 并用 `ReadRoutingStrategy` 采集拓扑；（b）在根 `Cargo.toml` 的 `[workspace.dependencies]` 打开 cluster feature；（c）维持单节点并标注 Redis Cluster 不受支持。

> **2026-10-28 第二轮：以上 (a)/(b)/(c) 均未采用**——改为**第四种方案**：用 `CLUSTER SLOTS`（普通命令，任何连接都能发）自行发现主节点，再把每 primary 的连接交给既有 `ConnectionScanPager`。因此**不需要** `cluster`/`cluster-async` feature，也**不需要**改根 `Cargo.toml`。实施与验证见 §8.5。

### 8.3 反向验证（3 项，均已恢复）

| 序号 | 临时改动 | 结果（`cargo test -p agentscope-extensions-redis`） | 恢复后 |
|---|---|---|---|
| R1 | `collect_keys_on_node` 在首页后无条件 `break`（只跑一次） | **FAILED 119 passed / 7 failed**：`scan_loop_follows_server_cursor_until_zero`、`scan_loop_aggregates_every_page`、`scan_loop_deduplicates_keys_repeated_across_pages`、`scan_loop_propagates_page_failures`、`scan_fanout_visits_every_node_and_merges_results`、`list_session_ids_aggregates_every_scan_page`、`clear_all_sessions_deletes_keys_from_every_scan_page` | 126 passed / 0 failed |
| R2 | `collect_keys_across_nodes` 只遍历首个节点（`.take(1)`） | **FAILED 124 passed / 2 failed**：`scan_fanout_visits_every_node_and_merges_results`、`scan_fanout_propagates_a_failing_node` | 126 passed / 0 failed |
| R3 | `collect_keys_on_node` 累积到 `Vec`（去掉 `HashSet` 去重） | **FAILED 125 passed / 1 failed**：`scan_loop_deduplicates_keys_repeated_across_pages`（仅此一例，精度良好） | 126 passed / 0 failed |

R1 同时证明「state 层两个用例走的是生产游标循环」（替身不再自实现 glob 全量过滤），R2 证明扇出用例不是空转。

### 8.4 计数勘误

- 2026-10-22 静态计数 **113** 与 §7.1 实测 **114** 的差 = 少算 1 个用例（`state/mod.rs` 静态 50 vs 实测 51）；本轮已把 §1.1 / §3.1 的分布表更新为**实测值**（`state/mod.rs` 53 等）。
- 本轮净增 **+12**（114 → 126），与新增用例数一致，无删测、无 `#[ignore]`、无放宽断言。

### 8.5 第二轮执行记录（2026-10-28）：`CLUSTER SLOTS` 拓扑发现 → P1-3 闭合

**角色 / 范围**：只改 `crates/agentscope-extensions-redis/src/state/connection.rs` 与本 tracker。**未改**根 `Cargo.toml`（`redis = { workspace = true }` 原样，**feature 未动**）、本 crate `Cargo.toml`（**一行未改**，`tokio-comp` / `connection-manager` / `script` 已够用）、`agentscope-core` / `agentscope-harness` / 其它 crate。

**Java 依据**

- `state/jedis/JedisClientAdapter.java:153-158`：`unifiedJedis.scanIteration(1000, pattern).collect(matchingKeys)`，注释「Unlike a single-node SCAN, Jedis' iteration traverses every master in cluster mode」。
- **错误处理**：Java 无 try/catch，驱动异常直接上抛 ⇒ 任一 master 失败即整体失败。Rust 保持同一语义（`ClusterScanNodeSource::scan_nodes` 用 `?`，不跳过节点、不返回部分结果）。
- **拓扑新鲜度**：Jedis Cluster 客户端按 slot map 自动刷新 ⇒ Rust 每次枚举都**重读** `CLUSTER SLOTS`（不缓存）。
- Java `RedisAgentStateClusterIntegrationTest` 用 `RedisClusterClient.create(Set.of(new HostAndPort(HOST, basePort)))`，**单一种子地址**即可 ⇒ 与 `RedisConnection::connect_cluster(seed_url)` 形态一致。

**实现（新增类型）**

| 类型 / 函数 | 可见性 | 职责 |
|---|---|---|
| `ClusterNodeAddr { host, port }` | `pub(crate)` | primary 地址；`to_redis_url()` 生成诊断 URL（IPv6 加方括号） |
| `parse_cluster_slots(&redis::Value, fallback_host) -> Result<Vec<ClusterNodeAddr>>` | `pub(crate)` **纯函数** | 解析 `[start, end, [host, port, node_id, ...], replicas...]`，只取 primary；按 (host,port) 去重并保持首现顺序；`host` 为空 / NIL ⇒ 用 `fallback_host`；端口须 `1..=65535`；结构非法一律 `Err`（错误信息带 slot range 序号与形状） |
| `trait ClusterTopologySource { async fn primaries() }` | `pub(crate)` 缝 | 拓扑来源（生产 = `CLUSTER SLOTS`） |
| `trait NodeConnectionFactory { async fn connect(&ClusterNodeAddr) -> Arc<dyn ScanPager> }` | `pub(crate)` 缝 | 每 primary 建一条独立单机连接 |
| `ClusterScanNodeSource<P, F>` | `pub(crate)` | `impl ScanNodeSource`：`primaries()` → 逐个 `connect()` → `Vec<Arc<dyn ScanPager>>`；`is_cluster() == true` |
| `ClusterSlotsTopologySource` | 私有（生产） | `redis::cmd("CLUSTER").arg("SLOTS")` + `parse_cluster_slots`；seed 主机作 fallback |
| `SeedClientNodeFactory` | 私有（生产） | 以 seed `Client` 的 `ConnectionInfo` 为**模板** `set_addr(...)` 后 `ConnectionManager::new` → `ConnectionScanPager` |
| `RedisConnection::connect_cluster(seed_url)` / `is_cluster_scan()` | `pub` | 生产入口；`find_keys_by_pattern` 改为 `collect_keys_across_nodes(self.scan_source, ...)`（单机仍是一条 pager，行为不变） |

**与任务书建议的偏离（有意）**：任务书建议用 `redis::Client::open(format!("redis://{host}:{port}"))` 建节点连接；实际改为**以 seed 的 `ConnectionInfo` 为模板、只替换地址**。理由：朴素 URL 会**丢失 AUTH 用户名/密码、DB 与 TLS 设置**，在启用鉴权的 Cluster 上每条节点连接都会 `NOAUTH` 失败。`to_redis_url()` 仍保留用于错误信息（IPv6 方括号规则可单测）。

**新增用例（19 个，全部离线，均在 `src/state/connection.rs::tests`）**

- 拓扑解析（11）：`cluster_slots_parses_every_primary_in_order`、`cluster_slots_merges_a_primary_covering_several_ranges`、`cluster_slots_falls_back_to_the_seed_host_for_empty_hostnames`、`cluster_slots_accepts_ipv6_hosts_and_string_ports`、`cluster_slots_over_an_empty_topology_is_empty`、`cluster_slots_rejects_a_non_array_reply`、`cluster_slots_rejects_a_truncated_range`、`cluster_slots_rejects_non_integer_slot_bounds`、`cluster_slots_rejects_an_inverted_slot_range`、`cluster_slots_rejects_a_truncated_primary_entry`、`cluster_slots_rejects_an_invalid_primary_port`
- 扇出（5）：`cluster_fanout_scans_every_primary_and_merges`（3 primary：断言每 primary 各连一次、各自跑完整游标链、union 去重、空 primary 也被访问、拓扑只读 1 次）、`cluster_fanout_propagates_a_primary_connection_failure`（第 2 个 primary 连不上 ⇒ `Err`，第 3 个从未连接）、`cluster_fanout_propagates_topology_failure`（拓扑失败 ⇒ `Err` 且不连任何节点）、`cluster_fanout_over_an_empty_topology_connects_no_node`、`cluster_fanout_rereads_the_topology_on_every_scan`（两次枚举 ⇒ 拓扑读 2 次，证明不缓存）
- 端点 / 工具（3）：`cluster_node_endpoint_keeps_the_seed_connection_flavour`（Tcp / TcpTls 保留 `insecure` / Unix ⇒ Tcp）、`cluster_seed_host_prefers_the_connected_host`、`normalize_url_adds_a_missing_scheme`

**验证命令与结果**

- `cargo build -p agentscope-extensions-redis` → **通过**，本 crate **0 warning**。
- `cargo test -p agentscope-extensions-redis` → **148 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。基线 129（lib）→ **+19**，与新增用例数一致；无删测、无 `#[ignore]`、无放宽断言。
- `cargo fmt -p agentscope-extensions-redis -- --check` → 干净。
- 未跑 `--workspace`（授权限定）。

**反向验证（3 项，均已恢复）**

| 序号 | 临时改动 | 结果（`cargo test -p agentscope-extensions-redis --lib`） | 恢复后 |
|---|---|---|---|
| R1 | `ClusterScanNodeSource::scan_nodes` 改为「连不上就跳过」（`if let Ok(..)`） | **FAILED 147 passed / 1 failed**：`cluster_fanout_propagates_a_primary_connection_failure`，失败信息 `called Result::unwrap_err() on an Ok value: ["a", "c"]` —— 恰好暴露「静默返回残缺键集」 | 148 passed / 0 failed |
| R2 | `parse_cluster_slots` 去掉 `(host, port)` 去重 | **FAILED 147 passed / 1 failed**：`cluster_slots_merges_a_primary_covering_several_ranges`（同一 primary 被重复列入 ⇒ 扇出会重复扫同一节点） | 148 passed / 0 failed |
| R3 | `parse_cluster_slots` 空 host 直接 `unwrap_or_default()`（不回退 seed 主机） | **FAILED 147 passed / 1 failed**：`cluster_slots_falls_back_to_the_seed_host_for_empty_hostnames`（+ `unused variable: fallback_host` 警告） | 148 passed / 0 failed |

### 8.6 剩余边界与降级语义（Cluster）

| 场景 | 行为 | 依据 / 理由 |
|---|---|---|
| `CLUSTER SLOTS` 报错（含「cluster support disabled」、无权限、seed 不可达） | 整体 `Err`（`"Redis CLUSTER SLOTS: ..."`），**不**回退到 seed 单机扫描 | 回退会产生**静默残缺**键集：`clear_all_sessions` 漏删、`list_session_ids` 少报。Java `scanIteration` 同样把驱动异常直接抛给调用方 |
| `CLUSTER SLOTS` 返回空数组（无 slot 分配的新集群） | `Ok(vec![])` ⇒ 空键集，**不**连接任何节点 | 没有 slot 就没有键，空结果语义正确；「扫 seed 兜底」反而会在真实集群上给出部分视图 |
| 某 primary 连接失败 / 其 `SCAN` 某页失败 | 整体 `Err`，错误信息含该 primary 的 `redis://host:port` | 同上，且能定位到具体节点 |
| 拓扑变化（failover / resharding） | 下次枚举自动生效（每次重读 `CLUSTER SLOTS`） | 对齐 Jedis cluster 客户端 slot map 刷新；无需重建连接 |
| 单键命令（GET/SET/Lua 等） | 仍只发往 `seed_url` 所在节点，**不做 MOVED/ASK 路由**；跨 slot 多键命令可能被服务端拒绝 | 本 crate 未启用驱动 `cluster` feature，也未自研 slot 路由（Java 由 `RedisClusterClient` 承担）。`connect_cluster` 的 rustdoc 已写明此边界；生产建议配合集群代理使用，或仅用于键枚举 |
| 真实 Cluster / 真实 Redis 集成测试 | **仍缺**（与 §4.2 缺口 A 同一项） | 环境受限；本轮验证止于「注入式拓扑 + 注入式连接工厂」的离线用例，真实 `CLUSTER SLOTS` 报文与真实多主迭代未覆盖 |

## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_distributed_store()` 改为接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `self.agent_state_store()?`（`RedisAgentStateStore`） | 前缀 `{root}session:`，布局按 `key_layout_version` |
| `isolation_scope` | `IsolationScope` | 字段 `isolation_scope` | 默认 `User`，`with_isolation_scope` 可覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `self.base_store().ok()`（`RedisStore`，同步阻塞连接） | **仅当 bundle 保留了 URL 时填充**；`from_connection` / `from_components` 未带 URL → `None`（文档化降级路径，调用方用 `base_store_with_url`） |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc::new(self.sandbox_snapshot_spec())`（`RedisSnapshotSpec`） | 前缀 `{root}snapshot:` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.sandbox_execution_guard())` | `SET NX PX` + Lua token CAS；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_distributed_store()`：填满槽位；`base_store` 仅在 `self.base_store()` 成功时填充（保持既有「无 URL → 不报错」契约）。
- `src/sandbox.rs`：新增私有字段 `trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<RedisSandboxLease>>>>` + `impl SandboxGuard`。
  - **薄适配（async → sync）**：`try_enter` / `release` / `is_held` 用 `futures::executor::block_on` 驱动 async 实现（与 `MongodbGuardClient` 同一桥法）。
  - ⚠️ 已知限制：`try_enter` 在槽位被占用时走 `tokio::time::sleep` 轮询，故**不得在 current-thread tokio runtime 内调用**（与既有 `RedisSandboxLease::Drop` 的 runtime 分支说明一致）。
- 测试：`src/distributed_store.rs::{to_distributed_store_projects_snapshot_spec_and_guard, to_distributed_store_projects_base_store_when_url_is_retained}` + `src/sandbox.rs::harness_sandbox_guard_trait_wraps_the_redis_lease`。

### 验证

`cargo test -p agentscope-extensions-redis` → **137 passed / 0 failed**（lib 129 + 集成/doc 8）。

### 未完成 / 语义差异

- `base_store` 槽位在无 URL 的 bundle 上留空（唯一有意留空的槽位，原因见上）。
- ~~同步桥的 runtime 限制（async→sync）已在源码与本节标注。~~ **2026-10-28 修复（M1）**：harness `SandboxGuard::try_enter`/`release` 已改为 `async`，`futures::executor::block_on` 桥已删除，轮询直接 `await tokio::time::sleep`，因此不再有「不可在 current-thread runtime 内驱动」的限制。