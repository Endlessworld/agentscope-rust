# MONGODB 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MONGODB** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-mongodb\`
> **Java 子模块**：（单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包）
> **Rust 目标**：`crates/agentscope-extensions-mongodb/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-mongodb` |
| Java 子模块 | （单模块：`root` + `sandbox` / `snapshot` / `state` / `store` 四个子包） |
| Java 规模（实测 2026-09-18） | 主代码 **2231 行（9 类）** / 测试 **2427 行（8 测试类 + 2 testutil，147 个 `@Test`，见 §2）** |
| Rust crate | `crates/agentscope-extensions-mongodb` |
| 迁移波次 | B |
| 当前状态 | **部分完成**（`cargo build` 通过；lib **62 passed / 0 failed** + **1 doc-test passed**）；P0 骨架全部落地，存在 P1/P2 语义与文档待确认项，见 §3 |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `mongodb.MongoConstants`（38 行） | 默认库名 + 四个集合名常量 | `constants.rs`（`DEFAULT_DATABASE` / `SESSIONS_COLLECTION` / `BASE_STORE_COLLECTION` / `SNAPSHOTS_COLLECTION` / `SANDBOX_LOCKS_COLLECTION`） | 常量值逐一对齐 |
| `mongodb.MongoDistributedStore`（299 行，`implements DistributedStore, AutoCloseable`） | 组合四组件 + 生命周期 + 级联删快照 | `distributed_store.rs::MongodbDistributedStore` | Java 直接实现接口；Rust 用组合 + `to_harness_store()` 投影（harness `DistributedStore` 是 struct，仅 `agent_state_store`+`isolation_scope`）。`new` 内注册 `on_delete` 级联删快照 |
| `mongodb.MongoIndexUtils`（160 行） | 索引创建 + error 85 冲突迁移（按 key pattern 查真实索引名后 drop+重试） | `index_utils.rs::create_index_with_migration` / `IndexSpec` / `key_pattern_json` / `MAX_MIGRATION_ATTEMPTS=3` | 语义保留；`tracing::info/warn/debug` 埋点 |
| `mongodb.MongoKeyEscaper`（147 行） | 递归转义 `.`/`$`/`\0`/`\` 键名 | `key_escaper.rs::escape/unescape/escape_key/unescape_key` | 编码对齐（`.`→`\E`、`$`→`\$`、`\0`→`\N`、`\`→`\\`） |
| `mongodb.state.MongoAgentStateStore`（676 行） | 会话状态：单值 + 列表 + 版本 CAS + 列表哈希 + TTL | `state_store.rs::MongodbAgentStateStore`（+ builder / `UNVERSIONED` / `validate_state_key`），`impl agentscope_core::state::AgentStateStore` | 复合 `_id {user_id, session_id}`；`states`/`versions`/`hashes` 子文档；`save_if_version`/`get_version` 为固有方法 |
| `mongodb.store.MongoBaseStore`（298 行） | 命名空间 KV + CAS + 前缀搜索 | `store.rs::MongodbStore`（+ `NS_SEPARATOR` / `NS_UPPER_BOUND`），`impl agentscope_harness::filesystem::remote::store::BaseStore` | US(0x1F) 命名空间编码 + `$gte/$lt` 范围前缀匹配；`version` 从 1 起，`put` 每次成功写 +1 |
| `mongodb.sandbox.MongoSandboxExecutionGuard`（402 行） | 分布式锁（insert + 过期 reclaim + 租约续约） | `sandbox_guard.rs::MongodbSandboxExecutionGuard` + `Builder` + `MongodbLeaseHandle` + `MongodbGuardClient` + `compose_lock_id` | harness `SandboxExecutionGuard` 为**具体 struct 非 trait** → `MongodbGuardClient` 镜像其方法（`try_enter/release/is_held/active_count`）；`compose_lock_id` 与 Java **字节级一致**（`lock:`+hex(SHA-256(`scope:value`))，scope 小写） |
| `mongodb.snapshot.MongoRemoteSnapshotClient`（178 行） | GridFS 快照上传/下载/存在 + legacy 单文档回退 | `snapshot.rs::MongodbRemoteSnapshotClient`，`impl RemoteSnapshotClient`，`delete` 为固有方法 | Rust 用统一 blob map（`gridfs_*`）；**无 GridFS 分块/legacy 回退语义**（见 §3.2/§3.3） |
| `mongodb.snapshot.MongoSnapshotSpec`（33 行，`extends RemoteSnapshotSpec`） | 快照 spec 便捷包装 | `snapshot.rs::MongodbSnapshotSpec`，`impl SandboxSnapshotSpec` | 内部包装 harness `RemoteSnapshotSpec`；`new` 当前忽略 `database_name`（`snapshot.rs:58`） |
| （测试替身）Java Testcontainers / Mockito | 离线/容器测试 | `client.rs::InMemoryMongoClient` | 忠实内存后端；duplicate-key 11000、`$set/$inc/$push/$setOnInsert/$unset`、dot-path 均已对齐 |
| （驱动缝隙）Java 直依赖 `mongodb-driver-sync` | 文档/索引/GridFS 原始 API | `client.rs::MongoClient` trait（异步 seam） | driver-agnostic；真实驱动可后续实现同一 trait（§4.2） |
| （列表哈希）Java core `ListHashUtil` | 列表变更检测 | `list_hash.rs`（crate 本地，SHA-256）**与** `agentscope_core::state::list_hash` 并存 | 两实现算法不同；**已裁定统一到 core**（见 §3.3、汇总 §3.3） |

> 实测规模/测试数：`MongoDistributedStoreTest`15、`MongoIndexLifecycleContractTest`10、`MongoSandboxExecutionGuardTest`24、`MongoRemoteSnapshotClientTest`16、`MongoSnapshotSpecTest`2、`MongoAgentStateStoreContractTest`9、`MongoAgentStateStoreTest`39、`MongoBaseStoreContractTest`9、`MongoBaseStoreTest`23（合计 147 `@Test`）；`testutil/` 另有 `DockerAvailabilityCondition`(59 行) / `RequireDocker`(42 行) 各 1 个 `@Test` 注解用于 Docker 门控。

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `org.mongodb:mongodb-driver-sync` | `MongoClient`/`MongoDatabase`/`MongoCollection` | **无真实驱动**；`client::MongoClient` trait（异步）+ `client::InMemoryMongoClient` | `Cargo.toml` 未引入 `mongodb` crate；`EXTENSIONS_GAP_TRACKER.md:122` 建议后续统一加入 `[workspace.dependencies]`，当前待汇总 |
| Jackson（`JsonUtils`） | `Document` ↔ Java 对象 | `serde` = `1`、`serde_json` = `1`（workspace） | — |
| BSON `Document`/`BsonDocument`/`IndexOptions` | 文档与索引描述 | `serde_json::{Map, Value}` + `index_utils::IndexSpec` | — |
| `GridFSBucket`/`GridFSFile` | 快照分块存储 | `MongoClient::gridfs_upload/download/exists/delete` + 内存 blob map | 无 255KB 分块/16GB 语义 |
| SLF4J `Logger` | 日志 | `tracing` = `0.1`（workspace） | `index_utils`/`sandbox_guard`/`state_store`/`store` 有埋点；`snapshot`/`client` 无 |
| `java.security.MessageDigest`（SHA-256） | 锁 id | `sha2` = `0.10`（workspace） | `compose_lock_id` 与 Java 字节级一致 |
| `java.util.UUID` | 租约 token | `uuid` = `1`（v4，workspace） | — |
| `java.util.Date` / `java.time` | 时间戳（`expiresAt`/`_updated_at`/`createdAt`） | `chrono` = `0.4`（`Utc::now().timestamp_millis()`，workspace） | — |
| `ScheduledExecutorService` | 租约续约 | `tokio` = `1`（`time`/`rt`）+ `JoinHandle` 后台任务 | `lease_ttl / 3` 续约 |
| `java.util.concurrent` 阻塞桥接 | 同步 `BaseStore` 调异步后端 | `futures` = `0.3`（`futures::executor::block_on`） | `store.rs`/`sandbox_guard.rs` 同步 trait 经此调用异步 client |
| `RuntimeException` / `MongoCommandException` | 错误传播 | `thiserror` = `2`（`client::MongoError`） | 映射 `agentscope_core::error::Error` |
| JUnit5 + Mockito + Testcontainers | 离线/容器测试 | Rust `#[cfg(test)]` 内联（`InMemoryMongoClient`） | Java contract test 需 Docker（`testutil`） |

**harness/core 复用契约（对齐 `EXTENSIONS_GAP_TRACKER.md` §2.1，未新建公共 store crate）**：
- `impl agentscope_core::state::AgentStateStore`（`state_store.rs:458`）—— trait 方法 `save/save_list/get/get_list/exists/delete/list_session_ids`（trait 定义见 `agentscope-core/src/state/store.rs:12`）。
- `impl agentscope_harness::filesystem::remote::store::BaseStore`（`store.rs:122`）—— `get/put/put_if_version/search/delete`；`StoreItem{key,value,version}`（`harness/src/filesystem/remote/store.rs:21`），`version` 从 1 起、0=未知。
- `impl agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（`snapshot.rs:86`）—— `upload/download/exists`。
- `impl agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec`（`snapshot.rs:148`）—— `build(snapshot_id)`。
- `agentscope_harness::agent::sandbox::SandboxExecutionGuard` 是**具体 struct**（`harness/src/agent/sandbox/guard.rs:13`），非 trait → 通过 `MongodbGuardClient`（`sandbox_guard.rs:372`）镜像其方法供 harness 直接使用。
- `agentscope_harness::agent::distributed_store::DistributedStore` 是**具体 struct**（`harness/src/agent/distributed_store.rs:14`，仅 `agent_state_store` + `isolation_scope`）→ `MongodbDistributedStore::to_harness_store()` 投影子集，其余组件经类型化访问器直连。
- `SandboxIsolationKey` 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`（`harness/src/agent/sandbox/key.rs:11`）。

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 导出 10 模块 | 全部 | 已完成 | `lib.rs` 声明 `client`/`constants`/`distributed_store`/`index_utils`/`key_escaper`/`list_hash`/`sandbox_guard`/`snapshot`/`state_store`/`store` 并 re-export 公共类型 |
| `MongodbAgentStateStore`（impl core `AgentStateStore`） | `state/MongoAgentStateStore.java` | 已完成 | 复合 `_id`、`states`/`versions`/`hashes` 子文档、TTL；`save_if_version`/`get_version`/`UNVERSIONED = -1` 固有 API |
| `MongodbStore`（impl harness `BaseStore`） | `store/MongoBaseStore.java` | 已完成 | US(0x1F) 编码 + 范围前缀搜索；`put_if_version` 对齐 harness CAS（`expected==0` = create-if-absent） |
| `MongodbRemoteSnapshotClient`（impl `RemoteSnapshotClient`） | `snapshot/MongoRemoteSnapshotClient.java` | 已完成 | `upload/download/exists` + 固有 `delete`；无 TTL |
| `MongodbSnapshotSpec`（impl `SandboxSnapshotSpec`） | `snapshot/MongoSnapshotSpec.java` | 已完成 | 包装 harness `RemoteSnapshotSpec`；快照 id 校验沿用 harness |
| `MongodbSandboxExecutionGuard` + builder + lease + guard client | `sandbox/MongoSandboxExecutionGuard.java` | 已完成 | 插入 + 过期 reclaim + token 作用域释放 + `lease_ttl/3` 续约 |
| `MongodbDistributedStore` 组合者 | `MongoDistributedStore.java` | 已完成（形态差异） | 五个组件访问器 + `to_harness_store()`；含 `on_delete` 级联删快照 |
| 无真实 DB 可用的内存后端 | Java Testcontainers / Mockito | 已完成 | `client::InMemoryMongoClient` |
| **离线单元测试** | Java 8 测试类 **147 `@Test`**（contract test 需 Docker） | **部分完成（P1 缺口）** | 现状 lib **62** + doc-test **1**；crate **无 `tests/` 目录**（实测 `Test-Path` = False），全为 `#[cfg(test)]` 内联单测。补测方向：contract-test 级 state/store/snapshot/guard 往返、error-85 索引迁移并发、级联删快照、TTL、键转义 round-trip |

### 3.2 其它待办（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `supportsVersioning()` / `getVersioned()` | `state/MongoAgentStateStore.java:160,361` | 设计决定（不迁移） | core `AgentStateStore` trait 无版本方法，故降为固有 `get_version` / `save_if_version` |
| 单 key 删除 `delete(user, session, key)` | `state/MongoAgentStateStore.java:523` | **待办** | Rust `state_store.rs:433 delete_key_inner` 仅被测试 `:745` 调用，**未接入公共/生产路径** → 非 test 构建触发 `dead_code` warning（见 §3.3、§4.2） |
| `close()` / `ownsClient` / `fromConnectionString` | `MongoDistributedStore.java`；各组件 `close()` | 设计决定（不迁移） | Rust `Arc<dyn MongoClient>` 无连接所有权/关闭语义 |
| 快照 legacy 单文档回退（`download`/`exists`） | `snapshot/MongoRemoteSnapshotClient.java` | **待确认** | Rust 用统一 blob map，未区分 GridFS / legacy 集合；真实驱动接入时需补 |
| GridFS 分块（255KB）/ 16GB 语义 | 同上 | **待确认** | 内存后端不模拟；无真实驱动 |
| `ensureIndexes`（TTL / 复合 / sparse） | 各组件 | 已完成 | state(`ttl_days`)、store、guard(TTL `expiresAt`) 均经 `create_index_with_migration` |

### 3.3 待确认 / 后续增强（P2）
| 缺口项 | 说明 | 状态 |
|---|---|---|
| `list_hash` 双实现 | crate `list_hash.rs`（SHA-256 over canonical JSON）vs core `agentscope_core::state::list_hash`（`compute_hash<T: Hash>` DefaultHasher 采样 + `EMPTY_HASH`/`SAMPLING_THRESHOLD`）；`state_store.rs` 当前用 **crate 本地实现** | **已裁定（见汇总 §3.3）：统一到 core，crate 本地转委托** |
| `MongodbSnapshotSpec::new` 忽略 `database_name` | `snapshot.rs:58` 显式 `let _ = database_name...`；InMemory 后端仅按 bucket 键 | **待确认** |
| `lib.rs` doc 表把 `MongodbGuardClient` 写作 "implements `SandboxExecutionGuard`" | harness 该类型是 struct 非 trait，实际为镜像方法，文档不准确 | **待办（文档）** |
| `delete_key_inner` dead_code warning | `state_store.rs:433` 定义，仅测试 `:745` 引用；前一轮记录为唯一残留 warning（本次未重跑 cargo 复核） | **待办** |
| `_cache: Arc<Mutex<()>>` 占位 | `distributed_store.rs:45`，对应 Java volatile double-checked 缓存的影子字段，实际未缓存任何值 | **待办** |
| 组合者形态偏差 | Java `implements DistributedStore, AutoCloseable`；Rust 为组合 + `to_harness_store()`，未暴露 `close` / `ownsClient` | **待确认** |
| `snapshot.rs` / `client.rs` 无 `tracing` 埋点 | Java `MongoRemoteSnapshotClient` 有 log.info/warn；Rust 该两文件无日志 | **待办** |
| 真实 MongoDB 驱动接入 | 未引入 `mongodb` crate（`EXTENSIONS_GAP_TRACKER.md:122` 建议统一加入 workspace deps）；无集成测试 | **待办（跨模块/汇总方）** |

### 3.4 设计决定（不迁移）汇总
| 项 | 依据 |
|---|---|
| 不新建公共 store crate；各 crate 自建 `XxxDistributedStore` | `EXTENSIONS_GAP_TRACKER.md` §2.1 已裁定 |
| 不引入真实 `mongodb` 驱动，改 `MongoClient` trait + `InMemoryMongoClient` | 保离线可编译/可测；驱动接入记 §4.2 |
| `close()` / 连接所有权 / `objectMapper` 注入 | harness/core 无对应语义，Rust 侧省略 |
| `supportsVersioning` / `getVersioned` 降为固有方法 | core trait 未定义版本 API |

## 4. 项目状态与下一步
### 4.1 当前状态
- **编译/测试基线（2026-09-18，本会话）**：`cargo build -p agentscope-extensions-mongodb` 与 `cargo test -p agentscope-extensions-mongodb` **均通过**。
  - `cargo test -p agentscope-extensions-mongodb`：**56 passed / 0 failed**（lib 单测）+ **1 passed**（doc-test，`lib.rs` line 35）。
  - 剩余 1 条 warning：`state_store.rs:433 delete_key_inner` 从未使用（既有死代码，非阻塞；后续决定是接线还是删除）。其余 warning 来自 core/harness，不在本 crate 范围。
- **本会话修复内容（3 处根因，均在本 crate 内）**：
  1. `distributed_store.rs`（tests）— 测试引用了 harness `sandbox` 的**私有子模块**路径 `sandbox::guard::SandboxExecutionGuard` / `sandbox::key::SandboxIsolationKey`，触发 `E0603 module is private`（这掩盖了 `cargo build` 已通过、但 `cargo test` 失败的差异）。改为使用 harness 的**公开重导出** `agentscope_harness::agent::sandbox::SandboxIsolationKey`；并在 tests 模块内按需 `use ... as _;` 引入 `BaseStore` / `SandboxSnapshotSpec` trait（曾误判为 lib 级 unused import，移出后反而破坏 test 编译，已回收进 tests 模块）。
  2. `client.rs`（`InMemoryMongoClient` 的 `find_one_and_update` / `update_one`）— upsert 分支在 `_id` 已存在却未命中 filter（例如带 `$exists:false` 的 create-if-absent）时**静默追加重复 `_id` 文档**，与真实 MongoDB / Java 的 duplicate-key(11000) 语义不符，导致 `state_store::versions_increment_and_cas_works` 期望“第二次 create-if-absent 返回 `UNVERSIONED`”落空。修复：upsert 插入前若已存在同 `_id` 文档则返回 `MongoError::DuplicateKey`（与 `insert_one` 及 store.rs `put_if_version` 的既有语义一致）。
  3. `list_hash.rs::needs_full_rewrite` — 原实现把**整个新列表**的哈希与存储哈希比较，导致等长/追加场景恒判定为需全量重写，`appended_list_does_not_need_rewrite` 失败。按 Java `ListHashUtil.needsFullRewrite` 语义重写：列表收缩 → 重写；无存储哈希但有既有数据 → 重写；否则比较**长度为 `existing_count` 的前缀哈希**与存储哈希，匹配即可增量追加。
- **未触碰**：根 `Cargo.toml`、`agentscope-core`、`agentscope-harness`、其它 crate。
- **未删除任何既有功能**。
- **本回填会话核对基线（汇总方提供，未重跑 cargo）**：`cargo build -p agentscope-extensions-mongodb` 通过；`cargo test -p agentscope-extensions-mongodb` = lib **62 passed / 0 failed** + **1 doc-test passed**；全量 `cargo test --workspace` = **1746 passed / 0 failed**（86 test target，exit 0）。crate 无 `tests/` 集成目录（实测 `Test-Path` = False），测试全部为 `#[cfg(test)]` 内联单测。
### 4.2 下一步与阻塞
1. 无阻塞；`RemoteSnapshotSpec: Clone` / `dyn MongoClient: Debug` / 闭包 move 三类历史错误在当前代码中已不存在（`MongodbDistributedStore` 已手写 `Debug` impl、含 `dyn MongoClient` 的字段用 `Arc` 包裹），本会话未新增对 harness 的改动需求。
2. 建议后续：确认 `state_store.rs::delete_key_inner` 是否接入 `delete` 路径，否则删除以消除唯一残留 warning。
3. 建议后续：为 `find_one_and_update`/`update_one` 的 duplicate-key upsert 语义补充显式回归测试（本会话已通过 `state_store` 用例间接覆盖）。
4. **已裁定（2026-09-19，依 Java 源码）**：crate 本地 `list_hash.rs`（SHA-256）与 `agentscope_core::state::list_hash` 并存 —— **以 core 为正解**。Java `io.agentscope.core.state.ListHashUtil` 采用**采样**策略（`"size:N;"` + 采样位 `"i:itemHashCode,"`，末了 `Integer.toHexString(sb.hashCode())`；`EMPTY_HASH="empty:0"`、`SAMPLING_THRESHOLD=5`、采样位 `[0,n/4,n/2,3n/4,n-1]`），**不是全量哈希**；本 crate 的 SHA-256 全量实现**偏离 Java**（也丧失了采样的 O(1) 特性）。**要求后续改动：`state_store.rs` 改调 `agentscope_core::state::list_hash`**，crate 本地 `list_hash.rs` 降为对 core 的 re-export（或删除）。若确有跨进程稳定性诉求，应作为**升级 core 算法**的独立议题讨论，而非保留双实现并存。
5. 待裁定/缺口：`state_store.rs:433 delete_key_inner` 仅被测试 `:745` 调用，非 test 构建触发 `dead_code` warning；应接入 `delete(user,session,key)` 公共路径或删除（见 §3.2、§3.3）。
6. 待裁定/缺口：`snapshot.rs` 未保留 Java 的 legacy 单文档回退与 GridFS 分块语义；`MongodbSnapshotSpec::new` 忽略 `database_name`。补真实驱动接入时一并处理（见 §3.2、§3.3）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mongodb` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mongodb` / `cargo test -p agentscope-extensions-mongodb` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mongodb/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

