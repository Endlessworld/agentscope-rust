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
| 当前状态 | **总判：已收口（2026-10-31 二轮）**。执行后端齐备：`backend::RusqliteExecutor`（默认）+ `PostgresExecutor`（`postgres-executor`）+ **`MysqlExecutor`（`mysql-executor`，本轮新增）**；真实库集成测试全部 **env 门控**（PG `AGENTSCOPE_TEST_POSTGRES_URL` 5 条、MySQL `AGENTSCOPE_TEST_MYSQL_URL` 8 条），无 `#[ignore]`。历史基线（2026-10-28）：`cargo test` = **217 passed / 0 failed**；本轮离线新增：`driver/mysql.rs` 内联 4 条、`tests/distributed_store_tests.rs` +1、`tests/mysql_integration_tests.rs` 8 条（仅 `--features mysql-executor` 时编译）。剩余 P2 仅为 dead_code helper 与跨语言表名/表组差异（见 §4.2）。核实证据与口径见 §9 与文末「二轮收口记录」 |

### 1.1 基线（开工/完工实测）
| 项 | 开工基线 | 完工基线 |
|---|---|---|
| `cargo build -p agentscope-extensions-jdbc` | exit 0（9 warning） | exit 0（**2026-10-28：8 warning**，均为既有 unused import / dead_code helper（`query_optional_int`/`slot_param`/`namespace_param`）；新增文件无 warning，`cargo clippy --all-targets` exit 0） |
| `cargo test -p agentscope-extensions-jdbc` | **2 passed / 0 failed**（仅 lib）+ doc-tests 0 | **217 passed / 0 failed**（2026-10-28：src 内联 7 + `tests/` 209 + doc-tests 1；开工实测基线 **198**，详见 §8） |
| test target 数 | 1（lib） | **11**（lib + `tests/` 下 **10** 个）+ doc-tests |
| `crate` 内 `tests/` 目录 | 无 | `tests/{dialect_sql,store,state,snapshot,sandbox,native_lock,**native_lock_provider**,skill_repository,distributed_store,table_schema_validation}_tests.rs` |
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
| **`jdbc.dialect.table.SkillDialect`** | **skill 表方言（表名 `skills`；`namespace`+`name` 唯一；含 `metadata_json`）** | `dialect::skill_dialect`（`src/dialect/skill_dialect.rs`） | **已完成（2026-10-29 勘误）**：原记「无 / 未迁移（P0）」；实际 `SkillDialect` + `SkillResourcesDialect` 已落地，P0 于 2026-10-22 关闭；依据见 §7 / `tests/skill_repository_tests.rs`（32 条全绿）。原缺口描述保留：Java 120 非空行；当时 `grep -i skill` = **0 命中**（§3.5） |
| **`jdbc.dialect.table.SkillResourcesDialect`** | **skill 资源表方言（复合 PK `(id, resource_path)`；级联 FK 到 skill 表）** | `dialect::skill_dialect`（同上，`SkillResourcesDialect`） | **已完成（2026-10-29 勘误）**：原记「无 / 未迁移（P0）」；实际已随 `src/dialect/skill_dialect.rs` 落地；依据见 §7 / `tests/skill_repository_tests.rs`（32 条）。原缺口描述保留：Java 98 非空行（§3.5） |
| **`jdbc.skill.JdbcAgentSkillRepository`** | **JDBC 版 `AgentSkillRepository`（namespace 隔离 + 资源 CRUD）** | `skill::JdbcAgentSkillRepository`（`src/skill/{mod,repository}.rs`） | **已完成（2026-10-29 勘误）**：原记「无 / 未迁移（P0）」；实际已迁移（`src/skill/repository.rs`，含 `SkillDialect`/`SkillResourcesDialect` + builder 4 个 skill 开关），P0 于 2026-10-22 关闭，§7 记录方法级对照；依据见 §7 / `tests/skill_repository_tests.rs`（32 条全绿）。原缺口描述保留：Java **1095 非空行**（本模块最大单类）；当时 `grep JdbcAgentSkillRepository` = 0 命中（§3.5） |
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
| （测试支撑）`H2TestSupport` + 18 个有 `@Test` 的测试类（H2/SQLite/Testcontainers）+ **4 个 skill 相关测试类** | 离线/集成测试 | `RusqliteExecutor::in_memory()`（测试后端） | Rust 侧已建 **8** 个 `tests/*.rs` 离线用例（159 test + src 内联 7），详见 §3.1 逐类映射。**已完成（2026-10-29 勘误）**：原记「8 个 / 159 test」，实际 **10** 个 `tests/*.rs`（**209** test）+ src 内联 7 + doc-tests 1 = **217 passed / 0 failed**（2026-10-28）；依据 §1.1 / §3.1 / §8.3 |

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
| `io.agentscope.harness...sandbox.SandboxExecutionGuard` / `SandboxIsolationKey` / `SandboxLease` | 沙箱护栏与租约 | `agentscope_harness::agent::sandbox::{SandboxExecutionGuard, SandboxIsolationKey, SandboxLease}`；`JdbcSandboxExecutionGuard` 另 `impl agentscope_harness::agent::sandbox::SandboxGuard` | 2026-10-28：guard 投影已解决 —— harness 新增 `SandboxGuard` trait，本 crate 以固有方法委托实现（见 §4.2） |
| `io.agentscope.harness.agent.DistributedStore` | 分布式存储组合 | `agentscope_harness::agent::distributed_store::DistributedStore`（字段 `agent_state_store` + `isolation_scope` + `base_store` + `sandbox_snapshot_spec` + `sandbox_execution_guard`） | 2026-10-28：`to_harness_store()` 已填满 5 槽（见 §4.2） |
| `java.sql.SQLIntegrityConstraintViolationException` | 唯一键冲突判定 | `backend::is_unique_violation(&JdbcError)` | — |
| `InputStream` + `PreparedStatement#setBinaryStream` | 流式快照上传 | `&[u8]`（`RemoteSnapshotClient::upload`） | 非流式（整块内存差异，见 §3.2） |
| JUnit5 / Testcontainers 1.21.3 | 测试 | `#[cfg(test)]`（`src/lib.rs` 2 个 + `src/dialect/table_schema.rs` 5 个 = **7**）+ `tests/*.rs` **8** 个集成文件（**159** 个 `#[test]`/`#[tokio::test]`，含 `#[tokio::test]`）。**已完成（2026-10-29 勘误）**：实际 **10** 个集成文件 / **209** 个用例（新增 `native_lock_provider_tests.rs` 18 + `skill_repository_tests.rs` 32）；依据 §1.1 / §8.3 | 见 §3.1；未新增 dev-dependencies（`tokio` 已在 `[dependencies]`，`sha2` 已作 dev-dep） |

> **harness/core 复用契约**（对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1，不新建公共 store crate）：本 crate `impl` `agentscope_core::state::AgentStateStore`（`state/jdbc_agent_state_store.rs:224`）、`agentscope_harness::filesystem::BaseStore`（`store/jdbc_store.rs:113`）、`agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot/jdbc_remote_snapshot_client.rs:58`）、`SandboxSnapshotSpec`（`snapshot/jdbc_snapshot_spec.rs:53`）；`put_if_version` CAS、`StoreItem.version`、快照 id 校验均以 harness 为准。harness `DistributedStore` 仅取 `agent_state_store` + `isolation_scope`。**已完成（2026-10-29 勘误）**：原记「仅取 2 槽」，实际 2026-10-28 起 harness `DistributedStore` 含 5 槽（+ `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`），本 crate `to_harness_store()` 已填满；依据 §4.2 / `distributed_store_tests::to_harness_store_fills_every_composite_slot` |

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
| `JdbcAgentStateStore`（`impl AgentStateStore`） | `state/JdbcAgentStateStore.java` | **已完成（2026-10-29 勘误）**：原记「部分完成」，实际 7 个 trait 方法 + 版本化 API 双暴露均已完成 | 7 个 trait 方法全实现 + `create_table_if_not_exist`/`verify_table_exists`；**版本化 API 已以固有方法 + core trait 方法双暴露**（§3.2）；版本化用例实测 15 条（`tests/state_tests.rs`，共 36 条） |
| `JdbcRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | `snapshot/JdbcRemoteSnapshotClient.java` | 已完成 | upload/download/exists |
| `JdbcSnapshotSpec`（`impl SandboxSnapshotSpec`） | `snapshot/JdbcSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec` |
| `JdbcSandboxExecutionGuard` + 表锁策略 | `sandbox/JdbcSandboxExecutionGuard.java` | 已完成 | 仅 `TableLockStrategy`；**本轮修正**：`insert_lock` 与原 `lock_create_table_ddl` 列不一致（DDL 建 `lock_name PK` 但 INSERT 写 `(lock_name, acquired_at)`）→ 统一为 Java 的 `(lock_name) VALUES (?)`；`release`/`is_held` 已是真实探针（`is_held` 查锁表，非恒 false） |
| `JdbcDistributedStore` 组合者 | `JdbcDistributedStore.java` | 已完成 | 4 组件访问器 + `to_harness_store()`（仅投影半量，§4.2） |
| 无真实 DB 的内存后端 | Java H2/SQLite | 已完成 | `RusqliteExecutor::in_memory()` |
| **离线单元测试** | 18 个测试类，共 **175** 个 `@Test` + **5** 个 `@ParameterizedTest`（H2 为主 + SQLite + 2 个 Testcontainers 集成 + **4 个 skill 测试类 37 条**） | **已完成（2026-10-28：217 passed / 0 failed）** | 现状（2026-10-28）：`cargo test -p agentscope-extensions-jdbc` = **217 passed / 0 failed**（**src 内联 7** + `tests/` **209** + doc-tests **1**，**10** 个集成测试文件）。逐类映射（Rust target ← Java 类 / 用例名已写入每条测试注释）：<br>• `tests/dialect_sql_tests.rs`（**45**）← `DialectSqlTests`（Java 实为 **30** 条 `@Test`，Rust 有直接对应 **26** 条；未对应 **4** 条：3 条 skill 断言（`skillSqlIsNamespaceScoped`/`skillDdlsIndexAndCollateNamespace`/`skillStatementsNeverUpdateNamespace`）+ `postgresStateUpsertIncrementsVersion`（PG 状态 UPSERT 的 `version = {}.version + 1` 断言缺失，SQL 本身已含该子句但无用例断言）；`mysqlDialectIsLockStrategy` 的 Rust 对应用例只断言 `supports_native_locks()`，**方言本身不实现 `SandboxLockStrategy`**）+ `AbstractJdbcDialectFromTest`（**6/8**：`detectsPostgres/Mysql/MariaDb/H2/Sqlite` + 不支持库降级；`throwsOnConnectionFailure`/`throwsForNullDataSource` 对应 Rust 无 `DataSource` 的连接面，见 §3.4）。<br>• `tests/store_tests.rs`（**14**）← `JdbcStoreH2Test` 10/10 全覆盖 + 4 条 CAS 三分支/命名空间编码加固。<br>• `tests/state_tests.rs`（**36**）← `JdbcAgentStateStoreH2Test` 非版本化部分 13/13 + **15 条版本化用例**（实测 15 个 `#[tokio::test]` 命中版本化语义：`supports_versioning`/`get_versioned`/`read_version`/`save_if_version` 三分支 + 冲突不写 + 隔离/校验 + `UNVERSIONED` 单语句原子性 + version-0 自愈正/反向）+ 8 条隔离/校验/`:_hash` 加固。版本化用例打在 `JdbcAgentStateStore` 的**固有方法**与 **core trait 方法**两处（core trait 自 2026-10 起已有版本化面，见 §3.2）。<br>• `tests/snapshot_tests.rs`（**10**）← `JdbcRemoteSnapshotClientH2Test` 6/6 全覆盖 + `JdbcSnapshotSpec` 4 条（含 `getClient` 往返、id 校验）。<br>• `tests/sandbox_tests.rs`（**13**）← `JdbcSandboxExecutionGuardH2Test` 4/4 + `JdbcSandboxExecutionGuardSqliteTest` 2/2 + `DEFAULT_*` 常量/`with_key_prefix`/`release`/`is_held` 7 条（便携表锁路径）。<br>• `tests/native_lock_tests.rs`（**21**）← `MysqlDialect.tryEnter`/`PostgresDialect.tryEnter` 行为对齐：MySQL 锁名归一（短名/恰 64 字符/超长截断+摘要、长名互异）、PG advisory key 折叠、两策略 acquire/release/is_held/drop、超时与后端失败不 panic、guard 接线。SHA-256 期望值用独立字面量 + dev-dep 重算双向交叉验证。<br>• `tests/distributed_store_tests.rs`（**13**）← `JdbcDistributedStoreH2Test` 5/5 + 组件工厂/`to_harness_store()` 投影/共享句柄/schema 关闭 8 条。<br>• `tests/table_schema_validation_tests.rs`（**7**）← `CreateTableDdlParserTest`（11）+ `DialectBuilderSchemaValidationTest`（9）的离线等价覆盖（2026-10-20 新增）。<br>• `tests/native_lock_provider_tests.rs`（**18**，2026-10-28 新增）← `MysqlDialect.tryEnter` / `PostgresDialect.tryEnter` 的**端到端**路径：provider 开独占会话 → 策略轮询 → guard 租约（详见 §8）。<br>**未覆盖**：① skill 包 37 条 `@Test` 中的 **32 条已覆盖**（2026-10-22，见 §7 与 `tests/skill_repository_tests.rs`）；仍缺 `DialectBuilderTableGroupsTest` 7 条（表组开关）；② `MysqlIntegrationTest`/`PostgresIntegrationTest`（各 **8** 条，Testcontainers + Docker，环境受限）；③ `DialectBuilderTableGroupsTest`（7 条，表组开关，依赖 skill 表组，同 ①）。补测已全部离线（`RusqliteExecutor::in_memory()` + `SqliteDialect`），无新依赖、无网络。 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| MySQL 原生 `GET_LOCK`/`RELEASE_LOCK` 锁 | `dialect/vendor/MysqlDialect.java:246-330` | **已完成** | `dialect/native_lock.rs` 的 `MysqlNativeLockStrategy`。**未**在方言类型上 `impl SandboxLockStrategy`：`GET_LOCK` 是会话级锁，Java 靠每 lease 独占 Connection + `close()` 释放，而方言不持有连接（见该模块头注释）。锁名归一已移植：超 64 字符则截为 31 字符 + `:` + SHA-256 前 32 hex（防高基数碰撞，Java 注释明确弃用 32-bit `String.hashCode`）。路由缝 `NativeLockSession` + SQL 常量 `MYSQL_GET_LOCK_SQL`/`MYSQL_RELEASE_LOCK_SQL`。**静态复核补充**：`NativeLockSession` 目前**无任何生产实现**（`src/` 内 `impl NativeLockSession` = 0 命中，仅 `tests/native_lock_tests.rs` 有 `FakeSession`），即真实适配层尚未存在 —— 该缝与策略本身可用，但端到端原生锁**尚未真正接线**（见 §3.5 P1）。**2026-10-28 已关闭该缺口**：新增 `NativeLockSessionProvider`（异步 `open()` 返回独占会话）+ `SessionScopedLockSession`（独占会话视图，drop 时显式 `RELEASE_LOCK` 再关连接）+ `ExecutorNativeLockProvider`（由 `JdbcExecutor::open_native_lock_session` 提供会话）+ `sandbox::NativeLockGuard`（每次 acquire 开一个新会话）+ 策略层 `poll_acquire` 轮询。见 §7 |
| PostgreSQL 原生 `pg_try_advisory_lock`/`pg_advisory_unlock` | `dialect/vendor/PostgresDialect.java:231-330` | **已完成** | 同 `dialect/native_lock.rs` 的 `PostgresNativeLockStrategy`；`postgres_lock_key` 折叠 SHA-256 前 8 字节（与 Java 一致，碰撞 ~2^-32）。与 Java 差异：Java 在策略内轮询至 deadline，Rust 交给 `NativeLockSession::try_acquire(timeout)`；`NATIVE_LOCK_POLL_INTERVAL=100ms` 已导出供真实适配层。后端失败 → `LockOutcome::TimedOut`（guard 降为 no-op lease，不 panic，见 §3.4）。**静态复核补充**：同 MySQL 行，`NativeLockSession` 无生产实现，真实 PG 适配层缺失（§3.5 P1）。**2026-10-28 更新**：轮询已上移到策略层（`native_lock::poll_acquire` + `NATIVE_LOCK_POLL_INTERVAL`），与 Java `PostgresDialect.tryEnter` 的 `while (true) { pg_try_advisory_lock; sleep(100ms) }` 一致；适配层不再负责轮询。生产实现路径见 §3.5 P1 与 §8 |
| state 版本化写入/读取（`saveIfVersion`/`readVersion`/`supportsVersioning`） | `state/JdbcAgentStateStore.java:148,153,176,203` | **已完成** | `JdbcAgentStateStore` 新增固有方法 `supports_versioning()`/`read_version()`/`get_versioned()`/`save_if_version()` + `VersionedState`/`UNVERSIONED`（已导出）。**静态复核更新**：core `AgentStateStore` trait **已含**版本化面（`agentscope-core/src/state/store.rs:68,76,98`：`supports_versioning`/`get_versioned`/`save_if_version`，含 `agentscope_core::state::{VersionedState, UNVERSIONED}`），本 crate 的 `impl AgentStateStore` 已**同时覆写这 3 个 trait 方法**（`state/jdbc_agent_state_store.rs:565,569,590`），把固有方法结果转发给 core 类型。**因此 §4.2 的「core trait 无版本化面，待汇总方裁定」已失效**（见 §6 勘误）。**遗留**：本 crate 仍**自己重复定义**了一份 `VersionedState`/`UNVERSIONED`（`jdbc_agent_state_store.rs:38,43`），与 core 的同名类型并存 —— 双份定义是 P2 清理项（§3.5）。**2026-10-28 已收敛**：本 crate 删除自有副本，改为 `pub use agentscope_core::state::{VersionedState, UNVERSIONED};`，对外公开路径不变（`agentscope_extensions_jdbc::{VersionedState, UNVERSIONED}` 与 `::state::{...}`）；trait 方法内的转换代码也随之删除 |
| `saveIfVersion(UNVERSIONED)` 写后读原子性 | `state/JdbcAgentStateStore.java:203` | **已完成（2026-10-20）** | 原实现 `save_single()` + 独立 `read_version_at()`，并发下返回的版本可能属于别人的写入。现走 `upsert_returning_version`：方言 `session_state_upsert_returning_version` 用 `RETURNING version`（PG/SQLite）单语句完成；**MySQL/H2 无 `RETURNING`，降级为 UPSERT + 独立 `SELECT version`，代码注释与测试均明确标注为「已知非原子退化」**（版本仍单调、始终反映已提交数据，但不能断言它标识本次 payload）。用例：`state_tests.rs` |
| `saveIfVersion(0)` 幽灵 CAS 自愈 | `state/JdbcAgentStateStore.java:231` | **已完成（2026-10-20）** | 历史行可能停在 DDL 默认 `version = 0`（`session_state_insert` 省略 version 列，只有 list 元素走它），这类行 insert-if-absent 永远主键冲突 → 曾经永久不可写。现 `expected_version == 0 && affected != 1` 时执行 `cas_zero_version`：以 `WHERE version = 0` 再做一次 CAS，影响 1 行返回 1，否则仍返回 `UNVERSIONED`。**未改 DDL 默认值、无需数据迁移**；反向用例保证「version=3 时 save_if_version(0) 仍冲突且不写」，防止自愈退化成无条件覆盖 |
| MySQL 键列二进制排序（session/snapshot 表） | `2349533a1` | **已完成（2026-10-20）** | `dialect/vendor/mysql.rs`：会话状态表 `session_id`/`state_key`、快照表 `snapshot_id` 均显式 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`（表级默认保持 `utf8mb4_unicode_ci`，只影响非键列）。DDL 断言见 `tests/dialect_sql_tests.rs`。**存量库仍需手工 ALTER**，见 `docs/migrations/mysql_binary_collation.md` §5 |
| 快照上传流式绑定（避免整包入内存） | `snapshot/JdbcRemoteSnapshotClient.java`（`setBinaryStream`） | 待确认 | Rust `RemoteSnapshotClient::upload(&[u8])` 需整包驻留内存；大快照场景存在内存差异 |
| 真实 MySQL/PostgreSQL 集成测试 | `integration/MysqlIntegrationTest.java`（8 条）、`integration/PostgresIntegrationTest.java`（8 条） | **部分完成（2026-10-31）**：PostgreSQL 已落地（`AGENTSCOPE_TEST_POSTGRES_URL` 门控、未设置即跳过，见文末收口记录）；MySQL 受阻（驱动不可用） | Rust 仍无 mysql/pg 驱动与 Docker 集成；本轮新增的 `tests/` 仅覆盖离线 SQLite 路径。对应 **16** 条 Java `@Test` 映射见 §3.1（**旧记录「10 条」已过期**） |
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
| `SandboxExecutionGuard` 的 `impl` | `JdbcSandboxExecutionGuard implements SandboxExecutionGuard` | **已解决（2026-10-28）** | harness 新增 `SandboxGuard` trait（`crates/agentscope-harness/src/agent/sandbox/guard.rs:14`），`JdbcSandboxExecutionGuard` 已 `impl SandboxGuard`，四个方法全部委托既有固有方法（`try_enter`/`release`/`is_held`/`active_count`），行为不变。差异说明：harness `SandboxLease` 无释放钩子，故 impl 内部用 `trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<JdbcSandboxLease>>>>` 保留具体租约（持有方言 `LockLease`），`release(&key)` 弹栈 drop 才真正解锁；`try_enter` 未获取则返回 `SandboxLease::noop()`（与 Java 超时降级一致）。已由 `tests/distributed_store_tests.rs::to_harness_store_fills_every_composite_slot` 覆盖 |
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
| **JDBC skill 仓库整体未迁移**（`JdbcAgentSkillRepository` + `SkillDialect` + `SkillResourcesDialect`） | `skill/JdbcAgentSkillRepository.java`（**1095 非空行**，本模块最大单类）、`dialect/table/SkillDialect.java`（120）、`dialect/table/SkillResourcesDialect.java`（98）；`AbstractJdbcDialectBuilder` 的 `skillTableName`/`skillResourcesTableName`/`enableSkillTables`；`AbstractJdbcDialect.createTableDdls()` 的 skill 表组分支 | **已完成（2026-10-22，见 §7；本行旧结论已过期）** | **勘误（2026-10-28）**：本行原记「未迁移（P0）」已与仓库现状不符——`src/skill/{mod,repository}.rs`、`src/dialect/skill_dialect.rs` 已存在，`tests/skill_repository_tests.rs` **32 条用例全绿**，§7（2026-10-22 P0 修复记录）已记录搬迁过程与 Java 方法级对照表。以下为历史依据（保留）：Rust 侧当时 `grep -ri 'skill'` 命中 **0**（`src/` + `tests/` + `Cargo.toml` 全无）；全仓 `grep JdbcAgentSkillRepository` 亦 0 命中。Java skill 主代码合计 **1313 非空行**（占模块 5265 的 **24.9%**）+ 测试 **1267 非空行 / 37 条 `@Test`**（`SkillTableDialectH2Test` 8 / `SkillTableDialectSqliteTest` 3 / `JdbcAgentSkillRepositoryH2Test` 27 / `JdbcAgentSkillRepositorySqliteTest` 2）。**注意**：Java 侧 `MysqlSkillRepository`/`PostgresSkillRepository` 已标 `@Deprecated`，官方替代正是本 JDBC skill 仓库（见 `SKILLS__GAP_TRACKER.md` 的上游状态提醒），故**不宜简单标「不迁移」**。需汇总方裁定归属（并入本 crate vs 独立 crate）。**依赖前置**：需 harness `SkillRepository`/`AgentSkillRepository` trait + `AgentSkill`/`SkillResources` 模型 —— `agentscope-harness/src/agent/skill/repository.rs` 与 `agentscope-harness/src/agent/skill/model.rs` **已存在**，故技术上可落地；另需 `agentscope-extensions-skills` 的 `AgentSkillRepository` trait（`crates/agentscope-extensions-skills/src/repository.rs`）二选一对齐 |
| **原生 advisory lock 未真正接线**（策略与缝齐备，但无生产 `NativeLockSession` 实现） | `MysqlDialect.tryEnter`（独占 Connection）、`PostgresDialect.tryEnter`（独占 Connection + 100ms 轮询至 deadline） | **已完成（2026-10-28）** | `src/dialect/native_lock.rs` 的 `MysqlNativeLockStrategy`/`PostgresNativeLockStrategy` **已实现** `SandboxLockStrategy`（`try_enter`/`release`/`is_held`），锁名归一（MySQL 64 字符 + SHA-256）与 advisory key 折叠（PG SHA-256 前 8 字节）与 Java 一致，`normalize_lock_name`/`postgres_lock_key` 均为 `pub` 且被 `tests/native_lock_tests.rs`（21 条）覆盖。**但 2026-10-22 静态复核时** `grep 'impl NativeLockSession' src/` = **0 命中**：唯一实现是 `tests/native_lock_tests.rs:74` 的 `FakeSession`；`JdbcExecutor` 是「共享单连接」语义（`backend/mod.rs:88`），无法表达 Java「每 lease 独占连接」，故**当时**原生锁端到端路径无生产可用实现（该结论已由下迹 2026-10-28 接线解除）。本 crate 已提供缝与策略，**真实适配层需在本 crate 之外的驱动 crate（mysql/pg）落地**，或在本 crate 增 `feature` 化的连接独占能力。**2026-10-28 已接线（不再只有测试 fake）**：①`NativeLockSessionProvider`（`dialect/native_lock.rs`，异步 `open() -> NativeLockSessionFuture`，每次返回一个新独占会话）；②`SessionScopedLockSession`（独占会话视图：转发 + 记录持锁键，drop 时先显式 release 再关会话）；③`ExecutorNativeLockProvider`（把 `JdbcExecutor::open_native_lock_session` 包成 provider，驱动 crate 只需覆盖 executor 的那一个默认方法）；④`JdbcExecutor::open_native_lock_session` 默认方法（返回 `JdbcError::Unsupported`，`RusqliteExecutor` 显式覆盖并说明 SQLite 无 advisory lock）；⑤`sandbox::NativeLockGuard`（`NativeLockFlavor::{Mysql,Postgres}`，每次 `try_enter` 开新会话→建策略→轮询→租约持有策略，租约 drop/release 即释放锁并关连接）。未新增任何驱动依赖。测试：`tests/native_lock_provider_tests.rs` 18 条（见 §8） |
| **PG advisory lock 轮询语义差异** | `PostgresDialect.java:236-262`（deadline 内 100ms 轮询 `pg_try_advisory_lock`） | **已完成（2026-10-28）** | Rust `PostgresNativeLockStrategy::try_enter`（`native_lock.rs:208`）把超时**完全下放**给 `NativeLockSession::try_acquire(lock_key, timeout)`，自身不轮询；`NATIVE_LOCK_POLL_INTERVAL`（100ms）仅导出未使用。行为是否等价取决于适配层实现（若适配层一次性 `try_acquire` 即返回 false，则 Rust 会在 0 时间内失败而 Java 会等到 deadline）。**建议**：在适配层契约中明确「`try_acquire` 需内部轮询至 deadline」，或把轮询上移到策略内。**2026-10-28 已按后者落地**：新增 `native_lock::poll_acquire(session, key, deadline)`，两个策略的 `enter()` 统一走它——每次尝试传入「剩余预算」（阻塞型会话如 MySQL `GET_LOCK` 可在服务端等到 deadline；非阻塞型如 `pg_try_advisory_lock` 立即返回 `false` 后被轮询），轮询间隔用已导出的 `NATIVE_LOCK_POLL_INTERVAL`（100ms = Java `sleep(100)`）。`NativeLockSession::try_acquire` 的 `timeout` 文档已改为「单次尝试上界」，适配层不再负责轮询。反弹用例：`first_attempt_failure_is_retried_until_it_succeeds` / `unavailable_lock_polls_until_the_deadline_then_times_out` |
| **`VersionedState`/`UNVERSIONED` 双份定义** | — | **已完成（2026-10-28）** | core `agentscope_core::state::{VersionedState, UNVERSIONED}`（`agentscope-core/src/state/store.rs:12,17`）与本 crate 自定义的 `state::VersionedState`/`state::UNVERSIONED`（`jdbc_agent_state_store.rs:38,43`）**同名并存**且语义相同（`UNVERSIONED = -1`）。`impl AgentStateStore` 的 trait 方法已转发到 core 类型（`jdbc_agent_state_store.rs:565-611`），但固有方法仍返回本 crate 的副本 → 调用方需在两个类型间转换。**建议**：删本 crate 副本，全部改用 core 类型（本 crate 非公共 API 破坏性变更，可在本 crate 内完成）。**2026-10-28 已执行**：`jdbc_agent_state_store.rs` 删除自有 `pub const UNVERSIONED` / `pub struct VersionedState`，改为 `pub use agentscope_core::state::{VersionedState, UNVERSIONED};`——全仓仅剩一份定义，且 `agentscope_extensions_jdbc::{VersionedState, UNVERSIONED}`、`::state::{VersionedState, UNVERSIONED}` 两个既有公开路径保持不变；trait 方法里原先的逐字段转换代码随之删除 |
| **`dialect/mod.rs` 模块文档与实际 trait 组成不符** | Java `AbstractJdbcDialect implements ... SandboxLockStrategy` | **已完成（2026-10-31，见文末「2026-10-31 loop 收口记录」）** | `src/dialect/mod.rs:10-12` 声称「`AbstractJdbcDialect` implements all table-domain traits **and the [`SandboxLockStrategy`] contract**」，但 `src/dialect/abstract_dialect.rs:148-150` 的 trait 定义为 `StoreDialect + SessionStateDialect + SnapshotDialect + TableNameConfigurable + Send + Sync` —— **不含** `SandboxLockStrategy`（表锁已拆为 `sandbox::TableLockStrategy`，原生锁拆为 `native_lock.rs` 两个策略）。文档误导，建议改写 |
| **`SessionStateDialect` 默认 `session_state_check_table_exists` 与 Java 不一致** | `SessionStateDialect.java`（Java 无该默认方法，各 vendor 自行提供） | **已完成（2026-10-31，见文末「2026-10-31 loop 收口记录」）** | Rust trait 默认实现（`session_state_dialect.rs:52-57`）为 `SELECT 1 FROM {table} WHERE 1 = 0`（无参数），而 ANSI/MySQL/PG/SQLite **四个 vendor 均已覆写**为 `INFORMATION_SCHEMA`/`information_schema`/`sqlite_master` 形式并绑定表名参数。故默认实现**无实际调用方**，但它是「表不存在时 SELECT 返回 0 行」的旧语义残留，与 §3.2「表存在性校验」条目的修正方向相反 —— 若将来新增 vendor 未覆写该方法，会静默误判。建议把默认实现改为「查得列才算存在」或删除默认值强制实现 |
| **PG 状态 UPSERT 版本自增缺断言** | `DialectSqlTests.postgresStateUpsertIncrementsVersion` | **已完成（2026-10-31，见文末「2026-10-31 loop 收口记录」）** | Rust `tests/dialect_sql_tests.rs` 覆盖了 `postgres_store_upsert_uses_on_conflict`（KV store 的 `ON CONFLICT`）与 `mysql_state_upsert_and_check` / `h2_state_upsert_and_check`，但**缺**「PG 状态表 UPSERT 在冲突时把 `version` 自增」这一条断言。SQL 本身正确（`src/dialect/vendor/postgres.rs:116`：`SET state_data = EXCLUDED.state_data, version = {}.version + 1`），只是无用例守护 → 后续改 SQL 不会被测出。建议补 1 条断言（`dialect_sql_tests.rs`） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **最新会话主题（2026-10-28）：关闭 P1「原生 advisory lock 端到端接线」+ 两项 P2（PG 轮询上移、`VersionedState` 收敛）。** 实测：`cargo build -p agentscope-extensions-jdbc` exit 0；`cargo test -p agentscope-extensions-jdbc` = **217 passed / 0 failed**（开工基线实测 **198**：src 内联 7 + `tests/` 191，无 doc-test；旧记录 166 已过期，差额 32 = `tests/skill_repository_tests.rs`，该文件由 2026-10-22 会话落地）；`cargo clippy --all-targets` exit 0；`cargo fmt --check` 干净。完整明细与反向验证见 §8。
- **上一会话主题（2026-10-22）：静态复核会话**（**只做静态核对，未运行 `cargo build` / `cargo test`，未改任何 Rust 代码**）。核对结论见 §6「静态复核记录（2026-10-22）」；新发现缺口见 §3.5（**skill 包整体未迁移 = P0**）。本会话只修正本 tracker 的统计口径与过期结论。
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

> **2026-10-31 收口口径（优先于下文 1–7 项的历史描述）**：① PostgreSQL 执行后端已落地（`feature = "postgres-executor"`）+ PostgreSQL 集成测试（env 门控）已落地；② 原生锁的宿主装配点已落地（`JdbcDistributedStore::with_native_lock_guard`），端到端已有真实装配路径用例；③ §3.5 三项 P2（`dialect/mod.rs` 文档、`check_table_exists` 默认实现、PG 版本自增断言）全部关闭；④ ~~MySQL 执行后端 + MySQL 集成测试转「跨模块请求」~~ → **2026-10-31 二轮已全部落地**（`feature = "mysql-executor"` + `tests/mysql_integration_tests.rs`，见文末「二轮收口记录」）；⑤ `in_transaction` 前置约束的最终文档措辞已在 `src/backend/mod.rs` 定稿。明细见文末收口记录。

1. **测试覆盖缺口**：**P0 缺口未完全关闭**（2026-10-22 静态复核修正）——skill 相关 **37 条 `@Test`** 完全未覆盖（§3.5 P0）；`MysqlIntegrationTest`/`PostgresIntegrationTest`（各 **8** 条 Testcontainers 用例，需 Docker + mysql/pg 驱动，环境受限）；`DialectSqlTests` 中依赖真实 H2 `MERGE INTO` 执行语义的部分。Java 总计 **175 `@Test` + 5 `@ParameterizedTest`**，Rust 侧当前 **166 test**（口径见 §6）。**已完成（2026-10-29 勘误）**：原记「skill 37 条未覆盖 / P0 未关闭 / 166 test」，实际 skill 仓库已于 2026-10-22 落地（`src/skill/repository.rs`）、**P0 已关闭**，`tests/skill_repository_tests.rs` 32 条全绿；真实剩余为 `MysqlIntegrationTest`/`PostgresIntegrationTest`（各 8 条，Docker）与 `DialectBuilderTableGroupsTest` 7 条；实测 **217 passed / 0 failed**（2026-10-28）；依据 §1.1 / §3.1 / §7 / §8.3。
2. **功能缺口**：① **P1 原生 advisory lock 已接线（2026-10-28）**：`NativeLockSessionProvider` / `SessionScopedLockSession` / `ExecutorNativeLockProvider` / `JdbcExecutor::open_native_lock_session` / `sandbox::NativeLockGuard` + 策略层 `poll_acquire`，见 §3.5 与 §8；剩余仅为「驱动 crate 覆盖 `open_native_lock_session`」这一实现动作（本 crate 禁止引入 `mysql_async`/`tokio-postgres`/`sqlx`）。**2026-10-29 核实**：该动作至今无人做——`rg "NativeLockGuard|ExecutorNativeLockProvider" crates/` 只命中 `tests/native_lock_provider_tests.rs`，生产代码**零调用**，所以「已接线」应读作「seam 已就绪」，端到端仍缺宿主挂载 → **整条降为 P2**（见 §9）。② **P2 已关闭**：PG 轮询已上移到策略层；`VersionedState`/`UNVERSIONED` 已收敛到 core。③ **P2 仍待办**：`dialect/mod.rs` 文档与 trait 组成不符、`session_state_check_table_exists` 默认实现残留（§3.5）；**P0 skill 仓库已由 2026-10-22 会话完成**（§7），§3.5 旧结论已勘误。
3. **跨模块需求（不改 harness/core）**：(a) harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`，本 crate 已在 `JdbcDistributedStore` 内保留全量 4 组件并通过 `to_harness_store()` 输出子集，是否扩展 harness 类型（`EXTENSIONS_GAP_TRACKER.md` §2.2）待裁定；(b) ~~harness `SandboxExecutionGuard` 为具体 struct，本 crate 无法 `impl`（§3.4）~~ → **已完成（2026-10-29 勘误）**：原记「harness guard 为具体 struct 无法接入」，实际 2026-10-28 harness 新增 `SandboxGuard` trait（`agent/sandbox/guard.rs:14`），`JdbcSandboxExecutionGuard` 已 `impl SandboxGuard`（§3.4 / `to_harness_store_fills_every_composite_slot`）；(c) **core `AgentStateStore` 版本化面已裁定并落地**（2026-10-22 静态复核）：core trait 已含 `supports_versioning`/`get_versioned`/`save_if_version` + `VersionedState`/`UNVERSIONED`（`agentscope-core/src/state/store.rs:12,17,68,76,98`），本 crate 的 `impl AgentStateStore` **已覆写这 3 个方法**（`jdbc_agent_state_store.rs:565,569,590`）。**原「需汇总方裁定是否提升为 core trait 方法」的阻塞已解除**；**遗留**：本 crate 仍重复定义了一份同名 `VersionedState`/`UNVERSIONED`，建议收敛到 core 类型（§3.5 P2）。
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

> **2026-10-29 勘误（本节为 2026-10-22 静态快照，状态已变）**：**P0 skill 仓库 → 已关闭**（2026-10-22，`src/skill/repository.rs` + `tests/skill_repository_tests.rs` 32 条，见 §7）；**P1 原生 advisory lock → 已接线**（2026-10-28，`NativeLockSessionProvider` + `tokio` 轮询，见 §8）；**P2 `VersionedState` 双份定义 → 已收敛 core**；**P2 PG 轮询 → 已上移到策略层**。实测 **217 passed / 0 failed**（2026-10-28）。原 P0/P1/P2 描述保留作历史存档。

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
| §3.1 | 「离线单元测试」状态 = **已完成（离线可覆盖部分）** | **部分完成** —— skill 相关 37 条 + `DialectBuilderTableGroupsTest` 7 条因 skill 未迁移而未覆盖（属 P0 功能缺口，非环境受限）；2026-10-29 勘误：skill 仓库已在 jdbc crate 落地、P0 已关，仅剩 `DialectBuilderTableGroupsTest` 7 条与 Docker 集成测试 |
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
| rollup（`EXTENSIONS_GAP_TRACKER.md` 第 7 行） | 「3106 / 1558」「146 test」「对照 Java 98 个 `@Test` 的 **P0 测试缺口已关闭**」 | 「**5265 / 4406**」「**166 test**」「对照 Java **175** 个 `@Test` + **5** 个 `@ParameterizedTest`；**P0 测试缺口未完全关闭（skill 37 条）**」——**建议状态改为「部分完成（skill 仓库未迁移，P0）」**。**已完成（2026-10-29 勘误）**：原记「P0 未关闭 / 198 test」，实际 skill 仓库已迁移、P0 于 2026-10-22 关闭，实测 **217 passed / 0 failed**（2026-10-28）；依据 §1.1 / §7 / §8.3 |


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
3. **事务能力（2026-10-29 改口径）**：新增 `JdbcExecutor::in_transaction`，默认实现非事务，
   目前仅 `RusqliteExecutor` 覆盖为真事务。**这是「新增执行后端的前置约束」而非现存缺陷**：
   当前只有 SQLite 一种可执行后端且已覆盖，不存在「默认实现被使用」的实际路径；约束对象是
   将来接入 MySQL/PostgreSQL 的 executor —— 漏覆盖会导致资源插入失败时 skill 行无法回滚。
4. **schema 缺列不降级**：与 Java 一致，但 task 中“老 schema 无 metadata_json 列时降级”的
   期望未实现——Java 的行为是校验期直接拒绝并给出 ALTER 提示。若需要自动降级需另立需求。
5. 四家 vendor 的 skill DDL 目前未接入 `AbstractJdbcDialect::create_table_ddls()`（保持既有
   “三个基表”断言不变）；仓库通过 `create_schema()` 自行建表，与其他组件的构造期建表一致。
6. `agentscope-extensions-skills` 的 `MysqlSkillRepository` / `PostgresSkillRepository`
   建议标注 `#[deprecated]`（本次硬约束禁止修改该 crate）。
## 8. 执行记录（2026-10-28）：原生 advisory lock 端到端接线 + 两项 P2

### 8.1 设计取舍（provider 契约）

| 要点 | 决定 | 理由 |
|---|---|---|
| 独占会话由谁提供 | `trait NativeLockSessionProvider { fn open(&self) -> NativeLockSessionFuture<'_> }`，异步返回 `Result<Box<dyn NativeLockSession>, NativeLockError>` | 真实驱动开连接是异步的；`open()` 每次必须给**新**会话——`GET_LOCK`/`pg_try_advisory_lock` 是会话级，PG 还是可重入的，共享连接会让两个 guard 同时「持有」同一把锁（Java 靠每 lease 一个 `Connection` 达到同样隔离） |
| 为什么不另立 `JdbcExclusiveConnection` | 不新增该 trait | `Box<dyn NativeLockSession>` 本身就是「独占连接」抽象；再立一个只会在驱动层多一层同义实现（任务里该项为「若有必要」） |
| 策略仍是同步的 | **2026-10-28 更新（M2）**：`SandboxLockStrategy` 新增**单次尝试**原语 `try_enter_once(lock_name, attempt_timeout)`；轮询上移到 trait 的 `try_enter_async(lock_name, timeout_seconds)`（`tokio::time::sleep(DEFAULT_LOCK_POLL_INTERVAL)`，不再有任何 `std::thread::sleep`）。`try_enter(...)` 保留为**同步单次**兼容入口，`TableLockStrategy` 仍提供阻塞式覆盖供同步调用方使用 | 同步轮询会把 Tokio worker 睡死（M2 缺陷根因）；异步层轮询让 `NativeLockGuard::try_enter` / `JdbcSandboxExecutionGuard::try_enter_async` 真正 yield |
| 轮询归属 | `SandboxLockStrategy::try_enter_async`（默认实现），间隔 `DEFAULT_LOCK_POLL_INTERVAL` = 100ms（= `NATIVE_LOCK_POLL_INTERVAL`） | 对齐 Java `PostgresDialect.tryEnter` 的 100ms 轮询；MySQL 侧把「剩余预算」传给会话，阻塞型 `GET_LOCK` 一次等到 deadline，语义仍与 Java 一致 |
| 失败语义 | **2026-10-28 更新（M3）**：deadline 到期 → no-op 租约（`is_acquired() == false`）；**后端故障 → `LockOutcome::Failed` / `NativeLockError::Failed` 向上返回，绝不折叠成 `TimedOut`**。`JdbcSandboxExecutionGuard::try_enter_async` 与 `NativeLockGuard::try_enter_checked` 返回 `Err`；同步无错误通道的 `try_enter` 记 `error!` 日志后降级 | 把坏掉的锁后端当成「锁被占用」会让多个进程在无保护状态下并行跑同一 isolation key |
| SQLite | `RusqliteExecutor::open_native_lock_session` 显式返回 `JdbcError::Unsupported(...)`；`JdbcExecutor` 默认实现同样返回 `Unsupported`（含 `type_name`） | SQLite 无 advisory lock；默认方法保证既有 `impl JdbcExecutor` 实现方无需改动 |

### 8.2 改动文件（仅本 crate）

| 文件 | 变更 |
|---|---|
| `src/dialect/native_lock.rs` | 新增 `NativeLockSessionProvider` / `NativeLockSessionFuture` / `SessionScopedLockSession` / `ExecutorNativeLockProvider`；策略改为 `try_enter_once`（单次尝试）+ trait 默认 `try_enter_async`（`tokio::time::sleep` 轮询）；`try_acquire` 的 `timeout` 文档为「单次尝试上界」；**2026-10-28（M2/M3）**：删除同步 `poll_acquire`/`enter`（`std::thread::sleep`），`NativeLockError::Failed` 映射为 `LockOutcome::Failed` 而非 `TimedOut` |
| `src/backend/mod.rs` | `JdbcExecutor` 新增默认方法 `open_native_lock_session() -> JdbcResult<Box<dyn NativeLockSession>>`（默认 `Unsupported`，带 `type_name::<Self>()`） |
| `src/backend/sqlite.rs` | `RusqliteExecutor` 覆盖为明确 `Unsupported`（说明 SQLite 无 advisory lock，MySQL/PG 由驱动层 executor 提供） |
| `src/sandbox/native_lock_guard.rs`（新增） | `NativeLockFlavor` / `NativeLockGuard`（+ `ProviderBackedGuard` 别名）/ `NativeLockGuardLease` / `from_executor` |
| `src/sandbox/jdbc_guard.rs` | 抽出 `normalize_key_prefix` / `compose_lock_name` 供两种 guard 共用（行为不变） |
| `src/sandbox/mod.rs`、`src/dialect/mod.rs`、`src/lib.rs` | 导出新类型 |
| `src/state/jdbc_agent_state_store.rs` | 删除自有 `VersionedState`/`UNVERSIONED`，改为 `pub use agentscope_core::state::{VersionedState, UNVERSIONED};`，trait 方法内转换代码删除 |
| `tests/native_lock_provider_tests.rs`（新增 18 条） | 见 8.3 |

未改根 `Cargo.toml`、未改 core/harness/其它 crate、未新增依赖（`futures`/`tokio`/`sha2` 均已在既有依赖表）。

### 8.3 新增测试（18 条，全离线，0 ignored）

| 用例 | 断言 |
|---|---|
| `mysql_guard_acquires_the_normalized_lock_name_through_a_provider` | 长锁名经 MySQL 归一（64 字符 + SHA-256 后缀）后落到会话；`opens == 1` |
| `postgres_guard_acquires_the_decimal_advisory_key_through_a_provider` | 锁标识为十进制 advisory key |
| `first_attempt_failure_is_retried_until_it_succeeds` | 第一次 `Ok(false)` → 轮询后成功，两次尝试同一 key |
| `unavailable_lock_polls_until_the_deadline_then_times_out` | 忙锁 1s 超时：尝试次数 ≥ 5、返回 no-op 租约、会话被关闭（不泄漏） |
| `zero_timeout_makes_a_single_attempt` | deadline 已过时只探测一次（不空转） |
| `releasing_the_lease_releases_the_lock` | 显式 release 发一次释放且幂等；会话在租约 drop 时才关 |
| `dropping_the_lease_releases_the_lock_and_closes_the_session` | drop → 释放 + 关会话 |
| `acquire_release_acquire_succeeds_through_the_guard` | 3 轮 acquire/release：3 次开、3 次关会话（可重复获取） |
| `a_second_guard_cannot_hold_the_same_lock` | 互斥：另一会话拿不到；释放后可获取 |
| `session_scoped_view_forwards_and_dedups_reentrant_acquires` | 同会话 PG 可重入：两次 acquire 都成功、持锁键去重、一次 release 清空 |
| `dropping_the_session_view_releases_what_it_still_holds` | 视图 drop 时对残余持锁键逐个显式释放再关会话 |
| `rusqlite_executor_reports_unsupported_native_lock_sessions` | `Unsupported` 文案含 SQLite / advisory / MySQL-PostgreSQL |
| `default_executor_reports_unsupported_with_its_own_type_name` | trait 默认方法返回 `Unsupported` 且含 `type_name`（不覆盖也不破坏既有 impl） |
| `executor_backed_provider_turns_unsupported_into_a_failed_session` | `ExecutorNativeLockProvider` 把 `JdbcError` 映射为 `NativeLockError::Failed` |
| `guard_over_a_sqlite_executor_degrades_to_a_noop_lease` | SQLite 上 guard 端到端降级为 no-op 租约 |
| `provider_open_failure_returns_a_noop_lease` | 开会话失败不 panic、不关不存在的会话 |
| `guard_composes_the_same_lock_names_as_the_jdbc_guard` | 与 `JdbcSandboxExecutionGuard` 同名规则；默认前缀/超时；空前缀报错 |
| `provider_backed_guard_alias_is_the_same_type` | `ProviderBackedGuard` 别名 + `new()` 构造 |

### 8.4 验证命令与结果

- `cargo build -p agentscope-extensions-jdbc` → exit 0（8 warning，全部为既有项）
- `cargo test -p agentscope-extensions-jdbc` → exit 0，**217 passed / 0 failed / 0 ignored**：lib 7 / `dialect_sql_tests` 45 / `distributed_store_tests` 13 / `native_lock_provider_tests` **18（新增）** / `native_lock_tests` 21 / `sandbox_tests` 13 / `skill_repository_tests` 32 / `snapshot_tests` 10 / `state_tests` 36 / `store_tests` 14 / `table_schema_validation_tests` 7 / doc-tests **1（新增：`NativeLockGuard` 的 `no_run` 示例）**
- `cargo clippy -p agentscope-extensions-jdbc --all-targets` → exit 0（无新增 warning）
- `cargo fmt -p agentscope-extensions-jdbc -- --check` → 干净

### 8.5 反向验证（把轮询改成一次性）

把 `native_lock::poll_acquire` 的 `Ok(false) | Err(NativeLockError::TimedOut)` 分支临时改为 `return Ok(false)`（即一次性尝试），再跑 `cargo test -p agentscope-extensions-jdbc --test native_lock_provider_tests --test native_lock_tests` → **FAILED，15 passed / 2 failed**，失败用例：

1. `first_attempt_failure_is_retried_until_it_succeeds` —— 第二次尝试从未发生，租约 `is_acquired()` 为 false；
2. `unavailable_lock_polls_until_the_deadline_then_times_out` —— 尝试次数只有 1，`attempt_count() >= 5` 断言失败。

恢复后重跑全绿（217/0）。既有 `tests/native_lock_tests.rs` 21 条在两种实现下都通过（它们是单次成功/失败语义，不涉及轮询），说明新增用例确实是为「轮询」这一行为提供守护。

### 8.6 未完成项 / 边界

- 驱动层 `open_native_lock_session` 的真实实现（mysql/postgres executor）不在本 crate：受「不得引入 `mysql_async`/`tokio-postgres`/`sqlx`」约束，需在驱动 crate 覆盖该默认方法（每 lease 一个独占连接）。
- `JdbcSandboxExecutionGuard` 仍是同步 API（未改）；`NativeLockGuard` 为 `async fn try_enter`。
- `NativeLockGuard` 不提供跨租约的 `release`/`is_held`：锁归属租约的独占会话，跨会话探测既不准也无必要（已在 rustdoc 说明）。
- §3.5 另两项 P2（`dialect/mod.rs` 文档、`session_state_check_table_exists` 默认实现）本轮未动。

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

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位 + `SandboxGuard` trait（`crates/agentscope-harness/src/agent/distributed_store.rs`、`.../sandbox/guard.rs`）。本 crate 的字面量构造（`to_harness_store()`）因缺字段编译失败（E0063），改为 **builder** 写法并顺带接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `Arc<JdbcAgentStateStore>` | 会话状态 |
| `isolation_scope` | `IsolationScope` | `JdbcDistributedStore::isolation_scope` | 默认 `User`，可用 `with_isolation_scope` 覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `Arc<JdbcStore>` | 工作区 KV |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc<JdbcSnapshotSpec>` | 沙箱快照 BLOB |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc<JdbcSandboxExecutionGuard>` | 表锁策略；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_harness_store()`：字面量 → `DistributedStore::builder()`，填满 5 槽（不再用 `..Default::default()`）。
- `src/sandbox/jdbc_guard.rs`：新增私有字段 `trait_leases`（保留具体 `JdbcSandboxLease`，否则租约提前 drop 会释放分布式锁）+ `impl SandboxGuard`（四方法委托固有方法）。
- 测试：`tests/distributed_store_tests.rs::to_harness_store_fills_every_composite_slot`（断言 5 槽填充 + `Arc<dyn SandboxGuard>` 的 `try_enter`/`is_held`/`active_count`/`release` 可调用）。

### 验证

`cargo test -p agentscope-extensions-jdbc` → **217 passed / 0 failed**（lib 7 + tests 209 + doc-test 1）。

### 未完成 / 语义差异

- 无留空槽位。
- `active_count()` 语义为「本 guard 经 trait 路径持有的租约数」（方言表锁无全局计数），与 Java 的进程内计数口径一致。

---

## 9. 2026-10-29 缺口核实（P1 口径修订）

> 对 §1 总判行列的 3 项 P1 逐条验源码。结论：**2 项真实、1 项被写重了**，另有 1 项应降级。
> 所有证据均可用文中命令复现。
>
> **2026-10-31 状态（本节 §9.1–§9.4 保留为当时证据，结论已变）**：§9.1 的 PostgreSQL 一半已关闭（`feature = "postgres-executor"`，见文末收口记录），MySQL 一半转跨模块请求；§9.2 的「生产代码零调用」已关闭（`JdbcDistributedStore::with_native_lock_guard` + `JdbcSandboxExecutionGuard::with_native_lock`）；§9.4 的 PostgreSQL 集成测试已改为 `AGENTSCOPE_TEST_POSTGRES_URL` 门控落地（MySQL 同 §9.1）。

### 9.1 缺口 1（真实，保持 P1）：无 MySQL/PG 执行后端

```text
rg "impl JdbcExecutor for" crates/
  crates/agentscope-extensions-jdbc/src/backend/sqlite.rs:136        <- 唯一真实实现
  crates/agentscope-extensions-jdbc/tests/state_tests.rs:148          <- 测试 fake
  crates/agentscope-extensions-jdbc/tests/native_lock_provider_tests.rs:501  <- 测试 fake
```

且 `crates/agentscope-extensions-mysql/Cargo.toml` 与 `-postgresql/Cargo.toml` **均不依赖本 crate**（无 jdbc 字样），
即没有任何一方提供 executor。后果：`JdbcStore` / `JdbcAgentStateStore` / `JdbcSnapshotSpec` /
`JdbcSandboxExecutionGuard` / `JdbcAgentSkillRepository` 在真实 MySQL/PG 上**无可用后端**，仅 SQLite 可跑。

**增量发现（更严重）**：`src/lib.rs:4-7` 宣称本 crate「Replaces the deprecated `agentscope-extensions-mysql` and
`agentscope-extensions-postgresql` modules … supports MySQL, PostgreSQL, H2, and SQLite」——
实际上①mysql/postgresql 两个 crate 仍在且各自独立实现（未被取代）；②H2 无后端；③MySQL/PG 无后端。
该 crate 级文档具有误导性，**已就地修正**。

### 9.2 缺口 2（真实，但由 P1 降为 P2）：原生锁无生产装配点

```text
rg "NativeLockGuard|ExecutorNativeLockProvider" crates/ -> 仅命中 tests/native_lock_provider_tests.rs
```

seam（`NativeLockSessionProvider` / `SessionScopedLockSession` / `ExecutorNativeLockProvider`）与策略、便捷入口
（`NativeLockGuard::from_executor`）均存在，18 条端到端用例也已覆盖，但**生产代码零调用**：
没有宿主挂载点，且与缺口 1 同根（没有真实 MySQL/PG executor 可包）。
⇒ 降为 **P2**：「seam 已就绪，缺宿主装配」。

### 9.3 缺口 3（被写重了）：`in_transaction` 不是当前缺陷

`src/backend/mod.rs:178` 默认实现确实非事务，`src/backend/sqlite.rs:196` 已覆盖为真事务。
但**当前只有 SQLite 一种执行后端且已覆盖**，不存在「默认实现被使用」的实际路径：
`src/skill/repository.rs:371/443/464` 的「写 skill 行 → 回查 id → 写 resource 行」事务包裹在 SQLite 上已生效。
⇒ 从 P1 改为「**新增执行后端的前置约束**」（trait 文档已有警告，§7.5-3 已改口径）。

### 9.4 缺口 4（真实，环境受限）：无真实容器集成测试

`crates/agentscope-extensions-jdbc/tests/` 共 10 个文件，全部基于 `RusqliteExecutor::in_memory()`；
`Cargo.toml` 无 `testcontainers`，测试中 `#[ignore]` **零命中**。Java 侧两个 `*IntegrationTest`（各 8 条）无对应物。

### 9.5 修订后的 JDBC 缺口口径

| 项 | 原口径 | 新口径 | 依据 |
|---|---|---|---|
| 无 MySQL/PG executor | P1 | **已关闭**：PostgreSQL 2026-10-31 一轮（`postgres-executor`），**MySQL 2026-10-31 二轮**（`mysql-executor`，`backend::driver::connect_mysql`） | §9.1、§10.4、文末二轮收口记录 |
| 真实容器集成测试 | P1 | **已关闭**：PG `AGENTSCOPE_TEST_POSTGRES_URL` / **MySQL `AGENTSCOPE_TEST_MYSQL_URL`**，均为 env 门控（未设置即 skip、无 `#[ignore]`、不引入 testcontainers） | §9.4、§10.3、§10.4 |
| 原生锁端到端接线 | P1（称「已接线」） | **已关闭（2026-10-31）**：`JdbcDistributedStore::with_native_lock_guard` + 真实装配路径用例 | §9.2、文末收口记录 |
| `in_transaction` 默认非事务 | P1 | **前置约束（非缺陷）；trait 文档已定稿（2026-10-31）** | §9.3、文末收口记录 |
| `lib.rs` crate 文档失真 | 未记录 | **已修（2026-10-29），2026-10-31 再校正（MySQL 驱动不在 registry）** | §9.1 |

### 9.6 建议的收口路径（2026-10-31 已按此执行，仅第 3 条受限）

1. ~~给 `JdbcDistributedStore` 加 `native_lock_guard(flavor)` 便捷入口~~ **已完成**：`with_native_lock_guard(flavor)`（替换 guard）+ `native_lock_guard(flavor)`（旁路构造），两者都经 `JdbcSandboxExecutionGuard::with_native_lock` → `NativeProviderStrategy` → `ExecutorNativeLockProvider`，生产代码已有调用方，见文末收口记录；
2. 执行后端：已按 **(b)** 落地 PostgreSQL（`feature = "postgres-executor"`，`tokio-postgres 0.7`，workspace 已登记、`Cargo.lock` 已含）；MySQL 未采用 (a)/(b) —— 详见文末「MySQL 执行后端裁定」（跨模块请求）；
3. 容器集成测试：PostgreSQL 改为**环境变量门控**（`AGENTSCOPE_TEST_POSTGRES_URL`，未设置即跳过，不用 `#[ignore]`、不引入 `testcontainers`）；MySQL 同第 2 条。

## 2026-10-31 loop 收口记录

> **本会话硬约束**：**未运行** `cargo` / `rustc` / `clippy` / `cargo fmt`（任务要求），只改 `crates/agentscope-extensions-jdbc/**` 与本 tracker。下文「已完成」的代码项均给出**代码位置 + 离线用例名**；用例是否真绿需主智能体统一实测（见 §10.7 验证缺口）。Java 基准：`D:\IdeaProjects\agentscope-java` @ `1524b3191`。

### 10.1 本轮关闭项

| # | 项 | 状态 | 代码位置 | 离线用例 |
|---|---|---|---|---|
| 1 | **PostgreSQL `JdbcExecutor` 实现**（原 P1 缺口 1 的一半） | 已完成 | `src/backend/driver/{mod,worker,postgres}.rs`；feature `postgres-executor`；入口 `backend::driver::connect_postgres` / `postgres_executor_from_env` / `PostgresExecutor` | `tests/driver_executor_tests.rs` 全 15 条（用 `RecordingClient` 走真实装配入口 `DriverExecutor::connect`）；真实 PG 5 条见 §10.3 |
| 2 | **MySQL `JdbcExecutor` 实现** | **已完成（2026-10-31 二轮）** | `src/backend/driver/{mod,worker,mysql}.rs`；feature `mysql-executor`；入口 `backend::driver::{connect_mysql,mysql_executor_from_env,MysqlExecutor}`；原生锁走通用会话（每 lease 新建 `Conn`）+ `MYSQL_{GET,RELEASE,IS_USED}_LOCK_SQL` | `driver::mysql::tests` 4 条（参数映射 / `Params::Empty` / BLOB vs TEXT 按字符集判定 / 全 `Value` 变体解码）+ `tests/mysql_integration_tests.rs` 8 条（env 门控） |
| 3 | **环境变量门控集成测试**（替代 Testcontainers） | 已完成（PostgreSQL） | `tests/postgres_integration_tests.rs`（`AGENTSCOPE_TEST_POSTGRES_URL`） | 5 条：`base_store_round_trip` / `agent_state_round_trip` / `snapshot_upload_download_round_trip` / `native_advisory_lock_round_trip` / `a_failed_transaction_rolls_back` |
| 4 | **原生锁生产装配点**（原 P2：`NativeLockGuard`/`ExecutorNativeLockProvider` 零调用） | 已完成 | `src/sandbox/native_lock_guard.rs`：`NativeProviderStrategy`（`impl SandboxLockStrategy`，每次获取开新会话）+ `JdbcSandboxExecutionGuard::with_native_lock`；`src/distributed_store.rs`：`with_native_lock_guard(flavor)` / `native_lock_guard(flavor)` | `tests/native_lock_wiring_tests.rs` 7 条，其中 `the_distributed_store_projects_a_native_lock_guard` 走 `JdbcDistributedStore::create_with_dialect_and_schema → with_native_lock_guard → to_harness_store → Arc<dyn SandboxGuard>::try_enter/release` **真实装配链**，不是内部字段注入 |
| 5 | §3.5 P2「`dialect/mod.rs` 文档与 trait 组成不符」 | 已完成（文档类，无需测试） | `src/dialect/mod.rs:10-20`、`src/dialect/abstract_dialect.rs` 模块文档与 `AbstractJdbcDialect` rustdoc（明确**不含** `SandboxLockStrategy`，并列出 5 个 table-domain trait） | 依据源码：`abstract_dialect.rs` supertrait 列表 = `StoreDialect + SessionStateDialect + SnapshotDialect + SkillDialect + SkillResourcesDialect + TableNameConfigurable`；锁策略为独立类型 `sandbox::TableLockStrategy` / `dialect::{Mysql,Postgres}NativeLockStrategy` |
| 6 | §3.5 P2「`session_state_check_table_exists` 默认实现残留」 | 已完成 | 保留默认值但**定性为 fail-closed 哨兵**并写清理由（`src/dialect/session_state_dialect.rs:51-66`）；四个 vendor 均已覆写（`ansi.rs:108`/`mysql.rs:109`/`postgres.rs:98`/`sqlite.rs:98`） | `dialect_sql_tests::default_session_state_probe_is_fail_closed`（用测试内 `BareDialect` 真正走到默认实现：即便表存在也返回 0 行 ⇒ `verify_table_exists` 报缺表；并断言 PG 走 `information_schema`） |
| 7 | §3.5 P2「PG 状态 UPSERT 版本自增缺断言」 | 已完成 | `src/dialect/vendor/postgres.rs:106-125,127-150` | `dialect_sql_tests::session_state_upsert_writes_version_one_and_increments_on_conflict`（PG + SQLite 同断言）+ `postgres_upsert_can_return_the_new_version_atomically`（`RETURNING version` ↔ MySQL 必须为 `None`，即文档化的非原子降级） |
| 8 | §9.3 `in_transaction` 前置约束**最终措辞** | 已完成 | `src/backend/mod.rs:173-190`（改为英文「Precondition for multi-statement writers」，点名 `RusqliteExecutor` 与 `DriverExecutor` 均已覆盖，新增 executor **必须**覆盖，否则不得接 skill 仓库） | `driver_executor_tests::{in_transaction_wraps_the_work_in_begin_and_commit,in_transaction_rolls_back_when_the_work_fails,execute_batch_is_atomic}` |
| 9 | 表组开关（`DialectBuilderTableGroupsTest` 5/7 条） | 已完成（设计决定，不迁移） | 依据：Rust `AbstractJdbcDialect::create_table_ddls()` 只汇总 store→sessions→snapshots（`src/dialect/abstract_dialect.rs:182-188`），skill 表由 `JdbcSkillRepository::initialize`（`src/skill/repository.rs:151-156`）自行创建；**无** `enableSkillTables`/`storeTables`/`skillTables` 开关，故「组开关」语义消失。影响：宿主无法一次只建 skill 表；替代：按需构造对应组件（或 `JdbcSkillRepository::with_schema`） | `dialect_sql_tests::create_table_ddls_covers_the_three_store_owned_tables_only`（断言三张表在内、`skill` 不在内） |
| 10 | 缺口语义用例补测（Java 未对上部分） | 已完成 | 见 §10.6 新增行 | `skill_ddl_keeps_the_namespace_unique_key_and_mysql_collation`、`skill_statements_never_update_the_namespace` |

### 10.2 改动文件 / feature / 依赖

- **新增（仅本 crate）**
  - `src/backend/driver/mod.rs`（`SqlClient` 缝 + `DriverExecutor` + 事务闸门 + `open_native_lock_session` 适配 + `to_dollar_placeholders`）
  - `src/backend/driver/worker.rs`（**专用驱动线程** + `current_thread` 运行时 + 作业通道：组件是同步的、驱动是异步的，而组件又被 async 代码调用，`Runtime::block_on` 会 panic）
  - `src/backend/driver/postgres.rs`（`tokio-postgres` 实现）
  - `tests/driver_executor_tests.rs`(15) / `tests/native_lock_wiring_tests.rs`(7) / `tests/postgres_integration_tests.rs`(5)
- **修改（仅本 crate）**：`Cargo.toml`、`src/lib.rs`、`src/backend/mod.rs`（`JdbcError::Configuration` 变体 + 模块声明 + `in_transaction` 文档）、`src/dialect/{mod.rs,abstract_dialect.rs,session_state_dialect.rs}`、`src/distributed_store.rs`、`src/sandbox/native_lock_guard.rs`、`tests/dialect_sql_tests.rs`(+6)
- **feature**：`default = []`（**零新增依赖**）；`postgres-executor = ["dep:tokio-postgres"]`
- **依赖**：`tokio-postgres = { workspace = true, optional = true }`（根 `[workspace.dependencies]` 已有 `tokio-postgres = "0.7"`，`Cargo.lock:5225` 已解析该 **package** ⇒ 打开 feature 无新 crate、无需联网；首次构建只会在 lock 里给 `agentscope-extensions-jdbc` 的依赖表补一行 `tokio-postgres`）。未改根 `Cargo.toml`。
- **`JdbcError::Configuration`**：新增变体，用于「没配连接串」与「服务器拒绝」区分；全仓无外部 `match`（`rg "JdbcError" -g '!crates/agentscope-extensions-jdbc/**'` = 0 命中）⇒ 加法式变更安全。

### 10.3 环境门控集成测试怎么跑（PostgreSQL）

```powershell
# 1) 起一个一次性服务器（任选其一；不是 testcontainers，不带进依赖）
docker run --rm -e POSTGRES_PASSWORD=secret -p 55432:5432 postgres:16
# 2) 指向它
$env:AGENTSCOPE_TEST_POSTGRES_URL = "host=localhost port=55432 user=postgres password=secret dbname=postgres"
# 3) 只跑这 5 条
cargo test -p agentscope-extensions-jdbc --features postgres-executor --test postgres_integration_tests
```

- 变量未设置/为空 ⇒ `postgres_executor_from_env()` 返回 `JdbcError::Configuration`，每条用例打印 `skipped: ...` 后 `return` —— **不是** `#[ignore]`，默认构建下 `cargo test` 依然全绿；文件顶层 `#![cfg(feature = "postgres-executor")]`，不开 feature 时该 target 为空。
- 键名与语义：`AGENTSCOPE_TEST_POSTGRES_URL`（常量 `backend::driver::POSTGRES_URL_ENV`）。
- 用例用的表名与默认前缀一致（`agentscope_store`/`agentscope_sessions`/`agentscope_snapshots`），首次运行自动 `CREATE TABLE IF NOT EXISTS`；数据用 `now_millis()+next_seq()` 生成唯一键，可反复跑同一库。

### 10.4 MySQL 执行后端（**2026-10-31 二轮已落地**）

**结论（覆盖本节一轮的旧裁定）**：采用一轮的 **(b) 方案**，`mysql_async` 现已在本地 registry 中（`index.crates.io-1949cf8c6b5b557f/mysql_async-0.37.1`，由 `agentscope-extensions-mysql` 先行编译验证），所以本 crate 可直接落地：

- `src/backend/driver/mysql.rs`（feature `mysql-executor`）：`MysqlSqlClient` 实现 `SqlClient` 四个方法；`execute` 在**无参时走文本协议**（`BEGIN`/`COMMIT`/`ROLLBACK` 与部分 DDL 不能 prepare），有参时走 prepare + `affected_rows()`；`query` 先取 `columns_ref()` 再 `collect::<Row>()`，逐格按**声明字符集**解码（`TEXT` 与 `BLOB` 共用 `MYSQL_TYPE_BLOB`，仅 `character_set == 63` 为二进制 —— 否则 ASCII 快照会被误读为 `Text`，`JdbcRemoteSnapshotClient::download` 只收 `Blob`）。
- 错误映射保留 `code` + `SQLSTATE`（`is_unique_violation` 靠 `duplicate`/`23000` 识别）。
- 原生锁：复用通用 `DriverNativeLockSession`（每 lease 调 `SqlClient::connect` 新建独立 `Conn`），`flavor = Mysql` 时发 `GET_LOCK` / `RELEASE_LOCK` / `IS_USED_LOCK`，符合“会话级、随连接释放”的语义。
- 入口：`backend::driver::{connect_mysql, mysql_executor_from_env, MysqlExecutor, MYSQL_URL_ENV}`；`lib.rs` 同步导出。

```powershell
docker run --rm -e MYSQL_ROOT_PASSWORD=secret -e MYSQL_DATABASE=agentscope -p 33306:3306 mysql:8.0
$env:AGENTSCOPE_TEST_MYSQL_URL = "mysql://root:secret@127.0.0.1:33306/agentscope"
cargo test -p agentscope-extensions-jdbc --features mysql-executor --test mysql_integration_tests
```

URL **必须**带库名（路径段）：MySQL 方言用 `DATABASE()` 解析表名。用例用 `now_millis()+next_seq()` 生成唯一键，可反复跑同一库；`build_blocks_when_a_legacy_table_lacks_the_version_column` 会在断言后把 `version` 列加回去，保持库可重用。

**历史：为何一轮未写**（三条当时的证据，均已失效）：

1. `mysql_async` 由 `agentscope-extensions-mysql` 以 optional 声明，`Cargo.lock:3274` **已解析**（`name = "mysql_async"`）；
2. 但本机 cargo registry **没有它的源码**：`C:\Users\Endless\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f` 下 `mysql*` 无一命中，`registry\cache` 亦无 `.crate` ⇒ 打开该 feature 必须联网下载；本会话被禁止运行 cargo，无法 fetch，更无法编译验证；`sqlx` 同理（`Cargo.lock` 与 registry 都无）。
3. 不能照抄 API 盲写：`mysql_async` 的动态取值 API（`Row` 的索引/取值、`exec` 与 `Params::Positional` 组合）没有本地源码可核，写出来无法验证，违反「不臆造 API / 不把未验证的东西标成已完成」。

**影响（历史）**：当时 `JdbcStore` / `JdbcAgentStateStore` / `JdbcSnapshotSpec` / `JdbcSandboxExecutionGuard` / `JdbcAgentSkillRepository` 在**真实 MySQL** 上无可用后端；现已由本轮 `mysql-executor` + `tests/mysql_integration_tests.rs` 关闭。

**备选（历史，已选 (b)）**：

- **(a) 推荐**：`agentscope-extensions-mysql` 的所有者补 `impl JdbcExecutor for MysqlAsyncBackend`——该 crate 已依赖 `mysql_async`（`src/driver/real.rs` 里 `Opts::from_url` / `Pool::new` / `exec_drop(&sql, Params::Positional(vec![]))` / `Value::from` / `result.affected_rows()` 均为**已在该 crate 编译通过**的范式），只需补 `in_transaction`（BEGIN/COMMIT/ROLLBACK）与 `open_native_lock_session`（每 lease 一条独占连接，`GET_LOCK`/`RELEASE_LOCK`）。
- **(b)** 允许联网后，本 crate 加 `mysql-executor = ["dep:mysql_async"]` + `src/backend/driver/mysql.rs`（约 120 行）：`SqlClient` 的 4 个方法 + `Row` 解码；锁身份常量与 `NativeLockFlavor::Mysql` 已就绪，无需再动其它文件。**→ 已采用（2026-10-31 二轮，实际 300 余行，含解码分岐与 4 条离线内联用例）**。

### 10.5 `in_transaction` 前置约束（最终措辞，摘自 `src/backend/mod.rs:173-190`）

> **Precondition for multi-statement writers** — `JdbcAgentSkillRepository` writes `agentscope_skills` and then, using the id it read back, `agentscope_skill_resources`. An executor that does **not** override this method has no atomicity at all, so a failed resource insert leaves an orphan skill row. In this crate `RusqliteExecutor` and every `backend::driver::DriverExecutor` override it (`BEGIN` … `COMMIT` / `ROLLBACK` on one connection); any new executor **must** do the same, or the skill repository must not be wired to it.

### 10.6 Java 测试类 → Rust 用例映射（本轮新增/校正部分）

| Java 测试类 / 方法 | Rust 用例 | 状态 |
|---|---|---|
| `PostgresIntegrationTest`（8 条，Testcontainers） | `tests/postgres_integration_tests.rs` 5 条（store / state / snapshot / advisory lock / 事务回滚）；另 3 条（连接失败、并发锁竞争、CRUD 组合）合并进上述 5 条 | **已完成（env 门控）** |
| `MysqlIntegrationTest`（8 条，Testcontainers） | `tests/mysql_integration_tests.rs` 8 条：`dialect_detection` / `state_store_crud` / `kv_store_cas` / `snapshot_blob_round_trip` / `long_lock_name_is_sha256_normalised` / `build_blocks_when_a_legacy_table_lacks_the_version_column` / `skill_repository_round_trip` / `namespaced_skill_isolation`（env 门控，与 Java 用例 1:1） | **已完成（2026-10-31 二轮，env 门控）** |
| `DialectSqlTests.postgresStateUpsertIncrementsVersion` | `session_state_upsert_writes_version_one_and_increments_on_conflict`、`postgres_upsert_can_return_the_new_version_atomically` | **已完成** |
| `DialectSqlTests.skillDdlsIndexAndCollateNamespace` | `skill_ddl_keeps_the_namespace_unique_key_and_mysql_collation` | **已完成** |
| `DialectSqlTests.skillSqlIsNamespaceScoped` | `skill_statements_never_update_the_namespace` + 既有 `skill_repository_tests::same_name_coexists_across_namespaces` 等 5 条 | **已完成** |
| `DialectSqlTests.skillStatementsNeverUpdateNamespace` | `skill_statements_never_update_the_namespace` | **已完成** |
| `DialectBuilderTableGroupsTest.defaultGroups/skillOnly/bothGroups/offGroupsFailBuildAndRevertedSwitch` | `create_table_ddls_covers_the_three_store_owned_tables_only` | **已完成（设计决定，不迁移）** |
| `DialectBuilderTableGroupsTest.autoCreateFalseValidatesSkillGroupOnly` | 既有 `skill_repository_tests::create_if_not_exist_false_rejects_a_missing_schema` | **已完成（既有）** |
| `DialectBuilderTableGroupsTest.repositoryConsistencyCheck` | 既有 `skill_repository_tests::every_vendor_declares_namespaced_unique_and_default_table_names` | **已完成（既有）** |
| `DialectBuilderTableGroupsTest.reopenWithAutoCreateFalse` | `distributed_store_tests::reopen_with_auto_create_disabled_accepts_the_existing_schema`（同一 in-memory 库先开建表、再以 `initialize_schema=false` 重开：验证通过且旧数据可读；空库路径由 `schema_creation_can_be_disabled` 反向钉住）；MySQL 真库侧由 `mysql_integration_tests` 与 `agentscope-extensions-mysql/tests/mysql_jdbc_bridge.rs::reopening_with_auto_create_disabled_verifies_the_existing_schema` 覆盖 | **已完成（2026-10-31 二轮新增独立用例）** |
| `JdbcSandboxExecutionGuardH2Test` / `MysqlDialect.tryEnter` / `PostgresDialect.tryEnter`（宿主装配） | `tests/native_lock_wiring_tests.rs` 7 条（含真实装配链与 SQLite 降级） | **已完成（本轮新增装配面）** |
| 事务语义（Java 由 `JdbcAgentSkillRepository` 隐式依赖） | `tests/driver_executor_tests.rs` 3 条 + PG 集成 `a_failed_transaction_rolls_back` | **已完成** |

### 10.7 未关闭项与验证缺口

1. **未关闭**：~~MySQL 执行后端（§10.4）+ `MysqlIntegrationTest` 8 条；`DialectBuilderTableGroupsTest.reopenWithAutoCreateFalse` 无独立用例~~ → **2026-10-31 二轮全部关闭**（见文末二轮收口记录）。现仅余 dead_code helper（`query_optional_int`/`slot_param`/`namespace_param`）与 Java `DialectBuilderTableGroupsTest` 的表组开关（设计决定，不迁移，§10.1 #9）。
2. **验证缺口（重要）**：本会话**未跑 cargo**，因此以下三组命令需由主智能体实测后才可对外称「全绿」：
   - `cargo build -p agentscope-extensions-jdbc`（默认 feature，**不应**拉入任何驱动）
   - `cargo test -p agentscope-extensions-jdbc`（预期 = 既有 217 + 本轮 **28** 条离线新增：`driver_executor_tests` 15 + `native_lock_wiring_tests` 7 + `dialect_sql_tests` +6；`postgres_integration_tests` 5 条在未开 feature 时不编译进来）
   - `cargo build -p agentscope-extensions-jdbc --features postgres-executor`（只验证编译，跑测试需 §10.3 的服务器；无服务器时 5 条会打印 skipped 并返回）
3. 若 `--features postgres-executor` 编译报错，风险集中在 `src/backend/driver/postgres.rs` 的两个点（均已按本地 registry 源码 `tokio-postgres-0.7.18` 核对过）：`Client::{execute,query,batch_execute}` 的签名、`Row::try_get` 的 `Option<T>` 解码；`Type` 只按 `name()` 比较（不比较 `Type` 值，因为 `Type` 未实现 `Copy`）。

## 2026-10-31 二轮收口记录 — MySQL 执行后端 + MySQL 集成测试（JDBC）

> **本会话硬约束**：**未运行** `cargo` / `rustc` / `clippy`；只改 `crates/agentscope-extensions-jdbc/**`、`crates/agentscope-extensions-mysql/**` 与两份 tracker。Java 基准：`D:\IdeaProjects\agentscope-java` @ `1524b3191`。

### 11.1 本轮关闭项

| # | 项 | 状态 | 代码位置 | 用例 |
|---|---|---|---|---|
| 1 | **MySQL `JdbcExecutor` 实现**（原 P1，一轮转“跨模块请求”） | 已完成 | `src/backend/driver/mysql.rs`（feature `mysql-executor`）+ `mod.rs` 模块声明/导出；入口 `connect_mysql` / `mysql_executor_from_env`（`MYSQL_URL_ENV = AGENTSCOPE_TEST_MYSQL_URL`）/ `MysqlExecutor` | `driver::mysql::tests`（4，离线）+ `driver_executor_tests::the_mysql_session_speaks_get_lock_sql`（既有，锁身份） |
| 2 | **MySQL 原生锁会话** | 已完成 | 复用 `DriverNativeLockSession`（`flavor = Mysql`）：每 lease 新 `Conn`（**独占会话**）+ `GET_LOCK`/`RELEASE_LOCK`/`IS_USED_LOCK`；无需新代码分支 | `native_lock_provider_tests` / `native_lock_wiring_tests`（既有）+ 真库 `long_lock_name_is_sha256_normalised` |
| 3 | **`MysqlIntegrationTest` 8 条**（原 P1） | 已完成 | `tests/mysql_integration_tests.rs`（`AGENTSCOPE_TEST_MYSQL_URL` 门控，未设置打印 skip 并 `return`，**无 `#[ignore]`**） | 8 条，与 Java 用例 1:1（见 §10.6） |
| 4 | **`reopenWithAutoCreateFalse` 独立用例**（原“部分”） | 已完成 | `tests/distributed_store_tests.rs` | `reopen_with_auto_create_disabled_accepts_the_existing_schema` |
| 5 | **MySQL 侧 `JdbcDistributedStore` 便利构造**（原 §4.2 阻塞项） | 已完成（在 mysql crate，依赖方向 mysql → jdbc） | `crates/agentscope-extensions-mysql/src/jdbc_bridge/{mod,real,stub}.rs`（feature `jdbc-bridge`）：`MysqlJdbcStoreBuilder` 一行包装 `JdbcDistributedStore::create_with_dialect_and_schema` + `MysqlDialect`，默认启用 `GET_LOCK` guard（与 Java 默认一致） | `jdbc_bridge::tests`（4，离线）+ `tests/mysql_jdbc_bridge.rs`（3，env 门控） |

### 11.2 改动文件 / feature / 依赖

- **jdbc crate 新增**：`src/backend/driver/mysql.rs`、`tests/mysql_integration_tests.rs`
- **jdbc crate 修改**：`Cargo.toml`（`mysql-executor = ["dep:mysql_async"]` + optional 依赖）、`src/backend/driver/mod.rs`（模块/导出/示例）、`src/lib.rs`（导出 + 文档：MySQL 不再是“未捆绑驱动”）、`tests/distributed_store_tests.rs`（+1）
- **feature**：`default = []`；`postgres-executor = ["dep:tokio-postgres"]`；`mysql-executor = ["dep:mysql_async"]`
- **依赖**：`mysql_async = { workspace = true, optional = true }`（根 `[workspace.dependencies]` 已有 `mysql_async = "0.37"`，`Cargo.lock` 已因 `agentscope-extensions-mysql` 解析该包，本地 registry 源码 `mysql_async-0.37.1` / `mysql_common-0.37.3` 已在 ⇒ 打开 feature 只加依赖边，无需联网）。未改根 `Cargo.toml`。
- **API 用法均以本地 registry 源码核对**：`Conn::new(Opts)` / `Queryable::{query_drop,exec_iter}` / `QueryResult::{columns_ref,collect,affected_rows}` / `Row::{len,as_ref}` / `Column::{column_type,character_set}` / `Value::{NULL,Bytes,Int,UInt,Float,Double,Date,Time}` / `Params::{Empty,Positional}` / `Error::Server{code,message,state}`。

### 11.3 设计决定（需后续会话知晓）

1. **`TEXT` 与 `BLOB` 同属 `MYSQL_TYPE_BLOB`**，只能靠列字符集（`63 = binary`）区分；`decode_cell` 因此按列元数据而非“是否能解成 UTF-8”分支 —— 否则 ASCII 快照会被读成 `Text`，`JdbcRemoteSnapshotClient::download` 只收 `Blob`，快照往返会断。
2. **无参语句走文本协议**（`query_drop`）：`BEGIN`/`COMMIT`/`ROLLBACK` 与部分 DDL 不能 prepare；有参语句走 prepare，`affected_rows()` 取自 `QueryResult`（无参时取自连接的上一个 OK 包）。
3. **`UInt` 解码饱和到 `i64::MAX`；`Float`/`Double`/`Date`/`Time` 降级为文本**：组件表结构不用这些类型，不引入新枚举变体。
4. **锁会话 = 新连接**：`DriverExecutor::open_native_lock_session` 已调 `SqlClient::connect`，MySQL 侧无需覆盖，也不能复用主连接（同连接重入会让互斥失效）。
5. **`jdbc-bridge` 依赖方向为 mysql → jdbc**（jdbc 必须保持驱动/厂商无关）；feature 未开时 `MysqlJdbcStoreBuilder::build()` / `MysqlJdbcStore::connect()` 返回 `MysqlError::DriverUnavailable`，不返回假存储。

### 11.4 待主智能体验证

```powershell
cargo build -p agentscope-extensions-jdbc                      # 默认 feature：不应拉入任何驱动
cargo test  -p agentscope-extensions-jdbc                      # 既有 217 + 本轮离线 5（mysql.rs 4 + distributed_store 1）；mysql_integration_tests 不编译
cargo test  -p agentscope-extensions-jdbc --features mysql-executor --lib   # 4 条解码/参数用例
cargo build -p agentscope-extensions-jdbc --features mysql-executor          # 驱动层编译
cargo test  -p agentscope-extensions-mysql                     # jdbc_bridge 存根用例（feature 未开）
cargo build -p agentscope-extensions-mysql --features jdbc-bridge            # 桥接层编译
```

- 真库用例需 `AGENTSCOPE_TEST_MYSQL_URL`（URL 必带库名），未设置时上述 8 + 3 条接线目标仍为绿（skip）。
- **反向验证建议**（主智能体）：把 `column_is_binary` 改为恒 `false` ⇒ `blobs_and_text_share_a_wire_type_and_differ_by_charset` 必 FAILED；把 `to_params` 恒返回 `Params::Positional` ⇒ `empty_parameter_lists_become_params_empty` 必 FAILED。
- **残余风险**：`mysql_async` 的 `QueryResult::collect` 返回的 future 需 `Send`（`#[async_trait]` 要求）；已按 crate 设计（tokio 多线程）预期成立，若报 E0277 则改为 `while let Some(row) = result.next().await` 循环（同样只需一次性改动 `query`）。

