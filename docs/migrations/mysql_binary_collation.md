# MySQL 键列二进制排序规则迁移（COLLATE utf8mb4_bin）

> 对应 Java 提交 `2349533a1`（#3288）及其 Rust 落地（P0-2 / 裁决 C5）。
> 适用版本：本次改动之后的 Rust 构建；存量部署需按本文手工迁移。

## 1. 背景：为什么必须改

MySQL 的 `DEFAULT CHARSET=utf8mb4` **不带** `COLLATE` 时，键列使用该字符集的**默认排序规则**：

- MySQL 8.0+ → `utf8mb4_0900_ai_ci`
- MySQL 5.7 / MariaDB → `utf8mb4_general_ci`

这些默认排序规则都是 **大小写不敏感（ci, case-insensitive）**。后果：两个只在字母大小写上不同的键在**主键/唯一索引**上会被判为相等，从而**静默互相覆盖**。

最小复现：

```text
put(ns, "README.md", A)   -- 写入成功
put(ns, "readme.md", B)   -- 与上一行主键冲突 → ON DUPLICATE KEY UPDATE 覆盖 A
```

`get(ns, "README.md")` 与 `get(ns, "readme.md")` 都会返回 B，A 永久丢失。

Rust 侧修复：把**键列**显式声明为 `COLLATE utf8mb4_bin`（二进制、大小写敏感），**payload 列保持表默认**（避免无必要地改变 `LIKE`/全文语义）。

- `crates/agentscope-extensions-mysql/src/store/dialect.rs`：`namespace_path VARCHAR(512) COLLATE utf8mb4_bin`、`item_key VARCHAR(255) COLLATE utf8mb4_bin`（`value_json` 不加）。

## 2. 受影响的表

| 逻辑表 | 默认表名 | 键列 | 现状态 |
|---|---|---|---|
| KV / 命名空间存储 | `agentscope_store` | `namespace_path`、`item_key` | 新 DDL 已 `utf8mb4_bin`；**存量库需迁移** |
| 会话状态 | `agentscope_sessions` | `session_id`、`state_key` | 新 DDL 仍为 `COLLATE utf8mb4_unicode_ci`（**大小写不敏感**）→ 见 §5「已知同类问题」 |
| 快照 | `agentscope_snapshots` | `snapshot_id`（主键） | DDL 未指定 COLLATE → 继承表默认；若快照 id 含大小写变体需一并迁移 |

**表名可配置**：通过 `TableNameConfig`（`prefix` 默认 `agentscope_`，或 `store_table_name` / `session_state_table_name` / `snapshot_table_name` 全名覆盖）。迁移脚本中的表名请替换为实际值。

## 3. 迁移脚本（幂等，先备份）

> 先做逻辑备份（`mysqldump`）或表快照，**低峰执行**。DDL 会锁表重建索引，大表耗时较长。

```sql
-- 1) KV 存储表：仅键列转二进制排序
ALTER TABLE agentscope_store
  MODIFY namespace_path VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
  MODIFY item_key       VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL;

-- 2) 快照表：主键列转二进制（若 id 含大小写变体）
ALTER TABLE agentscope_snapshots
  MODIFY snapshot_id VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL;
```

执行前确认字符集/长度与当前 DDL 一致（`SHOW CREATE TABLE <t>\G`）。

### 3.1 先检查是否存在会冲突的键（可选，但强烈建议）

迁移前若表中已存在**仅大小写不同**的键，它们在旧排序规则下不可能共存（早已互相覆盖），因此下表通常为空；若非空则说明有历史脏数据，需人工决定保留哪一行：

```sql
SELECT LOWER(namespace_path) lp, LOWER(item_key) lk, COUNT(*) c
FROM agentscope_store
GROUP BY lp, lk
HAVING c > 1;
```

## 4. 影响与回滚

- **查询行为变化**：迁移后键比较变为大小写敏感，`WHERE item_key = 'X'` 与 `=` 'x'` 不再等价；`LIKE 'prefix%'` 前缀匹配亦变为大小写敏感（前缀列 `namespace_path` 同样改了）。
- **不会丢数据**（前提按 §3.1 检查为空）。
- **回滚**：将两列改回原排序规则即可，例如
  `ALTER TABLE agentscope_store MODIFY item_key VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci NOT NULL;`
  （回滚后大小写变体键将再次互相覆盖，故不建议回滚。）

## 5. 会话/快照表（代码已修，存量库仍需迁移）

- 代码侧已对齐：`crates/agentscope-extensions-jdbc/src/dialect/vendor/mysql.rs` 的**会话状态表**
  `session_id`/`state_key` 与**快照表** `snapshot_id` 均已显式 `CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`
  （表级默认仍为 `utf8mb4_unicode_ci`，只影响非键列）。
- **存量库不会自动变**：已部署的旧表仍是大小写不敏感，需执行与 §3 同类的
  `ALTER TABLE ... MODIFY ... COLLATE utf8mb4_bin`（表名 `agentscope_session_state` / `agentscope_snapshots`，以实际前缀为准）。
- 迁移前的变体键去重检查同 §3.1。

## 6. 交叉引用

- Java 源：`2349533a1`（#3288）。
- Rust 落点：`crates/agentscope-extensions-mysql/src/store/dialect.rs`。
- 分析报告：`docs/java-recent-commits-migration-analysis.md`（GAP-03 与 OPS-01）。
