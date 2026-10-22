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
| Java 规模（实测，2026-10-22 静态复核） | 主代码 **5265 非空行 / 5731 物理行**（24 个 `.java`）/ 测试 **4406 非空行 / 4907 物理行**（19 个 `.java`；**175** 个 `@Test` + **5** 个 `@ParameterizedTest`（执行次数远多于 5），`H2TestSupport` 为支撑类无 `@Test`）。**注**：Java 侧含 `skill` 包（3 个主类 1313 非空行 / 4 个测试类 37 个 `@Test`），Rust 侧**完全缺失**，见 §3.5 |
| Rust crate | `crates/agentscope-extensions-jdbc` |
| 迁移波次 | B |
| 当前状态 | **部分完成**（`cargo build` exit 0；`cargo test` = **166 passed / 0 failed**，含 src 内联 **7** + `tests/` **159**（8 个文件）；**本轮关闭两项 P1 功能缺口**：MySQL/PG 原生 advisory lock + state 版本化 API。剩余为 skill 包整体未迁移（**P0**，见 §3.5）、环境受限的集成测试与跨语言不兼容项，见 §3.1/§3.2） |

### 1.1 基线（开工/完工实测）
| 项 | 开工基线 | 完工基线 |
|---|---|---|
| `cargo build -p agentscope-extensions-jdbc` | exit 0（9 warning） | exit 0（**7 warning**，均为既有 dead_code 辅助函数） |
| `cargo test -p agentscope-extensions-jdbc` | **2 passed / 0 failed**（仅 lib）+ doc-tests 0 | **166 passed / 0 failed** + doc-tests 0（src 内联 7 + `tests/` 159） |
| test target 数 | 1（lib） | 9（lib + `tests/` 下 8 个） |
| `crate` 内 `tests/` 目录 | 无 | `tests/{dialect_sql,store,state,snapshot,sandbox,native_lock,distributed_store,table_schema_validation}_tests.rs` |
| Java 测试类 `@Test` 总数 | 98（10 测试类 + `H2TestSupport`） | **175 `@Test` + 5 `@ParameterizedTest`**（18 个有测试的类 + `H2TestSupport`；**旧记录 98 已严重过期**，见 §6 勘误） |
| 本模块离线可覆盖的 Java `@Test` | — | 62 条 1:1 命名对齐（见 §3.1 逐类映射） |

## 2. Java 清单与依赖映射

| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `jdbc.JdbcDistributedStore` | 组合 4 组件为 harness `DistributedStore`；`create(DataSource)` SPI 自动检测方言 | `distributed_store::JdbcDistributedStore`（`src/distributed_store.rs`） | 持有 `SharedExecutor` + `Arc<dyn AbstractJdbcDialect>` + 4 组件 + `IsolationScope`；`create(executor, product_name)` / `create_with_dialect(_and_schema)`；`to_harness_store()` 仅投影 agent-state-store 半边（harness `DistributedStore` 只有 `agent_state_store`+`isolation_scope`，见 §3.4、§4.2）。Java 组件懒加载（volatile 缓存），Rust 在构造函数内一次性构建 |
| `jdbc.dialect.BoundSql` | 已装配 SQL + 绑定参数（record） | `dialect::BoundSql` + `dialect::SqlParam`（`src/dialect/bound_sql.rs`） | Java `List<Object>` ↔ Rust `Vec<SqlParam>`（`Text`/`Int`/`Blob`/`Null`） |
| `jdbc.dialect.SandboxLockStrategy` | 分布式锁契约（interface） | `dialect::SandboxLockStrategy` + `LockLease` + `LockOutcome`（`src/dialect/lock_strategy.rs`） | Rust 返回 `Result<LockLease, LockOutcome>`；`LockLease` 以闭包实现 `Drop` 释放（Java 用 `SandboxLease.close()`） |
| `jdbc.dialect.AbstractJdbcDialect` | 聚合方言（abstract class；实现 3 个 table 域接口 + 锁契约；表名解析、锁表 DDL、`tryEnter` 默认表锁） | `dialect::AbstractJdbcDialect`（trait，`src/dialect/abstract_dialect.rs`） | 含 `DEFAULT_TABLE_PREFIX="agentscope_"`、`validate_identifier`、`DialectKind`、`lock_create_table_ddl`、`resolved_*_table_name`、`supports_native_locks()`（默认 false）。Java `from(DataSource)` SPI 检测 → Rust 无。**差异**：Java 该类 `implements ... SandboxLockStrategy`（默认表锁 `tryEnter`）；Rust 的 trait **不含** `SandboxLockStrategy`，表锁策略改为 `sandbox::TableLockStrategy` 独立类型（§3.4） |
| `jdbc.dialect.AbstractJdbcDialectBuilder` | 链式配置 + `build()`；SPI 候选（按 `getOrder()`/继承深度排序） | `dialect::JdbcDialectBuilder` + `dialect::detect_dialect`（`src/dialect/detection.rs`） | Rust 以「产品名字符串包含匹配」替代 JDK `ServiceLoader`；builder 记录 prefix/表名覆盖/`auto_create_table`，`build()` 仅返回方言（建表由各组件负责）。**差异**：Java builder 有 `skillTableName`/`skillResourcesTableName`/`enableBaseTables`/`enableSkillTables` 四个 skill 相关开关，Rust **全部缺失**（§3.5） |
| `jdbc.dialect.table.StoreDialect` | KV 表方言（DDL 抽象 + ANSI 默认 SQL） | `dialect::StoreDialect`（`src/dialect/store_dialect.rs`） | SQL 模板逐条对齐（insert/cas_update/select/delete/search/upsert） |
| `jdbc.dialect.table.SessionStateDialect` | 会话状态表方言 | `dialect::SessionStateDialect`（`src/dialect/session_state_dialect.rs`） | Rust 用 `slot_id` 列编码 `user:session`；**Java 列名为 `session_id`**（见 §3.3） |
| `jdbc.dialect.table.SnapshotDialect` | 快照表方言 | `dialect::SnapshotDialect`（`src/dialect/snapshot_dialect.rs`） | — |
| `jdbc.dialect.vendor.MysqlDialect` | MySQL 方言（`ON DUPLICATE KEY UPDATE`、`LONGBLOB`、原生 `GET_LOCK`） | `dialect::vendor::MysqlDialect`（`src/dialect/vendor/mysql.rs`）+ `dialect::native_lock::MysqlNativeLockStrategy` | DDL/UPSERT 已迁移；**原生 `GET_LOCK` 已迁移**（锁名为连接级会话作用域，故独立成策略类型而非挂在方言上，见 §3.2） |
| `jdbc.dialect.vendor.PostgresDialect` | PG 方言（`ON CONFLICT DO UPDATE`、`BYTEA`、`pg_try_advisory_lock`） | `dialect::vendor::PostgresDialect`（`src/dialect/vendor/postgres.rs`）+ `dialect::native_lock::PostgresNativeLockStrategy` | DDL/UPSERT 已迁移；**原生 advisory lock 已迁移**（同上） |
| `jdbc.dialect.vendor.SqliteDialect` | SQLite 方言 | `dialect::vendor::SqliteDialect`（`src/dialect/vendor/sqlite.rs`） | — |
| `jdbc.dialect.vendor.H2Dialect` | H2 方言 | 无独立类型；由 `dialect::vendor::AnsiDialect` 承担（`detect_dialect` 对未知/`h2` 默认返回 Ansi） | 见 §3.4 |
| **`jdbc.dialect.table.SkillDialect`** | **skill 表方言（表名 `skills`；`namespace`+`name` 唯一；含 `metadata_json`）** | **无** | **未迁移（P0）**。Java 120 非空行；Rust crate 内 `grep -i skill` = **0 命中**。见 §3.5 |
| **`jdbc.dialect.table.SkillResourcesDialect`** | **skill 资源表方言（复合 PK `(id, resource_path)`；级联 FK 到 skill 表）** | **无** | **未迁移（P0）**。Java 98 非空行。见 §3.5 |
| **`jdbc.skill.JdbcAgentSkillRepository`** | **JDBC 版 `AgentSkillRepository`（namespace 隔离 + 资源 CRUD）** | **无** | **未迁移（P0）**。Java **1095 非空行**（本模块最大单类）；Rust 全仓 `grep JdbcAgentSkillRepository` = 0 命中。见 §3.5 |
| **`jdbc.dialect.TableSchemaValidator`** | 建表后表结构校验（`validate(conn, table, ddls)` + `VALID_IDENTIFIER`） | `dialect::table_schema`（`src/dialect/table_schema.rs`）`parse_create_table_columns` / `validate_table_schema` | 已迁移（§3.3「建表后表结构校验」行）；**未做** Java 的 `createTableDdls()` → `Map<table, List<ddl>>` 分组重构（Rust 保留 `Vec<String>`） |
| `jdbc.dialect.package-info` / `dialect.table.package-info` / `dialect.vendor.package-info` | 包文档（无逻辑） | 无 | — |
| `META-INF/services/io.agentscope.extensions.jdbc.dialect.AbstractJdbcDialect` | JDK SPI 方言注册表 | 无 | Rust 无 SPI 自动注册（见 §3.3） |
| `jdbc.store.JdbcStore` | JDBC `BaseStore`（零内联 SQL） | `store::JdbcStore`（`src/store/jdbc_store.rs`，`impl agentscope_harness::filesystem::BaseStore`）+ `store::store_value` | `get`/`put`/`put_if_version`/`search`/`delete` 逐一对齐；CAS：`expected==0`→`store_insert`（唯一键冲突=失败），`>0`→`store_cas_update` 影响行数==1 |
| `jdbc.store.JdbcStore.Builder` | 组件构建器 | `JdbcStore::new`/`with_schema`（无独立 Builder 类型） | Java 可注入 `dialect`/`ObjectMapper`/`initializeSchema`；Rust 固定 serde_json |
| `jdbc.state.JdbcAgentStateStore` | JDBC `AgentStateStore`（单值 + 增量列表 + `:_hash`） | `state::JdbcAgentStateStore`（`src/state/jdbc_agent_state_store.rs`，`impl agentscope_core::state::AgentStateStore`） | 覆盖 `save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`；列表哈希复用 `agentscope_core::state::list_hash`；**版本化 API 已以固有方法公开**（`save_if_version`/`get_versioned`/`read_version`/`supports_versioning` + `VersionedState`/`UNVERSIONED`，见 §3.2），**且 core trait 已于 2026-10 上提版本化面，本 crate 的 `impl AgentStateStore` 已同时覆写 `supports_versioning`/`get_versioned`/`save_if_version`（见 §3.2 与 §6 勘误）** |
| `jdbc.snapshot.JdbcRemoteSnapshotClient` | BLOB 上传/下载/存在性 | `snapshot::JdbcRemoteSnapshotClient`（`src/snapshot/jdbc_remote_snapshot_client.rs`，`impl RemoteSnapshotClient`） | 表默认 `agentscope_snapshots`；`upload` 用方言 `snapshot_upsert` |
| `jdbc.snapshot.JdbcSnapshotSpec` | 快照 spec 便捷类（extends `RemoteSnapshotSpec`） | `snapshot::JdbcSnapshotSpec`（`src/snapshot/jdbc_snapshot_spec.rs`，newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`） | 快照 id 校验沿用 harness |
| `jdbc.sandbox.JdbcSandboxExecutionGuard` | JDBC `SandboxExecutionGuard`，委托方言锁策略 | `sandbox::JdbcSandboxExecutionGuard`（`src/sandbox/jdbc_guard.rs`） | harness guard 为 **struct 非 trait**，故为固有方法 `try_enter`/`release`/`is_held`（见 §3.4） |
| `jdbc.sandbox.JdbcSandboxExecutionGuard.Builder` + 内部 lease | 构建器与租约 | `JdbcSandboxExecutionGuard::{new,table_lock,with_key_prefix,with_timeout_seconds}` + `sandbox::TableLockStrategy` + `dialect::LockLease` | 常量对齐：`DEFAULT_KEY_PREFIX="agentscope:sandbox:lock:"`、`DEFAULT_LOCK_TIMEOUT_SECONDS=1800` |
| `javax.sql.DataSource` / JDBC（组件入参） | 连接与 SQL 执行 | **Rust 独有**：`backend::JdbcExecutor` trait + `backend::RusqliteExecutor`（`src/backend/{mod,sqlite}.rs`）+ `ColumnValue`/`JdbcError`/`SharedExecutor`/`is_unique_violation` | Java 无对应类；Rust 以 executor seam 替代 DataSource（见 §2.1） |
| （测试支撑）`H2TestSupport` + 18 个有 `@Test` 的测试类（H2/SQLite/Testcontainers）+ **4 个 skill 相关测试类** | 离线/集成测试 | `RusqliteExecutor::in_memory()`（测试后端） | Rust 侧已建 **8** 个 `tests/*.rs` 离线用例（159 test + src 内联 7），详见 §3.1 逐类映射 |

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
| JUnit5 / Testcontainers 1.21.3 | 测试 | `#[cfg(test)]`（`src/lib.rs` 2 个 + `src/dialect/table_schema.rs` 5 个 = **7**）+ `tests/*.rs` **8** 个集成文件（**159** 个 `#[test]`/`#[tokio::test]`，含 `#[tokio::test]`） | 见 §3.1；未新增 dev-dependencies（`tokio` 已在 `[dependencies]`，`sha2` 已作 dev-dep） |

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
| `JdbcAgentStateStore`（`impl AgentStateStore`） | `state/JdbcAgentStateStore.java` | 部分完成 | 7 个 trait 方法全实现 + `create_table_if_not_exist`/`verify_table_exists`；**版本化 API 已以固有方法 + core trait 方法双暴露**（§3.2） |
| `JdbcRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | `snapshot/JdbcRemoteSnapshotClient.java` | 已完成 | upload/download/exists |
| `JdbcSnapshotSpec`（`impl SandboxSnapshotSpec`） | `snapshot/JdbcSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec` |
| `JdbcSandboxExecutionGuard` + 表锁策略 | `sandbox/JdbcSandboxExecutionGuard.java` | 已完成 | 仅 `TableLockStrategy`；**本轮修正**：`insert_lock` 与原 `lock_create_table_ddl` 列不一致（DDL 建 `lock_name PK` 但 INSERT 写 `(lock_name, acquired_at)`）→ 统一为 Java 的 `(lock_name) VALUES (?)`；`release`/`is_held` 已是真实探针（`is_held` 查锁表，非恒 false） |
| `JdbcDistributedStore` 组合者 | `JdbcDistributedStore.java` | 已完成 | 4 组件访问器 + `to_harness_store()`（仅投影半量，§4.2） |
| 无真实 DB 的内存后端 | Java H2/SQLite | 已完成 | `RusqliteExecutor::in_memory()` |
| **离线单元测试** | 18 个测试类，共 **175** 个 `@Test` + **5** 个 `@ParameterizedTest`（H2 为主 + SQLite + 2 个 Testcontainers 集成 + **4 个 skill 测试类 37 条**） | **部分完成（skill 部分未覆盖）** | 现状：`cargo test -p agentscope-extensions-jdbc` = **166 passed / 0 failed**（**src 内联 7** + `tests/` **159**，8 个集成测试文件）。逐类映射（Rust target ← Java 类 / 用例名已写入每条测试注释）：<br>• `tests/dialect_sql_tests.rs`（**45**）← `DialectSqlTests`（Java 实为 **30** 条 `@Test`，Rust 有直接对应 **26** 条；未对应 **4** 条：3 条 skill 断言（`skillSqlIsNamespaceScoped`/`skillDdlsIndexAndCollateNamespace`/`skillStatementsNeverUpdateNamespace`）+ `postgresStateUpsertIncrementsVersion`（PG 状态 UPSERT 的 `version = {}.version + 1` 断言缺失，SQL 本身已含该子句但无用例断言）；`mysqlDialectIsLockStrategy` 的 Rust 对应用例只断言 `supports_native_locks()`，**方言本身不实现 `SandboxLockStrategy`**）+ `AbstractJdbcDialectFromTest`（**6/8**：`detectsPostgres/Mysql/MariaDb/H2/Sqlite` + 不支持库降级；`throwsOnConnectionFailure`/`throwsForNullDataSource` 对应 Rust 无 `DataSource` 的连接面，见 §3.4）。<br>• `tests/store_tests.rs`（**14**）← `JdbcStoreH2Test` 10/10 全覆盖 + 4 条 CAS 三分支/命名空间编码加固。<br>• `tests/state_tests.rs`（**36**）← `JdbcAgentStateStoreH2Test` 非版本化部分 13/13 + **15 条版本化用例**（实测 15 个 `#[tokio::test]` 命中版本化语义：`supports_versioning`/`get_versioned`/`read_version`/`save_if_version` 三分支 + 冲突不写 + 隔离/校验 + `UNVERSIONED` 单语句原子性 + version-0 自愈正/反向）+ 8 条隔离/校验/`:_hash` 加固。版本化用例打在 `JdbcAgentStateStore` 的**固有方法**与 **core trait 方法**两处（core trait 自 2026-10 起已有版本化面，见 §3.2）。<br>• `tests/snapshot_tests.rs`（**10**）← `JdbcRemoteSnapshotClientH2Test` 6/6 全覆盖 + `JdbcSnapshotSpec` 4 条（含 `getClient` 往返、id 校验）。<br>• `tests/sandbox_tests.rs`（**13**）← `JdbcSandboxExecutionGuardH2Test` 4/4 + `JdbcSandboxExecutionGuardSqliteTest` 2/2 + `DEFAULT_*` 常量/`with_key_prefix`/`release`/`is_held` 7 条（便携表锁路径）。<br>• `tests/native_lock_tests.rs`（**21**）← `MysqlDialect.tryEnter`/`PostgresDialect.tryEnter` 行为对齐：MySQL 锁名归一（短名/恰 64 字符/超长截断+摘要、长名互异）、PG advisory key 折叠、两策略 acquire/release/is_held/drop、超时与后端失败不 panic、guard 接线。SHA-256 期望值用独立字面量 + dev-dep 重算双向交叉验证。<br>• `tests/distributed_store_tests.rs`（**13**）← `JdbcDistributedStoreH2Test` 5/5 + 组件工厂/`to_harness_store()` 投影/共享句柄/schema 关闭 8 条。<br>• `tests/table_schema_validation_tests.rs`（**7**）← `CreateTableDdlParserTest`（11）+ `DialectBuilderSchemaValidationTest`（9）的离线等价覆盖（2026-10-20 新增）。<br>**未覆盖**：① **skill 包全部 37 条 `@Test`**（`SkillTableDialectH2Test` 8 / `SkillTableDialectSqliteTest` 3 / `JdbcAgentSkillRepositoryH2Test` 27 / `JdbcAgentSkillRepositorySqliteTest` 2 —— 归入 §3.5 **P0 缺口**，非环境受限）；② `MysqlIntegrationTest`/`PostgresIntegrationTest`（各 **8** 条，Testcontainers + Docker，环境受限）；③ `DialectBuilderTableGroupsTest`（7 条，表组开关，依赖 skill 表组，同 ①）。补测已全部离线（`RusqliteExecutor::in_memory()` + `SqliteDialect`），无新依赖、无网络。 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| MySQL 原生 `GET_LOCK`/`RELEASE_LOCK` 锁 | `dialect/vendor/MysqlDialect.java:246-330` | **已完成** | `dialect/native_lock.rs` 的 `MysqlNativeLockStrategy`。**未**在方言类型上 `impl SandboxLockStrategy`：`GET_LOCK` 是会话级锁，Java 靠每 lease 独占 Connection + `close()` 释放，而方言不持有连接（见该模块头注释）。锁名归一已移植：超 64 字符则截为 31 字符 + `:` + SHA-256 前 32 hex（防高基数碰撞，Java 注释明确弃用 32-bit `String.hashCode`）。路由缝 `NativeLockSession` + SQL 常量 `MYSQL_GET_LOCK_SQL`/`MYSQL_RELEASE_LOCK_SQL`。**静态复核补充**：`NativeLockSession` 目前**无任何生产实现**（`src/` 内 `impl NativeLockSession` = 0 命中，仅 `tests/native_lock_tests.rs` 有 `FakeSession`），即真实适配层尚未存在 —— 该缝与策略本身可用，但端到端原生锁**尚未真正接线**（见 §3.5 P1） |
| PostgreSQL 原生 `pg_try_advisory_lock`/`pg_advisory_unlock` | `dialect/vendor/PostgresDialect.java:231-330` | **已完成** | 同 `dialect/native_lock.rs` 的 `PostgresNativeLockStrategy`；`postgres_lock_key` 折叠 SHA-256 前 8 字节（与 Java 一致，碰撞 ~2^-32）。与 Java 差异：Java 在策略内轮询至 deadline，Rust 交给 `NativeLockSession::try_acquire(timeout)`；`NATIVE_LOCK_POLL_INTERVAL=100ms` 已导出供真实适配层。后端失败 → `LockOutcome::TimedOut`（guard 降为 no-op lease，不 panic，见 §3.4）。**静态复核补充**：同 MySQL 行，`NativeLockSession` 无生产实现，真实 PG 适配层缺失（§3.5 P1） |
| state 版本化写入/读取（`saveIfVersion`/`readVersion`/`supportsVersioning`） | `state/JdbcAgentStateStore.java:148,153,176,203` | **已完成** | `JdbcAgentStateStore` 新增固有方法 `supports_versioning()`/`read_version()`/`get_versioned()`/`save_if_version()` + `VersionedState`/`UNVERSIONED`（已导出）。**静态复核更新**：core `AgentStateStore` trait **已含**版本化面（`agentscope-core/src/state/store.rs:68,76,98`：`supports_versioning`/`get_versioned`/`save_if_version`，含 `agentscope_core::state::{VersionedState, UNVERSIONED}`），本 crate 的 `impl AgentStateStore` 已**同时覆写这 3 个 trait 方法**（`state/jdbc_agent_state_store.rs:565,569,590`），把固有方法结果转发给 core 类型。**因此 §4.2 的「core trait 无版本化面，待汇总方裁定」已失效**（见 §6 勘误）。**遗留**：本 crate 仍**自己重复定义**了一份 `VersionedState`/`UNVERSIONED`（`jdbc_agent_state_store.rs:38,43`），与 core 的同名类型并存 —— 双份定义是 P2 清理项（§3.5） |
| `saveIfVersion(UNVERSIONED)` 写后读原子性 | `state/JdbcAgentStateStore.java:203` | **已完成（2026-10-20）** | 原实现 `save_single()` + 独立 `read_version_at()`，并发下返回的版本可能属于别人的写入。现走 `upsert_returning_version`：方言 `session_state_upsert_returning_version` 用 `RETURNING version`（PG/SQLite）单语句完成；**MySQL/H2 无 `RETURNING`，降级为 UPSERT + 独立 `SELECT version`，代码注释与测试均明确标注为「已知非原子退化」**（版本仍单调、始终反映已提交数据，但不能断言它标识本次 payload）。用例：`state_tests.rs` |
| `saveIfVersion(0)` 幽灵 CAS 自愈 | `state/JdbcAgentStateStore.java:231` | **已完成（2026-10-20）** | 历史行可能停在 DDL 默认 `version = 0`（`session_state_insert` 省略 version 列，只有 list 元素走它），这类行 insert-if-absent 永远主键冲突 → 曾经永久不可写。现 `expected_version == 0 && affected != 1` 时执行 `cas_zero_version`：以 `WHERE version = 0` 再做一次 CAS，影响 1 行返回 1，否则仍返回 `UNVERSIONED`。**未改 DDL 默认值、无需数据迁移**；反向用例保证「version=3 时 save_if_version(0) 仍冲突且不写」，防止自愈退化成无条件覆盖 |
| MySQL 键列二进制排序（session/snapshot 表） | `2349533a1` | **已完成（2026-10-20）** | `dialect/vendor/mysql.rs`：会话状态表 `session_id`/`state_key`、快照表 `snapshot_id` 均显式 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`（表级默认保持 `utf8mb4_unicode_ci`，只影响非键列）。DDL 断言见 `tests/dialect_sql_tests.rs`。**存量库仍需手工 ALTER**，见 `docs/migrations/mysql_binary_collation.md` §5 |
| 快照上传流式绑定（避免整包入内存） | `snapshot/JdbcRemoteSnapshotClient.java`（`setBinaryStream`） | 待确认 | Rust `RemoteSnapshotClient::upload(&[u8])` 需整包驻留内存；大快照场景存在内存差异 |
| 真实 MySQL/PostgreSQL 集成测试 | `integration/MysqlIntegrationTest.java`（8 条）、`integration/PostgresIntegrationTest.java`（8 条） | 待办（环境受限） | Rust 仍无 mysql/pg 驱动与 Docker 集成；本轮新增的 `tests/` 仅覆盖离线 SQLite 路径。对应 **16** 条 Java `@Test` 映射见 §3.1（**旧记录「10 条」已过期**） |
| 列表增量语义（`getListCount`/`likePrefixPattern`） | `state/JdbcAgentStateStore.java:414,481` | 已完成 | Rust `list_count`/`like_prefix_pattern` 已实现；本轮修正 `like_prefix_pattern` 使用方言 `*_like_escape_char()`（由 `'\\'` 改为与 Java 一致的 `'!'`），并新增增量追加/哈希伴生行用例 |
| 表存在性校验 | `state/JdbcAgentStateStore.java:101` | 已完成 | Rust `verify_table_exists` 使用方言 `session_state_check_table_exists`；**本轮修正根因**：原实现只用 `query().map(|_| ())` 视为成功，空结果集（表不存在时 SELECT 正常返回 0 行）不报错 → 改为「空结果集即抛错」，与 Java `if (!rs.next())` 一致。新增用例 `constructor_with_create_if_not_exist_false_throws_when_table_missing` |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 会话表列名差异（跨语言同库不兼容） | `dialect/table/SessionStateDialect.java`（`session_id`） | 已完成（已对齐） | Rust 列名已为 `session_id`（与 Java 一致）；仅 Rust 函数形参仍名为 `slot_id`（内部存储 `user:session` 的合成值），不影响表结构。**不再存在跨语言表结构不兼容**（本条旧记录已失效） |
| 锁表名差异（跨语言不兼容） | `dialect/AbstractJdbcDialect.java:253`（`tablePrefix + "distributed_locks"`） | 已完成（已对齐） | Rust `resolved_lock_table_name()` = `{prefix}distributed_locks`（`abstract_dialect.rs:187`），与 Java 一致；锁表 DDL 也已对齐为 `(lock_name VARCHAR(255) NOT NULL PRIMARY KEY)`。**本条旧记录（`{prefix}lock`）已失效** |
| JDK SPI 方言自动注册/发现 | `AbstractJdbcDialect.from(DataSource)` + `META-INF/services/...` | 待办 | Rust 用产品名子串匹配，第三方方言无法自动注册；扩展需改 `detect_dialect`。**注**：§3.4 已把同一项标为「已完成（设计决定，不迁移）」，两行口径冲突 —— 保留「待办」表示第三方方言无法自动注册这一**残余能力差距** |
| 组件懒加载缓存 | `JdbcDistributedStore` volatile 字段 | 待确认 | Rust 在构造函数中一次性构建全部组件（含 DDL），语义等价但非懒加载 |
| `tracing` 埋点对齐 SLF4J | 各方言/组件日志点 | 待办 | Rust 仅少量日志（guard/state/store） |
| dead_code 辅助函数 | — | 待确认 | `TableLockStrategy::delete_lock`、`namespace_param` 等当前无调用方；保留待接线，长期不用可评估删除。**静态复核补充**：`delete_lock` 实际**有**调用方（`jdbc_guard.rs:94` `SandboxLockStrategy::release` 内部调用），并非死代码；真正无调用方的是 `query_optional_int`（`backend/sqlite.rs:163`）、`slot_param`（`jdbc_agent_state_store.rs:630`）、`namespace_param`（`jdbc_store.rs:259`）三个 |
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

### 3.5 静态复核新发现缺口（2026-10-22）
> 本节由「静态复核」会话新增（仅静态核对，**未编译、未运行 cargo test**）。所有结论基于文件/符号/行号的逐条 grep 与源码阅读。

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **JDBC skill 仓库整体未迁移**（`JdbcAgentSkillRepository` + `SkillDialect` + `SkillResourcesDialect`） | `skill/JdbcAgentSkillRepository.java`（**1095 非空行**，本模块最大单类）、`dialect/table/SkillDialect.java`（120）、`dialect/table/SkillResourcesDialect.java`（98）；`AbstractJdbcDialectBuilder` 的 `skillTableName`/`skillResourcesTableName`/`enableSkillTables`；`AbstractJdbcDialect.createTableDdls()` 的 skill 表组分支 | **待办（P0）** | Rust 侧 `grep -ri 'skill'` 命中 **0**（`src/` + `tests/` + `Cargo.toml` 全无）；全仓 `grep JdbcAgentSkillRepository` 亦 0 命中。Java skill 主代码合计 **1313 非空行**（占模块 5265 的 **24.9%**）+ 测试 **1267 非空行 / 37 条 `@Test`**（`SkillTableDialectH2Test` 8 / `SkillTableDialectSqliteTest` 3 / `JdbcAgentSkillRepositoryH2Test` 27 / `JdbcAgentSkillRepositorySqliteTest` 2）。**注意**：Java 侧 `MysqlSkillRepository`/`PostgresSkillRepository` 已标 `@Deprecated`，官方替代正是本 JDBC skill 仓库（见 `SKILLS__GAP_TRACKER.md` 的上游状态提醒），故**不宜简单标「不迁移」**。需汇总方裁定归属（并入本 crate vs 独立 crate）。**依赖前置**：需 harness `SkillRepository`/`AgentSkillRepository` trait + `AgentSkill`/`SkillResources` 模型 —— `agentscope-harness/src/agent/skill/repository.rs` 与 `agentscope-harness/src/agent/skill/model.rs` **已存在**，故技术上可落地；另需 `agentscope-extensions-skills` 的 `AgentSkillRepository` trait（`crates/agentscope-extensions-skills/src/repository.rs`）二选一对齐 |
| **原生 advisory lock 未真正接线**（策略与缝齐备，但无生产 `NativeLockSession` 实现） | `MysqlDialect.tryEnter`（独占 Connection）、`PostgresDialect.tryEnter`（独占 Connection + 100ms 轮询至 deadline） | 待办（P1） | `src/dialect/native_lock.rs` 的 `MysqlNativeLockStrategy`/`PostgresNativeLockStrategy` **已实现** `SandboxLockStrategy`（`try_enter`/`release`/`is_held`），锁名归一（MySQL 64 字符 + SHA-256）与 advisory key 折叠（PG SHA-256 前 8 字节）与 Java 一致，`normalize_lock_name`/`postgres_lock_key` 均为 `pub` 且被 `tests/native_lock_tests.rs`（21 条）覆盖。**但** `grep 'impl NativeLockSession' src/` = **0 命中**：唯一实现是 `tests/native_lock_tests.rs:74` 的 `FakeSession`。`JdbcExecutor` 是「共享单连接」语义（`backend/mod.rs:88`），无法表达 Java「每 lease 独占连接」，故原生锁端到端路径**目前无生产可用实现**。本 crate 已提供缝与策略，**真实适配层需在本 crate 之外的驱动 crate（mysql/pg）落地**，或在本 crate 增 `feature` 化的连接独占能力 |
| **PG advisory lock 轮询语义差异** | `PostgresDialect.java:236-262`（deadline 内 100ms 轮询 `pg_try_advisory_lock`） | 待办（P2） | Rust `PostgresNativeLockStrategy::try_enter`（`native_lock.rs:208`）把超时**完全下放**给 `NativeLockSession::try_acquire(lock_key, timeout)`，自身不轮询；`NATIVE_LOCK_POLL_INTERVAL`（100ms）仅导出未使用。行为是否等价取决于适配层实现（若适配层一次性 `try_acquire` 即返回 false，则 Rust 会在 0 时间内失败而 Java 会等到 deadline）。**建议**：在适配层契约中明确「`try_acquire` 需内部轮询至 deadline」，或把轮询上移到策略内 |
| **`VersionedState`/`UNVERSIONED` 双份定义** | — | 待办（P2） | core `agentscope_core::state::{VersionedState, UNVERSIONED}`（`agentscope-core/src/state/store.rs:12,17`）与本 crate 自定义的 `state::VersionedState`/`state::UNVERSIONED`（`jdbc_agent_state_store.rs:38,43`）**同名并存**且语义相同（`UNVERSIONED = -1`）。`impl AgentStateStore` 的 trait 方法已转发到 core 类型（`jdbc_agent_state_store.rs:565-611`），但固有方法仍返回本 crate 的副本 → 调用方需在两个类型间转换。**建议**：删本 crate 副本，全部改用 core 类型（本 crate 非公共 API 破坏性变更，可在本 crate 内完成） |
| **`dialect/mod.rs` 模块文档与实际 trait 组成不符** | Java `AbstractJdbcDialect implements ... SandboxLockStrategy` | 待办（P2） | `src/dialect/mod.rs:10-12` 声称「`AbstractJdbcDialect` implements all table-domain traits **and the [`SandboxLockStrategy`] contract**」，但 `src/dialect/abstract_dialect.rs:148-150` 的 trait 定义为 `StoreDialect + SessionStateDialect + SnapshotDialect + TableNameConfigurable + Send + Sync` —— **不含** `SandboxLockStrategy`（表锁已拆为 `sandbox::TableLockStrategy`，原生锁拆为 `native_lock.rs` 两个策略）。文档误导，建议改写 |
| **`SessionStateDialect` 默认 `session_state_check_table_exists` 与 Java 不一致** | `SessionStateDialect.java`（Java 无该默认方法，各 vendor 自行提供） | 待办（P2） | Rust trait 默认实现（`session_state_dialect.rs:52-57`）为 `SELECT 1 FROM {table} WHERE 1 = 0`（无参数），而 ANSI/MySQL/PG/SQLite **四个 vendor 均已覆写**为 `INFORMATION_SCHEMA`/`information_schema`/`sqlite_master` 形式并绑定表名参数。故默认实现**无实际调用方**，但它是「表不存在时 SELECT 返回 0 行」的旧语义残留，与 §3.2「表存在性校验」条目的修正方向相反 —— 若将来新增 vendor 未覆写该方法，会静默误判。建议把默认实现改为「查得列才算存在」或删除默认值强制实现 |
| **PG 状态 UPSERT 版本自增缺断言** | `DialectSqlTests.postgresStateUpsertIncrementsVersion` | 待办（P2） | Rust `tests/dialect_sql_tests.rs` 覆盖了 `postgres_store_upsert_uses_on_conflict`（KV store 的 `ON CONFLICT`）与 `mysql_state_upsert_and_check` / `h2_state_upsert_and_check`，但**缺**「PG 状态表 UPSERT 在冲突时把 `version` 自增」这一条断言。SQL 本身正确（`src/dialect/vendor/postgres.rs:116`：`SET state_data = EXCLUDED.state_data, version = {}.version + 1`），只是无用例守护 → 后续改 SQL 不会被测出。建议补 1 条断言（`dialect_sql_tests.rs`） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **最新会话主题（2026-10-22）：静态复核会话**（**只做静态核对，未运行 `cargo build` / `cargo test`，未改任何 Rust 代码**）。核对结论见 §6「静态复核记录（2026-10-22）」；新发现缺口见 §3.5（**skill 包整体未迁移 = P0**）。本会话只修正本 tracker 的统计口径与过期结论。
- **上一会话主题（2026-10-08）：关闭两项 P1 功能缺口——MySQL/PG 原生 advisory lock 与 state 版本化 API——并补齐其离线测试。** `cargo build -p agentscope-extensions-jdbc` 与 `cargo test -p agentscope-extensions-jdbc` 均通过。
- **再上一会话主题（保留记录）：补齐离线测试覆盖（原 P0 缺口）+ 修复测试暴露的实现 bug。**
- 验证命令与结果（**2026-10-08 实测；2026-10-22 静态复核未复跑**）：
  - `cargo build -p agentscope-extensions-jdbc` → `Finished dev profile`，**退出码 0**（**crate 自身 7 warning**，均为既有 dead_code 辅助函数（`query_optional_int`/`slot_param`）+ 测试/快照等既有未用导入；无 error。较上一会话 8 → 7：原死代码 `read_version` 已接线为公开 `read_version`/`get_versioned`）。
  - `cargo test -p agentscope-extensions-jdbc` → **退出码 0**；各 target 结果（**2026-10-22 静态复核按 `#[test]`/`#[tokio::test]` 注解逐文件重数，实际应为下表**）：`unittests src/lib.rs` = **2 passed / 0 failed**（+ `src/dialect/table_schema.rs` 内联 **5**）；`tests/dialect_sql_tests.rs` = **45 / 0**；`tests/distributed_store_tests.rs` = **13 / 0**；`tests/sandbox_tests.rs` = **13 / 0**；`tests/native_lock_tests.rs` = **21 / 0**；`tests/snapshot_tests.rs` = **10 / 0**；`tests/state_tests.rs` = **36 / 0**；`tests/store_tests.rs` = **14 / 0**；`tests/table_schema_validation_tests.rs` = **7 / 0**；doc-tests = **0**。合计 **166 passed / 0 failed**（2026-10-08 记录为 146；差额来自 2026-10-20 新增的 `table_schema_validation_tests.rs`（7）与 `table_schema.rs` 内联（5），以及 `dialect_sql_tests.rs`(+3)/`state_tests.rs`(+5) 的后续增补；**9 个 test target**）。
- 本会话新增 `tests/`（全部离线，`RusqliteExecutor::in_memory()` + `SqliteDialect`/`AnsiDialect`/`MysqlDialect`/`PostgresDialect`；无网络、无新依赖、无 `#[ignore]`）：
  - `crates/agentscope-extensions-jdbc/tests/dialect_sql_tests.rs`（45，← `DialectSqlTests` + `AbstractJdbcDialectFromTest`）
  - `crates/agentscope-extensions-jdbc/tests/store_tests.rs`（14，← `JdbcStoreH2Test`）
  - `crates/agentscope-extensions-jdbc/tests/state_tests.rs`（36，← `JdbcAgentStateStoreH2Test` + 版本化 API）
  - `crates/agentscope-extensions-jdbc/tests/native_lock_tests.rs`（21，← `MysqlDialect.tryEnter` / `PostgresDialect.tryEnter`）
  - `crates/agentscope-extensions-jdbc/tests/snapshot_tests.rs`（10，← `JdbcRemoteSnapshotClientH2Test` + `JdbcSnapshotSpec`）
  - `crates/agentscope-extensions-jdbc/tests/sandbox_tests.rs`（13，← `JdbcSandboxExecutionGuardH2Test` + `...SqliteTest`）
  - `crates/agentscope-extensions-jdbc/tests/distributed_store_tests.rs`（13，← `JdbcDistributedStoreH2Test`）
  - `crates/agentscope-extensions-jdbc/tests/table_schema_validation_tests.rs`（7，← `CreateTableDdlParserTest` + `DialectBuilderSchemaValidationTest`，2026-10-20 新增）
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
  - **2026-10-22 静态复核结论：4 个修复形态全部可静态确认（详见 §6 表「4 个 bug 修复形态核验」）** —— ① `resolved_session_state_table_name()` 现为 `prefix + base` 无自调；② `namespace_path()` 每段后追加 `\u{001F}`；③ 增量分支改用 `execute_batch`（`execute_prepared_batch` 在 `src/` 内已无调用方）；④ 锁表 INSERT 为 `(lock_name) VALUES (?)`，与 DDL `(lock_name VARCHAR(255) NOT NULL PRIMARY KEY)` 一致。
- 遵守约束：未改根 `Cargo.toml`、未改 core/harness 或其它 crate；未新增/变更依赖（`rusqlite` 已是 workspace 固定依赖）；未删除任何既有功能、测试或断言；未 `#[ignore]`。
- **crate 文件清单**（2026-10-22 实测，`src/` **28 个 `.rs` / 3882 非空行（4286 物理行）**；`tests/` **8 个 `.rs` / 2356 非空行（2688 物理行）**）：`Cargo.toml`；`src/lib.rs`；`src/distributed_store.rs`；`src/backend/{mod.rs,sqlite.rs}`；`src/dialect/{mod.rs,abstract_dialect.rs,bound_sql.rs,detection.rs,lock_strategy.rs,native_lock.rs,session_state_dialect.rs,snapshot_dialect.rs,store_dialect.rs,table_schema.rs}` + `src/dialect/vendor/{mod.rs,ansi.rs,mysql.rs,postgres.rs,sqlite.rs}`；`src/{store,state,snapshot,sandbox}/`；`tests/{dialect_sql,store,state,native_lock,snapshot,sandbox,distributed_store,table_schema_validation}_tests.rs`。**本清单相对旧记录新增 `src/dialect/table_schema.rs` 与 `tests/table_schema_validation_tests.rs`（2026-10-20 会话）**。
- **本会话（2026-10-08）修改的 `src/`（仅本 crate）**：新增 `src/dialect/native_lock.rs`（`NativeLockSession` 缝 + `normalize_lock_name`/`postgres_lock_key` + `MysqlNativeLockStrategy`/`PostgresNativeLockStrategy` + `NativeLockError` + 4 个 SQL 常量 + `NATIVE_LOCK_POLL_INTERVAL`/`MYSQL_MAX_LOCK_NAME_LENGTH`）；`src/dialect/mod.rs` 接线导出；`src/dialect/lock_strategy.rs` 模块文档更新（指向新策略）；`src/state/jdbc_agent_state_store.rs` 新增 `supports_versioning`/`read_version`/`get_versioned`/`save_if_version` + `VersionedState`/`UNVERSIONED`（并将私有 `read_version` 改名为 `read_version_at` 以免与公开方法冲突）；`src/state/mod.rs`、`src/lib.rs` re-export；`Cargo.toml` 新增 `sha2 = { workspace = true }`（+ dev-dep）。

### 4.2 下一步与阻塞
1. **测试覆盖缺口**：**P0 缺口未完全关闭**（2026-10-22 静态复核修正）——skill 相关 **37 条 `@Test`** 完全未覆盖（§3.5 P0）；`MysqlIntegrationTest`/`PostgresIntegrationTest`（各 **8** 条 Testcontainers 用例，需 Docker + mysql/pg 驱动，环境受限）；`DialectSqlTests` 中依赖真实 H2 `MERGE INTO` 执行语义的部分。Java 总计 **175 `@Test` + 5 `@ParameterizedTest`**，Rust 侧当前 **166 test**（口径见 §6）。
2. **功能缺口**：① **P0 —— JDBC skill 仓库整体未迁移**（`JdbcAgentSkillRepository` 1095 行 + 2 个方言 trait + builder 4 个开关；Java skill 主代码 1313 非空行 = 模块的 24.9%），归属待汇总方裁定（§3.5）。② **P1 —— 原生 advisory lock 未真正接线**：`MysqlNativeLockStrategy`/`PostgresNativeLockStrategy` 与 `NativeLockSession` 缝已就绪，但 `src/` 内 **无任何 `impl NativeLockSession`**，真实适配层（每 lease 独占连接）需在驱动 crate 落地（§3.5）。③ **P2 —— PG advisory lock 轮询语义差异**（Rust 不轮询，超时下放适配层）、**`VersionedState`/`UNVERSIONED` 双份定义**、**`dialect/mod.rs` 文档与 trait 组成不符**、**`session_state_check_table_exists` 默认实现残留**（§3.5）。
3. **跨模块需求（不改 harness/core）**：(a) harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`，本 crate 已在 `JdbcDistributedStore` 内保留全量 4 组件并通过 `to_harness_store()` 输出子集，是否扩展 harness 类型（`EXTENSIONS_GAP_TRACKER.md` §2.2）待裁定；(b) harness `SandboxExecutionGuard` 为具体 struct，本 crate 无法 `impl`（§3.4）；(c) **core `AgentStateStore` 版本化面已裁定并落地**（2026-10-22 静态复核）：core trait 已含 `supports_versioning`/`get_versioned`/`save_if_version` + `VersionedState`/`UNVERSIONED`（`agentscope-core/src/state/store.rs:12,17,68,76,98`），本 crate 的 `impl AgentStateStore` **已覆写这 3 个方法**（`jdbc_agent_state_store.rs:565,569,590`）。**原「需汇总方裁定是否提升为 core trait 方法」的阻塞已解除**；**遗留**：本 crate 仍重复定义了一份同名 `VersionedState`/`UNVERSIONED`，建议收敛到 core 类型（§3.5 P2）。
4. **设计决定（不迁移）已逐条标记为「已完成（设计决定，不迁移）」于 §3.4**，依据已写明（H2 由 `AnsiDialect` 承担、SPI 自动注册改产品名匹配、`objectMapper` 注入/连接所有权/`close` 随 `DataSource` 消失等）。
5. 剩余 dead_code helper（2026-10-22 静态复核修正）：**`delete_lock` 有调用方**（`jdbc_guard.rs:94`）；真正无生产调用方的是 `query_optional_int`（`backend/sqlite.rs:163`）、`slot_param`（`jdbc_agent_state_store.rs:630`）、`namespace_param`（`jdbc_store.rs:259`）。
6. **旧记录已更正（§3.3）**：会话表列名、锁表名两语言现已一致（`session_id` / `{prefix}distributed_locks`），早前「跨语言同库不兼容」结论**已失效**。
7. **跨语言同库不兼容（2026-10-22 新增）**：Java skill 表组（`skills` / `skill_resources`）与 Rust 无对应表 —— 即便 Rust 不实现 skill，两侧在同一数据库上互不冲突；但 Java 侧 `enableSkillTables(true)` 会建 `agentscope_skills` / `agentscope_skill_resources`，Rust 侧不会读/写它们。**跨语言 skill 能力不可互通**（若后续迁移 skill，需保证表名/DDL 与 Java 完全一致）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-jdbc` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-jdbc` / `cargo test -p agentscope-extensions-jdbc` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-jdbc/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态核对**：**未运行** `cargo build` / `cargo test`（按复核任务约束），**未修改任何 Rust 代码**。本节所有结论均来自对文件内容的逐条 `Select-String` / `grep`、逐文件行数统计与源码阅读。
- **复核对象**：Rust `crates/agentscope-extensions-jdbc/`（28 个 `src/*.rs` + 8 个 `tests/*.rs`）vs Java `agentscope-extensions/agentscope-extensions-jdbc/`（24 个主 `.java` + 19 个测试 `.java`）。
- **行数口径**：同时给出「非空行」（`Measure-Object -Line`，忽略空行）与「物理行」（`File.ReadAllLines().Count`，含空行）两个数；**后续所有模块统一采用非空行口径**（与 `EXTENSIONS_GAP_TRACKER.md` 现有 `xxxx / yyyy` 列一致）。
- **测试数口径**：`@Test` 按**严格匹配** `^\s*@Test\s*$` 计数（排除 `@Testcontainers` / `@TestMethodOrder` 等前缀同名注解）；Rust 按 `^\s*#\[(test|tokio::test)\]\s*$` 计数，并**区分 `src/` 内联与 `tests/` 目录**。

### 6.2 实测统计（2026-10-22）
| 侧 | 文件数 | 非空行 | 物理行 | 测试数 |
|---|---|---|---|---|
| Java 主代码 `src/main` | **24** | **5265** | 5731 | — |
| Java 测试 `src/test` | **19** | **4406** | 4907 | **175 `@Test`** + **5 `@ParameterizedTest`**（`CreateTableDdlParserTest` 3 个 `@MethodSource`（每方法多组 vendor DDL）+ `JdbcUnconditionalVersionTest` 2 个 `@ValueSource(booleans)`（各 2 次））→ 测试方法共 **180** 个，实际执行次数更高 |
| └ 其中 skill 相关 | 3 主 + 4 测 | 1313 主 + 1267 测 | 1426 + 1402 | 37 `@Test`（8 + 3 + 27 + 2，另 `DialectBuilderTableGroupsTest` 7 条同属 skill 表组） |
| Rust `src/` | **28** | **3882** | 4286 | **7** 内联（`lib.rs` 2 + `dialect/table_schema.rs` 5） |
| Rust `tests/` | **8** | **2356** | 2688 | **159**（`dialect_sql` 45 / `state` 36 / `native_lock` 21 / `store` 14 / `distributed_store` 13 / `sandbox` 13 / `snapshot` 10 / `table_schema_validation` 7） |
| Rust 合计 | 36 | **6238** | 6974 | **166** |
| Rust crate 测试 target 数 | — | — | — | **9**（lib + 8 个 `tests/` 文件） |

> **口径统一说明**：rollup（`EXTENSIONS_GAP_TRACKER.md` 第 7 行）记的「**146 test**」是 **2026-10-08** 的快照，**已过期**。当前实际为 **166**（= src 内联 7 + `tests/` 159）。本文档 §1.1/§3.1/§4.1 已全部改为 166；**rollup 需同步更新为 166**。Java 侧 rollup 记的「**3106 / 1558**」同样过期，实际为 **5265 / 4406**（非空行口径）。

### 6.3 对齐结论表
| 核对项 | 结论 | 依据 |
|---|---|---|
| Java 主/测行数 | **原文过期，已更正** | tracker 记 3106/1558；实测 5265/4406（24/19 文件） |
| Java `@Test` 数 | **原文过期，已更正** | tracker 记 98；实测 **175 `@Test`** + **5 `@ParameterizedTest`**（旧 98 是 skill 包加入前的快照） |
| Rust 文件清单/文件数/行数 | **已更正（补 2 个文件）** | 旧清单缺 `src/dialect/table_schema.rs` 与 `tests/table_schema_validation_tests.rs`；实测 src 28/3882、tests 8/2356 |
| Rust 测试数 | **已更正 146 → 166** | src 内联 7 + `tests/` 159；旧记录只算 lib 2 + `tests/` 144 |
| 「Java 类 → Rust 对应」映射存在性 | **大部分成立**；§2 表已补 4 行（`SkillDialect`/`SkillResourcesDialect`/`JdbcAgentSkillRepository`/`TableSchemaValidator`）；`AbstractJdbcDialect` 的「实现锁契约」一栏已修正（Rust trait **不含** `SandboxLockStrategy`） | 逐类读源核对 |
| P1-1 MySQL `GET_LOCK` 原生锁 | **符号存在且行为对齐，但**「已接到 dialect 上」**不成立** | `native_lock.rs:151,168` 策略实现完整、锁名归一与 Java 一致；但 `src/` 内 `impl NativeLockSession` = **0**，且方言类型**不**实现 `SandboxLockStrategy`（Java `AbstractJdbcDialect`/`MysqlDialect` 实现了）。→ §3.5 P1 |
| P1-2 PG `pg_try_advisory_lock` 原生锁 | 同上；另有**轮询语义差异** | `native_lock.rs:190,207` + `postgres_lock_key` 与 Java `composeLockKey` 一致；轮询下放适配层（Java 在策略内轮询至 deadline）→ §3.5 P1/P2 |
| P1-3 state 版本化 API | **已真正接线（且比原记录更强）** | 固有方法 `jdbc_agent_state_store.rs:205,211,225,267`；`VersionedState`/`UNVERSIONED` 定义于 `:38,43` 并导出；**core trait 已含版本化面**（`agentscope-core/src/state/store.rs:68,76,98`），本 crate `impl AgentStateStore` **已覆写** `supports_versioning`/`get_versioned`/`save_if_version`（`:565,569,590`）→ 旧「core trait 无版本化面」结论**已失效** |
| 4 个 bug 修复形态 | **全部可静态确认** | 见 6.4 |
| skill 包（`JdbcAgentSkillRepository` 等） | **完全未迁移（P0，本次新发现）** | `grep -ri skill` 在 Rust crate = 0 命中 |

### 6.4 4 个 bug 修复形态核验（静态）
| # | bug | 修复形态核验 | 结论 |
|---|---|---|---|
| 1 | session 表名递归自调 | `src/dialect/session_state_dialect.rs:32-41` `resolved_session_state_table_name()`：`None` 分支为 `format!("{}{}", prefix, base)`，**无自调** | ✅ **已修复** |
| 2 | `search` 前缀漏项 | `src/store/jdbc_store.rs:79-86` `namespace_path()`：每段后追加 `NS_SEPARATOR = '\u{001F}'`（含最后一段）；`namespace_prefix()`（`:92-94`）复用同一函数，`search`（`:208-214`）拼 `escape_like(prefix) + '%'` | ✅ **已修复** |
| 3 | `save_list` 批处理模板混跑 | `src/state/jdbc_agent_state_store.rs:429`（全量重写）与 `:452`（增量追加）**均改用 `execute_batch`**；`execute_prepared_batch` 在 `src/` 内**已无任何调用方**（仅 `backend/mod.rs:108` 定义 + `backend/sqlite.rs:133` 实现）；`:448-451` 有明确注释说明「两种模板混跑会损坏追加」 | ✅ **已修复** |
| 4 | 锁表 DDL 与 INSERT 列不匹配 | DDL：`src/dialect/abstract_dialect.rs:179-184` `lock_create_table_ddl()` = `(lock_name VARCHAR(255) NOT NULL PRIMARY KEY)`；INSERT：`src/sandbox/jdbc_guard.rs:68-78` = `INSERT INTO {t} (lock_name) VALUES (?)`（**已无 `acquired_at`**）；DELETE（`:81-88`、`:122-129`）与 `is_held`（`:98-108`）均只按 `lock_name` | ✅ **已修复** |

### 6.5 新发现缺口（详见 §3.5）
| 优先级 | 缺口 | 一句话 |
|---|---|---|
| **P0** | JDBC skill 仓库整体未迁移 | `JdbcAgentSkillRepository`（1095 行）+ `SkillDialect`（120）+ `SkillResourcesDialect`（98）+ builder 4 个 skill 开关 + 37 条未覆盖 `@Test`；Java skill 主代码占模块 24.9%。**因 Java 侧旧 `MysqlSkillRepository`/`PostgresSkillRepository` 已 `@Deprecated` 且官方替代正是它**，不宜简单标「不迁移」，需汇总方裁定归属 |
| **P1** | 原生 advisory lock 未真正接线 | 策略 + `NativeLockSession` 缝齐备，但 `src/` 内 **0** 个 `impl NativeLockSession`；`JdbcExecutor` 是共享单连接语义，无法表达「每 lease 独占连接」。端到端原生锁目前**无生产可用实现** |
| **P2** | PG advisory lock 轮询语义差异 | Rust 不轮询，超时完全下放 `NativeLockSession::try_acquire`；若适配层一次性返回 false，行为与 Java（轮询至 deadline）不等价 |
| **P2** | `VersionedState`/`UNVERSIONED` 双份定义 | 本 crate 副本与 `agentscope_core::state` 同名类型并存，调用方需转换 |
| **P2** | `dialect/mod.rs` 模块文档失真 | 声称 `AbstractJdbcDialect` 实现 `SandboxLockStrategy`，实际 trait 不含 |
| **P2** | `session_state_check_table_exists` 默认实现残留 | trait 默认值为 `SELECT 1 FROM t WHERE 1 = 0`（旧语义），4 个 vendor 已全部覆写，但新增 vendor 漏覆写会静默误判 |
| **P2** | 跨语言 skill 能力不互通 | Java `enableSkillTables(true)` 建的 `agentscope_skills`/`agentscope_skill_resources` 在 Rust 侧无对应读写 |

### 6.6 勘误列表（原文 → 实际）
| 位置 | 原文 | 实际 |
|---|---|---|
| §1 模块概览 | Java 主 **3106 行**（20 个 `.java`）/ 测 **1558 行**（11 个 `.java`）/ **98** 个 `@Test` | 主 **5265 非空行**（**24** 文件）/ 测 **4406 非空行**（**19** 文件）/ **175 `@Test`** + **5 `@ParameterizedTest`** |
| §1 当前状态 | `cargo test` = **146 passed** | **166 passed**（src 内联 7 + `tests/` 159） |
| §1.1 完工基线 | **146 passed**；test target **8** | **166 passed**；test target **9** |
| §1.1 | `tests/` 下 **7** 个 | **8** 个（新增 `table_schema_validation_tests.rs`） |
| §1.1 | Java `@Test` 总数 98（未变） | **175 + 5 `@ParameterizedTest`**（旧 98 为 skill 包加入前快照） |
| §2 表 | Java `AbstractJdbcDialect` 「实现 3 个 table 域接口 + 锁契约」 | Java 确实实现 `SandboxLockStrategy`；**Rust 的 `AbstractJdbcDialect` trait 不含**（拆为 `TableLockStrategy` / `native_lock.rs` 两策略） |
| §2 表 | 未列 `SkillDialect`/`SkillResourcesDialect`/`JdbcAgentSkillRepository`/`TableSchemaValidator` | 已补 4 行；前三者标记**未迁移（P0）** |
| §2 表 | `JdbcStore.Builder` → `JdbcStore::new`/`with_schema` | 成立；另 `JdbcStore::new(executor, dialect, initialize_schema)` 三参版存在（`jdbc_store.rs:39`） |
| §2.1 | `#[cfg(test)]`（`src/lib.rs` 2 个）+ `tests/*.rs` **6** 个 | `src/` 内联 **7** 个（+ `table_schema.rs` 5 个）+ `tests/` **8** 个文件（**159** test） |
| §3.1 | `dialect_sql_tests.rs`（**42**） | **45** |
| §3.1 | `state_tests.rs`（**31**） | **36** |
| §3.1 | `DialectSqlTests`（21/21 全覆盖） | Java `DialectSqlTests` 实为 **30** 条 `@Test`；Rust 有直接对应 **26** 条，未对应 **4** 条：3 条 skill 断言（`skillSqlIsNamespaceScoped`/`skillDdlsIndexAndCollateNamespace`/`skillStatementsNeverUpdateNamespace`）+ `postgresStateUpsertIncrementsVersion`（PG 状态 UPSERT `version = {}.version + 1` 缺断言，SQL 已含该子句）。另 `mysqlDialectIsLockStrategy` 的 Rust 对应用例只断言 `supports_native_locks()` |
| §3.1 | `AbstractJdbcDialectFromTest`（6/8） | Java 实为 **8** 条 `@Test`；Rust 覆盖 **6/8**（`throwsOnConnectionFailure`/`throwsForNullDataSource` 因无 `DataSource` 而无对应），**结论不变** |
| §3.1 | 未覆盖「`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条）」 | 各 **8** 条，合计 **16** 条 |
| §3.1 | 「离线单元测试」状态 = **已完成（离线可覆盖部分）** | **部分完成** —— skill 相关 37 条 + `DialectBuilderTableGroupsTest` 7 条因 skill 未迁移而未覆盖（属 P0 功能缺口，非环境受限） |
| §3.2 | Java `MysqlDialect.java:181-238` | 实际 `tryEnter` 在 **246-330** 行 |
| §3.2 | Java `PostgresDialect.java:182-252` | 实际 `tryEnter` 在 **231-330** 行 |
| §3.2 | Java `JdbcAgentStateStore.java:203,231,252` | 实际 `supportsVersioning` **148** / `getVersioned` **153** / `readVersion` **176** / `saveIfVersion` **203** |
| §3.2 | 「core `AgentStateStore` trait 无版本化面，故这些**只能**以固有方法暴露」 | **core trait 已含** `supports_versioning`/`get_versioned`/`save_if_version` + `VersionedState`/`UNVERSIONED`（`agentscope-core/src/state/store.rs:12,17,68,76,98`），本 crate 已覆写（`jdbc_agent_state_store.rs:565,569,590`） |
| §3.2 | 「`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条）」 | **16** 条（各 8） |
| §3.3 | dead_code 列表含 `TableLockStrategy::delete_lock` | `delete_lock` **有**调用方（`jdbc_guard.rs:94`）；真正无调用方的是 `query_optional_int`/`slot_param`/`namespace_param` |
| §3.3 | 「`read_version` 死代码待接线」 | `read_version` 已是**公开固有方法**（`jdbc_agent_state_store.rs:211`），已接线 |
| §3.3 | `TableSchemaValidator` 引用写作 `dialect/TableSchemaValidator.java`（`9c8b10d4e`） | 路径正确（`dialect/TableSchemaValidator.java`，475 非空行）；已迁到 `src/dialect/table_schema.rs` |
| §4.1 | 146 passed，逐 target `dialect_sql`=42 / `state`=31 / 无 `table_schema_validation` target | **166**：`dialect_sql`=45 / `state`=36 / `table_schema_validation`=**7**（新 target）/ src 内联 table_schema=**5** |
| §4.1 | crate 文件清单 26 个 `src/*.rs` + 7 个 `tests/*.rs` | **28** 个 `src/*.rs` + **8** 个 `tests/*.rs`（+`table_schema.rs` / `+table_schema_validation_tests.rs`） |
| §4.2 | 「测试覆盖 P0 缺口已关闭」 | **未完全关闭**（skill 37 条 + 表组 7 条未覆盖） |
| §4.2 | 「core `AgentStateStore` 无版本化面 … 需汇总方裁定」 | 已裁定并落地（core trait 已有版本化面），阻塞解除；遗留为双份类型定义 |
| §4.2 | dead_code 含 `delete_lock` | 见上（`delete_lock` 有调用方） |
| §4.2 | 「`MysqlIntegrationTest`/`PostgresIntegrationTest`（10 条 Testcontainers）」 | **16** 条 |
| §3.3 | 「建表后表结构校验」状态「已完成（2026-10-20）」 | 成立；新增 `tests/table_schema_validation_tests.rs`（7 条）+ `src/dialect/table_schema.rs` 内联 5 条，§3.1/§4.1 已补齐计数 |
| rollup（`EXTENSIONS_GAP_TRACKER.md` 第 7 行） | 「3106 / 1558」「146 test」「对照 Java 98 个 `@Test` 的 **P0 测试缺口已关闭**」 | 「**5265 / 4406**」「**166 test**」「对照 Java **175** 个 `@Test` + **5** 个 `@ParameterizedTest`；**P0 测试缺口未完全关闭（skill 37 条）**」——**建议状态改为「部分完成（skill 仓库未迁移，P0）」** |


## 7. P0 修复记录（2026-10-22）

### 7.1 结论

`JdbcAgentSkillRepository` 已按 §4.5 裁定 2 迁入 `agentscope-extensions-jdbc`：实现复用本 crate
的 `JdbcExecutor` / `BoundSql` / `AbstractJdbcDialect`，仓库契约实现
`agentscope_extensions_skills::AgentSkillRepository`，技能模型复用
`agentscope_harness::agent::skill::AgentSkill`。`agentscope-extensions-skills` 未作任何修改
（其 MySQL/PostgreSQL 后端建议后续标注 `deprecated`，本次未动）。

依赖方向：`jdbc -> skills -> harness/core`，无循环依赖。

### 7.2 新增/修改文件

| 文件 | 变更 |
|---|---|
| `src/dialect/skill_dialect.rs` | 新增 `SkillDialect` / `SkillResourcesDialect`（默认 ANSI 业务 SQL、表名解析） |
| `src/dialect/abstract_dialect.rs` | `AbstractJdbcDialect` 增加两个 skill 表域 supertrait；`TableNameConfig` 增加 skill 表名覆盖 |
| `src/dialect/detection.rs` | builder 增加 `skill_table_name` / `skill_resources_table_name` 覆盖 |
| `src/dialect/vendor/{ansi,mysql,postgres,sqlite}.rs` | 四家 vendor 的 skill/resource DDL（H2/ANSI、MySQL、PostgreSQL、SQLite） |
| `src/backend/mod.rs` | 新增 `JdbcSession` + `JdbcExecutor::in_transaction`（默认非事务降级） |
| `src/backend/sqlite.rs` | `RusqliteExecutor` 实现真正的 SQLite 事务（失败自动回滚） |
| `src/skill/{mod,repository}.rs` | 新增 `JdbcAgentSkillRepository`、校验函数、metadata 降级逻辑 |
| `src/lib.rs` | 导出 `skill` 模块、`JdbcSession` |
| `Cargo.toml` | 增加 `agentscope-extensions-skills` path 依赖（未改根 workspace 清单） |
| `tests/skill_repository_tests.rs` | 新增 32 个用例（CRUD / namespace / 校验 / 只读 / schema / DDL 对齐） |

### 7.3 与 Java 的方法级对照

| Java 方法/行为 | Rust | 状态 |
|---|---|---|
| 构造函数（namespace + writeable；skill 表组未启用则抛错） | `JdbcAgentSkillRepository::new/with_schema` | 覆盖（用 trait 约束替代 builder 的 `enableSkillTables` 开关） |
| `getSkill` / namespace 重载 | `get_skill`（trait）/ `get_skill_in` | 覆盖（缺失 → `NotFound`） |
| `getAllSkillNames` | `get_all_skill_names` / `..._in` | 覆盖（跳过非法名） |
| `getAllSkills` | `get_all_skills` / `..._in` | 覆盖（按 name 排序、跳过不可构建行、忽略孤儿资源） |
| `save` / namespace 重载 | `save`（trait）/ `save_in` | 覆盖（空列表 false；只读 false；force=false 冲突；force=true 覆盖资源） |
| `delete` / namespace 重载 | `delete` / `delete_in` | 覆盖（不存在 false；资源显式删除） |
| `skillExists` | `skill_exists` / `skill_exists_in` | 覆盖（空名 false，不做长度/路径校验，与 Java 一致） |
| `clearAllSkills` / namespace 重载 | `clear_all_skills` / `clear_all_skills_in` | 覆盖（返回删除行数；只读 0） |
| `getRepositoryInfo` | `get_repository_info` | 覆盖（`jdbc` + `<表>@<namespace>`） |
| `getSource` | `get_source` | 覆盖（`jdbc_<表>@<namespace>`） |
| `setWriteable` / `isWriteable` | `set_writeable` / `is_writeable`（`AtomicBool`） | 覆盖 |
| `close` | `close` | 覆盖（no-op） |
| `validateNamespace` / `validateSkillName` / `validateResourcePath` | 同名公开函数 | 覆盖（字符集、64/255/500 上限、`.`/`..`、分隔符、绝对路径、盘符、`..` 段） |
| `metadata_json` 序列化 / name+description 覆盖 | `serialize_metadata` / `deserialize_metadata` | 覆盖（null/非法 JSON 降级；SQL 列权威） |
| `createIfNotExist` / schema 校验 | `create_schema` / `verify_schema` | 覆盖（建表后校验；缺表/缺 `metadata_json` 列报错并附参考 DDL） |
| Java 生成键（`RETURN_GENERATED_KEYS`） | 插入后按 `(namespace, name)` 回查 `id` | 语义等价；`JdbcExecutor` 无生成键接口（见 7.5） |
| `metadata_json` 列缺失时的“降级” | **不降级**，schema 校验拒绝 | 与 Java 一致（Java 亦要求该列） |
| `skill_content` 独立字段 | 写入时取 `SKILL.md` 资源正文（缺省空串）；读取不使用 | **已修（2026-10-22）**：与 skills crate 一致，见 7.5 |
| `isUniqueViolation` 窄分类 | `is_skill_unique_violation` | 覆盖（只匹配 unique/duplicate/23505；NOT NULL/CHECK 不误报为冲突） |

未覆盖 / 无对应物：Java 的 `DataSource` 生命周期（Rust 由外部持有 `SharedExecutor`）；
`enableSkillTables` 构建期开关（Rust 改为 trait 约束 + 仓库显式建表）；Java 测试中的
H2 专属 `SET REFERENTIAL_INTEGRITY`（Rust 用 `PRAGMA foreign_keys` 等价覆盖）。

### 7.4 测试

命令：`cargo check -p agentscope-extensions-jdbc`、`cargo test -p agentscope-extensions-jdbc`

结果：crate 全量 **198 passed / 0 failed**，其中新增
`tests/skill_repository_tests.rs` **32 passed**。新增用例与 Java 测试的对应关系已在测试文件
的注释中逐条标注（`saveAndGetRoundTrip`、`forceSemantics`、`deleteSemantics`、
`skillExistsCases`、`duplicateWithinOneSaveFailsOnUniqueConstraint`、`clearAllSkills`、
`skillWithoutResources`、`sameNameCoexistsAcrossNamespaces`、`getAllSkillsIsNamespaced`、
`forceIsNamespaced`、`deleteAndClearAreNamespaced`、`invalidNamespacesRejected`、
`boundNamespaceConstructor`、`skillNameValidation`、`saveInputValidation`、
`readOnlyAndToggle`、`infoAndSource`、`pathShapedNamespacesRejectedAtConstruction`、
`defaultNamespaceConstant`、`missingColumnBlocked`、`degradedRowsAreTolerated`、
`legacyTraversalRowsRefusedOnRead`、`allVendorDdlsParse`、
`allVendorDdlsDeclareNamespacedUnique`、builder 前缀/表名覆盖、`skill_content` 取自
`SKILL.md` 正文）。

### 7.5 已知偏差 / 后续待办

1. **`skill_content`（已修，2026-10-22）**：harness `AgentSkill` 把技能正文折叠在 `SKILL.md`
   资源里；写入时现取 `path.ends_with("SKILL.md")` 的内容，找不到写空串，与
   `agentscope-extensions-skills` 的 mysql/postgresql 后端一致。读取路径本次未改（仍不把
   `skill_content` 还原成资源）。
2. **生成 id 回查**：`JdbcExecutor` 未暴露 generated-keys，插入后在事务内按
   `(namespace, name)` 回查。UNIQUE 约束保证唯一；若将来支持“改名 upsert”需重新评估。
3. **事务能力**：新增 `JdbcExecutor::in_transaction`；默认实现仍是非事务（与旧后端兼容）。
   目前仅 `RusqliteExecutor` 提供真正事务。MySQL/PostgreSQL 若接入真实后端，应覆盖该方法，
   否则资源插入失败无法回滚 skill 行。
4. **schema 缺列不降级**：与 Java 一致，但 task 中“老 schema 无 metadata_json 列时降级”的
   期望未实现——Java 的行为是校验期直接拒绝并给出 ALTER 提示。若需要自动降级需另立需求。
5. 四家 vendor 的 skill DDL 目前未接入 `AbstractJdbcDialect::create_table_ddls()`（保持既有
   “三个基表”断言不变）；仓库通过 `create_schema()` 自行建表，与其他组件的构造期建表一致。
6. `agentscope-extensions-skills` 的 `MysqlSkillRepository` / `PostgresSkillRepository`
   建议标注 `#[deprecated]`（本次硬约束禁止修改该 crate）。
## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。本 crate 的 list 状态同样走 core `compute_hash`（`DefaultHasher`/SipHash-1-3），与 Java `String.hashCode()` **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**；否则每次写入退化为全量重写（功能正确、性能退化）。
- **触发条件**：确需混部时再实现 Java `String.hashCode()` 的 32 位截断语义，并加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 算法——已落库 hash 会全部失配，触发全量重写风暴。

## 附录 B：`in_transaction` 后端覆盖要求（2026-10-22）

- `JdbcAgentSkillRepository` 的写入是**多语句**（先 skill 行 → 回查 id → 资源行），依赖 `JdbcExecutor::in_transaction`。
- 当前仅 `RusqliteExecutor` 实现真事务；trait 默认实现是**非事务**的（失败会留孤儿 skill 行）。
- **新增 MySQL/PostgreSQL executor 时必须覆盖 `in_transaction`**，否则不得用于 skill 仓库写入。