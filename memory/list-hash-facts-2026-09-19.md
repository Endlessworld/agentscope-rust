# list_hash 跨模块实测真相（2026-09-19，更正前一期 tracker 的错误结论）

## 背景
前一轮我在汇总 `EXTENSIONS_GAP_TRACKER.md` §3.3 写下「core `list_hash` **全仓从未调用**」「**唯独 MONGODB** 偏离」，本轮复核时用 `grep` 实测，**该结论是错的**。已提交更正（`5d199ef`）。

## 实测分布（Rust 侧，6 个 crate）
| crate | 实现位置 | 与 Java 关系 |
|---|---|---|
| **core** | `src/state/list_hash.rs`，**已 `pub use`**（`compute_hash`/`has_changed`/`needs_full_rewrite`/`EMPTY_HASH`/`SAMPLING_THRESHOLD`） | 采样策略忠实移植（基准） |
| **JDBC** | `state/jdbc_agent_state_store.rs` | ✅ **已调用 core**（别名导入 `core_compute_hash`/`core_needs_full_rewrite`） |
| **PostgreSQL** | `state.rs` | ✅ **已调用 core**（`use agentscope_core::state::{list_hash, ...}`） |
| **REDIS** | `src/state/list_hash.rs` | 采样策略忠实重复实现（仅 per-element 输入由 Java `hashCode` 改为规范化 JSON + `DefaultHasher`）；tracker 已有准确的「低风险」标注 |
| **MONGODB** | `src/list_hash.rs` | 全量 SHA-256；但 `needs_full_rewrite` **正确比较 `existing_count` 前缀哈希**（语义对，算法/复杂度偏离） |
| **COS** | `agent_state_store.rs::compute_list_hash` | 全量 SHA-256 + **`needs_full_rewrite` 用整个新列表哈希做 `starts_with(存储哈希)`** |
| MySQL | — | 无 list_hash 逻辑 |

## 关键技术发现：COS 的追加优化恒不触发（行为偏离）
- COS 的 `needs_full_rewrite` 依赖「`hash(新列表).starts_with(hash(旧列表))`」来判定纯追加。
- **但全量 SHA-256 不满足该前缀性质** —— 已实测：`sha256([a,b])` = `cf6ab613e394...`，`sha256([a,b,c])` = `3d10f2e96612...`，`starts_with` = **False**。
- 后果：**纯追加场景恒判为需全量重写** → 增量追加优化失效（**数据无损**，仅性能与语义偏离）。
- 正解：core 用**相等比较** `has_changed(compute_hash(&values[..existing_count]), stored_hash)`（见 core `list_hash.rs` Case 3），不是 `starts_with`。
- ⚠️ 注意 COS 的**行为**偏离比 MONGODB **更严重**（MONGODB 只是算法不同，判定逻辑正确）——我原先误以为 MONGODB 是唯一/最严重者。

## 待办（未执行，仅记录）
MONGODB / COS / REDIS 三处应收敛到 `agentscope_core::state::list_hash`；若有跨进程稳定性诉求，应作为「升级 core 算法」的独立议题，而非多实现并存。

## 教训
**不要把「没见过调用」当成「未被调用」写进文档**。我前一轮写「全仓从未调用」时未做 grep 验证，属臆测。凡断言「X 未被使用/唯独 Y 偏离」必须**先 grep 全仓**。这与既有的「子智能体产出必须独立复核」是同一类错误：**未经验证的断言不得落笔**。
