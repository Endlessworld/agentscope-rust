# STUDIO 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **STUDIO** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-studio\`
> **Java 子模块**：(鍚?core.tracing.telemetry) studio/tracing
> **Rust 目标**：`crates/agentscope-extensions-studio/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-studio` |
| Java 子模块 | (鍚?core.tracing.telemetry) studio/tracing |
| Java 规模（初测） | 主代码 ~3700 行 / 测试 ~1452 行 |
| Rust crate | `crates/agentscope-extensions-studio` |
| 迁移波次 | C |
| 当前状态 | 已完成（build/test 全绿；**52 个离线单测**；Socket.IO 真实传输 + OTLP 降级导出已落地）|

## 2. Java 清单与依赖映射

> Java 主包 `io.agentscope.core.studio`（HTTP/WS/管理）与 `io.agentscope.core.tracing.telemetry`（OTel GenAI 遥测）。

### 2.0 子模块清单
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `studio.StudioConfig` | 连接配置（url/tracingUrl/project/runName/runId/maxRetries/reconnect*）| `studio::StudioConfig` + `StudioConfigBuilder` | 默认值对齐：url=`http://localhost:3000`、project=`UnnamedProject`、runName=`run_<ts>`、runId=UUID、maxRetries=3、reconnectAttempts=3、delay=1s、maxDelay=5s；tracingUrl 默认 `studioUrl + /v1/traces` |
| `studio.StudioClient` | OkHttp HTTP 客户端（registerRun/pushMessage/requestUserInput + 重试 + shutdown）| `studio::StudioClient` | reqwest + 指数退避重试（仅 IO/网络错误重试，`max_retries`，base 100ms，max 5s）；时间戳 `yyyy-MM-dd HH:mm:ss.SSS` |
| `studio.StudioWebSocketClient` | Socket.IO 连接 `/python` namespace，监听 `forwardUserInput`，`waitForInput` 等待 Mono | `studio::StudioWebSocketClient` + `studio::socket_io` | `StudioTransport` trait（可插拔）+ `PendingInputRegistry`（`HashMap<requestId, oneshot>`）；**`SocketIoTransport` 为真实实现**（tokio-tungstenite 0.29 + 自实现 Engine.IO v4 握手/`40` 入房/ping-pong/指数退避重连）；帧解析为纯函数 |
| `studio.StudioWebSocketClient.UserInputData` | 用户输入载荷（blocksInput + structuredInput）| `studio::UserInputData` | |
| `studio.StudioManager` | 全局静态单例（config/client/wsClient + init builder + shutdown + TracerRegistry 注册）| `studio::StudioManager` | Rust 用 `OnceLock<RwLock<Option<...>>>` 全局；不依赖 core `TracerRegistry`（core 无此类型，见 §4.2） |
| `studio.StudioMessageHook` | `Hook`，拦截 `PostCallEvent` 把最终消息推给 Studio | `studio::StudioMessageHook` | Rust 无 `Hook`（core `hook` 为设计决定不迁移）；改为 core `Middleware`：`on_agent` 包裹后在事件流上捕获终态消息并推送（fire-and-forget） |
| `studio.StudioUserAgent` | 用户代理 Agent（终端 / Studio 双模式）| `studio::StudioUserAgent` | 输出 Msg（USER 角色），Studio 模式失败回退终端 |
| `studio.pojo.RegisterRunRequest` | registerRun 请求体 | `studio::pojo::RegisterRunRequest` | 字段：id/project/name/timestamp/pid/status/run_dir |
| `studio.pojo.PushMessageRequest` | pushMessage 请求体 | `studio::pojo::PushMessageRequest` | 字段：runId/replyId/name/role/msg |
| `studio.pojo.RequestUserInputRequest` | requestUserInput 请求体 | `studio::pojo::RequestUserInputRequest` | 字段：requestId/runId/agentId/agentName/structuredInput |
| `studio.pojo.UserInputMetadata` | 用户输入元数据（source/requestId/structuredInput）| `studio::pojo::UserInputMetadata` | `@JsonInclude(NON_NULL)`；source 默认 `studio` |
| `telemetry.GenAiIncubatingAttributes` | OTel GenAI 语义约定 1.37.0 属性键 | `telemetry::gen_ai_attributes` | 常量字符串键（`gen_ai.*`）+ operation/provider 取值常量 |
| `telemetry.AgentScopeIncubatingAttributes` | AgentScope 自定义属性键 | `telemetry::agent_scope_attributes` | `agentscope.function.*`、`agentscope.format.*` |
| `telemetry.AttributesExtractors` | 从 agent/model/tool/formatter 抽取 OTel 属性（577 行）| `telemetry::AttributesExtractors` | 纯函数；复用 core 的 Msg/ContentBlock/ChatResponse/ToolSchema/GenerateOptions |
| `telemetry.AttributesExtractors.FormatterConverter` | formatter 简单类名 → provider 名映射 | `telemetry::FormatterConverter` | DashScope/Gemini/OpenAI→dashscope；Anthropic→anthropic；其余 unknown |
| `telemetry.AttributesExtractors.ProviderNameConverter` | ChatModelBase 实例 → provider 名（按类继承链判断）| `telemetry::ProviderNameConverter` | Rust 无运行时类反射；改为按模型名/`Model` 显式 provider 探测（见 §4.1） |
| `telemetry.StreamChatResponseAggregator` | 流式 ChatResponse 聚合（text/thinking/tool_calls + usage max + finishReason）| `telemetry::StreamChatResponseAggregator` | 复用 core `ChatResponse`/`ChatUsage`/`ContentBlock` |
| `telemetry.TelemetryTracer` | `Tracer` 实现：agent/model/tool/format 四类 span + OTLP HTTP 导出 | `telemetry::TelemetryTracer` | 包装为 core `Middleware`（on_agent/on_reasoning/on_acting/on_model_call）；OTLP 导出已落地为降级 exporter（`telemetry::exporter`，见 §4.1.0s） |
| `telemetry.model.*`（15 个类）| OTel 消息部件模型（Text/Reasoning/ToolCallRequest/ToolCallResponse/Blob/File/Uri Part、Input/OutputMessage、Role、SystemInstructions、ToolDefinition）| `telemetry::model` | serde 结构；`type` 字段判别 + JSON 字段名对齐（`mime_type`/`file_id`/`finish_reason`）|

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp3 (OkHttpClient) | Studio HTTP | `reqwest`（workspace 0.12, json/rustls）| 仅写本 crate Cargo.toml；根 `[workspace.dependencies]` 已有 reqwest，直接用 `reqwest = { workspace = true }` |
| reactor.core (Mono/Flux/Sinks) | 响应式 | `tokio::sync::oneshot/mpsc` + `futures::Stream` | 单值→async fn；等待→oneshot |
| socket.io-client（java） | Studio WS | `tokio-tungstenite = 0.29`（本 crate 直声明，default-features=false + connect/handshake/rustls-tls-webpki-roots）+ 本 crate 自实现 Engine.IO v4/Socket.IO 帧 | 已落地 `studio::socket_io`（见 §4.1.0s）；仅写本 crate `Cargo.toml`，未改根 `[workspace.dependencies]` |
| com.fasterxml.jackson（JSON） | 序列化 | `serde` / `serde_json` | 已建 |
| org.json (JSONArray/JSONObject) | WS 事件解析 | `serde_json::Value` | |
| reactor.util.retry.Retry | 重试退避 | 手写 `retry_with_backoff` 辅助 | base 100ms → max 5s，仅 IO/网络错误 |
| opentelemetry-api / sdk / exporter-otlp-http | OTel span + 导出 | **降级路径已落地**：自建 `Span` 模型 + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait（`OtlpHttpExporter` 真实 reqwest 实现）| 不引入 `opentelemetry`/`opentelemetry-otlp`（避免根 `[workspace.dependencies]` 变更）；见 §4.1.0s |
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
| crate 骨架 + lib.rs 模块树 | — | 已完成 | `lib.rs` 声明 `pub mod error/studio/telemetry` 并 re-export；`studio/`（client/config/pojo/message_hook/user_agent/websocket/mod）+ `telemetry/`（gen_ai_attributes/agent_scope_attributes/model/extractors/aggregator/mod），共 15 个文件 |
| `StudioConfig` + builder（默认值/校验/tracingUrl 推导）| StudioConfig | 已完成 | 默认值/tracingUrl 推导/尾部 `/` trim 已对齐 Java；单测 `studio::config::tests`（3） |
| studio POJO ×4（serde 字段名对齐 Java）| pojo/*.java | 已完成 | camelCase / `run_dir` / `structuredInput` / `NON_NULL` 跳过；单测 `studio::pojo::tests`（5） |
| `StudioClient`（registerRun/pushMessage/requestUserInput/shutdown + 重试）| StudioClient | 已完成 | reqwest；指数退避仅重试 `StudioError::Transport`（对齐 Java 仅重试 IOException）；网络路径未离线测（§3.2） |
| `StudioWebSocketClient` + `StudioTransport` trait + `PendingInputRegistry` | StudioWebSocketClient | 已完成 | 可插拔 trait + oneshot 注册表；`NoopStudioTransport` 供离线测；单测 `studio::websocket::tests`（9） |
| `UserInputData` + `parse_content_blocks` + `preprocess_tool_result_block` | StudioWebSocketClient | 已完成 | 纯函数；含 tool_result 字符串 output→text 块规范化；同上单测覆盖 |
| `StudioMessageHook`（Middleware on_agent 后推送）| StudioMessageHook | 已完成 | 实现 core `Middleware::on_agent`，捕获终态消息 fire-and-forget 推送，失败仅日志不中断 agent |
| `StudioManager` 全局单例（init builder/shutdown/getters）| StudioManager | 已完成 | `OnceLock<RwLock<Option<Installed>>>`；init/builder/getters/shutdown/initialize_with |
| `StudioUserAgent`（终端 + Studio 双模式）| StudioUserAgent | 已完成 | 独立输入提供者（core 无 `AgentBase`）；Studio 失败回退终端；`DEFAULT_INPUT_TIMEOUT=30min` |
| telemetry 属性常量 ×2 文件 | GenAi/AgentScopeIncubatingAttributes | 已完成 | 常量 `&'static str` 键 + operation/provider 取值常量 |
| telemetry model 部件 ×15 | telemetry/model/*.java | 已完成 | serde `type` 判别；字段名对齐（`mime_type`/`file_id`/`finish_reason`） |
| `StreamChatResponseAggregator` | 同名 | 已完成 | text/thinking/tool_calls 按 id 合并 + usage 取 max + finishReason；单测 `telemetry::aggregator::tests`（3） |
| `AttributesExtractors` + Formatter/Provider converter | 同名 | 已完成 | 纯函数返回 `(String, serde_json::Value)`；`GenerateOptions` 非 Serialize → 本地投影字段（不改 core）；单测 `telemetry::extractors::tests`（7） |

### 3.2 重要但可延后（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Socket.IO 真实握手/鉴权/重连 | StudioWebSocketClient.connect | 已完成（2026-10-08） | `studio::socket_io`：`socket_io_url`（http(s)→ws(s) + `?EIO=4&transport=websocket`）、`parse_engine_io_open`（`0{...}` 握手）、`namespace_connect_frame`（`40/python,{"runId":...}`）、`is_ping`/`pong_frame`、`parse_socket_io_event`（`42[...]`）、`reconnect_delay`（指数退避封顶）+ `SocketIoProtocol` 状态机 + `SocketIoTransport`（真实 WS）|
| `StudioClient` 网络路径离线测（mock HTTP） | StudioClient | 待办 | 需注入 HTTP 传输抽象或 mock server；当前仅离测请求体构造 |
| `StudioUserAgent` 与 core agent 循环集成 | StudioUserAgent | 待办 | 当前为独立输入提供者，调用方自行接入 agent 轮次 |

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `TelemetryTracer`（core `Middleware` 包装 + span 生命周期） | TelemetryTracer | 已完成（2026-10-21，`TelemetryTracing`） | `telemetry/tracing.rs`：`Middleware` 三拦截点（`OnAgent`/`OnModelCall`/`OnActing`）产出 `invoke_agent` / `chat {model}` / `execute_tool {name}`；span 用 **RAII 收尾**（guard 随事件流 drop 而结束，取消路径不漏 span）；父子靠 `RuntimeContext.run_id` 映射（`&RuntimeContext` 不可变，不能用 ctx 存 span）。`TraceExporter::export` 是 async 而 hook 是同步 → `SpanSink` 后台批量导出 + `flush()` 供测试/短进程。单元 7 个（含「drop 掉流仍导出 span」与真实 `ReActAgent` 端到端）|
| OTLP HTTP 导出 | exporter-otlp-http | 已完成（降级 exporter）| `telemetry::exporter`：`Span`/`SpanKind`/`SpanStatus`/`OtlpEncoder`（`resourceSpans`→`scopeSpans`→`spans`，id 十六进制校验、纳秒字符串、`AnyValue` 映射）+ `TraceExporter` trait + `OtlpHttpExporter`（reqwest `POST {tracingUrl}`）/`RecordingTraceExporter`/`NoopTraceExporter`；**未引入 `opentelemetry` 依赖**（设计决定，见 §4.2） |
| 多模态内容部件抽取 | TODO(multi-modal) | 待办 | Java 亦为 TODO；`block_to_part` 对 Image/Audio/Video 返回 None |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成**（`cargo build` / `cargo test` 全绿；本 crate 0 warning）。
- 验证命令与结果（2026-09-18）：
  - `cargo build -p agentscope-extensions-studio` → 成功，0 error / 0 warning（仅 core/harness 既有告警）。
  - `cargo test -p agentscope-extensions-studio` → **30 passed / 0 failed**（`--lib`）；doc-tests 0 passed。
- 交付：`src/` 15 个文件已接线并编译；`lib.rs` re-export `StudioClient/StudioConfig/StudioConfigBuilder/StudioWebSocketClient/StudioManager/StudioMessageHook/StudioUserAgent/StudioTransport/NoopStudioTransport/PendingInputRegistry/UserInputData` 及 telemetry 提取器/聚合器/模型。
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

新测试 22 个（52 − 30），全离线：Socket.IO 帧层 11 个（URL 改写/握手与缺 sid/连接帧/事件解析/ping 检测/退避封顶/协议三态），OTLP 导出 11 个（id 校验/时间范围/纳秒/AnyValue 映射/完整 payload/非法 span 报错/顺序与键/recording/noop/`from_config` 端点）。**真实 socket 路径无法离线测**（需运行中的 Studio 服务），已将其逻辑拆到纯函数/状态机以覆盖。

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

**测试**：`a_completed_run_exports_the_agent_span`、`a_failed_run_marks_the_span_as_error`、`a_dropped_run_still_ends_the_span`（**反向验证：去掉 Drop 路径后仅此用例 FAILED**）、`nested_phases_attach_to_the_run_span`、`a_run_without_a_run_id_falls_back_to_the_session`、`only_the_three_traced_points_are_active`、`a_real_agent_run_exports_agent_and_model_spans`（真实 `ReActAgent` + Mock 模型）。

**验证**：`cargo test -p agentscope-extensions-studio` → **59 passed / 0 failed**（基线 52，+7）。

### 4.2 下一步与阻塞（待汇总方裁定）
1. **【已解决 — 降级方案】** OTLP 导出**不引入 `opentelemetry`/`opentelemetry-otlp`**（避免根 `[workspace.dependencies]` 变更）：以 `telemetry::exporter` 自建 `Span` 模型 + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait 实现等价导出。`TelemetryTracer` 等价物已于 2026-10-21 落地（`TelemetryTracing`，§4.1.0u）。若后续要求与 OTel SDK 字节级兼容，再提请裁定。
2. **【已解决 — 本 crate 自声明依赖】** Socket.IO：已用 `tokio-tungstenite = 0.29`（与 CHANNEL crate 同版本/同 features）+ 自实现 Engine.IO v4/Socket.IO 帧（`studio::socket_io`）。**未改根 `Cargo.toml`。**
   附带说明：`StudioManager::initialize_with` 默认仍用 `NoopStudioTransport`（Java 为无条件连接）；宿主要启用真实连接需走 `initialize_with_transport` / `StudioWebSocketClient::with_transport_and_registry`。**原因**：默认连接会让本 crate 现有离线测试向 `localhost:3000` 发起真实网络请求（违反离线测试约定）。
3. **【core 缺口】** `GenerateOptions` 未实现 `Serialize`（本 crate 已用本地投影绕过，未改 core）；core 无 `Hook`/`TracerRegistry`（已按 §2 用 `Middleware` 替代）。
4. **【接线待办】** `TelemetryTracing` 目前由宿主显式挂到 `MiddlewareChain`（`chain.add(Box::new(tracing.clone()))`），尚未在 `StudioManager::initialize` 里按 `StudioConfig.tracing_url` 自动装配 —— 自动装配需决定「一个 Studio 实例 vs 每个 agent 一条链」，留待后续。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-studio` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-studio` / `cargo test -p agentscope-extensions-studio` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-studio/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

