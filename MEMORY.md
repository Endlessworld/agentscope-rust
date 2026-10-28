# MEMORY.md

> 精简于 2026-10-21（并入 G1–G10 提交级分析、裁决 A1–D3 落地、A2/A3/A4/A5/A6/C2/C4/C5 实施与提交、judge crate、双轨 session 对齐 Java、harness grep/glob 增强、迁移分析附录 B 重审、session/context 迁移在办、wave-reports 回执约定）；旧全文备份在 `.agentscope/workspace/MEMORY.md.bak`。原则：只保留对**后续任务仍有指导意义**的稳定事实、约定与教训。

## 环境与工作方式
- 路径跨会话不同（见过 `E:\local-github\agentscope-rust` 与 `D:\IdeaProjects\agentscope-rust`；Java 源在 `E:\local-github\agentscope-java`，`D:\IdeaProjects\agentscope-java` 不存在）。**以任务说明中的路径为准**，勿凭记忆断言盘符。
- Windows + PowerShell；偏好简洁中文；目录勘察一行一条并标 `[目录]`/`[文件]`，最多 20 个顶层条目；只读探索不落盘。
- 常见只读角色：①「只读任务模式」——用户明确「不要修改任何文件」，如全量测试跑 `cargo test --workspace -j 2 --no-fail-fast` 并把输出落文件，汇总须含所有 `test result:` 行、`FAILED`/`failures:`/`panicked` 原文与「是否有编译错误」；②「Rust 代码审核员」——对象为新增未提交文件 + 相关 `Cargo.toml`，逐文件给「路径 → 结论：OK / 具体问题+行号」，末尾按「必须修复 / 建议改进 / 可接受」三档 + 测试数量线索。
- 审核固定维度：①依赖最小必要、与 workspace 其它 crate 版本一致（须 grep 核对）、是否本应放 workspace 依赖却写死；②协议实现正确性（握手 / namespace connect / ping-pong / 事件解析 / 重连退避）；③单测充分且离线；④`unwrap`/panic 风险；⑤schema 符合性、id 校验、类型映射；⑥是否引入被限制 crate；⑦死代码 / 未接线模块；⑧**trait 方法是否漏实现**。
- 全局上下文： E:\local-github\agentscope-rust\.agents\context\index.md
## 构建 / 测试环境（Windows + PowerShell）
- **不要用 PowerShell 管道直连 `cargo test --workspace`**（巨量输出触发 .NET Stack overflow）→ 先 `Out-File $env:TEMP\xx.log` 再 `Select-String`。
- 全量用 `cargo test --workspace -j 2 --no-fail-fast`（降并行避 `os error 1455` 页面文件/rlib mmap 失败；`os error 112`=磁盘满）。`cargo test -p agentscope-harness` 全量可能 >300s 工具超时 → 改跑 `--lib`。`-j 2` 亦规避 `training::runner` 静态注册表竞争。
- `.cargo/config.toml` 设 `incremental=false`；**不得改 `CARGO_TARGET_DIR`**；不并发起多份 cargo（争用 target 目录）；单条命令超时上限约 300s，长命令写 `.bat` 后台跑 + 轮询日志。
- 命令护栏为**朴素子串匹配**：①`git add` 被误判拦截 → 改 `git stage`（`git restore --staged`/`git rm` 正常）；②命中 `rm` 子串即拒 → 拆分/换词；③`--format <value>` 被拒 → 省略或 `-j`。遇拦截换同义写法，勿重试原命令。
- 磁盘坑：全量报 `no space on device` + `LNK1318 LIMIT` 或 `link.exe failed exit code 1140` 时，真凶常是 `target/debug/deps` 的 `.pdb`（Windows MSVC 无条件生成，曾 146 个共 25.57GB、target 共 51GB，删 `.pdb` 即释放，可再生）。诊断用 `Get-PSDrive` + 分目录 `Measure-Object Length -Sum`。**此时 EXITCODE 仍可能为 0**，判定失败须看 `^failures:` 段与 `test result: FAILED`。
- **cargo 代理坑**：`git config --global http.proxy = http://127.0.0.1:10808`（端口未监听、系统代理为直连）被 cargo/libcurl 继承 → `Failed to connect to index.crates.io`，而 `Invoke-WebRequest` 直连正常。解法：给 cargo 加 `--config 'http.proxy=""'`（不改全局 git 配置）；新增依赖首次 fetch 需带，进 cache 后普通 build/test 不再联网。
- 既有偶发失败（非缺陷）：`agentscope-extensions-training::runner` 进程级 `OnceLock` 静态注册表 + `clear_all()` 测试间竞争（单跑稳定）；`acp_stdio_thought_test` 依赖 live 模型 reasoning，flaky。
- PowerShell 反引号转义（`` `r`n ``）经 shell 包装会被吞掉 → 拆行一律用单引号正则 `-split '\r?\n'`。

## 构建 profile / 体积优化
- 根 `Cargo.toml`：`[profile.release]`（lto=fat / cgu=1 / panic=abort / strip / opt-level=z + `release.package."*" opt-level=3`）；`[profile.embedded]`（inherits release，`package."*" opt-level="z"`，build-override opt-level=3）——除 `package."*"` 外二者 artifact 级等效。
- `.cargo/config.toml`（自动加载）**只放** `[build] incremental=false`；**切勿**放 nightly `-Z`（会打挂 stable 下所有命令）。`.cargo/config.embedded.toml`（需 `--config`）：`[unstable] build-std=["std","panic_abort"]` + `build-std-features=["panic_immediate_abort"]`；`[build] rustflags=[-Z,location-detail=none, -C link-arg=/OPT:REF, /OPT:ICON, /DEBUG:NONE]`。用法：`cargo +nightly build -p <crate> --profile embedded --config .cargo/config.embedded.toml`。
- 坑：`build-std*` 须放 `[unstable]`（放 rustflags 报 `unknown unstable option`）；必须 `cargo +nightly`（stable 下任何 `-Z` 在探测 target 阶段即失败）；自定义 profile 目录下 `incremental=false` 无效。工具链：stable 1.96.1（默认）、nightly 1.99.0（rust-src 已装）。

## codegraph CLI
- v1.5.0 已装；索引 `...\.codegraph`。本会话未暴露 MCP 工具，只能经 skill + shell 执行 CLI。
- 命令分层：定位 `query <name> [-k kind]`、`files --filter/--pattern`；上下文 `node <symbol>` / `node -f <file>`；探索 `explore <关键词> --max-files N`；关系 `callers/callees/impact/affected`；维护 `status/sync/index/unlock`。
- 坑：`callers`/`callees` 只收**纯符号名**（`path:line` 报 not found），同名符号按名聚合有噪声（用 `模块::符号` 收敛）；`files` 无 `-l`；未定义符号退出码 0。原则：**codegraph 定代码结构，rg 定文本字面量**；非代码文本或 not found 才回退 rg/read。

## harness 若干陷阱
- `agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 的遗留事件通道 **struct**，与路由 trait 同名；路由 trait 须全路径 `agentscope_harness::agent::gateway::channel::Channel`。`Channel`/`ChannelSink` 均有 `channel_id()`，同作用域须限定（否则 E0034）。
- `LocalFilesystem::write` **拒绝覆盖已存在文件**（`local.rs:260`）；追加须走 `WorkspaceManager::append_utf8_workspace_relative`；改文件要先 `Remove-Item` 再写。
- `LocalFilesystem::grep` 对目录 path 自身递归、`glob` 只匹配 basename（`memory` + `*.md` = 递归匹配）。
- `HarnessAgent::agent() -> Option<&ReActAgent>`（`harness_agent.rs:180`）；`HarnessAgent::task_repository()` 返回 `&Arc<dyn TaskRepository>`（**非 Option**）；`stream_events(Vec<Msg>, StreamOptions) -> BoxStream<Result<AgentEvent>>`，reply 从 `AgentEvent::AgentResult(EventData).fields["reply"]` 反序列化取；`Msg::generate_reason()` 无 metadata 默认 `ModelStop`；`GenerateReason` 含 `MaxIterations` 等 11 变体 + 已加 `ToolReturnDirect`；`HarnessAgentBuilder` **无 MCP/allow-deny 配置**，`model()` 只收 `Arc<dyn Model>`（按名构建用 `model_resolver(Fn(&str)->Option<Arc<dyn Model>>)`）；`TaskStatus::is_terminal()`；`SUPPRESS_COMPLETION_CALLBACK` 在 `agentscope_harness::agent::subagent::task`。
- `ReActAgent` 构建**必须提供 model**（`.model(...).build()`），否则报 `Agent("ReActAgent requires a model")`；单测需 StubModel。
- `memory_get`/`memory_save`/`memory_search` 对齐 Java：注入 `Arc<WorkspaceManager>`（注册点 `harness_agent.rs:1409`）；get 入参 `path/startLine/endLine`（1-based inclusive），`..`→"path traversal not allowed"；save 仅 `content`，**追加** `MEMORY.md` + 当日 `memory/YYYY-MM-DD.md`；search 仅 `query`，大小写不敏感子串。

## AgentScope Java→Rust 迁移：总览
- 并行迁移 20 个 extension 模块 → `crates\agentscope-extensions-<module>\`。波次：**A**（PROTOCOL/MEM/RAG/MODEL/SANDBOX/CHANNEL）；**B 存储侧**（JDBC/MYSQL/POSTGRESQL/MONGODB/REDIS/OSS/COS）；**C 非存储侧**（HIGRESS/STUDIO/SCHEDULER/SKILLS/NACOS/AISTIO/TRAINING）。
- 编排/回执格式见 `docs\extensions\EXTENSIONS_MIGRATION_PLAN.md`；汇总 tracker `docs\extensions\EXTENSIONS_GAP_TRACKER.md`（§2.1 已裁定公共抽象归属必须遵守；§2.2 待裁定）；各模块 `<MODULE>__GAP_TRACKER.md` 为单一事实来源。
- 必读顺序：本模块 tracker → EXTENSIONS_GAP_TRACKER → EXTENSIONS_MIGRATION_PLAN → `docs\MIGRATION_PLAN.md` / `docs\MIGRATION_GAP_TRACKER.md`；多用 codegraph 探查 core/harness 既有 trait，避免重建平行类型。
- **全量基线演进**：1485 → … → 2715 →（双轨 session 落地后）**103 targets / 3234 passed / 0 failed / 1 ignored / EXITCODE=0**。口径以「无 `failures:` 段 + EXIT=0」为准。1 个 ignored 是 `platform_channel.rs` 的 ```ignore 文档示例（既有，非缺陷）。core lib 219、harness lib 407。

### Java→Rust 迁移分析（提交级，2026-10-19）
- 逐提交分析产物写入 `.agentscope\workspace\java-migration\<两位编号>-<组名>.md`；按**提交组 G1…Gn** 聚合同类扩展模块的多个 Java 提交。每节固定格式：变更摘要 / Java 涉及文件(3-8) / Rust 对应位置 / 可迁移性（✅可迁移 / ⚠️部分 / ❌不可迁移 / ✔️已完成 + 理由）/ 实施步骤 / 影响范围 / 风险依赖；头部注明「分析提交数」，末尾 `## 组小结`。子智能体为**只读角色**，完工回 **≤300 字摘要**（G6a/G6b 为 ≤200 字）。
- 组划分与产物：
  - **G1** `01-core-structured-output.md` — core 结构化输出/生成结果/agentId/returnDirect/MCP 标题/role 参数（8）。
  - **G2** `02-core-reliability.md` — core 重试/状态持久化/HITL/runtime context/HTTP 传输（9）。
  - **G3** `03-tools-mcp-middleware.md` — tools/MCP/middleware 扩展点/skill 过滤（8）。
  - **G4** `04-model-providers-a.md` — openai/dashscope/anthropic/gemini 服务器工具/缓存控制（10）。
  - **G5** `05-model-providers-b.md` — deepseek/ollama/openai-official Responses API/embedding/usage 指标（8）。
  - **G6a** `06a-harness-session.md` — session journal/compaction 量化/pruning/skill staging/文件上传（7）。每节 ≤12 行、全篇 ≤350 行。
  - **G6b** `06b-harness-misc.md` — harness 其他（9，声明 7 实为 9）。每节 ≤12 行、全篇 ≤400 行。
  - **G7** `07-jdbc-db.md` — JDBC/MySQL/PostgreSQL/Redis 存储层（9）。
  - **G8** `08-sandbox-rag.md` — sandbox(e2b/kubernetes/docker) + RAG(Tika/WordReader/embedding)（9）。
  - **G9** `09-extensions-protocol-channel.md` — protocol(agui)/channel(个人微信)/higress/aistio/studio/judge/controlplane（7）。
  - **G10** `10-service-ci-deps-docs.md` — service/aistio 服务化/Harness context+Service session 重构/CI/依赖升级/文档（27）。
- 总报告 `docs/java-recent-commits-migration-analysis.md`。Java 仓库 main 近 30 天 = **113 提交**（2026-09-18~10-14，HEAD 5f7f8e6cf）。汇总计数（含跨组重叠，和=102）：✅28 / ⚠️42 / ❌24 / ✔️8。查看 Java 提交：`git -C E:\local-github\agentscope-java show <hash> [--stat]`（分析分支 `main`）。跨组重复：`c0d03ccea`（G2/G9）、`35cbbfcf9`（G3/G9）、`7393283d7`（G10 内含重复）。
- **2026-10-20 重审**：报告 311 → 438 行，结论写入**附录 B**（§6/§7 标记历史快照并指向 B；§A P0-2/P1-17 就地更正）。口径：**14 项已完成 / 5 项前提消失可改判 / 3 项妥协可撤销 / 12 项仍阻塞**。高价值单点缺口：`ToolProgress`/`CompactionMetrics` 0 命中、`ConflictPolicy` 无枚举、`react_agent.rs:1438` 仍 `Interrupted => "ERROR"`、e2b 缺 snapshot prune/`snapshotRetention`。存储侧残留：JDBC `UNVERSIONED` 写后读非原子、PG `version DEFAULT 0` 幽灵 CAS、JDBC `dialect/vendor/mysql.rs:79` session 表仍 `utf8mb4_unicode_ci`。**注**：原列①「双轨 session 阻塞」已于 2026-10-20 实施完毕。
- G10 报告须单列「### 需要 Rust 侧跟进的依赖版本」。Rust 用 **async-openai 0.41 / async-anthropic 0.6** SDK，与 Java 官方 SDK 版本无直接对应，升级无法照搬。
- 子智能体组过大失败（G6 14 提交整组 fail）→ 拆 G6a/G6b 后成功；提示「单组提交数 ≤9」并限输出行数。**上下文爆炸规避**：禁 `git show <hash>` 全量，先 `--stat` 再逐文件 `show <hash> -- <path>`。

## 通用硬约束（所有模块子智能体）
- 只能写本模块 crate `crates\agentscope-extensions-<module>\**`（含本 crate `Cargo.toml` `[dependencies]`，标具体版本）与本模块 tracker；禁止改根 `Cargo.toml`（尤其 `[workspace.dependencies]`）、`agentscope-core/**`、`agentscope-harness/**`、其它模块 crate 与汇总 tracker。**唯一例外**：新建 crate 登记时，用户已显式授权编辑根 `Cargo.toml` 的 `[workspace] members`（**仅此一处**，仍不得动 `[workspace.dependencies]`；本次为 `crates/agentscope-extensions-judge`）。
- 跨模块/workspace 级需求 → 记本模块 tracker §4.2 + 回执「跨模块需求」。复用 harness/core 既有 trait；harness 抽象不足时不得改 harness，记 §4.2 并用本 crate 适配层绕过。
- 不得删既有功能 / 删断言 / `#[ignore]` 凑绿；测试须修至全绿且离线（DTO serde 往返、纯函数映射、transport trait 注入/mock）。验证 `cargo build -p <crate>` 与 `cargo test -p <crate>`。
- 完工更新 tracker §1/§1.1（基线）/§2（逐类映射+依赖映射含版本）/§3（P0/P1/P2+状态）/§4.1（命令+实测）/§4.2（阻塞+跨模块）；「设计决定（不迁移）」须标**「已完成（设计决定，不迁移）」并写明依据**。
- 风格：最小改动、高内聚低耦合、不写投机性代码、不臆造 API。回执格式多变：四项（多数）；五字段（PROTOCOL/SANDBOX）；六字段（MEM/CHANNEL/TRAINING/STUDIO/NACOS/AISTIO）。
- **回执/汇报落盘**：部分波次要求写入 `.agentscope/workspace/wave-reports/wa-<task>.md`（含文件、测试名、命令+EXIT+passed/failed、反向验证、未完成项）+ 回复一句话摘要；做不了写原因，**不得静默结束**；改动须 `git status` 可见。
- 并行修改拆分：按「crate 归属 + 文件互不重叠」拆分（同 crate 不同文件可并存；`react_agent.rs` 被多项触及 → core 必须单智能体独占串行）。统一指令：只跑 `cargo build/test -p <crate>`、遇「来源在范围外」编译错误等 60-90s 重试且不改他人文件、必须反向验证（回退实现→FAILED→恢复）、`Blocking waiting for file lock` 属正常。**2026-10-20 起部分波次改为「子智能体只做代码编辑、禁跑 cargo/编译/测试」，由主智能体统一验证。**

## 各模块要点
- **MEM**（A）：实现活跃 core `Memory` trait（非 Java 已废弃的 `LongTermMemory`）；厂商 DTO / 消息→Mem0/Bailian 转换 / 检索→core `Msg`；reqwest 真实实现、测试离线。
- **CHANNEL**（A，**已完成**）：5 平台 `*Channel` + 4 个 axum CallbackController + `DingTalkStreamClient`（常驻 WS，ACK 在分发前发出）；另新增 **个人微信 iLink 渠道**（`src/weixin/` 13 文件 ≈4890 行，+85 测试）。harness 路由层 8 文件不可改；平台适配器只实现 `ctx()`+`deliver_reply()`。公共层：harness `agent/gateway/platform_channel.rs` 的 `PlatformChannel`/`PlatformCtx` + 对 `Channel`/`ChannelSink` 的 blanket impl；统一用泛型 `common::ChannelRegistry<C>`。
- **MODEL**：见「关键行为约定」（267 passed）。**无独立 `src/deepseek/`**（deepseek 逻辑在 `src/openai/mod.rs`）；dashscope→`src/dashscope/`、ollama→`src/ollama/`、能力位→`src/common/capabilities.rs`、base_url 回退→`src/common/base_url.rs`。
- **RAG**：5 子模块（bailian/dify/haystack/ragflow/simple）。已落地 dify/simple/ragflow；transport seam 范式（trait + 真实 reqwest + 内存 transport）。
- **PROTOCOL**：5 子模块（a2a-client/server、agent-protocol、agui、chat-completions-web）。已落地 a2a 系列 + agent_protocol + chat_completions + agui（`src/agui/**` 归专门子智能体）。
- **SANDBOX**：四平台 `*FilesystemSpec`、Kubernetes 文件 API、连接策略已落地；**e2b 已落地**（`src/e2b.rs`，含 `/files` REST 上传下载、`envd_sandbox_url`、`E2bSnapshotRefs`；仅缺 snapshot prune/`snapshotRetention`）。harness **无 `SandboxFilesystemSpec`** → 本 crate 自建等价 Spec 适配（记 §4.2）。用 `httpmock` 离线测。
- **NACOS**：三子模块统一走 `service::NacosAiService` trait，**不实现 `DistributedStore`**；skill 只读 harness `SkillRepository`。
- **AISTIO**：见「AISTIO 交付明细」。
- **STUDIO**：见「STUDIO P2」。
- **TRAINING**：backend/reward/runner/strategy 已接线；失败根因曾为 **provider HTTP 400 中断**（非「迭代上限」）。
- **SCHEDULER**：scheduler-common 为本 crate 内共享层 + quartz/xxl-job。
- **SKILLS**：JDBC skill 持久化**不在 jdbc crate**，在 `crates/agentscope-extensions-skills/src/{sql,mysql,postgresql,repository}.rs`（namespace 隔离改此处，裁决 B1）。
- **存储侧**（JDBC/MYSQL/POSTGRESQL/MONGODB/REDIS/OSS/COS）：各 crate 自建 `<X>DistributedStore` / `<X>AgentStateStore` / `<X>Store`(BaseStore) / `<X>SnapshotSpec` / `<X>SandboxExecutionGuard`；CAS/版本语义以 harness 为准。`SandboxExecutionGuard` 是具体 struct（impl Clone，可 `Arc<..>`），非 trait；`SandboxIsolationKey` 路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`。
- **JDBC 原生锁 / 版本化 state**：`src/dialect/native_lock.rs`（`NormalizeLockName`：MySQL ≤64 原样、超长截 31 + `:` + SHA-256 前 32 hex；`postgres_lock_key`：SHA-256 前 8 字节折叠 i64，碰撞 ~2^-32）。**关键裁定**：`GET_LOCK`/`pg_try_advisory_lock` 是**会话级**锁，Rust `JdbcExecutor` 单连接共享缝、同连接重复获取会成功（PG 可重入）→ **不能照搬 Java 方言 override**；新增 **`NativeLockSession` 缝**（每获取独占会话），`Mysql/PostgresNativeLockStrategy` 独立成类型。`JdbcAgentStateStore` 新增 `supports_versioning()/read_version()/get_versioned()/save_if_version()` + `VersionedState`/`UNVERSIONED`（`-1` 无条件覆写、`0` insert-if-absent、其余 CAS 不符返 `UNVERSIONED` 不写）。测试用**独立算得的字面量** + dev-dep 重算**双向交叉验证** SHA-256，防「实现与测试同源一起错」。
- **judge/jev**：见「judge crate」。
- **HIGRESS**（C，单模块）。

## judge crate（agentscope-extensions-judge）
- 落地裁决 B4；源 Java `a5be76105`。根 `Cargo.toml` 仅加 1 行 member。9 文件（`Cargo.toml` + `src/{lib,error}.rs` + `src/jev/{mod,dto,probability,validation,guardrail,review,execution}.rs`），**零第三方依赖**（仅 serde/serde_json/thiserror，全走 workspace）；骨架参照 `-studio`/`-higress`。
- 已落地：DTO（Question/Answer type 标签枚举、NoulCriteria、SystemOneRequest(Builder)/SystemOneResult/Usage）；`PROBABILITY_SUM_TOLERANCE_PER_OPTION = 0.00005`（per-option × n）；validation（Java 消息逐字对齐）；ReviewRegions.source/patch 纯函数 + JevReviewInput；JevGuardrail 纯 evaluate 折叠（Outcome 顺序 = Java ordinal，Support>Block>Review）+ 默认 hazard 集（jailbreak/harm/self_harm，0.35/0.7/severity 2）；JevExecution Mode/Status/Decision/Record/Options；RetryPolicy + system_one_url + default_api_key + error_message（message→error→detail）。
- 未落地（需 seam，已在 lib.rs 注明）：JevClient HTTP 传输/重试/超时、Guardrail::screen reactive caller、JevConfirmedCalls（需 RuntimeContext 扩展槽）、Playwright、context/evaluation/evidence/example/integration/routing/supervision 子包。
- 实测 57 passed / 0 failed；clippy/fmt 干净。反向验证：容差回退 1e-6 → 5 测试 FAILED。待办：建 `docs/extensions/JUDGE__GAP_TRACKER.md`；`JevClient` 复用 core `model::transport` 还是自建 seam；`JevConfirmedCalls` 需 core `RuntimeContext` keyed 扩展槽。

## AISTIO 交付明细
- **B 方案**：用户授权后可跨改 core。已在 `core/src/agent/react_agent.rs` 新增只读纯增量 getter `sys_prompt()`/`max_iters()`（+ `enable_pending_tool_recovery()`），未改行为（须在汇总 tracker 登记）。
- **P1/P2 主干**：`src/adapter/{observer_middleware,agent_scope_adapter,support,agent_task,collaboration_tool,task_starter,workspace_factory}.rs`、`src/transport/{collaboration_client,orchestration_client,contract_http_server}.rs`。`bridge.rs` 把 journal/trackers 移入 `Arc<Mutex<BridgeState>>`，**修复原 emit 为 no-op 闭包导致 Level-2 事件全被吞的真 bug**，新增 `BridgeReporter`。审批回环：`RuntimeApprovalSource` trait + `await_runtime_tool_approval` + `runtime_approval_confirm_handler`。实测 55 → 125 passed。
- **装配路径真 bug（同类第二次）**：`AgentScopeAdapter` 漏实现 `FrameworkAdapter::on_bridge_attached` → `bridge.attach()` 注入点缺失（trait 默认实现为空）→ reporter 恒 None → Level-2 事件全丢。根因：旧测试直接塞 `shared.reporter` 绕开装配路径。已修 + 回归测试（走真实 `crate::instrument` 入口）。
- **HITL 模型裁定**：Java 用 `GenerateReason::PERMISSION_ASKING` + `METADATA_CONFIRM_RESULTS` 恢复（32 轮上限）；**Rust core 无此路径**，走 `core.confirm_handler` **内联 await**（未装 handler 则 `Err("permission asking (HITL) is not yet supported")`）。⇒ Rust 映射 = **安装 `ConfirmHandler`**（`HarnessAgent::set_confirm_handler`，`harness_agent.rs:282`）；语义等价、无 32 轮上限；差异：控制面不可达时 **deny**（Java 抛异常）。
- **已知语义差异（非缺口）**：Java 按 (userId,sessionId) 多 state 槽，Rust core **单槽** → adapter 用「session_id 不匹配即返回空」+ `distributed_store().agent_state_store` 桥接；token 从 `AgentResult.reply` 的 `Msg.usage` 取（`ModelCallEnd` 为空 EventData）。
- **harness 侧显式差异（非缺口）**：①builder 无 MCP/工具 allow-deny → `tools()` 只落 `Toolkit::remove_tool`，`mcpServers` 建连归宿主；②MCP 工具名 Java `server__tool` vs Rust 原始名；③`builder.model(String)` → `with_model_resolver`；④`awaitRuntimeToolApproval` 未实现时不做 PERMISSION_ASKING 回环；⑤Java `boundedElastic` 非阻塞 → Rust `start` 全程 await。
- **剩余未完成**：`GrpcTransport`（需 ASDP proto 工具链）；`SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource`/`AgentTaskStarter` 宿主实现。

## STUDIO P2
- `src/studio/socket_io.rs`：Socket.IO/Engine.IO v4 真实传输，`tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots，与 CHANNEL 同版本）；纯帧层 + `SocketIoProtocol` 状态机 + 重连。
- `src/telemetry/exporter.rs`：`Span`/`SpanKind`/`SpanStatus` + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait（`OtlpHttpExporter`/`RecordingTraceExporter`/`NoopTraceExporter`）。实测 30 → 52 passed。
- **OTLP 导出不引入 `opentelemetry`/`opentelemetry-otlp`**，走「降级 exporter trait + 编码器」；`TelemetryTracer` 仍为待办（裁决 B5：tracer seam 归并到既有 exporter 方案，trait 注入 `Arc<dyn TraceExporter>`，对应 Java `9b611928e`）。
- `encode_attribute_value`：JSON 数组→`arrayValue`、对象→`kvlistValue`（递归），**仅 `null` 降级空串**。`is_hex_id`：**空字符串永不合法**。`initialize_with` 默认仍 `NoopStudioTransport`（避免离线测试真连 `localhost:3000`）。

## 跨模块裁定 / 阻塞
- **list_hash 已全量收敛到 `agentscope_core::state::list_hash`**：JDBC/PostgreSQL/MONGODB/COS/REDIS 5 个用 core，MySQL 无此逻辑。**严禁恢复本地实现**（`rg "fn compute_hash|fn needs_full_rewrite"` 应无结果）。曾修两处真实 Java 偏离：MONGODB/COS 的 `needs_full_rewrite` 在 `stored_hash==None && existing_count==0` 时误返 true（Java 返 false）；COS 曾用 `whole_hash.starts_with(stored)`——**全量 SHA-256 无前缀性质**。存量数据自愈（每 slot 首次 `save_list` 多一次全量重写，数据无损）。
- **滚动 tracker 状态口径**：以独立 tracker 为单一事实来源；滚动视图**不得高于**独立 tracker；汇总低于独立 tracker 时可修正。
- harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`）：各 crate 用 `to_harness_store()`/`to_distributed_store()` 投影子集；**不改 harness**。`SandboxExecutionGuard` 为具体 struct → JDBC 锁无法驱动沙箱隔离。
- **待裁定/并存**：`anthropic` 自建 wire DTO + `anthropic_async`（SDK 0.6）**并存**（裁决 B2=B）；RAG embedding provider 与 MODEL 边界；`supports_native_locks()` 语义。
- 真实驱动未进根 `[workspace.dependencies]`（MONGODB `mongodb`、REDIS `redis` 1.7.0）由各 crate 自声明。
- JDBC 跨语言不兼容：会话表列名 `slot_id` vs Java `session_id`；锁表名 `{prefix}lock` vs `{prefix}distributed_locks`。
- MySQL 键列已加 `COLLATE utf8mb4_bin`（`=`/`LIKE 'prefix%'` 转大小写敏感）→ 运维文档 `docs/migrations/mysql_binary_collation.md`（影响面 + `ALTER TABLE ... MODIFY <key> VARCHAR(...) COLLATE utf8mb4_bin` 脚本覆盖 `agentscope_store`/snapshots + 查重 SQL + 低峰执行 + **已写入大小写变体键会丢其中一行**告警 + 回滚 + 已知未修 `dialect/vendor/mysql.rs:79` session 表仍 `utf8mb4_unicode_ci`）。
- P2 清理项（勿误当 bug）：REDIS `sandbox.rs:175 released` 未读、`clear_all_sessions`（6 处，Java 已删）；MONGODB `state_store.rs:433 delete_key_inner` 仅测试引用；JDBC device helper dead_code；`channel/src/{feishu,wecom}/token.rs` 的 `StaticTokenProvider` 未使用导入。
- 遗留不可离线项：JDBC `MysqlIntegrationTest`/`PostgresIntegrationTest`（需 Docker）；AISTIO `GrpcTransport`；MODEL dashscope 长尾。

## Java 迁移裁决清单（2026-10-19 正式裁定，后续实施以此为准）
- **A1=A** 跟随 Java 新 session/context 设计（`a2cbea5c3`：新建 `core/session` + `harness/agent/{context,session,observation,verification}`，废弃 SessionTree/SessionEntry/TranscriptMiddleware/transcript）——大重构（**2026-10-20 双轨 session 已实施完毕**，见末节）。
- **A2=A** 版本化/CAS **上提为 core `AgentStateStore` trait 方法**（`7d08f8fd`、`e05d45e08`、JDBC `d786d8fb5`/`7854f0de7`）。**A3=A** 引入 **run 级控制**（`AgentRun`/`RunControl`/`run_id`）。**A4=A** `ChatUsage` 扩 usage 三字段 + `ToolSchema`/`Tool` 增 `defer_loading`，授权改 core。**A5=A** 响应侧 schema 校验 + strict 合并 + `ToolChoice::Specific` 能力位。**A6=A** 补发 `RequireExternalExecution` 事件。
- **B1=A** skill namespace 归 **skills crate**；**B2=B** anthropic 双实现并存；**B3=A** MCP meta 传播默认 **fail-open**；**B4=A** 新建 judge/jev crate；**B5=A** tracer seam 归并 STUDIO telemetry exporter；**B6=A** 补 plan-mode 白名单与拒绝逻辑。
- **C1=A** 引入 **liteparse** 做 RAG 文档解析；**C2=A** Redis **Cluster hash tag**；**C3=A** 接受 JDBC **破坏性重构**；**C4=A** server tools（Gemini/Anthropic）立项改 core；**C5=A** MySQL COLLATE 出迁移脚本/运维说明。
- **D1=A** 保留 `crates/agentscope-harness/memory/2026-10-19.md`；**D2=C** 改动继续叠加（分支 `dev`）；**D3=A** 主智能体逐个 grep+跑测试核实并修 4 处「已落盘未接线」。
- 约束：A1/A2/A3/A4/A5/A6/C4 全部改 **core** → **core 必须由单智能体串行独占**。

## 教训（最重要，务必遵守）
- **未经验证的断言不得落笔**：不要把「没见过调用」当「未被调用」。凡断言「X 未被使用 / 唯独 Y 偏离」**必须先 grep 全仓**。
- **grep 零命中 ≠ 未实现**：零命中结论须用 2-3 个同义词交叉验证（曾把 e2b `/files`（实际 `files_url`/`upload_file`/`download_file`）与 Redis hash tag（实际 `KeyLayoutVersion`）误判缺失）。
- **tracker 会过时，汇总层最易腐**：「单一事实来源」≠「可信」，引用前须以源码 + `lib.rs` 的 `mod` 声明复核。独立 tracker 也会过时；**独立 tracker 未被汇总方编辑时更可信，过时的一般是汇总行**。更正一处描述后**必须 grep 同义旧措辞全文清零**。
- **「文件已落盘但从未接线」多次发生**（`common/channel_core.rs`、`channel_registry.rs`、`dingtalk/stream.rs`、AISTIO `on_bridge_attached`、D3 四处）→ 复核先 `grep mod.rs` 确认模块已声明，再看 `git status` 是否 `??`，最后跑测试。
- **「测试绿」≠「生产路径正确」**：凡「A 靠 B 注入、B 由 trait 默认实现兜底」的装配点，必须有一条**走真实装配入口**的测试；直接 poke 内部字段会掩盖漏实现 trait 方法。审核须加「trait 方法是否漏实现」维度。
- 工作区中非本会话产生的改动**未必违规**（可能用户手动编辑）→ 报告并询问，勿自行判定「越界」而还原。该仓库可能被另一会话/进程并发编辑（曾见 `react_agent.rs` 被外部重写）→ 提交前用 mtime 采样确认稳定。
- 子智能体产出必须独立复核（构建/测试日志）后才可计入。**通病**：①**不交回执**（改动落盘却返回空）→ 判定产出必须看 `git status` + 跑测试；②**改一半**（只改局部、漏主循环/配对路径，遗留陈旧断言）→ 主智能体须逐项对照 Java 提交复核；③**上下文爆掉静默失败** → 禁 `git show` 全量（先 `--stat` 再逐文件）、限输出行数、要求落盘回执。
- 测试有效性须**反向验证**：逐个回退实现 → 跑对应测试确认 FAILED → 恢复。注意「无效回退」。
- `Select-String 'FAILED|panicked'` 会**误命中测试名**与 `test result: ok ... 0 failed` 行——判定失败须看 `^failures:` 段是否存在。
- `memory_save` 无法覆盖已存在的记忆文件（报 already exists）→ 另建路径或追加。
- **工具坑**：`edit_file` 的 `new_string` **不解析 `\n` 转义**（会写字面 `\n`）；`write_file` 正常。多行内容用 `write_file` 分片写 `_part_*.md` 再 `[System.IO.File]::AppendAllText` 拼接（`.Replace('\n',[Environment]::NewLine)` 修复字面 `\n`）。`write_file`/`edit_file` payload 内**空双引号 `""` 或大段 `\"` 转义**会破坏 JSON → 报 `Parameter validation failed: "path" is a required property`；规避=大段替换先把 old/new 写临时文件再用 `[System.IO.File]::ReadAllText` + `.Replace()` 落盘（先打印 `occurrences=` 确认）。`edit_file` 末尾多/少带一行会「误删相邻行」→ 每次替换后立即 read_file 核对。

## 关键行为约定（core / harness）
- **ACP 思考参数：只发 `reasoning_effort`**。`harness/acp/session.rs::thought_level_params` 所有模型统一下发 `reasoning_effort`，**永不**写 `enable_thinking`/`thinking_budget` 到 `/chat/completions` body。`auto`/空→default；`disabled`/`none`→`minimal`；`max`→`high`；其余原样。`GenerateOptions.thinking_budget` 保留仅供 **DashScope 原生 API**。测试：`cargo test -p agentscope-harness --lib acp::session::tests`。
- **ReActAgent 400 / reasoning_content**：①空壳 assistant 消息已修（空文本也发 `content:""`，`core/src/model/openai.rs:336-366`）；②`reasoning_content` 须完整回传（DeepSeek 带 tools 硬要求，否则 400；无条件回放，**缺则回填空串**），provider 级开关（模型名含 deepseek 自动开）。落地障碍：async-openai 0.41 无该字段 → 走 `Value` 通道（`serde_json::to_value` + merge `extra_body`），**不能用 `additional_body_params`**；`to_request_messages` 非 1:1 不能按下标回填。参考 **flux**（Go）。未处理：` thinking...</think>` 内联拆分、`reasoning_details` 迁移。
- **model provider 目录重构**：`core/src/model/{openai,responses,anthropic}.rs` **整体迁入** `extensions-model/src/{openai,responses,anthropic_async}/`；core 不再导出 `default_registry`/`OpenAiChatModel`/`OpenAiProvider`；harness 改 `use agentscope_extensions_model::OpenAiChatModel`。依赖下沉：`async-openai` 0.41 / `async-anthropic` 0.6 由 core 移入 extensions-model。
- **模型重试**：`extensions-model::common::error::ModelError::to_core()` 对「可重试但状态码非 429/5xx」返回 `Error::Http(...)`。重试在 core `react_agent.rs::model_call_impl`。默认 `ExecutionConfig::model_defaults()` = `max_attempts=7 / initial=2s / max_backoff=60s / mult=2.0`（等待序列 `[2,4,8,16,32,60]s`，单次 timeout 300s）。`tool_defaults`（max_attempts=1）未改。双前缀「`Agent error: Agent error: ...`」来自 `core/react_agent.rs:347` + `harness/acp/session.rs:771`。
- **中断/崩溃恢复**：①`run_agent_loop` 无条件下写 state（错误也持久化）；②`call_with_format` 入口 `interrupt.reset()`；③`Err(Error::Interrupted)` → `Ok(reconcile_interrupted(...))`；④接线 `enable_pending_tool_recovery` → `patch_pending_tool_calls`；⑤`StateCheckpointMiddleware::on_reasoning`（每迭代落盘）；⑥Ctrl-C/SIGTERM（harness tokio `signal` feature + shutdown handler）。**ACP 已开 `enable_pending_tool_recovery(true)`**（两个构建点）。`ReActAgent::enable_pending_tool_recovery() -> bool` 只读 getter 已加。
- **死代码清单（勿当「已实现」）**：①`Middleware::on_acting` + `MiddlewareChain::wrap_acting` **零调用**（`middleware.rs:244`）→ 用 `on_reasoning`/`on_agent_state_ready` 作挂点；②`GracefulShutdownManager` 无 `bind_state_saver` 等价物；③`AgentState.shutdown_interrupted` 死字段；④`AgentState::interrupt_control()` 返回 `new()` 而非字段；⑤`SubagentDeclaration.enable_pending_tool_recovery` 已接线。`react_agent.rs::tool_event` 在 HEAD 即 dead code。
- **core 迁移 Java `doCallInner` fail-fast**：`run_agent_loop` 开头，若 pending tool calls 非空、未开 recovery、messages 非空且调用方未自带 tool result → `Err(Error::Agent("Pending tool calls exist without results. ..."))`。未迁移（非缺口）：ASKING 分支（Rust 从不写 `Asking`，不可达）、resume 分支。
- **ACP（harness）**：`agent-client-protocol = "2.2.0"`（**移除 `unstable_tool_call_name`**，改逐开 6 个 unstable feature）。新增 `acp/support.rs`/`provider.rs`/`mcp_over_acp.rs`。compaction→ACP seam：`AgentEvent::Custom`（`kind="compaction"`）→ `acp/session.rs` 转通知（仅 client 广告 `session.compaction` 时）。MCP-over-ACP：client 声明 `McpServer::Acp`，agent 发 `mcp/connect|message|disconnect`。CLI 读 `ACP_SESSION_ID`（+ `ACP_SESSION_REPLAY`）。`ActiveSession` **非 Clone**，只能 `into_session()`。
- **双轨 session（2026-10-20 已对齐 Java，不做旧兼容）**：`HarnessAgentBuilder::session_history_mode` 默认 `EventLog`（`Legacy` 仅显式 opt-out）；`build()` 解析 `session_log_store` 一次（未提供则 `WorkspaceSessionLogStore` 兜底），**ReAct agent 与会话工具共用同一实例**；`ReActAgent::call_with_format_and_extensions` 在 **`call_gate` 内**（每 session 串行化）开 recorder 并 `start`/`result`/`finish`，已带 recorder 不重复开；`HarnessAgent` 新增门面 `session_key`/`session_log`/`inspect_session`/`session_transcript`/`session`（async，**不缓存句柄**——Java 的 `ConcurrentHashMap` 缓存会让 `Arc<HarnessAgent>` 自持造成引用循环）；三个 session 工具改持 `Arc<dyn SessionLogStore>` 经 `store.list()`+`transcript()` 读取；删除 `agent/transcript/`、`middleware/transcript.rs`、`WorkspaceManager::update_session_index`/`list_session_log_files`/`resolve_session_*`、`SESSIONS_STORE`/`SESSION_*_EXT` 常量、builder 旋钮 `transcript_*`；`memory/session.rs` 仅留 `SessionFreshnessEvaluator`。
  - **`AgentState::default()` 自动生成 `session_id`（`new_hex()`）** → 只要 `HarnessAgent` 有 model，`build()` 默认注入 store 后**每次调用都写 journal**（Java 默认行为，非 bug）。`local` 后端 journal 落 `WorkspaceSessionStorage` 命名空间（`agents/{agent}/sessions/{user}/{session}`）。
  - **`session_key(ctx)`**：session_id 与 user_id 各自独立取值，context 优先、缺失回落 agent state。
- **A2/A5/C4/A3 已实施并提交**（`dev`，未 push）：`9a8fd07` A2（版本化/CAS 上提 core `AgentStateStore`，5 存储 crate 实现，harness `state_checkpoint.rs` CAS+冲突回退）→ `deebf11` A5（`ToolChoice::Specific` 能力位 + `tool/validator.rs::validate_json_output` + 响应侧 schema 校验最多 3 次 + openai `strict_json_schema`）→ `6866c62` C4（core `is_server_tool` 标记 + `Msg::inline_server_tool_result_ids` + `has_actionable_tool_calls` + `extract_pending_tool_calls` 过滤 server tool；`ReasoningContext` 必须累积 `ToolResultBlock` 并在 `build_final_message` 输出，否则内联结果丢失；gemini `GeminiServerTool`/`merge_server_tools`，anthropic `ServerToolUse` + 6 个 `*ToolResult` 变体 + metadata 原样回放）→ `f201205` A3（新增 `core/src/agent/run.rs`：`RunStatus`/`RunControl`/`AgentRun`/`generate_run_id()` 32-hex；`RuntimeContext.run_id`；`AgentCore.active_run`；`call_with_format` 每次建 RunControl，`interrupt()` 优先 active_run，取消优先于结果）→ `12fc546` chore(memory)。回归：core(192)/model(267)/harness--lib(355)/jdbc(145) 全 0 failed。
  - **A3 产物是单个 `agent/run.rs`**（不是 `run_control.rs`+`agent_run.rs`）→ 复核勿按错文件名断言「未实现」。
  - **按功能拆分「一文件多特性」提交**：整文件归属单一特性的直接 `git stage <files>`；混合文件用**迭代式部分暂存**——`git diff -U0 -- <file>` → 按正则筛 hunk 写 LF 补丁 → `git apply --cached --unidiff-zero`，提交后重新生成 diff 避免行号漂移。`-U0` 补丁必须 LF 行尾（PowerShell 用 `` $sb.Append($l + "`n") ``）。`git diff` 的「移动伪影」（成对 `-`/`+` 文本相同）无害。辅助脚本 `.agentscope/workspace/split_hunk.ps1`。

## 2026-10-19 D3 执行与后续进展
- **D3 #1** `SessionSearchTool::register_all` 无调用 → 已在 `harness_agent.rs` 的 `!disableMemoryTools` 块接线。
- **D3 #2** `tool/fuzzy_text_matcher.rs` 孤儿 → 给 `agent/skill/manage.rs::SkillManageTool` 补 `edit`/`patch` 两 action 并接线 fuzzy 阶梯（唯一性校验 / `replace_all` 右→左 / `[fuzzy: level]` 后缀）。+4 测试。
- **D3 #3** `HarnessSkillMiddleware::invalidate()` 零调用 → 新增 `SkillManageTool::with_on_write(cb)`，装配处回填 `skill_mw_slot`，写后调 `invalidate()`。+1 测试。
- **D3 #4 = B6** plan-mode 白名单/拒绝：核心 `react_agent.rs` 新增 `state_handle()` getter + `plan_mode_denies()` + `PLAN_MODE_ALWAYS_ALLOWED`（plan_enter/plan_write/plan_exit/todo_write/agent_spawn/agent_send/agent_list/task_output/task_list）/`PLAN_MODE_DENY_MESSAGE`；`acting_phase` 在 `evaluate_permission` 前查，命中发 DENIED 不执行；`run_agent_loop` 末尾仅 `plan_mode_context` 变化时回写。harness `PlanModeHandle` 改 `Arc<std::sync::Mutex<AgentState>>` + `with_state(...)`，`plan_write` 锁内取路径后释放再 async 写；`harness_agent.rs` 构建后 `PlanModeHandle::with_state(...).register(&toolkit)`。
- **C5** 建 `docs/migrations/mysql_binary_collation.md`。
- 回归：core 110 / harness -lib 355 / plan_wait 4 passed。反向验证 2 项。
- **后续进展**：A2/A3/A4/A5/A6/C2/C4/C5 及双轨 session 已实施并提交；仍待 **B1/B3/B5/C1/C3** 与 A1 收尾。

## Session/Context 迁移在办（2026-10-21）
- 指导文档 `docs/harness-session-context-migration.md`（§5 编号待办）；已开工 #5/#6/#7。
- **#7 `ContextModelCalls::auxiliary`**（对齐 Java `harness/agent/context/ContextModelCalls.java`，调用方 `MemoryFlushManager`、`ConversationCompactor`）：新建 `crates/agentscope-harness/src/agent/context/model_calls.rs` + `context/mod.rs` re-export；用 `ContextPipeline::with_policy(default)` + `ContextRequest{ purpose: Summary }` 调 `model.stream` 收集 chunk；manifest 由 `observe` 发、不重复；该路径**不得再经 `ContextMiddleware`**（防递归）；接线 `memory/flush.rs::collect_text`（失败不报错）与 `memory/compaction.rs::summarize_prefix`（`ctx=None`、错误 `?` 透传、空摘要仍报错）。依赖 `core::session::SessionModels::wrap(model, ctx, purpose) -> Arc<dyn Model>`（并行子智能体实现）。
- **#5 `SqliteSessionStorage`**：`root/journal.sqlite3`，表 `session_objects(path TEXT PRIMARY KEY, version INTEGER NOT NULL CHECK(version>0), payload BLOB NOT NULL)`；`busy_timeout=10000`/`journal_mode=DELETE`/`synchronous=FULL`；`expected==0` 用 `INSERT ... ON CONFLICT(path) DO NOTHING`，否则 `UPDATE ... version=version+1 ... WHERE path=? AND version=?`；`expected==u64::MAX` 非法；文件不存在时 `read` 返回 `Ok(None)`；`list_paths` 用 `substr` 前缀过滤。
- **#6 `StoreSessionStorage`**：依赖 `BaseStore` 新能力位 `supports_atomic_session_storage()`（默认 false，仅 `InMemoryStore` 在 `put_if_version` 真 CAS 时 true）；namespace 尾追加 `__agentscope_session_log_v1__`；value `{"bytes": base64}`；`read` 对 `version < 1` 报 unversioned 错误；`list_paths` 用 `search(ns, 256, offset)` 分页 + `BTreeSet` 去重排序。

## Memory 工具与维护管线
- 记忆工具（memory_get/save/search）的 workspace 根 = **仓库根**（非 `.agentscope/workspace`）：产物为根 `MEMORY.md`（git 跟踪，committed `4eb1251`）+ `memory/YYYY-MM-DD.md` 日账 + `memory/.consolidation_state` 水位；`.agentscope/workspace` 只放 skills 缓存/tmp 与 `MEMORY.md.bak`。
- `MemorySearchTool` 经 `WorkspaceManager::list_memory_file_paths` 取文件，走 `fs.glob("*.md","memory")` 而 `LocalFilesystem::glob_recursive` **递归**，会扫到 `memory/` 下任意子目录。现存异常：①`memory/memory/` 冗余嵌套（3 个手工笔记）；②`crates/agentscope-harness/MEMORY.md` 仅 4 字节 `done`（与根 memory/ 是两套无关数据）。
- 2026-10-19 接线惰性配置 `MemoryConfig.consolidation_min_gap`：`harness_agent.rs:1178` 由 `MemoryMaintenanceMiddleware::new(...)`（硬编码 1800s）改为 `new_with_config(..., consolidation_min_gap, IsolationScope::User)`；顺带接线 `daily_file_retention_days`/`session_retention_days`。新增 getter `min_gap()` + lockstep 测试 `memory::tests::consolidation_min_gap_default_matches_maintenance_middleware`。**测试盲区（已裁定不修）**：`MiddlewareChain.middlewares` 私有、`dyn Middleware` 无 downcast → 靠「单一构造点 + lockstep」保证。
- **记忆管线是活的**：本轮会话自身 agent 调用触发维护，`memory/.consolidation_state` 曾由 `2026-10-08T08:52:54Z` 推进到 `2026-10-19T02:05:48Z`。闸门只是延迟，长会话终会跑到。副作用：consolidate **整篇覆盖** `MEMORY.md`，会吞掉未被日账覆盖的手工编辑。

## Git 提交历史
- `.gitignore` 仅 4 行（`.agentscope`/`agents`/`/target/**`/`/MEMORY.md`）——`memory/*.md` 与 `.agents/context/**` **被跟踪**；harness `memory/` 目录有意跟踪、每次提交同步。
- 提交信息含中文：先 `write_file` 写 `.agentscope/workspace/commitmsg.txt` 再 `git commit -F`（后 `Remove-Item`）。本地产物忽略写 `.git/info/exclude`（`flux/`、`.XCoding/settings.local.json`），不改仓库 `.gitignore`。
- 需精确控制用显式路径 `git stage <paths...>`（勿盲用 `git stage -A`）。
- 关键提交：`d86a578`（model provider 迁入 extensions-model）、`2816e4a`（AISTIO/STUDIO/JDBC 补缺波次）、`275b2f1`（模型重试 + 退避拉长）、`7619c33`（ACP 升 2.2.0）、`6f02226`（审批路径替换黑名单断言）；13 提交系列 `6875196`(docs)→`fa7cc4d`(judge)→`5165b28`(redis)→`347e77d`(agui)→`1097ce9`(sandbox)…；`9a8fd07`(A2)→`deebf11`(A5)→`6866c62`(C4)→`f201205`(A3)→`12fc546`(memory)。
- 分支变化无常（`main`/`dev`/`Endless/seafan`），**提交前先 `git branch` 确认**，勿凭记忆。当前实施分支 `dev`。
- 提交前回归惯例：`.bat` 后台跑 + 读日志，确认 `cargo build --workspace` EXIT 0 与相关 crate 测试 0 failed。

## PowerShell rg 用法（重要）
- **rg 通配符必用 `-g`，不要放 path 位置参数**：Windows 的 PowerShell/cmd 不给原生 exe 展开 glob，rg 的位置参数（PATTERN 之后的 path）**永不**做 glob → 收到字面 `*.rs` 报 `os error 123`。
  - `rg <pat> <目录> -g '*.rs'`；`rg --files <目录> -g '*.rs'`。
  - 需路径级 glob：**`-g` 写 cwd 相对全路径 + 根固定 `.`**，如 `rg --files -g 'crates/agentscope-core/src/*.rs' .`。
  - 锚点坑：含 `/` 的 glob 匹配**相对 cwd 的打印路径**，不是相对搜索根 → `rg --files crates -g 'agentscope-core/src/*.rs'` 返回 0；`-g` 值用正斜杠。
  - 备选：`rg --files @(Resolve-Path 'dir/*.rs').Path`（预展开）；或 `sh.exe -c "cd /e/... && rg ..."`。
  - **profile 包装函数不可依赖**（`execute` 工具总 `pwsh -NoProfile -EncodedCommand` 启动，`$PROFILE` 不加载）→ **已裁定不写包装函数**。

## harness grep/glob 能力增强（2026-10-20，工作区未提交，第 1+2 档已落地）
- 新增 workspace 依赖 `ignore = "0.4"`（根 `Cargo.toml` + `crates/agentscope-harness/Cargo.toml`）。改动：`src/filesystem/local.rs`、`src/filesystem/remote/mod.rs`、`src/tool/filesystem_tools.rs`、`src/tool/mod.rs`、新增 `src/tool/grep_options.rs`、`tests/harness_filesystem_test.rs`。
- **glob 语义对齐 `rg -g`**：`GrepGlob`（local.rs）——pattern 含 `/` 时匹配「相对搜索根的路径」，否则匹配「basename（保持任意深度递归）」。`src/*.rs`、`src/**/*.rs` 现可用。`remote` 侧 `relative_to()` 同步。
- **.gitignore/.ignore 支持**：`build_walker()` = `ignore::WalkBuilder` + `.hidden(false)` + `.require_git(false)`（**关键**：不加则非 git 仓库/临时目录里 .gitignore 不生效）+ `.sort_by_file_name()` + `filter_entry` 仅剪**目录**。同步 walker 跑 `spawn_blocking`。被 .gitignore 忽略的目录作为**搜索根**时仍可枚举（父级规则不会清空搜索根）。
- **二进制/编码**：`read_text()` 用 bytes→前 8KiB 含 NUL 判二进制跳过，否则 `from_utf8_lossy`。
- **工具层新增参数**（`grep_options.rs`，**不改 `Filesystem` trait 签名**）：`ignore_case`、`output_mode`（`content`/`files_with_matches`/`count`）、`type`（21 类→glob，与 `glob` 互斥）、`paths`（多路径）。
- **修复两个真缺陷（均反向验证）**：①`(?i)` 原靠字符串前缀，正则编译失败回退字面量时 `(?i)` 变可匹配文本 → 改 `PatternMatcher::new` 剥离 `(?i)` + `RegexBuilder::case_insensitive`，`Literal { text, ignore_case }` 在回退路径也生效；②`filter_entry` 原不看类型，会把名为 `build`/`dist` 的普通文件剪掉 → 加 `is_dir()`。
- 实测：harness `--lib` 398 passed；fs/memory 集成测试 66 passed；`cargo build --workspace` EXIT 0。反向验证 5 项。**未做（已评估）**：上下文行 `-A/-B/-C`、多行 `-U`、`--replace`；仍无 rg 并行遍历性能。
- 2026-10-21 完成 `docs/harness-session-context-migration.md` §5 #3 的 harness 接线（item3）与波次3：
  - `HarnessAgentBuilder::task_context(TaskContextOptions)`（对齐 Java `Builder.taskContext`）；`build()` 注册 `ContextMiddleware` 的门禁由「有材料」扩为「有材料 **或** `TaskContextOptions::is_enabled()`」。
  - **关键 core 修复**：`react_agent.rs::reasoning_phase` 必须每步 `state.tasks_context = core.tasks.lock().clone()`——`todo_write` 改的是共享任务袋，只有调用结束才回写 `AgentState`，否则 context 编译器与 manifest 的 `task_revision` 恒为 0（测试表现为 revisions `[0,0,0]`）。
  - 新增测试：`crates/agentscope-harness/tests/harness_task_context_wiring_test.rs`（manifest revision 0/1/2、陈旧 receipt 降级为 "Todo update recorded at revision N"、未 opt-in 时编译器不注册）、`crates/agentscope-core/tests/session_step_boundary.rs`（每步一个 `step/start`、summarizing 不开步、运行前 steer 在第一步折叠且 `inbox/applied` 仅一次）。
  - 反向验证 5 项全部生效：`session_continues→Ok(false)`、去掉 `before_step`、`iter >= max_iterations` 改 `>`、删 `core.tasks` 折叠、去掉 `is_enabled()` 门禁。
  - 实测：`cargo build --workspace` EXIT 0；`cargo test -p agentscope-core` 280 lib + 全部集成 passed；`cargo test -p agentscope-harness --no-fail-fast` 44 target 全 passed（`--lib` 430）。
- **既有红测（非本轮引入）**：`harness_tool_test.rs::filesystem_tools_registered_and_dispatch` 断言 `### /a.txt` 已过期——提交 `580691b` 有意「批量编辑工具输出去掉多余的 `###` 前缀」，测试未同步；已改为断言路径子串。教训：实现格式变更提交后须 grep 测试里的旧格式字面量。
- **仍未迁移（core 侧，需另立项）**：Java `RequirementTools`（`task_requirement_propose`）与 `TaskReminderMiddleware` 的 requirements 提醒位在 Rust 缺失 → `TaskContextOptions::requirement_proposals` 目前只能由宿主直接调 `TaskContextState::propose` 使用，模型侧无提议入口；`VerificationService` 的 `StateStoreActionObserver` 回退存储与 `CustomEvent("task_verification")` 发射同样未迁移（Java 侧 `verify` 也只有测试调用，属 caller-authorized API，非死代码）。

- 2026-10-21 工作区全量测试基线更新：`cargo test --workspace -j 2 --no-fail-fast` = **112 targets / 3397 passed / 1 failed / 1 ignored**（旧基线 103 targets / 3234 passed）。唯一失败为既有 flake `agentscope-extensions-training::runner::tests::run_registry_allocates_incrementing_ids`（`OnceLock` 静态注册表竞争），`cargo test -p agentscope-extensions-training --lib` 单跑 **64 passed / 0 failed** 即证非回归。1 个 ignored 仍是 `platform_channel.rs` 的 ```ignore 文档示例。
- **磁盘释放操作已验证有效**：`Get-ChildItem target -Recurse -Filter *.pdb | Remove-Item -Force` 清掉 **383 个 / 39.68 GB**，E 盘空闲 0.77 GB → 39.93 GB；全量测试跑完仍余 24.7 GB。注意删除后所有二进制需重新链接（本轮全量跑含重链接，约 8 分钟出结果）。
- 全量跑法（避免 300s 工具超时 + 巨量输出）：`Start-Process cmd /c "cargo test --workspace -j 2 --no-fail-fast > %TEMP%\ws_full.log 2>&1" -WindowStyle Hidden` 后轮询 `Get-Process cargo,rustc,link` 与日志行数；汇总用单行正则 `test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored` 累加，别用管道直连 cargo。

- 2026-10-21 session/context 迁移落地已按特性拆成 8 个提交（分支 `dev`，未 push，工作树仅余未跟踪的 `.agents/context/state/run-wstest.bat`）：
  - `c2cb828` feat(core): task context 版本化与投影（`state/context.rs`、`state/requirement.rs`、`middleware/task_context_projection.rs`、`tool/todo.rs` 回执元数据）
  - `85c9735` feat(core): session 执行运行时（`session/models.rs`、`recorder.prepare_model`、`execution` 开闭状态改 `Arc<AtomicBool>`、`reasoning_phase` 步边界 + `core.tasks` 折叠、`run_agent_loop` settle 回环、`tests/session_{settle_loop,step_boundary}.rs`；core 新增 `async-stream = "0.3"`，与 extensions-model 同写法，非 workspace 依赖）
  - `afddd13` feat(harness): context 编译器（`prepare_with_task` 投影 + `task_revision`、`ContextEviction` 预算 pass、`HarnessAgentBuilder::task_context` + `is_enabled()` 门禁、`context/model_calls.rs` 辅助调用）
  - `0ef8b1c` feat(harness): session journal 后端（`session/sqlite.rs`、`session/store.rs`、`BaseStore::supports_atomic_session_storage` + 6 存储 crate 覆写、删 `session.rs::launch` 重复 `before_step`）
  - `df7215a` feat(harness): `agent/verification`（`verify` 生产者）→ `f41d488` test(harness) → `46e19c6` docs → `cad5d0d` chore(memory)
- 提交顺序经验：core 必须**先 task context 后 session 运行时**（`react_agent.rs` 依赖 `TASK_CONTEXT_STATE_CONTEXT_KEY`），否则中间提交不可编译；`harness/context/mod.rs` 同时包含投影与 `model_calls` 导出 → 两者必须同一个提交。
- `write_file` 不能覆盖已存在文件 → 提交信息改用 `commitmsg<N>.txt` 逐条新建（提交后批量 `Remove-Item`）；`git commit -F` 输出中文标题可能显示乱码，以 `git log --oneline` 复核为准。

- 2026-10-21 ACP 已接入任务投影（提交 `4a25cc1`）：`acp/acp_harness_agent.rs` 与 `acp/coding_agent.rs` 两个构建点都加 `.task_context(TaskContextOptions { requirements: true, verification_results: true, requirement_proposals: false })`；只开投影、**不注册 source**（避免与 `WorkspaceContextMiddleware` 的 system prompt 注入重复）。ACP 仍未接会话运行时（`run_prompt_inner` 直接 `stream_events`，见 `acp/session.rs:916`）与 verification 调用。
- **重要纠正（我曾误判）**：`ToolCallPairSanitizerMiddleware` 实现的是 **`on_reasoning`**（`tool_call_sanitizer.rs:100`），不是 `on_model_call`；`OnReasoning` 按扩展点顺序**先于**任何 `OnModelCall`，所以 `ContextMiddleware`（OnModelCall）永远看到清洗后的消息，「编译器跑在 sanitizer 外面导致 `invalid_tool_pairs`」的担忧不存在。教训：断言两个中间件的先后，必须确认它们属于**同一个扩展点**再看 `add` 顺序（`wrap_model_call` 反向包装 → 末尾注册 = 最内层）。
- `harness_agent.rs::build()` 已把 `ContextMiddleware` 从链首移到链尾（最内层 OnModelCall）：语义上是「在最终请求上做校验与预算」。回归测试：`harness_task_context_wiring_test::the_compiler_accepts_input_the_sanitizer_repaired`（孤儿 tool result 经真实 `build()` 不再到达模型；注意 memory 抽取类 auxiliary 调用会把历史展平成文本，断言必须看 `ToolResultBlock.id` 而不是子串）。
- 本轮验证：`cargo build -p agentscope-harness` EXIT 0；`--lib` 430 passed；`harness_task_context_wiring_test` 3 passed；ACP 集成测试 `acp_event_flow`/`acp_cancel`/`acp_elicitation`/`acp_session_resume` 分别 1/2/1/1 passed。

- **Java 双轨真相（2026-10-21 核实，务必按此对齐）**：native(EventLog) 模式下 **journal 是唯一权威**，`AgentStateStore` 根本不装进 `ReActAgent`。证据 `HarnessAgent.java:2655`：`if (legacySessionHistory) inner.stateStore(effectiveSession);` + `if (legacySessionHistory) inner.actionObserver(new StateStoreActionObserver(...))`，注释写明「Native sessions restore only from their journal. A separate state store is needed only for explicit legacy mode or sandbox infrastructure metadata.」。读路径 `ReActAgent.getAgentState` → `nativeWorkingCopy` → `SessionStateAccess.read(sessionLog, () -> initialState)` → `SessionProjection.restore()`（`state/checkpoint` 事实），`initialState` 仅在 projection 无状态时兜底；管理性改写走 `SessionStateAccess.change`（writer lease + `asOfSeq` 乐观基准 + 差量事实 `context/replaced`/`message/*`/`task|plan|permission/changed` + 一个 checkpoint），stale snapshot 直接被拒。
- **Rust 目前是反的，尚未修**：`harness_agent.rs:1813` 只要有 store 就无条件注册 `StateCheckpointMiddleware`（与 `session_history_mode` 无关）；`acp_harness_agent.rs:909` 显式注入 `JsonFileAgentStateStore` → **双写**；而 `open_owned_recorder` 只写 journal、不 restore → 「snapshot 读、journal 只写」。对账风险：两写之间崩溃会分叉（读 snapshot 重放已完成工具 / 丢 compaction 摘要）。**下一步（步骤 C，未做）**：ACP 去掉 `state_store`，改由 journal 恢复（Java parity）。
- **ACP 已改用会话运行时（提交 `a10abe1`）**：`AgentSession` 新增 `set_event_sink(Arc<dyn Fn(&AgentEvent)+Send+Sync>)`，`launch` 每事件转发（`Ok(_) => {}` 不再吞事件）；ACP `run_prompt_inner` 用 `self.agent.session(ctx)` + `submit(text)` + 事件回放，`cancel` → `session.interrupt()` + `cancel_pending(log, turn_id)` 关 turn（**interrupted 的 turn 不算 ended**，不关则下一个 prompt 被 "Continue the unfinished task" 拒绝）。
- **`AgentSession::interrupt` 顺序坑**：先 `inbox.accept(command)` 会等运行中 recorder 占的 writer lease（`LEASE=120s`）→ 取消耗时 121s。正确顺序 = **先 `self.agent.interrupt()` 再后台 spawn 记录命令**（实测 121s → 0.35s）。
- **core：模型调用必须可被中断**。`model_call_impl` 原先只在 reasoning 阶段开头 `check_interrupted`，provider 流挂起（如 `futures::stream::pending()`）时中断永不生效 → run 与 writer lease 永不释放。已加 `interrupt_pending(core, ctx)`（25ms 轮询 `check_interrupted`）与 `stream.next()` 做 `tokio::select!`。
- **core 真 bug：`SessionProjection::read` 对 `action/end` 的 `"result": null` 会整篇失败**（`serde_json::from_value::<ToolResultBlock>(null)`），旧写入方无条件记该字段 → 该 session 的所有 resume 报 `JSON error: invalid type: null, expected expected struct ToolResultBlock`。已改为 `.filter(|v| !v.is_null())` + 回归测试 `action_end_with_a_null_result_does_not_break_the_projection`。**教训**：journal 恢复路径以前没有生产调用者，这类脏数据只有真正接上读路径才暴露。
- 本轮验证：core lib 281 / harness lib 430 / `harness_session_test` 10（新增 `the_event_sink_observes_the_whole_run`）/ ACP `acp_event_flow`+`acp_cancel`+`acp_elicitation`+`acp_session_resume` 全绿，未跑全量 workspace。

- **journal 收为唯一历史权威（提交 `8ea34d5`，2026-10-21）**：`HarnessAgentBuilder::build()` 现在**只在 `SessionHistoryMode::Legacy` 下注册 `StateCheckpointMiddleware`**（对齐 Java `if (legacySessionHistory) inner.stateStore(...)`）；EventLog 下即使传了 `state_store` 也不写，`SubagentBlueprint.state_store` 同样按模式置空。若要快照语义必须显式 `.session_history_mode(SessionHistoryMode::Legacy)`（`SessionHistoryMode` 从 `agentscope_core::session` 导入，**不在** `agentscope_harness::agent`）。
- **`AgentSession::launch` 之前从未把恢复结果交给 agent（真 bug）**：它只拿 `SessionProjection::restore()` 建了工作副本，agent 跑的是自己内存里的旧 state ⇒ 「重启后恢复」在生产路径从未生效。已加 `*react.state_handle().lock() = restored`（并在恢复时用 agent 的 pinned `session_id`/`user_id` 覆盖，防写错 journal 键）。收尾 `recorder.finish()` 也改成取 run 结束时的**真实** state，否则终态 checkpoint 丢掉本条回复与工具结果。
- **`AgentSession` 合并流不会自然结束**：`run_stream_with_extensions` 用 `select(stream, delta_rx)`，`delta_rx` 要等 run 自己的 ctx 释放 ⇒ 被取消/失败的 run 若「等流结束」会永久挂住、writer lease 不释放。修法：`launch` 在 `AgentResult`/`AgentEnd`/首个 `Err` 处 `break`。**这是 ACP 取消慢 121s 的真正根因**（此前误判为 `inbox.accept` 等 lease）。
- **ACP 取消路径（`acp/session.rs::run_prompt_inner`）**：`cancel` → 直接 `self.agent.interrupt()`（`AgentSession::interrupt` 需要 journal 已有 run id，dispatch 尚未启动时拿不到）→ 轮询 `session.task()` 直到 settled → `cancel_pending(log, turn_id)` 放后台 spawn（它会等 writer lease）。实测 121s → <0.5s。
- ACP 两个构建点**都不再有 `state_store`**：`acp_harness_agent.rs` 与 `coding_agent.rs` 只保留 `.session_id(...)`；`acp_session_resume_test` 改为纯 journal 恢复（`.cwd(root)` + `.workspace(root/.agentscope/workspace)`）。
- 新增回归 `event_log_mode_writes_no_state_snapshot`（`InMemoryAgentStateStore::get_session_count() == 0`）。反向验证 2 项均生效：禁用恢复写回 → `acp_session_resume` FAILED；移除 Legacy 门禁 → 新测试 FAILED。

## session writer 租约与 ACP 会话 ID（2026-10-22，务必记住）
- **`Session writer ownership expired or fenced` 的根因是缺少租约心跳**：Rust 移植时把 Java `SessionRecorder` 的 1s 心跳整个去掉，只在 `flush()` 里 `renew`。一次**静默超过 120s**（`LEASE`）的运行（长模型流 / 慢工具）期间没有 flush ⇒ 租约到期 ⇒ 工具结束回写 `action/end` 时续租失败。`Error::ActionObservation` 是**不可重试**变体（工具已真实执行），所以整个 run 直接失败。修复 = `recorder.rs` 新增 `LeaseHeartbeat`（周期 `lease/4`，**只 renew 不 commit**）。**不要**改成后台 flush：会和 agent loop 自己的 `flush()` 抢同一 pending 缓冲 ⇒ 粘性 `Stale committed prefix`。
- **`close()` 必须先 `stop_heartbeat()`（abort **并 await**）再 `release`**：只 abort 不够，一次已在等存储的 renew 仍可能落在 release 之后把租约抢回来，留下永远没人关闭的 recorder。回归测试 `a_closed_recorder_stops_renewing_the_lease`。
- **ACP session id 曾是 `session-<epoch_millis>`（真 bug，已修）**：session id 就是 journal 键，同毫秒创建两个会话 ⇒ 同一 journal ⇒ 第二个 recorder fence 第一个。并行测试几乎必撞，生产多客户端并发新建会话同样会撞。已改 `session-<uuid v4>`（Java 侧新建会话一律 UUID）。**之前被误判成 flaky 的 `acp_elicitation_test` 就是它**。教训：flaky 先抓失败现场（打印真实错误）再下结论。
- **trait 方法 async 化是签名破坏性变更**：aistio `observer_passes_through_without_reporter` 未跟上 `on_system_prompt` 改 async，导致 `cargo check --workspace --all-targets` 报 `E0369`/`E0277`。改实现时必须同步 grep 所有调用点（含测试）。


- 2026-10-23 CHANNEL 模块 P1/P2 全关闭：新增 `common/media.rs`（ChannelMediaMetadata 移植：9 个稀疏键 + `marker_text` + `sanitize_marker_file_name`（`[]`/Cc/显式 Cf 区间/U+2028-29→空格、折叠 ASCII 空白、截断 80 code point）+ `put_if_present`）；DingTalk(picture/audio/video/file)/Feishu(image/file/folder/audio/media)/WeCom(image/voice/video) mapper 均已映射媒体。
- CHANNEL seam：`common::InboundEventDeduplicator`（impl for IdempotencyStore）+ `AccessTokenStore`/`CachedAccessToken`/`InMemoryAccessTokenStore`（**旧 `TokenCache` 已删除**，三个 provider 改用 `with_store(...)`）；`common::ChannelSeams{deduplicator,token_store}` 经各平台 `from_json_with(..., &ChannelSeams)` 注入（weixin 走 `create(..., &ChannelSeams)`，多一参）。`ChannelCore::with_components` 现收 `Arc<dyn InboundEventDeduplicator>`。
- CHANNEL 语义修正：`FeishuChannel::handle_callback` 新增普通事件 `header.token` 恒定时间比对（未配置也 401，Java parity）；`BotLoopGuard` 补 `last_event_ms`/`maybe_sweep`/`evict_idle_peers[_at]`/`tracked_peers`；`IdempotencyStore::first_seen_at` TTL 过期后刷新 `seen_at_ms` 再放行（窗口重启）。基线 channel **385 passed / 0 failed**（266 `#[test]` + 119 `#[tokio::test]`；文件 71、非空 16334 行）。
- 本机 Java checkout `D:\IdeaProjects\agentscope-java`（main d7091070）的 `agentscope-extensions-channel` **无 weixin 子模块**（只有 common/dingtalk/feishu/github/gitlab/wecom），故 CHANNEL tracker §2.7 的 weixin Java 基线无法在本机复核。
- 镜像提醒：`Get-Content` 默认按 ANSI 解码会把 UTF-8 中文/破折号显示成 `��`，判断文件真实内容要用 read_file 或 `-Encoding utf8`；否则 edit_file 的 old_string 会不匹配。

- 2026-10-23 CHANNEL tracker 归档整理：`docs/extensions/CHANNEL__GAP_TRACKER.md` 由 495 行精简为 **237 行**（仅「当前基线 + 逐类映射 + 缺口清单 + 设计决定/差异 + 已知限制 + 增量流程 + 验证基线 + 归档指针」）；历史过程记录拆到新建的 `docs/extensions/CHANNEL__MIGRATION_HISTORY.md`（**312 行，冻结**，收录原 §4 项目状态、§6 静态复核快照、§7 G1 修复、§8 G2–G8 修复，章节编号沿用原 §6/§7/§8 以便旧引用可解析）。主 tracker 内引用历史一律写作 `history §7.x / §8.x`。
- CHANNEL 增量跟踪约定：新 Java 特性在 tracker §3 表尾追加 `G9/G10…`（状态 `待办`），完成后补关闭日期 + `history §…` 引用；过程记录只写归档文件，不写回主 tracker；基线数字在 §1 与 §7 同步。

- 2026-10-28 完成 ACP 稳定性第二轮（提交 `c714c38`/`74f66ce`/`6d5ef89`/`523472e`/`dfcd4db`，分支 `dev` 未 push）：P0-7/P0-9/P0-10/P0-12/P1-1/P1-3/P1-5/P1-6/P1-7/P1-8/P1-10 已落地，另补完 strict schema（`Tool::is_strict` + `tool/schema.rs`）。
- 关键实现约定：`Error::ActionDispatch` = action/start 确认失败（工具未执行、可重试）；`Error::ActionObservation` 仍是 end 失败（run 级）。`checkpoint_now` 现在先 flush 再提交，reason `after_acting` 在工具结果写入 context 后立即落盘。
- harness `AgentSession::prepare_open_turn_before_submit`：submit 前若 open turn 可恢复（无 pending 交互、无 uncertain 工具）先排 `resume` 再接收新 prompt；`stop_active_run()` 只打断自己拥有的 run。ACP 不再调用 `abandon_unfinished`。
- ACP cancel 已从 `AtomicBool` 改为 `cancel_epoch: AtomicU64`（per-turn）；交互等待 600s 超时且权限超时=拒绝（不可落入「通道失败自动批准」分支）。
- ACP 私有扩展方法必须 `_` 前缀：`_agentscope/session/respond`（`ext.rs`，自实现 `JsonRpcMessage`/`JsonRpcRequest`，Response = `serde_json::Value`）；ACP 无标准 respond 通道。
- `WorkspaceSessionStorage` CAS 现持 `{path}.lock` 文件锁（create-if-absent，30s 陈旧锁可破），`list_paths` 需过滤 `.version`/`.lock`/`.tmp-`；`Filesystem::sync` 为 trait 默认 no-op，`LocalFilesystem` 实现 `sync_all`。
- 仍未做（文档 §8.3 已写明原因）：P0-2 journal segment/compaction、P0-11 默认路径迁移、P1-4 幂等键、P1-11/12 有界 channel（会丢 delta）、P1-14/15、P2。

- 2026-10-28：`execute` 输出压缩已合入 dev（提交 d1f60ec，原提交 e132ae0 在 `rescue` 分支，通过 cherry-pick -n 应用；唯一冲突是 `harness/src/lib.rs` 的 `pub use tool::{..}` 列表，已合并为「dev 原有全部导出 + compact_grep_like/CompactConfig/CompactReport/OUTPUT_COMPACT_ENV」）。
- 新模块 `crates/agentscope-harness/src/tool/output_compact.rs`：`compact_grep_like(text, &CompactConfig) -> CompactReport` 把连续同路径的 `path:line:text` 改写成 heading 布局（路径独占一行 + `N:text`），保守策略（仅严格变短才生效、不重排、不改写命中文本、含 `[paths grouped` 前缀则视为已压缩幂等返回）。开关 `AGENTSCOPE_OUTPUT_COMPACT=0/false/off/no`。`ShellExecuteTool` 新增 `with_output_compaction`，输出整形抽到 `pub(crate) format_execute_output(exit_code, output, truncated, &CompactConfig)` 以便无 sandbox 测试，压缩时写 metadata `agentscope.output_compact.{savedChars,hoistedFiles,hoistedLines}`；`HarnessAgentBuilder::shell_output_compaction(config)` 默认 `CompactConfig::from_env()`。
- 注意：本仓库 rustfmt 会展开短结构体字面量/链式调用，与补丁作者本地格式不同 → 应用外来补丁后必须 `cargo fmt -p <crate>` 再提交；但 `cargo fmt -p` 会连带重排该 crate 既有未格式化文件（本次 `tests/harness_shutdown_test.rs` 属既有偏差），应 `git checkout --` 还原非本补丁文件。
- 本次验证：`cargo check -p agentscope-harness --all-targets` EXIT=0；`--lib` 482 passed / 0 failed；`tool::output_compact` 19、`tool::shell_execute_tool` 6、`--test harness_tool_test` 8 全绿；反向验证（把 `saved_chars <= 0` 守卫短路）→ 3 测试 FAILED，恢复后全绿。

- ACP 稳定性第三轮（2026-10-28，未提交）：P1-4/P1-14/P1-15 已落地并全部测试通过。
- P1-4：`extensions-acp/src/session.rs::prompt_idempotency_key(message, accepted_at_ms)` = `prompt-<sha256前16hex>-<ts_ms>`，取代 `Uuid::new_v4()`；acp crate 新增 `sha2 = { workspace = true }`。**跨进程幂等仍未解决**（ACP PromptRequest 无客户端 request id，重启后重试=新 turn）。
- P1-14：`JournalSessionLog::visit` 改为「重放可读后缀」（`segments_through(.., tolerant=true)`，只容忍 `Error::Session`/`Error::Json`，IO 仍上报）；`scan`/`read_after` 保持严格；`SessionProjection::as_of_seq` 改为 `head.seq`；`views.rs::fold_tolerantly`（required=false 跳过、required=true 中止）；transcript/turns/pending/recorder.open 改走 visit；inbox 未知事件忽略+warn。
- P1-15：`SessionRecorder::flush` 新增 `rebase_onto_head`（先 renew 确认所有权，再按新 head 重编号 in-flight+pending 事件并换新 batch id，`FLUSH_REBASES=3`）；`is_stale_prefix` 与 transient 重试、fatal 三分。
- 反向验证 4 项全部 FAILED 通过；core lib 340、harness lib 482、acp 58 全绿。坑：`cargo fmt -p agentscope-core` 会顺手重排 `shutdown/mod.rs`（既有格式漂移），需 `git restore` 掉无关文件。

- ACP 稳定性 P2 全部推进（2026-10-28 第四轮，未提交）：P2-1/2/4/5/6/7/8/9/11/12/14/17/18/19 已修；P2-3（Drop 不能 await）/P2-10（其他 crate 仍有 unwrap）/P2-13（blob 懒加载需改 visit 签名）/P2-15（大对象独立 segment）/P2-16（chunk 采样）为部分；P2-20 随 P1-12 未做。
- P2 关键落地：`SessionRecorder::metrics()` + `SessionMetrics`（durable_seq/pending/replay_ms/heartbeat_age_ms/lease_ms）；`LeaseHeartbeat` 记录 `last_renew_ms`；`SessionExecution` 用 `OnceLock<SessionInbox>` 复用 fold 缓存；`replay_views` 单次 visit 折叠多视图（`open_views`/`admission_views`/`recovery_views`）；`SessionLogExporter::drain` 批量 256 + 每批一次游标推进。
- ACP：`save_session_config` 临时文件+fsync+rename；`MCP_CONNECT_TIMEOUT=60s`；MCP server 连接成功后才写入 `bridged_servers`（失败可重试）；`sort_session_infos` 按 `updated_at` 倒序 + id 兜底（先按 id 去重再排序）；`last_input_tokens`→`Arc<AtomicU64>`、`updated_at`→`Arc<Mutex<_>>`（clone 间共享）；acp 全 crate `lock().unwrap()` 改 poison 容忍。
- harness：dispatcher 改 `Notify` 唤醒 + idle 指数退避至 `IDLE_POLL_MAX=500ms`（`next_idle_backoff` 纯函数可测），有 active_run 保持 50ms。
- 教训补充：仓库内 `docs/*.md` 与部分源码为 **LF** 行尾，PowerShell 脚本拼接多行模式必须用 `` "`n" `` 而非 `[Environment]::NewLine`（CRLF），否则 `Contains` 恒 false。`cargo fmt --all` 会改动仓库既有未格式化文件（`core/src/shutdown/mod.rs`、`extensions-sandbox/src/e2b.rs`、`harness/tests/harness_shutdown_test.rs`），只应 `rustfmt --check` 改动文件，勿全仓 fmt。
- 反向验证脚本：`.agentscope/workspace/reverse_p2.ps1`（回退实现→跑指定测试→恢复），14 项全部如预期 FAILED。

- 提交记录（2026-10-28 ACP 稳定性 P1+P2）：`04b5a30` core session（P1-14/P1-15 + P2 core）、`c2ea444` harness dispatcher、`6775164` acp 幂等键+P2、`f37d58d` 文档、`854de42` acp 有界事件队列（P1-12/P2-20）+ P2-15 回归测试。
- **反向验证必须 bump mtime**：用 `Copy-Item` 恢复被改坏的源文件时 PowerShell 会保留备份的旧 LastWriteTime，cargo 认为文件未变而**复用回退版构建**，导致「恢复后仍 FAILED」或「恢复后假 ok」。恢复后必须 `(Get-Item $p).LastWriteTime = Get-Date` 再跑测试。
- ACP 事件队列：`crates/agentscope-extensions-acp/src/event_queue.rs`（crate 私有模块），`EventQueue::new(4096)` + `EventQueueSender`（Drop 即 close，消费者 `recv()` 返回 None）。策略：满则丢**最新**事件并计数，`AgentResult` 永不丢；`dropped_deltas() > 0` 时 turn 结束前向客户端发「output was dropped」提示。core `react_agent.rs:845` 的 `delta_tx` 仍是 per-call 无界（未改）。
- blob/commit 不写 `.version` 的判定在 `harness/src/agent/session/storage.rs:117`（`/commits/` 或 `/blobs/`）；`/writer-seals/` **仍会**写 `.version`（有意，勿顺手改）。

- 2026-10-28 第四批提交：`91d9e1e`（P1-2 core：`interrupted_model_output` + 合并缓冲 1s 定时落盘）、`b54a2a4`（P1-2 harness：resume 用 assistant prefill 续写）、`c63af65`（P0-11 journal 迁到 `.agentscope/sessions` + 旧路径 fallback）、`87f1047`（P0-2 设计方案文档 + 状态更新）。
- **P0-11 实现要点**：`SessionKey::storage_path()` = `.agentscope/sessions/<agent>/<user>/<session>`，新增 `legacy_storage_path()` = 旧 `agents/<agent>/sessions/...`；`WorkspaceSessionLogStore::journal_prefix()` 优先新路径，只有旧位置存在（`head.json` 或 `session.json`）时原地打开（**不复制**，避免双写分叉）；`list()` 扫两处 + `sort`+`dedup`。匿名用户 encode 为空串，需 `Some(s).filter(|u| !u.is_empty())` 才能与 `user_id == None` 匹配。
- **P1-2 实现要点**：`interrupted_model_output(log)` 找「最后一条 `model/dispatch` 且无 `model/end`」的调用，把 `model/chunk` 的 `chunk`（ChatResponse）文本/thinking 拼回；harness `session.rs` 在 `command.kind == "resume"` 时**先弹掉尾部 `GenerateReason::Interrupted` 的恢复消息**（否则模型会去回答「I noticed that you have interrupted me…」），再保证半截文本是最后一条消息（已有就不重复加，加时带 `agentscope/prefill` metadata）。状态里通常已含该 partial（interrupt 时的 checkpoint），所以多数情况只是「弹恢复消息」。
- **合并缓冲定时落盘**：`MergedChunk::time_until_flush()` + `tokio::time::timeout` 包 `inner.next()`，`FLUSH_INTERVAL = 1s`；超时且 `chunks == 0` 要 `reset_window()`，否则 `since` 不推进会变成忙等。注意：定时器只在流被持续 poll 时才触发（测试里必须并发 poll，不能 `sleep` 后断言）。
- **P0-2 方案（未实现）**：`docs/session-journal-compaction-design.md` —— 不可变 `snapshots/<hash>.json` + `snapshot.json` 指针 CAS 发布（对象先写、指针是唯一提交点）、读路径「快照起步 + 尾段 `visit`」、retention 以「较旧快照的 asOfSeq」为截断点（`segments_through` 需感知边界）、保留 2 份快照。已否决：写侧多 batch 合并（崩溃丢 N batch）、单 append-only 文件（storage 无 append 原语）。分 S1-S4。

- 2026-10-28 提交 `1ca13e3`：P0-2 S1–S4（session journal 快照 + 指针发布 + 快照边界回收）落地。
- **快照实现要点（P0-2）**：`core/src/session/snapshot.rs` 定义 `SessionSnapshot{format,asOfSeq,asOfCommitId,turns,projection,pending,transcript,runs}`、`SnapshotPointer`、`SessionRetention`（默认 prune=true / 30 天，`AGENTSCOPE_SESSION_RETENTION=off|0|false|no` 关闭，数字=天数）。布局 `snapshots/<hash>.json`（不可变）+ `snapshot.json`（latest，唯一提交点 CAS）+ `snapshot.previous.json`（回收 floor）+ `reclaimed.json`（删除水位）。
- **与设计稿的两处必要偏离**：快照必须额外存 `pending`（admission/recovery 热路径都要它，否则快照白做）与 `runs`（最近 4096 个 run id，recorder 的「Run ID already used」守卫在从快照起步后看不到旧事件）；`SessionTurns/Turn/Transcript/SessionProjection` 为此加了 serde derive。
- **`AtomicSessionStorage` 新增 `remove`**（默认返回 Err=不支持→该后端跳过回收）：已实现 MemorySessionStorage / WorkspaceSessionStorage（连 `.version` 一起删）/ SqliteSessionStorage；`StoreSessionStorage`(BaseStore) 无删除 API 故不回收。workspace 侧 `/snapshots/` 归入 immutable 分支（不写 `.version`）且 `needs_cas_lock` 排除之。
- **读取语义**：`views::replay_views` 从快照起步折叠尾段（`log.snapshot()` + `log.commit_at(as_of_seq)` 双重校验，不匹配/损坏则全量重放）；`SessionProjection::read`/`transcript`/`pending_interactions`/`SessionTurns::read` 全部改走 `replay_views`。`scan(after,through)` 在 `after < floor` 时**显式报错**「events ... were reclaimed and cannot be read」；`visit` 返回可读后缀（含跨越 floor 的锚点 batch）；已回收且快照不可用 → `replay_views` 报错（绝不返回缺头部视图）。exporter cursor 落到 floor 之下时告警并跳到 floor 继续。
- **回收阈值**：`SNAPSHOT_EVERY_EVENTS=512`、`SNAPSHOT_EVERY=60s`、`PRUNE_MIN_EVENTS=1024`（floor 与 head 差不够不扫）、`PRUNE_BATCH=512`、`PRUNE_MAX_SCAN=4096`（只有完整扫过才做 blob GC）。
- **坑**：`cargo fmt --all` 会连带格式化其它会话留下的未格式化文件（`shutdown/mod.rs`、`acp_harness_agent.rs`、`e2b.rs`、`harness_shutdown_test.rs`）→ 必须 `git checkout --` 回退，改完用 `rustfmt --check <自己的文件>` 单独验证（这几个文件在 HEAD 本就不 fmt 干净，`cargo fmt --all --check` 会红）。反向验证脚本里「restored」一栏可能误报 FAILED，以事后直接单跑为准。
- **S4 只做了一半**：export/fork 适配已做（fork/export_state 走投影天然兼容；exporter 跳 floor），**transcript tool-result 摘要化未做**（会丢内容，属产品决策）。P1-11 仍为「部分」：快照里带完整 transcript，进程内内存仍随会话线性增长。
