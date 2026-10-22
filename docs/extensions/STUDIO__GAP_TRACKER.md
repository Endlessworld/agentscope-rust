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
| Rust 规模（实测 2026-10-22） | 18 个 `.rs` / **5048 行**（非空行 4578）/ **59 个内联测试**（46 `#[test]` + 13 `#[tokio::test]`）|
| 迁移波次 | C |
| 当前状态 | 已完成（**59 个离线单测**；Socket.IO 真实传输 + OTLP 降级导出已落地；`TelemetryTracing` 已落地但**未自动装配**，见 §4.2 第 4 条）。**2026-10-22 静态复核**：新增 2 项真实缺口 —— `StudioMessageHook` 终态消息捕获键与 core 不一致导致转发恒不触发（§6.4 P0-1）、`TelemetryTracer` 仍有 3 处 Java 语义未覆盖（§6.4 P1-1）；另有鉴权字段名分歧（§6.4 P1-2）|
| 行数口径 | Java 4305/1837 与 Rust 5048 均为**物理行**（含空行/注释）；非空行口径 Java 3738/1554、Rust 4578。汇总 tracker 的 3700/1452 与之同族，需按口径换算 |

## 2. Java 清单与依赖映射

> Java 主包 `io.agentscope.core.studio`（HTTP/WS/管理）与 `io.agentscope.core.tracing.telemetry`（OTel GenAI 遥测）。

### 2.0 子模块清单
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `studio.StudioConfig` | 连接配置（url/tracingUrl/project/runName/runId/maxRetries/reconnect*）| `studio::StudioConfig` + `StudioConfigBuilder` | 默认值对齐：url=`http://localhost:3000`、project=`UnnamedProject`、runName=`run_<ts>`、runId=UUID、maxRetries=3、reconnectAttempts=3、delay=1s、maxDelay=5s；tracingUrl 默认 `studioUrl + /v1/traces` |
| `studio.StudioClient` | OkHttp HTTP 客户端（registerRun/pushMessage/requestUserInput + 重试 + shutdown）| `studio::StudioClient` | reqwest + 指数退避重试（base 100ms，max 5s，`max_retries` 次）；时间戳 `yyyy-MM-dd HH:mm:ss.SSS`。**静态复核**：Rust 仅对 `StudioError::Transport` 重试，Java 因非 2xx 被包成 `IOException` **也会重试**（语义分歧，见 §6.4 P2-1）|
| `studio.StudioWebSocketClient` | Socket.IO 连接 `/python` namespace，监听 `forwardUserInput`，`waitForInput` 等待 Mono | `studio::StudioWebSocketClient` + `studio::socket_io` | `StudioTransport` trait（可插拔）+ `PendingInputRegistry`（`HashMap<requestId, oneshot>`）；**`SocketIoTransport` 为真实实现**（tokio-tungstenite 0.29 + 自实现 Engine.IO v4 握手/`40` 入房/ping-pong/指数退避重连）；帧解析为纯函数。**静态复核**：鉴权载荷键 Rust 用 `runId`，Java 用 `run_id`（见 §6.4 P1-2）|
| `studio.StudioWebSocketClient.UserInputData` | 用户输入载荷（blocksInput + structuredInput）| `studio::UserInputData` | |
| `studio.StudioManager` | 全局静态单例（config/client/wsClient + init builder + shutdown + TracerRegistry 注册）| `studio::StudioManager` | Rust 用 `OnceLock<RwLock<Option<...>>>` 全局；不依赖 core `TracerRegistry`（core 无此类型，见 §4.2） |
| `studio.StudioMessageHook` | `Hook`，拦截 `PostCallEvent` 把最终消息推给 Studio | `studio::StudioMessageHook` | Rust 无 `Hook`（core `hook` 为设计决定不迁移）；改为 core `Middleware`：`on_agent` 包裹后在事件流上捕获终态消息并推送（fire-and-forget）。**静态复核**：捕获键为 `fields["message"]`，而 core `react_agent` 写入的是 `fields["reply"]`（protocol crate 写 `fields["result"]`）→ **转发恒不触发**（§6.4 P0-1）|
| `studio.StudioUserAgent` | 用户代理 Agent（终端 / Studio 双模式）| `studio::StudioUserAgent` | 输出 Msg（USER 角色），Studio 模式失败回退终端 |
| `studio.pojo.RegisterRunRequest` | registerRun 请求体 | `studio::pojo::RegisterRunRequest` | 字段：id/project/name/timestamp/pid/status/run_dir |
| `studio.pojo.PushMessageRequest` | pushMessage 请求体 | `studio::pojo::PushMessageRequest` | 字段：runId/replyId/name/role/msg |
| `studio.pojo.RequestUserInputRequest` | requestUserInput 请求体 | `studio::pojo::RequestUserInputRequest` | 字段：requestId/runId/agentId/agentName/structuredInput |
| `studio.pojo.UserInputMetadata` | 用户输入元数据（source/requestId/structuredInput）| `studio::pojo::UserInputMetadata` | `@JsonInclude(NON_NULL)`；source 默认 `studio` |
| `telemetry.GenAiIncubatingAttributes` | OTel GenAI 语义约定属性键（Java 类注释写 1.41.0，Rust 模块文档写 1.37.0 —— **以 Java 源码为准：1.41.0**）| `telemetry::gen_ai_attributes` | 常量字符串键（`gen_ai.*`）+ operation/provider 取值常量。实测：Java 38 个 `AttributeKey` → Rust 34 个键，缺 `GEN_AI_TOOL_TYPE` 与 3 个 usage 键（§6.4 P2-3）|
| `telemetry.AgentScopeIncubatingAttributes` | AgentScope 自定义属性键 | `telemetry::agent_scope_attributes` | `agentscope.function.*`、`agentscope.format.*`。实测：Java 6 键 → Rust 5 键，缺 `AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS` |
| `telemetry.AttributesExtractors` | 从 agent/model/tool/formatter 抽取 OTel 属性（实测 648 行）| `telemetry::extractors`（**同名模块，不存在 `AttributesExtractors` 类型**）| 纯函数（`get_*_attributes`）；复用 core 的 Msg/ContentBlock/ChatResponse/ToolSchema/GenerateOptions |
| `telemetry.AttributesExtractors.FormatterConverter` | formatter 简单类名 → provider 名映射 | `telemetry::FormatterConverter` | DashScope/Gemini/OpenAI→dashscope；Anthropic→anthropic；其余 unknown |
| `telemetry.AttributesExtractors.ProviderNameConverter` | ChatModelBase 实例 → provider 名（按类继承链判断）| `telemetry::ProviderNameConverter` | Rust 无运行时类反射；改为按模型类型名后缀探测（`DashScopeChatModel`/`GeminiChatModel`/`AnthropicChatModel`/`OpenAIChatModel`）。**静态复核**：`TelemetryTracing` 未调用它，而是用 `DEFAULT_PROVIDER_NAME="unknown"`/宿主 `with_provider_name` 覆盖（§6.4 P1-1）|
| `telemetry.StreamChatResponseAggregator` | 流式 ChatResponse 聚合（text/thinking/tool_calls + usage max + finishReason）| `telemetry::StreamChatResponseAggregator` | 复用 core `ChatResponse`/`ChatUsage`/`ContentBlock`。**静态复核**：该聚合器**未被 `TelemetryTracing` 使用**（Java 用它给 `chat` span 补响应侧属性），仅作为公开 API 存在（§6.4 P1-1）|
| `telemetry.TelemetryTracer` | `Tracer` 实现：agent/model/tool/**format** 四类 span + OTLP HTTP 导出 | **`telemetry::TelemetryTracing`**（**不存在 `TelemetryTracer` 类型**）| 包装为 core `Middleware`，实际拦截点只有 `on_agent`/`on_model_call`/`on_acting`（**无 `on_reasoning`，且缺 Java 的 `callFormat` format span**）；OTLP 导出为降级 exporter（`telemetry::exporter`，见 §4.1.0s）。**静态复核**：span 上缺 `agentscope.function.name`、缺响应侧属性（§6.4 P1-1）|
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
| crate 骨架 + lib.rs 模块树 | — | 已完成 | `lib.rs` 声明 `pub mod error/studio/telemetry` 并 re-export；`studio/`（client/config/message_hook/mod/pojo/socket_io/user_agent/websocket）+ `telemetry/`（agent_scope_attributes/aggregator/exporter/extractors/gen_ai_attributes/mod/model/tracing），加 `lib.rs` + `error.rs` **共 18 个文件**（本行为 2026-10-22 复核时更新，原「15 个文件」为 2026-09-18 快照） |
| `StudioConfig` + builder（默认值/校验/tracingUrl 推导）| StudioConfig | 已完成 | 默认值/tracingUrl 推导/尾部 `/` trim 已对齐 Java；单测 `studio::config::tests`（3） |
| studio POJO ×4（serde 字段名对齐 Java）| pojo/*.java | 已完成 | camelCase / `run_dir` / `structuredInput` / `NON_NULL` 跳过；单测 `studio::pojo::tests`（5） |
| `StudioClient`（registerRun/pushMessage/requestUserInput/shutdown + 重试）| StudioClient | 已完成 | reqwest；指数退避仅重试 `StudioError::Transport`；网络路径未离线测（§3.2）。**静态复核**：Java 的非 2xx 被包成 `IOException` 因而**也会**被重试，Rust 不重试 HTTP 状态错误（§6.4 P2-1）|
| `StudioWebSocketClient` + `StudioTransport` trait + `PendingInputRegistry` | StudioWebSocketClient | 已完成 | 可插拔 trait + oneshot 注册表；`NoopStudioTransport` 供离线测；单测 `studio::websocket::tests`（**11** = 6 `#[test]` + 5 `#[tokio::test]`，原记 9 有误） |
| `UserInputData` + `parse_content_blocks` + `preprocess_tool_result_block` | StudioWebSocketClient | 已完成 | 纯函数；含 tool_result 字符串 output→text 块规范化；同上单测覆盖 |
| `StudioMessageHook`（Middleware on_agent 后推送）| StudioMessageHook | 已完成 | 实现 core `Middleware::on_agent`，捕获终态消息 fire-and-forget 推送，失败仅日志不中断 agent。**静态复核**：捕获键 `"message"` 与 core 实际写入的 `"reply"` 不一致 → 恒不推送（§6.4 P0-1）；另 `tokio::spawn` 需在 runtime 内调用 |
| `StudioManager` 全局单例（init builder/shutdown/getters）| StudioManager | 已完成 | `OnceLock<RwLock<Option<Installed>>>`；init/builder/getters/shutdown/initialize_with |
| `StudioUserAgent`（终端 + Studio 双模式）| StudioUserAgent | 已完成 | 独立输入提供者（core 无 `AgentBase`）；Studio 失败回退终端；`DEFAULT_INPUT_TIMEOUT=30min` |
| telemetry 属性常量 ×2 文件 | GenAi/AgentScopeIncubatingAttributes | 已完成 | 常量 `&'static str` 键 + operation/provider 取值常量 |
| telemetry model 部件 ×**13** | telemetry/model/*.java | 已完成 | serde `type` 判别；字段名对齐（`mime_type`/`file_id`/`finish_reason`）；Rust `telemetry::model` 13 个 `pub` 类型一一对应 |
| `StreamChatResponseAggregator` | 同名 | 已完成 | text/thinking/tool_calls 按 id 合并 + usage 取 max + finishReason；单测 `telemetry::aggregator::tests`（3） |
| `AttributesExtractors` + Formatter/Provider converter | 同名 | 已完成 | 纯函数返回 `(String, serde_json::Value)`；`GenerateOptions` 非 Serialize → 本地投影字段（不改 core）；单测 `telemetry::extractors::tests`（**8**，原记 7 有误） |

### 3.2 重要但可延后（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Socket.IO 真实握手/鉴权/重连 | StudioWebSocketClient.connect | 进行中（2026-10-22 复核：实现完整，但鉴权字段名与 Java 不一致） | `studio::socket_io`：`socket_io_url`（http(s)→ws(s) + `?EIO=4&transport=websocket`）、`parse_engine_io_open`（`0{...}` 握手）、`namespace_connect_frame`（`40/python,{...}`）、`is_ping`/`pong_frame`、`parse_socket_io_event`（`42[...]`）、`reconnect_delay`（指数退避封顶）+ `SocketIoProtocol` 状态机 + `SocketIoTransport`（真实 WS）；单测 `studio::socket_io::tests`（**12** = 11 帧层 + 1 transport 构造）。**待修**：鉴权载荷 Rust=`{"runId":...}`、Java=`{"run_id":...}`（§6.4 P1-2）|
| `StudioClient` 网络路径离线测（mock HTTP） | StudioClient | 待办 | 需注入 HTTP 传输抽象或 mock server（`StudioClient` 目前直接持 `reqwest::Client`，无注入点）；当前仅离测请求体构造 |
| `StudioUserAgent` 与 core agent 循环集成 | StudioUserAgent | 待办 | 当前为独立输入提供者，调用方自行接入 agent 轮次 |
| `StudioMessageHook` 终态消息捕获键修正 | StudioMessageHook | 待办 | core 终态消息在 `fields["reply"]`（protocol crate 为 `fields["result"]`），Rust 侧读的是 `fields["message"]` → 恒不触发（§6.4 P0-1）|

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `TelemetryTracer`（core `Middleware` 包装 + span 生命周期） | TelemetryTracer | 进行中（2026-10-22 复核：span 生命周期已达标，语义覆盖仍缺 3 项） | `telemetry/tracing.rs`：`Middleware` 三拦截点（`OnAgent`/`OnModelCall`/`OnActing`）产出 `invoke_agent` / `chat {model}` / `execute_tool {name}`；span 用 **RAII 收尾**（guard 随事件流 drop 而结束，取消路径不漏 span）；父子靠 `RuntimeContext.run_id` 映射（`&RuntimeContext` 不可变，不能用 ctx 存 span）。`TraceExporter::export` 是 async 而 hook 是同步 → `SpanSink` 后台批量导出 + `flush()` 供测试/短进程。单元 7 个（1 `#[test]` + 6 `#[tokio::test]`，含「drop 掉流仍导出 span」与真实 `ReActAgent` 端到端）。**待补**：①span 缺 `agentscope.function.name`；②缺响应侧属性（`get_agent_response_attributes`/`get_llm_response_attributes`/`get_tool_response_attributes` 未被调用，`StreamChatResponseAggregator` 未接入）；③缺 Java `callFormat` 的 `format {target}` span（§6.4 P1-1）|
| OTLP HTTP 导出 | exporter-otlp-http | 已完成（降级 exporter）| `telemetry::exporter`：`Span`/`SpanKind`/`SpanStatus`/`OtlpEncoder`（`resourceSpans`→`scopeSpans`→`spans`，id 十六进制校验、纳秒字符串、`AnyValue` 映射）+ `TraceExporter` trait + `OtlpHttpExporter`（reqwest `POST {tracingUrl}`）/`RecordingTraceExporter`/`NoopTraceExporter`；**未引入 `opentelemetry` 依赖**（设计决定，见 §4.2）；单测 `telemetry::exporter::tests`（**10** = 8 + 2）。**待补**：Java `TelemetryTracer.Builder.addHeader/headers`（Langfuse 等需鉴权头）在 Rust 侧无对应（§6.4 P2-2）|
| 多模态内容部件抽取 | TODO(multi-modal) | 待办 | Java 亦为 TODO；`block_to_part` 对 Image/Audio/Video 返回 None |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成**（`cargo build` / `cargo test` 全绿；本 crate 0 warning）。
  > **2026-10-22 静态复核声明**：本轮**未执行** `cargo build` / `cargo test`（按子智能体约束仅静态核对），下列 build/test 结论沿用上一会话记录，**未经本轮复验**。
- 验证命令与结果（2026-09-18，**历史快照**）：
  - `cargo build -p agentscope-extensions-studio` → 成功，0 error / 0 warning（仅 core/harness 既有告警）。
  - `cargo test -p agentscope-extensions-studio` → **30 passed / 0 failed**（`--lib`）；doc-tests 0 passed。
  - 后续增量：2026-10-08 → **52 passed**（§4.1.0s）；2026-10-21 → **59 passed**（§4.1.0u）。2026-10-22 静态复核按 `#[test]`/`#[tokio::test]` 注解逐文件计数得 **59**（46 + 13），与记录一致。
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

**测试**：`a_completed_run_exports_the_agent_span`、`a_failed_run_marks_the_span_as_error`、`a_dropped_run_still_ends_the_span`（**反向验证：去掉 Drop 路径后仅此用例 FAILED**）、`nested_phases_attach_to_the_run_span`、`a_run_without_a_run_id_falls_back_to_the_session`、`only_the_three_traced_points_are_active`、`a_real_agent_run_exports_agent_and_model_spans`（真实 `ReActAgent` + Mock 模型）。

**验证**：`cargo test -p agentscope-extensions-studio` → **59 passed / 0 failed**（基线 52，+7）。

### 4.2 下一步与阻塞（待汇总方裁定）
1. **【已解决 — 降级方案】** OTLP 导出**不引入 `opentelemetry`/`opentelemetry-otlp`**（避免根 `[workspace.dependencies]` 变更）：以 `telemetry::exporter` 自建 `Span` 模型 + `OtlpEncoder`（OTLP/HTTP JSON）+ `TraceExporter` trait 实现等价导出。`TelemetryTracer` 等价物已于 2026-10-21 落地（`TelemetryTracing`，§4.1.0u）。若后续要求与 OTel SDK 字节级兼容，再提请裁定。
2. **【已解决 — 本 crate 自声明依赖】** Socket.IO：已用 `tokio-tungstenite = 0.29`（与 CHANNEL crate 同版本/同 features）+ 自实现 Engine.IO v4/Socket.IO 帧（`studio::socket_io`）。**未改根 `Cargo.toml`。**
   附带说明：`StudioManager::initialize_with` 默认仍用 `NoopStudioTransport`（Java 为无条件连接）；宿主要启用真实连接需走 `initialize_with_transport` / `StudioWebSocketClient::with_transport_and_registry`。**原因**：默认连接会让本 crate 现有离线测试向 `localhost:3000` 发起真实网络请求（违反离线测试约定）。
3. **【core 缺口】** `GenerateOptions` 未实现 `Serialize`（本 crate 已用本地投影绕过，未改 core）；core 无 `Hook`/`TracerRegistry`（已按 §2 用 `Middleware` 替代）。
4. **【接线待办 — 2026-10-22 复核仍存在】** `TelemetryTracing` 目前由宿主显式挂到 `MiddlewareChain`（`chain.add(Box::new(tracing.clone()))`），尚未在 `StudioManager::initialize` 里按 `StudioConfig.tracing_url` 自动装配 —— 自动装配需决定「一个 Studio 实例 vs 每个 agent 一条链」，留待后续。
   **复核证据**：全仓 `TelemetryTracing` 的引用仅出现在 `telemetry/tracing.rs` 自身与其单测、`telemetry/mod.rs` 的 re-export；`StudioManager::initialize*` 未引用它。Java 侧对应 `StudioManager.initialize()` 内的 `TracerRegistry.register(TelemetryTracer.builder().endpoint(traceEndpoint).build())`（Java 自动装配）。
5. **【2026-10-22 新增 — 见 §6.4】** P0-1 `StudioMessageHook` 捕获键与 core 不一致（转发恒不触发）；P1-1 `TelemetryTracer` 三处语义缺口；P1-2 Socket.IO 鉴权字段名分歧；P2-1 HTTP 错误重试语义分歧；P2-2 OTLP exporter 无自定义 header。

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
| 测试用例数 | **91** 个 `@Test` 方法 | **59**（46 `#[test]` + 13 `#[tokio::test]`）|
| 顶层类/类型 | 28 文件 / **44 个类型声明**（含嵌套类与接口）| 18 文件 / 与 Java 一一映射（见 §6.3）|
| 属性键常量 | `GenAiIncubatingAttributes` 38 个 `AttributeKey` + `AgentScopeIncubatingAttributes` 6 个 | 34 + 5（缺 4 + 1，见 §6.4 P2-3）|
| OTLP 部件模型 | `telemetry/model` **13** 个类 | `telemetry::model` **13** 个 `pub` 类型（一一对应）|

Rust 逐文件测试分布（`#[test]` / `#[tokio::test]` / 合计）：
`config.rs` 3/0/3、`pojo.rs` 5/0/5、`socket_io.rs` 12/0/12、`websocket.rs` 6/5/11、`aggregator.rs` 3/0/3、`exporter.rs` 8/2/10、`extractors.rs` 8/0/8、`tracing.rs` 1/6/7 → **合计 46/13/59**。

### 6.3 对齐结论表
| Java 类 / 包 | Rust 对应 | 行为覆盖 | 结论 |
|---|---|---|---|
| `studio.StudioConfig` | `studio::StudioConfig` + `StudioConfigBuilder` | 默认值/尾部 `/` trim/`tracingUrl` 推导全对齐；Java 无 trim，Rust 多做一次 trim（无害）| ✅ 对齐 |
| `studio.StudioClient` | `studio::StudioClient` | 三方法 + 重试 + shutdown 均在；**重试谓词不同**（Java 含非 2xx，Rust 不含）| ⚠️ 部分（§6.4 P2-1）|
| `studio.StudioWebSocketClient` | `studio::StudioWebSocketClient` + `studio::socket_io` | 握手/入房/ping-pong/重连/`forwardUserInput` 解码全在；**鉴权键名不同** | ⚠️ 部分（§6.4 P1-2）|
| `studio.StudioWebSocketClient.UserInputData` | `studio::UserInputData` | blocksInput + structuredInput 结构一致 | ✅ 对齐 |
| `studio.StudioManager` | `studio::StudioManager` | 全局单例/init builder/getters/shutdown 在；**Java 的 `TracerRegistry.register` 自动装配缺失**（§4.2 第 4 条）| ⚠️ 部分 |
| `studio.StudioMessageHook` | `studio::StudioMessageHook` | Middleware 形态在，但**终态消息捕获键与 core 不符 → 恒不触发** | ❌ 未对齐（§6.4 P0-1）|
| `studio.StudioUserAgent` | `studio::StudioUserAgent` | 双模式 + 失败回退 + 30min 超时在；非 `AgentBase` 子类（core 无此类，§3.2 已登记）| ✅ 设计等价 |
| `studio.pojo.*`（4 个）| `studio::pojo.*`（4 个）| 字段名/`run_dir`/`camelCase`/`NON_NULL` 跳过逐条对齐 | ✅ 对齐 |
| `telemetry.GenAiIncubatingAttributes` | `telemetry::gen_ai_attributes` | 38→34，缺 `gen_ai.tool.type` + 3 个 usage 键 | ⚠️ 部分（§6.4 P2-3）|
| `telemetry.AgentScopeIncubatingAttributes` | `telemetry::agent_scope_attributes` | 6→5，缺 `agentscope.usage.tool_use_prompt_tokens` | ⚠️ 部分（§6.4 P2-3）|
| `telemetry.AttributesExtractors` | `telemetry::extractors`（模块，非同名类型）| 10 个 `get_*` 函数一一对应；`GenerateOptions` 用本地投影绕过 core `Serialize` 缺口 | ✅ 对齐 |
| `…FormatterConverter` | `telemetry::FormatterConverter` | 映射表一致 | ✅ 对齐 |
| `…ProviderNameConverter` | `telemetry::ProviderNameConverter` | 函数在，但 `TelemetryTracing` 未调用（provider 恒为 `unknown`）| ⚠️ 部分（§6.4 P1-1）|
| `telemetry.StreamChatResponseAggregator` | `telemetry::StreamChatResponseAggregator` | 聚合逻辑在；**未被 span 管线接入** | ⚠️ 部分（§6.4 P1-1）|
| `telemetry.TelemetryTracer` | **`telemetry::TelemetryTracing`** | span 生命周期/父子/RAII 达标；缺 `agentscope.function.name`、缺响应侧属性、缺 format span | ⚠️ 部分（§6.4 P1-1）|
| `telemetry.model.*`（13 类）| `telemetry::model`（13 类型）| 一一对应，`type` 判别 + `mime_type`/`file_id`/`finish_reason` 字段名对齐 | ✅ 对齐 |
| OTLP 导出（`exporter-otlp-http`）| `telemetry::exporter`（`OtlpEncoder` + `OtlpHttpExporter`）| OTLP/HTTP JSON 编码 + POST 实现；**未引入 `opentelemetry`（已核实）**；无自定义 header | ✅ 设计等价（§6.4 P2-2）|

**核实结论（逐条 grep）**：
- 「不引入 `opentelemetry`」**属实** —— `crates/agentscope-extensions-studio/Cargo.toml` 无 `opentelemetry*` 依赖；根 `Cargo.toml` 的 `[workspace.dependencies]` 亦无；crate 内 `opentelemetry` 仅出现在 6 处**文档注释**中。
- 「Socket.IO 真实握手/鉴权/重连」**属实但有一处键名分歧** —— `studio/socket_io.rs` 确有 `SocketIoTransport`（tokio-tungstenite 0.29 真连）、`SocketIoProtocol` 状态机、`parse_engine_io_open`/`namespace_connect_frame`/`is_ping`/`reconnect_delay`；`Cargo.toml` 的 `tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots）与 CHANNEL crate 同版本同 features。
- 「§3.3 / §4.1 的 `TelemetryTracer` 接线缺口」**仍存在** —— `TelemetryTracing` 全仓引用仅限 `telemetry/tracing.rs`、`telemetry/mod.rs` 与其单测；`StudioManager::initialize*` 未装配。

### 6.4 新发现缺口
| 编号 | 优先级 | 缺口 | 证据 | 影响 |
|---|---|---|---|---|
| P0-1 | **P0** | `StudioMessageHook` 捕获终态消息的键名错误，转发**恒不触发** | Rust `studio/message_hook.rs:92` 读 `data.fields.get("message")`；core `agentscope-core/src/agent/react_agent.rs:725` 写入的是 `fields["reply"]`；protocol crate 写入 `fields["result"]`。全仓无任何 `fields["message"]` 生产者 | Studio 端看不到 agent 最终消息 —— Studio 集成的主功能之一实际失效（静默失效，最坏情况）|
| P1-1 | **P1** | `TelemetryTracer` 三处 Java 语义未覆盖：①span 缺 `agentscope.function.name`；②响应侧属性（`get_agent_response_attributes`/`get_llm_response_attributes`/`get_tool_response_attributes`）完全未接入，`StreamChatResponseAggregator` 未被 `tracing.rs` 引用；③缺 Java `callFormat` 的 `format {target}` span（core `Middleware` 无 formatter 拦截点，需另找 seam）| `telemetry/tracing.rs:32-36` 的 import 只含 3 个 request 侧抽取器；`agent_scope_attributes.rs` 的 `AGENTSCOPE_FUNCTION_NAME` 全仓仅在定义处出现；`get_agent_response_attributes` 等在 `telemetry/` 内只被 `mod.rs` re-export | 遥测数据维度明显少于 Java：`gen_ai.usage.*`/`gen_ai.output.messages` 缺失、span 无法按函数名聚合、formatter 调用不可见 |
| P1-2 | **P1** | Socket.IO 鉴权载荷键名分歧 | Java `StudioWebSocketClient.java:98` `auth.put("run_id", config.getRunId())`（snake_case）；Rust `studio/socket_io.rs:288` `json!({ "runId": config.run_id() })`（camelCase）| 服务端按 `run_id` 校验时，Rust 握手鉴权字段对不上 → 真实 Studio 环境下入房可能被拒（离线测试无法暴露）|
| P2-1 | **P2** | HTTP 错误重试语义分歧 | Java `StudioClient.java:116/161/209` 把非 2xx 包成 `IOException` → 被 `Retry.filter(t -> t instanceof IOException)` 命中**会重试**；Rust `client.rs:137` 仅 `StudioError::Transport` 可重试，`StudioError::Http` 不重试 | 服务端 5xx 时 Java 重试 3 次、Rust 立即失败 —— 健壮性弱于 Java |
| P2-2 | **P2** | OTLP exporter 无自定义 header | Java `TelemetryTracer.Builder.addHeader/headers`（注释明写 "for authentication (e.g., Langfuse)"）；Rust `OtlpHttpExporter` 仅设 `Content-Type`（`exporter.rs:428`）| 对 Langfuse 等需鉴权头的 OTLP 后端无法落地 |
| P2-3 | **P2** | 属性键常量缺 5 个（Java 44 → Rust 39）| 缺 `gen_ai.tool.type`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.usage.cache_read.input_tokens`、`gen_ai.usage.reasoning.output_tokens`、`agentscope.usage.tool_use_prompt_tokens` | 语义约定覆盖不全；不影响主链路 |
| P2-4 | **P2** | `StudioUserAgent` 缺 Java 的 name 非空校验 | Java `StudioUserAgent.Builder.build()` 对空名抛 `IllegalArgumentException`；Rust `user_agent.rs:183` 直接 build | 低危 |

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
| 21 | §3.2 | 缺 `StudioMessageHook` 捕获键条目 | **新增**「`StudioMessageHook` 终态消息捕获键修正」行（待办）|
| 22 | §4.1 当前状态 | 「交付：`src/` **15 个文件**已接线并编译」| **18 个文件** |
| 23 | §4.1 当前状态 | 「状态：已完成（cargo build / cargo test 全绿）」| 补注：本轮**静态复核未编译**，build/test 结论沿用历史记录未经复验 |
| 24 | §4.1.0s | 「Socket.IO 帧层 11 个 …，OTLP 导出 **11** 个」| 导出实为 **10** 个（socket_io 12 + exporter 10 = 22，合计正确）|
| 25 | §4.1.0u 设计要点 | 「首版不含响应侧属性（`gen_ai.usage.*`）」| 表述偏窄：实际是**三个响应侧抽取器整体未接入** + 聚合器未接线 + 缺 `agentscope.function.name` + 缺 format span（P1-1）|
| 26 | §4.2 第 4 条 | 「【接线待办】」| 复核确认**仍存在**，补记证据（`TelemetryTracing` 引用范围）|
| 27 | §2.0 `GenAiIncubatingAttributes` 行 | 「OTel GenAI 语义约定 **1.37.0** 属性键」| Java 类注释明写「semantic conventions **1.41.0**」；Rust `gen_ai_attributes.rs` 模块文档也写 1.37.0 → 以 Java 为准应为 1.41.0（文档口径已更正，Rust 侧源码本次未动）|
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

- `TelemetryTracer` 缺 `agentscope.function.name` span、三个响应侧抽取器 + `StreamChatResponseAggregator` 未接入。
- Socket.IO 鉴权键 Rust `runId` vs Java `run_id`（真实环境可能握手被拒）。