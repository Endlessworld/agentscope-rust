# CORE 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CORE**（`io.agentscope.core` → `crates/agentscope-core`）迁移的**单一事实来源**，**取代 `docs/MIGRATION_GAP_TRACKER.md` 中 core 相关段落的口径**（该文件中的 core 行可能过时，引用前以本文件为准）。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-core\src\main\java`（分支 `main`）
> **Java 包根**：`io.agentscope.core`（22 个包 / 361 个 `.java` / 约 63k 行）
> **Rust 目标**：`crates/agentscope-core/`（103 个 `.rs` / 约 36.5k 行，`src/` 内 380 处 `#[test]` 标记 + `tests/` 12 个集成测试文件）
> **本轮复核基线**：Java `main` @ `1524b3191`；Rust 工作树 HEAD

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`。
- **优先级**：P0=核心链路 / 生产阻断；P1=重要能力缺失或语义偏离；P2=增强 / 长尾 / 测试密度。
- 编号：缺口与已完成项统一 `CORE-NN`（§3.5，单调递增、不复用）；`CORE-DNN` = 设计决定（不迁移，§4.3.1）；`CORE-XNN` = 与 Java 的已知差异（§4.3.2）。
- **证据门槛**：Java 参考写 `包路径/文件.java#符号`；Rust 写 `src/xxx.rs:行`，或 `rg '<sym>' 零命中（另核 '<sym2>'/'<sym3>' 亦零命中）`。**「未见调用」不等于「未实现」**。
- core 是**全仓共享底座**：任何 core 改动须单智能体串行独占（`crates/agentscope-harness`、各 `agentscope-extensions-*` 均依赖其 trait 形状）。
- 执行过程记录（回执 / 反向验证）写 `docs/extensions/history/`，本文件只写现行状态。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-core`（`io.agentscope.core`） |
| Java 规模 | 22 包 / 361 `.java` / 约 63k 行（`agent` 29、`tool` 68、`model` 41、`event` 36、`agent`+`session` 56 为主干） |
| Rust crate | `crates/agentscope-core` |
| Rust 规模 | 103 `.rs` / 约 36.5k 行（`src/agent/react_agent.rs` 7274 行、`src/session/*` 20 文件约 9.9k 行） |
| 迁移波次 | 基础底座（无波次编号，早于 extensions 波次 A/B/C） |
| 当前状态 | 主干可用：agent 循环 / 消息 / 事件 / 工具 / 状态 / 会话日志 / 权限 / 中断 / 关停均已落地；剩余缺口集中在 **middleware 停流语义、模型回退、工具级拦截缝、tool-search** 四类 |

> **2026-10-31 全量对齐复核**：按 Java 22 个包逐包核对，新增 §3.5（缺口 CORE-01..14，已对齐抽样 CORE-15..26）与 §4.3（`CORE-D01..D14` / `CORE-X01..X08`）。历史上曾被记录为缺口的两项已确认关闭：`Middleware::on_acting` 零调用（现 `wrap_acting` 已接线，CORE-16）、`GracefulShutdownManager` 无 `bind_state_saver`（现已存在并接线，CORE-17）。

## 2. Java 清单与依赖映射

### 2.1 按包映射（Java 文件数/行数 → Rust 落点）
| Java 包 | Java 文件/行 | Rust 落点 | 结论 |
|---|---|---|---|
| `agent` | 29 / 5306 | `src/agent/{react_agent,run,runtime_context,stream_options,lifecycle,agent_traits}.rs` | 部分迁移：主干对齐；缺 `ModelConfig`/`FailoverListener`（CORE-03）；`accumulator/*` 合并进 `react_agent.rs:202 ReasoningContext`；`SubagentEventBus` 架构差异（CORE-X07） |
| `agent/accumulator` | 6 | `src/agent/react_agent.rs:202`（`ReasoningContext` 单结构 + `accumulate`/`build_final_message`） | 已迁移（6 类合并为 1） |
| `agent/config` | 4 | `ReActAgentBuilder`（`react_agent.rs:1243`，`max_iters`/`stop_on_reject` 等旋钮） | 部分：`ReactConfig`→builder；`ModelConfig` 缺 |
| `agent/user` | 4 | — | 设计决定（Java `@Deprecated`，CORE-D04） |
| `credential` | 6 / 389 | `src/credential/{credential,model_card}.rs`（`Credential` trait + DeepSeek/Kimi/XAI） | 已迁移 |
| `event` | 36 / 2330 | `src/event/{event,event_type,emitter}.rs` | 已迁移：32 个事件类型 32/32 对齐，35 个事件类收敛为枚举 + `EventData`（CORE-15） |
| `exception` | 1 / 72 | `src/error.rs:8 Error` + `AgentExceptionInfo`/`CompositeAgentException` | 已迁移（分类方式差异，CORE-X06） |
| `formatter` | 6 / 1145 | `src/formatter/{base_formatter,media_utils,json_schema,response_format}.rs` | 已迁移 |
| `hook` | 20 / 2532 | `src/middleware/*.rs` | 设计决定：整包 `@Deprecated since 2.0.0`（→ `MiddlewareBase`），CORE-D01 |
| `interruption` | 3 / 271 | `src/interruption/mod.rs`（`InterruptSource:10` / `InterruptContext:29` / `InterruptControl:120`） | 已迁移 |
| `memory` | 8 / 994 | `src/memory/mod.rs`（`Memory:14` / `InMemoryMemory:46` / `StateBackedMemory:115`） | 部分：`LongTermMemory*` 系列 Java 已废弃（CORE-D02）；`Memory` 兼容层无生产引用（CORE-13 待裁定） |
| `message` | 25 / 4199 | `src/message/{msg,block,source,usage,keys}.rs`（`Msg:68` 字段 7/7 对齐） | 已迁移：`ToolExecutionDetails` 缺（CORE-07） |
| `middleware` | 10 / 956 | `src/middleware/{middleware,input,task_context_projection,task_reminder}.rs` | 部分：缺 `FinalAnswerFilterMiddleware`（CORE-06）；`ModelRequestPreparer`→harness `ContextPipeline`（CORE-10 残余） |
| `model` | 41 / 7951 | `src/model/{model,provider,generate_options,tool_choice,tool_schema,chat_response,execution_config,context_windows,cache_policy}.rs` + `src/model/catalog/*` | 部分：`ModelRegistry:155`/`ModelProvider:138`/`ModelCapabilities` 齐备；缺固定回退模型（CORE-03）、`FinishReasons`（CORE-11） |
| `model/transport` | 18（含 websocket 9） | `crates/agentscope-extensions-model/src/common/transport.rs`（618 行，reqwest + `SharedTransport` seam） | 部分迁移：HTTP 传输已迁（CORE-X09）；WebSocket 一组未迁（CORE-D10） |
| `observation` | 4 / 300 | `src/observation/mod.rs:56 ActionObservation` | 部分：缺 `exitCode`/`outputTruncated`/`kind`（CORE-07） |
| `permission` | 8 / 942 | `src/permission/mod.rs:216 PermissionEngine`（5 模式 + 4 行为 + 规则/工作目录） | 已迁移（CORE-23） |
| `rag` | 7 / 1212 | `crates/agentscope-extensions-rag` | 设计决定：Java 整包 `@Deprecated`，能力在 extensions-rag 重建（CORE-D06） |
| `session` | 27 / 3020 | `src/session/*.rs`（20 文件：journal/recorder/views/recovery/snapshot/migration/models/inbox/execution/export/state_access） | 大部分已迁移（CORE-20）；缺 v1 迁移入口（CORE-08） |
| `shutdown` | 10 / 776 | `src/shutdown/mod.rs:122 GracefulShutdownManager` + harness `agent/shutdown.rs` | 已迁移（CORE-17）；JVM hook 未迁（CORE-D13） |
| `skill` | 17 / 5051 | `crates/agentscope-harness/src/agent/skill/*`（13 文件）+ `crates/agentscope-extensions-skills`（持久化） | 已迁移（跨 crate；`SkillHook` 废弃 → CORE-D07） |
| `state` | 20 / 2858 | `src/state/{agent_state,store,context,requirement,verification,list_hash,conflict_policy,session_info}.rs` | 大多已迁移：CAS/版本化齐备（CORE-19）；`LegacyStateLoader`/`legacy/ToolkitState` 缺（CORE-08）；`State` 标记接口不迁（CORE-D12） |
| `tool` | 68 / 14691 | `src/tool/*.rs`（22 文件）+ harness `src/tool/*` | 部分：核心 Toolkit/Executor/Groups/RequestConfig/Emitter/Schema/Validator 齐备；缺 `ToolCallInterceptor`（CORE-04）、`tool/search`（CORE-05）、`ToolExecutionDetails`（CORE-07）、`task_requirement_propose`（CORE-09） |
| `tool/coding` | 4 | `src/tool/coding.rs`（校验器，非完整 Bash AST）+ `harness/src/tool/shell_execute_tool.rs` | 部分迁移：工具本体在 harness，校验器为 allow/deny 轻量实现（CORE-X03） |
| `tool/file` | 3 | `src/tool/file.rs`（read/write/list）+ `harness/src/tool/filesystem_tools.rs`（611 行） | 已迁移 |
| `tool/mcp` | 7 | `src/tool/mcp.rs`（契约层）+ `harness/src/agent/mcp_client_manager.rs` | 已迁移（跨 crate，SDK 由 rust-mcp-sdk 提供） |
| `tool/search` | 7 | 仅 `extensions-model/src/common/tool_search.rs`（`ToolSearchMode` + 元数据常量） | **未迁移**（CORE-05） |
| `tool/subagent` | 3 | `harness/src/tool/agent_spawn.rs` + `harness/src/agent/subagent/*` | 已迁移（跨 crate，异步执行模型 CORE-X07） |
| `tracing` | 4 / 636 | `src/tracing.rs`（13 行，tracing_subscriber 初始化）+ `extensions-studio/src/telemetry/tracing.rs`（1252 行） | 设计决定：`Tracer`/`TracerRegistry`/`NoopTracer` Java 已 `@Deprecated`（CORE-D08） |
| `util` | 8 / 1061 | `serde`/`serde_json` + `src/error.rs` | 设计决定（Jackson/反射专属，CORE-D11） |
| `workspace` | 1 / 23 | — | 设计决定（仅 `package-info.java`） |

### 2.2 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Project Reactor（`Mono`/`Flux`） | 流式执行、中间件洋葱模型 | `futures::Stream` + `BoxStream` + `Next<T>` continuation（`middleware.rs:11`） | 无 Reactor Context，改用 `RuntimeContext`（`agent/runtime_context.rs:151`） |
| Jackson | JSON 编解码、事件多态 | `serde` + `serde_json`（`#[serde(tag=...)]`/`EventData.fields`） | 字段名以 `camelCase`/显式 `rename` 对齐 |
| SLF4J | 日志 | `tracing` crate | `core/src/tracing.rs` 提供 subscriber 初始化 |
| JDK/OkHttp HTTP 传输 | 模型 API 调用 | `reqwest` + `common/transport.rs` 的 `SharedTransport` seam | 单实现 + 可注入 mock（离线测试） |
| `@Tool`/`@ToolParam` 注解 + 反射 | 工具注册与 schema 生成 | `FunctionTool`（显式闭包 + 手写 `ToolSchema`，`tool/tool.rs:194`） | CORE-D11 / CORE-14（待裁定是否补 proc-macro） |
| JVM ShutdownHook | 优雅关停 | 宿主显式调用 `GracefulShutdownManager::perform_graceful_shutdown`（`shutdown/mod.rs:352`） | CORE-D13 |
| OpenTelemetry Java SDK | 追踪 | `extensions-studio` 的 `TelemetryTracing` + `TraceExporter` trait | core 只保留日志初始化 |

## 3. 详细缺口清单

> 本文件为 2026-10-31 新建：`§3.1–§3.4` 无存量历史条目（若后续新增历史小节，**不得**重新编号 §3.5）。

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**（本文件内为空）。
> 编号规则：`CORE-NN`（缺口/已完成，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `CORE-D<NN>` / `CORE-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| CORE-01 | P0 | **middleware 停流（`RequestStop`）语义未接线**：`on_reasoning`/`on_model_call` 阶段把流 `collect()` 后只取 `Err`（`react_agent.rs:2134-2144`），`on_acting` 阶段把事件原样 `unbounded_send`（`react_agent.rs:2945-2949`），两处都**不检查 `AgentEvent::RequestStop`**；因此 `GenerateReason::{ReasoningStopRequested, ActingStopRequested, MiddlewareStopRequested}` 在 core 内无产生点（仅 `msg.rs:31/33/37` 定义 + `recorder.rs:144-146` 映射）。后果：中间件（如 `agentscope-extensions-judge` 的 `jev/runtime/middleware.rs:1184/1201` 已按 Java 语义发 `RequestStop`）请求停止被静默忽略；acting 阶段若只发 stop 不产出结果，还会被判定为 `acting phase produced no result`（`react_agent.rs:2963-2969`）硬错误。 | `event/RequestStopEvent.java#getGenerateReason`；`agent/ReActAgent.java#emitAllToolsDeniedThroughMiddleware`（`stopRef` → 以 `rs.getGenerateReason()` 收尾本轮） | `rg -n 'AgentEvent::RequestStop' crates/agentscope-core/src/agent` 零命中（另核 `RequestStop|StopRequested` 仅 `msg.rs`/`recorder.rs`） | 待办 | 在 reasoning / acting 两个消费点识别 `RequestStop`：提前中止流、丢弃该轮剩余执行、以事件携带的 `generateReason`（缺省按阶段给 `ReasoningStopRequested`/`ActingStopRequested`/`MiddlewareStopRequested`）收尾；补 3 条单测（reasoning 停、acting 停、无结果时按 stop 而非报错） |
| CORE-02 | P1 | **`AllToolsDenied` 事件与 `GenerateReason::AllToolsDenied` 无发射点**：全仓仅 `event/event.rs`（定义/映射）与 `session/recorder.rs:148`（映射为 `failed`）引用，`react_agent` 内零引用（`AgentEventType::` 检索 15 处均非该类型）。后果：全部工具被权限拒绝时，Rust 继续下一轮推理且最终 reason 为 `ModelStop`；Java 会经 `onActing` 链发 `AllToolsDeniedEvent`，并允许中间件 `RequestStop` 提前收尾（默认 reason = `ALL_TOOLS_DENIED`）。 | `event/AllToolsDeniedEvent.java#AllToolsDeniedEvent(List<ToolUseBlock>)`；`agent/ReActAgent.java#emitAllToolsDeniedThroughMiddleware` | `rg -n 'AllToolsDenied' crates -g '*.rs'` → 仅 `event.rs`/`event_type.rs`/`msg.rs`/`stream_options.rs`/controlplane/judge 测试；`crates/agentscope-core/src/agent/` 零命中 | 待办 | acting 结束后判定「本轮全部 call 均为 denied/suspend 外拒绝」→ 发 `AllToolsDenied`（携带被拒 `ToolUseBlock` 列表）并依 CORE-01 的停流机制收尾；补单测（全拒 → reason=`AllToolsDenied`、部分拒 → 继续） |
| CORE-03 | P1 | **固定回退模型（fallback model）+ 失效观察者缺失**：Rust 只有单模型 + `ExecutionConfig`（`max_attempts=7`/退避 2→60s，`execution_config.rs:8-46`），无「主模型耗尽后切备用模型」与切换观察面。 | `agent/config/ModelConfig.java#fallbackModel/#failoverListener/#DEFAULT_MAX_RETRIES=3`；`agent/config/FailoverListener.java` | `rg -ni 'failover|fallback_model|backup model' crates -g '*.rs'` → 仅 redis 拓扑 failover 与 studio 的 `FALLBACK_MODEL_TYPE_NAME`；core 零命中 | 待办 | 在 `ReActAgentBuilder`/`AgentCore` 增可选 `fallback_model: Option<Arc<dyn Model>>` + `failover_listener`；主模型重试耗尽后切备用（共享同一重试预算）；listener 异常吞掉只记日志；补单测（切模型成功、listener 被调用、备用也失败） |
| CORE-04 | P1 | **工具调用级拦截缝（`ToolCallInterceptor`）缺失**：Java 在 schema 校验与 preset 合并之后、真正执行之前插入 per-call 拦截链，可改写参数 / 策略拒绝（记 `denied` 决策）/ 审计 / 抛 `ToolSuspendException` 走 HITL。Rust 只有 `ToolValidator`（`tool/validator.rs:38`）+ 权限引擎 + `Middleware::on_acting`（粒度是整批，不是单次调用）。 | `tool/ToolCallInterceptor.java#intercept(ToolInvocation, Function)` | `rg -ni 'interceptor' crates/agentscope-core/src -g '*.rs'` 零命中（另核 `on_tool_call|before_call` 零命中）；`ToolInvocation` 零命中 | 待办 | 在 `tool/executor.rs` 的执行入口引入 `ToolCallInterceptor` trait + `ToolInvocation`（call 快照）；`ToolkitConfig` 承载链；拒绝路径写 `ToolResultState::Denied` 并记 denied 决策；补单测（通过/改写输入/策略拒绝/抛 suspend） |
| CORE-05 | P1 | **`tool/search` 包未迁移**：仅把 `ToolSearchMode`（FUNCTION/CLIENT/DEFERRED）+ 元数据常量/`DEFAULT_LIMIT` 搬到 `extensions-model/src/common/tool_search.rs`；core 缺 `ToolSearchMiddleware`、`ToolSearchStrategy`/`KeywordToolSearchStrategy`、`ToolSearchTool`、`ToolSearchRequest`/`Result`。注意 `extensions-model/src/common/tool_search.rs:6` 的注释「Search itself lives in core (`ToolSearchMiddleware`)」与事实不符（core 零命中），属陈旧注释，修 core 时一并更正。 | `tool/search/ToolSearchMiddleware.java`、`tool/search/KeywordToolSearchStrategy.java`、`tool/search/ToolSearchTool.java`、`tool/search/ToolSearchStrategy.java`、`tool/search/ToolSearchRequest.java`/`ToolSearchResult.java` | `rg -n 'ToolSearchMiddleware|KeywordToolSearchStrategy|ToolSearchTool|ToolSearchStrategy' crates/agentscope-core/src` 零命中；`ToolSearchRequest`/`ToolSearchResult` 全仓零命中 | 待办 | 在 core 实现关键词检索策略 + `tool_search` 只读工具（返回 `METADATA_MODE`/`METADATA_TOOLS` 元数据）+ 中间件（按模式裁剪工具面，配合 `ToolSchema.defer_loading`，`tool_schema.rs:26`）；补离线单测（命中/未命中/limit/模式） |
| CORE-06 | P2 | **`FinalAnswerFilterMiddleware` 缺失**（opt-in：只向调用方暴露最后一轮文本，缓冲到模型调用结束，若本轮有 tool call 则抑制对应文本事件）。 | `middleware/FinalAnswerFilterMiddleware.java#onReasoning`（`activePoints=ON_REASONING`） | `rg -n 'FinalAnswer|only_final|suppress_intermediate' crates -g '*.rs'` 零命中 | 待办 | 以 `Middleware::on_reasoning` 实现：缓冲 `TextBlock{Start,Delta,End}`，遇 `ToolCallStart` 丢弃并在 `ModelCallEnd` 前补发；补单测（纯文本保留 / 含工具调用抑制 / 多轮只留末轮） |
| CORE-07 | P2 | **`ToolExecutionDetails` 未建模**：Java 以 `(kind, Outcome{SUCCEEDED,FAILED,PARTIAL,UNKNOWN}, exitCode, outputTruncated)` 挂在工具结果上，并被 `ActionObservation` 消费；Rust `ActionObservation`（`observation/mod.rs:56`）只有 `status/error_type/时间戳`。 | `message/ToolExecutionDetails.java#ToolExecutionDetails`；`observation/ActionObservation.java`（引用 `ToolExecutionDetails`） | `rg -n 'exit_code|output_truncated|execution_details' crates/agentscope-core/src` 零命中 | 待办 | 在 `message` 增 `ToolExecutionDetails` 并在 `ActionObservation`/`observation::settle` 透出；由 `Tool` 返回路径（shell/文件类工具）填 `exit_code`/`output_truncated`；补 serde 往返 + settle 单测 |
| CORE-08 | P2 | **v1 legacy 迁移入口缺失**：Java `LegacyStateLoader` 从旧会话键（`memory_messages`、`toolkit_activeGroups`）重建 `AgentState`，只作显式迁移入口；`ToolkitState` 是其反序列化形状。Rust `session/migration.rs` 只有 `import_baseline`/`export_state`/`export_snapshot`/`fork`，无 legacy 键读取。 | `state/LegacyStateLoader.java#load`；`state/legacy/ToolkitState.java#ToolkitState(List<String> activeGroups)` | `rg -n 'LegacyStateLoader' crates -g '*.rs'` 零命中；`rg -n 'toolkit_activeGroups|toolkit_active_groups' crates -g '*.rs'` 零命中（仅 `memory/mod.rs:10 MEMORY_MESSAGES_KEY` 保留旧键名） | 待办 | 新增 `state::legacy::load`（读 `memory_messages` + `toolkit_active_groups` → `AgentState`，允许调用方显式传入 `PermissionContextState`，缺省 `Default` 模式）；与 `session::migration::import_baseline` 组合使用；补单测（旧键→state、无键→空 state、权限上下文透传） |
| CORE-09 | P2 | **`task_requirement_propose` 工具未注册**：状态层 API 已具备（`state/context.rs:360 propose`），但 Java 的 opt-in 工具没迁。 | `tool/builtin/RequirementTools.java#proposeRequirement`（`@Tool(name="task_requirement_propose", stateInjected=true, concurrencySafe=false)`） | `rg -n 'task_requirement_propose' crates -g '*.rs'` 零命中；`rg -n 'RequirementTool' crates -g '*.rs'` 零命中 | 待办 | 在 core 增 `RequirementProposeTool`（`stateInjected`，读 `AgentState`，`kind/text/source_ref` 三参，错误路径返回 `ToolResultBlock::error`），由宿主 opt-in 注册；补单测（成功/kind 非法/state 缺失） |
| CORE-10 | P2 | **`ModelRequestPreparer` 的事件面残余**：harness 已有 `ContextPipeline`/`PreparedRequest`/`MANIFEST_ATTRIBUTE_PREFIX`（`harness/src/agent/context/builder.rs:49/103`），但 Java 的 `BUILD_EVENT_NAME = "context_build"` 与「拒绝发生在任何 provider 调用之前也要发一条 build 结果」的 `CustomEvent` 尚未对齐。 | `middleware/ModelRequestPreparer.java#prepareObserved/#BUILD_EVENT_NAME/#MANIFEST_ATTRIBUTE_PREFIX` | `rg -n 'context_build' crates -g '*.rs'` 零命中（`MANIFEST_ATTRIBUTE_PREFIX` 已有，`builder.rs:49`） | 待办 | 在 harness 的 prepare 路径发 `AgentEvent::with_fields(AgentEventType::Custom, {kind:"context_build", ...})`，拒绝分支同样发；core 侧无需改动（本项归属 harness，见 §4.2） |
| CORE-11 | P2 | **`FinishReasons` 常量类缺失**：Java 以常量收敛 `stop`/`length`/`tool_calls` 等 finish reason 字面量；Rust 在 `extensions-model` 各 provider 内散落 `"end_turn"`/`"tool_use"`/`"tool_calls"` 字面量比较（如 `anthropic/model.rs:364`、`anthropic_async/mod.rs:742`）。 | `model/FinishReasons.java` | `rg -n 'FinishReasons' crates -g '*.rs'` 零命中（另核 `FinishReason` 仅见结构体字段 `finish_reason: Option<String>`） | 待办 | 在 core `model` 提供 `FinishReasons` 常量（或 `FinishReason` 枚举 + `from_wire`），`extensions-model` 改为引用；补映射单测（各 provider 归一） |
| CORE-12 | P2 | **`SubagentExposed` 事件无发射点**：core 侧类型齐备（`event_type.rs:39` + `event.rs:79`），但全仓无构造点；Java 在 `AgentSpawnTool` 暴露子代理时发出（含 `agentId`/名称/会话/类型）。 | `harness/agent/tool/AgentSpawnTool.java#...new SubagentExposedEvent(...)` | `rg -n 'SubagentExposed' crates -g '*.rs'` → 仅 core 的定义/映射与 `stream_options.rs:58` 类别划分；`crates/agentscope-harness/src` 零命中 | 待办 | 在 `harness/src/tool/agent_spawn.rs` 暴露/登记子代理处发 `SubagentExposed`（跨模块，见 §4.2）；core 无需改 |
| CORE-13 | P2 | **`Memory` 兼容层无生产引用**：`Memory` trait + `InMemoryMemory` + `StateBackedMemory` 仅在 `tests/state_memory_permission_test.rs:11/210` 使用；agent 循环直接读写 `AgentState.context`（`react_agent.rs` 无 `dyn Memory`）。Java 侧 `memory/Memory.java` 亦已 `@Deprecated since 2.0.0`（上下文改由 `AgentState` 持有）。 | `memory/Memory.java#@Deprecated since 2.0.0`；`memory/StateBackedMemory.java#@Deprecated` | `rg -n 'dyn Memory|Arc<.*Memory>|Box<dyn Memory' crates` 零命中；`rg -n 'InMemoryMemory|StateBackedMemory' crates` 仅 `memory/mod.rs` 与 1 个集成测试 + `extensions-mem/src/lib.rs` 注释 | 待裁定 | 二选一：①删 trait 与两实现（对齐 Java 2.0 方向，`MEMORY_MESSAGES_KEY` 保留给 v1 迁移 CORE-08 用）；②保留并标注 `#[deprecated]` 兼容层。需先确认无外部 crate 依赖（当前 grep 未见） |
| CORE-14 | P2 | **`@Tool` 注解等价物（proc-macro + schema 自动生成）缺失**：Java 用注解 + 反射生成 JSON Schema 与参数注入；Rust 需手写闭包与 `ToolSchema`（`tool/tool.rs:194 FunctionTool`），schema 生成靠 `tool/schema.rs` 手工 builder。属「生态差异」还是「需补 `agentscope-macros` crate」需裁定。 | `tool/Tool.java#@Tool`；`tool/ToolSchemaGenerator.java`；`tool/ToolMethodInvoker.java`；`tool/RegisteredToolFunction.java` | `rg -n 'ToolSchemaGenerator' crates -g '*.rs'` 仅 `tool/schema.rs` 的「mirrors」注释；`rg -n 'RegisteredToolFunction|ToolMethodInvoker|ReflectiveFunctionTool' crates` 零命中；仓库无 `agentscope-macros` crate | 待裁定 | 若裁定补宏：新建 `crates/agentscope-macros`（`#[tool]`/`#[tool_param]` 生成 `ToolSchema` + 反序列化），并让 `FunctionTool` 接受生成结果；若不补：在 `tool/schema.rs` 文档明确「schema 手工声明」并在模板/示例补最佳实践 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| CORE-15 | 事件类型表 32/32 对齐（含 `TOOL_PROGRESS`/`HINT_BLOCK`/`ALL_TOOLS_DENIED`/`SUBAGENT_EXPOSED`） | `event/AgentEventType.java` | `event/event_type.rs:6`（32 变体 + `wire_name`/`from_wire_name` 兼容别名） | 已完成 |
| CORE-16 | `Middleware::on_acting` 已接线（原「零调用」结论**已失效**） | `middleware/MiddlewareBase.java#onActing` | `middleware/middleware.rs:329 wrap_acting` → `react_agent.rs:2933`；测试 `react_agent.rs:6496 acting_middleware_is_invoked_and_the_tool_still_runs_once`、`:6544 acting_middleware_that_completes_without_results_is_a_diagnosable_error` | 已完成 |
| CORE-17 | `GracefulShutdownManager::bind_state_saver` 已存在并接线（原「无等价物」结论**已失效**） | `shutdown/GracefulShutdownManager.java#bindStateSaver` | `shutdown/mod.rs:312/321/329`；接线点 `harness/src/agent/harness_agent.rs:2231` | 已完成 |
| CORE-18 | `AgentState` 默认生成 `session_id`（与 Java 一致，**非差异**） | `state/AgentState.java:72`（`builder.sessionId == null ? newHex() : ...`） | `state/agent_state.rs:44-46`（`new_hex()` = uuid 去连字符） | 已完成 |
| CORE-19 | 版本化/CAS 状态存储 | `state/AgentStateStore.java` + `state/VersionedState.java`/`ConflictPolicy.java` | `state/store.rs:35`（trait，`save_if_version:135`）、`state/conflict_policy.rs`；测试 `tests/state_store_versioning_test.rs`（`in_memory_supports_versioning_and_tracks_versions`/`in_memory_cas_conflict_returns_unversioned_without_writing`/`json_file_store_degrades_to_unconditional_writes`） | 已完成 |
| CORE-20 | 原生 session 日志（commit chain / 恢复 / 快照 / 投影 / inbox / 迁移） | `session/JournalSessionLog.java`、`SessionRecorder.java`、`SessionRecovery.java`、`SessionViews.java`、`SessionInbox.java`、`SessionMigration.java` | `session/log.rs:49 SessionLog`/`:199 SessionLogStore`、`journal.rs:78`、`recorder.rs:376`、`views.rs`（含 `continuation_turn:693` 对齐 `SessionInteractions.continuationTurn`）、`recovery.rs`、`snapshot.rs`、`inbox.rs`、`migration.rs:27/87/95/128`；测试 `tests/session_{acting_checkpoint,settle_loop,step_boundary,verification}_*.rs` | 已完成 |
| CORE-21 | 结构化输出（原生 schema + 校验 + `generate_response` 回退 ≤3 次 + `ToolChoice::Specific`） | `model/StructuredOutputReminder.java`、`model/ToolChoice.java`、`tool/ToolValidator.java` | `tool/validator.rs:38 validate_json_output`、`react_agent.rs:1024 call_fallback_structured`、`model/tool_choice.rs`；测试 `react_agent.rs:6941/7051` | 已完成 |
| CORE-22 | `ToolProgress` 事件（工具经 emitter 发布的中间进度） | `event/ToolProgressEvent.java`、`tool/ToolEmitter.java` | `event/event_type.rs:29 ToolProgress`；发射 `react_agent.rs:2791`；`tool/emitter.rs:16 ToolEmitter`；测试 `react_agent.rs:4793 tool_progress_events_are_suppressed_when_acting_chunks_are_filtered` | 已完成 |
| CORE-23 | 权限引擎（5 模式 / 4 行为 / 规则 + 附加工作目录） | `permission/PermissionEngine.java`、`PermissionMode.java`、`PermissionRule.java`、`AdditionalWorkingDirectory.java` | `permission/mod.rs:11/27/40/67/216`；测试 `tests/state_memory_permission_test.rs` | 已完成 |
| CORE-24 | 每调用工具面（`ToolRequestConfig` + `ToolMergeMode` + preset 合并） | `tool/ToolRequestConfig.java`、`tool/ToolMergeMode.java` | `tool/tool_request_config.rs:24/62`、`toolkit.rs:332 merge_presets`；测试 `tests/tool_request_config_test.rs`、`tests/tool_governance_test.rs::preset_parameters_override_input` | 已完成 |
| CORE-25 | plan-mode 白名单/拒绝（DENIED 结果 + 不执行 + 不触发权限询问） | `state/PlanModeContextState.java`；`harness/agent/middleware/PlanModeMiddleware.java#PlanModeMiddleware`（Java 侧强制在 harness 中间件，core 只持状态） | `react_agent.rs:3911 plan_mode_denies`（`PLAN_MODE_ALWAYS_ALLOWED`/`PLAN_MODE_DENY_MESSAGE`）、`state/context.rs PlanModeContextState`；测试 `react_agent.rs:6684 plan_mode_denies_mutating_tool` | 已完成 |
| CORE-26 | 内置 `todo_write` 工具 | `tool/builtin/TodoTools.java` | `tool/todo.rs:47 TodoWriteTool`（`todo_write`） | 已完成 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **主干可用**：agent 循环（`react_agent.rs`，含迭代上限、returnDirect 短路、结构化输出回退、中断/恢复、checkpoint）、消息/事件模型、工具系统（Toolkit/Group/Executor/Schema/Validator/Emitter/RequestConfig）、状态与 CAS、原生 session 日志、权限、中断、关停、观测、formatter 均已落地，且**每项都有 core 内单测**（`src/` 380 处 `#[test]` 标记，`tests/` 12 个集成测试文件）；本表未运行任何测试，测试数以源码统计为准。
- **跨 crate 归属**（不算 core 缺口）：`skill` → `harness/src/agent/skill/*` + `extensions-skills`；`rag` → `extensions-rag`；`tool/mcp`、`tool/subagent`、`tool/coding`(工具本体)、`tool/file`(增强版) → `harness`；`tracing`(OTel) → `extensions-studio`；模型 provider / HTTP 传输 → `extensions-model`。
- **已知未闭环**：CORE-01（P0，middleware 停流）、CORE-02/03/04/05（P1）、CORE-06..12（P2）、CORE-13/14（待裁定）。

### 4.2 下一步与阻塞
1. **CORE-01 优先**：停流语义属 agent 循环契约，`extensions-judge` 已在发 `RequestStop`（`judge/src/jev/runtime/middleware.rs:1184/1201`），不修则该扩展的监督停流静默失效；改动集中在 `react_agent.rs` 两处消费点，须单智能体独占串行。
2. **跨模块需求（不由本 crate 修）**：
   - CORE-10 的 `context_build` `Custom` 事件、CORE-12 的 `SubagentExposed` 发射，均落在 `crates/agentscope-harness`，需在 harness tracker 登记执行；
   - CORE-05 修完后，`crates/agentscope-extensions-model/src/common/tool_search.rs:6` 的陈旧注释（「Search itself lives in core」）需同步更正该 crate 注释；
   - CORE-03 若上提为「provider 级 failover」，需与 `extensions-model` 的 `ModelError::to_core()` 分类对齐（重试 vs 回退边界）。
3. **待裁定转待办**：CORE-13（`Memory` 兼容层去留）、CORE-14（是否新建 `agentscope-macros`）需在汇总层裁定后再动 core，避免影响 20+ 依赖 crate。
4. **测试护栏**：core 改动后必须跑 `cargo test -p agentscope-core`（本 tracker 不记录未实测结果）；跨 crate 语义（CORE-01）另需至少一条 `agentscope-extensions-judge` 回归测试。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| CORE-D01 | `io.agentscope.core.hook` 整包（20 文件 / 2532 行，含 `Hook`/`HookEvent`/`HookEventType`/`LegacyHookDispatcher` 及 14 个 `*Event`） | `hook/Hook.java#@Deprecated since 2.0.0. Use MiddlewareBase instead`（18 个文件带 `@Deprecated`） | 整包已废弃；Rust 由 `Middleware` 洋葱链（`middleware/middleware.rs:15` + `ExtensionPoint`）取代，语义覆盖 `Pre/PostReasoning`、`Pre/PostActing`、`Pre/PostCall`、chunk 通知 | 无：hook 的每个扩展点均有 middleware 对应物（`on_agent_state_ready`/`on_reasoning_result`/`on_acting` 等） |
| CORE-D02 | `LongTermMemory` / `LongTermMemoryMode` / `LongTermMemoryTools` / `StaticLongTermMemoryHook` | `memory/LongTermMemory.java#@Deprecated(forRemoval=true, since="2.0.0")`（4 个文件） | Java 明确「long-term memory is removed」；Rust 长期记忆由 `extensions-mem`（mem0/bailian/reme）实现活跃的 `Memory` 语义，**不重建**废弃 trait（见 `extensions-mem/src/lib.rs` 顶部说明） | 无：`StaticLongTermMemoryHook` 依赖的 Hook 系统亦已废弃（CORE-D01） |
| CORE-D03 | `AgentStateMemoryView` | `memory/AgentStateMemoryView.java#@Deprecated since 2.0.0. New code should read context directly from AgentState` | Rust 直接暴露 `AgentState::context()/context_mut()`（`state/agent_state.rs:74/77`），无需视图包装 | 无 |
| CORE-D04 | `agent/user/*`（`UserAgent`/`UserInputBase`/`UserInputData`/`StreamUserInput`） | `agent/user/UserAgent.java#@Deprecated`（4 个文件） | 整组废弃；Rust 用户输入以 `RuntimeContext.user_input`（`runtime_context.rs:155`）+ `Msg` 表达 | 无 |
| CORE-D05 | legacy 事件 API：`agent/Event`/`EventSource`/`EventType`/`StreamableAgent`/`StreamingHook` | `agent/Event.java#@Deprecated`、`agent/StreamableAgent.java#@Deprecated`、`agent/StreamingHook.java#@Deprecated` | Rust 统一走 `AgentEvent` + `StreamOptions`（`event/event.rs:21`、`agent/stream_options.rs`）+ `ObservableAgent`（`agent/agent_traits.rs:15` / `ObservableAgent:62`） | 无 |
| CORE-D06 | `rag` 整包（`GenericRAGHook`/`Knowledge`/`KnowledgeRetrievalTools`/`RAGMode`/`model/*`） | `rag/GenericRAGHook.java#@Deprecated`（7 个文件全废弃） | 能力已在新扩展 crate 重建：`crates/agentscope-extensions-rag`（bailian/dify/haystack/ragflow/simple） | 无：知识检索以扩展形式提供 |
| CORE-D07 | `skill/SkillHook` | `skill/SkillHook.java#@Deprecated` | Rust 技能生命周期由 `harness/src/agent/skill/*`（middleware/runtime/curator）承担，不再走 hook | 无 |
| CORE-D08 | `tracing/Tracer`/`TracerRegistry`/`NoopTracer` + `hook/recorder/JsonlTraceExporter` | `tracing/Tracer.java#@Deprecated since 2.0.0. Use OtelTracingMiddleware instead` | 追踪改由 OpenTelemetry 路径：`extensions-studio/src/telemetry/tracing.rs`（`TelemetryTracing` + `TraceExporter`）；core 只留 `tracing.rs:1-13` 日志初始化 | 无：`OtelTracingMiddleware` 的等价物在 studio 扩展 |
| CORE-D09 | `tool/ContextStore`/`DefaultContextStore`/`ToolExecutionContext`/`ToolExecutionContextProvider`/`ToolCallParam` | `tool/ToolExecutionContext.java#@Deprecated. Use RuntimeContext instead`（5 个文件） | 类型化上下文容器由 `RuntimeContext`（`runtime_context.rs:151`，含 `metadata`/`extensions`/`toolkit`）取代；`ToolCallParam` 的调用快照形态由 `ToolUseBlock` + `ToolExecutionContext`（`tool/tool.rs:13`）承担 | 无 |
| CORE-D10 | `model/transport/websocket/*`（9 文件：`WebSocketTransport`、`Jdk*`/`OkHttp*`、`WebSocketConnection`、`Config`、`Exception`、`CloseInfo`） | `model/transport/WebSocketTransport.java` | Java 侧**在 core/extensions 内无任何使用者**（`rg 'WebSocketTransport' agentscope-extensions` 零命中），属预留基础设施；Rust 无对应 WS 协议 provider | 无：若未来接实时 WS 模型，再按需在 `extensions-model` 引入（`extensions-channel`/`extensions-studio` 已在用 `tokio-tungstenite`，依赖可复用） |
| CORE-D11 | `util/*`（`JacksonJsonCodec`/`JsonCodec`/`JsonException`/`JsonUtils`/`TypeUtils`/`ExceptionUtils`/`JsonSchemaUtils`/`MessageUtils`） | `util/JacksonJsonCodec.java`、`util/TypeUtils.java` 等 | JVM 专属：Jackson 绑定 → `serde`；类型擦除反射工具 → Rust 泛型/trait；`JsonSchemaUtils` → `formatter/json_schema.rs`；`MessageUtils` 的文本辅助 → `formatter`（`format_role_label`/`extract_text_content`） | 无 |
| CORE-D12 | `state/State` 标记接口 | `state/State.java`（序列化标记，`ToolkitState implements State`） | Rust 用 `serde::Serialize/Deserialize` 表达可持久化形状，无需标记 trait | 无 |
| CORE-D13 | `shutdown/AgentScopeJvmShutdownHook`；`shutdown/ShutdownSessionBinding` | `shutdown/AgentScopeJvmShutdownHook.java`；`shutdown/ShutdownSessionBinding.java#@Deprecated since 2.0.0` | 无 JVM：宿主显式调用 `perform_graceful_shutdown`（`shutdown/mod.rs:352`）并提供信号处理（harness `agent/shutdown.rs:3` 注明为等价入口）；`ShutdownSessionBinding` 已由 `ShutdownStateSaver`（`shutdown/mod.rs:104/312`）取代 | 无：语义等价，仅触发方式不同 |
| CORE-D14 | `tool/ReflectiveFunctionTool`/`RegisteredToolFunction`/`ToolMethodInvoker`/`ToolParam`/`ToolSchemaModule`/`ToolSchemaProvider` | `tool/ReflectiveFunctionTool.java`、`tool/ToolMethodInvoker.java`、`tool/ToolSchemaProvider.java` | 全部依赖 Java 注解 + 反射 + 类型擦除，Rust 无对应运行时反射模型；等价能力由 `FunctionTool`（`tool/tool.rs:194`）+ `ToolSchema` builder 表达（是否补宏见 CORE-14 待裁定） | 中：工具定义样板的书写成本高于 Java（功能不受限） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| CORE-X01 | AgentState 槽位模型 | `ReActAgent` 每次调用绑定**单个** call-scoped `AgentState`（`runtime_context.rs:186 state`），由 `AgentStateStore` 按 `(user_id, session_id)` 键持久化 | Java 在 `ReActAgent` 内以 `ConcurrentHashMap` 维护 `(userId, sessionId)` 槽位（`ReActAgent.java:337 stateCache` / `:486 slotKey(uid,sid)` / `:357 permissionEngineCache`） | Rust 单槽把并发隔离上移到 call gate（`agent/lifecycle.rs:15 CallGate`，按 key 串行）+ 存储层；减少跨会话状态泄漏面。宿主如需多会话并发，按 session 建 agent 实例（ACP/AISTIO 已如此接线） |
| CORE-X02 | 中断/崩溃恢复的接线方式 | `enable_pending_tool_recovery`（`react_agent.rs:379/1364`）+ state 无条件写回 + `StateCheckpointMiddleware`（每迭代落盘）+ `Err(Error::Interrupted) → reconcile_interrupted`；未开 recovery 时 fail-fast `Pending tool calls exist without results` | Java `doCallInner` 在 `GenerateReason::PERMISSION_ASKING`（`ReActAgent.java:2973/3549/3714`）与 `Msg.METADATA_CONFIRM_RESULTS`（`ReActAgent.java:2217/2294`）上做 ask→resume 循环，另有 resume 分支 | 显式开关让「重放工具」风险可控（工具副作用不可重放），并用 checkpoint + 会话日志（`session/recovery.rs`）替代隐式恢复；语义等价、无 32 轮上限 |
| CORE-X03 | 命令安全校验 | `tool/coding.rs`：allowlist/denylist 轻量校验（模块注释明示「lightweight validator … instead of a full Bash AST」） | Java `UnixCommandValidator`/`WindowsCommandValidator` 基于 tree-sitter Bash AST + 平台分支 | 避免引入 tree-sitter 语法依赖与平台分支；危险路径/命令基线由 `tool/dangerous.rs`（对齐 `ToolDangerousPathConstants`）承担。代价：对复杂管道的判定粒度更粗（可在 harness 侧加强） |
| CORE-X04 | 重试与退避默认值 | `ExecutionConfig::model_defaults()`：`max_attempts=7`、退避 2→4→8→16→32→60s、单次 300s（`model/execution_config.rs:40-54`） | Java `ModelConfig.DEFAULT_MAX_RETRIES = 3`（单次调用） | Rust 侧把「网关限流/短时不可达」的恢复覆盖到位（约 2 分钟等待预算）；Java 的 3 次在长尾故障下明显不够。注意：这不替代 CORE-03 的**换模型**回退 |
| CORE-X05 | 错误模型 | 单一 `Error` 枚举（`error.rs:8`）+ `AgentExceptionInfo` 承载结构化原因；`CompositeAgentException` 支持聚合 | Java 异常类层级（`Exception`/`RuntimeException` 派生）+ 各包自有异常类型（`ToolSuspendException`、`SessionLogException`、`ModelHttpException`…） | Rust 无异常，用 `Result` + 枚举分类（`Error::Session` 前缀 `session log: `、`Error::Http` 等），便于跨 async 边界传递与匹配；`ToolSuspendException` 仍以具体类型保留（`tool/suspend.rs:11`） |
| CORE-X06 | 子代理事件投递 | 异步子代理：`agent_spawn` 提交 run，结果以后续 `<system-reminder>` / inbox hint 回投父会话（`harness/src/tool/agent_spawn.rs:6-8`）；子代理事件不注入父流 | Java `SubagentEventBus`：同步调用的子代理把 `Event` 推入父 `Flux<Event>`（Reactor Context 键 `agentscope.subagent.event.bus`） | Rust 无 Reactor Context，且父 turn 不应被子代理阻塞（`agent_spawn` 非阻塞设计）；代价：父流看不到子代理的细粒度事件，需靠 `SubagentExposed`（CORE-12）与 inbox 通知补偿 |
| CORE-X07 | 执行身份表达 | `RuntimeContext` 平铺 `agent_id`/`session_id`/`user_id`/`run_id`（`runtime_context.rs:167-176`）+ `session::recorder::TURN_ID_CONTEXT_KEY` | Java `ExecutionIdentity(agentId, sessionId, turnId, runId)` record + `CONTEXT_KEY` 一次性取用 | Rust 的 Context 是可扩展 map（`Extensions`），平铺字段便于中间件/工具零成本读取；turn id 只在会话录制路径需要，故放 session 侧常量。语义等价（四元组齐备） |
| CORE-X08 | 模型传输层 | 单实现 `reqwest` + `SharedTransport` seam（`extensions-model/src/common/transport.rs`），可注入 mock | Java 双实现 `JdkHttpTransport`/`OkHttpTransport` + `HttpTransportFactory` + `ProxyConfig`/`HttpVersion` | Rust 的 reqwest 同时覆盖系统代理/TLS，避免维护两套实现；seam 形态让离线测试可注入内存传输（各 provider 已用）。差异仅在可配置面（无显式 `HttpVersion` 选择） |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-core` 基线。
- **core 改动必须单智能体串行独占**（`react_agent.rs`、`state/**`、`event/**` 被全仓依赖）；其它 crate 的并行任务不得触碰 core。
- 改动中：保持 `cargo build -p agentscope-core` / `cargo test -p agentscope-core` 通过；新增行为必须有离线单测（DTO serde 往返、纯函数映射、trait 注入/mock），不得 `#[ignore]` 凑绿、不得删既有断言。
- 对齐优先：实现细节（签名/语义/边界/错误分类/默认值）以 Java 为基线；Rust 已有的**修复与优化**不得因「Java 不是这样」而回退（此类记 `CORE-X*`）。
- 完工后：更新 §3.5 条目状态与 §4；涉及跨 crate 的项在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记；执行记录写入 `docs/extensions/history/`，**不要**在本文件追加历史章节。
- 不修改其它模块的 crate / tracker / `_TEMPLATE.md`；不修改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 引用结论前先复核：本文件亦会过时，动手前用 `rg` 验证「Rust 现状」列（尤其标 `零命中` 的行），避免按陈旧结论重复实现或误删。
