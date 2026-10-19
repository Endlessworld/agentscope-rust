# Java 近期提交迁移分析总报告

> 生成时间：2026-10-19　分析方式：10 个只读子智能体并行分析 Java 仓库近 30 天提交
> Java 仓库：`E:\local-github\agentscope-java`（分支 `main`，HEAD `5f7f8e6cf`）
> Rust 仓库：`E:\local-github\agentscope-rust`
> 分报告目录：`.agentscope/workspace/java-migration/`（11 份，逐提交明细）

## 0. 本报告阅读指引

- **逐提交明细**（变更摘要 / Java 涉及文件 / Rust 对应位置 / 可迁移性 / 实施步骤 / 影响范围 / 风险依赖）在**分报告**中，本报告只做跨组汇总、优先级排序与路线图。
- 可迁移性四档口径：
  - ✅ **可迁移**：Rust 存在明确落点，可 1:1 或近似 1:1 移植。
  - ⚠️ **部分迁移**：语义可迁移但缺前置 seam / 需先修依赖 / 需上层裁定 / 触碰禁改区。
  - ❌ **不可迁移**：JVM 特有（Spring Boot starter、Reactor 语义）或 Rust 侧前提不存在。
  - ✔️ **已完成**：Rust 侧已等价实现或天然满足，无需动作。
- **重要前提说明**：本报告全部结论来自只读静态分析（`git show` + `rg`）。**凡标 ✅/⚠️ 者在实施前必须按既有教训做「反向验证」**：先写/找失败测试 → 改实现 → 确认 FAILED 转 PASSED。
- ⚠️ 分析中发现多处「Rust 侧存在同形缺陷」与「Rust 侧语义已等价但无测试固化」两类情况，均已在下文区分标注。

## 1. 分析范围与方法

- 范围：Java 仓库**近 30 天**（2026-09-18 ~ 2026-10-14）共 **113 个提交**。
- 分组：按变更主题聚为 11 个提交组（G1–G10，其中 G6 因体量拆为 G6a/G6b），每组由 1 个只读子智能体独立分析（仅允许写自己的分报告，禁改两仓任何文件）。
- 输出：每组一份分报告（编号 01–10 + 06a/06b），逐提交给出上述七字段；本报告为跨组汇总。
- 分组说明：G9 与 G10 与其它组在个别提交上有重叠（如 `35cbbfcf9`、`c0d03ccea`、`3d1b998d7`、`146086684`），故**分组计数之和 > 113**，属预期。

## 2. 提交分布

按 Java 模块（顶层目录）聚类，提交密度最高的方向：

| 模块 | 提交数（约） | 主要主题 |
|---|---|---|
| `agentscope-core` | 30+ | 结构化输出校验、returnDirect、runtime context/runId、tool progress、HITL、MCP 标题、重试 |
| `agentscope-extensions`（各子模块） | 45+ | model provider（openai/anthropic/gemini/ollama/dashscope）、sandbox(e2b)、rag、protocol(agui)、channel(weixin)、jdbc/mysql/pg/redis |
| `agentscope-harness` | 20+ | session journal、compaction 量化、pruning 保留字段、skill staging、patch CRLF、文件上传、ConflictPolicy |
| `agentscope-service` / CI / docs | 20+ | service session 重构、CI 提速、依赖升级、文档治理 |

## 3. 分组总览（可迁移性计数）

| 组 | 分报告 | 提交数 | ✅ | ⚠️ | ❌ | ✔️ | 主题 |
|---|---|---|---|---|---|---|---|
| G1 | 01-core-structured-output.md | 8 | 2 | 4 | 2 | 0 | core 结构化输出 / 生成结果 / returnDirect / MCP 标题 |
| G2 | 02-core-reliability.md | 9 | 1 | 6 | 0 | 2 | core 重试 / 状态持久化 / HITL / runtime context |
| G3 | 03-tools-mcp-middleware.md | 8 | 1 | 6 | 0 | 1 | tools / MCP meta / middleware 扩展点 / skill 过滤 |
| G4 | 04-model-providers-a.md | 10 | 4 | 4 | 2 | 0 | provider A：openai/dashscope/anthropic/gemini |
| G5 | 05-model-providers-b.md | 8 | 3 | 3 | 2 | 0 | provider B：deepseek/ollama/responses/usage |
| G6a | 06a-harness-session.md | 7 | 2 | 3 | 2 | 0 | harness session / compaction / skill staging |
| G6b | 06b-harness-misc.md | 9 | 5 | 1 | 1 | 2 | harness pruning / patch CRLF / workspace |
| G7 | 07-jdbc-db.md | 9 | 2 | 5 | 1 | 1 | JDBC / MySQL / PostgreSQL / Redis |
| G8 | 08-sandbox-rag.md | 9 | 4 | 3 | 1 | 1 | sandbox(e2b/k8s/docker) + RAG |
| G9 | 09-extensions-protocol-channel.md | 7 | 2 | 3 | 1 | 1 | protocol(agui) / channel(weixin) / jev |
| G10 | 10-service-ci-deps-docs.md | 26 | 2 | 4 | 12 | 0 | service / CI / 依赖升级 / 文档 |
| **合计** | | **102（含重叠）** | **28** | **42** | **24** | **8** | |

**一句话结论**：近 30 天 Java 侧改动约 **27%（28/102）可直接迁移**、**41% 需适配或裁定**、**24% 不可迁移**（CI/文档/JVM 特有）、**8% 已完成**。





## 4. 优先级路线图

按「实施风险 × 收益」排序（不按提交时间）。

### P0 —— Rust 侧存在同形缺陷或明确行为偏离（建议优先修）

| 编号 | 提交 | 主题 | 落点 | 类型 |
|---|---|---|---|---|
| P0-1 | G7 `7d10efc3b` | PostgreSQL 侧残余路径分隔符守卫，阻塞沙箱 resume 落盘 | `extensions-postgresql/src/state.rs:161-165` | 真缺陷（同形） |
| P0-2 | G7 `2349533a1` | MySQL 键列缺 `COLLATE utf8mb4_bin` → 大小写不敏感静默丢数据 | `extensions-mysql` 建表 DDL | 真缺陷（同形） |
| P0-3 | G8 `c767160b1` | K8s 命令包装换行（`cd {} && ({command})` 吃掉 heredoc 终止符） | `extensions-sandbox/src/fs_spec.rs:380` + 单测 `:913` | 真缺陷（同形） |
| P0-4 | G1 | fallback 结构化提醒被**持久化**进上下文（Java 要求请求本地） | `agentscope-core` 结构化输出降级路径 | 行为偏离 |
| P0-5 | G1 `c2ff087ca` | fallback 重建回复**丢 id/timestamp/usage**（正是 Java 修掉的 bug） | `agentscope-core` 同上 | 真缺陷（同形） |
| P0-6 | G5 `9343ff0a8` | deepseek `reasoning_content` 缺回填空串（纯文本 assistant 轮/合成消息 → 400） | `extensions-model` deepseek seam | 真缺陷（语义缺口） |
| P0-7 | G6b `44ec9e71a` | lazy skill stager 空资源仍建目录且恒报 `Cached`（应报 `None`） | `agentscope-harness/src/agent/skill/stager*` | 真缺陷（同形） |
| P0-8 | G6b `ce0d32df3` | `session_search`/`session_history` 缺 100 上限 clamp | `agentscope-harness/src/tool/session_search.rs` | 真缺陷（同形） |
| P0-9 | G6a `d87c96d38` | compaction 裁剪重建丢 `metadata`/`state`/`usage` | `agentscope-harness/src/agent/memory/compaction.rs:756-826` | 真缺陷（同形） |
| P0-10 | G6a `9776a3711` | pruning 预算按整条 Msg 计费，应按单条 tool result | 同上（复用 `estimate_tool_result_tokens`） | 口径缺陷 |
| P0-11 | G2 `a6c94649` | 挂起的外部工具调用被伪造了结果（应留 `Suspended` 不造结果） | `agentscope-core` + `extensions-protocol` 穷尽匹配点 | 真缺陷（同形） |

### P1 —— 新功能/能力补齐（明确落点，价值中等）

| 编号 | 提交 | 主题 | 落点 | 备注 |
|---|---|---|---|---|
| P1-1 | G1 `3d1b998d7` | returnDirect 工具结果短路 | core 主循环 + `GenerateReason` + protocol 投影 | 跨 crate，需 core 授权 |
| P1-2 | G1 `c95eb1f1c` | `ToolEmitter` 暴露 tool call ID | core | |
| P1-3 | G1 `a299fb1c5` | 响应侧结构化输出校验 + 错误反馈重试 | core（`jsonschema 0.30` 已就绪） | |
| P1-4 | G1 `8eb91158c`/`b6a483086` | 降级重试上限 + `ToolChoice::Specific` + strict 开关 | core + extensions-model | |
| P1-5 | G3 `b4f468fe1` | MCP 工具 display title 可达 | core | 低频低风险 |
| P1-6 | G3 `20dc107e4` | tool 成功/失败经 `ToolResultBlock.state` 上报 | core + protocol | |
| P1-7 | G3 `e9721285c`/`addf82b89` | middleware 新扩展点（`onAgentStateReady`/`activePoints`） | core `Middleware` trait + **真实装配入口** | 只加 trait 方法会复现死代码 |
| P1-8 | G4 `8485ca87c` | DashScope 分片 tool call id 不应編造 + 按 id merge | `extensions-model/src/dashscope/parser.rs:52` | |
| P1-9 | G4 `69a3c2484` | Ollama 原生结构化输出（读 `response_format`） | `extensions-model/src/ollama/model.rs:321` | |
| P1-10 | G4 `3478329bb` | DashScope `reasoning_effort` 下发 | `extensions-model/src/dashscope`（照抄 `openai/mod.rs:423`） | |
| P1-11 | G4 `10b8a99a7` | `cache_control` 下移到 content block | `extensions-model`（anthropic 已有范式） | |
| P1-12 | G5 `146086684` | provider base URL 环境变量回退（OPENAI/ANTHROPIC/DASHSCOPE/GEMINI） | `extensions-model/*/provider.rs`（照抄 `provider.rs:62`） | 单行级 |
| P1-13 | G5 `ed77530a4` | Ollama thinking 输出为 `ThinkingBlock` | `extensions-model/src/ollama` | |
| P1-14 | G9 `16632ae84`/`da3b867af` | AGUI reasoning 消息转换 + 前端 tool error 字段 | `extensions-protocol` agui | 两提交同函数族，宜合并 |
| P1-15 | G6b `234a6977a` | fuzzy matcher CRLF 行体上界 | `agentscope-harness/src/tool/fuzzy_text_matcher.rs` | 注意该文件当前无生产调用点 |
| P1-16 | G6b `325f33d6d` | 记忆/会话检索多关键词 matchMode | `agentscope-harness`（与 P0-8 同文件） | |
| P1-17 | G8 `897c4866e`/`65a35d40a`/`febc795fc` | e2b envd 流解析加固 + `/files` REST + `E2B_SANDBOX_URL` | `extensions-sandbox/src/e2b.rs` | 三连，顺序依赖 |
| P1-18 | G9 `63a6f6168` | 个人微信 iLink channel | `extensions-channel/src/weixin/` 新建 | 体量较大 |

### P2 —— 低价值 / 需裁定 / 测试完善

- `addf82b89` 的性能收益在 Rust 不成立（空链直通），只剩诊断价值。
- `d0480834e` 补并发共享 workspace E2E（纯测试）。
- `161538319` 压缩量化指标（Rust 无 session-log，只能挂 in-band 事件）。
- `62f4b4281` 严格 UTF-8 回退（只能先修 `local.rs::read` 的 NUL 启发式）。
- `915507134`/`a3798a8f5` 等已天然等价，仅需补断言。

## 5. P0 项实施步骤与影响范围

> 以下均为「Rust 侧已确认存在同形缺陷或行为偏离」，建议优先修复。步骤为分析结论，实施前须反向验证。

### P0-1 PostgreSQL 路径分隔符守卫（7d10efc3b）

- 实施步骤：(1) 定位 extensions-postgresql/src/state.rs:161-165 对 slot id 的路径分隔符拒绝分支；(2) 对齐 Java：允许沙箱型 slot id 含路径分隔符（仅做规范化，不拒绝）；(3) 补单测：含斜杠的 slot id 做 save/load 往返成功。
- 影响范围：crates/agentscope-extensions-postgresql/src/state.rs（+同文件测试）。
- 风险：低（纯分支改动）；需确认 JdbcAgentStateStore 共用逻辑是否也需同步。

### P0-2 MySQL 键列 COLLATE（2349533a1）

- 实施步骤：(1) 定位 extensions-mysql 建表 DDL 的 key 列定义；(2) 为 key 列加 COLLATE utf8mb4_bin；(3) 补单测断言 DDL 字符串含该 COLLATE。
- 影响范围：crates/agentscope-extensions-mysql（建表/dialect 模块 + 测试）。
- 风险：低；但已部署库需迁移（重建表/改列），需在 tracker 记录运维提示。

### P0-3 K8s 命令包装换行（c767160b1）

- 实施步骤：(1) 改 extensions-sandbox/src/fs_spec.rs:380 的命令包装，在 command 前后加换行，避免 heredoc 终止符被拼上右括号；(2) 同步更新单测 fs_spec.rs:913；(3) 加回归用例。
- 影响范围：crates/agentscope-extensions-sandbox/src/fs_spec.rs（+同文件测试）。
- 风险：低。

### P0-4 / P0-5 core 结构化降级路径（G1）

- 实施步骤：(1) call_fallback_structured 的 structured_output_reminder 改为请求本地（仅拼入本次请求 messages，不落盘到 state.context）；(2) fallback 重建回复时从原 Msg 拷回 id/timestamp/usage；(3) 补单测断言提醒不入 state.context、重建回复字段与原回复一致。
- 影响范围：crates/agentscope-core/src（结构化输出降级 + 测试）。
- 风险：低（局部）；无新增依赖（jsonschema 0.30 已就绪）。

### P0-6 deepseek reasoning_content 空串回填（9343ff0a8）

- 实施步骤：(1) 定位 extensions-model deepseek 的 assistant 消息回传逻辑；(2) 把「有 Thinking 才写」改为「缺则写空串」，覆盖纯文本 assistant 轮与合成 assistant 消息；(3) 补离线单测：无 thinking 的 assistant 轮序列化后 reasoning_content 为空串。
- 影响范围：crates/agentscope-extensions-model/src/deepseek（+测试）。
- 风险：中；需与 core 现有 reasoning_content 通道（extra_body merge）协同，不可用 additional_body_params。

### P0-7 skill stager 空资源语义（44ec9e71a）

- 实施步骤：(1) agent/skill/stager 在无资源入参时直接返回 StageResult::None，不建目录；(2) 补单测。
- 影响范围：crates/agentscope-harness/src/agent/skill。
- 风险：harness 禁改区，需主智能体授权。

### P0-8 session 工具上限 clamp（ce0d32df3）

- 实施步骤：(1) 在 tool/session_search.rs 的 limit 解析处 clamp 到 100（同时给 memory_search 补 Java 已有的 limit clamp）；(2) 补单测：传入 1000 则实际生效 100。
- 影响范围：crates/agentscope-harness/src/tool/session_search.rs（+ memory 工具）。
- 风险：行位禁改区，需授权；建议与 P1-16（325f33d6d）同批。

### P0-9 compaction 裁剪保留字段（d87c96d38）

- 实施步骤：(1) agent/memory/compaction.rs:756-826 处 ToolResultBlock 重建补 metadata 与 state；(2) Msg 重建后补 usage；(3) 补镜像用例（metadata/state/usage 存活；多块只换超大块；excluded_tools 整体返回；低于 minimum_tokens 原集合）；(4) 反向验证：注释掉 metadata 赋值后用例必须 FAILED。
- 影响范围：crates/agentscope-harness/src/agent/memory/compaction.rs + tests/harness_memory_test.rs。
- 风险：行位禁改区，需授权；勿顺手扩大范围（truncate_args 两处同题但 Java 声明不在本次范围）。

### P0-10 pruning 预算按单条 result 计费（9776a3711）

- 实施步骤：(1) 改 compaction.rs 中 prune_tool_results 的预算累加，由整条 Msg 改为单条 tool result（复用已有 estimate_tool_result_tokens）；(2) 补单测。
- 影响范围：crates/agentscope-harness/src/agent/memory/compaction.rs。
- 风险：行位禁改区，需授权。

### P0-11 挂起工具不伪造结果（a6c94649）

- 实施步骤：(1) core 新增 ToolResultState::Suspended + METADATA_SUSPEND_REASON；(2) 挂起的外部工具调用不生成伪造 tool result；(3) 同步下游 extensions-protocol 的 ToolResultState/AgentEvent 穷尽 match 点；(4) 补单测。
- 影响范围：crates/agentscope-core + crates/agentscope-extensions-protocol。
- 风险：中（跨 crate 穷尽匹配）；需 core 授权。

## 6. 部分迁移 / 需裁定项

这类项「语义可迁移」但存在前置障碍，需上层先裁定。

| 提交 | 障碍类型 | 说明 | 建议 |
|---|---|---|---|
| G1 a299fb1c5 | 缺实现 | Rust 无响应侧 schema 校验 | 归入 P1-3 |
| G1 8eb91158c | 需扩 trait | 需 Model trait 增 supports_tool_choice_specific() | 归入 P1-4 |
| G1 b6a483086 | 跨 crate | strict 开关需 core + extensions-model | 归入 P1-4 |
| G2 707400eb | 未接线 | ToolEmitter 完全未接线、ToolExecutionContext.emitter 三处均为 None | 需先接线再迁 ToolProgress |
| G2 8d9bec68 / ea78c317 | 架构缺口 | Rust 无 AgentRun/RunControl/run_id；InterruptControl 仍 agent 级单例 | 需裁定是否引入 run 级控制 |
| G2 7d08f8fd | 待裁定 | core AgentStateStore 无版本化/CAS（bug 当前不可复现） | 纳入「版本化是否上提 core」裁定 |
| G2 860b9287 | 部分 | 无状态码重试已等价；已发射可见内容后禁止流式重试未实现 | 中优先级 |
| G3 3c1c29c01 | 跨 crate | ToolRequestConfig / 请求视图隔离 | 记 tracker 4.2 待裁 |
| G3 9b611928e | 依赖裁定 | tracer seam（app-owned OpenTelemetry SDK） | 与 STUDIO telemetry 一并裁定 |
| G3 35cbbfcf9 | 禁改区 | MCP meta 传播开关落 core/harness | fail-open vs fail-closed 需显式裁定 |
| G4 c0d69debd | 缺能力位 | 需 common/capabilities.rs 增 supports_tool_choice | 低优先级 |
| G4 c8ea6029a | 跨模块 | DeepSeek 上下文窗口表在 core（受限） | 记跨模块需求 |
| G4 f98a685fd / 857e3e6cf | core 语义阻塞 | server tools 本体依赖 core 的 is_server_tool / 累加器 | 需 core 立项 |
| G5 67dbbd777 | 需改 core | usage 三字段须扩 core ChatUsage | 与 G5 defer_loading 合并立项 |
| G5 9084fabe5 / a1f0b45f1 | 需改 core | server tools 主体可做，defer_loading 须改 core ToolSchema/Tool | 同上 |
| G6a 161538319 | 无对应物 | Rust 无 session-log，量化字段只能挂 in-band compaction 事件（二态） | 裁剪适配 |
| G6a 62f4b4281 | 缺 API 层 | 无 upload/download 面，只能先修 local.rs::read 的 NUL 启发式 | 先做局部 |
| G6b e05d45e08 | 跨模块阻塞 | ConflictPolicy 语义需 core AgentStateStore 版本化 | 同上裁定 |
| G7 d786d8fb5 / 7854f0de7 | 缺事务抽象 | CAS 版本语义；Rust 无 executeInWriteTransaction 等价物 | 合并评估（P0/CAS） |
| G7 9c8b10d4e | 破坏性重构 | create_table_ddls 返回类型 + 新增 TableSchemaValidator | 需评估兼容性 |
| G7 48f7257ae | Hard前提 | Redis hash tag（Cluster） | 需部署形态确认 |
| G7 768d9613d | 归属他 crate | skill namespace 属 skills crate | 记跨模块 |
| G8 ec9446023 | 需新增 hook | RemoteBackend 无文件传输 hook | 依赖 P1-17 |
| G8 a0e89a3aa | 禁改区 | SandboxLifecycleMiddleware.releaseForCall() 顺序修复在 harness | 记 SANDBOX tracker 4.2 |
| G8 815ed50bd | 新依赖 | Tika 等价依赖缺失 | 另立项 |
| G9 c0d03ccea | 小缺口 | core 已等价；protocol 两处小缺口 | 低优先级 |
| G10 a2cbea5c3 | 方向性裁定 | Java 新 session/context 设计与 Rust 现状相反（详见下） | **需产品级裁定** |
| G10 2ef3fd2e0 | 部分 | 并发守卫 + agentId | 部分已等价 |
| G10 00eb68685 | 缺口暴露 | plan-mode 白名单：Rust PlanModeMiddleware 只注入 prompt，无白名单/拒绝 | 待评估 |

### 6.1 方向性冲突：session/context 重构（a2cbea5c3）

- Java 删除了 SessionTree / SessionEntry / TranscriptMiddleware / transcript 整套，新建 core/session 与 harness/agent/{context,session,observation,verification}。
- Rust 恰好只有被删除的那批（memory/session.rs、transcript/、middleware/transcript.rs），而没有新增的那批。
- 结论：这是设计方向的选择而非简单补齐，**建议先裁定 Rust 是否跟随 Java 新设计**，否则会造成大量无效迁移。

## 7. 不可迁移 / 设计决定（不迁移）

这些项要么是 JVM/工具链特有，要么 Rust 前提不存在。建议在汇总 tracker 记为「已完成（设计决定，不迁移）」并写明依据。

| 提交 | 类别 | 不迁移依据 |
|---|---|---|
| G1 01da9e6fb | 文档 | 纯 javadoc 排版 |
| G1 0eea19e0e | 重构 | Java 子类构造器去 role 参数；Rust 单 struct 无此根因 |
| G4 a3798a8f5 | 已等价 | Rust serde DTO 无「类型化访问器抛异常」缺陷 |
| G4 915507134 | 已等价 | 无 extract_extension；core get_extension 已含 query 剥离 |
| H5 b4ee8e324 | JVM 特有 | 无 Spring Boot 宿主 |
| G5 6e6406610 | 无对应物 | 无 DashScope embedding；Reactor 阻塞语义不适用 |
| G6a 8b06f2103 | 文档 | native journal 文档重写；Rust 仍 jsonl + sessions.json 双轨 |
| G6a e68dd783f | 文档 | 技能刷新时机文档；Rust 缓存粒度不一致属另题 |
| G6b 320822d40 | 前提不存在 | Rust 无 filesystem-spec 层、LocalFilesystem 无 per-user 前缀 |
| G7 b68a8b0a8 | 架构冲突 | skill 并入 JDBC 模块，违 Rust SKILLS 裁定 |
| G8 a4f11b6cb | 无对应物 | 无 WordReader/TableFormat，仅登记规格 |
| G9 a5be76105 | 模块缺失 | Rust 无 judge/jev crate |
| G10（12 项） | CI/文档/依赖 | CI 提速、文档治理、Java 依赖版本升级（与 Rust SDK 无直接对应） |

## 8. 跨模块阻塞与授权需求

### 8.1 需 core 修改授权（改动 core 公共类型）

1. `ChatUsage` 扩 usage 三字段（G5 67dbbd777）。
2. `ToolSchema`/`Tool` 增 defer_loading（G5 a1f0b45f1）。
3. `Tool` trait 增 is_return_direct + GenerateReason 新增变体 + 事件 metadata（G1/G10 3d1b998d7）。
4. `ToolResultState::Suspended` + METADATA_SUSPEND_REASON（G2 a6c94649）。
5. `ToolEmitter` 接线 + 暴露 tool call ID（G1 c95eb1f1c、G2 707400eb）。
6. `Middleware` trait 新增扩展点（G3 e9721285c/addf82b89）——**必须同步真实装配入口调用**。
7. Response-side schema 校验与 strict 合并（G1 a299fb1c5、b6a483086 × extensions-model）。
8. 结构化降级路径修正（G1 P0-4/P0-5）。

### 8.2 需 harness 修改授权（禁改区）

- 全部 G6a / G6b 可迁移项：stager 语义、session 上限、compaction 字段保留与计费。
- 附：分析中发现 harness 侧隐患（需复核而非直接改）：HarnessSkillMiddleware::invalidate() 零调用、缓存粒度非 per-call；plan-mode 无白名单；tool/fuzzy_text_matcher.rs 无生产调用点。

### 8.3 跨 crate / 跨模块需求（记各模块 tracker 4.2）

- MCP meta 传播开关（core/harness）——G3 35cbbfcf9。
- toolbar tracer seam 与 STUDIO telemetry 归并裁定——G3 9b611928e。
- server tools 语义（core）——G4 f98a685fd/857e3e6cf。
- DeepSeek 上下文窗口表在 core——G4 c8ea6029a。
- skill namespace 属 skills crate——G7 768d9613d。
- SandboxLifecycleMiddleware.order——G8 a0e89a3aa。
- RemoteBackend 文件传输 hook——G8 ec9446023。
- judge/jev 模块缺失；RemoteConfirmDecision.reason（harness DTO）；weixin service/console 层——G9。
- ConflictPolicy 需 core AgentStateStore 版本化——G6b e05d45e08。

### 8.4 待复核的接线疑点（本报告副产品，未证实为 bug）

- `SessionSearchTool::register_all` 在 crates 内无调用点（仅测试直接 new）→ 疑似已落盘未接线。
- `tool/fuzzy_text_matcher.rs` 无生产调用点（仅 re-export + 单测）。
- `HarnessSkillMiddleware::invalidate()` 零调用。
- 上述均需按既有教训：先 grep mod.rs/注册点，再看 git status，最后跑测试，**不可仅凭未见到调用就断言未接线**。

## 9. 建议实施顺序（批次）

**第 1 批（低风险真缺陷，无授权依赖）**：P0-1、P0-2、P0-3、P0-6、P1-8、P1-9、P1-10、P1-11、P1-12、P1-13。
→ 落点在单 crate，可并行；建议先跑全量基线确认无回归。

**第 2 批（需 harness 授权）**：P0-7、P0-8、P0-9、P0-10 + P1-15、P1-16（后两者与 P0-8 同文件）。
→ 集中在 agent/memory/compaction.rs 与 tool/session_search.rs，建议同文件一批做，减少合并冲突。

**第 3 批（需 core 授权）**：P0-4、P0-5、P0-11 + P1-1、P1-2、P1-3、P1-4、P1-5、P1-6、P1-7。
→ 改 core 公共类型，建议先写一份 core 变更立项；P0-11/P1-6 需同步 protocol 穷尽匹配点。

**第 4 批（大件/多步）**：P1-17（e2b 三连，注意顺序）、P1-18（weixin channel）、P1-14（agui 两提交合并重构）。

**暂不动**：所有需裁定的方向性项（session/context 重构）、需新依赖项（RAG 文档解析）、JVM 特有项。

## 10. 分报告索引

| 文件（.agentscope/workspace/java-migration/） | 内容 |
|---|---|
| 01-core-structured-output.md | G1 core 结构化输出/生成结果/returnDirect/MCP 标题 |
| 02-core-reliability.md | G2 core 重试/状态/HITL/runtime context |
| 03-tools-mcp-middleware.md | G3 tools/MCP/middleware 扩展点/skill 过滤 |
| 04-model-providers-a.md | G4 provider A（openai/dashscope/anthropic/gemini） |
| 05-model-providers-b.md | G5 provider B（deepseek/ollama/responses/usage） |
| 06a-harness-session.md | G6a harness session/compaction/skill staging |
| 06b-harness-misc.md | G6b harness pruning/patch/workspace |
| 07-jdbc-db.md | G7 JDBC/MySQL/PostgreSQL/Redis |
| 08-sandbox-rag.md | G8 sandbox(e2b/k8s/docker) + RAG |
| 09-extensions-protocol-channel.md | G9 protocol(agui)/channel(weixin)/jev |
| 10-service-ci-deps-docs.md | G10 service/CI/依赖/文档 |

## 11. 分析局限与免责

- 本报告为**静态分析结论**，未建/未跑 Rust 代码，**所有可迁移性判定均为待验证假设**。
- 部分「同形缺陷」由行号与代码片段对比得出，行号可能随分支漂移，实施前需重新定位。
- Java 侧变动包含大量 CI/文档/依赖升级（约 24 项），已归为不迁移，若需逐项明细见分报告 10。
- 10 个子智能体的分报告已交叉引用同一提交（重叠提交），若两处分歧以更具体的分报告为准。


---

# 附录 A：实施进展（2026-10-19 并行修改）

> 本节记录基于上述分析**实际已实施**的修改（用户显式授权覆盖 core/harness 禁改区）。改动均在工作区未提交（分支 `dev`）。
> **终局回归**：`cargo build --workspace` EXIT=0；`cargo test --workspace -j 2 --no-fail-fast` EXIT=0，**无 `failures:` 段、无 FAILED**，1 ignored（既有 `platform_channel.rs` 文档示例）。

## A.1 已完成的 P0（真缺陷修复）

| 编号 | 内容 | 落点 | 测试证据 |
|---|---|---|---|
| P0-1 | PostgreSQL 移除 slot id 路径分隔符拒绝守卫（对齐 Java 7d10efc3b） | `extensions-postgresql/src/state.rs` | PG **105 passed**（+1），反向验证 FAILED |
| P0-2 | MySQL 键列加 `COLLATE utf8mb4_bin`（对齐 2349533a1） | `extensions-mysql/src/store/dialect.rs` | MySQL **45 passed**（+2），反向验证 FAILED |
| P0-3 | K8s 命令包装换行（对齐 c767160b1） | `extensions-sandbox/src/fs_spec.rs` | sandbox **126 passed**（+30），反向验证 FAILED |
| P0-4 | 结构化降级提醒改为请求本地、不落 `state.context` | `agentscope-core/src/agent/react_agent.rs` | core 全绿 |
| P0-5 | 降级重建回复保留 `id`/`timestamp`/`usage`（对齐 c2ff087ca） | 同上 | core 全绿 |
| P0-6 | deepseek `reasoning_content` 缺则回填空串（对齐 9343ff0a8） | `extensions-model/src/openai/mod.rs` | model **250 passed** |
| P0-7 | skill stager 空资源返回 `None` 且不建目录（对齐 44ec9e71a） | `harness/src/agent/skill/stager.rs` | harness --lib **351 passed**（+11） |
| P0-8 | `session_search`/`session_history` limit clamp 到 100（对齐 ce0d32df3） | `harness/src/tool/session_search.rs` | 同上，反向验证 FAILED |
| P0-9 | compaction 裁剪保留 `metadata`/`state`/`usage`（对齐 d87c96d38） | `harness/src/agent/memory/compaction.rs` | `harness_memory_test` **25 passed** |
| P0-10 | pruning 预算按单条 tool result 计费（对齐 9776a3711） | 同上 | 同上，反向验证 FAILED |
| P0-11 | `ToolResultState::Suspended` + `METADATA_SUSPEND_REASON`；挂起不伪造结果 **且不发 ToolResultEnd**（对齐 a6c946494） | `core/src/message/block.rs`、`core/src/agent/react_agent.rs`、`extensions-protocol/src/agui/.../converters.rs` | core **100 lib + 57 integration passed** |

> P0-11 备注：子智能体最初只改了 `ToolResultBlock`，遗漏了主循环仍把挂起结果映射为 `Success` 并照发 `ToolResultEnd`。**主智能体复核 Java 提交 a6c946494 后补修**（跳过 end 事件、state 用 `SUSPENDED`），并同步修正了过时断言。同时补了 protocol 的 `SUSPENDED => Suspended` 字符串映射。

## A.2 已完成的 P1（能力补齐）

| 编号 | 内容 | 落点 |
|---|---|---|
| P1-5 | `Tool::title()` + `FunctionTool::with_title`/`McpTool` 解析 display title（对齐 b4f468fe1） | `core/src/tool/**` |
| P1-6 | `ToolResultBlock::success()`/`final_state()`；状态写入持久化上下文（对齐 20dc107e4） | `core` |
| P1-2 | `ToolEmitter` 暴露 tool call ID 并在 3 处 `ToolExecutionContext` 接线（对齐 c95eb1f1c） | `core/src/tool/emitter.rs`、`react_agent.rs` |
| P1-1 | `returnDirect` 短路 + `GenerateReason::ToolReturnDirect` + 事件 metadata（对齐 3d1b998d7） | `core`（+ aistio/protocol 穷尽匹配已同步） |
| P1-7 | `Middleware::on_agent_state_ready` + `active_points()`，**在 `run_agent_loop` 真实入口调用**（对齐 e9721285c/addf82b89） | `core/src/middleware/**` |
| P1-8/9/10/11/12/13 | dashscope 分片 id/merge、ollama 原生结构化+thinking、dashscope `reasoning_effort`、`cache_control` 下移 content block、`*_BASE_URL` 环境变量回退（新增 `common/base_url.rs`） | `extensions-model/**`（12 文件 + 1 新文件） |
| P1-14 | AG-UI reasoning 消息转换 + 前端工具 error → ERROR tool result（对齐 16632ae84/da3b867af） | `extensions-protocol/src/agui/**`（**+11 测试，301 passed**） |
| P1-15 | fuzzy matcher CRLF 行体上界（对齐 234a6977a） | `harness/src/tool/fuzzy_text_matcher.rs` |
| P1-16 | session 检索多关键词 matchMode（对齐 325f33d6d） | `harness/src/tool/session_search.rs` |
| P1-17 | e2b envd 流解析加固 + `/files` REST 上传下载 + `E2B_SANDBOX_URL`（对齐 897c4866e/65a35d40a/febc795fc） | `extensions-sandbox/src/e2b.rs` |
| P1-18 | 个人微信 iLink 渠道（对齐 63a6f6168）：新建 `weixin/` 13 文件 ≈4890 行，**+85 测试，326 passed** | `extensions-channel/src/weixin/**` |

## A.3 尚未实施（需裁定 / 需新依赖 / 体量大）

- **P1-3**（响应侧 schema 校验）、**P1-4**（strict 合并 + `ToolChoice::Specific` 能力位）——留在 core/model，未做。
- **P1-9 未覆盖的同族**：无。
- **需裁定的方向性项**：`a2cbea5c3`（session/context 重构）、`ChatUsage` 扩字段、`ToolSchema::defer_loading`。
- **需新依赖**：RAG 文档解析（Tika 等价物）。
- **不可迁移**：JVM 特有 / CI / 文档（见 §7）。

## A.4 由此产生的跨模块/运维事项（须登记）

1. `docs/extensions/CHANNEL__GAP_TRACKER.md` 的 weixin 条目未登记（子智能体禁改 `docs/**`）。
2. harness `PlatformChannel` **无取消缝**（weixin lease 丢失只能抑制发送，无法中止在跑的一轮；Java 用 `Sinks.Empty` 可中止）。
3. MySQL 已部署库需手工迁移：`ALTER TABLE ... MODIFY <key> VARCHAR(...) COLLATE utf8mb4_bin`（低峰执行；已写入大小写变体键的库已丢失其中一行）。
4. PG 仍需确认 `JdbcAgentStateStore` 共享层是否也有同类守卫（本次仅改了 `extensions-postgresql`）。
5. `GenerateReason::ToolReturnDirect` 是 core 公共枚举新增变体，**任何下游穷尽 match 都需补分支**（本次已修 aistio；未来新增适配器需注意）。
6. P0-11 的「RequireExternalExecution 事件」在 Rust core 仍未发出（Java 以它为唯一信号）；Rust 目前依赖 `GenerateReason::ToolSuspended` 反映在最终 Msg。属**残留语义差异**，建议后续立项。

## A.5 D3 核实结论：4 处「已落盘未接线」

裁决 D3 要求逐项 grep + 跑测试核实。结论：**4 处均为真实缺口**，非误报。

| # | 项 | 核实证据 | 处置 |
|---|---|---|---|
| 1 | `SessionSearchTool::register_all` | Rust 仅定义（`session_search.rs:143`），全仓无调用者；Java `HarnessAgent.java:2882` 在 `!disableMemoryTools && !legacySessionHistory` 时默认注册 | **已修**：在 `harness_agent.rs` 的 `!disableMemoryTools` 块内 `SessionSearchTool::register_all(...)`（与 Java 同条件） |
| 2 | `tool/fuzzy_text_matcher.rs` | Java 仅在 `SkillManageTool` 的 `patch`/`edit` 动作使用；Rust `skill/manage.rs` 原只实现 create/update/promote/archive/list，**无 patch/edit**，故 fuzzy 模块是孤儿 | **已修**：`skill_manage` 新增 `edit`/`patch` 动作并接线 fuzzy 阶梯（含唯一性校验、`replace_all` 右到左应用、`[fuzzy: level]` 后缀）；+4 测试 |
| 3 | `HarnessSkillMiddleware::invalidate()` | 方法存在（`skill_middleware.rs:159`）但 harness 内零调用；Rust 惰性缓存（Java 每 `onSystemPrompt` 重建）导致写入后目录陈旧 | **已修**：`SkillManageTool::with_on_write(cb)` + 装配处 `skill_mw_slot` 延迟绑定，写入即失效缓存；+1 测试 |
| 4 | plan-mode 白名单/拒绝 | Java `PlanModeMiddleware.onActing` 用 `ALWAYS_ALLOWED` + DENY 消息拒绝非只读工具；Rust `PlanModeMiddleware` **只有 `on_system_prompt` 注入** | **已实施**（见下，即裁决 B6） |

### 第 4 项（= B6）实施说明

最初核实发现需三处协同改动方可完整落地（原记为结构性缺口）：

1. **无 acting 挂点**：core 的 `MiddlewareChain::wrap_acting` / `Middleware::on_acting` 是**死代码**，harness 无法像 Java 那样在 `onActing` 里拒绝。
2. **plan 状态未绑定会话**：`tool/plan_mode.rs::PlanModeHandle` 持有**私有** state，且 `register` **全仓无调用者** → 生产路径上 `plan_active` 永不被置位。
3. **需状态句柄**：agent 内部 `AgentCore.state` 未公开句柄。

**最终实现（本次已落地，未走 middleware 挂点，改为 core acting 内的等价拒绝缝）：**

- **core `react_agent.rs`**：
  - 新增只读 getter `ReActAgent::state_handle() -> Arc<Mutex<AgentState>>`（暴露会话 state 句柄，供宿主工具读写，对齐 Java `resolveAgentState`）。
  - `acting_phase` 内新增 `plan_mode_denies(...)`：`plan_active` 为真时，凡**非只读且不在** `PLAN_MODE_ALWAYS_ALLOWED`（`plan_enter/plan_write/plan_exit/todo_write/agent_spawn/agent_send/agent_list/task_output/task_list`，逐字对齐 Java）中的调用，均发 `DENIED` 结果且**不执行**（复用既有 deny 事件路径）。
  - `run_agent_loop` 回写处：仅当共享 state 的 `plan_mode_context` 相对本轮起点发生变化时才回带（修复「循环克隆 + 末尾整篇回写」吞掉工具直写的问题）。
- **harness**：
  - `tool/plan_mode.rs`：`PlanModeHandle` 改为 `Arc<std::sync::Mutex<AgentState>>`，新增 `with_state(...)`；`plan_write` 工具先锁内取/记路径再释放锁后做 async 写（避免 std guard 跨 await）。
  - `agent/workspace/plan.rs`：新增 `write_plan_file(rc, path, content)`（供 `plan_write` 无锁跨 await 写文件）。
  - `agent/harness_agent.rs`：`plan_mode_enabled` 时在 agent 构建后 `PlanModeHandle::with_state(fs, plan_dir, agent.state_handle()).register(&toolkit)`。
- **测试**：core `plan_mode_denies_mutating_tool`（反向验证：禁用 `plan_mode_denies` → FAILED；恢复后 core lib **110 passed**）；harness `plan_mode_handle_binds_shared_state` + 既有 `harness_plan_wait_tool_test`（4 passed）。

### 反向验证

- 第 2 项：把 `fuzzy_text_matcher::search` 回退为「仅精确 `match_indices`」→ `patch_applies_fuzzy_and_reports_level` **FAILED**；恢复后 **355 passed / 0 failed**。
- 第 4 项（B6）：`plan_mode_denies` 直接 `return false` → `plan_mode_denies_mutating_tool` **FAILED**；恢复后 **110 passed / 0 failed**。
- 第 1、3 项：新测试并入 `cargo test -p agentscope-harness --lib`，全绿（**355 passed / 0 failed**）。
