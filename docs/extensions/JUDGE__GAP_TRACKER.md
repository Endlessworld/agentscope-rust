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
| Rust 规模（实测 2026-10-28） | **55** 个 `src/*.rs` / **23185** 行 / **286** 个测试（`#[test]` + `#[tokio::test]`；无 `tests/`、无 doc-test、无 `#[ignore]`） |
| 是否注册 workspace | **是**（`Cargo.toml` 的 `crates/agentscope-extensions-judge`） |
| 当前状态 | **总判：进行中**（2026-10-30 更新：中间件 hook / @Tool / `StoreJevContextArchive` 已落地，见 §9；P1 未清零 —— 剩余 `JevPhaseRouting` 的 judge 选择、`JevCodeReviewer` 六阶段驱动、`JevStageRouter`、`JevResponseMiddleware` 的 quality/reviser 分支）。**基本完成**（两个 HTTP 客户端 + `openai-decisions` 整块 + `jev` 八个编排子包的纯函数/定义层/状态机已落地；**jev 去 `browser`(4)/`example`(4) 共 42 类 = 完整 32 / 部分 8 / 骨架 2**） |

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

> `Cargo.toml` 依赖：`serde` / `serde_json` / `thiserror` / `async-trait` / `tokio` / **`reqwest`（workspace 0.13）** / `futures`（runner 的 `buffered` 顺序并发）/ `sha2`（`FileJevContextArchive` 的 SHA-256 内容寻址）；均走 workspace。`reqwest` 只经 `http::JudgeHttpTransport` seam 触达，单测全离线。`agentscope-core` / `agentscope-harness` 自 **2026-10-30** 起以 path 依赖声明（仅 `jev::runtime` 使用：`BaseStore` 归档、`Tool`、`Middleware`，见 §9）；其余模块仍只用本地缝。
>
> **2026-10-28 新增目录**（详见 §8）：`src/jev/{call,judge,code_review}.rs`、`src/jev/evaluation/**`（10 文件）、`src/jev/context/**`（5）、`src/jev/application/**`（4）、`src/jev/evidence/**`（3）、`src/jev/routing/**`（2）、`src/jev/supervision/**`（3）、`src/jev/integration/**`（2）。

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
| `example` | 4 | 1130 | 无 | **设计决定（不迁移）**（2026-10-29 确认；**待汇总方最终确认**）——依据：示例代码非运行链路，且依赖 harness 中间件装配点（`JevModelRouterMiddleware` 等），与主干解耦 |
| `integration` | 3 | 607 | `jev/integration/**` | **基本完成**（`JevKnowledge` 完整；工具/中间件为投影 + 骨架） |
| `review` | 4 | 1332 | `jev/review.rs` + `jev/code_review.rs` | **基本完成**（`ReviewRegions`/`JevReviewInput` 完整；`JevCodeReviewer` 定义层 + 驱动骨架、`JevCodeReviewTool` 骨架） |
| `routing` | 2 | 818 | `jev/routing/**` | **基本完成**（`JevRouteCatalog` 完整；`JevPhaseRouting` 纯 helper + 骨架） |
| `supervision` | 3 | 855 | `jev/supervision/**` | **基本完成**（2 完整；中间件 Schedule/Observation/tail 已迁，观察循环待 seam） |
| `openai-decisions`（DecisionQuestion/Input/Answer/Request/Result/Usage/Client/Exception） | 8 | 795 | `openai_decisions/{mod,error,dto,protocol,client}.rs` | **已完成**（`example/` 除外） |

> **计数口径（2026-10-29 勘误）**：jev 去 `browser`(4) 与 `example`(4) 后共 **42 类 = 完整 32 / 部分 8 / 骨架 2**（明细 12+10+5+3+2+3+3+4 = 42）。§8.1 合计行记「41 类」为加总笔误（其分项 32+8+2 本身为 42），以本节为准。

### 2.2 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / `java.net.http` | `JevClient` / `OpenAIDecisionsClient` HTTP | **`reqwest` 0.13（workspace）**，经 `http::JudgeHttpTransport` 注入 | 已落地；生产用 `ReqwestJudgeTransport`，测试用内存 fake |
| Jackson | JSON（反）序列化 | `serde` + `serde_json` | 已落地 |
| Playwright（`browser` 子包） | 浏览器自动化 | 无 | 与 SANDBOX/CHANNEL 同级判定：**建议「设计决定（不迁移）」**，保留 trait 扩展点 |
| Reactor `Mono` | 异步流 | `async fn` + `tokio::time`（超时/退避） | 已落地；`Mono` 惰性与 `jitter(0.2)` 无对应物（见 §7.3） |
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
| `openai-decisions` 整块 | 8 类 / 795 行 | **已完成** | `src/openai_decisions/**`（5 文件 / 2802 行 / 39 测试）；仅 `example/` 中间件未迁（依赖 harness 中间件装配，见 §3.3） |
| `evaluation` 子包 | 12 类 / 1828 行 | **已完成**（11 类）+ **部分**（中间件 hooks） | 详见 §8.1：`jev/evaluation/**`；`TraceCapture` 状态机全量，`JevTraceEvaluationMiddleware::onAgent/onModelCall` 需 core 中间件 |
| `application` 子包 | 10 类 / 950 行 | **已完成**（9 类）+ **骨架** 1 | 详见 §8.1；`JevStageRouter` 需 `example/JevModelRouterMiddleware` |
| `context` 子包 | 5 类 / 903 行 | **已完成**（4 类）+ **骨架** 1 | 详见 §8.1；`StoreJevContextArchive` 需 harness `BaseStore`（`UnavailableContextArchive` fail-closed） |
| `evidence` 子包 | 3 类 / 744 行 | **已完成**（2 类）+ **部分** 1 | 详见 §8.1；`JevEvidenceTool` 投影已迁，`@Tool` 注册待 seam |
| `routing` 子包 | 2 类 / 818 行 | **已完成**（1 类）+ **部分** 1 | 详见 §8.1；`JevPhaseRouting` 纯 helper 已迁，派发循环待中间件 |
| `supervision` 子包 | 3 类 / 855 行 | **已完成**（2 类）+ **部分** 1 | 详见 §8.1；观察循环待 core 事件流 |
| `integration` 子包 | 3 类 / 607 行 | **已完成**（1 类）+ **部分** 2 | 详见 §8.1；工具注册与 `onModelCall` 待 seam |
| `review` 完整语义 | 4 类 / 1332 行 | **已完成**（2 类）+ **部分** 2 | 详见 §8.1；`JevCodeReviewer` 定义层全量（配置/问题/词汇表/signals/完整性/排序），驱动骨架 |
| 测试覆盖 | 140 个 `@Test` | **已完成（本 crate 口径）** | Rust **286** 个（未忽略，0 `#[ignore]`）；全部离线 |

> **「部分」/「骨架」的含义（2026-10-29 说明，避免误读为「主干未落地」）**：上表的「部分」= 该类**数据模型 / 纯函数 / 状态机 / 校验 / fail-closed 兜底已全部迁移并有单测**，只差需要 core/harness seam 的**中间件 hook（`onAgent`/`onModelCall`）与 `@Tool` 注册**；「骨架」= 接口形状与 fail-closed 语义已落地，驱动链留待 seam。全 crate 实测 **286 passed / 0 failed**（§7.2 / §8.4）。

### 3.3 P2（增强）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `browser` 子包（Playwright） | 4 类 / 1056 行 | **设计决定（不迁移）（2026-10-29 确认；待汇总方最终确认）** | 与 RAG 的 Tika/PDF reader 同理：引入浏览器自动化运行时（Node 驱动 + 真实浏览器）与本 crate「纯 Rust、离线可测」定位不符；保留 trait 扩展点由宿主实现 |
| `example` 子包 | 4 类 / 1130 行 | **设计决定（不迁移）（2026-10-29 确认；待汇总方最终确认）** | 示例代码，非运行链路；且依赖 harness 中间件装配点，与主干客户端解耦。补充：`openai_decisions` 的 `example/`（4 类）同为示例中间件，需要 harness 中间件装配点，本轮未迁 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **已完成**：crate 骨架、错误类型、System One 的 DTO/执行/guardrail/概率/校验/评审纯函数层；**HTTP 传输 seam + 两个客户端**；**`openai-decisions` 整块**；**`jev` 八个编排子包**（`evaluation`/`application`/`context`/`evidence`/`routing`/`supervision`/`integration`/`review` 的纯函数、定义层、状态机与 fail-closed 骨架）。共 55 文件 / 23185 行 / 286 test。
- **验证命令与结果（2026-10-28 二次实测）**：
  - `cargo test -p agentscope-extensions-judge` → **286 passed / 0 failed / 0 ignored**（基线 126 → +160）。
  - `cargo build -p agentscope-extensions-judge` → 成功，0 warning。
  - 反向验证 3 项：见 §8.5。
- **已知问题**：`jev` 的四个中间件 hook（trace evaluation / supervision / response / phase routing）、`@Tool` 注册（evidence/knowledge/code review）、`StoreJevContextArchive`、`JevStageRouter`、`JevCodeReviewer` 驱动仍未落地（均需 core/harness seam，见 §8.6）；无 `tests/` 集成测试；无 live API 测试（全离线）。

### 4.2 下一步与阻塞

1. ~~**P0**：落地 HTTP 客户端~~ → **已完成**（`http.rs` + `jev::client` + `openai_decisions::client`）。
2. ~~**P0**：逐字段核对 `SystemOneRequest`/`SystemOneResult`/`Usage`~~ → **已完成**（见 §2.3，修正 2 处）。
3. ~~**P1**：按依赖顺序迁 `evaluation` → `application` → `context` → `evidence`/`routing`/`supervision`/`integration`~~ → **已完成（2026-10-28，见 §8）**。
4. **需汇总方裁定（2026-10-29 已写清依据，等待确认）**：`browser`（Playwright）与 `example`（jev + openai-decisions 各 4 类）均按「设计决定（不迁移）」关闭——依据分别见 §2.1 / §3.3（浏览器自动化运行时与「纯 Rust、离线可测」定位冲突；示例代码非运行链路且依赖 harness 中间件装配点）。**汇总方确认前不计入缺口。**
5. **跨模块**：四个中间件 hook、`@Tool` 注册、`BaseStore` 归档、`RuntimeContext` 扩展槽需 core/harness 侧 seam（见 §8.6）；本轮未改 `agentscope-core` / `agentscope-harness` / 根 `Cargo.toml`。
6. **真实剩余（2026-10-29 复核；P0 已清零，实测 286 passed）**：
   - 四个中间件 hook 未接：trace evaluation（`JevTraceEvaluationMiddleware::onAgent/onModelCall`）/ supervision（观察循环）/ response（`JevResponseMiddleware`）/ phase routing（`JevPhaseRouting` 派发）。
   - `@Tool` 注册未接：evidence / knowledge / code_review 三个工具只有投影 + fail-closed 校验。
   - `StoreJevContextArchive` 为 fail-closed 骨架（缺 harness `BaseStore`）。
   - `JevStageRouter` 骨架 + `JevCodeReviewer` 的 screen→profile→locate→classify→severity→route 驱动链未接。
   - `JevGuardrail::screen` 无响应式调用方（纯 `evaluate`，Java 由 reactive 链路驱动）。
   - `JevConfirmedCalls` 缺 core `RuntimeContext` keyed 扩展槽。
   - 无 `tests/` 集成测试、无 live API 测试（全离线）。
   - 跨模块建议：core 提供 `Msg` → `TraceMessage`/`ContextMessage` 投影 trait（现由本 crate 自行投影，§8.2 / §8.6）。

## 5. 执行约定（所有会话必须遵守）

- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-judge` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-judge` / `cargo test -p agentscope-extensions-judge` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-judge/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

- **复核方式**：纯静态（Java 逐包统计 + Rust 逐文件统计），**未编译、未跑测试**。
- **口径**：行数为非空行；Rust 测试数为 `#[test]` 属性计数。
- **结论**：Rust 侧当前覆盖 ≈ **3181 / 12469 ≈ 26%**（按行），且集中在 `guardrail` / `validation` / `review` / `dto` 四个纯函数区域；**`openai-decisions` 与 8 个 jev 子包零实现**。
- **状态判定**：`进行中`（非「待办」——已有 57 个测试的纯函数层；非「已完成」——核心 HTTP 链路与过半子包缺失）。
- **注**：本节为 2026-10-22 的**历史快照**；最新实测口径与完成度以 §7 为准（HTTP 客户端与 `openai-decisions` 已于 2026-10-28 落地）。

## 7. 执行记录（2026-10-28）

### 7.1 落地项

| 项 | 产物 | 说明 |
|---|---|---|
| HTTP 传输 seam | `src/http.rs` | `JudgeHttpRequest`（JSON POST：url/headers/body）/`JudgeHttpResponse`/`JudgeTransportError{message,status_code,response_body,retryable}`；`#[async_trait] trait JudgeHttpTransport: Send+Sync`；`ReqwestJudgeTransport`（仅 connect timeout，不重试）；`backoff_ms(initial, retry_index)` = `initial*2^n` 上限 `8x`（Java `maxBackoff(initial*8)`）；`#[cfg(test)] testing::{ScriptedTransport, HangingTransport}` |
| `JevClient` | `src/jev/client.rs` | `system_one(&SystemOneRequest)`：校验 → 有效 model 替换（null/blank 回落）→ `POST /v1/systemone`（`Authorization: Bearer`、`Accept`、`Content-Type: application/json`）→ 每次尝试 `tokio::time::timeout` → 状态映射 → 解析 → `validate_result` → 仅可重试错误按 `RetryPolicy` 退避；`JevClientBuilder`（api_key/base_url/model/transport/retry_policy/timeout）+ `normalize_base_url`；`Debug` 不泄露 api key |
| `openai_decisions` | `src/openai_decisions/{mod,error,dto,protocol,client}.rs` | 8 类全覆盖（`DecisionSupport` 等 4 个 `example` 中间件除外）：`DecisionValue`(String|Bool)、`DecisionInput`(≤128 图、data/http(s) 校验)、`DecisionQuestion`(Predicate/Choice 2–255 唯一值/Score ≥1 级)、`DecisionRequest`(+Builder，1–400 题、名称唯一/非空)、`DecisionAnswer`(Predicate/Choice/Score/Refusal)、`DecisionResult`、`DecisionUsage`(6 个可空计数)、`DecisionsError`、`OpenAIDecisionsClient`(+Builder，默认 `disabled` 重试) |
| 请求体/响应协议 | `src/openai_decisions/protocol.rs` | `decision_body`（文本→字符串；含图→单条 user message；`questions` 为 `type` 标记数组；`choices`/`levels` 的 `description` 可选）+ `parse_decision_response`（题目数、名称未知/重复、类型不匹配、选项/等级覆盖、重复/未知值、和=1（±1e-6×n）、score=加权期望）、17 个 Java 消息常量 |

### 7.2 验证命令与实测

| 命令 | 结果 |
|---|---|
| `cargo build -p agentscope-extensions-judge` | 成功（`Finished dev profile`） |
| `cargo test -p agentscope-extensions-judge` | **126 passed / 0 failed / 0 ignored**（基线 57 → +69；0 个 `#[ignore]`） |
| `cargo clippy -p agentscope-extensions-judge --all-targets` | 0 warning / 0 error |
| `cargo fmt -p agentscope-extensions-judge` | 已执行，无 diff 遗留 |

新增测试（按文件）：`http.rs` 8（头部替换/大小写、2xx 判定、传输错误上下文、退避倍增与封顶、溢出饱和、脚本化 fake 重放/耗尽/重复）；`jev/client.rs` 20（URL/头/body 断言、model 覆盖与 blank 回落、401 消息+单次、429 重试成功、529 耗尽 3 次、408/409 重试、4xx 不重试、传输错误透传、超时、请求校验先于网络、`null` 响应、解析失败、契约失败不重试、builder 校验、URL 归一化、Debug 脱敏、`validate_response` 负 usage）；`openai_decisions/dto.rs` 7（输入/图片、choice/score 边界、request 边界与 builder、拒绝空/重复/非法）；`protocol.rs` 13（body 三种形态、全类型解析、refusal、usage 缺省、畸形体、39 条非法响应逐条、blank model、概率和容差、score 期望、选中项越界）；`client.rs` 16（body/头/URL、model 覆盖+cache key、URL 归一化、危险 URL 拒绝、builder 校验、无 key 不发 Authorization、HTTP 状态+脱敏+默认不重试、传输错误脱敏、超时、opt-in 重试、非法响应不重试、契约错误透传、重试策略、Debug 脱敏）。

### 7.3 反向验证（逐个回退实现 → 确认 FAILED → 恢复）

| 回退点 | 失败用例 | 结果 |
|---|---|---|
| `jev::client::is_retryable_status` 去掉 408/409 | `retries_gateway_timeouts_and_conflicts` | ✅ FAILED 如预期 |
| `Usage.input_tokens` 去掉 `serde(default)` | `usage_defaults_missing_counters_to_zero_like_a_java_primitive` | ✅ FAILED 如预期 |
| `openai_decisions::protocol::validate_sum` 直接返回 Ok | `rejects_every_out_of_contract_response` | ✅ FAILED 如预期 |
| `JevClient::system_one` 去掉 `!error.is_retryable()` 守卫 | `does_not_retry_client_errors` | ✅ FAILED 如预期 |

恢复后全量复跑：126 passed / 0 failed。

### 7.4 与 Java 的已知偏差（均为有意决定，非缺口）

1. **JEV 可重试状态码**：Java 仅 `429/529/5xx`；本 port 按任务指令扩展为 `408/409/429/529/5xx`（网关超时/并发冲突）。`RetryPolicy::is_retryable_status`（Java 逐字对照版）与 `JevError::http` 保持原样，二者并存并有注释说明。Decisions 保持 Java 逐字：`429/5xx`（Java 测试断言 `retryable == (429 || >=500)`）。
2. **退避抖动**：Java `Retry.backoff(...).jitter(0.2)` 带 ±20% 抖动；Rust 退避确定性（无 `rand` 依赖），上限仍为 `8×initial`，便于测试断言精确尝试次数。
3. **`Mono` 惰性/取消**：Java 返回 `Mono`（订阅才执行、可取消并中断传输）；Rust 为 `async fn`，取消由调用方 runtime 负责（drop future）。未提供 `system_one_blocking` / `decide_blocking` 等价物。
4. **`JevError` 未新增 `Http`/`Api` 变体**：现有 `Protocol{status_code,response_body,retryable}` 已覆盖 Java 单一 `JevException` 的全部语义，新增变体会造成语义重复；Decisions 侧则按 Java `OpenAIDecisionsException` 建了独立 `DecisionsError`（无可泄漏的 `response_body`）。
5. **响应 `type` 交换的报错层级**：Java SDK 字段全可空，`{"type":"choice"}` 配 predicate 字段会走到 `parseAnswer` 报 "Answer type does not match question type"；serde 因变体字段必填先报 "Unable to parse Decisions response"。两者同为不可重试 API 错误且不泄漏 body，仅消息层级不同（已在 §7.2 的 38 条非法响应表中标注）。
6. **重复 JSON 键**：Java SDK 启用重复键检测（`{"model":"a","model":"b"}` 拒绝），`serde_json` 默认后者覆盖。未引入额外 feature；影响面为畸形响应，不涉及正常路径。
7. **Decisions 消息文本**：Java 传输失败消息为 "Decisions SDK request failed"，Rust 为 "Decisions request failed"（无 SDK 概念）；HTTP/超时/解析三条消息逐字对齐。

### 7.5 未落地项

> 已在 **2026-10-28 第二轮**基本清零；最新逐类台账见 **§8**。仍明确未落地的部分是：

- **中间件 hook**：`JevTraceEvaluationMiddleware::onAgent/onModelCall`、`JevSupervisionMiddleware` 观察循环、`JevResponseMiddleware::onModelCall`、`JevPhaseRouting::onModelCall`（纯状态机/表已就位）。
- **`@Tool` 注册**：`JevEvidenceTool`、`JevKnowledgeTool`、`JevCodeReviewTool`（投影函数已就位）。
- **`StoreJevContextArchive`**（harness `BaseStore`）、`JevStageRouter`（`example/JevModelRouterMiddleware`）、`JevCodeReviewer` 分阶段驱动。
- `openai_decisions/example/`（4 类示例中间件）、`browser/`（4 类 Playwright）。
- `JevGuardrail::screen` 响应式调用方、`JevConfirmedCalls`（需 core `RuntimeContext` 扩展槽）。
- 无 `tests/` 集成测试、无 live API 测试（全部离线）。

### 7.6 跨模块需求

**第一轮：无**（`openai-decisions` 主干 8 类不依赖 harness 的 subagent/team 抽象）。**第二轮新增 4 项**，见 §8.6（core middleware hook 签名、`Msg` 投影 trait、harness `BaseStore` 适配、`RuntimeContext` keyed 扩展槽）；本轮仍未改 `agentscope-core` / `agentscope-harness` / 根 `Cargo.toml`。

## 8. 执行记录（2026-10-28，jev 八个编排子包）

> 口径：**完整**=核心语义 1:1；**部分**=纯函数/定义层与状态机已迁，仅剩需要 core/harness seam 的驱动层；**骨架**=类型与校验已给出并 fail-closed（`Err(protocol(...))`），无 `todo!()`。

### 8.1 逐子包：已迁移类 / 未迁移类 + 原因

| 子包 | Rust 落点 | 已迁移 | 未迁移 / 受限 | 原因 |
|---|---|---|---|---|
| `evaluation`（12） | `jev/evaluation/**`（`trace_data` `metric_result` `metric` `trace` `metrics` `evaluator` `runner` `evaluation_runner` `text_backend` `capture`） | `TraceData` `JevMetricResult` `JevTraceMetric` `JevTrace` `JevTraceMetrics` `JevTraceEvaluator` `JevTraceRunner` `Evaluator` `JevEvaluator` `JevEvaluationRunner` `JevTextBackend`（**11 完整**） | `JevTraceEvaluationMiddleware`（**部分**：`TraceCapture` 全部状态机 + 停止原因 + 上限 + `evaluate` 已迁；`onAgent`/`onModelCall` 未迁） | 中间件 hook 属 core `MiddlewareBase`，本 crate 不依赖 core |
| `application`（10） | `jev/application/{selector,planners,draft_pipeline,stage_router}.rs` | `JevCandidateSelector` `JevBrowserPlanner` `JevContextPlanner` `JevTeamPlanner` `JevRag` `JevMemoryGate` `JevSupervisor` `JevCustomerSupport` `JevDraftPipeline`（**9 完整**） | `JevStageRouter`（**骨架**） | 依赖 `example/JevModelRouterMiddleware` + core `RuntimeContext` |
| `context`（5） | `jev/context/**` | `JevContextArchive`（trait+Scope+envelope） `CompactionTranscript` `JevContextCompactor` `FileJevContextArchive`（**4 完整**） | `StoreJevContextArchive`（**骨架**：`UnavailableContextArchive` fail-closed） | 依赖 harness `BaseStore` |
| `evidence`（3） | `jev/evidence/**` | `JevPassage` `JevEvidenceProcessor`（阈值/四问/分类顺序/请求与状态限额/`Call` 记录/排序与交付折叠）（**2 完整**） | `JevEvidenceTool`（**部分**：`evidence_result` 投影 + 限额校验已迁；`@Tool` 注册未迁） | 需 core `Toolkit`/`RuntimeContext` |
| `routing`（2） | `jev/routing/**` | `JevRouteCatalog`（`Quota`/`Effort`/`Price::estimateUsd`/`Candidate`/`Route`/`Snapshot`/`excluded`/`effort` 全量）（**1 完整**） | `JevPhaseRouting`（**部分**：`input_restriction`/`candidate_state`/配置校验已迁；`onAgent`/`onModelCall`/重校验/派发未迁） | 需 core 中间件 + `ModelCallInput`/`AgentEvent`/`GenerateOptions` |
| `supervision`（3） | `jev/supervision/**` | `SupervisionEvidence`（+`Verification` scope/revision 校验） `JevTaskSupervisor`（`checks`/`arbitrate`/`uncertainForDecision`/`select`/状态与建议元数据全量）（**2 完整**） | `JevSupervisionMiddleware`（**部分**：`Schedule`/`Observation`/`EvidenceRequest`/`append_tail` 已迁；观察循环未迁） | 需 core 事件流与 bounded-elastic 定时 |
| `integration`（3） | `jev/integration/**` | `JevKnowledge`（`KnowledgeSource` 缝替代已废弃的 core `Knowledge`；ACL-优先 + `ENFORCE` 候选限额 + SHADOW/ENFORCE 语义）（**1 完整**） | `JevKnowledgeTool`（**部分**：`knowledge_passages` 投影 + 限额）；`JevResponseMiddleware`（**部分**：`quality_state`/`conversation_text`/`ResponseLimits`/`Rejected` 文案） | 需 core `@Tool`/`onModelCall`/`TextBlockDelta` |
| `review`（4） | `jev/review.rs` + `jev/code_review.rs` | `JevReviewInput` `ReviewRegions`（既有） `JevCodeReviewer` 定义层（`ReviewConfig` 校验、5 维/优先级/严重度常量、`screening_questions`、`categories`、`mechanisms`、`owners`、`signals`、`snippet`、`ReviewReport::findings`、`compute_complete`）（**2 完整 + 2 部分**） | `JevCodeReviewer` 的 screen→profile→locate→classify→severity→route 驱动（**部分**）；`JevCodeReviewTool`（**部分**：校验 + fail-closed） | 需 core `RuntimeContext` 与分阶段预算管线 |

合计：41 → **完整 32 / 部分 8 / 骨架 2**（不含 `browser` 4 类与 `example` 4 类，仍维持既有判定）。

附带补上根包 `JevJudge`（`jev/judge.rs`，174 行 Java → `JevJudge`/`Definition`/`Criterion`/`JudgeResult`/`ErrorCode`）：`evaluation::JevEvaluator` 与 `application` 的六个工作流均依赖它，属必要依赖而非额外扩大范围。

### 8.2 新增的注入缝（§4.2 相关）

| 缝 | 位置 | 用途 |
|---|---|---|
| `Caller` / `CallFailure` / `execute_guarded` | `jev/call.rs` | Java `Function<SystemOneRequest, Mono<SystemOneResult>>` 的 Rust 对应物；`InvalidReply{model,usage}` 保留可计费 usage（不伪造零成本） |
| `Caller` 适配 | `caller_from_client(JevClient)` | 生产路径复用既有 HTTP 客户端（超时/重试/脱敏不变） |
| `TraceMessage` / `TraceBlock` | `jev/evaluation/trace.rs` | `Msg` 的中性投影（本 crate 不依赖 core） |
| `ContextMessage` / `ContextBlock` | `jev/context/message.rs` | 压缩器改写的消息中性投影 |
| `JevContextArchive` | `jev/context/archive.rs` | Java `JevContextArchive`；`FileJevContextArchive` 为真实局部实现（SHA-256 + 原子发布 + scope 隔离） |
| `KnowledgeSource` | `jev/integration/knowledge.rs` | 替代已废弃的 core `Knowledge` SPI |
| `RouteRequest` | `jev/routing/catalog.rs` | `ModelCallInput` 中影响可路由性的字段（tools/max tokens/effort/pinned/non-text） |

### 8.3 与 Java 的偏差（有意决定）

1. **消息类型**：`Msg` → `TraceMessage`/`ContextMessage` 中性投影；角色用字符串（`MsgRole.name()` 拼写），避免猜测 core 枚举。
2. **句子切分**：`JevTraceMetrics.claims` 用确定性终止符切分替代 `java.text.BreakIterator`；显式 `claims` 列表两边都支持。
3. **流式 → 整段**：`JevDraftPipeline` 的 `Flux<String>` 生成/修订改为返回整段文本，`max_chars` 在整段上校验（结果枚举不变）。
4. **`JevDraftPipeline` / `JevCandidateSelector` 等校验错误**：Java 同步抛 `IllegalArgumentException`，Rust 返回 `Err(JevError::InvalidArgument)`；执行期失败仍折叠为 `Decision{status=Error}`。
5. **`JevContextCompactor`** 不实现 harness `ConversationCompactionStrategy`；以 `CompactionRequest` 为缝（`compact()` 对非 `ENFORCE` 返回 `value=None`，`ENFORCE` 且非 `DECIDED` 回退原消息）。
6. **`JevTraceEvaluator`** `reduce` 返回 `Result`（Java 抛异常 → `REDUCE_ERROR`），`Report::result(id)` 返回 `Option`。
7. **`JevPhaseRouting`** `onModelCall` 的 `RouterSource`/`share()` 重放语义未迁（无对应中间件）；`resolve()` fail-closed。
8. **`ports`**：`Effort` 顺序与 Java ordinal 严格一致（`None<Minimal<Low<Medium<High<Xhigh<Max<Ultra>`）。

### 8.4 验证命令与实测

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-extensions-judge` | **286 passed / 0 failed / 0 ignored**（基线 126 → +160） |
| `cargo build -p agentscope-extensions-judge` | 成功，0 warning |
| 规模 | 55 个 `src/*.rs` / 23185 行 / **0** `#[ignore]` |

新增测试按模块：`jev/call.rs` 4；`jev/judge.rs` 7；`evaluation` 69（trace_data 3 / metric_result 3 / trace 5 / metrics 11 / evaluator 10 / runner 8 / evaluation_runner 7 / text_backend 8 / capture 9 / …）；`context` 26（message 2 / archive 4 / file_archive 5 / transcript 8 / compactor 7）；`application` 25（selector 5 / planners 12 / draft_pipeline 5 / stage_router 1）；`evidence` 11；`routing` 7；`supervision` 14；`integration` 7；`code_review` 6。

### 8.5 反向验证（回退实现 → 确认 FAILED → 恢复）

| 回退点 | 结果 |
|---|---|
| `Thresholds::assess` 恒判 `Decided` | `jev::evaluation::metrics` **FAILED（2 个）**，恢复后 11 passed |
| `CompactionTranscript::apply` 恒返回原消息（`DROP`/`TRUNCATE` 失效） | `jev::context` **FAILED（4 个）**，恢复后 26 passed |
| `EvidenceProcessor::classify` 去掉“注入优先”早退 | `jev::evidence` **FAILED（1 个）**，恢复后 11 passed |

注：`Copy-Item` 恢复会保留旧 mtime，需重写文件时间戳后再跑，否则 cargo 复用旧产物（本轮已踩坑并规避）。

### 8.6 跨模块需求（§4.2 交给汇总方）

1. **core middleware 适配层**：`onAgent`/`onModelCall` 的通用 hook 签名（`MiddlewareBase` 等价物）——一旦具备，可同时接上 `JevTraceEvaluationMiddleware`/`JevSupervisionMiddleware`/`JevResponseMiddleware`/`JevPhaseRouting` 四个观察/拦截点（纯状态机已就位）。
2. **core `Msg` ↔ `TraceMessage`/`ContextMessage` 适配**：建议由 core 提供 `From<&Msg> for ...` 风格的投影 trait，否则每个宿主都要手写。
3. **harness `BaseStore` 适配**：`StoreJevContextArchive` 需要 `put_if_version/get/search` 三方法（`UnavailableContextArchive` 现为 fail-closed 占位）。
4. **`RuntimeContext` 扩展槽**：`JevConfirmedCalls`、`EvidenceSource`、`Source`（routing snapshot）等仍依赖 keyed 扩展槽。
5. **2026-10-30 更新（§9）**：`BaseStore` 归档、@Tool 三层、四个中间件 hook 已落地（`src/jev/runtime/**`）；仍剩 `JevPhaseRouting` 的 judge 选择、`JevCodeReviewer` 六阶段驱动、`JevStageRouter`、`JevResponseMiddleware` 的 quality/reviser 分支，原因见 §9.5。第 3/4 条中的 `EvidenceSource`/`Source` 注入改用工具构造期持有（`ToolExecutionContext` 无 `extensions` 包）。

## 9. 执行记录（2026-10-30）— 中间件 hook / @Tool / 归档 / 路由派发

> 本轮把 §8.6 列出的三个 core/harness seam 真正接上：`StoreJevContextArchive`、`@Tool` 注册层、四个中间件 hook。全部绑定集中在新模块 `src/jev/runtime/**`，**纯函数层签名与测试不变**（仅两处 `&dyn Fn` 加 `+ Sync`，见 §9.2）。

### 9.1 落地项

| 项 | Rust 产物 | Java 依据 | 说明 |
|---|---|---|---|
| `StoreJevContextArchive` | `src/jev/runtime/archive.rs` | `jev/context/StoreJevContextArchive.java` | 命名空间 `["jev","context", sha256(user), sha256(agent), sha256(session)]`；内容寻址 `ref = sha256(snapshot JSON)`；写入用 `BaseStore::put_if_version(..., 0)` create-if-absent，失败时回读比对 `snapshot` 字节，不一致即 `archive requires acknowledged create-if-absent storage`；`restore` 校验引用格式 / 大小 / 校验和 / `version==1` / scope 相等；**6 条 Java 消息逐字对齐**（`positive snapshot limit required` / `snapshot limit` / `missing or invalid snapshot` / `snapshot scope mismatch`） |
| `@Tool`：`search_evidence` | `src/jev/runtime/tools.rs` `EvidenceSearchTool` | `jev/evidence/JevEvidenceTool.java` | 名称 / 描述 / 参数 schema（`query`，required）逐字；scope 缺失 → `EVIDENCE_SCOPE_REQUIRED`；检索结果为空 decision → `EVIDENCE_SOURCE_UNAVAILABLE`；返回 Java 同构 `{status, passages[{id,version,text,classification,score}]}` |
| `@Tool`：`search_knowledge` | `tools.rs` `KnowledgeSearchTool` | `jev/integration/JevKnowledgeTool.java` | 复用 `JevKnowledge::retrieve(query, limit, acl)`（ACL 优先 + ENFORCE 候选限额 + SHADOW/ENFORCE）；返回 `[{id,text}]` |
| `@Tool`：`review_code_snapshot` | `tools.rs` `ReviewCodeTool` | `jev/review/JevCodeReviewTool.java` | scope → `REVIEW_SCOPE_REQUIRED`；快照 id 空/超 512/含控制字符 → `INVALID_SNAPSHOT_ID`；快照由宿主 `CodeReviewSource` 解析（非文件路径）；六阶段驱动未接时 `JevCodeReviewerDriver` 保持 fail-closed（§9.5） |
| `register_jev_tools(&Toolkit, cfg)` | `tools.rs` | — | 返回注册名列表；测试断言注册后 `Toolkit::get_tool` 可取到三个 |
| `JevTraceEvaluationMiddleware` | `runtime/middleware.rs` `TraceEvaluationMiddleware` | `jev/evaluation/JevTraceEvaluationMiddleware.java` | `on_agent` 开 capture、透传事件流、`AGENT_RESULT.reply` 触发 `finish`，流结束后 `evaluate`（有界、post-run）并回调 observer，不改事件；`on_model_call` 用 `observe` 喂快照；`order()==300`；`OFF` 纯透传 |
| `JevResponseMiddleware` | `middleware.rs` `ResponseMiddleware` | `jev/integration/JevResponseMiddleware.java` | `on_model_call`：`OFF` 透传；`SHADOW` 累积 `TEXT_BLOCK_DELTA` 后仅观察；`ENFORCE` 缓冲整条流 → screen → 未拦截重放、拦截则替换为 `Err(Agent("OUTPUT_BLOCK: ..."))`（等价 Java `Rejected` 使本次调用失败）；`order()==200` |
| `JevSupervisionMiddleware` | `middleware.rs` `SupervisionMiddleware` | `jev/supervision/JevSupervisionMiddleware.java` | `on_agent` 开 run（`Scope` from user/session/run_id）、`on_model_call` 按 `Schedule` 门控把用户请求与 `TEXT_BLOCK_DELTA` 追加进有界 tail（`append_tail`），流结束后调 `Auditor` 并发布 `Observation`；`order()==310` |
| `JevPhaseRouting` 派发 | `middleware.rs` `PhaseRoutingMiddleware` | `jev/routing/JevPhaseRouting.java` | `on_model_call`：`OFF` 透传；`input_restriction`（pinned：api_key/base_url/endpoint_path/**model_name**；non-text 块扫描）命中即原样放行；否则调注入 `RouteResolver`，`ENFORCE` 下把 `report.model` → `options.model_name`、`report.effort` → `options.reasoning_effort` 后再 `next`，并记录 `report(run_key)`；resolver 失败不改变目标 |

依赖：`crates/agentscope-extensions-judge/Cargo.toml` 新增 `agentscope-core = { path = "../agentscope-core" }`、`agentscope-harness = { path = "../agentscope-harness" }`（仅 `jev::runtime` 使用，无 workspace 版本锁定需求）。

### 9.2 结构决定与对既有代码的最小改动

1. **绑定集中在 `jev::runtime`**（`mod.rs` / `archive.rs` / `tools.rs` / `middleware.rs`），四个中间件与三个工具都是**新类型**，包装既有纯状态机；`evaluation/capture.rs`、`integration/response.rs`、`supervision/middleware.rs`、`routing/phase_routing.rs`、`evidence/tool.rs` 的公开 API 与测试均未改。
2. **两处签名收紧**（为使 `async_trait` 生成的 future 满足 `Send`）：`evidence/processor.rs::process(authorized: &dyn Fn(&JevPassage) -> bool)` → `&(dyn Fn(&JevPassage) -> bool + Sync)`；`integration/knowledge.rs::retrieve(acl: &dyn Fn(&KnowledgeDocument) -> bool)` → `&(dyn Fn(...) -> bool + Sync)`。语义不变，既有调用点（闭包）自然满足。
3. **结果序列化**：`code_review.rs` 的 `Screening` / `Profile` / `Signal` / `Disposition` / `ReviewAdvice` / `Finding` / `FollowUp` / `ReviewCall` / `ReviewReport` 补 `serde::Serialize`（结构体 `rename_all = "camelCase"`、枚举 `SCREAMING_SNAKE_CASE`），使工具返回与 Java record 同构。
4. **身份来源**：`ToolExecutionContext.runtime` 的 `userId`/`sessionId`（core `tool_runtime` 写入，另兼容 `user_id`/`session_id`），与 harness `artifact_delivery` / `wait_async` 一致。

### 9.3 测试与验证

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-extensions-judge -j 2` | **308 passed / 0 failed / 0 ignored**（基线 286 → +22） |
| `cargo build -p agentscope-extensions-judge -j 2` | EXIT=0，**本 crate 0 warning**（core/harness 的既有 warning 与本次无关） |

新增测试（22 个，全离线）：

- `jev::runtime::archive`（6）：`namespace_hashes_each_scope_component`（三个 SHA-256 用**跨实现算出的字面量**，与实现不同源）、`save_then_restore_round_trips_and_is_content_addressed`、`limits_and_references_are_enforced`、`a_store_without_create_if_absent_support_is_rejected`、`a_snapshot_for_another_scope_is_rejected`、`tampered_bytes_are_rejected`。
- `jev::runtime::tools`（8）：`schemas_match_the_java_tool_parameters`、`tool_scope_requires_both_ids`、`evidence_tool_returns_delivered_passages_only`、`evidence_tool_fails_closed_without_scope_or_query`、`knowledge_tool_projects_id_and_text_after_the_acl`、`review_tool_validates_scope_and_snapshot_id`、`register_jev_tools_wires_every_configured_tool`、`reviewer_driver_is_fail_closed_until_the_staged_driver_lands`。
- `jev::runtime::middleware`（8）：`trace_projection_carries_roles_blocks_and_tools`、`trace_middleware_off_is_a_passthrough`、`trace_middleware_shadow_opens_and_closes_a_capture`、`response_middleware_off_passes_through_and_enforce_withholds`、`supervision_middleware_publishes_one_bounded_observation`、`phase_routing_applies_the_resolved_model_and_effort_in_enforce`、`phase_routing_skips_pinned_and_non_text_calls`、`reasoning_hook_is_untouched`。

### 9.4 语义等价 / 差异（§8.6 更新）

| 项 | Java | Rust | 判定 |
|---|---|---|---|
| hook 可用性 | `onAgent` / `onModelCall` | `Middleware::on_agent` / `on_model_call` | **等价**，无需替代 hook（`§8.6` 第 1 条已解除） |
| 跨 hook 状态传递 | Reactor `Context` | 中间件内部 `Mutex<HashMap<run_id 或 session_id>>` | 等价（`&RuntimeContext` 不可变，无法写 `Extensions`）；并发多个 run 互不干扰 |
| trace 评价时机 | `concatWith`（流末尾、有界） | 流 `.chain(once(async ...))` 末尾 await | 等价 |
| response 拦截 | `ENFORCE` 缓冲后 `Rejected` | 缓冲后替换为 `Err(Error::Agent(...))` | 等价（本次调用失败）；`on_model_call` 已产生的更早事件无法撤回，与 Java 相同（Java 也只在 `ENFORCE` 缓冲） |
| response quality/reviser | `assess` + `reviser` + 最多 `maxRevisions` 轮 | **未接**（只用 `safety` 分支） | 差异，见 §9.5 |
| supervision 调度 | boundedElastic + jitter 定时 | 调用数 / `periodic_interval_millis` 门控（`SystemTime`） | 语义近似（无定时器线程）；同一 `Schedule` 结构 |
| routing 快照 | `Source.read` 读宿主快照 | `RouteResolver` 注入（`Snapshot` 由注入方提供，`output_reserve` 取 `max_tokens`） | 适配层差异；`pinned_model` 取 `options.model_name` |
| `JevEvidenceTool.Source` / `JevKnowledgeTool` 的 ACL | 从 `RuntimeContext` 取 `Source` | 构造期注入（`EvidenceSource` trait / `acl` 闭包） | 差异：`ToolExecutionContext` 无 `Extensions`；scope 仍来自调用上下文，失败码不变 |

### 9.5 未完成项与确切原因

1. **`JevPhaseRouting` 的 judge 选择（`resolve()` 仍 fail-closed）**：缺 (a) `JevPhaseRouting` 的 `routes()` 访问器；(b) route offer 组装 + `route`/`none` Choice 题 + 二次校验（`excluded` 重跑）＋ `share()` 重放语义。中间件侧的判定、注入 resolver、`ENFORCE` 覆盖与报告记录已可用。
2. **`JevCodeReviewer` 六阶段驱动**（screen→profile→locate→classify→severity→route）：Java 1099 行、5 类 SystemOne 调用 + 每阶段预算/限额/完整性汇总；本轮未接，`CodeReviewDriver` 缝 + `JevCodeReviewerDriver` 保持 fail-closed（有反向测试）。
3. **`JevStageRouter`**：依赖 `example/JevModelRouterMiddleware`（示例子包，未迁）与显式阶段入口 `jev.stage`。
4. **`JevResponseMiddleware` 的 quality 分支**：`quality_state` 纯函数已在，但还需 `JevJudge` 判定 + reviser 重写 + `maxRevisions` 循环 + `replaceText`（改写已发出的 delta）。
5. **`JevGuardrail::screen` 的 reactive caller**、**`JevConfirmedCalls`**：仍需宿主提供 SystemOne 调用与运行级确认记录。
6. **定时监督**：无 bounded-elastic 等价物，`Schedule` 的 jitter 语义只保留上界。

### 9.6 反向验证（回退实现 → FAILED → 恢复）

| 回退点 | 结果 | 恢复后 |
|---|---|---|
| `archive::save` 去掉 create-if-absent 回读比对 | `jev::runtime::archive` **FAILED 1**（`a_store_without_create_if_absent_support_is_rejected`） | 6 passed |
| `tools::validate_snapshot_id` 恒拒绝 | `jev::runtime::tools` **FAILED 1**（`review_tool_validates_scope_and_snapshot_id`） | 8 passed |
| `ResponseMiddleware` 的 `ENFORCE` 恒不拦截 | `jev::runtime::middleware` **FAILED 1**（`response_middleware_off_passes_through_and_enforce_withholds`） | 8 passed |

另有一处**真实缺陷**由测试当场捕获并修复：`PhaseRoutingMiddleware::restriction` 初版漏掉 `options.model_name`（Java 的 pinned 检查含 model 名），`phase_routing_skips_pinned_and_non_text_calls` FAILED → 补齐后通过。

### 9.7 遗留提醒

- `UnavailableContextArchive` 保留（无 store 的宿主仍需 fail-closed 占位）；`HARNESS_STORE_ARCHIVE_UNAVAILABLE` 文案仍指「本 crate 不依赖 harness」，现已在 §1.1/§9.1 说明该前提仅对 `archive.rs` 之外成立。
- `Cargo.lock` 因新增两个 path 依赖变化属预期，未 stage、未还原。
