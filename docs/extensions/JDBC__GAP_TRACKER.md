# JDBC 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **JDBC** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-jdbc\`
> **Java 子模块**：（单模块：`root` + `dialect`（子包 `table` / `vendor`）/ `store` / `state` / `snapshot` / `sandbox`）
> **Rust 目标**：`crates/agentscope-extensions-jdbc/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-jdbc`（单 Maven 模块） |
| Java 子模块 | 单模块：`root`（`JdbcDistributedStore`）+ 包 `dialect`（子包 `table` / `vendor`）/ `store` / `state` / `snapshot` / `sandbox` |
| Java 规模（实测） | 主代码 **3106 行**（20 个 `.java`）/ 测试 **1558 行**（11 个 `.java`；其中 10 个测试类共 **98** 个 `@Test`，1 个 `H2TestSupport` 支撑类） |
| Rust crate | `crates/agentscope-extensions-jdbc` |
| 迁移波次 | B |
| 当前状态 | **部分完成**（`cargo build` exit 0；`cargo test` = **146 passed / 0 failed**，含 lib 2 + 7 个集成测试文件；**本轮关闭两项 P1 功能缺口**：MySQL/PG 原生 advisory lock + state 版本化 API。剩余为环境受限的集成测试与跨语言不兼容项，见 §3.1/§3.2） |

### 1.1 基线（开工/完工实测）
| 项 | 开工基线 | 完工基线 |
|---|---|---|
| `cargo build -p agentscope-extensions-jdbc` | exit 0（9 warning） | exit 0（**7 warning**，均为既有 dead_code 辅助函数） |
| `cargo test -p agentscope-extensions-jdbc` | **2 passed / 0 failed**（仅 lib）+ doc-tests 0 | **146 passed / 0 failed** + doc-tests 0 |
| test target 数 | 1（lib） | 8（lib + `tests/` 下 7 个） |
| `crate` 内 `tests/` 目录 | 无 | `tests/{dialect_sql,store,state,snapshot,sandbox,native_lock,distributed_store}_tests.rs` |
| Java 测试类 `@Test` 总数 | 98（10 测试类 + `H2TestSupport`） | 98（未变） |
| 本模块离线可覆盖的 Java `@Test` | — | 62 条 1:1 命名对齐（见 §3.1 逐类映射） |

## 2. Java 清单与依赖映射

| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `jdbc.JdbcDistributedStore` | 组合 4 组件为 harness `DistributedStore`；`create(DataSource)` SPI 自动检测方言 | `distributed_store::JdbcDistributedStore`（`src/distributed_store.rs`） | 持有 `SharedExecutor` + `Arc<dyn AbstractJdbcDialect>` + 4 组件 + `IsolationScope`；`create(executor, product_name)` / `create_with_dialect(_and_schema)`；`to_harness_store()` 仅投影 agent-state-store 半边（harness `DistributedStore` 只有 `agent_state_store`+`isolation_scope`，见 §3.4、§4.2）。Java 组件懒加载（volatile 缓存），Rust 在构造函数内一次性构建 |
| `jdbc.dialect.BoundSql` | 已装配 SQL + 绑定参数（record） | `dialect::BoundSql` + `dialect::SqlParam`（`src/dialect/bound_sql.rs`） | Java `List<Object>` ↔ Rust `Vec<SqlParam>`（`Text`/`Int`/`Blob`/`Null`） |
| `jdbc.dialect.SandboxLockStrategy` | 分布式锁契约（interface） | `dialect::SandboxLockStrategy` + `LockLease` + `LockOutcome`（`src/dialect/lock_strategy.rs`） | Rust 返回 `Result<LockLease, LockOutcome>`；`LockLease` 以闭包实现 `Drop` 释放（Java 用 `SandboxLease.close()`） |
| `jdbc.dialect.AbstractJdbcDialect` | 聚合方言（abstract class；实现 3 个 table 域接口 + 锁契约；表名解析、锁表 DDL、`tryEnter` 默认表锁） | `dialect::AbstractJdbcDialect`（trait，`src/dialect/abstract_dialect.rs`） | 含 `DEFAULT_TABLE_PREFIX="agentscope_"`、`validate_identifier`、`DialectKind`、`lock_create_table_ddl`、`resolved_*_table_name`、`supports_native_locks()`（默认 false）。Java `from(DataSource)` SPI 检测 → Rust 无 |
| `jdbc.dialect.AbstractJdbcDialectBuilder` | 链式配置 + `build()`；SPI 候选（按 `getOrder()`/继承深度排序） | `dialect::JdbcDialectBuilder` + `dialect::detect_dialect`（`src/dialect/detection.rs`） | Rust 以「产品名字符串包含匹配」替代 JDK `ServiceLoader`；builder 记录 prefix/表名覆盖/`auto_create_table`，`build()` 仅返回方言（建表由各组件负责） |
| `jdbc.dialect.table.StoreDialect` | KV 表方言（DDL 抽象 + ANSI 默认 SQL） | `dialect::StoreDialect`（`src/dialect/store_dialect.rs`） | SQL 模板逐条对齐（insert/cas_update/select/delete/search/upsert） |
| `jdbc.dialect.table.SessionStateDialect` | 会话状态表方言 | `dialect::SessionStateDialect`（`src/dialect/session_state_dialect.rs`） | Rust 用 `slot_id` 列编码 `user:session`；**Java 列名为 `session_id`**（见 §3.3） |
| `jdbc.dialect.table.SnapshotDialect` | 快照表方言 | `dialect::SnapshotDialect`（`src/dialect/snapshot_dialect.rs`） | — |
| `jdbc.dialect.vendor.MysqlDialect` | MySQL 方言（`ON DUPLICATE KEY UPDATE`、`LONGBLOB`、原生 `GET_LOCK`） | `dialect::vendor::MysqlDialect`（`src/dialect/vendor/mysql.rs`）+ `dialect::native_lock::MysqlNativeLockStrategy` | DDL/UPSERT 已迁移；**原生 `GET_LOCK` 已迁移**（锁名为连接级会话作用域，故独立成策略类型而非挂在方言上，见 §3.2） |
| `jdbc.dialect.vendor.PostgresDialect` | PG 方言（`ON CONFLICT DO UPDATE`、`BYTEA`、`pg_try_advisory_lock`） | `dialect::vendor::PostgresDialect`（`src/dialect/vendor/postgres.rs`）+ `dialect::native_lock::PostgresNativeLockStrategy` | DDL/UPSERT 已迁移；**原生 advisory lock 已迁移**（同上） |
| `jdbc.dialect.vendor.SqliteDialect` | SQLite 方言 | `dialect::vendor::SqliteDialect`（`src/dialect/vendor/sqlite.rs`） | — |
| `jdbc.dialect.vendor.H2Dialect` | H2 方言 | 无独立类型；由 `dialect::vendor::AnsiDialect` 承担（`detect_dialect` 对未知/`h2` 默认返回 Ansi） | 见 §3.4 |
| `jdbc.dialect.package-info` / `dialect.table.package-info` / `dialect.vendor.package-info` | 包文档（无逻辑） | 无 | — |
| `META-INF/services/io.agentscope.extensions.jdbc.dialect.AbstractJdbcDialect` | JDK SPI 方言注册表 | 无 | Rust 无 SPI 自动注册（见 §3.3） |
| `jdbc.store.JdbcStore` | JDBC `BaseStore`（零内联 SQL） | `store::JdbcStore`（`src/store/jdbc_store.rs`，`impl agentscope_harness::filesystem::BaseStore`）+ `store::store_value` | `get`/`put`/`put_if_version`/`search`/`delete` 逐一对齐；CAS：`expected==0`→`store_insert`（唯一键冲突=失败），`>0`→`store_cas_update` 影响行数==1 |
| `jdbc.store.JdbcStore.Builder` | 组件构建器 | `JdbcStore::new`/`with_schema`（无独立 Builder 类型） | Java 可注入 `dialect`/`ObjectMapper`/`initializeSchema`；Rust 固定 serde_json |
| `jdbc.state.JdbcAgentStateStore` | JDBC `AgentStateStore`（单值 + 增量列表 + `:_hash`） | `state::JdbcAgentStateStore`（`src/state/jdbc_agent_state_store.rs`，`impl agentscope_core::state::AgentStateStore`） | 覆盖 `save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`；列表哈希复用 `agentscope_core::state::list_hash`；**版本化 API 已以固有方法公开**（`save_if_version`/`get_versioned`/`read_version`/`supports_versioning` + `VersionedState`/`UNVERSIONED`，见 §3.2） |
| `jdbc.snapshot.JdbcRemoteSnapshotClient` | BLOB 上传/下载/存在性 | `snapshot::JdbcRemoteSnapshotClient`（`src/snapshot/jdbc_remote_snapshot_client.rs`，`impl RemoteSnapshotClient`） | 表默认 `agentscope_snapshots`；`upload` 用方言 `snapshot_upsert` |
| `jdbc.snapshot.JdbcSnapshotSpec` | 快照 spec 便捷类（extends `RemoteSnapshotSpec`） | `snapshot::JdbcSnapshotSpec`（`src/snapshot/jdbc_snapshot_spec.rs`，newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`） | 快照 id 校验沿用 harness |
| `jdbc.sandbox.JdbcSandboxExecutionGuard` | JDBC `SandboxExecutionGuard`，委托方言锁策略 | `sandbox::JdbcSandboxExecutionGuard`（`src/sandbox/jdbc_guard.rs`） | harness guard 为 **struct 非 trait**，故为固有方法 `try_enter`/`release`/`is_held`（见 §3.4） |
| `jdbc.sandbox.JdbcSandboxExecutionGuard.Builder` + 内部 lease | 构建器与租约 | `JdbcSandboxExecutionGuard::{new,table_lock,with_key_prefix,with_timeout_seconds}` + `sandbox::TableLockStrategy` + `dialect::LockLease` | 常量对齐：`DEFAULT_KEY_PREFIX="agentscope:sandbox:lock:"`、`DEFAULT_LOCK_TIMEOUT_SECONDS=1800` |
| `javax.sql.DataSource` / JDBC（组件入参） | 连接与 SQL 执行 | **Rust 独有**：`backend::JdbcExecutor` trait + `backend::RusqliteExecutor`（`src/backend/{mod,sqlite}.rs`）+ `ColumnValue`/`JdbcError`/`SharedExecutor`/`is_unique_violation` | Java 无对应类；Rust 以 executor seam 替代 DataSource（见 §2.1） |
| （测试支撑）`H2TestSupport` + 10 个 `*Test`（H2/SQLite/Testcontainers） | 离线/集成测试 | `RusqliteExecutor::in_memory()`（测试后端） | Rust 侧已建 7 个 `tests/*.rs` 离线用例（146 passed），详见 §3.1 逐类映射 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `javax.sql.DataSource` + JDBC `Connection`/`PreparedStatement`/`ResultSet` | 连接、绑定、读取 | `backend::JdbcExecutor`（`execute`/`query`/`execute_batch`/`execute_prepared_batch`）+ `ColumnValue` | Rust 无通用 JDBC 面；组件全部经 executor seam，保持「零内联 SQL」 |
| `org.xerial:sqlite-jdbc` 3.47.1.0（optional） | SQLite 驱动 | `rusqlite`（`Cargo.toml`：`rusqlite = { workspace = true }`） | 版本由 workspace `[workspace.dependencies]` 固定（本 crate 未改根 Cargo.toml）；`RusqliteExecutor` 为其默认实现 |
| `com.h2database:h2` 2.4.240（test） | H2 内存测试库 | 无 | Rust 用 `rusqlite` 内存库 + `AnsiDialect`；H2 语义由 Ansi 近似（见 §3.4） |
| `com.fasterxml.jackson.databind.ObjectMapper` | JSON 序列化 | `serde` + `serde_json`（`StoreValue = serde_json::Map<String, serde_json::Value>`） | 固定使用，无注入点 |
| `org.slf4j.Logger` | 日志 | `tracing` | 现有少量 `tracing::warn/debug/trace`（guard/state/store） |
| `java.util.ServiceLoader`（SPI）+ `getOrder()`/`supports()` | 方言自动发现 | `dialect::detect_dialect(product_name)`（子串匹配） | Rust 无运行时 SPI（见 §3.3） |
| `io.agentscope.core.state.AgentStateStore` | 会话状态契约 | `agentscope_core::state::AgentStateStore`（本 crate `impl`） | 复用既有 trait，未造平行类型 |
| `io.agentscope.core.state.ListHashUtil` | 列表变更检测 | `agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite}` | 复用 core 实现 |
| `io.agentscope.harness...remote.store.BaseStore` / `StoreItem` | KV 契约 | `agentscope_harness::filesystem::{BaseStore, StoreItem, StoreValue}`（本 crate `impl`） | `put_if_version` CAS 语义以 harness 为准 |
| `io.agentscope.harness...snapshot.RemoteSnapshotClient` / `RemoteSnapshotSpec` / `SandboxSnapshotSpec` | 快照契约 | `agentscope_harness::agent::sandbox::snapshot::{RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshotSpec}` | 快照 id 校验以 harness 为准 |
| `io.agentscope.harness...sandbox.SandboxExecutionGuard` / `SandboxIsolationKey` / `SandboxLease` | 沙箱护栏与租约 | `agentscope_harness::agent::sandbox::{SandboxExecutionGuard, SandboxIsolationKey, SandboxLease}` | guard 为 struct，本 crate 无法 `impl`（见 §3.4） |
| `io.agentscope.harness.agent.DistributedStore` | 分布式存储组合 | `agentscope_harness::agent::distributed_store::DistributedStore`（字段仅 `agent_state_store` + `isolation_scope`） | 见 §4.2 |
| `java.sql.SQLIntegrityConstraintViolationException` | 唯一键冲突判定 | `backend::is_unique_violation(&JdbcError)` | — |
| `InputStream` + `PreparedStatement#setBinaryStream` | 流式快照上传 | `&[u8]`（`RemoteSnapshotClient::upload`） | 非流式（整块内存差异，见 §3.2） |
| JUnit5 / Testcontainers 1.21.3 | 测试 | `#[cfg(test)]`（`src/lib.rs` 2 个）+ `tests/*.rs` 6 个集成文件（含 `#[tokio::test]`） | 见 §3.1；未新增 dev-dependencies（`tokio` 已在 `[dependencies]`） |

> **harness/core 复用契约**（对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1，不新建公共 store crate）：本 crate `impl` `agentscope_core::state::AgentStateStore`（`state/jdbc_agent_state_store.rs:224`）、`agentscope_harness::filesystem::BaseStore`（`store/jdbc_store.rs:113`）、`agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot/jdbc_remote_snapshot_client.rs:58`）、`SandboxSnapshotSpec`（`snapshot/jdbc_snapshot_spec.rs:53`）；`put_if_version` CAS、`StoreItem.version`、快照 id 校验均以 harness 为准。harness `DistributedStore` 仅取 `agent_state_store` + `isolation_scope`。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 导出 | `JdbcDistributedStore` | 已完成 | 声明 `backend`/`dialect`/`distributed_store`/`sandbox`/`snapshot`/`state`/`store` 模块并 re-export 全部公共类型 |
| 执行后端 seam（替代 `DataSource`） | 无（Java 直接用 JDBC） | 已完成 | `backend::{JdbcExecutor, RusqliteExecutor, ColumnValue, JdbcError, SharedExecutor, is_unique_violation}` |
| `BoundSql` + `SqlParam` | `dialect/BoundSql.java` | 已完成 | — |
| 3 个 table 域方言 trait | `dialect/table/*.java` | 已完成 | `StoreDialect`/`SessionStateDialect`/`SnapshotDialect` |
| 聚合方言 trait + 表名解析 + 标识符校验 + 锁表 DDL | `dialect/AbstractJdbcDialect.java` | 已完成 | `AbstractJdbcDialect`（trait）、`validate_identifier`、`DEFAULT_TABLE_PREFIX="agentscope_"` |
| 方言检测 + 链式 builder | `dialect/AbstractJdbcDialectBuilder.java` | 已完成 | `detect_dialect` 用产品名子串匹配（替代 SPI）；**本轮补齐**：新增 `TableNameConfig`/`TableNameConfigurable`（`AbstractJdbcDialect` 超级 trait），`JdbcDialectBuilder.build()` 现将 prefix/三张表 override 应用到方言实例（与 Java `Builder.build()` 同语义），新增 4 条用例覆盖 `custom_prefix`/`per_table_override`/`snapshot_override` |
| 4 个厂商方言（MySQL/PG/SQLite/H2） | `dialect/vendor/*.java` | 已完成（DDL/UPSERT 层面） | MySQL/PG/SQLite 的 DDL/UPSERT/check-exists 已逐条对齐 Java（含 `ENGINE=InnoDB`/`utf8mb4`、`VARCHAR(2048)`/`CLOB`、`created_at` 列、独立 `CREATE INDEX IF NOT EXISTS`）；**H2 合并进 `AnsiDialect`**；**MySQL/PG 原生锁已迁移**（`dialect/native_lock.rs` 两个策略，未挂在方言类型上：锁为连接级会话作用域，见 §3.2） |
| `JdbcStore`（`impl BaseStore`）+ CAS | `store/JdbcStore.java` | 已完成 | 5 个 trait 方法全实现；`put_if_version` 严格对齐 harness CAS |
| `JdbcAgentStateStore`（`impl AgentStateStore`） | `state/JdbcAgentStateStore.java` | 部分完成 | 7 个 trait 方法全实现 + `create_table_if_not_exist`/`verify_table_exists`；**版本化 API 已以固有方法公开**（§3.2） |
| `JdbcRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | `snapshot/JdbcRemoteSnapshotClient.java` | 已完成 | upload/download/exists |
| `JdbcSnapshotSpec`（`impl SandboxSnapshotSpec`） | `snapshot/JdbcSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec` |
| `JdbcSandboxExecutionGuard` + 表锁策略 | `sandbox/JdbcSandboxExecutionGuard.java` | 已完成 | 仅 `TableLockStrategy`；**本轮修正**：`insert_lock` 与原 `lock_create_table_ddl` 列不一致（DDL 建 `lock_name PK` 但 INSERT 写 `(lock_name, acquired_at)`）→ 统一为 Java 的 `(lock_name) VALUES (?)`；`release`/`is_held` 已是真实探针（`is_held` 查锁表，非恒 false） |
| `JdbcDistributedStore` 组合者 | `JdbcDistributedStore.java` | 已完成 | 4 组件访问器 + `to_harness_store()`（仅投影半量，§4.2） |
| 无真实 DB 的内存后端 | Java H2/SQLite | 已完成 | `RusqliteExecutor::in_memory()` |
| **离线单元测试** | 10 个测试类，共 **98** 个 `@Test`（H2 为主 + SQLite + 2 个 Testcontainers 集成） | **已完成（离线可覆盖部分）** | 现状：`cargo test -p agentscope-extensions-jdbc` = **146 passed / 0 failed**（lib 2 + 7 个集成测试文件）。逐类映射（Rust target ← Java 类 / 用例名已写入每条测试注释）：<br>• `tests/dialect_sql_tests.rs`（**42**）← `DialectSqlTests`（21/21 全覆盖，`H2Dialect`→`AnsiDialect`）+ `AbstractJdbcDialectFromTest`（6/8：`detectsPostgres/Mysql/MariaDb/H2/Sqlite` + 不支持库降级；`throwsOnConnectionFailure`/`throwsForNullDataSource` 对应 Rust 无 `DataSource` 的连接面，见 §3.4）。<br>• `tests/store_tests.rs`（**14**）← `JdbcStoreH2Test` 10/10 全覆盖 + 4 条 CAS 三分支/命名空间编码加固。<br>• `tests/state_tests.rs`（**31**）← `JdbcAgentStateStoreH2Test` 非版本化 13/13 全覆盖 + **10 条版本化用例**（`supportsVersioning`/`getVersioned`/`saveIfVersion` 三分支 + 冲突不写 + 隔离/校验）+ 8 条隔离/校验/`:_hash` 加固。版本化用例打在 `JdbcAgentStateStore` 的**固有方法**上（core trait 无版本化面，见 §3.2/§4.2）。<br>• `tests/snapshot_tests.rs`（**10**）← `JdbcRemoteSnapshotClientH2Test` 6/6 全覆盖 + `JdbcSnapshotSpec` 4 条（含 `getClient` 往返、id 校验）。<br>• `tests/sandbox_tests.rs`（**13**）← `JdbcSandboxExecutionGuardH2Test` 4/4 + `JdbcSandboxExecutionGuardSqliteTest` 2/2 + `DEFAULT_*` 常量/`with_key_prefix`/`release`/`is_held` 7 条（便携表锁路径）。<br>• `tests/native_lock_tests.rs`（**21**）← `MysqlDialect.tryEnter`/`PostgresDialect.tryEnter` 行为对齐：MySQL 锁名归一（短名/恰 64 字符/超长截断+摘要、长名互异）、PG advisory key 折叠、两策略 acquire/release/is_held/drop、超时与后端失败不 panic、guard 接线。SHA-256 期望值用独立字面量 + dev-dep 重算双向交叉验证。<br>• `tests/distributed_store_tests.rs`（**13**）← `JdbcDistributedStoreH2Test` 5/5 + 组件工厂/`to_harness_store()` 投影/共享句柄/schema 关闭 8 条。<br>未覆盖（环境受限，非缺口）：`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条，Testcontainers + Docker）；`DialectSqlTests` 中依赖真实 H2 `MERGE INTO` 执行语义的部分。补测已全部离线（`RusqliteExecutor::in_memory()` + `SqliteDialect`），无新依赖、无网络。 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| MySQL 原生 `GET_LOCK`/`RELEASE_LOCK` 锁 | `dialect/vendor/MysqlDialect.java:181-238` | **已完成** | `dialect/native_lock.rs` 的 `MysqlNativeLockStrategy`。**未**在方言类型上 `impl SandboxLockStrategy`：`GET_LOCK` 是会话级锁，Java 靠每 lease 独占 Connection + `close()` 释放，而方言不持有连接（见该模块头注释）。锁名归一已移植：超 64 字符则截为 31 字符 + `:` + SHA-256 前 32 hex（防高基数碰撞，Java 注释明确弃用 32-bit `String.hashCode`）。路由缝 `NativeLockSession` + SQL 常量 `MYSQL_GET_LOCK_SQL`/`MYSQL_RELEASE_LOCK_SQL` |
| PostgreSQL 原生 `pg_try_advisory_lock`/`pg_advisory_unlock` | `dialect/vendor/PostgresDialect.java:182-252` | **已完成** | 同 `dialect/native_lock.rs` 的 `PostgresNativeLockStrategy`；`postgres_lock_key` 折叠 SHA-256 前 8 字节（与 Java 一致，碰撞 ~2^-32）。与 Java 差异：Java 在策略内轮询至 deadline，Rust 交给 `NativeLockSession::try_acquire(timeout)`；`NATIVE_LOCK_POLL_INTERVAL=100ms` 已导出供真实适配层。后端失败 → `LockOutcome::TimedOut`（guard 降为 no-op lease，不 panic，见 §3.4） |
| state 版本化写入/读取（`saveIfVersion`/`readVersion`/`supportsVersioning`） | `state/JdbcAgentStateStore.java:203,231,252` | **已完成** | `JdbcAgentStateStore` 新增固有方法 `supports_versioning()`/`read_version()`/`get_versioned()`/`save_if_version()` + `VersionedState`/`UNVERSIONED`（已导出）。**注意：** core `AgentStateStore` trait 无版本化面，故这些**只能**以固有方法暴露（跨模块待裁定，见 §4.2） |
| `saveIfVersion(UNVERSIONED)` 写后读原子性 | `state/JdbcAgentStateStore.java:203` | **已完成（2026-10-20）** | 原实现 `save_single()` + 独立 `read_version_at()`，并发下返回的版本可能属于别人的写入。现走 `upsert_returning_version`：方言 `session_state_upsert_returning_version` 用 `RETURNING version`（PG/SQLite）单语句完成；**MySQL/H2 无 `RETURNING`，降级为 UPSERT + 独立 `SELECT version`，代码注释与测试均明确标注为「已知非原子退化」**（版本仍单调、始终反映已提交数据，但不能断言它标识本次 payload）。用例：`state_tests.rs` |
| `saveIfVersion(0)` 幽灵 CAS 自愈 | `state/JdbcAgentStateStore.java:231` | **已完成（2026-10-20）** | 历史行可能停在 DDL 默认 `version = 0`（`session_state_insert` 省略 version 列，只有 list 元素走它），这类行 insert-if-absent 永远主键冲突 → 曾经永久不可写。现 `expected_version == 0 && affected != 1` 时执行 `cas_zero_version`：以 `WHERE version = 0` 再做一次 CAS，影响 1 行返回 1，否则仍返回 `UNVERSIONED`。**未改 DDL 默认值、无需数据迁移**；反向用例保证「version=3 时 save_if_version(0) 仍冲突且不写」，防止自愈退化成无条件覆盖 |
| MySQL 键列二进制排序（session/snapshot 表） | `2349533a1` | **已完成（2026-10-20）** | `dialect/vendor/mysql.rs`：会话状态表 `session_id`/`state_key`、快照表 `snapshot_id` 均显式 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`（表级默认保持 `utf8mb4_unicode_ci`，只影响非键列）。DDL 断言见 `tests/dialect_sql_tests.rs`。**存量库仍需手工 ALTER**，见 `docs/migrations/mysql_binary_collation.md` §5 |
| 快照上传流式绑定（避免整包入内存） | `snapshot/JdbcRemoteSnapshotClient.java`（`setBinaryStream`） | 待确认 | Rust `RemoteSnapshotClient::upload(&[u8])` 需整包驻留内存；大快照场景存在内存差异 |
| 真实 MySQL/PostgreSQL 集成测试 | `integration/MysqlIntegrationTest.java`、`integration/PostgresIntegrationTest.java`（Testcontainers） | 待办（环境受限） | Rust 仍无 mysql/pg 驱动与 Docker 集成；本轮新增的 `tests/` 仅覆盖离线 SQLite 路径。对应 10 条 Java `@Test` 映射见 §3.1 |
| 列表增量语义（`getListCount`/`likePrefixPattern`） | `state/JdbcAgentStateStore.java:414,481` | 已完成 | Rust `list_count`/`like_prefix_pattern` 已实现；本轮修正 `like_prefix_pattern` 使用方言 `*_like_escape_char()`（由 `'\\'` 改为与 Java 一致的 `'!'`），并新增增量追加/哈希伴生行用例 |
| 表存在性校验 | `state/JdbcAgentStateStore.java:101` | 已完成 | Rust `verify_table_exists` 使用方言 `session_state_check_table_exists`；**本轮修正根因**：原实现只用 `query().map(|_| ())` 视为成功，空结果集（表不存在时 SELECT 正常返回 0 行）不报错 → 改为「空结果集即抛错」，与 Java `if (!rs.next())` 一致。新增用例 `constructor_with_create_if_not_exist_false_throws_when_table_missing` |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 会话表列名差异（跨语言同库不兼容） | `dialect/table/SessionStateDialect.java`（`session_id`） | 已完成（已对齐） | Rust 列名已为 `session_id`（与 Java 一致）；仅 Rust 函数形参仍名为 `slot_id`（内部存储 `user:session` 的合成值），不影响表结构。**不再存在跨语言表结构不兼容**（本条旧记录已失效） |
| 锁表名差异（跨语言不兼容） | `dialect/AbstractJdbcDialect.java:253`（`tablePrefix + "distributed_locks"`） | 已完成（已对齐） | Rust `resolved_lock_table_name()` = `{prefix}distributed_locks`（`abstract_dialect.rs:187`），与 Java 一致；锁表 DDL 也已对齐为 `(lock_name VARCHAR(255) NOT NULL PRIMARY KEY)`。**本条旧记录（`{prefix}lock`）已失效** |
| JDK SPI 方言自动注册/发现 | `AbstractJdbcDialect.from(DataSource)` + `META-INF/services/...` | 待办 | Rust 用产品名子串匹配，第三方方言无法自动注册；扩展需改 `detect_dialect` |
| 组件懒加载缓存 | `JdbcDistributedStore` volatile 字段 | 待确认 | Rust 在构造函数中一次性构建全部组件（含 DDL），语义等价但非懒加载 |
| `tracing` 埋点对齐 SLF4J | 各方言/组件日志点 | 待办 | Rust 仅少量日志（guard/state/store） |
| dead_code 辅助函数 | — | 待确认 | `TableLockStrategy::delete_lock`、`read_version`、`namespace_param` 等当前无调用方；保留待接线，长期不用可评估删除 |
| `distributed_store` 模块可见性 | — | 待确认 | `lib.rs` 中为 `mod distributed_store;`（私有模块，仅 re-export 类型），与其余 `pub mod` 风格不一致，非阻塞 |
| 建表后表结构校验 | `dialect/TableSchemaValidator.java`（`9c8b10d4e`） | **已完成（2026-10-20）** | 新增 `src/dialect/table_schema.rs`：`parse_create_table_columns`（从 `CREATE TABLE` 列定义提取列名，跳过 `PRIMARY`/`UNIQUE`/`CONSTRAINT`/`FOREIGN`/`CHECK`/`KEY`/`INDEX`，支持嵌套括号与引号）；`validate_table_schema` 在**缺表/缺列**时 fail-fast，错误含表名、缺失列、实际列、参考 DDL，并声明「框架不会 ALTER 已有表」；表名先过 `[A-Za-z_][A-Za-z0-9_]*` 校验。方言新增 `table_columns_query`：ANSI/H2/PG/MySQL 走 `information_schema.columns`（PG 限 `current_schema()`，MySQL 限 `DATABASE()`），SQLite 走 `pragma_table_info(?)`。接入 `JdbcStore::initialize_schema` / `JdbcAgentStateStore::create_table_if_not_exist` / `JdbcRemoteSnapshotClient::init_schema`。**未做**：Java 把 `createTableDdls()` 改为 `Map<table, List<ddl>>` 的分组重构 —— Rust 保留 `Vec<String>`，避免 33 处调用点破坏性改动（校验只需第一个可解析的 `CREATE TABLE`）。测试：`tests/table_schema_validation_tests.rs` 7 项 |

### 3.4 设计决定（不迁移）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `SandboxExecutionGuard` 的 `impl` | `JdbcSandboxExecutionGuard implements SandboxExecutionGuard` | 已完成（设计决定，不迁移） | harness `SandboxExecutionGuard` 是 **struct 非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），无法 `impl`；改为镜像 API 的固有方法 `try_enter`/`release`/`is_held`（已由 `tests/sandbox_tests.rs` 覆盖）。依据：trait 无法为 struct 反向实现，且 harness 为本模块禁改区 |
| `DataSource` 连接所有权 / `getDataSource()` / `close()` | `JdbcDistributedStore`、各组件 | 已完成（设计决定，不迁移） | Rust 以 `SharedExecutor = Arc<dyn JdbcExecutor>` 共享，无连接所有权/关闭语义，也无 `bindDataSource`/`closeConnection` 辅助。已由 `tests/distributed_store_tests.rs::components_share_the_same_executor_and_dialect` 覆盖。依据：Rust 无 `javax.sql.DataSource`（§2.1） |
| `ObjectMapper` 注入 | `store/JdbcStore.java`（`Builder.objectMapper`） | 已完成（设计决定，不迁移） | Rust 固定 `serde_json`，无注入点。已由 `store_tests` 的往返用例覆盖。依据：`StoreValue` 固定为 `serde_json::Map`（§2.1） |
| `H2Dialect` 独立类 | `dialect/vendor/H2Dialect.java` | 已完成（设计决定，不迁移） | Rust 无 H2 驱动；`detect_dialect` 对 `h2`/未知默认返回 `AnsiDialect`，**且 `AnsiDialect` 已承载 H2 的 `MERGE INTO`/`CLOB` DDL 语义**。已由 `dialect_sql_tests::{h2_store_upsert_uses_merge_into,h2_store_ddl_uses_clob,h2_state_upsert_and_check,h2_snapshot_upsert_updates_created_at,detects_h2_as_ansi}` 覆盖。依据：无 `com.h2database:h2` 等价物，Ansi 即 H2 兼容面 |
| `StaticTokenProvider` 等无参构造 | — | 已完成（设计决定，不迁移） | 同第 1 行：Rust 组件均需显式注入 `executor + dialect`，无无参构造 |
| JSON 工具 `JsonUtils` | `io.agentscope.core.util.JsonUtils` | 已完成（设计决定，不迁移） | Rust 直接用 `serde_json`。依据：core 无等价 `JsonCodec` 注入点 |
| Java 包文档类（`package-info.java`） | `dialect/`、`dialect/table/`、`dialect/vendor/` | 已完成（设计决定，不迁移） | doc-only，Rust 用模块级 `//!` 文档代替 |
| JDK SPI 方言自动注册/发现 | `AbstractJdbcDialect.from(DataSource)` + `META-INF/services/...` | 已完成（设计决定，不迁移） | Rust 无 `ServiceLoader`；改用产品名子串匹配的 `detect_dialect`（`detection.rs`），未知/`h2` 降级为 `AnsiDialect`。已由 `dialect_sql_tests` 检测用例组覆盖。依据：Rust 无运行时类路径扫描，且 Java `from(null)`/连接失败分支（`throwsForNullDataSource`/`throwsOnConnectionFailure`）随 `DataSource` 一并消失 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **本会话主题（2026-10-08）：关闭两项 P1 功能缺口——MySQL/PG 原生 advisory lock 与 state 版本化 API——并补齐其离线测试。** `cargo build -p agentscope-extensions-jdbc` 与 `cargo test -p agentscope-extensions-jdbc` 均通过。
- **上一会话主题（保留记录）：补齐离线测试覆盖（原 P0 缺口）+ 修复测试暴露的实现 bug。**
- 验证命令与结果（实测）：
  - `cargo build -p agentscope-extensions-jdbc` → `Finished dev profile`，**退出码 0**（**crate 自身 7 warning**，均为既有 dead_code 辅助函数（`query_optional_int`/`slot_param`）+ 测试/快照等既有未用导入；无 error。较上一会话 8 → 7：原死代码 `read_version` 已接线为公开 `read_version`/`get_versioned`）。
  - `cargo test -p agentscope-extensions-jdbc` → **退出码 0**；各 target 结果：`unittests src/lib.rs` = **2 passed / 0 failed**；`tests/dialect_sql_tests.rs` = **42 / 0**；`tests/distributed_store_tests.rs` = **13 / 0**；`tests/sandbox_tests.rs` = **13 / 0**；`tests/native_lock_tests.rs` = **21 / 0**；`tests/snapshot_tests.rs` = **10 / 0**；`tests/state_tests.rs` = **31 / 0**；`tests/store_tests.rs` = **14 / 0**；doc-tests = **0**。合计 **146 passed / 0 failed**（上一会话 115 → 本会话 146，新增 31；8 个 test target）。
- 本会话新增 `tests/`（全部离线，`RusqliteExecutor::in_memory()` + `SqliteDialect`/`AnsiDialect`/`MysqlDialect`/`PostgresDialect`；无网络、无新依赖、无 `#[ignore]`）：
  - `crates/agentscope-extensions-jdbc/tests/dialect_sql_tests.rs`（42，← `DialectSqlTests` + `AbstractJdbcDialectFromTest`）
  - `crates/agentscope-extensions-jdbc/tests/store_tests.rs`（14，← `JdbcStoreH2Test`）
  - `crates/agentscope-extensions-jdbc/tests/state_tests.rs`（31，← `JdbcAgentStateStoreH2Test` + 版本化固有 API）
  - `crates/agentscope-extensions-jdbc/tests/native_lock_tests.rs`（21，← `MysqlDialect.tryEnter` / `PostgresDialect.tryEnter`）
  - `crates/agentscope-extensions-jdbc/tests/snapshot_tests.rs`（10，← `JdbcRemoteSnapshotClientH2Test` + `JdbcSnapshotSpec`）
  - `crates/agentscope-extensions-jdbc/tests/sandbox_tests.rs`（13，← `JdbcSandboxExecutionGuardH2Test` + `...SqliteTest`）
  - `crates/agentscope-extensions-jdbc/tests/distributed_store_tests.rs`（13，← `JdbcDistributedStoreH2Test`）
- 本会话修改的 `src/`（仅本 crate；均为最小改动，未删既有功能/断言）：
  - `src/dialect/session_state_dialect.rs`：**修 `resolved_session_state_table_name()` 无限递归 bug**（原 `None` 分支递归调自身，任何调用即栈溢出；改为 `prefix + base`）。同时对齐：`session_state_insert` 变 4 参模板、`session_state_update_if_version` 按 Java 绑定 `expectedVersion + 1`、like 转义符改 `'!'`、check-exists 接受并绑定表名参数。
  - `src/dialect/abstract_dialect.rs`：新增 `TableNameConfig` / `TableNames` / `TableNameConfigurable`；`AbstractJdbcDialect` 新增 `create_table_ddls()`、`configured_table_names()`，`table_prefix()` 改为 `String`，锁表 DDL 对齐 Java（`(lock_name VARCHAR(255) NOT NULL PRIMARY KEY)`，表名 `{prefix}distributed_locks`）。
  - `src/dialect/detection.rs`：`JdbcDialectBuilder::build()` 现将 prefix/三张表 override 应用入方言实例（等价 Java `Builder.build()`）；新增 `table_name_config()`。
  - `src/dialect/vendor/{ansi,mysql,postgres,sqlite}.rs`：所有方言改持 `TableNames` 并 `impl TableNameConfigurable`；DDL/UPSERT/check-exists 逐条对齐 Java（`VARCHAR(2048)`/`CLOB`/`LONGTEXT`/`ENGINE=InnoDB`/`utf8mb4`、`created_at` 列、独立 `CREATE INDEX IF NOT EXISTS`、`BYTEA`/`LONGBLOB`/`BLOB`）；`MysqlDialect`/`PostgresDialect` 补 `supports_native_locks() = true`。
  - `src/store/jdbc_store.rs`：**修 `namespace_path` 编码 bug**（原 `join` 缺少尾分隔符，导致 `search(["parent"])` 漏掉同命名空间下的条目；改为每段后追加 `\u{1F}`，与 Java 一致）。
  - `src/state/jdbc_agent_state_store.rs`：**修增量追加重写 bug**（`save_list` 增量分支不再用 `execute_prepared_batch` 混跑 INSERT/UPSERT 两种模板，改为 `execute_batch`）；`verify_table_exists` 空结果集即抛错；`like_prefix_pattern` 改用方言转义符。
  - `src/sandbox/jdbc_guard.rs`：**修锁表列不一致 bug**（DDL 建 `lock_name PK`，INSERT 却写 `(lock_name, acquired_at)` → 改为 Java 的 `(lock_name) VALUES (?)`）。
  - `src/snapshot/jdbc_snapshot_spec.rs`：新增 `client()` 访问器（对齐 Java `getClient()`）。
  - `src/dialect/{store_dialect,snapshot_dialect}.rs`：trait 改 supertrait `TableNameConfigurable`，解析函数返回 `String`；`store_like_escape_char` 默认 `'!'`。
- 本会话修复的 4 个**实现 bug**（均为测试暴露，非测试断言错误；根因已记 §3.1/§3.2）：
  1. `session_state_dialect.rs` 表名解析无限递归（栈溢出）。
  2. `jdbc_store.rs` 命名空间路径缺尾分隔符（`search` 漏数据）。
  3. `jdbc_agent_state_store.rs` 增量追加用错执行器（混跑 SQL 模板，新增条目丢失/哈希行错乱）。
  4. `jdbc_guard.rs` 锁表 INSERT/DDL 列不一致（锁永远插不进）。
- 遵守约束：未改根 `Cargo.toml`、未改 core/harness 或其它 crate；未新增/变更依赖（`rusqlite` 已是 workspace 固定依赖）；未删除任何既有功能、测试或断言；未 `#[ignore]`。
- **crate 文件清单**（实测，新增 `tests/`）：`Cargo.toml`；`src/lib.rs`；`src/distributed_store.rs`；`src/backend/{mod.rs,sqlite.rs}`；`src/dialect/{mod.rs,abstract_dialect.rs,bound_sql.rs,detection.rs,lock_strategy.rs,native_lock.rs,session_state_dialect.rs,snapshot_dialect.rs,store_dialect.rs}` + `src/dialect/vendor/{mod.rs,ansi.rs,mysql.rs,postgres.rs,sqlite.rs}`；`src/{store,state,snapshot,sandbox}/`；`tests/{dialect_sql,store,state,native_lock,snapshot,sandbox,distributed_store}_tests.rs`。
- **本会话（2026-10-08）修改的 `src/`（仅本 crate）**：新增 `src/dialect/native_lock.rs`（`NativeLockSession` 缝 + `normalize_lock_name`/`postgres_lock_key` + `MysqlNativeLockStrategy`/`PostgresNativeLockStrategy` + `NativeLockError` + 4 个 SQL 常量 + `NATIVE_LOCK_POLL_INTERVAL`/`MYSQL_MAX_LOCK_NAME_LENGTH`）；`src/dialect/mod.rs` 接线导出；`src/dialect/lock_strategy.rs` 模块文档更新（指向新策略）；`src/state/jdbc_agent_state_store.rs` 新增 `supports_versioning`/`read_version`/`get_versioned`/`save_if_version` + `VersionedState`/`UNVERSIONED`（并将私有 `read_version` 改名为 `read_version_at` 以免与公开方法冲突）；`src/state/mod.rs`、`src/lib.rs` re-export；`Cargo.toml` 新增 `sha2 = { workspace = true }`（+ dev-dep）。

### 4.2 下一步与阻塞
1. **测试覆盖 P0 缺口已关闭**（146 passed / 0 failed）。剩余未覆盖：`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条 Testcontainers 用例，需 Docker + mysql/pg 驱动，环境受限）；`DialectSqlTests` 中依赖真实 H2 `MERGE INTO` 执行语义的部分。二者均**不可离线**，归入下述功能缺口而非测试缺口。
2. **功能缺口（P1）——本轮已关闭两项**：MySQL `GET_LOCK`/`RELEASE_LOCK`、PostgreSQL `pg_try_advisory_lock`/`pg_advisory_unlock` 原生锁（含长锁名 SHA-256 归一）；state 版本化 API `saveIfVersion`/`getVersioned`/`readVersion`/`supportsVersioning`。均见 §3.2「已完成」行。**遗留（均需真实数据库/Docker，不可离线）**：`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条 Testcontainers）；真实适配层仍需实现 `NativeLockSession`（每获取一条独占连接）——本 crate 只提供缝与策略。
3. **跨模块需求（不改 harness/core）**：(a) harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`，本 crate 已在 `JdbcDistributedStore` 内保留全量 4 组件并通过 `to_harness_store()` 输出子集，是否扩展 harness 类型（`EXTENSIONS_GAP_TRACKER.md` §2.2）待裁定；(b) harness `SandboxExecutionGuard` 为具体 struct，本 crate 无法 `impl`（§3.4）；(c) **core `AgentStateStore` 无版本化面**：本轮已将版本化 API 以**固有方法**暴露（`supports_versioning`/`get_versioned`/`save_if_version`/`read_version` + `VersionedState`/`UNVERSIONED`），需汇总方裁定是否将 `supportsVersioning`/`getVersioned`/`saveIfVersion` + `VersionedState`/`UNVERSIONED` 提升为 core trait 方法（影响 JDBC/MYSQL/POSTGRESQL/MONGODB/REDIS 全部存储侧）。
4. **设计决定（不迁移）已逐条标记为「已完成（设计决定，不迁移）」于 §3.4**，依据已写明（H2 由 `AnsiDialect` 承担、SPI 自动注册改产品名匹配、`objectMapper` 注入/连接所有权/`close` 随 `DataSource` 消失等）。
5. 剩余 dead_code helper：`query_optional_int`/`delete_lock`/`namespace_param` 仍无生产调用方（既有项）；`slot_param` 同理。组件全线接线后应被调用；如长期不用可评估删除。
6. **旧记录已更正（§3.3）**：会话表列名、锁表名两语言现已一致（`session_id` / `{prefix}distributed_locks`），早前「跨语言同库不兼容」结论**已失效**。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-jdbc` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-jdbc` / `cargo test -p agentscope-extensions-jdbc` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-jdbc/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

