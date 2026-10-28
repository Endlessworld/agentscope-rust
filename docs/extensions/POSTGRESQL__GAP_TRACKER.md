# POSTGRESQL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **POSTGRESQL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-postgresql\`
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包）
> **Rust 目标**：`crates/agentscope-extensions-postgresql/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

> **当前结论（2026-09-19 回填，2026-10-22 静态复核校正，2026-10-28 更新）**：P0 组件已全部接入且 `build`/`test` 通过。**最新实测（2026-10-28）`cargo test -p agentscope-extensions-postgresql` = 115 passed / 0 failed / 0 ignored**；**§3.2 的 4 项 P1 语义偏差已全部关闭**（`DEFAULT 1`、`save_if_version(0)` 0→1 回退、builder `create_if_not_exist` 默认 `true`、快照 `initSchema` 失败仅告警——后两项见 §8）。以下为历史快照：**离线测试覆盖的 P0 缺口已收窄完毕**——`cargo test` 由 **7 passed** 提升至 **105 passed / 0 failed**（2026-10-22 静态复核实测 `#[test]`/`#[tokio::test]` 共 105 个，原记录 104），六处零测试模块（`state`/`store`/`snapshot`/`guard`/`distributed`/`sql`）均已补齐用例，并以 Java 侧 5 个测试类（实测 **148** 个 `@Test`）逐条对应。补测过程中发现并修复了 `MemoryClient`（内存 SQL 模拟器）一处**从未被触发的真实缺陷**（其 SQL 分派只认未加引号的 `AGENTSCOPE_SESSIONS`/`AGENTSCOPE_SNAPSHOTS`，而 `state.rs` 实际发出的是 schema 限定名 `"agentscope"."agentscope_sessions"`、`snapshot.rs` 支持自定义表名），详见 §4.1。P2 待确认项见 §3.3 与 §4。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-postgresql` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包） |
| Java 规模（实测 2026-10-22） | 主代码 **1589** 行（**6** 个类）/ 测试 **1604** 行（**5** 个测试类，**148** 个 `@Test`） |
| Rust crate | `crates/agentscope-extensions-postgresql` |
| Rust 规模（实测 2026-10-28） | `src/` **13** 个文件（新增 `murmur3.rs`）/ **115** 个测试属性（`#[test]` 84 + `#[tokio::test]` 31，见 §4.1） |
| 迁移波次 | B |
| 当前状态 | **已完成 P0 骨架/组件接线 + 离线测试覆盖（build 通过，115 passed / 0 failed）；P1 语义偏差 4 项已全部关闭（见 §3.2），剩余 P2 待确认项见 §3.3、§4.2** |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `postgresql.PostgresDistributedStore`（`@Deprecated(since="2.1", forRemoval=true)`） | 组合 4 组件为 harness `DistributedStore`；`create(DataSource)` | `distributed::PostgresDistributedStore`（`src/distributed.rs`） | 持有 `Arc<dyn PostgresClient>` + 工厂方法 `agent_state_store()` / `base_store()` / `snapshot_client()` / `snapshot_spec()` / `sandbox_execution_guard()`；`distributed_store()` 输出 harness `DistributedStore`（仅 state store，见 §4.2）。与 EXTENSIONS_GAP_TRACKER §2.1 建议的「字段式 builder」形态略有偏差，见 §3.3 |
| `postgresql.state.PostgresAgentStateStore` | 会话状态：单值 + 增量列表 + `:_hash` 伴随行 + 版本 CAS | `state::PostgresAgentStateStore`（`src/state.rs`，`impl agentscope_core::state::AgentStateStore`） | trait 覆盖 `save/save_list/get/get_list/exists/delete/list_session_ids`；版本能力以固有方法 `save_versioned/save_if_version/version_of` 保留 |
| `postgresql.store.PostgresBaseStore` | 命名空间 KV + CAS + `LIKE ... ESCAPE '!'` 前缀搜索 + US(0x1F) 命名空间编码 | `store::PostgresStore`（`src/store.rs`，`impl agentscope_harness::filesystem::BaseStore`）+ `PostgresStoreBuilder` | `put_if_version` 对齐 harness CAS：`expected==0` → INSERT 且重复键判失败；`>0` → 条件 `UPDATE` 影响行数=1。**Java 的 `putIfVersion` 在 `expectedVersion < 0` 时抛 `IllegalArgumentException`，Rust `u64` 入参使该分支不可表达（等价）** |
| `postgresql.snapshot.PostgresRemoteSnapshotClient` | BYTEA 快照上传/下载/存在性 | `snapshot::PostgresRemoteSnapshotClient`（`src/snapshot.rs`，`impl RemoteSnapshotClient`） | 默认表 `agentscope_snapshots`；`upload` 用 `ON CONFLICT (snapshot_id) DO UPDATE` |
| `postgresql.snapshot.PostgresSnapshotSpec`（`extends RemoteSnapshotSpec`） | PG 快照 spec 便捷类 | `snapshot::PostgresSnapshotSpec`（newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`）+ 辅助函数 `postgres_snapshot_spec()` / `postgres_snapshot_spec_with_table()` | 快照 id 校验沿用 harness `RemoteSnapshotSpec::build` |
| `postgresql.sandbox.PostgresSandboxExecutionGuard` | PG advisory lock 分布式护栏（MurmurHash3 64-bit key + `pg_try_advisory_lock` 轮询 + `pg_advisory_unlock` 释放）；Java 侧 `implements SandboxExecutionGuard`（接口） | `guard::PostgresSandboxExecutionGuard` + `PostgresSandboxExecutionGuardBuilder` + `PostgresSandboxLease`（`src/guard.rs`） | harness 的 `SandboxExecutionGuard` 是**具体 struct 非 trait**，故为镜像 API 的独立具体类型（见 §4.2）。轮询间隔：Java `Thread.sleep(100ms)`，Rust `sleep(20ms)`（语义等价，仅退避粒度不同） |
| `postgresql.sandbox.PostgresSandboxExecutionGuard.Builder` / 内部 `PostgresLease` | 构建器与租约（Java `close()` 释放锁 + 关连接） | `PostgresSandboxExecutionGuardBuilder` / `PostgresSandboxLease`（`Drop` 自动释放） | 常量已对齐：`DEFAULT_KEY_PREFIX="agentscope:sandbox:lock:"`、`DEFAULT_LOCK_TIMEOUT_SECONDS=1800` |
| （Java 各类的 private static 辅助）`namespacePath` / `likePrefixPattern` / `validateIdentifier` / `isDuplicateKey` / SQL/DDL 模板常量 | 标识符校验、命名空间编码、LIKE 转义、重复键判定、三张表 DDL | `sql.rs`（`validate_identifier` / `namespace_path` / `like_prefix_pattern` / `qualified_table` / `sessions_ddl` / `store_ddl` / `snapshots_ddl`；常量 `NS_SEPARATOR` / `LIKE_ESCAPE` / `MAX_IDENTIFIER_LENGTH`）+ `store::is_duplicate_key` | — |
| （测试替身）Java 用 H2 / `JdbcDataSource` + 手写 mock | 离线单测 | `memory::MemoryClient`（`src/memory.rs`，内存 SQL 模拟器） | 模拟 `ON CONFLICT` upsert、条件 CAS、LIKE 前缀、advisory lock；**未做 `cfg(test)` 门控**，随 `pub mod memory` 进入公共 API（见 §3.3） |
| （真实驱动）Java `javax.sql.DataSource`（HikariCP/Druid 等） | 连接与 SQL | `client::PostgresClient`（同步 SQL seam）+ `driver::PgSimpleClient`（feature `postgres-driver`，`tokio-postgres`） | 见 §2.1 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `javax.sql.DataSource` / JDBC（`Connection`/`PreparedStatement`/`ResultSet`） | 连接、SQL 绑定与读取 | `client::PostgresClient`（`query(sql, params) -> QueryOutcome`、`execute_batch`）+ `SqlValue`/`Cell`/`Row`/`QueryOutcome` | 所有 SQL 经此 seam；async trait 由 `blocking::query_async`（`tokio::task::spawn_blocking`）桥接。`Row = Vec<Cell>`（`client.rs:114`） |
| （真实驱动，可选） | 连接 PostgreSQL | `tokio-postgres = "0.7"`（+ `bytes = "1"`），feature `postgres-driver` | `Cargo.toml` `[features] postgres-driver`，**默认不启用**（默认构建零重依赖） |
| `com.fasterxml.jackson.databind.ObjectMapper` | JSON 序列化 | `serde_json`（`StoreValue = serde_json::Map<String, serde_json::Value>`） | 已用 |
| `org.apache.commons.codec.digest.MurmurHash3` | 锁 key 64-bit 哈希 | `std::collections::hash_map::DefaultHasher`（`guard::compose_lock_key`） | **算法不同**：Java `MurmurHash3.hash128(raw)[0]`（取 128 位结果的低 64 位），Rust `DefaultHasher::finish()`（SipHash-1-3，且**不保证跨 Rust 版本稳定**），故跨语言锁 key 不互通，见 §3.3 |
| `org.slf4j.Logger` | 日志 | `tracing` | 实测仅 2 处：`driver.rs:47`（连接错误）、`store.rs:217`（`put_if_version` insert 失败）；`state`/`snapshot`/`guard` 无埋点，见 §3.3 |
| `java.util.regex.Pattern`（`^[a-zA-Z_][a-zA-Z0-9_]*$`） | 标识符校验 | `sql::validate_identifier`（手写字符判定；`MAX_IDENTIFIER_LENGTH = 63`） | 语义对齐 |
| `RuntimeException` / `SQLException` | 错误传播 | `error::PostgresError`（`Query`/`Schema`/`Decode`/`Encode`/`InvalidIdentifier`/`CasFailed`/`LockTimeout`/`DriverUnavailable`）+ `From<PostgresError> for CoreError`；别名 `PgResult<T>` | — |
| `InputStream` / `byte[]`（快照） | 快照数据流 | `&[u8]` / `Vec<u8>`（`RemoteSnapshotClient` 签名） | — |
| JUnit5 + Mockito + H2（Java 测试） | 离线测试 | Rust 内置 `#[cfg(test)]`（`state`/`store`/`snapshot`/`guard`/`distributed`/`sql` 六模块合计 **105 用例**，2026-10-22 实测） | 已按 Java 5 个测试类逐条对应（实测 **106** 处 `// Java:` 溯源注释，覆盖 **94** 个 Java 用例名，其中 92 个与 `@Test` 方法名精确匹配）；每条用例注明 Java 参考用例名 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib 导出 | `PostgresDistributedStore` 等 | 已完成 | `src/lib.rs` 声明 **10** 个常规模块 + feature 门控 `driver`（共 11 个 `pub mod`），re-export 全部公共类型（`lib.rs:34-50`） |
| `PostgresAgentStateStore`（impl core `AgentStateStore`） | `state/PostgresAgentStateStore.java` | 已完成 | 单值 `item_index=0`；列表增量 + `:_hash` 检测（复用 `agentscope_core::state::list_hash`）；`save_versioned`/`save_if_version`/`version_of` 保留版本语义 |
| `PostgresStore`（impl harness `BaseStore`）+ builder | `store/PostgresBaseStore.java` | 已完成 | CAS/`StoreItem.version` 以 harness 为准；US(0x1F) 命名空间编码 + `LIKE ... ESCAPE '!'` |
| `PostgresRemoteSnapshotClient`（impl `RemoteSnapshotClient`） | `snapshot/PostgresRemoteSnapshotClient.java` | 已完成 | `upload`/`download`/`exists`；`ON CONFLICT (snapshot_id) DO UPDATE` |
| `PostgresSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/PostgresSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec`；快照 id 校验以 harness 为准 |
| `PostgresSandboxExecutionGuard` + lease + builder | `sandbox/PostgresSandboxExecutionGuard.java` | 已完成（语义差异见 §3.3） | `try_enter(&SandboxIsolationKey)` 轮询 `pg_try_advisory_lock`；lease `Drop` 释放 |
| `PostgresDistributedStore` 组合者 | `PostgresDistributedStore.java` | 已完成 | 5 个组件工厂方法 + `distributed_store()`（输出 harness 子集） |
| 无真实 DB 可用的内存后端 | Java H2 / mock | 已完成 | `memory::MemoryClient`（内存 SQL 模拟器） |
| **离线单元测试** | Java 5 个测试类实测 **148** 个 `@Test`（`state` 64 / `store` 40 / `guard` 20 / `snapshot` 18 / `distributed` 6） | **已完成（P0 缺口已收窄完毕）** | 现状：`cargo test -p agentscope-extensions-postgresql` = **105 passed / 0 failed**（由 7 → 105；2026-10-22 静态复核实测 **105** 个测试属性 = `#[test]` 75 + `#[tokio::test]` 30）。六个模块均已补测，每条用例注明对应 Java 测试类/用例名：`sql.rs`（19，DDL/标识符边界/注入样本/namespace 编码/LIKE 转义）、`state.rs`（**34**，builder 校验 + `INFORMATION_SCHEMA` 存在性 + 单值/列表 CAS + `:_hash` 分支 + `truncate`）、`store.rs`（19，get/put/CAS 三分支/前缀搜索/删除/命名空间）、`snapshot.rs`（14，往返/覆盖写/缺失报错/spec）、`guard.rs`（11，compose key/重入/竞争/超时/Drop 释放/幂等 release）、`distributed.rs`（8，5 个工厂 + `distributed_store()` 投影 + 跨组件共享后端）。合计 19+34+19+14+11+8 = **105**。全部离线（`MemoryClient` + trait fake），无网络调用、无 `#[ignore]`（实测 0）。溯源注释：共 **106** 处 `// Java:` 引用，覆盖 **94** 个 Java 用例名（其中 92 个与 Java 测试方法名精确匹配；`isDuplicateKey` / `saveIfVersion` 为 Java 私有方法名，非 `@Test`）。对照同波次 MySQL（实测 42，原记 43）、MongoDB（62+1）基本追平（本条已不构成缺口；剩余为 §3.2 的 4 项 P1 语义偏差与 §3.3 的锁哈希等 P2） |

### 3.2 其它待办（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `supportsVersioning()` / `getVersioned()` | `state/PostgresAgentStateStore.java:258,300` | **已完成（设计决定，不迁移）** | **依据**：core `AgentStateStore` trait 无版本方法，故 `getVersioned` 降为固有 `version_of()`（返回 `PgResult<i64>`），`save_if_version`/`save_versioned` 为固有方法；版本语义保留并有 `save_if_version_cas_three_branches` 等测试覆盖（版本从 1 起、`0` 为仅创建、不匹配失败、`UNVERSIONED` 无条件写） |
| `ensureVersionColumn()`（老表补 `version` 列迁移） | 同上 `:137` | **已完成（2026-10-22 实现，2026-10-28 复核确认）** | 实现于 `sql::ensure_version_column_ddl` + `state.rs` builder（`build()` 时无条件执行 `ALTER TABLE ... ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1`）；`MemoryClient` 模拟该 DDL 为 no-op，`state.rs` 测试断言其出现在 `execute_batch` 中（`builder_create_if_not_exist_provisions_schema_and_table`）。**`DEFAULT 0` 偏差已于提交 `789d04f` 修正为 `DEFAULT 1`**，与 Java 源码注释口径一致 |
| `saveIfVersion(..., 0)` 的「0→1 回退」分支 | 同上 `:375-387` | **已完成（2026-10-22 实现，2026-10-28 复核确认）** | Java 在 `expectedVersion==0` 时先 `insertIfAbsent`，若返回 `UNVERSIONED` 则再 `updateIfVersion(..., 0L)` 把迁移后仍为 0 的旧行抬到 1。Rust `state.rs::save_if_version`（现 `:314-339`）在 `DO NOTHING` 影响 0 行时**不再直接返回 `UNVERSIONED`**，而是继续走通用 CAS 分支（`version = 0 -> 1`），对应用例 `save_if_version_zero_falls_back_to_cas_for_backfilled_row`（`:1015`）；与 `DEFAULT 1` 合计已消除「`version=0` 历史行对 `save_if_version(...,0)` 永久幻影冲突」 |
| `Builder.createIfNotExist` 的**默认值** | 同上 `:799`（Java `private boolean createIfNotExist = true;`） | **已完成（2026-10-28）** | **已对齐 Java**：`state.rs` 的 `PostgresAgentStateStoreBuilder::new` 现默认 `create_if_not_exist: true`，故 `builder(client).build()` 在空库上自动 `CREATE SCHEMA/TABLE`（等价 Java builder），构造器语义的「只校验不建表」由显式 `create_if_not_exist(false)` 表达。新增用例 `builder_defaults_to_create_if_not_exist` / `builder_default_auto_create_applies_to_custom_schema`；三个旧断言 `build_without_auto_create_fails_when_schema_missing` / `_table_missing` / `build_succeeds_when_schema_and_table_already_exist` 补 `create_if_not_exist(false)`（它们是 Java **构造器**默认（false）的等价用例，原写法的隐含默认已不存在，断言本身未放宽）。同类字段核对：`PostgresStoreBuilder::initialize_schema` 默认 `false` ↔ Java `PostgresBaseStore.Builder.initializeSchema`（`private boolean initializeSchema;`）一致；guard builder 无此类字段；快照为显式构造参数 |
| `initSchema()` 失败**仅告警不抛错** | `snapshot/PostgresRemoteSnapshotClient.java:61-78` | **已完成（2026-10-28）** | **已对齐 Java**：`snapshot.rs::with_table` 在 `initialize_schema=true` 时改为 `if let Err(e) = ... { tracing::warn!(table, error = %e, "failed to initialize snapshot table; ...") }`，构造不再上抛（`PostgresSnapshotSpec::new` / `postgres_snapshot_spec*` 随之可用），并保留底层 message（`error = %e`）。新增用例 `constructor_tolerates_schema_init_failure`（DDL 失败的 fake client 下 4 条构造路径均成功、表名校验仍生效）。**刻意不一刀切**：`state` 的 `CREATE SCHEMA/TABLE` 与 `ensureVersionColumn`（Java `:155-186`/`:137-153` 均 `throw RuntimeException`）、`store` 的 `initializeSchema`（Java `PostgresBaseStore:175-188` `throw IllegalStateException`）、以及显式函数 `initialize_snapshot_table`（无条件上抛，用例 `explicit_initialize_helper_still_reports_failures`）保持上抛——Java 侧只有 snapshot 构造器这一处只告警 |
| Java 用例的**未覆盖清单**（147 个 `@Test` 中 53 个无 Rust 对应用例） | 见 §3.2 末「Java→Rust 用例覆盖差异」 | **待办（P2）** | 缺口集中在三类，均属 **Java 侧 Mockito 专属**（Rust 无 mock 框架，等价覆盖已由 trait fake 承担）：① **SQL 失败路径**（`getThrowsWhenSqlFails` / `putThrowsWhenSqlFails` / `deleteThrowsWhenSqlFails` / `searchThrowsWhenSqlFails` / `uploadThrowsWhenSqlFails` / `downloadThrowsWhenSqlFails` / `existsThrowsWhenSqlFails` / `saveListThrowsWhenSqlFails` 等，Rust 侧仅 `state.rs` 有 `query_failures_surface_through_inherent_methods` / `truncate_propagates_sql_errors` / `build_propagates_batch_failures` 少量覆盖，`store`/`snapshot`/`guard` 缺）；② **JDBC 异常包装细节**（`getPropagatesResultSetCloseFailure` / `tryEnterPropagatesResultSetCloseFailure` / `tryEnterHandlesInnerSQLExceptionAndCloseFailure` / `leaseCloseHandlesConnectionCloseFailure` / `rollbackFailureIsAddedAsSuppressed`）；③ **null 参数拒绝**（`uploadRejectsNullData` / `existsRejectsNullSnapshotId` / `namespaceSegmentCannotBeNull` / `rejectsNullDataSourceInConstructor`）。另有 **Java 事务语义未迁移**（`executeInWriteTransactionRollsBackOnException` / `executeInWriteTransactionKeepsAutoCommitWhenAlreadyDisabled`）——Rust `PostgresClient` seam 无事务抽象，`state.rs` 的多语句写为**非原子**（`save_list_sync` 的 delete+insert+hash 三步无事务保护），与 Java `executeInWriteTransaction` 不同 |
| `verifySchemaExists()` / `verifyTableExists()`（`INFORMATION_SCHEMA` 校验） | 同上 `:183,203` | **已完成** | 实现于 `sql::{SCHEMA_EXISTS_QUERY, TABLE_EXISTS_QUERY}` + `state.rs::{verify_schema_exists, verify_table_exists}`：`create_if_not_exist=false` 时先查 schema 再查 table，缺失报 `PostgresError::Schema`、查询失败报 `PostgresError::Query`；测试 `build_without_auto_create_fails_when_schema_missing` / `_table_missing` / `build_succeeds_when_schema_and_table_already_exist` 覆盖 |
| `getDataSource()` / `close()` | 同上 `:701,737` | **已完成（设计决定，不迁移）** | **依据**：Rust 无连接所有权（client 为 `Arc<dyn PostgresClient>`，由调用方持有并管理生命周期），无 `close` 语义可迁 |
| `truncateAllSessions()` | 同上 `:718` | 已完成 | `state::truncate_all_sessions()` |
| store 的 `objectMapper(ObjectMapper)` builder 注入 | `store/PostgresBaseStore.java:394` | **已完成（设计决定，不迁移）** | **依据**：Rust 固定使用 `serde_json`（`StoreValue = serde_json::Map<String, serde_json::Value>`），序列化器非可插拔，无注入点亦无需求 |
| store 的表名/schema 校验 | `store/PostgresBaseStore.java:439` | 已完成 | `sql::validate_identifier`（与 state 共用） |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| 锁 key 哈希算法差异 | Java 用 MurmurHash3（`MurmurHash3.hash128(raw.getBytes(UTF_8))[0]`，128 位结果的**低 64 位**）；Rust 用 `DefaultHasher`（SipHash-1-3，`guard.rs:105-107`）。**静态可判定为不一致**：两者是完全不同的哈希族，且 Rust `DefaultHasher` 的输出**不保证跨 Rust 版本稳定**（标准库明确声明），故同前缀下两语言生成的 advisory lock key **不互通**（跨语言混布存在并发互斥失效风险）。Java 的 `raw` 拼接式为 `keyPrefix + scope.name().toLowerCase() + ":" + value`（`IsolationScope` 是枚举），Rust 侧 `scope` 是 `String`（`SandboxIsolationKey.scope`），拼接式等价但**取值空间不同**（Java 仅 4 个枚举值，Rust 为任意字符串） | **待确认（本轮不改算法，属跨语言兼容议题）** |
| guard 文档与实现不一致 | 经核：`src/guard.rs:7` 现文档为 `try_enter` / `release`，**与实现一致**（早期声称的 `is_held` 已不存在） | **已完成（实测已一致）** |
| advisory lock 与连接池语义 | Java 每租约独占 `Connection`，锁随连接关闭自动释放；Rust `PgSimpleClient` 为共享 client + `spawn_blocking`，而 PG advisory lock 是**会话级**，共享连接下锁归属/释放时机可能与 Java 不同（`MemoryClient` 无法暴露该风险）。**静态可判定为真实风险**：`PgSimpleClient` 内部只有**单个** `tokio_postgres::Client`（`driver.rs:21-24`），`spawn_blocking` 并不保证两次 `query` 落在同一 PG 会话上；`pg_try_advisory_lock` / `pg_advisory_unlock` 若落在不同会话则语义失效 | **待确认** |
| `tracing` 埋点缺失 | Java 侧实测日志点：`PostgresSandboxExecutionGuard` 7 处（acquire/release/close 失败等 `log.debug`/`log.warn`）、`PostgresRemoteSnapshotClient` 1 处（initSchema 失败 `log.warn`）、`PostgresBaseStore` 1 处（`LOG.debug` build）；`PostgresAgentStateStore` / `PostgresSnapshotSpec` / `PostgresDistributedStore` **无日志**。Rust 侧仅 2 处：`driver.rs:47`（连接错误）、`store.rs:217`（`put_if_version` insert 失败） | 待办 |
| `memory::MemoryClient` 未门控 | 测试替身现为 `pub mod memory` + re-export，进入生产 API；MySQL 侧放在 `testing.rs` 子模块（`src/{state,store,snapshot,guard}/testing.rs`） | 待办（P2；若加门控需同步迁移全部测试与 `lib.rs` re-export） |
| 组合者形态偏差 | EXTENSIONS_GAP_TRACKER §2.1 建议「字段式追加 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` + 自建 builder」；本 crate 改为「持有 `client` + 工厂方法」，且未暴露 `isolation_scope`。功能等价但形态不一致 | **待确认** |
| `PostgresDistributedStore` 缺 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` 的**投影入口** | Java `PostgresDistributedStore` 直接 `implements DistributedStore`（4 个方法全部暴露给 harness）；Rust `distributed_store()` 只输出 harness `DistributedStore{agent_state_store, isolation_scope}`（`distributed.rs:81-87`），其余 3 个组件需调用方自行 `store.base_store()` 等工厂方法，**harness `SandboxManager` 无法自动取到 PG guard** | **已解决（2026-10-28）**：harness `DistributedStore` 已增 `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard` 三槽 + `SandboxGuard` trait，`distributed_store()` 用 builder 填满 5 槽，`PostgresSandboxExecutionGuard` 已 `impl SandboxGuard`（详见 §2026-10-28 执行记录） |
| 真实 PostgreSQL 集成测试 | 需真实实例（Java 侧用 H2 近 PG 方言离线跑）；当前仅内存模拟器 | 待办（环境受限） |
| `error.rs::to_core` / `PostgresError::CasFailed` / `DriverUnavailable` 为**死代码** | `to_core`（`error.rs:46`）全 crate 无调用；`PostgresError::CasFailed` / `DriverUnavailable` 变体**从未被构造**（实测引用数 0）。Java 侧对应 `IllegalStateException("PostgresBaseStore putIfVersion (update) failed")` 是**抛出**语义，Rust 侧改为返回 `false`，故 `CasFailed` 无用 | 待办（P2，清理或补齐） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态**：`crates/agentscope-extensions-postgresql/` 的 P0 组件已全部落地并接线；**离线测试覆盖 P0 缺口已补齐**，编译与全部测试通过。
- **验证命令与结果（2026-10-28，本轮实测）**：
  - `cargo build -p agentscope-extensions-postgresql` → **通过**（EXIT=0；仍仅 2 个既有警告：`error.rs::to_core` dead_code、`memory.rs::SessionRow::created_at` 未读）
  - `cargo test -p agentscope-extensions-postgresql` → **115 passed / 0 failed / 0 ignored**（EXIT=0；0 doc-tests）。分布：`sql` 19、`state` 37（15 `#[test]` + 22 `#[tokio::test]`）、`store` 19、`snapshot` 16（10 + 6）、`guard` 11、`distributed` 8（5 + 3）、`murmur3` 5 → 合计 **115**（`#[test]` 84 + `#[tokio::test]` 31）。基线（本轮开工前）为 **111 passed**，本次 +4 用例
  - 全部测试离线：`MemoryClient` / trait fake（`FailingClient`、`BatchOkQueryFailClient`、`TruncateFailClient`、`DdlFailClient`），无真实数据库、无网络、无 `#[ignore]`
- **验证命令与结果（2026-09-19，历史记录）**：
  - `cargo build -p agentscope-extensions-postgresql` → **通过**（exit 0；仅 2 个既有警告：`error.rs::to_core` dead_code、`memory.rs::SessionRow::created_at` 未读，均非本次新增）
  - `cargo test -p agentscope-extensions-postgresql` → **104 passed / 0 failed / 0 ignored**（+ 0 doc-tests）；用例数 **7 → 104**。分布：`sql` 19、`state` 33、`store` 19、`snapshot` 14、`guard` 11、`distributed` 8（合计 104；含 `#[tokio::test]` 异步用例）
  - 全部测试离线：后端为 `MemoryClient`（内存 SQL 模拟器）/ trait fake（`FailingClient`、`BatchOkQueryFailClient`、`TruncateFailClient`），**无真实数据库、无网络调用、无 `#[ignore]`**
- **2026-10-22 静态复核校正（本轮未编译，静态核对 `#[test]` / `#[tokio::test]` 属性数）**：实测测试属性 **105** 个（原记 104），分布 `sql` 19、`state` 34、`store` 19、`snapshot` 14、`guard` 11、`distributed` 8 = **105**（其中 `#[test]` 75 + `#[tokio::test]` 30）；`#[ignore]` 实测 **0**。逐模块确认：**六处零测试区（`state`/`store`/`snapshot`/`guard`/`distributed`/`sql`）均已补齐**，无一为 0——`state` 34（含 `INFORMATION_SCHEMA` 存在性校验、单值/列表 CAS、`:_hash` 分支、`truncate`）、`store` 19（get/put/CAS 三分支/前缀搜索/删除/命名空间）、`snapshot` 14（往返/覆盖写/缺失报错/spec）、`guard` 11（compose key/重入/竞争/超时/Drop 释放/幂等 release）、`distributed` 8（5 个工厂 + `distributed_store()` 投影 + 跨组件共享后端）、`sql` 19（DDL/标识符边界/注入样本/namespace 编码/LIKE 转义）。
- **`MemoryClient` SQL 分派缺陷修复确认（已核实）**：`memory.rs:124-133` 的 `normalize()` 确实**剥离双引号**并折叠空白/大写；分派键已从「表名字面量」改为「**列签名**」——`SESSION_ID`（session 写入/CAS/查询/删除，`:248`/`:283`/`:329`/`:398`）、`NAMESPACE_PATH`/`AGENTSCOPE_STORE`（KV，`:418`/`:454`/`:491`/`:511`）、`SNAPSHOT_ID`（快照，`:521`/`:532`/`:541`）。故 schema 限定名 `"agentscope"."agentscope_sessions"` 与任意自定义快照表名均可被识别——**该缺陷确已修复**。
- **crate 文件清单**（`src/` 平铺，无子目录，**12** 个文件 / **3722** 行非空）：`lib.rs`(2355B)、`client.rs`(4093B)、`blocking.rs`(1230B)、`sql.rs`(13547B)、`error.rs`(1909B)、`memory.rs`(25559B)、`store.rs`(22279B)、`state.rs`(45421B)、`snapshot.rs`(13479B)、`guard.rs`(16188B)、`distributed.rs`(8801B)、`driver.rs`(5648B，feature `postgres-driver` gated)；`Cargo.toml`(1148B)。
- **架构要点**：全部 SQL 经同步 `PostgresClient` seam；async trait（`AgentStateStore`、`RemoteSnapshotClient`）由 `blocking::query_async`（`spawn_blocking`）桥接；同步 `BaseStore` 直接调用 client。故**无真实数据库也能编译与测试**。
- **复用契约（对齐 EXTENSIONS_GAP_TRACKER §2.1，未新建公共 store crate）**：`impl agentscope_core::state::AgentStateStore`（`state.rs:446`）、`impl agentscope_harness::filesystem::BaseStore`（`store.rs:130`）、`impl agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot.rs:63`）、`impl SandboxSnapshotSpec`（`snapshot.rs:158`）；harness `DistributedStore` 仅取 `agent_state_store` + `isolation_scope`（见 §4.2）。
- **B 波次统一产出契约核对（5/5 全部存在且接线）**：`PostgresDistributedStore`（`distributed.rs:26`，工厂 `agent_state_store`/`base_store`/`snapshot_client`/`snapshot_spec`/`sandbox_execution_guard` + `distributed_store()`）、`PostgresAgentStateStore`（`state.rs:130`）、`PostgresStore`（`store.rs:80`，`impl BaseStore`）、`PostgresSnapshotSpec`（`snapshot.rs:139`，`impl SandboxSnapshotSpec`）、`PostgresSandboxExecutionGuard`（`guard.rs:77`）——**5 个类型均存在、均已 re-export 到 crate 根**（`lib.rs:34-50`），且 `lib.rs` 头部文档已逐条列出。
- **已知问题**：离线测试覆盖 P0 缺口已关闭（§3.1）；**P1 语义偏差 4 项已全部关闭**（`DEFAULT 1`、`save_if_version(0)` 0→1 回退、builder `create_if_not_exist` 默认 `true`、快照 `initSchema` 失败仅告警——见 §3.2 与 §7）。剩余为 P2：锁 key 哈希（已改 MurmurHash3，跨语言取值空间仍需真实实例验证）、advisory lock 连接语义待确认、`tracing` 埋点缺失、`memory::MemoryClient` 未门控、组合者形态偏差、Java 事务语义未迁移（§3.2 / §3.3）。

### 4.2 下一步与阻塞
1. ~~**P0（本模块内，最优先）**：补齐离线单元测试~~ → **已完成**（2026-09-19，7 → 104 passed；2026-10-22 静态复核实测 **105** 个测试属性）。后续测试可继续下沉到 §3.3 的 advisory lock 连接池语义与真实驱动路径（需环境）。
2. **跨模块需求（不改 harness）**：harness `SandboxExecutionGuard` 是**具体 struct 而非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），本 crate 无法 `impl`，只能提供镜像类型的 `PostgresSandboxExecutionGuard`（`try_enter`/`release`），**无法直接接入 harness `SandboxManager`**（后者接受具体 `SandboxExecutionGuard`）。若后续 harness 将 guard 抽为 trait，本 crate 可补 `impl`。记入此处由汇总方评估。
3. **harness `DistributedStore` 字段不足**：已按 EXTENSIONS_GAP_TRACKER §2.1 裁定「不改 harness」，本 crate `distributed_store()` 仅携带 `agent_state_store`；`base_store`/`snapshot_spec`/`guard` 以独立工厂方法暴露。是否需扩展 harness 组合包仍待汇总方。
4. **待确认**：~~锁 key 哈希（`DefaultHasher` vs Java MurmurHash3）跨语言一致性~~ → **已改为 MurmurHash3 x64_128 并与 Java 金标向量对齐（§7 裁定 6）**，剩余为混部实机验证；advisory lock 在共享连接/连接池下的归属与释放语义；真实 PostgreSQL 集成测试环境。
5. ~~**P1**：老表 `version` 列迁移（`ensureVersionColumn` 等价）与 `INFORMATION_SCHEMA` 存在性校验（`verifySchemaExists`/`verifyTableExists`）尚未迁移。~~ → **已完成**（两项均已迁移，见 §3.2）。~~复核另发现 4 项 P1 语义偏差待处理~~ → **4 项 P1 已全部关闭（2026-10-28）**：`DEFAULT 1` 与 `save_if_version(0)` 0→1 回退见 §7，builder `create_if_not_exist` 默认 `true` 与快照 `initSchema` 仅告警见 §8。
6. **P2（2026-10-22 复核新增）**：Java 侧 147 个 `@Test` 中 **53 个无 Rust 对应用例**（集中于 SQL 失败路径 / JDBC 异常包装 / null 参数拒绝，详见 §3.2 末行）；Java `executeInWriteTransaction` 的**事务原子性未迁移**（Rust `save_list_sync` 的 delete+insert+hash 三步为非原子）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-postgresql` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-postgresql` / `cargo test -p agentscope-extensions-postgresql` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-postgresql/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

---

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态核对**：未执行 `cargo build` / `cargo test`（本轮为子智能体复核任务，明令禁止编译与跑测试）。所有数字均由源码文本统计得出（`Get-Content` 行数、`Select-String` 属性/方法匹配），**未运行任何构建产物**。
- **复核范围**：`crates/agentscope-extensions-postgresql/`（Rust，**未改动任何 Rust 代码**）↔ `agentscope-extensions/agentscope-extensions-postgresql/`（Java）。
- **统计口径**：Java 行数 = **非空行**（total 1752 / 1893 减去空行）；Rust 行数 = **非空行**（total 4115 减去空行）。两者口径一致。

### 6.2 实测统计

| 项 | 实测值 | 口径 / 证据 |
|---|---|---|
| Java 主代码 | **1589** 行（非空，total 1752）/ **6** 个类 | `src/main/java/**`：`PostgresDistributedStore` / `sandbox.PostgresSandboxExecutionGuard` / `snapshot.PostgresRemoteSnapshotClient` / `snapshot.PostgresSnapshotSpec` / `state.PostgresAgentStateStore` / `store.PostgresBaseStore` |
| Java 测试代码 | **1604** 行（非空，total 1893）/ **5** 个测试类 | `src/test/java/**` |
| Java `@Test` 数 | **148** | `state` 64 + `store` 40 + `guard` 20 + `snapshot` 18 + `distributed` 6（逐 `@Test`→`void` 配对计数） |
| Java 唯一测试方法名 | 143（含 `setUp`/`tearDown` 各 5 个） | 去重后 143，其中 `@Test` 方法 138 |
| Rust `src/` | **12** 个文件 / **3722** 行（非空，total 4115） | `blocking` 24 + `client` 132 + `distributed` 173 + `driver` 128 + `error` 43 + `guard` 377 + `lib` 47 + `memory` 574 + `snapshot` 306 + `sql` 294 + `state` 1069 + `store` 555 |
| Rust 测试属性 | **105**（`#[test]` 75 + `#[tokio::test]` 30） | 逐文件：`sql` 19、`state` 34（13 plain + 21 tokio）、`store` 19、`snapshot` 14（8 + 6）、`guard` 11、`distributed` 8（5 + 3） |
| Rust `#[ignore]` | **0** | 全 crate 无匹配 |
| Rust `// Java:` 溯源注释 | **106** 处 / 覆盖 **94** 个 Java 用例名 | 其中 92 个与 Java `@Test` 方法名精确匹配；`isDuplicateKey` / `saveIfVersion` 为 Java **私有方法**名（非 `@Test`） |
| Java `@Test` 未被 Rust 引用 | **53** 个（143 − 90 匹配） | 集中于 SQL 失败路径 / JDBC 异常包装 / null 参数拒绝（见 §6.4 P2-1） |
| Rust 无 `// Java:` 溯源的用例 | 14 个 | `sql` 10（DDL/工具函数级断言，无一一对应的 Java 用例）+ `state` 1 + `snapshot` 1 + `distributed` 2 |

### 6.3 对齐结论表

| 声明（tracker 原文） | 复核结论 | 证据 |
|---|---|---|
| Java 主 1589 行 / 6 类 | ✅ **属实** | 非空行 1589，类 6 个 |
| Java 测 1604 行 / 5 类 / 148 `@Test` | ✅ **属实** | 逐 `@Test`→`void` 配对 = 148 |
| Java 规模「rollup 记 1571/1583」 | ⚠️ **rollup 偏低** | rollup 的 1571/1583 疑为「非空且去 import/注释」口径（实测 `nonblank_noLineComment` = 1291/1516，去 license 头 16 行 = 1493/1524）；本 tracker 的 1589/1604（纯非空行）与 Java 侧计数自洽，建议保留 1589/1604 并在 rollup 同步 |
| Rust src 12 文件 / 3722 行 | ✅ **属实** | 12 个 `.rs`；非空行合计 3722 |
| Rust 105 个测试 | ✅ **属实**（rollup 记 104 **偏低 1**） | `#[test]` 75 + `#[tokio::test]` 30 = 105；rollup 记 104 应为旧值 |
| Java 类 → Rust 对应映射「真实存在」 | ✅ **全部存在** | `PostgresDistributedStore`→`distributed.rs:26`；`PostgresAgentStateStore`→`state.rs:130`；`PostgresBaseStore`→`PostgresStore`→`store.rs:80`；`PostgresRemoteSnapshotClient`→`snapshot.rs:25`；`PostgresSnapshotSpec`→`snapshot.rs:139`；`PostgresSandboxExecutionGuard`→`guard.rs:77`；5 个全部 `lib.rs:34-50` re-export |
| 六处零测试区已补齐 | ✅ **属实** | `state` 34 / `store` 19 / `snapshot` 14 / `guard` 11 / `distributed` 8 / `sql` 19，无一为 0 |
| `MemoryClient` SQL 分派缺陷已修复 | ✅ **属实** | `memory.rs:124-133` `normalize()` 剥离双引号；分派键为列签名 `SESSION_ID` / `NAMESPACE_PATH` / `SNAPSHOT_ID`（`:248`/`:283`/`:329`/`:398`、`:418`/`:454`/`:491`/`:511`、`:521`/`:532`/`:541`） |
| B 波次统一产出契约 5 件套 | ✅ **全部存在且接线** | 见 §4.1「B 波次统一产出契约核对」 |
| §3.3 锁 key 哈希跨语言一致性（P2） | ⚠️ **静态可判定为不一致** | Java `MurmurHash3.hash128(UTF-8)[0]`（MurmurHash3 128 变体低 64 位）vs Rust `DefaultHasher`（SipHash-1-3）——**不同哈希族**，且 Rust 侧标准库声明其输出**不保证跨 Rust 版本稳定**；另 Java `scope` 是 4 值枚举、Rust `scope` 是任意 `String`，取值空间亦不同。**结论：跨语言 advisory lock key 确定不互通** |
| 「guard 文档与实现不一致 → 已一致」 | ✅ **属实** | `guard.rs:7` 文档为 `try_enter`/`release`，与实现一致 |
| 「`tracing` 埋点缺失：Java 日志分布在 guard/store/snapshot/state」 | ⚠️ **原文有误（已勘误）** | 实测 Java 日志点仅 **9** 处：`guard` 7、`snapshot` 1（`initSchema` 失败）、`store` 1（`build`）；**`state` 侧 0 处**。Rust 侧 2 处（`driver.rs:47`、`store.rs:217`） |
| 「Java `putIfVersion` 在 `expectedVersion < 0` 抛 `IllegalArgumentException`」 | ✅ **属实** | Java `PostgresBaseStore.java:244-246`；Rust `BaseStore::put_if_version` 签名 `expected_version: u64`（harness trait 约束），负值不可表达 |

### 6.4 新发现缺口

**P0：无**（未发现新的 P0 链路缺口；P0 组件接线与离线覆盖结论成立）。

**P1-1 `ensure_version_column` 的 `DEFAULT 0` vs Java `DEFAULT 1`**（原文已载）
- Java：`PostgresAgentStateStore.java:137-153`，`ALTER TABLE ... ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1`，源码有专门注释说明**不能**用 0。
- Rust：`sql.rs:131-135` `ensure_version_column_ddl` = `... NOT NULL DEFAULT 0`；`state.rs:86-87` 每次 `build()` 无条件执行。
- 影响：迁移后历史行 `version=0` 与「行不存在」哨兵撞车。

**P1-2 `save_if_version(..., 0)` 缺 0→1 回退**（原文已载）
- Java：`PostgresAgentStateStore.java:375-384`，`insertIfAbsent` 返回 `UNVERSIONED` 后**再执行** `updateIfVersion(..., 0L)`。
- Rust：`state.rs:310-328`，`DO NOTHING` 影响 0 行即 `Ok(UNVERSIONED)`，无回退。
- 影响：与 P1-1 叠加后，`version=0` 的历史行对 `save_if_version(...,0)` **永久报幻影 CAS 冲突**（Java 源码注释明确警告的场景）。
- Java 对应用例：`PostgresAgentStateStoreTest#saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow`（`:501-515`，回归 issue #3162）——Rust **无对应用例**。

**P1-3 `Builder.createIfNotExist` 默认值与 Java 相反（本轮新增）**
- Java：`PostgresAgentStateStore.java:799` `private boolean createIfNotExist = true;` → `builder(ds).build()` **默认自动建 schema + 表**（注意与构造器 `new PostgresAgentStateStore(ds)` 的默认 `false` 不一致，Java 自身即有两种默认）。
- Rust：`state.rs:53` `create_if_not_exist: false` → `builder(client).build()` 在 schema 缺失时直接 `Err(PostgresError::Schema)`。
- 影响：按 Java 写法迁移的调用方在 Rust 上会**构建失败**。现有测试 `build_without_auto_create_fails_when_schema_missing`（`state.rs:697-701`）把该差异固化为「预期行为」，需与设计方确认取哪一侧。
- 缓解面：`PostgresDistributedStore::agent_state_store()` 走 `create_if_not_exist(true)`（`distributed.rs:54`），组合者路径不受影响。

**P1-4 快照 `initSchema()` 失败：Java 告警 vs Rust 上抛（本轮新增）**
- Java：`PostgresRemoteSnapshotClient.java:61-78` 捕获 `SQLException` 后仅 `log.warn(...)`，构造仍返回对象；Java 用例 `initSchemaFailureIsLogged`（`:168-171`）断言 `assertNotNull(new PostgresRemoteSnapshotClient(dataSource, true))`。
- Rust：`snapshot.rs:45-49` `execute_batch` 失败 `.map_err(Error::from)?` **上抛**；`PostgresSnapshotSpec::new`（`snapshot.rs:143-145`）随之失败；`distributed.rs:70-73` 的 `snapshot_spec()` 亦把错误包装为 `PostgresError::Schema`。
- 影响：Java 可用的构造路径在 Rust 上被阻断（Rust 更严格，但语义不等价）。Rust 测试注释（`snapshot.rs:282-283`）已自认该差异，但 tracker 此前未登记。

**P2-1 Java `@Test` 覆盖缺口 53 个**
- 分布：SQL 失败路径（`get/put/delete/search/upload/download/exists/saveList` 各 `*ThrowsWhenSqlFails`）、JDBC 异常包装（`getPropagatesResultSetCloseFailure` / `tryEnterPropagatesResultSetCloseFailure` / `tryEnterHandlesInnerSQLExceptionAndCloseFailure` / `leaseCloseHandlesConnectionCloseFailure` / `rollbackFailureIsAddedAsSuppressed`）、null 参数拒绝（`uploadRejectsNullData` / `existsRejectsNullSnapshotId` / `namespaceSegmentCannotBeNull` / `rejectsNullDataSourceInConstructor` / `constructorWithNullTableNameUsesDefault`）、边界（`builderNullNamesFallbackToDefaults` / `builderBlankNamesFallbackToDefaults` / `closeIsNoOp` / `createTableFailureThrows` / `getListCount*`）。
- 说明：多数依赖 Java Mockito（Rust 无 mock 框架），Rust 侧以 trait fake（`FailingClient` 等）+ `MemoryClient` 等价覆盖，但 `store` / `snapshot` / `guard` 三模块的**失败路径**确实偏薄。

**P2-2 Java `executeInWriteTransaction` 的事务原子性未迁移**
- Java `PostgresAgentStateStore` 全部写路径走 `executeInWriteTransaction`（`setAutoCommit(false)` + `commit`/`rollback`）。
- Rust `PostgresClient` seam **无事务抽象**（`client.rs:138-146` 仅 `query` / `execute_batch`）；`state.rs:359-380` `save_list_sync` 的 delete + insert + hash 三步为**非原子**，中间失败会留下半写状态（Java 侧会 rollback）。对应 Java 用例 `executeInWriteTransactionRollsBackOnException`（`:518`）无 Rust 对应。

**P2-3 `PgSimpleClient` 单会话使 advisory lock 归属失效（§3.3 原「待确认」项，已可静态判定）**
- `driver.rs:21-24` 仅持有**单个** `tokio_postgres::Client`；`blocking.rs:18` 的 `spawn_blocking` 不保证两次 `query` 落在同一 PG 会话。
- Java 每租约独占 `Connection`（`PostgresSandboxExecutionGuard.java:75`），锁随连接关闭释放。Rust 侧 `pg_try_advisory_lock` / `pg_advisory_unlock` 若跨会话则互斥语义失效。

**P2-4 `PostgresError::CasFailed` / `DriverUnavailable` / `error::to_core` 为死代码**
- 实测引用数：`CasFailed` 0、`DriverUnavailable` 0、`to_core` 0（仅定义处）。Java 侧 `putIfVersion` update 失败是**抛 `IllegalStateException`**，Rust 改为返回 `false`——语义差异已被 §2 记录，但 `CasFailed` 变体与 `to_core` 应清理或补齐。

**P2-5 `PostgresDistributedStore` 未把 3 个组件投影进 harness `DistributedStore`**
- Java `PostgresDistributedStore implements DistributedStore`，4 个方法全部暴露；Rust `distributed_store()`（`distributed.rs:81-87`）只输出 `{agent_state_store, isolation_scope}`，`base_store` / `snapshot_spec` / `guard` 只能由调用方自行取用，harness `SandboxManager` **无法自动拿到 PG guard**。依赖 harness 扩字段（EXTENSIONS_GAP_TRACKER §2.2 已裁定「不改 harness」），故本 crate 侧暂记 P2。

### 6.5 勘误列表（原文 → 实际）

| # | 原文 | 实际 |
|---|---|---|
| 1 | §3.3「`tracing` 埋点缺失：Java SLF4J 日志点分布在 **guard/store/snapshot/state**」 | Java 实测日志点仅 **9** 处：`guard` 7、`snapshot` 1、`store` 1；**`state` 侧 0 处**（`PostgresAgentStateStore` 无 Logger 导入） |
| 2 | §4.1 crate 文件清单中 12 个 `.rs` 的字节数均为旧值（如 `sql.rs`(5719B)、`state.rs`(17749B)、`guard.rs`(7329B)） | 2026-10-22 实测：`sql.rs`(13547B)、`state.rs`(45421B)、`guard.rs`(16188B)、`memory.rs`(25559B)、`store.rs`(22279B)、`snapshot.rs`(13479B)、`distributed.rs`(8801B)、`lib.rs`(2355B)、`client.rs`(4093B)、`blocking.rs`(1230B)、`error.rs`(1909B)、`driver.rs`(5648B)；`Cargo.toml`(1148B，原记 1117B) |
| 3 | §4.1「验证命令与结果」仍写 **104 passed**，分布 `state` 33 | 静态实测 **105** 个测试属性（`#[test]` 75 + `#[tokio::test]` 30），分布 `state` **34**、`sql` 19、`store` 19、`snapshot` 14、`guard` 11、`distributed` 8 |
| 4 | §4.1「复用契约」标注 `impl AgentStateStore` 位于 `state.rs:388` | 实际位于 `state.rs:446`（`#[async_trait] impl AgentStateStore for PostgresAgentStateStore`） |
| 5 | §1「Java 规模（实测 2026-10-22）」未给出 Rust 侧规模 | 补记：Rust `src/` **12** 文件 / **3722** 行（非空） / **105** 测试属性 |
| 6 | §2.1「JUnit5 + Mockito + H2」行称 Rust 侧「**92** 处 `// Java:` 溯源注释」 | 实测 **106** 处 `// Java:` 引用，覆盖 **94** 个 Java 用例名（92 个精确匹配 `@Test` 方法名 + 2 个 Java 私有方法名） |
| 7 | §4.2 第 5 条「老表 `version` 列迁移与 `INFORMATION_SCHEMA` 校验**尚未迁移**」 | 两项**均已迁移**（`sql::ensure_version_column_ddl` + `state.rs:101-126`），原文与 §3.2「已完成」自相矛盾；已改为删除线 + 复核新增的 4 项 P1 偏差 |
| 8 | §3.3「advisory lock 与连接池语义 …… `MemoryClient` 无法暴露该风险」（仅描述） | 静态可判定为**真实风险**：`PgSimpleClient` 仅单 `Client`（`driver.rs:21-24`），`spawn_blocking` 不保证同会话 |

### 6.6 本轮复核结论

- **P0 链路无缺口**：5 个统一产出契约全部存在、接线并 re-export；六处零测试区全部补齐（105 个测试属性）；`MemoryClient` SQL 分派缺陷确已修复且修复方式正确（列签名分派 + 去引号归一化）。
- **剩余真实缺口**：**P1 共 4 项**（P1-1 `DEFAULT 0`、P1-2 缺 0→1 回退、P1-3 builder 默认值相反、P1-4 快照 init 失败上抛），其中 P1-1 与 P1-2 **必须同改**，否则迁移后老表行会被永久幻影 CAS 冲突锁死；**P2 共 5 项**（用例覆盖差 53、事务原子性未迁移、advisory lock 单会话归属、死代码、组合者投影）。
- **§3.3 锁 key 哈希**：**静态判定为跨语言不一致**（MurmurHash3-128 低 64 位 vs SipHash-1-3，且 Rust 侧不保证跨版本稳定；且 scope 取值空间不同）——若存在 Java/Rust 混部部署，此为**互斥失效**的真实风险，建议升级为 P1 或在文档中明示「禁止混部」。
- **建议 rollup 状态值**：**「已完成（P0 骨架/组件接线 + 离线测试覆盖；余 4 项 P1 语义偏差 + 5 项 P2）」**。理由：P0 组件、接线、统一产出契约、六模块零测试区补齐、`MemoryClient` 缺陷修复**均已静态核实成立**，测试数 105 亦属实（rollup 记 104 需上修为 105）；但 P1-1/P1-2 属**数据正确性**问题（老表迁移后 CAS 永久失败）、P1-3 属**API 语义反转**（按 Java 写法迁移的调用方直接构建失败）、P1-4 属**构造路径阻断**，三者均会在真实 PG 部署中暴露，故**不宜标为完全「已完成」**；P2 各项（尤其 P2-2 事务原子性、P2-3 advisory lock 单会话）建议在 tracker 与 rollup 中一并登记，避免被当作「零缺口」结项。


## 7. 裁定执行记录（2026-10-22，汇总方）

> 依据 `EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 1（`save_if_version(0)` 语义）与裁定 6（锁 key 哈希）。

### 7.1 已修复

| 项 | 内容 | 验证 |
|---|---|---|
| 裁定 1（P1-1 / P1-2） | `ensure_version_column_ddl` 的 `DEFAULT 0` → **`DEFAULT 1`**（对齐 Java `ensureVersionColumn()`：`0` 对 CAS 表示"行不存在"，回填 0 会让老行永久幻影冲突）；`save_if_version(0)` 在 INSERT 冲突后**落到通用 CAS 分支**（`WHERE version = 0`）实现 **0→1 回退** | 新增 `save_if_version_zero_falls_back_to_cas_for_backfilled_row`（对应 Java 用例 `saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow`）；`cargo test -p agentscope-extensions-postgresql` → **111 passed / 0 failed** |
| 裁定 6 | advisory-lock key 由 `DefaultHasher`(SipHash-1-3) 改为 **MurmurHash3 x64_128（seed 104729）取 `hash[0]`**，与 Java `PostgresSandboxExecutionGuard#composeLockKey` 逐位一致 → 混部部署互斥恢复 | 新增 `src/murmur3.rs`；金标向量由真实 `commons-codec 1.15` 在 JVM 实跑生成（7 条输入 + 8/9/12/15/16/32 字节长度扫描），全部通过 |

**范围修正**：裁定 6 原写"5 个存储 crate 复用"，逐 crate 核对后**仅本 crate**使用 `DefaultHasher`。JDBC（SHA-256 前 8 字节）、MySQL（SHA-256 normalize）、MONGODB（SHA-256 hex）、REDIS（字符串 key）**本就与各自 Java 实现一致，未改动**。因此实现放在本 crate 内（唯一消费者），未改 `core`。

### 7.2 仍待办

- **P1-3** builder `create_if_not_exist` 默认值与 Java **相反**（Java `true` / Rust `false`）→ 按 Java 写法迁移的调用方会直接构建失败。**（2026-10-28 已关闭，见 §8）**
- **P1-4** 快照 `initSchema` 失败：Java 仅告警，Rust 上抛阻断构造路径。**（2026-10-28 已关闭，见 §8）**
- **P2**：53 个 Java 用例未覆盖、`executeInWriteTransaction` 事务原子性未迁移、advisory lock 单会话归属、`CasFailed`/`DriverUnavailable`/`to_core` 死代码。

### 7.3 实施踩坑（供后续参考）

`MurmurHash3.hash128(byte[])` ≠ `hash128x64(byte[])`：前者是 **seed 104729** 的重载，后者是 **seed 0**；二者输出不同。另 x64_128 常量 `C2` 应为 `0x4cf5ad432745937f`（误写成 `0x4cf5ad435947937f` 会被金标向量立即发现——本次即因此定位）。
## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 8. 2026-10-28 执行记录（关闭 P1-3 / P1-4）

### 8.1 改动文件
- `crates/agentscope-extensions-postgresql/src/state.rs`：`PostgresAgentStateStoreBuilder::new` 的 `create_if_not_exist` 默认值 `false` → **`true`**（对齐 Java `PostgresAgentStateStore.Builder.createIfNotExist = true`），并在 builder 文档中写明 Java 两个入口的差异（构造器 false / builder true）。
- `crates/agentscope-extensions-postgresql/src/snapshot.rs`：`PostgresRemoteSnapshotClient::with_table` 在 `initialize_schema=true` 时，DDL 失败由 `?` 上抛改为 `tracing::warn!(table, error = %e, ...)` 后继续（对齐 Java `initSchema()` 只 `log.warn`），并保留底层 message。

### 8.2 断言同步（旧期望被证伪，非放宽）
| 用例 | 原写法 | 现写法 | 理由 |
|---|---|---|---|
| `state::tests::build_without_auto_create_fails_when_schema_missing` | `builder(c).build()` 期望 `PostgresError::Schema` | 显式 `.create_if_not_exist(false)` | 原断言编码的是「builder 默认 false」这一**错误默认值**；用例语义对应 Java **构造器** `new PostgresAgentStateStore(ds)`（默认 false），改为显式关闭自动建表后断言强度不变 |
| `state::tests::build_without_auto_create_fails_when_table_missing` | 同上 | 同上 | 同上 |
| `state::tests::build_succeeds_when_schema_and_table_already_exist` | `builder(c).build()`（隐含走 INFORMATION_SCHEMA 校验） | 显式 `.create_if_not_exist(false)` | 该用例意在覆盖 `verify_schema_exists` / `verify_table_exists` 两条探针；默认改 `true` 后会走 DDL 分支而不触发探针，故显式关闭以保住覆盖目标 |
| `snapshot::tests::initialize_snapshot_table_is_idempotent` 的注释 | 注明「Rust 构造器上抛、差异未登记」 | 「构造器不再上抛，本用例固定显式 helper 的幂等路径」 | 行为已对齐，注释同步 |

无删除用例、无 `#[ignore]`、无断言放宽。

### 8.3 新增用例（先失败后通过）
| 用例 | 断言 |
|---|---|
| `state::tests::builder_defaults_to_create_if_not_exist` | 空库上 `builder(c).build()` 成功，`has_schema` / `has_table` 为真且 DDL 含 `CREATE TABLE IF NOT EXISTS` |
| `state::tests::builder_default_auto_create_applies_to_custom_schema` | 只改 schema/table 名（不显式开自动建表）也能建成 |
| `snapshot::tests::constructor_tolerates_schema_init_failure` | DDL 全失败的 fake client（`DdlFailClient`）下，`new` / `with_table` / `postgres_snapshot_spec` / `PostgresSnapshotSpec::new` 四条构造路径均 `Ok` 且 `build("snap-1")` 可用；非法表名仍 `Err` |
| `snapshot::tests::explicit_initialize_helper_still_reports_failures` | 显式 `initialize_snapshot_table` 仍上抛 `PostgresError::Query`（含底层 message），防「一刀切」回归 |

### 8.4 验证
- `cargo build -p agentscope-extensions-postgresql` → **EXIT=0**（仅 2 个既有 dead_code 警告）。
- `cargo test -p agentscope-extensions-postgresql` → **115 passed / 0 failed / 0 ignored**（EXIT=0；开工前基线 111 passed）。
- 反向验证 1：`create_if_not_exist` 默认值回退为 `false` → `cargo test -p agentscope-extensions-postgresql builder_default` = **2 failed**（`builder_defaults_to_create_if_not_exist`、`builder_default_auto_create_applies_to_custom_schema`），EXIT=101；恢复后转绿。
- 反向验证 2：`snapshot.rs` 的 warn 分支回退为 `map_err(Error::from)?` → `cargo test -p agentscope-extensions-postgresql snapshot::tests` = **1 failed**（`constructor_tolerates_schema_init_failure`，`snapshot.rs:325` panic），EXIT=101；恢复后转绿。

### 8.5 逐个对照 Java 的结论（未一刀切）
| 初始化点 | Java 失败处理 | Rust 处理 | 结论 |
|---|---|---|---|
| `PostgresRemoteSnapshotClient.initSchema()` | `catch (SQLException) { log.warn }`（`:61-78`） | `tracing::warn!` + 继续 | **本次改为一致** |
| `PostgresAgentStateStore.createSchemaIfNotExist` / `createTableIfNotExist` / `ensureVersionColumn` | `throw RuntimeException`（`:155-186` / `:137-153`） | `?` 上抛 | 本就一致，未改 |
| `PostgresBaseStore.initializeSchema()` | `throw IllegalStateException`（`:175-188`） | `?` 上抛 | 本就一致，未改 |
| builder 默认值：`PostgresBaseStore.Builder.initializeSchema` | `false`（`:392`） | `false` | 一致，未改 |
| builder 默认值：`PostgresSandboxExecutionGuard.Builder` | 无此类字段 | 无 | 不适用 |

### 8.6 未完成 / 需裁定
- P2 维持不变：53 个 Java 用例未覆盖、`executeInWriteTransaction` 事务原子性未迁移、advisory lock 单会话归属、`CasFailed` / `DriverUnavailable` / `to_core` 死代码、`memory::MemoryClient` 未门控、组合者形态偏差、`tracing` 埋点仍缺（guard 7 处 / store 1 处 Java 日志点）。
- 需裁定：`PostgresSnapshotSpec::new` 无法感知 DDL 失败（Java 同样无法感知）；若上层要求「建表失败即启动失败」，需显式调用本 crate 的 `initialize_snapshot_table`。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `distributed_store()` 改为 builder 写法并接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `Arc::new(self.agent_state_store()?)` | `create_if_not_exist(true)` |
| `isolation_scope` | `IsolationScope` | 未设置（builder 默认） | 由调用方在 `HarnessAgent` 侧覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `Arc::new(self.base_store()?)`（`PostgresStore`） | 建表失败会以 `PgResult` 上抛 |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc::new(self.snapshot_spec()?)`（`PostgresSnapshotSpec`） | 包装 harness `RemoteSnapshotSpec` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.sandbox_execution_guard())` | advisory lock；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed.rs::distributed_store()`：填满 5 槽（builder，不用 `..Default::default()`）。
- `src/guard.rs`：新增私有字段 `trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<PostgresSandboxLease>>>>` + `impl SandboxGuard`。
  - `try_enter` 超时降级为 `SandboxLease::noop()`（与 JDBC guard 一致）。
  - `is_held`：PG guard 固有 API 无 `is_held`（advisory lock 是会话级、无查询接口），trait 侧返回**本 guard 是否仍持有该 key 的租约**（已在源码注明，属薄适配差异）。
  - `release`：优先 drop 具体租约（`Drop` 内 `pg_advisory_unlock`）；无租约时回退按 `compose_lock_key(key)` 解锁。
- 测试：`src/distributed.rs::distributed_store_projects_all_components`（原 `..._projects_agent_state_store_only` 已替换并加强断言）。

### 验证

`cargo test -p agentscope-extensions-postgresql` → **115 passed / 0 failed**。

### 未完成 / 语义差异

- 无留空槽位；`is_held` 为本地视图（非跨进程查询），与 Java 无对应方法（Java 侧也没有 `isHeld`）。
