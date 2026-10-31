# POSTGRESQL 迁移执行历史（归档）

> 本文件是 [`../POSTGRESQL__GAP_TRACKER.md`](../POSTGRESQL__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 裁定执行记录（2026-10-22，汇总方） · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 8. 2026-10-28 执行记录（关闭 P1-3 / P1-4） · 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决） · 9. 2026-10-30 执行记录 — advisory lock 会话归属 / `save_list_sync` 原子性 · 10. 2026-10-31 收口执行记录（版本化 trait 覆写 / tracing / 死代码 / 集成测试 / async 桥接））。

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

> **本节部分描述已被 §9 取代（2026-10-30）**：`try_enter` 现在是**每租约一条专用会话**（不再是共享连接）；`PostgresSandboxLease::release()` 走会话 `unlock` + `close()`；无租约时不再「按 `compose_lock_key(key)` 回退解锁」（跨会话无法解锁，返回 `Ok(false)`）。

---

## 9. 2026-10-30 执行记录 — advisory lock 会话归属 / `save_list_sync` 原子性

### Java 依据

- `sandbox/PostgresSandboxExecutionGuard.java`：`tryEnter` 每次 `dataSource.getConnection()` 取**独立 `Connection`**，并在该连接上轮询 `SELECT pg_try_advisory_lock(?)`；内部类 `PostgresLease#close()` 先 `SELECT pg_advisory_unlock(?)`，再在 `finally` 里 `conn.close()`。类注释明确「锁绑定在连接上，连接关闭即自动释放」。
- `state/PostgresAgentStateStore.java#executeInWriteTransaction`（`:239-258`）：`setAutoCommit(false)` → 执行 → `commit()`；异常 → `rollback()`（失败则 `addSuppressed`）→ 抛出；`finally` 复原 autoCommit。`saveList`（`:453`）的 delete + N×insert + `:_hash` 三步整体位于该事务内。

### 任务 1（原 P1）：advisory lock 的会话归属

**问题**：PG advisory lock 是**会话级**且**同会话可重入**。旧实现 `PgSimpleClient` 只持一个 `tokio_postgres::Client`，`spawn_blocking` 不改变会话，因此同一进程内两个租约对同一隔离 key 的第二次 `pg_try_advisory_lock` 会返回 true —— 互斥静默失效。

| 文件 | 改动 |
|---|---|
| `src/client.rs` | 新增 `PostgresLockSession` trait（`try_lock` / `unlock` / `close`）与 `PostgresClient::open_lock_session()`；**默认实现返回 `PostgresError::DriverUnavailable`**（显式失败，绝不假装加锁成功） |
| `src/guard.rs` | `try_enter` 为每个租约 `open_lock_session()` 取独立会话并在其上轮询；`PostgresSandboxLease` 改持 `Option<Box<dyn PostgresLockSession>>`，`release()`/`Drop` 先 `unlock` 再 `close`（unlock 失败也关会话，对齐 Java `finally`）；`release(lock_name)` 只在**本 guard 保留的租约**中查找（跨会话无法解锁，与 Java 无跨租约释放 API 一致）；`SandboxGuard::try_enter` 对 `DriverUnavailable` 用 `error!` 明确记录「无分布式互斥」 |
| `src/driver.rs` | `PgSimpleClient` 增持 `config: Config` + `Arc<Runtime>`；`open_lock_session()` 用同一 `Config` 新建**专用连接**（`PgLockSession`），`close()` 丢弃 client 关闭连接 |
| `src/memory.rs` | 新增 `MemoryLockSession`：每会话唯一 token + 重入计数 `depth`、异会话互斥、`close()` 释放该会话全部锁；**共享连接上的 advisory SQL 改为显式报错**；`fork()` 语义收敛为「同库的另一客户端」 |
| `crates/agentscope-extensions-postgresql/Cargo.toml` | `tokio-postgres` 增加 `runtime` feature。**既有缺陷**：`Config::connect` / `tokio_postgres::connect` 均受该 feature 门控，此前 `--features postgres-driver` 编译失败（`EXIT=101`） |
| `src/lib.rs` | 导出 `PostgresLockSession` / `SqlStatement`；架构文档补两条会话级能力 |

新增离线测试（`guard.rs`，均带 `// Java:` 溯源）：

- `try_enter_acquires_lock_and_returns_lease`
- `distinct_leases_on_the_same_key_are_mutually_exclusive` ← 需求①（含③：释放后他人可获取）
- `lock_session_is_reentrant_within_one_session` ← 需求②（同会话重入 + 异会话互斥 + 逐层 unlock + 非 owner 不能解锁）
- `closing_a_lock_session_releases_every_lock_it_holds` ← 需求⑤（含 `close()` 幂等、关闭后拒绝操作、锁可被他人重新获取）
- `client_without_dedicated_sessions_fails_loudly` ← 不支持独立会话的 client 必须 `DriverUnavailable`（fake 的 `query` 用 `unreachable!` 钉死「不得回退到共享连接」）
- 需求④（Drop 自动释放）与「锁名/permit 语义不变」由既有 `lease_drop_releases_lock` / `lease_release_is_idempotent` / `release_by_key_reports_owner_mismatch` / `compose_lock_key_*` / `murmur3` 5 用例继续覆盖

**行为变更（有意，向 Java 对齐）**：同一 guard 对同一 key 连续两次 `try_enter` 现在会**竞争**（旧用例 `try_enter_acquires_and_reenters_same_connection` 按「共享连接可重入」的假语义编写，已替换为租约级用例）。重入只存在于**单一租约会话内**。

### 任务 2（原 P2）：`save_list_sync` 原子性

- `src/client.rs`：新增能力式事务缝 `PostgresClient::execute_in_transaction(&[SqlStatement]) -> PgResult<Vec<QueryOutcome>>`（默认 `DriverUnavailable`，不做非原子兜底）+ `SqlStatement { sql, params }`。选批量能力而非 async 闭包：保持对象安全，规避 async trait 生命周期与 `spawn_blocking` 组合。
- `src/state.rs`：`save_list_sync` 两条写路径（全量重写 = delete + N×insert + hash；增量追加 = N×insert + hash）改为先构造语句再 `execute_in_transaction`；`delete_list_items_sync` / `insert_items_sync` / `save_hash_sync` 改为纯语句构造器（`delete_list_items_stmt` / `insert_items_stmts` / `save_hash_stmt`），SQL 文本与参数顺序不变（`MemoryClient` 分派不受影响）。
- `src/driver.rs`：`execute_in_transaction` 用专用连接 + `BEGIN` / `COMMIT`，任一语句失败 `ROLLBACK` 并返回原错误。
- `src/memory.rs`：`execute_in_transaction` 快照 `Tables` → 逐语句执行 → 失败整体恢复（真 rollback）；新增测试钩子 `fail_query_after(n)` / `query_count()`，把「失败落在第几条语句」钉死（避免用例悄悄退化）。
- 新增测试（`state.rs`，`// Java:` 溯源）：
  - `save_list_rolls_back_when_a_statement_fails_midway`（失败落在第 3 条 insert，断言行数 / 内容 / `:_hash` 行全部未变，且之后仍可用）
  - `save_list_append_rolls_back_when_the_hash_write_fails`（失败落在 hash upsert）
  - `save_list_requires_transaction_support`（不支持事务的 client → `DriverUnavailable` 且零写入）

### 验证

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-extensions-postgresql -j 2` | **EXIT=0，123 passed / 0 failed / 0 ignored**（基线 116；+7 = guard +5 新 −1 旧 + state +3） |
| `cargo test -p agentscope-extensions-postgresql -j 2 --features postgres-driver` | **EXIT=0，123 passed / 0 failed** |
| `cargo build -p agentscope-extensions-postgresql --features postgres-driver` | **EXIT=0**（改动前 EXIT=101，缺 `runtime` feature） |
| `cargo fmt -p agentscope-extensions-postgresql -- --check` | **EXIT=0** |

用例分布：`state` 40 / `sql` 19 / `store` 19 / `snapshot` 16 / `guard` 16 / `distributed` 8 / `murmur3` 5 = **123**。全部离线（`MemoryClient` + trait fake），无真实 DB、无网络、无 `#[ignore]`。

### 反向验证（3 组，均已恢复）

1. **共享会话**：把 `MemoryClient::open_lock_session` 的会话 token 改为按 client 指针固定（等价旧「一个连接服务全部租约」）→ `guard::tests::distinct_leases_on_the_same_key_are_mutually_exclusive` **FAILED**（第二个租约「成功」，正是被修掉的 bug），其余 guard 用例仍绿；恢复后全绿。
2. **unlock 不释放**：`MemoryLockSession::unlock` 改为只减计数不删行 → `guard::tests::lock_session_is_reentrant_within_one_session` **FAILED**；恢复后全绿。
3. **事务不回滚**：`MemoryClient::execute_in_transaction` 失败分支去掉快照恢复 → `state::tests::save_list_rolls_back_when_a_statement_fails_midway` 与 `save_list_append_rolls_back_when_the_hash_write_fails` 均 **FAILED**（`2 failed / 37 passed`）；恢复后 123 passed。

### 未完成 / 残留

1. **真实 PostgreSQL 集成测试**仍未做（环境受限，需 PG 实例 / Docker）：`PgLockSession` 的「专用连接 + 会话关闭释放」与 `execute_in_transaction` 的 `BEGIN/COMMIT/ROLLBACK` 目前**仅编译验证**，未对真实服务端跑过。
2. §3.3 余项：`tracing` 埋点缺失（Java guard 侧 7 处日志；本次仅补 `DriverUnavailable` 的 `error!`）、`memory::MemoryClient` 未门控、组合者形态偏差、`error.rs` 死代码。
3. §3.2 末行：Java 53 个 Mockito 专属 `@Test` 仍无对应用例；其中「`executeInWriteTransaction` 未迁移」已由本节关闭，`executeInWriteTransactionKeepsAutoCommitWhenAlreadyDisabled` 属连接池 autoCommit 复用语义，Rust 无对应概念 → 记「设计决定（不迁移）」。
4. 事务缝只覆盖 `save_list`：`save` / `save_if_version` / `truncate` / 单值 CAS 本就是单语句，无需事务。

## 10. 2026-10-31 收口执行记录（版本化 trait 覆写 / tracing / 死代码 / 集成测试 / async 桥接）

> 范围：仅 `crates/agentscope-extensions-postgresql/**` 与本文件；未动根 `Cargo.toml`/core/harness。
> **未跑 cargo**（按约定由主智能体统一验证），用例数为静态计数。

### 10.1 逐项处置

| 条目 | 处置 | 源码级依据 |
|---|---|---|
| §3.2 `supportsVersioning()`/`getVersioned()`（原「设计决定，不迁移 + 待复核」） | **已复核并实施**——A2 上提后 core 确认有版本化面，改回覆写 | `impl AgentStateStore for PostgresAgentStateStore`：`supports_versioning() -> true`；`get_versioned` 在 `spawn_blocking` 内同时读值与 `read_version_sync`，失败/非法 key 降为 `UNVERSIONED`；`save_if_version` 委托固有实现，冲突返回 `UNVERSIONED`（不误报成功） |
| §3.3 tracing 埋点缺失 | **已完成** | `guard.rs` 补 7 处（acquire/acquired debug、acquire 失败/超时 warn、release debug、not-owner warn、release 失败 warn，均带 `// Java: <行号>`）；`store.rs` builder 补 `debug!(table, "PostgresBaseStore built")` |
| §3.3 `to_core`/`CasFailed`/`DriverUnavailable` 死代码 | **已完成** | `error.rs` 删 `to_core` + `CasFailed`；`DriverUnavailable` 保留且有构造点（`open_lock_session` / `execute_in_transaction` 默认实现、`memory.rs` 共享连接分支） |
| §3.3 真实集成测试 | **已完成（代码就位，环境门控）** | `tests/postgres_integration_tests.rs`，5 用例，`AGENTSCOPE_TEST_POSTGRES_URL` 未设时打印 `skipping` 并 `return` |
| §3.2 SQL 失败路径用例缺口 | **已补齐** | `store::sql_failures_never_fabricate_a_successful_result`、`store::initialize_schema_failure_is_reported_on_build`、`snapshot::sql_failures_surface_through_the_remote_client` |
| trait 级 per-key `delete_key` | **已完成（设计决定，不迁移）** | **依据**：Java `PostgresAgentStateStore` 只有 `delete(userId, sessionId)`（`:657`），**无** per-key 重载，故本 crate 保留 core 默认实现（显式报错），不造假能力 |
| `Cargo.toml` 冗余依赖 | **已清理** | 删 `serde`、`futures`（`rg` 零引用）；`chrono`（`driver.rs` 时间戳转换）、`uuid`（`memory.rs` 测试）有实际引用，保留 |
| `memory::MemoryClient` 未门控 | **仍为待办（P2）** | 本轮未动：门控需同步迁移全部测试导入与 `lib.rs` re-export，属结构性改动，超出收口范围 |

### 10.2 关键修复：async trait 方法不得在运行时线程上跑同步 SQL

`AgentStateStore` 的 9 个方法全部改为经 `PostgresAgentStateStore::run_blocking`（`spawn_blocking` + `self.clone()`）执行；原因：`PostgresClient` seam 是同步的，而 `PgSimpleClient` 内部 `runtime.block_on`，在 Tokio 运行时线程上调用会 **panic**。

新增用例 `async_trait_methods_never_touch_the_client_on_the_runtime_thread`：`OffRuntimeClient` 在每次 `query` / `execute_batch` 时 `assert!(Handle::try_current().is_err())`，store 在 `spawn_blocking` 中构建，随后在 `#[tokio::test]` 内调用 `get` / `get_list` / `exists` / `list_session_ids` / `delete` —— 如果某个方法旦回了同步直调，断言立即失败（而非只在真实驱动下才炸）。

### 10.3 改动文件

| 文件 | 改动 |
|---|---|
| `src/state.rs` | `impl AgentStateStore` 全量重写（9 方法经 `run_blocking`）+ 版本化三方法覆写；+1 用例（异步桥接）；模块文档补「同步缝 / 阻塞池」约束 |
| `src/store.rs` | builder build 日志（对齐 Java `:440`）；+2 用例（SQL 失败不伪造成功 / `initializeSchema` 失败上报） |
| `src/snapshot.rs` | +1 用例（远程 client 的 SQL 失败路径） |
| `src/guard.rs` | 7 处日志（带 Java 行号溯源） |
| `src/error.rs` | 删 `to_core` / `CasFailed` |
| `Cargo.toml` | 删 `serde` / `futures` |
| `tests/postgres_integration_tests.rs` | **新增**（5 用例，`#![cfg(feature = "postgres-driver")]`） |

### 10.4 用例与计数（静态）

- 离线 lib 用例：**123 → 127**（`state` 40 → 41、`store` 19 → 21、`snapshot` 16 → 17，其余不变：`sql` 19 / `guard` 16 / `distributed` 8 / `murmur3` 5）。
- `--features postgres-driver` 额外运行集成测试 5 例（无 `AGENTSCOPE_TEST_POSTGRES_URL` 时内部 skip，仍计为 passed）。
- 新增用例名：`async_trait_methods_never_touch_the_client_on_the_runtime_thread`、`sql_failures_never_fabricate_a_successful_result`、`initialize_schema_failure_is_reported_on_build`、`sql_failures_surface_through_the_remote_client`；集成侧 5 例：`agent_state_roundtrip_and_versioned_cas_on_a_live_server`、`base_store_cas_and_prefix_search_on_a_live_server`、`snapshot_roundtrip_on_a_live_server`、`advisory_lock_is_exclusive_across_sessions_on_a_live_server`、`transaction_seam_commits_and_rolls_back_on_a_live_server`。

### 10.5 未完成 / 残留

1. **真实 PostgreSQL 实机未跑**：集成测试已就位，但本轮无实例/无 Docker；`PgSimpleClient` 的专用连接、`BEGIN/COMMIT/ROLLBACK`、advisory lock 跨会话互斥仍仅静态审核。
2. `memory::MemoryClient` 未门控（P2，见 10.1）。
3. 组合者形态偏差（工厂方法 vs 字段式）未改：与 §4.2-3 裁定一致，不入本轮。
4. Java 侧 JDBC 异常包装（`ResultSet#close` / `rollback` `addSuppressed`）与 null 参数拒绝仍无对应用例：`PostgresClient` seam 无 `ResultSet`/`Connection` 概念，属设计边界。
