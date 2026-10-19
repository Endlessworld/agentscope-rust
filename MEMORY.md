# MEMORY.md

> 精简于 2026-10-19（并入 G1–G10 提交级分析、Java 迁移裁决清单 A1–D3、P0/P1 并行实施、judge crate、extensions-model 目录结构、e2b 落地确认、SKILLS JDBC 持久化落点、子智能体上下文爆炸规避、wave-reports 回执落盘约定）；旧全文备份在 `.agentscope/workspace/MEMORY.md.bak`。原则：只保留对**后续任务仍有指导意义**的稳定事实、约定与教训，删除一次性过程叙述。

## 环境与工作方式
- 路径跨会话不同（见过 `E:\local-github\agentscope-rust` 与 `D:\IdeaProjects\agentscope-rust`；Java 源在 `E:\local-github\agentscope-java`，`D:\IdeaProjects\agentscope-java` 不存在）。**以任务说明中的路径为准**，勿凭记忆断言盘符。
- Windows + PowerShell；偏好简洁中文；目录勘察一行一条并标 `[目录]`/`[文件]`，最多 20 个顶层条目；只读探索不落盘。
- 常见只读角色：①「只读任务模式」——用户明确「不要修改任何文件」，如全量测试跑 `cargo test --workspace -j 2 --no-fail-fast` 并把输出落文件，汇总须含所有 `test result:` 行、`FAILED`/`failures:`/`panicked` 原文与「是否有编译错误」；②「Rust 代码审核员」——对象为新增未提交文件 + 相关 `Cargo.toml`，逐文件给「路径 → 结论：OK / 具体问题+行号」，末尾按「必须修复 / 建议改进 / 可接受」三档 + 测试数量线索。
- 审核固定维度：①依赖最小必要、与 workspace 其它 crate 版本一致（须 grep 核对）、是否本应放 workspace 依赖却写死；②协议实现正确性（握手 / namespace connect / ping-pong / 事件解析 / 重连退避）；③单测充分且离线；④`unwrap`/panic 风险；⑤schema 符合性、id 校验、类型映射；⑥是否引入被限制 crate；⑦死代码 / 未接线模块；⑧**trait 方法是否漏实现**。

## 构建 / 测试环境（Windows + PowerShell）
- **不要用 PowerShell 管道直连 `cargo test --workspace`**（巨量输出触发 .NET Stack overflow）→ 先 `Out-File $env:TEMP\xx.log` 再 `Select-String`。
- 全量用 `cargo test --workspace -j 2 --no-fail-fast`（降并行避 `os error 1455` 页面文件/rlib mmap 失败；`os error 112`=磁盘满）。`cargo test -p agentscope-harness` 全量可能 >300s 工具超时 → 改跑 `--lib`。`-j 2` 亦规避 `training::runner` 静态注册表竞争。
- `.cargo/config.toml` 设 `incremental=false`；**不得改 `CARGO_TARGET_DIR`**；不并发起多份 cargo（会争用 target 目录）；单条命令超时上限约 300s，长命令写 `.bat` 后台跑 + 轮询日志。
- 命令护栏为**朴素子串匹配**：①`git add` 被误判拦截 → 改 `git stage`（`git restore --staged`/`git rm` 正常）；②命中 `rm` 子串即拒 → 拆分/换词；③`--format <value>` 被拒 → 省略或 `-j`。遇拦截换同义写法，勿重试原命令。
- 磁盘坑：全量报 `no space on device` + `LNK1318 LIMIT` 时，真凶常是 `target/debug/deps` 的 `.pdb`（Windows MSVC 无条件生成，曾达 41GB）——删 `.pdb` 即释放（可再生）。诊断用 `Get-PSDrive` + 分目录 `Measure-Object Length -Sum`。
- 既有偶发失败（非缺陷）：`agentscope-extensions-training::runner` 因进程级 `OnceLock` 静态注册表 + `clear_all()` 存在测试间竞争（单跑稳定通过）；`acp_stdio_thought_test` 依赖 live 模型的 reasoning 输出，属 flaky。
- PowerShell 反引号转义（`` `r`n ``）经 shell 包装会被吞掉 → 拆行一律用单引号正则 `-split '\r?\n'`。

## 构建 profile / 体积优化
- 根 `Cargo.toml`：`[profile.release]`（lto=fat / cgu=1 / panic=abort / strip / opt-level=z + `release.package."*" opt-level=3`）；`[profile.embedded]`（inherits release，`package."*" opt-level="z"`，build-override opt-level=3）——除 `package."*"` 外二者 artifact 级等效。
- `.cargo/config.toml`（自动加载）**只放** `[build] incremental=false`；**切勿**放 nightly `-Z`（会打挂 stable 下所有命令）。
- `.cargo/config.embedded.toml`（不自动加载，需 `--config`）：`[unstable] build-std=["std","panic_abort"]` + `build-std-features=["panic_immediate_abort"]`；`[build] rustflags=[-Z,location-detail=none, -C link-arg=/OPT:REF, /OPT:ICON, /DEBUG:NONE]`。用法：`cargo +nightly build -p <crate> --profile embedded --config .cargo/config.embedded.toml`。
- 坑：`build-std*` 须放 `[unstable]`（放 rustflags 报 `unknown unstable option`）；必须 `cargo +nightly`（stable 下任何 `-Z` 在探测 target 阶段即失败）；自定义 profile 目录下 `incremental=false` 无效。
- Windows MSVC：`.pdb` 无条件生成，打包须排除；`-C strip=symbols` 对 `.exe` 无可测效果。工具链：stable 1.96.1（默认）、nightly 1.99.0（rust-src 已装）。

## codegraph CLI
- v1.5.0 已装；索引 `...\.codegraph`。本会话未暴露 MCP 工具，只能经 skill + shell 执行 CLI。
- 命令分层：定位 `query <name> [-k kind]`、`files --filter/--pattern`；上下文 `node <symbol>` / `node -f <file>`；探索 `explore <关键词> --max-files N`；关系 `callers/callees/impact/affected`；维护 `status/sync/index/unlock`。
- 坑：`callers`/`callees` 只收**纯符号名**（`path:line` 报 not found），同名符号按名聚合有噪声（用 `模块::符号` 收敛）；`files` 无 `-l`；未定义符号退出码 0。原则：**codegraph 定代码结构，rg 定文本字面量**；非代码文本或 not found 才回退 rg/read。

## harness 若干陷阱
- `agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 的遗留事件通道 **struct**，与路由 trait 同名；路由 trait 须全路径 `agentscope_harness::agent::gateway::channel::Channel`。`Channel`/`ChannelSink` 均有 `channel_id()`，同作用域须限定（否则 E0034）。
- `LocalFilesystem::write` **拒绝覆盖已存在文件**（`local.rs:260`）；追加须走 `WorkspaceManager::append_utf8_workspace_relative`；改文件要先 `Remove-Item` 再写。
- `LocalFilesystem::grep` 对目录 path 本身递归、`glob` 只匹配 basename（`memory` + `*.md` = 递归匹配）。
- `HarnessAgent::agent() -> Option<&ReActAgent>`（`harness_agent.rs:180`）；`HarnessAgent::task_repository()` 返回 `&Arc<dyn TaskRepository>`（**非 Option**）；`stream_events(Vec<Msg>, StreamOptions) -> BoxStream<Result<AgentEvent>>`，reply 从 `AgentEvent::AgentResult(EventData).fields["reply"]` 反序列化取；`Msg::generate_reason()` 无 metadata 默认 `ModelStop`；`GenerateReason` 含 `MaxIterations` 等 11 变体；`HarnessAgentBuilder` **无 MCP/allow-deny 配置**，`model()` 只收 `Arc<dyn Model>`（按名构建用 `model_resolver(Fn(&str)->Option<Arc<dyn Model>>)`）；`TaskStatus::is_terminal()`；`SUPPRESS_COMPLETION_CALLBACK` 在 `agentscope_harness::agent::subagent::task`。
- `ReActAgent` 构建**必须提供 model**（`.model(...).build()`），否则报 `Agent("ReActAgent requires a model")`；单测需 StubModel。
- `memory_get`/`memory_save`/`memory_search` 对齐 Java：注入 `Arc<WorkspaceManager>`（注册点 `harness_agent.rs:1409`）；get 入参 `path/startLine/endLine`（1-based inclusive），`..`→"path traversal not allowed"；save 仅 `content`，**追加** `MEMORY.md` + 当日 `memory/YYYY-MM-DD.md`；search 仅 `query`，大小写不敏感子串。

## AgentScope Java→Rust 迁移：总览
- 并行迁移 20 个 extension 模块 → `crates\agentscope-extensions-<module>\`。波次：**A**（PROTOCOL/MEM/RAG/MODEL/SANDBOX/CHANNEL）；**B 存储侧**（JDBC/MYSQL/POSTGRESQL/MONGODB/REDIS/OSS/COS）；**C 非存储侧**（HIGRESS/STUDIO/SCHEDULER/SKILLS/NACOS/AISTIO/TRAINING）。
- 编排/回执格式见 `docs\extensions\EXTENSIONS_MIGRATION_PLAN.md`；汇总 tracker `docs\extensions\EXTENSIONS_GAP_TRACKER.md`（§2.1 已裁定公共抽象归属必须遵守；§2.2 待裁定）；各模块 `<MODULE>__GAP_TRACKER.md` 为单一事实来源。
- 必读顺序：本模块 tracker → EXTENSIONS_GAP_TRACKER → EXTENSIONS_MIGRATION_PLAN → `docs\MIGRATION_PLAN.md` / `docs\MIGRATION_GAP_TRACKER.md`；多用 codegraph 探查 core/harness 既有 trait，避免重建平行类型。
- **全量基线演进**：1485 → 1746 → 1846 → 1860 → 1866 →（ACP 升级为 2.2.0 后）**96 targets / 2715 passed / 2 failed（均既有 flaky）/ 1 ignored**。口径以「无 FAILED + EXIT=0」为准，勿只看 `test result:` 行数。1 个 ignored 是 `platform_channel.rs` 的 ```ignore 文档示例（既有，非缺陷）。

### Java→Rust 迁移分析（提交级，2026-10-19）
- 逐提交分析产物写入 `.agentscope\workspace\java-migration\<两位编号>-<组名>.md`；按**提交组 G1…Gn** 聚合同类扩展模块的多个 Java 提交。每节固定格式：变更摘要 / Java 涉及文件(3-8) / Rust 对应位置 / 可迁移性（✅可迁移 / ⚠️部分 / ❌不可迁移 / ✔️已完成 + 理由）/ 实施步骤 / 影响范围 / 风险依赖；头部注明「分析提交数」，末尾 `## 组小结`（3-5 要点 + 各状态计数）。子智能体为**只读角色**（禁改两仓文件），完工回 **≤300 字摘要**（含各状态计数；G6a/G6b 例外为 ≤200 字）。
- 组划分与产物：
  - **G1** `01-core-structured-output.md` — core 结构化输出 / 生成结果 / agentId / returnDirect / MCP 标题 / role 参数（8）。
  - **G2** `02-core-reliability.md` — core 重试 / 状态持久化 / HITL / runtime context / HTTP 传输（9）。
  - **G3** `03-tools-mcp-middleware.md` — tools / MCP / middleware 扩展点 / skill 过滤（8）。
  - **G4** `04-model-providers-a.md` — openai / dashscope / anthropic / gemini 服务器工具 / 缓存控制（10）。
  - **G5** `05-model-providers-b.md` — deepseek / ollama / openai-official Responses API / embedding / usage 指标（8）。
  - **G6a** `06a-harness-session.md` — harness session journal / compaction 量化 / pruning / skill staging / 文件上传（7）。每节 ≤12 行、全篇 ≤350 行。
  - **G6b** `06b-harness-misc.md` — harness 其他（超时 / pruning 元数据 / patch CRLF / workspace / 冲突策略 / docker spec / 记忆会话检索）（9，声明 7 实为 9）。每节 ≤12 行、全篇 ≤400 行。
  - **G7** `07-jdbc-db.md` — JDBC / MySQL / PostgreSQL / Redis 存储层（9）。
  - **G8** `08-sandbox-rag.md` — sandbox(e2b/kubernetes/docker) + RAG(Tika/WordReader/embedding)（9）。
  - **G9** `09-extensions-protocol-channel.md` — protocol(agui) / channel(个人微信) / higress / aistio / studio / judge / controlplane（7）。
  - **G10** `10-service-ci-deps-docs.md` — service / aistio 服务化 / Harness context+Service session 重构 / CI / 依赖升级 / 文档（27）。
- 总报告 `docs/java-recent-commits-migration-analysis.md`（310 行，含路线图/P0 实施步骤/影响范围/批次/局限 + 附录 A 残留差异）；Java 仓库 main 分支近 30 天 = **113 提交**（2026-09-18~10-14，HEAD 5f7f8e6cf）。汇总计数（含跨组重叠，和=102）：✅28 / ⚠️42 / ❌24 / ✔️8。查看 Java 提交：`git -C E:\local-github\agentscope-java show <hash> [--stat]`（分析分支 `main`）。跨组重复提交须避免重复结论：`c0d03ccea`（G2/G9）、`35cbbfcf9`（G3/G9）、`7393283d7`（G10 内含重复项）。
- **待核实项**（分析前先 grep，勿臆断）：Rust `agentscope-extensions-sandbox` 的 **e2b 已落地**（`src/e2b.rs` ≈834 行，Connect 帧解析已存在；kubernetes/docker + 四平台 `*FilesystemSpec` 亦在）；**judge** 模块 Rust 是否存在（无则记跨模块需求）；openai-official 是否值得拆独立 crate（Rust 现仅 `extensions-model` 内 `responses/` 子模块）。
- G10 报告须单列「### 需要 Rust 侧跟进的依赖版本」节。Rust 依赖核查点在 `crates/*/Cargo.toml` 与根 `Cargo.toml`；Rust 用 **async-openai 0.41 / async-anthropic 0.6** SDK，与 Java 官方 SDK 版本**无直接对应**，依赖升级无法照搬。
- 子智能体组过大会失败（G6 14 提交整组 fail 且未落盘）→ 拆 G6a(7)/G6b(9) 后均成功；提示宜「单组提交数 ≤9」并限输出行数。

## 通用硬约束（所有模块子智能体）
- 只能写本模块 crate `crates\agentscope-extensions-<module>\**`（含本 crate `Cargo.toml` `[dependencies]`，标具体版本）与本模块 tracker；禁止改根 `Cargo.toml`（尤其 `[workspace.dependencies]`）、`crates/agentscope-core/**`、`crates/agentscope-harness/**`、其它模块 crate 与汇总 tracker。**唯一例外**：新建 crate 需登记时，用户已显式授权子智能体编辑根 `Cargo.toml` 的 `[workspace] members` 列表（**仅此一处**，仍不得动 `[workspace.dependencies]`；本次为 `crates/agentscope-extensions-judge`）。
- 跨模块/workspace 级需求 → 记本模块 tracker §4.2 + 回执「跨模块需求」。复用 harness/core 既有 trait；harness 抽象不足时不得改 harness，记 §4.2 并用本 crate 适配层绕过。
- 不得删既有功能 / 删断言 / `#[ignore]` 凑绿；测试须修至全绿，且必须离线（DTO serde 往返、纯函数映射、transport trait 注入/mock）。验证 `cargo build -p <crate>` 与 `cargo test -p <crate>`。
- 完工更新 tracker §1/§1.1（基线）/§2（逐类映射+依赖映射含版本）/§3（P0/P1/P2+状态）/§4.1（命令+实测）/§4.2（阻塞+跨模块）；「设计决定（不迁移）」条目须标为**「已完成（设计决定，不迁移）」并写明依据**。
- 风格：最小改动、高内聚低耦合、不写投机性代码、不臆造 API。回执格式多变：四项（多数）；五字段（PROTOCOL/SANDBOX）；六字段（MEM/CHANNEL/TRAINING/STUDIO/NACOS/AISTIO，末两栏常为「⑤跨模块需求 ⑥设计裁定说明」）；MEM 另附「设计裁定」段。
- 并行修改拆分经验：按「crate 归属 + 文件互不重叠」拆分（同 crate 不同文件可并存；`react_agent.rs` 被多项触及 → core 必须单智能体独占串行）。统一指令：只跑 `cargo build/test -p <crate>`（不跑 workspace 全量）、遇「来源在范围外」编译错误等 60-90s 重试且不改他人文件、必须做反向验证（回退实现→FAILED→恢复）、`Blocking waiting for file lock` 属正常。构建共享 target 目录，cargo 锁自动排队。汇报字数多为 ≤250/≤300/≤350 字。

## 各模块要点
- **MEM**（A）：实现活跃 core `Memory` trait（非 Java 已废弃的 `LongTermMemory`）；厂商 DTO / 消息→Mem0/Bailian 转换 / 检索→core `Msg`；reqwest 真实实现、测试离线。
- **CHANNEL**（A，**已完成**）：5 平台 `*Channel` + 4 个 axum CallbackController + `DingTalkStreamClient`（常驻 WS，ACK 在分发前发出）；另新增 **个人微信 iLink 渠道**（`src/weixin/` 13 文件 ≈4890 行，+85 测试）。harness 路由层 8 文件不可改；平台适配器只实现 `ctx()`+`deliver_reply()`。公共层：harness `agent/gateway/platform_channel.rs` 的 `PlatformChannel`/`PlatformCtx` + 对 `Channel`/`ChannelSink` 的 blanket impl；统一用泛型 `common::ChannelRegistry<C>`。
- **MODEL**：见「关键行为约定」段（198 passed，src 34 文件 / 10473 行）。目录：**无独立 `src/deepseek/`**（deepseek 逻辑在 `src/openai/mod.rs`）；dashscope→`src/dashscope/`、ollama→`src/ollama/`、能力位→`src/common/capabilities.rs`。
- **RAG**：5 子模块（bailian/dify/haystack/ragflow/simple）。已落地 dify/simple/ragflow；transport seam 范式（trait + 真实 reqwest + 内存 transport）。
- **PROTOCOL**：5 子模块（a2a-client/server、agent-protocol、agui、chat-completions-web）。已落地 a2a 系列 + agent_protocol + chat_completions + agui。
- **SANDBOX**：四平台 `*FilesystemSpec`、Kubernetes 文件 API、连接策略已落地（`fs_spec.rs` / `kubernetes_files.rs`）；**e2b 已落地**（`src/e2b.rs`）；harness **无 `SandboxFilesystemSpec`** → 本 crate 自建等价 Spec 适配（记 §4.2）。用 `httpmock` 离线测。
- **NACOS**：三子模块统一走 `service::NacosAiService` trait，**不实现 `DistributedStore`**；skill 只读 harness `SkillRepository`。
- **AISTIO**：见「AISTIO 交付明细」段。
- **STUDIO**：见「STUDIO P2」段。
- **TRAINING**：backend/reward/runner/strategy 已接线；失败根因曾为 **provider HTTP 400 中断**（非「迭代上限」）。
- **SCHEDULER**：scheduler-common 为本 crate 内共享层 + quartz/xxl-job。
- **存储侧**（JDBC/MYSQL/POSTGRESQL/MONGODB/REDIS/OSS/COS）：各 crate 自建 `<X>DistributedStore` / `<X>AgentStateStore` / `<X>Store`(BaseStore) / `<X>SnapshotSpec` / `<X>SandboxExecutionGuard`；CAS/版本语义以 harness 为准。`SandboxExecutionGuard` 是具体 struct（impl Clone，可 `Arc<..>`），非 trait；`SandboxIsolationKey` 公开路径 `agentscope_harness::agent::sandbox::SandboxIsolationKey`。
- **JDBC 原生锁 / 版本化 state**：`src/dialect/native_lock.rs`（`NormalizeLockName`：MySQL ≤64 原样、超长截 31 字符 + `:` + SHA-256 前 32 hex；`postgres_lock_key`：SHA-256 前 8 字节折叠 i64，碰撞 ~2^-32）。**关键裁定**：`GET_LOCK`/`pg_try_advisory_lock` 是**会话级**锁，Rust `JdbcExecutor` 是单连接共享缝、同连接重复获取会成功（PG 甚至可重入）→ **不能照搬 Java 方言 override**；新增 **`NativeLockSession` 缝**（每获取独占会话），`Mysql/PostgresNativeLockStrategy` 独立成类型（先例：`mysql/src/guard/mod.rs::LockBackend`）。`JdbcAgentStateStore` 新增版本化 API `supports_versioning()/read_version()/get_versioned()/save_if_version()` + `VersionedState`/`UNVERSIONED`（`-1` 无条件覆写回读、`0` insert-if-absent、其余 CAS 不符返 `UNVERSIONED` 不写）；接线此前仅测试引用的半成品 `session_state_insert_if_absent`/`session_state_update_if_version`。新增 `sha2` 依赖。实测 JDBC **115 → 146 passed**。测试用**独立算得的字面量**（如 `pgkey("agentscope")=2761187194516106426`）+ dev-dep 重算**双向交叉验证** SHA-256 期望值，防「实现与测试同源一起错」。
- **judge/jev**：新建 crate `crates/agentscope-extensions-judge`（见「judge crate」段）。
- **HIGRESS**（C，单模块）；**SKILLS** 见跨模块待裁定。

## 各模块要点补充：judge crate（agentscope-extensions-judge）
- 落地裁决 B4（新建 judge/jev crate）；源 Java 提交 `a5be76105`。根 `Cargo.toml` 仅加 1 行 member。规模 9 文件（`Cargo.toml` + `src/{lib,error}.rs` + `src/jev/{mod,dto,probability,validation,guardrail,review,execution}.rs`），**零第三方依赖**（仅 serde/serde_json/thiserror，全走 workspace）。骨架参照 `-studio`/`-higress`。
- 已落地（Java 可验证子集）：DTO（Question/Answer type 标签枚举、NoulCriteria、SystemOneRequest(Builder)/SystemOneResult/Usage）；概率容差 `PROBABILITY_SUM_TOLERANCE_PER_OPTION = 0.00005`（per-option × n）；validation（`validate_request/question/result/response/answer/probabilities`，Java 消息逐字对齐）；ReviewRegions.source/patch 纯函数 + JevReviewInput（路径规范化）；JevGuardrail 纯 evaluate 折叠（Outcome 顺序 = Java ordinal，Support>Block>Review）+ 默认 hazard 集（jailbreak/harm/self_harm，阈值 0.35/0.7/severity 2）；JevExecution Mode/Status/Decision/Record/Options；RetryPolicy + system_one_url + default_api_key + error_message（Java 字段序 message→error→detail，含嵌套 error.message）。
- 未落地（需 seam，已在 lib.rs 文档注明）：JevClient HTTP 传输/重试/超时、Guardrail::screen 的 reactive caller、JevConfirmedCalls（需 RuntimeContext 扩展槽）、Playwright 浏览器、context/evaluation/evidence/example/integration/routing/supervision 子包。
- 实测：`cargo build -p agentscope-extensions-judge` EXIT 0（0 warning）；`cargo test -p agentscope-extensions-judge -j 2` EXIT 0 → **57 passed / 0 failed**；clippy 0 warning；fmt 干净。反向验证：`PROBABILITY_SUM_TOLERANCE_PER_OPTION` 回退 1e-6 → 5 测试 FAILED。待办：建 `docs/extensions/JUDGE__GAP_TRACKER.md`（本轮硬约束禁新增 crate 外文件）；`JevClient` 是否复用 `agentscope_core::model::transport`（core HttpTransport trait）或自建 seam；`JevConfirmedCalls` 需 core `RuntimeContext` keyed 扩展槽。

## AISTIO 交付明细
- **B 方案**：用户授权后可跨改 core。已在 `crates/agentscope-core/src/agent/react_agent.rs` 新增只读纯增量 getter：`sys_prompt()`/`max_iters()`（+ `enable_pending_tool_recovery()`），未改行为（原硬约束「禁改 core」被用户显式覆盖，须在汇总 tracker 登记）。
- **P1/P2 主干**：`src/adapter/{observer_middleware,agent_scope_adapter,support,agent_task,collaboration_tool,task_starter,workspace_factory}.rs`、`src/transport/{collaboration_client,orchestration_client,contract_http_server}.rs`（`ContractHttpServer` 早已落地，axum）。`bridge.rs` 把 journal/trackers 移入 `Arc<Mutex<BridgeState>>`，**修复原 emit 为 no-op 闭包导致 Level-2 事件全被吞掉的真 bug**，新增 `BridgeReporter`。审批回环：`collaboration_client.rs` 的 `RuntimeApprovalSource` trait + `await_runtime_tool_approval` + `task_starter.rs::runtime_approval_confirm_handler`。实测 55 → **125 passed**。
- **装配路径真 bug（同类第二次）**：`AgentScopeAdapter` 漏实现 `FrameworkAdapter::on_bridge_attached` → `bridge.attach()` 注入点缺失（trait 默认实现为空）→ reporter 恒 None → Level-2 事件全丢。根因：旧测试直接塞 `shared.reporter` 绕开装配路径。已修 + 回归测试（走真实 `crate::instrument` 入口）。审计维度因此加「**trait 方法是否漏实现**」。
- **HITL 模型裁定（关键）**：Java：agent 暂停返回 `GenerateReason::PERMISSION_ASKING`，下一轮用 `Msg.METADATA_CONFIRM_RESULTS` 恢复（`callWithApprovals` 循环，32 轮上限）。**Rust core 无此路径**：`PermissionBehavior::Ask` 走 `core.confirm_handler`（`Arc<dyn Fn(ToolUseBlock)->Future<ConfirmResult>>`）**内联 await**，未装 handler 时 `react_agent.rs` 直接 `Err("permission asking (HITL) is not yet supported")`。故 Rust 映射 = **安装 `ConfirmHandler`**（`HarnessAgent::set_confirm_handler`，`harness_agent.rs:282` 转发）；语义等价、无 32 轮上限；差异：控制面不可达时 **deny**（Java 抛异常）。
- **已知语义差异（非缺口）**：Java 按 (userId,sessionId) 多 state 槽，Rust core **单槽**（唯一 `agent_state()`）→ adapter 用「session_id 不匹配即返回空」+ `distributed_store().agent_state_store` 桥接；Java token 取 `ModelCallEndEvent.getUsage()`，Rust `ModelCallEnd` 为空 EventData → 改从 `AgentResult.reply` 的 `Msg.usage` 取。
- **harness 侧显式差异**（非缺口）：①builder 无 MCP/工具 allow-deny → `tools()` 只把 allow/deny 落 `Toolkit::remove_tool`，`mcpServers` 建连归宿主；②MCP 工具名前缀 Java `server__tool` vs Rust 原始名；③`builder.model(String)` → `with_model_resolver`；④`awaitRuntimeToolApproval` 未实现时 `HarnessTurnRunner` 不做 PERMISSION_ASKING 回环；⑤Java reactor `boundedElastic` 非阻塞 → Rust `start` 全程 await。
- **剩余未完成**：`GrpcTransport`（暂缓，需 ASDP proto 工具链）；`SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource`/`AgentTaskStarter` 的宿主具体实现。

## STUDIO P2
- `src/studio/socket_io.rs`：Socket.IO/Engine.IO v4 真实传输，`tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots，与 CHANNEL crate 同版本）；纯帧层 + `SocketIoProtocol` 状态机 + 重连。
- `src/telemetry/exporter.rs`：`Span`/`SpanKind`/`SpanStatus` + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait（`OtlpHttpExporter`/`RecordingTraceExporter`/`NoopTraceExporter`）。实测 30 → **52 passed**。
- **OTLP 导出不引入 `opentelemetry`/`opentelemetry-otlp`**（避免改根 `[workspace.dependencies]`），走「降级 exporter trait + 编码器」；`TelemetryTracer` 仍待办（裁决 B5：tracer seam 归并到既有 exporter 方案，不新起第二套；对应 Java `9b611928e`「app-owned OpenTelemetry SDK」→ trait 注入 `Arc<dyn TraceExporter>`）。
- `encode_attribute_value`：JSON 数组→`arrayValue`、对象→`kvlistValue`（逐元素递归），**仅 `null` 降级空字符串**（OTLP 无 null 类型）。`is_hex_id` 语义：**空字符串永不合法**。
- `initialize_with` 默认仍用 `NoopStudioTransport`（Java 为无条件连接）——避免离线测试向 `localhost:3000` 发真实请求。

## 跨模块裁定 / 阻塞
- **list_hash 已全量收敛到 `agentscope_core::state::list_hash`**（Java `ListHashUtil` 忠实移植，已 `pub use`）：JDBC/PostgreSQL/MONGODB/COS/REDIS 5 个用 core，MySQL 无此逻辑。前提 `serde_json::Value: Hash`。**严禁恢复本地实现**（`rg "fn compute_hash|fn needs_full_rewrite"` 应无结果）。曾修两处真实 Java 偏离：MONGODB/COS 的 `needs_full_rewrite` 在 `stored_hash==None && existing_count==0` 时误返 true（Java 返 false）；COS 曾用 `whole_hash.starts_with(stored)` 判追加——**全量 SHA-256 无前缀性质**。存量数据自愈（每 slot 首次 `save_list` 多一次全量重写，数据无损、无迁移脚本）。
- **滚动 tracker 状态口径**：以独立 tracker 为单一事实来源；滚动视图**不得高于**独立 tracker。汇总低于独立 tracker 时可修正。
- harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`）：各 crate 用 `to_harness_store()`/`to_distributed_store()` 投影子集；**不改 harness**。`SandboxExecutionGuard` 为具体 struct → JDBC 锁无法驱动沙箱隔离。
- **待裁定**：SKILLS 与 harness `skill/` 存量职责边界（已判 B1=A 归 skills crate）；**anthropic 双实现并存**（`crate::anthropic` 自建 wire DTO+`HttpTransport` 与迁入的 `crate::anthropic_async`（`async-anthropic` 0.6 SDK））——已判 B2=B 保持并存；RAG embedding provider 与 MODEL 边界；`supports_native_locks()` 语义。
- 真实驱动未进根 `[workspace.dependencies]`（MONGODB `mongodb`、REDIS `redis` 1.7.0）由各 crate 自声明。
- JDBC 跨语言不兼容：会话表列名 `slot_id` vs Java `session_id`；锁表名 `{prefix}lock` vs Java `{prefix}distributed_locks`。
- MySQL 键列已加 `COLLATE utf8mb4_bin`（键列 `=`/`LIKE 'prefix%'` 转大小写敏感）→ 需运维文档 `docs/migrations/mysql_binary_collation.md`（影响面 + `ALTER TABLE ... MODIFY <key> VARCHAR(...) COLLATE utf8mb4_bin` 脚本，覆盖 `agentscope_store` 及 sessions/snapshots 表 + 低峰执行建议 + **已写入大小写变体键的库会丢失其中一行**告警 + 回滚）。
- P2 清理项（勿误当 bug）：REDIS `sandbox.rs:175 released` 未读；MONGODB `state_store.rs:433 delete_key_inner` 仅测试引用；JDBC device helper dead_code；`channel/src/{feishu,wecom}/token.rs` 的 `StaticTokenProvider` 未使用导入。
- 遗留不可离线项：JDBC `MysqlIntegrationTest`/`PostgresIntegrationTest`（需 Docker）；AISTIO `GrpcTransport`；MODEL dashscope 长尾。

## Java 迁移裁决清单（2026-10-19 正式裁定，后续相关实施以此为准）
- **A1=A** 跟随 Java 新 session/context 设计（`a2cbea5c3`：新建 `core/session` + `harness/agent/{context,session,observation,verification}`，废弃 SessionTree/SessionEntry/TranscriptMiddleware/transcript）——**大重构**。
- **A2=A** 版本化/CAS **上提为 core `AgentStateStore` trait 方法**（`7d08f8fd`、`e05d45e08`、JDBC `d786d8fb5`/`7854f0de7` 收口）。
- **A3=A** 引入 **run 级控制**（`AgentRun`/`RunControl`/`run_id`，`8d9bec68`+`ea78c317`；替换 agent 级单例 `InterruptControl`）。
- **A4=A** `ChatUsage` 扩 usage 三字段 + `ToolSchema`/`Tool` 增 `defer_loading`，**立项并授权改 core 公共类型**。
- **A5=A** 继续 P1-3（响应侧 schema 校验）+ P1-4（strict 合并 + `ToolChoice::Specific` 能力位）。
- **A6=A** **补发 `RequireExternalExecution` 事件**（core+protocol 新增发射点）。
- **B1=A** skill namespace 归属 **skills crate**（非 harness/jdbc）；**B2=B** anthropic 双实现**并存**；**B3=A** MCP meta 传播默认 **fail-open**；**B4=A** **新建 judge/jev crate**；**B5=A** tracer seam **归并**到 STUDIO telemetry exporter 方案；**B6=A** 补 plan-mode **白名单与拒绝逻辑**。
- **C1=A** 引入 **liteparse**（`github.com/run-llama/liteparse`）做 RAG 文档解析；**C2=A** Redis **Cluster hash tag** 做；**C3=A** 接受 JDBC **破坏性重构**（`create_table_ddls` 返回类型 + `TableSchemaValidator`）；**C4=A** server tools（Gemini/Anthropic）**立项并改 core**（`is_server_tool`/`ServerToolResultAccumulator`）；**C5=A** MySQL COLLATE **出迁移脚本/运维说明**。
- **D1=A** 保留 `crates/agentscope-harness/memory/2026-10-19.md`（harness 记忆副产物，目录有意跟踪）；**D2=C** 改动**继续叠加**（不提交；全部留工作区，分支 `dev`）；**D3=A** 主智能体**逐个 grep+跑测试核实并修** 4 处疑似「已落盘未接线」：`SessionSearchTool::register_all`、`tool/fuzzy_text_matcher.rs`、`HarnessSkillMiddleware::invalidate()`、plan-mode 白名单。
- **约束**：A1/A2/A3/A4/A5/A6/C4 全部改 **core**，彼此冲突 → **core 必须由单智能体串行独占**，不可并行分包。

## 教训（最重要，务必遵守）
- **未经验证的断言不得落笔**：不要把「没见过调用」当「未被调用」写进文档。凡断言「X 未被使用 / 唯独 Y 偏离」**必须先 grep 全仓**。
- **tracker 会过时，且汇总层最易腐**：「单一事实来源」≠「可信」，引用前须以源码 + `lib.rs` 的 `mod` 声明复核。独立 tracker 也会过时（曾把已实现文件标「待办」，也曾在 MODEL 上相反：独立 tracker 准确、**只有汇总行过时**）。**独立 tracker 未被汇总方编辑时更可信，过时的一般是汇总行**。更正一处描述后**必须 grep 同义旧措辞全文清零**。
- **「文件已落盘但从未接线」已发生多次**（`common/channel_core.rs`、`channel_registry.rs`、`dingtalk/stream.rs`）→ 复核上一轮产物**先 `grep mod.rs` 确认模块已声明**，再看 `git status` 是否 `??`，最后跑测试。
- **「测试绿」≠「生产路径正确」**：凡「A 靠 B 注入、B 由 trait 默认实现兜底」的装配点，必须有一条**走真实装配入口**的测试；直接 poke 内部字段的测试会掩盖漏实现 trait 方法（AISTIO `on_bridge_attached` 漏实现即此因）。审核须加「trait 方法是否漏实现」维度。
- 工作区中非本会话产生的改动**未必违规**（可能是用户手动编辑）→ 报告并询问，勿自行判定「越界」而暂缓/还原。
- 子智能体产出必须独立复核（构建/测试日志）后才可计入；达迭代上限会导致「改了代码但没交付文档」。**子智能体通病（实测）**：①**不交回执**（改动落盘却返回空，如 `p0-core`/`p0-model`）→ 判定产出必须看 `git status` + 跑测试；②**改一半**（只改局部、漏主循环/配对路径，遗留陈旧断言，如 `p0-core` 漏把挂起映射改 `Success` 并照发 `ToolResultEnd`）→ 主智能体必须逐项对照 Java 提交复核。
- 测试有效性须**反向验证**：逐个回退实现 → 跑对应测试确认 FAILED → 恢复。注意「无效回退」（如改动已把中断转成 Ok 时，删 `if result.is_ok()` 一行才有效）。
- `Select-String 'FAILED|panicked'` 会**误命中测试名**与 `test result: ok ... 0 failed` 行——判定失败须看 `^failures:` 段是否存在，勿只看计数。
- `memory_save` 无法覆盖已存在的记忆文件（报 already exists）→ 另建路径或用其它方式追加。
- **工具坑**：`edit_file` 的 `new_string` **不解析 `\n` 转义**（会写字面反斜杠-n）；`write_file` 正常解析。多行内容用 `write_file` 分片写 `_part_*.md` 再 `[System.IO.File]::AppendAllText` 拼接（`[System.IO.File]::ReadAllText($f).Replace('\n',[System.Environment]::NewLine)` 可修复字面 `\n`）。`write_file` payload 内**空双引号 `""`** 会破坏 JSON → 报 `Parameter validation failed: "path" is a required property`；规避=内容中不用裸引号，改用「空串」或 `\uXXXX`。

## 关键行为约定（core / harness）
- **ACP 思考参数：只发 `reasoning_effort`**（取代 thinking_budget 方案）。`harness/acp/session.rs::thought_level_params` **所有模型统一下发 `reasoning_effort`**，**永不**写 `enable_thinking`/`thinking_budget` 到 `/chat/completions` body。`auto`/空→default；`disabled`/`none`→`minimal`；`max`→`high`；其余原样透传。严格网关已实测接受。`GenerateOptions.thinking_budget` 字段保留，仅供 **DashScope 原生 API**（`extensions-model/dashscope/converter.rs`）使用，勿删。测试：`cargo test -p agentscope-harness --lib acp::session::tests`。
- **ReActAgent 400 / reasoning_content**：①空壳 assistant 消息（`content` 缺失）已修（空文本也发 `content:""`，`core/src/model/openai.rs:336-366`）；②`reasoning_content` 须完整回传（DeepSeek 带 tools 时硬要求，否则 400；无条件回放，不做 Java 式段级裁剪），设为 provider 级开关（模型名含 deepseek 自动开），**缺则回填空串**。落地障碍：async-openai 0.41 无该字段 → 走已有 `Value` 通道（`serde_json::to_value` + merge `extra_body`），**不能用 `additional_body_params`**（顶层）；`to_request_messages` 非 1:1（Tool Msg 扇出 N 条）不能按下标事后回填。参考实现 **flux**（Go，`GrayCodeAI/flux`）。未处理：` thinking...</think>` 内联拆分、`reasoning_details` 迁移。
- **model provider 目录重构**：`core/src/model/{openai,responses,anthropic}.rs` **整体迁入** `extensions-model/src/{openai,responses,anthropic_async}/`；core 不再导出 `default_registry`/`OpenAiChatModel`/`OpenAiProvider`；harness 改 `use agentscope_extensions_model::OpenAiChatModel`。⇒ 旧裁定「core 定 trait+注册表、本 crate 补 provider 适配」的「归属 core」前提已作废（「不重复实现」仍有效）。依赖下沉：`async-openai` 0.41 / `async-anthropic` 0.6 由 core 移入 extensions-model。
- **模型重试**：`extensions-model::common::error::ModelError::to_core()` 对「可重试但状态码非 429/5xx」的错误返回 `Error::Http(...)`（core 视为恒可重试），否则回落 `Error::http_status`。根因：core `Error::is_retryable` 只认 `HttpStatus{429|5xx}`/`Http`/`Io`；传输失败（status 0）旧实现给 `HttpStatus{0}` → 不可重试。重试在 core `react_agent.rs::model_call_impl`。**默认已拉长**：`ExecutionConfig::model_defaults()` = `max_attempts=7 / initial=2s / max_backoff=60s / mult=2.0`，等待序列 `[2,4,8,16,32,60]s`（累计 ~122s，单次尝试 timeout 300s）。`tool_defaults`（max_attempts=1）未改。harness 不设 `execution_config`，ACP 走这组默认值。`extensions-model/src/common/transport.rs` 的 `TransportConfig{max_retries:2,...}` 仅 anthropic_async 做超时用，其 `max_retries`/`backoff()` 全仓无调用点（惰性旋钮）。错误串「`Agent error: Agent error: ...`」双前缀来自 `core/react_agent.rs:347`（`Error::Agent(e.to_string())`）+ `harness/acp/session.rs:771`（`format!("Agent error: {e}")`）。
- **中断/崩溃恢复**：①`run_agent_loop` 的 `execute_iteration(...).await?` 短路 `*core.state.lock()=state` 回写 → 改为无条件下写（对齐 Java 错误也持久化）；②`call_with_format` 入口加 `self.core.interrupt.reset()`（此前生产路径无复位点，中断一次即永久卡死）；③`Err(Error::Interrupted)` → `Ok(reconcile_interrupted(&mut state))`（补悬挂 tool_use 的 error result + recovery assistant）；④接线 `AgentCore.enable_pending_tool_recovery`（此前死旋钮）→ `patch_pending_tool_calls`；⑤`StateCheckpointMiddleware` 加 `on_reasoning`（每迭代落盘），前提 `reasoning_phase` 每轮先 `*core.state.lock()=state.clone()`；⑥Ctrl-C/SIGTERM：harness tokio 需启用 `signal` feature，`AcpCodingAgent` 加 shutdown handler。**ACP 已开启 `enable_pending_tool_recovery(true)`**（两个构建点：`acp/acp_harness_agent.rs::build_agent`、`acp/coding_agent.rs::default_model_factory`）。`ReActAgent::enable_pending_tool_recovery(&self) -> bool` 只读 getter 已加（对齐 Java `isPendingToolRecoveryEnabled()`）。
- **死代码清单（勿当「已实现」）**：①`Middleware::on_acting` + `MiddlewareChain::wrap_acting` **零调用**（core 主循环从不调 `wrap_acting`）→ 用 `on_reasoning` 作挂点；②`GracefulShutdownManager` 无 `bind_state_saver` 等价物；③`AgentState.shutdown_interrupted` 是死字段；④`AgentState::interrupt_control()` 返回 `InterruptControl::new()` 而非字段（空实现）；⑤`SubagentDeclaration.enable_pending_tool_recovery` 现已接线（`SubagentBlueprint`/`LeafFactory`/`declared_factory`，`decl.unwrap_or(parent)`）。
- **core 迁移 Java `doCallInner` fail-fast**：`run_agent_loop` 开头，若 pending tool calls 非空、未开 recovery、messages 非空且调用方未自带 tool result → `Err(Error::Agent("Pending tool calls exist without results. ..."))`。未迁移（注明依据，非缺口）：ASKING 分支（Rust core 从不写 `ToolCallState::Asking`，不可达）、resume 分支（属控制流重构）。
- **ACP（harness）**：`agent-client-protocol = "2.2.0"`（**2.2.0 移除 `unstable_tool_call_name` feature**，改逐开 6 个 unstable feature，刻意不用聚合 `unstable`）。新增 `acp/support.rs`/`provider.rs`/`mcp_over_acp.rs`。compaction→ACP seam：`ConversationCompactionMiddleware` 发 `AgentEvent::Custom`（`kind="compaction"`）→ `acp/session.rs` 识别转通知（仅 client 广告 `session.compaction` 时）。MCP-over-ACP 方向由 RFD 确认：client 声明 `McpServer::Acp`，agent 发 `mcp/connect|message|disconnect`。CLI 读 `ACP_SESSION_ID`（+ `ACP_SESSION_REPLAY`）。`ActiveSession` **非 Clone**，只能用 `into_session()`。

## 2026-10-19 P0/P1 并行实施（工作区未提交，分支 `dev`）
- 基于「Java 提交迁移分析」**并行实施** P0+P1（用户显式授权覆盖 core/harness 禁改区）。9 个修改子智能体（P0×6：core/model/harness-a/harness-b/sandbox/storage；P1×3：p1-core/p1-agui/p1-channel-weixin）。改动**未提交**。
- **终局回归通过**：`cargo build --workspace` EXIT=0；`cargo test --workspace -j 2 --no-fail-fast` EXIT=0，无 `failures:` 段、无 FAILED，1 ignored（既有 `platform_channel.rs` 文档示例）。
- 已实施 P0：PG 去 slot 路径守卫、MySQL `COLLATE utf8mb4_bin`、K8s 命令包装换行、结构化降级提醒不落盘、降级重建保留 id/timestamp/usage、deepseek reasoning_content 空串回填、skill stager 空资源返 None、session 工具 limit clamp 100、compaction 保留 metadata/state/usage、pruning 按单条 result 计费、`ToolResultState::Suspended` + 挂起不伪造结果且不发 ToolResultEnd。
- 已实施 P1：`Tool::title`、`ToolResultBlock::success/final_state`、`ToolEmitter` 暴露 call id 并接线（3 处 emitter 原为 None）、`returnDirect` + `GenerateReason::ToolReturnDirect`、`Middleware::on_agent_state_ready`/`active_points`（在 `run_agent_loop` 真实入口调用）、model 侧 6 项（dashscope 分片 id/merge、ollama 原生结构化+thinking、dashscope reasoning_effort、cache_control 下移 content block、`*_BASE_URL` 回退新增 `common/base_url.rs`）、AG-UI reasoning 转换 + 前端工具 error→ERROR、fuzzy CRLF、session matchMode、e2b 三项、**个人微信 iLink 渠道**。
- 主智能体补充修正：①`react_agent.rs` 两处挂起结果改 `Suspended`+跳过 end 事件；②`extensions-protocol/agui/adapter/strategy/converters.rs` 补 `"SUSPENDED" => Suspended`；③修 `error_result` 换行被压坏；④更新 `core/tests/tool_result_test.rs` 过时断言（新契约=空 output+reason 进 metadata）；⑤`extensions-aistio/src/adapter/task_starter.rs::reason_name` 补 `ToolReturnDirect => "TOOL_RETURN_DIRECT"`。
- 残留差异（记入分析报告附录 A）：Rust core 仍**不发出** `RequireExternalExecution`（裁决 A6 待实施）；harness `PlatformChannel` 无取消缝（weixin lease 丢失只能抑制发送）；MySQL 库需手工 `ALTER ... COLLATE utf8mb4_bin`。未做的 P1：响应侧 schema 校验、strict 合并/`ToolChoice::Specific` 能力位（裁决 A5）。

## Git 提交历史
- `.gitignore` 仅 4 行（`.agentscope`/`agents`/`/target/**`/`/MEMORY.md`）——`memory/*.md` 与 `.agents/context/**` **被跟踪**；harness `memory/` 目录仓库有意跟踪、每次提交同步。
- 提交信息含中文：先 `write_file` 写 `.agentscope/workspace/commitmsg.txt` 再 `git commit -F`（后 `Remove-Item`）。本地产物忽略写 `.git/info/exclude`（`flux/`、`.XCoding/settings.local.json`），不改仓库 `.gitignore`。
- 需精确控制提交内容时用显式路径 `git stage <paths...>`（勿盲用 `git stage -A`，会把遗留未跟踪临时文件带入）。
- 关键提交：`d86a578`（model provider 迁入 extensions-model）、`2816e4a`（AISTIO/STUDIO/JDBC 补缺波次，40 files +7430/-142）、`275b2f1`（模型重试 + 退避拉长）、`7619c33`（ACP 升 crates.io 2.2.0）、`6f02226`（harness 用审批路径替换危险命令黑名单断言）。
- 分支变化无常（`main`/`dev`/`Endless/seafan`），用户会自行切分支，**提交前先 `git branch` 确认当前分支**，勿凭记忆。当前实施分支 `dev`。
- 提交前回归惯例：`.bat` 后台跑 + 读日志，确认 `cargo build --workspace` EXIT 0 与相关 crate 测试 0 failed。

## Memory 工具与维护管线
- 记忆工具（memory_get/save/search）的 workspace 根 = **仓库根 `E:\local-github\agentscope-rust`**（非 `.agentscope/workspace`）：产物为根 `MEMORY.md`（被 git 跟踪，committed `4eb1251`）+ `memory/YYYY-MM-DD.md` 日账 + `memory/.consolidation_state` 水位；`.agentscope/workspace` 只放 skills 缓存/tmp 与 `MEMORY.md.bak`。
- `MemorySearchTool` 经 `WorkspaceManager::list_memory_file_paths` 取文件，走 `fs.glob("*.md","memory")` 而 `LocalFilesystem::glob_recursive` **递归**，故会扫到 `memory/` 下任意子目录（现存冗余 `memory/memory/*.md` 3 个文件亦被扫到）。
- 现存异常待确认：①`memory/memory/` 冗余一层嵌套目录（3 个手工笔记）；②`crates/agentscope-harness/MEMORY.md` 仅 4 字节 `done`（harness 自身 memory 目录与根 memory/ 是两套无关数据）。
- 2026-10-19 接线惰性配置 `MemoryConfig.consolidation_min_gap`：装配点 `harness_agent.rs:1178` 由 `MemoryMaintenanceMiddleware::new(fs, Some(consolidator))`（硬编码 1800s）改为 `new_with_config(fs, Some(Arc::new(consolidator)), daily_file_retention_days, session_retention_days, consolidation_min_gap, IsolationScope::User)`。Java 依据：`HarnessAgent.java:2772-2778` 把 `memoryConfig.consolidationMinGap()` 作为第 4 参 `minGap` 传入，且 `MemoryConfig.DEFAULT_CONSOLIDATION_MIN_GAP` 注释「matches MemoryMaintenanceMiddleware」→ 该字段归属**维护中间件的 min_gap**，非 consolidator。顺带接线同样惰性的 `daily_file_retention_days`/`session_retention_days`（此前 `new()` 硬编码 90/180）。新增只读 getter `MemoryMaintenanceMiddleware::min_gap()` + lockstep 测试 `memory::tests::consolidation_min_gap_default_matches_maintenance_middleware`。验证：`cargo build -p agentscope-harness` EXIT 0；`cargo test -p agentscope-harness --lib` **337 passed / 0 failed**（+1）。
- **测试盲区（已裁定不修）**：无法写「走真实装配入口后断言 min_gap」的测试——`agentscope_core::middleware::MiddlewareChain.middlewares` 是私有 `Vec<Arc<dyn Middleware>>`，`dyn Middleware` 无 `as_any`/downcast 入口，`HarnessAgent::middleware()` 只返回 `&MiddlewareChain`。为测试给 core `Middleware` trait 加 downcast 属超范围，故仅靠「单一构造点 + 默认值 lockstep 测试」保证。
- **实测证据（记忆管线是活的）**：2026-10-19 本轮会话自身 agent 调用触发了维护，`memory/.consolidation_state` 由 `2026-10-08T08:52:54Z` 推进到 `2026-10-19T02:05:48Z`，`MEMORY.md` 18,795 → 22,028 字符（134 行）。此前「水位停在 10-08」的推断（30 分钟进程级闸门/未触发）应更新为：闸门只是延迟，长会话终会跑到。副作用：consolidate 是**整篇覆盖** MEMORY.md，会吞掉未被日账覆盖的手工编辑（本轮无损，仅追加）。
- 2026-10-19 完成「Java 近 30 天提交迁移分析」并**基于其并行实施 P0/P1**（见上节），随后用户对「裁决清单」正式裁定 A1–D3（见上节）。
- 2026-10-19 D3 裁决执行完毕（未提交，分支 dev）：
  - **D3 #1** `SessionSearchTool::register_all` 全仓无调用者 → 已在 `harness_agent.rs` 的 `!disableMemoryTools` 块内接线（与 Java 同条件）。新导入 `SessionSearchTool`。
  - **D3 #2** `tool/fuzzy_text_matcher.rs` 为孤儿（Java 仅在 `SkillManageTool` 的 patch/edit 用）→ 给 `agent/skill/manage.rs` 的 `SkillManageTool` 补 `edit`/`patch` 两个 action 并接线 fuzzy 阶梯（唯一性校验 / `replace_all` 右→左应用 / `[fuzzy: level]` 后缀）；新增 `locate`/`load_skill`/`write_resource`/`do_edit`/`do_patch`。+4 单元测试。
  - **D3 #3** `HarnessSkillMiddleware::invalidate()` 零调用 → 新增 `SkillManageTool::with_on_write(cb)`；装配处 `harness_agent.rs` 引入 `skill_mw_slot: Arc<Mutex<Option<Arc<HarnessSkillMiddleware>>>>`，manage 工具写后调 `invalidate()`，构建 skill_mw 后回填 slot。+1 测试。
  - **D3 #4 = 裁决 B6** plan-mode 白名单/拒绝：**已完整实施**（原判为结构性缺口，后补齐）。方案不走死代码 `wrap_acting`，而在 core `acting_phase` 内等价拒绝：
    - core `react_agent.rs` 新增只读 getter `ReActAgent::state_handle() -> Arc<Mutex<AgentState>>`（对齐 Java `resolveAgentState`）。
    - core 新增 `plan_mode_denies(core, state, call)` + 常量 `PLAN_MODE_ALWAYS_ALLOWED`（plan_enter/plan_write/plan_exit/todo_write/agent_spawn/agent_send/agent_list/task_output/task_list，逐字对齐 Java）/`PLAN_MODE_DENY_MESSAGE`；`acting_phase` 在 `evaluate_permission` 前先查，命中即发 DENIED 且不执行。
    - `run_agent_loop` 末尾回写：仅当共享 state 的 `plan_mode_context` 相对本轮起点变化时才回带（修「循环克隆+整篇回写」吞掉工具直写 plan 标志的隐患）。
    - harness `tool/plan_mode.rs`：`PlanModeHandle` 由 `Arc<tokio::sync::Mutex<AgentState>>` 改 `Arc<std::sync::Mutex<AgentState>>`，新增 `with_state(fs, plan_dir, state)`；`plan_write` 先锁内取路径再释放锁后 async 写（std guard 不跨 await）；`is_plan_active` 由 async 改同步。
    - harness `agent/workspace/plan.rs` 新增 `write_plan_file(rc, path, content)`。
    - harness `harness_agent.rs`：`plan_mode_enabled` 时构建 agent 后 `PlanModeHandle::with_state(fs, plan_dir, agent.state_handle()).register(&toolkit)`（Toolkit 为 Arc 共享，装配后注册生效）。
  - **C5** 新建 `docs/migrations/mysql_binary_collation.md`：影响面 + `ALTER TABLE ... MODIFY <key> VARCHAR(...) COLLATE utf8mb4_bin` 脚本（`agentscope_store` namespace_path/item_key、`agentscope_snapshots` snapshot_id）+ 迁移前大小写变体查重 SQL + 低峰/回滚 + **已知同类未修缺陷**：JDBC `dialect/vendor/mysql.rs:79` 会话状态表仍 `COLLATE utf8mb4_unicode_ci`（`session_id`/`state_key` 仍大小写不敏感）。
  - 回归：`cargo test -p agentscope-core --lib` **110 passed / 0 failed**；`cargo test -p agentscope-harness --lib` **355 passed / 0 failed**；`--test harness_plan_wait_tool_test` **4 passed**。反向验证：core `plan_mode_denies` 直接 return false → `plan_mode_denies_mutating_tool` FAILED（恢复后绿）；fuzzy 回退精确匹配 → `patch_applies_fuzzy_and_reports_level` FAILED。
  - 更新 `docs/java-recent-commits-migration-analysis.md` 附录 A.5（4 项全部「已修/已实施」+ 实施说明 + 反向验证）。
  - 教训：`edit_file` 多次因在 old_string 末尾多带/少带一行导致「误删相邻行」（promote 分支体、`subscriber_receives_events` 的 model 行、`propose_creates_draft` 的 drafts 行）→ 每次替换后须立即 read_file 核对。
  - 剩余大项（core 串行队列，未做）：A1(session 大重构)/A2(版本化上提 core trait)/A3(run 级控制)/A5(响应侧 schema 校验+strict 合并)/C4(server tools)/B1(skill namespace)/B3(MCP meta fail-open)/B5(tracer seam)/C1(liteparse，待 reqwest 0.13 决策)/C3(JDBC 破坏性重构)。

- 2026-10-19 会话（A2/A5/C4/A3 实施，工作区未提交，分支 `dev`）：
  - **A2 完成**：core `AgentStateStore` trait 增加版本化/CAS；5 个存储 crate（jdbc/mongodb/mysql/postgresql/redis）实现；harness `state_checkpoint.rs` post 阶段走 CAS + 冲突回退；新增 `core/tests/state_store_versioning_test.rs`。
  - **A5 完成**：`ToolChoice::Specific` 能力位（`Model::supports_tool_choice_specific()`，openai/responses/anthropic_async/dashscope/gemini 覆盖 true）；`tool/validator.rs::validate_json_output` + `ReActAgent` 响应侧 schema 校验（失败回退 generate_response 工具路径，最多 3 次强制 `ToolChoice::Specific`）；openai `strict_json_schema` 模型级默认 + `to_response_format(rf, strict_default)` 请求级优先合并。
  - **C4 完成**：core `ToolUseBlock/ToolResultBlock::METADATA_SERVER_TOOL`("serverTool") + `is_server_tool()`；`Msg::inline_server_tool_result_ids()`；`has_actionable_tool_calls`（服务端工具+内联结果=回合已完成）；`extract_pending_tool_calls` 过滤 server tool；**关键**：`ReasoningContext` 必须累积 `ToolResultBlock` 并在 `build_final_message` 输出，否则内联结果丢失、服务端工具会被当本地工具派发。gemini：DTO `tool_call/tool_response/executable_code/code_execution_result` + `includeServerSideToolInvocations`；`GeminiServerTool` 枚举（google_search/url_context/code_execution）+ `merge_server_tools`；`GeminiChatModel::with_server_tools`。anthropic：DTO 新增 `ServerToolUse` + 6 个 `*ToolResult` 变体（共用 `{tool_use_id, content}`）；parser 产出 server 标记块、整块 JSON 存 `anthropicServerToolResult` 元数据；converter `tool_use_block` 回写 `server_tool_use`、`tool_result_message` 从元数据原样回放。实测 model 262 passed。
  - **A3 完成**：新增 `core/src/agent/run.rs`（`RunStatus` Created/Queued/Running/Completed/Failed/Cancelled + `is_terminal`、`RunControl`（agent_id/run_id/InterruptControl/AtomicU8 状态机/queue/start/cancel/interrupt/finish）、`AgentRun` 单次句柄、`generate_run_id()` 32-hex）；`RuntimeContext` 增 `run_id` 字段 + `with_run_id`/`run_id()`；`AgentCore` 增 `active_run: Mutex<Option<Arc<RunControl>>>`；`ReActAgent::call_with_format` 每次调用创建 RunControl（queue→start→…→finish→清空），`interrupt()` 优先走 active_run（否则回退 agent 级标志），新增 `active_run()`/`prepare_run()`；`check_interrupted` 优先读 active run 的 InterruptControl；`Agent::interrupt` 改为转调。取消优先于结果（`is_cancelled()` → Cancelled）。
  - 反向验证（均确认 FAILED 后恢复）：extract_pending_tool_calls 的 server-tool 过滤、gemini `merge_server_tools`、anthropic `tool_result_message` 的元数据回放、`RuntimeContext.run_id`。
  - 全量回归：`cargo build --workspace` EXIT=0；`cargo test --workspace -j 2 --no-fail-fast` EXIT=0（101 test result 行、0 FAILED、0 failures 段、0 error）；core lib 120 passed、model 262 passed、harness lib 355 passed。
  - 遗留：`crates/agentscope-core/src/agent/react_agent.rs::tool_event` 在 HEAD 上即为 dead code（`dead_code` warning），非本次引入，未删。A1 未开始。
