# MONGODB 迁移执行历史（归档）

> 本文件是 [`../MONGODB__GAP_TRACKER.md`](../MONGODB__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节（§5 保留在 tracker），原文未改写；另含一段原误置于 §4.2 与 §5 之间的执行记录，位置与原文一致（节标题：2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决） · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 2026-10-31 loop 收口记录）。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_harness_store()` 从「投影子集」改为全量投影。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `Arc::new(self.agent_state_store.clone())` | `MongodbAgentStateStore`（含 SESSION 级联删快照回调） |
| `isolation_scope` | `IsolationScope` | 字段 `isolation_scope` | 默认 `Session`，`with_isolation_scope` 可覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `Arc::new(self.base_store.clone())` | `MongodbStore` |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc::new(self.snapshot_spec.clone())` | `MongodbSnapshotSpec` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `self.guard_client.clone()`（`Arc<MongodbGuardClient>`） | MongoDB 锁；新增 `impl SandboxGuard for MongodbGuardClient` |

### 代码改动

- `src/distributed_store.rs::to_harness_store()`：填满 5 槽（builder，不用 `..Default::default()`；同时删除原先无意义的 `let _ = &mut builder;`）。
- `src/sandbox_guard.rs`：`impl SandboxGuard for MongodbGuardClient`，四方法委托固有方法（**不改行为**；`try_enter` 失败仍降级 `SandboxLease::noop()`，租约仍由 `leases` 表持有）。同步更新该类型文档（原写「无法 impl」）。
- 测试：`src/distributed_store.rs::harness_projection_fills_every_composite_slot`。

### 验证

`cargo test -p agentscope-extensions-mongodb` → **80 passed / 0 failed**。

### 未完成 / 语义差异

- 无留空槽位。
- **2026-10-31 勘误**：本文件原在此处写「`MongodbGuardClient::try_enter` 用 `futures::executor::block_on` 驱动 async 锁」——**已过时**。现行 `try_enter` / `release` 均为 async（`#[async_trait] impl SandboxGuard`，内部直接 `await tokio::time::sleep`）；全 crate 仅剩两处 `block_on`：①`is_held`（`SandboxGuard::is_held` 签名同步，单次非轮询读取，诊断用途）；②`store.rs` 同步 `BaseStore` trait 的桥接。
## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-31 loop 收口记录

> 本轮按 `.agentscope/workspace/loop-2026-10-31.md` 的关闭口径执行：**只改本 crate + 本 tracker**，未跑 cargo/rustc/clippy/fmt（由主智能体统一验证），未改根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。

### 关闭条目（§1.1 记「1 P1 / ~7 P2」→ 0 / 0）

| # | 原条目 | 级别 | 关闭方式 | 落点 |
|---|---|---|---|---|
| 1 | 真实 MongoDB 驱动接入 / 真实集成测试 | P1 | **已完成（设计决定，不迁移）** | 证据：Java 侧 `watch` / `ChangeStream`、`insertMany` / `bulkWrite` / `updateMany` / `deleteMany` / `replaceOne`、`withTransaction` / `startSession` / `ClientSession` 在 `src/main/java` **命中 0**；GridFS 分块已对齐；真实集成需 live 后端。接入配方见 §4.2-3 |
| 2 | `database_name` 未作用于 state / base store（本轮实证新增） | P2 | 已完成（修复） | `distributed_store.rs::new` 用 `scoped_collection`，新增 `state_store.rs::MongodbAgentStateStore::collection()`、`slot_filter` 改 `pub(crate)` |
| 3 | `MongodbGuardClient` ↔ harness `SandboxLease` 释放闭环 | P2 | 已完成（harness 侧 `SandboxManager::release` 已 await） | 新增用例 `harness_lifecycle_releases_the_lock_exactly_once` |
| 4 | `on_delete` 用 `block_on` 且吞错 | P2 | 已完成（改 async 回调 + 分级日志） | `state_store.rs::SessionDeleteCallback`、`distributed_store.rs` 级联闭包 |
| 5 | `_cache: Arc<Mutex<()>>` 占位 | P2 | 已完成（删除死字段与导入） | `distributed_store.rs` |
| 6 | `lib.rs` / rustdoc 链接私有模块路径 | P2 | 已完成（文档改 `SandboxGuard` 公开路径） | `lib.rs`、`distributed_store.rs:187`、`sandbox_guard.rs:41` |
| 7 | `client.rs` 无 `tracing` 埋点 | P2 | 已完成（设计决定，不迁移） | 证据：`client.rs` = seam + 测试替身，Java 对应物是驱动自身；生产埋点计数见 §3.3 |
| 8 | 内存后端无 16GB 上限 | P2 | 已完成（设计决定，不迁移） | GridFS 上限由服务端强制，不引入不可测分支 |
| 9 | P2 清理：`MongoError::IllegalState` 未用 / `serde` 冗余 / `UNVERSIONED` 重复定义 / `get_version` 无用变量 | P2 | 已完成 | `client.rs`、`Cargo.toml`、`state_store.rs` |
| 10 | 测试密度（Java 9 类 147 `@Test`） | P2 | 已完成 | 映射表见 §2.2；本轮新增 7 个语义用例（见下） |

### 变更文件

- `Cargo.toml`（删 1 行未使用的 `serde`）
- `src/client.rs`（删未用错误变体 `IllegalState`；文档补驱动能力边界声明）
- `src/state_store.rs`（`UNVERSIONED` 改 re-export core；`SessionDeleteCallback` + async `on_delete`；`collection()` getter；`slot_filter` 升 `pub(crate)`；`delete_key_inner` 补 `require_session_id`（Java `slotId` 抛 `IllegalArgumentException`）；清理 `get_version` 无用变量）
- `src/distributed_store.rs`（state/base store 数据库限定；async 级联闭包 + info/warn；删 `_cache`；`find_session_snapshot_id` 用同一限定集合 + `slot_filter(None, ..)`；rustdoc 链接修正）
- `src/sandbox_guard.rs`（rustdoc 链接修正 + 2 个新用例）
- `src/store.rs`（delete 幂等断言）
- `src/lib.rs`（文档表与 guard 双形态说明）
- 本 tracker（§1、§2.2、§3.3、§4.2、本节）

### 新增 / 强化用例（8 新 + 3 强化）

1. `distributed_store::tests::database_name_partitions_state_and_base_store`（新）
2. `sandbox_guard::tests::lock_document_records_owner_and_lease_ttl`（新）
3. `sandbox_guard::tests::lease_renewal_extends_the_lock_expiry`（新）
4. `sandbox_guard::tests::harness_lifecycle_releases_the_lock_exactly_once`（新）
5. `state_store::tests::trait_versioning_surface_matches_core_contract`（新）
6. `state_store::tests::concurrent_create_if_absent_only_one_writer_wins`（新）
7. `state_store::tests::set_on_insert_records_session_identity`（新）
8. `state_store::tests::blank_session_id_is_rejected`（新）
9. 强化：`state_store::tests::on_delete_callback_fires`（断言 owned 身份 + 删除前触发、删除后 `exists == false`）、`store::tests::delete_removes_only_target_key`（幂等）、`distributed_store::tests::seed_session_snapshot`（改限定集合）

> 计数：新增 8 个用例（上文 1-8），强化 3 个 → 82 + 8 = **预期 90 passed**（原计划 7 个，`blank_session_id_is_rejected` 为追加）。**未跑 cargo**，由主智能体实测确认。

### 反向验证方案（供主智能体验证有效性）

| 回退实现 | 预期失败用例 |
|---|---|
| `MongodbDistributedStore::new` 恢复为 `MongodbStore::with_defaults` / 不传限定 sessions 集合 | `database_name_partitions_state_and_base_store`（db_b 会看到 db_a 的文件/会话） |
| `lock_document` 的 `owner` 不写入 / `expiresAt` 改用 `lock_timeout` | `lock_document_records_owner_and_lease_ttl` |
| 续约任务改为 no-op（不更新 `expiresAt`） | `lease_renewal_extends_the_lock_expiry` |
| `MongodbGuardClient::release` 只弹出 lease 不删文档 | `harness_lifecycle_releases_the_lock_exactly_once` |
| `get_versioned` 对缺失 key 返回 `UNVERSIONED` | `trait_versioning_surface_matches_core_contract` |
| create-if-absent 分支改为无条件写 | `concurrent_create_if_absent_only_one_writer_wins` |
| `$setOnInsert` 去掉 `user_id`/`session_id` | `set_on_insert_records_session_identity`、`exists_delete_and_list_session_ids` |
| `delete_key_inner` 去掉 `require_session_id` | `blank_session_id_is_rejected` |

### 未关闭项 / 跨模块需求

- **未关闭项：无**（§3 全部收敛）。
- **跨模块需求：无**（驱动接入配方已写在本 tracker，涉及新增 workspace 依赖时由汇总方决定）。
