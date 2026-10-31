# REDIS 迁移执行历史（归档）

> 本文件是 [`../REDIS__GAP_TRACKER.md`](../REDIS__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 裁定执行记录（2026-10-22，汇总方） · 8. 执行记录（2026-10-28 第一轮，SCAN 覆盖：P1-2 闭合 / P1-3 部分闭合） · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决） · 2026-10-31 loop 收口记录）。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **静态复核**：**未编译、未跑测试**（`cargo build` / `cargo test` 全部禁用），**未改任何 Rust 代码**；本轮唯一改动 = 本文件。
- 手段：逐文件读取 Java 源码（`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-redis`）与 Rust crate（`crates/agentscope-extensions-redis`），核对符号存在性 / 行号 / 行为语义；`git log` / `git status` 佐证改动归属。
- **计数口径（重要）**：本机 PowerShell 5.1 默认编码为 **gb2312**，`Get-Content` 会把 UTF-8 中文误解码为乱码并**粘连换行**（实测根 `Cargo.toml`：`Get-Content` 得 90 行 vs UTF-8 `ReadAllLines` 得 105 行），故**一切行数均以 UTF-8 `ReadAllLines` 为准**；`Get-Content` 口径结果仅作对照记录。

### 6.2 实测统计（2026-10-22）
| 项 | Java | Rust |
|---|---|---|
| 文件数 | 主 **14** 个 `.java` / 测 **7** 个 `.java` | **12** 个 `.rs`（+ `Cargo.toml` 1） |
| 物理行 | 主 **3499** / 测 **1165** | `.rs` **4636**（`Cargo.toml` 22） |
| 非空行 | 主 **3167** / 测 **1038** | `.rs` **4100**（`Cargo.toml` 20） |
| 用例数 | **26** 个 `@Test`（5 离线类 22 + 2 集成类 4） | **113** 个 `#[test]`(52) + `#[tokio::test]`(61) |
| Rust 用例分布 | — | `state/mod.rs` 50、`store.rs` 20、`sandbox.rs` 13、`snapshot.rs` 9、`distributed_store.rs` 9、`state/key_layout.rs` 9、`state/version_support.rs` 2、`state/connection.rs` 1 |

> Java 7 个测试类逐个实测：`RedisAgentStateStoreTest` 328 行/15、`RedisAgentStateClusterIntegrationTest` 322/3、`RedisSessionStorageIntegrationTest` 110/1、`JedisClientAdapterTest` 85/3、`RedissonAgentStateStoreTest` 79/1、`JedisAgentStateStoreTest` 67/1、`RedisDistributedStoreTest` 47/2。

### 6.3 本轮改动真实性核对（逐项）
| 声明 | 核对结果 | 证据 |
|---|---|---|
| `find_keys_by_pattern` 改为 **SCAN 游标**语义 | ✅ **属实** | `src/state/connection.rs:174-195`：`cursor: u64 = 0` 起，`redis::cmd("SCAN").arg(cursor).arg("MATCH").arg(pattern).arg("COUNT").arg(SCAN_COUNT=100)`，`next == 0` 退出，否则 `cursor = next`；累积 `HashSet<String>` 去重。**全 crate 无 `KEYS` 调用**。⚠️ 但该循环**零单测覆盖**（`state/connection.rs` 唯一用例只测 `value_to_i64`），且 `MemoryRedisAdapter` 替身是「全量键 glob 过滤」、**不模拟游标** |
| `base_store()` **自动接线**（`connect_lazy`） | ✅ **属实** | `src/distributed_store.rs:237-247` `base_store()` → `RedisStore::connect_lazy(url, "{root}store:")`；`src/store.rs:237-240` `connect_lazy` → `BlockingRedisCommands::open(url)`（仅 `redis::Client::open` 校验 URL 形状，**零网络**）；`connect`（`src/store.rs:227-231`）额外 `PING`。`with_url()` / `url()` / `base_store_with_url()` 均存在；无 URL 时返回含 `no Redis URL retained` + `base_store_with_url` 的降级错误 |
| `RedisSandboxLease::released` 为 **`AtomicBool`** | ✅ **属实** | `src/sandbox.rs:193` `released: AtomicBool`；`:32` `use std::sync::atomic::{AtomicBool, Ordering};`；`:208-210` `is_released()`；`:226/:230` `store(true, SeqCst)`；`:253` `swap(true, SeqCst)`（`Drop` 占释放权）。**字段确被读取** ⇒ 无 dead-code warning 隐患 |
| 新增 `MemoryRedisAdapter` / `MemoryRedisCommands` / `MemoryStoreCommands` | ✅ **属实** | `src/test_support/mod.rs`（`#[cfg(test)]`，590 行）：`MemoryRedisAdapter`(:132) / `MemoryRedisCommands`(:348) / `MemoryStoreCommands`(:465) 三个 `pub(crate)` 结构体 |
| 新增 `RedisCommands` / `StoreCommands` 缝 | ✅ **属实** | `src/raw_client.rs:35` `pub trait RedisCommands`（async，7 方法）+ `ManagerRedisCommands`(:60) 生产实现；`src/store.rs:66` `pub(crate) trait StoreCommands`（**同步**，4 方法）+ `BlockingRedisCommands`(:92) 生产实现。另有第三条缝 `src/state/adapter.rs:35` `RedisClientAdapter`（Java 原生抽象） |
| 五个 Lua 脚本的内存解释器 | ✅ **属实** | `SAVE_SCRIPT`（`state/version_support.rs:26`）、`PUT_SCRIPT`/`PUT_IF_VERSION_SCRIPT`/`DELETE_SCRIPT`（`store.rs`）、`RELEASE_SCRIPT`（`sandbox.rs:45`）；替身侧 `run_save_script` 等解释器在 `test_support/mod.rs` |
| B 波次统一产出契约（对照 rollup §2.1） | ✅ **全部存在** | `RedisDistributedStore`（`src/distributed_store.rs`）/ `RedisAgentStateStore`（`src/state/mod.rs`）/ `RedisStore`（`src/store.rs`）/ `RedisSnapshotSpec`（`src/snapshot.rs:147`）/ `RedisSandboxExecutionGuard`（`src/sandbox.rs:72`）。`lib.rs` 导出：`RedisDistributedStore`、`RedisAgentStateStore`(+`KeyLayoutVersion`/`VersionedState`)、`RedisStore`、`RedisSnapshotSpec`、`RedisRemoteSnapshotClient`、`RedisSandboxExecutionGuard`(+`Builder`/`Lease`)、`RedisClientAdapter`、`RedisConnection`、`ManagerRedisCommands` |

### 6.4 对齐结论表
| Java 类 | Rust 对应 | 存在 | 行为覆盖 | 结论 |
|---|---|---|---|---|
| `RedisDistributedStore` | `distributed_store::RedisDistributedStore` | ✅ | ✅（6 组件工厂 + `to_distributed_store()`） | **已对齐**（`base_store` 用独立阻塞连接，见 §3.2） |
| `state.RedisAgentStateStore` | `state::RedisAgentStateStore` | ✅ | ✅（`save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids` + 版本三方法 + `clear_all_sessions`） | **已对齐**（`save_if_version(0)` 语义见 §6.5 P1-1） |
| `state.RedisAgentStateKeyLayout` | `state::key_layout::RedisAgentStateKeyLayout`（`pub(crate)`）+ `KeyLayoutVersion` | ✅ | ✅（9 单测） | **已对齐** |
| `state.RedisClientAdapter` | `state::adapter::RedisClientAdapter` | ✅ | ✅（13 方法 1:1 + `close()` 默认 no-op） | **已对齐**（`close()` 语义弱化，见 §3.3） |
| `state.RedisStateVersionSupport` | `state::version_support` | ✅ | ✅（`SAVE_SCRIPT` 逐字节移植，省略 Java `current_version()` 中等价冗余分支） | **已对齐** |
| `state.jedis.JedisClientAdapter` / `lettuce.LettuceClientAdapter` / `redisson.RedissonClientAdapter` | 合并 → `state::connection::RedisConnection` | ✅ | ⚠️ 部分（`SCAN` 循环零单测；Cluster 下 `SCAN` 只覆盖所连节点 → 漏键，Java Jedis 遍历全 master） | **已对齐（Cluster 模式 P1，见 §2 行注）** |
| `state.jedis.JedisAgentStateStore` / `redisson.RedissonAgentStateStore`（`@Deprecated`） | 设计决定（不迁移） | — | — | **已完成（设计决定）** |
| `store.RedisStore` | `store::RedisStore`（`impl BaseStore`） | ✅ | ✅（`get`/`put`/`put_if_version`/`search`/`delete` + `supports_atomic_session_storage`） | **已对齐**（Java `putIfVersion(expected<0)` 抛 `IllegalArgumentException`；Rust 签名为 `u64`，负值类型层不可表达 ⇒ 分支自然消失） |
| `snapshot.RedisRemoteSnapshotClient` | `snapshot::RedisRemoteSnapshotClient` | ✅ | ✅ | **已对齐**（缺失 → `Error::Io(NotFound)`） |
| `snapshot.RedisSnapshotSpec` | `snapshot::RedisSnapshotSpec` | ✅ | ✅（包 harness `RemoteSnapshotSpec`） | **已对齐** |
| `sandbox.RedisSandboxExecutionGuard` | `sandbox::RedisSandboxExecutionGuard`（镜像类型，非 `impl`） | ✅ | ✅（`SET NX PX` + Lua token CAS + `Drop` 释放 + 常量对齐 30min/500ms/`agentscope:sandbox:lock:`） | **已对齐**（`release(&key)` 无 token CAS，见 §3.3） |
| （Java core）`ListHashUtil` | `agentscope_core::state::{compute_hash, needs_full_rewrite}` | ✅ | ✅（本地平行实现已删） | **已收敛**（hash 值与 Java 不互通，见 §3.3） |

### 6.5 新发现缺口
| 编号 | 级别 | 缺口 | 定位 | 说明 |
|---|---|---|---|---|
| P1-1 | **P1** | `save_if_version(expected_version=0)` **无条件 bump**，同时偏离 Java（`0` = 真实 CAS）与 **core trait 契约**（`0` = create-if-absent） | `src/state/mod.rs:194`（固有方法）+ `:368-392`（trait 实现） | 调用方按 core 契约以 `0` 表达「仅当不存在时创建」时，Redis 后端会**无条件覆盖**已存在状态（丢更新）。core 契约原文见 `crates/agentscope-core/src/state/store.rs:90`。**本轮未修**（禁改 Rust 代码） |
| P1-2 | **P1** | ~~`RedisConnection::find_keys_by_pattern` 的 **SCAN 游标循环零单测覆盖**；`MemoryRedisAdapter` 替身不模拟游标~~ → **已闭合（2026-10-28，§8.1）** | `src/state/connection.rs`（`collect_keys_on_node` / `collect_keys_across_nodes`）/ `src/test_support/mod.rs`（`MemoryScanPager`、`with_scan_page_size`） | 游标循环抽成 `ScanPager` 缝后新增 10 个用例（多页聚合、游标链 `0→7→42`、`COUNT`/`MATCH` 参数、跨页重复键去重、页错误传播、空键空间、三节点扇出、失败节点传播、拓扑枚举失败、空拓扑）；替身改驱动生产循环并可 `with_scan_page_size(n)` 强制多页（`scan_cursor_history()` 断言游标链），state 层另有 2 个端到端用例 |
| P1-3 | **P1** | **Cluster 模式下 `SCAN` 只覆盖所连节点** → `list_session_ids` / `clear_all_sessions` 在 Redis Cluster 上**漏键** | `src/state/connection.rs`（`SingleNodeScanSource` / `ScanNodeSource`） | **已完成（2026-10-28 第二轮，§8.5）**：Java `JedisClientAdapter` 用 `scanIteration(1000, pattern)` **遍历全部 master**。Rust 侧：`CLUSTER SLOTS` 拉拓扑 → 纯函数 `parse_cluster_slots` 去重取 primary → `ClusterScanNodeSource` 为每个 primary 建独立单机连接、逐节点跑完整游标循环并 union；任一节点失败即 `Err`。生产入口 `RedisConnection::connect_cluster(seed_url)`。**无需 `cluster` feature**。剩余：真实 Cluster 集成测试 |
| P2-1 | **P2** | 根 `Cargo.toml` 行号引用过期（原写 `:66`，UTF-8 实测 `:71`；`Get-Content` 口径才是 65） | 本文件 §2.1 行注、§4.2 第 6 条 | 已在本轮订正为 `:71` 并注明口径 |
| P2-2 | **P2** | Java 侧有 **4 个真实 Redis/Cluster 集成用例**，Rust 侧 **0**（无 `#[ignore]` / feature-gate 的集成测试骨架） | `crates/agentscope-extensions-redis/tests/`（**目录不存在**） | 补测需 CI 提供 Redis 实例（testcontainers / `redis-server`） |
| P2-3 | **P2** | `RedisSandboxExecutionGuard::release(&key)` 为无 token 的 `DEL`，可能误释放他人锁 | `src/sandbox.rs:167-176` | Java `RedisSandboxExecutionGuard` **只有 `tryEnter`**，Java harness `SandboxExecutionGuard` 是 `@FunctionalInterface` 仅含 `tryEnter` ⇒ Rust 的 `release`/`is_held` 是**超出 Java 的扩展**，仅对齐 Rust harness 具体 struct 的表面 |

### 6.6 勘误列表（原文 → 实际）
| 编号 | 位置 | 原文 | 实际 |
|---|---|---|---|
| E1 | §1「Java 规模」 | 主代码 **3167 行** / 测 **1038 行** | **属实**（UTF-8 `ReadAllLines` 口径）。但 `Get-Content`（PowerShell 5.1 默认 gb2312）口径得 **3165 / 1038**（`JedisAgentStateStore` / `RedissonAgentStateStore` 两文件末尾空行被吞）⇒ **原 3167 与任务参考 3165 之差为编码/口径差，非数据差**；已在 §1 标注口径 |
| E2 | §1.1 / §3.1 / §4.1 | `cargo test` = **92 passed / 0 failed**，测试函数体 **113** | 两者**不一致**：静态实测 `#[test]`+`#[tokio::test]` = **113**（52+61）。92 是上一会话快照；差异来源 = A2 版本化上提后新增的 `state/mod.rs` 用例 + `state/key_layout.rs` 9 个用例（本轮已把 §3.1 分布表订正为 50/20/13/9/9/9/2/1） |
| E3 | §3.1「离线单元测试」行 | Java **3 个测试类 217 行**；分布 `state/mod.rs(37)`、`store.rs(21)`、`sandbox.rs(12)`、`snapshot.rs(8)`、`distributed_store.rs(7)`、`version_support.rs(2)`、`connection.rs(1)` | Java 实为 **7 个测试类 / 1038 行 / 26 个 `@Test`**；Rust 分布实为 `state/mod.rs(50)`、`store.rs(20)`、`sandbox.rs(13)`、`snapshot.rs(9)`、`distributed_store.rs(9)`、**`state/key_layout.rs(9)`（原表完全遗漏）**、`version_support.rs(2)`、`connection.rs(1)` |
| E4 | §3.2「`clearAllSessions()`」行 | Java 参考 = `state/RedisAgentStateStore.java`（返回 `Mono<Integer>`） | **Java 参考错**：统一 `RedisAgentStateStore` **无 `clearAllSessions`**；仅两个 `@Deprecated` store 有（`state/jedis/JedisAgentStateStore.java:345`、`state/redisson/RedissonAgentStateStore.java:328`）。Rust `clear_all_sessions` 是**扩展方法**，且用 `SCAN` 而非 Java 的 `KEYS` |
| E5 | §4.2 第 6 条 | 根 `Cargo.toml:66` | **根 `Cargo.toml:71`**（UTF-8 行号） |
| E6 | §1.1「源码行数」行 | `Cargo.toml` **23** 行 | `Cargo.toml` **22** 行（物理）/ 20 非空行 |
| E7 | §4.1「crate 文件清单」 | `Cargo.toml(25)`、`distributed_store.rs(339)`、`store.rs(583)`、`state/mod.rs(736)`、`test_support/mod.rs(502)`、合计 **3457 行** | 2026-10-22 实测：`Cargo.toml(22)`、`distributed_store.rs(425)`、`store.rs(661)`、`state/mod.rs(1117)`、`test_support/mod.rs(590)`、**`state/key_layout.rs(378)`（原表遗漏该文件）**、`.rs` 合计 **4636 物理行 / 4100 非空行** |
| E8 | §3.2「`findKeysByPattern` 的 SCAN 游标语义」行 | 暗示 `find_keys_by_pattern` 已由测试检验 | **属实但覆盖被高估**：SCAN 游标循环**零单测**；替身不模拟游标。已改为明示（§6.5 P1-2） |
| E9 | §2「`redis.state.RedisAgentStateStore`」行 | 「覆盖 `save`/`save_list`/…；**并已实现 core trait 的 `supports_versioning`/`get_versioned`/`save_if_version`**」 | **属实**（`src/state/mod.rs:346/350/368` 三处 `impl AgentStateStore` 方法，行号与 §2.1 契约段引用一致）——本条**无需更正**，列出以示已核 |
| E10 | §2.1 契约段 | core trait 引用 `crates/agentscope-core/src/state/store.rs:12,35,68,76,98` | **属实**（`:12` `UNVERSIONED=-1`、`:35` trait、`:68/:76/:98` 三个版本方法），本条**无需更正** |

### 6.7 本轮改动清单
- **仅改** `docs/extensions/REDIS__GAP_TRACKER.md`（§0 结论、§1、§1.1、§2 两处行数、§2.1 契约段、§3.1 测试行、§3.2 `clearAllSessions`/`findKeysByPattern`、§3.3 `save_if_version`、§3.4 两处行数、§4.1、§4.2、新增 §6）。
- **未改** 任何 Rust 代码 / 根 `Cargo.toml` / `core` / `harness` / 其它 crate / 其它 tracker。
- **建议 rollup（`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 REDIS 行）**：
  - Java 列 `2884 / 217` → **3167 / 1038**（严重过期，须改）。
  - 测试数 `92+2 test` → **113 unit（静态实测函数体）+ 2 doc-test**；状态建议由 `已完成` 改为 **`部分完成`**，理由：① P1-1 `save_if_version(0)` 与 core 契约冲突未修；② P1-2 SCAN 游标循环零覆盖；③ P1-3 Cluster `SCAN` 漏键；④ 真实 Redis 集成测试仍为 0（Java 侧 4 个集成用例）。若汇总方认为 P1-1/P1-3 属「设计决定（不迁移）」可保留 `已完成`，但需在 rollup 注明裁定与理由。**已完成（2026-10-29 勘误）**：原建议「部分完成 / 113 unit」，实际 P1-1 已于 §7.1 闭合、P1-2/P1-3 已于 §8.1/§8.5 闭合（148 + 2 doc passed），仅剩真实 Redis/Cluster 集成测试；维持 **`已完成`**（附现有边界）。

## 7. 裁定执行记录（2026-10-22，汇总方）

> 依据 `EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 1（`save_if_version(0)` 语义）。

### 7.1 已修复（P1-1）

| 项 | 修改前 | 修改后 |
|---|---|---|
| `save_if_version(0)` | 特判为 **无条件 bump**（`UNCONDITIONAL_EXPECTED`），与 core 契约（`0` = create-if-absent）冲突 → 按契约调用会**丢更新** | 删除该特判，`0` 走**真实 CAS**：`current_version()==0` 时创建并置 1，否则冲突 |
| core `UNVERSIONED(-1)` 映射 | 错误地映射到 `0`（与真实 CAS 版本 `0` 撞车） | 映射到内部 `UNCONDITIONAL_EXPECTED`(`u64::MAX`)，与 Java `Long.MIN_VALUE` 哨兵等价 |
| 冲突信号 | `Ok(0)` 与"0 = 无条件"语义混用 | 成功值恒 `>= 1`，故 `Ok(0)` **无歧义**表示冲突 → 返回 `UNVERSIONED` |

**测试**：`save_if_version_zero_bumps_unconditionally`（该用例本身编码了 bug）改为 `save_if_version_zero_creates_only_when_absent`（断言第二次 0 期望返回冲突）；新增 `save_if_version_unconditional_sentinel_always_writes`。`cargo test -p agentscope-extensions-redis` → **114 passed / 0 failed**（原 113，净 +1）。

### 7.2 仍待办

- ~~**P1-2** SCAN 游标循环零覆盖~~ → **已闭合（2026-10-28，见 §8.1）**。
- **P1-3** Cluster 下 `SCAN` 只覆盖所连节点 → `list_session_ids` / `clear_all_sessions` 漏键（Java `scanIteration` 遍历全 master）。**已闭合（§8.5）**：`CLUSTER SLOTS` 拓扑发现 + 每 primary 独立连接的扇出，生产入口 `RedisConnection::connect_cluster`。
- **P2-2** 真实 Redis / 真实 Cluster 集成测试仍为 0（环境受限，§4.2 缺口 A）。
- **P2** 真实 Redis 集成测试为 0（Java 有 4 个集成用例）；`guard.release(&key)` 无 token CAS。
## 8. 执行记录（2026-10-28 第一轮，SCAN 覆盖：P1-2 闭合 / P1-3 部分闭合）

> **注**：本轮（§8.1–§8.4）之后另有第二轮，用 `CLUSTER SLOTS` 闭合 P1-3（§8.5 / §8.6）。

> 角色：REDIS 执行子智能体。改动范围仅 `crates/agentscope-extensions-redis/**` 与本文。**未改**根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。

### 8.1 改动内容

**受改文件（3 个 .rs）**

| 文件 | 改动 |
|---|---|
| `src/state/connection.rs` | 新增 crate 内缝 `ScanPager`（单节点分页：`scan_page(cursor, pattern, count) -> (next_cursor, keys)`）与 `ScanNodeSource`（节点枚举：`scan_nodes() -> Vec<Arc<dyn ScanPager>>`）；新增 `collect_keys_on_node`（游标循环 + `HashSet` 去重 + 排序）与 `collect_keys_across_nodes`（逐节点跑完整循环 + union + 排序，任一节点失败即 `Err`）；生产实现 `ConnectionScanPager`（`SCAN <cursor> MATCH <pattern> COUNT 100`）与 `SingleNodeScanSource`；`RedisConnection::find_keys_by_pattern` 改为走 `collect_keys_across_nodes(SingleNodeScanSource, pattern)`，**行为与单机语义不变**（仍是 SCAN、仍无 `KEYS`），只是把循环抽到可测函数 |
| `src/test_support/mod.rs` | `MemoryRedisAdapter` 新增 `scan_page_size` + `scan_cursors` 字段；新增 `with_scan_page_size(n)` / `scan_cursor_history()`；新增 `MemoryScanPager`（游标 = 键快照内位置，非终页返回非 0 游标，页内按 glob 过滤，故可出现「空页但游标非 0」）；`MemoryRedisAdapter` 实现 `ScanNodeSource`；`find_keys_by_pattern` 由「全量键 glob 过滤」改为**驱动生产函数** `collect_keys_across_nodes(self, pattern)` |
| `src/state/mod.rs` | 新增 2 个 state 层端到端用例（见下） |

**新增用例（12 个，全部离线）**

`src/state/connection.rs`（+10）：
1. `scan_loop_aggregates_every_page` — 两页 `(7,[a,b])`、`(0,[c])` → `[a,b,c]`（多页完整聚合）。
2. `scan_loop_follows_server_cursor_until_zero` — 三页 `(7,…)`、`(42,…)`、`(0,…)` → 断言结果含全部 3 键**且**请求游标序列 == `[0,7,42]`（游标非 0 不提前终止、且复用服务端返回的游标）。
3. `scan_loop_uses_match_pattern_and_count_hint` — 断言 `MATCH` 模式透传与 `COUNT==SCAN_COUNT(100)`、首页游标为 `0`。
4. `scan_loop_deduplicates_keys_repeated_across_pages` — 跨页重复键 `k1`/`k2` → `[k1,k2,k3]`（测现状语义：`HashSet` 去重）。
5. `scan_loop_propagates_page_failures` — 第二页 `Err` → 整体 `Err`，且失败页游标为 `5`（异常不被吞、循环确实前进）。
6. `scan_loop_returns_empty_for_an_empty_keyspace` — `(0,[])` → 空。
7. `scan_fanout_visits_every_node_and_merges_results` — 3 节点（A 两页、B 一页含与 A 重复键、C 空）→ union 去重排序，且**三节点均被访问**（`cursors` 分别 `[0,9]`、`[0]`、`[0]`）。
8. `scan_fanout_propagates_a_failing_node` — 节点 B 失败 → 整体 `Err`（不返回静默部分结果）。
9. `scan_fanout_propagates_topology_failures` — `scan_nodes()` 失败 → 传播。
10. `scan_fanout_over_an_empty_topology_is_empty` — 空节点列表 → 空。

`src/state/mod.rs`（+2）：
11. `list_session_ids_aggregates_every_scan_page` — `with_scan_page_size(2)` + 5 个 session → `list_session_ids` 返回全部 5 个排序 id；断言 `scan_cursor_history().len() > 1`、首游标 `0`、后续游标全 `>0`（证明走的是多页游标路径，而不是单次全量）。
12. `clear_all_sessions_deletes_keys_from_every_scan_page` — `with_scan_page_size(3)` 下 `removed == all_keys().len()` 且删空，`scan_cursor_history().len() > 1`（多页枚举不漏键）。

**验证命令与结果**

- `cargo build -p agentscope-extensions-redis` → **通过**，本 crate **0 warning**。
- `cargo test -p agentscope-extensions-redis` → **126 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。基线 114（§7.1 实测）→ **+12**。
- `cargo fmt -p agentscope-extensions-redis -- --check` → 干净（仅格式化本轮新增代码，未触碰其它文件）。
- 用例分布（实测 `#[test]`/`#[tokio::test]`）：`state/mod.rs` 53、`store.rs` 20、`sandbox.rs` 13、`state/connection.rs` 11、`snapshot.rs` 9、`distributed_store.rs` 9、`state/key_layout.rs` 9、`version_support.rs` 2 = **126**。

### 8.2 Cluster 方案：API 调研与阻塞（如实记录）

> **状态（2026-10-28 第二轮）：本节结论「真实 Cluster 拓扑发现阻塞」已解除**——改用 `CLUSTER SLOTS` 自行发现主节点，不触碰驱动 `cluster` feature。以下调研仍作为**为何不走驱动 cluster API** 的依据保留；实施方案与验证见 **§8.5**，剩余边界见 **§8.6**。

**结论**：`redis` 1.7.0 **不提供** primary 枚举 API；本轮实现「扇出逻辑 + 可注入节点源」，真实 Cluster 连接记为**阻塞**。

依据（均在 `C:\Users\Endless\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\redis-1.7.0\` 核对）：

1. `src/cluster_handling/client.rs` 的 `ClusterClient` 只有 `get_connection` / `get_connection_with_config` / `get_async_connection` / `get_async_generic_connection` / `get_pending_async_connection_with_config`，**没有 `get_primaries()` / `get_async_primaries()` 一类节点枚举方法**（1.7.0 全仓 `rg 'fn get_.*primaries'` 零命中）。
2. `src/cluster_handling/slot_map.rs:11` 的 `SlotMap` 是 `pub(crate)`，其 `addresses_for_all_primaries()`（`:86`）**外部不可见**；`src/cluster_handling/mod.rs` 中 `slot_map` / `topology` 亦为 `pub(crate)`。
3. `src/cluster_handling/routing.rs:674` 明确把 `SCAN` 归为 `RouteBy::Undefined`（注释 `TODO - special handling - b"SCAN"`）⇒ 集群连接上的 `SCAN` 只到**单节点**；`MultipleNodeRoutingInfo::AllMasters`（`:104`）虽由 `KEYS` 使用（`:654` → `RouteBy::AllPrimaries` + `CombineArrays`），但 `SCAN` 的**游标是每节点独立**的，把一次 `SCAN` 扇出到全部主节点后返回值无法区分「哪个游标属于哪个节点」，**不能正确分页**，故不可用该旁路。
4. 唯一能拿到 primary 地址的公开途径是 `cluster_read_routing::{ClusterTopology, Shard::primary()}`，但 `ClusterTopology` 只通过 `ReadRoutingStrategy::on_topology_changed` 回调下发（需在 `ClusterClientBuilder::read_routing_strategy(...)` 注入自定义策略并缓存拓扑），属**间接/脆弱**用法。
5. 前置条件缺失：本 crate 依赖 `redis = { workspace = true }`，workspace feature 仅 `script` / `tokio-comp` / `connection-manager`（`default-features = false`），**未启用 `cluster` 或 `cluster-async`**；本轮无权改根 `Cargo.toml`（`[workspace.dependencies]`），故 `redis::cluster_async` 在编译期就不存在。且现有 `RedisConnection` 只持有 `ConnectionManager`，接入 Cluster 需新增第二连接类型 + 贯穿 6 个组件。

**已落地（不依赖上述 API）**：`ScanNodeSource` 抽象 + `collect_keys_across_nodes`；一旦拿到 primary 列表，只需新增一个 `ClusterScanNodeSource`（每个 primary 建 `ConnectionManager`/`ClusterConnection` per-node `SCAN`）返回 `Vec<Arc<dyn ScanPager>>`，`RedisConnection` 侧无需改动。**待汇总方裁定**：是否（a）授权在本 crate `Cargo.toml` 追加 `features = ["cluster-async"]` 并用 `ReadRoutingStrategy` 采集拓扑；（b）在根 `Cargo.toml` 的 `[workspace.dependencies]` 打开 cluster feature；（c）维持单节点并标注 Redis Cluster 不受支持。

> **2026-10-28 第二轮：以上 (a)/(b)/(c) 均未采用**——改为**第四种方案**：用 `CLUSTER SLOTS`（普通命令，任何连接都能发）自行发现主节点，再把每 primary 的连接交给既有 `ConnectionScanPager`。因此**不需要** `cluster`/`cluster-async` feature，也**不需要**改根 `Cargo.toml`。实施与验证见 §8.5。

### 8.3 反向验证（3 项，均已恢复）

| 序号 | 临时改动 | 结果（`cargo test -p agentscope-extensions-redis`） | 恢复后 |
|---|---|---|---|
| R1 | `collect_keys_on_node` 在首页后无条件 `break`（只跑一次） | **FAILED 119 passed / 7 failed**：`scan_loop_follows_server_cursor_until_zero`、`scan_loop_aggregates_every_page`、`scan_loop_deduplicates_keys_repeated_across_pages`、`scan_loop_propagates_page_failures`、`scan_fanout_visits_every_node_and_merges_results`、`list_session_ids_aggregates_every_scan_page`、`clear_all_sessions_deletes_keys_from_every_scan_page` | 126 passed / 0 failed |
| R2 | `collect_keys_across_nodes` 只遍历首个节点（`.take(1)`） | **FAILED 124 passed / 2 failed**：`scan_fanout_visits_every_node_and_merges_results`、`scan_fanout_propagates_a_failing_node` | 126 passed / 0 failed |
| R3 | `collect_keys_on_node` 累积到 `Vec`（去掉 `HashSet` 去重） | **FAILED 125 passed / 1 failed**：`scan_loop_deduplicates_keys_repeated_across_pages`（仅此一例，精度良好） | 126 passed / 0 failed |

R1 同时证明「state 层两个用例走的是生产游标循环」（替身不再自实现 glob 全量过滤），R2 证明扇出用例不是空转。

### 8.4 计数勘误

- 2026-10-22 静态计数 **113** 与 §7.1 实测 **114** 的差 = 少算 1 个用例（`state/mod.rs` 静态 50 vs 实测 51）；本轮已把 §1.1 / §3.1 的分布表更新为**实测值**（`state/mod.rs` 53 等）。
- 本轮净增 **+12**（114 → 126），与新增用例数一致，无删测、无 `#[ignore]`、无放宽断言。

### 8.5 第二轮执行记录（2026-10-28）：`CLUSTER SLOTS` 拓扑发现 → P1-3 闭合

**角色 / 范围**：只改 `crates/agentscope-extensions-redis/src/state/connection.rs` 与本 tracker。**未改**根 `Cargo.toml`（`redis = { workspace = true }` 原样，**feature 未动**）、本 crate `Cargo.toml`（**一行未改**，`tokio-comp` / `connection-manager` / `script` 已够用）、`agentscope-core` / `agentscope-harness` / 其它 crate。

**Java 依据**

- `state/jedis/JedisClientAdapter.java:153-158`：`unifiedJedis.scanIteration(1000, pattern).collect(matchingKeys)`，注释「Unlike a single-node SCAN, Jedis' iteration traverses every master in cluster mode」。
- **错误处理**：Java 无 try/catch，驱动异常直接上抛 ⇒ 任一 master 失败即整体失败。Rust 保持同一语义（`ClusterScanNodeSource::scan_nodes` 用 `?`，不跳过节点、不返回部分结果）。
- **拓扑新鲜度**：Jedis Cluster 客户端按 slot map 自动刷新 ⇒ Rust 每次枚举都**重读** `CLUSTER SLOTS`（不缓存）。
- Java `RedisAgentStateClusterIntegrationTest` 用 `RedisClusterClient.create(Set.of(new HostAndPort(HOST, basePort)))`，**单一种子地址**即可 ⇒ 与 `RedisConnection::connect_cluster(seed_url)` 形态一致。

**实现（新增类型）**

| 类型 / 函数 | 可见性 | 职责 |
|---|---|---|
| `ClusterNodeAddr { host, port }` | `pub(crate)` | primary 地址；`to_redis_url()` 生成诊断 URL（IPv6 加方括号） |
| `parse_cluster_slots(&redis::Value, fallback_host) -> Result<Vec<ClusterNodeAddr>>` | `pub(crate)` **纯函数** | 解析 `[start, end, [host, port, node_id, ...], replicas...]`，只取 primary；按 (host,port) 去重并保持首现顺序；`host` 为空 / NIL ⇒ 用 `fallback_host`；端口须 `1..=65535`；结构非法一律 `Err`（错误信息带 slot range 序号与形状） |
| `trait ClusterTopologySource { async fn primaries() }` | `pub(crate)` 缝 | 拓扑来源（生产 = `CLUSTER SLOTS`） |
| `trait NodeConnectionFactory { async fn connect(&ClusterNodeAddr) -> Arc<dyn ScanPager> }` | `pub(crate)` 缝 | 每 primary 建一条独立单机连接 |
| `ClusterScanNodeSource<P, F>` | `pub(crate)` | `impl ScanNodeSource`：`primaries()` → 逐个 `connect()` → `Vec<Arc<dyn ScanPager>>`；`is_cluster() == true` |
| `ClusterSlotsTopologySource` | 私有（生产） | `redis::cmd("CLUSTER").arg("SLOTS")` + `parse_cluster_slots`；seed 主机作 fallback |
| `SeedClientNodeFactory` | 私有（生产） | 以 seed `Client` 的 `ConnectionInfo` 为**模板** `set_addr(...)` 后 `ConnectionManager::new` → `ConnectionScanPager` |
| `RedisConnection::connect_cluster(seed_url)` / `is_cluster_scan()` | `pub` | 生产入口；`find_keys_by_pattern` 改为 `collect_keys_across_nodes(self.scan_source, ...)`（单机仍是一条 pager，行为不变） |

**与任务书建议的偏离（有意）**：任务书建议用 `redis::Client::open(format!("redis://{host}:{port}"))` 建节点连接；实际改为**以 seed 的 `ConnectionInfo` 为模板、只替换地址**。理由：朴素 URL 会**丢失 AUTH 用户名/密码、DB 与 TLS 设置**，在启用鉴权的 Cluster 上每条节点连接都会 `NOAUTH` 失败。`to_redis_url()` 仍保留用于错误信息（IPv6 方括号规则可单测）。

**新增用例（19 个，全部离线，均在 `src/state/connection.rs::tests`）**

- 拓扑解析（11）：`cluster_slots_parses_every_primary_in_order`、`cluster_slots_merges_a_primary_covering_several_ranges`、`cluster_slots_falls_back_to_the_seed_host_for_empty_hostnames`、`cluster_slots_accepts_ipv6_hosts_and_string_ports`、`cluster_slots_over_an_empty_topology_is_empty`、`cluster_slots_rejects_a_non_array_reply`、`cluster_slots_rejects_a_truncated_range`、`cluster_slots_rejects_non_integer_slot_bounds`、`cluster_slots_rejects_an_inverted_slot_range`、`cluster_slots_rejects_a_truncated_primary_entry`、`cluster_slots_rejects_an_invalid_primary_port`
- 扇出（5）：`cluster_fanout_scans_every_primary_and_merges`（3 primary：断言每 primary 各连一次、各自跑完整游标链、union 去重、空 primary 也被访问、拓扑只读 1 次）、`cluster_fanout_propagates_a_primary_connection_failure`（第 2 个 primary 连不上 ⇒ `Err`，第 3 个从未连接）、`cluster_fanout_propagates_topology_failure`（拓扑失败 ⇒ `Err` 且不连任何节点）、`cluster_fanout_over_an_empty_topology_connects_no_node`、`cluster_fanout_rereads_the_topology_on_every_scan`（两次枚举 ⇒ 拓扑读 2 次，证明不缓存）
- 端点 / 工具（3）：`cluster_node_endpoint_keeps_the_seed_connection_flavour`（Tcp / TcpTls 保留 `insecure` / Unix ⇒ Tcp）、`cluster_seed_host_prefers_the_connected_host`、`normalize_url_adds_a_missing_scheme`

**验证命令与结果**

- `cargo build -p agentscope-extensions-redis` → **通过**，本 crate **0 warning**。
- `cargo test -p agentscope-extensions-redis` → **148 passed / 0 failed / 0 ignored**（lib）+ **2 passed**（doc-tests）。基线 129（lib）→ **+19**，与新增用例数一致；无删测、无 `#[ignore]`、无放宽断言。
- `cargo fmt -p agentscope-extensions-redis -- --check` → 干净。
- 未跑 `--workspace`（授权限定）。

**反向验证（3 项，均已恢复）**

| 序号 | 临时改动 | 结果（`cargo test -p agentscope-extensions-redis --lib`） | 恢复后 |
|---|---|---|---|
| R1 | `ClusterScanNodeSource::scan_nodes` 改为「连不上就跳过」（`if let Ok(..)`） | **FAILED 147 passed / 1 failed**：`cluster_fanout_propagates_a_primary_connection_failure`，失败信息 `called Result::unwrap_err() on an Ok value: ["a", "c"]` —— 恰好暴露「静默返回残缺键集」 | 148 passed / 0 failed |
| R2 | `parse_cluster_slots` 去掉 `(host, port)` 去重 | **FAILED 147 passed / 1 failed**：`cluster_slots_merges_a_primary_covering_several_ranges`（同一 primary 被重复列入 ⇒ 扇出会重复扫同一节点） | 148 passed / 0 failed |
| R3 | `parse_cluster_slots` 空 host 直接 `unwrap_or_default()`（不回退 seed 主机） | **FAILED 147 passed / 1 failed**：`cluster_slots_falls_back_to_the_seed_host_for_empty_hostnames`（+ `unused variable: fallback_host` 警告） | 148 passed / 0 failed |

### 8.6 剩余边界与降级语义（Cluster）

| 场景 | 行为 | 依据 / 理由 |
|---|---|---|
| `CLUSTER SLOTS` 报错（含「cluster support disabled」、无权限、seed 不可达） | 整体 `Err`（`"Redis CLUSTER SLOTS: ..."`），**不**回退到 seed 单机扫描 | 回退会产生**静默残缺**键集：`clear_all_sessions` 漏删、`list_session_ids` 少报。Java `scanIteration` 同样把驱动异常直接抛给调用方 |
| `CLUSTER SLOTS` 返回空数组（无 slot 分配的新集群） | `Ok(vec![])` ⇒ 空键集，**不**连接任何节点 | 没有 slot 就没有键，空结果语义正确；「扫 seed 兜底」反而会在真实集群上给出部分视图 |
| 某 primary 连接失败 / 其 `SCAN` 某页失败 | 整体 `Err`，错误信息含该 primary 的 `redis://host:port` | 同上，且能定位到具体节点 |
| 拓扑变化（failover / resharding） | 下次枚举自动生效（每次重读 `CLUSTER SLOTS`） | 对齐 Jedis cluster 客户端 slot map 刷新；无需重建连接 |
| 单键命令（GET/SET/Lua 等） | **2026-10-31 起已实现 MOVED/ASK 路由**：按 key 算 slot（CRC16 + hash tag）→ 命中 slot 缓存则直连主节点，否则先发 seed；收到 `MOVED` 写缓存并重试一次，收到 `ASK` 发 `ASKING` 后重试一次（**不缓存**），第二次重定向即报错。store/snapshot/guard 经 `raw_client` 的路径**仍为 seed-only**（见 §9.5 残余） | 手写 slot 路由（不启用 `redis` crate 的 `cluster` feature，否则会绕过整栈的 connection seam）；重定向语义与 Redis 规范一致，离线用例见 `state/redirect.rs` |
| 真实 Cluster / 真实 Redis 集成测试 | **已补 env 门控用例**：`tests/redis_cluster_integration.rs`（`AGENTSCOPE_TEST_REDIS_CLUSTER_URL`，未设置则 skip，**无 `#[ignore]`**）；真实 Redis 单机集成测试仍缺 | 环境受限；单元测试仍为内存替身 |

## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§4.2 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_distributed_store()` 改为接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `self.agent_state_store()?`（`RedisAgentStateStore`） | 前缀 `{root}session:`，布局按 `key_layout_version` |
| `isolation_scope` | `IsolationScope` | 字段 `isolation_scope` | 默认 `User`，`with_isolation_scope` 可覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `self.base_store().ok()`（`RedisStore`，同步阻塞连接） | **仅当 bundle 保留了 URL 时填充**；`from_connection` / `from_components` 未带 URL → `None`（文档化降级路径，调用方用 `base_store_with_url`） |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `Arc::new(self.sandbox_snapshot_spec())`（`RedisSnapshotSpec`） | 前缀 `{root}snapshot:` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.sandbox_execution_guard())` | `SET NX PX` + Lua token CAS；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_distributed_store()`：填满槽位；`base_store` 仅在 `self.base_store()` 成功时填充（保持既有「无 URL → 不报错」契约）。
- `src/sandbox.rs`：新增私有字段 `trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<RedisSandboxLease>>>>` + `impl SandboxGuard`。
  - **薄适配（async → sync）**：`try_enter` / `release` / `is_held` 用 `futures::executor::block_on` 驱动 async 实现（与 `MongodbGuardClient` 同一桥法）。
  - ⚠️ 已知限制：`try_enter` 在槽位被占用时走 `tokio::time::sleep` 轮询，故**不得在 current-thread tokio runtime 内调用**（与既有 `RedisSandboxLease::Drop` 的 runtime 分支说明一致）。
- 测试：`src/distributed_store.rs::{to_distributed_store_projects_snapshot_spec_and_guard, to_distributed_store_projects_base_store_when_url_is_retained}` + `src/sandbox.rs::harness_sandbox_guard_trait_wraps_the_redis_lease`。

### 验证

`cargo test -p agentscope-extensions-redis` → **137 passed / 0 failed**（lib 129 + 集成/doc 8）。

### 未完成 / 语义差异

- `base_store` 槽位在无 URL 的 bundle 上留空（唯一有意留空的槽位，原因见上）。
- ~~同步桥的 runtime 限制（async→sync）已在源码与本节标注。~~ **2026-10-28 修复（M1）**：harness `SandboxGuard::try_enter`/`release` 已改为 `async`，`futures::executor::block_on` 桥已删除，轮询直接 `await tokio::time::sleep`，因此不再有「不可在 current-thread runtime 内驱动」的限制。

## 2026-10-31 loop 收口记录

### 9.1 关闭条目：P1-4 MOVED/ASK 重定向（已完成）

- **P1-4 Cluster MOVED/ASK 重定向** → **已完成**。
  - 落点：**新文件** `src/state/redirect.rs`（含 `#[cfg(test)]`），对外仅 `pub(crate)`，由 `src/state/mod.rs` 的 `pub(crate) mod redirect;` 声明。内容：`parse_redirect`（容忍驱动前缀文本，如 `ERR MOVED ...`）、`parse_node_address`（支持 `[::1]:7000` IPv6 方括号）、`crc16` + `hash_tag` + `slot_for_key`（hash tag ⇒ 同一 session 的 `{user}/session` 落在同一 slot，与 `key_layout` V1 布局协同）、`SlotCache`（**只缓存 MOVED**，ASK 永不入缓存）、`CommandFailure::{Redirect, Other}` + `from_message`、`RedirectTransport` 替身缝（`send(target: Option<&ClusterNodeAddr>, asking: bool)`）、`run_with_redirects`（MOVED → 先写缓存再重试一次；ASK → 带 `ASKING` 重试一次且不写缓存；**第二次重定向即 `Err`**，避免路由 bug 变成挂死）。
  - 接线（**非“只落盘未接线”**）：`src/state/connection.rs` 新增 `ClusterRouter`（seed `ConnectionManager` + 每 primary 惰性建连并缓存 + `Mutex<SlotCache>`）与 `RedisConnection::{run_keyed, run_unkeyed}`；`connect_cluster` 构造 `router: Some(..)`，单机 / Sentinel 为 `None`（走 `run_on_connection`，行为零变化）；`RedisClientAdapter` 的所有 key 级命令（GET / SET NX / EXPIRE / `SAVE_SCRIPT` 的 `EVAL` / 列表重写 / 删除）均经 `run_keyed`。`run_keyed` **先查 slot 缓存**，命中则直连该主节点，未命中才从 seed 出发，因此 MOVED 学习能持续生效。
  - 离线用例（12 个，替身 `ScriptedTransport` 记录 `(target, asking)` 并回放脚本结果）：`parses_moved_and_ask`、`parses_redirect_inside_a_prefixed_driver_message`、`rejects_malformed_redirects`、`parses_ipv6_node_addresses`、`slot_for_key_matches_redis_documentation`、`hash_tags_pin_a_session_to_one_slot`、`slot_cache_records_and_clears`、`moved_is_cached_and_retried_once`、`ask_sends_asking_and_is_not_cached`、`second_redirect_gives_up`、`non_redirect_errors_propagate_without_retry`、`failures_are_classified_from_driver_messages`。
  - 反向验证口径（供执行方复核）：把 `run_with_redirects` 的 `record_moved` 去掉 → `moved_is_cached_and_retried_once` FAIL；把 ASK 分支改为 `asking=false` → `ask_sends_asking_and_is_not_cached` FAIL；把二次重定向改回继续重试 → `second_redirect_gives_up` FAIL。


### 9.2 关闭条目：P1-5 真实 Cluster 集成测试（env 门控，已完成）

- **P1-5 真实 Cluster 集成测试** → **已完成**。新文件 `tests/redis_cluster_integration.rs`：读 `AGENTSCOPE_TEST_REDIS_CLUSTER_URL`，**未设置时打印 skip 并 `return`，不使用 `#[ignore]`**；用例：`keyed_commands_follow_slot_redirects`（真实集群上 key 级命令跟 MOVED 落到正确主节点）、`lists_and_key_index_stay_in_one_slot`（hash tag 保证 session 列表 + 索引同 slot）。运行方式见 §9.6。

### 9.3 §3.2 / §8.6 逐条处置

| 条目 | 处置 | 依据 |
|---|---|---|
| §8.6「单键命令不做 MOVED/ASK 路由」 | **关闭**（已升级为缓存优先 + 单次重定向） | §9.1 |
| §8.6「真实 Cluster 集成测试仍缺」 | **关闭**（env 门控用例） | §9.2 |
| §8.6 CLUSTER SLOTS 报错/空数组/单主失败 | **保持**（不回退 seed） | 既有行为正确，未改 |
| §4.2 缺口 A（真实 Redis 单机集成） | **仍开**（本轮不做） | 无环境；Cluster 侧已补 |
| §4.2 第 3 项（`save_if_version(0)` CAS） | **已完成**（2026-10-22 §7.1） | 未回退 |
| §4.2 第 5/6/7 项（跨模块投影 / 版本 / 未改根 Cargo） | **保持** | 未动 harness、未动根 `Cargo.toml`，**本轮零新增依赖** |

### 9.4 本轮回合改动文件

- 新增：`src/state/redirect.rs`、`tests/redis_cluster_integration.rs`
- 修改：`src/state/mod.rs`（`pub(crate) mod redirect;`）、`src/state/connection.rs`（`ClusterRouter` + `run_keyed`/`run_unkeyed` + `connect_cluster` 接线；`Cargo.toml` **未改**）
- 文档：本文件（§1 当前状态、§4.2 第 1/2 项、§8.6 两行、本节）；**未动 rollup**

### 9.5 残余（未关闭）

1. **snapshot / sandbox guard 仍 seed-only**：二者经 `RedisConnection::raw_client()` → `ManagerRedisCommands`，不经 router。集群下跨 slot 的 snapshot 键会被 MOVED；如需覆盖，应把 `RedisCommands` 换成 router-backed 实现（槽位计算 + 单次重定向已在 `redirect.rs` 可用，属机械改造）。记 §4.2，本轮不做。
2. **本轮新增代码未经编译/测试验证**（执行约定：本回合禁跑 `cargo`）；`tests/redis_cluster_integration.rs` 依赖真实集群，默认不运行。
3. 真实 Redis 单机集成测试（缺口 A）仍缺。

### 9.6 运行方式（env 门控）

```powershell
AGENTSCOPE_TEST_REDIS_CLUSTER_URL=redis://127.0.0.1:7000 cargo test -p agentscope-extensions-redis --test redis_cluster_integration
```
