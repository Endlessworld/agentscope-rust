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
| Java 规模（2026-10-22 静态实测） | 主代码 **3529 物理行 / 3197 非空行**；测试 **3432 物理行 / 2813 非空行**（原记「~3177 / ~2813」是**非空行口径**，与实测一致，非「偏低」，见 history/SKILLS__MIGRATION_HISTORY.md） |
| Java 类数（静态实测） | 主代码 3 文件 / 3 顶层类 + 6 嵌套类 = **9 个类型**；测试 3 文件 / 12 个嵌套测试类 / **192 个 `@Test`**（git 23 + mysql 61 + pg 108） |
| Rust crate | `crates/agentscope-extensions-skills` |
| Rust 规模（2026-10-28 实测） | 8 个 `.rs` / **5304 物理行** / **93 个 `#[test]`**（git 35 / mysql 26 / postgresql 18 / repository 4 / sql 5 / util 5；0 个 `#[tokio::test]`、0 doc-test）；依赖收敛到 5 个（见 history/SKILLS__MIGRATION_HISTORY.md） |
| 迁移波次 | C |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 125 passed / 0 failed（默认 + 双驱动）/ 123（`postgres-driver`）/ 118（`mysql-driver`））—— §3.2 的 P1-01~P1-11 与 P2 全部关闭（P2-01 落为 trait 方法、P2-09 保留 `Result`、P0-01 归 jdbc crate 且已落地），P1-09 测试覆盖已收口。历史见 history/SKILLS__MIGRATION_HISTORY.md |

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
| `setWriteable(boolean)`（core 接口方法） | 运行时切换只读/可写 | `AgentSkillRepository::set_writeable(&self, bool)` | **已对齐**（见 history/SKILLS__MIGRATION_HISTORY.md）：SQL 后端用 `AtomicBool` 实时生效，git 按 Java 语义忽略并 warn；harness `SkillRepository` 不含该方法（保持只读语义） |
| `GitSkillRepository` | 只读；把远程 Git 仓库 clone 到临时目录，按 `skills/` 约定或 `skillsRoot` 加载；读操作触发同步 | `git::GitSkillRepository` | 走系统 `git` CLI（对齐 Java「依赖系统 Git 凭据」），无需 `git2` C 依赖。**语义差异**（见 history/SKILLS__MIGRATION_HISTORY.md；P1/P2 已收敛）：`save()` 返回 `Ok(false)`（P2-06 已对齐）、`skill_exists("")` 返回 `Ok(false)`（P2-08 已对齐）；其余 5 项已在 见 history/SKILLS__MIGRATION_HISTORY.md 收敛 |
| `MysqlSkillRepository` | MySQL 两张表（`agentscope_skills` / `agentscope_skill_resources`）CRUD；`createIfNotExist`；`metadata_json` 兼容探测 | `mysql::MysqlSkillRepository` | 通过 `SqlDatabase` 抽象（见 2.1），默认内存实现可测。**语义差异**：已无（P1-05/06/10、P2-02/03/05 均已收敛，见 history/SKILLS__MIGRATION_HISTORY.md）。本类型已 deprecated，**勿扩展** |
| `PostgresSkillRepository` | PostgreSQL schema+两张表 CRUD；`metadata_json` 兼容探测；identifier 双引号转义 | `postgresql::PostgresSkillRepository` | 同上；identifier 最大长度 63（Java）/ schema 概念。**语义差异**：已无（P1-07/10、P2-02/03/04 均已收敛，见 history/SKILLS__MIGRATION_HISTORY.md）；资源插入按 `RESOURCE_BATCH_SIZE = 1000` 分块。本类型已 deprecated，**勿扩展** |

> **上游状态提醒（2026-10-22 静态核对；2026-10-29 勘误）**：Java `MysqlSkillRepository` / `PostgresSkillRepository` 均标注 `@Deprecated`，官方替代是 `agentscope-extensions-jdbc` 的 `JdbcAgentSkillRepository`（**1184 物理行 / 1099 非空行**，namespace 列 + 方言抽象，配套 `dialect/table/SkillDialect.java` / `SkillResourcesDialect.java`）。本 crate 对齐的是**已弃用的 Java 实现**；**已完成（2026-10-29 勘误）**：原记「Rust `agentscope-extensions-jdbc` 目前无 skill 模块（`rg -i skill` = 0 命中），是否补需汇总方裁定」，实际该仓库已于 **2026-10-22** 在 jdbc crate 落地（`crates/agentscope-extensions-jdbc/src/skill/repository.rs`，`JdbcAgentSkillRepository` + `namespace` 列隔离，§3.2 P0-01）。

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
| `GitSkillRepository` | `GitSkillRepository` | 已完成 | `src/git.rs`，`git` CLI；skills_root 校验已修复；**2026-10-26** 已对齐 frontmatter name/description、`refs/heads/<branch>` 探测、远端不可达报错，见 history/SKILLS__MIGRATION_HISTORY.md |
| `MysqlSkillRepository` | `MysqlSkillRepository` | 已完成 | `src/mysql.rs`，两表 + metadata_json 兼容 |
| `PostgresSkillRepository` | `PostgresSkillRepository` | 已完成 | `src/postgresql.rs`，schema + 两表 |
| `RepoBackedSkillRepository`（harness 适配器） | — | 已完成 | `src/repository.rs`；已覆写 `skill_dir`（git 透出真实目录 / SQL 资源落盘缓存），见 history/SKILLS__MIGRATION_HISTORY.md P1-08 |
| 测试 | 3 个 `*Test` | 进行中 | 现 **93 个 `#[test]`**（git 35 / mysql 26 / pg 18 / repository 4 / sql 5 / util 5），git 侧 20 个为 `file://` 真实仓用例；Java 192（见 history/SKILLS__MIGRATION_HISTORY.md） |

### 3.2 静态复核发现的语义缺口（2026-10-22，未编译、仅静态核对）
> 状态取值沿用 §0：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
> 优先级：P0=核心链路阻断 / P1=重要行为不一致 / P2=增强或健壮性。

| # | 缺口项 | 子模块 | 优先级 | Java 行为 | Rust 现状 | 执行状态 |
|---|---|---|---|---|---|---|
| P0-01 | `JdbcAgentSkillRepository`（官方替代实现）缺失 | mysql / postgresql | P0 | `agentscope-extensions-jdbc` 的 `JdbcAgentSkillRepository`（1184 物理行）+ `SkillDialect` / `SkillResourcesDialect`，带 `namespace` 列隔离 | ~~`agentscope-extensions-jdbc` **无 skill 模块**（`rg -i skill` = 0 命中）~~ → **已在 jdbc crate 落地**：`crates/agentscope-extensions-jdbc/src/skill/{mod,repository}.rs`，`JdbcAgentSkillRepository` + `namespace` 列 + 无参构造绑定 namespace 的 `*_in` 变体 | **已完成（2026-10-29 勘误）**：原记「待办（需汇总方裁定归属：本 crate 内补 vs jdbc crate 内补）」，实际按裁定 B1 归 jdbc crate，于 **2026-10-22** 落地关闭 |
| P1-01 | git 技能名来源不一致 | git | P1 | `SkillFileSystemHelper.getAllSkillNames` → `readSkillName` 解析 **SKILL.md frontmatter 的 `name`** | `git.rs` 委托 harness `load_skills_from_dir`，取**目录名**；目录名 ≠ frontmatter name 时 `get_skill(name)` 找不到 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P1-02 | git 技能 description 不一致 | git | P1 | `SkillUtil.createFrom` → `MarkdownSkillParser.parse` 取 frontmatter 的 `description` | harness `skill_description()` 取 SKILL.md **首个非空行**并剥 `#`；对 frontmatter 文件会返回 `---` | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md；在 git 适配层重写身份，未改 harness） |
| P1-03 | git 分支远端探测未按分支过滤 | git | P1 | `resolveRemoteHead` 用 `lsRemote.setHeads(true)` + `matchesBranch`（`refs/heads/<branch>`） | `remote_head()` 固定 `git ls-remote <url> HEAD`（远端默认分支），未按配置的 `branch` 过滤 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P1-04 | git 远端不可达时的降级行为不一致 | git | P1 | `resolveRemoteHead` 异常 → 返回 null → `hasRemoteUpdates` 返回 **true** → 执行 pull 并抛错 | `remote_head()` 返回 `None` → `has_remote_updates` 返回 **false** → **静默跳过 pull**、继续用旧数据 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：改为报错） |
| P1-05 | MySQL 未建库 / 表名未带库前缀 | mysql | P1 | `createDatabaseIfNotExist`（`CREATE DATABASE IF NOT EXISTS ... utf8mb4`）+ `getFullTableName` = `<db>.<table>` | 只建表；所有 SQL 用裸表名（`<table>`） | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md，顺带 P2-05 utf8mb4） |
| P1-06 | MySQL `createIfNotExist=false` 不校验存在性 | mysql | P1 | `verifyDatabaseExists` + `verifyTablesExist`，缺表抛 `IllegalStateException` | **完全跳过校验**，后续查询静默返回空集 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P1-07 | PostgreSQL `metadata_json` 类型探测缺失 | postgresql | P1 | `isSupportedMetadataJsonColumnType`：`{text, character varying, character}`，`jsonb` → **false**（走 legacy 路径） | 只判列是否存在，不看 `DATA_TYPE`；`jsonb` 会被当作支持 → 写 `metadata_json` 报错 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P1-08 | 适配器未透出 `skill_dir` | 全部 | P1 | Java `AgentSkill` 带 `originDir`，`load_skill_through_path` 可定位磁盘目录 | `RepoBackedSkillRepository` 未覆写 `skill_dir` → harness `skill_middleware::load_index` 收不到目录 → **加载工具对 git/SQL 技能不可用** | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：trait + 适配器透出/落盘） |
| P1-09 | 测试覆盖远低于 Java | 全部 | P1 | 192 个 `@Test`（git 23 / mysql 61 / pg 108） | 93 个 `#[test]`（git 35 / mysql 26 / pg 18 / sql 5 / util 5 / repository 4）；git 侧 20 个 `file://` 真实仓用例 | **已完成（2026-10-31 收口）**：默认构建 +23 → **116**（git 37 / mysql 30 / pg 22 / repository 7 / sql 11 / util 9）；开 `postgres-driver` 再 +7 → 123、开 `mysql-driver` 再 +2。Java 测试类→Rust 用例映射表与语义缺口关闭理由见 history/SKILLS__MIGRATION_HISTORY.md |
| P1-10 | `get_source()` 命名不含库/表/模式 | mysql / postgresql | P1 | mysql → `mysql_<db>_<table>`；pg → `postgresql_<schema>_<table>` | 固定返回 `"mysql"` / `"postgresql"`（多实例时 `source` 冲突，`RepoBackedSkillRepository` 的 `repo_id` 也随之冲突） | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P1-11 | frontmatter 闭合分隔符未要求「独占一行」 | git | P1 | `MarkdownSkillParser.FRONTMATTER_PATTERN` 要求闭合 `---` / `...` 紧跟换行且**行首无缩进**，行尾只允许空白 | `split_frontmatter` 用 `line.trim() == "---"` 判定 → YAML 块标量（竖线 / 折行两类）里的缩进 `  ---` 会被误判为闭合围栏，YAML 被提前截断、后续正文被当成 body | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P2-01 | 缺 `set_writeable` | 全部 | P2 | Java 接口有 `setWriteable(boolean)`，三个实现均可运行时切换 | `AgentSkillRepository::set_writeable(&self, bool)`（见 history/SKILLS__MIGRATION_HISTORY.md）；SQL 后端 `AtomicBool` 实时生效、git 忽略并 warn | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md，按裁定 5 落地为 trait 方法） |
| P2-02 | 空/空白库名·表名不回退默认值 | mysql / postgresql | P2 | null/空 → 回退默认名；并 `trim()` | `validate_identifier("")` 直接报错；含空格也报错（Java `trim()` 后可用） | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：`util::resolve_identifier`，构造函数先 trim/回退再校验） |
| P2-03 | `delete()` 无事务 | mysql / postgresql | P2 | Java `delete` 开事务（`setAutoCommit(false)` → commit/rollback） | Rust `delete_skill_internal` 连发 2 条 DELETE（resources + skill）且**不开事务** → 中途失败留孤儿资源 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：`SqlConnection::in_transaction` + 存在性预检） |
| P2-04 | PostgreSQL 资源插入无 1000 行分批 | postgresql | P2 | `RESOURCE_BATCH_SIZE = 1000` 分块 `executeBatch`（避开绑定参数上限） | 逐条 `execute`，无分批 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：`SqlConnection::execute_batch` + `RESOURCE_BATCH_SIZE` 分块） |
| P2-05 | MySQL DDL 缺 utf8mb4 字符集 | mysql | P2 | `DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci`（两张表均带） | Rust 建表 DDL 未带字符集/排序规则 | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P2-06 | git `save()` 返回值不一致 | git | P2 | Java 只读仓 `save()` **返回 false** | Rust 返回 `Err(SkillRepositoryError::ReadOnly)` | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：改为 `Ok(false)`，与 `delete()` 一致） |
| P2-07 | git remote URL 未 trim | git | P2 | 构造时 `remoteUrl.trim()` 后保存 | 只用 `trim().is_empty()` 判空，**原样保存**未 trim 的 URL | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P2-08 | `skill_exists("")` 行为不一致 | mysql / postgresql | P2 | Java `skillExists(null/空)` **返回 false**，不抛异常 | Rust `validate_skill_name` → `Err(InvalidArgument)` | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：mysql / postgresql / git 三后端均返回 `Ok(false)`，git 侧不触发同步） |
| P2-09 | `close()` 失败语义不一致 | git | P2 | Java `close()` 删目录失败只 `warn`，不抛 | Rust `close()` 返回 `Err(Io)` | **已完成（2026-10-31 裁定 A）**：保留 `Result`（trait 为可失败的驱动后端保留出口），但 `GitSkillRepository::close` 只 `tracing::warn` 并返回 `Ok(())`，与 Java 逐字等价；trait 默认实现本就 `Ok(())`。用例 `git::close_never_returns_a_cleanup_failure`、`git::close_removes_an_owned_temp_dir_and_is_idempotent`（见 history/SKILLS__MIGRATION_HISTORY.md） |
| P2-10 | 未使用的依赖与死代码 | 全部 | P2 | — | `Cargo.toml` 的 `agentscope-core` / `async-trait` / `futures` / `chrono` / `serde`（derive）在 src 中零引用（`base64` 实际在用）；`InMemorySqlDatabase::has_metadata_column` 无任何调用者（与 `metadata_column_type().is_some()` 重复） | 已完成（见 history/SKILLS__MIGRATION_HISTORY.md：`agentscope-core`/`async-trait`/`futures`/`chrono`/`serde` 与未用 dev-dep `tokio` 已删；`base64` 实为在用以保留；`has_metadata_column` 已删） |

**分层结论（与 harness `skill/` 存量代码核对，无重复实现）**：
- harness `agent/skill/repository.rs` 提供**目录式** `SkillRepository`（`load()` → `Vec<AgentSkill>`）+ `FileSystemSkillRepository` / `WorkspaceSkillRepository`；本 crate 提供**后端 keyed CRUD** `AgentSkillRepository` + `RepoBackedSkillRepository` 适配器。两者是**不同抽象层次**，模型统一复用 harness `AgentSkill` / `Metadata` / `SkillResources`，**未发现重复的 skill 模型或重复的目录扫描实现**（git 侧直接调用 harness 的 `load_skills_from_dir`）。
- 唯一需补的衔接点：`RepoBackedSkillRepository` 应覆写 `skill_dir`（P1-08），否则 harness 的 `load_skill_through_path` 对非文件系统仓库无效。**2026-10-26 已完成**：trait 新增 `skill_dir`，git 透出真实目录，SQL 后端由适配器落盘缓存（见 history/SKILLS__MIGRATION_HISTORY.md）。

## 4. 项目状态与下一步
### 4.1 当前状态

**状态：§3.2 的 P1/P2 已全部收敛（2026-10-28）。** 波次 C，crate `crates/agentscope-extensions-skills` 实现 3 个后端仓库 + harness 适配器。§3.2 逐项终态：P1-01~P1-11 全部已完成；P2-02/03/04/05/06/07/08/10 已完成；P2-01 / P2-09 为已裁定「设计决定（不迁移）」（见 history/SKILLS__MIGRATION_HISTORY.md）；P0-01 归 jdbc crate 且已落地。**P1-09（测试覆盖）仍为进行中**——93 vs Java 192，见 history/SKILLS__MIGRATION_HISTORY.md。

**最近一次实测（2026-10-28）**：`cargo build -p agentscope-extensions-skills` 0 error 0 warning（本 crate）；`cargo test -p agentscope-extensions-skills` → **93 passed / 0 failed / 0 ignored**（2 个 `test result:` 行，第二个为 0 passed 的 doctest 汇总）。`cargo fmt -p agentscope-extensions-skills -- --check` 干净。

**测试修复完成（2026-09-18，历史快照；当时 33 passed）**：`cargo build -p agentscope-extensions-skills` 通过；`cargo test -p agentscope-extensions-skills` → **33 passed / 0 failed**（含 doc-tests）。

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
1. ~~真实 MySQL / PostgreSQL 驱动接入~~ **2026-10-31 已落地（feature-gated，见 history/SKILLS__MIGRATION_HISTORY.md）**：`src/driver/{mod,postgres,mysql}.rs`；`postgres-driver` → `tokio-postgres`（复用 workspace 已构建的同栈，与 `agentscope-extensions-postgresql` 的 `PgSimpleClient` 同驱动），`mysql-driver` → `mysql_async`（`minimal`，无 TLS 后端）。`default = []` 不引入任何新依赖；未启用 feature 时只有 `InMemorySqlDatabase`（行为不变）。
2. Git 仓库读写/同步路径需系统 `git` CLI。**静态复核补充**：Java 侧用 `file://` 本地仓 + JGit 即可离线测 23 个用例，Rust 侧同样可用 `git` CLI 建本地仓（`git init` + `file://` URL）离线覆盖克隆/拉取/分支。**2026-10-26 已落地**：git 侧新增 11 个 `file://` 真实仓用例（无网络、无 mock 库；需要本机有 `git` 二进制）。
3. ~~`has_metadata_column` dead_code warning + 6 个零引用依赖~~ **2026-10-28 已清理（见 history/SKILLS__MIGRATION_HISTORY.md）**：删除 `has_metadata_column`（全仓零调用者，与 `metadata_column_type().is_some()` 重复），`Cargo.toml` 依赖 10 → 5（`agentscope-core` / `async-trait` / `futures` / `chrono` / `serde` 与未用 dev-dep `tokio` 已删；`base64` 实际在用 → 保留）。本 crate 当前 **0 warning**；core/harness 的既有 warning 未触碰。
4. **§3.2 缺口清单为权威待办来源**；~~其中 P0-01（`JdbcAgentSkillRepository`）与 P2-01 / P2-09 需汇总方裁定后再动代码~~ → **2026-10-29 勘误**：P0-01 已按裁定 B1 在 jdbc crate 落地（2026-10-22）；P2-01 已按裁定 5 落地为 trait 方法（见 history/SKILLS__MIGRATION_HISTORY.md）；仅 P2-09 仍为「设计决定（不迁移）」**待汇总方确认**（见下第 5 条）。
5. **真实剩余（2026-10-31 收口后）**：
   - ~~测试覆盖 93 vs Java 192~~ → **116（默认构建）/ 123（+`postgres-driver`）/ 125（两 driver 全开）vs Java 192**（见 history/SKILLS__MIGRATION_HISTORY.md 映射表；差额主要为 Java 的重复参数化用例与真实库集成用例，逐类已给出理由）。
   - ~~真实 MySQL / PostgreSQL 驱动未接入~~ → **已落地**（见 history/SKILLS__MIGRATION_HISTORY.md），默认构建仍无驱动的限制未变（需显式 `--features`）。
   - ~~`close()` 失败返回 `Err`~~ → **已裁定并等价实现**（见 history/SKILLS__MIGRATION_HISTORY.md）。
   - **新增待办**：两个 driver 的**真库集成测试**仍缺（`AGENTSCOPE_TEST_PG_URL` / `AGENTSCOPE_TEST_MYSQL_URL` 环境变量门控）；`mysql-driver` 未开 TLS（`minimal`），TLS 实例需前置代理（见 history/SKILLS__MIGRATION_HISTORY.md）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-skills` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-skills` / `cargo test -p agentscope-extensions-skills` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-skills/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

---

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/SKILLS__MIGRATION_HISTORY.md`](./history/SKILLS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
