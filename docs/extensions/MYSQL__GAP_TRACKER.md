# MYSQL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MYSQL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-mysql\`
> **Java 子模块**：（单模块）
> **Rust 目标**：`crates/agentscope-extensions-mysql/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-mysql` |
| Java 子模块 | （单模块） |
| Java 规模（实测 2026-10-22） | 主代码 **2106** 行 / 测试 **358** 行（非空行口径，与 rollup「Java 主/测行」列一致；含空行总行数为 2323 / 416）（11 个主类 + 3 个测试类，共 14 个 `.java`；11 个 `@Test`） |
| Rust crate | `crates/agentscope-extensions-mysql` |
| Rust 规模（实测 2026-10-26） | `src/` 13 个 `.rs` 文件 / **3179** 行（非空行口径；含空行 3563）+ `Cargo.toml` 19 行；**108 passed / 0 failed**（2026-10-31 实测；2026-10-28 为 71） |
| 迁移波次 | B |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 108 passed / 0 failed（默认）/ 109 + 5 集成 + 2 doc；`mysql_async` feature，集成 `AGENTSCOPE_TEST_MYSQL_URL` 门控、不用 `#[ignore]`）—— 真实驱动适配器 `MysqlAsyncBackend` 与集成测试均已落地，P1 全部关闭；P2 已关闭 7 项，余 2 项为 §4.2 的跨模块/裁定说明。历史见 history/MYSQL__MIGRATION_HISTORY.md |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `MysqlDistributedStore` | 组合 4 个组件成 `DistributedStore` | `MysqlDistributedStore` / `MysqlDistributedStoreBuilder`（`distributed_store.rs`） | 持有 4 组件 + `isolation_scope`；提供 `to_harness_distributed_store()` 输出 harness 子集 |
| `state.MysqlAgentStateStore` | MySQL 会话状态（单值 + 增量列表 + 版本） | `state::MysqlAgentStateStore` | `impl AgentStateStore`；`save_if_version`/`read_version` 作为固有方法保留版本语义。**2026-10-26 已对齐**：版本存 `item_index = 0` 行的 `version` 列（`StateBackend::read_version`/`save_single_versioned`），CAS 走后端原子原语 `insert_single_if_absent` / `update_single_if_version`（单条 `INSERT` / `UPDATE ... WHERE version = ?`）；列表用 core `list_hash`（`compute_hash` / `needs_full_rewrite`）做增量判断，`{key}:_hash` 伴随行只存**列表哈希**（见 history/MYSQL__MIGRATION_HISTORY.md） |
| `store.JdbcStore` | JDBC 命名空间 KV + CAS | `store::MysqlStore`（`impl BaseStore`） | `put_if_version` 严格对齐 harness CAS 语义（v1 起、`expected==0` 为 create-if-absent） |
| `store.JdbcStoreDialect` | 方言抽象（4 条 SQL 模板） | `store::StoreDialect` | 含 `create/upsert/insert/cas_update/search/select/delete` |
| `store.MysqlJdbcStoreDialect` | MySQL 方言 | `store::MysqlStoreDialect` | 其余方言一并迁移：`Postgres/Sqlite/H2 StoreDialect`。**复核差异**：PG/H2 `namespace_path` 为 `VARCHAR(512)`，Java 为 `VARCHAR(2048)`（Java MySQL 方言为 512，已对齐）；**H2 upsert 双方均为 `MERGE INTO ... USING (VALUES ...)`（一致，非偏离）**——Java `H2JdbcStoreDialect` 的类 Javadoc 写「Uses ... `ON CONFLICT ... DO UPDATE`」属**过时注释**，其 `getUpsertSql()` 实际返回 `MERGE INTO`，故此处以代码为准 |
| `snapshot.JdbcSnapshotSpec` | 快照 spec（BLOB 表） | `snapshot::MysqlSnapshotSpec`（`impl SandboxSnapshotSpec`） | **复用 harness `RemoteSnapshotSpec`**，快照 id 校验以 harness 为准 |
| `snapshot.JdbcRemoteSnapshotClient` | BLOB 上传/下载/存在性 | `snapshot::MysqlRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | 表名默认 `agentscope_snapshots` |
| `sandbox.JdbcSandboxExecutionGuard` | MySQL `GET_LOCK()` 分布式锁 | `guard::MysqlSandboxExecutionGuard` / `MysqlLockLease` | **已解决（2026-10-28）**：harness 新增 `SandboxGuard` trait，`MysqlSandboxExecutionGuard` 已 `impl SandboxGuard`（四方法委托固有方法，内部 `trait_leases` 保留具体租约，见 §4.2 执行记录）。**复核差异**：Java `composeLockName` 有 64 字符上限（超长截断 50 + `hashCode` 十六进制），本 crate **无此截断**；Java `Builder.keyPrefix` 会自动补尾部 `:` 且拒绝空白，本 crate `key_prefix()` **原样使用**（`"app:lock:"` 需自带冒号）；Java 默认超时 **1800s**，本 crate 默认 **30s**；`SandboxIsolationKey.sub_key` 为 Rust 侧新增（Java 无），属扩展非缺口 |
| `_hash` 变更检测键（`HASH_KEY_SUFFIX`） | 列表变更检测伴随行 | `state::HASH_KEY_SUFFIX` + `state::save_list_impl` | **已对齐（2026-10-26）**：`{key}:_hash` 行存 `agentscope_core::state::list_hash::compute_hash(values)`，写入前用 `needs_full_rewrite` 判定「全量重写 / 仅追加尾部 / 跳过」；不再被借用为版本键（版本改存行内 `version` 列） |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `DataSource` / JDBC | 连接、SQL | **`SqlBackend` / `StateBackend` / `SnapshotBackend` / `LockBackend` 后端 trait** | 各组件依赖窄接口，不绑定驱动（4 个 trait 均已定义并接线，见 history/MYSQL__MIGRATION_HISTORY.md） |
| `com.fasterxml.jackson` | JSON | `serde_json` | 已用 |
| `PreparedStatement`/`ResultSet` | SQL 绑定/读取 | 后端 trait 方法参数/返回值 | 不在 crate 内绑定具体驱动 |
| `LONGTEXT`/`LONGBLOB` | 大字段 | 后端实现负责（`String`/`&[u8]`） | 本 crate **不含任何 state/snapshot DDL**，建表语句完全由后端实现提供 |
| SLF4J | 日志 | `tracing`（依赖已接入，**实测源码 0 处 `tracing::` 调用**） | 待办（P2） |

> **驱动决策**：本 crate **不**硬编码 MySQL 驱动依赖（避免给整个 workspace 引入 `mysql`/`sqlx` 等重依赖与版本约束）。所有数据库交互经上述 4 个后端 trait 抽象，由使用方/上层适配器注入。内存后端起于单元测试与集成装配。该决策符合交付约束“不新增重依赖”。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib 导出 | `MysqlDistributedStore` | 已完成 | `lib.rs` 导出全部公共类型 |
| `MysqlStore` (BaseStore) + 方言 | `JdbcStore`/`*Dialect` | 已完成 | `store/mod.rs` + `store/dialect.rs` |
| CAS/版本语义对齐 harness | `JdbcStore.putIfVersion` | 已完成 | `expected==0` → insert（重复键=失败）；`>0` → `UPDATE ... WHERE version=?` 影响行数=1 |
| `MysqlAgentStateStore` | `MysqlAgentStateStore` | 已完成 | 单值 index=0；列表增量写入 + 缩容修剪；`save_if_version`/`read_version` |
| `MysqlSnapshotSpec` | `JdbcSnapshotSpec` | 已完成 | 包装 harness `RemoteSnapshotSpec` |
| `MysqlRemoteSnapshotClient` | `JdbcRemoteSnapshotClient` | 已完成 | `impl RemoteSnapshotClient` |
| `MysqlSandboxExecutionGuard` | `JdbcSandboxExecutionGuard` | 已完成 | `try_enter`/`release`/`is_held`/`active_count`；lease Drop 自动释放 |
| `MysqlDistributedStore` 组合 | `MysqlDistributedStore` | 已完成 | 4 组件 + `isolation_scope`；`to_harness_distributed_store()` |
| 单元测试（无真实 MySQL） | Java 测试 358 行 / 11 `@Test` | 已完成 | 内存后端 **71 passed / 0 failed**（2026-10-28 主会话复跑；2026-10-26 静态统计 70 个 test 属性 = 49 `#[test]` + 21 `#[tokio::test]`，实测比静态多 1） |

### 3.2 待办 / 后续增强（P1/P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| 真实驱动适配器 | 用 `mysql_async`/`sqlx` 等实现 4 个后端 trait（可放入本 crate 的 feature 或上层应用） | **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）**：feature `mysql_async` → `src/driver/real.rs::MysqlAsyncBackend`（**同时实现** `StateBackend`/`SqlBackend`/`SnapshotBackend`/`LockBackend`，共享一个 pool）；未开 feature 时 `src/driver/stub.rs` 同名 API 返回 `MysqlError::DriverUnavailable`，构造期 ping 失败亦同 |
| 集合/集成测试 | 需真实 MySQL 实例（当前仅内存后端验证语义） | **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）**：`tests/mysql_integration.rs`，**env 门控**（`AGENTSCOPE_TEST_MYSQL_URL`，未设置则 skip，**不用 `#[ignore]`**），含无需服务器的 `DriverUnavailable` 用例 |
| `tracing` 日志 | 对齐 SLF4J 日志点 | **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）**：guard 获取/释放 `debug!`、超时 `warn!`；`try_build` 降级 `warn!`；快照建表失败 `warn!`（history/MYSQL__MIGRATION_HISTORY.md） |
| `JdbcStore.validateIdentifier` 等价 | 表/库名合法性校验 | **已完成（2026-10-26）**：新增 `src/identifier.rs`（`validate_identifier`，`[A-Za-z_][A-Za-z0-9_]*` + 64 字符上限），接入 state（库名/表名）、`MysqlStoreBuilder::table_name`、`MysqlRemoteSnapshotClient::with_table`、`MysqlSnapshotSpec::with_table` 与 `MysqlDistributedStoreBuilder::try_build` |
| **列表变更检测（`ListHashUtil`）** | Java `save(list)` 用 `HASH_KEY_SUFFIX` + `ListHashUtil.computeHash/needsFullRewrite` 判定「无变化则跳过 / 仅前缀变化则全量重写 / 纯追加则只插尾部」；本 crate 原仅按 `list_len()` 追加，中间元素被改写时不会重写 | **已完成（2026-10-26）**：`state::save_list_impl` 复用 core `agentscope_core::state::list_hash`（`compute_hash` / `needs_full_rewrite`），`{key}:_hash` 行存列表哈希。测试：`save_list_detects_mutated_prefix`、`save_list_persists_core_list_hash_row`、`save_list_unchanged_skips_backend_write` |
| **版本 CAS 原子性** | Java 版本存于独立 `version` 列，`saveIfVersion` 走 `INSERT` / `UPDATE ... WHERE version=?`，由数据库保证原子；本 crate 原用 `key:_hash` 伴随行「先读后写」 | **已完成（2026-10-26）**：`StateBackend` 新增 `save_single_versioned` / `insert_single_if_absent` / `update_single_if_version`（契约为**单条**原子语句）与必填 `read_version`（读 `version` 列）；`save_if_version` 不再先读版本。测试：`save_if_version_delegates_cas_instead_of_read_then_write`、`cas_conflict_does_not_write`、`save_upsert_bumps_version_column`、`cas_update_requires_matching_version` |
| **`saveIfVersion(0)` 迁移行回退** | 原记录称「Java #3162 遇重复键回退 `UPDATE ... WHERE version=0`」 | **勘误 + 已完成（2026-10-26）**：题面复核 Java `MysqlAgentStateStore#insertIfAbsent`（`:477-500`）**没有**回退——重复键（1062）直接返回 `UNVERSIONED`（PG/JDBC 版才有回退）。Rust 现按 MySQL Java 语义实现：`expected==0` 为真实 create-if-absent，重复键冲突即 `Conflict`。该回退是否需要（PG/JDBC 一致性）留待汇总方裁定，记 §4.2 |
| **schema 校验/版本列补齐** | Java 构造期 `verifyDatabaseExists`/`verifyTableExists`/`ensureVersionColumn`（`ALTER TABLE ... ADD COLUMN version BIGINT NOT NULL DEFAULT 1`）；本 crate 原只有 `ensure_schema()`（后端实现负责）与标识符校验，无存在性校验、无版本列迁移 | **已完成（2026-10-30，history/MYSQL__MIGRATION_HISTORY.md）**：`StateBackend` 的 `ensure_schema` 换成 `execute_ddl` + `database_exists`/`table_exists`/`table_columns` 探针；DDL 由本 crate 生成（`create_database_sql`/`create_sessions_table_sql`/`add_version_column_sql`），`create_if_not_exist=false` 先校验库再校验表（`MysqlError::Schema` + Java 原文），两分支末尾都跑幂等 `ensure_version_column`。测试：`auto_create_provisions_database_and_table`、`missing_database_is_reported_when_not_auto_creating`、`missing_table_is_reported_when_not_auto_creating`、`legacy_table_gets_the_version_column_migration`、`version_column_migration_is_idempotent`、`existing_version_column_is_left_alone`、`ddl_failure_propagates_when_auto_creating`、`custom_names_flow_into_the_provisioning_ddl`、`create_table_ddl_pins_binary_collation_on_key_columns` |
| **快照表自动建表（P1-7）** | Java `JdbcSnapshotSpec` 恒传 `initializeSchema=true`，`JdbcRemoteSnapshotClient#initSchema` 发 `CREATE TABLE IF NOT EXISTS`（`snapshot_id VARCHAR(512) COLLATE utf8mb4_bin PK`）；本 crate `MysqlSnapshotSpec` 原不发 DDL | **已完成（2026-10-30，history/MYSQL__MIGRATION_HISTORY.md）**：新增 `snapshot_table_ddl()` + `SnapshotBackend::execute_ddl`；`MysqlSnapshotSpec::new/with_table` 恒走 `*_and_schema` 构造，DDL 失败**仅 `tracing::warn!`**（对齐 Java `log.warn`，不抛错）。测试：`snapshot_ddl_matches_the_java_statement`、`spec_issues_the_table_ddl_on_construction`、`spec_with_table_issues_ddl_for_the_custom_table`、`ddl_failure_is_logged_not_raised`、`client_without_initialize_schema_emits_no_ddl` |
| **锁名 64 字符上限** | Java `composeLockName` 超 64 字符截断为 `前50 + ":" + hex(hashCode)`（MySQL `GET_LOCK` 限制）；本 crate 无截断，长 key 会由驱动侧失败 | **已完成（2026-10-30，history/MYSQL__MIGRATION_HISTORY.md）**：`truncate_lock_name`（UTF-16 计长，`前50 + ':' + Integer.toHexString(String.hashCode())`，负数 8 位无符号 hex）+ `java_string_hash_code`；`compose_lock_name` 统一走截断。金标向量由真实 Java 24 跑出（`f4aead7e`/`f3714f1b`/`2f91cd3e`/`80000000`）。测试：`java_string_hash_code_matches_java_gold_values`、`compose_lock_name_keeps_names_at_the_limit`、`compose_lock_name_truncates_over_the_limit`、`compose_lock_name_uses_unsigned_hex_for_negative_hash`、`truncated_names_never_exceed_the_limit` |
| **表名注入防护（store/snapshot）** | Java `JdbcStore.Builder.tableName` 强制 `[A-Za-z_][A-Za-z0-9_]*`；本 crate `MysqlStoreBuilder::table_name`/`MysqlRemoteSnapshotClient::with_table` 原不做校验，DDL 直接拼接 | **已完成（2026-10-26）**：两处均改为返回 `MysqlResult`；`MysqlDistributedStoreBuilder` 新增 `try_build()`（`build()` 委托并 `expect`）在构造期校验。测试：`rejects_injection_payloads`（identifier）、`table_name_rejects_injection_payloads`、`with_table_rejects_unsafe_identifiers`、`try_build_rejects_unsafe_table_names` |
| **空列表语义（与 Java 相反）** | Java `save(list)` 在 `values.isEmpty()` 时**直接 return**（不触碰既有行）；本 crate 原以 `from_index=list_len()` + 空切片调用后端，内存后端按「total=0」修剪 → **清空**既有列表 | **已完成（2026-10-22，history/MYSQL__MIGRATION_HISTORY.md）**：`save_list` 对空切片直接返回（no-op），测试 `save_list_empty_is_a_no_op` |
| **`key_prefix` 归一化 + 默认锁超时** | Java `Builder.keyPrefix` 自动补尾部 `:`、拒绝空白，默认超时 1800s（30 分钟）；本 crate `key_prefix()` 原样使用（须自带冒号，否则拼出 `app:lockuser:u1`），默认超时 **30s** | **归一化已完成（2026-10-30，history/MYSQL__MIGRATION_HISTORY.md）**：`key_prefix`（空白 → panic）/`try_key_prefix`（空白 → `InvalidArgument("keyPrefix must not be blank")`），缺尾冒号自动补（Java 不 trim，此处同）。测试：`key_prefix_is_normalised_with_a_trailing_colon`、`blank_key_prefix_is_rejected`。**默认锁超时已裁定（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md P2-8）**：改为 `DEFAULT_LOCK_TIMEOUT = 1800s` 对齐 Java，`lock_timeout()`/`lock_timeout_seconds()` 保持可配置，`default_lock_timeout_matches_java` 钉住。注意：此前“改默认值会让既有调用方静默等待 30 分钟”的顾忌，由“可显式收窄”化解；若调用方需快速失败，显式传短超时即可 |
| **状态 key / session id 长度校验** | Java `validateStateKey`/`validateSessionId` 限长 255 且 key 非空；本 crate 仅校验 `session_id` 非空，**不限长、不校验 `state_key`** | **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）**：新增 `validate_state_key` + `MAX_STATE_IDENTIFIER_LENGTH = 255`（`require_session_id` 亦限长），接入 `save_if_version`/`read_version`/`save_list_impl`。测试：`state_key_validation_mirrors_java` |
| **`truncateAllSessions` / `clearAllSessions`** | Java 提供测试/运维清理入口；本 crate 无对应 API（`delete` 仅按会话） | **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）**：`MysqlAgentStateStore::truncate_all_sessions()`（`TRUNCATE TABLE db.table`，返回 0，同 Java）；Java 已 `@Deprecated` 的 `clearAllSessions()`（`DELETE FROM` + 行数）**不迁移**（Java 自身指向 `truncateAllSessions`）。测试：`truncate_all_sessions_clears_rows` |
| **`getDataSource` 访问器** | Java `getDatabaseName`/`getTableName`/`getDataSource`；本 crate 有前两者，无后者（后端 trait 抽象下不适用） | 设计决定（不迁移） |
| MySQL 键列二进制排序（`2349533a1`） | 本 crate `store/dialect.rs` 的 `namespace_path`/`item_key` 早已 `COLLATE utf8mb4_bin`；**同族修复（2026-10-20）落在 `agentscope-extensions-jdbc`**：其 `dialect/vendor/mysql.rs` 的会话状态表 `session_id`/`state_key` 与快照表 `snapshot_id` 已补 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`。存量库仍需手工 ALTER，见 `docs/migrations/mysql_binary_collation.md` | 已完成（2026-10-20） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **P0 全部已完成**：`crates/agentscope-extensions-mysql/` 全部 P0 组件实现，均**复用既有 trait**（`AgentStateStore` / `BaseStore` / `SandboxSnapshotSpec` / `RemoteSnapshotClient`），CAS/版本语义以 harness 为准。
- **P1 已关闭 4 项（2026-10-26）**：P1-1 列表变更检测、P1-2 CAS 原子性、P1-3 create-if-absent（原记录的回退前提已勘误）、P1-5 表名校验；P1-4 空列表 no-op 已在上轮（history/MYSQL__MIGRATION_HISTORY.md）关闭。**剩余 P1**：P1-6 schema 存在性校验/`version` 列迁移、P1-7 快照表自动建表、P1-8 锁名 64 字符截断。模块整体仍为「进行中」（详见 history/MYSQL__MIGRATION_HISTORY.md）。**已完成（2026-10-29 勘误）**：P1 现存 **4 项**（上述 3 项 + 真实驱动适配器/集成测试，见 §3.2 前两行）；`saveIfVersion(0)` 回退一项已按 Java MySQL 语义勘误为「无回退」并落地（§3.2 / history/MYSQL__MIGRATION_HISTORY.md）。**已完成（2026-10-30）**：P1-6 / P1-7 / P1-8 已全部关闭（见 history/MYSQL__MIGRATION_HISTORY.md），P1 功能性缺口清零；`cargo test -p agentscope-extensions-mysql -j 2` → **98 passed / 0 failed**；剩余 P1 仅真实驱动适配器 + 真实 MySQL 集成测试（环境受限）。**已完成（2026-10-31）**：上述两项 P1 已闭合（`src/driver/*` + `tests/mysql_integration.rs`，见 history/MYSQL__MIGRATION_HISTORY.md），**本模块 P1 清零**；默认锁超时同步改为 1800s（history/MYSQL__MIGRATION_HISTORY.md P2-8）。
- **文件清单**（实测 2026-10-26，14 个文件；`src/` 13 个 `.rs` / 3563 行含空行、**3179 行非空**）：
  - `Cargo.toml`（19 行；workspace 依赖：agentscope-core/harness、serde、serde_json、thiserror、async-trait、tokio、tracing、chrono，**另含未被任何源码引用的 `futures`**，见文末勘误；本轮**未新增依赖**）
  - `src/lib.rs`（56）、`src/error.rs`（69）、`src/identifier.rs`（97，**本轮新增**）
  - `src/store/{mod.rs 515, dialect.rs 374, testing.rs 178}`
  - `src/state/{mod.rs 786, testing.rs 481}`
  - `src/snapshot/{mod.rs 220, testing.rs 52}`
  - `src/guard/{mod.rs 319, testing.rs 61}`
  - `src/distributed_store.rs`（355）
  - **2026-10-31 增量**：新增 `src/driver/{mod.rs, real.rs, stub.rs}` 与 `tests/mysql_integration.rs`；`Cargo.toml` 新增 `[features] mysql_async` + optional `mysql_async`，`futures` 从 `[dependencies]` 移到 `[dev-dependencies]`（完整清单见 history/MYSQL__MIGRATION_HISTORY.md）
- **验证命令与结果**（2026-10-20 上一会话实测值；2026-10-31 主智能体复跑：默认 **108 passed / 0 failed**，`mysql_async` **109 + 5 集成 + 2 doc**）：
  - `cargo build -p agentscope-extensions-mysql` → **Finished**（本 crate 零 warning；harness 既有 warning 与本次无关）
  - `cargo test -p agentscope-extensions-mysql` → **43 passed; 0 failed**（另 0 doc-tests）
  - ⚠️ **待复核**：静态统计（2026-10-26）为 **70** 个 test 属性（49 + 21），需 `cargo test` 确认实际执行数（**2026-10-28 已复核：71 passed / 0 failed**）
  - **已完成（2026-10-29 勘误）**：原记「43 passed / 待复核 70」，实际 **71 passed / 0 failed**（2026-10-28 主会话复跑）；依据 history/MYSQL__MIGRATION_HISTORY.md 新增用例 + 本次实测 |
- **已知问题**：无阻塞。真实 MySQL 集成测试待环境。

### 4.2 下一步与阻塞
1. **跨模块待汇总项（不改动其它 crate）**：harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`（实测 `crates/agentscope-harness/src/agent/distributed_store.rs:14-17` 仅两字段），而 Java 版含 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`（另 `message_bus`/`async_tool_registry`/`task_repository`/`session_turn_gate`/`team_client`，后者此前亦遗漏）。本 crate 已在 `MysqlDistributedStore` 内保留全量 4 组件并通过 `to_harness_distributed_store()` 输出 harness 子集；是否扩展 harness `DistributedStore`（EXTENSIONS_GAP_TRACKER §2.2）仍待汇总方裁定。**实测补充**：harness `HarnessAgent` 目前也**只**从 `DistributedStore` 取 `agent_state_store`（`harness_agent.rs:1759/1890`），`base_store`/`snapshot_spec`/`guard` 在 harness 侧**无任何消费点**——即便扩展 `DistributedStore` 字段，也需 harness 侧同步接线才算真正生效。**已完成（2026-10-29 勘误）**：原记「harness 仅 2 字段、仍待裁定」，实际 2026-10-28 harness 新增 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` 三个可选槽位，本 crate 已投影；依据文末「2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）」
2. **harness `SandboxExecutionGuard` 为具体 struct 而非 trait**（实测 `crates/agentscope-harness/src/agent/sandbox/guard.rs:13 pub struct SandboxExecutionGuard`，提供 `try_enter/release/is_held`）：故本 crate 无法 `impl` 之，改为镜像 Java API 的具体类型 `MysqlSandboxExecutionGuard`（`try_enter`/`release`/`is_held`/`active_count`）。已记入本 §4.2。**已完成（2026-10-29 勘误）**：原记「harness guard 为具体 struct 无法 `impl`」，实际 2026-10-28 harness 新增 `SandboxGuard` trait，`MysqlSandboxExecutionGuard` 已 `impl SandboxGuard`（见 §2 映射表同行）
3. **真实驱动适配器**：**已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md）** —— `MysqlAsyncBackend`（feature `mysql_async`）实现 4 个后端 trait，构造期 ping；未开 feature 时为 `DriverUnavailable` 存根。默认构建仍不引入驱动依赖。
4. ~~监点（`tracing`）与真实 MySQL 集成测试可作为后续 P1/P2。~~ **已完成（2026-10-31，history/MYSQL__MIGRATION_HISTORY.md P2-1）**：`tracing` 日志点已补，集成测试已为 env 门控用例。
5. **`StateBackend` 缝的破坏性变更（2026-10-26，本 crate 内）**：为让 CAS 真正原子，trait 新增必实现方法 `save_single_versioned` / `insert_single_if_absent` / `update_single_if_version`，`read_version` 由「默认实现（读 `:_hash` 行）」改为**必填**（读 `version` 列）；删除 `write_version`（唯一用途是写 `:_hash` 版本行）。`ensure_schema()` 的实现**必须**建出 `version BIGINT NOT NULL` 列（Java `ensureVersionColumn`，默认值 1 或不带默认亦可，CAS 语义要求初始为 1）。真实驱动适配器已于 2026-10-31 落地（history/MYSQL__MIGRATION_HISTORY.md，feature 门控），因此任何外部 `StateBackend` 实现都需满足新契约。
6. **`saveIfVersion(0)` 重复键回退是否要补**：Java **MySQL** 版无回退（`:477-500` 重复键直接 `UNVERSIONED`），而 Java **PG/JDBC** 版有 `UPDATE ... WHERE version=0` 回退（#3162，为覆盖 DDL 默认 0 回填的历史行）。本轮按 MySQL Java 语义实现；是否也要为 MySQL 补该回退（向后兼容历史库）属跨模块一致性问题，留待汇总方裁定（不改其它 crate）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mysql` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mysql` / `cargo test -p agentscope-extensions-mysql` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mysql/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/MYSQL__MIGRATION_HISTORY.md`](./history/MYSQL__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
