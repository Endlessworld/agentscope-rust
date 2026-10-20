# Java 近期提交迁移 — 未处理缺口跟踪（Gap Tracker）

> **用途**：本文件是「Java 近 30 天提交 → Rust 迁移」**剩余未处理项**的单一事实来源。
> **不含已完成项**：已落地/已修复/已裁定不迁移的条目**一律不再列出**，避免 tracker 腐化。追溯历史请查 `git log -- docs/java-recent-commits-migration-analysis.md` 或对应模块的 `docs/extensions/*__GAP_TRACKER.md`。
> **来源**：2026-10-19 由 10 个只读子智能体并行分析 Java 仓库近 30 天提交（113 个，聚为 G1–G10 组）；2026-10-20 重审并逐项 grep 核实后精简成本表。
> **Java 仓库**：`E:\local-github\agentscope-java`（`main`，分析时 HEAD `5f7f8e6cf`）
> **Rust 仓库**：`E:\local-github\agentscope-rust`
> **分报告**：`.agentscope/workspace/java-migration/`（11 份，逐提交明细；仅作背景，**排期以本文件为准**）

## 0. 如何阅读本文件

- **状态**取值：`待办` / `进行中` / `已阻塞（待裁定）`。
- **优先级**：P0=会造成静默数据错误；P1=下游可见的行为偏差；P2=能力/语义补齐；P3=内部重构，无行为收益。
- **证据列**必须可 `rg` 复现。行号会随代码漂移，**实施前必须重新定位**，不得直接照抄。
- 开工前：读本文件 + `git status` + 跑目标 crate 基线。
- 完工后：从本文件**整行删除**该条目（不是改成「已完成」），并在对应模块 tracker 记一行。
- **编号会不连续**：2026-10-20 已关闭 GAP-01…GAP-13、GAP-15、GAP-16；仍开放 GAP-14、GAP-17…GAP-19 + OPS-01。剩余条目**保留原编号**，以免外部引用失效。
- **反向验证强制**：每个条目都要先有失败测试，再改实现。既有教训——`grep` 零命中不等于未实现（e2b `/files`、Redis `KeyLayoutVersion` 都曾误判），须用 2–3 个同义词交叉验证。

## 1. 缺口清单

### 1.1 P2 — 语义/能力补齐

| ID | 缺口 | Java 参考 | Rust 现状（证据） | 验收标准 | 状态 |
|---|---|---|---|---|---|
| GAP-14 | RAG 无 Tika/PDF/Word 解析 | G8 `815ed50bd` | `extensions-rag/src/reader.rs:6` 仅注释说明留空 | 需新依赖（liteparse 或等价），**另行立项**，本表只登记 | 已阻塞（需新依赖） |

### 1.2 架构债 — 会让「能力声明」说谎

| ID | 缺口 | Java 参考 | Rust 现状（证据） | 验收标准 | 状态 |
|---|---|---|---|---|---|

### 1.3 跨模块 — 需上层裁定后才能动

| ID | 缺口 | Java 参考 | Rust 现状（证据） | 裁定问题 | 状态 |
|---|---|---|---|---|---|
| GAP-17 | AG-UI 工具注入窗口未走 core `ToolRequestConfig`，协议 crate 另有一套语义 | G3 `3c1c29c01` | core 已有 `tool/tool_request_config.rs`（`ToolMergeMode{ExternalOnly,AgentOnly,MergeExternalPriority}`）；协议侧 `agui/model.rs:537` 仍是 `ToolMergeMode{FrontendOnly,AgentOnly,MergeFrontendPriority}`，字段语义是 frontend 而非 external | 是否把协议枚举收敛为 core 唯一类型的投影？（禁止再造第二份平行类型的教训） | 已阻塞（待裁定） |
| GAP-18 | core 无 tracer seam，OTel span 无处挂 | G3 `9b611928e` | core 无 `Tracer`；`extensions-studio/src/telemetry/mod.rs:13` 明确「`opentelemetry` 依赖被刻意排除」，只到 `Span` 抽象 | tracer 归 core 还是 studio？与 STUDIO telemetry 一并定 | 已阻塞（待裁定） |
| GAP-19 | `PlatformChannel` 无取消缝：lease 丢失只能抑制发送，无法中止在跑的一轮 | §A.4 | weixin 侧已实现 lease 与抑制发送 | 是否引入等价 `Sinks.Empty` 的中止通道？ | 已阻塞（待裁定） |

### 1.4 运维/部署（非代码）

| ID | 事项 | 影响 | 状态 |
|---|---|---|---|
| OPS-01 | 已部署 MySQL 库需手工 `ALTER TABLE ... MODIFY <key> VARCHAR(...) COLLATE utf8mb4_bin` | 代码侧三张表已改为二进制排序（原 GAP-03），但**存量库不会自动变**；已写入大小写变体键的库**已丢失其中一行**，需低峰执行 | 待办 |

## 2. 建议顺序

1. **OPS-01**：代码已就绪，只剩运维执行（存量 MySQL 库手工 ALTER）。
2. **GAP-17/18/19**：三项都需先裁定方向（枚举收敛 / tracer 归属 / 取消缝），未裁定前不动手。
3. **GAP-14**：RAG 文档解析需新依赖，另行立项。

## 3. 执行约定

- 改动前确认目标 crate 基线绿；改动后跑该 crate 全量测试（非仅新测试）。
- 新增测试必须能**在回退实现后 FAILED**，否则不算验证。
- 跨 crate 改动前先在对应模块 tracker §4.2 登记，避免重复裁定。
- 本文件只增删条目，不写实施日志。实施细节写进 commit message 与模块 tracker。

## 5. 已知既有失败（不属本表条目，待裁定）

| 测试 | 现象 | 判断 |
|---|---|---|
| `harness_tool_test::filesystem_tools_registered_and_dispatch` | 断言 `### /a.txt`，实际输出 ` /a.txt Written` | 工作区中 `crates/agentscope-harness/src/tool/filesystem_tools.rs` 已把 write/edit 输出改为 `<path> Written` 形式（git diff 可见），测试断言未同步。实现与测试不一致，**非 GAP-15/16/07 引入**，需裁定改哪一侧 |
| `acp_stdio_thought_test::stdio_agent_streams_thinking_to_client` | 只收到 `MESSAGE:4`，无 `THOUGHT` 块 | 该测试文档自述需要真实模型服务（`CODING_MODEL_ID`/`OPENAI_API_KEY`/`OPENAI_BASE_URL`）；无凭据时不起 thinking 流。环境依赖测试 |

## 6. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-10-20 | 精简为纯缺口跟踪：移除全部已落地/已完成/不迁移/已等价条目（原 §A/§B 附录、P0/P1 完成表、§6/§7 快照），只保留经 grep 复核的未处理项；重编为 GAP-01…GAP-19 + OPS-01/02 |
| 2026-10-20 | 并行实施后关闭并删除 GAP-01…GAP-06、GAP-09…GAP-13（JDBC 版本原子性、幽灵 CAS、MySQL COLLATE、`Interrupted` 映射、重试守卫、agent id 规范化、AG-UI resume `reason`、`TaskVerification` 回放、e2b 快照删除/保留、NUL 嗅探窗口、`RemoteConfirmDecision.reason`）；OPS-02 随之删除（GAP-02 改用自愈方案，未改哨兵值，无需数据迁移） |
| 2026-10-20 | 关闭并删除 GAP-08（`TOOL_PROGRESS` 独立事件；AG-UI 侧按 Java `ToolResultEventConverter` 的既定行为**有意丢弃**，不新增 AG-UI 事件）。验收基线：core lib 246 / jdbc 154 / protocol 308 / sandbox 137 / harness 411 全绿；`cargo check --workspace --all-targets` 通过；GAP-04/08/12 均已做反向验证（回退实现→对应测试 FAILED）|
| 2026-10-20 | 关闭并删除 GAP-16（用户裁定「接线」）：`acting_phase` 在 `OnActing` 有参与者时改走 `MiddlewareChain::wrap_acting`，core 在 spawn 的任务里产出事件流、结果写入新增的 `ActingResultsHandle`；`AsyncToolMiddleware` 超时分支把占位结果写进该句柄（Rust 等价于 Java 直接写 `AgentState`）。无参与者时保留原单通路径。新增测试：`acting_middleware_is_invoked_and_the_tool_still_runs_once`、`acting_middleware_that_completes_without_results_is_a_diagnosable_error`（core）+ `async_tool_offsloads_slow_tool_to_background` 的句柄断言（harness）|
| 2026-10-20 | 关闭并删除 GAP-07（用户裁定「对齐 Java」）：MCP `_meta` 默认传播（`io.agentscope/toolCallId`）；双开关逻辑 AND —— 工具级（注册时固定，`McpTool::with_propagate_meta`）+ 连接级（`McpServerConfig.propagate_meta`，每次调用实时读，`set_propagate_meta` 可热关）；关闭时 `_meta` 整体省略；注册级 `McpMetaPropagationOptions` 支持 client-wide 默认 + per-tool 覆盖，未匹配任何已桥接工具的名字按稳定排序 fail-loud。**已知边界**：Java 的「用户 `McpMeta` 条目」未实现（Rust 无 typed context-key 机制），元数据仅含 toolCallId。测试：`harness_mcp_meta_test.rs` 6 项（含 Node fixture 服务器回显 `_meta` 的端到端断言）+ core 4 项 |
| 2026-10-20 | 关闭并删除 GAP-15（P3）：新增 `dialect/table_schema.rs`（`parse_create_table_columns` + `validate_table_schema`），方言新增 `table_columns_query`（ANSI/H2/PG/MySQL 走 `information_schema`，SQLite 走 `pragma_table_info(?)`），三处 `init_schema` 建表后校验 —— 缺表/缺列 fail-fast，错误含表名、缺失列、实际列、参考 DDL，并声明「框架不会 ALTER 已有表」。**未做**：Java 的 `createTableDdls()` 返回 `Map<table, List<ddl>>` 分组重构（Rust 保留 `Vec<String>`，避免 33 处调用点破坏性改动）。验收基线：core 251 / jdbc 159 / harness lib 411 全绿；`harness_tool_test::filesystem_tools_registered_and_dispatch` 与 `acp_stdio_thought_test` 为既有失败（见下方说明），非本轮引入 |
