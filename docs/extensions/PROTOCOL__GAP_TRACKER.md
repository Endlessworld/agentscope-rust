# PROTOCOL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **PROTOCOL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-protocol\`
> **Java 子模块**：a2a-client, a2a-server, agent-protocol, agui, chat-completions-web
> **Rust 目标**：`crates/agentscope-extensions-protocol/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-protocol` |
| Java 子模块 | a2a-client, a2a-server, agent-protocol, agui, chat-completions-web |
| Java 规模（实测行数） | a2a-client 2896 / a2a-server 3238 / agent-protocol 1458 / agui 8467 / chat-completions-web 1851（主代码，不含测试）；**合计主 17910 / 测 21254** |
| Rust crate | facade `crates/agentscope-extensions-protocol` + 5 个拆分子 crate（`-a2a-client` / `-a2a-server` / `-agent-protocol` / `-agui` / `-chat-completions-web`，见 history/PROTOCOL__MIGRATION_HISTORY.md） |
| Rust 规模（实测 2026-10-30） | 6 crate 共 **47 文件 / 21793 行 / 353 个测试**；分布：a2a-client 7 文件 2554 行 40 测、a2a-server 7 文件 4439 行 57 测、agent-protocol 3 文件 2093 行 29 测、agui 24 文件 11167 行 192 测、chat-completions-web 5 文件 1507 行 35 测、facade 1 文件 33 行 0 测（2026-10-28 时为 45 文件 / 20948 行 / 341 测） |
| 迁移波次 | A |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 384 passed / 0 failed：a2a-client 62 / a2a-server 63 / agent-protocol 29 / agui 195 / chat-completions-web 35，facade 0）—— 5 个子 crate 全部落地；`A2aAgent`/`A2aAgentConfig` 门面的 core/harness 卡点以 crate 内适配层绕过（未改 core/harness）；`bytes`/`tracing`/MIME/`AguiTransport` 四条为陈旧或设计决定项。历史见 history/PROTOCOL__MIGRATION_HISTORY.md |

### 1.1 验证基线（2026-10-28 全量批次 + 2026-10-30 增量实测）

> **2026-10-31**：主智能体实测 **384 passed / 0 failed**（a2a-client 62 / a2a-server 63 / agent-protocol 29 / agui 195 / chat-completions-web 35，facade 0）；下方代码块为 2026-10-30 历史批次记录。
```
cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client \
  -p agentscope-extensions-a2a-server -p agentscope-extensions-agent-protocol \
  -p agentscope-extensions-agui -p agentscope-extensions-chat-completions-web -j 2
-> a2a-client 39 passed / a2a-server 47 passed / agent-protocol 29 passed
   agui 191 passed / chat-completions-web 35 passed / facade 0 passed
   合计 341 passed; 0 failed; 0 ignored
```
> 2026-10-22 的静态复核（302）已被本次实测取代；`cargo build` 随 `cargo test` 一并通过（0 error）。

**2026-10-30 更新（本次实测，单条命令 -j 2）**
```
cargo test -p agentscope-extensions-a2a-server -j 2  -> 57 passed; 0 failed; 0 ignored
cargo test -p agentscope-extensions-agui -j 2        -> 192 passed; 0 failed; 0 ignored
cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client -j 2
   -> protocol 0 passed（仅 re-export）/ a2a-client 40 passed; 0 failed
```
> 规模（实测 2026-10-30）：6 crate 共 **47 文件 / 21793 行 / 353 个测试**；分布：a2a-client 7 文件 2554 行 40 测、a2a-server 7 文件 4439 行 57 测、agent-protocol 3 文件 2093 行 29 测、agui 24 文件 11167 行 192 测、chat-completions-web 5 文件 1507 行 35 测、facade 1 文件 33 行 0 测。

## 2. Java 清单与依赖映射

### 2.1 a2a-client（`io.agentscope.core.a2a.agent.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `agent.message.MessageConstants` | 元数据键与 block 类型常量 | `a2a::message::MessageConstants`（+ `message_constants::block_content` 子模块） | 已完成 |
| `agent.message.*ParserRouter`（Text/File/Data/Thinking/ToolUse/ToolResult/Hint 等） | Part ↔ ContentBlock 解析路由 | `a2a::message::part_to_content_block` / `content_block_to_part`（公开）；`file_part_to_block`/`data_part_to_block`/`media_part`/`mime_type_from_url` 为**私有**实现 | 已完成 |
| `agent.utils.MessageConvertUtil` | Msg ↔ A2A Message 转换 + 流式分块累积 | `a2a::message::MessageConvert`（`StreamingChunkAccumulator` 为**私有**结构体，行为等价） | 已完成 |
| `agent.message.MetadataUtil` | Part 元数据读写/复制 | `a2a::message::MessageConstants::{build_type_metadata, metadata_value}` + `a2a::message` 私有 `part_metadata`/`part_metadata_mut` | 部分完成（无独立工具类，元数据读写已内联） |
| `agent.card.AgentCardResolver` | AgentCard 解析接口 | `a2a::card::AgentCardResolver` (trait) | 已完成 |
| `agent.card.FixedAgentCardResolver` (+Builder) | 固定卡片解析 | `a2a::card::{FixedAgentCardResolver, FixedAgentCardResolverBuilder}` | 已完成 |
| `agent.card.WellKnownAgentCardResolver` | `/.well-known/agent-card.json` 拉取 | `a2a::card::{WellKnownAgentCardResolver, WellKnownAgentCardResolverBuilder, CardFetcher, ReqwestCardFetcher}` | 已完成 |
| `agent.A2aAgent` / `A2aAgentConfig` | 远程 A2A 智能体与配置 | `a2a::agent::{A2aAgent, A2aAgentBuilder, A2aAgentConfig, A2aAgentConfigBuilder}`（`crates/agentscope-extensions-a2a-client/src/agent.rs`）+ `a2a::client::{A2aClient, SendResult, JsonRpcTransport, ReqwestJsonRpcTransport}` | **已完成（2026-10-31，见 history/PROTOCOL__MIGRATION_HISTORY.md）**——`A2aAgent` 实现 core `Agent` + `CallableAgent`（`exchange`/`exchange_stream`/`interrupt`/`agent_state`/`toolkit`），`A2aAgentConfig` 建 `transport` + `auth_headers` + 轮询旋钮；`clientTransports` 映射为单个可注入 `JsonRpcTransport`，`ClientConfig` 映射为 auth headers |
| `agent.event.*`（ClientEventContext/Handler/Router、Message/Task/TaskUpdate EventHandler） | 流式事件处理 | `a2a::client::{decode_stream_result, parse_sse_stream, is_terminal_state, is_final_status}` + `a2a::result::{task_result_message, status_update_result_message, TaskResultAccumulator}` + `a2a::model::StreamEvent` | **已完成（2026-10-31）**——事件解码 + 终态结果提取（artifacts 优先 / completed 状态消息 / 非 completed 的 state 错误文案）已按 Java 两个 EventHandler 逐条对齐；**函数式替代继承树**（不建 `ClientEventHandler` trait 体系，见 history/PROTOCOL__MIGRATION_HISTORY.md）；`ClientEventContext` 的 hook/增量发射属 core 横切能力，未迁移（无消费者） |
| `agent.message.BaseMediaBlockParser` 等 | Source→FilePart MIME 推导 | `a2a::message::media_part` / `mime_type_from_url`（**私有**） | 已完成 |
| `agent.utils.LoggerUtil` | 日志封装 | 无（Rust 用 `tracing`；本 crate **未声明** `tracing` 依赖，`rg 'tracing::' crates/agentscope-extensions-a2a-client` 零命中） | 设计决定（不迁移） |

### 2.2 a2a-server（`io.agentscope.core.a2a.server.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeA2aServer` / `request.AgentScopeA2aRequestHandler` | JSON-RPC 请求处理 + 服务装配 | `a2a::server::{A2aRequestHandler, router}` + `a2a::server::AgentScopeA2aServer`（`agentscope-extensions-a2a-server/src/{server,transport,registry,executor}.rs`） | **已完成（2026-10-28）**——引导层 `Builder`/`post_endpoint_ready()`/`agent_card_json()`/`transport_wrapper()`/`close()`/`router()` 均已落地；`ServiceLoader` SPI 以 `default_transport_builders()` + `Builder::transport_builder(..)` 替代（Rust 无 SPI 扫描，记为偏差）；`close()` 为 no-op（Rust handler 无线程） |
| `executor.AgentScopeAgentExecutor` | 任务执行编排 | `a2a::server::{AgentRunner, A2aTaskRegistry, AgentOutputProjector}` | **已完成（2026-10-28，2026-10-31 补生命周期对齐）**——blocking（`message/send`）与非 blocking（`message/stream`）双模式已分离；`AgentOutputProjector` 复刻 Java `BaseFluxEventHandler` 的 required-event-type 表；订阅取消在 Rust 由流 drop 表达（无 `Subscription` 对象）；**2026-10-31**：`start_run` 的 session/user 解析对齐 `e6f015e14`（见 history/PROTOCOL__MIGRATION_HISTORY.md） |
| `executor.AgentExecuteProperties` | `completeWithMessage` / `requireInnerMessage` 执行属性 | `a2a::server::AgentExecuteProperties`（builder + getter） | **已完成并接线（2026-10-28）**——`A2aRequestHandler::new_with_properties` 持有；`require_inner_message` 控制 `TOOL_PROGRESS`/`TOOL_RESULT_*_DELTA`/`HINT_BLOCK` 是否投影；`complete_with_message` 控制非阻塞终态是否带 message（阻塞恒定带，与 Java 一致） |
| `executor.runner.{AgentRunner, BaseReActAgentRunner, ReActAgentWithBuilderRunner, AgentRequestOptions}` | runner 体系 | `a2a::server::{AgentRequestOptions, AgentRunner}` + `agentscope-extensions-a2a-server/src/runner.rs`（`TaskAgent`/`TaskAgentFactory`/`BaseReActAgentRunner`/`ReActAgentWithBuilderRunner`） | **已完成**（见 history/PROTOCOL__MIGRATION_HISTORY.md）；**2026-10-31** 对齐 `431031a7a` 的任务生命周期竞态修复：缓存改为「锁外 build + 锁内 check-and-insert」等价 `putIfAbsent`，`CacheGuard` 按实例身份（`Arc::ptr_eq`）删除，见 history/PROTOCOL__MIGRATION_HISTORY.md |
| `server.utils.MessageConvertUtil` | 服务端 Msg↔A2A 转换 | `a2a::server::ServerMessageConvert`（`convert_from_msgs_to_message` / `convert_from_msg_to_message` / `convert_from_content_blocks` / `compact_streaming_chunks` / `convert_from_message_to_msgs`） | 已完成 |
| `card.AgentScopeAgentCardConverter` | AgentDescriptor→AgentCard | `a2a::card::{AgentDescriptor, create_agent_card, ConfigurableAgentCard, TransportProperties}` | 已完成 |
| `registry.{AgentRegistry, AgentRegistryService}` | 智能体注册/发现 | `a2a::server::{AgentRegistry, AgentRegistryService, RegistrationReport}` | **已完成并接线（2026-10-28）**——`AgentRegistry::register(&AgentCard, &[TransportProperties])` + `registry_name()`；`AgentRegistryService` 遍历全部 registry、单个失败不阻断其它（返回 `RegistrationReport{succeeded,failed}`，对应 Java 的日志语义）；由 `AgentScopeA2aServer::post_endpoint_ready()` 触发。NACOS 等发现后端直接实现该 trait 即可复用 |
| `transport.*` / `transport.jsonrpc.*` | JSON-RPC 传输与包装 | `a2a::server::transport::{TransportWrapper, TransportWrapperBuilder, JsonRpcTransportWrapper(+Builder), JsonRpcTransportMetadata, DeploymentProperties(+Builder), CustomTransportProperties, default_transport_builders}` + `a2a::card::TransportProperties` | **已完成并接线（2026-10-28）**——`TransportResponse::{Json,Stream}` 对应 Java 的 `String`/`Flux<String>`；axum `router()` 的 `POST /` 已改走 `JsonRpcTransportWrapper`（原始 body + 解析错误映射 + 流式 SSE）；`DeploymentProperties` 默认 host 取本机 IP（UDP connect 技巧，离线），端口缺失报 `DeploymentError::MissingPort`；`CustomTransportProperties` 由 `Builder::with_custom_transport(..)` 在 build 时解析 |
| `constants.A2aServerConstants` | `ContextKeys.IS_STREAM_KEY` | `a2a::server::server_constants::IS_STREAM_KEY`（已声明但**当前无消费点**） | 已完成 |
| `utils.NetworkUtils` | 网络工具 | 未单独迁移（并入 card/client） | 设计决定 |

### 2.3 agent-protocol（`io.agentscope.extensions.agentprotocol.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentProtocolConstants` | 常量/保留键 | `agent_protocol::AgentProtocolConstants` | 已完成 |
| `AgentProtocolProperties` | 配置属性 | `agent_protocol::{AgentProtocolProperties, AgentProtocolPropertiesBuilder}` | 已完成 |
| `AgentRequest` | 请求体 | `agent_protocol::AgentRequest` | 已完成 |
| `RuntimeContextCustomizer` | 运行时上下文定制 | `agent_protocol::{RuntimeContextCustomizer, FlattenCustomizer}` | 已完成 |
| `AgentFactory` | 智能体工厂 | `agent_protocol::{AgentFactory, FixedAgentFactory}` | 已完成 |
| `AgentProtocolEventBus` / `AgentProtocolTaskEventBus` | 事件总线/订阅 | `agent_protocol::{AgentProtocolEventBus, AgentProtocolTaskEventBus, EventSubscription}` | 已完成 |
| `AgentProtocolTaskStore` | 任务存储/HITL 确认 | `agent_protocol::{AgentProtocolTaskStore, InMemoryProtocolTaskRepository, ConfirmResult, to_confirm_results, extract_pending_confirms, snapshot_value, AWAITING_CONFIRM_SENTINEL}` | 已完成 |
| `WorkspaceProtocolTaskRepository` / `ProtocolTaskRepository` | 任务持久化仓库 | `agent_protocol::{ProtocolTaskRepository, InMemoryProtocolTaskRepository, WorkspaceProtocolTaskRepository}` | **已完成并接线（2026-10-28）**——`WorkspaceProtocolTaskRepository` 复用 harness `WorkspaceManager::write_task_record_json`/`read_task_record_json`/`list_task_records_json`，落 `agents/{PROTOCOL_AGENT_ID}/tasks/{PROTOCOL_SESSION_ID}.json`；`AgentProtocolTaskStore::with_workspace_task_store(..)` 让 `task_store_path` 真正生效（此前无消费方） |
| `AgentProtocolController` | HTTP 控制器（Java 实际路径 `/tasks`、`/tasks/{taskId}`、`/tasks/{taskId}/wait`、`/tasks/{taskId}/cancel`、`/tasks/{taskId}/events`、`/tasks/{taskId}/resume`；**Java 侧无 `/api` 前缀**） | `agent_protocol::router` (axum，路径同为 `/tasks...`，未含 `/api` 前缀） | 已完成 |
| `AgentProtocolAutoConfiguration` | Spring 自动装配 | 不适用（Rust 无 Spring） | 设计决定（不迁移）|

### 2.4 agui（`io.agentscope.core.agui.*`）— **已迁移**
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `event.AguiEvent` / `AguiEventType` / `AguiEvents` | AG-UI 事件模型（~60KB） | `agui::event::{AguiEvent, AguiEventType, AguiEvents, JsonPatchOperation, Interrupt, RunFinishedOutcome}` | 已完成 |
| `model.AguiMessage` / `AguiToolCall` / `AguiFunctionCall` | 消息与工具调用模型 | `agui::model::{AguiMessage, AguiToolCall, AguiFunctionCall}` | 已完成 |
| `model.{MessageContent, InputContent*, InputContentSource*}` | content 联合类型与输入部件 | `agui::model::{MessageContent, InputContent, InputContentSource}` | 已完成 |
| `model.{RunAgentInput, AguiContext, AguiResume, AguiTool, ToolMergeMode}` | 请求/上下文/续跑模型 | `agui::model::{RunAgentInput, AguiContext, AguiResume, AguiTool, ToolMergeMode}` | 已完成 |
| `converter.{AguiMessageConverter, AguiStateConverter, AguiToolConverter}` | 核心转换 | `agui::converter::{message, state, tool}`（`AguiMessageConverter`/`AguiStateConverter`/`AguiToolConverter`） | 已完成 |
| `adapter.{AguiAgentAdapter, AguiAdapterConfig, ToolInjection}` + `adapter.strategy.*` | 智能体适配与事件转换策略 | `agui::adapter::{agent_adapter, config}`（**`tool_injection.rs` 已按 §4.4 GAP-17 删除**）+ `agui::adapter::strategy::{context, converters, enricher, registry}` | 已完成 |
| `processor.{AguiRequestProcessor, AguiResumeCoordinator}` | 请求处理/断点续跑 | `agui::processor::{request_processor, resume_coordinator, resolver}`（含 `AgentResolver`/`ProcessResult`/`AguiRequestProcessorBuilder`） | 已完成 |
| `encoder.AguiEventEncoder` | SSE 编码 | `agui::encoder::AguiEventEncoder` | 已完成 |
| `registry.AguiAgentRegistry` / `runtime.*` | 注册与运行时上下文 | `agui::registry::{AguiAgentRegistry, AgentFactory}` + `agui::runtime::{AguiRuntimeContextRequest, AguiRuntimeContextResolver, AguiRequestBodyParser, AguiTransport}` | 已完成 |
| `AguiUtil` / `AguiInterruptConstants` / `AguiException` | 工具/常量/异常 | `agui::constants`（`METADATA_*`/`*_INTERRUPT_REASON`）+ `agui::error::{AguiError, AguiResult}` | **已完成（2026-10-28）**（`AguiUtil` 的 `isHarnessAgent`/`asReActAgent` 由 `agui::adapter::AguiStreamableAgent` trait 替代——已对 `ReActAgent` 与 `HarnessAgent` 各实现一次；该 trait 新增 `interrupt_session(&str) -> bool` 承载 Java 的 `asReActAgent(...).interrupt(ctx)` 语义；`ProcessResult::interrupt(thread_id)` 已迁移） |

> **接线复核**：以上全部子模块已在 `src/agui/mod.rs` 声明 + re-export，且 `src/lib.rs` 有 `pub mod agui;`；均已随 crate 编译并产生测试（防「落盘但从未编译」）。

### 2.5 chat-completions-web（`io.agentscope.core.chat.completions.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `model.*`（ChatCompletionsRequest/Response/Chunk、ChatMessage、ChatChoice、ToolCall、OpenAITool*） | OpenAI 兼容 wire 模型 | `chat_completions::model::*` | 已完成 |
| `converter.{ChatMessageConverter, OpenAIToolConverter}` | Msg/Tool ↔ OpenAI 模型转换 | `chat_completions::{converter, builder}` | 已完成 |
| `builder.ChatCompletionsResponseBuilder` | 响应构造 | `chat_completions::builder::ChatCompletionsResponseBuilder` | 已完成 |
| `streaming.ChatCompletionsStreamingAdapter` | 流式增量适配 | `chat_completions::streaming::ChatCompletionsStreamingAdapter` | 已完成 |

### 2.6 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 状态 |
|---|---|---|---|
| `io.a2a.spec.*` (a2a-java-sdk) | A2A wire 模型 | 本 crate `a2a::model` 自建（serde，可空字段） | 已完成 |
| Jackson (`ObjectMapper`) | JSON (反)序列化 | `serde` + `serde_json`（workspace） | 已完成 |
| JUnit 5 / Mockito | 测试 | `#[test]`/`#[tokio::test]` + 内存 fake | 已完成 |
| Spring Web / AutoConfiguration | HTTP + DI | `axum` 0.7（`json,tokio,http1,query`） | 已完成（DI 不做） |
| Spring `SseEmitter` | SSE 流 | `axum` SSE（`agui::encoder::AguiEventEncoder::to_sse_event`）+ `agent_protocol` 手写 `text/event-stream` 帧 + a2a `A2aRequestHandler::stream_responses` | 部分完成。**已完成（2026-10-29 勘误）**：原记「`message/stream` 返回一次性数组非 SSE 增量」，实际 a2a 服务端已改真 SSE（`is_streaming_method` 只认 `message/stream`，每帧 `data: <json-rpc response>`）；依据 §3.2 / 见 history/PROTOCOL__MIGRATION_HISTORY.md #5 / 用例 `message_stream_is_served_incrementally_over_a_transport`。仍留：`tasks/resubscribe` 未流式（回 `-32601`，见 history/PROTOCOL__MIGRATION_HISTORY.md 第 6 条） |
| `HttpClient` (Java 11) | A2A 客户端 | `reqwest`（workspace 0.13）+ `JsonRpcTransport` trait | 已完成 |
| `java.net.URL` + `Files.probeContentType` | URL 扩展名→MIME | `mime_type_from_url`（内置扩展名表） | 已完成（受控子集） |
| `java.util.UUID` | 生成文件名 | `uuid`（workspace） | 已完成 |
| AG-UI 协议类型 / OpenTelemetry（**Java `agentscope-extensions-agui/pom.xml` 实测仅依赖 `agentscope-core`（provided/optional），无 ag-ui / opentelemetry 依赖**） | agui 事件 | 无 | 设计决定（不迁移）——原「待办」基于错误前提，Java 侧不存在该依赖 |

## 3. 详细缺口清单

### 3.1 P0（核心链路）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A wire 模型 | `io.a2a.spec.*` | 已完成 | `a2a/model.rs`（27400B / 733 行），10 单测 |
| A2A Msg↔Part 转换 | `a2a-client/.../message/*` | 已完成 | `a2a/message.rs`（37200B / 834 行），16 单测 |
| A2A 卡解析 | `a2a-client/.../card/*` | 已完成 | `a2a/card.rs`（27149B / 685 行），3 单测 |
| A2A 客户端 | `a2a-client/.../A2aAgent` | **已完成（2026-10-31）** | `a2a/client.rs`（451 行，9 单测）+ `a2a/agent.rs`（`A2aAgent`/`A2aAgentConfig` 门面，实现 core `Agent` + `CallableAgent`）+ `a2a/result.rs`（终态结果提取）；门面用例 `call_returns_the_task_artifacts`、`a_working_task_is_settled_by_polling`、`interrupt_cancels_the_running_task`、`an_unresolvable_card_fails_fast_and_names_the_agent` |
| A2A 服务端 + JSON-RPC 路由 | `a2a-server/*` | **已完成（2026-10-28）** | `a2a/server` 拆为 `lib.rs`（939→ 约 900 行 handler/转换）+ `server.rs`/`transport.rs`/`registry.rs`/`executor.rs`；15 个旧测试全保留，新增 31 个（合计 46） |
| agent-protocol 任务生命周期 + HTTP 路由 | `agent-protocol/*` | **已完成（2026-10-28）** | `agent_protocol`（`lib.rs` + `error.rs` + `workspace_repository.rs`，共 2097 行）；24 旧测试 + 5 新增 = 29；workspace 落盘仓库已接线 |
| chat-completions-web 模型/转换/构造/流式 | `chat-completions-web/*` | 已完成 | `chat_completions/`（model 545 行 / converter 307 / builder 280 / streaming 349），35 单测 |
| agui 事件模型 | `agui/event/*`（`AguiEvent`） | **已完成** | `agui/event.rs`（63147B / 1497 行），含 12 单测（事件类型名/序列化/容忍未知类型/RFC 6902 patch payload） |
| agui 消息/内容模型 | `agui/model/*` | **已完成** | `agui/model.rs`（31798B / 805 行）（`AguiMessage`/`MessageContent`/`InputContent`/`InputContentSource`/`AguiToolCall`/`AguiFunctionCall`），19 单测（含 2026-10-31 补的角色工厂与 null 省略用例） |
| agui 转换/适配/编码/处理/注册 | `agui/{converter,adapter,encoder,processor,registry,runtime}/*` | **已完成** | `agui/converter/{message,state,tool}.rs`、`agui/adapter/{agent_adapter,config}.rs` + `strategy/{context,converters,enricher,registry}.rs`、`agui/encoder.rs`、`agui/processor/{request_processor,resume_coordinator,resolver}.rs`、`agui/registry.rs`、`agui/runtime.rs`、`agui/constants.rs`、`agui/error.rs`；均已在 `mod.rs` 声明（`tool_injection.rs` 已按 §4.4 GAP-17 删除） |
| agui 请求/上下文/续跑模型 | `agui/model/{RunAgentInput,AguiContext,AguiResume,...}` | **已完成** | `agui/model.rs`（805 行）含 `RunAgentInput`/`AguiContext`/`AguiResume`/`AguiTool`/`ToolMergeMode`（`ToolMergeMode` 复用 core 枚举） |

### 3.2 P1（重要）
> **2026-10-28 状态**：原 7 项 P1（服务端引导层 / `AgentExecuteProperties` / registry 服务发现 / transport 包装层 / `message/stream` SSE 增量 / agent-protocol 落盘仓库 / agui `ProcessResult.interrupt`）**全部关闭**，详见 history/PROTOCOL__MIGRATION_HISTORY.md；仅「A2A 服务端 runner 体系 1:1」与本批无关，仍为部分完成。
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A 服务端 runner 体系 1:1 | `executor.runner.*` | **已完成（2026-10-30，见 history/PROTOCOL__MIGRATION_HISTORY.md）** | 以 `AgentRunner` trait + `A2aTaskRegistry` 抽象；**仍缺**：`BaseReActAgentRunner`（按 `taskId` 缓存 `ReActAgent` + `stop()` 调 `interrupt()` + 流结束移除缓存）、`ReActAgentWithBuilderRunner`（`ReActAgent.Builder` → `buildReActAgent()`）；Rust `AgentRunner` 现有 `agent_name`/`agent_description`/`stream_events`/`stream_events_incremental`/`stop`，但**无 per-task agent 缓存与自动 interrupt 语义**。`stream_events_incremental` 为 2026-10-28 新增（默认 `None`，由 handler 回放 `stream_events`），是接 core 流式 agent 的挂点。→ **2026-10-30 已落地**：上述「仍缺」清单全部实现（`src/runner.rs`，见 history/PROTOCOL__MIGRATION_HISTORY.md） |
| A2A 智能体注册与发现 | `registry.*` | **已完成（2026-10-28）** | `a2a::server::{AgentRegistry, AgentRegistryService, RegistrationReport}`（`agentscope-extensions-a2a-server/src/registry.rs`）；`register(agent_card, transports)` 遍历全部 registry、单点失败隔离；`AgentScopeA2aServer::post_endpoint_ready()` 触发。测试 `register_reaches_every_registry_with_the_served_transports` / `one_failing_registry_never_blocks_the_others` / `no_transport_means_no_registry_call` / `post_endpoint_ready_registers_with_every_registry_once` |
| A2A 传输包装层 | `transport.*` | **已完成（2026-10-28）** | `src/transport.rs`：`TransportWrapper`（`async fn handle_request(body, headers, metadata) -> TransportResponse`，Java 同步版因 Rust handler 为 async 而改 async，已记偏差）、`TransportWrapperBuilder`（去掉 Java 的 `Executor` 参数——流在调用方 async 任务上跑）、`JsonRpcTransportWrapper`+`Builder`、`JsonRpcTransportMetadata`、`DeploymentProperties`+`Builder`（默认 host=本机 IP，端口缺失报错）、`CustomTransportProperties`（由 `Builder::with_custom_transport` 在 build 时 `set_deployment_properties` 后 `to_transport_properties`）。axum `POST /` 已改走 wrapper。测试 8 个（含 `non_streaming_requests_return_one_json_response` / `streaming_requests_return_a_stream_of_json_responses` / `custom_transport_properties_are_resolved_at_build_time`） |
| A2A 服务端引导层 | `AgentScopeA2aServer` | **已完成（2026-10-28）** | `src/server.rs`：`AgentScopeA2aServer` + `Builder`（`agent_card`/`with_transport`/`transport_builder`/`deployment_properties`/`deployment_port`/`with_agent_registry`/`with_custom_transport`/`agent_execute_properties`/`task_registry`），`build()` 归一化 transport→派生 card→建 handler→建 wrapper（失败即丢弃该 transport）；`transport_wrapper`/`require_transport_wrapper`/`agent_card_json`/`post_endpoint_ready`/`handle_request`/`router`/`close`。`A2aServerBuildError::{NoTransportProperties, NoAvailableTransport}` 对齐 Java 两条 `IllegalArgumentException` 文案。`ServiceLoader` → `default_transport_builders()`（偏差：Rust 无 SPI）。测试 7 个 |
| A2A 执行属性 | `executor.AgentExecuteProperties` | **已完成（2026-10-28）** | `src/executor.rs`：`AgentExecuteProperties{complete_with_message, require_inner_message}` + builder；`AgentOutputProjector` 复刻 `generateRequiredEventTypes`/`convertToResponseMessage`（TEXT/THINKING delta 恒投影；TOOL_PROGRESS/TOOL_RESULT_TEXT_DELTA/TOOL_RESULT_DATA_DELTA/HINT_BLOCK 需 `require_inner_message`；`AGENT_RESULT` 仅在该 run 无流式输出时兜底）。**偏差**：Rust `AgentRunner` 默认只有消息级契约，故保留 `AGENT_RESULT` 兜底分支（Java 无此事件形态）。测试 `inner_events_need_require_inner_message` / `agent_result_falls_back_only_when_nothing_was_streamed` / `require_inner_message_gates_tool_output` / `complete_with_message_only_applies_to_non_blocking_runs` |
| agent-protocol workspace 落盘仓库 | `WorkspaceProtocolTaskRepository` | **已完成（2026-10-28）** | `agentscope-extensions-agent-protocol/src/workspace_repository.rs`：复用 harness `TaskRecord` + `WorkspaceManager`（**未另造类型**；harness `TaskRepository` trait 是会话级后台子智能体模型，与协议侧全局 `task_id` 查询语义不同，故 `ProtocolTaskRepository` 保持既有形状并在 tracker 记录理由）；`save/find` 同步 trait 实现用 `futures::executor::block_on` 驱动 workspace 的 async 写读（本地盘 `std::fs` + runtime 无关的 `tokio::sync` 锁，无需 reactor），另提供 `save_async`/`find_async`/`list_async`。接线：`AgentProtocolTaskStore::with_workspace_task_store(..)` 消费 `AgentProtocolProperties::resolved_task_store_path()`。测试 5 个 |
| agui 运行中断入口 | `AguiRequestProcessor.ProcessResult#interrupt(threadId)` | **已完成（2026-10-28）** | `agui::processor::ProcessResult::interrupt(&self, thread_id) -> bool`：经 `AguiStreamableAgent::interrupt_session`（新增 trait 方法，默认 `interrupt()` 并返回 false；`ReActAgent`/`HarnessAgent` 覆写为各自的 `interrupt_session`）。`ProcessResult` 新增 `agent: Option<Arc<dyn AguiStreamableAgent>>`（**偏差**：Rust 在 agent 解析失败时以 `RUN_ERROR` 事件收尾、无 agent 可挂，故为 `Option`；Java 先解析再建惰性 Flux，恒有值）。测试 `interrupt_targets_the_thread_session` / `interrupt_falls_back_for_agents_without_a_session_scope` / `process_exposes_the_resolved_agent_for_interruption` |
| SSE 流式响应 | Spring `SseEmitter` / `Flux` | **已完成（2026-10-28）** | a2a 服务端新增 `A2aRequestHandler::stream_responses(request) -> BoxStream<'static, JsonRpcResponse>`（`agentscope-extensions-a2a-server/src/lib.rs` 的 `StreamPhase` 状态机：首帧 `working`、逐事件 `artifact-update`、终帧 `completed`；`AgentRunner::stream_events_incremental(...)` 有则直连、无则回放缓冲结果）；`is_streaming_method` **仅**覆盖 `message/stream`（Java 的 wrapper 也接受 `tasks/resubscribe`，但 Java 有 per-task 事件队列；Rust handler 无队列，订阅只能用重跑实现，故保持非流式并回 `-32601`，已记偏差）；axum `POST /` 对流式方法返回 `text/event-stream`（每帧 `data: <json-rpc response>`，对齐 Java `Flux<String>` + Spring SSE）。测试 `stream_responses_emits_before_the_run_finishes`（用 channel 驱动，缓冲实现必然挂死）/ `message_stream_relays_the_run_incrementally` / `message_stream_is_served_incrementally_over_a_transport` / `resubscribe_is_not_streamed_and_reports_method_not_found` |
| agui 适配器 + 转换策略 | `agui/adapter/*` | **已完成** | `AguiAgentAdapter`（981 行）/`AguiAdapterConfig`（302 行，12 个配置项与 Java 一一对应）+ 策略层（`context` 603 行/`converters` 1489 行/`enricher` 129 行/`registry` 334 行）；11 个内建 converter 与 Java `strategy/*` 11 个类一一对应 |
| agui 请求处理/续跑 | `agui/processor/*` | **已完成** | `AguiRequestProcessor`（638 行）/`AguiResumeCoordinator`（600 行）/`AgentResolver`；**注**：`AguiRequestProcessorBuilder` 未暴露 Java 的 `adapterFactory` 注入点（Rust 侧 `AguiAgentAdapterFactory` trait 已存在但 processor 未接线）。→ **2026-10-30 已接线**（`AguiRequestProcessorBuilder#adapter_factory`，见 history/PROTOCOL__MIGRATION_HISTORY.md） |
| `TOOL_PROGRESS` 事件投影（`707400eb`） | `agui/adapter/strategy/ToolResultEventConverter` | **已完成（2026-10-20，有意不映射）** | core 已发独立 `TOOL_PROGRESS` 事件；AG-UI 侧按 Java 既定行为**显式丢弃**（Java 注释：AG-UI 没有工具进度通道，故丢弃而非透传为原始事件），Rust 用带注释的 no-op 分支对齐，**未新增任何 AG-UI 事件**。原先进度借 `TOOL_RESULT_DATA_DELTA` 透传导致的「进度被当成工具结果」风险已消除 |
| `AguiResume.reason`（`c0d03ccea`） | `AguiMessageConverter#RESUME_PAYLOAD_REASON` | **已完成（2026-10-20）** | `AguiResume` 增 `reason: Option<String>`（`serde(default)` + `skip_serializing_if`，旧客户端缺键可反序列化）；`cancelled` 且带 reason 时，`resume_content` 用它作工具结果文本，并写入 `ConfirmResult.message`；无 reason 时保留历史默认文案 |
| COMPLETED 任务优先取 artifacts（`6e1ee68d7`） | `A2aAgent.call()` / `TaskEvent` 结果提取 | **已完成（2026-10-31，见 history/PROTOCOL__MIGRATION_HISTORY.md）** | Java：COMPLETED 任务若带非空 output artifacts，**优先返回 artifacts**；无 artifacts 才回落 status message；失败终态仍保持「status message 优先于残留 artifacts」。Rust：新增 `a2a::result::{task_result_message, status_update_result_message, TaskResultAccumulator}`（`crates/agentscope-extensions-a2a-client/src/result.rs`）逐条复刻两个 EventHandler；用例 `completed_task_prefers_artifacts_over_the_status_message` / `completed_task_falls_back_to_the_status_message` / `failed_task_keeps_the_status_message_ahead_of_partial_artifacts` / `final_update_keeps_the_completed_status_message_without_artifacts`（00e4a66b6）/ `streamed_events_accumulate_artifacts_before_the_terminal_update` |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A MIME 探测全表 | `Files.probeContentType` | **已完成（设计决定，不迁移；2026-10-31）** | Java 走 JVM/操作系统探测（结果随平台变化，且可返回 `null`）；Rust 为保证离线可测与跨平台一致，保留受控扩展名表（png/jpg/jpeg/gif/webp/bmp/mp3/wav/ogg/mp4/webm/mov，`message.rs::mime_type_from_url`）并对 `media_type` 做前缀校验，未知扩展名回退到 block 的 primary type（与 Java `getPrimaryType` 一致）。影响：Java 能识别的部分冷门类型（如 `svg`/`heic`）在 Rust 侧回落为 `image`/`audio` 等 primary type；wire 仍合法 |
| A2A 客户端事件 Handler 类体系 | `agent.event.*` | **已完成（函数式替代；2026-10-31）** | 不建 Java 的 `ClientEventHandler`/`Router` 继承树（无可变 context 对象、无 hook 宿主），改由 `decode_stream_result`/`parse_sse_stream` 解码 + `result::{task_result_message, status_update_result_message, TaskResultAccumulator}` 求终态结果，两者覆盖 Java `TaskEventHandler`/`TaskUpdateEventHandler` 的全部结果分支（见 history/PROTOCOL__MIGRATION_HISTORY.md 用例）。`ClientEventContext` 的 Pre/PostReasoning hook 与 `AgentEventEmitter` 增量发射属 core 横切能力，本 crate 不迁移（无消费者；宿主可在自己的 event loop 中发事件） |
| agui 工具/常量/异常 | `AguiUtil` 等 | **已完成（2026-10-28）** | `agui::constants`（中断元数据/原因常量）+ `agui::error::{AguiError, AguiResult}`（覆盖 Java `AguiException` 的 `AgentNotFound`/`Encoding` 两个子类语义）；`AguiUtil` 由 `AguiStreamableAgent` trait 替代，`ProcessResult.interrupt` 已迁移（见 §3.2） |
| agui transport 枚举粒度 | `AguiRuntimeContextRequest.Transport`（MVC/WEBFLUX/CUSTOM） | **已完成（设计决定，不迁移；2026-10-31）** | 取证：全仓只有 `agentscope-agui-spring-boot-starter` 的 `AguiMvcController`/`AguiWebFluxHandler` 会 set `MVC`/`WEBFLUX`，其余代码（含 `AguiRuntimeContextResolver` 消费方）均不分支这个值（仅测试断言）。Rust 无 Spring starter crate，宿主传输为 axum/hyper → `AguiTransport::{Custom, Http}` 表达同一信息；若宿主需要区分线路，用自己的枚举包一层即可 |
| Java `AguiMessage` 工厂方法族 | `AguiMessage.userMessage/assistantMessage/systemMessage/toolMessage/...` | **已完成（2026-10-31）** | 在既有 `user_message`/`user_message_blocks`/`blocks_message`/`reasoning_message` 基础上补齐 `assistant_message`/`system_message`/`tool_message`（`agui/model.rs`），与 Java 9 个工厂一一对应（`textMessage`/`blocksMessage` 即既有 `blocks_message`）；用例 `message_factories_fill_the_role_specific_fields` |
| Spring AutoConfiguration | `*AutoConfiguration` | **已完成（设计决定，不迁移）** | Rust 无 DI 容器；配置由各 crate 的 builder/构造参数显式传入 |
| `NetworkUtils` | `a2a-server/utils` | **已完成（设计决定，不迁移）** | 逻辑并入 card/client，无独立工具类必要 |
| `bytes` 依赖 | `Cargo.toml` `[dependencies]` | **作废（2026-10-31 取证）** | 实测 6 个 crate 的 `Cargo.toml` 均**无** `bytes` 声明（`rg 'bytes' crates/agentscope-extensions-{protocol,a2a-client,a2a-server,agent-protocol,agui,chat-completions-web}/Cargo.toml` 零命中），本行源自 crate 拆分前的旧状态，无需清理 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **Rust 产物（2026-10-28，crate 已拆分，见 history/PROTOCOL__MIGRATION_HISTORY.md）**：
  - `agentscope-extensions-protocol`（facade）1 文件 33 行；
  - `agentscope-extensions-a2a-client` 6 文件 2414 行（`card`/`client`/`error`/`lib`/`message`/`model`）；
  - `agentscope-extensions-a2a-server` 6 文件 3778 行（`lib`/`card`/`server`/`transport`/`registry`/`executor`）；
  - `agentscope-extensions-agent-protocol` 3 文件 2097 行（`lib`/`error`/`workspace_repository`）；
  - `agentscope-extensions-agui` 24 文件 11076 行；
  - `agentscope-extensions-chat-completions-web` 5 文件 1507 行；
  - 合计 **45 文件 / 20905 行**。
- **测试**：**341 个**（`#[test]` 317 + `#[tokio::test]` 24）。分布见 §1 与 见 history/PROTOCOL__MIGRATION_HISTORY.md。
- **验证（2026-10-28 实测，单条命令 -j 2）**：
  ```
  cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client \
    -p agentscope-extensions-a2a-server -p agentscope-extensions-agent-protocol \
    -p agentscope-extensions-agui -p agentscope-extensions-chat-completions-web
  -> 39 / 47 / 29 / 191 / 35 / 0 passed；合计 341 passed; 0 failed; 0 ignored
  ```
- **本批（2026-10-28 P1×7 关闭）交付**：见 history/PROTOCOL__MIGRATION_HISTORY.md（Java 依据、改动文件、测试名、反向验证）。
- **历史批次（agui 整包迁移 + 收尾，2026-10-21）**：
  - agui 整包落盘（~8800 行）；**首个子智能体因网络错误中断**，留下 5 个编译错误 + 11 个失败用例且 **tracker 完全未回填**。
  - 汇总方修复 5 个编译错误：①`tool_injection.rs` 的 `AgentTool`→core `Tool`；②core `AgentEventType` **不实现 `Hash`** → `registry.rs` 转换表由 `HashMap` 改 `Vec<(AgentEventType, Arc<dyn AgentEventConverter>)>` + 线性查找；③事件流 `then(async{...}).flatten()` 替代 async `flat_map`（E0277）；④`build_tool_call_interrupt(&result, …)` 补引用（E0308）；⑤补 3 处测试模块 import。
  - 汇总方修复剩余失败用例至全绿（均已在同文件内修正，未删测试/未 `#[ignore]`）：`block_message_id` 无显式 id 时按事件类型推导块类型（`-text`）、`RunFinished.outcome` 与 Java 同为 NON_NULL 语义（断言修正）、ToolResult 数据块换行分隔断言改走两个 data 块、registry native 模式断言改为 START+CONTENT 两事件、`ToolInjection` 复原断言改为「原实例复原」而非「非 external」、`headers().len()` 随大小写敏感存储改为 3、`request_processor` 两个用例补上 agentId 与 active-run 前置条件。
- **会话内根因修复（早期）**：
  - `chat_completions/streaming.rs` E0700、`builder.rs` 单测非法 `Msg`、`agui` 接线（`lib.rs` 缺 `pub mod agui;`）、`event.rs` `DeserializeOwned` 约束、`a2a/message.rs` 断言写错（详见 §4.3）。
- **已知问题**：build 存在若干 unused-import / unused-const 警告（`FunctionCallStream`、`HarnessAgentBuilder`、`SdkResult`、`TextBlock`、`SandboxState`、`WorkspaceSpec`、`ExecResult`、`BUILT_IN_EVENT_TYPES` 等），不影响编译与测试；属既有状态，本批未动。

### 4.2 下一步与阻塞
1. **P0/P1 全部落地**（见 §3.1/§3.2、见 history/PROTOCOL__MIGRATION_HISTORY.md、见 history/PROTOCOL__MIGRATION_HISTORY.md、见 history/PROTOCOL__MIGRATION_HISTORY.md），无 P0 阻塞。
2. **无剩余 P1/P2 任务事项（2026-10-31）**：`A2aAgent`/`A2aAgentConfig` 门面、客户端 artifacts 优先提取、客户端事件体系、`AguiTransport` 粒度、`AguiMessage` 工厂、MIME 全表、`bytes`/`tracing` 均已按 见 history/PROTOCOL__MIGRATION_HISTORY.md 口径收敛（实现或带源码证据的设计决定）。
3. **仍为设计决定（不迁移）的偏差**（均有 见 history/PROTOCOL__MIGRATION_HISTORY.md / 见 history/PROTOCOL__MIGRATION_HISTORY.md 取证，不计入任务事项）：`ServiceLoader`→显式 builder、`TransportWrapper::handle_request` async 化、`AGENT_RESULT` 兜底投影、`ProcessResult.agent: Option`、`tasks/resubscribe` 不流式、MIME 受控表、`AguiTransport` MVC/WEBFLUX 合并、`ClientEventHandler` 继承树→函数式、Spring AutoConfiguration / `NetworkUtils` / `LoggerUtil`。
4. **跨模块需求**：本轮**未改 core/harness**。`A2aAgent` 的四条 Java 卡点（`agent_state_for(session)`、可等待 interrupt、toolkit 视图、远端结构化输出）全部以 **a2a-client crate 内的适配层**绕过：单槽 `AgentState` + `tokio::sync::Mutex` 串行化 + 同步 `interrupt()` 置标志、由运行循环在下个轮询边界发 `tasks/cancel` + 空 `Toolkit` + `call_with_schema` 显式报错（契约见 history/PROTOCOL__MIGRATION_HISTORY.md）。若后续希望门面支持多会话或可等待取消，需 core 提供 `Agent::agent_state_for(session)` 与 async 中断通道 —— **仅记录，不阻塞**。
5. **依赖变更（2026-10-31）**：`crates/agentscope-extensions-a2a-client/Cargo.toml` 增加 `async-trait = { workspace = true }`（实现 core `CallableAgent` 所需；根 `[workspace.dependencies]` 已登记）。`bytes` 已确认在 6 个 crate 中均未声明（§3.3）。

### 4.3 失败测试根因（已修复）
- **用例**：`a2a::message::tests::media_part_mime_falls_back_to_primary_type_for_unknown_extension`（`src/a2a/message.rs:765`）。
- **现象**：`assertion failed: part_to_content_block(&part).is_none()`。
- **根因**：**测试断言写错（非实现 bug）**。该测试构造 `AudioBlock(URLSource("https://example.com/sound.unknownext"))`，`mime_type_from_url` 因扩展名未知回退为 `media_type="audio"`，序列化后 `file.mimeType == "audio"`。回程 `part_to_content_block → file_part_to_block` 取 primary type 为 `"audio"`（无 `/` 时返回整串），命中 `TYPE_AUDIO` 分支 → 正常还原为 `AudioBlock`。Java `FilePartParser#getPrimaryType` 对无 `/` 的串返回原串，`switch case MessageConstants.BlockContent.TYPE_AUDIO` 即 `"audio"` 同样还原为 `AudioBlock`，故 **Java 语义下必然可映射回**，原断言 `is_none()` 与 Java 及测试名相悖。
- **修法**：将断言改为断言可还原为 `ContentBlock::Audio` 且 URL 源为原址（与 Java `FilePartParser` 一致）；未删断言、未删功能、未 `#[ignore]`。
- **变更文件**：`crates/agentscope-extensions-protocol/src/a2a/message.rs`（仅测试体）。
- **Java 依据**：`a2a-client/src/main/java/io/agentscope/core/a2a/agent/message/FilePartParser.java`（`getPrimaryType` + `switch ... TYPE_AUDIO`）。

### 4.4 GAP-17：AG-UI 工具视图对齐 Java（2026-10-21）
- **裁定**：用户「对齐 Java」。Java `3c1c29c01` 把「运行时注入/回滚 toolkit」改为「不可变 per-call `ToolRequestConfig`」，并删除 `agui.adapter.ToolInjection` 与 `agui.model.ToolMergeMode`（后者上移 core）。
- **改动**：
  - `agui/model.rs`：删除本地 `ToolMergeMode`，改 `pub use agentscope_core::tool::ToolMergeMode`（`agui::model::ToolMergeMode` 路径不变，wire 名从 `FRONTEND_ONLY/MERGE_FRONTEND_PRIORITY` 变为 `EXTERNAL_ONLY/MERGE_EXTERNAL_PRIORITY`；该枚举不在 AG-UI 线上协议里，无 wire 兼容负担）。
  - `agui/adapter/tool_injection.rs`：**整文件删除**；`AguiRunStream` 不再持有 scope，`close()` 移除（不再有 registry 需要复原）。
  - `agui/adapter/agent_adapter.rs`：`buildToolRequestConfig` 等价逻辑（`tool_request_config()`）把前端工具转成 core `ToolRequestConfig`，经 `Extensions` 的 `TOOL_REQUEST_CONFIG_CONTEXT_KEY` 随新增 trait 方法 `stream_events_with_extensions` 下传；共享 `Toolkit` 全程只读。
  - `agentscope-harness`：`HarnessAgent::stream_events_with_extensions` 透传到内嵌 `ReActAgent`。
  - `AguiAdapterConfig` 默认 `MergeExternalPriority`（语义等同旧的 `MergeFrontendPriority`）。
- **测试**：`frontend_tools_travel_as_a_per_call_config_without_touching_the_toolkit`、`external_only_hides_the_backend_without_mutating_the_toolkit`、`agent_only_mode_passes_no_tool_config`、`tool_merge_mode_is_core_s_enum`（均为回退实现即失败的行为断言）。
- **基线**：`cargo test -p agentscope-extensions-protocol` → 302 passed；`cargo check --workspace --all-targets` 通过。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-protocol` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-protocol` / `cargo test -p agentscope-extensions-protocol` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-protocol/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/PROTOCOL__MIGRATION_HISTORY.md`](./history/PROTOCOL__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
