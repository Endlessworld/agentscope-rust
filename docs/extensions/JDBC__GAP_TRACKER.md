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
| 当前状态 | **部分完成**（`cargo build` + `cargo test` 通过；`cargo test` = lib **2 passed / 0 failed**，测试覆盖为 P0 缺口，见 §3.1） |

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
| `jdbc.dialect.vendor.MysqlDialect` | MySQL 方言（`ON DUPLICATE KEY UPDATE`、`LONGBLOB`、原生 `GET_LOCK`） | `dialect::vendor::MysqlDialect`（`src/dialect/vendor/mysql.rs`） | DDL/UPSERT 已迁移；**原生 `GET_LOCK` 未迁移**（见 §3.2） |
| `jdbc.dialect.vendor.PostgresDialect` | PG 方言（`ON CONFLICT DO UPDATE`、`BYTEA`、`pg_try_advisory_lock`） | `dialect::vendor::PostgresDialect`（`src/dialect/vendor/postgres.rs`） | DDL/UPSERT 已迁移；**原生 advisory lock 未迁移** |
| `jdbc.dialect.vendor.SqliteDialect` | SQLite 方言 | `dialect::vendor::SqliteDialect`（`src/dialect/vendor/sqlite.rs`） | — |
| `jdbc.dialect.vendor.H2Dialect` | H2 方言 | 无独立类型；由 `dialect::vendor::AnsiDialect` 承担（`detect_dialect` 对未知/`h2` 默认返回 Ansi） | 见 §3.4 |
| `jdbc.dialect.package-info` / `dialect.table.package-info` / `dialect.vendor.package-info` | 包文档（无逻辑） | 无 | — |
| `META-INF/services/io.agentscope.extensions.jdbc.dialect.AbstractJdbcDialect` | JDK SPI 方言注册表 | 无 | Rust 无 SPI 自动注册（见 §3.3） |
| `jdbc.store.JdbcStore` | JDBC `BaseStore`（零内联 SQL） | `store::JdbcStore`（`src/store/jdbc_store.rs`，`impl agentscope_harness::filesystem::BaseStore`）+ `store::store_value` | `get`/`put`/`put_if_version`/`search`/`delete` 逐一对齐；CAS：`expected==0`→`store_insert`（唯一键冲突=失败），`>0`→`store_cas_update` 影响行数==1 |
| `jdbc.store.JdbcStore.Builder` | 组件构建器 | `JdbcStore::new`/`with_schema`（无独立 Builder 类型） | Java 可注入 `dialect`/`ObjectMapper`/`initializeSchema`；Rust 固定 serde_json |
| `jdbc.state.JdbcAgentStateStore` | JDBC `AgentStateStore`（单值 + 增量列表 + `:_hash`） | `state::JdbcAgentStateStore`（`src/state/jdbc_agent_state_store.rs`，`impl agentscope_core::state::AgentStateStore`） | 覆盖 `save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`；列表哈希复用 `agentscope_core::state::list_hash`；**版本化 API 未公开**（见 §3.2） |
| `jdbc.snapshot.JdbcRemoteSnapshotClient` | BLOB 上传/下载/存在性 | `snapshot::JdbcRemoteSnapshotClient`（`src/snapshot/jdbc_remote_snapshot_client.rs`，`impl RemoteSnapshotClient`） | 表默认 `agentscope_snapshots`；`upload` 用方言 `snapshot_upsert` |
| `jdbc.snapshot.JdbcSnapshotSpec` | 快照 spec 便捷类（extends `RemoteSnapshotSpec`） | `snapshot::JdbcSnapshotSpec`（`src/snapshot/jdbc_snapshot_spec.rs`，newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`） | 快照 id 校验沿用 harness |
| `jdbc.sandbox.JdbcSandboxExecutionGuard` | JDBC `SandboxExecutionGuard`，委托方言锁策略 | `sandbox::JdbcSandboxExecutionGuard`（`src/sandbox/jdbc_guard.rs`） | harness guard 为 **struct 非 trait**，故为固有方法 `try_enter`/`release`/`is_held`（见 §3.4） |
| `jdbc.sandbox.JdbcSandboxExecutionGuard.Builder` + 内部 lease | 构建器与租约 | `JdbcSandboxExecutionGuard::{new,table_lock,with_key_prefix,with_timeout_seconds}` + `sandbox::TableLockStrategy` + `dialect::LockLease` | 常量对齐：`DEFAULT_KEY_PREFIX="agentscope:sandbox:lock:"`、`DEFAULT_LOCK_TIMEOUT_SECONDS=1800` |
| `javax.sql.DataSource` / JDBC（组件入参） | 连接与 SQL 执行 | **Rust 独有**：`backend::JdbcExecutor` trait + `backend::RusqliteExecutor`（`src/backend/{mod,sqlite}.rs`）+ `ColumnValue`/`JdbcError`/`SharedExecutor`/`is_unique_violation` | Java 无对应类；Rust 以 executor seam 替代 DataSource（见 §2.1） |
| （测试支撑）`H2TestSupport` + 10 个 `*Test`（H2/SQLite/Testcontainers） | 离线/集成测试 | `RusqliteExecutor::in_memory()`（可作测试后端） | Rust 侧**尚无对应用例**（见 §3.1） |

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
| JUnit5 / Testcontainers 1.21.3 | 测试 | Rust `#[cfg(test)]`（当前仅 `src/lib.rs` 2 个） | 见 §3.1 |

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
| 方言检测 + 链式 builder | `dialect/AbstractJdbcDialectBuilder.java` | 部分完成 | `detect_dialect` 用产品名子串匹配（替代 SPI）；`JdbcDialectBuilder.build()` 仅返回方言 |
| 4 个厂商方言（MySQL/PG/SQLite/H2） | `dialect/vendor/*.java` | 部分完成 | MySQL/PG/SQLite 的 DDL/UPSERT 已迁移；**H2 合并进 `AnsiDialect`**；**MySQL/PG 原生锁未迁移**（§3.2） |
| `JdbcStore`（`impl BaseStore`）+ CAS | `store/JdbcStore.java` | 已完成 | 5 个 trait 方法全实现；`put_if_version` 严格对齐 harness CAS |
| `JdbcAgentStateStore`（`impl AgentStateStore`） | `state/JdbcAgentStateStore.java` | 部分完成 | 7 个 trait 方法全实现 + `create_table_if_not_exist`/`verify_table_exists`；**版本化 API 未公开**（§3.2） |
| `JdbcRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | `snapshot/JdbcRemoteSnapshotClient.java` | 已完成 | upload/download/exists |
| `JdbcSnapshotSpec`（`impl SandboxSnapshotSpec`） | `snapshot/JdbcSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec` |
| `JdbcSandboxExecutionGuard` + 表锁策略 | `sandbox/JdbcSandboxExecutionGuard.java` | 部分完成 | 仅 `TableLockStrategy`；`release`/`is_held` 为对齐表面的空探针（§3.2/§3.3） |
| `JdbcDistributedStore` 组合者 | `JdbcDistributedStore.java` | 已完成 | 4 组件访问器 + `to_harness_store()`（仅投影半量，§4.2） |
| 无真实 DB 的内存后端 | Java H2/SQLite | 已完成 | `RusqliteExecutor::in_memory()` |
| **离线单元测试** | 10 个测试类，共 **98** 个 `@Test`（H2 为主 + SQLite + 2 个 Testcontainers 集成） | **未完成（P0 缺口）** | 现状：`cargo test -p agentscope-extensions-jdbc` = **2 passed / 0 failed**，且**仅有 `src/lib.rs` 的 `now_millis_is_positive`、`next_seq_is_monotonic`**；`store`/`state`/`snapshot`/`sandbox`/`dialect`/`backend` 全部零测试，crate 内**无 `tests/` 目录**。对照同波次 MySQL（43）、MongoDB（56+1）差距明显。补测方向（全离线，用 `RusqliteExecutor::in_memory()`）：CAS 三分支、state 单值/列表+`:_hash`、快照往返、表锁获取/超时/释放、各方言 DDL/SQL 模板、`detect_dialect` |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| MySQL 原生 `GET_LOCK`/`RELEASE_LOCK` 锁 | `dialect/vendor/MysqlDialect.java:181-238` | 待办 | Rust `MysqlDialect` 未 `impl SandboxLockStrategy`，仅在文档注释提及；`AbstractJdbcDialect::supports_native_locks()` 返回 false 且无覆盖 |
| PostgreSQL 原生 `pg_try_advisory_lock`/`pg_advisory_unlock` | `dialect/vendor/PostgresDialect.java:182-252` | 待办 | 同上 |
| state 版本化写入/读取（`saveIfVersion`/`readVersion`/`supportsVersioning`） | `state/JdbcAgentStateStore.java:203,231,252` | 待办 | Rust dialect 已定义 `session_state_insert_if_absent`/`session_state_update_if_version`，但 `JdbcAgentStateStore` 仅有私有 `read_version`，**未公开 `save_if_version`/`version_of`**；当前单值写入仅走 `session_state_upsert`（last-write-wins） |
| 快照上传流式绑定（避免整包入内存） | `snapshot/JdbcRemoteSnapshotClient.java`（`setBinaryStream`） | 待确认 | Rust `RemoteSnapshotClient::upload(&[u8])` 需整包驻留内存；大快照场景存在内存差异 |
| 真实 MySQL/PostgreSQL 集成测试 | `integration/MysqlIntegrationTest.java`、`integration/PostgresIntegrationTest.java`（Testcontainers） | 待办（环境受限） | Rust 无对应 `tests/`，且当前无 mysql/pg 驱动 |
| 列表增量语义（`getListCount`/`likePrefixPattern`） | `state/JdbcAgentStateStore.java:414,481` | 已完成 | Rust `list_count`/`like_prefix_pattern` 已实现 |
| 表存在性校验 | `state/JdbcAgentStateStore.java:101` | 已完成 | Rust `verify_table_exists` 使用方言 `session_state_check_table_exists` |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 会话表列名差异（跨语言同库不兼容） | `dialect/table/SessionStateDialect.java`（`session_id`） | 待确认 | Rust 用 `slot_id`（`src/dialect/session_state_dialect.rs`）；Java 用 `session_id`。两语言指向同一数据库时表结构不互通 |
| 锁表名差异（跨语言不兼容） | `dialect/AbstractJdbcDialect.java:253`（`tablePrefix + "distributed_locks"`） | 待确认 | Rust 用 `{prefix}lock`（`abstract_dialect.rs:106`）；Java 用 `{prefix}distributed_locks` |
| JDK SPI 方言自动注册/发现 | `AbstractJdbcDialect.from(DataSource)` + `META-INF/services/...` | 待办 | Rust 用产品名子串匹配，第三方方言无法自动注册；扩展需改 `detect_dialect` |
| 组件懒加载缓存 | `JdbcDistributedStore` volatile 字段 | 待确认 | Rust 在构造函数中一次性构建全部组件（含 DDL），语义等价但非懒加载 |
| `tracing` 埋点对齐 SLF4J | 各方言/组件日志点 | 待办 | Rust 仅少量日志（guard/state/store） |
| dead_code 辅助函数 | — | 待确认 | `TableLockStrategy::delete_lock`、`read_version`、`namespace_param` 等当前无调用方；保留待接线，长期不用可评估删除 |
| `distributed_store` 模块可见性 | — | 待确认 | `lib.rs` 中为 `mod distributed_store;`（私有模块，仅 re-export 类型），与其余 `pub mod` 风格不一致，非阻塞 |

### 3.4 设计决定（不迁移）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `SandboxExecutionGuard` 的 `impl` | `JdbcSandboxExecutionGuard implements SandboxExecutionGuard` | 设计决定（不迁移） | harness `SandboxExecutionGuard` 是 **struct 非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），无法 `impl`；改为镜像 API 的固有方法 `try_enter`/`release`/`is_held` |
| `DataSource` 连接所有权 / `getDataSource()` / `close()` | `JdbcDistributedStore`、各组件 | 设计决定（不迁移） | Rust 以 `SharedExecutor = Arc<dyn JdbcExecutor>` 共享，无连接所有权/关闭语义 |
| `ObjectMapper` 注入 | `store/JdbcStore.java`（`Builder.objectMapper`） | 设计决定（不迁移） | Rust 固定 `serde_json`，无需注入 |
| `H2Dialect` 独立类 | `dialect/vendor/H2Dialect.java` | 设计决定（不迁移） | Rust 无 H2 驱动；`detect_dialect` 对 `h2`/未知默认返回 `AnsiDialect`（H2 用 ANSI 语法，DDL 差异由 Ansi 近似） |
| JSON 工具 `JsonUtils` | `io.agentscope.core.util.JsonUtils` | 设计决定（不迁移） | Rust 直接用 `serde_json` |
| Java 包文档类（`package-info.java`） | `dialect/`、`dialect/table/`、`dialect/vendor/` | 设计决定（不迁移） | doc-only，无逻辑 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **编译修复已完成**（本会话）。`cargo build -p agentscope-extensions-jdbc` 与 `cargo test -p agentscope-extensions-jdbc` 均通过。
- 验证命令与结果：
  - `cargo build -p agentscope-extensions-jdbc` → `Finished dev profile`，退出码 0（仅剩 9 条 warning，无 error）。
  - `cargo test -p agentscope-extensions-jdbc` → `test result: ok. 2 passed; 0 failed`（lib 单测）+ doc-tests 0 passed；退出码 0。
- 本会话修复的 5 个编译错误（均为**命名空间/导入路径**问题，未删功能）：
  1. `E0433/E0432`（`backend/sqlite.rs`）：`super::dialect::*` 路径错误 —— `dialect` 是 crate 级模块，不在 `backend` 的子模块中。改为 `crate::dialect::{BoundSql, SqlParam}`（`bound_sql` 为私有模块，改从 `crate::dialect` 再导出导入）。
  2. `E0423`（`backend/sqlite.rs:169`）：`OptionalExtension` 是 rusqlite 的 **trait**，被当作值使用。该行本就是 `let _ = ...; // keep import used` 的占位代码，随第 1 项一并移除（`OptionalExtension` 实际并未被需要，导入已去除）。未改动既有功能。
  3. `E0404`（`sandbox/jdbc_guard.rs:190`）：harness 的 `SandboxExecutionGuard` 是 **struct（具体类型）**，不是 trait。移除 `impl SandboxExecutionGuard for JdbcSandboxExecutionGuard`，改为把 `try_enter`/`release`/`is_held` 作为 `JdbcSandboxExecutionGuard` 的**固有方法**。该类型仍 `#[derive(Clone)]`，可用 `Arc<JdbcSandboxExecutionGuard>` 共享。
  4. `E0425`（`state/jdbc_agent_state_store.rs:170`）：缺失的 `crate::hash_list` 改为复用 core 已有实现 `agentscope_core::state::list_hash::compute_hash`（导入为 `core_compute_hash`），未另造并行实现。
- 修复过程中连带解决的次生错误（同一轮迭代引入/暴露，均为派生约束问题）：
  - `E0603`：`bound_sql` 私有 → 改从 `crate::dialect` 导入（见上）。
  - `E0277`（×2）：`JdbcDialectBuilder` 与 `JdbcSandboxExecutionGuard` 的 `Arc<dyn ...>` 字段不满足 `Debug` 派生（trait 对象未要求 `Debug`）。改为手写 `Debug` impl，保留 `#[derive(Clone)]`。
  - `E0277`（`JdbcSnapshotSpec`）：harness `RemoteSnapshotSpec` 未实现 `Clone`。改为手写 `Clone`（通过 `RemoteSnapshotSpec::new(self.inner.client().clone())` 复用同一 `Arc<dyn RemoteSnapshotClient>`），保留 `#[derive(Debug)]`。
  - `E0004`（非穷尽 match）：`SandboxLockStrategy::try_enter` 返回 `Result<LockLease, LockOutcome>`，补全 `LockOutcome::Acquired` 分支（与 `TimedOut` 一并无锁降级为 `SandboxLease::noop()`）。
- 改动文件（仅限本 crate）：
  - `crates/agentscope-extensions-jdbc/src/backend/sqlite.rs`
  - `crates/agentscope-extensions-jdbc/src/dialect/detection.rs`
  - `crates/agentscope-extensions-jdbc/src/dialect/vendor/mysql.rs`
  - `crates/agentscope-extensions-jdbc/src/sandbox/jdbc_guard.rs`
  - `crates/agentscope-extensions-jdbc/src/snapshot/jdbc_snapshot_spec.rs`
  - `crates/agentscope-extensions-jdbc/src/state/jdbc_agent_state_store.rs`
- 遵守约束：未改根 `Cargo.toml`、未改 core/harness 或其它 crate；未删除既有功能（仅移除一行占位 `let _ = OptionalExtension::<i64>;` 与其未用导入）。
- 已知（非阻塞）warning：`query_optional_int`、`TableLockStrategy::delete_lock`、`read_version`、`slot_param`、`namespace_param` 等为尚未被调用的辅助函数（dead_code），保留以备后续 lock strategy / 组件接线，不删。
- **基线（汇总方实测，本次未重跑）**：`cargo build -p agentscope-extensions-jdbc` 通过；`cargo test -p agentscope-extensions-jdbc` = lib **2 passed / 0 failed**，doc-tests 0；全量 `cargo test --workspace` = **1746 passed / 0 failed**（86 test target，exit 0）。仅有的 2 个测试为 `src/lib.rs` 的 `now_millis_is_positive`、`next_seq_is_monotonic`（见 §3.1 测试覆盖 P0 缺口）。
- **crate 文件清单**（实测）：`Cargo.toml`(887B)；`src/lib.rs`；`src/distributed_store.rs`；`src/backend/{mod.rs,sqlite.rs}`；`src/dialect/{mod.rs,abstract_dialect.rs,bound_sql.rs,detection.rs,lock_strategy.rs,session_state_dialect.rs,snapshot_dialect.rs,store_dialect.rs}` + `src/dialect/vendor/{mod.rs,ansi.rs,mysql.rs,postgres.rs,sqlite.rs}`；`src/{store,state,snapshot,sandbox}/`（各含 `mod.rs` + 组件文件）。无 `tests/` 目录。
### 4.2 下一步与阻塞
1. 后续若需把 JDBC 锁语义接入 harness `SandboxManager`（现仅接受具体 `SandboxExecutionGuard`），需在 harness 侧提供 trait 抽象或适配层；本会话未改 harness，按约定记入此处由汇总方评估。
2. 剩余 dead_code helper 在组件全线接线后应被调用；如长期不用可评估删除（非本会话职责）。
3. **测试覆盖为 P0 缺口**（见 §3.1）：Java 10 个测试类 98 个 `@Test`，Rust 仅 2 个 lib 单测。补测可全部离线（`RusqliteExecutor::in_memory()`），无需新依赖。
4. **功能缺口**：MySQL `GET_LOCK` / PostgreSQL `pg_try_advisory_lock` 原生锁（§3.2）与 state 版本化 API `saveIfVersion`/`readVersion`（§3.2）未迁移；`JdbcSandboxExecutionGuard::release`/`is_held` 当前为空探针（表锁策略下释放依赖 lease、探针恒 false）。
5. **跨模块需求（不改 harness）**：harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`；本 crate 已在 `JdbcDistributedStore` 内保留全量 4 组件并通过 `to_harness_store()` 输出子集，是否扩展 harness `DistributedStore`（`EXTENSIONS_GAP_TRACKER.md` §2.2）待汇总方裁定。
6. **已记录的跨语言差异**（§3.3）：会话表列名 Rust `slot_id` vs Java `session_id`；锁表名 Rust `{prefix}lock` vs Java `{prefix}distributed_locks`——同库混布不兼容，按需裁定是否对齐。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-jdbc` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-jdbc` / `cargo test -p agentscope-extensions-jdbc` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-jdbc/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

