# agentscope-extensions 迁移 — 汇总进度跟踪文件（Rollup Gap Tracker）

> **用途**：本文件是 extensions 迁移的**汇总视图**，由**汇总方（主会话）**维护。每个模块的**单一事实来源**是它自己的独立 tracker：`docs/extensions/<MODULE>__GAP_TRACKER.md`。本文件不替代它们，只汇总状态、跨模块依赖与待办。
> **方案**：`docs/extensions/EXTENSIONS_MIGRATION_PLAN.md`
> **上游**：`docs/MIGRATION_PLAN.md` + `docs/MIGRATION_GAP_TRACKER.md`（core/harness）
> **范围**：`agentscope-extensions` 下 20 个模块（`agentscope-spring-boot-starters` 不迁移）。
> **基线**：2026-09-18 创建；`cargo build --workspace` 通过（20 个空壳 crate 已注册 workspace member）。

## 0. 如何阅读本文件
- 子智能体**不直接改本文件**；完工后在回执中报告，由汇总方登记（避免并发写冲突）。
- 状态取值：`待办` / `进行中` / `部分完成` / `已完成` / `阻塞` / `设计决定（不迁移）`。

## 1. 总览

| # | 波次 | 模块 | Rust crate | Java 主/测行 | 状态 | 独立 tracker |
|---|---|---|---|---|---|---|
| 1 | A | MODEL | `agentscope-extensions-model` | 30058 / 54660 | 部分完成（**2026-10-22 静态复核**：42 文件 / 13833 行 / **266 + 5 测试**（内联 266 + 集成 5；**✅ 2026-10-22 新增 OpenAI Chat Completions 非流式路径**，`GenerateOptions.stream` 现已生效）；provider 主链路 6 个均已接线；**P0×2**：① **✅ 已修（2026-10-22，Phase 1，见 §4.8 #11）**：`openai-official` 子模块（14 类 / **3330 行**，原记 3716 有误）已迁 Phase 1——provider id `openai-official` 可解析 + 调用链 + 错误翻译；与 `openai-responses` 仍保持 id 分离；② `responses::map_openai_error` 未走 `ModelError` 结构化归一（丢 `type`/`code`，与 `openai/mod.rs` 不一致）（**✅ 已修（2026-10-22）**：改为与 Chat Completions 同款 `ModelError::from_parts` + `Reqwest` 超时/网络分类 + `StreamError` 归一，用例 `api_error_preserves_type_code_and_status`；MODEL **263 + 5 passed**）；**P1×5**：dashscope 加密零实现 / dashscope 与会话合并 / ollama 多智能体 / anthropic `AnthropicServerTool` / responses multi-agent 分组；§6 的 new-api 十项能力经复核**全部命中**） | `MODEL__GAP_TRACKER.md` |
| 2 | A | PROTOCOL | `agentscope-extensions-protocol` | 17910 / 21254 | 部分完成（**2026-10-22 静态复核下调**：38 文件 / 17248 行 / **302 test**；`agui` 整包 10259 行 1:1 属实、chat-completions-web 已落地；**P0×0**；**P1×7**：A2A 服务端引导层 `AgentScopeA2aServer`、`AgentExecuteProperties`（0 引用）、`AgentRegistry`/`AgentRegistryService`（0 引用）、`TransportWrapper*`/`DeploymentProperties`、**a2a `message/stream` 返回一次性数组而非 SSE 增量**、`WorkspaceProtocolTaskRepository`、`ProcessResult.interrupt(threadId)`；另修正：若干"已完成"项实为**私有函数**（`file_part_to_block`/`data_part_to_block`/`media_part`/`mime_type_from_url`/`StreamingChunkAccumulator`），非公开 API 对齐） | `PROTOCOL__GAP_TRACKER.md` |
| 3 | A | RAG | `agentscope-extensions-rag` | 15946 / 16792 | **进行中**（**2026-10-22 静态复核**：42 文件 / 10002 行 / **265 test**；分模块判定 **dify/ragflow/bailian/haystack = 已完成**（逐类逐键等价），**simple = 进行中**（40 类中 19 类未迁移）；**推翻两条"设计决定（不迁移）"**：① `ImageReader` 的 Java import **零第三方 JVM 依赖**（仅 `ImageBlock`/`URLSource`/core），可机械直译；② `ReaderUtils` 的「仅服务未迁移 reader」前提因 ①失效；simple 未迁移三块 ≈6100 行：外部向量库 4 件（ES 526 / Milvus 919 / PgVector 979 / Qdrant 996）、HTTP embedding 4 件、reader 增强；新增 P2：`EmbeddingUsage`/`EmbeddingResponse`/`usageListener` 缺失、`RAGMode`/`GenericRAGHook` 全仓 grep **0 命中**、`ExternalApiReader`（408 行，可基于 `crate::transport`）） | `RAG__GAP_TRACKER.md` |
| 4 | A | SANDBOX | `agentscope-extensions-sandbox` | 8832 / 4554 | **已完成（遗留 P1×3 未接线）**（**2026-10-22 静态复核**：12 文件 / 6721 行 / **137 test**；四后端均为真 reqwest HTTP 实现、非 trait 占位；**P0×0**；**P1×3**：① `KubernetesFileApi`+`FileEntry` 已实现但 `file_api_base_dir` **无消费者**（持久化仍走 base64-over-exec）；② E2B 未实现 `SandboxFileTransfer`、缺 `workspaceTransferPath` 校验；③ 连接策略**语义错位**——Rust gateway 分支实为 Java `InClusterStrategy`，真正的 `DirectStrategy`/`GatewayStrategy` 不存在，且 `X-Sandbox-*` 头未注入；另：`E2bCodec::Proto` 实为默认但 `exec` 直接 `Unsupported`；四类 `*FilesystemSpec` 均已存在（原记"待办"有误）） | `SANDBOX__GAP_TRACKER.md` |
| 5 | A | CHANNEL | `agentscope-extensions-channel` | 8251 / 3954 | **已完成**（2026-10-23）：G1–G8 全部关闭（G1 DingTalk HTTP 回调见 §4.8 #7；G2/G4/G5 与 G6/G7/G8 见模块 tracker §3 + 归档 `CHANNEL__MIGRATION_HISTORY.md`）。Rust **71** 文件 / **16334** 行（非空；原始 18000）/ **385 test**（266 `#[test]` + 119 `#[tokio::test]`）。保留 2026-10-22 复核的三处纠错结论：Java 有 **25 个测试文件 / 3954 行 / 162 个测试方法**（并非「0 行」）；`*Channel` 适配实为 **6 个**（含 `WeixinChannel`）；axum 控制器 **5 个**（含 DingTalk） | `CHANNEL__GAP_TRACKER.md`（历史：`CHANNEL__MIGRATION_HISTORY.md`） |
| 6 | A | MEM | `agentscope-extensions-mem` | 4821 / 5746 | 已完成（**2026-10-22 静态复核维持**：15 文件 / 2697 行 / **66 test**；**25/25 类真实对应**（mem0 9 + bailian 8 + reme 8），无缺失无空壳；**P0×0**；但**推翻原 §6.4 一条错误结论**：`Mem0Client`/`ReMeClient` **根本没有** `api_base_url` 必填校验（`new("")` 可成功构造）→ 升为 P1；另 P2：`Mem0Message` 多带 `name`（Java 只填 role/content）、`Mem0SearchRequest` 直接写 `pub user_id` 会**漏掉 `filters` 镜像**致静默丢过滤条件；测试比 66/326 ≈ 20%） | `MEM__GAP_TRACKER.md` |
| 7 | B | JDBC | `agentscope-extensions-jdbc` | 5265 / 4406 | 部分完成（**2026-10-22 静态复核，理由已变**：src 28 文件 / 3882 行 / 7 内联 + `tests/` 8 文件 / 2356 行 / 159 + skill 32 = **198 test**；版本化 API **已由 core 提供**（`core/src/state/store.rs:68/76/98`）且本 crate 已覆写 → **原阻塞解除**；但 **✅ P0×1 已修（2026-10-22，见 §4.8 #8）**：JDBC skill 仓库已迁移（`JdbcAgentSkillRepository` 1095 行 + `SkillDialect` 120 + `SkillResourcesDialect` 98 + builder 4 个 skill 开关），**占 Java 主代码 24.9%**，且 Java 旧 `Mysql/PostgresSkillRepository` 已 `@Deprecated`、官方替代正是它；**P1**：原生 advisory lock 未接线（`src/` 内 `impl NativeLockSession` = **0**，`JdbcExecutor` 单连接语义无法表达"每 lease 独占连接"）；4 个历史 bug 修复形态**全部可静态确认**） | `JDBC__GAP_TRACKER.md` |
| 8 | B | REDIS | `agentscope-extensions-redis` | 3167 / 1038 | 部分完成（**2026-10-22 静态复核 + 裁定 1 修复**：12 文件 / 4100 行 / **114 test**（52 `#[test]`+62 `#[tokio::test]`）；上一轮四项改动（SCAN 游标 / `connect_lazy` / `AtomicBool` / 三层替身+两条命令缝）**全部属实**；**✅ 已修（裁定 1）**：`save_if_version(0)` 由"无条件 bump"改为真实 CAS（create-if-absent），core `UNVERSIONED(-1)` 正确映射到内部 `UNCONDITIONAL_EXPECTED(u64::MAX)`，不再与 0 冲突 → 不再丢更新；**P1×2 仍存**：① SCAN 游标循环**零单测**（`connection.rs:174-195`，替身为全量 glob 过滤不模拟游标）；② Cluster 下 `SCAN` 只覆盖所连节点 → `list_session_ids`/`clear_all_sessions` **漏键**（Java `scanIteration` 遍历全 master）；另纠正：`clearAllSessions` 在统一 store **不存在**，仅两个 `@Deprecated` store 有 → Rust 侧属扩展方法非对齐） | `REDIS__GAP_TRACKER.md` |
| 9 | B | MONGODB | `agentscope-extensions-mongodb` | 2235 / 2429 | **进行中**（**2026-10-22 静态复核下调**：11 文件 / 4121 行 / **76 test** + 1 doc-test；四项改动属实，但 `database_name` **仅快照侧**生效；**P0×0**；**P1×3（均为"字段只写不读"）**：① builder `ttl_days` 在 `build()` 中**被丢弃**（`state_store.rs:641-651`）→ TTL 索引静默不建、会话永不过期；② guard builder `database_name` 被丢弃（`sandbox_guard.rs:348-357`）→ **跨库共用锁集合互相抢锁**；③ 级联删快照链路 harness 侧**无写入方** → 级联恒 no-op、快照成孤儿；另：§3.2「core trait 无版本方法」**已过时**（commit `9a8fd07` 已上提，本 crate 已实装）；P2：`MongodbGuardClient` 与 `SandboxLease::drop` 不接 → 锁只能等 TTL 30min 过期） | `MONGODB__GAP_TRACKER.md` |
| 10 | B | MYSQL | `agentscope-extensions-mysql` | 2106 / 358 | **进行中**（**2026-10-22 静态复核下调**：12 文件 / 2443 行 / **45 test**；B 波次 5 项契约齐全、`LockBackend` 注入缝为真、**P0×0**；**P1×8**，其中 3 项数据正确性：① 列表变更检测缺失（core `list_hash` 可直接复用，JDBC/PG 已接入）；② CAS 非原子（版本存 `key:_hash` 伴随行，先读后写）；③ **✅ 已修（2026-10-22）**：**空列表语义与 Java 相反**——Java `values.isEmpty()` 直接 `return`（`MysqlAgentStateStore.java:571-573`），Rust 无此守卫且 `list_len` 得旧长度后触发裁剪 → **清空原有数据**（已加早退守卫 + 用例 `save_list_empty_is_a_no_op`）；④ 表名无校验（DDL 拼接）；另纠正：H2 upsert「Rust 用 MERGE、Java 用 ON CONFLICT」**不成立**（两者均 `MERGE INTO`，Java 类 Javadoc 为过时注释）；默认锁超时 **30s vs Java 1800s**） | `MYSQL__GAP_TRACKER.md` |
| 11 | B | POSTGRESQL | `agentscope-extensions-postgresql` | 1589 / 1604 | **已完成（P0 骨架/组件接线 + 离线测试覆盖；余 P1×2 / P2×5）**（**2026-10-22 静态复核 + 裁定 1/6 修复**：12 文件 / 3722 行 + `murmur3.rs` / **111 test** / **148 个 Java `@Test`**；六处零测试区补齐**全部成立**、`MemoryClient` SQL 分派缺陷**确已修复**；**P0×0**；**✅ 已修（裁定 1）**：`ensure_version_column` `DEFAULT 0` → **`DEFAULT 1`**，`save_if_version(0)` 补 **0→1 CAS 回退**（对应 Java 回归用例 `saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow`）→ 老表行不再永久幻影冲突；**✅ 已修（裁定 6）**：advisory-lock key 改用 Commons Codec 兼容的 **MurmurHash3 x64_128 + seed 104729**（金标向量钉住）→ **混部部署互斥恢复**；**P1×2 仍存**：③ builder `create_if_not_exist` 默认值与 Java **相反**（Java `true` / Rust `false`）；④ 快照 `initSchema` 失败 Java 仅告警、Rust 上抛阻断；P2：53 个 Java 用例未覆盖、`executeInWriteTransaction` 事务原子性未迁移、advisory lock 单会话归属、死代码） | `POSTGRESQL__GAP_TRACKER.md` |
| 12 | B | COS | `agentscope-extensions-cos` | 882 / 760 | 已完成（**2026-10-22 静态复核维持**：9 文件 / 1517 行 / **31 test**（19 `#[test]`+12 `#[tokio::test]`）+ **2 个 `no_run` doctest（只编译不执行）**；5/5 契约齐备、`list_hash` 收敛与「追加恒全量重写」修复均属实；**P0×0**；**措辞更正**：① 原「31+2 = 33 passed」不成立（实执行 31）；② **真实 COS SDK 适配（含 `q-sign-algorithm` 签名）实际未开始**——全 crate `q-sign-algorithm|Hmac|Sha1|secretId` **零命中**，workspace 无 `sha1`/`hmac`；③ 「连续第二次追加仍判全量重写」为 **core `needs_full_rewrite` 特性，Java 同款，非本 crate 缺陷**；P1×3（①为 core、③为 harness）；P2：`serde`/`futures`/`chrono` 冗余依赖（`chrono` 命中是 "**synchronous**" 子串）、`CosError::Backend`/`NotFound` 死变体、根 `object_store` **全 workspace 零引用**且不在 `Cargo.lock`） | `COS__GAP_TRACKER.md` |
| 13 | B | OSS | `agentscope-extensions-oss` | 795 / 290 | **进行中**（**2026-10-22 静态复核下调**：`src/` 8 文件 / 1064 行 / **0 内联 test** + `tests/` 5 文件 / 446 行 / **41 test** + 1 doc-test；5/5 契约齐全、5 个 Java 类全映射；**✅ P0×1 已修（2026-10-22，裁定 8，见 §4.8 #10）**：**唯一实机后端是空壳 feature**——原 `aliyun = ["dep:aliyun-oss-rust-sdk"]` 已删除（`#[cfg]` 命中 0、crate 不可投产）；改为 `s3 = ["dep:rust-s3"]`，`S3OssClient` 覆盖 put/get/exists/delete/list_page（真 continuation token），`httpmock` 真实 HTTP 端到端 7 条；OSS **41（默认）/ 52（`--features s3`）+ 2 doc-test**；**P1**：缺 per-key `delete`（core trait 无、但 **MONGODB 已有先例** `mongodb/src/state_store.rs:438`，宜用 crate 固有方法）、分页零覆盖 + `InMemoryOssClient` **无分页能力**（无法构造多页场景）、错误传播弱化（`search` prefix 不匹配 Java 抛异常/Rust 静默错值）、`OssDistributedStore` 无法产出完整 `DistributedStore`、`lib.rs:19-20` **误导性文档**「开 feature 即得实机后端」；原「Java 7 处 `RuntimeException`」实为 **15 处**） | `OSS__GAP_TRACKER.md` |
| 14 | C | **CONTROLPLANE**（原 AISTIO） | `agentscope-extensions-controlplane`（**改名完成 2026-10-22**） | 9518 / 3017 | **进行中**（**2026-10-22 静态复核下调**：37 文件 / 10709 行 / **125 test**；**🔴 对齐基准已漂移**：Java 侧 `agentscope-extensions-aistio` 已在 commit `a2cbea5c3`（2026-10-08）**整体重命名为 `agentscope-extensions-controlplane`**（54 文件**全为 rename 无增删**；类名 `Aistio`→`ControlPlane`、`AistioConfig`→`ControlPlaneConfig`、`AistioObserverMiddleware`→`ControlPlaneObserverMiddleware`，**其余 36 类保留原名**；另含 4 项语义变更：native session-log 提交链路新增、`supportsAtomicSessionStorage()` 新增、env 变量改名、默认 journal 目录改名）；控制面 39 类 → **✅29 / ⚠️9 / ❌1**；**✅ 改名已执行（2026-10-22，裁定 3）**：crate / 类型 `ControlPlane*` / env `CONTROL_PLANE_*` / journal `~/.agentscope/service-controlplane/event-journal` / sink `controlplane-native-v1`；**✅ P0×1 已修**：native session-log 提交链路已接线（`ControlPlaneCommittedEventSink` + `publish_committed` + `sourceWatermark` 持久化），core/harness 仅加可选注入位、未配置时零行为变化；**P1×2**：EventJournal 缺按会话 ack（仅 `ack_all()`）/ `CollaborationClient` 缺 `replanRun`/`signalRun`/`uploadArtifact`/`downloadArtifact`） | `CONTROLPLANE__GAP_TRACKER.md` |
| 15 | C | STUDIO | `agentscope-extensions-studio` | 3738 / 1554 | **已完成（遗留 P0×1 / P1×2）**（**2026-10-22 静态复核**：18 文件 / 4578 行 / **59 test**；不引入 `opentelemetry`、Socket.IO 真实握手/入房/ping-pong/重连、OTLP 降级导出为真 reqwest POST，均核实为真；**P0×1**：`StudioMessageHook` 读 `fields["message"]`，而 core 写的是 **`fields["reply"]`**（`react_agent.rs:725`）→ `final_message()` 恒 `None`，**Studio 消息转发静默失效**（**✅ 已修（2026-10-22）**：改读 `reply` 并保留 `message` 回退，3 条单测钉住，STUDIO **62 passed**）；**P1×2**：`TelemetryTracer`（实为 `TelemetryTracing`）缺 `agentscope.function.name` span / 三个响应侧抽取器 + `StreamChatResponseAggregator` 未接入 / 缺 `callFormat` format span；Socket.IO 鉴权键 Rust=`runId` vs Java=`run_id`（真实环境可能握手被拒）；另修正：`telemetry.model.*` 实为 13 类、`reqwest` 实为 **0.13**、非 2xx 亦为 IOException **会重试**） | `STUDIO__GAP_TRACKER.md` |
| 16 | C | SKILLS | `agentscope-extensions-skills` | 3197 / 2813 | **进行中**（**2026-10-22 静态复核下调**：8 文件 / 2753 行 / **34 内联 test**（0 `#[tokio::test]`、0 doc-test）；骨架/3 后端仓储/harness 适配器均真实存在**无重复实现**、与 harness `skill/` 分层裁定仍成立；**但结构对齐 ≠ 语义对齐**：**21 项真实缺口（P0×1 / P1×10 / P2×10）**；**✅ P0×1 已修（2026-10-22，见 §4.8 #9）**：`JdbcAgentSkillRepository`（按裁定 2 落在 `agentscope-extensions-jdbc`，按 namespace 分区）已迁移，本 crate 的 `Mysql/PostgresSkillRepository` 保持兼容、不再扩展；**P1×10**：git 技能名取目录名 vs frontmatter `name`、description 取首个非空行（frontmatter 文件返回 `---`）、`ls-remote` 未按 `refs/heads/<branch>` 过滤（实测只查 `HEAD`）、远端不可达时静默跳过 pull、MySQL 未 `CREATE DATABASE`、`createIfNotExist=false` 无存在性校验、PG `metadata_json` 不校验 `DATA_TYPE`、适配器**未透出 `skill_dir`** → `load_skill_through_path` 对 git/SQL 不可用、测试 34 vs **192 `@Test`**、`get_source()` 不含库/表/schema；测试密度 **0.18 vs Java 1.25（≈1/7）**；**归因纠正**：「Git 依赖系统凭据故未测」**不成立**——Java 用 `file://`+JGit 纯离线测了 23 例，Rust 5 个 git 测试**均未使用 `file://` 真实仓**；根 `Cargo.toml` 的 `sqlx`/`mysql_async`/`tokio-postgres` **已登记**，真实待办是在本 crate 实现 `SqlDatabase`） | `SKILLS__GAP_TRACKER.md` |
| 17 | C | SCHEDULER | `agentscope-extensions-scheduler` | 3234 / 3178 | **进行中**（**2026-10-22 静态复核 + 第二轮补充复核**：14 文件 / 2416 行 / **37 test**；180 个 Java `@Test`；**P0×2 均已修（2026-10-22，见 §4.8 #2/#3）**：① `QuartzAgentScheduler::schedule*` **未调用 `with_factory(self.factory)`**——`task.rs:55` 恒 `NoopFactory`，`with_factory` 全 crate 3 处命中**全在 `#[cfg(test)]`**，`self.factory` 只写不读；三条执行路径（trigger / run_task / `job.rs` fire）全汇聚 `execute()` → **定时/手动/trait run 三者 100% 返回 `Err("No agent factory attached…")`**；② `schedule_with_input` 初始消息被丢弃（`runtimes` 仅 4 处且**无读取点**，`fire()` 走 `run_with(Vec::new())`）；**P1×12**：cron 缺 `L`/`W`/`LW`/`#`/年字段、**星期基准 `0-7` vs Quartz `1-7` 静默错位一天**（且 Rust 接受 `0` 而 Quartz 抛 `ParseException`）、**dom/dow 同时指定 Rust 按 OR 触发而 Quartz 直接拒绝（语义分叉）**、`zone_id` 死字段恒 UTC、`a/1` 退化、非法 cron 静默失败（Java 快速失败无副作用）、按秒轮询 `next_after` 生产零调用、`interrupt()` 不中止 in-flight、`get_status` 3 值 vs Java 6 值、xxl-job 缺 `XxlJobHelper`/`handleSuccess`/`XxlJobContext` 协议、`cancel` 返回 `false` vs Java 抛 `UnsupportedOperationException`；**可观测性根因**：`QuartzScheduleAgentTask` 不暴露 `execution_count()`（Java 集成测试正靠它断言）→ Rust 写不出等价端到端断言，这正是"P0 存在但测试全绿"的结构性原因） | `SCHEDULER__GAP_TRACKER.md` |
| 18 | C | TRAINING | `agentscope-extensions-training` | 2890 / 2924 | **进行中**（**2026-10-22 静态复核下调**：5 文件 / 2767 行 / **64 test**（57 `#[test]`+7 `#[tokio::test]`）；21 个 Java 类映射无虚假条目、规模声明可复现；**✅ P0×1 已闭环（2026-10-22，见 §4.8 #6）**：`TrinityModelAdapter` —— Java `TrinityModelAdapter.java:177` 在 builder 内**强制 `.stream(false)`**（`// Trinity doesn't support streaming, force disable`）。**定位更正**：`stream: Some(true)` 实际位于 `crates/agentscope-extensions-model/src/openai/mod.rs:381`（training crate **从无 `src/model/`**，`git log --all` 为空），本 crate 的真实问题是**委托任意 `Model` 时原样透传 options**。**✅ 已修**：② `msg_ids` 重复——adapter 对每个 `Ok(resp)` 都 `add_msg_id`（`backend.rs:296-312`），流式 delegate 会把同一 id 记 N 次；现改为**连续去重**，并向下传 `options.stream = Some(false)`（用例 `adapter_records_duplicate_chunk_ids_once` / `adapter_forces_stream_off`）；**⚠️ 未闭环**：① 协议层 `stream:true`——`OpenAiChatModel` 仅实现 SSE 路径、不读 `options.stream`，需先给 MODEL 补非流式请求/响应路径（见 MODEL 行），否则 `base_url` 模式仍会向 Trinity 发 `stream:true`；**P0-1 经核实为「非缺口」**：全仓 Java 检索 `sharegpt`/`toTrainingData`/`sft`/`rft` **0 命中**（50 处 `conversations` 全在 node_modules）——Java 侧**不存在**训练数据格式转换器，格式化由外部 Trinity-RFT（Python）承担 → 属新增需求须单独立项；**P1×4**：`AgentCloner` 丢 `sys_prompt`/`max_iters`（静默数据失真）、`start()` 无 `addSystemHook` 等价物、3 个 ExecutionConfig 字段未迁移、`enable_auto_commit` 语义不一致（Java 从不读该字段）；**推翻旧结论**：「`sysPrompt`/`maxIters` core 无 getter」**不成立**（`react_agent.rs:283/288` 已有）、「core 无 PreCallEvent」**部分不准确**（`middleware::Middleware::on_pre_call` 注释明写 Mirrors Java `PreCallEvent`，只缺 post-call）；P2：`shadow_pool_size`/`shadow_pool_capacity` **零消费点**、4 个零引用依赖；测试明细原记 strategy12/backend9/reward2/runner21 → 实为 **18/15/3/28**） | `TRAINING__GAP_TRACKER.md` |
| 19 | C | NACOS | `agentscope-extensions-nacos` | 2095 / 2452 | **进行中**（**2026-10-22 静态复核下调**：11 文件 / 2750 行 / **43 test**；三子域主体已迁移；**P0×0**；**推翻一条关键前提**：原「protocol crate 的 `a2a` **不可编译**故本 crate 自建平行类型」**已证伪**——`protocol/src/a2a/{card,client,message,model,server}.rs` 均存在且 `lib.rs` 已 `pub mod a2a`，`card.rs` 已有 `AgentCardResolver` trait、`model.rs` 已有 `AgentCard`/`AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`/`TransportProperties`；**唯一真缺位的是 `AgentRegistry`** → 本 crate 的 `A2aAgentCard` 等属**重复实现债务**，应改为复用；**P1**：`SecuritySchemeConverterUtil`（209 行）无对应实现（Rust 仅 opaque JSON 透传，未按 `type` 分派 apiKey/http/oauth2/openIdConnect/mutualTLS）、`supportedInterfaces` 列表↔三元组组装/拆分缺失 + `capabilities.extendedAgentCard`/`stateTransitionHistory=false` 强制语义缺失、**prompt 推送回调语义相反**（Java 对**空串模板照写**、只丢 `null`；Rust 把空串也丢弃且未判 key 为空）、skill 资源 key 语义/必填字段校验/zip 结构校验三处不一致；P2：`do_overwrite` 缺 `removeIf(equals)` 去重 + `add(0, ..)` 前插、endpoint 无 `Set` 去重） | `NACOS__GAP_TRACKER.md` |
| 20 | C | HIGRESS | `agentscope-extensions-higress` | 1050 / 1016 | **已完成（遗留 P0×1 / P1×4）**（**2026-10-22 静态复核维持**：4 文件 / 1353 行 / **30 test**；4/4 主类均有真实对应、`x_higress_tool_search` 协议契约 1:1、`search_tools` 编译阻塞缺陷确已修复；**✅ P0×1 已修（2026-10-22，见 §4.8 #4）**：**`build()` 不建连**——`client.rs:401-409` 的 `initialize()` **仅把 `initialized` 置 true，不做任何连接动作**，`build()` 因此返回"自称已初始化"的 wrapper，`list_tools()` 必报 `No connected MCP client`；`transport_type`/`timeout`/`initialization_timeout` **三个配置项全部无效**；**P1×4**：`call_tool` 无 meta 形参（Java 三参重载）、`convert_to_mcp_tool` 丢 `outputSchema`、`HigressToolkit` 只能接 Higress 类型且无 `enableTools`/`group`/`prefix`/`preset`/`propagateMeta`、wrapper 硬依赖具体 `McpClientManager` 无 trait 抽象 → tool-search 成功路径/缓存/`topK` 请求体**均不可测**；另：Java 74 `@Test` vs Rust 30；WASM/插件契约 Java 侧不存在，无需对齐） | `HIGRESS__GAP_TRACKER.md` |
| 21 | — | **JUDGE**（**本轮新增登记**） | `agentscope-extensions-judge` | 12469 / 4035 | 进行中（**此前完全未在 rollup 与 `EXTENSIONS_MIGRATION_PLAN.md` 登记**；**已裁定纳入迁移范围**（§4.5 裁定 4），**tracker 已建**：`JUDGE__GAP_TRACKER.md`；Java 76 类（jev 68 + openai-decisions 8）/ **140 个 `@Test`**；Rust 9 文件 / 3181 行 / **57 test**，覆盖按行 ≈ **26%**；**P0×2**：HTTP 客户端缺失（`JevClient`/`OpenAIDecisionsClient`，crate 无任何网络能力）、System One DTO 待逐字段核对；**P1×8**：`openai-decisions`（795 行）整块缺失 + jev 的 `application`/`context`/`evaluation`/`evidence`/`routing`/`supervision`/`integration` 零实现；P2：`browser`（Playwright，建议「设计决定（不迁移）」）、`example`（不迁移）） | `JUDGE__GAP_TRACKER.md`（**已创建**） |
| 22 | — | **CONTROLPLANE**（**改名已落地**） | `agentscope-extensions-controlplane` | 9518 / 3017 | **✅ 已并入第 14 行**（2026-10-22）：改名、类型前缀、env、journal 目录、sink 名均已对齐；后续进度统一跟踪于第 14 行，本行不再单独维护 | `CONTROLPLANE__GAP_TRACKER.md` |

## 2. 跨模块依赖与汇总方待办

### 2.1 已裁定的公共抽象归属（**B 波次 7 个存储模块必须遵守**）

**裁定：不新建公共 `agentscope-extensions-store` crate。** 复用 core/harness 已有 trait（对齐 Java `harness.agent.DistributedStore` 的组合方式）：

| Java 组件 | Rust 既有 trait（**实现它，不重建**） | 位置 |
|---|---|---|
| `core.state.AgentStateStore` | `AgentStateStore`（async_trait；`save/save_list/get/get_list/exists/delete/list_session_ids`） | `agentscope_core::state::store` |
| `BaseStore`（远程 KV） | `BaseStore`（`StoreItem{key,value,version}`；`get/put/put_if_version/search/delete`） | `agentscope_harness::filesystem::remote::store` |
| `SandboxSnapshotSpec` | `SandboxSnapshotSpec`（`build(&self, snapshot_id) -> Result<Arc<dyn SandboxSnapshot>>`） | `agentscope_harness::agent::sandbox::snapshot` |
| `SandboxExecutionGuard` | `SandboxExecutionGuard`（`try_enter/release/is_held`） | `agentscope_harness::agent::sandbox::guard` |
| `MessageBus` | `MessageBus`（async_trait；Mode A/C/D 原语 + 领域 helper） | `agentscope_harness::agent::bus::message_bus` |
| `AsyncToolRegistry` | `AsyncToolRegistry` | `agentscope_harness::agent::bus::async_tool` |
| `TaskRepository` | `TaskRepository` | `agentscope_harness::agent::subagent::task::repository` |
| `DistributedStore`（组合包） | `agentscope_harness::agent::distributed_store::DistributedStore`（**当前仅 `agent_state_store` + `isolation_scope`，字段不足 → 见 §2.2**） | harness |

各存储模块的**统一产出契约**：本 crate 内提供 `XxxDistributedStore`（组合者）、`XxxAgentStateStore`、`XxxStore`（`BaseStore` 实现）、`XxxSnapshotSpec`、`XxxSandboxExecutionGuard`。

**避免重复发明**：`put_if_version` 的 CAS 语义、`StoreItem.version` 含义、快照 id 校验等一律以 harness 定义为准，不得自定义平行类型。

### 2.2 汇总方待办

| 项 | 说明 | 状态 |
|---|---|---|
| `DistributedStore` 字段不足 | **已裁定：不改 harness**。harness `DistributedStore` 保持 `agent_state_store` + `isolation_scope` 两字段不动，避免 7 个存储模块阻塞于一次共享编辑。

各存储 crate 提供自己的组合者 `XxxDistributedStore`，在原两字段基础上追加 `base_store: Option<Arc<dyn BaseStore>>` / `sandbox_snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>` / `sandbox_execution_guard: Option<Arc<SandboxExecutionGuard>>`（用 `Arc` 包裹，`BaseStore`/`SandboxSnapshotSpec` 本为 `Send+Sync` trait 对象）。各 crate 的 `XxxDistributedStore` 自建 builder，不依赖 harness builder。

**若后续发现需要 harness 内置该组合**：作为独立后续任务重估，不由本轮 14 个子智能体处理。 | 已裁定 |
| 根 `Cargo.toml` workspace 依赖 | **已执行**（见 §3.3）。统一加入 `sqlx`/`mysql_async`/`tokio-postgres`/`redis`/`mongodb`/`object_store`/`axum`/`http`/`bytes`；`protocol`/`aistio`/`channel` 已改引 workspace。 | 已完成 |
| Sandbox 后端抽象 | 已裁定：SANDBOX 模块实现 harness `Sandbox`/`SandboxClient` trait | 已裁定 |
| Channel 抽象 | 已裁定：harness `gateway/channel` 为路由骨架；**新增公共层 `gateway/platform_channel`**（`PlatformChannel`/`PlatformCtx`）承载各平台相同的分发/投递流程，各模块只写不通用部分 | **已裁定并已落地** |
| Skills repository | SKILLS 模块与 harness `skill/` 存量代码的关系 | **已裁定**：分层与 Java 一致（Java 接口在 core、实现在 extensions-skills；Rust 接口在 harness、实现在本 crate），属**归属位移非层次错位**。见 §3.3 |
| MODEL 模块与 core `model/openai.rs`（已用 async-openai） | 已下发约束：core 定义 trait + 注册表，本 crate 补 provider 适配，不重复实现 openai。<br>**2026-09-21 更新（`d86a578`）**：该约束的「core 持有 openai 实现」前提已不成立——core 的 `model/{openai,responses,anthropic}.rs` **实现体已整体迁入** `agentscope-extensions-model`（`src/openai/mod.rs` / `src/responses/mod.rs` / `src/anthropic_async/mod.rs`），core 现仅保留 `Model` trait / `ModelRegistry` / provider SPI。原意（**不重复实现**）仍有效，但「归属 core」部分作废。详见 `MODEL__GAP_TRACKER.md` §2.3 与 §4.2 第 7 条。 | 已裁定（2026-09-21 部分被取代） |

## 3. 汇总登记区（执行后由汇总方填写）

### 3.1 子智能体回执

### 3.1.2 子智能体回执（第七批：CHANNEL P1 主体攻坚）

**本批为汇总方亲自执行 + 4 个后台子智能体并行**（每子智能体只改自己平台目录）：

| 模块 | 任务 | 处置/产出 | 状态 |
|---|---|---|---|
| HARNESS | 公共层落地（汇总方） | 新增 `agent/gateway/platform_channel.rs`：`PlatformChannel` trait + `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl；后续顺手移除冗余包装类型 `PlatformChannelSink` | ✅ 6 test 通过 |
| CHANNEL(common) | 接线从未编译的 `channel_core.rs`/`channel_registry.rs`（汇总方） | `ChannelCore` 由自存 routing/gateway **收敛为组合 harness `PlatformCtx`**，新增 `admit()`/`admitted()`；`ChannelRegistry<C>` 接线并修 `Channel`/`ChannelSink` 同名方法歧义 | ✅ |
| CHANNEL(feishu) | 参考样板（汇总方） | `channel.rs`（`handle_callback` 全流程）+ `controller.rs`（axum 薄壳）；修复测试载荷缺 mapper 必需字段 | ✅ 173 test（crate 级） |
| CHANNEL(dingtalk) | 平台适配（子智能体） | `channel.rs`：`DingTalkChannel` + `handle_stream_payload` | ✅ dingtalk:: 28 test |
| CHANNEL(wecom) | 平台适配 + controller（子智能体） | `channel.rs`（`verify_url`/`handle_callback`）+ `controller.rs`（GET URL 验证 + POST 事件） | ✅ wecom:: 45 test |
| CHANNEL(github) | 平台适配 + controller（子智能体） | `channel.rs`（HMAC 校验 + 防自回环 `is_self`）+ `controller.rs` | ✅ github:: 32 test |
| CHANNEL(gitlab) | 平台适配 + controller（子智能体） | `channel.rs`（`X-Gitlab-Token` 校验 + 防自回环）+ `controller.rs` | ✅ gitlab:: 35 test |
| CHANNEL(dingtalk-stream) | `DingTalkStreamClient` 常驻 WebSocket 网关（汇总方接线） | `stream.rs`（注册 → 连接 → ACK → 分发 → 指数退避重连；I/O 走 `HttpTransport` + `WebSocketConnector`/`StreamSocket` seam；生产连接器 `TungsteniteConnector`）；`mod.rs` 声明接线 + 清理未用导入 | ✅ crate 227 → **241 test** |

**本批修复的真实缺陷（2 处，均在汇总方收尾阶段）**：
1. **`agentscope_harness::agent::Channel` 名称陷阱**：`gateway/mod.rs` 里保留的**遗留事件通道 struct** `Channel` 与路由 trait `Channel` 同名，`agent::Channel` 路径解析到 struct 而非 trait（`E0404 expected trait, found struct`）。修法：路由 trait 一律用全路径 `agentscope_harness::agent::gateway::channel::Channel` 导入（已写入 CHANNEL tracker §1.2 注意事项）。
2. **`Channel`/`ChannelSink` 的 `channel_id()` 歧义（E0034）**：新增 `ChannelSink` blanket impl 后，两 trait 同名方法同时在作用域内会使 `c.channel_id()` 无法推导。修法：在 `channel_registry.rs` 与 harness 测试中限定为 `Channel::channel_id(&…​)`。

> 另外两次失败经查为**环境/既有问题、非代码缺陷**：①全量测试在默认并行度下偶发 3 个 `training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`，属既有测试间竞争；单独跑 3/3 全绿）；②`os error 1455/112`（页面文件不足导致 rlib mmap 失败）会产生「could not compile」假阳性，降并行度（`-j 2`）可规避。

### 3.1.4 子智能体回执（第十批：七模块缺口攻坚）

**本批目标**：针对七个模块的未完成部分并行攻坚，并把「裁定不迁移」的条目统一标记为「已完成（设计决定，不迁移）」。

| 模块 | 任务 | 处置/产出 | 状态（经汇总方独立复跑） |
|---|---|---|---|
| MODEL | 迁移 `anthropic`/`gemini` | 新增 `src/anthropic/`（7 文件）+ `src/gemini/`（7 文件），实现 core `Model`/`ModelProvider`，复用 `HttpTransport` seam；零新增依赖 | ✅ **169 passed / 0 failed**（基线 58） |
| RAG | 迁移 `bailian`/`haystack` | 新增 `src/bailian/`（9 文件，含**自建 ACS4-HMAC-SHA256 签名**，金标向量钉住）+ `src/haystack/`（7 文件）；本 crate 新增 `hmac`/`sha2`/`uuid` 依赖 | ✅ **265 passed / 0 failed**（基线 152） |
| JDBC | 补齐测试（Java 98 `@Test`） | 新增 6 个 `tests/*.rs`（115 用例）；补测暴露并修复 **4 个真 bug**（session 表名递归自调、`search` 前缀漏项、`save_list` 批处理模板混跑致数据丢失、锁表 DDL 与 INSERT 列不匹配） | ✅ **115 passed / 0 failed**（基线 lib 2；P0 测试缺口已关闭） |
| POSTGRESQL | 补齐测试（Java 130+ `@Test`） | `state`/`store`/`snapshot`/`guard`/`distributed` 零测试区补齐 + `sql` 扩展；修复 `MemoryClient` SQL 分派缺陷（只匹配未加引号表名） | ✅ **104 passed / 0 failed**（基线 7） |
| MONGODB | GridFS 分块 / legacy 回退 | GridFS 255KB 分块 + chunk 完整性校验 + legacy 单文档回退、`database_name` 生效、`delete_key` 接线（消除 dead code） | ✅ **76 + 1 doc-test passed / 0 failed**（基线 61） |
| REDIS | `base_store()` 接线 / `find_keys` SCAN | `SCAN` 游标语义、`base_store()` 自动接线（保留 URL + `connect_lazy`）、`released` 接线为 `AtomicBool`；新增内存替身 + `RedisCommands`/`StoreCommands` 缝 | ✅ **92 + 2 doc-test passed / 0 failed**（基线 21） |
| PROTOCOL | `agui` 整包（converter/adapter/encoder/processor/registry/runtime） | 首个子智能体**中途网络错误中断**，已落盘 ~8800 行；汇总方修复 5 个编译错误 + 3 个失败用例（11→8 failed），再派收尾任务清零剩余 8 个 | ⏳ 收尾中 |

**本批教训（已写入 tracker）**：
1. **子智能体因网络错误中断后会留下「可编译/不可编译的半成品」**：PROTOCOL 首个子智能体落盘 8800 行但从未跑过编译（5 个编译错误 + 11 个失败用例），且 **tracker 完全未回填**。收尾必须先 `cargo build`，再逐用例判定实现 bug 还是断言错。
2. **两个收尾 agent 连续网络失败**：PROTOCOL 收尾首次派发亦失败（0 改动落盘）；REDIS 收尾成功。说明**长任务应尽早落盘并分段验证**。
3. **补测是发现真 bug 的高效手段**：JDBC 一轮 113 个新用例暴露 4 个真 bug（含 `save_list` **数据丢失**与锁表 DDL 不匹配导致的**锁永远插不进**）；POSTGRESQL 暴露 `MemoryClient` 对大半个模块**实际不可用**（既有 7 用例全在 `sql.rs` 故从未触发）。
4. **「裁定不迁移」= 已完成**：各 tracker §3.4/§3.3 的设计决定项（Spring AutoConfiguration、已弃用的 Jedis/Redisson store、需真实 API key 的 e2e、依赖 JVM 库的 Tika/PDFBox/POI reader 等）已按用户指令统一标记为「已完成（设计决定，不迁移）」并写明依据。

### 3.1.3 子智能体回执（第五批：全量复测与收尾）

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| AISTIO | ①`Cargo.toml` 重复 `tokio` key；②误删 6 个依赖（`futures`/`tracing`/`chrono`/`reqwest`/`sha2`/`uuid`）；③axum 0.7 `fallback` 拒绝同步零参 `not_found`；④测试 `E0596` | 恢复 6 个依赖；`not_found` 改 `async fn`；测试 `let server` → `let mut server` | ✅ 65 test 通过 |
| PROTOCOL | `agui` 整包未接线（`lib.rs` 缺 `pub mod agui;`、缺 `model.rs`）；`event.rs` 泛型约束不足 | 新增 `agui/model.rs`+`agui/mod.rs`（忠实映射 Java `agui/model`）、`lib.rs` 声明；`parse_json` bound 改 `DeserializeOwned` | ✅ 130 test 通过 |
| RAG | float 精度导致测试失败 | `serde_json` 启用 `float_roundtrip`；`ragflow` 落地 | ✅ 152 test 通过 |
| SANDBOX | `fs_spec` 未落地 | 补齐 | ✅ 108 test 通过 |
| CHANNEL | WeCom 签名 `Clone` 约束；base64 `decode_allow_trailing_bits` 语义 | 修 `wecom/signature.rs`、`wecom/crypto.rs`（对齐 Java `Base64.getDecoder()`） | ✅ 149 test 通过 |
| MODEL / STUDIO / HIGRESS | — | 复测绿 | ✅ 58 / 30 / 30 |

- **第九批汇总验收（文档同步批，2026-09-19）**：`cargo test --workspace -j 2 --no-fail-fast` → **1866 passed / 0 failed**（86 test target；唯一 `1 ignored` 为 `platform_channel.rs` 的 ` ```ignore ` 文档示例，早于基线即存在）。本批**仅改文档**：逐模块核对并修正独立 tracker 中过时/错误结论（MODEL `dashscope` 已接线非阻塞、RAG `ragflow` 已落地、SANDBOX `fs_spec`/`kubernetes_files` 已落地），汇总 §1 SANDBOX 行由「部分完成」校正为「已完成」。
- **第七批汇总验收**（保留备查）：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace -j 2` → **1846 passed / 0 failed**（较第六批 1746 增加 100：harness 299 +6，channel 227 +78，其余持平）。
- **第六批汇总验收**（保留备查）：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace` → **1746 passed / 0 failed**（86 test target，TEST_EXIT=0，见 §3.2）。
- **第五批汇总验收**（保留备查）：`cargo test --workspace` → **1728 passed / 0 failed**（86 target）。

> 波次：A=MODEL/PROTOCOL/RAG/SANDBOX/CHANNEL/MEM；B=7 个存储模块；C=AISTIO/STUDIO/SKILLS/SCHEDULER/TRAINING/NACOS/HIGRESS。

**第一批（20 个模块迁移）**：全部 20 个模块均产出了实质性代码（`crates/agentscope-extensions-*`）。首轮逐 crate 构建结果：**16 PASS / 4 FAIL**（FAIL：`jdbc`、`redis`、`mongodb`、`higress`）。

**第二批（修复轮）**

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| REDIS | 9 个编译错误：`state::adapter`/`state::connection` 未 `mod` 声明；harness `sandbox::key` 私有 | 补 `mod` 声明接线既有源码（未造空模块）；`key` 改用 harness 公开 re-export；修正 `lrange` 泛型签名 | ✅ 21+2 test 通过 |
| MONGODB | 测试编译失败（私有模块）+ 2 个测试逻辑失败（upsert 重复 `_id`、list_hash 前缀语义）+ `E0499` 双可变借用 | 改用公开 re-export；按 Java 语义修 `find_one_and_update`/`needs_full_rewrite`；`client.rs` GridFS 路径限定 `files` 借用作用域 | ✅ 62+1 test 通过 |
| MONGODB / COS / REDIS（list_hash 收敛，2026-09-19） | 三 crate 各持一份平行 list_hash 实现（MONGODB/COS 为全量 SHA-256，REDIS 为采样重复实现）；其中 MONGODB 与 COS 的 `needs_full_rewrite` 在 `stored_hash == None && existing_count == 0` 时返回 `true`，**偏离 Java**（Java 返回 `false`）；COS 另用 `!whole_hash.starts_with(stored)` 判追加，因全量哈希无前缀性质而**恒判需全量重写** | 统一删除本地实现、委托 `agentscope_core::state::list_hash`；修正与 Java 相反的测试断言；COS 解耦 `existing_count`（原仅在 hash 存在时读取，致 core Case 2 不可达）；COS 移除已无用途的 `sha2` 依赖 | ✅ 三 crate 全绿（MONGODB 61+1、COS 31+2、REDIS 21+2），workspace **1866 passed / 0 failed** |
| JDBC | 5 个编译错误：`super::dialect` 路径、`OptionalExtension` 误用、`SandboxExecutionGuard` 误当 trait、`hash_list` 缺失 | 修正导入路径；`JdbcSandboxExecutionGuard` 改固有方法；手写 `Debug`/`Clone`；复用 core `list_hash` | ✅ 2 test 通过 |
| HIGRESS | `search_tools(&query, top_k)` 调用与单参定义不符 | 依据 Java `HigressMcpClientWrapper` 两参重载，改调 `search_tools_with_top_k`（保留 `tool_search_top_k` 生效） | ✅ build + test 通过 |
| SCHEDULER | lib test 编译失败：`ScheduleMode` 未导入、`Msg::get_text_content` 不存在（3 处） | 补 `use crate::common::config::ScheduleMode`；改 `text_content()`；另修 cron `a/step` 解析真实 bug | ✅ 37 test 通过 |
| SKILLS | 6 个测试失败：建表/表名解析、`metadata_json_column_supported`、resource 落库、绝对路径校验 | 均判为**实现 bug**：扩展 `parse_table`/`execute` INSERT、探测前置取第 2 参数、POSIX 绝对路径校验 | ✅ 33 test 通过 |

**第三批（续作与接线轮）**

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| STUDIO | 13 个 src 文件从未编译（`lib.rs` 仅 1 行注释、无 `mod` 声明）；E0252/E0277 等接线错误 | 接线 `pub mod studio/telemetry` + re-export + crate 级文档；修重名导入与 trait 约束 | ✅ 30 test 通过 |
| TRAINING | 子智能体达迭代上限；tracker 未回填 | 收尾并补 `TrainingContext::mark_with_metadata(...)` | ✅ 64 test 通过 |
| MEM | 子智能体达迭代上限，但磁盘实际已修复 | 复核后确认可编译 | ✅ 66 test 通过 |
| AISTIO | `store::task_repository` 2 个测试失败（本地任务完成状态持久化顺序） | 新增 `persist_status_detached(after, …)` 等；按持久化顺序修实现 | ✅ 55 test 通过 |
| PROTOCOL | 1 个测试失败：`media_part_mime_falls_back_to_primary_type_for_unknown_extension` | 判为**测试断言写错**（与本测试名 falls back to primary type 及 Java `FilePartParser#getPrimaryType` 语义矛盾）：改为断言可还原为 `ContentBlock::Audio` 且 URL 源不变 | ✅ 79 test 通过 |
| CHANNEL | 子智能体产出仅 11 文件 / 1054 行且严重损坏（`lib.rs` 声明不存在的子模块、缺 `quick_xml`）；修复子智能体再次达迭代上限，最终 114 passed / 4 failed | 主会话直接处置：补全六平台适配层（38 文件），并修 3 处真实缺陷 —— ①`common/xml.rs` 未处理 `quick-xml` 的 `Event::GeneralRef`，实体被静默丢弃（影响 WeCom inbound）；②feishu `send` 测试未播种 `code:0` 响应；③幂等库测试断言与 Java [sweep 先于 putIfAbsent] 语义冲突 | ✅ 118 test 通过 |

### 3.1.1 子智能体回执（第四批：tracker 回填）

| 模块 | 任务 | 状态 |
|---|---|---|
| CHANNEL | 回填独立 tracker §1–§4（Java 39 类逐文件映射、P0/P1/P2 缺口、3 处缺陷根因、跨模块需求） | ✅ 主会话完成 |
| MODEL / RAG / SANDBOX / POSTGRESQL | 并行回填独立 tracker（4 个后台子智能体） | 进行中 |

### 3.2 汇总验收

- **基线**：`cargo build --workspace` ✅（仅 warning，无 error）。
- **验收（第八批，当前）**：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace -j 2 --no-fail-fast` → **1860 passed / 0 failed**（86 个 test target，EXIT=0）。较第七批 1846 增加 14：仅 `channel` 149→227→**241**（+14 为 `dingtalk/stream.rs` 的常驻 WebSocket 客户端单测），其余 crate 持平。CHANNEL 模块至此 **P0+P1+P2 全部落地**。
- **验收（第七批）**：`cargo build --workspace` → **BUILD_EXIT=0**（无 error）；`cargo test --workspace -j 2` → **1846 passed / 0 failed**（TEST_EXIT=0）。较第六批 1746 增加 100：`harness` 291→**299**（+6 `platform_channel` 单测）、`channel` 149→**227**（+78：5 个 `*Channel` + 4 个 controller），其余 crate 持平。
- **验收（第六批，保留备查）**：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace` → **1746 passed / 0 failed**（86 个 test target）。较上轮基线 1485 增加 261。逐 crate 明细（第六批实测日志）：`core` 73、`harness` 291、`aistio` 65、`channel` 149、`cos` 24、`higress` 30、`jdbc` 2、`mem` 66、`model` 58、`mongodb` 62、`mysql` 43、`nacos` 43、`postgresql` 7、`protocol` 130、`rag` 152、`redis` 21、`sandbox` 108、`scheduler` 37、`skills` 33、`studio` 30、`training` 64（另含 1 个 mongodb doc-test）。
- **上轮基线**（保留备查）：`cargo test --workspace` → 1485 passed / 0 failed；第五批 1728 passed / 0 failed。
- **约束遵守**：除汇总方已裁定的两处例外（根 `Cargo.toml` `[workspace.dependencies]`、harness 公共层 `gateway/platform_channel.rs`），各模块仅在本 crate 内改动 + 各自 tracker。
- **构建注意事项（非代码缺陷）**：
  1. **磁盘/页面文件**：曾因 E: 盘耗尽（`os error 112`）报假阳性；当前项目盘已为 `D:`（实测剩余 407GB）。此外 `os error 1455`（页面文件太小，rlib mmap 失败）会在高并行度下报假 `could not compile`，降并行度（`cargo test --workspace -j 2`）可规避。
  2. **既有测试间竞争**：`cargo test --workspace` 在默认并行度下偶发 3 个 `agentscope-extensions-training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`）；**与 CHANNEL/harness 本轮改动无关**，单跑 `-p agentscope-extensions-training` 为 3/3 全绿（64 passed）。第八批以 `-j 2` 复跑未复现该失败。
  3. **PowerShell 管道**：直接管道接 `cargo test --workspace` 全量输出会触发 PowerShell 栈溢出，建议先 `Out-File` 再分析。
- **本批 harness 注意点**：`agentscope_harness::agent::Channel` 是遗留事件 struct（与路由 trait 同名），路由 trait 需全路径导入 `agent::gateway::channel::Channel`。

### 3.3 汇总方待办（跨模块，未决）

| 项 | 说明 | 状态 |
|---|---|---|
| 根 `Cargo.toml` workspace 依赖 | **已执行**。统一加入 `[workspace.dependencies]`：`sqlx`/`mysql_async`/`tokio-postgres`（MYSQL/POSTGRESQL/SKILLS）、`redis`（REDIS）、`mongodb`（MONGODB）、`object_store`（COS/OSS）、`axum`/`http`/`bytes`（HTTP 统一）。已验证 `cargo metadata` 与 `cargo build --workspace` 通过；`protocol`/`aistio`/`channel` 均改引 workspace。 | 已完成 |
| harness `DistributedStore` 组合 | 已裁定不改 harness；各存储 crate 自建 `XxxDistributedStore`（见 §2.2）。若后续要在 harness 内置该组合，另立任务。 | 已裁定 |
| harness `SandboxExecutionGuard` 为具体 struct | **已裁定（依 Java 源码）：确认 harness 存在缺口，维持「不改 harness」直到另立任务**。Java 侧 `io.agentscope.harness.agent.sandbox.SandboxExecutionGuard` 是 `@FunctionalInterface` **接口**（唯一方法 `SandboxLease tryEnter(SandboxIsolationKey) throws InterruptedException`，语义为**阻塞直到获得槽位**，并提供 `noop()`）；Rust `harness/src/agent/sandbox/guard.rs` 却是**具体 struct**（`leases: HashMap<key,usize>`），其 `try_enter` **只做可重入计数、不阻塞也不跨进程**。故 JDBC/Redis/Mongo 等的分布式锁**无法驱动沙箱隔离**（各 crate 只能自带 guard 类型，形成平行类型）。修复需 harness 把该 struct 改为 trait + 默认 `NoopGuard`（同 Java），属 harness 公共层变更。**影响面（实测）**：aistio/cos/jdbc/mongodb/mysql/oss/postgresql/redis 共 8 个 crate 均已有各自的 guard 类型。 | **已裁定**：缺口确认；改 harness 列为独立任务（本批不动） |
| Skills↔harness `skill/` 关系 | **已裁定：维持原结论，分层与 Java 一致**。Java 源码核实（2026-09-19）：`agentscope-extensions-skills` **只含 repository 实现**（`GitSkillRepository` / `MysqlSkillRepository` / `PostgresSkillRepository`），模块根 `src/` 下**无任何 `.java`**；接口 `AgentSkillRepository` 位于 **core**（`io.agentscope.core.skill.repository`，由 core 的 `ReActAgent` / `DynamicSkillMiddleware` 消费）⇒ Java 分层 =【core：接口+模型】【extensions-skills：后端实现】。Rust 侧分工一致，唯一差异是接口**落在 harness 而非 core**（因 `agentscope-core` 无 `skill` 模块），属**归属位移而非层次错位**。残留观察：若将来 core 的 agent 也需挂载 skill 仓库，需重议是否上提 trait 到 core。 | **已裁定**（见 `SKILLS__GAP_TRACKER.md` §2.0） |
| `list_hash` 算法并存 | **已裁定并已落地（依 Java 源码 + Rust 实测）：以 core 为正解；MONGODB / COS / REDIS 三处平行实现已于 2026-09-19 全部收敛到 `agentscope_core::state::list_hash`**。Java `io.agentscope.core.state.ListHashUtil` 采用**采样**策略：`"size:N;"` + 采样位 `"i:itemHashCode,"`，末了 `Integer.toHexString(sb.hashCode())`；常量 `EMPTY_HASH="empty:0"`、`SAMPLING_THRESHOLD=5`、采样位 `[0,n/4,n/2,3n/4,n-1]`；`needsFullRewrite` 也是拿 `existingCount` 前缀哈希与存储哈希比。`agentscope-core/src/state/list_hash.rs` **忠实移植**了该策略，且**已经在 `state/mod.rs` `pub use`**。**Rust 实测分布（2026-09-19）**：① **JDBC** `state/jdbc_agent_state_store.rs` 与 ② **PostgreSQL** `state.rs` **已调用 core**（`agentscope_core::state::list_hash`，共 2 个 crate 落地）；③ **REDIS** `state/list_hash.rs` 策略忠实（采样，仅 per-element 输入改为 JSON 文本），属**重复实现但语义一致**；④ **MONGODB** 与 ⑤ **COS** 均为全量 SHA-256；⑥ MySQL 未涉及该逻辑。**两处真实行为偏离（已修）**：(a) ④MONGODB 与 ⑤COS 的 `needs_full_rewrite` 在 `stored_hash == None && existing_count == 0` 时返 `true`，**与 Java 相反**——Java 经 Case 3 得空前缀 → `computeHash(空)="empty:0"` → `hasChanged(·, null) == false` ⇒ 返 **false**（MONGODB 的测试断言本身即错，已按 Java 语义更正）；(b) ⑤COS 另用 `!whole_hash.starts_with(stored)` 判追加，**实测全量 SHA-256 无前缀性质**（`hash([a,b,c])` 不以 `hash([a,b])` 开头），致**纯追加恒判需全量重写**（增量优化失效；数据无损）。**收敛结果**：三 crate 弃本地实现 → 委托 core（MONGODB 保留 `list_hash.rs` 作薄 re-export 以不动公开路径；REDIS 删除 `state/list_hash.rs`；COS 删除两个 private 函数并移除已无用的 `sha2` 依赖，另解耦 `existing_count` 使 core Case 2 可达）。存量数据自愈：hash 值不可比 → 首次写入走全量重写，数据无损、无需迁移脚本。 | **已裁定并已落地**（见 `MONGODB__GAP_TRACKER.md` §3.3、`COS__GAP_TRACKER.md` §3.1、`REDIS__GAP_TRACKER.md` §4.1.1） |
| 各模块 tracker §2/§3 回填 | **全部完成**。MODEL / RAG / SANDBOX / POSTGRESQL / CHANNEL 已由主会话与后台子智能体回填（`docs/extensions/` 下已无空模板，各 tracker 8.9K–31K）；CHANNEL 本轮再次回填（§1.2 公共层归属、§2.6 逐类映射、§3.2 P1 状态、§4.1-§4.3 验收与遗留）。MEM / STUDIO / NACOS / TRAINING / AISTIO / PROTOCOL / SKILLS / SCHEDULER / HIGRESS / 7 个存储模块均已回填。 | 已完成 |
| HTTP 框架选型（CHANNEL 依赖） | **已裁定：统一用 `axum`**（与 `agentscope-extensions-protocol`、`agentscope-extensions-aistio` 一致）。4 个 `*CallbackController`（feishu/github/gitlab/wecom）已以 `axum::Router` 承载，路由/解析为薄壳，签名校验、解密、inbound 映射复用已落地纯逻辑。 | 已裁定并**已落地** |
| harness `agent::Channel` 契约是否足够 | **已裁定：契约足够，公共层写 harness，不通用部分写各模块**。已在 harness 新增 `agent::gateway::platform_channel`（`PlatformChannel` trait + `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl，6 个单测）；五个平台 `*Channel` 只实现 `ctx()` + `deliver_reply()`。 | 已裁定并**已落地** |
| **core 改动登记：AISTIO P1 所需 ReActAgent getter** | **2026-10-08，经用户明确授权（原「禁改 core」硬约束被本轮覆盖）**。为使 `AgentScopeAdapter` 能读 base system prompt / 迭代上限，在 `crates/agentscope-core/src/agent/react_agent.rs` **仅新增两个只读 getter**：`ReActAgent::sys_prompt() -> &str`、`ReActAgent::max_iters() -> i32`（对应 Java `getSysPrompt()`/`getMaxIters()`）。**纯增量、未改任何行为/架构、无调用方变更**。验证：`cargo test -p agentscope-core` 全绿；`cargo build --workspace` = 0。 | **已执行（2026-10-08）**；详见 `AISTIO__GAP_TRACKER.md` §4.1.1/§4.2 |

## 4. 2026-10-22 全模块静态复核（20 模块并行，每模块一智能体）

### 4.1 复核方式与口径

- **强度**：**纯静态**（逐类打开 Java 源码 + 逐符号核对 Rust 实现），**未编译、未跑测试**。故本文所有 Rust 测试数为 `#[test]`/`#[tokio::test]` **属性计数**，不等于 `cargo test` 执行结果。
- **行数口径**：统一为**非空行**（与 rollup 历史口径一致）。Java 侧同时保留物理行备查（如 MODEL 物理 33819/63416 vs 非空 30058/54660）。
- **状态取值**：严格四值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）`；`部分完成` 仅用于 rollup 汇总层。
- **幂等**：各模块 tracker 末尾均只追加**一份** `静态复核记录（2026-10-22）` 节，无重复。

### 4.2 结论总览（12 下调 / 7 保持 / 1 上调 / 2 新增登记）

| 模块 | 原状态 | 复核后 | 幅度 |
|---|---|---|---|
| MODEL | 部分完成 | 部分完成 | 保持（新增 P0×2） |
| PROTOCOL | 已完成 | **部分完成** | ↓ |
| RAG | 部分完成 | **进行中** | ↓ |
| SANDBOX | 已完成 | 已完成（P1×3 未接线） | 保持（限定） |
| CHANNEL | 已完成 | **进行中** | ↓ |
| MEM | 已完成 | 已完成 | **保持（唯一全绿）** |
| JDBC | 部分完成 | 部分完成 | 保持（理由已变） |
| REDIS | 已完成 | **部分完成** | ↓ |
| MONGODB | 已完成 | **进行中** | ↓ |
| MYSQL | 已完成 | **进行中** | ↓ |
| POSTGRESQL | 部分完成 | **已完成（P1×4）** | **↑** |
| COS | 已完成 | 已完成 | 保持 |
| OSS | 已完成 | **进行中** | ↓ |
| AISTIO | 部分完成 | **进行中** | ↓ |
| STUDIO | 已完成 | 已完成（P0×1） | 保持（限定） |
| SKILLS | 已完成 | **进行中** | ↓ |
| SCHEDULER | 已完成 | **进行中** | ↓ |
| TRAINING | 已完成 | **进行中** | ↓ |
| NACOS | 已完成 | **进行中** | ↓ |
| HIGRESS | 已完成 | 已完成（P0×1） | 保持（限定） |

> **唯一真正无 P0 且无功能缺口的模块是 MEM**；COS/OSS/HIGRESS/STUDIO/SANDBOX 的"保持"均附带 P0 或未接线 P1。

### 4.3 本轮发现的 13 个 P0（按严重度排序）

| # | 模块 | P0 | 证据位置 | 后果 |
|---|---|---|---|---|
| 1 | MYSQL | **空列表语义与 Java 相反** | `state/mod.rs:269-290` vs `MysqlAgentStateStore.java:571-573` | `save_list([])` 会**清空**原有数据（Java 直接 return） |
| 2 | SCHEDULER | `schedule*` 未 `with_factory` | `quartz/mod.rs:119-120`、`task.rs:55` | 定时/手动/trait run **100% 失败** |
| 3 | SCHEDULER | `schedule_with_input` 丢初始消息 | `runtimes` 无读取点 | 带输入的任务丢上下文 |
| 4 | HIGRESS | `build()` 不建连 | `client.rs:401-409`（仅翻 bool） | `list_tools()` 必失败；3 个配置项全部无效 |
| 5 | STUDIO | 键名 `message` vs `reply` | `message_hook.rs:93` vs `react_agent.rs:725` | Studio 消息转发**恒不触发** |
| 6 | TRAINING | `stream:true` 违反 Trinity 协议 | `model/openai/mod.rs:381` vs `TrinityModelAdapter.java:177` | 协议相反 + `msg_ids` 重复 N 次 |
| 7 | CHANNEL | DingTalk HTTP 回调模式整块缺失 | 无 `crypto.rs`/`controller.rs` | Java 已发布能力（228 行测试）不可用 |
| 8 | JDBC | skill 仓库整体未迁移 | `JdbcAgentSkillRepository` 1095 行（在 jdbc crate） | 占主代码 **24.9%** |
| 9 | SKILLS | 同上（同一 P0，两模块独立发现） | 同上 | 只对齐了已 `@Deprecated` 的旧实现 |
| 10 | OSS | 实机后端是空壳 feature | `aliyun` feature 已声明，`cfg` **命中 0** | crate 不可投产 |
| 11 | MODEL | `openai-official` 完全漏记 | 14 类 / 3716 行；provider id 不可顶替 | 按该 provider 注册直接失败 |
| 12 | MODEL | `responses` 错误归一化不一致 | `responses/mod.rs:216-224` vs `openai/mod.rs:338` | 丢 `type`/`code`，重试/降级判断失效 |
| 13 | AISTIO | native session-log 链路未接线 | core 已有 `SessionExportSink`，本 crate 零引用 | session 提交事件不外发 |

### 4.4 系统性缺陷模式（跨模块，最有价值的一类）

**模式 A：代码已实现但未接线 / 字段只写不读**（10 处，均不报编译错、不被现有测试捕获）

| 模块 | 丢失项 | 位置 |
|---|---|---|
| SCHEDULER | `factory` | `quartz/mod.rs:48,63` 只写不读；`task.rs:55` 恒 `NoopFactory` |
| SCHEDULER | `runtimes` | 4 处存在、**无读取点** |
| MONGODB | `ttl_days` | `state_store.rs:641-651` `build()` 未带入 |
| MONGODB | guard `database_name` | `sandbox_guard.rs:348-357` `build()` 未带入 |
| TRAINING | `shadow_pool_size`/`shadow_pool_capacity` | **零消费点** |
| HIGRESS | `transport_type`/`timeout`/`initialization_timeout` | `initialize()` 不建连，全部无效 |
| SANDBOX | `file_api_base_dir` | `KubernetesFileApi` 存在但**无消费者** |
| CHANNEL | `DingTalkStreamClient` | 存在但未接入 `DingTalkChannel`（无 `start()`/`stop()`） |
| AISTIO | `SessionExportSink`/`SessionLogExporter` | core 已有、本 crate 零引用 |
| MODEL | `embed_with_options` 的 `options` | `let _ = options;` 显式丢弃 |

> **共性**：这 10 处全部满足「`cargo build` 通过 + 现有测试全绿 + 功能不可用」。**这是本轮"已完成"判定普遍偏乐观的根本原因**，也是静态复核相对编译测试的增量价值所在。

**模式 B：被推翻的「core/harness 没有」类前提**（5 处）

| 模块 | 原前提 | 实际 |
|---|---|---|
| NACOS | 「protocol crate `a2a` 不可编译」 | `a2a/{card,client,message,model,server}.rs` 均在，`pub mod a2a` 已声明，`AgentCardResolver`/`AgentCard` 等已有；**仅缺 `AgentRegistry`** |
| JDBC | 「版本化 API 需 core 裁定」 | core `state/store.rs:68/76/98` **已有** trait 默认方法 |
| MONGODB | 「core trait 无版本方法」 | commit `9a8fd07` 已上提，本 crate 已实装 |
| TRAINING | 「core 无 `sysPrompt`/`maxIters` getter」 | `react_agent.rs:283/288` **已有** |
| TRAINING | 「core 无 `PreCallEvent`」 | `middleware::Middleware::on_pre_call` 注释明写 Mirrors Java `PreCallEvent`，只缺 post-call |

> **共性**：原 tracker 作者在判定「跨 crate 阻塞」时**未实际检索目标 crate**，导致 5 处伪阻塞被长期沿用。**今后凡记「core/harness 无 X」，必须附检索命令与结果**。

**模式 C：被推翻的「设计决定（不迁移）」**（3 处）

| 模块 | 原裁定 | 复核结论 |
|---|---|---|
| RAG | `ImageReader` 依赖 JVM 库 | **零第三方 JVM 依赖**（仅 `ImageBlock`/`URLSource`/core），可机械直译；且 OCR 在 Java 侧本就是空设参数 |
| RAG | `ReaderUtils` 仅服务未迁移 reader | 调用方含 `ImageReader.java:91`，前提随 ① 失效 |
| NACOS | 自建 A2A 平行类型 | protocol 侧可复用，属**重复实现债务** |

**模式 D：默认值/哨兵值反转**（4 处，最隐蔽）

| 模块 | 项 | Java | Rust |
|---|---|---|---|
| POSTGRESQL | `version` 列 ALTER 默认 | `DEFAULT 1` | `DEFAULT 0` → 老表行被当成"未创建" |
| POSTGRESQL | builder `create_if_not_exist` | `true` | `false` → Java 写法迁移直接失败 |
| REDIS | `save_if_version(0)` | 真实 CAS | 无条件 bump → **丢更新** |
| MYSQL | 默认锁超时 | 1800s | 30s |
| CHANNEL | `IdempotencyStore` TTL 过期 | 窗口重启只放行一次 | 不刷新 `seen_at_ms` → **连续放行** |

**模式 E：语义分叉（双方都"能跑"但结果不同）**

| 模块 | 项 | Java | Rust |
|---|---|---|---|
| SCHEDULER | cron 星期基准 | `1-7`（SUN=1） | `0-7`（SUN=0）→ **静默错位一天** |
| SCHEDULER | dom+dow 同时指定 | **直接拒绝**（`ParseException`） | 按 **OR** 触发 |
| SKILLS | git 技能名 | frontmatter `name` | 目录名 |
| SKILLS | description | frontmatter | 首个非空行（frontmatter 文件返回 `---`） |
| NACOS | prompt 空模板推送 | **照写**（只丢 `null`） | 空串也丢弃 |
| PROTOCOL | a2a `message/stream` | SSE 增量 | 一次性数组 |
| STUDIO | Socket.IO 鉴权键 | `run_id` | `runId` |

### 4.5 汇总方裁定（2026-10-22，共 7 项，**全部已裁定**）

> 裁定依据均为 **Java 源码实测**，非推测。执行时须同步回写各模块 tracker 与本文 §1 状态。

| # | 事项 | **裁定** | 依据 / 执行要求 |
|---|---|---|---|
| 1 | `save_if_version(0)` 语义 | **以 Java + core 契约为唯一准绳，`0` = create-if-absent，不得复用为"无条件"** | **Java 实证**：`AgentStateStore.UNVERSIONED = -1L`；`RedisAgentStateStore.UNCONDITIONAL_EXPECTED = Long.MIN_VALUE`（**独立哨兵，非 0**）；`PostgresAgentStateStore.java:375-381` 对 `expectedVersion == 0L` 走 `insertIfAbsent`，冲突且存量 version 仍为 0 时**回退 0→1 CAS**（回归用例 `saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow`）。<br>**执行**：① REDIS 删除 `expected_version == 0 → UNCONDITIONAL_EXPECTED` 分支，改为真实 CAS；`UNCONDITIONAL_EXPECTED` 仅保留给 `save()`；`UNVERSIONED` 统一用 `-1`，弃用 `u64::MAX` 语义混用。② POSTGRESQL 补 0→1 回退 + `DEFAULT 1`（两项必须同改）。③ JDBC / MYSQL 同轮核对同等语义。 |
| 2 | JDBC skill 仓库归属 | **在 `agentscope-extensions-jdbc` 实现，`agentscope-extensions-skills` 不重复实现** | Java 官方实现位于 `agentscope-extensions-jdbc/src/main/java/.../jdbc/skill/JdbcAgentSkillRepository.java`（按 namespace 分区，H2 + SQLite 双套测试）；SKILLS 侧 `MysqlSkillRepository`/`PostgresSkillRepository` 已被 `@Deprecated`。<br>**执行**：JDBC 新增 `JdbcAgentSkillRepository`（含 `SkillDialect` / `SkillResourcesDialect`）；SKILLS 现有两个 SQL 后端标注「已废弃，仅兼容，勿扩展」，不新增功能。 |
| 3 | aistio → controlplane 改名 | **执行改名**：crate 改名为 `agentscope-extensions-controlplane`，类名/环境变量对齐 `ControlPlane*` / `CONTROL_PLANE_*` | Java 侧已于 `a2cbea5c3` 完成整体 rename（**54 文件全为 rename，无增删**），仅 3 类改名、36 类保留原名，另有 4 项语义变更。<br>**执行**：① 目录 + `Cargo.toml` `name` + workspace member 改名；② 类名 `Aistio*` → `ControlPlane*`（3 处）；③ env `AISTIO_*` → `CONTROL_PLANE_*`（`control_plane_stores.rs:57-60`）；④ tracker 重命名为 `CONTROLPLANE__GAP_TRACKER.md`，**保留旧文件并在文首指向新文件**（避免历史链接失效）。<br>**理由**：双名并存会持续制造认知负担；改名成本远低于长期分叉。<br>**✅ 已执行（2026-10-22）**：目录 / 包名 / 根 `Cargo.toml` / 类型前缀 `ControlPlane*` / env `CONTROL_PLANE_*` / journal `~/.agentscope/service-controlplane/event-journal` / sink `controlplane-native-v1` 全部完成；旧 tracker 文件保留为重定向。验证：`cargo test -p agentscope-extensions-controlplane` → **128 + 4 passed**；`cargo check --workspace` → **EXIT=0**。 |
| 4 | JUDGE 是否纳入迁移范围 | **纳入**。补建 `JUDGE__GAP_TRACKER.md` 并做一次独立静态复核 | crate 已注册 workspace member（9 文件 / 3181 行 / 57 test），属既成事实；当前仅 DTO + 纯函数，HTTP 客户端未落地（`Cargo.toml` 自述刻意未声明 `reqwest`）。<br>**执行**：状态先记「进行中」；比照本轮 20 模块口径补实测统计与缺口清单（Java 76 类 / 12469 / 4035）。<br>**理由**：不登记会形成约 1.2 万行 Java 代码的 tracker 盲区。 |
| 5 | SKILLS `set_writeable` / `close()` | **`set_writeable` 迁移（仅限本 crate trait）；`close()` 不视为缺口** | **Java 实证**：`AgentSkillRepository extends AutoCloseable` 且 `setWriteable(boolean)` 是接口方法，`FileSystemSkillRepositoryTest` 有切换用例；`close()` 是 `default { /* no-op */ }`。<br>**执行**：① 在 `agentscope-extensions-skills` 自有 trait `AgentSkillRepository` 增加 `fn set_writeable(&self, writeable: bool)`，实现用 `AtomicBool` 内部可变；**不加进 harness `SkillRepository`**（harness 保持只读语义）。② `close()` 保留 Rust 现有 `Result<(), _>` + 默认 `Ok(())`，与 Java no-op 语义等价，不改 void。 |
| 6 | 锁 key 哈希跨语言一致性 | **统一按 Java 实现：`MurmurHash3 x64 128` 取 `hash[0]`** | **Java 实证**：`PostgresSandboxExecutionGuard.java:128-133` — `raw = keyPrefix + scope.name().toLowerCase() + ":" + value`，`MurmurHash3.hash128(raw.getBytes(UTF_8))`，**取 `hash[0]`**。<br>**执行（范围已修正）**：逐 crate 核对后，**仅 POSTGRESQL 独立模块**使用 `DefaultHasher`；JDBC 的 `postgres_lock_key` 用 **SHA-256 前 8 字节**（与 Java `PostgresDialect.composeLockKey` 逐位一致）、MySQL 用 SHA-256 normalize（与 `MysqlDialect.normalizeLockName` 一致）、MONGODB 用 SHA-256 hex（与 `MongoSandboxExecutionGuard.composeLockId` 一致）、REDIS 直接用字符串 key——**这四者本就与 Java 对齐，不改**。故实现落在 `agentscope-extensions-postgresql/src/murmur3.rs`（唯一消费者，不为此改 core）。<br>**实现要点**：Commons Codec `hash128` 是 **x64_128 + seed 104729**（不是 `hash128x64` 的 seed 0 重载）；已用真实 commons-codec 1.15 输出作金标向量钉住（含 8/9/12/15/16/32 字节长度扫描）。<br>**理由**：混部部署时锁 key 不碰撞 → **互斥直接失效**，属资源/数据安全风险，**不能按「设计决定」关闭**。 |
| 7 | `list_hash` 算法跨语言 | **接受现状，登记为「设计决定（不迁移）」，附两条使用约束** | core `compute_hash` 用 `DefaultHasher`(SipHash-1-3) vs Java `String.hashCode()`，两侧 hash **永不相等**。<br>**约束**：① 单语言内自愈——首次写入走全量重写，**数据无损**，无需迁移脚本；② **禁止 Java/Rust 混部共享同一份 list 状态**（否则每次写入退化为全量重写，功能正确但性能退化）。<br>**触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义。<br>**理由**：与第 6 项不同，此差异**不破坏正确性**，仅影响增量优化命中率，故可按设计决定关闭。 |
| 8 | OSS 实机后端选型（2026-10-22） | **使用 `rust-s3` 实现通用 S3 存储支持**，feature 名 `s3`；删除空壳 `aliyun` feature 及其未使用的 `aliyun-oss-rust-sdk` 依赖 | **依据**：现有 `aliyun` feature 在 `src/` 中 `#[cfg]` **命中 0**，crate 不可投产（§1 第 13 行）。`rust-s3` 0.38 同时支持 AWS S3 与 MinIO / Ceph / OSS-S3 兼容端点（自定义 `endpoint` + path-style），比绑定阿里云专有 SDK 更通用，也避免为一个不可用 feature 维护死依赖。<br>**执行**：① `OssClient` trait 保持 sync 不变，新增 `S3OssClient`（自有 Tokio runtime `block_on` 适配）；② 配置支持 bucket / region / endpoint / 凭证 / path-style；③ `get` 的 NotFound → `None`、`delete` 幂等、`list_page` 走 continuation token；④ 用 `httpmock`（已在 `Cargo.lock`，0.7.0）起本地假 S3 做端到端，**不依赖真实云账号**；⑤ 修掉 crate 文档「启用 aliyun 即得实机后端」的误导描述。<br>**约束**：默认 feature **不启用** `s3`，避免给所有使用者拉入 HTTP/TLS 依赖。 |
| 9 | `openai-official` 落地范围（2026-10-22） | **只做 Phase 1**：provider id `openai-official` 可注册 + `Model` 调用链 + 错误翻译；**Phase 2（server tools / multi-agent formatter）押后** | **依据** §4.9 拆解：3330 行中约 **1054 行可复用**、**324 行属 Java SDK 专有不适用**，真正需要新写约 900–1200 行；而 Phase 1 已能消除核心缺口「provider id `openai-official` 不可用 `openai-responses` 顶替」（约 300–400 行 / ~15 测试）。<br>**执行**：按 Phase 1 口径验收；若 agent 已产出 Phase 2 代码，标注「已实现但未验收/可回退」，**不计入完成度**。Phase 2 仅在确有 `web_search` / `code_interpreter` / `image_generation` 需求时再立项。<br>**2026-10-22 追加**：Phase 2 已随同批交付，经汇总方**语义验收通过**（metadata 键 / include / 分组规则对齐 Java，且已接入请求路径），不再要求回退；遗留项见 §4.8 #11 与 §4.9。 |

#### 4.5.1 裁定后的执行清单（按优先级）

| 优先级 | 事项 | 涉及 crate | 状态 | 验证 |
|---|---|---|---|---|
| **P0** | `save_if_version(0)` 语义统一（裁定 1） | REDIS / POSTGRESQL | **✅ 已执行（2026-10-22）** | REDIS **114 passed**；POSTGRESQL **111 passed** |
| **P0** | 锁 key 改 MurmurHash3（裁定 6） | POSTGRESQL（唯一需要，见裁定 6 修正） | **✅ 已执行（2026-10-22）** | 金标向量 7 条 + 长度扫描 6 条全绿 |
| **P1** | JDBC skill 仓库实现（裁定 2） | JDBC / SKILLS | **✅ 已执行（2026-10-22）** | JDBC **198 passed**（含 skill 仓库 32 条） |
| **P1** | aistio → controlplane 改名（裁定 3） | CONTROLPLANE | **✅ 已执行（2026-10-22）** | `cargo test -p agentscope-extensions-controlplane` → 128+4；`cargo check --workspace` EXIT=0 |
| **P1** | `set_writeable` 迁移（裁定 5） | SKILLS | **✅ 已执行（2026-10-22）** | SKILLS **34 passed**（+1） |
| **P2** | JUDGE tracker + 复核（裁定 4） | JUDGE | **✅ 已执行（2026-10-22）** | `JUDGE__GAP_TRACKER.md` 已建（静态基线，未编译） |
| **—** | `list_hash` 约束登记（裁定 7） | 全部（文档约束） | **✅ 已登记 2026-10-22**（REDIS / POSTGRESQL / MYSQL / MONGODB / COS 已加「附录 A」；JDBC 待本批 agent 落地后补） | 5 个 tracker UTF-8 校验通过 |

**P0 执行明细（2026-10-22）**

1. **REDIS**（`src/state/mod.rs`）
   - 删除 `save_if_version()` 中 `expected_version == 0 → UNCONDITIONAL_EXPECTED` 的特判；`0` 现在走真实 CAS（`current_version()==0` 时创建并置 1）。
   - trait 实现把 core `UNVERSIONED(-1)` 映射到 `UNCONDITIONAL_EXPECTED(u64::MAX)`（此前错误地映射到 `0`，与真实 CAS 版本冲突）。
   - 成功值恒 `>= 1`，故 `Ok(0)` 无歧义表示冲突 → 返回 `UNVERSIONED`。
   - 测试：`save_if_version_zero_bumps_unconditionally`（编码了 bug）改为 `save_if_version_zero_creates_only_when_absent` + 新增 `save_if_version_unconditional_sentinel_always_writes`。
2. **POSTGRESQL**（`src/sql.rs` / `src/state.rs`）
   - `ensure_version_column_ddl` 的 `DEFAULT 0` → **`DEFAULT 1`**（与 Java `ensureVersionColumn()` 一致；`0` 对 CAS 表示"行不存在"，回填 0 会让老行永久幻影冲突）。
   - `save_if_version(0)` 在 INSERT 冲突后**落到通用 CAS 分支**（`WHERE version = 0`）→ 老行 0→1 回退。
   - 测试：新增 `save_if_version_zero_falls_back_to_cas_for_backfilled_row`（对应 Java 回归用例），并同步 3 处断言。
3. **MurmurHash3**（`src/murmur3.rs`，新增）
   - x64_128 + seed `104729`，逐位复刻 Commons Codec `hash128`；金标向量由**本体 jar 实跑 JVM 生成**。
   - 踩坑记录：`MurmurHash3.hash128` ≠ `hash128x64`（后者 seed 0）；常量 `C2` 应为 `0x4cf5ad432745937f`（曾误写 `...5947...`，被金标向量当场发现）。
4. **JDBC / MYSQL**：核对后**无需改动** —— JDBC 已有 `cas_zero_version` 0→1 回退；MYSQL `Some(0)` 语义即 create-if-absent（其 CAS 非原子性属独立 P1-2，不在本裁定范围）。
5. **SKILLS（裁定 5）**：`AgentSkillRepository` trait 新增 `fn set_writeable(&self, writeable: bool)`；MySQL/PG 实现改为 `AtomicBool`（`is_writeable()` 读实时值，`save`/`delete`/`clear_all` 均在关闸后拒绝写入）；Git 后端按 Java 语义**忽略并 warn**；适配器与测试替身同步。新增用例 `set_writeable_toggles_runtime_flag`。**`close()` 保持 `Result` + 默认 `Ok(())`，未改 void**（与 Java `default void close(){}` 语义等价）。


### 4.6 本轮的积极发现

1. **MEM**：25/25 类真实对应、无空壳，是唯一可判「实现层已完成」的模块。
2. **MODEL §6**：对标 new-api 的十项能力（错误归一/超时/重试/usage 合并/能力降级/诊断/媒体 resolver/反向编码/路由注册表/Ollama embedding）**逐项复核全部命中**，此前的加分项经得起检验。
3. **RAG 四个集成子模块**（dify/ragflow/bailian/haystack）：**逐类逐键等价**，其中 bailian 的 ACS4-HMAC-SHA256 自建签名保真度最高。
4. **POSTGRESQL**：唯一**上调**的模块——原「版本化 API 待 core 裁定」阻塞已解除，六处零测试区补齐且 `MemoryClient` SQL 分派缺陷已修。
5. **COS**：唯一无 P0 且契约齐备的存储模块。
6. **多项"已完成但 tracker 未更新"**（MODEL 的 Ollama embedding / 重试超时 / diagnostics / tests 目录）：方向与前述相反，说明 tracker 存在双向滞后，不全是虚报。

### 4.7 遗留与后续建议

- **新基线（2026-10-22，重要）**：`cargo test --workspace -j 2 --no-fail-fast` → **EXIT=0 / 128 个测试二进制 / 3589 passed / 0 failed / 1 ignored**。这是**两批 P0 修复全部落地后**的全工作区实测基线（第一批 SCHEDULER / HIGRESS / MODEL 非流式 / JDBC skill / CHANNEL DingTalk；第二批 OSS `rust-s3` / MODEL `openai-official` / AISTIO session-log）。**AISTIO→CONTROLPLANE 改名与 OSS MSRV 调整后已复跑，结果一致（128 / 3589 / 0 failed）**。工作树含未提交改动，非某个 commit。此前两轮分别为 124/3445、126/3520。§3.2 的 1860 passed 属上一批历史数据，**已作废，不要再引用**。
- **基线期间发现并修复的测试缺陷**：`agentscope-extensions-training` 的 `run_registry_allocates_incrementing_ids` 与另一条测试共用进程级 `RunRegistry` 且并行执行，互相 `clear_all()` → 偶发 `count == 0`。已用测试内互斥锁串行化（连跑 3 次全绿），非生产代码回归。**后续凡测试触碰全局 static 注册表，都必须串行化。**
- **本轮曾未做的验证（已由上条补齐）**：20 个 crate 的 `cargo build` / `cargo test` 在静态复核阶段确实未执行；现已有全量基线，后续每批改动都应保持该基线不退。
- **建议下一轮**：按 §4.5.1 与 §4.8 推进。修完 §4.3 的 13 个 P0 后，**再取一次全量基线**，并为模式 A 的 10 处补**端到端断言**（尤其 SCHEDULER 的 `execution_count()`、HIGRESS 的建连、STUDIO 的 `reply` 键），否则这类缺陷仍会逃过验收。
- **并发编译注意**：多个会话/agent 同时 `cargo test` 会争 `target/` 文件锁（出现 `Blocking waiting for file lock on build directory` 属正常，等待即可）。**不要 kill cargo/rustc**——2026-10-22 曾因中断构建导致 `target/` 的 dep-info 与 fingerprint 损坏（表现为 `could not parse/generate dep info` 与 `link.exe` exit 1318），需清理对应 fingerprint 才能恢复。
- **✅ 已裁定（OSS MSRV，2026-10-22）**：`rust-s3 0.38` 为 edition 2024，要求 rustc ≥ **1.85**；据此把 **OSS crate 的 `rust-version` 提升到 1.85**（workspace 其余 crate 仍保持 1.80）。`s3` 仍为可选 feature，默认构建不受影响；开启 `s3` 时最低工具链为 1.85（当前 1.96 实测通过）。
- **工具注意（重要）**：本机 PowerShell 5.1 默认代码页为 GB2312，`Get-Content`/`>` 会**误解码 UTF-8 并粘连换行**（实测根 `Cargo.toml` 90 vs 实际 105 行）。统计行数与读写 `docs/extensions/*.md` **必须显式指定 UTF-8**，否则会得出错误编码结论（本轮已有一次将合法 UTF-8 的 rollup 误判为 GB18030）。


---

## 2026-10-22 PROTOCOL / ACP crate 拆分登记

本轮把两个「单 crate 多协议」的模块拆开，新 crate 均已在根 `Cargo.toml` 注册：

- PROTOCOL → `agentscope-extensions-a2a-client` / `agentscope-extensions-a2a-server` / `agentscope-extensions-agent-protocol` / `agentscope-extensions-agui` / `agentscope-extensions-chat-completions-web`；`agentscope-extensions-protocol` 降级为 facade。
- ACP → `agentscope-extensions-acp`（从 `agentscope-harness` 抽出；harness 不再依赖 `agent-client-protocol`，normal 依赖树 781 → 668 行）。
- 验证：`cargo check --workspace --all-targets` 通过；protocol 5 crate 304 tests 全绿；acp 46 inline + 7 集成 tests 全绿；`-p agentscope-extensions-acp --no-default-features --features lite --bins` 通过。
- 映射明细见 `docs/PROTOCOL_CRATE_SPLIT.md`。

### 4.8 §4.3 的 13 个 P0 执行状态（滚动更新）

> §4.3 只记录「发现」，本表记录「处理」。**改动完成 ≠ 关闭**：须有能复现原缺陷的用例（先失败后通过）才算 ✅。

| # | 模块 | P0 | 状态 | 验证 |
|---|---|---|---|---|
| 1 | MYSQL | save_list([]) 清空数据 | **✅ 2026-10-22** | 新增 `save_list_empty_is_a_no_op`（改前必失败：`from_index` 裁剪清空）；mysql **46 passed** |
| 2 | SCHEDULER | `schedule*` 未接 `with_factory` | **✅ 2026-10-22** | 删除 `NoopFactory`，`schedule_runtime` 传入 `self.factory`；3 条端到端用例（`trigger_and_trait_run_use_injected_factory_and_count` 等）**改前 `trigger()` 直接 `Err`**；scheduler **37 + 3 passed** |
| 3 | SCHEDULER | `schedule_with_input` 丢初始消息 | **✅ 2026-10-22** | 初始消息改由任务持有（`initial_input`），每次触发重放（对齐 `AgentQuartzJob` 每次读 JobDataMap）；用例 `schedule_with_input_delivers_initial_message_to_factory`（**只修 #2 也过不了**） |
| 4 | HIGRESS | `build()` 不建连 | **✅ 2026-10-22** | `initialize()` 经窄 trait `McpConnection` 真正 `create_client → connect`（失败回滚 `remove_client`），接通 `initialization_timeout` / `timeout` / transport 校验；7 条新用例；higress **36 passed** |
| 5 | STUDIO | 读 `message` / core 写 `reply` | **✅ 2026-10-22** | 3 条单测（`reply` 命中、`message` 回退、非结果事件为 None）；studio **62 passed** |
| 6 | TRAINING | `stream:true` 违反 Trinity 协议 | **✅ 2026-10-22** | 两层均闭环：adapter 下传 `options.stream=false`（`adapter_forces_stream_off`）+ MODEL 新增非流式路径（`non_stream_request_sets_stream_false_and_omits_stream_options`）→ 委托 `OpenAiChatModel` 时请求体 `stream:false` 且解析纯 JSON completion；`msg_ids` 连续去重（`adapter_records_duplicate_chunk_ids_once`）。training **66** / model **266+5** |
| 7 | CHANNEL | DingTalk HTTP 回调模式缺失 | **✅ 2026-10-22** | 新增 `crypto.rs`（HMAC-SHA256 验签 + AES-256-CBC + 严格 PKCS#7）与 `controller.rs`（401/400/200、`check_url`），`properties` 加 `mode`/`aesKey`/`token`，stream/http 生命周期接线；**31 条新用例**（改前因模块不存在而编译失败），channel **358 passed**（+31） |
| 8 | JDBC | skill 仓库整体未迁移（1095 行） | **✅ 2026-10-22** | 新增 `skill/{repository.rs,dialect/skill_dialect.rs}` + 四家 vendor DDL + SQLite 真事务；32 条用例（CRUD/namespace 隔离/resources/metadata 降级/只读/唯一冲突窄分类/DDL 解析），jdbc **198 passed**（166 → 198） |
| 9 | SKILLS | 同上（同一缺口） | **✅ 2026-10-22** | 按裁定 2 在 **jdbc crate** 落地，skills 侧不重复实现（`Mysql/PostgresSkillRepository` 保持兼容、不再扩展） |
| 10 | OSS | `aliyun` feature 空壳 | **✅ 2026-10-22（裁定 8）** | 删除 `aliyun` 死 feature/依赖，新增 `s3` feature + `S3OssClient`（`put/get/exists/delete/list_page`，真 continuation token、404→`None`、delete 幂等）；`httpmock` 真实 HTTP 7 条；oss **41 默认 / 52 `--features s3` + 2 doc-test** |
| 11 | MODEL | `openai-official` 未登记 | **✅ 2026-10-22（Phase 1 + Phase 2 均已验收）** | provider id 可解析 + 调用链 + 错误翻译 + server tools + multi-agent formatter；model **327 + 5 passed**；遗留 P1：`ProxyConfig` / `x-should-retry` / `reasoning.*`·`service_tier`·`prompt_cache_options` 白名单未 fail-fast / 与 `responses` 双实现 |
| 12 | MODEL | `responses` 错误归一化不一致 | **✅ 2026-10-22** | 用例 `api_error_preserves_type_code_and_status`；model **263 + 5 passed** |
| 13 | CONTROLPLANE（原 AISTIO） | native session-log 未接线 | **✅ 2026-10-22** | `ControlPlaneCommittedEventSink` 实现 core `SessionExportSink`，按 `sourceWatermark` 去重并落盘；core/harness 仅加「可选 handle → exporter」，未配置时零行为变化；controlplane **128 + 4 passed**、core **292 passed**（crate 改名已于同日完成，见 §4.5 裁定 3） |
| 14 | CHANNEL | P1/P2 残留（G2/G4/G5 + G6/G7/G8） | **✅ 2026-10-23** | ①入站媒体契约 `common/media.rs` + DingTalk/Feishu/WeCom 三平台媒体映射；②`InboundEventDeduplicator`/`AccessTokenStore` seam + `ChannelSeams` 工厂注入；③Feishu 普通事件 `header.token` 恒定时间闸门；④`BotLoopGuard` 空闲 peer 淘汰；⑤`IdempotencyStore` TTL 窗口重启语义；⑥过期注释清理。**+27 用例**（含 4 项反向验证：回退实现→FAILED→恢复），channel **385 passed / 0 failed** |

**进度**：**14 ✅ / 0 ⚠️ / 0 待办**（2026-10-23 追加 #14：CHANNEL P1/P2 残留）。

> **全量回归（2026-10-22）**：首次 `cargo test --workspace` 出现 **1 个失败**，定位为 **TRAINING 的测试竞态**（`run_registry_allocates_incrementing_ids` 与另一条测试并行时互相 `RunRegistry::clear_all()`，属既有测试缺陷，非生产代码回归）。已加测试内互斥锁修复，连跑 3 次全绿。此类全局 static 注册表的测试后续都应按此方式串行化。

> **订正（CHANNEL）**：DingTalk 回调签名**不是**经典 `token + SHA1(msg_signature)` 方案。本仓库 Java（`DingTalkCallbackCrypto.java:114`）实测为 **`Base64(HmacSHA256(appSecret, timestamp + "\n" + appSecret))`**，`aesKey` 负责报文解密，普通消息回空 200。Rust 实现以 Java 为准，SHA1 仅保留为可选兼容路径。此前派发说明中的 SHA1 描述有误，已由执行 agent 实测纠正。

**本批验证命令（均已实跑，EXIT=0）**：
- `cargo test -p agentscope-extensions-mysql -p agentscope-extensions-studio -p agentscope-extensions-training -p agentscope-extensions-model` → mysql 46 / studio 62 / training 66 / model 266+5
- `cargo test -p agentscope-extensions-scheduler -p agentscope-extensions-higress` → scheduler 37+3 / higress 36
- `cargo test -p agentscope-extensions-jdbc` → 198（含 skill 仓库 32 条）
- `cargo test -p agentscope-extensions-channel` → 358（+31）
- `cargo test -p agentscope-extensions-oss`（默认 feature）→ 41 + 1 doc-test
- `cargo test -p agentscope-extensions-oss --features s3` → 待裁定 8 落地后补
- `cargo test -p agentscope-extensions-model` → 327 + 5（含 openai-official 61 条）
- `cargo test -p agentscope-extensions-controlplane -p agentscope-core --lib` → controlplane 128+4 / core 292

**下一批建议**：① 裁定 OSS MSRV（§4.7 待决策，推荐「维持 1.80 + 文档注明 `s3` 需 1.85」）；② 清理 `openai-official` 的 Phase 2 遗留（`ProxyConfig` / `x-should-retry` / 枚举白名单 fail-fast）；③ 统一 core 的 BYPASS 双常量；④ 评估 `openai_official` 与 `responses` 的合并；⑤ 为模式 A 的 10 处补端到端断言。
### 4.9 `openai-official` 规模拆解（2026-10-22，防止「3330 行」被误读）

| Java 类 | 行数 | 作用 | Rust 现状 |
|---|---:|---|---|
| `ResponsesRequestMapper` | 753 | Msg/options/tools → `ResponseCreateParams`；reasoning 回放、图片/DataBlock、fail-fast 校验 | **部分**：现有 `responses` 已覆盖基础映射 |
| `ResponsesServerToolHelper` | 495 | 内建工具 merge / `include` / 输出解码 / 原文 replay（web_search、code_interpreter、image_generation） | **无** |
| `ResponsesStreamingAssembler` | 325 | SSE 事件 → `ChatResponse` | **已有** |
| `ResponsesMultiAgentFormatter` | 318 | 多智能体历史分组 | **无** |
| `OpenAIResponsesChatModel` | 360 | 模型主循环 / 重试 / 生命周期 | **已有**（`OpenAiResponsesModel`） |
| `ResponsesResponseParser` | 193 | 非流式 `Response` → `ChatResponse` | **已有**（本轮补的非流式路径） |
| `ResponsesHelper` | 176 | usage / metadata 提取 | **已有** |
| `OpenAISdkClientFactory` | 125 | 官方 Java SDK 客户端构造 | **不适用**：Rust 用 `async-openai` |
| `OpenAIErrorTranslator` | 117 | SDK 异常 → 领域异常 | **部分**（`ModelError`） |
| `OpenAIServerTool` | 109 | 内建工具定义/校验 | **无** |
| `OpenAIOfficialCredential` | 107 | 凭据 JSON 模型 | **不适用/可省**（Rust 走 `GenerateOptions`） |
| `OpenAIOfficialModelProvider` | 100 | ServiceLoader 注册 + `openai-official:` 前缀匹配 | **需新增（小）** |
| `OpenAIOfficialModelException` | 92 | 异常载体 | **不适用** |
| `OpenAIOfficialConstants` | 60 | 常量 | 小 |

**结论**：3330 行中约 **1054 行可直接复用**、**324 行属 Java SDK 专有不适用**；真正需要新写的约 **900–1200 行**（server tools ~600、multi-agent formatter ~320、错误翻译/provider 注册 ~150）。

**真正的工作量大头不是主代码，而是测试**：15 个测试文件 / **6569 行 / 335 个 `@Test`**（约为主代码 2 倍）。建议按增量落地：
① Phase 1（必需）：provider id 可解析 + Model 调用链 + 错误翻译（~300–400 行 / 15 测试）；
② Phase 2（真差异）：server tools + multi-agent formatter（~500 行 / 20 测试）；
③ **不迁移**：Java SDK 工厂/异常/凭据 JSON（Rust 无对应层）。

> 后续会话不要再把「3330 行」当作评估口径；按上表的 Rust 增量估算。
> **裁定（2026-10-22，用户确认）**：`openai-official` **只做 Phase 1**——provider id 可注册 + `Model` 调用链 + 错误翻译；Phase 2（server tools / multi-agent formatter）**押后**，仅在确有 `web_search` / `code_interpreter` / `image_generation` 需求时再立项。
>
> 验收口径：Phase 1 约 **300–400 行主代码 + 15 条左右测试**；若 agent 额外产出了 Phase 2 代码，一律标为「已实现但未验收 / 可回退」，**不计入 §4.8 #11 的完成度**。
**执行结果（2026-10-22）**：实际交付 **4046 行 / 10 文件 / 61 条新测试**（**超过 Java 原模块的 3330 行**），明显超出 Phase 1 范围：

- **Phase 1（已验收）**：provider id openai-official 可解析（`can_resolve("openai-official:gpt-5")`）、`Model` 调用链、错误翻译（408/409/429/5xx 可重试）；model **327 + 5 passed**。
- **Phase 2（✅ 已验收，2026-10-22 用户确认）**：`server_tool.rs` (736) + `formatter.rs` (455) + 相关用例。验收要点：metadata 键逐一对齐 Java（`openai.serverToolItem` / `openai.codeInterpreter.containerId` / `openai.reasoning.encrypted_content` / `serverTool`）、include 列表一致（`web_search_call.results` + `web_search_call.action.sources` + `code_interpreter_call.outputs`）、formatter 的 SYSTEM / TOOL_SEQUENCE / AgentConversation / BYPASS 四类分组与 Java 一致，且**真的接入请求路径**（`request.rs` 调 `format_history` + `merge_tools`，非死代码）；18 条针对性用例覆盖内建工具、回放、分组与加密推理路由。
- **Phase 2 遗留（记录，不阻塞验收）**：① `ProxyConfig`（HTTP/SOCKS/认证/nonProxyHosts）未接入；② 未处理 `x-should-retry` 响应头；③ `reasoning.*` / `service_tier` / `prompt_cache_options` 的枚举白名单**未做 fail-fast 校验**（当前透传，Java 会拒绝）——属行为差异，已在 §4.8 #11 登记；④ 无真实 OpenAI 端点 e2e。
- **验收中发现的核心层既有问题（P1，非本次引入）**：core 存在**两个同名 BYPASS 常量且取值不同**——`formatter/base_formatter.rs:12` 为 `"bypassMultiagentHistoryMerge"`，`message/keys.rs:5` 为 `"_bypass_multiagent_history_merge"`（后者与 Java 一致）。`base_formatter` 的 bypass 判定因此看不到 Java 写入的标记；官方 formatter 用的是正确那个。需后续统一（让 `base_formatter` 引用 `keys.rs` 常量）。
- **维护债（P1，已登记）**：`openai_official` 自行实现了 request/parser/model，与既有 `responses`（690 行）形成**两套 Responses 实现**，属「平行实现」模式。Phase 2 已验收，但后续维护应评估合并，避免复制扩散。