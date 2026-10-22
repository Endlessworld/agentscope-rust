# MONGODB 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MONGODB** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-mongodb\`
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包）
> **Rust 目标**：`crates/agentscope-extensions-mongodb/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-mongodb` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包） |
| Java 规模（实测 2026-10-22 静态复核） | 主代码 **2235 行（9 类）** / 测试 **2429 行（9 测试类 + 2 testutil，147 个 `@Test`，见 §2）** |
| Rust crate | `crates/agentscope-extensions-mongodb` |
| 迁移波次 | B |
| 当前状态 | **进行中**（静态复核 2026-10-22：上一轮声明的 4 项改动**核实属实**，但复核发现 **3 项 P1 未闭环**——①builder `ttl_days` 未生效、②guard `database_name` 未生效、③级联删快照链路在 Rust harness 侧无写入方；另有 5 项 P2。测试数 76（实测 `#[test]` 21 + `#[tokio::test]` 55）✅ 与记载一致。详见 §3.3 / §4.1 复核节） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `mongodb.MongoConstants`（38 行） | 默认库名 + 四个集合名常量 | `constants.rs`（`DEFAULT_DATABASE` / `SESSIONS_COLLECTION` / `BASE_STORE_COLLECTION` / `SNAPSHOTS_COLLECTION` / `SANDBOX_LOCKS_COLLECTION`） | 常量值逐一对齐 |
| `mongodb.MongoDistributedStore`（299 行，`implements DistributedStore, AutoCloseable`） | 组合四组件 + 生命周期 + 级联删快照 | `distributed_store.rs::MongodbDistributedStore` | Java 直接实现接口；Rust 用组合 + `to_harness_store()` 投影（harness `DistributedStore` 是 struct，仅 `agent_state_store`+`isolation_scope`）。`new` 内注册 `on_delete` 级联删快照 |
| `mongodb.MongoIndexUtils`（160 行） | 索引创建 + error 85 冲突迁移（按 key pattern 查真实索引名后 drop+重试） | `index_utils.rs::create_index_with_migration` / `IndexSpec` / `key_pattern_json` / `MAX_MIGRATION_ATTEMPTS=3` | 语义保留；`tracing::info/warn/debug` 埋点 |
| `mongodb.MongoKeyEscaper`（147 行） | 递归转义 `.`/`$`/`\0`/`\` 键名 | `key_escaper.rs::escape/unescape/escape_key/unescape_key` | 编码对齐（`.`→`\E`、`$`→`\$`、`\0`→`\N`、`\`→`\\`） |
| `mongodb.state.MongoAgentStateStore`（676 行） | 会话状态：单值 + 列表 + 版本 CAS + 列表哈希 + TTL | `state_store.rs::MongodbAgentStateStore`（+ builder / `UNVERSIONED` / `validate_state_key`），`impl agentscope_core::state::AgentStateStore` | 复合 `_id {user_id, session_id}`；`states`/`versions`/`hashes` 子文档；`save_if_version`/`get_version` 为固有方法 |
| `mongodb.store.MongoBaseStore`（302 行） | 命名空间 KV + CAS + 前缀搜索 | `store.rs::MongodbStore`（+ `NS_SEPARATOR` / `NS_UPPER_BOUND`），`impl agentscope_harness::filesystem::remote::store::BaseStore` | US(0x1F) 命名空间编码 + `$gte/$lt` 范围前缀匹配；`version` 从 1 起，`put` 每次成功写 +1 |
| `mongodb.sandbox.MongoSandboxExecutionGuard`（402 行） | 分布式锁（insert + 过期 reclaim + 租约续约） | `sandbox_guard.rs::MongodbSandboxExecutionGuard` + `Builder` + `MongodbLeaseHandle` + `MongodbGuardClient` + `compose_lock_id` | harness `SandboxExecutionGuard` 为**具体 struct 非 trait** → `MongodbGuardClient` 镜像其方法（`try_enter/release/is_held/active_count`）；`compose_lock_id` 与 Java **字节级一致**（`lock:`+hex(SHA-256(`scope:value`))，scope 小写）；**缺口**：builder 的 `database_name` 收下但未用于限定集合（见 §3.3） |
| `mongodb.snapshot.MongoRemoteSnapshotClient`（178 行） | GridFS 快照上传/下载/存在 + legacy 单文档回退 | `snapshot.rs::MongodbRemoteSnapshotClient`，`impl RemoteSnapshotClient`，`delete` 为固有方法 | GridFS 分块（255KB，`{prefix}.files`+`{prefix}.chunks`，chunk 含 `files_id`/`n`/`data`）+ `length`/`chunkSize`/`uploadDate`/`metadata.createdAt` 已落地；下载按 `n` 重组并校验连续性/长度；`exists`/`download`/`delete` 均先 GridFS 再回退 legacy 单文档。**已补齐** |
| `mongodb.snapshot.MongoSnapshotSpec`（33 行，`extends RemoteSnapshotSpec`） | 快照 spec 便捷包装 | `snapshot.rs::MongodbSnapshotSpec`，`impl SandboxSnapshotSpec` | 内部包装 harness `RemoteSnapshotSpec`；`database_name` 已生效（数据库限定 collection key） |
| （测试替身）Java Testcontainers / Mockito | 离线/容器测试 | `client.rs::InMemoryMongoClient` | 忠实内存后端；duplicate-key 11000、`$set/$inc/$push/$setOnInsert/$unset`、dot-path 均已对齐 |
| （驱动缝隙）Java 直依赖 `mongodb-driver-sync` | 文档/索引/GridFS 原始 API | `client.rs::MongoClient` trait（异步 seam） | driver-agnostic；真实驱动可后续实现同一 trait（§4.2） |
| （列表哈希）Java core `ListHashUtil` | 列表变更检测 | `list_hash.rs`（**薄 re-export** → `agentscope_core::state::{compute_hash, needs_full_rewrite}`） | **已收敛（2026-09-19）**：本地 SHA-256 平行实现已删除；`state_store.rs` 直接调用 core（见 §3.3、§4.1） |

> 实测规模/测试数（2026-10-22 静态复核，脚本按 `^\s*@Test` 统计）：`MongoDistributedStoreTest`15、`MongoIndexLifecycleContractTest`10、`MongoSandboxExecutionGuardTest`24、`MongoRemoteSnapshotClientTest`16、`MongoSnapshotSpecTest`2、`MongoAgentStateStoreContractTest`9、`MongoAgentStateStoreTest`39、`MongoBaseStoreContractTest`9、`MongoBaseStoreTest`23（合计 **147 `@Test`**，分属 **9 个测试类**）；`testutil/` 另有 `DockerAvailabilityCondition`(59 行) / `RequireDocker`(42 行) —— 二者为 Docker 门控辅助类（`@FunctionalInterface` 条件 + 组合注解），**实测 `@Test` 计数均为 0**（原文称「各 1 个 `@Test` 注解」不实，见 §4.1 复核节勘误 #3）。

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `org.mongodb:mongodb-driver-sync` | `MongoClient`/`MongoDatabase`/`MongoCollection` | **无真实驱动**；`client::MongoClient` trait（异步）+ `client::InMemoryMongoClient` | `Cargo.toml` 未引入 `mongodb` crate；**勘误**：根 `Cargo.toml:69` 已有 `mongodb = "3"`（workspace 依赖，汇总方已统一加入，见 `EXTENSIONS_GAP_TRACKER.md:187`），但**本 crate 未引用**（`crates/agentscope-extensions-mongodb/Cargo.toml` 无 `mongodb` 条目）→ 真实驱动仍待接入 |
| Jackson（`JsonUtils`） | `Document` ↔ Java 对象 | `serde` = `1`、`serde_json` = `1`（workspace） | — |
| BSON `Document`/`BsonDocument`/`IndexOptions` | 文档与索引描述 | `serde_json::{Map, Value}` + `index_utils::IndexSpec` | — |
| `GridFSBucket`/`GridFSFile` | 快照分块存储 | `MongoClient::gridfs_upload/download/exists/delete` + 内存 blob map | 已验证 255KB 分块（`GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024`）：文件文档含 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`，chunk 按 `n` 顺序重组并校验完整性与总长 |
| SLF4J `Logger` | 日志 | `tracing` = `0.1`（workspace） | `index_utils`/`sandbox_guard`/`state_store`/`store` 有埋点；**`snapshot` 已有 4 处埋点**（`snapshot.rs:126/133/163/217`，warn/info/debug）；**`client.rs` 仍无埋点**（原文「`snapshot`/`client` 无」不实，见 §4.1 复核节勘误 #6） |
| `sha2` = `0.10`（workspace） | 锁 id **及** `sandbox_guard` 的 SHA-256 | **保留**（收敛 `list_hash` 后仍被 `sandbox_guard.rs` 使用，见 §3.3） |
| `java.util.UUID` | 租约 token | `uuid` = `1`（v4，workspace） | — |
| `java.util.Date` / `java.time` | 时间戳（`expiresAt`/`_updated_at`/`createdAt`） | `chrono` = `0.4`（`Utc::now().timestamp_millis()`，workspace） | — |
| `ScheduledExecutorService` | 租约续约 | `tokio` = `1`（`time`/`rt`）+ `JoinHandle` 后台任务 | `lease_ttl / 3` 续约 |
| `java.util.concurrent` 阻塞桥接 | 同步 `BaseStore` 调异步后端 | `futures` = `0.3`（`futures::executor::block_on`） | `store.rs`/`sandbox_guard.rs` 同步 trait 经此调用异步 client |
| `RuntimeException` / `MongoCommandException` | 错误传播 | `thiserror` = `2`（`client::MongoError`） | 映射 `agentscope_core::error::Error` |
| JUnit5 + Mockito + Testcontainers | 离线/容器测试 | Rust `#[cfg(test)]` 内联（`InMemoryMongoClient`） | Java contract test 需 Docker（`testutil`） |

**harness/core 复用契约（对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1，未新建公共 store crate）**：
- `impl agentscope_core::state::AgentStateStore`（`state_store.rs:458`）—— trait 方法 `save/save_list/get/get_list/exists/delete/list_session_ids`（trait 定义见 `agentscope-core/src/state/store.rs:12`）。
- `impl agentscope_harness::filesystem::remote::store::BaseStore`（`store.rs:122`）—— `get/put/put_if_version/search/delete`；`StoreItem{key,value,version}`（`harness/src/filesystem/remote/store.rs:21`），`version` 从 1 起、0=未知。
- `impl agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot.rs:86`）—— `upload/download/exists`。
- `impl agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec`（`snapshot.rs:148`）—— `build(snapshot_id)`。
- `agentscope_harness::agent::sandbox::SandboxExecutionGuard` 是**具体 struct**（`harness/src/agent/sandbox/guard.rs:13`），非 trait → 通过 `MongodbGuardClient`（`sandbox_guard.rs:372`）镜像其方法供 harness 直接使用。**勘误**：`harness/src/agent/sandbox/mod.rs:19` 中 `mod guard;` 为**私有**，`sandbox::guard::SandboxExecutionGuard` 路径**不可公开引用**；公开路径是 `agentscope_harness::agent::sandbox::SandboxExecutionGuard`（`mod.rs:31` 重导出）。本 crate `lib.rs:15` / `distributed_store.rs:170` 的 rustdoc 链接仍指向私有路径 `sandbox::guard::SandboxExecutionGuard`（rustdoc 会解析失败，属文档瑕疵）。
- `agentscope_harness::agent::distributed_store::DistributedStore` 是**具体 struct**（`harness/src/agent/distributed_store.rs:14`，仅 `agent_state_store` + `isolation_scope`）→ `MongodbDistributedStore::to_harness_store()` 投影子集，其余组件经类型化访问器直连。
- `SandboxIsolationKey` 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`（`harness/src/agent/sandbox/key.rs:11`）。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 导出 10 模块 | 全部 | 已完成 | `lib.rs` 声明 `client`/`constants`/`distributed_store`/`index_utils`/`key_escaper`/`list_hash`/`sandbox_guard`/`snapshot`/`state_store`/`store` 并 re-export 公共类型 |
| `MongodbAgentStateStore`（impl core `AgentStateStore`） | `state/MongoAgentStateStore.java` | 已完成 | 复合 `_id`、`states`/`versions`/`hashes` 子文档、TTL；`save_if_version`/`get_version`/`UNVERSIONED = -1` 固有 API。**缺口**：builder 的 `ttl_days` 未在 `build()` 中生效（见 §3.3） |
| `MongodbStore`（impl harness `BaseStore`） | `store/MongoBaseStore.java` | 已完成 | US(0x1F) 编码 + 范围前缀搜索；`put_if_version` 对齐 harness CAS（`expected==0` = create-if-absent） |
| `MongodbRemoteSnapshotClient`（impl `RemoteSnapshotClient`） | `snapshot/MongoRemoteSnapshotClient.java` | 已完成 | `upload/download/exists` + 固有 `delete`；无 TTL |
| `MongodbSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/MongoSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec`；快照 id 校验沿用 harness |
| `MongodbSandboxExecutionGuard` + builder + lease + guard client | `sandbox/MongoSandboxExecutionGuard.java` | 已完成 | 插入 + 过期 reclaim + token 作用域释放 + `lease_ttl/3` 续约。**缺口**：builder `database_name` 未生效（见 §3.3） |
| `MongodbDistributedStore` 组合者 | `MongoDistributedStore.java` | 已完成（形态差异） | 五个组件访问器 + `to_harness_store()`；含 `on_delete` 级联删快照 |
| 无真实 DB 可用的内存后端 | Java Testcontainers / Mockito | 已完成 | `client::InMemoryMongoClient` |
| **离线单元测试** | Java 8 测试类 **147 `@Test`**（contract test 需 Docker） | **已完成（P1 收尾）** | 现状 lib **76** + doc-test **1**；crate **无 `tests/` 目录**（实测 `Test-Path` = False），全为 `#[cfg(test)]` 内联单测。本轮新增：GridFS 分块边界（`255*1024-1/-/+1`）、chunk 缺失/乱序报错、legacy 回退、`exists` 双路径、`database_name` 隔离、级联删快照、键转义 `.`/`$`/`\0`/`\` round-trip、TTL 索引、error-85 迁移（瞬态恢复 + 耗尽传播）、`delete_key` 公共路径 |

### 3.2 其它待办（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `supportsVersioning()` / `getVersioned()` | `state/MongoAgentStateStore.java:160,361` | **已完成（已上提 core trait，2026-10-22 复核修正）** | core `AgentStateStore` trait 已于 commit `9a8fd07`（2026-10-19）新增 `supports_versioning` / `get_versioned` / `save_if_version` 与 `UNVERSIONED` 常量；本 crate `state_store.rs:558/562/577` 已实装三者，同时保留固有 `get_version` / `save_if_version` API。**原「core trait 无版本方法 → 不迁移」判定已过时**（见 §4.1 复核节勘误 #8） |
| 单 key 删除 `delete(user, session, key)` | `state/MongoAgentStateStore.java:523` | **已完成** | 新增公共 `MongodbAgentStateStore::delete_key`（校验 key 后调 `delete_key_inner` 三路 `$unset`），`delete_key_inner` 保留为内部实现 → **dead-code warning 消除**，未用 `#[allow]` |
| `close()` / `ownsClient` / `fromConnectionString` | `MongoDistributedStore.java`；各组件 `close()` | 已完成（设计决定，不迁移） | Rust `Arc<dyn MongoClient>` 无连接所有权/关闭语义；依据：harness/core 无对应 trait，不重建平行抽象 |
| 快照 legacy 单文档回退（`download`/`exists`） | `snapshot/MongoRemoteSnapshotClient.java` | **已完成** | `download`/`exists`/`delete` 均先查 GridFS files，未命中则回退 legacy `{_id, data}` 单文档；有 `download_and_exists_fall_back_to_legacy_document` / `gridfs_takes_precedence_over_legacy_document` / `delete_gridfs_also_cleans_up_legacy_document` 用例覆盖 |
| GridFS 分块（255KB）/ 16GB 语义 | 同上 | **已完成** | `client.rs` 按 `GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024` 分块存 `{prefix}.files`/`{prefix}.chunks`（chunk 含 `files_id`/`n`/`data`），文件文档含 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`；下载按 `n` 重组并校验连续性/总长（缺失/乱序报 `corrupt GridFS file`）。与 Java `GridFSBucket` 等价（内存后端不设 16GB 上限，属实现差异） |
| `ensureIndexes`（TTL / 复合 / sparse） | 各组件 | 已完成 | state(`ttl_days`)、store、guard(TTL `expiresAt`) 均经 `create_index_with_migration` |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| ~~`list_hash` 双实现~~（**已收敛**） | **2026-09-19 解决**：删除 crate 本地 SHA-256 平行实现，`list_hash.rs` 降为 `pub use agentscope_core::state::{compute_hash, needs_full_rewrite}`；`state_store.rs` 直接调用 core。**理由**：①`serde_json::Value` 已实现 `Hash`（满足 core `T: Hash`，JDBC/Redis/PostgreSQL 同样调法）；②原注释声称「必须 SHA-256 才能跨进程稳定」不成立——`DefaultHasher::new()` 用固定常量种子（SipHasher13 keys=(0,0)），本就跨进程稳定；③原本地 `needs_full_rewrite` 在 `stored_hash==None && existing_count==0` 时返回 `!values.is_empty()`（即 `true`），**与 Java 相反**（Java Case3 `hasChanged(x, null)==false`，返回 `false`）。**存量数据影响**：hash 值格式变化（`size:N;i:hc,` hex vs SHA-256 hex）→ 首次写入旧数据会被判为需全量重写（`needs_full_rewrite==true`）并覆写 `hashes.<key>`，**数据无损且自愈**；详见 §4.1 | **已完成** |
| `MongodbSnapshotSpec::new` 忽略 `database_name` | **已完成（本轮）**：新增 `client::scoped_collection(db, coll)`，`MongodbRemoteSnapshotClient` 的 GridFS/legacy 调用均改用数据库限定的 collection key（`<db>::<bucket>`）；新增 `database_name_isolates_snapshots` / `spec_exposes_configured_database` 用例 | **已完成** |
| `lib.rs` doc 表把 `MongodbGuardClient` 写作 "implements `SandboxExecutionGuard`" | harness 该类型是 struct 非 trait，实际为镜像方法，文档不准确；且 `lib.rs:15` 的 rustdoc 链接指向**私有**模块路径 `sandbox::guard::SandboxExecutionGuard`（公开路径为 `sandbox::SandboxExecutionGuard`） | **待办（文档，非阻塞）** |
| `delete_key_inner` dead_code warning | **已完成（本轮）**：新增公共 `delete_key` 接入，`delete_key_inner` 成为其内部实现；`cargo build` 本 crate **0 warning** | **已完成** |
| `_cache: Arc<Mutex<()>>` 占位 | `distributed_store.rs`，对应 Java volatile double-checked 缓存的影子字段，实际未缓存任何值 | **待办（非阻塞）** |
| 组合者形态偏差 | Java `implements DistributedStore, AutoCloseable`；Rust 为组合 + `to_harness_store()`，未暴露 `close` / `ownsClient` | 已完成（设计决定，不迁移） |
| `snapshot.rs` 无 `tracing` 埋点 | **已修正（2026-10-22 复核）**：`snapshot.rs` 实有 4 处埋点（`126/133/163/217`）；Java `MongoRemoteSnapshotClient` 的 log.info/warn 已被等价覆盖 | **已完成** |
| `client.rs` 无 `tracing` 埋点 | Java `MongoIndexUtils`/`MongoSandboxExecutionGuard`/`MongoBaseStore`/`MongoAgentStateStore` 均有 log；Rust `client.rs`（`MongoClient` trait + `InMemoryMongoClient`）无任何 `tracing::` 调用（实测 0 处） | **待办（埋点，非阻塞）** |
| **builder `ttl_days` 未生效** | **2026-10-22 复核新发现**：Java `MongoAgentStateStore` 构造函数末尾调 `ensureIndexes(builder.ttlDays)`（`MongoAgentStateStore.java:152`），TTL 索引随 `build()` 自动创建。Rust `MongodbAgentStateStoreBuilder::build()`（`state_store.rs:641-651`）**只取 `client`/`collection`/`on_delete`，`ttl_days` 被丢弃**——`builder().ttl_days(7).build()` **不会**创建 TTL 索引，必须另调 `ensure_indexes(Some(7))`。（字段只写不读，故 rustc `dead_code` 不告警，与「0 warning」不矛盾） | **待办（行为差异，P1）** |
| **guard builder `database_name` 未生效** | **2026-10-22 复核新发现**：Java `MongoSandboxExecutionGuard` 构造时 `mongoClient.getDatabase(databaseName).getCollection(collectionName)`，锁文档**按库隔离**。Rust `MongodbSandboxExecutionGuardBuilder::build()`（`sandbox_guard.rs:348-357`）只把 `collection` 塞进 guard，**`database_name` 从未参与 collection key 限定**（全 crate 仅 `scoped_collection` 在 `snapshot.rs` 使用）→ 不同 `database_name` 的 guard **共用同一锁集合**，跨库互相抢锁。`MongodbDistributedStore::new` 传入的 `database_name` 对锁无效 | **待办（行为差异，P1）** |
| **级联删快照的 session id 语义** | **2026-10-22 复核新发现**：Java `MongoDistributedStore.cascadeDeleteSnapshots` 读 `sandbox/session/<sessionId>` 槽位的 `_sandbox_state`，该槽位由 harness `SessionSandboxStateStore` 写入（`sandbox/session/`+`key.getValue()`，即**沙箱 session id**）。Rust `find_session_snapshot_id` 沿用同一槽位命名，但 Rust harness **不存在 `SessionSandboxStateStore`**（`sandbox/state.rs` 只有 `SandboxStateStore` trait + `InMemorySandboxStateStore`，其 key 为 `SandboxIsolationKey::to_string()` 即 `scope:value`，且存的是 `SandboxState` 而非含 `snapshot.id` 的 JSON）→ Rust 侧**无任何生产路径写入 `states._sandbox_state.json`**，级联删除在真实链路中**恒为 no-op**（现有 2 个用例靠手工 `seed_session_snapshot` 造数据）。跨 crate 契约缺口 | **待办（跨 crate，P1）** |
| 真实 MongoDB 驱动接入 | 未在本 crate 引入 `mongodb` crate（**勘误**：根 `Cargo.toml:69` 已有 `mongodb = "3"`，但本 crate `Cargo.toml` 未引用）；无集成测试 | **待办（设计决定，不迁移；跨模块/汇总方）** |

### 3.4 设计决定（不迁移）汇总
| 项 | 依据 |
|---|---|
| 不新建公共 store crate；各 crate 自建 `XxxDistributedStore` | `EXTENSIONS_GAP_TRACKER.md` §2.1 已裁定 |
| 不引入真实 `mongodb` 驱动，改 `MongoClient` trait + `InMemoryMongoClient` | 保离线可编译/可测；驱动接入记 §4.2（**设计决定，不迁移**） |
| `close()` / 连接所有权 / `objectMapper` 注入 | harness/core 无对应语义，Rust 侧省略（**设计决定，不迁移**） |
| `supportsVersioning` / `getVersioned` 降为固有方法 | **已过时（2026-10-22 修正）**：core trait 已上提版本 API（commit `9a8fd07`），本 crate 已实装 → 见 §3.2 |

## 4. 项目状态与下一步
### 4.1 当前状态

#### 本会话（2026-09-19，P1/P2 收尾）实测
- **命令与结果**：
  - `cargo build -p agentscope-extensions-mongodb` → **exit 0，本 crate 0 warning**（此前唯一残留 warning `state_store.rs:432 delete_key_inner` 已消除；其余 warning 来自 core/harness，不在本 crate 范围）。
  - `cargo test -p agentscope-extensions-mongodb` → **lib 76 passed / 0 failed** + **doc-test 1 passed / 0 failed**（exit 0）。
- **用例数变化**：**61 → 76**（+15 lib 用例；doc-test 1 不变）。
- **本次变更文件**：`src/client.rs`、`src/snapshot.rs`、`src/state_store.rs`、`src/index_utils.rs`、`src/distributed_store.rs`（均仅测试与实现，无新增依赖，`Cargo.toml` 未改）。
- **实现变更**：
  1. **GridFS 元数据**：`gridfs_upload` 写入 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`（对齐 Java `GridFSUploadOptions.metadata({createdAt})` + 驱动 stamp）。
  2. **chunk 完整性校验**：`gridfs_download` 按 `n` 排序后要求连续 `0..chunk_count`，且重组长度等于存储 `length`；缺失/乱序/非 base64 均报 `corrupt GridFS file '<id>': ...`（`MongoError::Backend`）。
  3. **`database_name` 生效**：新增 `client::scoped_collection(db, coll)`，`MongodbRemoteSnapshotClient` 的 GridFS/legacy 调用改用数据库限定 key，跨库快照不再互相可见。
  4. **`delete_key` 接线**：新增公共 `MongodbAgentStateStore::delete_key(user, session, key)`，校验后三路 `$unset`（states/versions/hashes）；`delete_key_inner` 降为内部实现。
- **新增 offline 用例（全 `InMemoryMongoClient`，无网络/Docker）**：
  - snapshot：`gridfs_chunk_boundary_sizes_round_trip`（255KB-1 / 255KB / 255KB+1）、`gridfs_empty_payload_round_trips`、`missing_chunk_is_reported_as_corruption`、`out_of_order_duplicate_chunk_is_reported_as_corruption`、`gridfs_file_document_records_length_chunksize_and_upload_date`、`database_name_isolates_snapshots`、`spec_exposes_configured_database`。
  - index_utils：`migration_recovers_after_transient_conflicts`、`migration_gives_up_after_max_attempts`（error-85 最多 3 次 drop+重试）。
  - state_store：`public_delete_key_removes_value_version_and_hash`、`delete_key_validates_state_key`、`key_escaper_round_trips_dots_dollars_nul_and_backslash`、`ttl_index_is_created_only_when_positive`。
  - distributed_store：`deleting_session_cascades_to_snapshot`、`deleting_session_without_snapshot_does_not_error`。
- **测试基础设施**：`InMemoryMongoClient` 新增测试钩子 `force_index_conflicts(n)`，用于确定性验证 error-85 迁移的恢复与耗尽路径；不改变生产路径行为。

#### 历史基线（上一轮，供对照）
- **编译/测试基线（2026-09-19，本会话实测）**：`cargo build -p agentscope-extensions-mongodb` 与 `cargo test -p agentscope-extensions-mongodb` **均通过**（exit 0）。
  - `cargo build`：仅 1 条 warning（`state_store.rs:432 delete_key_inner` 从未使用，既有死代码，非阻塞）。其余 warning 来自 core/harness，不在本 crate 范围。
  - `cargo test -p agentscope-extensions-mongodb`：**lib 61 passed / 0 failed** + **doc-test 1 passed / 0 failed**。
  - 测试数变化：旧记录 62 → 现 61（**-1**）。原因：`list_hash.rs` 原 5 个测试中 2 个属纯算法重复覆盖（`hash_is_stable_and_order_sensitive`、`empty_hash_is_deterministic`，core `list_hash` 已有等价覆盖）已删除；原错误断言的 `new_absent_key_with_content_needs_rewrite` 被改名并修正为 `absent_hash_with_no_existing_data_needs_no_rewrite`，另新增 `missing_hash_with_existing_data_needs_rewrite`（Java Case2）。净：-2 +1 = -1。**非降低覆盖**，而是去重；语义测试（追加/改动/缩短/无 hash 有数据）全部保留。
- **本次变更（2026-09-19，`list_hash` 收敛到 core）**：
  1. `src/list_hash.rs` — 删除本地 `compute_hash`（SHA-256 over canonical JSON）与 `needs_full_rewrite` 实现及 `sha2` 引用；改为 `pub use agentscope_core::state::{compute_hash, needs_full_rewrite};`（**公开路径 `crate::list_hash::{compute_hash, needs_full_rewrite}` 保持不变**，无损 re-export）。
  2. `src/state_store.rs` — 调用点（原 364、371 行）改为直接 `use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};`，移除 `use crate::list_hash;`。
  3. **测试断言修正**：原 `new_absent_key_with_content_needs_rewrite` 断言 `assert!(needs_full_rewrite(&[json!(1)], None, 0))` 期望 `true`，**与 Java 相反**。依据 Java `ListHashUtil.needsFullRewrite` Case3（`prefix = currentValues.subList(0, existingCount)`，空前缀）+ `hasChanged(x, null) == false` → 应返回 `false`。已改名并断言 `!needs_full_rewrite(...)`。这是**修正错误断言以对齐 Java**，非编译/测试失败规避。
  4. `Cargo.toml` — **未改动**：`sha2` 仍被 `sandbox_guard.rs`（`compose_lock_id` 的 SHA-256）使用，不得移除（已 `rg` 核实）。
  5. **残留检查**：`rg "fn compute_hash|fn needs_full_rewrite" crates/agentscope-extensions-mongodb` **无结果**（exit 1）→ 确认无平行实现残留。
- **存量数据影响（hash 值格式变化的自愈性）**：core 与旧本地实现的 hash 字符串格式不同（core：`"size:N;idx:hc,..."` 的 DefaultHasher hex vs 旧：SHA-256 hex）。对已存在的 MongoDB 会话文档：下次写列表时 `stored_hash != compute_hash(prefix)` → `needs_full_rewrite == true` → 走全量重写分支覆写 `states.<key>` 与 `hashes.<key>`。**数据无损**（仅重写一次，之后恢复增量追加），属**自愈**，无需迁移脚本。
- **本会话修复内容（3 处根因，均在本 crate 内）**：
  1. `distributed_store.rs`（tests）— 测试引用了 harness `sandbox` 的**私有子模块**路径 `sandbox::guard::SandboxExecutionGuard` / `sandbox::key::SandboxIsolationKey`，触发 `E0603 module is private`（这掩盖了 `cargo build` 已通过、但 `cargo test` 失败的差异）。改为使用 harness 的**公开重导出** `agentscope_harness::agent::sandbox::SandboxIsolationKey`；并在 tests 模块内按需 `use ... as _;` 引入 `BaseStore` / `SandboxSnapshotSpec` trait（曾误判为 lib 级 unused import，移出后反而破坏 test 编译，已回收进 tests 模块）。
  2. `client.rs`（`InMemoryMongoClient` 的 `find_one_and_update` / `update_one`）— upsert 分支在 `_id` 已存在却未命中 filter（例如带 `$exists:false` 的 create-if-absent）时**静默追加重复 `_id` 文档**，与真实 MongoDB / Java 的 duplicate-key(11000) 语义不符，导致 `state_store::versions_increment_and_cas_works` 期望“第二次 create-if-absent 返回 `UNVERSIONED`”落空。修复：upsert 插入前若已存在同 `_id` 文档则返回 `MongoError::DuplicateKey`（与 `insert_one` 及 store.rs `put_if_version` 的既有语义一致）。
  3. `list_hash.rs::needs_full_rewrite` — 原实现把**整个新列表**的哈希与存储哈希比较，导致等长/追加场景恒判定为需全量重写，`appended_list_does_not_need_rewrite` 失败。按 Java `ListHashUtil.needsFullRewrite` 语义重写：列表收缩 → 重写；无存储哈希但有既有数据 → 重写；否则比较**长度为 `existing_count` 的前缀哈希**与存储哈希，匹配即可增量追加。
- **未触碰**：根 `Cargo.toml`、`agentscope-core`、`agentscope-harness`、其它 crate。
- **未删除任何既有功能**。
- **本回填会话核对基线（汇总方提供，未重跑 cargo）**：`cargo build -p agentscope-extensions-mongodb` 通过；`cargo test -p agentscope-extensions-mongodb` = lib **62 passed / 0 failed** + **1 doc-test passed**；全量 `cargo test --workspace` = **1746 passed / 0 failed**（86 test target，exit 0）。crate 无 `tests/` 集成目录（实测 `Test-Path` = False），测试全部为 `#[cfg(test)]` 内联单测。

#### 静态复核记录（2026-10-22）

> **复核方式**：**静态核对，未编译、未运行 cargo**（本会话为静态复核子智能体，禁止 `cargo build` / `cargo test`）。结论来源：逐文件读源码 + PowerShell 脚本统计行数/测试数 + `git log` / `git blame` 溯源 + `rustc` 独立最小样例验证 lint 行为（样例写在**临时目录**，非本仓 crate）。
> **统计口径**：行数用 `Get-Content <f> | Measure-Object -Line`（与原 tracker 同口径；`Get-Content .Count` 含末尾空行，一并记录供对照）。测试数按 `^\s*#\[(tokio::)?test\]`（Rust）/ `^\s*@Test`（Java）统计。

**实测统计（2026-10-22）**

| 项 | 实测值 | 备注 |
|---|---|---|
| Java 主代码 | **2235 行 / 9 类** | `Measure-Object -Line`；`.Count` 口径 2420 行。分文件：MongoConstants 38、MongoDistributedStore 299、MongoIndexUtils 160、MongoKeyEscaper 147、MongoSandboxExecutionGuard 402、MongoRemoteSnapshotClient 178、MongoSnapshotSpec 33、MongoAgentStateStore 676、MongoBaseStore 302 |
| Java 测试代码 | **2429 行 / 11 文件（9 测试类 + 2 testutil）** | `.Count` 口径 2830 行 |
| Java `@Test` | **147**（分属 9 个测试类） | 15/10/24/16/2/9/39/9/23；`testutil` 2 个类 **`@Test` 计数为 0** |
| Rust src | **11 文件 / 4121 行** | `.Count` 口径 4512 行。分文件：client 1019、constants 18、distributed_store 343、index_utils 265、key_escaper 159、lib 63、list_hash 53、sandbox_guard 482、snapshot 527、state_store 835、store 357 |
| Rust 测试 | **76**（`#[test]` 21 + `#[tokio::test]` 55） | 分文件：client 5、distributed_store 9、index_utils 6、key_escaper 6、list_hash 4、sandbox_guard 6、snapshot 18、state_store 15、store 7 = **76** ✅ 与 tracker 记载一致 |
| Rust 集成测试目录 | **不存在**（`Test-Path tests` = False） | 全部为 `#[cfg(test)]` 内联单测 ✅ 与 tracker 一致 |
| doc-test | **1**（`lib.rs:35` `no_run` 示例） | 与 tracker 一致 |

**对齐结论表**

| Java 类 → Rust 对应 | 存在 | 行为覆盖 | 结论 |
|---|---|---|---|
| `MongoConstants` → `constants.rs` | ✅ | 5/5 常量逐字对齐（`agentscope` / `agentscope_sessions` / `agentscope_base` / `agentscope_snapshots` / `agentscope_sandbox_locks`） | **完全对齐** |
| `MongoKeyEscaper` → `key_escaper.rs` | ✅ | 编码逐字节一致（`.`→`\E`、`$`→`\$`、`\0`→`\N`、`\`→`\\`）；未知转义保留原样；递归遍历 map/list | **完全对齐**（6 个 Rust 用例） |
| `MongoIndexUtils` → `index_utils.rs` | ✅ | `MAX_MIGRATION_ATTEMPTS=3`、按 key pattern 查真实索引名后 drop、容错 27/ns-not-found、重建循环一致 | **完全对齐**（含 2 个新增迁移用例） |
| `MongoAgentStateStore` → `state_store.rs` | ✅ | 复合 `_id{user,session}`、`states`/`versions`/`hashes` 子文档、key 正则 `^[a-zA-Z_][a-zA-Z0-9_]*$`、CAS 三分支（0=create-if-absent / UNVERSIONED=无条件 / 期望版本）、`$size` 语义等价（`first_field_size`）、list 增量追加 + 全量重写 | **基本对齐**，两处偏差：① builder `ttl_days` 未生效（**P1**）；② per-key `delete` 为固有方法 `delete_key`（Java 是 core trait 的 `default void delete(u,s,key)`，Rust core trait **无**该方法）→ 属**设计差异**，但调用面收窄 |
| `MongoBaseStore` → `store.rs` | ✅ | US(0x1F) 编码 + 尾分隔符 + `$gte/$lt`（`\u{FFFF}` 上界）+ 按 key 排序 + skip/limit、`version` 从 1 起、`put` 每次 +1、`put_if_version(0)`=insert-if-absent（11000→false）、`supports_atomic_session_storage`=true | **完全对齐** |
| `MongoRemoteSnapshotClient` → `snapshot.rs` | ✅ | GridFS 分块 255KB（`GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024`）、`<bucket>.files`/`<bucket>.chunks`、chunk 字段 `files_id`/`n`/`data`、文件文档 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`、下载按 `n` 排序并要求连续 `0..n-1` 且总长 == `length`（否则 `corrupt GridFS file '<id>': ...`）、`exists`/`download`/`delete` 均 GridFS 优先 + legacy `{_id,data}` 回退、`delete` 双路径清理 | **完全对齐**（本轮 4 项改动属实，见下） |
| `MongoSnapshotSpec` → `snapshot.rs` | ✅ | Java `extends RemoteSnapshotSpec` → Rust `impl SandboxSnapshotSpec` + 内持 harness `RemoteSnapshotSpec`；`database_name` 已生效 | **对齐**（形态差异已记录） |
| `MongoSandboxExecutionGuard` → `sandbox_guard.rs` | ✅ | insert 抢占 → 11000 → `expiresAt<=now` reclaim → 轮询至 `lock_timeout`；UUID token 作用域续约/释放；`lease_ttl/3` 续约；`compose_lock_id` = `lock:`+hex(SHA-256(lowercase(scope)+":"+value)) 与 Java **字节级一致**；TTL 索引 `expiresAt`/`expireAfterSeconds(0)` | **基本对齐**，一处偏差：builder `database_name` 未生效（**P1**） |
| `MongoDistributedStore` → `distributed_store.rs` | ✅ | 五组件访问器 + `to_harness_store()` 投影 + `on_delete` 级联删快照 | **形态差异（设计决定）**，但级联链路在 Rust harness 侧无生产写入方（**P1**，见 §3.3） |
| （测试替身）Testcontainers/Mockito → `client.rs::InMemoryMongoClient` | ✅ | 11000 重复键、`$set/$inc/$push/$pushEach/$setOnInsert/$unset`、dot-path 读写、`$gte/$lt/$lte/$exists`、error-85 冲突 | **忠实**（另含 2 个测试钩子：`force_index_conflicts(n)`、`document_count`） |

**本轮改动属实性核查（4 项）**

| 声明 | 核实 | 证据 |
|---|---|---|
| GridFS 255KB 分块 | ✅ 属实 | `client.rs:34` `GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024`；`client.rs:879` `data.chunks(chunk_size.max(1))`；边界用例 `snapshot.rs:470` 覆盖 255KB-1/255KB/255KB+1 → 1/1/2 块 |
| legacy 回退 | ✅ 属实 | `snapshot.rs:106/116/141` `exists_legacy`/`download_legacy`/`delete_legacy`；`snapshot.rs:215` 回退分支；3 个用例覆盖（`download_and_exists_fall_back_to_legacy_document`、`gridfs_takes_precedence_over_legacy_document`、`delete_gridfs_also_cleans_up_legacy_document`） |
| chunk 完整性校验 | ✅ 属实 | `client.rs:938-964`：排序后要求 `n` 连续、`reassembled length == stored length`，否则 `MongoError::Backend("corrupt GridFS file ...")`；2 个损坏用例 |
| `database_name` 生效 | ⚠️ **部分属实** | 快照侧属实：`client.rs:72` `scoped_collection(db, coll)` + `snapshot.rs:93` `scoped_bucket()`，用例 `database_name_isolates_snapshots` ✅。**但 guard 侧不属实**：`sandbox_guard.rs:348` `build()` 丢弃 `database_name`，锁集合未按库隔离（**P1**） |
| `delete_key` 接线（消除 dead code） | ✅ 属实 | `state_store.rs:438` 公共 `delete_key` → `state_store.rs:447` `delete_key_inner`；`delete_key_inner` 现被调用，rustc `dead_code` 不再触发；全 crate 0 处 `#[allow(dead_code)]` ✅ |

**新发现缺口**

| 级别 | 缺口 | 影响 |
|---|---|---|
| **P1** | builder `ttl_days` 未生效（`state_store.rs:641-651` 丢弃字段） | Java `build()` 自动建 TTL 索引；Rust `builder().ttl_days(7).build()` **静默不建**，须另调 `ensure_indexes(Some(7))` → 会话永不过期，**无报错、无日志** |
| **P1** | guard builder `database_name` 未生效（`sandbox_guard.rs:348-357`） | 不同库共用 `agentscope_sandbox_locks` 集合 → 跨库互相抢锁 / 误 reclaim |
| **P1** | 级联删快照链路在 Rust harness 侧**无写入方** | Java 由 `SessionSandboxStateStore` 写 `sandbox/session/<id>` + `_sandbox_state`；Rust harness 无该类（`sandbox/state.rs` 只有 `SandboxStateStore` trait + `InMemorySandboxStateStore`，key 为 `SandboxIsolationKey::to_string()` 即 `scope:value`，且存的是 `SandboxState` 而非含 `snapshot.id` 的 JSON）→ `states._sandbox_state.json` **无生产写入** → 级联删除恒 no-op，快照成孤儿（跨 crate 契约缺口） |
| **P2** | `client.rs` 无 `tracing` 埋点（实测 0 处） | 内存后端故障无诊断线索（Java 对应类均有 log） |
| **P2** | `lib.rs:15` / `distributed_store.rs:170` rustdoc 链接指向**私有**模块 `sandbox::guard::SandboxExecutionGuard` | rustdoc 链接失效（公开路径应为 `sandbox::SandboxExecutionGuard`） |
| **P2** | `MongodbGuardClient` 释放语义与 harness 生命周期不接 | `MongodbGuardClient::try_enter` 返回 harness `SandboxLease`，但 harness `SandboxLease::drop` **不调用 guard.release**（`lease.rs:44-53` 明确留空）；MongoDB 锁只能靠 `MongodbLeaseHandle::drop` 释放，而该 handle 被 `MongodbGuardClient` 存入内部 map、**仅在显式 `release(key)` 时弹出** → harness 正常路径（`SandboxLifecycleMiddleware` → `manager.release`）**不会**调用 `MongodbGuardClient::release` → 锁文档只能等 TTL 过期（默认 30 分钟）才释放，**并发槽位被长时间占用** |
| **P2** | `MongodbDistributedStore::new` 的 `on_delete` 回调用 `futures::executor::block_on` | 在 tokio 运行时内同步阻塞；且回调失败被 `let _ =` 吞掉（Java 记 `log.warn`） |
| **P2** | 内存后端不设 16GB 上限 | 已在 tracker 记录，属实现差异（保持） |

**勘误列表（原文 → 实际）**

| # | 原文 | 实际 |
|---|---|---|
| 1 | `主代码 **2231 行（9 类）**`（§1） | **2235 行（9 类）**（`Measure-Object -Line` 实测） |
| 2 | `测试 **2427 行（8 测试类 + 2 testutil，147 个 @Test）**`（§1） | **2429 行 / 9 测试类 + 2 testutil**（测试类实为 9 个，非 8） |
| 3 | `testutil/ … 各 1 个 @Test 注解用于 Docker 门控`（§2 脚注） | 两个 testutil 类的 `@Test` **计数均为 0**（`DockerAvailabilityCondition` 是 `ExecutionCondition` 实现，`RequireDocker` 是 `@Target(TYPE)` 组合注解） |
| 4 | `mongodb.store.MongoBaseStore（298 行）`（§2） | **302 行** |
| 5 | `Cargo.toml 未引入 mongodb crate；EXTENSIONS_GAP_TRACKER.md:122 建议后续统一加入 [workspace.dependencies]，当前待汇总`（§2.1） | 根 `Cargo.toml:69` **已有** `mongodb = "3"`（汇总方已统一加入，见 `EXTENSIONS_GAP_TRACKER.md:187`）；**但本 crate `Cargo.toml` 未引用** → 真实驱动仍待接入 |
| 6 | `snapshot.rs / client.rs 无 tracing 埋点`（§2.1 / §3.3） | `snapshot.rs` **已有 4 处**埋点（`126/133/163/217`）；仅 `client.rs` 无（实测 0 处）→ §3.3 原行已拆分修正 |
| 7 | 以 `sandbox::guard::SandboxExecutionGuard` 作为公开路径引用 | `harness/src/agent/sandbox/mod.rs:19` 为 `mod guard;`（**私有**）；公开路径是 `agentscope_harness::agent::sandbox::SandboxExecutionGuard`（`mod.rs:31` 重导出）。crate 内 `lib.rs:15`、`distributed_store.rs:170` 仍引用私有路径 |
| 8 | §3.2「`supportsVersioning()` / `getVersioned()` … core `AgentStateStore` trait 无版本方法」 | **已过时**：core trait 于 2026-10-19（commit `9a8fd07`）新增 `supports_versioning` / `get_versioned` / `save_if_version` + `UNVERSIONED` 常量；本 crate `state_store.rs:558/562/577` 已实装三者 → 原「不迁移」判定应改「**已完成（已上提 trait）**」 |
| 9 | §1「当前状态：已完成（本轮 P1/P2 收尾）」 | 应下调为「**进行中**」——静态复核发现 3 项 P1 未闭环（见上表） |

### 4.2 下一步与阻塞
1. **无阻塞**。本次变更未新增对 harness/core 的改动需求；`Cargo.toml` 未改，无新增依赖。
2. **已解决（2026-09-19 会话）**：`delete_key_inner` 已接入公共 `delete_key`，dead-code warning 消除（未用 `#[allow]`）；`snapshot` legacy 回退 + GridFS 分块已落地并有边界/损坏用例；`database_name` 已在**快照侧**生效。
3. **剩余缺口（设计决定，不迁移）**：
   - **真实 `mongodb` 驱动接入**：本 crate 未引用 `mongodb` crate（根 workspace 已有 `mongodb = "3"`）。由**汇总方**/后续会话在本 crate `Cargo.toml` 增加 `mongodb = { workspace = true }` 并实现同一 `MongoClient` trait 即可。内存后端不设 16GB 上限，属实现差异。
   - **`close()` / 连接所有权 / `objectMapper` 注入**：harness/core 无对应语义，不迁移。
   - **`supportsVersioning` / `getVersioned`**：**已上提为 core trait 方法**（commit `9a8fd07`，2026-10-19），本 crate `state_store.rs` 已实装 → 见 §4.1 复核节勘误 #8。
   - **`_cache: Arc<Mutex<()>>` 占位**：对应 Java volatile double-checked 缓存的影子字段，不缓存任何值；保留以备后续 harness 扩展。
4. **非阻塞文档/埋点项**：`lib.rs` doc 表述与 `MongodbGuardClient` 实际（镜像方法，非 trait 实现）不符，且 rustdoc 链接指向私有模块路径 `sandbox::guard::SandboxExecutionGuard`；`client.rs` 无 `tracing` 埋点（`snapshot.rs` 已有 4 处）。
5. **后续建议**：真实驱动接入后，将 `chunkSize` 作为 `GridFSBucket` 参数（当前固定 255KB 默认值），并验证 `uploadDate`/`metadata` 在真驱动下的字节级一致。
6. **2026-10-22 静态复核新增待办**：① builder `ttl_days` 生效（§3.3，P1）；② guard `database_name` 生效（§3.3，P1）；③ 级联删快照的跨 crate 契约（Rust harness 需提供写 `states._sandbox_state.json` 的 `SessionSandboxStateStore` 等价物，或改由本 crate 直接管理快照 id，P1）；④ `MongodbGuardClient` 与 harness `SandboxLease` 的释放闭环（§3.3，P2）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mongodb` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mongodb` / `cargo test -p agentscope-extensions-mongodb` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mongodb/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。