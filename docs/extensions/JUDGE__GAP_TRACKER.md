# JUDGE 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **JUDGE** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-judge\`
> **Java 子模块**：`agentscope-extensions-jev`（68 类）、`agentscope-extensions-openai-decisions`（8 类）
> **Rust 目标**：`crates/agentscope-extensions-judge/`
> **建立缘由**：本模块此前**未在 rollup 与 `EXTENSIONS_MIGRATION_PLAN.md` 登记**；2026-10-22 经 §4.5 裁定 4 **纳入迁移范围**。本文件为首次建立（静态复核基线，未编译验证）。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览

| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-judge` |
| Java 子模块 | `jev`（System One 判定框架，68 类）、`openai-decisions`（OpenAI 决策 API 客户端，8 类） |
| Java 规模（实测 2026-10-22，非空行） | 主代码 **12469**（jev 11674 + openai-decisions 795）/ 测试 **4035**（15 文件 / **140 个 `@Test`**） |
| Rust crate | `crates/agentscope-extensions-judge` |
| Rust 规模（2026-11-01 重测：`src` **66** 个文件 / **32461** 行 / **426** 个测试；另 `tests/runtime_integration.rs` 1 文件 / 430 行 / 5 个集成用例；`#[ignore]` 共 **0** 个） | **2026-11-01 实测 426 passed / 0 failed**（Round 2 新增 4 例）+ 5 个集成用例全绿 |
| 是否注册 workspace | **是**（`Cargo.toml` 的 `crates/agentscope-extensions-judge`） |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 392 passed / 0 failed，另 5 个集成用例）—— jev 语义（中间件 hook / @Tool / `StoreJevContextArchive` / `JevPhaseRouting` / `JevResponseMiddleware` / `JevCodeReviewer` 六阶段驱动 / `JevStageRouter` / supervision 结果分类 / `JevGuardrail::screen` / `JevConfirmedCalls`）与 openai-decisions（三个中间件 + `DecisionSupport`）均已迁移；`browser` 子包与 **JEV** `example` 子包为「已完成（设计决定，不迁移）」（见 history/JUDGE__MIGRATION_HISTORY.md 已拆分订正：openai-decisions 的 `example/` **不是**示例，已按其真实运行时语义迁移） |

### 1.1 Rust crate 文件清单

| 文件 | 行数 | 测试 | 职责 |
|---|---|---|---|
| `src/lib.rs` | 47 | 0 | crate 根：`error` / `http` / `jev` / `openai_decisions` + crate 文档 |
| `src/error.rs` | 147 | 2 | `JevError` |
| `src/http.rs` | 408 | 8 | **新增**：`JudgeHttpTransport` 可注入 seam、`ReqwestJudgeTransport`、`backoff_ms`、离线 `ScriptedTransport`/`HangingTransport` |
| `src/jev/mod.rs` | 279 | 5 | 模块聚合 + `RetryPolicy` + URL/错误消息/API key 纯函数 |
| `src/jev/dto.rs` | 530 | 7 | Answer / Question / Usage / SystemOne 请求响应 DTO |
| `src/jev/client.rs` | 739 | 20 | **新增**：`JevClient`（System One 调用、超时、重试、错误映射、契约校验） |
| `src/jev/execution.rs` | 326 | 4 | `JevExecution` |
| `src/jev/guardrail.rs` | 590 | 7 | `JevGuardrail` |
| `src/jev/probability.rs` | 172 | 8 | 概率/评分工具 |
| `src/jev/review.rs` | 623 | 9 | `JevReview` 相关 |
| `src/jev/validation.rs` | 828 | 17 | 校验/评估编排 |
| `src/openai_decisions/mod.rs` | 96 | 1 | **新增**：模块聚合 + Java 常量 |
| `src/openai_decisions/error.rs` | 98 | 2 | **新增**：`DecisionsError`（对齐 `OpenAIDecisionsException`，无 body） |
| `src/openai_decisions/dto.rs` | 779 | 7 | **新增**：Input / Question / Option / Level / Request(+Builder) / Answer / Result / Usage |
| `src/openai_decisions/protocol.rs` | 1063 | 13 | **新增**：请求体编码 + 响应解析/校验（Java `parseAnswer/parseChoice/parseScore`） |
| `src/openai_decisions/client.rs` | 766 | 16 | **新增**：`OpenAIDecisionsClient` + `DecisionsRetryPolicy` + 重试/超时/脱敏 |
| `src/openai_decisions/support.rs` | 247 | 7 | **2026-11-01 新增**：`DecisionSupport` 投影（最新用户文本 / 受限角色·文本历史 / 确定性 JSON / 概率校验 / run key） |
| `src/openai_decisions/middleware.rs` | 2020 | 21 | **2026-11-01 新增**：`DecisionCaller` 缝 + 三个可选中间件（`ModelRouterMiddleware` / `ToolSelectionMiddleware` / `AutoModeMiddleware`） |

> `Cargo.toml` 依赖：`serde` / `serde_json` / `thiserror` / `async-trait` / `tokio` / **`reqwest`（workspace 0.13）** / `futures`（runner 的 `buffered` 顺序并发）/ `sha2`（`FileJevContextArchive` 的 SHA-256 内容寻址）；均走 workspace。`reqwest` 只经 `http::JudgeHttpTransport` seam 触达，单测全离线。`agentscope-core` / `agentscope-harness` 自 **2026-10-30** 起以 path 依赖声明（仅 `jev::runtime` 使用：`BaseStore` 归档、`Tool`、`Middleware`，见 history/JUDGE__MIGRATION_HISTORY.md）；其余模块仍只用本地缝。
>
> **2026-10-28 新增目录**（详见 history/JUDGE__MIGRATION_HISTORY.md）：`src/jev/{call,judge,code_review}.rs`、`src/jev/evaluation/**`（10 文件）、`src/jev/context/**`（5）、`src/jev/application/**`（4）、`src/jev/evidence/**`（3）、`src/jev/routing/**`（2）、`src/jev/supervision/**`（3）、`src/jev/integration/**`（2）。

## 2. Java 清单与依赖映射

### 2.1 jev 子包规模（实测）

| 子包 | 文件数 | 行数 | Rust 对应 | 状态 |
|---|---|---|---|---|
| `<root>`（Answer/Question/Choice/Score/SystemOne/JevClient 等） | 18 | 1451 | `jev/dto.rs`、`jev/mod.rs`、`jev/client.rs`、**`jev/judge.rs`（`JevJudge`，2026-10-28 补）**（+ `http.rs` seam） | **已完成** |
| `application`（Planner/Selector/Context/CustomerSupport/DraftPipeline/MemoryGate/Rag/StageRouter/Supervisor/TeamPlanner） | 10 | 950 | `jev/application/{selector,planners,draft_pipeline,stage_router}.rs` | **基本完成**（9 完整 + `JevStageRouter` 骨架） |
| `browser`（Navigator/ReadTool/Session/PlaywrightSession） | 4 | 1056 | 无 | **设计决定（不迁移）**（2026-10-29 确认；**待汇总方最终确认**）——依据：引入 Playwright 浏览器自动化运行时（Node 驱动 + 真实浏览器）与「纯 Rust 离线可测 crate」定位冲突；保留 trait 扩展点由宿主实现 |
| `context`（CompactionTranscript/FileJevContextArchive/JevContextArchive/JevContextCompactor/StoreJevContextArchive） | 5 | 903 | `jev/context/**` | **基本完成**（4 完整 + `StoreJevContextArchive` 骨架） |
| `evaluation`（Evaluator/JevEvaluationRunner/JevEvaluator 等） | 12 | 1828 | `jev/evaluation/**` | **基本完成**（11 完整 + 中间件 `onAgent/onModelCall` 骨架，`TraceCapture` 已全量） |
| `evidence` | 3 | 744 | `jev/evidence/**` | **基本完成**（2 完整 + `JevEvidenceTool` 投影已迁、`@Tool` 注册待 seam） |
| `example`（**仅 JEV**） | 4 | 1130 | 无 | **设计决定（不迁移）**（2026-10-29 确认；**待汇总方最终确认**）——依据：示例代码非运行链路，且依赖 harness 中间件装配点（`JevModelRouterMiddleware` 等），与主干解耦。**openai-decisions 的 `example/` 不在此列**，已迁（见 history/JUDGE__MIGRATION_HISTORY.md） |
| `integration` | 3 | 607 | `jev/integration/**` | **基本完成**（`JevKnowledge` 完整；工具/中间件为投影 + 骨架） |
| `review` | 4 | 1332 | `jev/review.rs` + `jev/code_review.rs` | **基本完成**（`ReviewRegions`/`JevReviewInput` 完整；`JevCodeReviewer` 定义层 + 驱动骨架、`JevCodeReviewTool` 骨架） |
| `routing` | 2 | 818 | `jev/routing/**` | **基本完成**（`JevRouteCatalog` 完整；`JevPhaseRouting` 纯 helper + 骨架） |
| `supervision` | 3 | 855 | `jev/supervision/**` | **基本完成**（2 完整；中间件 Schedule/Observation/tail 已迁，观察循环待 seam） |
| `openai-decisions`（DecisionQuestion/Input/Answer/Request/Result/Usage/Client/Exception） | 8 | 795 | `openai_decisions/{mod,error,dto,protocol,client,support,middleware}.rs` | **已完成**（含 `example/` 的 4 类：`DecisionSupport` → `support.rs`，三个中间件 → `middleware.rs`，见 history/JUDGE__MIGRATION_HISTORY.md） |

> **计数口径（2026-10-29 勘误）**：jev 去 `browser`(4) 与 `example`(4) 后共 **42 类 = 完整 32 / 部分 8 / 骨架 2**（明细 12+10+5+3+2+3+3+4 = 42）。见 history/JUDGE__MIGRATION_HISTORY.md 合计行记「41 类」为加总笔误（其分项 32+8+2 本身为 42），以本节为准。

### 2.2 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / `java.net.http` | `JevClient` / `OpenAIDecisionsClient` HTTP | **`reqwest` 0.13（workspace）**，经 `http::JudgeHttpTransport` 注入 | 已落地；生产用 `ReqwestJudgeTransport`，测试用内存 fake |
| Jackson | JSON（反）序列化 | `serde` + `serde_json` | 已落地 |
| Playwright（`browser` 子包） | 浏览器自动化 | 无 | 与 SANDBOX/CHANNEL 同级判定：**建议「设计决定（不迁移）」**，保留 trait 扩展点 |
| Reactor `Mono` | 异步流 | `async fn` + `tokio::time`（超时/退避） | 已落地；`Mono` 惰性与 `jitter(0.2)` 无对应物（见 history/JUDGE__MIGRATION_HISTORY.md） |
| Slf4j | 日志 | 未引入（crate 不写日志） | 保持现状 |

### 2.3 System One 契约逐字段核对（2026-10-28）

> 依据：Java `SystemOneRequest`（Builder）/`SystemOneResult`/`Usage`/`Question`/`Answer` 及其 Jackson 配置
> （`PropertyNamingStrategies.SNAKE_CASE` + `FAIL_ON_UNKNOWN_PROPERTIES=false`；`NoulQuestion`/`NoulCriteria`
> 带 `@JsonInclude(NON_NULL)`）。

| Java 字段 | Rust 字段 | 结论 |
|---|---|---|
| `SystemOneRequest.state : Object` | `state: Option<Value>` | ✅ 一致（Java 允许 null，由 `validateRequest` 拒绝；Rust 亦在 `validate_request` 报同一消息） |
| `SystemOneRequest.model : String` | `model: Option<String>` + `skip_serializing_if` | ⚠️ 语义一致（客户端填有效 model 后总存在）；独立序列化时 Java 发 `"model":null`、Rust 省略 —— 仅影响裸 DTO，不影响请求 |
| `SystemOneRequest.questions : Map<String,Question>`（`LinkedHashMap`→`Map.copyOf`） | `BTreeMap<String, Question>` | ✅ 语义一致（键序无语义；空 map 由校验拒绝） |
| `Question.type` = `noul`/`choice`/`score`（`@JsonTypeInfo`） | `#[serde(tag="type", rename_all="lowercase")]` | ✅ 一致 |
| `NoulQuestion.instructions : Object` | `instructions: Option<Value>` + **`skip_serializing_if`（本轮新增）** | ✅ 修正：Java `NON_NULL` 会省略 null，Rust 原会发 `null` |
| `NoulQuestion.criteria` + `NoulCriteria.true/false` | `criteria: Option<NoulCriteria>` + `true_value`/`false_value`（`rename="true"/"false"`，均 skip null） | ✅ 一致 |
| `ChoiceQuestion.instructions` / `criteria : Map<String,Object>` | `instructions: Option<Value>` / `criteria: Option<JsonMap>` | ✅ 一致（Java 发 `"criteria":null`、Rust 省略；该请求必被校验拒绝，语义等价） |
| `ScoreQuestion.criteria : List<Object>` | `criteria: Option<Vec<Value>>` | ✅ 同上 |
| `Answer.type` = `noul`/`choice`/`score` | 同上 tagged enum | ✅ 一致 |
| `NoulAnswer.noul : Double` | `noul: Option<f64>` | ✅ 一致（缺失/null → 由 `validate_answer` 报 invalid probability） |
| `ChoiceAnswer.choice/probabilities/confidence` | 同名字段（`BTreeMap<String,f64>`） | ✅ 一致 |
| `ScoreAnswer.score/legend/probabilities/confidence` | 同名字段（legend `BTreeMap<String,String>`） | ✅ 一致 |
| `Usage.inputTokens` → `@JsonProperty("input_tokens")`，`long` 原始类型 | `input_tokens: i64` + **`serde(default)`（本轮新增）** | ✅ 修正：Jackson 缺字段→`0`，Rust 原会反序列化失败；`null` 仍失败（与 Jackson 绑 primitive 一致） |
| `Usage.outputTokens` → `output_tokens` | `output_tokens: i64` + `serde(default)` | ✅ 同上 |
| `SystemOneResult.model/answers/usage`（均可 null，由 `validateResult` 拒绝） | `Option<String>` / `Option<BTreeMap>` / `Option<Usage>` | ✅ 一致（错误消息逐字对齐，见 `jev::validation`） |
| 未知属性（Jackson `FAIL_ON_UNKNOWN_PROPERTIES=false`） | serde 默认忽略未知字段 | ✅ 一致 |
| `Usage` 非负语义 | `Usage::is_non_negative` + `validate_response` | ✅ 一致（`validate_result` 不查符号，与 Java HTTP 路径相同） |

## 3. 详细缺口清单

### 3.1 P0（核心链路）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| HTTP 客户端 | `JevClient`（root）、`OpenAIDecisionsClient` | **已完成** | `http::JudgeHttpTransport` seam + `ReqwestJudgeTransport`；`jev::client::JevClient`（超时/重试/错误映射）、`openai_decisions::OpenAIDecisionsClient`（默认不重试）。`JevError` 沿用 `Protocol{status_code,response_body,retryable}` 覆盖 HTTP/API 两种语义（Java 亦为单一 `JevException`），未另加 `Http`/`Api` 变体以免语义重复。 |
| System One 请求/响应契约 | `SystemOneRequest` / `SystemOneResult` | **已完成** | 逐字段核对见 §2.3；本轮修正 2 处（`NoulQuestion.instructions` 补 `skip_serializing_if`、`Usage` 补 `serde(default)`）。 |

### 3.2 P1（重要）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `openai-decisions` 整块 | 8 类 / 795 行（+ `example/` 4 类） | **已完成** | `src/openai_decisions/**`（7 文件 / 5087 行 / 67 测试）+ **2026-11-01 新增** `support.rs` / `middleware.rs`（`DecisionSupport` + 三个中间件，见 history/JUDGE__MIGRATION_HISTORY.md） |
| `evaluation` 子包 | 12 类 / 1828 行 | **已完成**（12 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md；中间件 hook 已接线（`runtime/middleware.rs` 的 `TraceEvaluationMiddleware`，见 history/JUDGE__MIGRATION_HISTORY.md） |
| `application` 子包 | 10 类 / 950 行 | **已完成**（10 类，legacy 模型选择模式不迁） | 详见 history/JUDGE__MIGRATION_HISTORY.md 行 4；`JevStageRouter` 显式阶段入口已落地 |
| `context` 子包 | 5 类 / 903 行 | **已完成**（5 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md；`StoreJevContextArchive` 已接 harness `BaseStore`（见 history/JUDGE__MIGRATION_HISTORY.md） |
| `evidence` 子包 | 3 类 / 744 行 | **已完成**（3 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md；`@Tool` 注册已落地（`register_jev_tools`，见 history/JUDGE__MIGRATION_HISTORY.md） |
| `routing` 子包 | 2 类 / 818 行 | **已完成**（2 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md 行 1；派发由 `PhaseRoutingMiddleware` 驱动 |
| `supervision` 子包 | 3 类 / 855 行 | **已完成**（3 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md 行 1；观察循环已接线（`runtime/capture.rs` 的 `SupervisionCapture` + `SupervisionMiddleware`），终态按 agent 结果分类（Java `6ec57d245`） |
| `integration` 子包 | 3 类 / 607 行 | **已完成**（3 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md；工具注册与 `on_model_call` 已接 |
| `review` 完整语义 | 4 类 / 1332 行 | **已完成**（4 类） | 详见 history/JUDGE__MIGRATION_HISTORY.md 行 3；六阶段驱动见 `runtime/review_driver.rs` |
| `JevGuardrail::screen` 调用方 | `guardrail.rs`（纯 `evaluate`） | **已完成**（2026-10-31） | `screening.rs`：`GuardrailScreener::{request,screen,screener,from_client}`，经既有 `http::JudgeHttpTransport` 缝驱动；用例 `screen_folds_a_clean_reply` / `a_scripted_transport_drives_a_real_request` |
| `JevConfirmedCalls` | core `RuntimeContext` 键值扩展槽 | **已完成**（2026-10-31） | `confirmed_calls.rs`：绑定**既有** `RuntimeContext::{set_extension,set_extension_arc,extension}`，**未改 core**；用例 `binds_parameters_at_prompt_time_not_at_confirm_return` / `binds_exact_arguments_and_context` |
| 测试覆盖 | 140 个 `@Test` | **已完成（本 crate 口径）** | Rust **336** 个（2026-10-30 实测，未忽略，0 `#[ignore]`）+ 2026-10-31 新增（`screening`/`confirmed_calls`/`capture` 单测 + `tests/runtime_integration.rs` 5 例）+ 2026-11-01 新增 28 例（`openai_decisions/{support,middleware}.rs`）；**实测总数 392 + 5**（§1）；全部离线 |

> **「部分」/「骨架」的含义（2026-10-29 说明，避免误读为「主干未落地」）**：上表的「部分」= 该类**数据模型 / 纯函数 / 状态机 / 校验 / fail-closed 兜底已全部迁移并有单测**，只差需要 core/harness seam 的**中间件 hook（`onAgent`/`onModelCall`）与 `@Tool` 注册**；「骨架」= 接口形状与 fail-closed 语义已落地，驱动链留待 seam。全 crate 实测 **286 passed / 0 failed**（见 history/JUDGE__MIGRATION_HISTORY.md）。

### 3.3 P2（增强）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `browser` 子包（Playwright） | 4 类 / 1056 行 | **已完成（设计决定，不迁移）**（2026-10-31 正式化，见 history/JUDGE__MIGRATION_HISTORY.md） | 源码级依据：Java `jev/browser/PlaywrightSession` 依赖 Node 驱动 + 真实浏览器二进制，本工作区无 Playwright/Node 运行时依赖；影响：不提供 `Navigator`/`ReadTool` 实现；替代：保留 `EvidenceSource`/`CodeReviewSource` 注入缝由宿主提供 |
| `example` 子包（**仅 JEV**） | 4 类 / 1130 行 | **已完成（设计决定，不迁移）**（2026-10-31 正式化，见 history/JUDGE__MIGRATION_HISTORY.md） | 源码级依据：位于 `agentscope-examples/`，非运行链路；`JevModelRouterMiddleware` 仅装配示例；影响：`JevStageRouter` 无 legacy 模型选择模式；替代：`JevStageRouter::begin` + `PhaseRoutingMiddleware`（`STAGE_ROUTER_UNAVAILABLE` 常量说明）。**`openai_decisions` 的 `example/`（4 类）不适用本判定**——它承载真实运行时中间件（Java `31c7db8c9`），已迁为 `openai_decisions::middleware`（见 history/JUDGE__MIGRATION_HISTORY.md） |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`（本地 `D:\IdeaProjects\agentscope-java`）；Rust 工作树 `crates/agentscope-extensions-judge`（`src` **66** 文件 / **30 033** 行 / **392** 个 `#[test]`；另 `tests/runtime_integration.rs` **5** 例；`#[ignore]` **0**）。本表为本轮逐文件核对结论，**存量 §3.1–§3.3 历史清单保持原样未重新编号**；凡本表结论与历史行冲突，以本表为准，仅在「Rust 现状/下一动作」列说明依据，不改历史原文。
> 编号规则：`JUDGE-NN`（缺口与待裁定，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `JUDGE-D<NN>` / `JUDGE-X<NN>` 系列。编号本模块内唯一、单调递增。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 复核口径（`Get-ChildItem -Recurse -Filter *.java` + `@Test` 计数）：Java `jev` **68** 类（去 `browser` 4 + `example` 4 后 60 类）；`openai-decisions` **13** 类（9 主类 + `example` 4 类，**非** §1 记的 8/9 类）；测试 **17** 文件 / **166** 个 `@Test`。
> **openai-decisions 结论（本轮首要核对项）**：`src/openai_decisions/**` 已覆盖该子模块**全部 13 类**（含 `example/` 的 `DecisionSupport` + 三个 `OpenAIDecisions*Middleware`——它们是 `MiddlewareBase` 运行时能力，非示例代码），见 `JUDGE-05`。**未迁的是 `jev/example/`（另一子模块）**，见 `JUDGE-01`。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| JUDGE-01 | P1 | **JEV 版三中间件未迁**：`JevModelRouterMiddleware`（`choices` 模型路由，ON_AGENT+ON_MODEL_CALL）、`JevToolSelectionMiddleware`（ON_REASONING 工具集裁剪）、`JevAutoModeMiddleware`（ON_ACTING 自动执行前放行/拒绝）+ 共享 `JevSelectionSupport`；连带 `JevStageRouter` 的 legacy 模型选择模式 fail-closed | `jev/example/JevModelRouterMiddleware.java:54`（`implements MiddlewareBase`）、`#onAgent`、`#onModelCall`；`jev/example/JevToolSelectionMiddleware.java:49`；`jev/example/JevAutoModeMiddleware.java:73`；`jev/example/JevSelectionSupport.java:32`；`jev/application/JevStageRouter.java:35`（字段即 `JevModelRouterMiddleware`） | `rg 'ToolSelection\|AutoMode\|auto_mode\|tool_selection' src` **仅命中 `src/openai_decisions/{mod.rs,middleware.rs}`（Decisions 后端）**；`src/jev/**` 三同义词均零命中；`src/jev/application/stage_router.rs:31` `STAGE_ROUTER_UNAVAILABLE` 仍是 fail-closed 常量，`:55 JevStageRouter` 只实现显式 phase 分支 | 已完成 | 历史 §3.3 以「位于 `agentscope-examples/`、非运行链路」为由判 D，该依据**不成立**：这 4 类在 `agentscope-extensions-jev/src/main/java/.../jev/example/`（main source set），且 `JevStageRouter` 直接依赖之；同类 `openai-decisions/example` 已按同一理由迁入 `openai_decisions/middleware.rs`。请汇总方裁定：**撤销判定** → 照 `openai_decisions/middleware.rs` 结构在 `src/jev/runtime/` 增 `jev/middleware.rs`（三中间件均走 `JevExecution` + `call::Caller`，`JevStageRouter` legacy 分支改为委托），验收＝ENFORCE 才改写、OFF 透传、每中间件 ≥6 个离线 `ScriptedCaller` 用例；**维持不迁** → 改写 §3.3 依据并把本节判据同步到 §4.3.1（不得再用「不在运行链路」措辞） 【裁定 2026-10-31：撤销原判定：照 openai_decisions/middleware.rs 迁移三中间件到 src/jev/runtime】【2026-10-31 落地：新增 `src/jev/runtime/selection.rs`（已由 `runtime/mod.rs` 声明并 re-export），三中间件 + 支持函数全量落地：`JevModelRouterMiddleware`（`on_agent` 每次调用先写 fallback 再路由，`on_model_call` 按 `Arc::ptr_eq` 回找 choice，不兼容则清回 fallback；`decisions` 以 `run_id`/`session_id` 为键存 map 以替代 Reactor Context）、`JevToolSelectionMiddleware`（`on_reasoning`，`TOOL_BATCH_SIZE=64` 分批提问、`always_include` 免评、任一模糊分数则整集不动）、`JevAutoModeMiddleware`（`on_acting`，已确认但参数变更 / 无 state / 校验失败 → 整批 deny，拒绝结果经 `ActingResultsHandle` 记录并合成 `TOOL_RESULT_*` 事件）；全部走 `JevExecution` + `call::Caller` seam，OFF 透传 / SHADOW 只观察 / ENFORCE 才改写。详情差异写入 selection.rs 模块头（phaseRouting advanced 模式不重复、denied 结果走 `ActingResults` 而非 `AgentState.context`）。离线用例（`ScriptedCaller`）：`the_router_is_a_passthrough_when_disabled`、`enforce_routes_once_per_invocation_and_swaps_the_model`、`shadow_mode_asks_but_never_switches_the_model`、`low_confidence_or_an_unknown_choice_keeps_the_fallback`、`a_request_with_tools_is_incompatible_and_keeps_the_caller_model`、`the_eligible_filter_removes_every_candidate`、`the_router_validates_its_configuration`、`tool_selection_is_a_passthrough_when_disabled`、`enforce_keeps_only_the_tools_above_the_threshold_in_score_order`、`an_ambiguous_score_leaves_the_tool_set_untouched`、`always_included_tools_are_never_judged`、`a_malformed_relevance_answer_keeps_the_full_tool_set`、`shadow_mode_asks_but_does_not_filter`、`tool_selection_validates_its_thresholds`、`the_guard_is_a_passthrough_when_disabled`、`enforce_denies_a_call_below_the_safety_threshold`、`enforce_forwards_a_safe_call_unchanged`、`unguarded_and_user_allowed_calls_skip_the_check`、`a_malformed_risk_answer_denies_the_whole_batch`、`a_changed_confirmation_is_denied_without_asking`、`a_missing_agent_state_denies_the_guarded_calls`、`the_guard_validates_its_configuration`、`the_support_projection_matches_java`、`selected_names_and_top_name_respect_none_and_confidence`。**未删除**`src/openai_decisions/middleware.rs`：其中 `ModelRouterMiddleware`/`ToolSelectionMiddleware`/`AutoModeMiddleware` 是 Decisions 后端的独立类型（无同名/同义重复，`rg 'struct (ModelRouter|ToolSelection|AutoMode)Middleware'` 两处分属两模块），JEV 版只是照其结构另建。`JevStageRouter` legacy 分支改委派**仍未做**：`jev/application` 层刻意零框架依赖，而 `JevModelRouterMiddleware` 的两个 hook 需 `RuntimeContext`，在 `stage_router.rs` 内直接委托会使 application → runtime → core 反向依赖；宿主正确装配 = `JevStageRouter`（阶段）+ `JevModelRouterMiddleware`（模型选择）同时挂载；`STAGE_ROUTER_UNAVAILABLE` 仍为 legacy 模式下的 fail-closed 兜底】 |
| JUDGE-02 | P2 | `JevExecution` 的**观察者回调**（`Options.observer`）与**取消观测**（`CANCELLED`）未迁：`execute_guarded` 返回 `Decision` 即结束，既不产出 `Record` 也不上报取消 | `jev/JevExecution.java:74/78`（`Options.observer : BiConsumer<RuntimeContext,Record>`）、`:106 #execute`（`doOnNext → observe`）、`:139 doOnCancel → Status.CANCELLED`、`:150 #observe`（观察器异常必须被吞掉） | `src/jev/execution.rs:152 Options` 只有 `mode/budget/version`（无 `observer`）；`src/jev/call.rs:134 execute_guarded` 只有 mode/budget/timeout/error 四分类；`rg 'observer' src/jev/execution.rs src/jev/call.rs` 仅命中文档与测试注释（`Record` 需调用方自行 `:231 record()` 组装） | 已完成 | 增 `Options.observer`（`Arc<dyn Fn(&RuntimeContext,&Record) + Send + Sync>`）与 `execute_guarded` 的成功/失败/超时上报，并用 drop-guard 在 future 被丢弃时上报 `Status::Cancelled`；验收：观察器 panic 不改结果、`elapsed` 单调、3 例（正常/超时/撤销） 【2026-11-01 Round 2 落地：已完成——`src/jev/execution.rs` 新增 `pub type Observer = Arc<dyn Fn(Option<&RuntimeContext>, &Record) + Send + Sync>`、`Options.observer`（`Options::new`/`disabled()` 装 `noop_observer()`，`with_observer()` 替换）与 `JevExecution::observe(ctx, status, reason, elapsed, recommendation)`（`catch_unwind` 吞观察器 panic，注意 release `panic=abort` 下不能吞）；`Options` 的 `Debug`/`PartialEq` 改为手写（只比数据字段）。`src/jev/call.rs` 新增 `execute_guarded_with_context(execution, Option<&RuntimeContext>, op)`，`execute_guarded` 以 `None` 委派；每次执行产出**恰好一条** `Record`（OFF 也报 `Skipped/DISABLED`），正常/失败/超时上报后由 `Observation` 的 `Drop` 在 future 被丢弃时补报 `Cancelled/CANCELLED`（等价 `doOnCancel`）。用例：`the_observer_receives_decided_timeout_and_skipped_runs`（3 条记录：Decided / Error+TIMEOUT 且 elapsed ≥ budget / Skipped+DISABLED）、`the_observer_receives_the_supplied_runtime_context`（`session_id` 透传）、`dropping_the_future_reports_a_cancelled_run`（poll 一次后 drop）、`a_panicking_observer_does_not_change_the_decision`】 |
| JUDGE-03 | P2 | **陈旧 fail-closed 常量与模块文档**：多处仍宣称「需要 seam / 尚未实现」，而对应能力已由 `jev::runtime` 落地；同名 `*Unavailable` 类型因此成为只有测试引用的死路径，会误导接入方 | —（Rust 内部一致性问题；Java 侧对应能力均已实现并有测试） | `src/jev/guardrail.rs:3-7`（称网络调用未实现，实际 `src/jev/screening.rs:34 GuardrailScreener` 已实现）；`src/jev/evidence/tool.rs:13 TOOL_UNAVAILABLE` 且 `:124 register()` 直接返回 `Err`，实际 `src/jev/runtime/tools.rs:496 register_jev_tools` 已注册三工具；`src/jev/integration/knowledge.rs:22` + `:198` 同构；`src/jev/integration/response.rs:126 ResponseMiddlewareUnavailable`（实际 `src/jev/runtime/middleware.rs:319 ResponseMiddleware` 全量）；`src/jev/supervision/middleware.rs:106`（实际 `runtime/middleware.rs` 的 `SupervisionMiddleware` 已接线）；`src/jev/context/archive.rs:15`（实际 `src/jev/runtime/archive.rs:49 StoreJevContextArchive`）；`src/jev/evaluation/mod.rs` 模块文档称 `MiddlewareBase` hooks 未迁（实际 `src/jev/evaluation/capture.rs:180 JevTraceEvaluationMiddleware` + `runtime/middleware.rs:205`） | 已完成 | 改文案为「host 未提供 seam 时的 fail-closed 兜底，运行时实现见 `jev::runtime::*`」；`JevEvidenceTool::register`/`KnowledgeTool::register` 改为委托 `runtime::tools` 或标 `#[deprecated]`；验收：`rg 'does not implement yet\|needs the core Toolkit\|hooks are not ported' src` 零命中，且 `cargo test -p agentscope-extensions-judge` 仍 392+5 全绿 【2026-11-01 Round 2 落地：已完成——7 处文案/常量改为「host 未提供 seam 时的 fail-closed 兜底，运行时实现见 `jev::runtime::*`」：`jev/guardrail.rs` 模块头（指向 `screening::GuardrailScreener`）、`jev/evidence/tool.rs` 模块头 + `TOOL_UNAVAILABLE` + `register()` 文档、`jev/integration/knowledge.rs` 同构、`jev/integration/response.rs` 模块头 + `RESPONSE_MIDDLEWARE_UNAVAILABLE` + 占位类型文档、`jev/supervision/middleware.rs` 同构、`jev/context/archive.rs` 模块常量 + `UnavailableContextArchive` 文档、`jev/evaluation/mod.rs` 模块头。验收 `rg 'does not implement yet|needs the core Toolkit|hooks are not ported' src` **零命中**。**偏离说明**：`JevEvidenceTool::register`/`JevKnowledgeTool::register` 未改为委托（签名不收 `Toolkit`，委托需改公开 API）也未加 `#[deprecated]`（会在本 crate 既有断言测试里制造警告噪声），只保留 fail-closed 并把文档指向 `runtime::tools::register_jev_tools`；两处占位类型的 `*_UNAVAILABLE` 常量文案已同步，断言均引用常量，测试无需改】 |
| JUDGE-04 | P1 | `JevConfirmedCalls` 的**自动生效点缺失**：本 crate 只提供 API，core 的 HITL 恢复路径不消费它，导致「用户确认的是 A 参数、恢复时执行 B 参数」无法被拦下（**归属 core**，本 crate 不改 core） | `jev/JevConfirmedCalls.java#snapshot/remember/contains/matches`（`snapshot` 必须在询问用户**之前**取，`matches` 比对 name+arguments 的排序 SHA-256）；装配用例 `src/test/.../JevPermissionIntegrationTest.java:106` | `src/jev/confirmed_calls.rs:37 JevConfirmedCalls` 已完整实现（`install/ledger/snapshot/remember/contains/matches`，键 `CONFIRMED_CALLS_KEY`），`rg 'JevConfirmedCalls\|confirmed_calls::\|CONFIRMED_CALLS_KEY\|remember_confirmed_call' crates/agentscope-core crates/agentscope-harness` **零命中**（4 个同义词交叉验证），crate 内调用点仅 `tests/runtime_integration.rs:385` | 已完成 | 请 core 裁定是否在 `confirm_handler` 恢复 pending tool call 前调用 `matches`（不一致则重新征求确认，或直接拒绝）；若裁定**不做**（宿主负责），本条转 §4.3.1 并注明「core 无 HITL 记账槽，安全语义由宿主装配保证」 【裁定 2026-10-31：不改 core：judge 提供 JevConfirmGuard::wrap，由装配层比对】【2026-10-31 落地：新增 `src/jev/confirm_guard.rs`（`jev::ConfirmGuardError` / `jev::JevConfirmGuard` 已 re-export），**未改 core/harness**。`wrap(inner)` 在询问用户**之前**取 `JevConfirmedCalls::snapshot`，仅在 `ConfirmResult::is_confirmed()` 时记账（用户改参时以 `ConfirmResult.tool_call` 为准，记实际批准的那份）；`verify(call)`/`verify_all` 在恢复路径比对 `name+arguments` 的排序 SHA-256：id 已批准但参数/名称变了 → `ConfirmGuardError::ArgumentsChanged{call_id}`（`Display` 含 “was approved with different arguments; re-ask the user”）；未批准过的 id 与空 id 一律放行（与 `contains` 语义一致），因此不装 guard 的宿主保持原语义，残余风险已在模块头文档化。用例：`an_approved_call_verifies_after_the_prompt`、`changed_arguments_are_rejected_at_resume`、`a_denied_call_is_never_remembered`、`a_user_edited_call_binds_the_edited_arguments`、`the_wrapper_records_every_prompt_once`、`installing_twice_reuses_the_same_ledger`】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| JUDGE-05 | **`openai-decisions` 全 13 类**：9 主类（`DecisionAnswer/Input/Question/Request/Result/Usage/Client/Exception/RetryPolicy`）+ `example/` 4 类（`DecisionSupport` + 路由/工具选择/自动模式三中间件） | `openaidecisions/OpenAIDecisionsClient.java#params/execute/parseAnswer/parseChoice/parseScore`；`openaidecisions/example/DecisionSupport.java#latestUserText/recentMessages` | `src/openai_decisions/{dto.rs,protocol.rs:79 decision_body,client.rs:107 decide,error.rs,support.rs:36 latest_user_text,middleware.rs:237/548/829}`；测试 `honours_a_model_override_and_the_prompt_cache_key`、`the_router_selects_once_per_invocation_and_swaps_the_model`、`selection_keeps_essential_tools_caps_relevant_ones_and_preserves_order`、`the_guard_denies_risky_and_refused_calls_and_keeps_the_others` | 已完成 |
| JUDGE-06 | System One 契约：DTO、`snake_case`/`NON_NULL` 语义、`2..=255` 选项 / `2..=10` 档位、per-option 概率容差、错误消息逐字 | `jev/JevClient.java#validateRequest/validateQuestion/validateAnswer/validateProbabilities`；`jev/SystemOneRequest.java` | `src/jev/dto.rs:248 SystemOneRequest`、`src/jev/validation.rs:38/58/174/291`、`src/jev/probability.rs:30 probability_sum_tolerance`；测试 `accepts_a_valid_request_and_response`、`rejects_invalid_request_input` | 已完成 |
| JUDGE-07 | `JevClient`：URL 归一（`/v1` 与 `/v1/systemone` 两种尾缀）、Bearer/Accept 头、逐次超时、指数退避（`initial×2^n`，上限 `×8`）、`message→error→detail` 错误解析、`TYPESAFE_API_KEY→JEV_API_KEY` 回落、按契约校验响应 | `jev/JevClient.java#systemOne/execute/retrySpec/httpException/errorMessage/defaultApiKey`；`jev/JevRetryPolicy.java#defaults` | `src/jev/client.rs:107 system_one`、`:224 JevClientBuilder`、`src/jev/mod.rs:134 system_one_url`、`:166 api_key_from_env`、`src/http.rs#backoff_ms`；测试 `sends_the_validated_request_and_parses_the_response` | 已完成 |
| JUDGE-08 | 判定层：`JevJudge`（全条件判定 + TIMEOUT/INVALID_RESPONSE/BACKEND 分类）、`JevGuardrail`（hazard 折叠、severity 升级、`blocked(blockOnReview)`）、`screen` 的真实调用方、`JevConfirmedCalls` | `jev/JevJudge.java#judge/evaluate`；`jev/JevGuardrail.java#defaults/screen/evaluate`；`jev/JevConfirmedCalls.java` | `src/jev/judge.rs:226 judge`、`src/jev/guardrail.rs:101 JevGuardrail`、`src/jev/screening.rs:34 GuardrailScreener`、`src/jev/confirmed_calls.rs:37`；测试 `default_policy_matches_java_defaults`、`screen_folds_a_clean_reply`、`a_scripted_transport_drives_a_real_request`、`binds_parameters_at_prompt_time_not_at_confirmation_return` | 已完成 |
| JUDGE-09 | 评审链路：`ReviewRegions`（source/patch）、`JevReviewInput`、`JevCodeReviewer` 六阶段驱动、`JevCodeReviewTool` | `jev/review/JevCodeReviewer.java#run`；`review/ReviewRegions.java#source/patch`；`review/JevCodeReviewTool.java` | `src/jev/review.rs:43 source_regions`、`:74 patch_regions`、`:280 JevReviewInput`、`src/jev/code_review.rs:523 JevCodeReviewer`、`src/jev/runtime/review_driver.rs:99 ReviewDriver`、`runtime/tools.rs:373 ReviewCodeTool` | 已完成 |
| JUDGE-10 | 路由与监督：`JevRouteCatalog`（quota/effort/price/candidate/route/snapshot + `excluded`/`effort`）、`JevPhaseRouting`、`JevTaskSupervisor`、`SupervisionEvidence`、监督中间件 | `jev/routing/JevRouteCatalog.java#excluded/effort`；`routing/JevPhaseRouting.java#onAgent/onModelCall`；`supervision/JevTaskSupervisor.java` | `src/jev/routing/catalog.rs:163 Candidate`、`:403 effort`、`src/jev/routing/phase_routing.rs:256 JevPhaseRouting`、`src/jev/supervision/supervisor.rs:259`、`supervision/evidence.rs:57`、`runtime/capture.rs:38 SupervisionCapture`；测试 `offers_apply_the_allowlist_and_exclusion_rules`、`a_complete_verified_job_recommends_completion_review`、`supervision_classifies_the_agent_result_not_boundary_events`、`phase_routing_applies_the_resolved_model_and_effort_in_enforce` | 已完成 |
| JUDGE-11 | 上下文与评测：`CompactionTranscript`、`JevContextCompactor`、`FileJevContextArchive`、`StoreJevContextArchive`；trace/metrics/runner/evaluator/text backend/capture | `jev/context/{CompactionTranscript,JevContextCompactor,FileJevContextArchive,StoreJevContextArchive}.java`；`jev/evaluation/{JevTraceMetrics,JevTraceRunner,JevTraceEvaluator,JevTextBackend}.java` | `src/jev/context/transcript.rs:55`、`compactor.rs:174`、`file_archive.rs:23`、`src/jev/runtime/archive.rs:49`、`src/jev/evaluation/{trace.rs:149,metrics.rs:648 agent_metrics,evaluator.rs:129,runner.rs:187,evaluation_runner.rs:195,text_backend.rs:57,capture.rs:180}`；测试 `save_then_restore_round_trips_and_is_content_addressed`、`a_store_without_create_if_absent_support_is_rejected` | 已完成 |
| JUDGE-12 | `application` 10 类 + `evidence`/`integration` + 宿主装配（三工具、四中间件、归档、集成测试） | `jev/application/{JevBrowserPlanner,JevCandidateSelector,JevContextPlanner,JevCustomerSupport,JevDraftPipeline,JevMemoryGate,JevRag,JevSupervisor,JevTeamPlanner}.java`；`evidence/{JevEvidenceProcessor,JevEvidenceTool}.java`；`integration/{JevKnowledge,JevKnowledgeTool,JevResponseMiddleware}.java` | `src/jev/application/planners.rs:98/243/322/407/575/677/778`、`selector.rs:38`、`draft_pipeline.rs:68`、`evidence/processor.rs:270`、`integration/knowledge.rs:59`、`runtime/tools.rs:496 register_jev_tools`、`runtime/middleware.rs:205 TraceEvaluationMiddleware`/`:319 ResponseMiddleware`/`:564 SupervisionMiddleware`/`:759 PhaseRoutingMiddleware`；测试 `register_jev_tools_wires_every_configured_tool`、`the_screening_request_matches_the_java_shape`、`response_middleware_off_passes_through_and_enforce_withholds` | 已完成 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **已完成**：crate 骨架、错误类型、System One 的 DTO/执行/guardrail/概率/校验/评审纯函数层；**HTTP 传输 seam + 两个客户端**；**`openai-decisions` 整块（含 `example/` 三个中间件，见 history/JUDGE__MIGRATION_HISTORY.md）**；**`jev` 八个编排子包全量**（见 history/JUDGE__MIGRATION_HISTORY.md）；**`jev::runtime` 四个中间件 hook + 三个 `@Tool` + `StoreJevContextArchive` + 六阶段 reviewer 驱动 + `JevStageRouter`**；**2026-10-31 收口**：supervision 结果分类（Java `6ec57d245`）、`JevGuardrail::screen` 调用方（`screening.rs`）、`JevConfirmedCalls`（`confirmed_calls.rs`）、`tests/runtime_integration.rs`（见 history/JUDGE__MIGRATION_HISTORY.md）；**2026-11-01**：Java `31c7db8c9` 的 `OpenAIDecisions{ModelRouter,ToolSelection,AutoMode}Middleware` + `DecisionSupport`（`openai_decisions/{support,middleware}.rs`，见 history/JUDGE__MIGRATION_HISTORY.md），`16cb024ae` 裁定为无需等价实现（见 history/JUDGE__MIGRATION_HISTORY.md）。文件数/行数/测试数以 §1 的 2026-11-01 重测为准（66 `src` 文件 / 32753 行 / 392 test + 5 集成用例）。
- **2026-11-01 Round 2（JUDGE-02 / JUDGE-03）**：`src/jev/execution.rs` 新增 `Observer` / `Options.observer` / `JevExecution::observe`，`src/jev/call.rs` 新增 `execute_guarded_with_context` + `Observation` drop-guard（正常/失败/超时上报后再补报 `Cancelled`），新增 4 个离线用例；7 处陈旧 fail-closed 文案/常量改为指向 `jev::runtime::*`（验收 `rg 'does not implement yet|needs the core Toolkit|hooks are not ported' src` 零命中）。**实测**：`cargo build -p agentscope-extensions-judge -j 2` → EXIT=0；`cargo test -p agentscope-extensions-judge -j 2 --no-fail-fast` → **426 + 5 passed / 0 failed / EXIT=0**。反向验证：移除 `execute_guarded_with_context` 的 `observation.report(...)` 调用 → `jev::call::tests::the_observer_receives_decided_timeout_and_skipped_runs` **FAILED**（EXIT=101，7 passed / 1 failed）→ 还原后全绿。**未改 core/harness**。
- **验证命令与结果**：2026-10-31（主智能体实测）`cargo test -p agentscope-extensions-judge -j 2` → **392 passed / 0 failed / 0 ignored**、`cargo build -p agentscope-extensions-judge` → 0 warning（见 history/JUDGE__MIGRATION_HISTORY.md）。另 `tests/runtime_integration.rs` 5 个集成用例同样全绿。
- **本轮（JUDGE-01 / JUDGE-04 落地，2026-10-31 Round 1）**：`src/jev/runtime/selection.rs` 三中间件 + 24 个离线用例；新增 `src/jev/confirm_guard.rs`（`JevConfirmGuard::wrap/verify/verify_all` + `ConfirmGuardError`，6 个用例）；`jev/mod.rs` 增 `pub mod confirm_guard` + re-export，`runtime/mod.rs` 增 `pub mod selection` + re-export；`openai_decisions/middleware.rs` **保留**（Decisions 后端独立类型，无重复）。**本轮受批次约束未执行 cargo**，实测数由主智能体统一编译/跑测后回填；**未改 core/harness**。
- **已知问题**：①`JevConfirmedCalls` 的自动生效点（core HITL 恢复路径）需 core 侧接线，本 crate 不改 core（属设计边界）；②无 live API 测试（有意：全离线，wire 契约由 `http.rs` seam 的脚本化传输覆盖）。**原列「四个中间件 hook / `@Tool` 注册 / `StoreJevContextArchive` / `JevStageRouter` / `JevCodeReviewer` / `JevGuardrail::screen` 调用方 / `JevConfirmedCalls` / OpenAI Decisions 中间件未落地」已全部失效（已落地，见 history/JUDGE__MIGRATION_HISTORY.md）**。

### 4.2 下一步与阻塞

1. ~~**P0**：落地 HTTP 客户端~~ → **已完成**（`http.rs` + `jev::client` + `openai_decisions::client`）。
2. ~~**P0**：逐字段核对 `SystemOneRequest`/`SystemOneResult`/`Usage`~~ → **已完成**（见 §2.3，修正 2 处）。
3. ~~**P1**：按依赖顺序迁 `evaluation` → `application` → `context` → `evidence`/`routing`/`supervision`/`integration`~~ → **已完成（2026-10-28，见 history/JUDGE__MIGRATION_HISTORY.md）**。
4. ~~**需汇总方裁定**：`browser` / `example`~~ → **已按「已完成（设计决定，不迁移）」正式化（2026-10-31，见 history/JUDGE__MIGRATION_HISTORY.md，含源码级依据/影响/替代方案）**；汇总方如不同意，回退为 P2 缺口即可。**适用范围仅 `browser` 与 JEV `example`**：openai-decisions 的 `example/` 是真实运行时中间件，已于 2026-11-01 迁移（见 history/JUDGE__MIGRATION_HISTORY.md），不在裁定范围。
5. **跨模块（已解除）**：四个中间件 hook、`@Tool` 注册、`BaseStore` 归档、`RuntimeContext` 扩展槽曾需 core/harness 侧 seam（见 history/JUDGE__MIGRATION_HISTORY.md），2026-10-31 已全部用**既有** API 接通（见第 6 条）；2026-10-31 与 2026-11-01 两轮均未改 `agentscope-core` / `agentscope-harness` / 根 `Cargo.toml`。
6. **真实剩余（2026-10-31 收口后）**：
   - ~~四个中间件 hook 未接~~ → **已接**（`jev/runtime/middleware.rs`，见 history/JUDGE__MIGRATION_HISTORY.md）。
   - ~~`@Tool` 注册未接~~ → **已接**（`register_jev_tools`，见 history/JUDGE__MIGRATION_HISTORY.md）。
   - ~~`StoreJevContextArchive` 为骨架~~ → **已接 harness `BaseStore`**（见 history/JUDGE__MIGRATION_HISTORY.md）。
   - ~~`JevStageRouter` 骨架 + `JevCodeReviewer` 驱动未接~~ → **已接**（见 history/JUDGE__MIGRATION_HISTORY.md 行 3/4）。
   - ~~`JevGuardrail::screen` 无响应式调用方~~ → **已落地**（`screening.rs`，见 history/JUDGE__MIGRATION_HISTORY.md）。
   - ~~`JevConfirmedCalls` 缺扩展槽~~ → **已落地**（`confirmed_calls.rs`，用既有 `RuntimeContext` 键值槽，未改 core）。
   - ~~无 `tests/` 集成测试~~ → **已建 `tests/runtime_integration.rs`**（5 个装配级用例，全离线）。
   - ~~`OpenAIDecisions*Middleware` 未迁~~ → **已迁**（`openai_decisions/middleware.rs`，见 history/JUDGE__MIGRATION_HISTORY.md）。
   - **仍剩**：①`JevConfirmedCalls` 的 core 侧自动生效点（HITL 恢复，属 core 边界，本 crate 不改 core）；②无 live API 测试（有意，全离线）。
   - 跨模块建议（非阻塞）：core 提供 `Msg` → `TraceMessage`/`ContextMessage` 投影 trait（现由本 crate 自行投影，见 history/JUDGE__MIGRATION_HISTORY.md）。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本节仅覆盖**有意为之**的差异。真实缺口与待裁定项（`JUDGE-01`~`JUDGE-04`）不在本节，见 §3.5.1；历史 §3.2/§3.3 中已正式化的 `browser` 判定在本节以 `JUDGE-D01` 保留。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| JUDGE-D01 | `jev/browser/` 子包（4 类 / 1056 行）：`JevBrowserNavigator`、`JevBrowserReadTool`、`JevBrowserSession`、`JevPlaywrightSession` | `jev/browser/JevPlaywrightSession.java:19-25`（`import com.microsoft.playwright.{Browser,BrowserType,Page,JSHandle,Dialog}`）；`agentscope-extensions-jev/pom.xml:34-40`（`com.microsoft.playwright:playwright`，注释「Optional adapter」） | 该子包是**真实浏览器驱动**：Playwright 的 JVM 客户端需 Node driver + Chromium 二进制（`Playwright.create()`/`BrowserType.launch`/`Page`/`JSHandle`），属宿主级运行时而非纯逻辑。本工作区全部 `Cargo.toml` 中 `rg -i 'playwright\|chromiumoxide\|headless.chrome\|fantoccini'` **零命中**（4 个同义词交叉验证），引入即打破本 crate「纯 Rust、默认全离线可测」定位（`Cargo.toml` 依赖仅 serde/serde_json/thiserror/async-trait/tokio/reqwest/futures/sha2）。**决策层已迁**：`JevBrowserPlanner` 的纯状态机在 `src/jev/application/planners.rs:98`（`BrowserOperation`/`BrowserAction`/`BrowserProposal`，测试 `browser_planner_proposes_only_verified_non_done_actions`、`browser_planner_abstains_when_anything_is_uncertain`），缺的只是驱动层 | 不提供 `Navigator`/`ReadTool` 的 Playwright 实现：`EvidenceSource`（`src/jev/runtime/tools.rs:111`）与 `CodeReviewSource`（`:288`）两个 trait 由宿主注入等价能力；`JevBrowserReadTool` 的读页/收据语义（`Permit`/`Receipt`）需宿主自行保证。JUDGE-01 的三中间件不依赖此子包 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| JUDGE-X01 | JEV 可重试状态码集合 | `src/jev/client.rs:39 is_retryable_status`：`408 \| 409 \| 429 \| 529 \| >=500` | Java `JevClient.httpException` 仅 `429 \| 529 \| >=500`；`openai_decisions` 侧保持 Java 原集合（`src/openai_decisions/client.rs:35`，含注释说明为何不跟随） | 网关前置场景下 408（请求超时）/409（乐观并发冲突）是瞬时故障，重试无损；两个客户端的集合差异在源码注释中显式声明，避免误判为同一条策略 |
| JUDGE-X02 | 退避无抖动 | `src/http.rs#backoff_ms`：确定性 `initial×2^n`，上限 `initial×8` | Java `Retry.backoff(...).maxBackoff(initial×8).jitter(0.2)` 含 ±20% 抖动（`JevClient.retrySpec`、`OpenAIDecisionsClient.decide`） | 确定性退避使离线断言可精确比对尝试次数与间隔（`ScriptedTransport`/`HangingTransport`）；抖动只影响多客户端同步重试的碰撞概率，不影响重试预算 |
| JUDGE-X03 | 错误类型与分类 | 单一 `JevError::{InvalidArgument,Protocol{status,body,retryable}}`（`src/error.rs`）与 `DecisionsError::{Invalid,Api{status,retryable}}`（`src/openai_decisions/error.rs`） | Java 用 `IllegalArgumentException`（参数）+ `JevException`（协议，带 status/body/retryable）+ `OpenAIDecisionsException`；`OpenAIDecisionsClient` 额外把 `OpenAIInvalidDataException`/`IllegalArgumentException` 统一折成「Unable to parse」 | Rust 以同一 `Protocol` 变体承载 HTTP 与 API 两种语义（Java 亦为单一 `JevException`），`thiserror` 使分类可在 `match` 中穷尽；调用方按 `is_retryable()`/`status_code()` 判定，不依赖异常类型名 |
| JUDGE-X04 | HTTP 传输缝 | `src/http.rs`：`JudgeHttpTransport` trait + 生产 `ReqwestJudgeTransport` + 测试 `ScriptedTransport`/`HangingTransport`；`openai_decisions` 用 `protocol.rs:79 decision_body` + `parse_decision_response` 手写 wire | Java JEV 走 core `HttpTransport`；Java Decisions 借官方 SDK（`OpenAIOkHttpClient`/`DecisionCreateParams`/`JsonField`）并 `client.close()` 管理所有权 | Rust 无官方 Decisions SDK；手写 wire 逐字段复刻 `params/parseAnswer/parseChoice/parseScore`（含 `input` 文本/多模态两种编码、`prompt_cache_key`、400 条问题上限分批），换来零厂商 SDK 依赖 + 单测全离线；代价：SDK 的 `close()`/借用客户端语义消失（`OpenAIDecisionsClient` 仅 `Clone`），注入 transport 时 `api_key` 可缺省（不发 `Authorization`，源码注释已声明） |
| JUDGE-X05 | 并发/时间模型 | `async fn` + `tokio::time::{timeout,sleep}`；`futures::stream::buffered` 保留 Java `flatMapSequential` 的有序并发 | Java `Mono.defer` + `subscribeOn(Schedulers.boundedElastic())` + `Retry` 算子：惰性、可背压、`doOnCancel` 可观测 | Rust 无惰性流等价物：`system_one()` 调用即执行；由此产生的差异只有「取消观测」（已单列 `JUDGE-02`）。改 async 后同一 crate 可被任何 Tokio 宿主直接 await，无阻塞线程池 |
| JUDGE-X06 | 运行期状态槽 | `agentscope_core::RuntimeContext::{set_extension,set_extension_arc,extension}` 键值槽（`src/jev/confirmed_calls.rs:24 CONFIRMED_CALLS_KEY`）；`&RuntimeContext` 不可变，故 `openai_decisions/middleware.rs#run_key` 用 `run_id`→`session_id` 键控的内部 map | Java 用 Reactor `Context`（`ctx.put(Class, value)`）与可变 `RuntimeContext`，`JevStageRouter` 亦以 `ctx.put("jev.stage", ...)` 传阶段 | 未改 core；`RuntimeContext` 不可变使中间件无需 `&mut` 即可跨 hook 传值；`run_key` 的降级键（无 run/session id 时共用默认槽）在源码中显式声明为已知边界 |
| JUDGE-X07 | 静态工具类 → 模块函数 | `src/jev/routing/catalog.rs` 把 Java `JevRouteCatalog` 的静态成员摊平为 `Quota`/`Effort`/`Price`/`Candidate`/`Route`/`Snapshot` 类型 + `excluded()`/`effort()` 自由函数 | Java `JevRouteCatalog` 是纯静态工具类（`public static String excluded(...)`），无实例状态 | Rust 无静态方法宿主：自由函数 + 类型更符合 module 组织，调用点（`src/jev/routing/phase_routing.rs` 的 `excluded`/`effort` 引用）可 `use` 到最小集合；语义与签名一一对应 |
| JUDGE-X08 | 工具注册 | 显式 `register_jev_tools(&Toolkit, JevToolConfig)`（`src/jev/runtime/tools.rs:496`），按 `JevToolConfig` 的 `with_evidence/with_knowledge/with_code_review` 选择性装配 | Java `JevEvidenceTool`/`JevKnowledgeTool`/`JevCodeReviewTool` 以注解式 `@Tool` 方法暴露，由框架扫描注册 | harness/core 无注解扫描机制，显式注册表更可测（`register_jev_tools_wires_every_configured_tool` 断言装配结果）；未注册的工具不会静默出现 |
| JUDGE-X09 | 分层边界 | 一个 crate 两顶层模块：`jev`（纯逻辑 + 子包）+ `openai_decisions`；**仅** `jev::runtime` 依赖 `agentscope-core`/`agentscope-harness`（见 `Cargo.toml` 注释与 `src/jev/runtime/mod.rs` 模块文档） | Java 中 DTO/纯逻辑与 `MiddlewareBase`/`Tool` 实现混在同一包（如 `jev/example/` 直接 `implements MiddlewareBase`） | 纯逻辑层零框架依赖 ⇒ 392 个单测可离线秒级运行，宿主绑定集中在一处；副作用：Java 的 `example/` 包在本 crate 无对应目录，其运行时代码被拆到 `jev::runtime` 与 `openai_decisions::middleware`（JEV 三中间件已入 `jev::runtime::selection`，见 `JUDGE-01`） |
| JUDGE-X10 | `JevStageRouter` 的阶段传递 | `src/jev/application/stage_router.rs:39 Phase{name,goal}` + `:55 JevStageRouter::resolve` 直接走 `JevPhaseRouting` 的 `EXPLICIT_PHASE` 分支 | Java `JevStageRouter.begin` 先 `ctx.put("jev.stage", stage)` 再 `router.onAgent(..., Flux.empty())`，随后两个 hook 全部委托 `JevModelRouterMiddleware` | 显式传递 `Phase` 比隐式 context key 更难误用（未知阶段 fail-closed 为 `EXPLICIT_TARGET_UNAVAILABLE`）；等价于 Java 的「配置了 `JevPhaseRouting` 的 advanced 模式」，legacy 模式差异见 `JUDGE-01` |

## 5. 执行约定（所有会话必须遵守）

- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-judge` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-judge` / `cargo test -p agentscope-extensions-judge` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-judge/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/JUDGE__MIGRATION_HISTORY.md`](./history/JUDGE__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
