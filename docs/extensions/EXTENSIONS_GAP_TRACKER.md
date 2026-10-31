# extensions 迁移 — 汇总 Gap Tracker

> **用途**：`agentscope-extensions` 迁移的**汇总视图**（22 个模块的状态、跨模块裁定、验收口径）。**不替代**任何模块 tracker。
> **单一事实来源**：`docs/extensions/<MODULE>__GAP_TRACKER.md`。本文件与独立 tracker 冲突时**以独立 tracker 为准**，并在下次汇总时回填本文。
> **开工顺序**：本文件 → 本模块 tracker → `docs/extensions/EXTENSIONS_MIGRATION_PLAN.md`（范围/波次/并行约束）→ `docs/MIGRATION_PLAN.md` + `docs/MIGRATION_GAP_TRACKER.md`（core/harness）。
> **结论（2026-10-31 全量对齐复核后）**：历史 22 个模块的**存量**缺口清单仍全部关闭；但本轮**逐文件复核**（Java `main` @ `1524b3191` ↔ 本仓 HEAD，覆盖 30 个 crate：22 个 extensions 模块 + core + harness + acp + 6 个 protocol 子 crate）共 **543 行带唯一编号的清单条目**（`HARNESS-15` 去重退役，原 544）：**待办 254 / 设计决定（不迁移）5 / 已完成（本轮确认已对齐）265 / 无显式状态 19 / 待裁定 0**，其中 **P0 30 条**（§3.1）。逐行核对时 `待裁定` 实为 **43 行**（汇总表原把 `ACP-03`/`ACP-04` 并入「待办」）；其中 **41 项已于 2026-10-31 全部裁定**（§3.3），另 2 行按「已修 / 重复」关闭。⇒ 结论：**迁移未真正完成，P0 未清零前不达生产可用**。
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
| CORE | [`CORE__GAP_TRACKER.md`](./CORE__GAP_TRACKER.md)（新建） | `agentscope-core` 22 个 Java 包（361 类 / 63k 行）↔ 103 个 `.rs` / 36.5k 行 | 26 | 1（CORE-01） | 0（2026-10-31 已裁定） |
| HARNESS | [`HARNESS__GAP_TRACKER.md`](./HARNESS__GAP_TRACKER.md)（新建） | `agentscope-harness` 全包（295 类 / 53k 行）↔ 153 个 `.rs` / 49.8k 行 | 28 | 0（HARNESS-15 去重退役） | 0（2026-10-31 已裁定） |
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
| C1=A | 引入 liteparse 做 RAG 文档解析（替代 JVM 解析器） | 落地口径：`TextReader` 等文本路径已迁；`TikaReader`/`PDFReader`/`WordReader` 判不迁移（`rag/src/reader.rs` 头注释 + RAG tracker §3.3）；**2026-10-31 `RAG-09` 改判**：不引入 liteparse，改 `Reader` seam + 可选 `pdf`/`docx` feature + `ExternalApiReader` 接外部服务（§3.3.1） |
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

**现行结论（2026-10-31 全量对齐复核 + 汇总方裁定）**：逐文件复核对出 **P0 30 / P1 101 / P2 136**（裁定后补录优先级：`JDBC-12`/`OSS-09`/`TRAIN-10`/`TRAIN-11` = P1；`MODEL-12`/`MODEL-13`/`OSS-10`/`STUDIO-12`/`STUDIO-13` = P2）；`待裁定` 43 行已于 2026-10-31 全部处理完毕（**41 项裁定**：转 `待办` 36 项、转 `设计决定（不迁移）` 5 项；另 `HARNESS-16` 关闭为 `已完成`、`HARNESS-15` 去重退役），**`待裁定` 归零**，结论见 §3.3。**P0 清零前不得声称生产可用**。编号规范：`<MOD>-NN` = 缺口（状态取 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；`<MOD>-DNN` = 设计决定（不迁移）；`<MOD>-XNN` = 设计决定与已知差异。**存量历史清单保持原样未重编号**。

### 3.1 各模块复核结果（2026-10-31，**裁定前快照**）

> 口径：以各模块 tracker 的「2026-10-31 全量对齐复核清单」为准；数字仅计算带模块前缀的编号行（`已对齐` 行计入「已完成」列）；`PROTOCOL` 按子 crate 前缀 `PROTO-A2A-*` / `PROTO-AP-*` / `PROTO-AGUI-*` / `PROTO-CCW-*` 统计。
> **本表为复核时点快照（裁定前）**：43 行 `待裁定`（41 项裁定 + `HARNESS-16` 关闭 + `HARNESS-15` 退役）已于 2026-10-31 全部处理完毕（§3.3）；原 ACP 行 `待办 7 / 待裁定 0` 是汇总口径错误（`ACP-03`/`ACP-04` 行内状态为 `待裁定`），已就地更正为 `待办 5 / 待裁定 2`（裁定后均转 `待办`）。

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
| ACP | `ACP-` | 5 | 2 | 0 | 4 | 3 | 13 | ACP-01 降为 P1（测试侧已修 `5883c98`，残留为 `running` 无上限设计风险） |

**P0 分组（30 条，建议按此顺序清零）**

| 组 | 主题 | 编号 |
|---|---|---|
| 1 | 装配/接线（「文件已落盘但未接线」） | HARNESS-01 / 04 / 05、MEM-01、MODEL-01 / 02、CORE-01、STUDIO-01 / 02、SKILLS-01、SCHED-01、TRAIN-01、CP-01 / 02 / 03 / 04 |
| 2 | 互操作与协议（跨语言共库/互通） | PROTO-A2A-01、PROTO-AP-01、MYSQL-01、JDBC-01、SANDBOX-02、NACOS-01 / 02 / 03 / 04 |
| 3 | 消息可靠性（丢消息/静默失败） | CHANNEL-N01 / N02 / N03 |
| 4 | 运行时安全（panic / 不可恢复） | OSS-01、PG-01、REDIS-03、SANDBOX-01 / 03 |

**P0 现状（2026-10-31 Round 1 收口）**：上表 30 条**全部关闭**（逐条交付见 §3.3.5，逐条状态见各模块 tracker §3.5；模块 tracker 内已无 `P0 + 待办/进行中/待实施` 行）。**P0 清零**，但「P0 清零」不等于生产可用：§3.3.5 末列出的 P1 阻塞项（`try_enter -> Result` 族）与 P2 长尾仍未完成。

**~~待裁定（41 条）~~ 已于 2026-10-31 全部裁定（见 §3.3）；以下为复核时点快照，不再代表现行状态**：均为 trait/抽象归属或跨模块决策，需汇总方裁定后才能在模块内落地。典型：`SandboxGuard::try_enter` 无 `Result` 通道（REDIS-06 / MYSQL-05）、`Channel::deliver_with_receipt` 归属（CHANNEL-N05）、ACP cancel 归属（HARNESS-15）、`@Tool` 宏是否新建 `agentscope-macros`（CORE-14）、A2A 单版本 vs 双版本协商（PROTO-A2A-01）、hook 是否上提 harness（TRAIN-10）、`Memory` 是否标 deprecated（CORE-13）、JEV 中间件归属（JUDGE-01/04）。

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

### 3.3 裁定记录（2026-10-31，汇总方已确认）

> 口径：本节是 `待裁定` 项的**唯一裁定结论**；各模块 tracker 的状态列与行内 `【裁定 2026-10-31：…】` 标注与本节一致。
> 裁定原则（冲突时按序）：① 正确性 > 兼容性 > 便利性（静默丢数据 / 静默失去互斥一律 fail-closed，允许破内部 API）；② core/harness 只做「加默认方法 / 只读 getter / 可选字段」，默认值保持现行为；③ 不引入反向依赖（harness 不依赖 extension crate，装配入口由能力方提供）；④ 不新增硬依赖换能力（seam + 可选 feature 优先）；⑤ 对齐 Java 语义、不照抄实现；⑥ 每条裁定必须落到一条会失败的测试，只改文档不算关闭。

**结论：43 行 `待裁定` 全部处理完毕 → 转 `待办` 36 / 转 `设计决定（不迁移）` 5 / 关闭 2（`HARNESS-16` 已完成、`HARNESS-15` 去重退役）→ `待裁定` 归零。** 裁定后总数：待办 254 / 设计决定 5 / 已完成 265 / 无显式状态 19 = 543 条（`HARNESS-15` 退役后）。

#### 3.3.1 转 `待办`（36 项）

| 编号 | 裁定结论（一行） |
|---|---|
| `JDBC-12`（P1） | core `AgentStateStore` 增 `try_*` 默认方法族（默认 `Err(unsupported)`），harness 落盘改走 `try_*` |
| `COS-02` | 同上；读解析失败返回 `Err`，不再 `.ok()` / `unwrap_or_default()` |
| `REDIS-05` | 同上；另把 `delete` 错误文案补 user/session |
| `REDIS-06` | `SandboxGuard::try_enter` 改返回 `Result`，取锁失败即中止 |
| `MYSQL-05` | 同 `REDIS-06` |
| `PG-04` | 同 `REDIS-06` |
| `MONGO-04` | 同 `REDIS-06` |
| `MODEL-12`（P2） | 上提 `Model::tool_search_mode()`，默认 `Function` |
| `STUDIO-13`（P2） | 上提 `Model::provider_name()` 默认 `unknown`；`with_provider_name` 失效改为可见 |
| `STUDIO-12`（P2） | core 增 `ExtensionPoint::OnFormat` + `Middleware::on_format`，在 `call_with_format` 包裹 |
| `ACP-03` | handler 改用 `cx` + `agent.inspect_session` 回放 chunk |
| `ACP-04` | core `SessionLogStore::delete` 默认 `Err(unsupported)` + acp 调 delete + 删 sidecar |
| `HARNESS-17` | training 注册表改实例级，删全局 `clear_all` 依赖 |
| `MEM-01`（P0） | mem crate 提供 `MemLongTermMemoryMiddleware`，宿主经 builder `.middleware()` 挂载 |
| `CHANNEL-N09` | channel crate 提供 `build_gateway_registry` 单入口 + 真实装配测试 |
| `TRAIN-10`（P1） | harness 加通用 `on_agent_built` + `agent_handle`；training 提供 `attach_to(builder)` |
| `TRAIN-11`（P1） | （a）`RuntimeContext.extensions` 为主 +（b）`annotation_provider` 兜底，并存 |
| `HIGRESS-07` | `McpToolSource` trait 上提 harness，`McpClientManager` 提供 adapter |
| `PROTO-A2A-01`（P0） | 单版本切 1.0（`supportedInterfaces`/`protocolBinding`/`securityRequirements`/PascalCase），dispatch 保留 0.3 方法名别名两个版本周期 |
| `PROTO-A2A-05` | 不做 push：能力位保持 `false`，四方法返回 `UnsupportedOperationError` |
| `CHANNEL-N05` | harness `Channel`/`ChannelSink` 加默认 `deliver_with_receipt`（默认 `Err(Unsupported)`），飞书/微信覆写 |
| `HIGRESS-06` | `McpServerConfig` 加 `transport: Option<McpTransport>`（`None` = 保持嗅探） |
| `SCHED-05` | 文档声明单实例前提 + `SchedulerLock` trait（默认 no-op）由宿主注入 |
| `SCHED-06` | 宿主桥接契约（`ExecutorBridge` + 字段映射文档），不内置 HTTP server |
| `SCHED-09` | 可配 `Arc<Semaphore>`，默认 10 |
| `SKILLS-10` | 保持系统 `git` CLI + 启动自检（`git --version`）+ 明确错误文案 |
| `OSS-09`（P1） | 提升 workspace MSRV 至 1.85 |
| `RAG-09` | 撤销 C1=A（不引入 liteparse）：`Reader` seam + 可选 feature（pdf/docx）+ `ExternalApiReader` 接外部服务 |
| `RAG-10` | 维持 RAG 自持 `EmbeddingModel`，加 rag→model 的 wire 契约测试挡漂移 |
| `NACOS-15` | 升级为分类枚举 `Transport`/`Http`/`Business`/`Serde`，保留 `code/message` 构造 |
| `NACOS-16` | 配置级 `namespace` 必填 + 方法级可选 override，顺带关闭 `NACOS-04` |
| `JUDGE-01` | 撤销原「非运行链路」判定：照 `openai_decisions/middleware.rs` 迁移三中间件到 `src/jev/runtime` |
| `JUDGE-04` | 不改 core：judge 提供 `JevConfirmGuard::wrap`，由装配层比对 |
| `CORE-13` | `Memory` 兼容层保留 + `#[deprecated]`，一个 minor 周期后再评估删除 |
| `SANDBOX-09` | `SandboxManager::release` 按值消费 `SandboxAcquireResult` 并 `close()` |
| `PG-03` | 接受与 Java 的 DDL 差异：`DEFAULT 1` + insert 显式 `version=1` + 旧库迁移脚本 |

#### 3.3.2 转 `设计决定（不迁移）`（5 项）

| 编号 | 裁定结论（一行） |
|---|---|
| `COS-08` | 对象存储无租约原语：保持进程内 + `is_distributed=false`，跨进程互斥由 jdbc/redis/pg/mysql/mongo guard 承担 |
| `MODEL-13`（P2） | 维持无解析缓存；若将来加缓存须同时做注册驱逐 + 单飞 |
| `CORE-14` | 本轮不建 `agentscope-macros`；若做只做 `#[derive(ToolParams)]`（参数结构 → schema），不做函数宏 |
| `CP-13` | 不做主动推送：控制面走契约 HTTP 拉取；若协议要求再立项 |
| `OSS-10`（P2） | 仅 S3 兼容子集；原生特性将来走 `opendal` 第二后端（可选 feature） |

#### 3.3.3 关闭（2 项）

| 编号 | 关闭原因 |
|---|---|
| `HARNESS-16` → `已完成` | `acp_stdio_thought_test` 早已改为内置脚本化 OpenAI 兼容 SSE（`tests/acp_stdio_thought_test.rs:96-199`），不再依赖 live reasoning；§4.2-2 的 flaky 记录同步更正 |
| `HARNESS-15` → 去重退役 | 与 `docs/ACP_AGENT__GAP_TRACKER.md` 的 `ACP-01` 为同一问题（同 `session.rs` 行号、同用例），执行归 `ACP-01`；编号保留不复用 |

#### 3.3.4 裁定后执行批次

1. **批次 0（唯二 P0 裁定项，已转 `待办`）**：`PROTO-A2A-01`、`MEM-01`。
2. **批次 1（core，单智能体串行）**：`try_*` 方法族、`SandboxGuard::try_enter -> Result`、`Model::tool_search_mode`/`provider_name`、`ExtensionPoint::OnFormat`、`SessionLogStore::delete`、`Memory` 弃用标注 —— 全部非破坏式（默认方法 / 新增扩展点）。
3. **批次 2（harness）**：`on_agent_built` + `agent_handle`、`Channel::deliver_with_receipt`、`McpServerConfig.transport`、`McpToolSource` 上提、`SandboxManager::release` 按值 + fail-closed 接线。
4. **批次 3（模块落地）**：各 crate 实现 + 故障注入测试。
5. **批次 4（文档收口）**：`COS-08`/`MODEL-13`/`CORE-14`/`CP-13`/`OSS-10` 落 §4.3.1、`RAG-09` 改判记录、`SCHED-05/06` 契约文档、`HARNESS-15/16` 状态更正。

#### 3.3.5 Round 1 执行结果（2026-10-31，22 个子智能体并行）

> 口径：本节是 **§3.3.4 批次 1–3 首轮的实测结果**，也是各模块 tracker 里「待主智能体实测」字段的**唯一回填来源**（模块 tracker 不再各自维护's passed 数）。子智能体只做编辑、禁跑 `cargo`；编译与测试由主智能体统一执行。

**基础设施（批次 1/2，core + harness）**

| 交付 | 位置 |
|---|---|
| `AgentStateStore::try_get_versioned` / `try_save_if_version` 默认方法族（`Err(unsupported)`；`InMemory` 拒空 sessionId、`JsonFile` 报损坏/写失败） | `core/src/state/store.rs:131/169` |
| `Model::provider_name()`（默认 `unknown`）、`Model::tool_search_mode()` + `model::ToolSearchMode` | `core/src/model/model.rs:31/41`、`model/tool_search_mode.rs`（新） |
| `ExtensionPoint::OnFormat`（8→9）+ `Middleware::on_format` + `MiddlewareChain::apply_format`，接进 `reasoning_phase`/`call_with_format` | `core/src/middleware/middleware.rs:162/304`、`agent/react_agent.rs:1995` |
| `SessionLogStore::delete` 默认 `Err(unsupported)`；`Memory` 标 `#[deprecated]` | `core/src/session/log.rs:221`、`core/src/memory/mod.rs` |
| `on_agent_built` + `HarnessAgentHandle`；`Channel`/`ChannelSink::deliver_with_receipt` + `DeliveryReceipt`；`SandboxManager::release` 按值消费 + `take_sandbox()`/`close()` | `harness/src/agent/harness_agent.rs`、`gateway/channel/channel.rs`、`sandbox/manager.rs` |
| `agent/tools.rs`（tools.json 治理）、`mcp_registrar` + `McpServerConfig.transport` + `McpToolSource`/`McpClientSource`、`AsyncToolMiddleware`/`SandboxLifecycleMiddleware` 接线 | `harness/src/agent/{tools.rs(新), mcp.rs, mcp_client_manager.rs, middleware/sandbox_lifecycle.rs}` |

**模块落地**

| 模块 | 本轮关闭 | 关键交付 | 新增测试 |
|---|---|---|---|
| CORE | `CORE-01`、`CORE-13` 收尾 | `event_generate_reason`/`stop_reason`/`stop_message`（中间件停止语义生效、结果成对落盘） | `tests/state_store_versioning_test.rs`(+11)、`model_test.rs` 扩展 |
| HARNESS | `HARNESS-29/30/01/09/04/05`、`HARNESS-17` | 见上表；`tools.json` 过滤点位于「全部 catalogued 工具后、平台工具前」 | 5 个新集成文件（17 用例） |
| ACP | `ACP-01`、`ACP-03` | closed 轮询分支 + `cancel_epoch_moved`（turn 代次在 admission 前捕获）+ load 先回放再应答 | `tests/acp_session_cancel_test.rs`(2)、`acp_load_replay_test.rs`(1) |
| PROTOCOL | `PROTO-A2A-01`、`PROTO-A2A-05`、`PROTO-AP-01` | A2A 切 1.0（PascalCase + 旧名别名、`supportedInterfaces`/`securityRequirements`）；push 四方法 `-32004`；SSE `event:`/`timeout_seconds`/`Last-Event-ID` | 既有套件内新增 |
| MEM | `MEM-01/02/03` | `MemLongTermMemoryMiddleware`(order=50，`run_id` 每轮仅检索 1 次)、`MemError::HttpStatus` 带 vendor/operation、ReMe 文案 | `tests/long_term_memory_wiring_test.rs`(1) |
| MODEL | `MODEL-01/02/11`、`MODEL-09` 部分 | `src/assembly.rs` 装配入口（7 provider + 协议实现表）、`OpenAiChatModel::default_options`/`from_context`、anthropic/responses usage 映射 | 既有 428 用例内新增 |
| SANDBOX | `SANDBOX-01/02/03` | AgentRun MCP streamable-HTTP（`initialize`+`Mcp-Session-Id`）、Crockford base32 sandboxId 派生、`sandbox_id` 元数据双向读写 + resume 重建 | 既有套件内新增 |
| CHANNEL | `N01/N02/N03/N06/N07/N08/N09` | 机器人 `start()` 触发 `refresh()`；Feishu 先 dispatch 后落幂等键 + `Unavailable`→503；WeCom `errcode` 校验 + 截断；UTF-8 严格解码→400；`gateway.rs::build_gateway_registry` 单入口 | +22 离线用例（另有 5 条期望更新） |
| CONTROLPLANE | `CP-01/02/03/04` | 契约 HTTP 服务装配 + 自注册 + ASDP 事件泵（批 20、in-flight 15s 同 `report_id` 重投、5s 泵）；`FrameworkAdapter` 9 个 Java 对齐默认方法 | +17 离线用例 |
| STUDIO | `STUDIO-01/02/04` | `StudioTransport::input_registry` 复用 registry（重连不丢 pending input）；span 周期 flush（5s/512）；模型 span 四个用量字段 | `tests/manager_input_loop.rs`(1) |
| SCHEDULER | `SCHED-01/05/06/09` | factory 解析模型、`SchedulerLock` 门控每次 firing、dispatch 映射、并发上限 gate | +10 离线用例 |
| TRAINING | `TRAIN-01/11`、`HARNESS-17` | `resolve_annotation(ctx)`（`RuntimeContext.extensions` 优先、`annotation_provider` 兜底）+ `shadow_rounds()`；注册表实例化 | +5 用例（loopback mock + 10s timeout） |
| JDBC | `JDBC-01/02/03` | `save_list` 读改写事务化、无 `RETURNING` 回退事务化、负 `expected_version` 拒绝 | `state_tests.rs` 36→39 |
| MYSQL | `MYSQL-01/03` | `save_if_version(Some(0))` 冲突回退、`key_access_is_valid` 接入 4 个 key 入口 | 既有套件内新增 |
| OSS / COS | `OSS-01`、`COS-01` | `S3Worker` 专有 OS 线程 + `mpsc`；COS 超时/重试（全抖动、POST 不重放） | `oss/tests/async_bridge.rs`(3) |
| POSTGRESQL / MONGODB | `PG-01/03`、`MONGO-01/02` | `block_on` 三态运行时策略；session `DEFAULT 1` + 显式 `version=1` + 迁移脚本；`default_database()` seam；legacy JSON 兼容 | `docs/migrations/postgresql_session_version_default.md`（新） |
| REDIS | `REDIS-02/03/04` | 拓扑重定向（`READONLY`/`CLUSTERDOWN`/`TRYAGAIN` + 槽缓存失效）、连接重试、`delete_key_inner` 两形态清理 | 13 离线 + 1 环境门控 live |
| NACOS | `NACOS-01/02/03/04/11/15/16` | 复用 a2a-server 协议类型、登录 token 复用 + 90% TTL 刷新、401 重放一次、错误分类枚举、namespace 必填 + override | +13 fake transport 用例 |
| RAG / JUDGE | `RAG-09/10`、`JUDGE-01/04` | `Reader` seam（pdf/docx 可选 feature）+ `ExternalApiReader`；embedding wire 契约；JEV 三中间件 + `JevConfirmGuard` | `rag/tests/embedding_wire_contract.rs`(6)、judge +24/+6 |
| SKILLS | `SKILLS-01/02/10` | `SyncBridge` 改专用 OS 线程驱动（`block_in_place` 不用）；`git --version` 启动自检 + `git_binary` 注入；PG schema 校验 | +10 离线用例 |

**仍阻塞 / 未开工（Round 2 候选）**

- `SandboxGuard::try_enter` **仍是 `-> SandboxLease`**（`harness/src/agent/sandbox/guard.rs:29/105`，未改）→ `MYSQL-05`、`PG-04`、`MONGO-04`、`REDIS-06`、`SANDBOX-09` 五个模块阻塞在这一条 trait 变更上；改它需要同步改 5 个存储 crate 的 guard 实现与 `SandboxManager`，建议单独一轮、由 harness 独占。
- 需 in-process fixture / 新传输层：`ACP-02`（MCP-over-ACP fixture）、`SKILLS-03`（TLS）、`SKILLS-04`（git 同步互斥）、`REDIS-01`（真实集群路由）、`CP-05/CP-12`。
- 需 core/harness 既有能力已具备、可直接开工：`JDBC-12`、`COS-02`、`REDIS-05`（`try_*` 已落地）、`STUDIO-12/13`（`OnFormat`/`provider_name` 已落地）、`MODEL-12/13`（`tool_search_mode` 已落地）、`ACP-04`（`delete` 已落地）、`CHANNEL-N05`（`deliver_with_receipt` 已落地）、`TRAIN-10`（`on_agent_built` 已落地）。
- P2 长尾未动：`MEM-04/05/06`、`NACOS-05~10/12~14`、`MYSQL-02/04/06`、`JDBC-04/05/11`、`STUDIO-05/06`、`HARNESS-02/03/08/12/13`。

**本轮新增依赖（均已实测编译通过）**

- `agentscope-extensions-sandbox`：`sha2`（workspace 版本，Cargo 内 `{ workspace = true }`）。
- `agentscope-extensions-mem`：`futures`（dev 侧另加 `agentscope-harness`）。
- `agentscope-extensions-nacos`：path 依赖 `agentscope-extensions-a2a-server`（连带 axum/reqwest，记 NACOS tracker §4.2-7）。
- `agentscope-extensions-studio`：dev-deps tokio 追加 `net`/`test-util`。
- 根 `Cargo.toml`：MSRV 说明（`OSS-09`，`rust-version = "1.85"` 判定依据），未动 `[workspace.dependencies]`。

**分派说明**：本轮共 22 个并行子智能体；其中 harness 第二路（复核 `on_agent_built`/`deliver_with_receipt`/`McpTransport`/`McpToolSource`/`SandboxManager::release`/`HARNESS-04`）**净改动 0**——六项均已在批次 2 落盘，其自建测试与既有用例重复已删除。

## 4. 状态与下一步

### 4.1 验证命令与实测（2026-10-31，stable **1.97.1**）

```powershell
cargo test -p <crate> -j 2 --no-fail-fast                 # 逐 crate（本表数字来源）
cargo test --workspace -j 2 --no-fail-fast                # 全量，无 skip
```

- **全量（Round 1 收口实测，2026-10-31）**：**158 targets / 5652 passed / 0 failed / 6 ignored / EXITCODE=0**（无 `^failures:` 段）。工具链 `rustc 1.97.1 (8bab26f4f 2026-07-14)`。
- **6 个 ignored**：`agentscope-core` lib 3 个（既有）+ 3 个 ```ignore 文档示例（CHANNEL `platform_channel.rs`、RAG `hook.rs`、HARNESS 新增示例），**无 `#[ignore]` 新增**。
- **本轮修掉的失败（主智能体统一验证 loop）**：`agentscope-core` `FormatInjector::on_format` 签名、MODEL `openai` `Option<Option<_>>`、agent-protocol `LAST_EVENT_ID`、RAG `docx` 字节串转义、harness `tools.rs` 生命周期/所有权、mem 测试 trait 转型、nacos `FakeTransport` 强转与 A2A 1.0 形状、scheduler quartz 注入锁、CHANNEL wecom 截断、STUDIO span 周期导出、CONTROLPLANE bridge 三例（缺 `start()` 未开 journal）、ACP `session/close`（closed 分支仍等 journal 写者租约 → 加 500ms 有界关闭）、RAG `embedding_wire_contract` 字节序。
- **live 实测**：真实连通 `redis_integration`（6）、`redis_cluster_integration`（2）、`mongodb_live`（5）、`harness_docker_sandbox_test`（1）。JDBC/MySQL 的 `*_integration_tests` 在默认特性下为 0 用例，需显式 `--features`（数字见各 tracker §4.1）。
- **`--features` 补充实测**：`-p agentscope-extensions-oss --features s3` **全绿（EXITCODE=0）**；`-p agentscope-extensions-rag --features pdf,docx` 由 RAG tracker 记录。
- **上下文数字**（非 extensions 口径）：`agentscope-core` 476（lib 384 + 12 集成 target）、`agentscope-harness` lib 530。
- **判失败**：只看 `^failures:` 段（§0）；`EXITCODE=0` 但不能排除「磁盘满导致假失败」（历史曾把 `.pdb` 占满盘表现为 link 失败）。
- **环境**：`-j 2` 规避 `os error 1455`（页面文件/rlib mmap）；PowerShell 管道直连全量输出会栈溢出 → 先 `Out-File` 再分析；`Blocking waiting for file lock` 属正常排队；全量 `cargo test --workspace` 首轮编译约 15–20 分钟，需后台进程 + 日志轮询（工具单命令上限 ~300s）。
- **反向验证**：每模块至少回退 1 处实现 → 对应用例 `FAILED` → 恢复后全绿；明细见各 tracker 的「执行记录 / 反向验证」小节。

### 4.2 已知非本轮问题（pre-existing，不计入模块缺口）

1. ~~ACP `a_cancelled_turn_does_not_block_the_next_prompt` 永久挂起~~ **已修**（提交 `5883c98`，仅测试代码）。真因不是 cancel 链路：用例硬编码 `sleep(400ms)` 后取消，而冷会话到首次模型调用前的簿记约 0.7–1.0s；取消若抢赢，第 1 轮在到达 provider 前就被取消，于是 fixture 的「首次调用永久挂起」落到没人取消的第 2 轮 → 永久等待。改为等模型调用计数 ≥1 再取消（10s 上限 + 超时断言），断言强度未降。残留风险：provider 永不返回且客户端永不取消时 `running` 仍无上限（有意不加粗粒度超时，避免杀掉健康长任务）。
2. ~~`acp_stdio_thought_test` 依赖 live 模型的 reasoning 输出 → 偶发失败~~ **已修（`HARNESS-16` 关闭，2026-10-31 裁定）**：测试改为内置脚本化 OpenAI 兼容 SSE（`tests/acp_stdio_thought_test.rs:96-199`），不再依赖 provider 返回 reasoning；遗留仅是 `agent_exe()` 依赖预构建 bin（见 ACP-05）。
3. `agentscope-extensions-training::runner` 使用进程级 `OnceLock` 静态注册表 + `clear_all()`，默认并行度下测试间竞争 → 单跑稳定（`-j 2` 可规避）。

### 4.3 后续事项

1. **~~P0 清零（30 条）~~ 已完成（2026-10-31 Round 1，见 §3.3.5）**：四组顺序执行完毕，全量 158 targets / 5652 passed / 0 failed。下一步按 §3.3.5「仍阻塞 / 未开工」清单推进：先由 harness 独占一轮改 `SandboxGuard::try_enter -> Result` 解锁 5 个存储模块，再处理已解锁项（`JDBC-12`/`COS-02`/`REDIS-05`/`STUDIO-12,13`/`MODEL-12,13`/`ACP-04`/`CHANNEL-N05`/`TRAIN-10`）。
2. **~~待裁定 41 条~~ 已完成（2026-10-31 全部裁定，见 §3.3）**：36 项转 `待办`（按 §3.1 的 P0 分组顺序执行）、5 项转 `设计决定（不迁移）`（`COS-08` / `MODEL-13` / `CORE-14` / `CP-13` / `OSS-10`）；`HARNESS-16` 关闭为 `已完成`、`HARNESS-15` 去重退役。执行批次见 §3.3.4。
3. **真实后端联调**：MySQL / PostgreSQL / Redis（单机 + 集群）/ MongoDB / Docker 沙箱**已实测通过**（§3.2 + `history/EXTENSIONS_MIGRATION_HISTORY.md`）；仍缺环境：E2B、Kubernetes、MEM 三厂商、CHANNEL 真实租户、Trinity-RFT。
4. **可选增强（P2，136 条）**：测试密度对齐 Java（如 SKILLS 108 vs 192 `@Test`、POSTGRESQL 53 个 Java 用例未覆盖）；各 tracker §3.5 的 P2 行即候选清单。
5. **历史遗留（已部分消化）**：ACP `running` 无上限仅在「provider 永不返回且客户端永不取消」时可触发——测试侧竞态已由 `5883c98` 修复，**ACP-01 已降为 P1（`HARNESS-15` 已于 2026-10-31 去重退役，执行归 `ACP-01`）**，动手改设计前需先论证不会误杀健康长任务；`acp_stdio_thought_test` 已改脚本化 SSE，不再依赖 live reasoning（HARNESS-16 注记）；`training::runner` 进程级静态注册表竞争（HARNESS-17）。

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
