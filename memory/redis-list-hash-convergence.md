# REDIS 模块：list_hash 收敛到 core（2026-09-19）

## 事实
- 删除 `crates/agentscope-extensions-redis/src/state/list_hash.rs`（原 108 行本地平行实现）。
- `src/state/mod.rs`：移除 `mod list_hash;`；新增 `use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};`；调用点 `save_list_inner`（原 324/328 行）改为直接调用 core 泛型函数。
- 5 个集成语义测试迁入 `src/state/mod.rs` 的 `tests` 模块（`empty_hash_matches_java_constant` / `unchanged_prefix_needs_no_rewrite` / `append_growth_needs_no_rewrite` / `shrink_or_edit_needs_rewrite` / `missing_hash_with_existing_data_needs_rewrite`）；纯算法重复测试不迁移（core 已覆盖）。
- `EMPTY_HASH` / `SAMPLING_THRESHOLD` 本 crate 无其他引用（原为私有 `mod`，未 `pub use`）→ 无需改写，仅测试内 `use agentscope_core::state::EMPTY_HASH;`。
- 无命名冲突。

## 关键结论（可复用的裁定）
- `serde_json::Value` 实现 `Hash`，core `compute_hash<T: Hash>` 可直接接 `&[Value]`，**无需本地 JSON 串中转**（JDBC/PostgreSQL 亦此调用范式）。
- 本地 Case3 的 `&current[..existing_count.min(current_size)]` 与 core 的 `&current[..existing_count]` **行为等价**（Case1 已保证 `current_size >= existing_count`）。
- 存量数据自愈：新旧 hash 值不逐位一致 → 每 slot 首次 `save_list` 多一次全量重写（`DEL` + `RPUSH` 原值），数据无损、无需迁移脚本。

## 验证
- `cargo build -p agentscope-extensions-redis` → 通过（仅 1 条 pre-existing warning：`sandbox.rs:175 released` 未读）。
- `cargo test -p agentscope-extensions-redis` → **21 passed / 0 failed** + 2 doc-tests（与收敛前一致，删 0 增 0，仅用例位移）。
- `rg "fn compute_hash|fn needs_full_rewrite" crates/agentscope-extensions-redis` → 无结果。

## 关联
- 跨模块待裁定项「list_hash 算法并存冲突（MONGODB）」：MONGODB 的本地 `list_hash.rs`（SHA-256）**仍未收敛**，可仿此例处理（本地实现为 SHA-256 而非采样策略，语义不同，需单独裁定）。
