# MYSQL 迁移执行历史（归档）

> 本文件是 [`../MYSQL__GAP_TRACKER.md`](../MYSQL__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. §4.3 P0 修复记录（2026-10-22，汇总方） · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决） · 8. 2026-10-30 执行记录 — P1-6 / P1-7 / P1-8（MYSQL） · 9. 2026-10-31 执行记录 — P1 真实驱动/集成测试收口（MYSQL） · 2026-10-31 loop 收口记录 · 2026-10-31（二轮）JDBC 桥接收口（MYSQL））。

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

> **历史快照（2026-10-22 静态复核）**：P1-1 / P1-2 / P1-3 / P1-5 已于 **2026-10-26** 关闭（见 §7.3），P1-4 已于 §7.1 关闭，P1-6/P1-7/P1-8 已于 **2026-10-30** 关闭（§8）；下表保留原样以存档。**P2-1…P2-9 九项已于 2026-10-31 全部处置完毕**（其中 P2-3 的 `clearAllSessions` 为「已完成（设计决定，不迁移）」，P2-8 已裁定改 1800s），逐条见 §9.3。

**P0**：无。（§2.1 五项契约齐全；`lib.rs` 导出完整；无编译级断链的静态迹象。）

**P1（正确性/安全，建议本轮或下一轮修）**

| # | 缺口 | Java 路径 | Rust 现状 |
|---|---|---|---|
| P1-1 | 列表**变更检测缺失**：中间元素被改写时 `save_list` 只按 `list_len()` 追加尾部，陈旧数据留存；Java 用 `ListHashUtil.needsFullRewrite` 全量重写 | `state/MysqlAgentStateStore.java:566-601`（+ `core.state.ListHashUtil`） | 缺失（crate 内 `list_hash` 引用数 0；core 已有 `agentscope_core::state::list_hash` 可直接复用，JDBC/PG 已接入） |
| P1-2 | 版本 **CAS 非原子**：`read_version`→比对→`save`→`write_version` 先读后写，并发双写均可成功 | `state/MysqlAgentStateStore.java:464-546`（`INSERT`/`UPDATE ... WHERE version=?`） | 部分（语义有、原子性无；版本存 `key:_hash` 伴随行而非 `version` 列）；2026-10-29 勘误：已改为行内 `version` 列原子 CAS |
| P1-3 | `saveIfVersion(0)` **重复键回退**缺失（#3162 迁移回填 version=0 行） | 同上 `:481-493`、`:501-524` | 缺失（直接返回 `InvalidArgument` 冲突）。**已完成（2026-10-29 勘误）**：原记「回退缺失」，实际 MySQL Java `insertIfAbsent` **无回退**（重复键即 `Conflict`），Rust 已按此实现并于 2026-10-26 落地（§3.2 同行 / §7.3） |
| P1-4 | 空列表 **no-op** 语义相反：Java `values.isEmpty()` 直接 return；Rust 会以 `from_index` + 空切片调用 `save_list`，后端按「total=0」修剪 → **清空**既有列表 | `state/MysqlAgentStateStore.java:571-573` | 部分（行为相反，`InMemoryStateBackend` 实测会全删）；2026-10-29 勘误：`save_list([])` 已为 no-op |
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

> **已完成（2026-10-29 勘误）**：原记「**P1 缺口 8 项**」「**45 test**」，实际 P1 现存 **4 项**（P1-6 schema 校验/`version` 列、P1-7 快照建表、P1-8 锁名截断、真实驱动与集成测试）；列表变更检测/CAS 原子性/create-if-absent/空列表 no-op/表名校验均已关闭（2026-10-22 / 2026-10-26，§3.2 / §7.1 / §7.3）；实测 **71 passed / 0 failed**（2026-10-28 主会话复跑）。



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
  - **更新（2026-10-30）**：64 字符截断已于 §8.3 落地；默认超时 1800s vs 30s 仍未变更（见 §8.6）。

## 8. 2026-10-30 执行记录 — P1-6 / P1-7 / P1-8（MYSQL）

> 范围：只改 `crates/agentscope-extensions-mysql/**` + 本 tracker；未 `git add`/`git commit`，未碰根 `Cargo.toml`、core、harness、其它 extension crate。**未新增任何依赖**。测试全部离线（`InMemory*` 测试双）。
> 验证：`cargo test -p agentscope-extensions-mysql -j 2` → **98 passed / 0 failed / 0 ignored**（基线 71，+27 用例），EXITCODE=0，无 `^failures:` 段。

### 8.1 P1-6 schema 存在性校验 + `version` 列迁移

- **Java 依据**：`state/MysqlAgentStateStore.java:174-184`（构造期分支）、`:186-213`（`ensureVersionColumn`，`INFORMATION_SCHEMA.COLUMNS` 计数 → `ALTER TABLE ... ADD COLUMN version BIGINT NOT NULL DEFAULT 1`）、`:215-252`（`createDatabaseIfNotExist` / `createTableIfNotExist`，`version BIGINT NOT NULL DEFAULT 0` + `session_id`/`state_key` 的 `COLLATE utf8mb4_bin`）、`:272-296`（`verifyDatabaseExists`）、`:298-320`（`verifyTableExists`）。
- **改前失败点**：`MysqlAgentStateStore::new()` 用 `let _ = backend.ensure_schema()` **吞掉 DDL 错误**；`create_if_not_exist == false` 时**完全不校验**库/表是否存在；`version` 列是否存在于表上无人过问 → 指向旧库时所有 CAS 读到 0（`get_versioned` 语义错、`saveIfVersion(0)` 走 INSERT 撞重复键）。
- **改法（缝的形态换了，理由见下）**：`StateBackend` 的 `ensure_schema()` 删除，改为
  `execute_ddl(&self, sql)` + `database_exists(db)` + `table_exists(db, table)` + `table_columns(db, table)`；
  DDL 文本由本 crate 生成（与 `store/*Dialect` 的既有做法一致）：`create_database_sql` / `create_sessions_table_sql` / `add_version_column_sql`（均 `pub` 并 re-export）。
  构造期 `bootstrap_schema()`：`true` → CREATE DATABASE IF NOT EXISTS + CREATE TABLE IF NOT EXISTS；`false` → 先库后表，缺失返回 `MysqlError::Schema` 并带 Java 原文（`Database does not exist: {db}. Use ... to auto-create.` / `Table does not exist: {db}.{table}. ...`）；**两分支末尾都**调 `ensure_version_column`（列探针命中即跳过，幂等）。
- **与 §7.3「`ensure_schema` 必须建出 `version` 列」的关系**：该契约已由本节的 `create_sessions_table_sql` 承担（`version BIGINT NOT NULL DEFAULT 0`，**照抄 Java**；写路径一律显式写 `1`，迁移路径 `DEFAULT 1` 给存量行回填）。不再有 `ensure_schema`，§4.2 第 5 条描述的是当时形态，现状以本节为准。
- **破坏性 API 变更（本 crate 内）**：`MysqlAgentStateStore::new(backend)` 由 `-> Self` 改为 `-> MysqlResult<Self>`（不再吞错）；`StateBackend` 4 个新必实现方法。crate 内调用点已全部改（`distributed_store::try_build`、单测 3 处）。
- **测试**：`auto_create_provisions_database_and_table`、`create_table_ddl_pins_binary_collation_on_key_columns`、`missing_database_is_reported_when_not_auto_creating`、`missing_table_is_reported_when_not_auto_creating`、`legacy_table_gets_the_version_column_migration`、`version_column_migration_is_idempotent`、`existing_version_column_is_left_alone`、`ddl_failure_propagates_when_auto_creating`、`custom_names_flow_into_the_provisioning_ddl`；测试双侧：`create_statements_update_the_modelled_schema`、`create_table_requires_the_database_first`、`alter_adds_the_version_column_once`、`alter_on_an_unknown_table_fails`。

### 8.2 P1-7 快照表自动建表

- **Java 依据**：`snapshot/JdbcSnapshotSpec.java:27-35`（两个构造器**恒传** `initializeSchema=true`）、`snapshot/JdbcRemoteSnapshotClient.java:56-81`（`initSchema`：`CREATE TABLE IF NOT EXISTS`，`snapshot_id VARCHAR(512) COLLATE utf8mb4_bin NOT NULL PRIMARY KEY`、`data LONGBLOB`、`ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 ROW_FORMAT=DYNAMIC`；**`catch (SQLException) → log.warn`，不抛**）。
- **改法**：新增 `snapshot_table_ddl(table_name)`（逐字对齐 Java DDL）+ `SnapshotBackend::execute_ddl`；`MysqlRemoteSnapshotClient` 增 `initialize_schema` 字段与 `new_with_schema` / `with_table_and_schema`（`new`/`with_table` 保持 `initializeSchema=false`，对齐 Java 的显式开关）；`MysqlSnapshotSpec::new`/`with_table` 恒走 `*_and_schema`（= Java spec 的 `true`）；DDL 失败**仅 `tracing::warn!`**，构造不失败。
- **失败口径**：**构造期不上抛**（Java 只 `log.warn`）——与 P1-6 的 state 侧（Java 抛 `RuntimeException`/`IllegalStateException`，Rust 用 `MysqlError::Schema`）刻意相反，勿「统一」。
- **测试**：`snapshot_ddl_matches_the_java_statement`、`spec_issues_the_table_ddl_on_construction`、`spec_with_table_issues_ddl_for_the_custom_table`、`ddl_failure_is_logged_not_raised`、`client_without_initialize_schema_emits_no_ddl`；测试双侧 `ddl_is_recorded_and_can_be_made_to_fail`。

### 8.3 P1-8 `GET_LOCK` 锁名 64 字符截断 + `key_prefix` 归一化

- **Java 依据**：`sandbox/JdbcSandboxExecutionGuard.java:99-107`（`composeLockName`：`if (raw.length() <= 64) return raw; int hash = raw.hashCode(); return raw.substring(0, 50) + ":" + Integer.toHexString(hash);`）、`:155-161`（`Builder.keyPrefix`：`isBlank()` → `IllegalArgumentException("keyPrefix must not be blank")`，缺尾 `:` 自动补）。
- **改法**：`java_string_hash_code`（UTF-16 code unit 上 `h*31+c` 的 `i32` wrapping）与 `truncate_lock_name`（Java `toHexString` = `format!("{:x}", hash as u32)`，负数 8 位无符号）；`compose_lock_name` 组装后统一截断（`sub_key` 为 Rust 侧扩展，一并参与截断）。`key_prefix` 归一化：`try_key_prefix`（空白 → `MysqlError::InvalidArgument("keyPrefix must not be blank")`），`key_prefix` 委托并 `expect`（Java 抛异常，对应 panic；构造器 builder 非 `Result`，保持既有风格）。
- **金标向量（独立来源）**：直接跑 **真实 Java 24**（`java LockNameGold2.java`，独立于 Rust 实现）得到，硬编码进测试：
  - 65 字符（`...user:` + 36×`y`）→ `hashCode = -189878914` → `f4aead7e`
  - 68 字符（39×`y`）→ `-210677989` → `f3714f1b`；89 字符（60×`x`）→ `798084414` → `2f91cd3e`
  - `"polygenelubricants"` → `Integer.MIN_VALUE` → `80000000`；`"abc"` → `96354`；`""` → `0`
  - 64 字符边界原样返回（`agentscope:sandbox:lock:` 24 + `user:` 5 + 35×`y`）
- **测试**：`java_string_hash_code_matches_java_gold_values`、`compose_lock_name_keeps_names_at_the_limit`、`compose_lock_name_truncates_over_the_limit`、`compose_lock_name_uses_unsigned_hex_for_negative_hash`、`truncated_names_never_exceed_the_limit`、`key_prefix_is_normalised_with_a_trailing_colon`、`blank_key_prefix_is_rejected`。

### 8.4 反向验证（逐项回退实现 → 确认 FAILED → 恢复）

| # | 回退动作 | 观测 |
|---|---|---|
| 1 | `truncate_lock_name` 开头加 `if true { return raw.to_string() }` | `guard::tests` → **FAILED 12 passed / 3 failed**：`compose_lock_name_truncates_over_the_limit`、`compose_lock_name_uses_unsigned_hex_for_negative_hash`、`truncated_names_never_exceed_the_limit` |
| 2 | `MysqlRemoteSnapshotClient::init_schema` 开头提前 `return` | `snapshot::tests` → **FAILED 7 passed / 3 failed**：`spec_issues_the_table_ddl_on_construction`、`spec_with_table_issues_ddl_for_the_custom_table`、`client_without_initialize_schema_emits_no_ddl` |
| 3 | `bootstrap_schema` 的 `database_exists` 校验短路（`if false && ...`） | `state::tests` → **FAILED 27 passed / 1 failed**：`missing_database_is_reported_when_not_auto_creating` |
| 4 | `ensure_version_column` 开头提前 `return Ok(())` | `state::tests` → **FAILED 26 passed / 2 failed**：`legacy_table_gets_the_version_column_migration`、`version_column_migration_is_idempotent` |

恢复后复跑全量：`cargo test -p agentscope-extensions-mysql -j 2` → **98 passed / 0 failed**（`^failures:` 段不存在）。

### 8.5 改动文件

`crates/agentscope-extensions-mysql/src/{lib.rs,guard/mod.rs,snapshot/mod.rs,snapshot/testing.rs,state/mod.rs,state/testing.rs}`（`Cargo.toml` **未改**，无新增依赖）；本 tracker。

### 8.6 未完成项与原因

1. **真实驱动适配器**（P1，环境/依赖受限）：`StateBackend` 新增的 4 个方法、`SnapshotBackend::execute_ddl` 均**未**有 `mysql_async`/`sqlx` 实现；本 crate 不引入重依赖（§2.1 决策）。接入时 state 侧需把 `execute_ddl` 映射到 `Statement::execute`、三个探针映射到 `INFORMATION_SCHEMA` 查询；snapshot 侧 `execute_ddl` 直接执行传入的 `CREATE TABLE`。
2. **真实 MySQL 集成测试**（P1，环境受限）：同上，仍需实例。
3. **默认锁超时 30s vs Java 1800s**（P2，**未改，有意**）：本 crate 的 `MysqlSandboxExecutionGuard` 目前由 harness `SandboxGuard::try_enter` 调用（无超时入参），把默认值抬到 1800s 会让锁竞争路径静默阻塞 30 分钟；应连同 `MysqlDistributedStoreBuilder` 的超时配置面与 harness 侧接线一起改，属跨模块议题（记 §4.2）。
4. **`tracing` 日志（P2）**：本轮已在快照建表失败点启用 `tracing::warn!`（Java `log.warn` 对齐），其余 Java 日志点（store 构建、guard 获取/释放的 `log.debug`）仍未补。
5. 其余 P2（状态 key 长度校验、`truncateAllSessions`、锁超时）未动，见 §3.2。

## 9. 2026-10-31 执行记录 — P1 真实驱动/集成测试收口（MYSQL）

> 范围：只改 `crates/agentscope-extensions-mysql/**`（含本 crate `Cargo.toml`）+ 本 tracker；未动根 `Cargo.toml`、core、harness、其它 extension crate。按本会话约定**未运行 cargo / rustc / clippy / fmt**，编译与测试由主智能体统一验证。

### 9.1 P1 真实驱动适配器（`feature = "mysql_async"`）

- **落点**：`src/driver/mod.rs`（特性开关 + `MysqlAsyncConfig` + 文档）、`src/driver/real.rs`（真实实现）、`src/driver/stub.rs`（未开 feature 的显式失败存根）。
- **依赖**：`Cargo.toml` 新增 `[features] default = [] mysql_async = ["dep:mysql_async"]` + `mysql_async = { workspace = true, optional = true }`（版本在根 `[workspace.dependencies]`，本 crate 不写死）。**默认 feature 不引入任何驱动依赖**。
  - 另新增 `[dev-dependencies] futures = { workspace = true }`，仅供集成测试用 `futures::executor::block_on` 驱动 async trait（原 `[dependencies]` 中未被引用的 `futures` 已移除，P2-2 关闭）。
- **`MysqlAsyncBackend`**（一个类型，**四个 trait 都实现**，共享一个 `mysql_async::Pool`）：
  | trait | 关键实现要点 |
  |---|---|
  | `StateBackend` | DDL 直发；`INFORMATION_SCHEMA.SCHEMATA/TABLES/COLUMNS` 三个探针；`item_index = 0` 单值；`INSERT ... ON DUPLICATE KEY UPDATE` + `LAST INSERT`/`SELECT version` 回读；CAS 走单条 `INSERT`（1062→`Conflict`）/`UPDATE ... WHERE version = ?`（0 行→`Conflict`）；列表多行 upsert + `item_index >= len` 修剪；`session_id` 列内含 `user\u{1F}session`（Java 表无 user 列，Rust core trait 有 user 维，故折叠进主键并在 `list_session_ids` 剥离前缀） |
  | `SqlBackend` | 直接用方言渲染好的 `?` 占位 SQL + 固定绑定顺序；`cas_update` 取 `affected_rows()`；重复键错误消息保留原文以让 `MysqlError::is_duplicate_key` 命中（1062 含 `Duplicate entry`） |
  | `SnapshotBackend` | `SELECT/INSERT ... ON DUPLICATE KEY UPDATE data`；缺快照返回 `Error::Tool("snapshot not found: …")`（对应 `RemoteSnapshotClient::download` 的错误语义） |
  | `LockBackend` | `GET_LOCK` / `RELEASE_LOCK` / `IS_USED_LOCK`；**每个锁独占一个 `Conn`**（MySQL `GET_LOCK` 是会话级：同会话重入会成功、异会话释放无效），释放后关连接（对齐 Java `SandboxLease.close()`）；同 backend 重入为本地 no-op |
- **失败语义（硬要求）**：`connect`/`connect_with_config` 在构造期构建 runtime、解析 `Opts`、`pool.get_conn()` ping，任一失败 → `MysqlError::DriverUnavailable`（**新增变体**，`src/error.rs`），**绝不返回“看似可用”的后端**。未开 feature 时 `stub.rs` 的同名 API 一律返回 `DriverUnavailable`（含提示 `--features mysql_async`）。
- **线程/运行时桥接**：seam 是同步的（服务同步 `BaseStore` / guard），适配器持有单 worker 的专用 Tokio runtime，用 `Runtime::block_on` 驱动（与 `agentscope-extensions-postgresql` 的 `PgSimpleClient` 同一范式）。**已知约束**：在 Tokio worker 线程内直接同步调用会阻塞该线程（PG 适配器同），async trait 侧应在 `spawn_blocking` 里调用。
- **测试（离线）**：`driver::tests::connect_without_the_feature_reports_driver_unavailable`（默认 feature 下必跑，证明“不假装成功”）、`default_config_matches_the_component_defaults`、`feature_flag_matches_the_compile_target`。

### 9.2 P1 真实 MySQL 集成测试（env 门控，非 `#[ignore]`）

- **落点**：`tests/mysql_integration.rs`，文件级 `#![cfg(feature = "mysql_async")]`（默认构建编译为空，不影响离线基线）。
- **门控**：`AGENTSCOPE_TEST_MYSQL_URL` 未设置（或为空）→ 打印 skip 提示后 `return`，**不使用 `#[ignore]`**；可选 `AGENTSCOPE_TEST_MYSQL_DATABASE`（默认 `agentscope_it`，由 state store 的 schema bootstrap 创建）。
- **用例**：`state_store_round_trip_cas_and_lists`（单值/版本 CAS/陈旧版本拒绝/列表中间改写/缩容/按用户枚举/`truncate_all_sessions`）、`kv_store_cas_and_prefix_search`（`put_if_version` 0→create、陈旧拒绝、前缀搜索、`supports_atomic_session_storage`）、`snapshot_upload_download_and_exists`（含重复上传覆盖、缺快照报错）、`advisory_lock_is_session_scoped_across_backends`（第二个 backend 在锁被持有时必须失败）、`connect_to_an_unreachable_server_reports_driver_unavailable`（**不需要服务器**，端口 1 必拒）。
- **运行方式**：
  ```powershell
  $env:AGENTSCOPE_TEST_MYSQL_URL = 'mysql://root:secret@127.0.0.1:3306/mysql'
  # 可选：$env:AGENTSCOPE_TEST_MYSQL_DATABASE = 'agentscope_it'
  cargo test -p agentscope-extensions-mysql --features mysql_async --test mysql_integration
  ```
  URL 的默认 schema 必须是该用户可连接的库（测试自身会 `CREATE DATABASE IF NOT EXISTS agentscope_it`，需要 `CREATE` 权限）。

### 9.3 P2 逐条处置

| P2 | 处置 | 落点 / 测试 |
|---|---|---|
| P2-1 tracing 日志 | **已完成**：guard 获取/释放 `debug!`、超时 `warn!`；`try_build` 降级 `warn!`（带底层错误）；快照建表失败 `warn!`（§8.2） | `guard/mod.rs`、`distributed_store.rs` |
| P2-2 未用依赖 `futures` | **已完成**：从 `[dependencies]` 移除（改为 `[dev-dependencies]`，集成测试需要用） | `Cargo.toml` |
| P2-3 `truncateAllSessions`/`clearAllSessions` | **已完成 / 部分设计决定**：`truncate_all_sessions()` 落地（`TRUNCATE TABLE`，返回 0）；Java 已 `@Deprecated` 的 `clearAllSessions()`（`DELETE` + 行数）**不迁移**——Java 自身指向 `truncateAllSessions`，且 `TRUNCATE` 语义更贴近“测试/运维清理” | `state/mod.rs`；`truncate_all_sessions_clears_rows`（测试双 `InMemoryStateBackend` 新增 `TRUNCATE` 分支：真的删行，不只记录 DDL） |
| P2-4 方言 DDL 长度 | **已完成**：PostgreSQL / H2 `namespace_path` 由 512 → **2048**（与 Java `Postgres/H2JdbcStoreDialect` 一致）；MySQL 保持 512（InnoDB 3072 字节主键上限），SQLite `TEXT` | `store/dialect.rs`；`namespace_path_length_matches_each_java_dialect` |
| P2-5 `try_build` 静默降级 | **已完成**：降级前 `tracing::warn!` 记录首个错误，第二分支仍 `?` 抛错（不再静默） | `distributed_store.rs` |
| P2-6 `new` 吞掉 `ensure_schema` 错误 | 已于 2026-10-30（§8.1）关闭（`new -> MysqlResult<Self>`） | `state/mod.rs` |
| P2-7 `key_prefix` 归一化 | 已于 2026-10-30（§8.3）关闭 | `guard/mod.rs` |
| P2-8 **默认锁超时 30s vs Java 1800s** | **已裁定并落地**：改为常量 `DEFAULT_LOCK_TIMEOUT = 1800s`（对齐 Java），并通过既有 `lock_timeout(...)`/`lock_timeout_seconds(...)` 保持可配置。裁定理由：30s 会让长任务在正常负载下被判成“争用失败”，且与 Java 行为不一致；需要快速失败调用方显式收窄即可 | `guard/mod.rs`；`default_lock_timeout_matches_java`（同时断言可配置路径） |
| P2-9 state key / session id 长度 | **已完成**：新增 `validate_state_key`（非空白 + ≤255）与 `MAX_STATE_IDENTIFIER_LENGTH`，`require_session_id` 同步限长；接入 `save_if_version` / `read_version` / `save_list_impl` | `state/mod.rs`；`state_key_validation_mirrors_java` |

### 9.4 语义缺失用例补齐（§6.6 遗留）

| Java 用例 | Rust 对应 | 说明 |
|---|---|---|
| `saveIfVersionUnconditionalAbsentKeyReturnsZero` | `absent_key_reports_version_zero` | 未写过的键 `read_version == 0` |
| `saveIfVersionUnconditionalPropagatesReadVersionException` | `cas_backend_errors_propagate` | CAS 后端错误必须上抛（`StateBackend` 读后写路径已删除，等价契约是“任何 CAS 错误都上抛”）；测试双新增 `fail_cas(true)` |
| `createTablePrimaryKeyFitsInnoDbUtf8mb4Limit` | `create_table_primary_key_fits_inno_db_limit` | `(512 + 255) * 4 = 3068 ≤ 3072` 显式字节断言 |
| `saveIfVersionUnconditionalUsesReadVersion` | `save_if_version_delegates_cas_instead_of_read_then_write`（已有） | 版本读 `/version` 列、不反序列化 payload |
| state/snapshot DDL 二进制排序断言 | `create_table_ddl_pins_binary_collation_on_key_columns`、`snapshot_ddl_matches_the_java_statement`（已有） | 本 crate 已持有 DDL 文本 |

### 9.5 测试密度映射表（Java `@Test` → Rust 用例）

| Java 测试类（11 `@Test`） | 覆盖行为 | Rust 用例（新/既有） |
|---|---|---|
| `MysqlAgentStateStoreTest`（7） | 列表增量/改写、CAS、版本、空列表、schema、DDL 排序、主键字节 | `save_list_detects_mutated_prefix`、`save_list_unchanged_skips_backend_write`、`save_if_version_delegates_cas_instead_of_read_then_write`、`cas_conflict_does_not_write`、`save_if_version_zero_creates_then_conflicts`、`save_upsert_bumps_version_column`、`save_list_empty_is_a_no_op`、`legacy_table_gets_the_version_column_migration`、`create_table_ddl_pins_binary_collation_on_key_columns`、**`create_table_primary_key_fits_inno_db_limit`**、**`absent_key_reports_version_zero`**、**`cas_backend_errors_propagate`**、**`state_key_validation_mirrors_java`**、**`truncate_all_sessions_clears_rows`** |
| `JdbcRemoteSnapshotClientTest`（2） | 建表 DDL/排序、上传下载 | `snapshot_ddl_matches_the_java_statement`、`spec_issues_the_table_ddl_on_construction`、`ddl_failure_is_logged_not_raised`、**`snapshot_upload_download_and_exists`**（live） |
| `MysqlJdbcStoreDialectTest`（2） | DDL/upsert/insert/CAS 逐字对齐 | `mysql_upsert_increments_the_version`、`cas_update_guards_on_version`、`mysql_key_columns_use_binary_collation`、**`namespace_path_length_matches_each_java_dialect`** |
| （无 Java 对应用例） | 驱动适配器 | **`connect_without_the_feature_reports_driver_unavailable`**、**`connect_to_an_unreachable_server_reports_driver_unavailable`**（live）、**`advisory_lock_is_session_scoped_across_backends`**（live） |

### 9.6 改动文件

- `crates/agentscope-extensions-mysql/Cargo.toml`（features + optional `mysql_async` + dev-dep `futures`；移除未用 `futures` 正式依赖）
- `src/lib.rs`（`pub mod driver`、re-export `MysqlAsyncBackend`/`MysqlAsyncConfig`/`driver_available`/`DEFAULT_LOCK_TIMEOUT`/`validate_state_key`/`MAX_STATE_IDENTIFIER_LENGTH`）
- `src/error.rs`（`DriverUnavailable`）
- `src/driver/{mod.rs, real.rs, stub.rs}`（**新增**）
- `src/state/{mod.rs, testing.rs}`（长度校验、`truncate_all_sessions`、测试双 `TRUNCATE` + `fail_cas`、新用例）
- `src/guard/mod.rs`（默认超时 1800s + 日志）、`src/store/dialect.rs`（PG/H2 2048 + 用例）、`src/distributed_store.rs`（降级日志）
- `tests/mysql_integration.rs`（**新增**，env 门控）

### 9.7 反向验证与残余

- **反向验证**（本会话禁跑 cargo，需主智能体执行）：① 把 `DEFAULT_LOCK_TIMEOUT` 改回 30s → `default_lock_timeout_matches_java` 必 FAILED；② `validate_state_key` 直接 `Ok(())` → `state_key_validation_mirrors_java` 必 FAILED；③ `truncate_all_sessions` 改为仅 `Ok(0)` 不发 DDL → `truncate_all_sessions_clears_rows` 必 FAILED；④ `InMemoryStateBackend::fail_cas` 改回不短路 → `cas_backend_errors_propagate` 必 FAILED；⑤ PG/H2 改回 512 → `namespace_path_length_matches_each_java_dialect` 必 FAILED。
- **未关闭项**：
  1. **`mysql_async` 特性构建未经编译验证**（本会话禁止跑 cargo）：若 0.37 的 `Pool`/`Opts`/`Queryable` 签名与实现有出入，由主智能体以 `cargo build -p agentscope-extensions-mysql --features mysql_async` 修正。
  2. ~~**`MysqlAsyncBackend` 未接入 `MysqlDistributedStoreBuilder`**~~ → **已关闭（2026-10-31 二轮）**：新增 `MysqlDistributedStore::connect(url)`（driver 路径）与 `jdbc_bridge::MysqlJdbcStore(Builder)`（jdbc 门面路径），两者均在 feature 未开时返回 `DriverUnavailable`；详见文末二轮记录。
  3. **Java `getDataSource`** 仍为“设计决定，不迁移”（后端 trait 抽象下不适用）。
  4. **列表写入非事务**：`save_list` 用一条多行 upsert + 一条裁剪 `DELETE`（两条语句），与 Java 逐条 `INSERT` 相当；不引入事务是因为 `list_hash` 已保证“重写/追加”决策正确，且 `{key}:_hash` 写入在两条之后。

## 2026-10-31 loop 收口记录

### 关闭条目
- **P1 真实 MySQL 驱动适配器** → 已完成：feature `mysql_async` + `src/driver/{mod,real,stub}.rs`；`MysqlAsyncBackend` 同时实现 `StateBackend`/`SqlBackend`/`SnapshotBackend`/`LockBackend`；构造失败一律 `MysqlError::DriverUnavailable`（含未开 feature 的存根）。测试：`driver::tests::connect_without_the_feature_reports_driver_unavailable`。
- **P1 真实 MySQL 集成测试** → 已完成：`tests/mysql_integration.rs`，`AGENTSCOPE_TEST_MYSQL_URL` 门控（未设置 skip，**无 `#[ignore]`**）。
- **P2-1/P2-2/P2-3/P2-4/P2-5/P2-8/P2-9** → 全部关闭（详见 §9.3）；**P2-8 裁定**：默认锁超时改为 **1800s（对齐 Java）**，保持 `lock_timeout()` 可配置，并用测试钉住。
- **§6.6 语义缺失用例** → `absent_key_reports_version_zero`、`cas_backend_errors_propagate`、`create_table_primary_key_fits_inno_db_limit`、`state_key_validation_mirrors_java`。

### 未关闭项
- `mysql_async` 特性构建**未经编译验证**（本会话禁跑 cargo）；~~`MysqlAsyncBackend` 未再包一层 `MysqlDistributedStoreBuilder` 便利构造器~~ → **已关闭（二轮）**；Java `getDataSource` 保持“设计决定，不迁移”。

## 2026-10-31（二轮）JDBC 桥接收口（MYSQL）

> **硬约束**：本轮**未运行** cargo；改动仅限 `crates/agentscope-extensions-mysql/**`、`crates/agentscope-extensions-jdbc/**` 与本 tracker。

### 关闭条目
1. **便利构造器（两条路径均补齐）**
   - `MysqlDistributedStore::connect(url)`（`src/distributed_store.rs`）：`MysqlAsyncBackend::connect` 一次后分别强转为 4 个 `Arc<dyn …>`，等价于 `builder(backend, backend, backend, backend).try_build()`；`feature = "mysql_async"` 未开时返回 `MysqlError::DriverUnavailable`（不持假存储）。
   - `jdbc_bridge::MysqlJdbcStoreBuilder` / `MysqlJdbcStore`（新模块，feature `jdbc-bridge`）：一行包装 `JdbcDistributedStore::create_with_dialect_and_schema(Arc::new(connect_mysql(url)), MysqlDialect)`，默认 `native_lock_guard = true`（对应 Java `MysqlDialect.tryEnter` 即 `GET_LOCK`）、`isolation_scope = User`、`initialize_schema = true`；未开 feature 时同样 `DriverUnavailable`（`src/jdbc_bridge/stub.rs`）。
2. **依赖方向**：mysql → jdbc（`agentscope-extensions-jdbc = { optional = true }` + `jdbc-bridge = ["dep:...", "agentscope-extensions-jdbc/mysql-executor"]`）；jdbc 仍不依赖本 crate，无环。

### 用例
- 离线（`src/jdbc_bridge/{real,stub}.rs` 内联 4 条）：`builder_defaults_match_the_java_lock_path`、`builder_knobs_are_independent`、`empty_url_reports_driver_unavailable`、`malformed_url_reports_driver_unavailable`；未开 feature 另加 `building_without_the_feature_reports_driver_unavailable`（`jdbc_bridge/mod.rs`）。
- env 门控（`tests/mysql_jdbc_bridge.rs`，`AGENTSCOPE_TEST_MYSQL_URL`）：`harness_bundle_carries_every_component`、`native_lock_guard_is_the_default`、`reopening_with_auto_create_disabled_verifies_the_existing_schema`。

### 改动文件
- `Cargo.toml`（features + optional `agentscope-extensions-jdbc`）
- `src/lib.rs`（`pub mod jdbc_bridge` + 文档 + re-export）
- `src/distributed_store.rs`（`MysqlDistributedStore::connect`，feature 双实现）
- `src/jdbc_bridge/{mod.rs, real.rs, stub.rs}`（**新增**）
- `tests/mysql_jdbc_bridge.rs`（**新增**，env 门控）

### 未关闭项
- 本轮未跑 cargo：`cargo build -p agentscope-extensions-mysql --features jdbc-bridge` 与 `cargo test -p agentscope-extensions-mysql` 需由主智能体实测；若 `MysqlJdbcStoreBuilder` 的 `IsolationScope` 等类型与方法签名有出入，集中在 `src/jdbc_bridge/real.rs` 一处。
- `MysqlAsyncBackend` 与 `MysqlJdbcStore` 仍是两套并行入口（分别对应 crate 自有 seam 与 jdbc 门面）；未做相互复用，避免把 jdbc 类型引入默认构建。
