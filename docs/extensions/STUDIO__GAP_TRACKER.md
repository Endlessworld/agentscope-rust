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
| Rust 规模（实测 2026-10-31） | 19 个 `.rs` / **6998 行**（非空行 6363）/ **92 个内联测试**（62 `#[test]` + 30 `#[tokio::test]`；新增 9 个待主智能体编译验证）|
| 迁移波次 | C |
| 当前状态 | **总判：已完成（P1 清零）**—— 原 2 项 P1（`StudioClient` 无 HTTP 注入点 / `StudioUserAgent` 未集成 core agent 循环）已于 **2026-10-30** 关闭（见 §9）。已完成（**92 个离线单测**；Socket.IO 真实传输 + OTLP 降级导出已落地；`TelemetryTracing` 已落地但**未自动装配**（设计决定，不迁移；§4.2 第 4 条））。**2026-10-22 静态复核**发现的 3 项已于 2026-10-28 全部关闭：P0-1 `StudioMessageHook` 捕获键（§7.1）、P1-1 `TelemetryTracer` 三处语义（§4.1.0v）、P1-2 Socket.IO 鉴权键（§4.1.0v）。**2026-10-31 收口（§10）**：P2-2 / P2-3 / P2-4 落代码 + 离线测试；P2-1、§4.2 第 4 条（`StudioManager` 自动装配）、多模态抽取裁定为「设计决定（不迁移）」并附源码证据；仅 §4.2 第 6 条 `format` span 自动接线保留为**跨模块需求**（core 无 formatter 钩子） |
| 行数口径 | Java 4305/1837 与 Rust 5048 均为**物理行**（含空行/注释）；非空行口径 Java 3738/1554、Rust 4578。汇总 tracker 的 3700/1452 与之同族，需按口径换算 |

## 2. Java 清单与依赖映射

> Java 主包 `io.agentscope.core.studio`（HTTP/WS/管理）与 `io.agentscope.core.tracing.telemetry`（OTel GenAI 遥测）。

### 2.0 子模块清单
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `studio.StudioConfig` | 连接配置（url/tracingUrl/project/runName/runId/maxRetries/reconnect*）| `studio::StudioConfig` + `StudioConfigBuilder` | 默认值对齐：url=`http://localhost:3000`、project=`UnnamedProject`、runName=`run_<ts>`、runId=UUID、maxRetries=3、reconnectAttempts=3、delay=1s、maxDelay=5s；tracingUrl 默认 `studioUrl + /v1/traces` |
| `studio.StudioClient` | OkHttp HTTP 客户端（registerRun/pushMessage/requestUserInput + 重试 + shutdown）| `studio::StudioClient` | reqwest + 指数退避重试（base 100ms，max 5s，`max_retries` 次）；时间戳 `yyyy-MM-dd HH:mm:ss.SSS`。**静态复核**：Rust 仅对 `StudioError::Transport` 重试，Java 因非 2xx 被包成 `IOException` **也会重试**（语义分歧，见 §6.4 P2-1）|
| `studio.StudioWebSocketClient` | Socket.IO 连接 `/python` namespace，监听 `forwardUserInput`，`waitForInput` 等待 Mono | `studio::StudioWebSocketClient` + `studio::socket_io` | `StudioTransport` trait（可插拔）+ `PendingInputRegistry`（`HashMap<requestId, oneshot>`）；**`SocketIoTransport` 为真实实现**（tokio-tungstenite 0.29 + 自实现 Engine.IO v4 握手/`40` 入房/ping-pong/指数退避重连）；帧解析为纯函数。**鉴权键已对齐 Java（2026-10-28，§4.1.0v）**：`auth_payload()` 默认发 `{"run_id": <runId>}`（Java `StudioWebSocketClient.java:98`），旧 `runId` 仅在 `SocketIoTransport::with_legacy_auth_key()` 显式开启时一并发送 |
| `studio.StudioWebSocketClient.UserInputData` | 用户输入载荷（blocksInput + structuredInput）| `studio::UserInputData` | |
| `studio.StudioManager` | 全局静态单例（config/client/wsClient + init builder + shutdown + TracerRegistry 注册）| `studio::StudioManager` | Rust 用 `OnceLock<RwLock<Option<...>>>` 全局；不依赖 core `TracerRegistry`（core 无此类型，见 §4.2） |
| `studio.StudioMessageHook` | `Hook`，拦截 `PostCallEvent` 把最终消息推给 Studio | `studio::StudioMessageHook` | Rust 无 `Hook`（core `hook` 为设计决定不迁移）；改为 core `Middleware`：`on_agent` 包裹后在事件流上捕获终态消息并推送（fire-and-forget）。**已完成（2026-10-29 勘误）**：原记「静态复核：捕获键为 `fields["message"]`…→ 转发恒不触发（§6.4 P0-1）」，实际已于 2026-10-22 修复（§7.1）：`final_message` 优先 `fields["reply"]`，`fields["message"]` 仅作第三方回退（`studio/message_hook.rs:88-97`）|
| `studio.StudioUserAgent` | 用户代理 Agent（终端 / Studio 双模式）| `studio::StudioUserAgent` | 输出 Msg（USER 角色），Studio 模式失败回退终端 |
| `studio.pojo.RegisterRunRequest` | registerRun 请求体 | `studio::pojo::RegisterRunRequest` | 字段：id/project/name/timestamp/pid/status/run_dir |
| `studio.pojo.PushMessageRequest` | pushMessage 请求体 | `studio::pojo::PushMessageRequest` | 字段：runId/replyId/name/role/msg |
| `studio.pojo.RequestUserInputRequest` | requestUserInput 请求体 | `studio::pojo::RequestUserInputRequest` | 字段：requestId/runId/agentId/agentName/structuredInput |
| `studio.pojo.UserInputMetadata` | 用户输入元数据（source/requestId/structuredInput）| `studio::pojo::UserInputMetadata` | `@JsonInclude(NON_NULL)`；source 默认 `studio` |
| `telemetry.GenAiIncubatingAttributes` | OTel GenAI 语义约定属性键（**semantic conventions 1.41.0**，Java 类注释 `GenAiIncubatingAttributes.java:28`；Rust 模块文档 2026-10-31 同步为 1.41.0）| `telemetry::gen_ai_attributes` | 常量字符串键（`gen_ai.*`）+ operation/provider 取值常量。**已补齐（2026-10-31，§10）**：38 键与 Java `AttributeKey` 声明一一对应（新增 `GEN_AI_TOOL_TYPE` + 3 个 usage 键），并有「常量 vs Java 字面量」双向交叉校验用例。`opentelemetry-bom` 1.66→1.67（`8690ab3f5`）只动 SDK，不改键集 |
| `telemetry.AgentScopeIncubatingAttributes` | AgentScope 自定义属性键 | `telemetry::agent_scope_attributes` | `agentscope.function.*`、`agentscope.format.*`、`agentscope.usage.*`。**已补齐（2026-10-31）**：6 键与 Java 一一对应（新增 `AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS`）|
| `telemetry.AttributesExtractors` | 从 agent/model/tool/formatter 抽取 OTel 属性（实测 648 行）| `telemetry::extractors`（**同名模块，不存在 `AttributesExtractors` 类型**）| 纯函数（`get_*_attributes`）；复用 core 的 Msg/ContentBlock/ChatResponse/ToolSchema/GenerateOptions |
| `telemetry.AttributesExtractors.FormatterConverter` | formatter 简单类名 → provider 名映射 | `telemetry::FormatterConverter` | DashScope/Gemini/OpenAI→dashscope；Anthropic→anthropic；其余 unknown |
| `telemetry.AttributesExtractors.ProviderNameConverter` | ChatModelBase 实例 → provider 名（按类继承链判断）| `telemetry::ProviderNameConverter` | Rust 无运行时类反射；改为按模型类型名后缀探测（`DashScopeChatModel`/`GeminiChatModel`/`AnthropicChatModel`/`OpenAIChatModel`）。**设计取舍**：`TelemetryTracing` 只用 `DEFAULT_PROVIDER_NAME="unknown"`/宿主 `with_provider_name` 覆盖（拿到的只是 `Arc<dyn Model>`，无类名）——2026-10-28 复核确认无法在本 crate 内反射出 provider，未随 §6.4 P1-1 关闭 |
| `telemetry.StreamChatResponseAggregator` | 流式 ChatResponse 聚合（text/thinking/tool_calls + usage max + finishReason）| `telemetry::StreamChatResponseAggregator` | 复用 core `ChatResponse`/`ChatUsage`/`ContentBlock`。**已接入（2026-10-28，§4.1.0v）**：`TelemetryTracing::on_model_call` 用 `AggregatingModel` tee 模型流逐 chunk `append`（等价 Java `doOnNext(aggregator::append)`），span 结束时写 `get_llm_response_attributes` |
| `telemetry.TelemetryTracer` | `Tracer` 实现：agent/model/tool/**format** 四类 span + OTLP HTTP 导出 | **`telemetry::TelemetryTracing`**（**不存在 `TelemetryTracer` 类型**）| 包装为 core `Middleware`：`on_agent`/`on_model_call`/`on_acting`（无 `on_reasoning`，core 无该必要）+ `call_format`（同步包装器，core 无 formatter 拦截点）；OTLP 导出为降级 exporter（`telemetry::exporter`，见 §4.1.0s）。**2026-10-28 已补齐（§4.1.0v）**：`agentscope.function.name`（四类 span）、三个响应侧抽取器、format span。**2026-10-31 裁定（裁决 B5，§10）**：Java `Tracer` 接口语义已完全映射到既有 seam，不再新增 `TelemetryTracer` 类型——`callAgent/callModel/callTool` → `Middleware::{on_agent,on_model_call,on_acting}`，`callFormat` → `TelemetryTracing::call_format`，`Tracer.shutdown()` → `flush()`，exporter 注入 → `TelemetryTracing::new(Arc<dyn TraceExporter>)`（离线用 `RecordingTraceExporter`），`TracerRegistry.register` → 宿主 `MiddlewareChain::add` |
| `telemetry.model.*`（**13 个类**）| OTel 消息部件模型（Text/Reasoning/ToolCallRequest/ToolCallResponse/Blob/File/Uri Part、Input/OutputMessage、Role、SystemInstructions、ToolDefinition）| `telemetry::model`（13 个 `pub` 类型，与 Java 一一对应）| serde 结构；`type` 字段判别 + JSON 字段名对齐（`mime_type`/`file_id`/`finish_reason`）|

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp3 (OkHttpClient) | Studio HTTP | `reqwest`（**workspace 0.13**，json/rustls）| 仅写本 crate Cargo.toml；根 `[workspace.dependencies]` 已有 reqwest（0.13），直接用 `reqwest = { workspace = true }` |
| reactor.core (Mono/Flux/Sinks) | 响应式 | `tokio::sync::oneshot/mpsc` + `futures::Stream` | 单值→async fn；等待→oneshot |
| socket.io-client（java） | Studio WS | `tokio-tungstenite = 0.29`（本 crate 直声明，default-features=false + connect/handshake/rustls-tls-webpki-roots）+ 本 crate 自实现 Engine.IO v4/Socket.IO 帧 | 已落地 `studio::socket_io`（见 §4.1.0s）；仅写本 crate `Cargo.toml`，未改根 `[workspace.dependencies]` |
| com.fasterxml.jackson（JSON） | 序列化 | `serde` / `serde_json` | 已建 |
| org.json (JSONArray/JSONObject) | WS 事件解析 | `serde_json::Value` | |
| reactor.util.retry.Retry | 重试退避 | `StudioClient::post_json` 内联退避循环（**无独立 `retry_with_backoff` 辅助函数**）| base 100ms → max 5s；重试谓词与 Java 有分歧（§6.4 P2-1）|
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
| `StudioClient`（registerRun/pushMessage/requestUserInput/shutdown + 重试）| StudioClient | 已完成 | reqwest；指数退避仅重试 `StudioError::Transport`；网络路径未离线测（§3.2）。**静态复核**：Java 的非 2xx 被包成 `IOException` 因而**也会**被重试，Rust 不重试 HTTP 状态错误（§6.4 P2-1）|
| `StudioWebSocketClient` + `StudioTransport` trait + `PendingInputRegistry` | StudioWebSocketClient | 已完成 | 可插拔 trait + oneshot 注册表；`NoopStudioTransport` 供离线测；单测 `studio::websocket::tests`（**11** = 6 `#[test]` + 5 `#[tokio::test]`，原记 9 有误） |
| `UserInputData` + `parse_content_blocks` + `preprocess_tool_result_block` | StudioWebSocketClient | 已完成 | 纯函数；含 tool_result 字符串 output→text 块规范化；同上单测覆盖 |
| `StudioMessageHook`（Middleware on_agent 后推送）| StudioMessageHook | 已完成 | 实现 core `Middleware::on_agent`，捕获终态消息 fire-and-forget 推送，失败仅日志不中断 agent。**已完成（2026-10-29 勘误）**：原记「静态复核：捕获键 `"message"` 与 core 实际写入的 `"reply"` 不一致 → 恒不推送（§6.4 P0-1）」，实际已于 **2026-10-22** 修复（§7.1）——`final_message` 优先读 `fields["reply"]`、`fields["message"]` 仅作第三方回退；依据 `crates/agentscope-extensions-studio/src/studio/message_hook.rs:88-97` 与用例 `reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`。另 `tokio::spawn` 需在 runtime 内调用 |
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
| `StudioClient` 网络路径离线测（mock HTTP） | StudioClient | 已完成（2026-10-30，§9） | 新增 `studio/http.rs` 注入点：`trait StudioHttpTransport { async fn execute(&self, StudioHttpRequest) -> StudioResult<StudioHttpResponse> }` + `ReqwestStudioTransport`（生产，行为不变）+ `StudioClient::with_transport`；`StudioClient` 改持 `Arc<dyn StudioHttpTransport>`。11 个离线用例走真实公开方法（方法/路径/头/JSON body、2xx、非 2xx 不重试、传输错误重试/耗尽）|
| `StudioUserAgent` 与 core agent 循环集成 | StudioUserAgent | 已完成（2026-10-30，§9；诚实范围见交接信息） | 新增 `StudioUserAgent::drive_round(&A)` / `drive_round_with(&A, Msg)`，泛型约束为 core `CallableAgent`（`ReActAgent`/`HarnessAgent` 直接可用）：取输入 → 推送用户消息到 Studio（失败仅告警）→ 跑一轮 agent → 返回回复；agent 错误映射为 `StudioError::Agent`。**不做**「自动多轮 / 终端交互」循环，那是宿主职责（Java 靠 `AgentBase` 子类化，core 无此机制）|
| `StudioMessageHook` 终态消息捕获键修正 | StudioMessageHook | 已完成（2026-10-22；2026-10-29 勘误：原记「待办」） | core 终态消息在 `fields["reply"]`（protocol crate 为 `fields["result"]`），Rust 侧原先读 `fields["message"]` → 恒不触发（§6.4 P0-1）。现 `final_message` 优先 `reply`、`message` 作第三方回退（`studio/message_hook.rs:88-97`），用例 `reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`（§7.1）|

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `TelemetryTracer`（core `Middleware` 包装 + span 生命周期） | TelemetryTracer | 已完成（2026-10-28：§6.4 P1-1 ①②③ 全部关闭） | `telemetry/tracing.rs`：`Middleware` 三拦截点（`OnAgent`/`OnModelCall`/`OnActing`）产出 `invoke_agent {agent}` / `chat {model}` / `execute_tool {name}`，`call_format` 产出 `format {target}`；span 用 **RAII 收尾**（guard 随事件流 drop 而结束，取消路径不漏 span）；父子靠 `RuntimeContext.run_id` 映射（`&RuntimeContext` 不可变，不能用 ctx 存 span）。`TraceExporter::export` 是 async 而 hook 是同步 → `SpanSink` 后台批量导出 + `flush()` 供测试/短进程。**2026-10-28 补齐**：①四类 span 均带 `agentscope.function.name`；②三个响应侧抽取器接入（`SpanGuard::finalizer`）：agent ← `AgentResult.fields["reply"]`、model ← `AggregatingModel` tee 模型流 + `StreamChatResponseAggregator`、tool ← `ActingResultsHandle`；③`call_format`（等价 Java `callFormat`）。单测 13 个（2 `#[test]` + 11 `#[tokio::test]`），含真实 `ReActAgent` 端到端断言 usage/finish_reason（§4.1.0v）|
| OTLP HTTP 导出 | exporter-otlp-http | 已完成（降级 exporter）| `telemetry::exporter`：`Span`/`SpanKind`/`SpanStatus`/`OtlpEncoder`（`resourceSpans`→`scopeSpans`→`spans`，id 十六进制校验、纳秒字符串、`AnyValue` 映射）+ `TraceExporter` trait + `OtlpHttpExporter`（reqwest `POST {tracingUrl}`）/`RecordingTraceExporter`/`NoopTraceExporter`；**未引入 `opentelemetry` 依赖**（设计决定，见 §4.2）；单测 `telemetry::exporter::tests`（**13** = 11 + 2）。**2026-10-31 已补齐**：Java `TelemetryTracer.Builder.addHeader/headers` 对应 `with_header` / `with_headers` / `headers()`（同名覆盖语义同 Java `HashMap`），POST 时随请求发送；Debug 输出只显 header 个数（不泄露鉴权值）|
| 多模态内容部件抽取 | TODO(multi-modal) | **已完成（设计决定，不迁移）**（2026-10-31） | Java 侧同为 TODO 占位：`AttributesExtractors.java:400` / `:461` / `:503` / `:548` 四处 `// TODO: support multi modal content`，其中 `:548` 对 `VideoBlock` 也只回一个 `<VideoBlock is not supported>` 文本块。Rust `block_to_part`（`telemetry/extractors.rs:316`）对 Image/Audio/Video 返回 None，行为等价；跟随上游即可 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成**（`cargo build` / `cargo test` 全绿；本 crate 0 warning）。
  > **2026-10-29 勘误**：原记 §3.1「`StudioMessageHook` 恒不推送（§6.4 P0-1）」与 §3.2「鉴权键 `runId`/`function.name`/响应侧抽取器/`callFormat` 待办」均已关闭——P0-1 于 2026-10-22 修复（§7.1），P1-1/P1-2 于 2026-10-28 修复（§4.1.0v，实测 **70 passed**）。
  > **2026-10-22 静态复核声明**：本轮**未执行** `cargo build` / `cargo test`（按子智能体约束仅静态核对），下列 build/test 结论沿用上一会话记录，**未经本轮复验**。
- 验证命令与结果（2026-09-18，**历史快照**）：
  - `cargo build -p agentscope-extensions-studio` → 成功，0 error / 0 warning（仅 core/harness 既有告警）。
  - `cargo test -p agentscope-extensions-studio` → **30 passed / 0 failed**（`--lib`）；doc-tests 0 passed。
  - 后续增量：2026-10-08 → **52 passed**（§4.1.0s）；2026-10-21 → **59 passed**（§4.1.0u）；2026-10-22 → **62 passed**（§7.1）；**2026-10-28 → 70 passed / 0 failed（本次实测，§4.1.0v）**。2026-10-22 静态复核按 `#[test]`/`#[tokio::test]` 注解逐文件计数得 **59**（46 + 13），与当次记录一致。
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
- **同步 hook / 异步 exporter**：`SpanSink` 用无界 channel + 后台任务，等价 Java `BatchSpanProcessor`；无 tokio runtime 时丢弃 span 并 warn（不 panic、不阻塞）。
- **首版不含响应侧属性**（`gen_ai.usage.*`）：需从事件流聚合，留作后续；当前只记请求侧 + 状态 + 时间。
  > **2026-10-22 静态复核补充**：响应侧属性未落地不只是「缺 usage」——`get_agent_response_attributes`/`get_llm_response_attributes`/`get_tool_response_attributes` 三个抽取器**完全未被 `tracing.rs` 引用**，`StreamChatResponseAggregator` 也**未被接入**；同时 span 上缺 `agentscope.function.name`，且缺 Java `callFormat` 对应的 format span。三项合并计入 §6.4 P1-1。
  > **2026-10-28 已全部补齐**（§4.1.0v）；上述两行保留为历史记录。

**测试**：`a_completed_run_exports_the_agent_span`、`a_failed_run_marks_the_span_as_error`、`a_dropped_run_still_ends_the_span`（**反向验证：去掉 Drop 路径后仅此用例 FAILED**）、`nested_phases_attach_to_the_run_span`、`a_run_without_a_run_id_falls_back_to_the_session`、`only_the_three_traced_points_are_active`、`a_real_agent_run_exports_agent_and_model_spans`（真实 `ReActAgent` + Mock 模型）。

**验证**：`cargo test -p agentscope-extensions-studio` → **59 passed / 0 failed**（基线 52，+7）。

#### 4.1.0v 2026-10-28 执行记录：关闭 P1×2（Socket.IO 鉴权键 + `TelemetryTracing` 缺失项）

> Java 参考：`agentscope-java` HEAD `16cb024ae`（`StudioWebSocketClient.java`、`TelemetryTracer.java`、`AttributesExtractors.java`）。本轮**只改本 crate**（`crates/agentscope-extensions-studio/**`）与本 tracker。

**① Socket.IO 鉴权键对齐 Java（§3.2 / §6.4 P1-2）**

| Java 依据 | 内容 |
|---|---|
| `studio/StudioWebSocketClient.java:96-99` | `Map<String,String> auth = new HashMap<>(); auth.put("run_id", config.getRunId()); options.auth = auth;` |
| 全模块 grep `run_id\|auth\|query` | 握手鉴权**只有一个键** `run_id`（无 `project`/`token` 等其它键；`runId` 只出现在 POJO 字段，如 `PushMessageRequest.runId`，与 WS 鉴权无关） |

- Rust 原实现：`serde_json::json!({ "runId": config.run_id() })` → 与 Java/Studio 服务端校验键不一致，命名空间入房可能被判未鉴权。
- 现实现：`AUTH_RUN_ID_KEY = "run_id"`、`LEGACY_AUTH_RUN_ID_KEY = "runId"`、`auth_payload(run_id, include_legacy_key)`；`SocketIoTransport::new` 默认发 `{"run_id": ...}`；**兼容回退** `SocketIoTransport::with_legacy_auth_key()` 把旧 `runId` **叠加**在 `run_id` 之上（默认不叠加，保证与 Java 字节级一致）。
- 测试：`handshake_auth_uses_the_java_snake_case_run_id_key`（默认载荷 + 实际 `40/python,{...}` 连接帧）、`the_legacy_camel_case_auth_key_is_opt_in`（旧键仅显式开启；Java 键恒在）；`connect_frame_carries_namespace_and_auth` / `protocol_joins_the_namespace_after_the_handshake` 中的样例载荷同步改为 `run_id`。

**② `TelemetryTracing` 三处缺失（§3.3 / §6.4 P1-1）**

| # | Java 依据 | 补法 |
|---|---|---|
| ① `agentscope.function.name` | `TelemetryTracer.java:104/124/169/215`（四类 span 均 `setAttribute(AGENTSCOPE_FUNCTION_NAME, getFunctionName(instance, method))`）；`AttributesExtractors.java:350` `class.getSimpleName() + "." + method` | `TelemetryTracing` 在四类 span 上写 `agentscope.function.name`：agent=`ReActAgent.callAgent`（`with_agent_type_name` 可覆盖）、model=`<ProviderChatModel>.callModel`（provider→Java 类名映射，未知=`ChatModel`）、tool=`Toolkit.callTool`（`TOOL_FUNCTION_NAME`）、format=`<Formatter>.format` |
| ② 三个响应侧抽取器 + `StreamChatResponseAggregator` | `TelemetryTracer.java:110`（agent `doOnSuccess` → `getAgentResponseAttributes`）、`:143-148`（model `doOnNext(aggregator::append)` + `doFinally` → `getLLMResponseAttributes`）、`:190`（tool `doOnSuccess` → `getToolResponseAttributes`） | `SpanGuard` 新增 `finalizer: Option<Box<dyn FnOnce() -> Attributes + Send>>`（等价 Java `doFinally`/`doOnSuccess`，drop 路径同样执行）。**agent**：`SpanStream` 从 `AgentEvent::AgentResult.fields["reply"]` 反序列化 `Msg`（core 写入点 `react_agent.rs:864`）→ `get_agent_response_attributes`。**model**：`on_model_call` 用 `AggregatingModel` 包装 `input.model`，逐 chunk `StreamChatResponseAggregator::append` → `get_llm_response_attributes`（`on_model_call` 的事件流只有 `ModelCallEnd`，文本 delta 走 `ctx.delta_tx`，故必须 tee provider 流；每次 `stream()` 调用重置聚合器，重试不叠加）。**tool**：`ActingInput.results`（`ActingResultsHandle`）快照按 `tool_call_id` 匹配 → `get_tool_response_attributes`（事件流只有 `ToolResultEnd` 的 state/metadata，无输出块） |
| ③ `callFormat` 的 format span | `TelemetryTracer.java:203-232`：span 名 `<FORMAT> <formatterTarget>`、请求侧 `getFormatRequestAttributes`、成功侧 `getFormatResponseAttributes`、`finally span.end()` | 新增 `TelemetryTracing::call_format(ctx: Option<&RuntimeContext>, formatter_type, msgs, closure)`：`FormatterConverter::get_formatter_target` 映射 target、`SpanKind::Internal`、`ctx` 提供父 span（`None`=根 span，对应 Java `Context.current()` 无上下文）；core 无 formatter 拦截点 → 宿主在格式化处调用（§4.2 第 6 条） |

- **顺带对齐**：agent span 名改为 `invoke_agent <agent name>`（Java `TelemetryTracer.java:96` `INVOKE_AGENT + " " + instance.getName()`），原先只有 `invoke_agent`。
- **未改动（记录在案）**：`gen_ai.provider.name` 仍取配置（默认 `unknown`）—— `TelemetryTracing` 只拿到 `Arc<dyn Model>`，Rust 无运行时反射（§6.3 ProviderNameConverter 行）。

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
5. **【P0/P1/P2 状态收口 — 2026-10-31】** ~~P1-1~~ / ~~P1-2~~ 于 2026-10-28 关闭（§4.1.0v）；~~P0-1~~ 于 2026-10-22 修复（§7.1）；~~P2-2~~ / ~~P2-3~~ / ~~P2-4~~ 于 2026-10-31 关闭（§10）；~~P2-1~~ 同日裁定为设计决定（不迁移）。
6. **【跨模块需求（本 crate 无法单独关闭）— core 缺口，2026-10-31 复核保留】** `format` span 只能以 `TelemetryTracing::call_format(ctx, formatter_type, msgs, closure)` 形式提供：`ExtensionPoint` 实为 **8 个变体，无 formatter 拦截点**（`crates/agentscope-core/src/middleware/middleware.rs:136-147`：OnAgent/OnReasoning/OnReasoningResult/OnActing/OnModelCall/OnSystemPrompt/OnPreCall/OnAgentStateReady），`agentscope_core::formatter` 也只有自由函数（`base_formatter.rs`/`media_utils.rs`/`json_schema.rs`/`response_format.rs`）、无 formatter trait。**宿主须在真正格式化处调用该包装器**，否则 `format {target}` span 不会产生（不阻塞其他 span）。Java 侧由 `Tracer.callFormat(formatter, msgs, supplier)` 在框架内自动包裹；若需框架级自动包裹，需 core 新增 formatter 钩子（不在本 crate 写入范围，已作回执「跨模块需求」）。
7. **【真实剩余（2026-10-29 复核，均为已在册项，非新缺口）】**
   - ~~`StudioClient` 无 HTTP 注入点（`studio/client.rs:22` 直接持 `reqwest::Client`）→ 网络路径不可离线测（§3.2）。~~ **已解决 2026-10-30（§9）**：`studio/http.rs` 注入点 + 11 个离线用例。
   - ~~`StudioUserAgent` 未与 core agent 循环集成（仅独立输入提供者，§3.2）。~~ **已解决 2026-10-30（§9）**：`drive_round` / `drive_round_with`（泛型 `CallableAgent`）。
   - ~~`TelemetryTracing` 未在 `StudioManager::initialize*` 自动装配（第 4 条）。~~ **已裁定 2026-10-31**：设计决定（不迁移），宿主显式 `MiddlewareChain::add`。
   - `format` span 需宿主显式调 `call_format`（第 6 条）—— **跨模块需求**（core 无 formatter 钩子），保留。
   - ~~非 2xx 不重试（Java 会重试，§6.4 P2-1）。~~ **已裁定 2026-10-31**：设计决定（不迁移），`is_retryable()` 文档已改正。
   - ~~`OtlpHttpExporter` 无自定义 header（§6.4 P2-2）。~~ **已解决 2026-10-31（§10）**：`with_header`/`with_headers`。
   - ~~属性键缺 5 个（Java 44 → Rust 39，§6.4 P2-3）。~~ **已解决 2026-10-31（§10）**：44/44。
   - `gen_ai.provider.name` 恒 `unknown`（`ProviderNameConverter` 未被 `TelemetryTracing` 调用；Rust 无运行时反射）—— 设计决定，用 `with_provider_name` 覆盖。
   - ~~多模态 `block_to_part` 对 Image/Audio/Video 返回 None（Java 亦 TODO）。~~ **已裁定 2026-10-31**：设计决定（不迁移），跟 Java TODO（§3.3）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-studio` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-studio` / `cargo test -p agentscope-extensions-studio` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-studio/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **方式**：纯静态核对（逐文件读源码 + `rg` 交叉检索 + PowerShell 行数/注解计数）。**未执行** `cargo build` / `cargo test`，**未修改任何 Rust 代码**；本节及上文所有"已更正"均为本 tracker 文档内的定点修订。
- **核对范围**：Java `agentscope-extensions/agentscope-extensions-studio`（主/测全部 `.java`）↔ Rust `crates/agentscope-extensions-studio`（全部 18 个 `.rs`）+ 跨仓检索 `agentscope-core` / `agentscope-extensions-protocol` / `agentscope-extensions-controlplane` / `agentscope-harness`（用于确认事件字段契约与依赖）。
- **计数口径**：行数 = `[System.IO.File]::ReadAllLines().Length`（物理行，含空行与注释）；**非空行**另计（剔除空字符串行）；测试数 = 源码中 `#[test]` 与 `#[tokio::test]` 注解逐条计数（非 `cargo test` 实际执行数）。

### 6.2 实测统计
| 项 | Java | Rust |
|---|---|---|
| 主代码文件 | 28 | 18（`src/` 全量 `.rs`）|
| 主代码行（物理）| **4305** | **5048** |
| 主代码行（非空）| **3738** | **4578** |
| 测试文件 | 10 | 8（含测试的文件；`error.rs`/`lib.rs`/`mod.rs`/`user_agent.rs`/`message_hook.rs`/`model.rs` 等无内联测试）|
| 测试行（物理）| **1837** | — |
| 测试行（非空）| **1554** | — |
| 测试用例数 | **91** 个 `@Test` 方法 | **92**（62 `#[test]` + 30 `#[tokio::test]`；2026-10-31 静态计数，待主智能体 `cargo test` 验证。历史：2026-10-28 实测 70 passed）|
| 顶层类/类型 | 28 文件 / **44 个类型声明**（含嵌套类与接口）| 18 文件 / 与 Java 一一映射（见 §6.3）|
| 属性键常量 | `GenAiIncubatingAttributes` 38 个 `AttributeKey` + `AgentScopeIncubatingAttributes` 6 个 | **38 + 6**（2026-10-31 已补齐，§10）|
| OTLP 部件模型 | `telemetry/model` **13** 个类 | `telemetry::model` **13** 个 `pub` 类型（一一对应）|

Rust 逐文件测试分布（`#[test]` / `#[tokio::test]` / 合计；**2026-10-28 实测**）：
`config.rs` 3/0/3、`message_hook.rs` 3/0/3、`pojo.rs` 5/0/5、`socket_io.rs` 14/0/14、`websocket.rs` 6/5/11、`aggregator.rs` 3/0/3、`exporter.rs` 8/2/10、`extractors.rs` 8/0/8、`tracing.rs` 2/11/13 → **合计 54/16/70**（原记 `socket_io` 12、`tracing` 7 且漏 `message_hook` 3，合计 59 为 2026-10-22 静态计数）。
> **2026-10-31 静态重算（§10，待主智能体验证）**：`exporter.rs` 11/2/13、`extractors.rs` 9/0/9、`gen_ai_attributes.rs` 2/0/2、`agent_scope_attributes.rs` 2/0/2、`user_agent.rs` 1/3/4（其余文件不变）→ **合计 62/30/92**。

### 6.3 对齐结论表
| Java 类 / 包 | Rust 对应 | 行为覆盖 | 结论 |
|---|---|---|---|
| `studio.StudioConfig` | `studio::StudioConfig` + `StudioConfigBuilder` | 默认值/尾部 `/` trim/`tracingUrl` 推导全对齐；Java 无 trim，Rust 多做一次 trim（无害）| ✅ 对齐 |
| `studio.StudioClient` | `studio::StudioClient` | 三方法 + 重试 + shutdown 均在；**重试谓词不同**（Java 含非 2xx，Rust 不含）| ⚠️ 部分（§6.4 P2-1）|
| `studio.StudioWebSocketClient` | `studio::StudioWebSocketClient` + `studio::socket_io` | 握手/入房/ping-pong/重连/`forwardUserInput` 解码全在；鉴权键已对齐 Java `run_id`（旧 `runId` 仅 opt-in，§4.1.0v）| ✅ 对齐 |
| `studio.StudioWebSocketClient.UserInputData` | `studio::UserInputData` | blocksInput + structuredInput 结构一致 | ✅ 对齐 |
| `studio.StudioManager` | `studio::StudioManager` | 全局单例/init builder/getters/shutdown 在；**Java 的 `TracerRegistry.register` 自动装配不迁移**（core 无 `TracerRegistry`，§4.2 第 4 条）| ✅ 设计决定（不迁移，2026-10-31）|
| `studio.StudioMessageHook` | `studio::StudioMessageHook` | Middleware 形态在，**终态消息捕获键已修正**（`reply` 优先，`message` 回退，§7.1）| ✅ 对齐 |
| `studio.StudioUserAgent` | `studio::StudioUserAgent` | 双模式 + 失败回退 + 30min 超时在；非 `AgentBase` 子类（core 无此类，§3.2 已登记）| ✅ 设计等价 |
| `studio.pojo.*`（4 个）| `studio::pojo.*`（4 个）| 字段名/`run_dir`/`camelCase`/`NON_NULL` 跳过逐条对齐 | ✅ 对齐 |
| `telemetry.GenAiIncubatingAttributes` | `telemetry::gen_ai_attributes` | 38/38（含 `gen_ai.tool.type` + 3 个 usage 键）| ✅ 对齐（2026-10-31）|
| `telemetry.AgentScopeIncubatingAttributes` | `telemetry::agent_scope_attributes` | 6/6（含 `agentscope.usage.tool_use_prompt_tokens`）| ✅ 对齐（2026-10-31）|
| `telemetry.AttributesExtractors` | `telemetry::extractors`（模块，非同名类型）| 10 个 `get_*` 函数一一对应；`GenerateOptions` 用本地投影绕过 core `Serialize` 缺口 | ✅ 对齐 |
| `…FormatterConverter` | `telemetry::FormatterConverter` | 映射表一致 | ✅ 对齐 |
| `…ProviderNameConverter` | `telemetry::ProviderNameConverter` | 函数在，但 `TelemetryTracing` 未调用（provider 恒为 `unknown`）| ⚠️ 部分（§6.4 P1-1）|
| `telemetry.StreamChatResponseAggregator` | `telemetry::StreamChatResponseAggregator` | 聚合逻辑在；**已接入 `on_model_call`**（`AggregatingModel` tee 模型流）| ✅ 对齐 |
| `telemetry.TelemetryTracer` | **`telemetry::TelemetryTracing`** | span 生命周期/父子/RAII 达标；`agentscope.function.name` 四类 span、三个响应侧抽取器、`call_format` format span 均已落地（2026-10-28）；provider 名仍取配置（Rust 无反射）| ✅ 对齐（§4.1.0v）|
| `telemetry.model.*`（13 类）| `telemetry::model`（13 类型）| 一一对应，`type` 判别 + `mime_type`/`file_id`/`finish_reason` 字段名对齐 | ✅ 对齐 |
| OTLP 导出（`exporter-otlp-http`）| `telemetry::exporter`（`OtlpEncoder` + `OtlpHttpExporter`）| OTLP/HTTP JSON 编码 + POST 实现；**未引入 `opentelemetry`（已核实）**；自定义 header 已支持（`with_header`/`with_headers`）| ✅ 设计等价（2026-10-31）|

**核实结论（逐条 grep）**：
- 「不引入 `opentelemetry`」**属实** —— `crates/agentscope-extensions-studio/Cargo.toml` 无 `opentelemetry*` 依赖；根 `Cargo.toml` 的 `[workspace.dependencies]` 亦无；crate 内 `opentelemetry` 仅出现在 6 处**文档注释**中。
- 「Socket.IO 真实握手/鉴权/重连」**属实但有一处键名分歧** —— `studio/socket_io.rs` 确有 `SocketIoTransport`（tokio-tungstenite 0.29 真连）、`SocketIoProtocol` 状态机、`parse_engine_io_open`/`namespace_connect_frame`/`is_ping`/`reconnect_delay`；`Cargo.toml` 的 `tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots）与 CHANNEL crate 同版本同 features。
- 「§3.3 / §4.1 的 `TelemetryTracer` 接线缺口」**仍存在** —— `TelemetryTracing` 全仓引用仅限 `telemetry/tracing.rs`、`telemetry/mod.rs` 与其单测；`StudioManager::initialize*` 未装配。

### 6.4 新发现缺口
| 编号 | 优先级 | 缺口 | 证据 | 影响 |
|---|---|---|---|---|
| P0-1 | **P0** | `StudioMessageHook` 捕获终态消息的键名错误，转发**恒不触发** | 已于 2026-10-22 修复（§7.1）：优先读 `fields["reply"]`，`fields["message"]` 仅作第三方回退 | — |
| P1-1 | ~~P1~~ **已关闭（2026-10-28）** | `TelemetryTracer` 三处 Java 语义未覆盖：①span 缺 `agentscope.function.name`；②响应侧属性 + `StreamChatResponseAggregator` 未接入；③缺 `callFormat` 的 `format {target}` span | 原证据：`tracing.rs:32-36` import 只含 3 个 request 侧抽取器；`AGENTSCOPE_FUNCTION_NAME` 仅定义处出现；`get_*_response_attributes` 只被 `mod.rs` re-export | 已修复，依据与用例见 §4.1.0v；残留设计取舍：`gen_ai.provider.name` 仍取配置（Rust 无反射）、`format` span 需宿主调用 `call_format` |
| P1-2 | ~~P1~~ **已关闭（2026-10-28）** | Socket.IO 鉴权载荷键名分歧 | 原证据：Java `StudioWebSocketClient.java:98` `auth.put("run_id", ...)`；Rust 曾发 `json!({ "runId": ... })` | 已修复：`auth_payload()` 默认 `run_id`，旧 `runId` 经 `with_legacy_auth_key()` 可选叠加；用例 `handshake_auth_uses_the_java_snake_case_run_id_key` / `the_legacy_camel_case_auth_key_is_opt_in`（§4.1.0v）|
| P2-1 | **P2** | HTTP 错误重试语义分歧 | Java `StudioClient.java:116/161/209` 把非 2xx 包成 `IOException` → 被 `Retry.filter(t -> t instanceof IOException)` 命中**会重试**；Rust `client.rs` 仅 `StudioError::Transport` 可重试，`StudioError::Http` 不重试 | **裁定：已完成（设计决定，不迁移），2026-10-31（§10）**。Rust 保留「传输失败 / HTTP 状态」两类可区分错误；盲重试 4xx 会重复 `pushMessage`/`requestUserInput`。已有断言 `http_status_errors_are_not_retried` 固化该边界；`error.rs::is_retryable` 文档已改正（原文误称「镜像 Java」）|
| P2-2 | **P2** | OTLP exporter 无自定义 header | Java `TelemetryTracer.Builder.addHeader/headers`（注释明写 "for authentication (e.g., Langfuse)"）| **已关闭（2026-10-31）**：`OtlpHttpExporter::with_header` / `with_headers` / `headers()`（同名覆盖 = Java `HashMap.put`），export 时逐个下发；用例 `otlp_headers_replace_same_named_entries_like_java_add_header` / `with_headers_replaces_the_whole_header_set_like_java_headers` / `an_exporter_from_config_sends_no_extra_headers` |
| P2-3 | **P2** | 属性键常量缺 5 个（Java 44 → Rust 39）| 缺 `gen_ai.tool.type`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.usage.cache_read.input_tokens`、`gen_ai.usage.reasoning.output_tokens`、`agentscope.usage.tool_use_prompt_tokens` | **已关闭（2026-10-31）**：5 个常量已补（38 + 6 = Java 44）；`get_llm_response_attributes` 同时接入 4 个 usage 属性（Java `AttributesExtractors.java:229-243` 无零值过滤）。用例 `the_key_set_matches_the_java_class_exactly`（gen_ai/agent_scope 各一）、`the_previously_missing_keys_match_the_java_literals`、`llm_response_attributes_record_every_usage_metric` |
| P2-4 | **P2** | `StudioUserAgent` 缺 Java 的 name 非空校验 | Java `StudioUserAgent.Builder.build()`（`StudioUserAgent.java:327-331`）对空名抛 `IllegalArgumentException("Agent name cannot be null or empty")` | **已关闭（2026-10-31）**：`build()` 改返回 `StudioResult<StudioUserAgent>`，blank name 返 `StudioError::InvalidArgument`（新增变体，带同一文案）。用例 `blank_names_are_rejected_like_java`（空/空白/制表符三种）|

### 6.5 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | Java 规模「主代码 28 文件 / 4305 行；测试 10 文件 / 1837 行」 | 数值**正确**（物理行口径）；补注非空行 3738/1554 与 `@Test` 91 个，并说明与汇总 tracker 3700/1452 的口径差异 |
| 2 | §1 模块概览 | Rust 规模「18 个 `.rs` / 5048 行 / 59 个内联测试（46+13）」 | **正确**；补注非空行 4578 |
| 3 | §1 当前状态 | 「见 §4.2.4」 | 交叉引用错误，实际为 **§4.2 第 4 条** |
| 4 | §2.0 `StudioWebSocketClient` 行 | 未提鉴权键 | 补注 Rust=`runId` vs Java=`run_id`（P1-2）|
| 5 | §2.0 `StudioClient` 行 | 「仅 IO/网络错误重试（**对齐 Java 仅重试 IOException**）」| 断言**不成立**：Java 的非 2xx 也是 IOException → 也会重试。改为标注语义分歧（P2-1）|
| 6 | §2.0 `StudioMessageHook` 行 | 「在事件流上捕获终态消息并推送」| 补注捕获键不符导致恒不触发（P0-1）|
| 7 | §2.0 `AttributesExtractors` 行 | Rust 对应「`telemetry::AttributesExtractors`」| Rust **无该类型**，实际是模块 `telemetry::extractors`（一组 `get_*` 自由函数）|
| 8 | §2.0 `ProviderNameConverter` 行 | 「改为按模型名/`Model` 显式 provider 探测（见 §4.1）」| 实际按**模型类型名后缀**探测；且 `TelemetryTracing` 未调用它，provider 恒为 `unknown`（P1-1）|
| 9 | §2.0 `TelemetryTracer` 行 | Rust 对应「`telemetry::TelemetryTracer`」+「on_agent/on_reasoning/on_acting/on_model_call」| Rust **无 `TelemetryTracer` 类型**（实际 `telemetry::TelemetryTracing`）；拦截点**无 `on_reasoning`**，只有 on_agent/on_model_call/on_acting；缺 Java 的 format span |
| 10 | §2.0 `telemetry.model.*` 行 | 「（15 个类）」| Java `telemetry/model` 实为 **13 个类**（Rust 亦 13 个类型，一一对应）|
| 11 | §2.1 依赖映射 | 「`reqwest`（workspace **0.12**, json/rustls）」| 根 `[workspace.dependencies]` 为 **0.13** |
| 12 | §3.1 crate 骨架行 | 「共 15 个文件」| 现为 **18 个文件**（新增 socket_io.rs / exporter.rs / tracing.rs）|
| 13 | §3.1 `StudioClient` 行 | 「（对齐 Java 仅重试 IOException）」| 同 #5，标注分歧（P2-1）|
| 14 | §3.1 `StudioWebSocketClient` 行 | 「单测 `studio::websocket::tests`（**9**）」| 实际 **11**（6 `#[test]` + 5 `#[tokio::test]`）|
| 15 | §3.1 `StudioMessageHook` 行 | 「捕获终态消息 fire-and-forget 推送」| 补注捕获键不符 → 恒不触发（P0-1）；另 `tokio::spawn` 需 runtime |
| 16 | §3.1 telemetry model 行 | 「telemetry model 部件 ×**15**」| 实际 **13** |
| 17 | §3.1 `AttributesExtractors` 行 | 「单测 `telemetry::extractors::tests`（**7**）」| 实际 **8** |
| 18 | §3.2 Socket.IO 行 | 状态「**已完成**（2026-10-08）」| 实现完整，但存在鉴权键名分歧 → 降级为「**进行中**」，并新增待办行（§3.2）|
| 19 | §3.3 `TelemetryTracer` 行 | 状态「**已完成**（2026-10-21，`TelemetryTracing`）」| span 生命周期达标，但语义覆盖缺 3 项 → 降级为「**进行中**」，并列出待补 ①②③（P1-1）|
| 20 | §3.3 OTLP 导出行 | 「已完成（降级 exporter）」| 编码与「不引入 opentelemetry」**属实**；补记单测 **10** 个 + 缺自定义 header（P2-2）|
| 21 | §3.2 | 缺 `StudioMessageHook` 捕获键条目 | **新增**「`StudioMessageHook` 终态消息捕获键修正」行（待办）；2026-10-29 勘误：已修（`reply` 优先 + `message` 回退，3 用例） |
| 22 | §4.1 当前状态 | 「交付：`src/` **15 个文件**已接线并编译」| **18 个文件** |
| 23 | §4.1 当前状态 | 「状态：已完成（cargo build / cargo test 全绿）」| 补注：本轮**静态复核未编译**，build/test 结论沿用历史记录未经复验 |
| 24 | §4.1.0s | 「Socket.IO 帧层 11 个 …，OTLP 导出 **11** 个」| 导出实为 **10** 个（socket_io 12 + exporter 10 = 22，合计正确）|
| 25 | §4.1.0u 设计要点 | 「首版不含响应侧属性（`gen_ai.usage.*`）」| 表述偏窄：实际是**三个响应侧抽取器整体未接入** + 聚合器未接线 + 缺 `agentscope.function.name` + 缺 format span（P1-1）|
| 26 | §4.2 第 4 条 | 「【接线待办】」| 复核确认**仍存在**，补记证据（`TelemetryTracing` 引用范围）|
| 27 | §2.0 `GenAiIncubatingAttributes` 行 | 「OTel GenAI 语义约定 **1.37.0** 属性键」| Java 类注释明写「semantic conventions **1.41.0**」；Rust `gen_ai_attributes.rs` 模块文档也写 1.37.0 → 以 Java 为准应为 1.41.0（文档口径已更正，Rust 侧源码 2026-10-31 已改为 1.41.0，§10）|
| 28 | §2.1 依赖映射 | 「手写 `retry_with_backoff` 辅助」| Rust **无该函数**，退避循环内联在 `StudioClient::post_json` 中 |
| 29 | §1 当前状态 | 「见 §4.2.4」| 交叉引用错误，实际为 **§4.2 第 4 条** |
| 30 | §6.2 实测统计 | 「顶层类/类型 39 个声明」| 实为 **44** 个类型声明（含嵌套类/接口），已更正 |

> **未发现问题的声明（逐条核实为真）**：①「不引入 `opentelemetry`」；②「Socket.IO 真实握手/重连/ping-pong 已落地」；③「OTLP 降级导出为真实 reqwest POST」；④「59 个离线测试」；⑤「`GEN_AI_TOOL_TYPE` + 3 个 usage 键缺失」与「`AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS` 缺失」的计数；⑥「core 无 `Hook`/`TracerRegistry`/`AgentBase`」；⑦「`GenerateOptions` 未实现 `Serialize`」。


## 7. §4.3 P0 修复记录（2026-10-22，汇总方）

### 7.1 已修：Studio 消息转发恒不触发（§4.3 #5）

- **缺陷**：`StudioMessageHook::final_message` 读 `fields["message"]`，而 core 在 `react_agent.rs` 写的是 `fields["reply"]` → 提取恒为 `None`，转发逻辑从未执行。
- **修复**：优先读 `reply`，保留 `message` 作为第三方生产者的回退键。
- **用例**（3 条）：`reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`。
- **验证**：`cargo test -p agentscope-extensions-studio` → **62 passed / 0 failed**。

### 7.2 仍待办

- ~~`TelemetryTracer` 缺 `agentscope.function.name` span、三个响应侧抽取器 + `StreamChatResponseAggregator` 未接入~~ → **2026-10-28 已修，见 §4.1.0v**。
- ~~Socket.IO 鉴权键 Rust `runId` vs Java `run_id`~~ → **2026-10-28 已修，见 §4.1.0v**。
- **2026-10-31 已了结**：P2-2/P2-3/P2-4 已修复（§10）；P2-1 裁定为设计决定（不迁移）。
- **仍开放（非本 crate 可解决）**：`format` span 的宿主调用点（§4.2 第 6 条，core 无 formatter 钩子 → 跨模块需求）；多模态抽取跟随 Java TODO（§3.3）。

## 8. 2026-10-28 执行记录（子智能体：关闭 P1×2）

**改动文件**（仅本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `crates/agentscope-extensions-studio/src/studio/socket_io.rs` | `AUTH_RUN_ID_KEY`/`LEGACY_AUTH_RUN_ID_KEY`/`auth_payload()`；`SocketIoTransport::new` 默认发 `run_id`；`with_legacy_auth_key()` 兼容回退；+2 测试 |
| `crates/agentscope-extensions-studio/src/telemetry/tracing.rs` | `agentscope.function.name`（agent/model/tool/format）；`SpanGuard::finalizer` + 三个响应侧抽取器接入；`AggregatingModel`（tee 模型流）+ `StreamChatResponseAggregator`；`call_format()` format span；agent span 名 `invoke_agent <name>`；+6 测试，1 个端到端用例加强 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§2.0/§3.2/§3.3/§4.1/§4.2/§6.2/§6.3/§6.4/§7.2 状态与计数；新增 §4.1.0v 与本节 |

**Java 依据**：`StudioWebSocketClient.java:96-99`（auth `run_id`）；`TelemetryTracer.java:96/104/110`（agent span 名、function.name、响应侧）、`:123-148`（model function.name + aggregator + doFinally）、`:169/190`（tool）、`:203-232`（callFormat）；`AttributesExtractors.java:350`（`getFunctionName`）；`StudioWebSocketClient.java` 全模块 grep 确认握手鉴权只有 `run_id` 一个键。

**命令与结果（实测）**
- `cargo build -p agentscope-extensions-studio` → EXIT=0，本 crate 0 warning。
- `cargo test -p agentscope-extensions-studio` → **70 passed / 0 failed**（基线 62，+8）。
- `cargo fmt -p agentscope-extensions-studio -- --check` → 干净。

**反向验证**：5 项回退逐一确认对应用例 FAILED 后恢复（明细见 §4.1.0v 表）。

**未完成**：`StudioManager` 自动装配（§4.2 #4）；`format` span 需宿主调用（§4.2 #6）；P2-1/P2-2/P2-3/P2-4。

## 9. 2026-10-30 执行记录 — StudioClient 传输注入 / UserAgent 集成评估

**任务与 Java 依据**
- 任务 1（P1）：`StudioClient` 加 HTTP 传输注入点，使网络路径可离线测。Java 依据 `StudioClient.java`：`registerRun` → `POST {base}/trpc/registerRun`、`pushMessage` → `POST {base}/trpc/pushMessage`、`requestUserInput` → `POST {base}/trpc/requestUserInput`；三者均 `RequestBody.create(json, MediaType.get("application/json; charset=utf-8"))`，**无鉴权头**；`executeWithRetry` = `Retry.backoff(config.getMaxRetries(), 100ms).maxBackoff(5s).filter(IOException)`。
- 任务 2（P1）：`StudioUserAgent` 与 core agent 循环集成。Java `StudioUserAgent extends AgentBase`（`getInputFromStudio` = `requestUserInput` + `waitForInput(timeout)`，`onErrorResume` 回退终端）。

**改动文件**（仅本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `src/studio/http.rs`（**新增**） | `StudioHttpRequest`/`StudioHttpResponse`/`trait StudioHttpTransport`（`async fn execute(&self, StudioHttpRequest) -> StudioResult<StudioHttpResponse>`）+ `ReqwestStudioTransport`（生产实现，10s connect / 30s request，行为与旧内联实现一致）+ `JSON_CONTENT_TYPE`/`HTTP_POST` + `#[cfg(test)] testing::{ScriptedTransport, Outcome}` |
| `src/studio/client.rs` | `http: reqwest::Client` → `transport: Arc<dyn StudioHttpTransport>`；新增 `with_transport()`；`post_json` 改为「序列化 → 构建 `StudioHttpRequest` → 重试循环（`send_once`）」；`send_once` 负责非 2xx → `StudioError::Http`；+10 测试 |
| `src/studio/mod.rs` | `pub mod http` + re-export；新增 `StudioManager::initialize_with_transports(config, http, ws)`（`initialize_with_transport` 改为委托，语义不变） |
| `src/studio/user_agent.rs` | 新增 `drive_round<A: CallableAgent>` / `drive_round_with<A: CallableAgent>(agent, Msg)`；+3 测试 |
| `src/error.rs` | 新增 `StudioError::Agent(String)`（不可重试，`is_retryable` 仍仅 `Transport`） |
| `src/lib.rs` | re-export `StudioHttpRequest/Response/Transport/ReqwestStudioTransport`；模块文档补一条 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§3.2/§4.2 #7 + 本节 |

**新增测试（+13，均离线；transport 为脚本化）**
- `studio::client::tests`（10，全部通过 `StudioClient` 公开方法）：`register_run_posts_java_shaped_request`（方法/路径/`Content-Type`/无鉴权头/body 六字段/`yyyy-MM-dd HH:mm:ss.SSS` 时间戳）、`push_message_posts_java_shaped_request`、`push_message_falls_back_to_reply_id_for_unnamed_messages`、`request_user_input_posts_java_shaped_request`（返回值 == body `requestId`、无 `structuredInput`）、`request_user_input_forwards_structured_schema`、`success_statuses_are_decoded_as_ok`（200/204）、`http_status_errors_are_not_retried`（§4.2 P2-1 现状锁定：500 仅 1 次尝试）、`transport_errors_are_retried_then_succeed`（重试请求体与 URL 不变）、`transport_errors_stop_after_max_retries`（1 + max_retries 次）、`role_labels_match_java`
- `studio::user_agent::tests`（3）：`drive_round_with_feeds_a_core_agent_loop`（真实 `ReActAgent` + StubModel，断言回复与 `/trpc/pushMessage` 的用户消息）、`drive_round_with_tolerates_a_failed_push`（500 不中断本轮）、`drive_round_with_surfaces_agent_errors`（→ `StudioError::Agent`）

**命令与结果（实测）**
- `cargo test -p agentscope-extensions-studio -j 2` → **EXIT=0，83 passed / 0 failed / 0 ignored**（doc-tests 0 passed；基线 70）
- `cargo clippy -p agentscope-extensions-studio --all-targets` → EXIT=0；本 crate 仅 2 条**既有** `clippy::type_complexity`（`telemetry/tracing.rs:510/520`），新代码 0 告警
- `cargo fmt -p agentscope-extensions-studio -- --check` → 干净

**反向验证（2 组，均回退 → FAILED → 恢复 → 全绿）**
| # | 回退内容 | 结果 |
|---|---|---|
| 1 | `client.rs::send_once` 的 `if !response.is_success()` → `if false && !response.is_success()`（非 2xx 不再报错） | `http_status_errors_are_not_retried` FAILED（`test result: FAILED. 0 passed; 1 failed`） |
| 2 | `user_agent.rs::drive_round_with` 推送失败改为 `client.push_message(...).await?`（不再容忍） | `drive_round_with_tolerates_a_failed_push` FAILED |

**设计要点与诚实边界**
- 传输契约：非 2xx 以 `Ok(StudioHttpResponse)` 返回、由 `StudioClient` 映射为 `StudioError::Http`；网络失败为 `StudioError::Transport`（唯一可重试变体）；传输层自身不重试（与 `judge` crate 的 `JudgeHttpTransport` 同构）。
- **只锁定不改语义**：Java 把非 2xx 包成 `IOException` 会重试，Rust 不重试 —— 该分歧仍归 §4.2 P2-1，本轮用测试把现状固化（`http_status_errors_are_not_retried`），**未顺手修改**。
- `StudioManager` 新增 `initialize_with_transports(config, http, ws)`；`initialize_with_transport` 语义不变（HTTP 侧 reqwest）。
- UserAgent 集成**未改 core/harness**：`drive_round*` 以 core `CallableAgent`（`agentscope_core::agent::agent_traits`）为泛型边界，`ReActAgent` 与 `HarnessAgent` 直接可用；用户消息先推 Studio（失败仅告警），agent 错误映射为 `StudioError::Agent`（Java 的 `getInputFromStudio` 失败才回退终端，agent 错误不吞）。

**未完成项与原因**
1. `StudioUserAgent` 自动多轮循环（`loop { drive_round }` + 退出条件 + 中断传播）→ 需宿主决定终止/中断策略（Java `AgentBase` 子类化后由框架驱动，本 crate 无同构宿主），不臆造。
2. `StudioManager` 自动装配 `TelemetryTracing`（§4.2 #4）、`format` span 宿主调用（§4.2 #6）—— 本轮未涉及。
3. P2-1（HTTP 错误重试语义分歧，本轮按现状锁定）、P2-2（OTLP 自定义 header）、P2-3（属性键缺 5）、P2-4（`StudioUserAgent` name 校验）仍开放。**→ 后续：2026-10-31 已全部收口（§10）。**

## 10. 2026-10-31 执行记录 — P2 收口 + Java 增量提交核对

**任务**：把 §6.4 未关闭条目与 §4.2 遗留项收敛为「落代码 + 离线测试」或「已完成（设计决定，不迁移）+ 源码级证据」。只改本 crate 与本 tracker。

**处置总表**
| 条目 | 处置 | 依据 / 位置 |
|---|---|---|
| P2-2 OTLP 自定义 header | **已关闭（代码+测试）** | `exporter.rs`：`OtlpHttpExporter::with_header`（同名覆盖 = Java `HashMap.put`）/ `with_headers`（整体替换 = Java `Builder.headers`）/ `headers()`；`export` 逐个下发；Debug 只显个数 |
| P2-3 属性键缺 5 个 | **已关闭（代码+测试）** | `gen_ai_attributes.rs` +4 键（38）、`agent_scope_attributes.rs` +1 键（6）；`extractors::get_llm_response_attributes` 接入 4 个 usage 属性（Java `AttributesExtractors.java:229-243`，无零值过滤）|
| P2-4 `StudioUserAgent` name 校验 | **已关闭（代码+测试）** | `user_agent.rs::StudioUserAgentBuilder::build() -> StudioResult<StudioUserAgent>`，blank → `StudioError::InvalidArgument`；对应 Java `StudioUserAgent.java:327-331` |
| P2-1 HTTP 错误重试语义 | **已完成（设计决定，不迁移）** | 保留 `StudioError::Http` 不可重试（`is_retryable` 只认 `Transport`）；`error.rs` 文档已改正（原文误称「镜像 Java」）；既有断言 `http_status_errors_are_not_retried` 不动 |
| §4.2 #4 `StudioManager` 自动装配 | **已完成（设计决定，不迁移）** | core 无 `TracerRegistry`（`rg` 零命中），`MiddlewareChain::add` 是唯一挂载入口；自动装配还需产品级裁定（共享实例 vs 每 agent 一条）|
| §4.2 #6 `format` span 自动接线 | **跨模块需求（保留）** | `ExtensionPoint` 8 变体无 formatter 钩子（`middleware/middleware.rs:136-147`）；`core::formatter` 只有自由函数无 formatter trait |
| 多模态 `block_to_part` | **已完成（设计决定，不迁移）** | Java 自身 4 处 `// TODO: support multi modal content`（`AttributesExtractors.java:400/461/503/548`）|
| 裁决 B5 `TelemetryTracer` seam | **已完成（既有实现即最小适配）** | `TelemetryTracing::new(Arc<dyn TraceExporter>)`；映射见 §2.0 `TelemetryTracer` 行；离线由 `RecordingTraceExporter` 覆盖（`tracing.rs` 12 个用例）|

**改动文件**（均在本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `src/telemetry/gen_ai_attributes.rs` | 模块文档 1.37.0 → 1.41.0（对齐 Java 类注释且与 `opentelemetry-bom` 版本解耦）；+4 常量；+2 测试（含「Rust 常量 vs Java 字面量」双向交叉校验）|
| `src/telemetry/agent_scope_attributes.rs` | +`AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS`；+2 测试 |
| `src/telemetry/extractors.rs` | `get_llm_response_attributes` 接入 4 个 usage 属性；+1 测试 |
| `src/telemetry/exporter.rs` | `headers` 字段 + `with_header`/`with_headers`/`headers()`，export 时下发；Debug 不泄密；+3 测试 |
| `src/error.rs` | 新增 `StudioError::InvalidArgument`；`is_retryable` 文档改正为「与 Java 的偏差」 |
| `src/studio/user_agent.rs` | `build()` 返回 `StudioResult` + 空名校验；3 处调用点补 `expect`；+1 测试 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§2.0/§2.1/§3.3/§4.2/§6.4/§7.2 与本节 |

**新增测试（9 个，均为离线）**
- `telemetry::gen_ai_attributes::tests::the_previously_missing_keys_match_the_java_literals`
- `telemetry::gen_ai_attributes::tests::the_key_set_matches_the_java_class_exactly`
- `telemetry::agent_scope_attributes::tests::the_key_set_matches_the_java_class_exactly`
- `telemetry::agent_scope_attributes::tests::the_previously_missing_usage_key_matches_the_java_literal`
- `telemetry::extractors::tests::llm_response_attributes_record_every_usage_metric`
- `telemetry::exporter::tests::otlp_headers_replace_same_named_entries_like_java_add_header`
- `telemetry::exporter::tests::with_headers_replaces_the_whole_header_set_like_java_headers`
- `telemetry::exporter::tests::an_exporter_from_config_sends_no_extra_headers`
- `studio::user_agent::tests::blank_names_are_rejected_like_java`

**验证状态**：本轮按子智能体约束**未执行** `cargo build` / `cargo test` / `cargo fmt` / 反向验证（由主智能体统一验证）。静态自检口径：19 个 `.rs` / 6998 行（非空 6363）/ 92 个内联测试（62 `#[test]` + 30 `#[tokio::test]`）。
**建议的反向验证点**（主智能体执行时）：①去掉 4 个 usage 属性 → `llm_response_attributes_record_every_usage_metric` 应 FAILED；②`with_header` 不做同名覆盖 → `otlp_headers_replace_same_named_entries_like_java_add_header` 应 FAILED；③`with_headers` 改为合并而非替换 → `with_headers_replaces_the_whole_header_set_like_java_headers` 应 FAILED；④`build()` 去掉空名校验 → `blank_names_are_rejected_like_java` 应 FAILED。

**Java 增量提交 `8690ab3f5` 核对（`chore(deps): bump opentelemetry-bom 1.66.0→1.67.0`）**
- 影响面：仅 `agentscope-dependencies-bom/pom.xml`（`git show --stat` = 1 file, 2 insertions / 2 deletions），无源码、无 attribute key 变更。
- Rust 侧不依赖 `opentelemetry`/`opentelemetry-otlp`（§4.2 #1），键为字面量字符串；Java 类注释固定的 **semantic conventions 1.41.0** 与 SDK BOM 版本无关 ⇒ **无需动作**。
- 顺带核对出的既有语义差异（已裁定，非新增缺口）：Java `OtlpHttpSpanExporter` 默认 protobuf，本 crate 发 OTLP/HTTP **JSON**；协议允许两种编码，属 §4.2 #1 降级设计的一部分。
