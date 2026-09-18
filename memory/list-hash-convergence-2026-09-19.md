# list_hash 三 crate 收敛已完成（2026-09-19，提交 daa700d）

## 结论（取代此前「待整改」记录）
MONGODB / COS / REDIS 三处平行 list_hash 实现**已全部删除，统一委托** `agentscope_core::state::list_hash`（Java `ListHashUtil` 的忠实移植）。
至此 6 个 crate 中：JDBC / PostgreSQL / MONGODB / COS / REDIS **5 个用 core**，MySQL 无此逻辑。**不再有算法并存**。

## 关键技术前提（我下发前实证，非假设）
- **`serde_json::Value: Hash` 成立** —— 这是能否收敛的决定性前提（我一度怀疑 `Number` 含 `f64` 而 `f64: !Hash`）。
  实证：`serde_json-1.0.151/src/value/mod.rs:115` `#[derive(Clone, Eq, PartialEq, Hash)] enum Value`；
  `number.rs:21` 枚举 derive Hash + `number.rs:53` 手写 `impl Hash for N`（因 `f64` 需特殊处理）。
- MONGODB 原注释称「hash 必须跨进程稳定，故用 SHA-256」**理由不成立**：
  `DefaultHasher::new()` 用**固定常量种子**（SipHasher13 keys=(0,0)），本就跨进程稳定。

## 修复的两处真实 Java 行为偏离（这是本次的实质价值）
1. **MONGODB 与 COS** 的 `needs_full_rewrite` 在 `stored_hash == None && existing_count == 0` 时返 `true`，**与 Java 相反**。
   Java 经 Case 3：空前缀 → `computeHash(空) = "empty:0"` → `hasChanged(·, null) == false` ⇒ 返 **false**。
   ⚠️ **MONGODB 的测试断言本身即错**（`assert!(needs_full_rewrite(&[json!(1)], None, 0))` 期望 true），
   我上一轮 tracker 还写成「MONGODB 语义与 Java 一致，仅算法偏离」——**该表述也是错的，已一并更正**。
   首次写入不靠这条判定建文档：MONGODB 追加分支带 `upsert: true`，COS 有 `values.len() != existing_count` 兜底，均安全。
2. **COS** 另用 `!whole_hash.starts_with(stored)` 判追加。**实测全量 SHA-256 无前缀性质**
   （`sha256([a,b])`=cf6ab613…，`sha256([a,b,c])`=3d10f2e9…，互不为前缀）⇒ **纯追加恒判需全量重写**（增量优化完全失效；数据无损）。
   改用 core 的「对 `existing_count` 前缀哈希做**相等**比较」后修复；测试中**保留了对旧假设的直接断言**（`old_predicate` 必真）以锁死回归。

## 三 crate 具体落法
| crate | 落法 | sha2 | 测试 |
|---|---|---|---|
| MONGODB | `list_hash.rs` 降为 `pub use agentscope_core::state::{compute_hash, needs_full_rewrite};`（**保留公开路径不动**，无损 re-export）；错误断言改名更正 + 新增 Case2 测试 | **保留**（`sandbox_guard.rs:24,280` 的 `compose_lock_id` 仍在用，已实证） | 62→61 lib（删 2 纯算法重复 + 新增 1） |
| COS | 删两个 private 函数改调 core；**额外解耦 `existing_count`**（原有代码仅在 `stored_hash.is_some()` 时才读长度，致 core **Case 2 永远不可达**）；移除 `sha2` 依赖 | **移除**（全 crate 无其他用途，已 grep 实证） | 26→31 lib（+7 新测试，含 `CountingStore` 写次数计数） |
| REDIS | 删 `src/state/list_hash.rs`，`state/mod.rs` 改调 core | 无 | 21→21（5 用例位移，删纯算法重复） |

## 子智能体越界与自查（我复核抓到）
- COS 子智能体**超出我指令**改了 `existing_count` 语义 —— 复核后**接受**：这是忠实 Java 的必要修正，否则 core Case 2 死代码。
- COS 子智能体自述**编辑 Cargo.toml 时误删 UTF-8 BOM 后已还原** —— 我已用字节级验证 `EF BB BF` 存在，与其它 crate 一致。
- COS 曾建 `old_impl_regression_probe` 一次性探针 —— 已确认清理，无残留。
- 三 crate 均无 `#[ignore]`、无残留平行实现（`rg "fn compute_hash|fn needs_full_rewrite"` 全空）。

## 验收
- 三 crate 独立复跑（我不采信回执）：MONGODB **61+1**、COS **31+2**、REDIS **21+2**，全 0 failed。
- 全量：`cargo test --workspace -j 2` → **1866 passed / 0 failed / 86 targets**（基线 1860 → **+6**，与预期吻合）。
- 那 **1 个 ignored** 查明是我上一轮 `platform_channel.rs:22` 的 ` ```ignore ` 文档示例，**早于基线即存在，非本轮引入**。

## 存量数据自愈（无迁移脚本，数据无损）
core 与旧实现 hash 值不可比 → 存量 slot 首次 `save_list` 判为需全量重写 → 覆盖后恢复正确增量语义。
已由 COS 的 `legacy_state_without_hash_self_heals` 测试覆盖。跨语言 Java/Rust 共读同一存储同理，仅多一次全量重写。

## 提交
`daa700d` — 16 files, +462/-283（含 `Cargo.lock`、4 个 tracker、harness 与 agent 记忆产物）。
