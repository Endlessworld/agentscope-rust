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
| 当前状态 | 已完成（build/test 全绿；30 个离线单测）|

## 2. Java 清单与依赖映射

> Java 主包 `io.agentscope.core.studio`（HTTP/WS/管理）与 `io.agentscope.core.tracing.telemetry`（OTel GenAI 遥测）。

### 2.0 子模块清单
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `studio.StudioConfig` | 连接配置（url/tracingUrl/project/runName/runId/maxRetries/reconnect*）| `studio::StudioConfig` + `StudioConfigBuilder` | 默认值对齐：url=`http://localhost:3000`、project=`UnnamedProject`、runName=`run_<ts>`、runId=UUID、maxRetries=3、reconnectAttempts=3、delay=1s、maxDelay=5s；tracingUrl 默认 `studioUrl + /v1/traces` |
| `studio.StudioClient` | OkHttp HTTP 客户端（registerRun/pushMessage/requestUserInput + 重试 + shutdown）| `studio::StudioClient` | reqwest + 指数退避重试（仅 IO/网络错误重试，`max_retries`，base 100ms，max 5s）；时间戳 `yyyy-MM-dd HH:mm:ss.SSS` |
| `studio.StudioWebSocketClient` | Socket.IO 连接 `/python` namespace，监听 `forwardUserInput`，`waitForInput` 等待 Mono | `studio::StudioWebSocketClient` | Rust 无 socket.io 客户端；抽 `StudioTransport` trait（可插拔），内置 `PendingInputRegistry`（`HashMap<requestId, oneshot>`）；解析/规范化为纯函数 |
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
| `telemetry.TelemetryTracer` | `Tracer` 实现：agent/model/tool/format 四类 span + OTLP HTTP 导出 | `telemetry::TelemetryTracer` | 包装为 core `Middleware`（on_agent/on_reasoning/on_acting/on_model_call）；OTLP 导出见 §4.2 |
| `telemetry.model.*`（15 个类）| OTel 消息部件模型（Text/Reasoning/ToolCallRequest/ToolCallResponse/Blob/File/Uri Part、Input/OutputMessage、Role、SystemInstructions、ToolDefinition）| `telemetry::model` | serde 结构；`type` 字段判别 + JSON 字段名对齐（`mime_type`/`file_id`/`finish_reason`）|

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp3 (OkHttpClient) | Studio HTTP | `reqwest`（workspace 0.12, json/rustls）| 仅写本 crate Cargo.toml；根 `[workspace.dependencies]` 已有 reqwest，直接用 `reqwest = { workspace = true }` |
| reactor.core (Mono/Flux/Sinks) | 响应式 | `tokio::sync::oneshot/mpsc` + `futures::Stream` | 单值→async fn；等待→oneshot |
| socket.io-client（java） | Studio WS | **无直接等价**；`StudioTransport` trait + 纯函数解析 | 见 §4.1/§4.2；`tokio-tungstenite` 仅底层 WS，需自实现 Socket.IO 握手 → 本 crate 先出 trait，真实实现留待接线 |
| com.fasterxml.jackson（JSON） | 序列化 | `serde` / `serde_json` | 已建 |
| org.json (JSONArray/JSONObject) | WS 事件解析 | `serde_json::Value` | |
| reactor.util.retry.Retry | 重试退避 | 手写 `retry_with_backoff` 辅助 | base 100ms → max 5s，仅 IO/网络错误 |
| opentelemetry-api / sdk / exporter-otlp-http | OTel span + 导出 | **设计：核心 span 抽象 + 属性抽取先落地**；OTLP 导出见 §4.2 | core/harness 当前无 `opentelemetry` 依赖；加 workspace 依赖需汇总方裁定 |
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
| Socket.IO 真实握手/鉴权/重连 | StudioWebSocketClient.connect | 待办 | 本 crate 仅落地 `StudioTransport` trait + `NoopStudioTransport`；真实实现待依赖裁定（§4.2） |
| `StudioClient` 网络路径离线测（mock HTTP） | StudioClient | 待办 | 需注入 HTTP 传输抽象或 mock server；当前仅离测请求体构造 |
| `StudioUserAgent` 与 core agent 循环集成 | StudioUserAgent | 待办 | 当前为独立输入提供者，调用方自行接入 agent 轮次 |

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `TelemetryTracer`（core `Middleware` 包装 + span 生命周期） | TelemetryTracer | 待办 | 依赖 OTel 依赖裁定（§4.2） |
| OTLP HTTP 导出 | exporter-otlp-http | 待办 | 需 `opentelemetry`(exporter-otlp-http) workspace 依赖，由汇总方裁定（§4.2） |
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

### 4.2 下一步与阻塞（待汇总方裁定）
1. **【跨模块/workspace】** OTLP 导出与 `TelemetryTracer` 全链路需 `opentelemetry` + `opentelemetry-otlp` 依赖，属根 `[workspace.dependencies]` 变更 → 本子智能体未改根 `Cargo.toml`，提请汇总方裁定。
2. **【跨模块/workspace】** Socket.IO：workspace 无 Socket.IO/Engine.IO 客户端；若需真实实现需引入新依赖或自实现握手，需汇总方裁定依赖归属。
3. **【core 缺口】** `GenerateOptions` 未实现 `Serialize`（本 crate 已用本地投影绕过，未改 core）；core 无 `Hook`/`TracerRegistry`（已按 §2 用 `Middleware` 替代）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-studio` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-studio` / `cargo test -p agentscope-extensions-studio` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-studio/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

