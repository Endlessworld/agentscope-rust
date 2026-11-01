# STUDIO 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **STUDIO** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-studio\`
> **Java 子模块**：`io.agentscope.core.studio` + `io.agentscope.core.tracing.telemetry`
> **Rust 目标**：`crates/agentscope-extensions-studio/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-studio` |
| Java 子模块 | `io.agentscope.core.studio` + `io.agentscope.core.tracing.telemetry` |
| Java 规模（实测 2026-10-22） | 主代码 28 文件 / **4305 行**（非空行 3738）；测试 10 文件 / **1837 行**（非空行 1554）；`@Test` 方法 91 个 |
| Rust crate | `crates/agentscope-extensions-studio` |
| Rust 规模（实测 2026-10-31） | 19 个 `.rs` / **6998 行**（非空行 6363）/ **92 个内联测试**（62 `#[test]` + 30 `#[tokio::test]`）—— 上列为复核时点快照；同日本轮又新增 3 内联 + 1 集成测试（见下注）|
| 迁移波次 | C |
| 当前状态 | **存量 §3.1–§3.4 口径：已完成**（P1/P2 清零；2026-10-31 实测 92 passed / 0 failed）—— Socket.IO 真实传输 + OTLP 降级导出 + `TelemetryTracing` 已落地；`StudioManager` 自动装配与多模态抽取为「设计决定（不迁移）」（§4.2 第 4 条），仅 §4.2 第 6 条 `format` span 自动接线保留为**跨模块需求**（core 无 formatter 钩子）。**2026-10-31 全量复核另开 §3.5 缺口（2 P0 / 4 P1 / 5 P2），其中 STUDIO-01/02/04 当日已修复，余 STUDIO-05/06 与 P2，状态以 §3.5.1 为准**。**2026-11-01 Round 4 后 §3.5.1 的 7 项已全部关闭（109 内联 + 1 集成 passed / 0 failed），表中已无 `待办` 行**。历史见 history/STUDIO__MIGRATION_HISTORY.md |
| 行数口径 | Java 4305/1837 与 Rust 5048 均为**物理行**（含空行/注释）；非空行口径 Java 3738/1554、Rust 4578。汇总 tracker 的 3700/1452 与之同族，需按口径换算 |

> **2026-10-31 全量对齐复核**：上表「当前状态」为当日之前快照；本轮逐文件复核新增 **2 项 P0**（`STUDIO-01` Manager 装配下 Socket.IO 输入回环断链、`STUDIO-02` span 无周期/批量导出），另有 4 P1 / 5 P2 / 2 待裁定，详见 §3.5 与 §4.3。存量 §3.1–§3.4 未改动。
>
> **2026-10-31 同日本轮落地（studio 子智能体）**：`STUDIO-01` / `STUDIO-02` / `STUDIO-04` 已修复（行内【2026-10-31 落地】）；`STUDIO-12` / `STUDIO-13` 按批次约定跳过（等 core 的 `ExtensionPoint::OnFormat` / `Model::provider_name`）。新增 4 条测试：`tests/manager_input_loop.rs::the_manager_assembly_delivers_user_input_to_wait_for_input`（集成）+ `spans_are_exported_on_the_schedule_without_a_flush` / `a_full_batch_is_exported_immediately` / `cache_reasoning_and_tool_use_usage_are_max_aggregated`（单测）。无新增 crate，仅本 crate dev-dependencies 的 `tokio` 追加 `net` / `test-util` feature（本地假 WS 服务 + `tokio::time::pause`）。
>
> **2026-11-01 Round 2（Wave B）落地**：关闭 **STUDIO-06**（导出失败重试）/ **STUDIO-12**（框架级 `format` span，走 core `ExtensionPoint::OnFormat`）/ **STUDIO-13**（`Model::provider_name` 生效 + `with_provider_name` 失效可见）。实测 **102 内联 + 1 集成 = 103 passed / 0 failed / 0 ignored**（HEAD 基线实测 95+1=96；上文「92」为旧口径快照）。无新增依赖、未改 core/harness。
>
> **2026-11-01 Round 4（缺口收敛）落地**：关闭 §3.5.1 剩余 7 项 —— **STUDIO-03**（`connect()` 等命名空间 ACK：`ConnectionState` + `wait_ready` + 10s 默认超时）、**STUDIO-05**（心跳 watchdog：`pingInterval + pingTimeout` 静默即断开重连）、**STUDIO-07**（取消路径 span 状态 `Ok`→`Unset`）、**STUDIO-08**（registerRun 时间戳改用本地时区）、**STUDIO-09**（`StudioManagerBuilder` 增 `transport`/`http_transport` 旋钮，默认仍 `NoopStudioTransport`）、**STUDIO-10**（`structuredInput` 默认 `{}` + `StudioUserAgent::agent_id`）、**STUDIO-11**（终端提示符 + 可注入 `TerminalIo`）。实测 **109 内联 + 1 集成 = 110 passed / 0 failed / 0 ignored**（Round 2 基线 103，+7）。无新增依赖、未改 core/harness。

## 2. Java 清单与依赖映射

> Java 主包 `io.agentscope.core.studio`（HTTP/WS/管理）与 `io.agentscope.core.tracing.telemetry`（OTel GenAI 遥测）。

### 2.0 子模块清单
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `studio.StudioConfig` | 连接配置（url/tracingUrl/project/runName/runId/maxRetries/reconnect*）| `studio::StudioConfig` + `StudioConfigBuilder` | 默认值对齐：url=`http://localhost:3000`、project=`UnnamedProject`、runName=`run_<ts>`、runId=UUID、maxRetries=3、reconnectAttempts=3、delay=1s、maxDelay=5s；tracingUrl 默认 `studioUrl + /v1/traces` |
| `studio.StudioClient` | OkHttp HTTP 客户端（registerRun/pushMessage/requestUserInput + 重试 + shutdown）| `studio::StudioClient` | reqwest + 指数退避重试（base 100ms，max 5s，`max_retries` 次）；时间戳 `yyyy-MM-dd HH:mm:ss.SSS`。**静态复核**：Rust 仅对 `StudioError::Transport` 重试，Java 因非 2xx 被包成 `IOException` **也会重试**（语义分歧，见 history/STUDIO__MIGRATION_HISTORY.md P2-1）|
| `studio.StudioWebSocketClient` | Socket.IO 连接 `/python` namespace，监听 `forwardUserInput`，`waitForInput` 等待 Mono | `studio::StudioWebSocketClient` + `studio::socket_io` | `StudioTransport` trait（可插拔）+ `PendingInputRegistry`（`HashMap<requestId, oneshot>`）；**`SocketIoTransport` 为真实实现**（tokio-tungstenite 0.29 + 自实现 Engine.IO v4 握手/`40` 入房/ping-pong/指数退避重连）；帧解析为纯函数。**鉴权键已对齐 Java（2026-10-28，§4.1.0v）**：`auth_payload()` 默认发 `{"run_id": <runId>}`（Java `StudioWebSocketClient.java:98`），旧 `runId` 仅在 `SocketIoTransport::with_legacy_auth_key()` 显式开启时一并发送 |
| `studio.StudioWebSocketClient.UserInputData` | 用户输入载荷（blocksInput + structuredInput）| `studio::UserInputData` | |
| `studio.StudioManager` | 全局静态单例（config/client/wsClient + init builder + shutdown + TracerRegistry 注册）| `studio::StudioManager` | Rust 用 `OnceLock<RwLock<Option<...>>>` 全局；不依赖 core `TracerRegistry`（core 无此类型，见 §4.2） |
| `studio.StudioMessageHook` | `Hook`，拦截 `PostCallEvent` 把最终消息推给 Studio | `studio::StudioMessageHook` | Rust 无 `Hook`（core `hook` 为设计决定不迁移）；改为 core `Middleware`：`on_agent` 包裹后在事件流上捕获终态消息并推送（fire-and-forget）。**已完成（2026-10-29 勘误）**：原记「静态复核：捕获键为 `fields["message"]`…→ 转发恒不触发（见 history/STUDIO__MIGRATION_HISTORY.md P0-1）」，实际已于 2026-10-22 修复（见 history/STUDIO__MIGRATION_HISTORY.md）：`final_message` 优先 `fields["reply"]`，`fields["message"]` 仅作第三方回退（`studio/message_hook.rs:88-97`）|
| `studio.StudioUserAgent` | 用户代理 Agent（终端 / Studio 双模式）| `studio::StudioUserAgent` | 输出 Msg（USER 角色），Studio 模式失败回退终端 |
| `studio.pojo.RegisterRunRequest` | registerRun 请求体 | `studio::pojo::RegisterRunRequest` | 字段：id/project/name/timestamp/pid/status/run_dir |
| `studio.pojo.PushMessageRequest` | pushMessage 请求体 | `studio::pojo::PushMessageRequest` | 字段：runId/replyId/name/role/msg |
| `studio.pojo.RequestUserInputRequest` | requestUserInput 请求体 | `studio::pojo::RequestUserInputRequest` | 字段：requestId/runId/agentId/agentName/structuredInput |
| `studio.pojo.UserInputMetadata` | 用户输入元数据（source/requestId/structuredInput）| `studio::pojo::UserInputMetadata` | `@JsonInclude(NON_NULL)`；source 默认 `studio` |
| `telemetry.GenAiIncubatingAttributes` | OTel GenAI 语义约定属性键（**semantic conventions 1.41.0**，Java 类注释 `GenAiIncubatingAttributes.java:28`；Rust 模块文档 2026-10-31 同步为 1.41.0）| `telemetry::gen_ai_attributes` | 常量字符串键（`gen_ai.*`）+ operation/provider 取值常量。**已补齐（2026-10-31，见 history/STUDIO__MIGRATION_HISTORY.md）**：38 键与 Java `AttributeKey` 声明一一对应（新增 `GEN_AI_TOOL_TYPE` + 3 个 usage 键），并有「常量 vs Java 字面量」双向交叉校验用例。`opentelemetry-bom` 1.66→1.67（`8690ab3f5`）只动 SDK，不改键集 |
| `telemetry.AgentScopeIncubatingAttributes` | AgentScope 自定义属性键 | `telemetry::agent_scope_attributes` | `agentscope.function.*`、`agentscope.format.*`、`agentscope.usage.*`。**已补齐（2026-10-31）**：6 键与 Java 一一对应（新增 `AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS`）|
| `telemetry.AttributesExtractors` | 从 agent/model/tool/formatter 抽取 OTel 属性（实测 648 行）| `telemetry::extractors`（**同名模块，不存在 `AttributesExtractors` 类型**）| 纯函数（`get_*_attributes`）；复用 core 的 Msg/ContentBlock/ChatResponse/ToolSchema/GenerateOptions |
| `telemetry.AttributesExtractors.FormatterConverter` | formatter 简单类名 → provider 名映射 | `telemetry::FormatterConverter` | DashScope/Gemini/OpenAI→dashscope；Anthropic→anthropic；其余 unknown |
| `telemetry.AttributesExtractors.ProviderNameConverter` | ChatModelBase 实例 → provider 名（按类继承链判断）| `telemetry::ProviderNameConverter` | Rust 无运行时类反射；改为按模型类型名后缀探测（`DashScopeChatModel`/`GeminiChatModel`/`AnthropicChatModel`/`OpenAIChatModel`）。**设计取舍**：`TelemetryTracing` 只用 `DEFAULT_PROVIDER_NAME="unknown"`/宿主 `with_provider_name` 覆盖（拿到的只是 `Arc<dyn Model>`，无类名）——2026-10-28 复核确认无法在本 crate 内反射出 provider，未随 见 history/STUDIO__MIGRATION_HISTORY.md P1-1 关闭 |
| `telemetry.StreamChatResponseAggregator` | 流式 ChatResponse 聚合（text/thinking/tool_calls + usage max + finishReason）| `telemetry::StreamChatResponseAggregator` | 复用 core `ChatResponse`/`ChatUsage`/`ContentBlock`。**已接入（2026-10-28，§4.1.0v）**：`TelemetryTracing::on_model_call` 用 `AggregatingModel` tee 模型流逐 chunk `append`（等价 Java `doOnNext(aggregator::append)`），span 结束时写 `get_llm_response_attributes` |
| `telemetry.TelemetryTracer` | `Tracer` 实现：agent/model/tool/**format** 四类 span + OTLP HTTP 导出 | **`telemetry::TelemetryTracing`**（**不存在 `TelemetryTracer` 类型**）| 包装为 core `Middleware`：`on_agent`/`on_model_call`/`on_acting`/`on_format`（无 `on_reasoning`，core 无该必要）+ `call_format`（宿主自格式化时的同步包装器）；OTLP 导出为降级 exporter（`telemetry::exporter`，见 §4.1.0s）。**2026-10-28 已补齐（§4.1.0v）**：`agentscope.function.name`（四类 span）、三个响应侧抽取器、format span。**2026-10-31 裁定（裁决 B5，见 history/STUDIO__MIGRATION_HISTORY.md）**：Java `Tracer` 接口语义已完全映射到既有 seam，不再新增 `TelemetryTracer` 类型——`callAgent/callModel/callTool` → `Middleware::{on_agent,on_model_call,on_acting}`，`callFormat` → `Middleware::on_format`（框架内，2026-11-01 起可见 STUDIO-12）/`TelemetryTracing::call_format`（宿主自格式化），`Tracer.shutdown()` → `flush()`，exporter 注入 → `TelemetryTracing::new(Arc<dyn TraceExporter>)`（离线用 `RecordingTraceExporter`），`TracerRegistry.register` → 宿主 `MiddlewareChain::add` |
| `telemetry.model.*`（**13 个类**）| OTel 消息部件模型（Text/Reasoning/ToolCallRequest/ToolCallResponse/Blob/File/Uri Part、Input/OutputMessage、Role、SystemInstructions、ToolDefinition）| `telemetry::model`（13 个 `pub` 类型，与 Java 一一对应）| serde 结构；`type` 字段判别 + JSON 字段名对齐（`mime_type`/`file_id`/`finish_reason`）|

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp3 (OkHttpClient) | Studio HTTP | `reqwest`（**workspace 0.13**，json/rustls）| 仅写本 crate Cargo.toml；根 `[workspace.dependencies]` 已有 reqwest（0.13），直接用 `reqwest = { workspace = true }` |
| reactor.core (Mono/Flux/Sinks) | 响应式 | `tokio::sync::oneshot/mpsc` + `futures::Stream` | 单值→async fn；等待→oneshot |
| socket.io-client（java） | Studio WS | `tokio-tungstenite = 0.29`（本 crate 直声明，default-features=false + connect/handshake/rustls-tls-webpki-roots）+ 本 crate 自实现 Engine.IO v4/Socket.IO 帧 | 已落地 `studio::socket_io`（见 §4.1.0s）；仅写本 crate `Cargo.toml`，未改根 `[workspace.dependencies]` |
| com.fasterxml.jackson（JSON） | 序列化 | `serde` / `serde_json` | 已建 |
| org.json (JSONArray/JSONObject) | WS 事件解析 | `serde_json::Value` | |
| reactor.util.retry.Retry | 重试退避 | `StudioClient::post_json` 内联退避循环（**无独立 `retry_with_backoff` 辅助函数**）| base 100ms → max 5s；重试谓词与 Java 有分歧（见 history/STUDIO__MIGRATION_HISTORY.md P2-1）|
| opentelemetry-api / sdk / exporter-otlp-http | OTel span + 导出 | **降级路径已落地**：自建 `Span` 模型 + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait（`OtlpHttpExporter` 真实 reqwest 实现，支持自定义 header）| 不引入 `opentelemetry`/`opentelemetry-otlp`（避免根 `[workspace.dependencies]` 变更）；见 §4.1.0s。**语义差异（已裁定）**：Java `OtlpHttpSpanExporter` 默认 protobuf，本 crate 发 JSON（`Content-Type: application/json`），OTLP/HTTP 协议允许两种编码；`opentelemetry-bom` 1.66→1.67 对本 crate 无影响 |
| io.opentelemetry.instrumentation.reactor | Reactor 上下文传播 | core `Middleware` 洋葱链天然携带 `RuntimeContext` | 无需等价物 |
| slf4j | 日志 | `tracing` | 已建 |

### 2.2 Rust 侧复用点（不重复发明）
| 需求 | 复用的既有类型 | 位置 |
|---|---|---|
| 消息与内容块 | `Msg` / `MsgRole` / `ContentBlock`（Text/Thinking/ToolUse/ToolResult/Image/Audio/Video）| `agentscope_core::message` |
| 模型响应/用量/参数 | `ChatResponse` / `ChatUsage` / `GenerateOptions` / `ToolSchema` | `agentscope_core::model` |
| 工具与描述 | `Toolkit::get_tool` / `Tool::description` | `agentscope_core::tool` |
| 拦截点 | `Middleware`（on_agent/on_reasoning/on_acting/on_model_call/on_system_prompt）+ `AgentInput`/`ReasoningInput`/`ActingInput`/`ModelCallInput` | `agentscope_core::middleware` |
| 事件流 | `AgentEvent`（AgentResult 等）| `agentscope_core::event` |
| 上下文 | `RuntimeContext` | `agentscope_core::agent` |

## 3. 详细缺口清单
### 3.1 核心链路（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + lib.rs 模块树 | — | 已完成 | `lib.rs` 声明 `pub mod error/studio/telemetry` 并 re-export；`studio/`（client/config/message_hook/mod/pojo/socket_io/user_agent/websocket）+ `telemetry/`（agent_scope_attributes/aggregator/exporter/extractors/gen_ai_attributes/mod/model/tracing），加 `lib.rs` + `error.rs` **共 18 个文件**（本行为 2026-10-22 复核时更新，原「15 个文件」为 2026-09-18 快照） |
| `StudioConfig` + builder（默认值/校验/tracingUrl 推导）| StudioConfig | 已完成 | 默认值/tracingUrl 推导/尾部 `/` trim 已对齐 Java；单测 `studio::config::tests`（3） |
| studio POJO ×4（serde 字段名对齐 Java）| pojo/*.java | 已完成 | camelCase / `run_dir` / `structuredInput` / `NON_NULL` 跳过；单测 `studio::pojo::tests`（5） |
| `StudioClient`（registerRun/pushMessage/requestUserInput/shutdown + 重试）| StudioClient | 已完成 | reqwest；指数退避仅重试 `StudioError::Transport`；网络路径未离线测（§3.2）。**静态复核**：Java 的非 2xx 被包成 `IOException` 因而**也会**被重试，Rust 不重试 HTTP 状态错误（见 history/STUDIO__MIGRATION_HISTORY.md P2-1）|
| `StudioWebSocketClient` + `StudioTransport` trait + `PendingInputRegistry` | StudioWebSocketClient | 已完成 | 可插拔 trait + oneshot 注册表；`NoopStudioTransport` 供离线测；单测 `studio::websocket::tests`（**11** = 6 `#[test]` + 5 `#[tokio::test]`，原记 9 有误） |
| `UserInputData` + `parse_content_blocks` + `preprocess_tool_result_block` | StudioWebSocketClient | 已完成 | 纯函数；含 tool_result 字符串 output→text 块规范化；同上单测覆盖 |
| `StudioMessageHook`（Middleware on_agent 后推送）| StudioMessageHook | 已完成 | 实现 core `Middleware::on_agent`，捕获终态消息 fire-and-forget 推送，失败仅日志不中断 agent。**已完成（2026-10-29 勘误）**：原记「静态复核：捕获键 `"message"` 与 core 实际写入的 `"reply"` 不一致 → 恒不推送（见 history/STUDIO__MIGRATION_HISTORY.md P0-1）」，实际已于 **2026-10-22** 修复（见 history/STUDIO__MIGRATION_HISTORY.md）——`final_message` 优先读 `fields["reply"]`、`fields["message"]` 仅作第三方回退；依据 `crates/agentscope-extensions-studio/src/studio/message_hook.rs:88-97` 与用例 `reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`。另 `tokio::spawn` 需在 runtime 内调用 |
| `StudioManager` 全局单例（init builder/shutdown/getters）| StudioManager | 已完成 | `OnceLock<RwLock<Option<Installed>>>`；init/builder/getters/shutdown/initialize_with |
| `StudioUserAgent`（终端 + Studio 双模式）| StudioUserAgent | 已完成 | 独立输入提供者（core 无 `AgentBase`）；Studio 失败回退终端；`DEFAULT_INPUT_TIMEOUT=30min` |
| telemetry 属性常量 ×2 文件 | GenAi/AgentScopeIncubatingAttributes | 已完成 | 常量 `&'static str` 键 + operation/provider 取值常量 |
| telemetry model 部件 ×**13** | telemetry/model/*.java | 已完成 | serde `type` 判别；字段名对齐（`mime_type`/`file_id`/`finish_reason`）；Rust `telemetry::model` 13 个 `pub` 类型一一对应 |
| `StreamChatResponseAggregator` | 同名 | 已完成 | text/thinking/tool_calls 按 id 合并 + usage 取 max + finishReason；单测 `telemetry::aggregator::tests`（3） |
| `AttributesExtractors` + Formatter/Provider converter | 同名 | 已完成 | 纯函数返回 `(String, serde_json::Value)`；`GenerateOptions` 非 Serialize → 本地投影字段（不改 core）；单测 `telemetry::extractors::tests`（**8**，原记 7 有误） |

### 3.2 重要但可延后（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Socket.IO 真实握手/鉴权/重连 | StudioWebSocketClient.connect | 已完成（2026-10-28：鉴权键已对齐 Java `run_id`） | `studio::socket_io`：`socket_io_url`（http(s)→ws(s) + `?EIO=4&transport=websocket`）、`parse_engine_io_open`（`0{...}` 握手）、`namespace_connect_frame`（`40/python,{...}`）、`is_ping`/`pong_frame`、`parse_socket_io_event`（`42[...]`）、`reconnect_delay`（指数退避封顶）+ `SocketIoProtocol` 状态机 + `SocketIoTransport`（真实 WS）；单测 `studio::socket_io::tests`（**14** = 12 帧层 + 2 鉴权键）。**鉴权键**：`auth_payload(run_id, include_legacy_key)` 默认 `{"run_id":...}`（Java `StudioWebSocketClient.java:98`），历史 `runId` 只能经 `with_legacy_auth_key()` 叠加（见 §4.1.0v）|
| `StudioClient` 网络路径离线测（mock HTTP） | StudioClient | 已完成（2026-10-30，见 history/STUDIO__MIGRATION_HISTORY.md） | 新增 `studio/http.rs` 注入点：`trait StudioHttpTransport { async fn execute(&self, StudioHttpRequest) -> StudioResult<StudioHttpResponse> }` + `ReqwestStudioTransport`（生产，行为不变）+ `StudioClient::with_transport`；`StudioClient` 改持 `Arc<dyn StudioHttpTransport>`。11 个离线用例走真实公开方法（方法/路径/头/JSON body、2xx、非 2xx 不重试、传输错误重试/耗尽）|
| `StudioUserAgent` 与 core agent 循环集成 | StudioUserAgent | 已完成（2026-10-30，见 history/STUDIO__MIGRATION_HISTORY.md；诚实范围见交接信息） | 新增 `StudioUserAgent::drive_round(&A)` / `drive_round_with(&A, Msg)`，泛型约束为 core `CallableAgent`（`ReActAgent`/`HarnessAgent` 直接可用）：取输入 → 推送用户消息到 Studio（失败仅告警）→ 跑一轮 agent → 返回回复；agent 错误映射为 `StudioError::Agent`。**不做**「自动多轮 / 终端交互」循环，那是宿主职责（Java 靠 `AgentBase` 子类化，core 无此机制）|
| `StudioMessageHook` 终态消息捕获键修正 | StudioMessageHook | 已完成（2026-10-22；2026-10-29 勘误：原记「待办」） | core 终态消息在 `fields["reply"]`（protocol crate 为 `fields["result"]`），Rust 侧原先读 `fields["message"]` → 恒不触发（见 history/STUDIO__MIGRATION_HISTORY.md P0-1）。现 `final_message` 优先 `reply`、`message` 作第三方回退（`studio/message_hook.rs:88-97`），用例 `reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`（见 history/STUDIO__MIGRATION_HISTORY.md）|

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `TelemetryTracer`（core `Middleware` 包装 + span 生命周期） | TelemetryTracer | 已完成（2026-10-28：见 history/STUDIO__MIGRATION_HISTORY.md P1-1 ①②③ 全部关闭） | `telemetry/tracing.rs`：`Middleware` 三拦截点（`OnAgent`/`OnModelCall`/`OnActing`）产出 `invoke_agent {agent}` / `chat {model}` / `execute_tool {name}`，`call_format` 产出 `format {target}`；span 用 **RAII 收尾**（guard 随事件流 drop 而结束，取消路径不漏 span）；父子靠 `RuntimeContext.run_id` 映射（`&RuntimeContext` 不可变，不能用 ctx 存 span）。`TraceExporter::export` 是 async 而 hook 是同步 → `SpanSink` 后台批量导出 + `flush()` 供测试/短进程。**2026-10-28 补齐**：①四类 span 均带 `agentscope.function.name`；②三个响应侧抽取器接入（`SpanGuard::finalizer`）：agent ← `AgentResult.fields["reply"]`、model ← `AggregatingModel` tee 模型流 + `StreamChatResponseAggregator`、tool ← `ActingResultsHandle`；③`call_format`（等价 Java `callFormat`）。单测 13 个（2 `#[test]` + 11 `#[tokio::test]`），含真实 `ReActAgent` 端到端断言 usage/finish_reason（§4.1.0v）|
| OTLP HTTP 导出 | exporter-otlp-http | 已完成（降级 exporter）| `telemetry::exporter`：`Span`/`SpanKind`/`SpanStatus`/`OtlpEncoder`（`resourceSpans`→`scopeSpans`→`spans`，id 十六进制校验、纳秒字符串、`AnyValue` 映射）+ `TraceExporter` trait + `OtlpHttpExporter`（reqwest `POST {tracingUrl}`）/`RecordingTraceExporter`/`NoopTraceExporter`；**未引入 `opentelemetry` 依赖**（设计决定，见 §4.2）；单测 `telemetry::exporter::tests`（**13** = 11 + 2）。**2026-10-31 已补齐**：Java `TelemetryTracer.Builder.addHeader/headers` 对应 `with_header` / `with_headers` / `headers()`（同名覆盖语义同 Java `HashMap`），POST 时随请求发送；Debug 输出只显 header 个数（不泄露鉴权值）|
| 多模态内容部件抽取 | TODO(multi-modal) | **已完成（设计决定，不迁移）**（2026-10-31） | Java 侧同为 TODO 占位：`AttributesExtractors.java:400` / `:461` / `:503` / `:548` 四处 `// TODO: support multi modal content`，其中 `:548` 对 `VideoBlock` 也只回一个 `<VideoBlock is not supported>` 文本块。Rust `block_to_part`（`telemetry/extractors.rs:316`）对 Image/Audio/Video 返回 None，行为等价；跟随上游即可 |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`（`agentscope-extensions-studio` 主代码 28 文件 / 4305 行；`@Test` 91）；Rust 工作树 `crates/agentscope-extensions-studio` 19 个 `.rs`（本 crate 既有实测 92 用例）。
> 本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**；与历史结论冲突处以本表为准。
> 编号规则：`STUDIO-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `STUDIO-D<NN>` / `STUDIO-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 图例：`rg` 零命中均以「命令片段 → 命中情况」形式给出；行号为 2026-10-31 工作树快照。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| STUDIO-01 | P0 | **Manager 装配下输入回环断链**：`initialize_with_transports` 用 `StudioWebSocketClient::with_transport`（每次新建 `PendingInputRegistry`），而 `SocketIoTransport` 把 `forwardUserInput` resolve 进**构造时**传入的另一个 registry；`with_transport_and_registry`（唯一能共享 registry 的入口）全仓**零调用**。结果：经 `StudioManager::web_socket_client()` 构造的 `StudioWebSocketClient::wait_for_input` 永远等不到数据（走到 30min 超时），`StudioUserAgent` 的 Studio 模式因此不可用 | `StudioWebSocketClient#waitForInput` / `#handleUserInput`（单一 `pendingRequests` 表，socket 回调直接 `sink.tryEmitValue`） | `src/studio/mod.rs:120-128`（126 行 `StudioWebSocketClient::with_transport`）；`src/studio/websocket.rs:146`（`with_transport` → `Arc::new(PendingInputRegistry::new())`）；`rg 'with_transport_and_registry' src` → 仅 `websocket.rs:146/153` 两处（定义与自调用），**无生产调用点**；`src/studio/socket_io.rs:302/322`（registry 为私有字段，无访问器） | 已完成 | ① 给 `SocketIoTransport` 加 `registry()` 访问器，或新增 `StudioManager::initialize_with_transports_and_registry(config, http, transport, registry)`；② `mod.rs` 装配处与 transport 共用同一 `Arc<PendingInputRegistry>`；③ **验收**：新增一条走真实装配入口（`initialize_with_transports` → `connect` → 注入 `42["forwardUserInput",[rid,blocks,{}]]` → `wait_for_input(rid)` 返回 Ok）的用例，禁止直接 poke 内部字段 【2026-10-31 落地：采用 ①+②——`StudioTransport::input_registry()` 默认方法（`src/studio/websocket.rs:66`）+ `SocketIoTransport::registry()` 访问器并 override（`src/studio/socket_io.rs:371` / `:509`），`initialize_with_transports` 命中时改用 `with_transport_and_registry`（`src/studio/mod.rs:133-142`）；验收用例 `tests/manager_input_loop.rs::the_manager_assembly_delivers_user_input_to_wait_for_input`（本地 tokio-tungstenite 假 Studio：`0{...}` → `40/python` → `42["forwardUserInput",...]`，走 `initialize_with_transports` → `StudioManager::web_socket_client()` → `wait_for_input` 返回 Ok；修复前 5s 超时失败）】 |
| STUDIO-02 | P0 | **span 无周期/批量导出**：`SpanSink` 只把 finished span 推入本地 `Vec<Span>`，唯一触发 `exporter.export` 的是显式 `flush()` 与 channel 关闭（producer drop）。长驻进程里 `TelemetryTracing` 被 `MiddlewareChain` 长期持有 → 既不导出、`Vec` 又无上限增长（内存泄漏 + 遥测全丢） | `TelemetryTracer.Builder#build`：`BatchSpanProcessor.builder(exporter).build()`（SDK 默认 5s 调度 / 512 批） | `src/telemetry/tracing.rs:696-753`（`Batch` 仅 `Span`/`Flush` 两态；`Batch::Span` 只 `batch.push`，`export` 仅在 `Batch::Flush`（716）与循环结束（723）调用）；`rg 'interval\|schedule\|batch_size' src/telemetry/tracing.rs` → 零命中（仅 `batch` 变量名） | 已完成 | 后台任务加定时 flush（默认 5s，可由 `StudioConfig` 注入）与批量上限（512 满即导）；`flush()` 语义保留；**验收**：用例「不调 `flush()`，仅推进 tokio 时间 6s」后 `RecordingTraceExporter.len() > 0` 【2026-10-31 落地：`SpanSink` 后台任务改 `tokio::select!` + `interval_at` 周期导出（`src/telemetry/tracing.rs:760-806`，`BatchConfig` 默认 5s/512，`MissedTickBehavior::Delay`），满批即导；注入点 `StudioConfig::telemetry_batch_interval/telemetry_batch_size`（`src/studio/config.rs:12-18`）+ `TelemetryTracing::with_config`（`tracing.rs:175-179`），`new` 仍用默认；用例 `spans_are_exported_on_the_schedule_without_a_flush`（`start_paused` + `advance(6s)`）、`a_full_batch_is_exported_immediately`】 |
| STUDIO-03 | P1 | **`connect()` 不等连接确认**：`SocketIoTransport::connect` 起后台任务后立刻 `Ok(())`；`connected` 在 WS 打开即置 `true`（早于 `40` 命名空间 ACK）；因此 `StudioWebSocketClient::connect` 的 10s `timeout` 形同虚设，`StudioManager::initialize_with_transports` 对不可达 Studio 仍返回成功（只 warn），宿主无从判断「是否真的连上」 | `StudioWebSocketClient#connect`：`EVENT_CONNECT` → `sink.success()`、`EVENT_CONNECT_ERROR` → `sink.error(...)`，整体 `.timeout(Duration.ofSeconds(10))` | `src/studio/socket_io.rs:370-395`（370 `async fn connect`；393 `connect_async`；398 `connected.store(true)` 在 `40` ACK 之前）；`:235`（`SocketIoProtocol::is_connected`，仅协议态）；`src/studio/websocket.rs:176-180`（外层 10s timeout 包住的是「立即返回」的 future） | 已完成（2026-11-01 Round 4） | transport 增 `wait_ready(Duration)`：`ProtocolAction::Connected` 时 fulfill oneshot，握手失败/超时返 Err（`StudioError::WebSocket`）；`connect()` 等待它；`is_connected()` 改读 namespace 已加入；**验收**：指向不可达端口的用例必须 `Err`，而非 Ok 【2026-11-01 Round 4 落地：`SocketIoTransport` 新增 `ConnectionState`（Connecting/Ready/Failed/Closed，`tokio::sync::watch` 广播）+ `wait_ready(Duration)` + `with_connect_timeout`（默认 `DEFAULT_CONNECT_TIMEOUT=10s`）；`connect()` 等到命名空间 `40` ACK 才返回，失败/超时返回 `StudioError::WebSocket` 并 `close()` 复位；`is_connected()` 改读 `Ready`。用例：`connect_fails_when_studio_is_unreachable`、`connect_waits_for_the_namespace_ack`（本地假 Studio 延迟 150ms 才发 ACK，返回时 `acked==true` 且 `state()==Ready`）】 |
| STUDIO-04 | P1 | **模型 span 丢 4 项 usage**：聚合器只对 `input/output_tokens` 取 max，`cached/cache_creation/reasoning/tool_use_prompt` 恒 0；但 `get_llm_response_attributes` 无条件写这 4 个键 → `gen_ai.usage.cache_read.input_tokens`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.usage.reasoning.output_tokens`、`agentscope.usage.tool_use_prompt_tokens` **永远为 0**（Anthropic/DeepSeek 缓存与推理用量全部丢失） | `StreamChatResponseAggregator#append`（6 个 usage 字段各取 max）、`AttributesExtractors#getLLMResponseAttributes`（写 6 键） | `src/telemetry/aggregator.rs:104-113`（仅 max input/output）、`:132`（`cached_tokens: 0` + `..Default::default()`，其余 3 项同为 0）；`src/telemetry/extractors.rs:208-229`（照写 4 键） | 已完成 | 聚合器补 4 个 `max` 字段并在 `response()` 回填；**验收**：两 chunk 分别带 cache_read=5/cache_creation=7/reasoning=9/tool_use=3 与全 0 → 断言取 max 且 span 上 4 键非 0 【2026-10-31 落地：`StreamChatResponseAggregator` 新增 `cached_tokens/cache_creation_tokens/reasoning_tokens/tool_use_prompt_tokens` 四个 max 累加并在 `response()` 回填（`src/telemetry/aggregator.rs:78-86/113-119/144-146`）；用例 `cache_reasoning_and_tool_use_usage_are_max_aggregated`（聚合器）与 `the_model_span_aggregates_the_streamed_response` 新增 4 键断言（span 侧）】 |
| STUDIO-05 | P1 | **无心跳 watchdog，半开连接永不重连**：`parse_engine_io_open` 解出 `pingInterval/pingTimeout` 却无人消费；读侧 `select!` 没有超时分支，只有 `Close`/`Err` 才 break 并重连 → 网络静默断开（NAT 超时、对端无 FIN）后连接假活，`wait_for_input` 只能等业务超时 | socket.io-client 内置 pingTimeout（引擎层超时即断开并触发 `reconnection`） | `src/studio/socket_io.rs:97-128`（两字段仅解析入 `EngineIoHandshake`）、`:420-460`（`tokio::select!` 仅 `receiver.recv()` / `source.next()`）；`rg 'ping_timeout\|timeout_at\|sleep_until' src/studio/socket_io.rs` → 仅测试断言命中 | 已完成（2026-11-01 Round 4） | 用 `tokio::time::timeout(ping_interval + ping_timeout, source.next())` 包住读分支，超时按断开处理并走既有退避重连；**验收**：假 WS 服务只握手不回 ping → 断言 N 秒后触发重连 【2026-11-01 Round 4 落地：`SocketIoProtocol` 保存整份 `EngineIoHandshake` 并暴露 `handshake()`/`heartbeat_window()`；读循环用 `tokio::time::timeout_at` 包住 `source.next()`，窗口 = `pingInterval + pingTimeout`、每收到一帧刷新，超时按断开走既有退避重连（握手前不设超时）。用例 `a_silent_server_trips_the_heartbeat_watchdog_and_reconnects`（假 Studio 用 50ms/50ms 且从不 ping → 第二次 accept 出现）】 |
| STUDIO-06 | P1 | **导出失败即丢批、无重试**：`export()` 单次调用，失败只 `tracing::warn!`，该批 span 已被 `mem::take` 丢弃；`OtlpHttpExporter` 亦无重试/退避 | Java SDK `BatchSpanProcessor` + `OtlpHttpSpanExporter` 自带重试/背压 | `src/telemetry/tracing.rs:754-763`（失败仅 warn，batch 已清空）、`src/telemetry/exporter.rs:454-480`（一次 POST，非 2xx 直接 `Err`） | 已完成（2026-11-01 Round 2） | 【2026-11-01 落地】`export()` 改「失败保留该批 + 指数退避重试」：`EXPORT_MAX_ATTEMPTS=3`、`EXPORT_RETRY_BASE_DELAY=100ms`（逐次翻倍，封顶 `EXPORT_RETRY_MAX_DELAY=5s`），见 `src/telemetry/tracing.rs::export`——span 不再在首个失败后被 `mem::take` 丢弃，耗尽 3 次才放弃并 warn（带丢弃条数）。用例：`a_failed_export_is_retried_until_the_spans_arrive`（前 2 次失败、第 3 次成功 → attempts==3 且 span 到达）、`a_permanently_failing_export_gives_up_without_blocking_the_sink`（永不成功时 `flush()` 仍返回、attempts==3、0 条落盘）。原「下一动作」保留作背景：失败时把该批放回待导队列（设上限，防内存涨）并按 100ms→5s 指数退避重试（与 `StudioClient` 同族语义）；**验收**：注入「前 2 次失败、第 3 次成功」的 exporter → 断言 span 最终到达 |
| STUDIO-07 | P2 | **取消路径 span 状态不当**：`SpanStream::drop` → `finish(SpanStatus::Ok)`，把被取消的运行标成成功；同文件 `SpanGuard::drop` 却是 `Unset`，两条路径不一致 | `TelemetryTracer#callAgent/callModel/callTool` 的 `doFinally(span::end)`（取消时不设 status） | `src/telemetry/tracing.rs:590-596`（Drop → `Ok`）、`:495-500`（`SpanGuard::drop` → `Unset`） | 已完成（2026-11-01 Round 4） | Drop 路径改 `Unset`（或引入 `Cancelled` 再编码为 `Unset`）；`a_dropped_run_still_ends_the_span` 增断言 `status == Unset` 【2026-11-01 Round 4 落地：`SpanStream::drop` 由 `Ok` 改为 `Unset`，与 `SpanGuard::drop` 及 Java `doFinally(span::end)` 一致；`a_dropped_run_still_ends_the_span` 增 `status == Unset` 断言】 |
| STUDIO-08 | P2 | **registerRun 时间戳时区偏离**：Rust 用 UTC 墙钟按 `%Y-%m-%d %H:%M:%S%.3f` 格式化（无时区后缀），Java 用 `ZoneId.systemDefault()`；Studio 按本地时间解析时，非 UTC 主机上会整体偏移时区差 | `StudioClient.TIMESTAMP_FORMATTER`（`withZone(ZoneId.systemDefault())`） | `src/studio/client.rs:16-18`（`DateTime<Utc>` + 该 pattern）、`:77`（`Utc::now()`） | 已完成（2026-11-01 Round 4） | 改 `chrono::Local::now()` 并保持同一 pattern；**验收**：用固定本地时刻构造断言格式化结果（避免依赖机器时区） 【2026-11-01 Round 4 落地：`StudioClient::format_timestamp` 改收 `DateTime<Local>`、`register_run` 用 `Local::now()`（pattern 不变，对齐 Java `ZoneId.systemDefault()`）；用例 `timestamps_use_the_local_zone`（固定本地时刻 2026-06-15 12:34:56.700 → 同字面量输出）】 |
| STUDIO-09 | P2 | **builder 主入口拿不到真实传输**：`StudioManager::init()...initialize()` 走 `initialize_with` → `NoopStudioTransport`，且 `StudioManagerBuilder` 无 `transport/http_transport/registry` 旋钮；生产只能绕到 `initialize_with_transports`（本身又受 STUDIO-01 影响），文档与实现易误导 | `StudioManager.Builder#initialize`：直接 `new StudioWebSocketClient(config)`（真连 socket.io） | `src/studio/mod.rs:209-212`（`initialize()` → `initialize_with`）、`:97-99`（`initialize_with` → `Arc::new(NoopStudioTransport)`）、`:149-205`（builder 仅有 config 类 setter） | 已完成（2026-11-01 Round 4） | builder 增 `http_transport(...)`/`transport(...)`/`input_registry(...)`，或至少在 §1/§2 与 rustdoc 明确「生产必须用 `initialize_with_transports`」；与 STUDIO-01 一并做 【2026-11-01 Round 4 落地：采用后一种——`StudioManagerBuilder` 增 `http_transport(Arc<dyn StudioHttpTransport>)`/`transport(Arc<dyn StudioTransport>)`（改手写 `Debug`），`initialize()` 装配注入传输，**默认仍 `NoopStudioTransport`**（离线不连 localhost:3000）+ rustdoc 声明生产须传真实 transport；用例 `the_builder_installs_the_injected_transports`（注入 ReadyTransport/ScriptedTransport → registerRun 走注入 HTTP、`web_socket_client().is_connected()` 为真）】 |
| STUDIO-10 | P2 | **requestUserInput 线上字段偏离**：Rust `structuredInput` 缺省时 **省略键**（`skip_serializing_if`），Java builder 默认 `Map.of()` → 发 `{}`；另 `agentId` Rust 传 agent **名**，Java 传 `getAgentId()` | `RequestUserInputRequest.Builder`（`structuredInput = Map.of()`）；`StudioUserAgent#getInputFromStudio`（`requestUserInput(getAgentId(), getName(), null)`） | `src/studio/pojo.rs:173-176`（`Option<Value>` + `skip_serializing_if`）、`:193/213-225`；`src/studio/user_agent.rs:144`（`request_user_input(&self.name, &self.name, None)`） | 已完成（2026-11-01 Round 4） | `RequestUserInputRequestBuilder` 默认 `Some(json!({}))`；`StudioUserAgentBuilder` 增 `agent_id`（缺省回退 name）；**验收**：序列化断言 `structuredInput == {}` 【2026-11-01 Round 4 落地：`RequestUserInputRequest.structured_input` 去掉 `skip_serializing_if`、builder 默认 `{}`（Java `Map.of()`）、缺键反序列化回落 `{}`；`StudioUserAgent` 增 `agent_id` 字段 + `agent_id()` getter + builder `agent_id(...)`（缺省回退 name），`input_from_studio` 改发 `request_user_input(&self.agent_id, &self.name, None)`。用例：`request_user_input_defaults_structured_input_to_an_empty_object`、`request_user_input_posts_java_shaped_request`（断言 `{}`）、`agent_id_defaults_to_the_name_and_can_be_overridden`】 |
| STUDIO-11 | P2 | **终端输入无提示符**：`input_from_terminal` 直接读 stdin，交互式 CLI 下用户看不到任何提示（Java 先 `System.out.print(getName() + " > ")`）| `StudioUserAgent#getInputFromTerminal` | `src/studio/user_agent.rs:116-131`（无 print/flush） | 已完成（2026-11-01 Round 4） | 读前 `print!` + `stdout().flush()`（或提供开关，避免污染管道输出）；不需单测覆盖 stdin 【2026-11-01 Round 4 落地：新增 `TerminalIo` trait（`print` + `read_line`）与 `StdTerminalIo`（stdout 写+flush / stdin 读），`StudioUserAgent` 持 `Arc<dyn TerminalIo>`，`input_from_terminal` 先打印 `Name > `（Java `System.out.print`）再读行，builder `terminal(...)` 可注入；用例 `terminal_input_prints_the_prompt_then_reads_a_line`（ScriptedTerminal → 提示符 `User > `、文本来自注入行，不触真实终端）】 |
| STUDIO-12 | P2 | **`format` span 无框架级拦截点**：只能宿主显式调 `TelemetryTracing::call_format`；Java 由 `Tracer.callFormat` 在框架内自动包裹 | `TelemetryTracer#callFormat` | `crates/agentscope-core/src/middleware/middleware.rs` 的 `ExtensionPoint` 无 formatter 变体（`rg 'OnFormat' crates/agentscope-core` → 零命中）；`TelemetryTracing::call_format`（`src/telemetry/tracing.rs:254-289`）**无生产调用点**（`rg 'call_format' src` 仅定义 + 测试） | 已完成（2026-11-01 Round 2） | 【2026-11-01 落地】core 的 `ExtensionPoint::OnFormat` + `Middleware::on_format` 已落地，本 crate 据此新增 `TelemetryTracing::on_format`（`active_points()` 增 `OnFormat`）：每次带 `ResponseFormat` 的模型请求前产出 `format <target>` span（`gen_ai.operation.name=format`、`agentscope.format.output=<target>`、`agentscope.function.name=<Fmt>.format`、`SpanKind::Internal`、父 span＝当前 run）。因钩子只给请求、不给 formatter 实例/结果，span 只记请求侧并以 `Unset` 状态即时收尾（等价 Java `finally { span.end() }`）；`format=None` 不出 span（同 Java 只在 `callFormat` 被调时追踪）。formatter 类型按 provider 族推导（`formatter_type_name` → `FormatterConverter`），故 target 与 `call_format` 一致；`call_format` 保留给宿主自格式化场景。用例：`the_traced_points_cover_the_four_java_calls`、`the_framework_format_hook_emits_a_format_span`、`an_unconstrained_call_emits_no_format_span`、`a_structured_call_runs_the_format_span_through_the_core_seam`（真实 `ReActAgent::call_with_schema` 装配入口，禁 poke 内部字段） |
| STUDIO-13 | P2 | **provider/模型类型标识只能靠宿主配置**：`TelemetryTracing` 只拿到 `Arc<dyn Model>`，core `Model` trait 无 provider 标识 → `gen_ai.provider.name` 恒 `unknown`、`agentscope.function.name` 的模型类型退化为 `ChatModel`（Java 从 `ChatModelBase` 实例反射类名）| `AttributesExtractors.ProviderNameConverter#getProviderName`；`#getFunctionName`（`instance.getClass().getSimpleName()`） | `rg 'provider_name' crates/agentscope-core/src` → 零命中；`src/telemetry/tracing.rs:59-78`（`DEFAULT_PROVIDER_NAME="unknown"` + `model_type_name` 后缀映射）、`:174-181`（`with_provider_name`，`Arc::get_mut` 为 None 时静默不生效） | 已完成（2026-11-01 Round 2） | 【2026-11-01 落地】`on_model_call` 改为先问 core `Model::provider_name()`（`resolved_provider_name`：模型报非空且非 `unknown` 即采用），配置值退为 fallback → `gen_ai.provider.name` 与 `<ChatModelType>.callModel` 不再恒 `unknown`/`ChatModel`；`with_provider_name` / `with_agent_type_name` 在 tracer 已共享（`Arc::get_mut` 为 None）时改为 `tracing::warn!`（带被忽略的值），不再静默失效。用例：`the_model_reported_provider_name_wins_over_the_configured_fallback`、`a_model_without_a_provider_name_uses_the_configured_fallback`、`a_structured_call_runs_the_format_span_through_the_core_seam`（真实链路断言 provider=anthropic + `AnthropicChatModel.callModel`） |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| STUDIO-14 | `StudioConfig` 默认值 / `tracingUrl` 推导 / 尾 `/` trim / 重连参数 | `StudioConfig.Builder`（`studioUrl`、`UnnamedProject`、`run_<millis>`、UUID、maxRetries=3、reconnect 3/1s/5s） | `src/studio/config.rs:96-196`；`studio::config::tests`（3，含 `defaults_match_java`） | 已完成 |
| STUDIO-15 | Engine.IO v4 / Socket.IO 帧层与重连退避 | socket.io-client（`IO.Options` reconnection/attempts/delay） | `src/studio/socket_io.rs:74-320`（`socket_io_url` 改写 scheme + `?EIO=4&transport=websocket`、`0{}` 握手、`40/python,{...}`、`2`→`3`、`42[...]`、`reconnect_delay` 指数封顶）；`studio::socket_io::tests`（14） | 已完成 |
| STUDIO-16 | 握手鉴权键 `run_id`（snake_case） | `StudioWebSocketClient.connect`（`auth.put("run_id", ...)`） | `src/studio/socket_io.rs:39-70`（`AUTH_RUN_ID_KEY`/旧键 opt-in）、`:322`（`new` 默认 Java 键）；用例 `handshake_auth_uses_the_java_snake_case_run_id_key`、`the_legacy_camel_case_auth_key_is_opt_in` | 已完成 |
| STUDIO-17 | REST 三端点 + 请求体字段名（`run_dir`/camelCase/`NON_NULL`）+ 指数退避重试 | `StudioClient#registerRun/pushMessage/requestUserInput`、`pojo/*` | `src/studio/client.rs:72-130`、`src/studio/pojo.rs:10-30/95-115`、`src/studio/http.rs:36-120`（`Content-Type: application/json; charset=utf-8`、10s/30s 超时）；`studio::client::tests`（10）、`studio::pojo::tests`（5） | 已完成 |
| STUDIO-18 | 终态消息转发读 `fields["reply"]`（含 `message` 回退） | `StudioMessageHook#onEvent`（`PostCallEvent#getFinalMessage`） | `src/studio/message_hook.rs:88-97`；用例 `reads_reply_key_written_by_core`、`falls_back_to_message_key`、`non_result_yield_none` | 已完成 |
| STUDIO-19 | OTLP/JSON 编码：`resourceSpans→scopeSpans→spans`、`AnyValue` 的 string/int(字符串)/double/bool/arrayValue/kvlistValue、id 十六进制校验、纳秒以字符串发、非法 span 报错 | （Java 由 OTel SDK 承担） | `src/telemetry/exporter.rs:181-290`（`encode_attribute_value`/`encode_attributes`/`OtlpEncoder::encode`）、`:161-193`（`is_hex_id`/`unix_nanos`）；`telemetry::exporter::tests`（13） | 已完成 |
| STUDIO-20 | OTLP 自定义 header（鉴权，如 Langfuse） | `TelemetryTracer.Builder#addHeader/#headers` | `src/telemetry/exporter.rs:420-446`（`with_header` 同名覆盖、`with_headers` 整体替换、`headers()`）、`:461-472`（随请求发送）；Debug 仅输出 `header_count` | 已完成 |
| STUDIO-21 | 四类 span（`invoke_agent`/`chat`/`execute_tool`/`format`）+ `agentscope.function.name` + 响应侧属性 + RAII 收尾 | `TelemetryTracer#callAgent/callModel/callTool/callFormat` | `src/telemetry/tracing.rs:291-450`（三拦截点 + 请求侧属性）、`:551-596`（`SpanStream`/`Drop`）、`:254-289`（`call_format`）；`telemetry::tracing::tests`（13，含真实 `ReActAgent` 端到端） | 已完成 |
| STUDIO-22 | 属性键集 44 键（38 `gen_ai.*` + 6 `agentscope.*`）与 Java 字面量双向交叉校验 | `GenAiIncubatingAttributes`（44 个 `AttributeKey`）、`AgentScopeIncubatingAttributes` | `src/telemetry/gen_ai_attributes.rs:118-179`（`the_key_set_matches_the_java_class_exactly`，Rust 常量 vs 独立字面量双向比对）、`src/telemetry/agent_scope_attributes.rs:32-52` | 已完成 |
| STUDIO-23 | 流式 text/thinking/tool-call 合并（按 id 合并 + 内容拼接）+ 请求侧参数抽取（temperature/top_p/top_k/max_tokens/presence/frequency/seed） | `StreamChatResponseAggregator#append`、`AttributesExtractors#getLLMRequestAttributes` | `src/telemetry/aggregator.rs:19-116`、`src/telemetry/extractors.rs:92-190`；`telemetry::aggregator::tests`（3）、`telemetry::extractors::tests`（9） | 已完成（usage 4 项缺失另见 STUDIO-04） |

> 说明：`STUDIO-23` 的「usage 取 max」只覆盖 input/output；cache/reasoning/tool_use 四项未聚合，已单列为 `STUDIO-04`，两者不冲突。

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成**（`cargo build` / `cargo test` 全绿；本 crate 0 warning）。
  > **2026-10-29 勘误**：原记 §3.1「`StudioMessageHook` 恒不推送（见 history/STUDIO__MIGRATION_HISTORY.md P0-1）」与 §3.2「鉴权键 `runId`/`function.name`/响应侧抽取器/`callFormat` 待办」均已关闭——P0-1 于 2026-10-22 修复（见 history/STUDIO__MIGRATION_HISTORY.md），P1-1/P1-2 于 2026-10-28 修复（§4.1.0v，实测 **70 passed**）。
  > **2026-10-22 静态复核声明**：本轮**未执行** `cargo build` / `cargo test`（按子智能体约束仅静态核对），下列 build/test 结论沿用上一会话记录，**未经本轮复验**。
- 验证命令与结果（2026-09-18，**历史快照**）：
  - `cargo build -p agentscope-extensions-studio` → 成功，0 error / 0 warning（仅 core/harness 既有告警）。
  - `cargo test -p agentscope-extensions-studio` → **30 passed / 0 failed**（`--lib`）；doc-tests 0 passed。
  - 后续增量：2026-10-08 → **52 passed**（§4.1.0s）；2026-10-21 → **59 passed**（§4.1.0u）；2026-10-22 → **62 passed**（见 history/STUDIO__MIGRATION_HISTORY.md）；**2026-10-28 → 70 passed / 0 failed（本次实测，§4.1.0v）**。2026-10-22 静态复核按 `#[test]`/`#[tokio::test]` 注解逐文件计数得 **59**（46 + 13），与当次记录一致。
- **2026-11-01 Round 4（本次实测）**：`cargo build -p agentscope-extensions-studio -j 2` → **EXIT=0**（本 crate 0 error / 0 warning）；`cargo test -p agentscope-extensions-studio -j 2 --no-fail-fast` → **109 内联 + 1 集成 = 110 passed / 0 failed / 0 ignored**（Round 2 基线 103，+7：socket_io 3 / user_agent 2 / client 1 / mod 1）。反向验证：把 `connect()` 尾部的 `wait_ready(...)` 调用删掉改为 `Ok(())` → `studio::socket_io::tests::connect_fails_when_studio_is_unreachable` 与 `connect_waits_for_the_namespace_ack` **2 FAILED**（`an unreachable Studio must not report success` / `connect() must not return before the namespace ACK`），恢复后全绿。
- **2026-11-01 Round 2（本次实测）**：`cargo build -p agentscope-extensions-studio -j 2` → **EXIT=0**（本 crate 0 error / 0 warning）；`cargo test -p agentscope-extensions-studio -j 2 --no-fail-fast` → **102 内联 + 1 集成 = 103 passed / 0 failed / 0 ignored**（HEAD 基线实测 95+1=96，+7）。反向验证：同时回退 `export` 重试 / `on_format` 产出 / provider 解析 → **5 FAILED**，恢复后全绿（详见 §3.5.1 STUDIO-06/12/13 行）。
- 交付：`src/` **18 个文件**已接线并编译（**2026-10-22 更正**，原记 15 为 2026-09-18 快照）；`lib.rs` re-export `StudioClient/StudioConfig/StudioConfigBuilder/StudioWebSocketClient/StudioManager/StudioMessageHook/StudioUserAgent/StudioTransport/NoopStudioTransport/PendingInputRegistry/UserInputData` 及 telemetry 提取器/聚合器/模型。
- 上一子智能体遗留 8 个编译错误的根因与修法：
  1. **E0252 ×4**：`studio/mod.rs` 里 `use crate::studio::{client,config,websocket}::…` 与同文件 `pub use` 重复导入同名符号 → 删除冗余 `use`（保留 `pub use` 作为唯一公开路径）。
  2. **E0277 ×2**：`StudioUserAgentBuilder` `#[derive(Debug)]`，但字段含未实现 `Debug` 的 `StudioClient`/`StudioWebSocketClient` → 为二者手写 `impl std::fmt::Debug`（`finish_non_exhaustive`，不硬套 derive）。
  3. **E0271**：`aggregator.rs` 把 `Vec<ToolUseBlock>` 直接 `extend` 进 `Vec<ContentBlock>` → 改为 `tool_use_blocks.into_iter().map(ContentBlock::ToolUse)`。
  4. **E0277**：`extractors.rs` `json!` 内联 `GenerateOptions`（core 未实现 `Serialize`）→ 本地投影为 JSON 对象（不改 core，见 §4.2）。
  5. 清理本 crate 无用 `json_attr` 告警（加 `#[allow(dead_code)]` 并保留，供后续抽取器复用，不做删除）。
- 新增离线单测 30 个（均为 `#[cfg(test)]` 内联，无网络/无外部服务）：POJO serde 往返与字段名、Config 默认值与 tracingUrl 推导、WS `forwardUserInput` 事件映射与 `PendingInputRegistry` 解析/超时、telemetry 属性抽取器、流式响应聚合。
- 已知问题：`AttributesExtractors::get_llm_response_attributes(None)` 会写入 `agentscope.function.output="null"`（Jackson `writeValueAsString(null)` 行为），已按 Java 实际行为对齐并在单测中断言。

#### 4.1.0s 本次交付：Socket.IO 真实传输 + OTLP 降级导出（2026-10-08）
| 文件 | 类型 | 内容 |
|---|---|---|
| `src/studio/socket_io.rs` | 新 | 纯帧层：`socket_io_url`（http(s)→ws(s) + `?EIO=4&transport=websocket`）、`parse_engine_io_open`（`0{sid,pingInterval,pingTimeout}`）、`namespace_connect_frame`（`40/python,{...}`）、`is_ping`/`pong_frame`、`parse_socket_io_event`（`42[...]` / `42/ns,[...]`）、`reconnect_delay`（指数退避封顶）+ `SocketIoProtocol`（状态机，`on_text -> Vec<ProtocolAction>`）+ `SocketIoTransport`（tokio-tungstenite 真实连接：握手→入 namespace→回 pong→真重连；`forwardUserInput` 解析后 resolve 到共享 `PendingInputRegistry`）|
| `src/telemetry/exporter.rs` | 新 | `Span`/`SpanKind`/`SpanStatus` + `is_hex_id`/`unix_nanos`/`encode_attribute_value`/`encode_attributes` + `OtlpEncoder`（OTLP/HTTP JSON `ExportTraceServiceRequest`）+ `TraceExporter` trait + `OtlpHttpExporter`（`from_config` 取 `tracing_url`）/`RecordingTraceExporter`/`NoopTraceExporter` + `resource_attributes` |
| `src/studio/websocket.rs` | 改 | 新增 `StudioWebSocketClient::with_transport_and_registry`（让 transport 持有共享 registry）；`with_transport` 委派 |
| `src/studio/mod.rs` | 改 | 声明/导出 `socket_io`；新增 `StudioManager::initialize_with_transport`（`initialize_with` 委派，默认仍为 `NoopStudioTransport`）|
| `src/telemetry/mod.rs` | 改 | 声明/导出 `exporter`；更新模块文档（OTLP 不再是 deferred）|
| `Cargo.toml` | 改 | 新增 `tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots，与 CHANNEL crate 同版本/同 features）|

新测试 22 个（52 − 30），全离线：Socket.IO 帧层 11 个（URL 改写/握手与缺 sid/连接帧/事件解析/ping 检测/退避封顶/协议三态）+ transport 构造 1 个 = **12**；OTLP 导出 **10** 个（id 校验/时间范围/纳秒/AnyValue 映射/完整 payload/非法 span 报错/顺序与键/recording/noop/`from_config` 端点）。**2026-10-22 更正**：原文「帧层 11 + 导出 11」的分项有误（导出实为 10），但合计 22 正确。**真实 socket 路径无法离线测**（需运行中的 Studio 服务），已将其逻辑拆到纯函数/状态机以覆盖。

验证命令与结果（本次实测，2026-10-08）：
- `cargo test -p agentscope-extensions-studio` → **52 passed / 0 failed / 0 ignored**（基线 30，**+22**）
- `cargo build --workspace` → **BUILD_EXIT=0**；本 crate 0 warning

#### 4.1.0t 修正：`encode_attribute_value` 的 `AnyValue` 注释与数组/对象保留（2026-10-08）
**问题（交叉审核发现，已复核原文）**：`src/telemetry/exporter.rs` 原注释断言「OTLP `AnyValue` has no map/array variant」——**事实错误**：OTLP `AnyValue` 确含 `arrayValue`/`kvlistValue`。当前实现把 JSON 数组/对象一律降级为 `stringValue`（丢类型），属**记录在案的降级**，但注释断言不成立，违反本仓「未验证断言不得落笔」教训。
**修复**：`encode_attribute_value` 现正确编码 `Value::Array` → `{"arrayValue":{"values":[...]}}`、`Value::Object` → `{"kvlistValue":{"values":[{"key":..,"value":..}]}}`（逐元素递归）；仅 `null` 仍降级为空串 `{"stringValue":""}`（**OTLP 无 null 类型**，这一处才是不变量）。原错误注释已删除。
**测试同步**：`attribute_values_map_to_the_otlp_any_value_shape` 原断言对象降级为 `stringValue` 字符串（**该断言本身编码了错误行为**）→ 改为断言 `kvlistValue`，并新增数组 → `arrayValue` 断言。测试数不变（仍 52，为同测试内断言加强）。

#### 4.1.0u 新增：`TelemetryTracing` span 生产者（2026-10-21，关闭汇总 tracker GAP-18）
**裁定**：GAP-18「tracer 归 core 还是 studio」→ **归 studio**（core 不加 `opentelemetry` 依赖、不改 feature 矩阵）。理由见汇总 tracker §2。

| 文件 | 类型 | 内容 |
|---|---|---|
| `src/telemetry/tracing.rs` | 新 | `TelemetryTracing`（`impl Middleware`，`active_points` 只开三个点）+ `SpanGuard`（RAII end）+ `SpanStream`（持有 guard 的事件流，drop 即结束 span）+ `SpanSink`（`mpsc` 队列 + 后台批量 `export` + `flush()`）+ `new_trace_id`(32 hex)/`new_span_id`(16 hex) |
| `src/telemetry/mod.rs` | 改 | 声明/导出 `tracing`、`TelemetryTracing`、`DEFAULT_PROVIDER_NAME`；模块文档补生产者 |

**设计要点**
- **RAII**：span guard 放在 `on_*` 返回的事件流里，`SpanStream::drop`/`SpanGuard::drop` 都会收尾 —— 这正是 GAP-19「取消 = drop future」下 span 不漏 end 的唯一保证；不需要 `on_cancel` 钩子。
- **父子关联**：`on_*` 只拿到 `&RuntimeContext`（不可变），无法往 ctx 写当前 span，故用 `Arc<Mutex<HashMap<run_key, SpanCtx>>>`，`run_key` = `run_id` → `session_id` → `agent_id` 降级；run 结束（流结束或 drop）即移除。
- **同步 hook / 异步 exporter**：`SpanSink` 用无界 channel + 后台任务，等价 Java `BatchSpanProcessor`（2026-10-31 起包含定时导出 5s + 满批 512 即导，见 STUDIO-02）；无 tokio runtime 时丢弃 span 并 warn（不 panic、不阻塞）。
- **首版不含响应侧属性**（`gen_ai.usage.*`）：需从事件流聚合，留作后续；当前只记请求侧 + 状态 + 时间。
  > **2026-10-22 静态复核补充**：响应侧属性未落地不只是「缺 usage」——`get_agent_response_attributes`/`get_llm_response_attributes`/`get_tool_response_attributes` 三个抽取器**完全未被 `tracing.rs` 引用**，`StreamChatResponseAggregator` 也**未被接入**；同时 span 上缺 `agentscope.function.name`，且缺 Java `callFormat` 对应的 format span。三项合并计入 见 history/STUDIO__MIGRATION_HISTORY.md P1-1。
  > **2026-10-28 已全部补齐**（§4.1.0v）；上述两行保留为历史记录。

**测试**：`a_completed_run_exports_the_agent_span`、`a_failed_run_marks_the_span_as_error`、`a_dropped_run_still_ends_the_span`（**反向验证：去掉 Drop 路径后仅此用例 FAILED**）、`nested_phases_attach_to_the_run_span`、`a_run_without_a_run_id_falls_back_to_the_session`、`only_the_three_traced_points_are_active`、`a_real_agent_run_exports_agent_and_model_spans`（真实 `ReActAgent` + Mock 模型）。

**验证**：`cargo test -p agentscope-extensions-studio` → **59 passed / 0 failed**（基线 52，+7）。

#### 4.1.0v 2026-10-28 执行记录：关闭 P1×2（Socket.IO 鉴权键 + `TelemetryTracing` 缺失项）

> Java 参考：`agentscope-java` HEAD `16cb024ae`（`StudioWebSocketClient.java`、`TelemetryTracer.java`、`AttributesExtractors.java`）。本轮**只改本 crate**（`crates/agentscope-extensions-studio/**`）与本 tracker。

**① Socket.IO 鉴权键对齐 Java（§3.2 / 见 history/STUDIO__MIGRATION_HISTORY.md P1-2）**

| Java 依据 | 内容 |
|---|---|
| `studio/StudioWebSocketClient.java:96-99` | `Map<String,String> auth = new HashMap<>(); auth.put("run_id", config.getRunId()); options.auth = auth;` |
| 全模块 grep `run_id\|auth\|query` | 握手鉴权**只有一个键** `run_id`（无 `project`/`token` 等其它键；`runId` 只出现在 POJO 字段，如 `PushMessageRequest.runId`，与 WS 鉴权无关） |

- Rust 原实现：`serde_json::json!({ "runId": config.run_id() })` → 与 Java/Studio 服务端校验键不一致，命名空间入房可能被判未鉴权。
- 现实现：`AUTH_RUN_ID_KEY = "run_id"`、`LEGACY_AUTH_RUN_ID_KEY = "runId"`、`auth_payload(run_id, include_legacy_key)`；`SocketIoTransport::new` 默认发 `{"run_id": ...}`；**兼容回退** `SocketIoTransport::with_legacy_auth_key()` 把旧 `runId` **叠加**在 `run_id` 之上（默认不叠加，保证与 Java 字节级一致）。
- 测试：`handshake_auth_uses_the_java_snake_case_run_id_key`（默认载荷 + 实际 `40/python,{...}` 连接帧）、`the_legacy_camel_case_auth_key_is_opt_in`（旧键仅显式开启；Java 键恒在）；`connect_frame_carries_namespace_and_auth` / `protocol_joins_the_namespace_after_the_handshake` 中的样例载荷同步改为 `run_id`。

**② `TelemetryTracing` 三处缺失（§3.3 / 见 history/STUDIO__MIGRATION_HISTORY.md P1-1）**

| # | Java 依据 | 补法 |
|---|---|---|
| ① `agentscope.function.name` | `TelemetryTracer.java:104/124/169/215`（四类 span 均 `setAttribute(AGENTSCOPE_FUNCTION_NAME, getFunctionName(instance, method))`）；`AttributesExtractors.java:350` `class.getSimpleName() + "." + method` | `TelemetryTracing` 在四类 span 上写 `agentscope.function.name`：agent=`ReActAgent.callAgent`（`with_agent_type_name` 可覆盖）、model=`<ProviderChatModel>.callModel`（provider→Java 类名映射，未知=`ChatModel`）、tool=`Toolkit.callTool`（`TOOL_FUNCTION_NAME`）、format=`<Formatter>.format` |
| ② 三个响应侧抽取器 + `StreamChatResponseAggregator` | `TelemetryTracer.java:110`（agent `doOnSuccess` → `getAgentResponseAttributes`）、`:143-148`（model `doOnNext(aggregator::append)` + `doFinally` → `getLLMResponseAttributes`）、`:190`（tool `doOnSuccess` → `getToolResponseAttributes`） | `SpanGuard` 新增 `finalizer: Option<Box<dyn FnOnce() -> Attributes + Send>>`（等价 Java `doFinally`/`doOnSuccess`，drop 路径同样执行）。**agent**：`SpanStream` 从 `AgentEvent::AgentResult.fields["reply"]` 反序列化 `Msg`（core 写入点 `react_agent.rs:864`）→ `get_agent_response_attributes`。**model**：`on_model_call` 用 `AggregatingModel` 包装 `input.model`，逐 chunk `StreamChatResponseAggregator::append` → `get_llm_response_attributes`（`on_model_call` 的事件流只有 `ModelCallEnd`，文本 delta 走 `ctx.delta_tx`，故必须 tee provider 流；每次 `stream()` 调用重置聚合器，重试不叠加）。**tool**：`ActingInput.results`（`ActingResultsHandle`）快照按 `tool_call_id` 匹配 → `get_tool_response_attributes`（事件流只有 `ToolResultEnd` 的 state/metadata，无输出块） |
| ③ `callFormat` 的 format span | `TelemetryTracer.java:203-232`：span 名 `<FORMAT> <formatterTarget>`、请求侧 `getFormatRequestAttributes`、成功侧 `getFormatResponseAttributes`、`finally span.end()` | 新增 `TelemetryTracing::call_format(ctx: Option<&RuntimeContext>, formatter_type, msgs, closure)`：`FormatterConverter::get_formatter_target` 映射 target、`SpanKind::Internal`、`ctx` 提供父 span（`None`=根 span，对应 Java `Context.current()` 无上下文）；core 无 formatter 拦截点 → 宿主在格式化处调用（§4.2 第 6 条）**【2026-11-01 更新：core 已增 `ExtensionPoint::OnFormat`，`format` span 由 `TelemetryTracing::on_format` 自动产出（但不带 formatter 结果）；`call_format` 保留给宿主自格式化场景】** |

- **顺带对齐**：agent span 名改为 `invoke_agent <agent name>`（Java `TelemetryTracer.java:96` `INVOKE_AGENT + " " + instance.getName()`），原先只有 `invoke_agent`。
- **未改动（记录在案）**：`gen_ai.provider.name` 仍取配置（默认 `unknown`）—— `TelemetryTracing` 只拿到 `Arc<dyn Model>`，Rust 无运行时反射（见 history/STUDIO__MIGRATION_HISTORY.md ProviderNameConverter 行）。

**新增/修改测试（+8，62 → 70）**
`studio::socket_io::tests`：`handshake_auth_uses_the_java_snake_case_run_id_key`、`the_legacy_camel_case_auth_key_is_opt_in`。
`telemetry::tracing::tests`：`function_names_follow_the_java_type_and_method_shape`、`the_agent_span_records_the_reply_as_response_attributes`、`the_model_span_aggregates_the_streamed_response`（两 chunk → text 合并 `Hello`、usage 取 max、finishReasons、response id）、`the_tool_span_records_the_result_from_the_acting_sink`、`call_format_emits_a_format_span_with_both_sides`、`call_format_attaches_to_the_run_span`；`a_real_agent_run_exports_agent_and_model_spans` 加强为断言真实链路下的 `function.name` 与 usage/finish_reason/response id。

**验证（本次实测）**
- `cargo build -p agentscope-extensions-studio` → **EXIT=0**，本 crate 0 error / 0 warning（仅 core/harness 既有告警）。
- `cargo test -p agentscope-extensions-studio` → **70 passed / 0 failed / 0 ignored**。
- `cargo fmt -p agentscope-extensions-studio -- --check` → 干净。

**反向验证（逐个回退 → 对应用例 FAILED → 恢复）**
| 回退内容 | 结果 |
|---|---|
| `auth_payload(...)` → `json!({"runId": ...})` | `handshake_auth_uses_the_java_snake_case_run_id_key` **FAILED**（1 failed） |
| `AGENTSCOPE_FUNCTION_NAME` 键改成错的键名 | `the_tool_span_records_the_result_from_the_acting_sink`、`call_format_emits_a_format_span_with_both_sides`、`the_model_span_aggregates_the_streamed_response`、`a_real_agent_run_exports_agent_and_model_spans` **4 FAILED** |
| `SpanGuard::end` 不应用 finalizer | 上述 3 个响应侧用例 + 真实链路用例 **4 FAILED**（`call_format` 用例仍 ok，因其直接写 span，验证两条路径相互独立） |
| `on_model_call` 不包装 `AggregatingModel`（不 tee） | `the_model_span_aggregates_the_streamed_response`、`a_real_agent_run_exports_agent_and_model_spans` **2 FAILED** |
| `call_format` 不写响应侧属性 | `call_format_emits_a_format_span_with_both_sides` **FAILED** |

**未完成**：`StudioManager` 自动装配（§4.2 第 4 条）；`format` span 的宿主调用点（§4.2 第 6 条）；P2-1/P2-2/P2-3/P2-4 未动。

### 4.2 下一步与阻塞（待汇总方裁定）
1. **【已解决 — 降级方案】** OTLP 导出**不引入 `opentelemetry`/`opentelemetry-otlp`**（避免根 `[workspace.dependencies]` 变更）：以 `telemetry::exporter` 自建 `Span` 模型 + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait 实现等价导出。`TelemetryTracer` 等价物已于 2026-10-21 落地（`TelemetryTracing`，§4.1.0u）。若后续要求与 OTel SDK 字节级兼容，再提请裁定。
2. **【已解决 — 本 crate 自声明依赖】** Socket.IO：已用 `tokio-tungstenite = 0.29`（与 CHANNEL crate 同版本/同 features）+ 自实现 Engine.IO v4/Socket.IO 帧（`studio::socket_io`）。**未改根 `Cargo.toml`。**
   附带说明：`StudioManager::initialize_with` 默认仍用 `NoopStudioTransport`（Java 为无条件连接）；宿主要启用真实连接需走 `initialize_with_transport` / `StudioWebSocketClient::with_transport_and_registry`。**原因**：默认连接会让本 crate 现有离线测试向 `localhost:3000` 发起真实网络请求（违反离线测试约定）。
3. **【core 缺口】** `GenerateOptions` 未实现 `Serialize`（本 crate 已用本地投影绕过，未改 core）；core 无 `Hook`/`TracerRegistry`（已按 §2 用 `Middleware` 替代）。
4. **【已完成（设计决定，不迁移）— 2026-10-31 裁定】** `StudioManager` **不**自动装配 `TelemetryTracing`，宿主显式挂到 `MiddlewareChain`（`chain.add(Box::new(tracing.clone()))`）。
   **源码级依据**：①core 无 `TracerRegistry` 或任何全局 tracer 槽（`rg -n 'TracerRegistry' crates/agentscope-core crates/agentscope-harness` → 零命中），`MiddlewareChain::add` 是唯一挂载入口，Java「initialize 时 register 到静态注册表」在 Rust 无对应物；②自动装配需先裁定「一个 Studio 实例共享一条 middleware vs 每 agent 一条」（共享实例可行：per-run 状态以 `run_id` 为键，但 span 生命周期跨 agent，属产品级选择，不在迁移范围）；③本 crate 离线契约是不做隐式 I/O（默认 `NoopStudioTransport`/`NoopTraceExporter`）。与 Java 自动装配等价的宿主写法：用 `OtlpHttpExporter::from_config(&cfg).with_header(arg, token)` 构造 exporter，再 `chain.add(Box::new(TelemetryTracing::new(Arc::new(exporter))))`。`StudioConfig.tracing_url` 仍由 `OtlpHttpExporter::from_config` 消费，不是死配置。
5. **【P0/P1/P2 状态收口 — 2026-10-31】** ~~P1-1~~ / ~~P1-2~~ 于 2026-10-28 关闭（§4.1.0v）；~~P0-1~~ 于 2026-10-22 修复（见 history/STUDIO__MIGRATION_HISTORY.md）；~~P2-2~~ / ~~P2-3~~ / ~~P2-4~~ 于 2026-10-31 关闭（见 history/STUDIO__MIGRATION_HISTORY.md）；~~P2-1~~ 同日裁定为设计决定（不迁移）。
6. **【跨模块需求（本 crate 无法单独关闭）— core 缺口，2026-10-31 复核保留】** `format` span 只能以 `TelemetryTracing::call_format(ctx, formatter_type, msgs, closure)` 形式提供：`ExtensionPoint` 实为 **8 个变体，无 formatter 拦截点**（`crates/agentscope-core/src/middleware/middleware.rs:136-147`：OnAgent/OnReasoning/OnReasoningResult/OnActing/OnModelCall/OnSystemPrompt/OnPreCall/OnAgentStateReady），`agentscope_core::formatter` 也只有自由函数（`base_formatter.rs`/`media_utils.rs`/`json_schema.rs`/`response_format.rs`）、无 formatter trait。**宿主须在真正格式化处调用该包装器**，否则 `format {target}` span 不会产生（不阻塞其他 span）。Java 侧由 `Tracer.callFormat(formatter, msgs, supplier)` 在框架内自动包裹；若需框架级自动包裹，需 core 新增 formatter 钩子（不在本 crate 写入范围，已作回执「跨模块需求」）。
   > **2026-11-01 已解决（Round 2）**：core 的 `ExtensionPoint::OnFormat` + `ApplyFormat` 缝已落地，`TelemetryTracing::on_format` 在每次带 `ResponseFormat` 的模型请求前产出 `format <target>` span，不再依赖宿主调用 `call_format`（仅记请求侧、状态 `Unset`，因钩子不给 formatter 结果）。详见 §3.5.1 STUDIO-12 行。
7. **【真实剩余（2026-10-29 复核，均为已在册项，非新缺口）】**
   - ~~`StudioClient` 无 HTTP 注入点（`studio/client.rs:22` 直接持 `reqwest::Client`）→ 网络路径不可离线测（§3.2）。~~ **已解决 2026-10-30（见 history/STUDIO__MIGRATION_HISTORY.md）**：`studio/http.rs` 注入点 + 11 个离线用例。
   - ~~`StudioUserAgent` 未与 core agent 循环集成（仅独立输入提供者，§3.2）。~~ **已解决 2026-10-30（见 history/STUDIO__MIGRATION_HISTORY.md）**：`drive_round` / `drive_round_with`（泛型 `CallableAgent`）。
   - ~~`TelemetryTracing` 未在 `StudioManager::initialize*` 自动装配（第 4 条）。~~ **已裁定 2026-10-31**：设计决定（不迁移），宿主显式 `MiddlewareChain::add`。
   - `format` span 需宿主显式调 `call_format`（第 6 条）—— **跨模块需求**（core 无 formatter 钩子），保留。
   - ~~非 2xx 不重试（Java 会重试，见 history/STUDIO__MIGRATION_HISTORY.md P2-1）。~~ **已裁定 2026-10-31**：设计决定（不迁移），`is_retryable()` 文档已改正。
   - ~~`OtlpHttpExporter` 无自定义 header（见 history/STUDIO__MIGRATION_HISTORY.md P2-2）。~~ **已解决 2026-10-31（见 history/STUDIO__MIGRATION_HISTORY.md）**：`with_header`/`with_headers`。
   - ~~属性键缺 5 个（Java 44 → Rust 39，见 history/STUDIO__MIGRATION_HISTORY.md P2-3）。~~ **已解决 2026-10-31（见 history/STUDIO__MIGRATION_HISTORY.md）**：44/44。
   - `gen_ai.provider.name` 恒 `unknown`（`ProviderNameConverter` 未被 `TelemetryTracing` 调用；Rust 无运行时反射）—— 设计决定，用 `with_provider_name` 覆盖。
   - ~~多模态 `block_to_part` 对 Image/Audio/Video 返回 None（Java 亦 TODO）。~~ **已裁定 2026-10-31**：设计决定（不迁移），跟 Java TODO（§3.3）。

8. **【2026-11-01 Round 4 收口 — §3.5.1 七项清零】** 本轮无新增跨模块需求（core/harness 未改）。新行为须知：
   - `SocketIoTransport::connect()` 现在等命名空间 `40` ACK（默认 10s，`with_connect_timeout` 可调），失败/超时返回 `StudioError::WebSocket`；`is_connected()` 仅在 `Ready` 为真。`StudioManager::initialize_with_transports` 仍**只 warn 不中断**（与 Java `StudioManager.Builder#initialize` 的 error-log 行为一致），要判断「是否真的连上」请查 `StudioManager::web_socket_client()?.is_connected()`。
   - `StudioManagerBuilder::initialize()` 默认仍为 `NoopStudioTransport`（离线契约，测试不得连 `localhost:3000`）；生产须显式 `.transport(Arc::new(SocketIoTransport::new(&cfg, registry)?))`。
   - 读循环新增心跳 watchdog（窗口 `pingInterval + pingTimeout`，握手前不加超时），半开连接会按既有退避重连；`pingInterval`/`pingTimeout` 由 `0{...}` 握手提供。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本节取代以往散落在 §4.2 / history 中的「差异」描述，作为本模块**唯一**的差异表；执行计划类内容仍留在 §4.1/§4.2。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| STUDIO-D01 | `StudioManager.initialize()` 自动装配 `StudioMessageHook` + `TelemetryTracer` | `StudioManager.java`：`AgentBase.addSystemHook(new StudioMessageHook(...))`、`TracerRegistry.register(TelemetryTracer.builder()...)`；`TracerRegistry` 为 core 全局静态注册表 | ① core 无 `TracerRegistry` 或等价全局 tracer 槽：`rg 'TracerRegistry' crates/agentscope-core crates/agentscope-harness` → **零命中**，`MiddlewareChain::add` 是唯一挂载入口；② core 无 `Hook` trait 与全局 hook 注册 API：`rg 'pub trait Hook|add_system_hook|addSystemHook' crates/agentscope-core/src` → **零命中**（`Hook` 仅存于注释引述 Java 的 `HookEvent`，如 `middleware/input.rs:20`）；③ 自动装配还需先裁定「一个 Studio 实例共享一条 middleware vs 每 agent 一条」——属产品级选择；④ 本 crate 离线契约不做隐式 I/O（默认 `NoopStudioTransport`/`NoopTraceExporter`，见 `src/studio/mod.rs:97-99`、`src/telemetry/exporter.rs:308-319`） | 宿主须显式两行：`chain.add(Box::new(StudioMessageHook::new(client)))`、`chain.add(Box::new(TelemetryTracing::new(Arc::new(otel_exporter))))`；Studio 初始化不再「自动生效」，需在宿主装配点校验 |
| STUDIO-D02 | 多模态内容部件抽取（Image/Audio/Video → OTel part） | `AttributesExtractors.java:400/461/503/548` 四处 `// TODO: support multi modal content`；`:548` 对 `VideoBlock` 仅回 `<VideoBlock is not supported>` | Java 侧同为占位实现，行为已等价：Rust `block_to_part`（`src/telemetry/extractors.rs:334-359`）对 Image/Audio/Video 返回 `None`，`get_tool_call_result`（`:420-436`）返回同样的 `<ImageBlock/AudioBlock/VideoBlock is not supported>` 文本块 | `gen_ai.input.messages`/`output.messages` 中多媒体块缺失（与 Java 一致）；待上游实现后再跟 |
| STUDIO-D03 | 非 2xx 响应不重试 | `StudioClient.executeWithRetry` 以 `t instanceof IOException` 过滤，而 `:115/160/208` 把**每个**非 2xx 都包成 `IOException` → Java 对 4xx/5xx 也会重试（盲目重投 `pushMessage`/`requestUserInput`） | Rust 把状态码显式建模为 `StudioError::Http{status,message}` 且 `is_retryable()` 只认 `Transport`（`src/error.rs:47-58`），避免重复提交副作用请求；语义已写入 rustdoc 与用例 `http_status_errors_are_not_retried` | 瞬时 5xx（网关抖动）不再自动重试；需要时由宿主/上层重试。若后续要求「仅 5xx 重试」，属策略变更而非缺口 |
| STUDIO-D04 | 不新增 `TelemetryTracer` 类型，也不复刻 `Tracer` trait 接口形态 | `io.agentscope.core.tracing.Tracer`（`callAgent/callModel/callTool/callFormat/runWithContext/shutdown`）+ `TelemetryTracer implements Tracer` | Java 的 `Tracer` 是为「可插拔 + 全局注册表」设计；Rust 的等价点已全部映射到既有 seam（裁决 B5，见 §2.0 `telemetry.TelemetryTracer` 行）：`callAgent/callModel/callTool` → `Middleware::{on_agent,on_model_call,on_acting}`、`callFormat` → `Middleware::on_format`（框架内）/`TelemetryTracing::call_format`（宿主侧）、`shutdown()` → `flush()`、exporter 注入 → `TelemetryTracing::new(Arc<dyn TraceExporter>)`、`TracerRegistry.register` → `MiddlewareChain::add`；`runWithContext` 在 Rust 由参数显式传 `&RuntimeContext` 取代，无需 reactor 上下文搬运 | 无 `tracer::<T>()` 动态取用；宿主以具体类型 `TelemetryTracing` 组合，不支持运行时替换 tracer 实现（换 exporter 即可） |
| STUDIO-D05 | `Hook` 接口与 `AgentBase` 子类化（`StudioUserAgent extends AgentBase`）不迁移 | `StudioMessageHook implements Hook`；`StudioUserAgent extends AgentBase`（`doCall`/`handleInterrupt`/终端与 Studio 双模式循环） | ① core 无 `Hook`（`StudioMessageHook` 已改实现 `Middleware::on_agent`，`src/studio/message_hook.rs:40-79`）；② core 无 agent 子类化机制，`StudioUserAgent` 只能是独立输入提供者（`src/studio/user_agent.rs:1-10` 模块文档说明）；③ 多轮/中断语义因此由宿主循环 + `RunControl` 承担，`drive_round`/`drive_round_with`（`:88-118`）只负责单轮 | 宿主需自建「读输入→跑一轮→再读输入」循环；Java `handleInterrupt` 的 `Interrupted by user` 消息无直接对应物（Rust 中断经 `Error::Interrupted`/`RunControl` 表达，`drive_round` 映射为 `StudioError::Agent`） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| STUDIO-X01 | OTLP 导出**不引入** `opentelemetry` / `opentelemetry-otlp`，自建编码器 | `src/telemetry/exporter.rs`：`Span`/`SpanKind`/`SpanStatus` + `OtlpEncoder`（OTLP/HTTP **JSON**）+ `TraceExporter` trait + `OtlpHttpExporter`（reqwest） | Java 用 OTel SDK：`SdkTracerProvider` + `BatchSpanProcessor` + `OtlpHttpSpanExporter`，默认 **protobuf** 编码；Rust 发 `application/json` | 收益：① 不为一个模块改动根 `[workspace.dependencies]`（`opentelemetry` 全套会带入 tonic/prost 或 protox 构建链）；② `Span` 是纯数据，编码/属性映射可**完全离线**断言（`telemetry::exporter::tests` 13 例）；③ 字段集合可控（不随 SDK 版本漂移）。代价：见 STUDIO-06（导出重试需自建；批量调度已于 2026-10-31 自建，STUDIO-02）与 STUDIO-X07（省略可选字段） |
| STUDIO-X02 | 默认不连网：`initialize_with` 用 `NoopStudioTransport`，测试用 `NoopTraceExporter` | `src/studio/mod.rs:97-99`（`initialize_with` → `NoopStudioTransport`）、`:107-113`（`initialize_with_transport` 仍默认 `ReqwestStudioTransport` 做 HTTP）、`src/telemetry/exporter.rs:308-319` | Java `initialize()` 无条件 `wsClient.connect()`（连不上则整体失败） | 保留：本 crate 的 92 个用例全离线（约定见 §5），默认连 `localhost:3000` 会让单测发起真实网络请求；生产入口为 `initialize_with_transports`（其可用性问题见 STUDIO-01/09） |
| STUDIO-X03 | `tracingUrl` 推导为**绝对** URL：`{studioUrl 去尾斜杠}/v1/traces` | `src/studio/config.rs:169-196`（`build()` 推导）、`src/telemetry/exporter.rs:397-400`（`from_config` 消费） | Java `StudioManager` 用 `URI.create(studioUrl).getPath() + "/v1/traces"`：对 `http://localhost:3000` 得 **`/v1/traces`**（丢掉 scheme/host，OTLP exporter 会因非绝对 URI 失败），且 `StudioConfig.getTracingUrl()` 默认为 `null` | 保留：这是 Java 侧缺陷，Rust 直接给出可用端点；`tracing_url` 因此不是死配置 |
| STUDIO-X04 | span 生命周期 RAII + `run_id` 父子映射（不走 OTel `Context`/reactor 上下文） | `src/telemetry/tracing.rs`：`SpanGuard`/`SpanStream` 随事件流 drop 收尾（`:479-503`、`:551-596`）、父子经 `Arc<Mutex<HashMap<run_key, SpanCtx>>>`（`:202-210`，`run_key` = `run_id` → `session` → `agent` 降级） | Java 用 `ContextPropagationOperator.get/storeOpenTelemetryContext` 在 reactor 流里传播 OTel `Context`，`Span.makeCurrent()` | 保留：Rust `Middleware` hook 只拿 `&RuntimeContext`（不可变，不能写回 span），且取消 = drop future，没有 `on_cancel` 钩子；RAII 是「取消不漏 end」的唯一可靠保证（反向验证见 §4.1.0u/§4.1.0v）。代价：同一 `run_id` 下并发子 agent 共享父 span（Java 按 Reactor 订阅链各自继承） |
| STUDIO-X05 | 属性以 `(String, serde_json::Value)` 有序 `Vec` 表示 | `src/telemetry/extractors.rs:19-22`（`Attribute`/`Attributes` 类型别名） | Java 用 OTel `Attributes`/`AttributesBuilder`（含 `AttributeKey<T>` 类型约束） | 保留：抽取值可离线比对、调用处不需要 `opentelemetry` 类型泄漏到本 crate 公共 API；类型安全由各 `get_*_attributes` 的签名保证（`&str`/`Option<&T>` 入参） |
| STUDIO-X06 | 三处可注入 seam + 显式超时参数 | `StudioHttpTransport`（`src/studio/http.rs:113-117`）、`StudioTransport`（`src/studio/websocket.rs:46-56`）、`TraceExporter`（`src/telemetry/exporter.rs:301-305`）；`wait_for_input(request_id, timeout)`（`websocket.rs:187-205`） | Java 在 `StudioClient` 里直接 `new OkHttpClient()`、在 `StudioWebSocketClient` 里直接 `IO.socket(...)`；`waitForInput(requestId)` 内部硬编码 `timeout(Duration.ofMinutes(30))` | 保留：Java 的网络路径只能靠真实服务器/拦截器测；Rust 的 `studio::client::tests`（10）、`studio::websocket::tests`（11）、`studio::socket_io::tests`（14）走真实公开方法断言方法/路径/头/JSON body、2xx、非 2xx 不重试、传输错误重试耗尽。超时外置让宿主可配置（默认仍为 `DEFAULT_INPUT_TIMEOUT=30min`，`user_agent.rs:24`） |
| STUDIO-X07 | OTLP payload 只发 `traceId/spanId/parentSpanId/name/kind/startTimeUnixNano/endTimeUnixNano/attributes/status`；resource 为 `service.name=agentscope` + `agentscope.project` + `agentscope.run_id` | `src/telemetry/exporter.rs:256-290`（span 字段）、`:291-296`（`resource_attributes`） | Java/OTel SDK 额外发 `droppedAttributesCount`/`events`/`links`/`flags`（通常为空）与默认 resource `telemetry.sdk.{name,language,version}`、`service.version` | 保留：proto3 JSON 允许省略默认值字段，Studio/collector 均可解析；Rust 侧不伪造 `telemetry.sdk.*`（本 crate 不是 OTel SDK）。若后端按 `telemetry.sdk.language` 过滤，需宿主在 exporter 增加 resource 属性 |
| STUDIO-X08 | 「禁用遥测」= 注入 `NoopTraceExporter`；`StudioClient::shutdown()` 为空实现 | `NoopTraceExporter`（`src/telemetry/exporter.rs:308-319`）由宿主注入；`StudioClient::shutdown`（`src/studio/client.rs:131-135`）只记 debug 日志 | Java `TelemetryTracer.builder().enabled(false)` → `TracerProvider.noop()` 的 NOOP tracer；`StudioClient.shutdown` 显式 `dispatcher().executorService().shutdown()` + `connectionPool().evictAll()` | 保留：`TraceExporter` 是 trait，禁用只需换实现，无需 `enabled` 布尔贯穿全链；reqwest 的 `Client` 池随 `Arc` drop 释放，无显式 shutdown API，硬造一个空壳开关反而误导 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-studio` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-studio` / `cargo test -p agentscope-extensions-studio` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-studio/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/STUDIO__MIGRATION_HISTORY.md`](./history/STUDIO__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
