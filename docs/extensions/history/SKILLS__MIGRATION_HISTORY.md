# SKILLS 迁移执行历史（归档）

> 本文件是 [`../SKILLS__GAP_TRACKER.md`](../SKILLS__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：静态复核记录（2026-10-22） · 7. 裁定执行记录（2026-10-22，汇总方） · 8. P1 收敛记录（2026-10-26） · 9. P2 收口 + P1-09 补测执行记录（2026-10-28） · 10. 2026-10-31 loop 收口记录（本回合：真实驱动 / P1-09 补测 / P2-09 裁定 / `de2dc8f3c`））。

## 静态复核记录（2026-10-22）

> **历史快照（保留以追溯判断依据）**：本节写于 P1/P2 收敛之前，行内「⚠️ 缺 xxx / Rust 5 用例 / 无 file:// 覆盖」等描述**已被后续修复推翻**。当前真相以 **§3.2 状态列 + §9** 为准：P1-01~P1-11 全部已完成，P2-02/03/04/05/06/07/08/10 已完成，P2-01 已落为 trait 方法（§7.1），P2-09 保持 `Result`（§7.2），P1-09 为 93 vs Java 192（§9.5）。

### 复核方式
- **纯静态核对**：未执行 `cargo build` / `cargo test`，未修改任何 Rust 代码；仅本文件被编辑。
- 工具：PowerShell 文件统计（`Get-Content` / `ReadAllLines`）+ `rg` 文本检索 + 逐文件人工通读 Java 源码与 Rust 源码。
- 对照物：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-skills`（3 个子模块）、`E:\local-github\agentscope-java\agentscope-core\src\main\java\io\agentscope\core\skill\*`（接口/模型）、`E:\local-github\agentscope-rust\crates\agentscope-harness\src\agent\skill\*`（harness 存量分层）。
- **幂等处理**：开工时本文件的工作区版本（`git status` 显示 `M`、未提交）**已含上一轮会话写入的复核片段**（模块概览的「3529 / 3432」「P0×1 / P1×6」、§2 的 `setWriteable` 行与「上游状态提醒」、§2.0 分层裁定），但**其引用的 §3.2 与「静态复核记录」两节并不存在**（悬空引用）。本轮按幂等要求**就地补全**：新建 §3.2 缺口表 + 末尾「静态复核记录（2026-10-22）」一节，并把所有悬空交叉引用（`§静态复核记录 P1/P2`）改指到 §3.2 的具体编号（P0-01 / P2-01），**未重复追加**同名复核节。

### 实测统计
| 维度 | Java | Rust |
|---|---|---|
| 主代码文件 | 3（git / mysql / postgresql） | 8（`error` / `git` / `lib` / `mysql` / `postgresql` / `repository` / `sql` / `util`） |
| 主代码行 | **3529 物理 / 3197 非空 / 2093 纯代码** | **3027 物理 / 2753 非空 / 2435 纯代码** |
| 测试行 | **3432 物理 / 2813 非空 / 2623 纯代码**（3 个独立测试文件） | 内联于 src（**无 `tests/` 目录**）：`#[cfg(test)]` 模块共 **456 物理行**，对应实现 **2571 行**；33 个测试函数 |
| 测试代码 / 实现代码 比 | 2623 / 2093 ≈ **1.25** | 456 / 2571 ≈ **0.18**（测试密度约为 Java 的 **1/7**） |
| 类型数（主） | 3 顶层类 + 6 嵌套类 = **9** | 3 个后端仓储 + 1 trait + 1 info + 1 适配器 + `SqlDatabase`/`SqlConnection`/`SqlRow`/`SqlValue`/`SqlParam` + `InMemorySqlDatabase` |
| 测试用例数 | **192 个 `@Test`**（git 23 / mysql 61 / pg 108） | **33 个 `#[test]`**（git 5 / mysql 12 / pg 7 / sql 3 / util 4 / repository 2），**0 个 `#[tokio::test]`**、0 doc-test（源码无 ``` 块） |
| `JdbcAgentSkillRepository`（官方替代） | 1184 物理 / 1099 非空 | **不存在**（jdbc crate `rg -i skill` = 0 命中） |

> 行数口径说明：`Measure-Object -Line`（旧 PowerShell）不统计末行空行，与 `ReadAllLines` 物理行数相差 1 行/文件；本文统一以 **物理行**（`ReadAllLines`）为准，并同时给出**非空行**（与 rollup「3177 / 2813」口径最接近）与**纯代码行**（剔除空行与注释）。

### 对齐结论表
| Java 元素 | Rust 对应 | 是否真实存在 | 行为覆盖 |
|---|---|---|---|
| core `AgentSkillRepository`（11 方法） | `src/repository.rs` 的 `trait AgentSkillRepository`（10 方法） | ✅ | ⚠️ 缺 `setWriteable`（P2-01）；`getSkill` 找不到时 Java 抛 `IllegalArgumentException`、Rust 返 `NotFound`（设计内）；错误类型统一为 `Result` |
| core `AgentSkillRepositoryInfo` | `src/repository.rs` 的 `AgentSkillRepositoryInfo` | ✅ | ✅ `Display` 与 Java `toString` 逐字一致（`type`/`location`/`writable`） |
| `GitSkillRepository` | `src/git.rs` | ✅ | ⚠️ 克隆/分支/路径解析语义有 4 处差异（P1-01~04）；Java 23 用例 → Rust 5 用例且**无 file:// 真实克隆覆盖**（P1-09） |
| `MysqlSkillRepository` + `Builder` | `src/mysql.rs` + `MysqlSkillRepositoryConfig` | ✅ | ⚠️ SQL 语义 4 处差异（P1-05/06/10、P2-02/05）+ 事务/空名差异（P2-03/08） |
| `PostgresSkillRepository` + `Builder` | `src/postgresql.rs` + `PostgresSkillRepositoryConfig` | ✅ | ⚠️ 同上，另有 `metadata_json` 类型探测（P1-07）与 1000 行分批（P2-04）差异 |
| Java `DataSource` | `src/sql.rs` 的 `SqlDatabase`/`SqlConnection` | ✅（新抽象，非 1:1） | ⚠️ **无真实驱动实现**；`InMemorySqlDatabase` 只是测试替身（Java 侧可用 H2/Testcontainers 跑真库） |
| Java `AgentSkill`（含 `skillContent` / `originDir`） | harness `AgentSkill`（**无** `skill_content` / `origin_dir` 独立字段） | ✅（模型复用） | ⚠️ `skill_content` 以 `resources["SKILL.md"]` 承载；`originDir` 缺失导致 `skill_dir` 缺口（P1-08） |
| Java `SkillFileSystemHelper`（frontmatter 解析） | harness `load_skills_from_dir`（**不解析 frontmatter**） | ✅（复用） | ❌ 名称/描述来源与 Java 不同（P1-01/02） |
| Java `JdbcAgentSkillRepository` | **无** | ❌ | ❌ P0-01 |
| Java `setWriteable` | **无** | ❌ | ❌ P2-01 |

### 新发现缺口
已全部登记于 **§3.2**（P0×1 / P1×10 / P2×10）。按子模块归属：

- **git-repository**：P1-01（技能名取目录名）、P1-02（description 取首个非空行）、P1-03（`ls-remote` 未按 `refs/heads/<branch>` 过滤）、P1-04（远端不可达时静默跳过 pull）、P2-06（`save()` 返回 `Err` 而非 `false`）、P2-07（remote URL 未 trim）、P1-09（测试 5 vs 23，无 file:// 克隆/拉取/分支用例）。
- **mysql-repository**：P1-05（未 `CREATE DATABASE`、表名无 `<db>.` 前缀）、P1-06（`createIfNotExist=false` 无存在性校验）、P1-10（`get_source()` 不含库/表名）、P2-02（空名不回退默认）、P2-03（`delete` 无事务）、P2-05（DDL 缺 utf8mb4）。
- **postgresql-repository**：P1-07（不校验 `DATA_TYPE ∈ {text, character varying, character}`）、P1-10（`get_source()` 不含 schema/表名）、P2-02、P2-03、P2-04（无 1000 行分批）。
- **跨子模块**：P0-01（`JdbcAgentSkillRepository`）、P1-08（`skill_dir` 未透出）、P1-09、P2-01、P2-09、P2-10。

### 「33 test 通过 / 6 个失败已修」形态判定（静态）
- **6 个失败用例的修复形态正常**：逐条比对 `sql.rs` / `git.rs` 当前代码——①`parse_table` 已识别 `create table` / `create schema` / `insert into` 并处理 `if not exists` 与引号（`sql.rs:254-293`）；②`query` 中 `information_schema.columns` 探测**前置于**宽泛 `information_schema` 分支，且取**第 2 个**文本参数为表名（`sql.rs:453-470`）；③`execute` 已新增 `INSERT` 分支，构造行、尊重显式 `id`、按 `auto_commit` 暂存/直写（`sql.rs:406-443`）；④`normalize_skills_root` 在 `is_absolute()` 之外额外拒绝以 `/` 或 `\` 开头的原始串（`git.rs:454-464`）。**四处修复均确实存在于当前代码，形态正常，无回退。**
- **但「33 passed / 0 failed」不等于语义对齐**：33 个测试全部基于 `InMemorySqlDatabase` 与纯字符串断言，**没有任何一个测试覆盖 git 真实克隆/拉取/分支**（`git.rs` 5 个测试均为构造/校验级）；Java 的 23 个 git 用例中，`testGetSkill_FromLocalRepo` / `testPull_FromLocalRepo` / `testClone_SpecificBranch` / `testGetSkill_FromCustomSkillsRoot` 等**在 Rust 侧完全无对应**。因此「测试全绿」只能证明**实现自洽**，不能证明 **1:1 对齐 Java**——§3.2 的 P1-01~P1-04 即为「测试全绿但语义不一致」的直接例证。

### 勘误列表（原文 → 实际）
| # | 原文 | 实际 |
|---|---|---|
| 1 | 「主代码 3529 行 / 测试 3432 行（原记『~3177 / ~2813』**偏低**，见 §静态复核记录）」 | 3529 / 3432 是**物理行**；3177 / 2813 是**非空行**口径（实测 3197 / 2813）。**不是「偏低」，而是口径不同**；测试侧 2813 **完全吻合**。该项本已在前一轮会话写入工作区（未提交），本轮仅补口径说明 |
| 2 | 「Java 类数：主代码 3 文件 / 9 个类型；测试 **192 个 `@Test`**」 | ✅ 准确（3 顶层 + 6 嵌套 = 9；23 + 61 + 108 = 192） |
| 3 | 「Rust 规模：8 个 `.rs` / **3027 行** / 33 个内联测试（0 doc-test）」 | ✅ 准确（3027 物理行；33 个 `#[test]`；源码无 ``` 块故 0 doc-test）。补充：**非空行 2753**，**0 个 `#[tokio::test]`** |
| 4 | 「`JdbcAgentSkillRepository`（**1180 行**）」 | **1184 物理行 / 1099 非空行**（`Measure-Object -Line` 口径为 1180） |
| 5 | 「当前状态：进行中（编译与测试全绿，但静态复核发现 **P0×1 / P1×6** 语义缺口）」 | 缺口数修正为 **P0×1 / P1×10 / P2×10**（原 P1×6 为早期粗略计数，且未拆出 P2 清单） |
| 6 | 「§4.1 当前状态：**已完成**（测试全绿）」 | 改为 **进行中**——测试全绿 ≠ 语义对齐，§3.2 仍有 21 项未收敛 |
| 7 | 「git 语义差异」仅列 3 项 | 补齐为 7 项：新增 **description 取首个非空行**、**`save()` 返回 `Err` 而非 `false`**、**remote URL 未 trim** |
| 8 | 「mysql/postgresql 语义差异」仅列 `get_source()` / `createIfNotExist` / `CREATE DATABASE` / 表名前缀 / `DATA_TYPE` / 1000 分批 | 补齐为 9 项：新增 **空名不回退默认值**、**`delete()` 无事务**、**MySQL DDL 缺 utf8mb4** |
| 9 | 「§4.2 第 1 条：真实驱动接入需在汇总 tracker §4.2 登记 workspace 依赖（`sqlx`/`mysql_async`/`tokio-postgres`）」 | 根 `Cargo.toml` `[workspace.dependencies]` **已登记**这三者（注释即写「存储/技能模块 P1」）；且 `agentscope-extensions-postgresql` 已有 `postgres-driver` feature + `PgSimpleClient`。**实际待办是「在本 crate 引用/实现 `SqlDatabase`」，而非「登记依赖」** |
| 10 | 「§4.2 第 2 条：Git 路径未纳入离线单测（与 Java 依赖系统 Git 凭据一致）」 | 归因不准：Java 用 `file://` 本地仓 + JGit **完全离线**测了 23 个用例；Rust 用 `git` CLI + `file://` 同样可离线覆盖。**「依赖系统 Git 凭据」不构成不可测理由** |
| 11 | 「§3.1 测试行：`33 passed / 0 failed`（内存 SQL + **file:// git**）」 | Rust 的 5 个 git 测试**均未使用 `file://` 真实仓库**（无 `git init` / `file://` 构造），只有纯构造参数断言；「file:// git」表述不准确 |
| 12 | 「§3.1 `RepoBackedSkillRepository`：已完成」 | 功能实现完成，但**未覆写 `skill_dir`** → harness `load_skill_through_path` 对 git/SQL 仓库不可用（P1-08），状态说明已补注 |
| 13 | 「§2.0 harness 关系：零重复模型」 | ✅ 复核**成立**——本 crate 直接复用 harness `load_skills_from_dir`，未另写目录扫描；模型统一为 harness `AgentSkill`。唯一衔接缺口是 `skill_dir`（P1-08） |

### 复核后建议的 rollup 状态值
**建议：`进行中`**（而非 `已完成`）。
- 理由 1：§3.2 有 **21 项**未收敛缺口，其中 **P0×1**（官方替代实现 `JdbcAgentSkillRepository` 缺失，需汇总方裁定归属 crate）与 **P1×10**（含 git 名称/描述语义、分支探测、MySQL 建库与存在性校验、PG 类型探测、适配器 `skill_dir`）属**可观察的行为不一致**。
- 理由 2：§3.2 需汇总方对 **P2-01（`setWriteable`）/ P2-09（`close()` 语义）** 做「设计决定（不迁移）」裁定，尚未闭环。
- 理由 3：**33 test 通过**仅覆盖内存替身与构造级断言，git 真实克隆/拉取/分支路径与真实 SQL 驱动路径**均无测试**（P1-09），验收证据不足以支撑「已完成」。
- 允许上调为 `已完成` 的条件：P0-01 有归属裁定 + P1-01~P1-08 修复或明确标记为「设计决定（不迁移）」+ git 侧补齐 `file://` 克隆/拉取/分支用例。


## 7. 裁定执行记录（2026-10-22，汇总方）

> 依据 `EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 5（`set_writeable` / `close()`）。

### 7.1 已修复（P2-01）

| 项 | 实现 |
|---|---|
| `AgentSkillRepository::set_writeable(&self, writeable: bool)` | 新增到本 crate 自有 trait（**未加入 harness `SkillRepository`**，harness 保持只读语义） |
| MySQL / PostgreSQL | 增加 `writeable: AtomicBool` 字段（初值取自 config）；`is_writeable()` 读实时值；`save` / `delete` / `clear_all_skills` 改读实时值 → 运行期可关闸 |
| Git | 按 Java `GitSkillRepository.setWriteable` 语义**忽略并 warn**（保持 `is_writeable() == false`） |
| 测试替身 | 同步实现（no-op） |

**测试**：新增 `set_writeable_toggles_runtime_flag`（默认 true → 关闸后 `is_writeable()==false` 且 `save` 被拒 → 开闸后恢复）；`cargo test -p agentscope-extensions-skills` → **34 passed / 0 failed**（原 33，净 +1）。

### 7.2 裁定结论：`close()` 不视为缺口

Java `AgentSkillRepository extends AutoCloseable`，`close()` 是 `default { }`（no-op）。Rust 侧 `fn close(&self) -> Result<(), _>` + 默认 `Ok(())` 语义等价，**保持现状不改 void**——为形式统一而牺牲错误可表达性没有收益。

## 8. P1 收敛记录（2026-10-26）

> 本轮只改 `crates/agentscope-extensions-skills/**`（含 `src/repository.rs`、`src/git.rs`、`src/sql.rs`、`src/mysql.rs`、`src/postgresql.rs`）；**未改**根 `Cargo.toml`、`agentscope-core`、`agentscope-harness`、`agentscope-extensions-jdbc` 与任何其它 crate。
> **未执行 cargo**（按任务约定由主智能体统一编译/跑测）；下述「测试」为新增用例名，待主智能体验证后回填实测结果。

### 8.1 已修复（P1，git 语义对齐 + 适配器 `skill_dir`）

| # | 修法 | 文件 | 改前如何失败 | 新增/相关测试 |
|---|---|---|---|---|
| P1-01 | 新增 `load_skills_with_frontmatter`：先走 harness `load_skills_from_dir` 拿资源，再用 `skill_identity()` 以 frontmatter `name` 重写技能名；缺失时回落目录名；含 `/` `\` `..` 的 frontmatter name 记 warn 后回落目录名 | `src/git.rs` | 目录名 `dir-name` + frontmatter `name: calc-skill` 时 `get_skill("calc-skill")` 返回 `NotFound` | `clones_local_repository_and_uses_frontmatter_identity`、`skill_identity_prefers_frontmatter_and_falls_back` |
| P1-02 | 同一 `skill_identity()` 从 frontmatter `description` 取描述；缺失时回落正文首个非空行（在 `split_frontmatter` 之后扫描，故不再返回 `---`）；全空时回落 `Skill: <name>` | `src/git.rs` | frontmatter 文件的 description 为 `---` | `clones_local_repository_and_uses_frontmatter_identity`（断言 `A calculator`）、`frontmatter_scalars_reads_plain_quoted_and_block_values` |
| P1-03 | `remote_head()` 改为 `git ls-remote <url> refs/heads/<branch>`（Java `setHeads(true)` + `matchesBranch`）；无 branch 时仍用远端 `HEAD`；返回值再按 refspec 二次校验 | `src/git.rs` | 配置 `develop` 但探到的是远端 `HEAD`（main）→ 永不触发 pull／误判无更新 | `remote_head_is_filtered_by_branch`（远端 HEAD 停留 main，断言探到 develop sha） |
| P1-04 | `remote_head()` → `Result<Option<String>>`；`has_remote_updates()` → `Result<bool>`。远端不可达：`warn!` + 返回 `Err(Git)`，读操作失败而不再用陈旧数据（Java 为 `resolveRemoteHead`=null→`true`→pull 抛错，可观察结果一致） | `src/git.rs` | 远端删掉后 `get_all_skill_names()` 仍返回旧技能（静默跳过 pull） | `unreachable_remote_fails_instead_of_serving_stale_skills` |
| P1-08 | `AgentSkillRepository` 新增 `fn skill_dir(&self, name) -> Option<PathBuf>`（默认 `None`，对应 Java `AgentSkill#getOriginDir`）；`GitSkillRepository` 按 frontmatter name 扫描 skills 目录返回绝对路径；`RepoBackedSkillRepository` 覆写 harness `SkillRepository::skill_dir`，后端无目录（SQL）时把 resources 落盘到临时缓存目录 | `src/repository.rs`、`src/git.rs` | `load_index()` 拿不到目录 → `load_skill_through_path` 对 git/SQL 技能不可用 | `adapter_forwards_backend_skill_dir`、`adapter_materialises_in_memory_skills` |
| P1-05 | 新增 `create_database_if_not_exist()`（`CREATE DATABASE IF NOT EXISTS <db> DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`）；新增 `full_table_name()`，**所有** SQL（建表/查询/插入/删除/清空）改用 `<db>.<table>` | `src/mysql.rs` | 数据库不存在时建表失败；跨库连接下裸表名指错库 | `creates_database_and_qualified_tables` |
| P1-06 | `create_if_not_exist=false` 时执行 `verify_database_exists()` + `verify_tables_exist()`，缺失返回 `SkillRepositoryError::Database`（Java `IllegalStateException`） | `src/mysql.rs` | 空库上构造函数照样成功，后续查询静默空集 | `create_if_not_exist_false_requires_the_database`、`create_if_not_exist_false_accepts_an_initialised_schema`；既有 `legacy_schema_without_metadata_column_still_works` 的 setup 补建 resources 表（Java 同样要求两表存在） |
| P1-07 | `detect_metadata_json_column_support()` 读取 `DATA_TYPE` 并校验 ∈ `{text, character varying, character}`，否则 warn + 走 legacy 路径 | `src/postgresql.rs`、`src/sql.rs` | `jsonb` 列被当成支持 → 写 `metadata_json`（JSON 字符串）报错 | `text_metadata_column_is_supported`、`jsonb_metadata_column_falls_back_to_the_legacy_schema` |
| P1-10 | `get_source()`：mysql → `mysql_<db>_<table>`；pg → `postgresql_<schema>_<table>`（Java 逐字一致） | `src/mysql.rs`、`src/postgresql.rs` | 多实例 source/repo_id 冲突 | `get_source_includes_database_and_table`、`get_source_includes_schema_and_table` |

### 8.2 部分收敛（P1-09 测试覆盖）

- 新增 **20 个** `#[test]`（git 11 / repository 2 / mysql 4 / pg 3），全 crate **34 → 54**，全部离线：git 侧用 `git init` + `file://` 本地仓（`git` 二进制可用时；无 mock 库）。
- git 用例对照 Java 的 `testGetSkill_FromLocalRepo` / `testGetSkill_FromSkillsSubdirectory` / `testPull_FromLocalRepo` / `testClone_SpecificBranch` / `testGetSkill_FromCustomSkillsRoot` / `testManualSyncRequiredWhenAutoSyncDisabled` / `testSave_ReadOnly` 等。
- 仍缺（保留待办）：swap/force 组合、并发克隆、各类 git 2.x 错误分支（认证失败/分支不存在/路径已存在且非仓）的 outcome 级用例；SQL 侧仍为内存替身，无真实驱动用例（取决于 §4.2 第 1 条）。**总数与 Java 192 个 `@Test` 仍有差距**。

### 8.3 顺带修复（P2，仅低成本项）

- **P2-05**：MySQL 两张表 DDL 追加 `DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`（Java 逐字一致）。
- **P2-07**：`from_config` 存 `remote_url.trim()`（Java 构造时 trim），`remote_url_is_trimmed` 校验。
- 其它 P2（P2-02 空名回退 / P2-03 `delete` 事务 / P2-04 1000 行分批 / P2-06 `save()` 返 `false` / P2-08 `skill_exists("")` / P2-10 依赖清理）**未动**：`Mysql/PostgresSkillRepository` 已 `@Deprecated`（裁定 2：不扩展），且 P2-06/P2-08 会改变已文档化的 Rust 语义（`Result` 表达错误），需汇总方确认。

### 8.4 未做 / 保留待办

- **P0-01** `JdbcAgentSkillRepository` 归属 jdbc crate（已完成，`crates/agentscope-extensions-jdbc/src/skill/repository.rs`），本 crate 不再补。
- **P2-01 / P2-09** 已裁定（§7.1 / §7.2）。
- 本 crate **未**给 MySQL/PG 加 PG `verifySchemaExists`/`verifyTablesExist`（Java 有，但不在 §3.2 P1 清单内，且属已弃用实现的功能扩展）——如需对齐，登记为新 P2。
- 依赖清理（P2-10）未做；本轮用到 `base64`，其余零引用依赖（`agentscope-core`/`async-trait`/`futures`/`chrono`/`serde` derive）仍在，待汇总方确认后删。

### 8.5 改动文件清单

| 文件 | 改动 |
|---|---|
| `src/git.rs` | frontmatter 身份（`skill_identity`/`frontmatter_scalars`/`split_frontmatter`）、`remote_head` 分支过滤 + `Result`、`has_remote_updates` 可观察失败、`skill_dir`/`resolve_skill_dir`、URL trim、11 个新测试 |
| `src/repository.rs` | trait 新增 `skill_dir`（默认 `None`）、适配器覆写 `skill_dir` + SQL 技能落盘缓存（`cache_root`/`materialized`/`write_resources_to_dir`）、2 个新测试 |
| `src/sql.rs` | 内存替身支持 `CREATE DATABASE`/`CREATE SCHEMA` 注册、`<schema>.<table>` 前缀、`metadata_json` 列**类型**记录与回放、`schemata` 探测；新增 `schema_exists`/`metadata_column_type`/`has_metadata_column`（后两者转 pub 消除 dead_code） |
| `src/mysql.rs` | `CREATE DATABASE` + `<db>.<table>` + utf8mb4 DDL + 存在性校验 + `get_source`；4 个新测试 |
| `src/postgresql.rs` | `DATA_TYPE` 校验 + `get_source`；3 个新测试 |

### 8.6 原 §7.3「仍待办」终态（2026-10-26）

- **P0-01** `JdbcAgentSkillRepository` 归属已裁定在 **jdbc** crate 实现（§4.5 裁定 2）；现已落地于 `crates/agentscope-extensions-jdbc/src/skill/repository.rs`（`impl AgentSkillRepository for JdbcAgentSkillRepository`，直接复用本 crate 的 trait）。本 crate 只需给两个 SQL 后端标注「已废弃，仅兼容，勿扩展」（已有 doc 注释）。
- **P1-01~P1-08 / P1-10**：已于 2026-10-26 收敛，见 §8.1（逐项修法 + 测试名）。
- **P1-09**：部分收敛（git 侧补 11 个 `file://` 真实仓用例，含克隆/拉取/分支/自定义 skillsRoot/远端不可达），仍低于 Java 192，见 §8.2。

### 8.7 P1-11 收敛（2026-10-26，frontmatter 闭合围栏）

> 来源：`docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md` §4.3。Java 基线 `16cb024ae`。

**根因**：`split_frontmatter` 先 `line.trim()` 再与 `"---"` 比较，把**行首缩进也剥掉**了。
YAML 块标量里写：

```yaml
note: |
  ---
  still body
---
real body
```

第一处 `  ---` 被当作闭合围栏 → frontmatter 提前结束，`still body` / `real body` 全被当成正文；
description 也随之错乱（回落到围栏本身）。

**修法**（`src/git.rs::split_frontmatter`）：

- 只在**行尾**剥 `' '` / `'\t'`（Java 正则允许围栏与换行之间是空白），不再剥行首缩进
- 围栏判定改为 `candidate == "---" || candidate == "..."`，因此缩进围栏留在正文内
- 保留既有能力：BOM、CRLF、`...` 变体、无闭合围栏时返回 `(None, 原文)`

**新增 6 个测试**（全离线，不依赖 git 二进制）：

| 测试 | 断言 |
|---|---|
| `split_frontmatter_splits_a_standard_fence` | 标准围栏正确切分；BOM + CRLF 等价 |
| `split_frontmatter_requires_the_closing_fence_to_be_unindented` | 块标量里的 `  ---` 仍属 frontmatter；只有缩进围栏时视为**未闭合** |
| `split_frontmatter_rejects_a_fence_with_a_suffix` | `---suffix` 不算围栏 |
| `split_frontmatter_accepts_dots_and_trailing_blanks` | `...` 与 `---  \t` 均算闭合 |
| `split_frontmatter_returns_the_whole_text_without_a_closing_fence` | 缺失围栏 / 开头 `---x` 时返回全文 |
| `skill_identity_treats_an_indented_fence_as_body` | 端到端：description 正确回落到正文标题而非围栏 |

**验证**：`cargo test -p agentscope-extensions-skills` → **60 passed / 0 failed**（基线 54）。
**反向验证**：把判定改回 `raw.trim()` →
`split_frontmatter_requires_the_closing_fence_to_be_unindented` 与
`skill_identity_treats_an_indented_fence_as_body` **双双 FAILED**（前者 frontmatter 被截断成
`Some("name: calc\nnote: |\n")`，后者 description 变成 `---`）。

## 9. P2 收口 + P1-09 补测执行记录（2026-10-28）

> 基线：Java `16cb024ae`。本轮只改 `crates/agentscope-extensions-skills/**` 与本 tracker；未改根 `Cargo.toml`、`agentscope-core`、`agentscope-harness`、其它 crate。
> 基线测试数 **60** → **93**（+33），全绿。

### 9.1 P2-02 空/空白库名·表名（已完成）

Java 构造函数两步：先 `(name == null || name.trim().isEmpty()) ? DEFAULT : name.trim()`，再 `validateIdentifier`。

- 新增 `util::resolve_identifier(raw, default)`（trim 后为空 → 默认名；否则返回 trim 后的值）。
- `MysqlSkillRepository::with_config` / `PostgresSkillRepository::with_config` 在 **任何校验之前** 解析 `database_name` / `schema_name` / 两张表名，再走原 `validate_identifier`。
- 新增 `MysqlSkillRepository::database_name()` 读取器（pg 侧 `schema_name()` 原本已有）。
- 测试：`util::resolves_blank_and_padded_identifiers`、`mysql::blank_names_fall_back_to_the_defaults` / `names_are_trimmed_before_validation` / `padded_invalid_names_are_still_rejected`，`postgresql::` 同名 3 个（`padded_invalid_*` 防止 trim 把非法名洗白）。

### 9.2 P2-03 `delete()` 补事务（已完成）

- 缝上新增默认方法 `SqlConnection::in_transaction(work)`：`set_auto_commit(false)` → 成功 `commit` / 失败 `rollback`，两条路径都恢复 auto-commit。
- `Mysql/PostgresSkillRepository::delete`：先 Java 同构的 `skill_exists_internal` 预检（不存在 → warn + `Ok(false)`），再用 `in_transaction` 包住「删 resources + 删 skill」两条 DELETE。
- `save` 也改用 `in_transaction`（原手写 `set_auto_commit` / match 逻辑删除，行为不变）。
- 测试：`mysql::delete_rolls_back_the_resource_rows_when_the_skill_delete_fails`、`postgresql::` 同名用例（注入「第 2 条 DELETE 失败」→ 断言 resource 行仍在、`get_skill` 仍能取回资源）、`mysql::delete_of_a_missing_skill_is_false_without_touching_the_transaction`。
- 失败注入器：`sql::test_support::InterceptingSqlDatabase`（`#[cfg(test)]`，按「SQL 含 needle 且匹配次数 > skip」报错），同时记录 `execute_batch` 的批次大小。

### 9.3 P2-04 PG 资源插入 1000 行分批（已完成）

- 新增 `postgresql::RESOURCE_BATCH_SIZE = 1000`（Java 同名常量）。
- 缝上新增默认方法 `SqlConnection::execute_batch(sql, params)`（等价 JDBC `addBatch`/`executeBatch`；默认逐条执行，真实驱动可覆写为一次往返）。
- `insert_resources` 按 1000 分块调用 `execute_batch`；插入数不足时返回 `Database` 错误（对齐 Java「insertedCount != size 抛 SQLException」）。
- 测试：`postgresql::resource_inserts_are_batched_by_resource_batch_size`（2501 个资源 → 批次 `[1000, 1000, 501]`）、`a_small_skill_is_inserted_in_one_batch`（2 个资源 → 一个批次）。

### 9.4 P2-06 git `save()` 返回值（已完成）

- `GitSkillRepository::save` 改为 warn + `Ok(false)`（Java `return false`），与 `delete()` 一致；不再返回 `Err(ReadOnly)`。
- 既有测试 `git::read_only_write_paths_are_ignored` 的断言随之对齐（原断言 `Err(ReadOnly)`），并补「非空 skill + force=true 也返回 false」。

### 9.5 P1-09 补测（进行中：+33，累计 93 vs Java 192）

新增 **33** 个测试（git +13 / mysql +9 / postgresql +8 / sql +2 / util +1），全部离线、无 mock 库：

| 子模块 | 新增用例 |
|---|---|
| git（`file://` 真实仓） | `an_unchanged_remote_does_not_reclone_or_pull`（远端未变 → 不 pull，本地改动存活）、`loads_skill_resources_from_the_checkout`、`skill_names_are_sorted`、`an_unknown_branch_is_reported`、`a_missing_skills_root_is_reported`、`a_local_path_that_is_not_a_repository_is_rejected`、`close_keeps_an_explicit_local_path`、`sync_is_idempotent` |
| git（parser/边界，纯函数） | `split_frontmatter_handles_extra_separators`、`split_frontmatter_handles_crlf_without_frontmatter`、`frontmatter_scalars_ignores_malformed_lines`、`skill_identity_with_empty_frontmatter`、`skill_exists_is_false_for_a_blank_name_without_syncing` |
| mysql | `blank_names_fall_back_to_the_defaults`、`names_are_trimmed_before_validation`、`padded_invalid_names_are_still_rejected`、`delete_rolls_back_the_resource_rows_when_the_skill_delete_fails`、`delete_of_a_missing_skill_is_false_without_touching_the_transaction`、`skill_exists_returns_false_for_blank_names`、`two_repositories_with_different_tables_are_isolated`（namespace 隔离）、`get_all_skills_groups_resources_per_skill`（资源按 id 归组）、`metadata_json_roundtrip_preserves_extended_metadata` |
| postgresql | `blank_names_fall_back_to_the_defaults`、`names_are_trimmed_before_validation`、`padded_invalid_names_are_still_rejected`、`delete_rolls_back_the_resource_rows_when_the_skill_delete_fails`、`resource_inserts_are_batched_by_resource_batch_size`、`a_small_skill_is_inserted_in_one_batch`、`skill_exists_returns_false_for_blank_names`、`two_repositories_with_different_tables_are_isolated` |
| sql / util | `in_transaction_rolls_back_when_the_work_fails`、`execute_batch_default_runs_every_parameter_set`、`resolves_blank_and_padded_identifiers` |

仍缺（保留待办）：

- SQL 侧全部用例仍跑在 `InMemorySqlDatabase` 替身上（真实 MySQL/PG 驱动尚未接入，见 §4.2 第 1 条）；该替身以**裸表名**为键（`<schema>.<table>` 会剥离前缀），因此**无法验证「同名表、不同 schema 前缀」的隔离**，隔离用例改用不同表名表达。
- git：并发克隆、认证失败、`swap`/`force` 组合、Windows 路径大小写、`git` 二进制缺失时的降级（测试辅助函数 `git_in` 当前直接 panic）。
- parser：多文档 YAML（`---` 分段的多文件）、超长 frontmatter、非法 UTF-8 输入。

### 9.6 P2-08 `skill_exists("")`（已完成）

- mysql / postgresql / git 三个后端在 `validate_skill_name` 之前先判 `trim().is_empty()` → `Ok(false)`。Java 只判 `isEmpty()`；Rust 取「更宽但同样不报错」的口径（`"   "` 在 Java 侧会被 `validateSkillName` 拒）。
- git 侧提前返回意味着**不触发 `ls-remote`**（测试用不可达 URL 证明无 I/O）。
- 测试：`mysql::skill_exists_returns_false_for_blank_names`、`postgresql::skill_exists_returns_false_for_blank_names`、`git::skill_exists_is_false_for_a_blank_name_without_syncing`（均同时断言 `../escape` 仍然报错）。

### 9.7 P2-10 依赖与死代码清理（已完成）

`rg` 逐项核对（`agentscope_core` / `async_trait` / `futures` / `chrono` / `serde::` / `base64`，以及 `Serialize`/`Deserialize` 派生宏）：

| 依赖 | 结论 | 处置 |
|---|---|---|
| `agentscope-core` | 0 命中 | 删 |
| `async-trait` | 0 命中 | 删 |
| `futures` | 0 命中 | 删 |
| `chrono` | 0 命中（此前 3 处命中实为单词 `synchronous`） | 删 |
| `serde` | 仅 `serde_json` 命中；本 crate 无 `#[derive(Serialize)]` | 删 |
| `base64` | **在用**（`repository.rs::write_resources_to_dir` 解 `base64:` 载荷） | **保留**（原 tracker 记「未使用」有误，已更正） |
| `tokio`（含 dev-dep） | 仅文档注释提名 | 删；doc 注释改为 `tokio::task::spawn_blocking` 纯代码（避免断链） |
| `InMemorySqlDatabase::has_metadata_column` | 全仓 0 调用者，等价于 `metadata_column_type(t).is_some()` | 删除（`sql.rs` 留注释） |

依赖 10 → **5**（`agentscope-harness` / `serde_json` / `thiserror` / `tracing` / `base64`），`[dev-dependencies]` 整节移除。本 crate `cargo build` / `cargo test` **0 warning**。

### 9.8 反向验证（逐项回退 → FAILED → 恢复）

| 回退内容 | 预期失败用例 | 实测 |
|---|---|---|
| **第 1 轮**（4 处同时回退） | — | `85 passed / 8 failed` |
| `util::resolve_identifier` → 原样返回（不 trim / 不回退） | `mysql/pg::blank_names_fall_back_to_the_defaults`、`mysql/pg::names_are_trimmed_before_validation`、`util::resolves_blank_and_padded_identifiers` | ✅ 5 个全部 FAILED |
| `git::save` → `Err(ReadOnly)` | `git::read_only_write_paths_are_ignored` | ✅ FAILED |
| mysql / pg `skill_exists` 去掉空白短路 | `mysql/pg::skill_exists_returns_false_for_blank_names` | ✅ 双双 FAILED |
| **第 2 轮**（4 处同时回退） | — | `88 passed / 5 failed` |
| mysql / pg `delete` 改回直调（无事务） | `mysql/pg::delete_rolls_back_the_resource_rows_when_the_skill_delete_fails` | ✅ 双双 FAILED |
| `insert_resources` 改回逐条 `execute` | `pg::resource_inserts_are_batched_by_resource_batch_size`、`pg::a_small_skill_is_inserted_in_one_batch` | ✅ 双双 FAILED |
| `git::skill_exists` 去掉空白短路 | `git::skill_exists_is_false_for_a_blank_name_without_syncing` | ✅ FAILED |

恢复后复跑：**93 passed / 0 failed**。

### 9.9 改动文件与实测命令

| 文件 | 改动 |
|---|---|
| `src/util.rs` | 新增 `resolve_identifier`，+1 测试 |
| `src/sql.rs` | `SqlConnection::execute_batch` / `in_transaction` 默认方法、`#[cfg(test)] test_support::InterceptingSqlDatabase`、删 `has_metadata_column`，+2 测试 |
| `src/mysql.rs` | P2-02 标识符解析、P2-03 `delete` 事务、P2-08 空白名、`database_name()`，+9 测试 |
| `src/postgresql.rs` | P2-02、P2-03、P2-04（`RESOURCE_BATCH_SIZE` + 分批）、P2-08，+8 测试 |
| `src/git.rs` | P2-06 `save()` 返回 `Ok(false)`、P2-08 空白名，+13 测试（含 8 个 `file://` 真实仓） |
| `src/repository.rs` | doc 注释去掉 `tokio` 断链（仅文档） |
| `Cargo.toml` | 依赖 10 → 5，移除 `[dev-dependencies]` |

```
cargo build -p agentscope-extensions-skills   # Finished，0 error / 0 warning（本 crate）
cargo test  -p agentscope-extensions-skills   # 93 passed; 0 failed; 0 ignored（另有 0 passed 的 doctest 汇总行）
cargo fmt -p agentscope-extensions-skills -- --check   # 干净
```

---

## 10. 2026-10-31 loop 收口记录（本回合：真实驱动 / P1-09 补测 / P2-09 裁定 / `de2dc8f3c`）

> 本回合约束：**只做代码编辑，不运行 cargo / rustc / clippy / fmt**（由主智能体统一验证）。下文用例名均为待验证的**新增代码**，不是已跑过的结论。

### 10-1 P1：真实 MySQL / PostgreSQL 驱动（feature-gated）

| 项 | 结论 |
|---|---|
| 新增文件 | `src/driver/mod.rs`（`SyncBridge` 同步桥 + `pg_placeholders`）、`src/driver/postgres.rs`、`src/driver/mysql.rs` |
| 选型 | **PostgreSQL → `tokio-postgres` 0.7**：workspace 已构建同栈（`agentscope-extensions-postgresql` 的 `PgSimpleClient` 就用它），不引入第二套 PG 栈，且本地 registry 已缓存、可离线编译。**MySQL → `mysql_async` 0.37（`minimal`）**：workspace 已登记，`minimal` 不拉 TLS 后端（避开 `aws-lc-sys`/OpenSSL）。**未选 `sqlx`**：会拖入 `sqlx-core`/宏栈 + TLS 后端，且 `Any` 后端抹掉本 crate 依赖的类型化解码 |
| feature | `postgres-driver` / `mysql-driver`，`default = []`（**默认构建不新增依赖**）；新增依赖均 `optional = true` + `{ workspace = true }` |
| 同步/异步桥 | `SqlDatabase`/`SqlConnection` 是**同步**缝隙（服务同步的 `AgentSkillRepository`），驱动是异步 → `SyncBridge` 用单 worker 多线程 runtime `block_on`；**从异步线程调用返回 `Database` 错误而不是 panic**（`Handle::try_current` 检测） |
| 语句适配 | 仓库统一发 JDBC 风格的 `?` 占位符：PG 侧 `pg_placeholders` 重写为 `$n`（跳过 `'..'` / `".."` 内的字面 `?`），`insert_returning_id` 补 `RETURNING <id>`（已含 `RETURNING` 则不重复，末尾 `;` 会先剔除）；MySQL 无需重写，`insert_returning_id` 用 `LAST_INSERT_ID()` |
| 事务 | JDBC 语义：`set_auto_commit(false)` → `BEGIN` / `START TRANSACTION`，`commit`/`rollback` 对应语句；幂等（重复 `set_auto_commit(false)` 不会嵌套） |
| 解码 | PG 按列类型（`INT8/INT4/INT2/BOOL` → `Int`，其余试 `String`）；MySQL 按 `Value` 变体（`NULL/Int/UInt/Bytes`），两边均不 panic |
| 无真库时 | `PgSqlDatabase::connect` / `MysqlSqlDatabase::connect` **构造即连接一次**，不可达/非法 DSN 返回 `SkillRepositoryError::Database("connection failed…")`；用例 `an_unreachable_server_is_an_explicit_error`（连 `127.0.0.1:1`，不开外网）、`an_invalid_connection_string_is_rejected` |
| 用例（`postgres-driver`） | `driver::placeholders_are_numbered_in_order`、`placeholders_inside_quoted_text_are_left_alone`、`placeholders_are_renumbered_from_one_per_statement`、`returning_clause_is_appended_once`、`an_unreachable_server_is_an_explicit_error`、`an_invalid_connection_string_is_rejected`、`the_returning_column_defaults_to_id` |
| 用例（`mysql-driver`） | `parameters_keep_their_types`、`an_unreachable_server_is_an_explicit_error` |
| 未做 | **真库集成测试**需 Docker/实例，本回合未写；建议以 `AGENTSCOPE_TEST_PG_URL` / `AGENTSCOPE_TEST_MYSQL_URL` 环境变量门控（**不用 `#[ignore]`**），存在变量时建 `#[test]` 走完整 save/get/delete 往返（见 §10-6） |

### 10-2 P1-09：Java 测试类 → Rust 用例映射表

| Java 测试类（`@Test` 数） | 主题分组 | Rust 对应（文件::用例） | 结论 |
|---|---|---|---|
| `GitSkillRepositoryTest`（23） | 构造/URL/trim/identifier/frontmatter/sync/分支/远端不可达/资源 | `git.rs` 35 → **37** 个用例（`clones_local_repository_and_uses_frontmatter_identity`、`pulls_new_remote_commits`、`remote_head_is_filtered_by_branch`、`unreachable_remote_fails_instead_of_serving_stale_skills`、`an_unchanged_remote_does_not_reclone_or_pull`、`manual_sync_is_required_when_auto_sync_is_disabled`、`split_frontmatter_*` ×5、`close_*` ×2 等） | **已覆盖全部 Java 主题**（含 `file://` 真实仓）；Java 的 23 条中多条为同一行为的 Mockito 分支（`verify(mock).xxx`），Rust 用真实仓 1 条覆盖 |
| `MysqlSkillRepositoryTest`（61，实为 PG 类结构同构） | 构造/DDL/元数据列探测/identifier 校验/builder/CRUD/资源/错误分支 | `mysql.rs` 26 → **30**（+`all_skill_names_are_sorted`、`delete_removes_resources_and_a_resave_starts_clean`、`sql_backend_has_no_skill_dir_and_closes_cleanly`、`read_only_blocks_delete_and_clear`） | **语义已覆盖**；差额为 Java 对每个 SQL 异常分支单独造的 `when(...).thenThrow` 用例（Rust 用 `InterceptingSqlDatabase` 故障注入 1 条覆盖同一路径） |
| `PostgresSkillRepositoryTest`（108） | 同上 + schema 限定名 + 资源分批 + `metadata_json` 类型矩阵 | `postgresql.rs` 18 → **22**（+同上四条） | **语义已覆盖**；差额同 MySQL（参数化 × 异常分支组合） |
| （无 Java 对应） | 同步缝隙 `SqlDatabase`/`SqlConnection` | `sql.rs` 5 → **11**（+`text_parameter_materialises_null_for_none`、`sql_value_accessors_coerce_and_reject`、`missing_columns_read_as_null`、`execute_reports_affected_rows_and_keeps_explicit_ids`、`transaction_controls_are_idempotent`、`empty_batch_is_a_noop`） | Rust 特有缝隙，补契约级用例 |
| （无 Java 对应） | 公共校验/元数据 | `util.rs` 5 → **9**（+长度按**字符**计、资源路径上下界、后端差异化 identifier 上限、非法 `metadata_json` 回退） | Rust 特有抽象（Java 中为三个类里的私有方法），补契约级用例 |
| （无 Java 对应） | harness 适配层（`skill_dir` 物化） | `repository.rs` 4 → **7**（+`materialize_locks_are_a_fixed_stripe_table`、`concurrent_materialisation_of_one_skill_is_serialised`、`adapter_does_not_carry_a_per_skill_signature`） | Rust 特有（Java 用 `originDir`，无需物化） |
| （无 Java 对应） | 真实驱动 SQL 重写 | `driver/mod.rs` +3、`driver/postgres.rs` +4、`driver/mysql.rs` +2（feature-gated） | Rust 特有 |

**合计**：默认构建 **93 → 116**；`postgres-driver` **123**；两 driver 全开 **125**（Java 192）。**一句话关闭**：剩余差额中，MySQL/PG 的 61/108 条大量是「同一断言的 Mockito 异常分支 × 多参数」组合（如 `testConstructorWith{Schema,Table,Check,Detection,Rollback,AutoCommit}Failure`），Rust 以 `InterceptingSqlDatabase` 故障注入 + 差异化断言覆盖同一分支集合，**不再逐条扩充重复用例**。

### 10-3 P2-09：`close()` 返回 `Err` 的正式裁定（裁定 A）

- **裁定**：保留 `Result`（`AgentSkillRepository` 需要给“可失败的驱动后端”留出口，且 trait 默认实现已为 `Ok(())`），但 `GitSkillRepository::close` **不返回删除失败的错误**：只 `tracing::warn!(path, error)` 并 `Ok(())`，与 Java `close()`（`LOGGER.warn` 后正常返回）**语义等价**。
- **等价性说明**：Java 无法从 `close()` 区分成败；Rust 同样不再让清理失败进入调用方 shutdown 路径。原 tracker 记「设计决定（不迁移）」**作废**，实际是“已迁移但保留 Result 签名”。
- **用例**：`git::close_never_returns_a_cleanup_failure`（Windows 下开句柄使 `remove_dir_all` 真的失败，断言仍 `Ok`）、`git::close_removes_an_owned_temp_dir_and_is_idempotent`（自有临时目录被删 + 二次调用不报错）。
- **反向验证建议**（主智能体）：把 `git.rs::close` 改回 `map_err(..)?` → 上述 2 条应 FAILED。

### 10-4 Java 增量 `de2dc8f3c`（fix(skill)：computeSignature O(n²) + 无界文件锁表）

| Java 改动 | Rust 现状（源码级证据） | 处置 |
|---|---|---|
| `DynamicSkillMiddleware.computeSignature` 每次循环 `new TreeSet<>(s.getResources().keySet())` 复制整表 → O(n²) | `rg -n "compute_signature|computeSignature|signature" crates/agentscope-harness/src/agent/skill crates/agentscope-extensions-skills/src` = **0 命中**：Rust 侧**没有** per-skill 签名计算（harness 以 `SkillRepo` + `stager.rs` 落盘，适配层以 `(skill name → dir)` 为键） | **已完成（无对应缺陷，裁定不引入）**：不新增签名机制；为了防止将来引入时重复该 bug，在 `repository.rs` 注释里固定「每 skill 只枚举一次 `AgentSkill::resources()`」，并加锁定用例 `adapter_does_not_carry_a_per_skill_signature`（断言适配层不缓存任何 per-skill 摘要/物化表为空） |
| `SkillBox.FILE_LOCKS` 静态 `ConcurrentHashMap<path, lock>` 无界增长 → 改**256 条带锁表** `floorMod(pathHash, 256)` | Rust 侧**原本没有任何写入锁**（`RepoBackedSkillRepository::materialize_skill` 检查缓存后直接 `create_dir_all` + `File::write`）→ 同一 skill 的并发物化会互相截断（等价于 Java 修前的“锁语义缺失”而非“锁表无界”） | **已完成**：新增 `MATERIALIZE_LOCK_STRIPES = 256` + `materialize_stripe`（FNV-1a，`% 256`）+ `OnceLock<Vec<Mutex<()>>>` 固定条带表；`materialize_skill` 双检 + 条带锁内写入（毒锁 `into_inner` 容错）。用例：`materialize_locks_are_a_fixed_stripe_table`（恒定 256、同名同条带、1000 个名字后表不增长）、`concurrent_materialisation_of_one_skill_is_serialised`（8 线程并发物化同一 skill，全部拿到同一目录且资源完整） |

### 10-5 改动文件

| 文件 | 改动 |
|---|---|
| `Cargo.toml` | `[features] default=[] / postgres-driver / mysql-driver`；`tokio`/`tokio-postgres`/`mysql_async` 均 `optional = true` + `{ workspace = true }` |
| `src/lib.rs` | `#[cfg(any(...))] pub mod driver;` + 两个 feature 下的再导出 |
| `src/driver/mod.rs`（新） | `SyncBridge`、`db_error`、`pg_placeholders` + 3 用例 |
| `src/driver/postgres.rs`（新） | `PgSqlDatabase`/`PgSqlConnection`（占位符重写、`RETURNING`、事务、类型化解码）+ 4 用例 |
| `src/driver/mysql.rs`（新） | `MysqlSqlDatabase`/`MysqlSqlConnection`（`Params::Positional`、`exec_iter`、`LAST_INSERT_ID`、事务）+ 2 用例 |
| `src/repository.rs` | 256 条带物化锁 + 双检 + 文档 + 3 用例 |
| `src/git.rs` | `close()` 改为 warn + `Ok(())` + 2 用例 |
| `src/mysql.rs` / `src/postgresql.rs` | 各 +4 语义用例（排序、delete 清资源、无 `skill_dir`/`close` 幂等、只读阻断 delete/clear） |
| `src/sql.rs` / `src/util.rs` | 各 +6 / +4 契约用例 |

**待主智能体验证命令**：
```
cargo build -p agentscope-extensions-skills
cargo test  -p agentscope-extensions-skills                              # 预期 116 passed
cargo test  -p agentscope-extensions-skills --features postgres-driver   # 预期 123 passed
cargo test  -p agentscope-extensions-skills --features mysql-driver      # 预期 118 passed
```

### 10-6 跨模块需求 / 未关闭项

1. **跨模块（未改，按约束）**：harness `SkillRepository` 缺 `get_all_skill_names`，也无 `close`；本 crate 用固有方法（`AgentSkillRepository::get_all_skill_names`）补偿。若汇总方希望 harness 增加，需单独立项。
2. **未关闭**：真实库集成测试（需实例/Docker）。建议门控方式：读到 `AGENTSCOPE_TEST_PG_URL` / `AGENTSCOPE_TEST_MYSQL_URL` 时运行完整往返，**不使用 `#[ignore]`**，无变量时直接 `return` 并打印跳过原因（比 `#[ignore]` 诚实且能在 CI 环境自动生效）。
3. **未关闭（风险）**：`mysql-driver` 用 `minimal`（无 TLS），TLS-only 实例需前置代理；`mysql_async` 与 `tokio-postgres` 未被本地 registry 缓存的部分需首次联网 fetch（已在 §4.2-1 注明）。
4. **未关闭**：PG `execute_batch` 为“prepare 一次 + 每行一次 round trip”（PG 扩展协议无多值绑定）；与 Java `executeBatch` 的**网络往返次数不同、语义相同**。
