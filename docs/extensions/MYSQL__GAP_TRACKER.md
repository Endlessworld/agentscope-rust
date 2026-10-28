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
| Rust 规模（实测 2026-10-26） | `src/` 13 个 `.rs` 文件 / **3179** 行（非空行口径；含空行 3563）+ `Cargo.toml` 19 行；**70** 个 test 属性（49 `#[test]` + 21 `#[tokio::test]`，静态统计，实际执行数待 `cargo test` 复核） |
| 迁移波次 | B |
| 当前状态 | **进行中（2026-10-26：P1-1 列表变更检测、P1-2 CAS 原子性、P1-3 create-if-absent、P1-5 表名注入防护 已关闭，见 §7.3；剩余 P1：schema 存在性校验/`version` 列迁移（P1-6）、快照表自动建表（P1-7）、锁名 64 字符截断（P1-8）与 P2 若干）** |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `MysqlDistributedStore` | 组合 4 个组件成 `DistributedStore` | `MysqlDistributedStore` / `MysqlDistributedStoreBuilder`（`distributed_store.rs`） | 持有 4 组件 + `isolation_scope`；提供 `to_harness_distributed_store()` 输出 harness 子集 |
| `state.MysqlAgentStateStore` | MySQL 会话状态（单值 + 增量列表 + 版本） | `state::MysqlAgentStateStore` | `impl AgentStateStore`；`save_if_version`/`read_version` 作为固有方法保留版本语义。**2026-10-26 已对齐**：版本存 `item_index = 0` 行的 `version` 列（`StateBackend::read_version`/`save_single_versioned`），CAS 走后端原子原语 `insert_single_if_absent` / `update_single_if_version`（单条 `INSERT` / `UPDATE ... WHERE version = ?`）；列表用 core `list_hash`（`compute_hash` / `needs_full_rewrite`）做增量判断，`{key}:_hash` 伴随行只存**列表哈希**（见 §7.3） |
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
| `DataSource` / JDBC | 连接、SQL | **`SqlBackend` / `StateBackend` / `SnapshotBackend` / `LockBackend` 后端 trait** | 各组件依赖窄接口，不绑定驱动（4 个 trait 均已定义并接线，见 §6） |
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
| 单元测试（无真实 MySQL） | Java 测试 358 行 / 11 `@Test` | 已完成 | 内存后端 **70** 个 test 属性（静态统计：49 `#[test]` + 21 `#[tokio::test]`；2026-10-26 新增 25 个，见 §7.3。实际执行数待 `cargo test` 复核） |

### 3.2 待办 / 后续增强（P1/P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| 真实驱动适配器 | 用 `mysql_async`/`sqlx` 等实现 4 个后端 trait（可放入本 crate 的 feature 或上层应用） | 待办 |
| 集合/集成测试 | 需真实 MySQL 实例（当前仅内存后端验证语义） | 待办（环境受限） |
| `tracing` 日志 | 对齐 SLF4J 日志点 | 待办（P2） |
| `JdbcStore.validateIdentifier` 等价 | 表/库名合法性校验 | **已完成（2026-10-26）**：新增 `src/identifier.rs`（`validate_identifier`，`[A-Za-z_][A-Za-z0-9_]*` + 64 字符上限），接入 state（库名/表名）、`MysqlStoreBuilder::table_name`、`MysqlRemoteSnapshotClient::with_table`、`MysqlSnapshotSpec::with_table` 与 `MysqlDistributedStoreBuilder::try_build` |
| **列表变更检测（`ListHashUtil`）** | Java `save(list)` 用 `HASH_KEY_SUFFIX` + `ListHashUtil.computeHash/needsFullRewrite` 判定「无变化则跳过 / 仅前缀变化则全量重写 / 纯追加则只插尾部」；本 crate 原仅按 `list_len()` 追加，中间元素被改写时不会重写 | **已完成（2026-10-26）**：`state::save_list_impl` 复用 core `agentscope_core::state::list_hash`（`compute_hash` / `needs_full_rewrite`），`{key}:_hash` 行存列表哈希。测试：`save_list_detects_mutated_prefix`、`save_list_persists_core_list_hash_row`、`save_list_unchanged_skips_backend_write` |
| **版本 CAS 原子性** | Java 版本存于独立 `version` 列，`saveIfVersion` 走 `INSERT` / `UPDATE ... WHERE version=?`，由数据库保证原子；本 crate 原用 `key:_hash` 伴随行「先读后写」 | **已完成（2026-10-26）**：`StateBackend` 新增 `save_single_versioned` / `insert_single_if_absent` / `update_single_if_version`（契约为**单条**原子语句）与必填 `read_version`（读 `version` 列）；`save_if_version` 不再先读版本。测试：`save_if_version_delegates_cas_instead_of_read_then_write`、`cas_conflict_does_not_write`、`save_upsert_bumps_version_column`、`cas_update_requires_matching_version` |
| **`saveIfVersion(0)` 迁移行回退** | 原记录称「Java #3162 遇重复键回退 `UPDATE ... WHERE version=0`」 | **勘误 + 已完成（2026-10-26）**：题面复核 Java `MysqlAgentStateStore#insertIfAbsent`（`:477-500`）**没有**回退——重复键（1062）直接返回 `UNVERSIONED`（PG/JDBC 版才有回退）。Rust 现按 MySQL Java 语义实现：`expected==0` 为真实 create-if-absent，重复键冲突即 `Conflict`。该回退是否需要（PG/JDBC 一致性）留待汇总方裁定，记 §4.2 |
| **schema 校验/版本列补齐** | Java 构造期 `verifyDatabaseExists`/`verifyTableExists`/`ensureVersionColumn`（`ALTER TABLE ... ADD COLUMN version BIGINT NOT NULL DEFAULT 1`）；本 crate 只有 `ensure_schema()`（后端实现负责）与标识符校验，无存在性校验、无版本列迁移 | 部分（P1，**未变**；现已要求 `ensure_schema()` 必须建出 `version` 列，见 §7.3） |
| **锁名 64 字符上限** | Java `composeLockName` 超 64 字符截断为 `前50 + ":" + hex(hashCode)`（MySQL `GET_LOCK` 限制）；本 crate 无截断，长 key 会由驱动侧失败 | 待办（P1） |
| **表名注入防护（store/snapshot）** | Java `JdbcStore.Builder.tableName` 强制 `[A-Za-z_][A-Za-z0-9_]*`；本 crate `MysqlStoreBuilder::table_name`/`MysqlRemoteSnapshotClient::with_table` 原不做校验，DDL 直接拼接 | **已完成（2026-10-26）**：两处均改为返回 `MysqlResult`；`MysqlDistributedStoreBuilder` 新增 `try_build()`（`build()` 委托并 `expect`）在构造期校验。测试：`rejects_injection_payloads`（identifier）、`table_name_rejects_injection_payloads`、`with_table_rejects_unsafe_identifiers`、`try_build_rejects_unsafe_table_names` |
| **空列表语义（与 Java 相反）** | Java `save(list)` 在 `values.isEmpty()` 时**直接 return**（不触碰既有行）；本 crate 原以 `from_index=list_len()` + 空切片调用后端，内存后端按「total=0」修剪 → **清空**既有列表 | **已完成（2026-10-22，§7.1）**：`save_list` 对空切片直接返回（no-op），测试 `save_list_empty_is_a_no_op` |
| **`key_prefix` 归一化 + 默认锁超时** | Java `Builder.keyPrefix` 自动补尾部 `:`、拒绝空白，默认超时 1800s（30 分钟）；本 crate `key_prefix()` 原样使用（须自带冒号，否则拼出 `app:lockuser:u1`），默认超时 **30s** | 待办（P2） |
| **状态 key / session id 长度校验** | Java `validateStateKey`/`validateSessionId` 限长 255 且 key 非空；本 crate 仅校验 `session_id` 非空，**不限长、不校验 `state_key`** | 待办（P2） |
| **`truncateAllSessions` / `clearAllSessions`** | Java 提供测试/运维清理入口；本 crate 无对应 API（`delete` 仅按会话） | 待办（P2） |
| **`getDataSource` 访问器** | Java `getDatabaseName`/`getTableName`/`getDataSource`；本 crate 有前两者，无后者（后端 trait 抽象下不适用） | 设计决定（不迁移） |
| MySQL 键列二进制排序（`2349533a1`） | 本 crate `store/dialect.rs` 的 `namespace_path`/`item_key` 早已 `COLLATE utf8mb4_bin`；**同族修复（2026-10-20）落在 `agentscope-extensions-jdbc`**：其 `dialect/vendor/mysql.rs` 的会话状态表 `session_id`/`state_key` 与快照表 `snapshot_id` 已补 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`。存量库仍需手工 ALTER，见 `docs/migrations/mysql_binary_collation.md` | 已完成（2026-10-20） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **P0 全部已完成**：`crates/agentscope-extensions-mysql/` 全部 P0 组件实现，均**复用既有 trait**（`AgentStateStore` / `BaseStore` / `SandboxSnapshotSpec` / `RemoteSnapshotClient`），CAS/版本语义以 harness 为准。
- **P1 已关闭 4 项（2026-10-26）**：P1-1 列表变更检测、P1-2 CAS 原子性、P1-3 create-if-absent（原记录的回退前提已勘误）、P1-5 表名校验；P1-4 空列表 no-op 已在上轮（§7.1）关闭。**剩余 P1**：P1-6 schema 存在性校验/`version` 列迁移、P1-7 快照表自动建表、P1-8 锁名 64 字符截断。模块整体仍为「进行中」（详见 §7.3）。
- **文件清单**（实测 2026-10-26，14 个文件；`src/` 13 个 `.rs` / 3563 行含空行、**3179 行非空**）：
  - `Cargo.toml`（19 行；workspace 依赖：agentscope-core/harness、serde、serde_json、thiserror、async-trait、tokio、tracing、chrono，**另含未被任何源码引用的 `futures`**，见文末勘误；本轮**未新增依赖**）
  - `src/lib.rs`（56）、`src/error.rs`（69）、`src/identifier.rs`（97，**本轮新增**）
  - `src/store/{mod.rs 515, dialect.rs 374, testing.rs 178}`
  - `src/state/{mod.rs 786, testing.rs 481}`
  - `src/snapshot/{mod.rs 220, testing.rs 52}`
  - `src/guard/{mod.rs 319, testing.rs 61}`
  - `src/distributed_store.rs`（355）
- **验证命令与结果**（下列为 2026-10-20 上一会话的实测值，**2026-10-26 本轮按约定未跑 cargo**——改动后由主智能体统一验证）：
  - `cargo build -p agentscope-extensions-mysql` → **Finished**（本 crate 零 warning；harness 既有 warning 与本次无关）
  - `cargo test -p agentscope-extensions-mysql` → **43 passed; 0 failed**（另 0 doc-tests）
  - ⚠️ **待复核**：静态统计（2026-10-26）为 **70** 个 test 属性（49 + 21），需 `cargo test` 确认实际执行数
- **已知问题**：无阻塞。真实 MySQL 集成测试待环境。

### 4.2 下一步与阻塞
1. **跨模块待汇总项（不改动其它 crate）**：harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`（实测 `crates/agentscope-harness/src/agent/distributed_store.rs:14-17` 仅两字段），而 Java 版含 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`（另 `message_bus`/`async_tool_registry`/`task_repository`/`session_turn_gate`/`team_client`，后者此前亦遗漏）。本 crate 已在 `MysqlDistributedStore` 内保留全量 4 组件并通过 `to_harness_distributed_store()` 输出 harness 子集；是否扩展 harness `DistributedStore`（EXTENSIONS_GAP_TRACKER §2.2）仍待汇总方裁定。**实测补充**：harness `HarnessAgent` 目前也**只**从 `DistributedStore` 取 `agent_state_store`（`harness_agent.rs:1759/1890`），`base_store`/`snapshot_spec`/`guard` 在 harness 侧**无任何消费点**——即便扩展 `DistributedStore` 字段，也需 harness 侧同步接线才算真正生效。
2. **harness `SandboxExecutionGuard` 为具体 struct 而非 trait**（实测 `crates/agentscope-harness/src/agent/sandbox/guard.rs:13 pub struct SandboxExecutionGuard`，提供 `try_enter/release/is_held`）：故本 crate 无法 `impl` 之，改为镜像 Java API 的具体类型 `MysqlSandboxExecutionGuard`（`try_enter`/`release`/`is_held`/`active_count`）。已记入本 §4.2。
3. **真实驱动适配器**（`mysql_async`/`sqlx`）可由上层按需在本 crate 加 feature 实现后端 trait；当前不强加 workspace 级重依赖。
4. 监点（`tracing`）与真实 MySQL 集成测试可作为后续 P1/P2。
5. **`StateBackend` 缝的破坏性变更（2026-10-26，本 crate 内）**：为让 CAS 真正原子，trait 新增必实现方法 `save_single_versioned` / `insert_single_if_absent` / `update_single_if_version`，`read_version` 由「默认实现（读 `:_hash` 行）」改为**必填**（读 `version` 列）；删除 `write_version`（唯一用途是写 `:_hash` 版本行）。`ensure_schema()` 的实现**必须**建出 `version BIGINT NOT NULL` 列（Java `ensureVersionColumn`，默认值 1 或不带默认亦可，CAS 语义要求初始为 1）。真实驱动适配器尚未落地（§3.2 第 1 行），故目前无外部实现受影响；若上层已有实现，需同步补齐。
6. **`saveIfVersion(0)` 重复键回退是否要补**：Java **MySQL** 版无回退（`:477-500` 重复键直接 `UNVERSIONED`），而 Java **PG/JDBC** 版有 `UPDATE ... WHERE version=0` 回退（#3162，为覆盖 DDL 默认 0 回填的历史行）。本轮按 MySQL Java 语义实现；是否也要为 MySQL 补该回退（向后兼容历史库）属跨模块一致性问题，留待汇总方裁定（不改其它 crate）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mysql` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mysql` / `cargo test -p agentscope-extensions-mysql` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mysql/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态**：只读源码 + 文本统计（`Select-String` 正则计数、逐文件行数 / 非空行数）。**未**运行 `cargo build` / `cargo test`，**未**修改任何 Rust 源码。
- 对照源：`agentscope-java/agentscope-extensions/agentscope-extensions-mysql`（Java 侧 `git status` 干净，HEAD `ed8073259`，含 `2349533a1`/`7854f0de7`/`c6af78183`/`a2cbea5c3`）。
- 契约基准：`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1（B 波次统一产出契约）。
- **行数口径说明**：rollup「Java 主/测行」列为**非空行**口径（已用 oss/mem/cos/skills 等 9 个模块交叉验证：rollup 值 == 非空行数）。故本节同时给出「含空行总行数」与「非空行数」，以非空行为对外口径。

### 6.2 实测统计
**Java（`agentscope-extensions-mysql`）**

| 项 | 实测 |
|---|---|
| `.java` 文件数 | 14（main 11 + test 3） |
| 主代码行数 | **非空 2106** / 含空行 2323（rollup 原记 2070，偏差 +36） |
| 测试行数 | **非空 358** / 含空行 416（rollup 原记 204，偏差 +154） |
| `@Test` 数 | 11（`MysqlAgentStateStoreTest` 7 / `JdbcRemoteSnapshotClientTest` 2 / `MysqlJdbcStoreDialectTest` 2） |
| 主类 | `MysqlDistributedStore`、`state.MysqlAgentStateStore`、`store.{JdbcStore,JdbcStoreDialect,Mysql/Postgres/Sqlite/H2 JdbcStoreDialect}`、`snapshot.{JdbcSnapshotSpec,JdbcRemoteSnapshotClient}`、`sandbox.JdbcSandboxExecutionGuard`（共 11） |
| 测试类 | `state.MysqlAgentStateStoreTest`(258)、`snapshot.JdbcRemoteSnapshotClientTest`(83)、`store.MysqlJdbcStoreDialectTest`(75) |
| 主类逐文件（含空行） | `state/MysqlAgentStateStore` 1053、`store/JdbcStore` 423、`store/JdbcStoreDialect` 179、`sandbox/JdbcSandboxExecutionGuard` 176、`snapshot/JdbcRemoteSnapshotClient` 127、`MysqlDistributedStore` 98、`store/H2` 61、`store/Mysql` 59、`store/Sqlite` 57、`store/Postgres` 54、`snapshot/JdbcSnapshotSpec` 36 |

**Rust（`crates/agentscope-extensions-mysql`）**

| 文件 | 含空行 | 非空行 | `#[test]` | `#[tokio::test]` |
|---|---|---|---|---|
| `Cargo.toml` | 19 | 18 | — | — |
| `src/lib.rs` | 54 | 52 | 0 | 0 |
| `src/error.rs` | 69 | 58 | 0 | 0 |
| `src/distributed_store.rs` | 302 | 267 | 2 | 1 |
| `src/store/mod.rs` | 469 | 412 | 8 | 0 |
| `src/store/dialect.rs` | 374 | 325 | 8 | 0 |
| `src/store/testing.rs` | 178 | 161 | 2 | 0 |
| `src/state/mod.rs` | 535 | 483 | 1 | 9 |
| `src/state/testing.rs` | 165 | 147 | 1 | 0 |
| `src/snapshot/mod.rs` | 187 | 162 | 2 | 2 |
| `src/snapshot/testing.rs` | 52 | 43 | 1 | 0 |
| `src/guard/mod.rs` | 319 | 282 | 7 | 0 |
| `src/guard/testing.rs` | 61 | 51 | 1 | 0 |
| **合计** | **2784**（src 2765） | **2461**（src **2443**） | **33** | **12** → **45** |

- 源码内 **0 处** `tracing::` 调用；**0 处** `futures` 引用（`Cargo.toml` 仍声明该依赖）；`chrono` 1 处（`SqlBackend::now_ms` 默认实现）。
- **0 处** `list_hash`/`compute_hash`/`needs_full_rewrite` 引用（对比：`agentscope-extensions-jdbc` `jdbc_agent_state_store.rs:16-17`、`agentscope-extensions-postgresql` `state.rs:11` 均已接入 core `list_hash`）。
- 无 `#[ignore]`、无 `cfg(feature)` 门控、无 doc-test（`//!` 内无 ``` 代码块）。
- **静态 test 属性数 45（33 + 12）**；上轮记录的 `cargo test` 运行值 **43** 尚未重跑校准（差 2 待验，非本轮可静态判定）。

### 6.3 B 波次统一产出契约核对（EXTENSIONS_GAP_TRACKER §2.1）

| 契约项 | Rust 产出 | 存在 | 接线 |
|---|---|---|---|
| `XxxDistributedStore` | `MysqlDistributedStore` + `MysqlDistributedStoreBuilder`（`distributed_store.rs`） | ✅ | 4 组件 + `isolation_scope` 全量持有；`to_harness_distributed_store()` **全量投影 5 槽**（2026-10-28） |
| `XxxAgentStateStore` | `state::MysqlAgentStateStore`（`impl AgentStateStore`） | ✅ | 实现 `save/save_list/get/get_list/exists/delete/list_session_ids/supports_versioning/get_versioned/save_if_version` |
| `XxxStore`（`BaseStore`） | `store::MysqlStore`（`impl BaseStore`） | ✅ | `get/put/put_if_version/search/delete/supports_atomic_session_storage` |
| `XxxSnapshotSpec` | `snapshot::MysqlSnapshotSpec`（`impl SandboxSnapshotSpec`，包 `RemoteSnapshotSpec`） | ✅ | `build()` 委托 harness，快照 id 校验复用 harness |
| `XxxSandboxExecutionGuard` | `guard::MysqlSandboxExecutionGuard` + `MysqlLockLease` + `LockBackend` | ✅ | builder 注入 `Arc<dyn LockBackend>`；`try_enter/release/is_held/active_count`；lease `Drop` 自动释放 |
| 统一导出 | `lib.rs` 导出全部 5 类 + 4 个后端 trait | ✅ | — |

> **结论**：§2.1 的 5 项契约 **全部存在且已接线**（`LockBackend` 注入缝真实：`builder(backend)` → 字段 `Arc<dyn LockBackend>` → 三个调用点 `try_acquire`/`release_lock`/`is_held`，并有 `InMemoryLockBackend` 测试双）。`MysqlSandboxExecutionGuard` **不是** `impl` harness 类型而是镜像 API 的具体类型——原因（harness guard 为具体 struct）**已核实为真**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13 pub struct SandboxExecutionGuard`），但该 struct 实际已提供 `try_enter/release/is_held/active_count`，与 crate 内镜像签名几乎一致。
>
> **接线深度的实测补充（较上轮更精确）**：`MysqlSandboxExecutionGuard` 与 `MysqlStore`/`MysqlSnapshotSpec` 在本 crate 内**齐备且自洽**（`MysqlDistributedStore` 持有 4 组件并暴露访问器），但 harness 侧目前**无消费点**：
> - harness `DistributedStore`（`agent/distributed_store.rs:14-17`）仅 `agent_state_store` + `isolation_scope` 两字段，**无** `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`；
> - harness `HarnessAgent` 也**只**从 `DistributedStore` 取 `agent_state_store`（`harness_agent.rs:1759`、`:1890`）。
>
> 即：本 crate 的 4 组件中，**只有 `MysqlAgentStateStore` 能真正驱动 harness 运行时**；其余 3 个（BaseStore / SnapshotSpec / Guard）目前是「已实现但未接入」。这是跨 crate 的 harness 公共层缺口（EXTENSIONS_GAP_TRACKER §2.2 已裁定「不改 harness」），**不是本 crate 可单独闭合的缺口**。

### 6.4 对齐结论表（Java 类 → Rust）

| Java 类 | Rust 现状 | 判定 |
|---|---|---|
| `MysqlDistributedStore` | `MysqlDistributedStore`/`Builder`，4 组件齐全 | 等价（超出：可配置表名/前缀/scope） |
| `state.MysqlAgentStateStore` | `state::MysqlAgentStateStore` | **部分（2026-10-26 起：版本语义/列表变更检测已等价）**：API 面等价；剩余差异 = schema 存在性校验、`truncateAllSessions`、`state_key` 长度校验 |
| `store.JdbcStore` | `store::MysqlStore` | 等价（CAS/命名空间/前缀搜索/分页语义对齐；表名校验**已补**，2026-10-26） |
| `store.JdbcStoreDialect` | `store::StoreDialect` | 等价（模板 6 条齐全 + `like_escape_char`/`render`） |
| `store.MysqlJdbcStoreDialect` | `store::MysqlStoreDialect` | 等价（DDL/upsert/insert/cas 逐字对齐） |
| `store.Postgres/Sqlite/H2 JdbcStoreDialect` | 同名 `*StoreDialect` | **部分**：PG/H2 `namespace_path` 512 vs Java 2048（长度收窄）；**H2 upsert 双方均为 `MERGE INTO ... USING (VALUES ...)`，实际等价**（Java 类 Javadoc 声称 `ON CONFLICT` 为过时注释，实际代码为 `MERGE`）；PG/SQLite upsert 与 Java 逐字一致 |
| `snapshot.JdbcSnapshotSpec` | `snapshot::MysqlSnapshotSpec` | **部分**：`build` 委托等价；**无** 建表（Java 构造期 `initializeSchema=true` 直接发 DDL） |
| `snapshot.JdbcRemoteSnapshotClient` | `snapshot::MysqlRemoteSnapshotClient` | 等价（upload/download/exists + 默认表名 + 空白表名回退默认；Java `tableName=null` 回退默认，Rust 空白亦回退） |
| `sandbox.JdbcSandboxExecutionGuard` | `guard::MysqlSandboxExecutionGuard` | **部分**：锁获取/释放/超时等价；**无** 64 字符锁名截断；`key_prefix` 无 Java 的补冒号/拒空白归一化；默认超时 30s vs Java 1800s；`sub_key` 为 Rust 扩展 |
| `HASH_KEY_SUFFIX`（列表哈希） | `state::HASH_KEY_SUFFIX`（2026-10-26 起 = 列表哈希） | 等价（语义已归位；版本改存行内 `version` 列） |
| （Java `MysqlDistributedStore` 已 `@Deprecated(forRemoval=true)`，官方替代为 `agentscope-extensions-jdbc` 的 `JdbcDistributedStore`） | 本 crate 仍完整实现 `MysqlDistributedStore` | **等价但过时**：保留符合 Rust 侧迁移范围（Java 模块整体迁移），但对外应优先推荐 JDBC crate |

### 6.5 新发现缺口

> **历史快照（2026-10-22 静态复核）**：P1-1 / P1-2 / P1-3 / P1-5 已于 **2026-10-26** 关闭（见 §7.3），P1-4 已于 §7.1 关闭；下表保留原样以存档。

**P0**：无。（§2.1 五项契约齐全；`lib.rs` 导出完整；无编译级断链的静态迹象。）

**P1（正确性/安全，建议本轮或下一轮修）**

| # | 缺口 | Java 路径 | Rust 现状 |
|---|---|---|---|
| P1-1 | 列表**变更检测缺失**：中间元素被改写时 `save_list` 只按 `list_len()` 追加尾部，陈旧数据留存；Java 用 `ListHashUtil.needsFullRewrite` 全量重写 | `state/MysqlAgentStateStore.java:566-601`（+ `core.state.ListHashUtil`） | 缺失（crate 内 `list_hash` 引用数 0；core 已有 `agentscope_core::state::list_hash` 可直接复用，JDBC/PG 已接入） |
| P1-2 | 版本 **CAS 非原子**：`read_version`→比对→`save`→`write_version` 先读后写，并发双写均可成功 | `state/MysqlAgentStateStore.java:464-546`（`INSERT`/`UPDATE ... WHERE version=?`） | 部分（语义有、原子性无；版本存 `key:_hash` 伴随行而非 `version` 列） |
| P1-3 | `saveIfVersion(0)` **重复键回退**缺失（#3162 迁移回填 version=0 行） | 同上 `:481-493`、`:501-524` | 缺失（直接返回 `InvalidArgument` 冲突） |
| P1-4 | 空列表 **no-op** 语义相反：Java `values.isEmpty()` 直接 return；Rust 会以 `from_index` + 空切片调用 `save_list`，后端按「total=0」修剪 → **清空**既有列表 | `state/MysqlAgentStateStore.java:571-573` | 部分（行为相反，`InMemoryStateBackend` 实测会全删） |
| P1-5 | **表名注入防护**：`MysqlStoreBuilder::table_name` / `MysqlRemoteSnapshotClient::with_table` 不校验，DDL 直接拼接 | `store/JdbcStore.java:88,392-401`（`[A-Za-z_][A-Za-z0-9_]*`） | 缺失（state 模块有 `validate_identifier`，store/snapshot 无） |
| P1-6 | **schema 存在性校验 + `version` 列迁移**：Java 构造期 verify DB/table + `ALTER TABLE ADD COLUMN version` | `state/MysqlAgentStateStore.java:174-214,272-321` | 缺失（仅 `ensure_schema()` 交后端 + 标识符校验；`with_config(create_if_not_exist=false)` 不校验存在性） |
| P1-7 | **快照表自动建表**：Java `JdbcSnapshotSpec` 恒传 `initializeSchema=true` 发 DDL | `snapshot/JdbcSnapshotSpec.java:29-35`、`JdbcRemoteSnapshotClient.java:61-81` | 缺失（`MysqlSnapshotSpec` 无 DDL，依赖后端实现） |
| P1-8 | **锁名 64 字符截断**：`GET_LOCK` 上限 64，Java 超长截 50 + `hex(hashCode)` | `sandbox/JdbcSandboxExecutionGuard.java:99-107` | 缺失（无截断、无长度校验） |

**P2（增强/清理）**

| # | 缺口 | 说明 |
|---|---|---|
| P2-1 | `tracing` 日志点 | 依赖已声明，源码 0 处调用；Java 有 `log.debug/warn`（store 构建、guard 获取/释放、快照建表失败降级） |
| P2-2 | 未用依赖 `futures` | `Cargo.toml:16` 声明但源码 0 引用，建议移除 |
| P2-3 | `truncateAllSessions` / `clearAllSessions` | Java 有（运维/测试清理）；Rust 无 |
| P2-4 | 方言 DDL 长度不一致 | PG/H2 `namespace_path` 512（Java 2048）；**H2 upsert 语法族实际一致（双方均 `MERGE INTO`），不属缺口**（见 §6.7 勘误 #11） |
| P2-5 | `MysqlDistributedStoreBuilder::build` 用 `.expect()` / `unwrap_or_else` 吞掉 state store 构造错误 | `distributed_store.rs:184-196`：`initialize_schema=true` 失败时静默降级为 `false`，再失败则 panic；Java 构造期直接抛 `RuntimeException` |
| P2-6 | `MysqlAgentStateStore::new` 忽略 `ensure_schema()` 错误（`let _ =`） | `state/mod.rs:155`；与 Java「构造即校验」相悖 |
| P2-7 | **`key_prefix` 缺 Java 的归一化** | Java `Builder.keyPrefix` 会自动补尾部 `:` 并拒绝空白/空串；本 crate `key_prefix()` 原样保存（`guard/mod.rs:207-211`），调用方若传 `"app:lock"` 会拼出 `app:lockuser:u1`（缺分隔符） |
| P2-8 | **默认锁超时不一致** | Java 默认 **1800s**（30 分钟，`JdbcSandboxExecutionGuard.java:145`）；本 crate 默认 **30s**（`guard/mod.rs:128`）。超时过短会让长任务在正常负载下被误判为争用失败 |
| P2-9 | **`state_key` / `session_id` 长度未校验** | Java `validateStateKey`/`validateSessionId` 限长 255、key 非空；本 crate 仅校验 `session_id` 非空（`state/mod.rs:392-397`），`state_key` 完全不校验 |

### 6.6 测试覆盖薄弱区（Rust 45 test 属性 vs Java 11 `@Test`）

> **历史快照（2026-10-22）**：本节统计为 P1 修复前口径。2026-10-26 起静态统计 **70** 个 test 属性，其中 `save_if_version(0)` 的 create-if-absent / 冲突 / CAS 委托路径已补测试（见 §7.3）。

**数量与广度**：Rust 45 个 test 属性在 **API 形状** 上明显超过 Java 11 条（store CAS/分页/前缀搜索、guard 争用/幂等/Drop/自定义前缀、distributed 四组件装配、state 列表增缩/会话列举/匿名用户隔离）。但 Java 11 条 `@Test` 中有 **7 条** 对应行为在 Rust **既无测试亦无实现**（或无断言对象），属**静默漂移区**：

| Java 测试 | 覆盖行为 | Rust 对应 | 缺口 |
|---|---|---|---|
| `saveIfVersionZeroFallsBackToUpdateOnDuplicateKey` | 重复键回退 CAS（#3162） | **缺失** | 功能未实现（P1-3）+ 无测试 |
| `saveIfVersionZeroConflictWhenRowAlreadyVersioned` | 已过 version 0 时仍报冲突 | **缺失** | 无 create-if-absent 重复键路径测试 |
| `saveIfVersionUnconditionalPropagatesReadVersionException` | 版本读取失败向上传播 | **缺失** | 内存后端不会失败，无错误注入双（`StateBackend` 可自定义但无用例） |
| `saveIfVersionUnconditionalAbsentKeyReturnsZero` | 键缺失时版本 0 | **部分** | `read_version` 缺「absent → 0」直测（`state/mod.rs:104-107` 已实现，行为未断言） |
| `saveIfVersionUnconditionalUsesReadVersion` | readVersion 只读 `version` 列、**不反序列化 `state_data`** | **缺失** | Rust 版本存伴随行（读的是 `key:_hash` 的 JSON），语义不同，无对应断言 |
| `createTablePinsBinaryCollationOnKeyColumnsOnly`（state） | 会话表 DDL 二进制排序 | **缺失** | crate 内无 state DDL，无可断言对象 |
| `initSchemaPinsBinaryCollation` / `payloadStaysOnTableDefault`（snapshot） | 快照表 DDL 二进制排序 | **缺失** | 同上，无 snapshot DDL |
| `createTablePrimaryKeyFitsInnoDbUtf8mb4Limit` | 复合主键 ≤3072 字节 | **部分** | `mysql_key_columns_use_binary_collation` 断言了 `(512)`/`(255)` 字面量，但**未做字节上限计算断言**（Java 侧是显式 `(512+255)*4 = 3068 ≤ 3072` 的计算断言） |

**Rust 45 条中未覆盖的关键 Java 行为（反向核对）**：

| 行为 | Rust 现状 | 关联缺口 |
|---|---|---|
| 列表**中间元素改写**后 `get_list` 应返回新值 | **无测试**（`save_list` 语义本身也不正确） | P1-1 |
| 空列表 `save_list` 应为 **no-op**（不清空） | **无测试**（实际行为相反，会清空） | P1-4 |
| `save_if_version` 在 `expected=0` 时的 create-if-absent 语义 | 仅测 `None`（unversioned）与 `Some(1)` 冲突，**未测 `Some(0)`** | P1-3 |
| 锁名超 64 字符被截断 | **无测试**（功能亦缺） | P1-8 |
| `key_prefix` 缺尾部 `:` 时的行为 | **无测试** | P2-7 |
| 默认锁超时值 | **无测试**（30s vs Java 1800s） | P2-8 |

> **结论**：Rust 测试**数量**（45 > 11）与**API 形状**覆盖强于 Java，但**「语义正确性」维度弱于「API 形状」维度**——Java 有测试守住的 4 条 CAS/版本行为在 Rust **既无实现也无测试**，另有 3 条 DDL 类断言因本 crate 不持有 DDL 而天然不可迁移（应由后端适配层或集成测试承担）。P1-1~P1-4 属静默漂移区，是本轮最应优先补齐的部分。

### 6.7 勘误列表（原文 → 实际）

| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | 主代码 ~2070 行 / 测试 ~204 行 | **非空行**口径下：主代码 **2106** 行 / 测试 **358** 行（含空行则为 2323 / 416；rollup 原值 2070/204 已过期） |
| 2 | §3.1 | 「内存后端 43 个测试全过」 | 静态统计 **45** 个 test 属性（33 `#[test]` + 12 `#[tokio::test]`）；43 为上一会话运行值，**待重跑确认** |
| 3 | §4.1 文件清单 | 未给行数/文件数 | 13 文件 / 2784 行（src 12 个 `.rs` / 2765 行；**非空 2443**） |
| 4 | §2 映射表 `_hash` 行 | 「`_hash` 版本键 … state 版本伴随行 … 保留跨语言一致性」 | **语义错误**：Java `HASH_KEY_SUFFIX` 是**列表哈希**键（`ListHashUtil` 变更检测），**不是**版本键；Rust 把它挪用为版本键，属偏离而非一致 |
| 5 | §2 映射表 `MysqlJdbcStoreDialect` 行 | 「其余方言一并迁移」 | 方言类齐全，但 PG/H2 `namespace_path` 为 512（Java 2048）；**H2 upsert 语法族无差异（见 #11）** |
| 6 | §2 映射表 `JdbcSandboxExecutionGuard` 行 | 仅注「harness 为具体 struct」 | 另缺 Java 的 64 字符锁名截断、`key_prefix` 补冒号归一化、默认超时 1800s→30s；`sub_key` 是 Rust 新增（Java 无） |
| 7 | §2.1 依赖映射 SLF4J 行 | 「`tracing`（依赖已接入，暂未写日志）」 | 属实（0 处调用），但 `futures` 同样声明未用，未记录 |
| 8 | §4.2 第 1 条 | Java `DistributedStore` 另含 `message_bus`/`async_tool_registry`/`task_repository` | 漏 `session_turn_gate`（Java 接口另有 `default SessionTurnGate sessionTurnGate()`）**与 `team_client`**（`default TeamClient teamClient()`） |
| 9 | §1 / 页首 | `(鍗曟ā鍧?`（乱码） | 应为「（单模块）」，已修 |
| 10 | §1 当前状态 | 「已完成（P0 全部落地…）」 | P0 契约属实，但存在 8 项 P1（含 3 项数据正确性/语义偏离）与 9 项 P2，状态需降级为「进行中」 |
| 11 | §2 映射表 + §6.4 + §6.5 P2-4 | 「H2 upsert 用 `MERGE INTO ... USING (VALUES ...)`，Java 用 H2 2.x 的 `ON CONFLICT ... DO UPDATE`」 | **原文错误**：Java `H2JdbcStoreDialect.getUpsertSql()`（`:40-47`）**实际返回 `MERGE INTO ... USING (VALUES ...)`**，与 Rust **完全一致**；Java 类 Javadoc（`:21-22`）写 `ON CONFLICT` 属**过时注释**。**H2 upsert 不构成缺口**，仅 PG/H2 的 `namespace_path` 长度（512 vs 2048）仍是差异 |
| 12 | §6.3 结论 | 「5 项契约全部存在且已接线」 | 表述过强：契约产出齐备属实，但 harness 侧 `DistributedStore` 仅 2 字段、`HarnessAgent` 仅消费 `agent_state_store`，故 4 组件中**只有 `MysqlAgentStateStore` 真正驱动 harness 运行时**（已补精确说明） |
| 13 | §6.2 Java 统计 | 「主代码 2323 行 / 测试 416 行（tracker 原记 ~2070，偏差 +253）」 | 偏差计算基于**含空行**口径；rollup 用**非空行**口径，实际偏差为 **+36 / +154** |
| 14 | §6.6 | 「Java 11 条里有 4 条对应行为在 Rust 无任何测试」 | 实为 **7 条**（另加 `saveIfVersionUnconditionalUsesReadVersion`、state/snapshot 两组 DDL 断言，且 `createTablePrimaryKeyFitsInnoDbUtf8mb4Limit` 缺字节计算断言） |
| 15 | §4.2 第 1 条 | 「Java 版含 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`」 | 漏 `team_client`；且实测 harness 侧**无任何** `base_store`/`snapshot`/`guard` 消费点（已补） |

### 6.8 建议 rollup 状态

**建议 `EXTENSIONS_GAP_TRACKER.md` 第 26 行（MYSQL）由「已完成」改为「进行中」**，理由：
1. §2.1 五项 B 波次契约**确实齐全且已接线**（`LockBackend` 注入缝为真），P0 无缺口 → **不应**退回「待办」；
2. 但 P1-1（列表中间改写丢更新）、P1-2（CAS 非原子）、P1-4（空列表语义与 Java **相反**，会误删数据）属**数据正确性**缺口，不宜对外标「已完成」；
3. P1-5（表名未校验）属 SQL 注入面，安全维度不可留白；
4. rollup 现有数字两处过期需校准：Java 主/测行 **2070 / 204 → 2106 / 358**（非空行口径），Rust test 数 **43 → 45**（静态统计，执行数待 `cargo test` 重跑）。

> 建议 rollup 行文本：
> `| 10 | B | MYSQL | `agentscope-extensions-mysql` | 2106 / 358 | 进行中（build + test 通过；**45 test**；B 波次 5 项契约齐全：`MysqlDistributedStore`/`MysqlAgentStateStore`/`MysqlStore`/`MysqlSnapshotSpec`/`MysqlSandboxExecutionGuard` + `LockBackend` 注入缝；**P1 缺口 8 项**：列表变更检测缺失、CAS 非原子、`saveIfVersion(0)` 回退缺失、空列表语义相反、表名未校验、schema 校验缺失、快照表不自动建表、锁名未截断，见 `MYSQL__GAP_TRACKER.md` §6） | `MYSQL__GAP_TRACKER.md` |`



## 7. §4.3 P0 修复记录（2026-10-22，汇总方）

### 7.1 已修：`save_list([])` 静默清空数据（§4.3 #1）

- **缺陷**：未实现 Java 的 `if (values.isEmpty()) return;`（`MysqlAgentStateStore.java:571-573`）。空写会带着 `from_index = list_len(...)` 进入后端，`total = 0` 触发「裁剪高于新长度的尾项」→ 原有列表被整体清空。
- **修复**：`state/mod.rs` 的 `save_list` 在 session 校验后增加空列表早退；语义为 **空写 = no-op**（不是 clear）。
- **用例**：`save_list_empty_is_a_no_op`（先写 `[1,2]`，再写 `[]`，断言仍为 `[1,2]`；无守卫时该用例必然失败）。
- **验证**：`cargo test -p agentscope-extensions-mysql` → **46 passed / 0 failed**。
- **注意**：清空列表应走 `delete`，与 Java 一致；不要把它当成 `save_list([])` 的替代语义。

### 7.2 仍待办（本模块 P1）

- ~~列表变更检测缺失~~ → **已完成（§7.3）**。
- ~~CAS 非原子~~ → **已完成（§7.3）**。
- ~~表名无校验~~ → **已完成（§7.3）**。
- **仍待办**：schema 存在性校验 + `version` 列迁移（P1-6）、快照表自动建表（P1-7）、锁名 64 字符截断（P1-8）、默认锁超时 **30s vs Java 1800s**（P2-8）；其余见 §3.2。

### 7.3 P1 数据正确性 / 注入面修复（2026-10-26，本轮）

> 范围：只改 `crates/agentscope-extensions-mysql/**` + 本 tracker；**未跑 cargo**（按本轮约定由主智能体验证）。测试全部离线（`InMemory*` 测试双），无新增依赖。

**(1) P1-1 列表变更检测（`ListHashUtil`）**

- **改前失败点**：`save_list` 只取 `list_len()` 作 `from_index` 追加 + 修剪尾部，中间元素被改写时**不重写**；`save_list([1,2,3])` → `save_list([1,9,3])` 后 `get_list` 仍返回 `[1,2,3]`。
- **改法**：`state::save_list_impl` 复用 core `agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite}`（与 JDBC/PG 同一实现），`{key}:_hash` 行存哈希 → 三分支：全量重写（`from_index = 0`）/ 仅追加尾部 / 无变化直接跳过（Java 同）。
- **新增测试**：`save_list_detects_mutated_prefix`、`save_list_persists_core_list_hash_row`、`save_list_unchanged_skips_backend_write`。

**(2) P1-2 / P1-3 版本 CAS 原子性**

- **改前失败点**：版本存 `{key}:_hash` 伴随行，`save_if_version` 走 `read_version` → 比对 → `save` → `write_version` **先读后写**，两个并发写者可同时通过；且无条件 `save` 不 bump 版本（`save_upsert_bumps_version_column` 改前读到 0）。
- **改法**：版本归位到 `item_index = 0` 行的 `version` 列（Java 同）。`StateBackend` 新增三个**原子原语**（契约写明必须单条语句）：`save_single_versioned`（`INSERT ... 1 ON DUPLICATE KEY UPDATE version = version + 1`，返回本次写入产生的版本）、`insert_single_if_absent`（重复键 → `CasOutcome::Conflict`）、`update_single_if_version`（`UPDATE ... WHERE version = ?`，0 行受影响 → `Conflict`）；`read_version` 改为必填（读列，不反序列化 payload），删除 `write_version`。新增 `CasOutcome` 枚举。`save_if_version` 三个分支（`None` / `Some(0)` / `Some(n)`）各委托一个原语，不再先读版本。
- **新增测试**：`save_if_version_delegates_cas_instead_of_read_then_write`（用 `read_version` 恒返回 0 的 `StaleVersionBackend` 证明写路径不经「先读」）、`cas_conflict_does_not_write`、`save_if_version_zero_creates_then_conflicts`、`save_upsert_bumps_version_column`、`get_versioned_returns_stored_version`；测试双侧 `versioned_save_starts_at_one_and_bumps`、`unversioned_hash_row_does_not_touch_version`、`create_if_absent_conflicts_when_row_exists`、`cas_update_requires_matching_version`、`save_list_from_zero_rewrites_every_slot`。
- **P1-3 勘误**：原记录称「Java #3162 在 `expected==0` 遇重复键时回退 `UPDATE ... WHERE version=0`」——复核 Java `MysqlAgentStateStore#insertIfAbsent`（`:477-500`）**无该回退**（重复键 1062 直接返回 `UNVERSIONED`）；回退只存在于 PG/JDBC 版。故本轮按 MySQL Java 语义实现，是否补回退见 §4.2 第 6 条。
- **存量数据自愈**：旧实现的 `{key}:_hash` 行存的是**版本号**而非哈希。升级后首次 `save_list` 的 `needs_full_rewrite` 必然判定「哈希不匹配」→ 全量重写并覆盖该行，数据无损（仅多一次全量写）；版本改读 `version` 列，故**存量库需先跑 `ALTER TABLE ... ADD COLUMN version BIGINT NOT NULL DEFAULT 1`**（P1-6 未做，迁移脚本仍待补）。

**(3) P1-5 表名/标识符注入防护**

- **改前失败点**：`MysqlStoreBuilder::table_name("x; DROP TABLE y")`、`MysqlRemoteSnapshotClient::with_table("`t`")` 原样接受并被拼进 DDL/DML。
- **改法**：新增 `src/identifier.rs`（`validate_identifier`：`[A-Za-z_][A-Za-z0-9_]*`、≤ 64 字符、去首尾空白；非法即 `MysqlError::InvalidArgument`，消息带 label 与原文）；state 模块改用该函数（原本地副本删除，`-` 亦被拒——MySQL 未加反引号时本就非法）；`MysqlStoreBuilder::table_name` / `MysqlRemoteSnapshotClient::with_table` / `MysqlSnapshotSpec::with_table` 改为返回 `MysqlResult<Self>`；`MysqlDistributedStoreBuilder` 新增 `try_build() -> MysqlResult<...>`（`build()` 委托 + `expect`），在构造期完成校验。
- **新增测试**：`rejects_injection_payloads` / `accepts_plain_identifiers` / `trims_surrounding_whitespace` / `enforces_length_limit` / `error_message_names_the_label`（identifier.rs）、`table_name_rejects_injection_payloads`、`table_name_enforces_length_limit`、`table_name_is_trimmed_and_applied`、`with_table_rejects_unsafe_identifiers`、`try_build_rejects_unsafe_table_names`、`try_build_accepts_safe_table_names`。

**未做（保留待办）**：P1-6 schema 存在性校验/`version` 列迁移、P1-7 快照表自动建表、P1-8 锁名 64 字符截断、P2-1…P2-9 全部（见 §3.2）。

**给主智能体的编译风险提示**：①`StateBackend` trait 新增必实现方法 + `read_version` 改为必填 → 任何其它实现（本轮仅 crate 内 `InMemoryStateBackend` / `StaleVersionBackend`）需补齐；②`table_name` / `with_table` 返回类型由 `Self` 变为 `MysqlResult<Self>`，crate 内调用点已全部改（`?` + `.expect()`），crate 外若有调用会断；③`list_hash::compute_hash(&[serde_json::Value])` 依赖 `Value: Hash`（serde_json 已 derive，JDBC/PG 同用法）。

**格式说明**：本轮对改动文件跑过一次 `rustfmt`（仅格式化，无语义变化），因此 diff 中夹带少量**原有代码**的换行/导入排序变动（`store/mod.rs`、`state/mod.rs`、`snapshot/mod.rs`）；`store/dialect.rs`、`store/testing.rs` 被 rustfmt 顺带改动后已 `git restore` 还原，未纳入 diff。

## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_harness_distributed_store()` 改为 builder 写法并接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `Arc<MysqlAgentStateStore>` | 会话状态 |
| `isolation_scope` | `IsolationScope` | 字段 `isolation_scope` | 默认 `User` |
| `base_store` | `Arc<dyn BaseStore>` | `Arc<MysqlStore>` | 工作区 KV |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc<MysqlSnapshotSpec>` | 快照 BLOB |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc<MysqlSandboxExecutionGuard>` | `GET_LOCK()`；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_harness_distributed_store()`：填满 5 槽（builder，不用 `..Default::default()`）。
- `src/guard/mod.rs`：新增私有字段 `trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<MysqlLockLease>>>>`（harness 租约无释放钩子，需保留具体租约否则提前解锁）+ `impl SandboxGuard`；`try_enter` 超时降级为 `SandboxLease::noop()`（与 JDBC guard 一致）。
- 测试：`src/distributed_store.rs::to_harness_distributed_store_fills_every_composite_slot`。

### 验证

`cargo test -p agentscope-extensions-mysql` → **71 passed / 0 failed**。

### 未完成 / 语义差异

- 无留空槽位；仍存在 §3 已登记的 Java 差异（64 字符截断、默认超时 1800s vs 30s），本次未变更。