# extensions 迁移 tracker 回填（文档-only 批次）

## 状态
- 任务性质：**只写各模块 `<MODULE>__GAP_TRACKER.md`，严禁改代码/测试**。
- 2026-09-18 本会话完成最后一批：**JDBC / MONGODB / REDIS**（3 个子智能体并行）。
- **至此 `docs/extensions/` 下已无空模板占位**（`_（调研后填写）_` 全部清除）。
- 三个子智能体均未改代码、未跑 cargo，仅只读调研（rg/glob/read + codegraph 契约核对 + 读 Cargo.toml 取版本）。

## 基线（汇总方给定，直接采用，未重跑）
- workspace 全量：1485 passed / 0 failed。
- JDBC：`cargo test -p agentscope-extensions-jdbc` = lib **2 passed / 0 failed**，doc-tests 0。
- MONGODB：lib **56 passed / 0 failed** + **1 doc-test**。
- REDIS：**21 passed / 0 failed** + **2 doc-tests**。

## 实测规模（本次回填写入 §1）
- JDBC：主 3106 行（20 .java）/ 测 1558 行（11 .java，10 测试类共 98 个 `@Test`）。
- MONGODB：主 2231 行（9 类）/ 测 2427 行（8 测试类 + 2 testutil，147 个 `@Test`）。
- REDIS：主 2884 行（13 类）/ 测 217 行（3 测试类）。
- 三者 Java 原「子模块」字段此前为**乱码**（如 `(鍗曟ā鍧?`），本轮修正为真实结构。

## 裁定：滚动 tracker 状态口径（重要）
- **问题**：`EXTENSIONS_GAP_TRACKER.md` 第 23–25 行原写 JDBC/REDIS/MONGODB「已完成」，而三份独立 tracker §1 依实测证据写「部分完成」。
- **裁定：以独立 tracker 为单一事实来源 → 统一「部分完成」**，已修正滚动 tracker 三行并追加缺口摘要与 §3 指引。
- 通用原则：滚动视图**不得高于**独立 tracker；被测通过的事实不降级，缺口下沉到 §3 表格。

## 待汇总方处理的跨模块需求
- harness `DistributedStore` 字段不足（仅 `agent_state_store` + `isolation_scope`）：JDBC/REDIS/MONGODB 各自 `to_harness_store()` / `to_distributed_store()` 投影子集；**已裁定不改 harness**（§2.2 决议保持）。
- **list_hash 算法并存冲突（MONGODB）**：crate 本地 `list_hash.rs`（SHA-256）与 `agentscope_core::state::list_hash`（DefaultHasher 采样）算法不同，state_store 用前者 → 需裁定统一。
- 真实驱动未进根 `[workspace.dependencies]`：MONGODB `mongodb`、REDIS `redis` 1.7.0 由各 crate 自行声明。
- JDBC 跨语言不兼容差异待裁定：会话表列名 `slot_id` vs Java `session_id`；锁表名 `{prefix}lock` vs Java `{prefix}distributed_locks`。

## 已知 P2 清理项（勿误当 bug）
- REDIS `src/sandbox.rs:175` 字段 `released` 未读取（dead-code warning）。
- MONGODB `state_store.rs:433 delete_key_inner` 仅测试引用（非 test 构建 dead_code warning）。
- JDBC device helper（`query_optional_int` / `TableLockStrategy::delete_lock` / `read_version` / `slot_param` / `namespace_param`）为 dead_code，待组件接线后调用。
