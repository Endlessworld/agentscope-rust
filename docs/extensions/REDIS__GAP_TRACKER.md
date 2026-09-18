# REDIS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **REDIS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-redis\`
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state`（含 `jedis` / `lettuce` / `redisson` 子包）/ `store`）
> **Rust 目标**：`crates/agentscope-extensions-redis/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

> **当前结论（2026-09-18 回填）**：P0 组件（state / store / snapshot / sandbox-guard / combiner）均已落地并接线，`build` / `test` 通过；缺口集中在离线测试覆盖、`base_store()` 自动接线与若干语义差异，详见 §3 与 §4。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-redis` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state`（含 `jedis` / `lettuce` / `redisson` 子包）/ `store`） |
| Java 规模（实测） | 主代码 **2884 行**（13 个类）/ 测试 **217 行**（3 个测试类，JUnit5 + Mockito） |
| Rust crate | `crates/agentscope-extensions-redis` |
| 迁移波次 | B |
| 当前状态 | **部分完成**（`cargo build -p agentscope-extensions-redis` 通过，1 条 pre-existing dead-code warning；`cargo test` = **21 passed / 0 failed** unit + **2 passed** doc-tests） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `redis.RedisDistributedStore`（99 行） | 组合 4 组件为 harness `DistributedStore`；`fromJedis(UnifiedJedis[, keyPrefix])` | `distributed_store::RedisDistributedStore`（`src/distributed_store.rs`） | 持有 `RedisConnection` + 根前缀 + `IsolationScope`；工厂方法 `agent_state_store()` / `base_store_with_url(url)` / `sandbox_snapshot_spec()` / `sandbox_snapshot_spec_dyn()` / `snapshot_client()` / `sandbox_execution_guard()`；`to_distributed_store()` 投影到 harness（仅 state store + isolation scope）。`base_store()`（无 URL）返回 `Err`，见 §3.2 |
| `redis.state.RedisAgentStateStore`（521 行） | 会话状态：单值 JSON String + 版本计数（`:ver`）+ 列表（`:list`）+ 列表哈希（`:list:_hash`）+ 键集（`:_keys`）；CAS/版本 | `state::RedisAgentStateStore`（`src/state/mod.rs`，`impl agentscope_core::state::AgentStateStore`） | 覆盖 `save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`；版本能力降为固有方法 `save_if_version`/`get_versioned`/`supports_versioning`，另有 `clear_all_sessions`；`RedisAgentStateStoreBuilder` 只接 `Arc<dyn RedisClientAdapter>` |
| `redis.state.RedisClientAdapter`（126 行） | client-agnostic 适配接口（`set`/`get`/`rightPushList`/`rangeList`/`getListLength`/`deleteKeys`/`addToSet`/`getSetMembers`/`getSetSize`/`keyExists`/`findKeysByPattern`/`evalScript`/`close`） | `state::adapter::RedisClientAdapter`（`src/state/adapter.rs`，`#[async_trait]` trait） | 方法名与 Java 1:1；`close()` 提供默认 no-op 实现 |
| `redis.state.RedisStateVersionSupport`（90 行） | `VERSION_SUFFIX=":ver"`、`UNCONDITIONAL="-1"`、`SAVE_SCRIPT` Lua、`versionKey`/`parseVersion`/参数拼装 | `state::version_support`（`src/state/version_support.rs`） | `SAVE_SCRIPT` 逐字节移植（Java 脚本内 `current_version()` 含一段冗余分支，行为等价，见 §3.4）；`UNCONDITIONAL="-1"`、`parse_version`、`version_key` 对齐 |
| `redis.state.jedis.JedisClientAdapter`（166 行） | Jedis 适配（`UnifiedJedis`/`RedisClient`/`RedisClusterClient`/`RedisSentinelClient`），`findKeysByPattern` 用 SCAN 游标 | → 合并进 `state::connection::RedisConnection`（`src/state/connection.rs`） | Rust 单一 async 驱动，见 §3.4 |
| `redis.state.lettuce.LettuceClientAdapter`（323 行） | Lettuce 适配（standalone/sentinel + cluster 两套命令 API） | → 同上（合并） | 不迁移 Lettuce reactive 栈 |
| `redis.state.redisson.RedissonClientAdapter`（223 行） | Redisson 适配（Standalone/Cluster/Sentinel/Master-Slave） | → 同上（合并） | 不迁移 Redisson 分布式对象 API |
| `redis.state.jedis.JedisAgentStateStore`（368 行，`@Deprecated`） | 基于 `JedisPool` 的旧会话实现 | 设计决定（不迁移） | Java 已弃用并指向统一的 `RedisAgentStateStore`；见 §3.4 |
| `redis.state.redisson.RedissonAgentStateStore`（356 行，`@Deprecated`） | 基于 `RedissonClient` 的旧会话实现 | 设计决定（不迁移） | 同上 |
| `redis.store.RedisStore`（257 行） | 命名空间 KV + CAS + `ZRANGEBYLEX` 前缀搜索；item hash（`value`/`version`）+ idx sorted set；Lua `PUT`/`PUT_IF_VERSION`/`DELETE` | `store::RedisStore`（`src/store.rs`，`impl agentscope_harness::filesystem::remote::store::BaseStore`） | `put_if_version` 对齐 harness CAS（`expected==0` ⇒ create-only-if-absent）；`RedisStore::connect(url, prefix)` / `from_connection`；同步阻塞实现（`Mutex<redis::Connection>`） |
| `redis.snapshot.RedisRemoteSnapshotClient`（80 行） | 快照上传/下载/存在性（Redis binary value，key `{prefix}{snapshotId}.tar`，可选 TTL 秒） | `snapshot::RedisRemoteSnapshotClient`（`src/snapshot.rs`，`impl RemoteSnapshotClient`） | async；`download` 缺失返回 `Error::Io(NotFound)`（对应 Java `FileNotFoundException`） |
| `redis.snapshot.RedisSnapshotSpec`（34 行，`extends RemoteSnapshotSpec`） | Redis 快照便捷 spec | `snapshot::RedisSnapshotSpec`（newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`） | 快照 id 校验沿用 harness `RemoteSnapshotSpec::build` |
| `redis.sandbox.RedisSandboxExecutionGuard`（241 行） | `SET NX PX` 分布式租约 + Lua token CAS 释放；`composeKey = <prefix><scope_lower>:<value>` | `sandbox::RedisSandboxExecutionGuard` + `RedisSandboxExecutionGuardBuilder` + `RedisSandboxLease` + 别名 `SharedRedisSandboxExecutionGuard`（`src/sandbox.rs`） | harness `SandboxExecutionGuard` 是**具体 struct 非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），故为镜像 API（`try_enter`/`release`/`is_held`）的独立类型；租约实现 `Drop` 释放；常量对齐（30min TTL / 500ms retry / `agentscope:sandbox:lock:`） |
| （Java core）`io.agentscope.core.state.ListHashUtil` | 列表变更检测（size 前缀 + 采样 hash） | **已收敛到 core**：复用 `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`crates/agentscope-core/src/state/list_hash.rs`，经 `state/mod.rs` `pub use`） | 本地平行实现 `src/state/list_hash.rs` **已删除**（2026-09-19）；`serde_json::Value` 实现 `Hash`，core 泛型 `compute_hash<T: Hash>` 直接接收 `&[Value]`（与 JDBC/PostgreSQL 调用方式一致），无需本地 JSON 串中转 |
| （测试替身）Java 用 Mockito mock `RedisClientAdapter` / `JedisPool` / `RedissonClient` | 离线单测（`state/RedisAgentStateStoreTest.java` 71 行、`state/jedis/JedisAgentStateStoreTest.java` 67 行、`state/redisson/RedissonAgentStateStoreTest.java` 79 行） | crate 内 `#[cfg(test)]`（21 个纯逻辑单测） | 无 `tests/` 目录、无内存 Redis 替身，见 §3.1 覆盖缺口 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `redis.clients:jedis`（`pom.xml`） | Standalone/Cluster/Sentinel 客户端 | `redis = { version = "1.7", default-features = false, features = ["script", "tokio-comp", "connection-manager"] }` + `redis::aio::ConnectionManager`（`Cargo.toml`） | 版本由本 crate 本地固定，**未写入根 `[workspace.dependencies]`**（已在 `EXTENSIONS_GAP_TRACKER.md` §2.2/§4 记录，由汇总方统一处理） |
| `org.redisson:redisson`（`pom.xml`） | 统一客户端 + 分布式对象（`RBucket`/`RList`/`RScript`/`RSet`/`RKeys`） | 同上（合并为 `redis` 驱动） | 不迁移 Redisson 专属 API 与编解码（`StringCodec`） |
| `io.lettuce:lettuce-core`（`pom.xml`） | reactive/async 客户端（standalone/sentinel/cluster 双命令 API） | 同上（合并） | Lettuce reactive 栈不对应迁移 |
| `com.fasterxml.jackson.databind.ObjectMapper` / `io.agentscope.core.util.JsonUtils` | JSON 序列化 | `serde_json` | store 侧 `StoreValue = serde_json::Map<String, serde_json::Value>`；状态值 `serde_json::Value` |
| `reactor.core.publisher.Mono` + `Schedulers.boundedElastic()` | `clearAllSessions()` 异步包装 | `async fn clear_all_sessions() -> Result<usize>`（`src/state/mod.rs`） | Rust 直接 async，无 reactive 包装 |
| `org.slf4j.Logger`（guard 有 debug/warn 埋点） | 日志 | `tracing` | state store 在 `save`/`save_list`/`delete`/`list_session_ids`/`exists` 失败路径有 `tracing::error!`；`sandbox.rs` 有 `debug!`/`warn!`；store/snapshot/connection 无埋点 |
| `RuntimeException` / `IllegalArgumentException` / `FileNotFoundException` | 错误传播 | `agentscope_core::error::Error`（主要为 `Error::Other`；快照缺失 `Error::Io(NotFound)`） | 无本模块专用 error 枚举 |
| `java.time.Duration` / `java.util.UUID` | 租约 TTL / 锁 token | `std::time::Duration` / `uuid` crate（`Uuid::new_v4()`） | 常量对齐：`DEFAULT_LEASE_TTL=30min`、`DEFAULT_RETRY_INTERVAL=500ms` |
| `java.io.InputStream` / `byte[]`（快照） | 快照数据流 | `&[u8]` / `Vec<u8>`（`RemoteSnapshotClient` 签名） | — |
| JUnit5 + Mockito（3 个测试类 217 行） | 离线测试 | Rust 内置 `#[cfg(test)]`（21 个用例） | 全部为纯函数/键名/前缀归一化断言，**未覆盖** trait 路径与 Lua 脚本，见 §3.1 |

**harness / core 复用契约（不改 harness/core，对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1）**：
- `impl agentscope_core::state::AgentStateStore`（`src/state/mod.rs`；trait 定义 `crates/agentscope-core/src/state/store.rs:12`，签名用 `Option<&str>` 与 `serde_json::Value`）。
- `impl agentscope_harness::filesystem::remote::store::BaseStore`（`src/store.rs`；`StoreItem`/`BaseStore` 定义 `crates/agentscope-harness/src/filesystem/remote/store.rs:21,34`；CAS 语义以 harness 为准，`StoreItem.version` 从 1 起单调递增）。
- `impl RemoteSnapshotClient`（`src/snapshot.rs`；trait 定义 `crates/agentscope-harness/src/agent/sandbox/snapshot.rs:134`）。
- `impl SandboxSnapshotSpec`（`src/snapshot.rs`；trait 定义 `crates/agentscope-harness/src/agent/sandbox/snapshot.rs:242`，包装 `RemoteSnapshotSpec` 见 `:274`）。
- `SandboxIsolationKey` 使用 harness 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`（`crates/agentscope-harness/src/agent/sandbox/key.rs:11`），**不得**用私有 `sandbox::key::`。
- `to_distributed_store()` → harness `DistributedStore`（`src/distributed_store.rs`；struct 定义 `crates/agentscope-harness/src/agent/distributed_store.rs:14`，仅 `agent_state_store` + `isolation_scope`）。
- `RedisSandboxExecutionGuard`：harness `SandboxExecutionGuard` 是具体 struct（`.../sandbox/guard.rs:13`）非 trait，无法 `impl`，本 crate 提供镜像类型。

**client-agnostic 适配层设计**：`RedisClientAdapter`（`src/state/adapter.rs`）保留 Java 的抽象意图，使 `RedisAgentStateStore` 只依赖 `Arc<dyn RedisClientAdapter>`（`src/state/mod.rs` 的 `client` 字段），从而支持自定义后端/内存替身。Rust 生态单一主流 async 驱动 `redis`，故只提供一个规范化实现 `RedisConnection`（`ConnectionManager`，`Clone` 廉价、可 `Arc` 共享，`src/state/connection.rs`）；Java 的 Jedis/Lettuce/Redisson 三套适配器合并为其一。注意：`RedisStore`/`RedisSnapshotSpec`/`RedisSandboxExecutionGuard` **不经** `RedisClientAdapter`，而是直接持有 `redis::Connection` / `ConnectionManager`（见 §3.3）。

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
| `RedisDistributedStore` 组合者 | `RedisDistributedStore.java` | 已完成（`base_store()` 需 URL，见 §3.2） | 5 个组件工厂方法 + `to_distributed_store()` |
| 依赖声明（`jedis`/`redisson`/`lettuce-core`） | `pom.xml` | 已完成 | 本 crate 仅 `redis = "1.7"`（features `script`/`tokio-comp`/`connection-manager`）；`serde`/`serde_json`/`thiserror`/`async-trait`/`tokio`/`futures`/`tracing`/`chrono`/`uuid` 取 workspace |
| **离线单元测试** | Java 3 个测试类 217 行（Mockito mock 适配器/连接池） | **部分完成（P1 缺口）** | 现状：`cargo test -p agentscope-extensions-redis` = **21 passed / 0 failed**（+ 2 doc-tests），21 个全部为纯逻辑用例（前缀归一化、键名/命名空间拼接、`slot_id`、列表 hash 变更检测集成语义、`value_to_i64`、guard 键格式），分布于 `src/store.rs`(5)、`src/state/mod.rs`(9，含 2026-09-19 由已删除的 `src/state/list_hash.rs` 迁入的 5 个列表 hash 集成语义用例)、`src/sandbox.rs`(2)、`src/state/version_support.rs`(2)、`src/distributed_store.rs`(1)、`src/snapshot.rs`(1)、`src/state/connection.rs`(1)。**零测试经由 trait 对象 / `eval_script` / 真实 Redis 路径**（无内存 Redis 替身，crate 内无 `tests/` 目录）。补测方向：`RedisClientAdapter` 内存实现 + state 的 CAS 冲突/列表全量重写 vs 增量/`:_hash`、store 的 `put_if_version` 三分支、snapshot TTL 往返、guard 租约 token CAS 与释放 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `RedisDistributedStore::base_store()` 自动接线 | `RedisDistributedStore.baseStore()`（返回 `new RedisStore(jedis, keyPrefix+"store:")`，共享同一 `jedis`） | **待办（P1）** | Rust 侧 `src/distributed_store.rs` 的 `base_store()` 因未保留 URL 而**直接返回 `Err`**，提示改调 `base_store_with_url(url)`；后者会**另开一条阻塞连接**而非复用 `RedisConnection`（`BaseStore` 为同步 trait，无法直接复用 async `ConnectionManager`）。这是组合者与 Java「单连接自动接线」的最大功能差异 |
| `supportsVersioning()` / `getVersioned()` | `state/RedisAgentStateStore.java`（`supportsVersioning`、泛型 `<T extends State> VersionedState<T> getVersioned(..., Class<T>)`） | 已完成（降为固有方法） | core `AgentStateStore` trait 无版本方法，故保留为固有 `supports_versioning() -> bool` 与 `get_versioned(...) -> Result<VersionedState>`；`VersionedState { value: Option<Value>, version: u64 }` |
| `saveIfVersion()` | `state/RedisAgentStateStore.java` | 已完成（固有方法） | `save_if_version(..., expected_version: u64)`，冲突返回 `0`；`expected==0` 时转为无条件 bump（见 §3.3 待确认） |
| `clearAllSessions()` | `state/RedisAgentStateStore.java`（返回 `Mono<Integer>`） | 已完成 | `clear_all_sessions() -> Result<usize>`，用 `prefix*` 模式 `KEYS` + `DEL` |
| `close()` | `state/RedisAgentStateStore.java`（`client.close()`） | 设计决定（不迁移） | Rust `RedisConnection` 为 `Clone` + `ConnectionManager`，无所有权/关闭语义 |
| `findKeysByPattern` 的 SCAN 游标语义 | `RedisClientAdapter.findKeysByPattern` 的三个实现（`state/jedis/JedisClientAdapter.java:155`、`state/lettuce/LettuceClientAdapter.java:249`、`state/redisson/RedissonClientAdapter.java:208`）**均基于 SCAN 游标**（Jedis `scan`、Lettuce `scan`、Redisson `KeysScanOptions`） | **待办（P1 语义差异）** | Rust `RedisConnection::find_keys_by_pattern` 直接用 Redis `KEYS`（`src/state/connection.rs`），在大 keyspace 下会阻塞 Redis；Java 仅已弃用的 `JedisAgentStateStore` 用 `jedis.keys()`。建议改用 `SCAN` 游标保持与 Java 适配器一致 |
| `RedisStore` 自定义 `ObjectMapper` 注入 | `store/RedisStore.java`（3 参构造器） | 设计决定（不迁移） | Rust 固定 `serde_json`，无需注入 |
| 三个适配器（Jedis/Lettuce/Redisson）的部署模式矩阵 | `state/jedis`、`state/lettuce`、`state/redisson`（Standalone/Cluster/Sentinel/Master-Slave） | 设计决定（不迁移，合并） | Rust 统一由 `redis::Client::open(url)` + `ConnectionManager` 承担，URL scheme 决定模式；Sentinel/Cluster 需 `redis` crate 对应 feature（未启用），基础 Standalone 已覆盖 |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| `RedisSandboxLease::released` dead-code warning | `src/sandbox.rs:175` 字段 `released` 写入后从未读取，产生 dead-code warning；构建仍在，系既有代码 | **待办（P2 清理）** |
| `save_if_version(expected_version=0)` 的 CAS 语义差异 | Java：`expectedVersion == UNVERSIONED`（`Long.MIN_VALUE`，非 0）才走无条件分支，`expected == 0` 会直接拿 `0` 与真实版本比较（除非键不存在）。Rust（`src/state/mod.rs`）：`expected_version == 0` 也走「无条件 bump」。而 harness `BaseStore::put_if_version` 约定 `0` = create-only-if-absent → 两语义**可能冲突**，需以 Java 实际调用约定 / harness 契约为准裁定 | **待确认** |
| `find_keys_by_pattern` 使用 `KEYS` | 见 §3.2：`KEYS` 在大 keyspace 下会阻塞 Redis；Java 三个适配器的 `findKeysByPattern` 均基于 SCAN 游标 | **待办（P1）** |
| `list_hash` 每元素 hash 输入差异与收敛 | **已收敛到 core**（2026-09-19）：本地平行实现 `src/state/list_hash.rs` 已删除，改用 `agentscope_core::state::{compute_hash, needs_full_rewrite}`（泛型 `T: Hash` 直接接收 `&[Value]`）。core 亦用 `DefaultHasher`，**非** Java `hashCode`，故 hash 值仍与 Java 不互通。**存量数据影响**：收敛后算法（size 前缀 + 采样索引 + `DefaultHasher`）与旧本地实现**并非逐位一致**（旧实现 per-element hash 以 `Value::to_string()` 文本哈希后拼字符串；core 直接对 `Value` 调 `Hash`）→ 同一 Redis 实例中已存的 `:_hash` 与新算值不匹配，**首次 `save_list` 会走 Case3 全量重写一次**（`DEL` + `RPUSH` 原值），**数据无损、结果正确、自愈**；此后写入即稳定。跨语言 Java/Rust 混用同 slot 同理（仅多一次全量重写） | **已缓解（低风险，无需迁移脚本）** |
| `state::adapter::RedisClientAdapter::close()` | Java `close()` 为必需方法；Rust 提供默认 no-op 实现（未强制实现） | 已完成（语义弱化） |
| `RedisSandboxExecutionGuard::release()` 为 best-effort `DEL` | Java 无对应无 token 的 `release(key)`（仅 lease `close()` 做 token CAS）；Rust `release(&key)` 直接 `DEL`，可能误释放他人锁，仅作 parity | **待确认** |
| store/snapshot/connection 无 `tracing` 埋点 | Java `RedisStore`/`RedisRemoteSnapshotClient` 无日志，guard 有 SLF4J debug/warn；Rust `sandbox.rs` 已对齐，store/snapshot 亦无（与 Java 一致） | 已完成（与 Java 对齐） |
| 真实 Redis 集成测试 | 当前无真实 Redis 实例测试；21 个单测全为纯逻辑 | 待办（环境受限） |

### 3.4 设计决定（不迁移）汇总
| 项 | Java 参考 | 裁定 | 理由 |
|---|---|---|---|
| `JedisAgentStateStore` | `state/jedis/JedisAgentStateStore.java`（368 行，`@Deprecated`） | 不迁移 | Java 自身已弃用并指向统一的 `RedisAgentStateStore` |
| `RedissonAgentStateStore` | `state/redisson/RedissonAgentStateStore.java`（356 行，`@Deprecated`） | 不迁移 | 同上 |
| `JedisClientAdapter` / `LettuceClientAdapter` / `RedissonClientAdapter` | 三个适配器（共 712 行） | 合并为单一 `RedisConnection` | Rust 生态单一 async 驱动；保留 `RedisClientAdapter` trait 作扩展缝 |
| Redisson 分布式对象 / Lettuce reactive 栈 | 上列适配器内部 | 不迁移 | 超出 `RedisClientAdapter` 抽象所需 |
| `Store` 自定义 `ObjectMapper`、`RedisAgentStateStore.close()` | 见 §3.2 | 不迁移 | Rust 无相应所有权/注入模型 |
| Java `RedisStateVersionSupport.current_version()` 内冗余分支 | `state/RedisStateVersionSupport.java`（`if EXISTS(KEYS[1]) return 0` 后仍 `return 0`） | 行为等价，Rust 略去 | 不影响 CAS 结果 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **编译修复会话（2026-09-18）**：`cargo build -p agentscope-extensions-redis` 原报 **9 个错误**，已全部修复，现 `build` 与 `test` 均通过。
- **修复内容**：
  1. `state/mod.rs` 缺少子模块声明：`adapter.rs` / `connection.rs` / `version_support.rs` 文件已存在（`list_hash.rs` 已于 2026-09-19 收敛删除），但 `mod.rs` 只 `use self::...` 未 `mod` 声明 → 补齐 `pub mod adapter; pub mod connection; mod version_support;`（`adapter`/`connection` 需对 `lib.rs` re-export 可见，故 `pub`）。
  2. 去掉随之冗余的 `use self::version_support::{self, ...}`（与 `mod` 声明冲突，E0255），改为 `use self::version_support::{SAVE_SCRIPT, UNCONDITIONAL};`（`list_hash` 相关条目已在 2026-09-19 收敛后失效）。
  3. `state/connection.rs` 的 `lrange` 调用签名错误（`redis` 1.7.0 为 2 个泛型参数 `K, RV` 且边界为 `isize`）→ 改为 `conn.lrange::<_, Vec<String>>(key, start as isize, end as isize)`。
  4. `sandbox.rs` 引用了私有模块 `agentscope_harness::agent::sandbox::key::SandboxIsolationKey`（E0603）→ 改用 harness 已公开的 re-export `agentscope_harness::agent::sandbox::SandboxIsolationKey`（未改 harness）。
- **改动文件（仅本 crate）**：
  - `crates/agentscope-extensions-redis/src/state/mod.rs`
  - `crates/agentscope-extensions-redis/src/state/connection.rs`
  - `crates/agentscope-extensions-redis/src/sandbox.rs`
- **验证命令与结果**：
  - `cargo build -p agentscope-extensions-redis` → **通过**（仅 1 条 pre-existing dead-code warning：`RedisSandboxLease::released` 未被读取）。
  - `cargo test -p agentscope-extensions-redis` → **21 passed; 0 failed**（unit）+ **2 passed**（doc-tests）。
- **遗留 warning（非阻塞）**：`sandbox.rs:175` 字段 `released` 未读取。属既有代码，未在本次修复范围内，功能未删减；已登记为 §3.3 的 P2 清理项。
- **基线（汇总方提供，本会话未重跑）**：
  - `cargo build -p agentscope-extensions-redis` → **通过**（1 条 pre-existing dead-code warning：`RedisSandboxLease::released`）。
  - `cargo test -p agentscope-extensions-redis` → **21 passed / 0 failed**（unit）+ **2 passed**（doc-tests）。
  - 全量 `cargo test --workspace` → **1485 passed / 0 failed**。
- **crate 文件清单（实测行数，2026-09-19 收敛后）**：`Cargo.toml`(20)、`src/lib.rs`(49)、`src/distributed_store.rs`(210)、`src/sandbox.rs`(276)、`src/snapshot.rs`(163)、`src/store.rs`(351)、`src/state/mod.rs`(522)、`src/state/adapter.rs`(77)、`src/state/connection.rs`(201)、`src/state/version_support.rs`(97)。**`src/state/list_hash.rs` 已删除**（原 108 行）。

### 4.1.1 list_hash 向 core 收敛（2026-09-19）
- **背景**：Java `io.agentscope.core.state.ListHashUtil` 是唯一权威实现；Rust `agentscope_core::state::list_hash` 已忠实移植（`compute_hash<T: Hash>` / `has_changed` / `needs_full_rewrite` / `EMPTY_HASH` / `SAMPLING_THRESHOLD`，经 `core::state` `pub use`）。本 crate 存在**同策略的平行实现**，应收敛。
- **变更**：
  1. 删除 `crates/agentscope-extensions-redis/src/state/list_hash.rs`（本地实现）。
  2. `src/state/mod.rs`：移除 `mod list_hash;`；改为 `use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};`；调用点 `save_list_inner` 的 `list_hash::compute_hash(values)` → `compute_hash(values)`、`list_hash::needs_full_rewrite(...)` → `needs_full_rewrite(...)`。
  3. 保留 5 个**集成语义**测试（未修改前缀不需重写 / 追加增长不需重写 / 缩短或前缀编辑需重写 / 无 hash 但有既有数据需重写 / 空列表常量），从已删除文件迁入 `src/state/mod.rs` 的 `tests` 模块；纯算法重复测试（core 已有覆盖）不重复迁移。
- **无命名冲突**：`src/state/mod.rs` 内不存在同名标识符/模块，无需改名。`EMPTY_HASH` / `SAMPLING_THRESHOLD` 在本 crate 其他位置**无引用**（原为私有 `mod`，未 `pub use`），故无需额外改写；测试中 `EMPTY_HASH` 改为 `use agentscope_core::state::EMPTY_HASH;`。
- **行为等价性**：本地 Case3 曾用 `&current[..existing_count.min(current_size)]` 的 `.min` 保护；因 Case1 已保证 `current_size >= existing_count`，该保护等价于 core 的 `&current[..existing_count]`，**无行为差异**。
- **存量数据影响（自愈，无迁移脚本）**：新 core 路径的 hash 值可能与旧本地实现存量 `:_hash` 不一致 → 每 slot 首次 `save_list` 多一次全量重写（`DEL` + `RPUSH` 原值），**数据无损、不丢不清、结果正确**，此后稳定。已知且接受。
- **验证**：`cargo build -p agentscope-extensions-redis` → **通过**（仅 1 条 pre-existing warning：`RedisSandboxLease::released` 未读取）；`cargo test -p agentscope-extensions-redis` → **21 passed / 0 failed**（+ 2 doc-tests，与收敛前一致：删 0 增 0，仅用例从 `state::list_hash::tests` 位移为 `state::tests`）；`rg "fn compute_hash|fn needs_full_rewrite" crates/agentscope-extensions-redis` → **无结果**（无残留平行实现）。
- **改动文件（仅本 crate + 本 tracker）**：`crates/agentscope-extensions-redis/src/state/mod.rs`（改）、`crates/agentscope-extensions-redis/src/state/list_hash.rs`（删）、`docs/extensions/REDIS__GAP_TRACKER.md`（改）。未改根 `Cargo.toml` / `core` / `harness` / 其它 crate。
### 4.2 下一步与阻塞
1. 无阻塞项。`state` 四个子模块此前“缺失”实为**未接线**（文件均在），已按实际源码接入，未新增空模块凑数。
2. 后续可考虑的增强（非本次任务）：清理 `RedisSandboxLease::released` 的 dead-code warning。§2/§3 的 Java→Rust 映射与缺口清单已由本会话（2026-09-18 回填）补齐，不再是占位。
3. 未改根 `Cargo.toml` / `core` / `harness` / 其它 crate。
4. **P1（本模块内）**：`RedisDistributedStore::base_store()` 未保留 URL，无法自动接线同步 `BaseStore`，需改用 `base_store_with_url(url)`（§3.2）；离线测试覆盖偏弱、无内存 Redis 替身（§3.1），建议补 `RedisClientAdapter` 内存实现并覆盖 state/store/snapshot/guard 分支。
5. **P2 清理/待确认**：删除或启用 `RedisSandboxLease::released`（`src/sandbox.rs:175`）以消除 dead-code warning；核对 `save_if_version(expected=0)` 的 CAS 语义、`find_keys_by_pattern` 用 `KEYS` 而非 SCAN（均见 §3.3）。列表 hash 跨语言不互通一项已于 2026-09-19 收敛后降级为「已缓解」（见 §3.3 与 §4.1.1）。
6. **跨模块需求（不改 harness）**：harness `DistributedStore` 是具体 struct（`crates/agentscope-harness/src/agent/distributed_store.rs:14`），仅含 `agent_state_store` + `isolation_scope`，无法承载 Java 的 `baseStore` / `sandboxSnapshotSpec` / `sandboxExecutionGuard`；本 crate 以镜像 combiner + `to_distributed_store()` 暴露，未扩展 harness。若后续扩展 harness 组合字段，`src/distributed_store.rs` 的投影函数是唯一改动点。请汇总方评估。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-redis` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-redis` / `cargo test -p agentscope-extensions-redis` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-redis/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

