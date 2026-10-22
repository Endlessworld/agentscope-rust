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
| Java 规模（2026-10-22 静态实测） | 主代码 **3529 物理行 / 3197 非空行**；测试 **3432 物理行 / 2813 非空行**（原记「~3177 / ~2813」是**非空行口径**，与实测一致，非「偏低」，见 §静态复核记录 → §3.2） |
| Java 类数（静态实测） | 主代码 3 文件 / 3 顶层类 + 6 嵌套类 = **9 个类型**；测试 3 文件 / 12 个嵌套测试类 / **192 个 `@Test`**（git 23 + mysql 61 + pg 108） |
| Rust crate | `crates/agentscope-extensions-skills` |
| Rust 规模（静态实测） | 8 个 `.rs` / **3027 物理行 / 2753 非空行**（纯代码 2435；其中 `#[cfg(test)]` 456 行，实现 2571 行）/ **33 个 `#[test]`**（0 个 `#[tokio::test]`、0 doc-test，源码无 ``` 代码块） |
| 迁移波次 | C |
| 当前状态 | 进行中（编译与测试全绿，但静态复核发现 **P0×1 / P1×10 / P2×10** 语义缺口，见 §3.2） |

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
| `AgentSkillRepositoryInfo`（core） | 仓库元信息：type/location/writable | `AgentSkillRepositoryInfo`（本 crate） | 字段一一对应（`repo_type`/`location`/`writable`），`Display` 格式与 Java `toString` 一致 |
| `setWriteable(boolean)`（core 接口方法） | 运行时切换只读/可写 | **无对应** | Rust trait 无 `set_writeable`，`writeable` 在构造后不可变（`is_writeable()` 只读）。Java 三个实现均有该方法，属接口级缺口（§3.2 P2-01） |
| `GitSkillRepository` | 只读；把远程 Git 仓库 clone 到临时目录，按 `skills/` 约定或 `skillsRoot` 加载；读操作触发同步 | `git::GitSkillRepository` | 走系统 `git` CLI（对齐 Java「依赖系统 Git 凭据」），无需 `git2` C 依赖。**语义差异**（§静态复核记录）：技能名取自**目录名**（Java 取 SKILL.md frontmatter 的 `name`）；**description 取 SKILL.md 首个非空行**（Java 取 frontmatter 的 `description`，Rust 对 frontmatter 文件会返回 `---`）；`ls-remote` 未按 `refs/heads/<branch>` 过滤；remote 不可达时 Java 会 pull、Rust 跳过；`save()` Rust 返回 `Err(ReadOnly)`（Java 返回 `false`） |
| `MysqlSkillRepository` | MySQL 两张表（`agentscope_skills` / `agentscope_skill_resources`）CRUD；`createIfNotExist`；`metadata_json` 兼容探测 | `mysql::MysqlSkillRepository` | 通过 `SqlDatabase` 抽象（见 2.1），默认内存实现可测。**语义差异**：未 `CREATE DATABASE`、表名不带 `<db>.` 前缀、`createIfNotExist=false` 时不做存在性校验、`get_source()` 返回 `"mysql"`（Java 为 `mysql_<db>_<table>`）、DDL 缺 `utf8mb4` 字符集、空/空白表名不回退默认值（Java 回退）、`delete()` 无事务（§静态复核记录） |
| `PostgresSkillRepository` | PostgreSQL schema+两张表 CRUD；`metadata_json` 兼容探测；identifier 双引号转义 | `postgresql::PostgresSkillRepository` | 同上；identifier 最大长度 63（Java）/ schema 概念。**语义差异**：探测不校验 `DATA_TYPE ∈ {text, character varying, character}`（Java 对 `jsonb` 返回 false）；`get_source()` 返回 `"postgresql"`（Java 为 `postgresql_<schema>_<table>`）；资源无 1000 行分批；空/空白表名不回退默认值；`delete()` 无事务（§静态复核记录） |

> **上游状态提醒（2026-10-22 静态核对）**：Java `MysqlSkillRepository` / `PostgresSkillRepository` 均标注 `@Deprecated`，官方替代是 `agentscope-extensions-jdbc` 的 `JdbcAgentSkillRepository`（**1184 物理行 / 1099 非空行**，namespace 列 + 方言抽象，配套 `dialect/table/SkillDialect.java` / `SkillResourcesDialect.java`）。本 crate 对齐的是**已弃用的 Java 实现**；Rust `agentscope-extensions-jdbc` 目前**无 skill 模块**（`rg -i skill` = 0 命中）。是否补 `JdbcAgentSkillRepository` 需汇总方裁定（§3.2 P0-01）。

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
| `RepoBackedSkillRepository`（harness 适配器） | — | 已完成 | `src/repository.rs`；**静态复核发现未覆写 `skill_dir`**（见 §3.2 P1-08） |
| 测试 | 3 个 `*Test` | 已完成 | 33 passed / 0 failed（内存 SQL + file:// git）；**Rust 仅 5 个 git 测试，Java 有 23 个**（§3.2 P1-09） |

### 3.2 静态复核发现的语义缺口（2026-10-22，未编译、仅静态核对）
> 状态取值沿用 §0：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
> 优先级：P0=核心链路阻断 / P1=重要行为不一致 / P2=增强或健壮性。

| # | 缺口项 | 子模块 | 优先级 | Java 行为 | Rust 现状 | 执行状态 |
|---|---|---|---|---|---|---|
| P0-01 | `JdbcAgentSkillRepository`（官方替代实现）缺失 | mysql / postgresql | P0 | `agentscope-extensions-jdbc` 的 `JdbcAgentSkillRepository`（1184 物理行）+ `SkillDialect` / `SkillResourcesDialect`，带 `namespace` 列隔离 | `agentscope-extensions-jdbc` **无 skill 模块**（`rg -i skill` = 0 命中）；本 crate 只对齐**已 `@Deprecated`** 的旧实现 | 待办（需汇总方裁定归属：本 crate 内补 vs jdbc crate 内补） |
| P1-01 | git 技能名来源不一致 | git | P1 | `SkillFileSystemHelper.getAllSkillNames` → `readSkillName` 解析 **SKILL.md frontmatter 的 `name`** | `git.rs` 委托 harness `load_skills_from_dir`，取**目录名**；目录名 ≠ frontmatter name 时 `get_skill(name)` 找不到 | 待办 |
| P1-02 | git 技能 description 不一致 | git | P1 | `SkillUtil.createFrom` → `MarkdownSkillParser.parse` 取 frontmatter 的 `description` | harness `skill_description()` 取 SKILL.md **首个非空行**并剥 `#`；对 frontmatter 文件会返回 `---` | 待办（根因在 harness `load_skills_from_dir`，需与 harness 层协同） |
| P1-03 | git 分支远端探测未按分支过滤 | git | P1 | `resolveRemoteHead` 用 `lsRemote.setHeads(true)` + `matchesBranch`（`refs/heads/<branch>`） | `remote_head()` 固定 `git ls-remote <url> HEAD`（远端默认分支），未按配置的 `branch` 过滤 | 待办 |
| P1-04 | git 远端不可达时的降级行为不一致 | git | P1 | `resolveRemoteHead` 异常 → 返回 null → `hasRemoteUpdates` 返回 **true** → 执行 pull 并抛错 | `remote_head()` 返回 `None` → `has_remote_updates` 返回 **false** → **静默跳过 pull**、继续用旧数据 | 待办 |
| P1-05 | MySQL 未建库 / 表名未带库前缀 | mysql | P1 | `createDatabaseIfNotExist`（`CREATE DATABASE IF NOT EXISTS ... utf8mb4`）+ `getFullTableName` = `<db>.<table>` | 只建表；所有 SQL 用裸表名（`<table>`） | 待办 |
| P1-06 | MySQL `createIfNotExist=false` 不校验存在性 | mysql | P1 | `verifyDatabaseExists` + `verifyTablesExist`，缺表抛 `IllegalStateException` | **完全跳过校验**，后续查询静默返回空集 | 待办 |
| P1-07 | PostgreSQL `metadata_json` 类型探测缺失 | postgresql | P1 | `isSupportedMetadataJsonColumnType`：`{text, character varying, character}`，`jsonb` → **false**（走 legacy 路径） | 只判列是否存在，不看 `DATA_TYPE`；`jsonb` 会被当作支持 → 写 `metadata_json` 报错 | 待办 |
| P1-08 | 适配器未透出 `skill_dir` | 全部 | P1 | Java `AgentSkill` 带 `originDir`，`load_skill_through_path` 可定位磁盘目录 | `RepoBackedSkillRepository` 未覆写 `skill_dir` → harness `skill_middleware::load_index` 收不到目录 → **加载工具对 git/SQL 技能不可用** | 待办 |
| P1-09 | 测试覆盖远低于 Java | 全部 | P1 | 192 个 `@Test`（git 23 / mysql 61 / pg 108） | 33 个 `#[test]`（git **5** / mysql 12 / pg 7 / sql 3 / util 4 / repository 2）；**git 侧无 file:// 真实克隆/拉取/分支用例** | 待办 |
| P1-10 | `get_source()` 命名不含库/表/模式 | mysql / postgresql | P1 | mysql → `mysql_<db>_<table>`；pg → `postgresql_<schema>_<table>` | 固定返回 `"mysql"` / `"postgresql"`（多实例时 `source` 冲突，`RepoBackedSkillRepository` 的 `repo_id` 也随之冲突） | 待办 |
| P2-01 | 缺 `set_writeable` | 全部 | P2 | Java 接口有 `setWriteable(boolean)`，三个实现均可运行时切换 | Rust trait 无该方法，`writeable` 构造后不可变 | 设计决定（不迁移）— Rust 用配置表达，**需汇总方确认** |
| P2-02 | 空/空白库名·表名不回退默认值 | mysql / postgresql | P2 | null/空 → 回退默认名；并 `trim()` | `validate_identifier("")` 直接报错；含空格也报错（Java `trim()` 后可用） | 待办 |
| P2-03 | `delete()` 无事务 | mysql / postgresql | P2 | Java `delete` 开事务（`setAutoCommit(false)` → commit/rollback） | Rust `delete_skill_internal` 连发 2 条 DELETE（resources + skill）且**不开事务** → 中途失败留孤儿资源 | 待办 |
| P2-04 | PostgreSQL 资源插入无 1000 行分批 | postgresql | P2 | `RESOURCE_BATCH_SIZE = 1000` 分块 `executeBatch`（避开绑定参数上限） | 逐条 `execute`，无分批 | 待办（真实驱动落地后处理） |
| P2-05 | MySQL DDL 缺 utf8mb4 字符集 | mysql | P2 | `DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`（两张表均带） | Rust 建表 DDL 未带字符集/排序规则 | 待办 |
| P2-06 | git `save()` 返回值不一致 | git | P2 | Java 只读仓 `save()` **返回 false** | Rust 返回 `Err(SkillRepositoryError::ReadOnly)` | 待办（`delete()` 已是 `Ok(false)`，与之不一致） |
| P2-07 | git remote URL 未 trim | git | P2 | 构造时 `remoteUrl.trim()` 后保存 | 只用 `trim().is_empty()` 判空，**原样保存**未 trim 的 URL | 待办 |
| P2-08 | `skill_exists("")` 行为不一致 | mysql / postgresql | P2 | Java `skillExists(null/空)` **返回 false**，不抛异常 | Rust `validate_skill_name` → `Err(InvalidArgument)` | 待办 |
| P2-09 | `close()` 失败语义不一致 | git | P2 | Java `close()` 删目录失败只 `warn`，不抛 | Rust `close()` 返回 `Err(Io)` | 设计决定（不迁移）— Rust 用 Result 表达，**需汇总方确认** |
| P2-10 | 未使用的依赖与死代码 | 全部 | P2 | — | `Cargo.toml` 的 `agentscope-core` / `async-trait` / `futures` / `chrono` / `base64` / `serde`（derive）**均未在 src 中使用**；`InMemorySqlDatabase::has_metadata_column` 为 dead code（既有 warning） | 待办（清理依赖需汇总方登记 §4.2） |

**分层结论（与 harness `skill/` 存量代码核对，无重复实现）**：
- harness `agent/skill/repository.rs` 提供**目录式** `SkillRepository`（`load()` → `Vec<AgentSkill>`）+ `FileSystemSkillRepository` / `WorkspaceSkillRepository`；本 crate 提供**后端 keyed CRUD** `AgentSkillRepository` + `RepoBackedSkillRepository` 适配器。两者是**不同抽象层次**，模型统一复用 harness `AgentSkill` / `Metadata` / `SkillResources`，**未发现重复的 skill 模型或重复的目录扫描实现**（git 侧直接调用 harness 的 `load_skills_from_dir`）。
- 唯一需补的衔接点：`RepoBackedSkillRepository` 应覆写 `skill_dir`（P1-08），否则 harness 的 `load_skill_through_path` 对非文件系统仓库无效。

## 4. 项目状态与下一步
### 4.1 当前状态

**状态：进行中（编译与测试全绿，但存在未收敛的语义缺口）。** 波次 C，crate `crates/agentscope-extensions-skills` 实现 3 个后端仓库 + harness 适配器。**静态复核（2026-10-22，未编译）新增 P0×1 / P1×10 / P2×10，见 §3.2**；下面「测试修复完成」记录**仅描述 2026-09-18 的那一次修复**，不代表当前语义已与 Java 对齐。

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
1. 真实 MySQL / PostgreSQL 驱动接入（`SqlDatabase` 的真实实现）仍为待办。**静态复核更新**：workspace 根 `Cargo.toml` **已**在 `[workspace.dependencies]` 登记 `sqlx = "0.9"` / `mysql_async = "0.37"` / `tokio-postgres = "0.7"`（注释即写明「存储/技能模块 P1」），但本 crate 的 `Cargo.toml` **仍未引用**；另 `agentscope-extensions-postgresql` 已有 `postgres-driver` feature + `PgSimpleClient`（`tokio-postgres`），可作为 `SqlDatabase` 真实实现的首选落点（避免重复引入驱动）。
2. Git 仓库读写/同步路径需系统 `git` CLI，未纳入离线单测（与 Java 依赖系统 Git 凭据一致）。**静态复核补充**：Java 侧用 `file://` 本地仓 + JGit 即可离线测 23 个用例，Rust 侧同样可用 `git` CLI 建本地仓（`git init` + `file://` URL）离线覆盖克隆/拉取/分支，**不构成不可测的理由**（§3.2 P1-09）。
3. 本 crate 仍有 1 个既有 `dead_code` warning（`InMemorySqlDatabase::has_metadata_column` 未使用）与 core/harness 既有 warning，非本次修复引入，未改动核心/harness。**静态复核补充**：`Cargo.toml` 中 `agentscope-core` / `async-trait` / `futures` / `chrono` / `base64` / `serde` 六个依赖在 `src/` 中**零引用**（§3.2 P2-10），建议一并清理（清理依赖属本 crate 范围，但需汇总方确认 workspace 依赖登记表口径）。
4. **§3.2 缺口清单为权威待办来源**；其中 P0-01（`JdbcAgentSkillRepository`）与 P2-01 / P2-09 需汇总方裁定后再动代码。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-skills` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-skills` / `cargo test -p agentscope-extensions-skills` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-skills/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

---

## 静态复核记录（2026-10-22）

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

### 7.3 仍待办（本模块其它 P1）

- **P0-01** `JdbcAgentSkillRepository` 归属已裁定在 **jdbc** crate 实现（§4.5 裁定 2），本 crate 待办仅是给两个 SQL 后端标注「已废弃，仅兼容，勿扩展」。
- **P1-01~P1-10**（git 技能名/description/`ls-remote` 分支过滤/静默跳过 pull、MySQL `CREATE DATABASE`/存在性校验、PG `DATA_TYPE` 校验、`skill_dir` 透出、测试 34 vs Java 192 `@Test`、`get_source` 含库表）均未动。