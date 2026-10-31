# JUDGE 迁移执行历史（归档）

> 本文件是 [`../JUDGE__GAP_TRACKER.md`](../JUDGE__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 执行记录（2026-10-28） · 8. 执行记录（2026-10-28，jev 八个编排子包） · 9. 执行记录（2026-10-30）— 中间件 hook / @Tool / 归档 / 路由派发 · 10. 2026-10-30 执行记录（二）— phase routing / quality·reviser / reviewer 驱动 / stage router · 11. 2026-10-31 执行记录（loop 收口：supervision 结果分类 / screening 调用方 / confirmed calls） · 12. 2026-11-01 执行记录（OpenAI Decisions 中间件 / `6ec57d245` 复核 / `16cb024ae` 裁定））。

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

---

## 10. 2026-10-30 执行记录（二）— phase routing / quality·reviser / reviewer 驱动 / stage router

> 本轮代码由子智能体完成；**子智能体回执丢失（返回空）且未改本 tracker**，§10 与反向验证由主智能体根据落盘代码与实测补齐。
> 未改根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**`、其它 extension crate。

| # | 项 | 结果 | 产物 |
|---|---|---|---|
| 1 | `JevPhaseRouting` 的 judge 选择与二次校验 | **已完成** | `src/jev/routing/phase_routing.rs`（+1221）：`routes()` 访问器、offer 组装（allowlist / exclusion）、命中后二次校验、`None` 路由不适用、低置信度仅报告不采用、skipped 候选带 Java reason code |
| 2 | `JevResponseMiddleware` 的 quality / reviser 分支 | **已完成** | 新增 `src/jev/runtime/quality.rs`（25 KB）：`ScreenPolicy`（复用既有 `Screener` 缝）、`Revision`、`QualitySettings`、`revise_with_model`（模型调用走注入缝）、`capture`/`collect_text`/`replace_text`；`runtime/middleware.rs` 接 OFF / SHADOW / ENFORCE 三态 |
| 3 | `JevCodeReviewer` 六阶段驱动 | **已完成（驱动层）** | 新增 `src/jev/runtime/review_driver.rs`（35 KB）：`ReviewDriver::{new, from_reviewer, config, mode, review}`，按 Java 顺序串行执行六阶段、`StageFailure` 短路、错误传播与结果聚合 |
| 4 | `JevStageRouter` | **已完成（不含 legacy 模式）** | `src/jev/application/stage_router.rs`（+230）：`Phase` / `JevStageRouter::{new, begin, metadata_key, routing}`；**legacy 模型选择模式不迁**（`example` 子包依赖 core `Model` 句柄与 eligibility 谓词），以常量 `STAGE_ROUTER_UNAVAILABLE` 显式说明，并提供 `JevPhaseRouting` 显式阶段作为替代 |

**测试**：`cargo test -p agentscope-extensions-judge -j 2` → EXIT=0，**336 passed / 0 failed / 0 ignored**（基线 308，+28）。
`cargo build -p agentscope-extensions-judge` → EXIT=0，**本 crate 0 warning**（仅 core/harness 既有 warning）。
新增用例（部分）：`resolve_routes_to_the_first_model_of_the_chosen_route`、`offers_apply_the_allowlist_and_exclusion_rules`、`resolution_without_a_caller_fails_closed`、`a_low_confidence_route_is_reported_but_not_used`、`the_none_route_is_not_applicable_and_leaves_the_call_alone`、`skipped_candidates_carry_the_java_reason_codes`、`a_pinned_model_is_used_without_asking_the_judge`、`an_explicit_phase_picks_its_first_usable_model`、`revalidation_drops_a_target_that_no_longer_fits`、`phase_routing_applies_the_resolved_model_and_effort_in_enforce`、`quality_enforce_revises_a_failing_draft_and_replaces_the_delta`、`quality_enforce_withholds_an_inconclusive_assessment`、`quality_shadow_observes_without_replacing`、`quality_skips_tool_calls_and_blank_drafts`、`a_blocked_revision_is_withheld`、`stages_and_goals_are_required`、`a_stage_routes_to_its_first_capable_model_without_asking_the_judge`、`an_unknown_or_uncapable_stage_fails_closed`、`a_pinned_request_still_keeps_its_target`。

**反向验证（主智能体补做，2 组）**：
1. 注释掉 `revalidate` 的 `input_restriction(...)` 分支 → `revalidation_drops_a_target_that_no_longer_fits` **FAILED**（断言 `!...is_valid()` 失败），恢复后绿；
2. 在 `ResponseMiddleware` 的 ENFORCE 分支提前 `return events`（既不改写也不拦）→ `quality_enforce_revises_a_failing_draft_and_replaces_the_delta` **FAILED**，恢复后 336/0 全绿。

**未完成（P1 剩余，均需 core seam）**：`JevGuardrail::screen` 的 reactive caller、`JevConfirmedCalls`（需 core `RuntimeContext` 键值扩展槽）；`browser` / `example` 两个子包仍为「设计决定（不迁移）」，待汇总方确认。

> **2026-10-31 追记（本节为历史快照，勿据此判定现状）**：上述两项已落地（`jev/screening.rs`、`jev/confirmed_calls.rs`，见 §11.1）；`browser` / JEV `example` 已定案（见 §11.3）。

## 11. 2026-10-31 执行记录（loop 收口：supervision 结果分类 / screening 调用方 / confirmed calls）

> 依据 Java HEAD `1524b3191`，本期主提交 `6ec57d2459f7a46677f40d26db54ef85f599d08a`（`fix(jev): classify supervision outcomes from agent results (#3862)`，XiaoyuPeng，2026-10-30）。
> **本轮按任务约束禁止运行 `cargo`/`rustc`/`clippy`/`fmt`，未做编译或测试实测**；下列状态为**源码级**核对结果，编译与测试验证由主智能体统一执行后再改「实测」字段。未改 core / harness / 根 `Cargo.toml`。

### 11.1 本轮落地项（已完成，代码位置 + 离线用例名）

| # | 项 | 状态 | 代码位置 | 代表离线用例 |
|---|---|---|---|---|
| 1 | supervision 终态改为「按 agent 结果分类」（Java `6ec57d245`） | **已完成** | 新增 `src/jev/runtime/capture.rs`（`SupervisionCapture`）；`src/jev/runtime/middleware.rs` 的 `SupervisionMiddleware::on_agent` 折叠每个事件并在流末取快照；`src/jev/runtime/mod.rs` 导出 | `suspended_results_stay_active_and_are_not_failures`、`failed_and_interrupted_results_are_failures`、`a_successful_continuation_after_denial_is_not_a_failure`、`a_forwarded_child_outcome_cannot_overwrite_the_parent`、`an_own_iteration_limit_fails_immediately`、`the_snapshot_carries_the_classified_flags` |
| 2 | `JevGuardrail::screen` 的 reactive caller | **已完成** | 新增 `src/jev/screening.rs`：`GuardrailScreener::{request,screen,screener,from_client}` + `caller_from_client` 复用既有 `http::JudgeHttpTransport` 缝；`ResponseMiddleware` 的 `Screener` 即由此闭合 | `the_screening_request_matches_the_java_shape`、`screen_folds_a_clean_reply`、`screen_folds_a_blocking_reply`、`an_incomplete_reply_is_rejected`、`a_caller_failure_is_surfaced_unchanged`、`the_screener_adapter_plugs_into_the_response_policy`、`a_scripted_transport_drives_a_real_request`、`concurrent_screens_share_one_binding` |
| 3 | `JevConfirmedCalls` | **已完成** | 新增 `src/jev/confirmed_calls.rs`：`JevConfirmedCalls::{install,ledger,snapshot,remember,remember_call,contains,matches,clear}` + 自由函数 `remember/contains/matches`；绑定**既有** core 键值扩展槽 `RuntimeContext::{set_extension,set_extension_arc,extension}`（`crates/agentscope-core/src/agent/runtime_context.rs`），**无需改 core** | `binds_parameters_at_prompt_time_not_at_confirm_return`、`binds_exact_arguments_and_context`、`a_call_without_an_id_is_never_remembered_or_matched`、`installing_is_idempotent_and_keeps_the_ledger`、`no_ledger_means_no_match` |
| 4 | 真实装配入口的集成测试 | **已完成** | 新增 `tests/runtime_integration.rs`（本 crate 首个 `tests/` 目标，全离线；仅 in-process transport double） | `a_chain_routes_the_guardrail_screener_and_the_supervisor`、`the_toolkit_runs_a_registered_jev_tool`、`the_store_backed_archive_round_trips_a_snapshot`、`an_approval_survives_a_resume_but_not_an_argument_change`、`the_guardrail_screen_drives_a_real_request_and_folds_the_verdict` |

**语义要点（对齐 Java `6ec57d245`）**：
- 终态只来自 `AGENT_RESULT` 的 `generateReason`，经 `agentscope_core::session::outcome` 分为 `completed` / `suspended` / `failed` / `interrupted`；
- `suspended`（`PERMISSION_ASKING` / `TOOL_SUSPENDED` / `*_STOP_REQUESTED`）→ `worker_active = true` 且**不是**失败，暂停不得被判为可完成；
- `EXCEED_MAX_ITERS` 仍立即失败，但仅限**本 agent** 事件（`EventData.source` 为空/空白）；子 agent 转发的 `EXCEED_MAX_ITERS` / `AGENT_RESULT` 不再覆盖父级结论；
- `REQUEST_STOP` / `ALL_TOOLS_DENIED` 边界事件**不再**单独视为失败（此前「全工具被拒后继续成功」被误判为 worker 失败即该 bug）。

### 11.2 对既有「骨架」记述的更正（源码复核，非本轮新代码）

以下各项在 §9 / §10 已落地，本节仅确认其**存在且已接线**（测试名取自源码，未在本轮重跑）：

| 项 | 位置 | 状态 |
|---|---|---|
| `StoreJevContextArchive` | `src/jev/runtime/archive.rs`（命名空间 `jev/context/<u>/<a>/<s>`、key `snapshot`、内容寻址 + create-if-absent） | **已完成**（原记「骨架」→ 更正） |
| `@Tool` 注册（`search_evidence` / `search_knowledge` / `review_code_snapshot`） | `src/jev/runtime/tools.rs` 的 `register_jev_tools` + `JevToolConfig` | **已完成**（原记「待 seam」→ 更正） |
| 四个中间件 hook | `src/jev/runtime/middleware.rs`：`TraceEvaluationMiddleware` / `ResponseMiddleware` / `SupervisionMiddleware` / `PhaseRoutingMiddleware` | **已完成** |
| `JevCodeReviewer` 六阶段驱动 | `src/jev/runtime/review_driver.rs` | **已完成（驱动层）** |
| `JevStageRouter` | `src/jev/application/stage_router.rs` | **已完成（不含 legacy 模型选择模式）** |

### 11.3 `browser` / `example` 判定正式化

| 子包 | 状态 | 源码级依据 | 影响 | 替代方案 |
|---|---|---|---|---|
| `browser`（4 类 / 1056 行） | **已完成（设计决定，不迁移）** | Java `jev/browser/*` 的 `PlaywrightSession` 依赖 Node 驱动 + 真实浏览器二进制；本 crate 定位「纯 Rust、离线可测」，且工作区无 Playwright/Node 运行时依赖 | 无浏览器自动化驱动（不提供 `Navigator`/`ReadTool` 实现） | 保留 `EvidenceSource` / `CodeReviewSource` 注入缝，由宿主提供真实浏览器或抓取实现 |
| `example`（**仅 JEV** 4 类 / 1130 行） | **已完成（设计决定，不迁移）**（2026-11-01 拆分订正：原行含 openai-decisions，已不适用） | 位于 `agentscope-examples/`，非运行链路；`JevModelRouterMiddleware` 只是装配示例，其能力已由 `PhaseRoutingMiddleware` + `JevStageRouter` 覆盖；**openai-decisions 的 `example/` 不是示例**（Java `31c7db8c9` 的提交说明称其为 opt-in middleware，已安装在 agent loop 上），已迁，见 §12 | `JevStageRouter` 无 legacy「模型选择」模式 | `JevStageRouter::begin(stage, goal)` + `PhaseRoutingMiddleware` 显式阶段路由（常量 `STAGE_ROUTER_UNAVAILABLE` 说明） |

### 11.4 仍未完成与原因

1. ~~**编译/测试实测缺位**~~ → **已完成（实测）**：`cargo test -p agentscope-extensions-judge -j 2` = **392 passed / 0 failed**（lib）+ 5 集成；`openai_decisions` 28 条含在内。
2. **`JevConfirmedCalls` 自动生效点** → **已完成（设计决定：宿主装配）**。依据：Rust core 的 HITL 走 `ConfirmHandler` **内联 await**（`react_agent.rs`，未装 handler 即返回错误），不存 Java 的 `PERMISSION_ASKING` + `METADATA_CONFIRM_RESULTS` 恢复路径，也没有 `ToolCallState::Asking` 被写出的分支 → 本 crate **不存在**可自动插入的恢复点；因此按库的 seam 交付：`install/remember/contains/matches` + `JevConfirmedCalls::{install,ledger,snapshot,remember_call}` 全部公开（`src/jev/confirmed_calls.rs`，经 `jev/mod.rs` 重导出），宿主在 prompt 前 `remember_confirmed_call`、恢复前 `confirmed_call_matches`。已由 5 条用例钉住语义（`binds_parameters_at_prompt_time_not_at_confirm_return` 等）；未改 core / 未新增跨模块依赖。
3. **`browser` / JEV `example`** → **已完成（设计决定，不迁移）**，已由汇总方于 2026-10-31 确认：`browser` 需 Playwright（JVM 浏览器驱动）且无 Rust 对应运行时；JEV `example` 是演示用装配代码，非 wire 契约（`openai-decisions` 的 `example/` 已迁，见 §12）。

### 11.5 跨模块需求

- **无新增**。`JevConfirmedCalls` 使用既有 `RuntimeContext` 键值扩展槽，未要求 core 新增 API；screening 走既有 `http::JudgeHttpTransport`；supervision 走既有 `session::outcome`。
- 建议（非阻塞）：core 的 HITL 恢复路径在重放 pending tool call 前调用 `jev::confirmed_calls::matches(ctx, call)`，否则该账本只能由宿主手工驱动。

---

## 12. 2026-11-01 执行记录（OpenAI Decisions 中间件 / `6ec57d245` 复核 / `16cb024ae` 裁定）

> 依据 Java HEAD `1524b3191`。本期主提交 `31c7db8c9`（`feat(judge): add OpenAI Decisions middleware and runnable examples`，2026-10-22）。
> **本轮按任务约束禁止运行 `cargo` / 编译 / 测试**；状态为**源码级**核对。新建两个文件仅用 `rustfmt --check` 做了解析校验（输出为空、无语法错），编译/测试由主智能体统一执行。未改 core / harness / 根 `Cargo.toml`，未改 `Cargo.toml` 依赖。

### 12.1 本轮落地项（`31c7db8c9`）

| # | Java 类 | 状态 | Rust 位置 | 代表离线用例 |
|---|---|---|---|---|
| 1 | `DecisionSupport`（109 行） | **已完成** | `src/openai_decisions/support.rs`（`latest_user_text` / `recent_messages` / `json` / `require_probability` / `run_key`） | `the_latest_user_turn_is_trimmed_and_skips_other_roles`、`recent_messages_flatten_tool_results_and_keep_newest_last`、`the_projection_respects_both_budgets_without_splitting_characters`、`probabilities_must_be_finite_and_bounded` |
| 2 | `OpenAIDecisionsModelRouterMiddleware`（255 行） | **已完成** | `middleware.rs` `ModelRouterMiddleware` + `ModelRouterMiddlewareBuilder` + `ModelChoice` / `RoutingDecision` / `Compatibility` | `the_router_selects_once_per_invocation_and_swaps_the_model`、`a_refusal_low_confidence_or_unknown_choice_keeps_the_caller_model`、`the_router_requires_explicit_compatibility_for_tool_calls`、`a_reused_context_does_not_inherit_a_previous_route` |
| 3 | `OpenAIDecisionsToolSelectionMiddleware`（277 行） | **已完成** | `middleware.rs` `ToolSelectionMiddleware` + builder | `selection_keeps_essential_tools_caps_relevant_ones_and_preserves_order`、`selection_retains_the_original_tools_on_ambiguity_refusal_or_failure`、`selection_batches_more_than_four_hundred_tools`、`a_forced_tool_is_never_judged` |
| 4 | `OpenAIDecisionsAutoModeMiddleware`（294 行） | **已完成** | `middleware.rs` `AutoModeMiddleware` + builder + `denied_result` / `denied_events` | `the_guard_denies_risky_and_refused_calls_and_keeps_the_others`、`the_guard_fails_closed_without_a_state_or_on_an_error`、`explicit_fail_open_allows_a_failed_assessment`、`the_guard_batches_more_than_four_hundred_calls` |
| 5 | `OpenAIDecisionsRetryPolicy` + `Builder.retryPolicy` | **已完成（2026-10-28 已落，无需改动）** | `openai_decisions/client.rs` 的 `DecisionsRetryPolicy` / `OpenAIDecisionsClientBuilder::retry_policy` | 既有 `retry_policy_matches_java_constructors`、`retries_only_when_the_policy_opts_in`、`exposes_http_status_and_retry_hint_without_retrying_by_default` |
| 6 | Spring Boot 自动配置（`OpenAIDecisionsProperties` / 两个 starter 类） | **已完成（设计决定，不迁移）** | 无 | 依据：本仓库无 Spring Boot / 依赖注入容器，中间件的 `enabled` / `fail_open` / 阈值已全部暴露为 builder 方法，由宿主自己构造并注册；影响：没有 YAML 配置绑定；替代：`ModelRouterMiddleware::from_client(client)` + 链式 builder |
| 7 | 可运行示例（`OpenAIDecisionsMiddlewareExample` 238 行 + `docs/v2/{en,zh}/integration/judge/openai-decisions.md`） | **已完成（设计决定，不迁移）** | 无 | 依据：示例位于 `agentscope-examples/documentation` 演示工程（`exec:java` + 真实 API key + JVM 代理），非库代码；影响：不提供 route/select/guard 三模式 demo；替代：本 §12.1 的 21 个离线用例逐项覆盖同样行为 |

依赖：**无新增**。三个中间件只用 `agentscope-core`（已依赖）与已有 `serde_json` / `futures`。

### 12.2 语义对照（Java → Rust）

| 项 | Java | Rust | 判定 |
|---|---|---|---|
| 路由决定的存与读 | `ctx.put(RoutingDecision.class, ...)` / 静态 `decision(ctx)` | 中间件内 `run_id`→`session_id` 键的 `Mutex<HashMap>` + `decision(&ctx)` | 适配层差异（`&RuntimeContext` 不可变）；同一 run 语义等价，测试见 `a_reused_context_does_not_inherit_a_previous_route` |
| 被拒调用的结果落盘 | `state.contextMutable().add(ToolResultMessageBuilder.buildToolResultMsg(...))` | `ActingInput::results.record(denied_result(...), false)`（core 在 acting 结束后写成 tool 消息） | 等价（同一落盘点，见 `react_agent.rs` 的 `results.snapshot()`） |
| 被拒调用的三事件 | `AgentEventEmitter.emit(Start/TextDelta/End)` | 直接返回 `AgentEvent::with_fields(ToolResultStart/ToolResultTextDelta/ToolResultEnd)` 事件流 | 等价（中间件返回的流由 core 转发到 `delta_tx`），字段名与 core 自身发射的一致（`reply_id` / `tool_call_id` / `tool_name` / `delta` / `state`） |
| 兼容性谓词异常 | `try/catch (RuntimeException) → usable=false` | 不捕获 panic | 有意差异；默认谓词与 builder 注入的闭包均为纯判定，不预期 panic |
| 批内失败 | `onErrorResume(failOpen ? batch : List.of())`（每批独立） | `assess` 逐批 match，失败时按 `fail_open` 逐批放行/全拒 | 等价（`explicit_fail_open_allows_a_failed_assessment`） |
| 无 `AgentState` | `state == null ? failOpen ? guarded : List.of()` | `ctx.state` 为 `None` 时同规则 | 等价 |
| 工具集过滤 | Java 用 `Set<String> required` + 保留原 `input.tools()` 顺序 | `Vec<String>` + 保留原顺序 | 等价（`selection_keeps_essential_tools_caps_relevant_ones_and_preserves_order` 断言了顺序） |
| 并发批 | `Flux.range(...).concatMap(...)`（串行） | `chunks(400)` + 逐个 `await`（串行） | 等价（`vec![400, 1]` 断言了请求数与顺序） |
| 空结果 | `switchIfEmpty(error("Decisions returned no result"))` | 调用缝返回 `Result`，没有空流，故无此分支 | 无对应物（已写入代码模块文档） |

### 12.3 代码内已声明的差异（不重复展开）

`src/openai_decisions/middleware.rs` 模块文档已列出三条：`RuntimeContext` 不可变导致的 decision 存储方式、被拒结果走 `ActingInput::results`、不把 panic 翻译为「不兼容」。此处仅登记，不另作缺口。

### 12.4 订正：`openai-decisions` 的 `example/` **不是**“示例”

- **依据（源码级）**：`31c7db8c9` 的提交说明写明三个类为 “three opt-in middleware implementations”，并被 `OpenAIDecisionsAutoConfiguration` 引用；类实现 `MiddlewareBase`，带 `activePoints()` / `order()`，属**运行时链路**。
- **同步修改**：§2.1 的 `openai-decisions` 行、§3.2 的整块行、§3.3 与 §11.3 的 `example` 行均已改为「仅 JEV」。
- **遗留口径**：§7.5 / §8.1 / §9.5 等**历史快照**中的「`openai_decisions/example/` 4 类示例中间件未迁」为当时事实，不再回写；以本节为准。

### 12.5 `6ec57d245` / `16cb024ae` 裁定

| 提交 | 裁定 | 证据 |
|---|---|---|
| `6ec57d245` `fix(jev): classify supervision outcomes from agent results` | **已完成（2026-10-31 已落，本轮源码复核）** | `src/jev/runtime/capture.rs` 逐条对上 Java diff：①`own_event` 判定（`source` 为 `None`/空）后才采信 `AGENT_RESULT` 与 `EXCEED_MAX_ITERS`；②`outcome(reply.generate_reason())` 把 `failed`/`interrupted` 记为失败、`suspended` 记 `worker_suspended`，`REQUEST_STOP` / `ALL_TOOLS_DENIED` 边界事件不再单独置失败；③`worker_active() = !completed \|\| worker_suspended` 对应 Java `!complete \|\| workerSuspended`。另核 `agentscope_core::session::outcome` 与 Java `SessionRecorder.outcome` 分支逐字一致（含 `MAX_ITERATIONS`/`ALL_TOOLS_DENIED → failed`）。用例：`suspended_results_stay_active_and_are_not_failures`、`a_successful_continuation_after_denial_is_not_a_failure`、`a_forwarded_child_outcome_cannot_overwrite_the_parent` |
| `16cb024ae` `test: deflake JEV trace evaluation ...` | **已完成（无需等价实现）** | Java 本提交对 JEV 的改动**只有测试侧**：`JevTraceEvaluationTest` 把评估预算 3s → 60s（另两处 flake 在 Feishu channel（涉及生产码 `responseTimeout` 可配，归 CHANNEL 模块）与 subagent，不属本模块）。Rust 侧：`JevTraceEvaluator::Limits.budget` 是构造参数（`evaluation/evaluator.rs:29`），默认 3s 仅作为 `Limits::default()` 的默认值；`Limits::default()` 在本 crate 的 `src/**` 与 `tests/**` **0 命中**（`tests/` 未构造 `Limits`；`ResponseLimits::defaults()` 是别的类型），凡走评估的用例都显式给预算，后端为内存 caller，不存在「调度停顿消耗预算」的 flaky 断言。故不改代码；新增用例仍按显式预算书写（如 `selection_batches_more_than_four_hundred_tools` 与本题无关，但同一约定）。 |

### 12.6 仍未完成与跨模块

1. **编译/测试实测缺位**（同 §11.4 第 1 条）：本轮新增 `support.rs`（247 行 / 7 测试）与 `middleware.rs`（2020 行 / 21 测试），仅 `rustfmt --check` 通过。**未完成，原因：任务约束禁跑 cargo**。
2. **中间件装配点由宿主负责**：三个中间件默认 `enabled=false`，宿主须 `enabled(true)` 后放入 `MiddlewareChain`；harness `HarnessAgentBuilder` 没有 OpenAI Decisions 的注册入口（与无 Spring Boot starter 同因）。**跨模块建议（非阻塞）**：harness 提供 `with_decisions_middlewares(...)` 一类的注册快捷方式，否则宿主需自行拼 `MiddlewareChain`。
3. **`JevConfirmedCalls` 的 core 侧自动生效点**：沿用 §11.4 第 2 条，未变。
4. **`browser` / JEV `example` 汇总方确认**：沿用 §11.4 第 3 条，未变。

### 12.7 跨模块需求

- **无新增**。未要求 core 新增 API（`ToolChoice::Specific`、`ActingInput::results`、`AgentEvent::with_fields`、`Model::supports_native_structured_output` 均为既有能力）。
- 非阻塞建议一条：见 §12.6 第 2 条（harness 侧注册入口）。
