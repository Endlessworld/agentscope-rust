# POSTGRESQL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **POSTGRESQL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-postgresql\`
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包）
> **Rust 目标**：`crates/agentscope-extensions-postgresql/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

> **当前结论（2026-09-18 回填）**：P0 组件已全部接入且 `build`/`test` 通过；但**离线测试覆盖存在 P0 缺口**（5 个测试且全部集中在 `sql.rs`），以及若干 P1/P2 语义差异，详见 §3 与 §4。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-postgresql` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包） |
| Java 规模（初测） | 主代码 ~1571 行（6 个类）/ 测试 ~1583 行（5 个测试类，约 130+ `@Test`） |
| Rust crate | `crates/agentscope-extensions-postgresql` |
| 迁移波次 | B |
| 当前状态 | **已完成 P0 骨架/组件接线（build + test 通过）；测试覆盖存在 P0 缺口，见 §3.1、§4.2** |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `postgresql.PostgresDistributedStore`（`@Deprecated(since="2.1", forRemoval=true)`） | 组合 4 组件为 harness `DistributedStore`；`create(DataSource)` | `distributed::PostgresDistributedStore`（`src/distributed.rs`） | 持有 `Arc<dyn PostgresClient>` + 工厂方法 `agent_state_store()` / `base_store()` / `snapshot_client()` / `snapshot_spec()` / `sandbox_execution_guard()`；`distributed_store()` 输出 harness `DistributedStore`（仅 state store，见 §4.2）。与 EXTENSIONS_GAP_TRACKER §2.1 建议的「字段式 builder」形态略有偏差，见 §3.3 |
| `postgresql.state.PostgresAgentStateStore` | 会话状态：单值 + 增量列表 + `:_hash` 伴随行 + 版本 CAS | `state::PostgresAgentStateStore`（`src/state.rs`，`impl agentscope_core::state::AgentStateStore`） | trait 覆盖 `save/save_list/get/get_list/exists/delete/list_session_ids`；版本能力以固有方法 `save_versioned/save_if_version/version_of` 保留 |
| `postgresql.store.PostgresBaseStore` | 命名空间 KV + CAS + `LIKE ... ESCAPE '!'` 前缀搜索 + US(0x1F) 命名空间编码 | `store::PostgresStore`（`src/store.rs`，`impl agentscope_harness::filesystem::BaseStore`）+ `PostgresStoreBuilder` | `put_if_version` 对齐 harness CAS：`expected==0` → INSERT 且重复键判失败；`>0` → 条件 `UPDATE` 影响行数=1 |
| `postgresql.snapshot.PostgresRemoteSnapshotClient` | BYTEA 快照上传/下载/存在性 | `snapshot::PostgresRemoteSnapshotClient`（`src/snapshot.rs`，`impl RemoteSnapshotClient`） | 默认表 `agentscope_snapshots`；`upload` 用 `ON CONFLICT (snapshot_id) DO UPDATE` |
| `postgresql.snapshot.PostgresSnapshotSpec`（`extends RemoteSnapshotSpec`） | PG 快照 spec 便捷类 | `snapshot::PostgresSnapshotSpec`（newtype 包装 harness `RemoteSnapshotSpec`，`impl SandboxSnapshotSpec`）+ 辅助函数 `postgres_snapshot_spec()` / `postgres_snapshot_spec_with_table()` | 快照 id 校验沿用 harness `RemoteSnapshotSpec::build` |
| `postgresql.sandbox.PostgresSandboxExecutionGuard` | PG advisory lock 分布式护栏（MurmurHash3 64-bit key + `pg_try_advisory_lock` 轮询 + `pg_advisory_unlock` 释放） | `guard::PostgresSandboxExecutionGuard` + `PostgresSandboxExecutionGuardBuilder` + `PostgresSandboxLease`（`src/guard.rs`） | harness 的 `SandboxExecutionGuard` 是**具体 struct 非 trait**，故为镜像 API 的独立具体类型（见 §4.2） |
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
| `org.apache.commons.codec.digest.MurmurHash3` | 锁 key 64-bit 哈希 | `std::collections::hash_map::DefaultHasher`（`guard::compose_lock_key`） | **算法不同**：跨语言锁 key 不互通，见 §3.3 |
| `org.slf4j.Logger` | 日志 | `tracing` | 仅 `driver.rs` 有 `tracing::warn`；store/state/snapshot/guard 无埋点，见 §3.3 |
| `java.util.regex.Pattern`（`^[a-zA-Z_][a-zA-Z0-9_]*$`） | 标识符校验 | `sql::validate_identifier`（手写字符判定；`MAX_IDENTIFIER_LENGTH = 63`） | 语义对齐 |
| `RuntimeException` / `SQLException` | 错误传播 | `error::PostgresError`（`Query`/`Schema`/`Decode`/`Encode`/`InvalidIdentifier`/`CasFailed`/`LockTimeout`/`DriverUnavailable`）+ `From<PostgresError> for CoreError`；别名 `PgResult<T>` | — |
| `InputStream` / `byte[]`（快照） | 快照数据流 | `&[u8]` / `Vec<u8>`（`RemoteSnapshotClient` 签名） | — |
| JUnit5 + Mockito + H2（Java 测试） | 离线测试 | Rust 内置 `#[cfg(test)]`（当前仅 `sql.rs` 5 个用例） | **覆盖缺口见 §3.1、§4.2** |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib 导出 | `PostgresDistributedStore` 等 | 已完成 | `src/lib.rs` 声明 11 个模块（含 feature 门控 `driver`），re-export 全部公共类型 |
| `PostgresAgentStateStore`（impl core `AgentStateStore`） | `state/PostgresAgentStateStore.java` | 已完成 | 单值 `item_index=0`；列表增量 + `:_hash` 检测（复用 `agentscope_core::state::list_hash`）；`save_versioned`/`save_if_version`/`version_of` 保留版本语义 |
| `PostgresStore`（impl harness `BaseStore`）+ builder | `store/PostgresBaseStore.java` | 已完成 | CAS/`StoreItem.version` 以 harness 为准；US(0x1F) 命名空间编码 + `LIKE ... ESCAPE '!'` |
| `PostgresRemoteSnapshotClient`（impl `RemoteSnapshotClient`） | `snapshot/PostgresRemoteSnapshotClient.java` | 已完成 | `upload`/`download`/`exists`；`ON CONFLICT (snapshot_id) DO UPDATE` |
| `PostgresSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/PostgresSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec`；快照 id 校验以 harness 为准 |
| `PostgresSandboxExecutionGuard` + lease + builder | `sandbox/PostgresSandboxExecutionGuard.java` | 已完成（语义差异见 §3.3） | `try_enter(&SandboxIsolationKey)` 轮询 `pg_try_advisory_lock`；lease `Drop` 释放 |
| `PostgresDistributedStore` 组合者 | `PostgresDistributedStore.java` | 已完成 | 5 个组件工厂方法 + `distributed_store()`（输出 harness 子集） |
| 无真实 DB 可用的内存后端 | Java H2 / mock | 已完成 | `memory::MemoryClient`（内存 SQL 模拟器） |
| **离线单元测试** | Java 5 个测试类约 130+ `@Test` | **部分完成（P0 缺口收窄）** | 现状：`cargo test -p agentscope-extensions-postgresql` = **7 passed / 0 failed**，**全部集中在 `src/sql.rs`**（新增 `ensure_version_column_ddl_targets_full_table`）。`state.rs` / `store.rs` / `snapshot.rs` / `guard.rs` / `distributed.rs` / `memory.rs` **零测试**；crate 内**无 `tests/` 目录**。对照同波次 MySQL（43 test）、MongoDB（62+1 test），本模块覆盖仍明显偏少（Java 侧测试量本为同波次最大之一）。补测方向（全部可离线，用 `MemoryClient` + trait 注入）：state 单值/列表 CAS 与 `:_hash` 重写、store `put_if_version` 三分支、snapshot 上传/下载往返、guard 轮询/超时/释放、组合者工厂返回类型、SQL 模板与标识符校验 |

### 3.2 其它待办（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `supportsVersioning()` / `getVersioned()` | `state/PostgresAgentStateStore.java:258,300` | 设计决定（不迁移） | core `AgentStateStore` trait 无版本方法，故 `getVersioned` 降为固有 `version_of()`（返回 `PgResult<i64>`），`save_if_version`/`save_versioned` 为固有方法；语义保留 |
| `ensureVersionColumn()`（老表补 `version` 列迁移） | 同上 `:137` | **待办** | Rust `state.rs` 无等价实现；老库升级路径缺失（见 §4.2） |
| `verifySchemaExists()` / `verifyTableExists()`（`INFORMATION_SCHEMA` 校验） | 同上 `:183,203` | **待办** | Rust 仅 `CREATE ... IF NOT EXISTS`，未做存在性校验/错误上报 |
| `getDataSource()` / `close()` | 同上 `:701,737` | 设计决定（不迁移） | Rust 无连接所有权（client 为 `Arc<dyn PostgresClient>`），无关闭语义 |
| `truncateAllSessions()` | 同上 `:718` | 已完成 | `state::truncate_all_sessions()` |
| store 的 `objectMapper(ObjectMapper)` builder 注入 | `store/PostgresBaseStore.java:394` | 设计决定（不迁移） | Rust 固定使用 `serde_json`，无需注入 |
| store 的表名/schema 校验 | `store/PostgresBaseStore.java:439` | 已完成 | `sql::validate_identifier`（与 state 共用） |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| 锁 key 哈希算法差异 | Java 用 MurmurHash3 64-bit；Rust 用 `DefaultHasher`。同前缀下两语言生成的 advisory lock key **不互通**（跨语言混布存在并发互斥失效风险） | **待确认** |
| guard 文档与实现不一致 | `src/guard.rs:7` 文档声称 `try_enter / release / is_held`，实际仅有 `try_enter` / `release`（+ lease） | **待办** |
| advisory lock 与连接池语义 | Java 每租约独占 `Connection`，锁随连接关闭自动释放；Rust `PgSimpleClient` 为共享 client + `spawn_blocking`，而 PG advisory lock 是**会话级**，共享连接下锁归属/释放时机可能与 Java 不同（`MemoryClient` 无法暴露该风险） | **待确认** |
| `tracing` 埋点缺失 | Java SLF4J 日志点分布在 guard/store/snapshot/state；Rust 仅 `driver.rs` 有日志 | 待办 |
| `memory::MemoryClient` 未门控 | 测试替身现为 `pub mod memory` + re-export，进入生产 API；MySQL 侧放在 `testing.rs` 子模块 | 待办 |
| 组合者形态偏差 | EXTENSIONS_GAP_TRACKER §2.1 建议「字段式追加 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` + 自建 builder」；本 crate 改为「持有 `client` + 工厂方法」，且未暴露 `isolation_scope`。功能等价但形态不一致 | **待确认** |
| 真实 PostgreSQL 集成测试 | 需真实实例（Java 侧用 H2 近 PG 方言离线跑）；当前仅内存模拟器 | 待办（环境受限） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态**：`crates/agentscope-extensions-postgresql/` 的 P0 组件已全部落地并接线，编译与现有测试通过。
- **验证命令与结果（汇总方实测，本次未重跑）**：
  - `cargo build -p agentscope-extensions-postgresql` → **通过**
  - `cargo test -p agentscope-extensions-postgresql` → **7 passed / 0 failed**（全部在 `src/sql.rs`；无 `tests/` 集成测试目标，另有 0 用例的空 test target 与 0 doc-tests）
  - 全量 `cargo test --workspace` → **1746 passed / 0 failed**（86 test target，exit 0）
- **crate 文件清单**（`src/` 平铺，无子目录）：`lib.rs`(2305B)、`client.rs`(3947B)、`blocking.rs`(1202B)、`sql.rs`(5719B)、`error.rs`(1861B)、`memory.rs`(19223B)、`store.rs`(9894B)、`state.rs`(17749B)、`snapshot.rs`(6306B)、`guard.rs`(7329B)、`distributed.rs`(3408B)、`driver.rs`(5508B，feature `postgres-driver` gated)；`Cargo.toml`(1117B)。
- **架构要点**：全部 SQL 经同步 `PostgresClient` seam；async trait（`AgentStateStore`、`RemoteSnapshotClient`）由 `blocking::query_async`（`spawn_blocking`）桥接；同步 `BaseStore` 直接调用 client。故**无真实数据库也能编译与测试**。
- **复用契约（对齐 EXTENSIONS_GAP_TRACKER §2.1，未新建公共 store crate）**：`impl agentscope_core::state::AgentStateStore`（`state.rs:388`）、`impl agentscope_harness::filesystem::BaseStore`（`store.rs:130`）、`impl agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot.rs:63`）、`impl SandboxSnapshotSpec`（`snapshot.rs:158`）；harness `DistributedStore` 仅取 `agent_state_store` + `isolation_scope`（见 §4.2）。
- **已知问题**：测试覆盖 P0 缺口（§3.1）；锁 key 哈希与 advisory lock 连接语义待确认（§3.3）。

### 4.2 下一步与阻塞
1. **P0（本模块内，最优先）**：补齐离线单元测试（state / store / snapshot / guard / distributed / memory），覆盖 §3.1 所列语义分支；统一使用 `MemoryClient`，不引入真实数据库。
2. **跨模块需求（不改 harness）**：harness `SandboxExecutionGuard` 是**具体 struct 而非 trait**（`crates/agentscope-harness/src/agent/sandbox/guard.rs:13`），本 crate 无法 `impl`，只能提供镜像类型的 `PostgresSandboxExecutionGuard`（`try_enter`/`release`），**无法直接接入 harness `SandboxManager`**（后者接受具体 `SandboxExecutionGuard`）。若后续 harness 将 guard 抽为 trait，本 crate 可补 `impl`。记入此处由汇总方评估。
3. **harness `DistributedStore` 字段不足**：已按 EXTENSIONS_GAP_TRACKER §2.1 裁定「不改 harness」，本 crate `distributed_store()` 仅携带 `agent_state_store`；`base_store`/`snapshot_spec`/`guard` 以独立工厂方法暴露。是否需扩展 harness 组合包仍待汇总方。
4. **待确认**：锁 key 哈希（`DefaultHasher` vs Java MurmurHash3）跨语言一致性；advisory lock 在共享连接/连接池下的归属与释放语义；真实 PostgreSQL 集成测试环境。
5. **P1**：老表 `version` 列迁移（`ensureVersionColumn` 等价）与 `INFORMATION_SCHEMA` 存在性校验（`verifySchemaExists`/`verifyTableExists`）尚未迁移。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-postgresql` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-postgresql` / `cargo test -p agentscope-extensions-postgresql` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-postgresql/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

