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
| Rust 规模（2026-11-01 重测：`src` **66** 个文件 / **32461** 行 / **392** 个测试；另 `tests/runtime_integration.rs` 1 文件 / 430 行 / 5 个集成用例；`#[ignore]` 共 **0** 个） | **2026-10-31 实测 392 passed / 0 failed**，另 5 个集成用例 |
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

## 4. 项目状态与下一步

### 4.1 当前状态

- **已完成**：crate 骨架、错误类型、System One 的 DTO/执行/guardrail/概率/校验/评审纯函数层；**HTTP 传输 seam + 两个客户端**；**`openai-decisions` 整块（含 `example/` 三个中间件，见 history/JUDGE__MIGRATION_HISTORY.md）**；**`jev` 八个编排子包全量**（见 history/JUDGE__MIGRATION_HISTORY.md）；**`jev::runtime` 四个中间件 hook + 三个 `@Tool` + `StoreJevContextArchive` + 六阶段 reviewer 驱动 + `JevStageRouter`**；**2026-10-31 收口**：supervision 结果分类（Java `6ec57d245`）、`JevGuardrail::screen` 调用方（`screening.rs`）、`JevConfirmedCalls`（`confirmed_calls.rs`）、`tests/runtime_integration.rs`（见 history/JUDGE__MIGRATION_HISTORY.md）；**2026-11-01**：Java `31c7db8c9` 的 `OpenAIDecisions{ModelRouter,ToolSelection,AutoMode}Middleware` + `DecisionSupport`（`openai_decisions/{support,middleware}.rs`，见 history/JUDGE__MIGRATION_HISTORY.md），`16cb024ae` 裁定为无需等价实现（见 history/JUDGE__MIGRATION_HISTORY.md）。文件数/行数/测试数以 §1 的 2026-11-01 重测为准（66 `src` 文件 / 32753 行 / 392 test + 5 集成用例）。
- **验证命令与结果**：2026-10-31（主智能体实测）`cargo test -p agentscope-extensions-judge -j 2` → **392 passed / 0 failed / 0 ignored**、`cargo build -p agentscope-extensions-judge` → 0 warning（见 history/JUDGE__MIGRATION_HISTORY.md）。另 `tests/runtime_integration.rs` 5 个集成用例同样全绿。
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

## 5. 执行约定（所有会话必须遵守）

- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-judge` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-judge` / `cargo test -p agentscope-extensions-judge` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-judge/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/JUDGE__MIGRATION_HISTORY.md`](./history/JUDGE__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
