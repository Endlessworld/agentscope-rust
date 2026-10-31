# PostgreSQL：会话表 `version` 列默认值迁移（PG-03）

## 背景

`agentscope_sessions` 表的 `version` 列用于版本化 CAS：

- `0` 是 **「行不存在」哨兵**（`get_versioned()` / `version_of()` 对缺失键返回 `0`）；
- 真实行的版本从 `1` 开始。

旧 DDL（与 Java `PostgresAgentStateStore` 同源）为 `version BIGINT NOT NULL DEFAULT 0`，
而列表写路径（`save_list`）与 `:_hash` 伴随行**不写** `version` 列 → 这些行的版本恒为 `0`。
后果：对同一 key 再调 `save_if_version(key, 0)`（语义＝「仅当键缺失才创建」）时，
`INSERT ... DO NOTHING` 影响 0 行，触发 0→1 回退的 `UPDATE ... WHERE version = 0`，
命中原有的第 0 条列表项 → **把「已存在」误判为「不存在」并覆盖既有数据**
（tracker 中的 *幽灵 CAS 成功*）。

## Rust 侧现行实现（已改）

| 位置 | 变更 |
| --- | --- |
| `crates/agentscope-extensions-postgresql/src/sql.rs` `sessions_ddl` | `version BIGINT NOT NULL DEFAULT 1` |
| `.../src/state.rs` `insert_items_stmts` | INSERT 显式绑定 `version = 1` |
| `.../src/state.rs` `save_hash_stmt` | INSERT 显式绑定 `version = 1`，冲突时 `version = EXCLUDED.version`（把旧行归一到 1） |

单值写路径（`save_versioned` / `save_if_version`）本就显式写 `version`，不受影响。

> **跨语言差异**：Java 侧 DDL 仍是 `DEFAULT 0`（其源码注释已承认 0 是错误默认值）。
> 因此 **Java 写入的列表行在 Rust 侧会被视为 `version = 0`**：若先用 Java 写列表、
> 再用 Rust 做 `save_if_version(key, 0)`，仍会走 0→1 回退分支（该分支是**故意保留**给
> 旧库「回填行」的，见用例 `save_if_version_zero_falls_back_to_cas_for_backfilled_row`）。
> 执行下方迁移脚本后两侧语义一致。

## 迁移脚本（已有库）

`CREATE TABLE IF NOT EXISTS` 不会修改已存在的表，**必须手工执行**：

```sql
-- 1) 新插入的缺省版本改为 1（DDL 默认值对齐）
ALTER TABLE agentscope.agentscope_sessions
    ALTER COLUMN version SET DEFAULT 1;

-- 2) 归一历史行：把遗留的 0 版本行提到 1
UPDATE agentscope.agentscope_sessions
   SET version = 1
 WHERE version = 0;

-- 3) 校验（应返回 0 行）
SELECT count(*) FROM agentscope.agentscope_sessions WHERE version = 0;
```

自定义 schema / 表名时替换 `agentscope.agentscope_sessions`（标识符需按需加双引号）。

### 执行注意事项

- 第 2 步是**全表更新**，行数多时会取行锁并产生大量 WAL；请在低峰执行，必要时分批：
  ```sql
  UPDATE agentscope.agentscope_sessions SET version = 1
   WHERE session_id IN (
       SELECT session_id FROM agentscope.agentscope_sessions WHERE version = 0 LIMIT 10000
   );
  ```
- 与 Java 实例**混跑**期间无需停机：Java 的 `ensureVersionColumn()` 使用 `DEFAULT 1`，
  且单值写路径总是显式写版本；唯一残留是 Java 写的列表行仍为 0（第 2 步可反复执行以收敛）。
- 回滚：`ALTER TABLE ... ALTER COLUMN version SET DEFAULT 0;`（不需要把 1 改回 0，
  版本单调递增是安全方向）。

## 相关测试（离线）

- `sql::tests::sessions_ddl_declares_primary_key_and_version_column` — 钉住 `DEFAULT 1` 且不存在 `DEFAULT 0`
- `state::tests::list_and_hash_inserts_bind_version_one` — 列表行与 `:_hash` 行显式绑定 `version = 1`
- `state::tests::create_if_absent_after_save_list_is_rejected_and_list_is_intact` — `save_list` 后 `save_if_version(key, 0)` 必为冲突且列表不变
