# SKILLS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **SKILLS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-skills\`
> **Java 子模块**：skill-git-repository, skill-mysql-repository, skill-postgresql-repository
> **Rust 目标**：`crates/agentscope-extensions-skills/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-skills` |
| Java 子模块 | skill-git-repository, skill-mysql-repository, skill-postgresql-repository |
| Java 规模（初测） | 主代码 ~3177 行 / 测试 ~2813 行 |
| Rust crate | `crates/agentscope-extensions-skills` |
| 迁移波次 | C |
| 当前状态 | 已完成（测试全绿） |

## 2. Java 清单与依赖映射

> **调研结论（波次 C，已实测）**：Java `agentscope-extensions-skills` 是 3 个**互相独立**的 Maven 子模块，分别实现 core 的 `AgentSkillRepository` 接口（git / mysql / postgresql 后端）。三者**不共享**公共基类代码，但重复了同一套契约。

### 2.0 与 harness `skill/` 存量代码的关系（**裁定，见 §4.2 汇总登记**）

**裁定：扩展 crate 实现一个 driver-agnostic 的 `AgentSkillRepository` trait（keyed CRUD），并**复用** harness 的 `AgentSkill`/`Metadata`/`SkillResources` 模型与 `SkillRepository` trait；两侧通过适配器 `RepoBackedSkillRepository` 打通，不重复发明存储模型。**

依据（codegraph 实测 harness `crates/agentscope-harness/src/agent/skill/`）：

| harness 既有 | 位置 | 与 Java 扩展仓库的关系 |
|---|---|---|
| `SkillRepository`（`repo_id/source/is_writeable/repo_kind/load/write_skill/delete_skill/skill_dir`，同步、目录导向） | `skill/repository.rs` | harness 组合器 `compose_skill_repositories` 消费的是 `load() -> Vec<AgentSkill>`。扩展仓库**不重建**它，而是提供适配器让其可被 harness 直接挂载 |
| `AgentSkill` + `AgentSkillBuilder` + `Metadata` + `SkillResources` | `skill/model.rs` | **唯一存储模型**，扩展仓库的读写全部以它为返回/入参类型；**不**自定义平行 skill 类型 |
| `FileSystemSkillRepository` / `WorkspaceSkillRepository` | `skill/repository.rs` | Git 仓库 = 「远程 FS 仓库」：clone 到临时目录后把 `load()` 委托给同一套目录扫描逻辑 |
| `SkillRepo` / `load_skills_from_dir` / `sha256_hex` | `skill/stager.rs` | 扩展仓库以 `SkillRepo::External{source}` 参与 staging；Git 的 `source` 命名沿用 Java `git-<owner/repo>@<branch>` |
| core 尚无独立 `skill` 模块（`agentscope-core` 无 `AgentSkill`） | — | 模型当前落在 harness，本次**不搬移、不新增 core 类型**（避免跨 crate 改动） |

结论：**Java 的 `AgentSkillRepository` 与 harness `SkillRepository` 是不同抽象层次**——前者是「后端 keyed CRUD」，后者是「harness 可消费的目录式只读装载」。本 crate 实现前者（后端适配），并提供后者适配器（`RepoBackedSkillRepository`），从而既覆盖 Java 行为、又接入 harness 消费路径，**零重复模型**。

**Java 源码核实（2026-09-19，已定案，裁定生效）**：
- Java 侧 `agentscope-extensions-skills` 模块**只有 repository 实现**，无业务逻辑：`agentscope-extensions-skill-git-repository/.../repository/GitSkillRepository.java`、`-skill-mysql-repository/.../repository/mysql/MysqlSkillRepository.java`、`-skill-postgresql-repository/.../repository/postgresql/PostgresSkillRepository.java`（模块根 `src/` 下无 `.java`）。
- 接口 `AgentSkillRepository` 位于 **core**：`agentscope-core/src/main/java/io/agentscope/core/skill/repository/AgentSkillRepository.java`（正是扩展模块包名前缀 `io.agentscope.core.skill.repository` 的来源），由 core 的 `ReActAgent` 与 `DynamicSkillMiddleware` 消费。
- ⇒ **Java 分层 = 【core：接口 + 模型】【extensions-skills：后端实现】**。本 crate 的分工与之一致；唯一差异是接口**落在 Rust harness 而非 core**（因 `agentscope-core` 无 `skill` 模块，见上表末行），属可接受的**归属位移而非层次错位**。**裁定维持原结论。**
- 残留观察（非本 crate 职责）：Java 侧该接口被 **core 的 `ReActAgent`** 消费；Rust 侧对应 trait 仅在 harness。若将来 core 的 agent 也需挂载 skill 仓库，需重新讨论是否把 trait 上提到 core。

| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `io.agentscope.core.skill.repository.AgentSkillRepository`（core 接口） | skill 仓库契约：`getSkill/getAllSkillNames/getAllSkills/save/delete/skillExists/getRepositoryInfo/getSource/isWriteable/close` | `AgentSkillRepository`（本 crate，`src/repository.rs`） | 复用 harness `AgentSkill`；用 `Result` 表达错误，不再靠返回 bool + 抛异常混用 |
| `AgentSkillRepositoryInfo`（core） | 仓库元信息：type/location/writable | `AgentSkillRepositoryInfo`（本 crate） | 字段一一对应 |
| `GitSkillRepository` | 只读；把远程 Git 仓库 clone 到临时目录，按 `skills/` 约定或 `skillsRoot` 加载；读操作触发同步 | `git::GitSkillRepository` | 走系统 `git` CLI（对齐 Java「依赖系统 Git 凭据」），无需 `git2` C 依赖 |
| `MysqlSkillRepository` | MySQL 两张表（`agentscope_skills` / `agentscope_skill_resources`）CRUD；`createIfNotExist`；`metadata_json` 兼容探测 | `mysql::MysqlSkillRepository` | 通过 `SqlDatabase` 抽象（见 2.1），默认内存实现可测 |
| `PostgresSkillRepository` | PostgreSQL schema+两张表 CRUD；`metadata_json` 兼容探测；identifier 双引号转义 | `postgresql::PostgresSkillRepository` | 同上；identifier 最大长度 63（Java）/ schema 概念 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `javax.sql.DataSource` + JDBC `Connection/PreparedStatement/ResultSet` | 数据库连接与参数化查询 | **本 crate 内 `SqlDatabase`/`SqlConnection`/`SqlRow` trait**（`src/sql.rs`） | 无法直接映射 JDBC；以最小 trait seam 承载「参数化查询 + 事务」，默认 `InMemorySqlDatabase` 供测试，真实驱动由 feature 后接（记 §4.2） |
| `org.eclipse.jgit`（clone/ls-remote/pull） | Git 操作 | 系统 `git` CLI（`std::process::Command`） | 对齐 Java 依赖系统 Git 配置/凭据；避免引入 `git2`（未在 Cargo.lock，需汇总方加 workspace 依赖） |
| Jackson `JsonUtils` | metadata JSON 序列化 | `serde_json` | 已具备 |
| SLF4J | 日志 | `tracing` | 已具备 |
| JUnit + Mockito | 测试 | `#[cfg(test)]` + 内存 `SqlDatabase` / 本地 `file://` git 仓 | 不引入 mock 库，用真实内存实现 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + 依赖 | — | 已完成 | 见 §3.2；`cargo build` 通过 |
| `AgentSkillRepository` trait + `AgentSkillRepositoryInfo` | core `AgentSkillRepository` | 已完成 | `src/repository.rs`，复用 harness `AgentSkill` |
| `SqlDatabase` 抽象 + 内存实现 | `DataSource` | 已完成 | `src/sql.rs`；真实驱动待办（§4.2） |
| `GitSkillRepository` | `GitSkillRepository` | 已完成 | `src/git.rs`，`git` CLI；skills_root 校验已修复 |
| `MysqlSkillRepository` | `MysqlSkillRepository` | 已完成 | `src/mysql.rs`，两表 + metadata_json 兼容 |
| `PostgresSkillRepository` | `PostgresSkillRepository` | 已完成 | `src/postgresql.rs`，schema + 两表 |
| `RepoBackedSkillRepository`（harness 适配器） | — | 已完成 | `src/repository.rs` |
| 测试 | 3 个 `*Test` | 已完成 | 33 passed / 0 failed（内存 SQL + file:// git） |

## 4. 项目状态与下一步
### 4.1 当前状态

**状态：已完成（测试全绿）。** 波次 C，crate `crates/agentscope-extensions-skills` 实现 3 个后端仓库 + harness 适配器。

**测试修复完成（2026-09-18）**：`cargo build -p agentscope-extensions-skills` 通过；`cargo test -p agentscope-extensions-skills` → **33 passed / 0 failed**（含 doc-tests）。

修复的 6 个失败用例（均为**实现 bug**，非测试期望错误；已对照 Java `skill-git-repository` / `skill-mysql-repository` / `skill-postgresql-repository` 源码确认语义）：

| 失败用例 | 根因 | 修法（文件） |
|---|---|---|
| `sql::tests::create_insert_query_roundtrip`（`table_exists("agentscope_skills")` 为 false） | `InMemoryConnection::parse_table` 只识别 `" from "`；`CREATE TABLE ...` 无 `from` → 表名被解析为首个词 `CREATE`，表注册到错误名下 | 扩展 `parse_table` 识别 `CREATE TABLE [IF NOT EXISTS]` / `CREATE SCHEMA [IF NOT EXISTS]` / `INSERT INTO`，并兼容引号/反引号（`sql.rs`） |
| `mysql::tests::create_tables_and_detect_metadata_column` + `postgresql::tests::default_schema_and_tables`（`metadata_json_column_supported()` 为 false） | `InMemoryConnection::query` 先命中宽泛的 `information_schema` 分支，导致 `information_schema.columns` 探测永不执行；且存在性探测用首个文本参数（schema 名）当表名，永远查不到 | 将 `information_schema.columns` 探测**前置**，取**第 2 个**文本参数为表名（对齐 Java `WHERE TABLE_SCHEMA=? AND TABLE_NAME=? AND COLUMN_NAME=?`）；宽泛存在性探测改为在任意文本参数中命中 `tables`/`metadata_columns`（`sql.rs`） |
| `mysql::tests::save_then_get_roundtrip_preserves_resources` + `postgresql::tests::save_then_get_roundtrip`（resource 取回为 `None`） | `insert_resources` 走 `conn.execute("INSERT INTO ... VALUES (?,?,?)")`，但内存实现 `execute` 只处理 `CREATE TABLE` / `DELETE`，INSERT 被静默丢弃（Java 侧资源走 `executeUpdate` 真实落库） | 在 `execute` 中新增 `INSERT` 处理：解析列/参数构造行，尊重显式 `id`（resources 显式传 id），否则自增；按 auto_commit 提交或暂存（`sql.rs`） |
| `git::tests::skills_root_rejects_absolute_and_parent`（`skills_root("/abs")` 未被拒绝） | Windows 上 `Path::new("/abs").is_absolute()` 返回 false（需盘符/UNC），而 Java `Path.isAbsolute()` 按 POSIX 视为绝对 → 校验漏放绝对路径 | `normalize_skills_root` 在 `is_absolute()` 之外，额外拒绝以 `/` 或 `\` 开头的原始字符串（POSIX 语义）（`git.rs`） |

验证命令与结果：
```
cargo build -p agentscope-extensions-skills   # Finished，0 error（仅 core/harness 既有 warning 与 1 个本 crate 既有 dead_code warning）
cargo test  -p agentscope-extensions-skills   # 33 passed; 0 failed
```

### 4.2 下一步与阻塞
1. 真实 MySQL / PostgreSQL 驱动接入（`SqlDatabase` 的真实实现）仍为待办，需在汇总 tracker §4.2 登记 workspace 依赖（如 `sqlx`/`mysql_async`/`tokio-postgres`）；当前仅内存实现可测。
2. Git 仓库读写/同步路径需系统 `git` CLI，未纳入离线单测（与 Java 依赖系统 Git 凭据一致）。
3. 本 crate 仍有 1 个既有 `dead_code` warning（`InMemorySqlDatabase::has_metadata_column` 未使用）与 core/harness 既有 warning，非本次修复引入，未改动核心/harness。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-skills` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-skills` / `cargo test -p agentscope-extensions-skills` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-skills/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

