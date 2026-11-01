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


### 3.3.6 Round 2 执行结果（2026-11-01，Wave A 独占 + Wave B 20 路并行）

> 口径：每个子智能体**自验收**（`cargo build -p <crate> -j 2` + `cargo test -p <crate> -j 2 --no-fail-fast` + feature 门控 + 至少 1 处反向验证：回退实现 → `FAILED` → 恢复全绿；测试失败必须自己修到绿）。主智能体最后跑一次全 workspace 全量并判定。

#### Wave A（harness 独占单路，前置）

| 交付 | 位置 |
|---|---|
| `SandboxGuard::try_enter(&self, key) -> Result<SandboxLease>`（async，**fail-closed**：拿不到租约返回 `Err`，不再降级 `SandboxLease::noop()`） | `harness/src/agent/sandbox/guard.rs` |
| 唯一生产落点改为 `lease = self.execution_guard.try_enter(key.clone()).await?;`——失败**不 resume / 不 create**、不 fallback | `harness/src/agent/sandbox/manager.rs:213` |
| 7 个存储 crate 签名同步；原有 fail-open 分支（拿不到锁仍继续）一律改 `Err`（cos/oss 委托进程内 guard 恒 `Ok`，无需失败分支） | `extensions-{jdbc,mysql,postgresql,mongodb,redis,cos,oss}/src/sandbox/*` |
| 新用例 `guard_acquisition_failure_aborts_the_acquire`（`FailingGuard` + `CountingSandboxClient`：断言**未**调用 create/resume） | `harness/src/agent/sandbox/manager.rs` |

实测：harness lib 531 / redis 187 / mysql 115 / postgresql 130 / mongodb 108 / cos 119 / oss 77 / jdbc 257，全 0 failed，EXITCODE=0。反向验证 2 处（`?` → `warn!` + `noop`；redis `Err` → `Ok(noop)`）均 `FAILED` → 恢复。

**有意不收口的遗留**：`jdbc/src/sandbox/jdbc_guard.rs` 的**同步**兼容入口 `try_enter` 仍返回 `JdbcSandboxLease`（超时降级 noop）——它不在 harness 生产路径上，收口需连带改造同步调用方；理由记 `JDBC__GAP_TRACKER.md` §4.2 项 8。`controlplane` 自建 `ControlPlaneSandboxLease::try_enter` 不受本 trait 影响。

#### Wave B（20 路并行，全部交付并通过自验收）

| 模块 | 关闭缺口 | 关键交付 | 实测 |
|---|---|---|---|
| MODEL | `MODEL-03/04/09` | OpenAI prompt 缓存标记（`MAX_CACHE_MARKERS=4`）；音频 `input_audio` / 视频降级文本；Gemini `PROVIDER_OPTION_KEYS` 过滤 + per-call `strictJsonSchema` | 445（419→+8 新增用例） |
| PROTOCOL | `PROTO-A2A-04/02/06` | `tasks/resubscribe` per-task 事件 log + `lastEventSeq` 游标回放、未知任务 `-32001`、重连不重跑 agent；`send_stream` + `SseDecoder` 半帧安全 + `A2aAgent::stream_events`（顺带修分块文本被拼成 `"Hel\nlo"`）；`tasks/list` → `-32004` | a2a-client 71 / a2a-server 70 |
| RAG | `RAG-01/02/05` | `is_retryable_status` / `execute_with_retry` / `RetryingTransport`（**预算耗尽返回最后一次响应**而非 `Err`，保住 RAGFlow/HayStack 语义）；Dify `topK>=1`、scoreThreshold/weights ∈[0,1]；`limit==0` → `Limit must be positive` | lib 492（`--features pdf,docx` 494） |
| SANDBOX | `SANDBOX-05/06/07` | exec 默认 cwd=`workspace_root`（k8s 仅在有 cwd 时 `cd <root> && ( cmd )`）；`OUTPUT_TRUNCATE_BYTES=512KiB` + `truncate_utf8_bytes` 码点边界安全；E2B `append_capped` 超限仍消费保 exitCode | 244（227→+9） |
| CHANNEL | `CHANNEL-N05/N04` | **结构发现**：harness 对 `T: PlatformChannel` 有 `ChannelSink` blanket impl → 平台适配器无法覆写默认方法；新增 `common/receipt.rs` 的 `ReceiptChannel`/`ReceiptSink<C>`、`ChannelRegistry::with_manager_and_receipts`、`build_gateway_registry_with_receipts`，飞书 `delivery_id`→返回 `message_id`；`common::http::ensure_success` 非 2xx → `ChannelError::Remote{status, body≤200}` | 440（426→+14） |
| ↳ 后续 | Round 3 已删除该适配层 | harness 提供 `PlatformChannel::deliver_with_receipt` 后，`ReceiptSink`/`with_manager_and_receipts`/`build_gateway_registry_with_receipts` 全部移除，改由适配器直接覆写 trait 方法（见 §3.3.7） | 444 |
| CONTROLPLANE | `CP-05/06/07` | `dispatch_downstream(AsdpEvent)` 接事件泵（`sessionCmd`/`executionAttempt`（cancel 终止）/`conversationTurn`）；`context` adapter 权威、`subagents`/`workspaces` 委托、无 adapter → `Unsupported`；压缩后锁内写回 `state_handle()` + `agent_state_store.save` | lib 183 + 4 e2e |
| STUDIO | `STUDIO-06/12/13` | export 失败指数退避（`EXPORT_MAX_ATTEMPTS=3`，100ms 起翻倍封顶 5s）；`TelemetryTracing::on_format` + `active_points()` 纳入 `OnFormat`（`format=None` 不出 span）；provider 名优先 core `Model::provider_name()` | 102 lib + 1 集成（96→+7） |
| SKILLS | `SKILLS-04/05/07` | `sync_lock: Mutex<()>` git 同步单飞 + `clone_count()`；`split_frontmatter` 允许 `---` 尾随空白；MySQL 单次 `execute_batch` + `inserted != len` 报错；顺修 `SKILLS-01` 残留 bug：`SyncBridge` 在 async 上下文 drop 多线程 runtime panic → `Option<Runtime>` + `Drop::shutdown_background` | 128 / 138(pg) / 133(mysql) |
| SCHEDULER | `SCHED-02/03/04` | 取消即从 `jobs` 摘除（`Arc<RwLock<HashMap>>` + `Weak` 无环）；`FIXED_RATE` 显式 `period` 网格跳空不补触发、`CRON` `FIRE_ONCE_NOW`；首触发时序用例（`[dev-dependencies]` 加 tokio `test-util`） | 115 lib + 3 集成 |
| TRAINING | `TRAIN-02/03`、`TRAIN-10`（本 crate 侧） | `on_agent` 后处理改 `handle.spawn`（无 runtime 只告警不 panic）；`has_agent()` + `unbound_hook_count()`；`ShadowAgentBlueprint` 快照取代 `Weak`、`bind_blueprint`/`clone_blueprint`/`run_shadow_blueprint`、`bind_agent` 改收 `&ReActAgent` | 91 |
| NACOS | `NACOS-05/06/07` | `release_agent_card(card, endpoint_type, set_as_latest)`（恒传 `"service"`，HTTP query 首位）；`RetryPolicy` 默认 3 次 / 200ms×2 / 上限 2s（仅 Transport / 429 / 5xx），逐请求 timeout 默认 30s，polling 退避上限 60s；`heartbeat()` + 可 abort 的 `spawn_heartbeat()` | 115（`--features http-client`，92→+23；默认 113） |
| HIGRESS | `HIGRESS-02/06/07` | `call_tool_with_meta` 每次读连接级 `propagate_meta`；`to_server_config()` 投影 `McpServerConfig.transport`；删本 crate `McpToolSource`，re-export harness 的 + `register_higress_client(...)`（`instanceof` 语义替代记 `HIGRESS-X11`） | 63 lib + 1 doc |
| MEM + JUDGE（合并一路） | `MEM-04/06`、`JUDGE-02/03` | `is_recordable` 完整 Java 谓词（角色 / 无 `ToolUseBlock` / 文本非空 / 无 `<compressed_history>`；Bailian `trim()` vs ReMe `is_empty()`）；`Options.observer` + `Observation::Drop` 补报 `Cancelled`（`catch_unwind` 吞观察器 panic）；7 处陈旧 fail-closed 文案改指 `jev::runtime` | MEM 138+3+1 / JUDGE 426+5 |
| JDBC | `JDBC-12/04/10` | 覆写 `try_get_versioned`/`try_save_if_version`，`JdbcError`→`core::Error::Session`（torn row 不再冒充 `version==0`）；`MAX_STATE_COLUMN_LENGTH=255` + `utf16_len()` 对齐 Java `String.length()` + `validate_ids`；`strip_comments` | 268（257→+11） |
| REDIS | `REDIS-05/11/06/13` | `try_*` 覆写（无错版由 `try_*` 派生 + `tracing::error!` 后降级 `UNVERSIONED`；`Err` 与 CAS 冲突可区分）；`is_held` 先 `warn!` 再 false，同 key 不可重入（vs host 进程内 guard 可重入）；`exists` 改单条 `SCARD` | lib 186 + 9 env-gated + 2 doc |
| MONGODB | `MONGO-03/04/06` | `build_with_indexes()` / `DistributedStore::try_new()` async（**不做** lazy ensure，偏差记 tracker）；fail-closed 收尾 + 经 `SandboxManager::with_shared_guard` 真实装配路径；续约 `find_one_and_update(upsert:false)`，`Ok(None)` → `is_lock_lost()` + 停续约 | lib 106 + live 5 + doc 2 |
| MYSQL | `MYSQL-04/05/13/10` | 会话键 `0x1F`→`:`，收敛 `compose_slot_id`/`slot_id_prefix`；poison 容忍、`release` 仅释放自己的租约；`user:session` ≤255 拒 `:` 歧义；`MysqlError::Conflict{key,expected,found}` | 118（`--features mysql_async` 117 + 5 skip） |
| POSTGRESQL | `PG-04/02/10` | 四类失败全 `Err`（含租约登记表 Mutex 中毒），`rg "SandboxLease::noop" src` 零命中；`PostgresError::Sql{code,message}` 保留 SQLSTATE（`is_duplicate_key` 以 class-23 为准）；解码失败 `warn!`，坏 payload ≠ `UNVERSIONED`；顺修两处既有测试缺陷（`store.get()` 与 `Option<StoreValue>` 比较编译失败；`driver::tests::block_on_*` async 上下文 drop Runtime panic） | 136（`--features postgres-driver` lib 139 + 集成 6） |
| COS | `COS-02/03/04` | `try_get_versioned`/`try_save_if_version`（不可失败版出错回落 `version=0`，**绝不返回 `UNVERSIONED`**）；`.version` sidecar + 条件写真 CAS（`get_etagged`/`put_if_match`（412→Conflict）/`put_if_absent`/`supports_conditional_writes()`）；`exists` 改 `list_page(prefix, None, 1)` | 129（119→+13） |
| OSS | `OSS-02/03/04`、`OSS-09` | `OssClient` 条件写 seam（`get_with_etag`/`put_conditional`/`PutOutcome`，默认 `Unsupported` 不破坏第三方后端；`put_if_version_strict` 以 `.version` sidecar 为 CAS 锚点）；ETag → 63-bit FNV-1a 令牌作 version；`run()` `tokio::time::timeout` 默认 60s + `100ms·2^n` 上限 5s、0~25% 抖动、默认 3 次重试 | 82（`--features s3` 107）+ doc |

#### 跨模块需求（Wave C 候选，累计 10 条，均需 harness/core 独占）

1. `HarnessAgentHandle` 补 `description` / `sys_prompt` / `max_iters`（或 `on_agent_built` 给 `&HarnessAgent`）→ 解锁 `TRAIN-10` 的 `attach_to(builder)`。
2. harness `McpClientManager::call_tool_with_meta` 读 `config.propagate_meta`；`McpToolInfo` 补 `title` / `output_schema`（HIGRESS）。
3. core `convert_call_tool_result`（`HIGRESS-03/05`）。
4. `AsdpTransport` 补上行回报方法（CONTROLPLANE `executionAttempt`(ack/start/heartbeat/cancelled) 与 `conversationTurn`；与 `CP-13`「不主动推送」同源）。
5. ~~harness `PlatformChannel::deliver_with_receipt` 默认方法（CHANNEL `XM1`；落地后可删 `ReceiptSink` 适配层）~~ **已完成（Round 3，`4676a29` + `ReceiptSink` 已删）**。
6. `PreparedResponse` 扩响应头（CHANNEL `N04` 限流诊断）。
7. core `AgentStateStore` 补 `try_save`/`try_save_list`/`try_delete`（JDBC `save`/`save_list`/`delete` 仍只能 warn 降级）。
8. core 补 `try_get`/`try_list`/`try_exists`/`try_list_session_ids`（REDIS）+ `try_get_list`/`try_save_list`（COS）。
9. harness skill 单目录装载暴露缝（`SKILLS-06`，否则要复制第三份资源扫描）。
10. harness `SandboxGuard::is_held` 同步签名导致只能阻塞探测（`REDIS-07`）；另 harness 侧 release→`kill_sandbox` 的 manager 级用例（`SANDBOX-09`）。

**非阻塞提示**：core `ExtensionPoint::OnFormat` 不带 formatter 输出，format span 无 `agentscope.function.output`；`async-openai` 0.41 无 `video_url` part（上游限制，已降级文本）。

#### 反作弊审计

- 无新增 `#[ignore]`（此前 2 处 `rg` 命中为 tracker 文档文字，非代码）；无新增 `todo!()` / `unimplemented!()`。
- 断言净增 +582 / −21；测试函数净增 +153。
- 反向验证残留扫描：`rg "REVERSE-VERIFICATION" crates -g '*.rs'` **零命中**（`r2-cos` 中途曾把 `conditional_write` 改成恒 `Written`，交付前已恢复；仅 `history/PROTOCOL__MIGRATION_HISTORY.md` 正文提及该词）。

#### 提交（按功能模块分组，`dev` 线性）

24 条，按「一模块一组」拆分：`feat(harness)` ×1（Wave A `SandboxGuard`）、`feat(extensions/<mod>)` ×20（model / protocol / rag / sandbox / channel / controlplane / studio / skills / scheduler / training / nacos / higress / mem+judge / jdbc / redis / mongodb / mysql / postgresql / cos / oss）、`chore(deps)` ×1（`Cargo.lock`：a2a-client `futures` / scheduler `test-util`）、`chore(memory)` ×1（文本/marker 收尾随对应模块提交）。每条正文列缺口编号 + 验证结果。`git log --merges --oneline` 在 `dev` 上**无输出**（线性历史，无 merge commit）。

#### 剩余 P1/P2（Round 3 候选）

- MODEL-05~08/10 + P2 14~18；SANDBOX-04/08 + 10~14；CP-08 + CP-13 上行；STUDIO-03/05/07~11；SCHED-07/08/10；HIGRESS-01/04；JDBC-05（需 `JdbcError` 携带 SQLSTATE）/06~09/11。
- REDIS-01（`ManagerRedisCommands` 注入 `ClusterRouter`）/07~10/12/14；MONGO-05/07/08/09/10；MYSQL-02（写事务）/06~09/11/12；NACOS-08/09/10/12/13/14。
- PROTO-A2A-03、PROTO-AP-02；RAG-03/04/06/07/22；SKILLS-03（TLS 需根 workspace 加 rustls feature）/06/08/09/11；COS-05/06/07；OSS-08（列表/snapshot 组合 + 真机 412 验证）；MEM-05；TRAIN-04~09①。

### 3.3.7 Round 3 执行结果（2026-11-01，Wave C：harness/core 独占 5 路 + 下游接线 8 路）

目标：关闭 §3.3.6 列出的 **10 条跨模块需求**（均须 harness/core 独占串行），再由下游模块接线。分两阶段：Phase 1 五路（缝本身）→ Phase 2 八路（下游接线 + 存储侧 `try_*` 补齐）。

#### 缝设计裁定（与任务示例的偏差，均已按仓库现状收敛）

| 需求 | 裁定 |
|---|---|
| core `AgentStateStore::try_*`（8 个） | 全部提供**加性默认实现**，否则 8 个存储 crate 同时编译失败 |
| `convert_call_tool_result` | 放 **harness**（`agent/mcp_content.rs`）而非 core：core 不依赖 `rust-mcp-sdk`，harness 本就是 MCP 宿主层，HIGRESS 直接复用 |
| `SandboxGuard::is_held` | **不改 async 签名**（会强制 8 个 crate 同步改）；新增异步 `try_is_held`，同步 `is_held` 退化为本地视图 |
| `PlatformChannel::deliver_with_receipt` | 沿用既有 `Channel::deliver_with_receipt` 的 fail-closed 签名（`&Msg` + `Option<&str>`）；默认 `Err(unsupported_delivery_receipt(..))`，blanket `Channel`/`ChannelSink` 转发 |
| 「kill_sandbox」 | harness 侧无该方法：后端 kill 走 `Sandbox::close()` → `shutdown()`；manager 级用例钉住这条链 |
| 控制面上行 | `dispatch_downstream(adapter, transport: Option<&AsdpTransport>, event)`；`None` 零上报；发送失败仅 `tracing::warn!`，命令结果原样返回 |
| CHANNEL `ReceiptSink` | Round 2 的镜像层**整体删除**（harness 已提供默认方法），飞书/微信改为在 `impl PlatformChannel` 直接覆写 |

#### Phase 1（5 路，缝本身）

| 提交 | 模块 | 交付 |
|---|---|---|
| `88dedfa` | core | `AgentStateStore` 8 个 `try_*` 默认方法（`try_save`/`try_save_list`/`try_get`/`try_get_list`/`try_exists`/`try_delete`/`try_delete_key`/`try_list_session_ids`）；`Tool::output_schema()`；`McpTool` output schema |
| `1f17724` | harness | `HarnessAgentHandle` 补 `description`/`sys_prompt`/`max_iters`；`McpToolInfo` 补 `title`/`output_schema`；`call_tool_with_meta` 读连接级 `propagate_meta`（fail-closed）；新建 `agent/mcp_content.rs::convert_call_tool_result`；`load_skill_from_dir` |
| `4676a29` | harness | `PlatformChannel::deliver_with_receipt` 默认方法 + blanket 转发；`SandboxGuard::try_is_held` 异步缝；manager `release→close/shutdown` 用例 |
| `91aeb29` | controlplane | `ExecutionAttemptPhase`(Ack/Start/Heartbeat/Cancelled)、`ExecutionAttemptReport`；`ConversationTurnPhase`(Accepted/Started/Completed/Failed)、`ConversationTurnReport`；`AsdpTransport::report_execution_attempt/report_conversation_turn` |
| `22b5994` | channel | `PreparedResponse` 增 `headers: Vec<(String, String)>` + `with_header(s)` |

#### Phase 2（8 路，下游接线）

| 提交 | 模块 | 交付 | 测试 |
|---|---|---|---|
| `d153cf5` | TRAINING | `TRAIN-10` 收尾：`TrainingRunner::attach_to(builder)` / `attach_to_with`、`ShadowAgentBlueprint::from_handle`、`TrainingHook::blueprint`；`Cargo.toml` 加 path 依赖 harness | 93 |
| `7037b1c` | HIGRESS | `HIGRESS-03/05`：`render_call_result` 委托 harness `convert_call_tool_result`；`mcp_tool_info_to_tool` 透传 `title`/`output_schema` | 65 |
| `bbe82cc` | CONTROLPLANE | `CP-05` 上行回报接线；`dispatch_downstream` 加 `Option<&AsdpTransport>`；attempt/turn 回报点；补 `ExecutionAttemptPhase::Completed/Failed`（wire `complete`/`fail`） | lib 195 + e2e 4 |
| `d0d1b04` | CHANNEL | `CHANNEL-XM1`：删 `common/receipt.rs`、`ReceiptChannel`/`ReceiptSink`、`with_manager_and_receipts`、`build_gateway_registry_with_receipts`；飞书/微信覆写 `deliver_with_receipt`；`ReqwestTransport::execute` 填充响应头 + 大小写不敏感取值 | 444 |
| `8ccf350` | SKILLS | `SKILLS-06`：`get_skill` 只加载目标目录；`skill_names_in_dir` 只读 `SKILL.md` frontmatter；复用 harness `load_skill_from_dir` | 132 / pg 142 / my 137 |
| `93b0342` | JDBC | `JDBC-12` 扩展：覆写 7 个 `try_*`（本 crate 无 `delete_key`）；错误如实上抛 | 274 |
| `d400a8c` | REDIS | `REDIS-07`：`try_is_held` + `probe_is_held`；`is_held` 改纯本地视图（零网络往返）；补齐 9 个 `try_*` | 203 |
| `89fe151` | COS | 7 个 `try_*` 覆写；坏 payload/连接失败 `Err`（不再冒充「不存在」） | 132 |

收口文档：`e525db2 docs(extensions): Wave C 收口 —— 10 条跨模块需求清零`（9 files，+73/−38）。

#### 收口结果

- **10 条跨模块需求全部清零**（§3.3.6 列表逐条对应：1→`1f17724`+`d153cf5`；2→`1f17724`+`7037b1c`；3→`1f17724`+`7037b1c`；4→`91aeb29`+`bbe82cc`；5→`4676a29`+`d0d1b04`；6→`22b5994`；7/8→`88dedfa`+`93b0342`/`d400a8c`/`89fe151`；9→`1f17724`+`8ccf350`；10→`4676a29`+`d400a8c`）。
- **全量（Round 3 收口实测，2026-11-01）**：**173 targets / 5890 passed / 0 failed / 6 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 2 基线 **+76 passed**，targets 与 ignored 未增（Wave C 未新增测试二进制，只在既有 target 内加用例）。
- 反向验证：各路至少 1 处（回退实现 → 对应用例 `FAILED` → 恢复全绿）；`rg "REVERSE-VERIFICATION" crates -g '*.rs'` 零命中。
- 无新增 `#[ignore]` / `todo!()` / `unimplemented!()`。
- `git log --merges --oneline` 在 `dev` 上无输出（14 条 Wave C 提交全部线性）。
- `Cargo.lock` 仅随 `d153cf5`（TRAINING 新增 harness path 依赖）变动，归属正确。

#### 剩余待办（Round 4 候选）

Round 2 §3.3.6「剩余 P1/P2」清单整体顺延（MODEL-05~08/10、SANDBOX-04/08、CP-08/CP-13、STUDIO-03/05/07~11、SCHED-07/08/10、HIGRESS-01/04、JDBC-05/06~09/11、REDIS-01/07~10/12/14、MONGO-05/07/08/09/10、MYSQL-02/06~09/11/12、NACOS-08~14、PROTO-A2A-03/AP-02、RAG-03/04/06/07/22、SKILLS-03/06/08/09/11、COS-05/06/07、OSS-08、MEM-05、TRAIN-04~09①），另加 **SANDBOX-09**（harness manager 级 kill 链用例已在 `4676a29` 落地，剩余为后端侧断言）。

### 3.3.8 Round 4 执行结果（2026-11-01，21 路全并行：core/harness 各独占 + 19 扩展模块）

目标：清掉 Round 3 收口后各模块 tracker §3.5.1 中仍挂 `待办` 的 P1/P2 长尾。按「crate 归属 + 文件互不重叠」拆成 **21 路**：**core 与 harness 各自独占**（分别只有一条路径可改该 crate），其余 19 个扩展模块按 crate 隔离。每路交付前必须完成自验收（`cargo build -p` / `cargo test -p -j 2 --no-fail-fast` / feature 组合 / **≥1 处反向验证**），主智能体统一跑全量闸门并按模块拆分提交。

#### 执行结果（按模块）

| 模块 | 本轮关闭 | 实测 passed |
|---|---|---|
| CORE | `CORE-02`（全 deny 判定 + `last_assistant_message_with_reason`）、`CORE-03`（`agent/failover.rs`：`AgentCore.fallback_model` + `FailoverListener`）、`CORE-04`（`tool/interceptor.rs`：`ToolCallInterceptor` 链）、`CORE-06`（`middleware/final_answer_filter.rs`）、`CORE-07`（`message/execution_details.rs`） | 14 targets / **507** / 3 ignored |
| HARNESS | `HARNESS-07`（`subagent/spec_generator.rs`）、`HARNESS-08`（`McpServerConfig.prefix_tool_names` → `{server}__{tool}` 注册）、`HARNESS-10`（`PinnedSandboxFilesystem` + `RoutedSandboxFilesystem`）、`HARNESS-11`（`BaseStoreSkillUsageBackend` + `CompositeFilter`）、`HARNESS-13`（builder 直连 core 两个执行配置）、`HARNESS-14`（`assets/context-conventions.md`，opt-in 不默认注入） | lib **573** / 0 failed |
| MODEL | `MODEL-05`（`reasoning_details` / `thoughtSignature` 回传）、`MODEL-07/08`（openai/anthropic/gemini 的 `multiAgentPrompt`）、`MODEL-14/15/16`（dashscope `enable_search` / `enable_code_interpreter` / `multimodal_patterns` / native structured output）、`MODEL-12`（复核 + 接线 `Model::tool_search_mode()`） | **457**（+12） |
| RAG | `RAG-03`（`src/tool.rs::KnowledgeRetrievalTool`）、`RAG-04`（`RetryPolicy.max_delay` + 10s cap）、`RAG-06`（删除空转 `check_compatibility` 旋钮）、`RAG-07`（文档指向）、`RAG-22`（gRPC 端口误用的 REST 提示） | **509** / `pdf,docx` 511 |
| JDBC | `JDBC-05`（`JdbcError::Database(DbError{message,sql_state,vendor_code})` + 窄化 `is_unique_violation`）、`JDBC-06`（`enable_base_tables` / `enable_skill_tables`）、`JDBC-07`（跨域重名表校验）、`JDBC-11`（4 写者×25 轮并发用例） | **289**（16 target） |
| MYSQL | `MYSQL-02`（`StateBackend::save_list_and_hash` 单事务）、`MYSQL-06`（标识符允许 `-` + 反引号）、`MYSQL-07`（`COALESCE(MAX(item_index)+1,0)`）、`MYSQL-08`（upload 刷新 `created_at`）、`MYSQL-09`（`MysqlError::Server{code,sql_state,message}`）、`MYSQL-11`（`connect_with_config` 表名单一来源）、`MYSQL-12`（`get_single_versioned` 单条 SELECT） | **129** 默认 / 135 `mysql_async` |
| POSTGRESQL | `PG-05`（`ConnectionPool` + `postgres-pool` feature，零新依赖）、`PG-06`（`// Java:` 溯源 + 4 条补测）、`PG-07`（`testing` feature 门控 memory 模块）、`PG-08`（19 处吞错补 `warn!`）、`PG-09`（`Statement::columns()` 判 Rows/Affected） | **142** 默认 / 148 `postgres-driver,postgres-pool` |
| REDIS | `REDIS-01`（`raw_client.rs` 注入 `ClusterRouter`，7 个键命令走 slot 路由；snapshot/guard 在集群下可用）、`REDIS-08`（`set_bytes_with_ttl` 单命令 `SET..EX`）、`REDIS-09`（`SAVE_LIST_SCRIPT` 一次 EVAL）、`REDIS-10`（死代码取舍入 rustdoc）、`REDIS-12`（空白 keyPrefix / 零 TTL / 零重试间隔显式拒绝）、`REDIS-14`（Drop 语义文档化 + live 断言） | **207**（195 lib + 7 单机 + 3 集群 + 2 doc） |
| MONGODB | `MONGO-05`（投影读 `find_one_projected` + `$project{count:$cond}`）、`MONGO-07`（`ttl_days<=0` 在 `build()` 拒绝）、`MONGO-08`（续约上限 `MAX_LEASE_LIFETIME_MULTIPLE=10`）、`MONGO-09`（`connect_with_options`）、`MONGO-10`（注释/错误文案/`count` 接线） | **118**（111 lib + 5 live + 2 doc） |
| COS | `COS-05`（`multipart_threshold > 5 GiB` build 期拒绝）、`COS-06`（`list_parts` 翻页 + `resume_multipart` ETag 复用 + 进度回调 + 有序 complete）、`COS-07`（`presign_*_with_params` + 保留字校验） | **139**（+7） |
| OSS | `OSS-05`（`delete_session_strict` 按 1000 分批走 `delete_many` 单次 `?delete`）、`OSS-06`（`search_strict` 有界翻页 `max_keys=剩余×2≤1000`）、`OSS-07`（`put_multipart`，part ETag 自算 MD5，默认 `Unsupported`）、`OSS-08`（新建 `tests/s3_store.rs` 11 例） | **88** 默认 / **128** `s3`（真机 412 未验证） |
| NACOS | `NACOS-08`（`build_transport_properties` 改 `collect::<Result<_,_>>()?`，非法 transport 使 register 返 `Err`）、`NACOS-09`（`format_host_for_url` 包 IPv6 方括号）、`NACOS-10`（`skill_exists_detailed` + 非 NOT_FOUND 补 `warn!`）、`NACOS-12`（`unsubscribe_prompt`/`unsubscribe_agent_card`/`close`）、`NACOS-13`（`NacosHttpAiService::polling`）| `NACOS-14`（注释对齐） | **120** 默认 / **121** `http-client` |
| HIGRESS | `HIGRESS-01`（`list_tool_specs` 保原始 schema + `tool_parameters` 对齐 Java 转换）、`HIGRESS-04`（复用前比对 endpoint，不一致 fail-loud） | **69** + 1 doc |
| MEM | `MEM-05`（非幂等 `add` 重试风险文档化 + 构造期 `warn!`） | **143**（139+3+1） |
| SCHEDULER | `SCHED-07`（`SchedulerError` 四变体）、`SCHED-08`（`impl TaskControl` + 真实调用点）、`SCHED-10`（`AgentConfig`/`ScheduleConfig` camelCase + snake_case alias） | **123**（lib 120 + 集成 3） |
| TRAINING | `TRAIN-04`（shadow 独立 toolkit + 自绑 todo）、`TRAIN-05`（`cleanup` 移出循环）、`TRAIN-06`（`TrinityModelAdapter` 三个能力转发）、`TRAIN-07`（`sample_rate` 返回 `Result`）、`TRAIN-08`（`TrainingHook::order()=-500`）、`TRAIN-09`（100% 采样 permit 用例） | **99** |
| STUDIO | `STUDIO-03`（`ConnectionState` + `wait_ready`）、`STUDIO-05`（心跳看门狗 `timeout_at`）、`STUDIO-07`（`SpanStream::drop` → `Unset`）、`STUDIO-08`（`Local::now()`）、`STUDIO-09`（builder 注入 transport）、`STUDIO-10`（`structuredInput` 默认 `{}` + `agent_id`）、`STUDIO-11`（`TerminalIo` + 提示符） | **110** |
| SKILLS | `SKILLS-08`（`split_frontmatter` 上移 + 只存正文）、`SKILLS-09`（真库集成测试 env 门控）、`SKILLS-11`（`file://` 仓经真实装配路径挂载） | **138** 默认 / 148 pg / 143 mysql |
| PROTOCOL | `PROTO-A2A-03`（`A2aAgentBuilder::middlewares` + `wrap_agent`/`wrap_reasoning` 接入）、`PROTO-AP-02`（cancel 200 空体、`task_id` 蛇形超集、`tool_call_id` 双收） | a2a-client 72 + agent-protocol 36 |
| CONTROLPLANE | `CP-08`（`MessageBus` → `log_append`/`log_read` 有界重放）、`CP-09`（能力集合与 info/自注册同源）、`CP-10`（session phase 状态机 + `sessions`/`session_state`）、`CP-11`（`BusyGuard` Drop + Err 即时复位）、`CP-12`（`update_identity` 通知后续帧） | lib **206** + 4 e2e |

#### 缝设计裁定（本轮确认，后续勿反复）

1. **core / harness 必须独占**：两条路径分别只能改自己的 crate，否则并行必冲突。`CORE-07` 因此选了 **metadata key `executionDetails` + 类型化存取器**（`ToolResultBlock` 字面量构造遍布 26+ 处，加字段会击穿 harness/extensions）。
2. **`PROTO-A2A-03` 未改 core**：`wrap_agent` / `wrap_reasoning` / `Next<T>` 早已存在（`core/src/middleware/middleware.rs:322/343`），A2A 侧只是接入；`Hook` 无 Rust trait → 用 `on_reasoning` / `on_reasoning_result` 等价覆盖。
3. **`SKILLS-03` 阻塞**：需根 `[workspace.dependencies]` 加 rustls feature（硬禁区），已行内标注待裁定。
4. **`MYSQL-09` 的 JDBC 侧不改代码**：`jdbc/src/dialect/vendor/mysql.rs` 键列已显式 `utf8mb4_bin`，表默认保留 `utf8mb4_unicode_ci` 是带注释的有意设计（只影响非键列）；本轮只把 `docs/migrations/mysql_binary_collation.md` 的不精确表述改正。
5. **`HARNESS-08` 与 Round 3 不重复**：Round 3 落的是连接级 `propagate_meta`，本轮是 `prefix_tool_names`（`{server}__{tool}` 注册名）。

#### 跨模块需求（Round 5 输入，累计 12 条）

| 编号 | 归属 | 需求 |
|---|---|---|
| a | harness | `CORE-07` 的 `kind`/`exit_code`/`output_truncated` 由 shell / 文件工具填充 |
| b | core | `MODEL-17`：`JsonSchema` 增 `description`、`ImageBlock` 增 `detail` |
| c | core | `RAG-03`：工具上下文暴露会话历史（当前用 `ConversationHistorySource` seam 绕过） |
| d | harness | `SANDBOX-12`：暴露 `LayoutSpec`（`SandboxClient::create`/`SandboxContext`）或 `WorkspaceSpec.bind_mounts` |
| e | harness | `COS-08`：跨进程互斥归属 harness |
| f | harness | `HIGRESS`：`list_tools` 端口暴露原始 schema（通用 `McpToolSource` 路径拿不到） |
| g | harness | `HARNESS-02`：core failover 已就绪（`CORE-03`），可接 |
| h | core | `HARNESS-12`：`stopOnReject` 字段 |
| i | 根 `Cargo.toml` | `SKILLS-03`：rustls feature（**待裁定**） |
| j | 跨 crate | `PROTOCOL`：AP `status` 词表统一（`pending/running/success/error/cancelled`） |
| k | JDBC | `MYSQL-09` 的 JDBC 表默认 collation（已按「有意设计」归档，非缺陷） |
| l | core | `MODEL-10` thinking 回放（本轮未做） |

#### 全量闸门（Round 4）

**177 targets / 6066 passed / 0 failed / 7 ignored / EXITCODE=0**，无 `^failures:` 段；较 Round 3 **+4 targets / +176 passed**。

#### 提交（按功能模块分组，`dev` 线性）

21 条模块提交（`feat(core)` / `feat(harness)` / `feat(extensions/<mod>)` ×19）+ 1 条 docs + 1 条 `chore(memory)`。`Cargo.lock` 随 OSS 模块提交（`md5` 依赖）；`git log --merges --oneline` 在 `dev` 上无输出。

#### 反作弊审计（Round 4）

- 源代码**零新增** `#[ignore]` / `todo!()` / `unimplemented!()`（diff 命中的 13 + 1 处全在 tracker / 回执正文）。
- 断言净增 **+609 / −27**；新增测试函数 **+106**；`rg "REVERSE-VERIFICATION" crates -g '*.rs'` 零命中。
- 每路均提交了「回退了什么 → 哪条用例 FAILED → 已恢复」的反向验证记录；抽样复核有效（`r4-jdbc` 恢复前后文件 SHA-256 一致）。
- 范围体检：改动的 `Cargo.toml` 全部属各自 crate，`docs/` 只有各模块 tracker + 一份运维文档（主智能体改），无越界。
### 3.3.9 Round 5 执行结果（2026-11-01，11 路并行：core 两串行段 + harness 独占 + 9 扩展模块）

**范围**：Round 4 之后各模块 tracker 剩余的未闭合项 + 4 项待决策裁定后的派生工作。

| 路 | 关闭缺口 | 实测 |
|---|---|---|
| core-1 | CORE-05（`tool/search` 包：策略/工具/中间件 + `ToolContextState.discovered_tools`）、CORE-11（`FinishReasons`）、CORE-27（`JsonSchema.description` / `ImageBlock.detail`）、CORE-28（`stop_on_reject` + `rejected_turn_reason`） | 14 targets / **526** / 3 ignored |
| core-2 | CORE-08（`state::legacy::load`，Java 实际键名 `toolkit_activeGroups`）、CORE-09（`RequirementProposeTool`）、CORE-29（`ReasoningContext` 保 thinking metadata）、CORE-30（会话历史通道） | 14 targets / **543** / 3 ignored（+17） |
| harness | HARNESS-02（failover 接线）/ 03（三 Spec + `LayoutSpec`/`bind_mounts`）/ 06（两个校验器 + `load`）/ 12（`stop_on_reject` 透传）、CORE-07（`executionDetails` 工具侧）/ 10（`context_build` 事件）/ 12（`SubagentExposed` 发射） | lib **602**（+29） |
| model | MODEL-06（能力位选项）/ 10（`responses` reasoning 回放）/ 17（description + detail 透传）/ 18（anthropic signature 回放）+ CORE-11 字面量收敛 | **467**（+11） |
| skills | SKILLS-03（`postgres-tls` / `mysql-tls`，opt-in，未改根依赖） | **138 / 150 / 154 / 146** |
| jdbc | JDBC-08（指数退避 50→1000 封顶）/ 09（快照表 ENGINE/CHARSET）+ tracing 埋点 | **292**（+3） |
| mysql | tracing 埋点 23 处（零逻辑改动）+ MYSQL-26 复核确认 | **129** / `mysql_async` 128 |
| training | HARNESS-17（实例级注册表复核确认，`global()` 标 legacy） | **99**（默认并行连跑 3 次） |
| agent-protocol | AP status 词表对齐 Java + snake_case 键并存 | **38**（+2） |
| rag | RAG-03 接入 core `ToolExecutionContext::conversation_history()` | **512** / `pdf,docx` 514 |

#### 全量闸门（Round 5）

**177 targets / 6149 passed / 0 failed / 7 ignored / EXITCODE=0**，无 `^failures:` 段；较 Round 4 **+83 passed**。

#### 裁定与实施（2026-11-01，Round 5 前置）

| 项 | 结论 | 依据 |
|---|---|---|
| AP status 词表 | **AP 服务端改用 Java 词表**（`pending/running/awaiting_confirm/success/error/cancelled`，snake_case 键并存） | 真 bug：harness `RemoteTaskStatus`（`protocol_client.rs:54-70`）只认 `success/error/awaiting_confirm/cancelled`，而 AP `snapshot_value` 发 `Completed/Failed` → Rust 服务端 + Rust 客户端永远判不出成功；且 `awaiting_confirm` 时 status 仍为 `Running` |
| MODEL-10 thinking 回放 | **做**：`include:["reasoning.encrypted_content"]` + `Item::Reasoning` 回放；缺 encrypted 时**跳过 + warn**（与 `openai_official` 硬 `Err` 有意不同——`responses` 还服务 OpenAI 兼容网关） | async-openai 0.41.3 已具备 `ReasoningItem.encrypted_content`/`Item::Reasoning`/`IncludeEnum::ReasoningEncryptedContent`；同 crate `openai_official/request.rs:605` 有可复用范式 |
| COS-08 | **不迁 harness，维持 crate 设计决定**，仅在 §4.2 登记为可选项 | ①COS 委托的 harness 默认 guard 是进程内可重入计数，`try_enter` 永不失败，阻塞语义对 COS 零收益；②harness `SandboxGuard::try_enter` 契约是 fail-closed 非阻塞（`sandbox/guard.rs:38-47`），改阻塞波及 8 个存储 crate；③对象存储无租约原语，真跨进程互斥需条件 PUT + 过期时间（产品决策） |
| SKILLS-03 TLS | **不动根 `[workspace.dependencies]`**：`mysql-tls` 复用 `mysql_async` 既有 `rustls-tls/tls12/ring`；`postgres-tls` 在 crate 内实现 `MakeTlsConnect`（`tokio-rustls 0.26` + 平台证书/webpki 回退） | 依赖已在 lock 与本地 cache，零新下载；`default-features=false` 只编 `ring`，不引入 `aws-lc-sys` |

#### 关键交叉修复

- **CORE-29 `ReasoningContext` 丢 metadata**（model 路在实施 MODEL-10/18 时发现的真实缺陷）：`thinking: String` → `Option<ThinkingBlock>`，按 key 合并、空 map 不清空。修前 MODEL-10/18 的回放元数据在**流式路径**（ACP 即走流式）到不了持久化 `Msg`。
- **CORE-30 会话历史通道**：为不击穿 12 处 `ToolExecutionContext { .. }` 字面量构造，**不加结构体字段**，改由 `tool_runtime` 写 `runtime` 键 `conversationHistory`（尾部 32 条、空历史不写键）+ 类型化只读器 `conversation_history()`。
- **harness `shell_execute_tool` 的 `metadata =` 覆写 bug**：填 `executionDetails` 时把既有 metadata 整块覆盖，已改为合并。
- **`ROW_FORMAT` 争议定论**：Java `MysqlDialect.java:157-163` 无该子句、`JdbcRemoteSnapshotClient` 有 → jdbc 与 mysql 各对齐各自 Java 对端，**不存在需保平的偏差**。

#### 跨模块需求处置

- **已解决**：CORE-07 工具侧填充、MODEL-17（core 扩字段）、MODEL-10/18 的 metadata 通路（CORE-29）、RAG-03 会话历史（CORE-30）、SANDBOX-12（`LayoutSpec`/`bind_mounts`）、HARNESS-02（core failover 已接）、HARNESS-12（core `stop_on_reject` 已接）。
- **仍开放**：`ToolSearchMiddleware` 的宿主装配（Java harness 无对应装配点，不接线，宿主可经 `builder.middleware(...)` 自行注册）；MODEL-18 流式 `signature_delta` 未捕获；HIGRESS 原始 schema（本 crate 已有 wrapper 绕过）；COS-08（见上表）。

#### 反作弊审计（Round 5）

- 源代码**零新增** `#[ignore]` / `todo!()` / `unimplemented!()`。
- 断言净增 **+142 / −7**；新增 `#[test]` **48**（表驱动用例按一条计）。
- 各路均附「回退了什么 → 哪条用例 FAILED → 已恢复」，抽样复核有效。
- 范围体检：改动的 `Cargo.toml` 仅 `crates/agentscope-extensions-skills/Cargo.toml`（crate 内具体版本 + opt-in feature）；`Cargo.lock` 仅 +7 行依赖列表、零新 package、零版本漂移；根 `Cargo.toml` 未动。

### 3.3.10 Round 6 执行结果（2026-11-01，2 路并行：CORE+HARNESS 合路 + MODEL）

**范围**：经 2026-11-01 裁决确认执行的两项（CORE-09 真闭合、MODEL-18 流式收尾）；其余四项按裁定归档（见下）。

| 路 | 关闭缺口 | 实测 |
|---|---|---|
| core+harness | CORE-09（真闭合）：`task_requirements_enabled` 默认 `false` + 构建期条件注册 `RequirementProposeTool`；harness 默认按 `ContextSources::task_context().requirement_proposals` 推导，`task_requirements(bool)` 为显式覆盖 | core 14 targets / **546**（+3）/ 3 ignored；harness lib **603**（+1） |
| model | MODEL-18（流式）：`signature_delta` 按 `content_block` index 累积拼接（不覆盖），落 `METADATA_THINKING_SIGNATURE`；每片以「空文本 + 累计签名」thinking 增量 + core last-wins 合并得完整签名 | **471**（+4） |

#### 全量闸门（Round 6）

**177 targets / 6157 passed / 0 failed / 7 ignored / EXITCODE=0**，无 `^failures:` 段；较 Round 5 **+8 passed**（targets / ignored 不变）。

#### 关键修复与纠偏

- **CORE-09 端到端用例抓到真实缺陷**：Round 5 把 `Arc<Mutex<AgentState>>` 注入工具，但 agent 每迭代用主干 `tasks_context` 快照回写 `AgentState` → 工具写入的提案在下一迭代被覆盖丢失。改为注入共享 task bag `Arc<Mutex<TaskContextState>>`（`tool/requirement.rs`），即 agent 循环真正读写并持久化的对象；用例 `a_proposal_made_during_a_call_survives_into_the_agents_state` 锁定。
- **brief 纠偏**：`ContextSources::task_context()`（`context/source.rs:182`）+ `TaskContextOptions::allow_requirement_proposals()`（`policy.rs:177`）即 Java `contextSources.taskContext().requirementProposals()` 的等价物 → 默认路径按 Java 自动推导，`task_requirements(bool)` 降级为显式覆盖，不引入 Rust-only 语义。

#### 归档裁定（2026-11-01）

| 项 | 结论 | 依据 |
|---|---|---|
| `ToolSearchMiddleware` 宿主装配 | **不接线**（维持 Round 5 决定；宿主可 `builder.middleware(...)` 自注册） | Java harness 模块无对应装配点（全仓只见 `agentscope-examples/.../ToolSearchExample.java` 与 core 测试），`HarnessAgent.Builder` 无旋钮 |
| JDK SPI 方言自动注册 | **已完成（设计决定，不迁移）**，与 JDBC tracker §3.4 口径统一 | Rust 无运行时类路径扫描；第三方方言经 `detect_dialect` / `JdbcDialectBuilder` / 构造参数 `Arc<dyn AbstractJdbcDialect>` 显式注入已覆盖需求，残余仅「零配置自动发现」 |
| SKILLS P1-09 测试覆盖 | **关为基线记录**（93，不与 Java 192 追数） | Rust 以表驱动 + 集成用例为主，粒度不同，数字不可比 |
| HIGRESS 原始 schema | **不改 harness** | `rust-mcp-sdk` 的 `ToolInputSchema` 为封闭类型，未建模根级关键字在反序列化即丢，harness 侧无原始 JSON 可取（非取舍问题）；需要者各自旁路 |
| AP wire 双键（snake_case + camelCase） | **保留，不设淘汰期** | 成本 0，同时兼容 Java 端与 Rust 端消费者 |
| CORE-30（历史 32 条上限）/ MODEL-10（缺 encrypted 软跳过） | **保留（有意偏离）** | 均已文档化，差异理由明确 |

#### 记账修正

- CORE-10 / CORE-12 由「待办」改「已完成（跨 crate 落地）」：harness `agent/context/context_middleware.rs:30/32` 发 `context_build`（含拒绝分支 `status:"failed"` + `error_type` + manifest），`tool/agent_spawn.rs:65/951` 发 `SubagentExposed`。
- 全 tracker 现已无 `待办` / `进行中` / `待裁定` 行，扩展模块迁移的编号缺口清零。

#### 反作弊审计（Round 6）

- 源代码零新增 `#[ignore]` / `todo!()` / `unimplemented!()`。
- 断言净增 **+46 / −4**；新增 `#[test]` **8**。
- 根 `Cargo.toml` / `Cargo.lock` 未动；源码改动 5 文件 + 6 tracker。
- 三处反向验证（core 条件注册 / harness 透传 / model 签名拼接）均先 FAILED 后恢复全绿。

## 4. 状态与下一步

### 4.1 验证命令与实测（2026-10-31，stable **1.97.1**）

```powershell
cargo test -p <crate> -j 2 --no-fail-fast                 # 逐 crate（本表数字来源）
cargo test --workspace -j 2 --no-fail-fast                # 全量，无 skip
```

- **全量（Round 6 收口实测，2026-11-01）**：**177 targets / 6157 passed / 0 failed / 7 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 5 **+8 passed**（targets / ignored 不变）。
- **全量（Round 5 收口实测，2026-11-01）**：**177 targets / 6149 passed / 0 failed / 7 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 4 **+83 passed**（targets / ignored 不变）。
- **全量（Round 4 收口实测，2026-11-01）**：**177 targets / 6066 passed / 0 failed / 7 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 3 基线 **+4 targets / +176 passed**，ignored +1（harness 新增 `context/conventions.rs` 的 ```ignore 文档示例，非 `#[ignore]`）。
- **全量（Round 3 收口实测，2026-11-01）**：**173 targets / 5890 passed / 0 failed / 6 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 2 基线 **+76 passed**，targets 与 ignored 未增（Wave C 只在既有 target 内加用例）。
- **全量（Round 2 收口实测，2026-11-01）**：**173 targets / 5814 passed / 0 failed / 6 ignored / EXITCODE=0**（无 `^failures:` 段）——较 Round 1 基线 **+15 targets / +162 passed**，ignored 未增。
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

1. **~~P0 清零（30 条）~~ 已完成（2026-10-31 Round 1，见 §3.3.5）**；**~~P1 解锁项 + P2 长尾一轮~~ 已完成（2026-11-01 Round 2，见 §3.3.6）**：Wave A（`SandboxGuard::try_enter -> Result` fail-closed）解锁 5 个存储模块，Wave B 20 路并行关闭 63 条 P1/P2，全量 173 targets / 5814 passed / 0 failed。**~~Wave C（§3.3.6 列出的 10 条跨模块需求）~~ 已完成（2026-11-01 Round 3，见 §3.3.7）**：Phase 1 五路落地缝、Phase 2 八路下游接线，全量 173 targets / 5890 passed / 0 failed。下一步：按 §3.3.7 结尾「Round 4 候选」清单（即 §3.3.6 剩余 P1/P2 顺延）逐模块推进。
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
