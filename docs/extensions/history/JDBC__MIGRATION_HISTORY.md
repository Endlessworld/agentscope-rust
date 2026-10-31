# JDBC 迁移执行历史（归档）

> 本文件是 [`../JDBC__GAP_TRACKER.md`](../JDBC__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. P0 修复记录（2026-10-22） · 8. 执行记录（2026-10-28）：原生 advisory lock 端到端接线 + 两项 P2 · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 附录 B：`in_transaction` 后端覆盖要求（2026-10-22） · 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决） · 9. 2026-10-29 缺口核实（P1 口径修订） · 2026-10-31 loop 收口记录 · 2026-10-31 二轮收口记录 — MySQL 执行后端 + MySQL 集成测试（JDBC））。

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
| §3.1 | 「离线单元测试」状态 = **已完成（离线可覆盖部分）** | **部分完成** —— skill 相关 37 条 + `DialectBuilderTableGroupsTest` 7 条因 skill 未迁移而未覆盖（属 P0 功能缺口，非环境受限）；2026-10-29 勘误：skill 仓库已在 jdbc crate 落地、P0 已关，仅剩 `DialectBuilderTableGroupsTest` 7 条与 Docker 集成测试；**2026-10-31 收口 → 已完成**：两个集成测试各 8 条由 `tests/{mysql,postgres}_integration_tests.rs`（`AGENTSCOPE_TEST_{MYSQL,POSTGRES}_URL` 门控）覆盖，表组用例由 `tests/dialect_sql_tests.rs` 覆盖 |
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

