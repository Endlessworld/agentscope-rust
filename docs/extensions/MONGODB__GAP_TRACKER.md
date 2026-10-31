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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 **106 passed / 0 failed**：99 lib + 5 live 集成（`tests/mongodb_live.rs`，`AGENTSCOPE_TEST_MONGODB_URL` 门控）+ 2 另一 target，另 doc-test 1）—— **真驱动已接入**（`src/client_mongodb.rs`，官方 `mongodb = "3"`，18 个方法全实现）；三项 P1（builder `ttl_days`、guard `database_name`、级联删快照链路）均已闭环（级联链路经 harness `AgentStateStoreSandboxStateStore` 真实写入方验证），余 5 项 P2 全清零（§3.3）；`DistributedStore` 五槽投影已完成（§4.2-1 不成立）。详见 §3.3 / §4.1 与 history/MONGODB__MIGRATION_HISTORY.md |

> **2026-10-31 全量对齐复核**：新增 §3.5（`MONGO-01..MONGO-22`）与 §4.3（`MONGO-D01..D06` / `MONGO-X01..X11`）。结论 **P0=0 / P1=4 / P2=6 / 待裁定=1**；§3.1–§3.4 历史清单未改动。§1「当前状态」行不需调整（能力面仍为已完成，残余为入口默认值、索引自动创建、互斥失败降级与读放大型问题）。

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `mongodb.MongoConstants`（38 行） | 默认库名 + 四个集合名常量 | `constants.rs`（`DEFAULT_DATABASE` / `SESSIONS_COLLECTION` / `BASE_STORE_COLLECTION` / `SNAPSHOTS_COLLECTION` / `SANDBOX_LOCKS_COLLECTION`） | 常量值逐一对齐 |
| `mongodb.MongoDistributedStore`（299 行，`implements DistributedStore, AutoCloseable`） | 组合四组件 + 生命周期 + 级联删快照 | `distributed_store.rs::MongodbDistributedStore` | Java 直接实现接口；Rust 用组合 + `to_harness_store()` 投影（harness `DistributedStore` 是 struct，仅 `agent_state_store`+`isolation_scope`）。`new` 内注册 `on_delete` 级联删快照 |
| `mongodb.MongoIndexUtils`（160 行） | 索引创建 + error 85 冲突迁移（按 key pattern 查真实索引名后 drop+重试） | `index_utils.rs::create_index_with_migration` / `IndexSpec` / `key_pattern_json` / `MAX_MIGRATION_ATTEMPTS=3` | 语义保留；`tracing::info/warn/debug` 埋点 |
| `mongodb.MongoKeyEscaper`（147 行） | 递归转义 `.`/`$`/`\0`/`\` 键名 | `key_escaper.rs::escape/unescape/escape_key/unescape_key` | 编码对齐（`.`→`\E`、`$`→`\$`、`\0`→`\N`、`\`→`\\`） |
| `mongodb.state.MongoAgentStateStore`（676 行） | 会话状态：单值 + 列表 + 版本 CAS + 列表哈希 + TTL | `state_store.rs::MongodbAgentStateStore`（+ builder / `UNVERSIONED` / `validate_state_key`），`impl agentscope_core::state::AgentStateStore` | 复合 `_id {user_id, session_id}`；`states`/`versions`/`hashes` 子文档；`save_if_version`/`get_version` 为固有方法 |
| `mongodb.store.MongoBaseStore`（302 行） | 命名空间 KV + CAS + 前缀搜索 | `store.rs::MongodbStore`（+ `NS_SEPARATOR` / `NS_UPPER_BOUND`），`impl agentscope_harness::filesystem::remote::store::BaseStore` | US(0x1F) 命名空间编码 + `$gte/$lt` 范围前缀匹配；`version` 从 1 起，`put` 每次成功写 +1 |
| `mongodb.sandbox.MongoSandboxExecutionGuard`（402 行） | 分布式锁（insert + 过期 reclaim + 租约续约） | `sandbox_guard.rs::MongodbSandboxExecutionGuard` + `Builder` + `MongodbLeaseHandle` + `MongodbGuardClient` + `compose_lock_id` | harness `SandboxExecutionGuard` 为**具体 struct 非 trait** → `MongodbGuardClient` 镜像其方法（`try_enter/release/is_held/active_count`）；`compose_lock_id` 与 Java **字节级一致**（`lock:`+hex(SHA-256(`scope:value`))，scope 小写）；`database_name` **已生效**：`build()` 以 `client::scoped_collection(db, collection)` 生成数据库限定的 `collection_key`，所有读写/续约/释放/建索引均走该 key（见 §3.3、§4.1） |
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

### 2.2 Java 测试类 → Rust 用例映射（2026-10-31）

> 口径：Java 9 个测试类共 **147 `@Test`**（其中 `setUp`/`tearDown` 之外的用例；builder 参数校验、null 校验类占相当比例）→ Rust 侧 **82 → 90** 内联用例（本轮 +8）。Java 用例普遍需要 Docker（`testutil/RequireDocker`）或 Mockito；Rust 用例**全部离线**（`InMemoryMongoClient`）。

| Java 测试类（@Test） | 覆盖语义 | Rust 用例 | 结论 |
|---|---|---|---|
| `MongoDistributedStoreTest`(15) | 构造 / 默认库 / 组件非空 / 缓存实例 / close / fromConnectionString | `defaults_to_agentscope_database`、`state_store_component_round_trips`、`base_store_component_round_trips`、`snapshot_spec_component_round_trips`、`guard_component_exposed`、`harness_projection_carries_state_store`、`harness_projection_fills_every_composite_slot`、**`database_name_partitions_state_and_base_store`（新）** | 已覆盖；`close` / `ownsClient` / `fromConnectionString` / null 校验 = 设计决定（§3.4） |
| `MongoIndexLifecycleContractTest`(8) | 复合索引 / TTL opt-in / guard TTL / snapshot 无 TTL / 级联删 / legacy 回退 | `ensure_indexes_creates_all_component_indexes`、`creates_index_when_absent`、`idempotent_create_is_noop`、`migrates_conflicting_index_by_renamed_name`、`builder_ttl_days_is_retained_and_creates_ttl_index`、`ttl_index_is_created_only_when_positive`、`deleting_session_cascades_to_snapshot`、`deleting_session_cascades_to_snapshot_written_by_the_harness_store` | 已覆盖（索引形态由 `IndexSpec` 断言，无需真库） |
| `MongoSandboxExecutionGuardTest`(24) | 获取 / 回收 / 轮询 / 超时 / 错误传播 / 租约 token / 续约 / owner | `acquire_and_release_roundtrip`、`second_acquire_times_out_while_held`、`expired_lock_can_be_reclaimed`、`release_is_token_scoped`、**`lock_document_records_owner_and_lease_ttl`（新）**、**`lease_renewal_extends_the_lock_expiry`（新）**、`database_name_partitions_locks`、`adapter_mirrors_harness_guard_surface`、`trait_acquisition_and_release_are_awaitable`、**`harness_lifecycle_releases_the_lock_exactly_once`（新）** | 已覆盖；builder 非正值校验 = `Duration` 类型层面不可表达 |
| `MongoRemoteSnapshotClientTest`(16) | upload / download / exists / delete + legacy + null 参数 | `upload_download_exists_delete_roundtrip`、`upload_replaces_existing`、`download_missing_errors`、legacy 3 例、`gridfs_*` 6 例、`database_name_isolates_snapshots` | 已覆盖；null 参数 = 类型系统保证 |
| `MongoSnapshotSpecTest`(2) | 默认 / 自定义 collection | `spec_builds_snapshot_and_round_trips`、`spec_exposes_configured_database`、`spec_rejects_unsafe_snapshot_id` | 已覆盖 |
| `MongoAgentStateStoreContractTest`(9) | versioning / CAS / 并发创建 / listSessionIds | **`trait_versioning_surface_matches_core_contract`（新）**、`versions_increment_and_cas_works`、`unconditional_save_if_version_uses_negative_one`、**`concurrent_create_if_absent_only_one_writer_wins`（新）**、`exists_delete_and_list_session_ids` | 已覆盖 |
| `MongoAgentStateStoreTest`(39) | builder / ttl / save / get / list / version / delete / escape / 校验 | `save_and_get_single_value`、`list_append_and_full_rewrite`、**`set_on_insert_records_session_identity`（新）**、**`blank_session_id_is_rejected`（新）**、`delete_single_key_unset`、`public_delete_key_removes_value_version_and_hash`、`delete_key_validates_state_key`、`dotted_and_dollar_keys_are_escaped`、`anonymous_user_normalisation`、`user_namespaces_are_isolated`、`invalid_state_key_is_rejected`、`key_escaper_round_trips_dots_dollars_nul_and_backslash`、`on_delete_callback_fires` | 已覆盖；`closeWithExternalClient` = 设计决定 |
| `MongoBaseStoreContractTest`(7) | 版本从 1 / CAS / create-if-absent / delete 幂等 / 子命名空间 | `put_assigns_increasing_versions`、`put_if_version_create_if_absent`、`delete_removes_only_target_key`（本轮补幂等断言）、`search_includes_child_namespaces` | 已覆盖 |
| `MongoBaseStoreTest`(25) | CRUD / 11000 重试 / 转义 / 排序分页 | `get` / `put` / `delete` / `search_is_scoped_paginated_and_sorted` / `keys_with_dots_and_dollars_round_trip` / `invalid_keys_are_rejected` + `client::duplicate_id_errors` | 已覆盖 |
| `testutil/{DockerAvailabilityCondition,RequireDocker}`(0 `@Test`) | Docker 门控 | —（无等价物） | Rust 侧无 Docker 依赖，无需门控辅助类 |

**harness/core 复用契约（对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1，未新建公共 store crate）**：
- `impl agentscope_core::state::AgentStateStore`（`state_store.rs:458`）—— trait 方法 `save/save_list/get/get_list/exists/delete/list_session_ids`（trait 定义见 `agentscope-core/src/state/store.rs:12`）。
- `impl agentscope_harness::filesystem::remote::store::BaseStore`（`store.rs:122`）—— `get/put/put_if_version/search/delete`；`StoreItem{key,value,version}`（`harness/src/filesystem/remote/store.rs:21`），`version` 从 1 起、0=未知。
- `impl agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot.rs:86`）—— `upload/download/exists`。
- `impl agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec`（`snapshot.rs:148`）—— `build(snapshot_id)`。
- `agentscope_harness::agent::sandbox::SandboxExecutionGuard` 是**具体 struct**（`harness/src/agent/sandbox/guard.rs:13`），但 harness 另有 `SandboxGuard` trait（`guard.rs:14`，方法 `try_enter`/`release`/`is_held`/`active_count`）→ **已解决（2026-10-28）**：`MongodbGuardClient`（`sandbox_guard.rs`）已 `impl SandboxGuard`（四方法委托固有方法），并经 `to_harness_store()` 投进 `sandbox_execution_guard` 槽。**勘误**：`harness/src/agent/sandbox/mod.rs:19` 中 `mod guard;` 为**私有**，`sandbox::guard::SandboxExecutionGuard` 路径**不可公开引用**；公开路径是 `agentscope_harness::agent::sandbox::SandboxExecutionGuard`（`mod.rs:31` 重导出）。
- `agentscope_harness::agent::distributed_store::DistributedStore` 是**具体 struct**（`harness/src/agent/distributed_store.rs:14`），**2026-10-28 起含 5 槽**：`agent_state_store` / `isolation_scope` / `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard` → `MongodbDistributedStore::to_harness_store()` 已**全量投影**（详见文末执行记录）。
- `SandboxIsolationKey` 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`（`harness/src/agent/sandbox/key.rs:11`）。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 导出 10 模块 | 全部 | 已完成 | `lib.rs` 声明 `client`/`constants`/`distributed_store`/`index_utils`/`key_escaper`/`list_hash`/`sandbox_guard`/`snapshot`/`state_store`/`store` 并 re-export 公共类型 |
| `MongodbAgentStateStore`（impl core `AgentStateStore`） | `state/MongoAgentStateStore.java` | 已完成 | 复合 `_id`、`states`/`versions`/`hashes` 子文档、TTL；`save_if_version`/`get_version`/`UNVERSIONED = -1` 固有 API。builder 的 `ttl_days` 由 `build()` 保留，无参 `ensure_indexes()` 据此建 TTL 索引（见 §3.3、§4.1） |
| `MongodbStore`（impl harness `BaseStore`） | `store/MongoBaseStore.java` | 已完成 | US(0x1F) 编码 + 范围前缀搜索；`put_if_version` 对齐 harness CAS（`expected==0` = create-if-absent） |
| `MongodbRemoteSnapshotClient`（impl `RemoteSnapshotClient`） | `snapshot/MongoRemoteSnapshotClient.java` | 已完成 | `upload/download/exists` + 固有 `delete`；无 TTL |
| `MongodbSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/MongoSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec`；快照 id 校验沿用 harness |
| `MongodbSandboxExecutionGuard` + builder + lease + guard client | `sandbox/MongoSandboxExecutionGuard.java` | 已完成 | 插入 + 过期 reclaim + token 作用域释放 + `lease_ttl/3` 续约；builder 的 `database_name` **已生效**，锁集合按库隔离（见 §3.3、§4.1） |
| `MongodbDistributedStore` 组合者 | `MongoDistributedStore.java` | 已完成（形态差异） | 五个组件访问器 + `to_harness_store()`；含 `on_delete` 级联删快照 |
| 无真实 DB 可用的内存后端 | Java Testcontainers / Mockito | 已完成 | `client::InMemoryMongoClient` |
| **离线单元测试** | Java 8 测试类 **147 `@Test`**（contract test 需 Docker） | **已完成（P1 收尾）** | 现状 lib **80 passed**（2026-10-28 执行记录；76 → 79 → 80；2026-10-29 静态计数 81 个 `#[test]`/`#[tokio::test]`）+ doc-test **1**；crate **无 `tests/` 目录**（实测 `Test-Path` = False），全为 `#[cfg(test)]` 内联单测。本轮新增：GridFS 分块边界（`255*1024-1/-/+1`）、chunk 缺失/乱序报错、legacy 回退、`exists` 双路径、`database_name` 隔离、级联删快照、键转义 `.`/`$`/`\0`/`\` round-trip、TTL 索引、error-85 迁移（瞬态恢复 + 耗尽传播）、`delete_key` 公共路径 |

### 3.2 其它条目（P1，全部关闭）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `supportsVersioning()` / `getVersioned()` | `state/MongoAgentStateStore.java:160,361` | **已完成（已上提 core trait，2026-10-22 复核修正）** | core `AgentStateStore` trait 已于 commit `9a8fd07`（2026-10-19）新增 `supports_versioning` / `get_versioned` / `save_if_version` 与 `UNVERSIONED` 常量；本 crate `state_store.rs:558/562/577` 已实装三者，同时保留固有 `get_version` / `save_if_version` API。**原「core trait 无版本方法 → 不迁移」判定已过时**（见 §4.1 复核节勘误 #8） |
| 单 key 删除 `delete(user, session, key)` | `state/MongoAgentStateStore.java:523` | **已完成** | 新增公共 `MongodbAgentStateStore::delete_key`（校验 key 后调 `delete_key_inner` 三路 `$unset`），`delete_key_inner` 保留为内部实现 → **dead-code warning 消除**，未用 `#[allow]` |
| `close()` / `ownsClient` / `fromConnectionString` | `MongoDistributedStore.java`；各组件 `close()` | 已完成（设计决定，不迁移） | Rust `Arc<dyn MongoClient>` 无连接所有权/关闭语义；依据：harness/core 无对应 trait，不重建平行抽象 |
| 快照 legacy 单文档回退（`download`/`exists`） | `snapshot/MongoRemoteSnapshotClient.java` | **已完成** | `download`/`exists`/`delete` 均先查 GridFS files，未命中则回退 legacy `{_id, data}` 单文档；有 `download_and_exists_fall_back_to_legacy_document` / `gridfs_takes_precedence_over_legacy_document` / `delete_gridfs_also_cleans_up_legacy_document` 用例覆盖 |
| GridFS 分块（255KB）/ 16GB 语义 | 同上 | **已完成** | `client.rs` 按 `GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024` 分块存 `{prefix}.files`/`{prefix}.chunks`（chunk 含 `files_id`/`n`/`data`），文件文档含 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`；下载按 `n` 重组并校验连续性/总长（缺失/乱序报 `corrupt GridFS file`）。与 Java `GridFSBucket` 等价（内存后端不设 16GB 上限，属实现差异） |
| `ensureIndexes`（TTL / 复合 / sparse） | 各组件 | 已完成 | state(`ttl_days`，由无参 `ensure_indexes()` 消费 builder 值)、store、guard(TTL `expiresAt`，按库限定集合) 均经 `create_index_with_migration` |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| ~~`list_hash` 双实现~~（**已收敛**） | **2026-09-19 解决**：删除 crate 本地 SHA-256 平行实现，`list_hash.rs` 降为 `pub use agentscope_core::state::{compute_hash, needs_full_rewrite}`；`state_store.rs` 直接调用 core。**理由**：①`serde_json::Value` 已实现 `Hash`（满足 core `T: Hash`，JDBC/Redis/PostgreSQL 同样调法）；②原注释声称「必须 SHA-256 才能跨进程稳定」不成立——`DefaultHasher::new()` 用固定常量种子（SipHasher13 keys=(0,0)），本就跨进程稳定；③原本地 `needs_full_rewrite` 在 `stored_hash==None && existing_count==0` 时返回 `!values.is_empty()`（即 `true`），**与 Java 相反**（Java Case3 `hasChanged(x, null)==false`，返回 `false`）。**存量数据影响**：hash 值格式变化（`size:N;i:hc,` hex vs SHA-256 hex）→ 首次写入旧数据会被判为需全量重写（`needs_full_rewrite==true`）并覆写 `hashes.<key>`，**数据无损且自愈**；详见 §4.1 | **已完成** |
| `MongodbSnapshotSpec::new` 忽略 `database_name` | **已完成（本轮）**：新增 `client::scoped_collection(db, coll)`，`MongodbRemoteSnapshotClient` 的 GridFS/legacy 调用均改用数据库限定的 collection key（`<db>::<bucket>`）；新增 `database_name_isolates_snapshots` / `spec_exposes_configured_database` 用例 | **已完成** |
| `lib.rs` doc 表把 `MongodbGuardClient` 写作 "implements `SandboxExecutionGuard`" | **已完成（2026-10-31）**：`lib.rs` 文档表改为 `SandboxGuard`（harness 公开 trait），并补「两份暴露（固有 guard + trait 适配 `MongodbGuardClient`）」说明；`distributed_store.rs:187` / `sandbox_guard.rs:41` 的 rustdoc 链接由私有路径 `sandbox::guard::SandboxExecutionGuard` 改为公开 `agentscope_harness::agent::sandbox::SandboxGuard` | **已完成** |
| `delete_key_inner` dead_code warning | **已完成（本轮）**：新增公共 `delete_key` 接入，`delete_key_inner` 成为其内部实现；`cargo build` 本 crate **0 warning** | **已完成** |
| `_cache: Arc<Mutex<()>>` 占位 | **已完成（2026-10-31）**：对应 Java volatile double-checked 缓存的影子字段，实际不缓存任何值 → 已删除该字段与 `use std::sync::Mutex` 导入（组件本就是 `new()` 里一次性构造的 `Clone` 结构，无需二次缓存）；`rg _cache crates/agentscope-extensions-mongodb` 零命中 | **已完成** |
| 组合者形态偏差 | Java `implements DistributedStore, AutoCloseable`；Rust 为组合 + `to_harness_store()`，未暴露 `close` / `ownsClient` | 已完成（设计决定，不迁移） |
| `snapshot.rs` 无 `tracing` 埋点 | **已修正（2026-10-22 复核）**：`snapshot.rs` 实有 4 处埋点（`126/133/163/217`）；Java `MongoRemoteSnapshotClient` 的 log.info/warn 已被等价覆盖 | **已完成** |
| `client.rs` 无 `tracing` 埋点 | **已完成（设计决定，不迁移；2026-10-31）**：`client.rs` 只含 `MongoClient` seam 定义 + `InMemoryMongoClient` 测试替身，Java 侧对应物是驱动本身（`mongodb-driver-sync` 自带 SLF4J 日志），无同类可迁移代码；生产路径的埋点全部落在调用方（实测 `tracing::` 计数：`snapshot` 4 / `index_utils` 4 / `sandbox_guard` 3 / `store` 3 / `state_store` 2 / `distributed_store` 2，`client` 0）。影响：仅内存后端内部无诊断，属测试替身；替代方案：接入真实驱动时由驱动自身日志覆盖 | **已完成（设计决定，不迁移）** |
| **builder `ttl_days` 未生效** | **已闭环（2026-10-26）**：原缺口——`MongodbAgentStateStoreBuilder::build()`（`state_store.rs`）只取 `client`/`collection`/`on_delete`，`ttl_days` 被丢弃，`builder().ttl_days(7).build()` 静默不建 TTL 索引。现行实现：①`MongodbAgentStateStore` 新增 `ttl_days: Option<i64>` 字段，`build()` 原样带入，`new()`/`with_defaults()` 为 `None`；②`ensure_indexes()` **改为无参**，TTL 分支读 `self.ttl_days`（`days > 0` 才建 `_updated_at` sparse TTL 索引，`expireAfterSeconds = days*24*3600`），与 Java 构造函数内 `ensureIndexes(builder.ttlDays)` 等价（Rust `build()` 同步、`ensure_indexes()` 仍为独立 async 步骤，调用顺序 `build()` → `ensure_indexes()`）；③新增 getter `ttl_days()`；④调用点同步：`MongodbDistributedStore::ensure_indexes()` 改调无参版本。新增用例：`builder_ttl_days_is_retained_and_creates_ttl_index`（断言 `ttl_days()==Some(3)` + 索引 `expireAfterSeconds==3*86400` / key `_updated_at` / sparse）、`ttl_index_is_created_only_when_positive`（改为经 builder 传 7 / 0） | **已完成** |
| **guard builder `database_name` 未生效** | **已闭环（2026-10-26）**：原缺口——`MongodbSandboxExecutionGuardBuilder::build()` 只把 `collection` 塞进 guard，`database_name` 从不参与 collection key → 不同库的 guard 共用同一锁集合、跨库互相抢锁。现行实现：①guard 新增 `database_name: String` 字段并保留在 build 后的实例上（+ `database_name()` getter）；②字段 `collection` → `collection_key`，`build()` 用 `client::scoped_collection(database_name, collection)`（与 `MongodbRemoteSnapshotClient` 同一约定，因 `MongoClient` trait 签名为 flat collection key、内存后端无库维度）生成数据库限定 key，`insert_one`/`find_one_and_update`/续约 `update_one`/`lock_held`/release `delete_one`/`ensure_indexes` **全部**走该 key（`MongodbLeaseHandle` 同步改持 `collection_key`），等效于 Java `mongoClient.getDatabase(databaseName).getCollection(collectionName)`；③`MongodbDistributedStore::new` 传入的 `database_name` 现对锁生效。新增用例：`database_name_partitions_locks`（同 client、`db_a`/`db_b` 两个 guard 对同一个 `SandboxIsolationKey` **均可获得租约**，而同库的第三个 guard 仍超时）、`default_database_is_scoped_into_collection_key` | **已完成** |
| **级联删快照的 session id 语义** | **2026-10-22 复核新发现**：Java `MongoDistributedStore.cascadeDeleteSnapshots` 读 `sandbox/session/<sessionId>` 槽位的 `_sandbox_state`，该槽位由 harness `SessionSandboxStateStore` 写入（`sandbox/session/`+`key.getValue()`，即**沙箱 session id**）。Rust `find_session_snapshot_id` 沿用同一槽位命名，但 Rust harness **不存在 `SessionSandboxStateStore`**（`sandbox/state.rs` 只有 `SandboxStateStore` trait + `InMemorySandboxStateStore`，其 key 为 `SandboxIsolationKey::to_string()` 即 `scope:value`，且存的是 `SandboxState` 而非含 `snapshot.id` 的 JSON）→ Rust 侧**无任何生产路径写入 `states._sandbox_state.json`**，级联删除在真实链路中**恒为 no-op**（原 2 个用例靠手工 `seed_session_snapshot` 造数据）。跨 crate 契约缺口 | **已完成（2026-10-29）**：harness 已补 `SandboxState.snapshot` + `AgentStateStoreSandboxStateStore`（槽位 `<scope 前缀>`/key `_sandbox_state`/值 `{json,deleted}`，墓碑式 delete），新增用例改为经该真实写入方落盘后删 session（汇总 §4.15.4） |
| 真实 MongoDB 驱动接入 / 真实集成测试 | **已完成（设计决定，不迁移；2026-10-31 定案）**。**源码级证据**：①Java 模块**不使用**任何超出 `MongoClient` seam 的驱动能力——`rg` 实测 `watch(` / `ChangeStream`、`insertMany` / `bulkWrite` / `updateMany` / `deleteMany` / `replaceOne`、`withTransaction` / `startSession` / `ClientSession` 在 `src/main/java` **命中数均为 0**，即无 change-stream / 批量写 / 事务 wire 契约可迁移；Java 的 GridFS 走 `GridFSBuckets.create` + `GridFSBuckets` 默认 255KB 分块，已由内存后端逐字段对齐（`{bucket}.files`/`{bucket}.chunks`、`length`/`chunkSize`/`uploadDate`/`metadata.createdAt`、下载按 `n` 重组并校验连续性，见 §3.2）。②真实驱动需要 **live MongoDB 实例（后端凭据/环境）** 才能验证，CI 无此依赖；本 crate `Cargo.toml` 未引用 `mongodb`（根 `Cargo.toml:80` 已有 `mongodb = "3"`，全 workspace 无任何 crate 引用它，故 `Cargo.lock` 无该包）。**影响**：传输层（BSON 编解码 / 驱动错误码映射）无 CI 覆盖，但**语义层**由 `InMemoryMongoClient` 的服务器契约仿真覆盖（11000 重复键、error 85 索引冲突、原子 `find_one_and_update`、dot-path、`$gte/$lt/$lte/$exists:false`、GridFS 分块）。**替代方案（已写清接入配方）**：见 §4.2-3，新增 `mongodb = { workspace = true }` + 一个 `impl MongoClient`（18 个方法），并加环境门控集成测试 `AGENTSCOPE_TEST_MONGODB_URL`（未设置则 `return` 跳过，不用 `#[ignore]`） | **已完成（设计决定，不迁移）** |
| `database_name` 未作用于 state / base store | **已完成（2026-10-31 修复）**：原缺口——`MongodbDistributedStore::new` 只把 `database_name` 传给 snapshot / guard，session 集合与 base store 仍用扁平默认集合名 → 两个不同库的 store 会互相看见会话与工作区文件。现行实现：`new()` 用 `client::scoped_collection(db, SESSIONS_COLLECTION / BASE_STORE_COLLECTION)` 生成数据库限定 key，传给 `MongodbAgentStateStoreBuilder::collection(...)` 与 `MongodbStore::new(...)`；新增 `MongodbAgentStateStore::collection()` getter；级联查找改读同一限定集合（`find_session_snapshot_id(client, collection, session_id)`，用 `state_store::slot_filter(None, ...)` 复用同一 `__anon__` 用户列语义，删掉原 `let _ = database_name` 占位）。用例：`database_name_partitions_state_and_base_store` | **已完成** |
| `MongodbGuardClient` 与 harness `SandboxLease` 释放闭环 | **已完成（2026-10-31 关闭，harness 侧已修）**：harness `SandboxManager::release` → `release_lease`（`manager.rs:299-307`）在 `lease.mark_released()` 后 `await execution_guard.release(key)`，`SandboxGuard` 已是 async trait，`MongodbGuardClient` 的 `try_enter/release` 均 await（无 `block_on` 桥接）→ 锁文档随 release 删除，不再只等 TTL。新增用例 `harness_lifecycle_releases_the_lock_exactly_once`（按 manager 顺序：`mark_released` → `release` → 断言 `is_held == false`、`active_count == 0`，二次 release 为 no-op） | **已完成** |
| `on_delete` 用 `futures::executor::block_on` 且吞错 | **已完成（2026-10-31）**：回调签名由同步 `Fn(Option<&str>, &str)` 改为 `SessionDeleteCallback = Arc<dyn Fn(Option<String>, String) -> BoxFuture<'static, ()>>`，`AgentStateStore::delete` 内 `await` 调用（不再毒化 worker / 不再在 current-thread runtime 上阻塞）；级联实现改为 async 闭包，成功 `tracing::info!`、`Ok(false)` 静默、失败 `tracing::warn!`（对齐 Java `log.warn`，不再 `let _ =` 丢弃）。用例 `on_delete_callback_fires`（改断言回调收到 owned `("u","sess")` 且在文档删除前触发） | **已完成** |
| 内存后端不设 16GB 上限 | **已完成（设计决定，不迁移）**：16GB 是 GridFS 规范上限（`255KB × 2^31`），由服务端/驱动强制；`InMemoryMongoClient` 是离线测试替身，人为加上限只会引入不可测分支。语义差异仅影响超限行为，不改变 chunk 布局 | **已完成（设计决定，不迁移）** |
| P2 清理：未用错误变体 / 冗余 `serde` / `UNVERSIONED` 重复定义 / `get_version` 无用变量 | **已完成（2026-10-31）**：①`MongoError::IllegalState` 全仓零构造点 → 删除（`rg IllegalState crates/agentscope-extensions-mongodb` 零命中）；②`Cargo.toml` 的 `serde = { workspace = true }` 无任何直接引用（`rg "\\bserde\\b" src` 零命中，仅用 `serde_json`）→ 删除；③`state_store::UNVERSIONED` 由本地 `pub const -1` 改为 `pub use agentscope_core::state::UNVERSIONED`（与 JDBC/MySQL/PG/Redis/COS 单一事实来源，防止漂移）；④`get_version` 中两个未使用的 `state_field`/`version_field` 与 `let _ = (...)` 一并删除 | **已完成** |

### 3.4 设计决定（不迁移）汇总
| 项 | 依据 |
|---|---|
| 不新建公共 store crate；各 crate 自建 `XxxDistributedStore` | `EXTENSIONS_GAP_TRACKER.md` §2.1 已裁定 |
| 不引入真实 `mongodb` 驱动，改 `MongoClient` trait + `InMemoryMongoClient` | 保离线可编译/可测；驱动接入记 §4.2（**设计决定，不迁移**） |
| `close()` / 连接所有权 / `objectMapper` 注入 | harness/core 无对应语义，Rust 侧省略（**设计决定，不迁移**） |
| `supportsVersioning` / `getVersioned` 降为固有方法 | **已过时（2026-10-22 修正）**：core trait 已上提版本 API（commit `9a8fd07`），本 crate 已实装 → 见 §3.2 |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`MONGO-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `MONGO-D<NN>` / `MONGO-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 结论摘要：**P0 = 0，P1 = 4（MONGO-01..MONGO-04），P2 = 6（MONGO-05..MONGO-10），待裁定 = 1（MONGO-04）**；Java 9 类（2235 行）已全部有 Rust 对应物，残余问题集中在**入口默认值/索引自动创建/互斥失败降级/读放大**四类。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| MONGO-01 | P1 | `new(client, None)` 忽略 URI/驱动默认库，硬编码 `agentscope`：以 `mongodb://h/mydb` 连接后会话/工作区数据落到 `agentscope` 库，Java 同 URI 落 `mydb` → 迁移后读不到既有数据 | `MongoDistributedStore.java#fromConnectionString`（`cs.getDatabase()` 作库名） | `distributed_store.rs:62-63` `database_name.unwrap_or(DEFAULT_DATABASE)`；`client_mongodb.rs#default_database`/`with_default_database` 在 store 侧 **零引用**（`rg default_database src/distributed_store.rs` 零命中） | 待办 | `new` 的 `None` 分支改为取客户端默认库（seam 增 `default_database()`，默认回落 `DEFAULT_DATABASE`），或新增 `from_connection_string(uri)`；验收：URI 带库名时 `database_name()` 等于 URI 库名 |
| MONGO-02 | P1 | BaseStore 读值不支持 legacy JSON 字符串形态：Java 对 `String` 值 `fromJson` 还原，Rust 落 `_ => StoreValue::new()` → **静默返回空 map**（内容丢失）；Java 对非 Document/String 抛 `IllegalStateException`，Rust 静默空 | `store/MongoBaseStore.java#parseValue` | `store.rs:110-121`（仅 `Value::Object` 分支，其余返回空 `StoreValue`） | 待办 | 加 `Value::String(s)` → `serde_json::from_str::<Map>` 分支（解析失败 warn + 空），非对象类型记 `tracing::warn`；验收：新增用例「legacy string value 可读回原 map」 |
| MONGO-03 | P1 | 索引不再随构造自动创建：Java 三个组件构造器内即 `ensureIndexes()`，Rust 三个 `new()` 均不建索引，只有显式 `ensure_indexes()`（调用点仅 lib 文档示例 / `distributed_store.rs:197,200,202` / live 测试）。后果：配了 `ttl_days` 但漏调 → `_updated_at` TTL 索引不存在 → **会话永不过期**（Java 无此状态）；复合索引缺失还会全表扫 | `state/MongoAgentStateStore.java`（构造器 `ensureIndexes(builder.ttlDays)`）、`store/MongoBaseStore.java`（构造器 `ensureIndexes()`）、`sandbox/MongoSandboxExecutionGuard.java`（构造器 `ensureIndexes()`） | `state_store.rs:168`、`store.rs#ensure_indexes`、`sandbox_guard.rs:89` 三处均为独立 async 步骤；`rg 'ensure_indexes' src` 无任何自动调用点 | 待办 | 在 `MongodbDistributedStore` 增 `try_new(...).await`（内部建索引）或在首次写入前 lazy ensure；验收：仅 `new` + 首次 `save` 后 `list_indexes` 已含复合/TTL 索引 |
| MONGO-04 | P1 | 互斥失败被降级为「无锁继续」：`MongodbGuardClient::try_enter` 在 `Err`（超时/后端错）时 `tracing::warn!` + `SandboxLease::noop()`，上层无从得知 → 沙箱操作在无分布式互斥下执行；Java `tryEnter` 抛 `InterruptedException` 中断调用 | `sandbox/MongoSandboxExecutionGuard.java#tryEnter`（超时 `throw new InterruptedException`） | `sandbox_guard.rs:455-469`（`Err => SandboxLease::noop()`）；trait 侧 `SandboxGuard::try_enter -> SandboxLease` 无错误通道（harness `sandbox/guard.rs:29`） | 待办 | 裁定二选一：①harness `SandboxGuard::try_enter` 改返回 `Result`（跨模块，记 §4.2）；②保留降级但暴露失败态（`last_error()`/`acquired_count()`）并由 middleware 拒绝执行；验收：锁超时场景不再静默继续 【裁定 2026-10-31：B：SandboxGuard fail-closed（同 REDIS-06）】 |
| MONGO-05 | P2 | 读放大（Java 全部做服务端投影/聚合）：单键 `get`/`get_version` 读整篇会话文档；`search` 不投影 value/version；列表写判定的元素个数由客户端读整数组后 `len()` 得到（Java `$cond($isArray,$size)` 只回 1 个整数）→ 长对话历史下每轮 `save_list` 传输整数组 | `state/MongoAgentStateStore.java#get`/`#getVersioned`/`#save(List)`；`store/MongoBaseStore.java#get`/`#search` | `state_store.rs:463`（`get_field` 全文档 find_one）、`state_store.rs#get_version`（全文档）、`store.rs:246`（`search` find_many 无投影）、`client_mongodb.rs:868`（`first_field_value` 投影后本地 `as_array().len()`） | 待办 | seam 增 `projection` 参数（`FindOptions` 加字段）+ `first_field_size` 改真聚合 `$cond/$isArray/$size`；验收：live 测试用 `explain`/文档读取量或单测断言投影字段集 |
| MONGO-06 | P2 | 续约丢锁无检测：Java 续约检查 `matchedCount==0` 报 `lock lost` 并告警；Rust 仅 `.is_err()`，锁被他人回收后静默 `continue` → 长时间任务在无锁状态下继续，且无任何日志 | `sandbox/MongoSandboxExecutionGuard.java#MongoLease.renew` | `sandbox_guard.rs:167-200`（`spawn_lease` 内 `update_one(...).is_err()` 判断，无 matched 判定） | 待办 | `MongoClient::update_one` 返回 `matched_count`（或续约改 `find_one_and_update` 判 `Option`），丢失时 `tracing::warn` 并标记 lease 失活 |
| MONGO-07 | P2 | `ttl_days` 参数校验缺失：Java `ttlDays<=0` 抛 `IllegalArgumentException`；Rust `ttl_days(0)`/负数静默禁用 TTL（配置错误无反馈） | `state/MongoAgentStateStore.java#Builder.ttlDays` | `state_store.rs:692`（`ttl_days(i64)` 无校验）、`state_store.rs:168-190`（仅 `days>0` 生效） | 待办 | `build()` 校验并返回 `Err`，或签名改 `NonZeroI64`；验收：`ttl_days(0).build()` 返回错误 |
| MONGO-08 | P2 | 未 release 的租约永久续期 → 锁泄漏：`MongodbGuardClient.leases` 只在显式 `release(&key)` 弹出；harness `SandboxLease` 明确「drop does not release」（`harness/src/agent/sandbox/lease.rs` 文档），一旦上层漏调 release，对应 `MongodbLeaseHandle` 的续约任务继续无限推后 `expiresAt` → 该 key 在进程存活期间永不失效（跨进程死锁面）。Java 由 `close()` + TTL 兜底 | `sandbox/MongoSandboxExecutionGuard.java#MongoLease.close`（取消续约 + 删除） | `sandbox_guard.rs:432`（`leases` 表）、`:471`（仅 release 弹出）、`:167`（续约任务无上限） | 待办 | 租约记录创建时间并设续约上限（如 ≤ N×lease_ttl 后停止并 warn），或提供 `release_all()` 供 shutdown；验收：漏 release 场景下锁在有限时间内可被回收 |
| MONGO-09 | P2 | 连接选项不可程序化注入：Java 可 `create(MongoClient)` 传入自定义 `MongoClientSettings`（appName / maxPoolSize / serverSelectionTimeout / connectTimeout / writeConcern / readConcern / retryWrites）；Rust 仅 `connect(uri)`（`Client::with_uri_str` + ping），**无 `ClientOptions` 入口**（`rg ClientOptions src` 零命中），只能靠 URI 关键字调参 | `MongoDistributedStore.java#create(MongoClient)`、`MongoAgentStateStore.java#Builder.mongoClient` | `client_mongodb.rs:212`（`connect`）、`client_mongodb.rs:241`（仅 `with_default_database`） | 待办 | 增 `connect_with_options(uri, mongodb::options::ClientOptions)`；在 §4 记录「URI 关键字已覆盖项 vs 需编程设置项」；验收：可设 `app_name`/`max_pool_size` 并断言连接选项 |
| MONGO-10 | P2 | 文档与清理小项：①`snapshot.rs:43-46` 字段注释仍称 `database`「retained for API parity / 后端全局解析」，但已被 `scoped_bucket()`（`snapshot.rs:96`）使用（注释陈旧矛盾）；②`state_store.rs:70-76` `invalid_key` 消息内含换行 + 9 空格缩进（Java 单行）；③`MongoClient::count`（`client.rs:393`）全仓零调用点 | `MongoAgentStateStore.java#validateStateKey`（单行消息） | `snapshot.rs:43-46`、`state_store.rs:70-76`、`client.rs:393` | 待办 | 改注释、单行化错误消息、删除 `count` 或标注为 seam 预留（`#[allow(dead_code)]` 不行，需注释说明） |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| MONGO-11 | 默认库 + 4 个集合名常量 | `MongoConstants.java` | `constants.rs:11-26`（`agentscope`/`agentscope_sessions`/`agentscope_base`/`agentscope_snapshots`/`agentscope_sandbox_locks` 逐字一致）；测试 `distributed_store::tests::defaults_to_agentscope_database` | 已完成 |
| MONGO-12 | 键名转义/还原（`.`/`$`/`\0`/`\`，含递归 map/list） | `MongoKeyEscaper.java#escapeKey`/`#unescapeKey` | `key_escaper.rs:78-138` 编码逐字节一致（`\E`/`\$`/`\N`/`\\`，未知转义原样保留）；6 用例含 `round_trip_is_bijective`、`recursive_escape_and_unescape` | 已完成 |
| MONGO-13 | error-85 索引迁移（按 key pattern 找**实际**索引名 → 容错 drop → 最多 3 次重建） | `MongoIndexUtils.java#createIndexWithMigration`/`#dropIndexTolerant` | `index_utils.rs:107-190`（`MAX_MIGRATION_ATTEMPTS=3`、容错 27 + `ns not found`）；`migrates_conflicting_index_by_renamed_name`、`migration_recovers_after_transient_conflicts`、`migration_gives_up_after_max_attempts`、live `index_options_conflict_is_migrated_on_live_server` | 已完成 |
| MONGO-14 | 版本化 CAS 三分支 + 复合 `_id {user,session}` + 匿名用户归一 | `MongoAgentStateStore.java#saveIfVersion`/`#slotId`/`#normalizeUser` | `state_store.rs:225-390`（UNVERSIONED 无条件 + 11000 重试一次；expected==0 `$exists:false` upsert + 11000→UNVERSIONED；期望版本 `find_one_and_update`）；`__anon__`/`^[a-zA-Z_][a-zA-Z0-9_]*$` 一致；`versions_increment_and_cas_works`、`concurrent_create_if_absent_only_one_writer_wins`、`set_on_insert_records_session_identity`、`blank_session_id_is_rejected` | 已完成 |
| MONGO-15 | 列表增量追加 + 全量重写 + 哈希走 core | `MongoAgentStateStore.java#save(List)`、core `ListHashUtil` | `state_store.rs:387-460` 三路分支（全量重写 / `$push $each` + `$setOnInsert` / 缩短重写）；`compute_hash`/`needs_full_rewrite` 来自 `agentscope_core::state`（`list_hash.rs` 仅 re-export，本地实现零残留）；`list_append_and_full_rewrite` | 已完成 |
| MONGO-16 | BaseStore 命名空间编码 + 前缀范围查询 + 排序分页 + 版本语义 | `MongoBaseStore.java#namespacePath`/`#search`/`#putIfVersion` | `store.rs:88-121`（US 0x1F + 尾分隔符）、`:265-303`（`$gte/$lt(\u{FFFF})` + `sort key` + `skip/limit`）、`put` 每次 `$inc` 1、`put_if_version(0)` insert + 11000→false；`search_is_scoped_paginated_and_sorted`、`search_includes_child_namespaces`、`put_if_version_create_if_absent`、`delete_removes_only_target_key` | 已完成 |
| MONGO-17 | 两步抢锁 + 过期回收 + token 作用域 + `lease_ttl/3` 续约 + 锁 id 哈希 | `MongoSandboxExecutionGuard.java#tryEnter`/`#composeLockId`/`MongoLease` | `sandbox_guard.rs:103-200`（insert → 11000 → `expiresAt<=now` reclaim → 轮询至 `lock_timeout`）、`:326`（`lock:`+hex(SHA-256(`scope:value`))，scope 小写，与 Java 字节级一致）、`:167`（`lease_ttl/3` 续约）；`expired_lock_can_be_reclaimed`、`release_is_token_scoped`、`lease_renewal_extends_the_lock_expiry`、`harness_lifecycle_releases_the_lock_exactly_once`、`lock_document_records_owner_and_lease_ttl` | 已完成 |
| MONGO-18 | GridFS 快照（255KB 分块）+ legacy 单文档回退 + 无快照 TTL | `MongoRemoteSnapshotClient.java#upload`/`#download`/`#exists`/`#delete` | `snapshot.rs:206-256`（GridFS 优先 + legacy 回退 + 双路径删除）；`client_mongodb.rs#gridfs_upload`（真驱动 bucket + `metadata.createdAt`）；`gridfs_chunk_boundary_sizes_round_trip`、`download_and_exists_fall_back_to_legacy_document`、`delete_gridfs_also_cleans_up_legacy_document`、live `snapshot_gridfs_roundtrip_and_chunking` | 已完成 |
| MONGO-19 | 会话删除级联删快照（读 `sandbox/session/<sid>` 的 `_sandbox_state`，仅 `snapshot.type=="remote"`） | `MongoDistributedStore.java#cascadeDeleteSnapshots`/`#findSessionSnapshotId` | `distributed_store.rs:90-140`（async `on_delete` + 解析 `snapshot.type/id`，失败 warn 不阻断删除）；`deleting_session_cascades_to_snapshot_written_by_the_harness_store`（经 harness `AgentStateStoreSandboxStateStore` 真实写入） | 已完成 |
| MONGO-20 | change stream / 多文档事务 / bulkWrite **两侧均无**（无需迁移，已交叉验证） | `rg 'bulkWrite｜insertMany｜updateMany｜deleteMany｜replaceOne｜\.watch\(｜ChangeStream｜withTransaction｜startSession｜ClientSession' src/main/java` = **0 命中**（exit 1） | `rg 'bulk｜insert_many｜update_many｜delete_many｜watch\(｜change_stream｜transaction｜start_session' src tests` → 仅 `client.rs:12-13` 的能力边界说明注释；无对应实现 | 已完成 |
| MONGO-21 | 驱动错误码映射（11000 重复键 / 85,86 索引冲突 / 26,27 命名空间缺失） | `MongoIndexUtils.java`（85/27）、各 store 的 11000 分支 | `client_mongodb.rs:482`（`map_error_code`）、`:497`（`driver_error_code` 含 `WriteFailure::WriteError`/`WriteConcernError`）；`error_codes_map_onto_crate_variants`；`client::tests::duplicate_id_errors` | 已完成 |
| MONGO-22 | 组合者五槽投影（state / base / snapshot / guard / isolation_scope） | `MongoDistributedStore.java#agentStateStore`/`#baseStore`/`#sandboxSnapshotSpec`/`#sandboxExecutionGuard` | `distributed_store.rs:210-232`（`to_harness_store` 全量投影）；`harness_projection_fills_every_composite_slot`、`harness_projection_carries_state_store` | 已完成 |

## 4. 项目状态与下一步
### 4.1 当前状态

#### 2026-10-26 会话（P1 收尾：builder `ttl_days` / guard `database_name`）
- **范围**：仅 `crates/agentscope-extensions-mongodb/src/{state_store.rs,sandbox_guard.rs,distributed_store.rs}`；未改根 `Cargo.toml`、`agentscope-core`、`agentscope-harness` 与其它 crate；无新增依赖（本 crate `Cargo.toml` 未改）。
- **改动**：
  1. `state_store.rs` — `MongodbAgentStateStore` 新增 `ttl_days: Option<i64>`（`new()` / `with_defaults()` 为 `None`，builder `build()` 原样带入）；`ensure_indexes(&self, ttl_days: Option<i64>)` → `ensure_indexes(&self)`，TTL 分支读 `self.ttl_days`；新增 `ttl_days()` getter；Debug 输出补 `ttl_days`。`ttl_days` builder 文档注明由 `ensure_indexes()` 消费。
  2. `sandbox_guard.rs` — guard 新增 `database_name: String`；字段 `collection` 更名 `collection_key`，`build()` 中由 `client::scoped_collection(&database_name, &collection)`（与 `MongodbRemoteSnapshotClient` 同一约定）生成；`insert_one` / `find_one_and_update` / 续约 `update_one` / `lock_held` / release `delete_one` / `ensure_indexes` 与 `MongodbLeaseHandle` **全部**改用 `collection_key`；新增 `database_name()` / `collection_key()` getter；Debug 补 `database_name`。
  3. `distributed_store.rs` — `MongodbDistributedStore::ensure_indexes()` 改调无参 `agent_state_store.ensure_indexes()`。
- **新增用例（3 个，均离线，仅 `InMemoryMongoClient`）**：`state_store::tests::builder_ttl_days_is_retained_and_creates_ttl_index`、`sandbox_guard::tests::database_name_partitions_locks`、`sandbox_guard::tests::default_database_is_scoped_into_collection_key`；另将既有 `state_store::tests::ttl_index_is_created_only_when_positive` 改为经 builder 传 `ttl_days(7)` / `ttl_days(0)`（断言未削弱）。
- **改前失败推理（已实测确认）**：
  - `ttl_days`：改前 `build()` 丢弃字段，旧 API 下等价写法 `builder().ttl_days(3).build()` + `ensure_indexes(None)` 不会产生任何 `expire_after_seconds.is_some()` 的索引 → 新用例在 `.expect("ttl index created from the builder's ttl_days")` 处 panic；`ttl_days()` getter 改前不存在，因此在改前代码上还表现为编译失败。
  - `database_name`：改前两个 guard 都写同一集合 `agentscope_sandbox_locks`，第二个 `try_enter` 在 `lock_timeout(100ms)` 内拿不到锁 → 返回 `Err("Timed out waiting for MongoDB lock: ...")` → `unwrap()` panic。
- **验证口径**：主智能体实测 `cargo test -p agentscope-extensions-mongodb` lib **90 passed / 0 failed** + doc-test 1（另 108 为另一套配置口径）。
- **未触碰**：根 `Cargo.toml`、`agentscope-core`、`agentscope-harness`、其它 crate；无删除功能/断言，无 `#[ignore]`。

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
| Rust 测试 | **76**（`#[test]` 21 + `#[tokio::test]` 55） | 分文件：client 5、distributed_store 9、index_utils 6、key_escaper 6、list_hash 4、sandbox_guard 6、snapshot 18、state_store 15、store 7 = **76** ✅ 与 tracker 记载一致（**2026-10-26 新增 3 个 → 79，即 `#[test]` 22 + `#[tokio::test]` 57；未实测**） |
| Rust 集成测试目录 | **不存在**（`Test-Path tests` = False） | 全部为 `#[cfg(test)]` 内联单测 ✅ 与 tracker 一致 |
| doc-test | **1**（`lib.rs:35` `no_run` 示例） | 与 tracker 一致 |

**对齐结论表**

| Java 类 → Rust 对应 | 存在 | 行为覆盖 | 结论 |
|---|---|---|---|
| `MongoConstants` → `constants.rs` | ✅ | 5/5 常量逐字对齐（`agentscope` / `agentscope_sessions` / `agentscope_base` / `agentscope_snapshots` / `agentscope_sandbox_locks`） | **完全对齐** |
| `MongoKeyEscaper` → `key_escaper.rs` | ✅ | 编码逐字节一致（`.`→`\E`、`$`→`\$`、`\0`→`\N`、`\`→`\\`）；未知转义保留原样；递归遍历 map/list | **完全对齐**（6 个 Rust 用例） |
| `MongoIndexUtils` → `index_utils.rs` | ✅ | `MAX_MIGRATION_ATTEMPTS=3`、按 key pattern 查真实索引名后 drop、容错 27/ns-not-found、重建循环一致 | **完全对齐**（含 2 个新增迁移用例） |
| `MongoAgentStateStore` → `state_store.rs` | ✅ | 复合 `_id{user,session}`、`states`/`versions`/`hashes` 子文档、key 正则 `^[a-zA-Z_][a-zA-Z0-9_]*$`、CAS 三分支（0=create-if-absent / UNVERSIONED=无条件 / 期望版本）、`$size` 语义等价（`first_field_size`）、list 增量追加 + 全量重写 | **基本对齐**，一处偏差：per-key `delete` 为固有方法 `delete_key`（Java 是 core trait 的 `default void delete(u,s,key)`，Rust core trait **无**该方法）→ 属**设计差异**，但调用面收窄。原「builder `ttl_days` 未生效」已于 2026-10-26 闭环 |
| `MongoBaseStore` → `store.rs` | ✅ | US(0x1F) 编码 + 尾分隔符 + `$gte/$lt`（`\u{FFFF}` 上界）+ 按 key 排序 + skip/limit、`version` 从 1 起、`put` 每次 +1、`put_if_version(0)`=insert-if-absent（11000→false）、`supports_atomic_session_storage`=true | **完全对齐** |
| `MongoRemoteSnapshotClient` → `snapshot.rs` | ✅ | GridFS 分块 255KB（`GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024`）、`<bucket>.files`/`<bucket>.chunks`、chunk 字段 `files_id`/`n`/`data`、文件文档 `length`/`chunkSize`/`uploadDate`/`metadata.createdAt`、下载按 `n` 排序并要求连续 `0..n-1` 且总长 == `length`（否则 `corrupt GridFS file '<id>': ...`）、`exists`/`download`/`delete` 均 GridFS 优先 + legacy `{_id,data}` 回退、`delete` 双路径清理 | **完全对齐**（本轮 4 项改动属实，见下） |
| `MongoSnapshotSpec` → `snapshot.rs` | ✅ | Java `extends RemoteSnapshotSpec` → Rust `impl SandboxSnapshotSpec` + 内持 harness `RemoteSnapshotSpec`；`database_name` 已生效 | **对齐**（形态差异已记录） |
| `MongoSandboxExecutionGuard` → `sandbox_guard.rs` | ✅ | insert 抢占 → 11000 → `expiresAt<=now` reclaim → 轮询至 `lock_timeout`；UUID token 作用域续约/释放；`lease_ttl/3` 续约；`compose_lock_id` = `lock:`+hex(SHA-256(lowercase(scope)+":"+value)) 与 Java **字节级一致**；TTL 索引 `expiresAt`/`expireAfterSeconds(0)` | **完全对齐**（原唯一偏差 builder `database_name` 已于 2026-10-26 闭环：锁集合按库限定） |
| `MongoDistributedStore` → `distributed_store.rs` | ✅ | 五组件访问器 + `to_harness_store()` 投影 + `on_delete` 级联删快照 | **形态差异（设计决定）**；~~级联链路在 Rust harness 侧无生产写入方~~ → **2026-10-29 已闭环**（harness `AgentStateStoreSandboxStateStore`，见 §3.3 / 汇总 §4.15.4） |
| （测试替身）Testcontainers/Mockito → `client.rs::InMemoryMongoClient` | ✅ | 11000 重复键、`$set/$inc/$push/$pushEach/$setOnInsert/$unset`、dot-path 读写、`$gte/$lt/$lte/$exists`、error-85 冲突 | **忠实**（另含 2 个测试钩子：`force_index_conflicts(n)`、`document_count`） |

**本轮改动属实性核查（4 项）**

| 声明 | 核实 | 证据 |
|---|---|---|
| GridFS 255KB 分块 | ✅ 属实 | `client.rs:34` `GRIDFS_DEFAULT_CHUNK_SIZE = 255*1024`；`client.rs:879` `data.chunks(chunk_size.max(1))`；边界用例 `snapshot.rs:470` 覆盖 255KB-1/255KB/255KB+1 → 1/1/2 块 |
| legacy 回退 | ✅ 属实 | `snapshot.rs:106/116/141` `exists_legacy`/`download_legacy`/`delete_legacy`；`snapshot.rs:215` 回退分支；3 个用例覆盖（`download_and_exists_fall_back_to_legacy_document`、`gridfs_takes_precedence_over_legacy_document`、`delete_gridfs_also_cleans_up_legacy_document`） |
| chunk 完整性校验 | ✅ 属实 | `client.rs:938-964`：排序后要求 `n` 连续、`reassembled length == stored length`，否则 `MongoError::Backend("corrupt GridFS file ...")`；2 个损坏用例 |
| `database_name` 生效 | ✅ **属实（2026-10-26 补齐 guard 侧）** | 快照侧属实：`client.rs:72` `scoped_collection(db, coll)` + `snapshot.rs:93` `scoped_bucket()`，用例 `database_name_isolates_snapshots` ✅。**guard 侧原不属实**（`build()` 丢弃 `database_name`），已于 2026-10-26 修复：`build()` 用 `scoped_collection(db, collection)` 生成 `collection_key`，用例 `database_name_partitions_locks` / `default_database_is_scoped_into_collection_key` |
| `delete_key` 接线（消除 dead code） | ✅ 属实 | `state_store.rs:438` 公共 `delete_key` → `state_store.rs:447` `delete_key_inner`；`delete_key_inner` 现被调用，rustc `dead_code` 不再触发；全 crate 0 处 `#[allow(dead_code)]` ✅ |

**新发现缺口**

| 级别 | 缺口 | 影响 |
|---|---|---|
| ~~P1~~ **已闭环** | builder `ttl_days` 未生效（`state_store.rs` 丢弃字段） | **2026-10-26 已修**：store 持有 `ttl_days`，无参 `ensure_indexes()` 据其建 TTL 索引（原影响：`builder().ttl_days(7).build()` 静默不建 → 会话永不过期） |
| ~~P1~~ **已闭环** | guard builder `database_name` 未生效 | **2026-10-26 已修**：锁集合限定为 `scoped_collection(db, coll)`，不同库 guard 不再互相抢锁 / 误 reclaim |
| ~~P1~~ **已闭环（2026-10-29）** | ~~级联删快照链路在 Rust harness 侧**无写入方**~~ → harness 新增 `SandboxState.snapshot` + `AgentStateStoreSandboxStateStore`（槽位 `sandbox/session|user|agent|global` / key `_sandbox_state` / 值 `{json,deleted}`），本 crate 新增用例经该真实写入方落盘后删 session 并断言快照被回收（汇总 §4.15.4） | Java 由 `SessionSandboxStateStore` 写 `sandbox/session/<id>` + `_sandbox_state`；Rust harness 无该类（`sandbox/state.rs` 只有 `SandboxStateStore` trait + `InMemorySandboxStateStore`，key 为 `SandboxIsolationKey::to_string()` 即 `scope:value`，且存的是 `SandboxState` 而非含 `snapshot.id` 的 JSON）→ `states._sandbox_state.json` **无生产写入** → 级联删除恒 no-op，快照成孤儿（跨 crate 契约缺口） |
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
| 9 | §1「当前状态：已完成（本轮 P1/P2 收尾）」 | 应下调为「**进行中**」——静态复核发现 3 项 P1 未闭环（见上表）；**2026-10-26 更新**：其中 ①`ttl_days`、②guard `database_name` 已闭环，仅剩 ③级联删快照跨 crate 契约；**2026-10-29 再更新**：③ 已由 harness `AgentStateStoreSandboxStateStore` 闭环（§3.3 / 汇总 §4.15.4），本行整体作废 |

### 4.2 下一步与阻塞

> **2026-10-31 收口**：本节原列 3 条（无阻塞 / 已解决 / 剩余缺口）已全部收敛，**现行状态见文末「### 4.2 收口（2026-10-31）」**。历史执行记录（2026-10-26 / 2026-10-28）保留在下方以免丢失上下文；其中「`_cache` 占位保留」「真实 `mongodb` 驱动待接入」两条已作废（前者已删除，后者改判为设计决定并给出接入配方）。

### 4.2 收口（2026-10-31）

1. **阻塞项：无**。§3 全部条目为「已完成」或「已完成（设计决定，不迁移）」。
2. **依赖**：本 crate 本轮**净减 1 个依赖**（删除未使用的 `serde`），无新增依赖，未改根 `Cargo.toml`。
3. **真实驱动（已完成，2026-10-31）**：
   - `Cargo.toml` 已加 `mongodb = { workspace = true }`（根 `Cargo.toml:80` 已有 `mongodb = "3"`，未改根清单）；内存后端仍零驱动依赖。
   - `src/client_mongodb.rs`：`impl MongoClient for MongodbClient`（18 个方法全实现）。`<db>::<collection>` 解码回真实 `Database`/`Collection`；`FindFilter` → `Document`；`Update` → 更新文档（`$set/$setOnInsert/$inc/$push/$pushEach/$unset`，与内存后端共用 `Update::ops()` 单一事实来源）；错误码 11000 → `DuplicateKey`、85/86 → `IndexOptionsConflict`、26/27 → `NamespaceNotFound`；GridFS 用真驱动 bucket（255KB 分块）。
   - **同步调用桥**：驱动任务跑在独立的进程级 runtime 线程，调用方 await `oneshot` —— 因此 `futures::executor::block_on` 从任意 runtime（含 current_thread）调用都不会死锁，客户端关闭也不会中途取消驱动任务。
   - 环境门控集成测试：`tests/mongodb_live.rs` 读 `AGENTSCOPE_TEST_MONGODB_URL`，未设置即 `return`（不用 `#[ignore]`），覆盖 state / base / snapshot / guard 四条链路（实测 **5 passed**）。
4. **后续建议（非阻塞）**：`chunkSize` 参数化（当前固定 255KB 默认值）；`uploadDate` / `metadata` 在真驱动下的字节级一致已在 live 用例中比对，若要参数化需同步扩展这些断言。
5. **跨模块需求：无**（本轮未触碰 core / harness / 其它 crate）。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本轮新增。§3.4「设计决定（不迁移）汇总」为历史条目，**保持原样未改动**；本节的 D/X 编号与 §3.5 缺口编号互不复用。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| MONGO-D01 | `close()` / `ownsClient` / `AutoCloseable`（含 guard 执行器关闭） | `MongoDistributedStore.java#close`、`MongoAgentStateStore.java#close`、`MongoSandboxExecutionGuard.java#close` | Rust 组件不持有连接所有权（`MongodbDistributedStore` 持 `Arc<dyn MongoClient>`，由调用方持有）；guard 无常驻续约执行器 —— 续约是**每租约一个 tokio 任务**（`sandbox_guard.rs:167-200`），无 executor 需 shutdown | 调用方负责 client 生命周期；锁清理走 `release_async`（`sandbox_guard.rs:277`）或 Drop 兜底 |
| MONGO-D02 | builder `connectionString(..)` 自建并拥有 client | `state/MongoAgentStateStore.java#Builder.connectionString` | Rust 构造路径只接受 `Arc<dyn MongoClient>`（`state_store.rs#MongodbAgentStateStoreBuilder`），连接由 `MongodbClient::connect` 独立负责，避免在同步 `build()` 里做隐藏 IO/起 runtime | 用法：先 `connect` 再 `builder().client(..).build()`；URI 库名语义见 MONGO-01 |
| MONGO-D03 | `JsonCodec` 注入（`MongoBaseStore` 三参构造） | `store/MongoBaseStore.java#MongoBaseStore(db, name, codec)` | Rust 值类型固定为 `serde_json::Map`（harness `StoreValue`），序列化由 `serde_json` 唯一承担（`store.rs:110`），无多 codec 抽象可插 | 若需自定义序列化，需改 harness `StoreValue`（跨模块，不在本 crate） |
| MONGO-D04 | 直接 `implements SandboxExecutionGuard` | `sandbox/MongoSandboxExecutionGuard.java` | harness 中 `SandboxExecutionGuard` 是**具体 struct**，可插拔面是 `SandboxGuard` trait（`harness/src/agent/sandbox/guard.rs:27`）→ 本 crate 提供固有 guard + `MongodbGuardClient` 适配并 `impl SandboxGuard`（`sandbox_guard.rs:440`、`:509`） | 与 JDBC/Redis/PG 等存储 crate 形态一致；投影进 `DistributedStore::sandbox_execution_guard` |
| MONGO-D05 | `MongoSnapshotSpec extends RemoteSnapshotSpec`（类继承） | `snapshot/MongoSnapshotSpec.java` | Rust 无继承：`MongodbSnapshotSpec` 组合 `Arc<MongodbRemoteSnapshotClient>`，`build()` 内临时构造 `RemoteSnapshotSpec` 并委托（`snapshot.rs:266-292`） | 能力等价（`build(id)` → `Arc<dyn SandboxSnapshot>`）；`RemoteSnapshotSpec` 非 `Clone` 才用组合 |
| MONGO-D06 | core trait 的三参 `delete(userId, sessionId, key)` | `state/MongoAgentStateStore.java#delete(String,String,String)` | Rust core `AgentStateStore` trait 无此默认方法 → 以固有 `delete_key`（`state_store.rs:494`）+ 私有 `delete_key_inner`（:503）提供同等三路 `$unset`（states/versions/hashes + `_updated_at`） | 能力等价；调用面收窄（须经具体类型而非 `dyn AgentStateStore` 调用） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| MONGO-X01 | 驱动接缝 `MongoClient` trait + 进程级驱动 runtime 桥 | `client.rs#MongoClient`（18 方法）、`client_mongodb.rs#exec_on_driver`（专用线程 + 多线程 runtime + `oneshot` 回传） | Java 编译期直依赖 `mongodb-driver-sync` 并同步调用；Rust 抽象 21 方法 seam，异步实现，同步调用方（`BaseStore`/`is_held`）经 `block_on` 安全驱动 | 离线可测（`InMemoryMongoClient`）+ 真驱动可选；driver 任务固定在同一 runtime，`block_on` 从任意 runtime 调用不死锁、closing client 不中途取消任务 |
| MONGO-X02 | 数据库限定集合键 `"<db>::<collection>"` | `client.rs#scoped_collection`，各组件 `new`/`build` 生成 key（`distributed_store.rs:70-72`、`snapshot.rs:96`、`sandbox_guard.rs#build`） | Java 用 `getDatabase(db).getCollection(c)` 原生隔离；Rust 把库名编码进扁平 key，真驱动在 `split_key` 解码回 `Database`+`Collection` | 同一份 store 逻辑同时服务内存后端与真驱动；`database_name_partitions_state_and_base_store`、`database_name_partitions_locks`、`database_name_isolates_snapshots` 保证隔离 |
| MONGO-X03 | 组合式 `MongodbDistributedStore` + 五槽投影 | `distributed_store.rs:62-232`，`to_harness_store()` 投 `agent_state_store`/`isolation_scope`/`base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` | Java `implements DistributedStore, AutoCloseable` 并在 volatile DCL 中懒缓存四组件；Rust 在 `new()` 内一次性构造 | harness `DistributedStore` 是 struct（`harness/src/agent/distributed_store.rs:14`），组合+投影即可复用，不建平行类型；组件均为 `Clone`，无需缓存 |
| MONGO-X04 | 错误传播降级为日志 / `None` / `UNVERSIONED` | `state_store.rs:530-560`（trait `save`/`save_list` 吞错 warn）、`save_if_version` trait 版 `Err → UNVERSIONED`、`store.rs:157-176`（`put`/`put_if_version` 失败 warn 或 false）、`get_versioned` 读失败 → `UNVERSIONED` | Java 抛 `RuntimeException`/`IllegalArgumentException`（非法 key、非对象值、后端错误）；Rust harness/core trait 无 `Result` 通道 | 保持 trait 签名不变（harness 约束）；读失败回 `UNVERSIONED` 而非 `0`，避免 CAS 写者把「读失败」误判为「创建缺失」 |
| MONGO-X05 | `list_session_ids` 去重 + 排序 | `state_store.rs#list_session_ids` 用 `BTreeSet<String>` | Java `LinkedHashSet`（保留 `distinct` 服务端返回顺序） | 去重 + 确定性顺序，调用方不依赖顺序；`exists_delete_and_list_session_ids` 覆盖 |
| MONGO-X06 | 租约释放模型 | `release_async`（await 删除，token 作用域）+ `Drop` 内 `Handle::try_current().spawn` 清理，无 runtime 时退回 TTL 并 warn（`sandbox_guard.rs:277-330`） | Java `MongoLease.close()` 同步 `deleteOne` + executor 关闭兜底 | 不在 async 上下文里 `block_on`（避免毒化 worker/死锁）；Drop 无法 await，故 spawn 或退 TTL |
| MONGO-X07 | `insert_one` 缺 `_id` 时补字符串 UUID | `client_mongodb.rs#insert_one` | Java 由驱动生成 `ObjectId` | 让 `_id` 可 JSON 往返（内存后端、日志、断言一致），避免 `ObjectId` 无法表达为 `serde_json::Value` |
| MONGO-X08 | 内存后端的二进制/时间表示 | `client.rs#encode_binary`/`#decode_binary`；`bson_to_json` 把 `DateTime`→millis、`Binary`→base64、`ObjectId`→hex；不设 16GB 上限 | Java 全程 BSON `Binary`/`DateTime`，GridFS 受 16GB 规范上限约束 | 离线测试替身零依赖；真驱动路径保留原生 BSON 类型（`bson_to_json_projects_driver_only_types` 覆盖） |
| MONGO-X09 | `gridfs_upload` 先删**全部**同名文件 | `client_mongodb.rs#gridfs_upload`（`gridfs_file_ids` → 全删 → 重新上传） | Java `deleteIfExists` 只删 `find().first()` 命中的那一个，并发上传竞态下可能残留同名副本 | 消除同名多版本歧义（`download`/`exists` 语义无歧义）；`reupload_drops_stale_chunks` 覆盖 |
| MONGO-X10 | 删除回调为 async | `SessionDeleteCallback = Arc<dyn Fn(..) -> BoxFuture<'static, ()>>`，`AgentStateStore::delete` 内 `await`（`state_store.rs#delete`） | Java 同步 `BiConsumer`，异常只 `log.warn` 后继续删除 | 不再用 `futures::executor::block_on`（会阻塞/死锁 current-thread runtime）；级联失败仍 warn 不阻断删除（对齐 Java best-effort） |
| MONGO-X11 | 官方驱动接入 + 环境门控 live 测试 | `client_mongodb.rs`（`mongodb = "3"`，18 个 seam 方法全实现）、`tests/mongodb_live.rs`（`AGENTSCOPE_TEST_MONGODB_URL` 未设即 `return`，不用 `#[ignore]`） | Java 编译期直依赖驱动、contract test 需 Testcontainers/Docker 才能跑 | 默认离线全绿、真库按需开启（2026-10-31 提交 `a4da839`）；跨库/跨进程语义由 live 用例补足 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mongodb` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mongodb` / `cargo test -p agentscope-extensions-mongodb` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mongodb/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/MONGODB__MIGRATION_HISTORY.md`](./history/MONGODB__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
