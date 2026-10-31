# extensions 迁移 — 汇总 Gap Tracker

> **用途**：`agentscope-extensions` 迁移的**汇总视图**（22 个模块的状态、跨模块裁定、验收口径）。**不替代**任何模块 tracker。
> **单一事实来源**：`docs/extensions/<MODULE>__GAP_TRACKER.md`。本文件与独立 tracker 冲突时**以独立 tracker 为准**，并在下次汇总时回填本文。
> **开工顺序**：本文件 → 本模块 tracker → `docs/extensions/EXTENSIONS_MIGRATION_PLAN.md`（范围/波次/并行约束）→ `docs/MIGRATION_PLAN.md` + `docs/MIGRATION_GAP_TRACKER.md`（core/harness）。
> **结论（2026-10-31 全量对齐复核后）**：历史 22 个模块的**存量**缺口清单仍全部关闭；但本轮**逐文件复核**（Java `main` @ `1524b3191` ↔ 本仓 HEAD，覆盖 30 个 crate：22 个 extensions 模块 + core + harness + acp + 6 个 protocol 子 crate）新增 **544 行带唯一编号的清单条目**：**待办 220 / 待裁定 41 / 已完成（本轮确认已对齐）264 / 无显式状态 19**，其中 **P0 30 条**（§3.1）。⇒ 结论更正为：**迁移未真正完成，P0 未清零前不达生产可用**。
> **历史**：裁剪前的完整历史快照存档于 [`history/EXTENSIONS_MIGRATION_HISTORY.md`](./history/EXTENSIONS_MIGRATION_HISTORY.md)（只读，仅供追溯）。

## 0. 如何阅读本文件

- **状态取值**：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`；**优先级**：P0=核心链路 / P1=重要 / P2=增强。
- **关闭规则**：模块标 `已完成` 当且仅当 **P1 清零**，剩余仅 P2 / 环境门控 / 设计决定。标「设计决定（不迁移）」**必须**给出源码级证据（文件+符号，或 `rg` 零命中）+ 影响面，否则不得作为关闭理由。
- **判失败口径**：只看 `cargo test` 输出是否存在 `^failures:` 段（辅以 `test result: FAILED`）。`Select-String 'FAILED|panicked'` 会误命中测试名与 `test result: ok … 0 failed`，不能单独作判据。
- **硬禁止**：删断言、`#[ignore]` 凑绿、把「文件已落盘」写成「已完成」（须走真实装配入口的测试）。
- 逐批回执、静态复核与执行记录不再进本文件，见 [`history/EXTENSIONS_MIGRATION_HISTORY.md`](./history/EXTENSIONS_MIGRATION_HISTORY.md)。

## 1. 模块总览

> 测试数 = 2026-10-31 收口复跑实测（`cargo test -p <crate> -j 2 --no-fail-fast`，含集成测试；doc-test 单独标注）。22 行对应 22 份 `*__GAP_TRACKER.md`；#14 与 #22 是同一模块（AISTIO → CONTROLPLANE 改名）的前后两行。

| # | 波次 | 模块 | crate | 状态 | 测试数（实测） | 独立 tracker |
|---|---|---|---|---|---|---|
| 1 | A | MODEL | `agentscope-extensions-model` | `已完成` | 419（另 5 集成） | [MODEL](./MODEL__GAP_TRACKER.md) |
| 2 | A | PROTOCOL | `agentscope-extensions-protocol`（facade + 5 子 crate） | `已完成` | 384（62 / 63 / 29 / 195 / 35） | [PROTOCOL](./PROTOCOL__GAP_TRACKER.md) |
| 3 | A | RAG | `agentscope-extensions-rag` | `已完成` | 469 | [RAG](./RAG__GAP_TRACKER.md) |
| 4 | A | SANDBOX | `agentscope-extensions-sandbox` | `已完成` | 227（另 1） | [SANDBOX](./SANDBOX__GAP_TRACKER.md) |
| 5 | A | CHANNEL | `agentscope-extensions-channel` | `已完成` | 405 | [CHANNEL](./CHANNEL__GAP_TRACKER.md)（历史：[CHANNEL__MIGRATION_HISTORY](./history/CHANNEL__MIGRATION_HISTORY.md)） |
| 6 | A | MEM | `agentscope-extensions-mem` | `已完成` | 121（另 3 env 门控） | [MEM](./MEM__GAP_TRACKER.md) |
| 7 | B | JDBC | `agentscope-extensions-jdbc` | `已完成` | 217（+1 doc；另 env 门控 13） | [JDBC](./JDBC__GAP_TRACKER.md) |
| 8 | B | REDIS | `agentscope-extensions-redis` | `已完成` | 162 lib + 6 单机 live + 2 集群 live（+2 doc） | [REDIS](./REDIS__GAP_TRACKER.md) |
| 9 | B | MONGODB | `agentscope-extensions-mongodb` | `已完成` | 99 lib + 5 live（真驱动）+ 2（另 1 doc） | [MONGODB](./MONGODB__GAP_TRACKER.md) |
| 10 | B | MYSQL | `agentscope-extensions-mysql` | `已完成` | 108 默认 / 109 + 5（`--features mysql_async`） | [MYSQL](./MYSQL__GAP_TRACKER.md) |
| 11 | B | POSTGRESQL | `agentscope-extensions-postgresql` | `已完成` | 127（含 `--features postgres-driver`；tracker 上次记 123 @10-30） | [POSTGRESQL](./POSTGRESQL__GAP_TRACKER.md) |
| 12 | B | COS | `agentscope-extensions-cos` | `已完成` | 105（+3 `no_run` doc） | [COS](./COS__GAP_TRACKER.md) |
| 13 | B | OSS | `agentscope-extensions-oss` | `已完成` | 64 默认 / 75（`--features s3`） | [OSS](./OSS__GAP_TRACKER.md) |
| 14 | C | CONTROLPLANE（原 AISTIO） | `agentscope-extensions-controlplane` | `已完成` | 158（另 4 e2e） | [CONTROLPLANE](./CONTROLPLANE__GAP_TRACKER.md) |
| 15 | C | STUDIO | `agentscope-extensions-studio` | `已完成` | 92 | [STUDIO](./STUDIO__GAP_TRACKER.md) |
| 16 | C | SKILLS | `agentscope-extensions-skills` | `已完成` | 108 / 116 / 123（pg）/ 118（mysql）/ 125（双开） | [SKILLS](./SKILLS__GAP_TRACKER.md) |
| 17 | C | SCHEDULER | `agentscope-extensions-scheduler` | `已完成` | 100（另 3） | [SCHEDULER](./SCHEDULER__GAP_TRACKER.md) |
| 18 | C | TRAINING | `agentscope-extensions-training` | `已完成` | 82 | [TRAINING](./TRAINING__GAP_TRACKER.md) |
| 19 | C | NACOS | `agentscope-extensions-nacos` | `已完成` | 92（`--features http-client`） | [NACOS](./NACOS__GAP_TRACKER.md) |
| 20 | C | HIGRESS | `agentscope-extensions-higress` | `已完成` | 60 | [HIGRESS](./HIGRESS__GAP_TRACKER.md) |
| 21 | — | JUDGE | `agentscope-extensions-judge` | `已完成` | 392（另 5 集成） | [JUDGE](./JUDGE__GAP_TRACKER.md) |
| 22 | — | AISTIO | —（已并入 `agentscope-extensions-controlplane`） | `已完成`（并入 #14） | 见 #14 | [AISTIO](./AISTIO__GAP_TRACKER.md)（重定向） |

- **波次**见 `EXTENSIONS_MIGRATION_PLAN.md` §2：A=协议/记忆/RAG/模型/沙箱/渠道，B=存储侧，C=非存储侧；JUDGE 为后续新增登记（裁定 B4），AISTIO 随 Java `a2cbea5c3` 改名为 CONTROLPLANE。
- 另有独立 crate `agentscope-extensions-acp`（自 harness 抽出，与 protocol 拆分同批登记），**不属** extensions 20 模块计划；其 tracker 位于 `docs/ACP_AGENT__GAP_TRACKER.md`。

### 1.1 core / harness / acp（本轮新增或挂接的 tracker）

| 项 | tracker | 对照面 | 缺口行 | P0 | 待裁定 |
|---|---|---|---|---|---|
| CORE | [`CORE__GAP_TRACKER.md`](./CORE__GAP_TRACKER.md)（新建） | `agentscope-core` 22 个 Java 包（361 类 / 63k 行）↔ 103 个 `.rs` / 36.5k 行 | 26 | 1（CORE-01） | 2（CORE-13/14） |
| HARNESS | [`HARNESS__GAP_TRACKER.md`](./HARNESS__GAP_TRACKER.md)（新建） | `agentscope-harness` 全包（295 类 / 53k 行）↔ 153 个 `.rs` / 49.8k 行 | 28 | 1（HARNESS-15，归属 acp） | 3（HARNESS-15/16/17） |
| ACP | [`docs/ACP_AGENT__GAP_TRACKER.md`](../ACP_AGENT__GAP_TRACKER.md)（沿用并补充） | **Java 无 ACP**（`rg -i acp` 对 Java 源码零命中）→ 改按协议规范/`ACP_AGENT_PLAN.md` 核对 | 20 | 1（ACP-01） | 0 |

- 两份新 tracker **取代**根 `GAP_TRACKER.md` 与 `docs/MIGRATION_GAP_TRACKER.md` 中 core/harness 相关段落的口径（旧文件保留作历史）。
- 逐模块明细与证据（Java `文件#符号` ↔ Rust `文件:行`）在各模块 tracker 的 §3.5 / §4.3。
- 各 crate 的数字以独立 tracker 的「实测」小节为最终依据；本表为汇总口径。

## 2. 跨模块依赖与裁定

### 2.1 裁定台账（A/B/C/D，2026-10-19 正式裁定 → 现状）

| 编号 | 裁定 | 结论与依据（一行） |
|---|---|---|
| A1=A | 跟随 Java 新 session/context 设计，废弃 SessionTree/Transcript | 已落地：core `session` + harness `agent/{context,session,verification}`；harness 侧无独立 `observation` 模块（对应类型为 core `observation::ActionObservation`，由 `verification` 消费）；`session_history_mode` 默认 `EventLog`，`Legacy` 仅显式 opt-out |
| A2=A | 版本化/CAS **上提** core `AgentStateStore` trait | 已落地：core trait 提供 `get_versioned`/`save_if_version`/`read_version`，5 个存储 crate 实现，harness `state_checkpoint` 走 CAS + 冲突回退 |
| A3=A | 引入 run 级控制 `AgentRun`/`RunControl`/`run_id` | 已落地：`core/src/agent/run.rs` + `RuntimeContext.run_id`，`AgentCore.active_run`，取消优先于结果 |
| A4=A | `ChatUsage` 扩 usage 三字段 + `Tool`/`ToolSchema` 加 `defer_loading` | 已落地：`core/src/model/tool_schema.rs::defer_loading`（含默认值用例） |
| A5=A | 响应侧 schema 校验 + strict 合并 + `ToolChoice::Specific` | 已落地：`tool/validator.rs::validate_json_output`（最多 3 次重试）+ openai `strict_json_schema` |
| A6=A | 补发 `RequireExternalExecution` 事件 | 已落地：`core/src/event/{event,event_type}.rs`（`REQUIRE_EXTERNAL_EXECUTION`） |
| B1=A | skill namespace 归 **skills crate** | 已落地：`agentscope-extensions-skills/src/{sql,mysql,postgresql,repository}.rs`（**不在** jdbc crate） |
| B2=B | anthropic **双实现并存**（自建 wire DTO + SDK） | 已落地：`src/anthropic/` 与 `src/anthropic_async/` 并存；`responses` 与 `openai_official` 同例，各自独立用例覆盖 |
| B3=A | MCP meta 传播默认 **fail-open** | 已落地（harness）：`McpServerConfig.propagate_meta` 默认 `true`，每次调用实时读取（`set_propagate_meta` 即时生效） |
| B4=A | 新建 judge/jev crate | 已落地：`crates/agentscope-extensions-judge`；根 `Cargo.toml` 仅加 1 行 `[workspace] members`（唯一授权例外） |
| B5=A | tracer seam 归并到 STUDIO telemetry exporter | 已落地：`studio/src/telemetry/exporter.rs::TraceExporter`（trait 注入，不引入 `opentelemetry` crate） |
| B6=A | 补 plan-mode 白名单与拒绝逻辑 | 已落地：core `PLAN_MODE_ALWAYS_ALLOWED` / `plan_mode_denies()`，harness `PlanModeHandle` 注册进 toolkit |
| C1=A | 引入 liteparse 做 RAG 文档解析（替代 JVM 解析器） | 落地口径：`TextReader` 等文本路径已迁；`TikaReader`/`PDFReader`/`WordReader` 判不迁移（`rag/src/reader.rs` 头注释 + RAG tracker §3.3） |
| C2=A | Redis **Cluster hash tag** | 已落地：REDIS `state/redirect.rs`（MOVED/ASK）+ `CLUSTER SLOTS` 拓扑扇出 + env 门控集群用例 |
| C3=A | 接受 JDBC **破坏性重构** | 按新结构落地（`dialect`/`store`/`state`/`snapshot`/`sandbox` 重划），不设旧 API 兼容层 |
| C4=A | server tools（Gemini/Anthropic）立项改 **core** | 已落地：core `is_server_tool` / `inline_server_tool_result_ids`；gemini `GeminiServerTool`、anthropic 6 个 `*ToolResult` 变体 + metadata 回放 |
| C5=A | MySQL COLLATE 出迁移脚本/运维说明 | 已落地：`docs/migrations/mysql_binary_collation.md`（影响面 + `ALTER` 脚本 + 回滚 + 告警） |
| D1=A | 保留 `crates/agentscope-harness/memory/2026-10-19.md` | 已保留（harness `memory/` 有意跟踪） |
| D2=C | 改动继续叠加（不再回滚重做） | 执行于分支 `dev` |
| D3=A | 主智能体逐个 grep + 跑测试核实「已落盘未接线」并修 | 4 处已修（SessionSearchTool 接线、fuzzy matcher 接线、`invalidate()` 回调、plan-mode 白名单） |

### 2.2 共享抽象归属（B 波次 7 个存储模块必须遵守）

- **不新建** `agentscope-extensions-store`：复用 core/harness 既有 trait ——`AgentStateStore`（core）、`BaseStore` / `SandboxSnapshotSpec` / `SandboxGuard` / `MessageBus` / `AsyncToolRegistry` / `TaskRepository` / `DistributedStore`（harness）。
- 每个存储 crate 的统一产出契约：`XxxDistributedStore`（组合者）+ `XxxAgentStateStore` + `XxxStore`（`BaseStore`）+ `XxxSnapshotSpec` + `XxxSandboxExecutionGuard`（`impl SandboxGuard`）。
- `DistributedStore` 字段不足已解决（2026-10-28）：harness 增三个**可选**复合槽位 `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`，各 crate 用 `DistributedStore::builder()` 投影；`SandboxGuard` 已抽为 trait（`try_enter`/`release` 为 async），存储 crate 可直接驱动跨进程沙箱隔离。
- **`list_hash` 唯一实现在 `agentscope_core::state::list_hash`**（JDBC/POSTGRESQL/MONGODB/COS/REDIS 共用）。**严禁恢复本地实现**（`rg "fn compute_hash|fn needs_full_rewrite"` 应无结果）；存量数据自动自愈，无需迁移脚本。
- CAS/版本语义、`StoreItem.version` 含义、快照 id 校验一律以 harness 定义为准，不得自定义平行类型。

## 3. 缺口清单

**历史结论（2026-10-31 复核前）**：22 份独立 tracker 的原缺口表已无 `- [ ]`、无有效「未完成 / 待办 / 待实施 / 未迁移」项，剩余差异落为 §3.2 的两种已裁定形态。

**现行结论（2026-10-31 全量对齐复核）**：逐文件复核对出 **P0 30 / P1 97 / P2 131 / 待裁定 41**，**P0 清零前不得声称生产可用**。编号规范：`<MOD>-NN` = 缺口（状态取 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；`<MOD>-DNN` = 设计决定（不迁移）；`<MOD>-XNN` = 设计决定与已知差异。**存量历史清单保持原样未重编号**。

### 3.1 各模块复核结果（2026-10-31）

> 口径：以各模块 tracker 的「2026-10-31 全量对齐复核清单」为准；数字仅计算带模块前缀的编号行（`已对齐` 行计入「已完成」列）；`PROTOCOL` 按子 crate 前缀 `PROTO-A2A-*` / `PROTO-AP-*` / `PROTO-AGUI-*` / `PROTO-CCW-*` 统计。

| 模块 | 编号前缀 | 待办 | 待裁定 | P0 | P1 | P2 | 已完成/已对齐 | 最高优先级项（一行） |
|---|---|---|---|---|---|---|---|---|
| MODEL | `MODEL-` | 15 | 2 | 2 | 9 | 5 | 15 | MODEL-01 传输 seam 实现（anthropic / openai_official）无生产构造点；MODEL-02 openai 丢弃 `ModelCreationContext.options` |
| PROTOCOL | `PROTO-A2A-` / `PROTO-AP-` | 6 | 2 | 2 | 3 | 3 | 15 | PROTO-A2A-01 A2A 线协议仍 0.3 vs Java SDK 1.4.0（1.0）；PROTO-AP-01 `timeout_seconds`/`from_seq`/SSE `event:` 不兼容 |
| RAG | `RAG-` | 8 | 2 | 0 | 5 | 6 | 11 | 无 P0；P1：分块/检索语义与 Qdrant 无 scheme 修复对齐项 |
| SANDBOX | `SANDBOX-` | 13 | 1 | 3 | 6 | 5 | 13 | SANDBOX-01 AgentRun 缺 MCP streamable-HTTP；-02 sandboxId 派生；-03 resume 重建 |
| CHANNEL | `CHANNEL-` | 7 | 2 | 3 | 3 | 3 | 13 | N01 机器人身份 `refresh()` 生产零调用；N02 Feishu 先占幂等键再 dispatch 且吞错；N03 WeCom 不校 `errcode` |
| MEM | `MEM-` | 5 | 1 | 1 | 1 | 4 | 14 | MEM-01 生产装配点缺位：crate 外零引用（Java `ReActAgent.Builder.longTermMemory*` 对应物缺失） |
| JDBC | `JDBC-` | 9 | 1 | 1 | 4 | 6 | 5 | JDBC-01 `save_list` 的「写 `:_hash`」与「按 `MAX(item_index)` 删除」非原子 |
| REDIS | `REDIS-` | 12 | 2 | 1 | 5 | 8 | 11 | REDIS-03 连接断开后 `BaseStore` 在本进程内永久失败（无重连回填） |
| MONGODB | `MONGO-` | 9 | 1 | 0 | 4 | 6 | 12 | 无 P0；索引/分页/search 语义 4 项 P1 |
| MYSQL | `MYSQL-` | 12 | 1 | 1 | 5 | 7 | 12 | MYSQL-01 `save_if_version(Some(0))` 缺 version=0 自愈回退（jdbc 已有，本 crate 无） |
| POSTGRESQL | `PG-` | 8 | 2 | 1 | 3 | 6 | 10 | PG-01 `PgSimpleClient` 在生产 async 路径 `Runtime::block_on` → 运行期 panic |
| COS | `COS-` | 6 | 2 | 0 | 2 | 6 | 10 | 无 P0；签名/ETag CAS 两项 P1 |
| OSS | `OSS-` | 8 | 2 | 1 | 3 | 4 | 10 | OSS-01 `S3OssClient` 在 `async fn` 内 `block_on` → Tokio worker 线程 panic |
| CONTROLPLANE | `CP-` | 12 | 1 | 4 | 4 | 5 | 8 | CP-01 未装 `/agentscope/*` 契约 HTTP 面；CP-02 `ContractProvider` 多方法空实现；CP-03 adapter 缺 9 个能力位；CP-04 Level-2 上报无 flush/ack 定时 |
| STUDIO | `STUDIO-` | 11 | 2 | 2 | 4 | 5 | 9 | STUDIO-01 `with_transport` 每次新建 registry → 重连丢 pending input；STUDIO-02 span 未按时 flush |
| SKILLS | `SKILLS-` | 10 | 1 | 1 | 3 | 7 | 13 | SKILLS-01 `SyncBridge` 在 Tokio runtime 内必失败 → SQL skill 后端返回 0 行 |
| SCHEDULER | `SCHED-` | 8 | 3 | 1 | 5 | 5 | 13 | SCHED-01 `schedule(AgentConfig, ScheduleConfig)` 携带配置时执行失败（`RuntimeAgentConfig::from` 丢弃 model 等） |
| TRAINING | `TRAIN-` | 9 | 2 | 1 | 2 | 6 | 13 | TRAIN-01 `TrainingHook::on_agent` 硬编码 `annotation=None` → 显式标记策略 100% reject |
| NACOS | `NACOS-` | 14 | 2 | 4 | 4 | 8 | 4 | NACOS-01/02 自建 trait 与协议 crate 不兼容；-03 无 token 刷新；-04 读路径丢 `namespaceId` |
| HIGRESS | `HIGRESS-` | 3 | 2 | 0 | 3 | 4 | 9 | 无 P0 |
| JUDGE | `JUDGE-` | 2 | 2 | 0 | 2 | 2 | 8 | JUDGE-01 JEV 三个中间件未迁（待裁定归属）；openai-decisions 已全量覆盖 |
| CORE | `CORE-` | 12 | 2 | 1 | 4 | 9 | 12 | CORE-01 middleware `RequestStop` 无消费点 → 中间件停止语义失效 |
| HARNESS | `HARNESS-` | 14 | 3 | 0 | 9 | 8 | 11 | HARNESS-15 降为 P1（测试侧已修 `5883c98`）；HARNESS-04/05 `AsyncToolMiddleware`/`SandboxLifecycleMiddleware` 未接线；HARNESS-01 `tools.json` 治理缺失 |
| ACP | `ACP-` | 7 | 0 | 0 | 4 | 3 | 13 | ACP-01 降为 P1（测试侧已修 `5883c98`，残留为 `running` 无上限设计风险） |

**P0 分组（30 条，建议按此顺序清零）**

| 组 | 主题 | 编号 |
|---|---|---|
| 1 | 装配/接线（「文件已落盘但未接线」） | HARNESS-01 / 04 / 05、MEM-01、MODEL-01 / 02、CORE-01、STUDIO-01 / 02、SKILLS-01、SCHED-01、TRAIN-01、CP-01 / 02 / 03 / 04 |
| 2 | 互操作与协议（跨语言共库/互通） | PROTO-A2A-01、PROTO-AP-01、MYSQL-01、JDBC-01、SANDBOX-02、NACOS-01 / 02 / 03 / 04 |
| 3 | 消息可靠性（丢消息/静默失败） | CHANNEL-N01 / N02 / N03 |
| 4 | 运行时安全（panic / 不可恢复） | OSS-01、PG-01、REDIS-03、SANDBOX-01 / 03 |

**待裁定（41 条）**：均为 trait/抽象归属或跨模块决策，需汇总方裁定后才能在模块内落地。典型：`SandboxGuard::try_enter` 无 `Result` 通道（REDIS-06 / MYSQL-05）、`Channel::deliver_with_receipt` 归属（CHANNEL-N05）、ACP cancel 归属（HARNESS-15）、`@Tool` 宏是否新建 `agentscope-macros`（CORE-14）、A2A 单版本 vs 双版本协商（PROTO-A2A-01）、hook 是否上提 harness（TRAIN-10）、`Memory` 是否标 deprecated（CORE-13）、JEV 中间件归属（JUDGE-01/04）。

原「零未关闭条目」口径见 §3.2（存量差异，仍有效）。

### 3.2 存量差异（非缺口）

| 类型 | 项 | 依据 / 说明 |
|---|---|---|
| 环境门控 | 真实 MySQL（JDBC / MYSQL / SKILLS） | `AGENTSCOPE_TEST_MYSQL_URL`（+ `_DATABASE`）；未设即 skip + `return`，**非 `#[ignore]`**；**已实跑通过**（2026-10-31，容器 `mysql:8.0`；需 `--features mysql-driver`/`mysql_async`，默认特性下为 0 用例） |
| 环境门控 | 真实 PostgreSQL（JDBC / POSTGRESQL / SKILLS） | `AGENTSCOPE_TEST_POSTGRES_URL`（SKILLS 侧 `AGENTSCOPE_TEST_PG_URL`）；**已实跑通过**（2026-10-31，容器 `postgres:16`；需 `--features postgres-driver`/`postgres-executor`） |
| 环境门控 | 真实 Redis（单机 + Cluster） | `AGENTSCOPE_TEST_REDIS_URL`（6 用例：Lua 脚本 / SCAN 游标 / 重连 / 锁 CAS）、`AGENTSCOPE_TEST_REDIS_CLUSTER_URL`（2 用例：槽路由 / 拓扑扇出）；**均已实跑通过**（2026-10-31，容器 `redis:7-alpine` + 3 主集群） |
| 环境门控 | 真实 MongoDB | `AGENTSCOPE_TEST_MONGODB_URL`（5 用例：state / base / snapshot / guard）；**已实跑通过**（2026-10-31，容器 `mongo:7`，官方驱动 `mongodb = "3"`）；Java 侧未用 change-stream/事务/批量写（`rg` 零命中） |
| 环境门控 | E2B / Kubernetes | `AGENTSCOPE_TEST_E2B_API_KEY`/`_URL`/`_TEMPLATE`；`AGENTSCOPE_TEST_K8S_API_URL`/`_WARM_POOL`（+`_TOKEN`/`_NAMESPACE`）；本机无 E2B 凭据、无 kube context（Docker Desktop K8s 未启用），**未实跑** |
| 环境门控 | MEM 三厂商真实端点 | `MEM0_LIVE_BASE_URL` / `BAILIAN_LIVE_API_KEY` / `REME_LIVE_BASE_URL`（`tests/live_endpoints.rs`，wire 契约已由 mock 全链路覆盖） |
| 环境门控 | CHANNEL 真实租户联调 / TRAINING live Trinity | 需厂商凭据与在线服务；签名、报文、路由均已离线覆盖 |
| 设计决定 | CONTROLPLANE `GrpcTransport` **wire 层** | 需 `protoc` + `tonic`/`prost` + 构建期拷贝 service proto，且 bidi 流无法经 unary JSON 通道；语义层已落地 `transport/asdp.rs`（握手/退避/心跳/ack 门控），wire 由宿主 `AsdpLink` 注入 |
| 设计决定 | STUDIO `StudioManager` / `TelemetryTracing` 自动装配 | core 无 `TracerRegistry`（`rg` 零命中），由宿主 `MiddlewareChain::add` 显式挂载；`TelemetryTracing` span 语义由 92 个用例覆盖 |
| 设计决定 | JUDGE `browser`（Playwright）/ `example` 子包 | Playwright 属浏览器自动化、example 为示例代码，均不属库能力 |
| 设计决定 | SKILLS `computeSignature` 签名机制 | Java 该实现被 O(n²) 拷贝/无界锁表修复引入，Rust 适配层无对应结构，保留守卫用例 `adapter_does_not_carry_a_per_skill_signature` |
| 设计决定 | TRAINING 三个 `ExecutionConfig` 字段 + `sharegpt` 数据格式 | core 无 agent 级 execution-config 字段（按调用/按工具下发，避免双套真相）；Java 侧训练数据转换器不存在（`sharegpt`/`toTrainingData` 零命中），由外部 Trinity-RFT 承担 |
| 设计决定 | MODEL 多模态工具 / 凭据类 / 异常层次 / Vertex AI / e2e-tests | 多模态归工具层；凭据统一走 `ModelCreationContext.api_key`；错误用 `ModelError` + `ErrorKind` 分类而非 1:1 异常类；`create_rejects_vertex_ai` 显式报错 |
| 设计决定 | SANDBOX `watch=true` 模板与 port-forward | port-forward 为宿主 seam（`port_forward_strategy_uses_the_forwarder_local_endpoint`） |
| 设计决定 | HIGRESS `build_input_schema` 键裁剪 | 为 Java 内部字段裁剪行为，无协议影响 |

## 4. 状态与下一步

### 4.1 验证命令与实测（2026-10-31，stable 1.96.1）

```powershell
cargo test -p <crate> -j 2 --no-fail-fast                 # 逐 crate（本表数字来源）
cargo test --workspace -j 2 --no-fail-fast                # 全量，无 skip
```

- **全量**：**147 targets / 5419 passed / 0 failed / 5 ignored / EXITCODE=0**（无 `^failures:` 段；ACP 用例已修复并计入）。
- **5 个 ignored**：MODEL `catalog` 3 个（联网读 models.dev / 依赖 OS 临时目录的已下载 payload）+ 2 个 ```ignore 文档示例（RAG `hook.rs`、CHANNEL `platform_channel.rs`），均为既有状态。
- **live 实测**：本次全量带环境变量执行，真实连通 `redis_integration`（6）、`redis_cluster_integration`（2）、`mongodb_live`（5）、`harness_docker_sandbox_test`（1，真实容器 8.4s）。JDBC/MySQL 的 `*_integration_tests` 在默认特性下为 0 用例，需显式 `--features`（数字见 §1 与各 tracker §4.1）。
- **上下文数字**（非 extensions 口径）：`agentscope-core --lib` 377、`agentscope-harness --lib` 519。
- **逐 crate 数字**见 §1；`--features` 差异（mysql_async / s3 / http-client / postgres-driver / mysql-executor）已在表中标注。
- **判失败**：只看 `^failures:` 段（§0）；`EXITCODE=0` 但不能排除「磁盘满导致假失败」（历史曾把 `.pdb` 占满盘表现为 link 失败）。
- **环境**：`-j 2` 规避 `os error 1455`（页面文件/rlib mmap）；PowerShell 管道直连全量输出会栈溢出 → 先 `Out-File` 再分析；`Blocking waiting for file lock` 属正常排队。
- **反向验证**：每模块至少回退 1 处实现 → 对应用例 `FAILED` → 恢复后全绿；明细见各 tracker 的「执行记录 / 反向验证」小节。

### 4.2 已知非本轮问题（pre-existing，不计入模块缺口）

1. ~~ACP `a_cancelled_turn_does_not_block_the_next_prompt` 永久挂起~~ **已修**（提交 `5883c98`，仅测试代码）。真因不是 cancel 链路：用例硬编码 `sleep(400ms)` 后取消，而冷会话到首次模型调用前的簿记约 0.7–1.0s；取消若抢赢，第 1 轮在到达 provider 前就被取消，于是 fixture 的「首次调用永久挂起」落到没人取消的第 2 轮 → 永久等待。改为等模型调用计数 ≥1 再取消（10s 上限 + 超时断言），断言强度未降。残留风险：provider 永不返回且客户端永不取消时 `running` 仍无上限（有意不加粗粒度超时，避免杀掉健康长任务）。
2. `acp_stdio_thought_test` 依赖 live 模型的 reasoning 输出 → 偶发失败（既有 flaky）。
3. `agentscope-extensions-training::runner` 使用进程级 `OnceLock` 静态注册表 + `clear_all()`，默认并行度下测试间竞争 → 单跑稳定（`-j 2` 可规避）。

### 4.3 后续事项

1. **P0 清零（30 条）**：按 §3.1 的四组顺序执行（装配接线 → 互操作协议 → 消息可靠性 → 运行时安全）。每组完工后回填本模块 tracker §3.5 状态并同步本文 §3.1。
2. **待裁定 41 条**：需汇总方先就抽象归属做裁定（trait 签名是否加 `Result`、`deliver_with_receipt` 归属、ACP cancel 归属、`@Tool` 宏、A2A 单/双版本、hook 上提 harness 等），裁定后转为 `待办`。
3. **真实后端联调**：MySQL / PostgreSQL / Redis（单机 + 集群）/ MongoDB / Docker 沙箱**已实测通过**（§3.2 + `history/EXTENSIONS_MIGRATION_HISTORY.md`）；仍缺环境：E2B、Kubernetes、MEM 三厂商、CHANNEL 真实租户、Trinity-RFT。
4. **可选增强（P2，131 条）**：测试密度对齐 Java（如 SKILLS 108 vs 192 `@Test`、POSTGRESQL 53 个 Java 用例未覆盖）；各 tracker §3.5 的 P2 行即候选清单。
5. **历史遗留（已部分消化）**：ACP `running` 无上限仅在「provider 永不返回且客户端永不取消」时可触发——测试侧竞态已由 `5883c98` 修复，**ACP-01 / HARNESS-15 已降为 P1**，动手改设计前需先论证不会误杀健康长任务；`acp_stdio_thought_test` 已改脚本化 SSE，不再依赖 live reasoning（HARNESS-16 注记）；`training::runner` 进程级静态注册表竞争（HARNESS-17）。

## 5. 执行约定（所有会话必须遵守）

- **编号规范（2026-10-31 起强执行）**：任何新增清单条目必须带本模块唯一编号：`<MOD>-NN`（缺口，状态五选一 `待办`/`进行中`/`已完成`/`设计决定（不迁移）`/`待裁定`）、`<MOD>-DNN`（设计决定，不迁移）、`<MOD>-XNN`（设计决定与已知差异）；子 crate 可加中缀（`PROTO-A2A-NN`）。**禁止复用/重编历史清单编号**；历史表里非前缀形式的旧 ID（如 `P0-1`）保持原样不动，新条目一律用前缀形式。
- **边界**：只写本模块 crate `crates/agentscope-extensions-<module>/**` 与本模块 tracker；不改 `agentscope-core/**`、`agentscope-harness/**`（除非该 tracker 明确授权）、不改其它模块 crate 与已归档 history。
- **文档归档**：现行状态写 `<MODULE>__GAP_TRACKER.md`（§0–§5）；执行历史 / 回执 / 静态复核 / 勘误 / 反向验证明细一律写 `docs/extensions/history/<MODULE>__MIGRATION_HISTORY.md`，tracker 内只留一行相对路径链接，**不复制正文**。
- **根 `Cargo.toml`**：不得改 `[workspace.dependencies]`；新增依赖写本 crate `[dependencies]` 并标具体版本；workspace 级需求记本模块 tracker §4.2。**唯一例外**：新建 crate 登记 `[workspace] members`（judge，用户显式授权）。
- **测试**：必须可编译、可测且**离线**（DTO serde 往返 / 纯函数映射 / transport trait 注入或 mock）；真实端点集成测试用「未设环境变量即 skip + return」，**不得用 `#[ignore]`**；不得删既有功能、不得删断言、不得放宽断言。
- **完工**：更新本模块 tracker §1/§3/§4（命令 + 实测）与本文 §1 对应行；「设计决定」标 **「已完成（设计决定，不迁移）」+ 源码级依据 + 影响面**；回执含 module / 状态 / 交付文件 / 验证命令与结果 / 剩余缺口 / 跨模块需求。
- **判绿口径**：只看 `^failures:`（同 §0）；`grep` 零命中不等于未实现。

### 5.1 最贵的教训（每次开工先看）

1. **`grep` 零命中 ≠ 未实现**：须用 2–3 个同义词交叉验证（曾把 e2b `/files`、Redis hash tag 误判缺失）。
2. **「文件已落盘」≠「已接线」**：trait 默认实现（如 `on_bridge_attached`）会掩盖漏实现的方法 → 必须有走**真实装配入口**的测试，直接 poke 内部字段的测试不算。
3. **tracker 会过时，汇总层最易腐**：引用前以源码 + `lib.rs` 的 `mod` 声明复核；更正一处措辞后 `grep` 同义旧措辞清零。
4. **跨模块契约只认单一实现**：`list_hash`/CAS 语义一律以 core/harness 为准；本地平行实现曾造成两处真实偏离（`needs_full_rewrite` 空列表分支、COS 用 `starts_with` 判追加——全量 SHA-256 无前缀性质）。
5. **`JoinHandle` drop 是 detach 不是 abort** → 取消/超时必须显式 `abort()` 或带取消令牌。
6. **测试有效性靠反向验证**：回退实现 → 对应用例 `FAILED` → 恢复全绿；注意排除「无效回退」（改动未被测试路径覆盖）。
