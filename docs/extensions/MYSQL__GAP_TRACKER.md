# MYSQL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MYSQL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-mysql\`
> **Java 子模块**：(鍗曟ā鍧?
> **Rust 目标**：`crates/agentscope-extensions-mysql/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-mysql` |
| Java 子模块 | (鍗曟ā鍧? |
| Java 规模（初测） | 主代码 ~2070 行 / 测试 ~204 行 |
| Rust crate | `crates/agentscope-extensions-mysql` |
| 迁移波次 | B |
| 当前状态 | **已完成（P0 全部落地；已 build + test 通过）** |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `MysqlDistributedStore` | 组合 4 个组件成 `DistributedStore` | `MysqlDistributedStore` / `MysqlDistributedStoreBuilder`（`distributed_store.rs`） | 持有 4 组件 + `isolation_scope`；提供 `to_harness_distributed_store()` 输出 harness 子集 |
| `state.MysqlAgentStateStore` | MySQL 会话状态（单值 + 增量列表 + 版本） | `state::MysqlAgentStateStore` | `impl AgentStateStore`；`save_if_version`/`read_version` 作为固有方法保留版本语义 |
| `store.JdbcStore` | JDBC 命名空间 KV + CAS | `store::MysqlStore`（`impl BaseStore`） | `put_if_version` 严格对齐 harness CAS 语义（v1 起、`expected==0` 为 create-if-absent） |
| `store.JdbcStoreDialect` | 方言抽象（4 条 SQL 模板） | `store::StoreDialect` | 含 `create/upsert/insert/cas_update/search/select/delete` |
| `store.MysqlJdbcStoreDialect` | MySQL 方言 | `store::MysqlStoreDialect` | 其余方言一并迁移：`Postgres/Sqlite/H2 StoreDialect` |
| `snapshot.JdbcSnapshotSpec` | 快照 spec（BLOB 表） | `snapshot::MysqlSnapshotSpec`（`impl SandboxSnapshotSpec`） | **复用 harness `RemoteSnapshotSpec`**，快照 id 校验以 harness 为准 |
| `snapshot.JdbcRemoteSnapshotClient` | BLOB 上传/下载/存在性 | `snapshot::MysqlRemoteSnapshotClient`（`impl RemoteSnapshotClient`） | 表名默认 `agentscope_snapshots` |
| `sandbox.JdbcSandboxExecutionGuard` | MySQL `GET_LOCK()` 分布式锁 | `guard::MysqlSandboxExecutionGuard` / `MysqlLockLease` | harness 的 guard 是**具体 struct 非 trait**，故此处为镜像 API 的具体类型（见 §4.2）|
| `_hash` 版本键（`HASH_KEY_SUFFIX`） | state 版本伴随行 | `state::HASH_KEY_SUFFIX` + `StateBackend::{read_version,write_version}` | 保留跨语言一致性 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `DataSource` / JDBC | 连接、SQL | **`SqlBackend` / `StateBackend` / `SnapshotBackend` / `LockBackend` 后端 trait** | 各组件依赖窄接口，不绑定驱动 |
| `com.fasterxml.jackson` | JSON | `serde_json` | 已用 |
| `PreparedStatement`/`ResultSet` | SQL 绑定/读取 | 后端 trait 方法参数/返回值 | 不在 crate 内绑定具体驱动 |
| `LONGTEXT`/`LONGBLOB` | 大字段 | 后端实现负责（`String`/`&[u8]`） | — |
| SLF4J | 日志 | `tracing`（依赖已接入，暂未写日志） | — |

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
| 单元测试（无真实 MySQL） | Java 测试 204 行 | 已完成 | 内存后端 43 个测试全过 |

### 3.2 待办 / 后续增强（P1/P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| 真实驱动适配器 | 用 `mysql_async`/`sqlx` 等实现 4 个后端 trait（可放入本 crate 的 feature 或上层应用） | 待办 |
| 集合/集成测试 | 需真实 MySQL 实例（当前仅内存后端验证语义） | 待办（环境受限） |
| `tracing` 日志 | 对齐 SLF4J 日志点 | 待办（P2） |
| `JdbcStore.validateIdentifier` 等价 | 表/库名合法性校验（已在 state 模块有；store 的表名允许自定义） | 部分 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：`crates/agentscope-extensions-mysql/` 全部 P0 组件实现，均**复用既有 trait**（`AgentStateStore` / `BaseStore` / `SandboxSnapshotSpec` / `RemoteSnapshotClient`），CAS/版本语义以 harness 为准。
- **文件清单**：
  - `Cargo.toml`（仅用 workspace 依赖：agentscope-core/harness、serde、serde_json、thiserror、async-trait、tokio、tracing、chrono）
  - `src/lib.rs`、`src/error.rs`
  - `src/store/{mod.rs,dialect.rs,testing.rs}`
  - `src/state/{mod.rs,testing.rs}`
  - `src/snapshot/{mod.rs,testing.rs}`
  - `src/guard/{mod.rs,testing.rs}`
  - `src/distributed_store.rs`
- **验证命令与结果**：
  - `cargo build -p agentscope-extensions-mysql` → **Finished**（本 crate 零 warning；harness 既有 warning 与本次无关）
  - `cargo test -p agentscope-extensions-mysql` → **43 passed; 0 failed**（另 0 doc-tests）
- **已知问题**：无阻塞。真实 MySQL 集成测试待环境。

### 4.2 下一步与阻塞
1. **跨模块待汇总项（不改动其它 crate）**：harness `DistributedStore` 仅含 `agent_state_store` + `isolation_scope`，而 Java 版含 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`（另 `message_bus`/`async_tool_registry`/`task_repository`）。本 crate 已在 `MysqlDistributedStore` 内保留全量 4 组件并通过 `to_harness_distributed_store()` 输出 harness 子集；是否扩展 harness `DistributedStore`（EXTENSIONS_GAP_TRACKER §2.2）仍待汇总方裁定。
2. **harness `SandboxExecutionGuard` 为具体 struct 而非 trait**：故本 crate 无法 `impl` 之，改为镜像 Java API 的具体类型 `MysqlSandboxExecutionGuard`（`try_enter`/`release`/`is_held`）。若后续 harness 将 guard 抽为 trait，本 crate 可再补 `impl`。已记入本 §4.2。
3. **真实驱动适配器**（`mysql_async`/`sqlx`）可由上层按需在本 crate 加 feature 实现后端 trait；当前不强加 workspace 级重依赖。
4. 监点（`tracing`）与真实 MySQL 集成测试可作为后续 P1/P2。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mysql` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mysql` / `cargo test -p agentscope-extensions-mysql` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mysql/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

