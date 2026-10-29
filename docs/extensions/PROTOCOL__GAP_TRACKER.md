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
| Rust crate | facade `crates/agentscope-extensions-protocol` + 5 个拆分子 crate（`-a2a-client` / `-a2a-server` / `-agent-protocol` / `-agui` / `-chat-completions-web`，见 §7） |
| Rust 规模（实测 2026-10-28） | 6 crate 共 **45 文件 / 20948 行 / 341 个测试**（`#[test]` 317 + `#[tokio::test]` 24）；分布：a2a-client 6 文件 2414 行 39 测、a2a-server 6 文件 3819 行 47 测、agent-protocol 3 文件 2093 行 29 测、agui 24 文件 11082 行 191 测、chat-completions-web 5 文件 1507 行 35 测、facade 1 文件 33 行 0 测 |
| 迁移波次 | A |
| 当前状态 | **总判：进行中**（仍有 P1 代码缺口：runner 体系 / adapterFactory / A2aAgent 门面）—— **已完成** — 5 个子 crate 全部落地；**2026-10-22 复核列出的 P1×7 已于 2026-10-28 全部关闭**（服务端引导层 / 执行属性 / registry 服务发现 / transport 包装层 / `message/stream` SSE 增量 / agent-protocol 落盘仓库 / agui `ProcessResult.interrupt`），余下仅 P2 长尾（见 §3.3、§6.4） |

### 1.1 验证基线（最近一次实测，2026-10-28，本 crate 内单条命令）
```
cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client \
  -p agentscope-extensions-a2a-server -p agentscope-extensions-agent-protocol \
  -p agentscope-extensions-agui -p agentscope-extensions-chat-completions-web -j 2
-> a2a-client 39 passed / a2a-server 47 passed / agent-protocol 29 passed
   agui 191 passed / chat-completions-web 35 passed / facade 0 passed
   合计 341 passed; 0 failed; 0 ignored
```
> 2026-10-22 的静态复核（302）已被本次实测取代；`cargo build` 随 `cargo test` 一并通过（0 error）。

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
| `agent.A2aAgent` / `A2aAgentConfig` | 远程 A2A 智能体与配置 | `a2a::client::{A2aClient, SendResult, JsonRpcTransport, ReqwestJsonRpcTransport}` | 部分完成（客户端与传输已实现；高层 Agent 门面未迁移，**`A2aAgentConfig` 无对应物**——Java 的 `clientTransports`+`ClientConfig` 未建模） |
| `agent.event.*`（ClientEventContext/Handler/Router、Message/Task/TaskUpdate EventHandler） | 流式事件处理 | `a2a::client::{decode_stream_result, parse_sse_stream, is_terminal_state, is_final_status}` + `a2a::model::StreamEvent` | 部分完成（事件解码已实现，Java 的 Handler 类体系未 1:1 迁移；`ClientEventContext` 的 hook/事件发射语义亦未迁移） |
| `agent.message.BaseMediaBlockParser` 等 | Source→FilePart MIME 推导 | `a2a::message::media_part` / `mime_type_from_url`（**私有**） | 已完成 |
| `agent.utils.LoggerUtil` | 日志封装 | 无（Rust 用 `tracing`；本 crate 实际 `tracing::` 调用仅 `agent_protocol.rs:290` 一处） | 设计决定（不迁移） |

### 2.2 a2a-server（`io.agentscope.core.a2a.server.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeA2aServer` / `request.AgentScopeA2aRequestHandler` | JSON-RPC 请求处理 + 服务装配 | `a2a::server::{A2aRequestHandler, router}` + `a2a::server::AgentScopeA2aServer`（`agentscope-extensions-a2a-server/src/{server,transport,registry,executor}.rs`） | **已完成（2026-10-28）**——引导层 `Builder`/`post_endpoint_ready()`/`agent_card_json()`/`transport_wrapper()`/`close()`/`router()` 均已落地；`ServiceLoader` SPI 以 `default_transport_builders()` + `Builder::transport_builder(..)` 替代（Rust 无 SPI 扫描，记为偏差）；`close()` 为 no-op（Rust handler 无线程） |
| `executor.AgentScopeAgentExecutor` | 任务执行编排 | `a2a::server::{AgentRunner, A2aTaskRegistry, AgentOutputProjector}` | **已完成（2026-10-28）**——blocking（`message/send`）与非 blocking（`message/stream`）双模式已分离；`AgentOutputProjector` 复刻 Java `BaseFluxEventHandler` 的 required-event-type 表；订阅取消在 Rust 由流 drop 表达（无 `Subscription` 对象） |
| `executor.AgentExecuteProperties` | `completeWithMessage` / `requireInnerMessage` 执行属性 | `a2a::server::AgentExecuteProperties`（builder + getter） | **已完成并接线（2026-10-28）**——`A2aRequestHandler::new_with_properties` 持有；`require_inner_message` 控制 `TOOL_PROGRESS`/`TOOL_RESULT_*_DELTA`/`HINT_BLOCK` 是否投影；`complete_with_message` 控制非阻塞终态是否带 message（阻塞恒定带，与 Java 一致） |
| `executor.runner.{AgentRunner, BaseReActAgentRunner, ReActAgentWithBuilderRunner, AgentRequestOptions}` | runner 体系 | `a2a::server::AgentRequestOptions` + `AgentRunner` trait | 部分完成 |
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
| Spring `SseEmitter` | SSE 流 | `axum` SSE（`agui::encoder::AguiEventEncoder::to_sse_event`）+ `agent_protocol` 手写 `text/event-stream` 帧 + a2a `A2aRequestHandler::stream_responses` | 部分完成。**已完成（2026-10-29 勘误）**：原记「`message/stream` 返回一次性数组非 SSE 增量」，实际 a2a 服务端已改真 SSE（`is_streaming_method` 只认 `message/stream`，每帧 `data: <json-rpc response>`）；依据 §3.2 / §8.1 #5 / 用例 `message_stream_is_served_incrementally_over_a_transport`。仍留：`tasks/resubscribe` 未流式（回 `-32601`，§8.4 第 6 条） |
| `HttpClient` (Java 11) | A2A 客户端 | `reqwest`（workspace 0.13）+ `JsonRpcTransport` trait | 已完成 |
| `java.net.URL` + `Files.probeContentType` | URL 扩展名→MIME | `mime_type_from_url`（内置扩展名表） | 已完成（受控子集） |
| `java.util.UUID` | 生成文件名 | `uuid`（workspace） | 已完成 |
| AG-UI 协议类型 / OpenTelemetry（**Java `agentscope-extensions-agui/pom.xml` 实测仅依赖 `agentscope-core`（provided/optional），无 ag-ui / opentelemetry 依赖**） | agui 事件 | 无 | 设计决定（不迁移）——原「待办」基于错误前提，Java 侧不存在该依赖 |

## 3. 详细缺口清单

### 3.1 P0（核心链路）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A wire 模型 | `io.a2a.spec.*` | 已完成 | `a2a/model.rs`（27400B / 733 行），9 单测 |
| A2A Msg↔Part 转换 | `a2a-client/.../message/*` | 已完成 | `a2a/message.rs`（37200B / 834 行），16 单测 |
| A2A 卡解析 | `a2a-client/.../card/*` | 已完成 | `a2a/card.rs`（27149B / 685 行），6 单测 |
| A2A 客户端 | `a2a-client/.../A2aAgent` | 部分完成 | `a2a/client.rs`（18440B / 451 行），9 单测；**高层 Agent 门面（`A2aAgent`/`A2aAgentConfig`）未迁移** |
| A2A 服务端 + JSON-RPC 路由 | `a2a-server/*` | **已完成（2026-10-28）** | `a2a/server` 拆为 `lib.rs`（939→ 约 900 行 handler/转换）+ `server.rs`/`transport.rs`/`registry.rs`/`executor.rs`；15 个旧测试全保留，新增 31 个（合计 46） |
| agent-protocol 任务生命周期 + HTTP 路由 | `agent-protocol/*` | **已完成（2026-10-28）** | `agent_protocol`（`lib.rs` + `error.rs` + `workspace_repository.rs`，共 2097 行）；24 旧测试 + 5 新增 = 29；workspace 落盘仓库已接线 |
| chat-completions-web 模型/转换/构造/流式 | `chat-completions-web/*` | 已完成 | `chat_completions/`（model 545 行 / converter 307 / builder 280 / streaming 349），35 单测 |
| agui 事件模型 | `agui/event/*`（`AguiEvent`） | **已完成** | `agui/event.rs`（63147B / 1497 行），含 11 单测（事件类型名/序列化/容忍未知类型）；事件变体 28 个与 Java `AguiEvent` record 一一对应 |
| agui 消息/内容模型 | `agui/model/*` | **已完成** | `agui/model.rs`（31798B / 805 行）（`AguiMessage`/`MessageContent`/`InputContent`/`InputContentSource`/`AguiToolCall`/`AguiFunctionCall`），17 单测 |
| agui 转换/适配/编码/处理/注册 | `agui/{converter,adapter,encoder,processor,registry,runtime}/*` | **已完成** | `agui/converter/{message,state,tool}.rs`、`agui/adapter/{agent_adapter,config}.rs` + `strategy/{context,converters,enricher,registry}.rs`、`agui/encoder.rs`、`agui/processor/{request_processor,resume_coordinator,resolver}.rs`、`agui/registry.rs`、`agui/runtime.rs`、`agui/constants.rs`、`agui/error.rs`；均已在 `mod.rs` 声明（`tool_injection.rs` 已按 §4.4 GAP-17 删除） |
| agui 请求/上下文/续跑模型 | `agui/model/{RunAgentInput,AguiContext,AguiResume,...}` | **已完成** | `agui/model.rs`（805 行）含 `RunAgentInput`/`AguiContext`/`AguiResume`/`AguiTool`/`ToolMergeMode`（`ToolMergeMode` 复用 core 枚举） |

### 3.2 P1（重要）
> **2026-10-28 状态**：原 7 项 P1（服务端引导层 / `AgentExecuteProperties` / registry 服务发现 / transport 包装层 / `message/stream` SSE 增量 / agent-protocol 落盘仓库 / agui `ProcessResult.interrupt`）**全部关闭**，详见 §8；仅「A2A 服务端 runner 体系 1:1」与本批无关，仍为部分完成。
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A 服务端 runner 体系 1:1 | `executor.runner.*` | 部分完成（**不在 2026-10-28 P1×7 范围**） | 以 `AgentRunner` trait + `A2aTaskRegistry` 抽象；**仍缺**：`BaseReActAgentRunner`（按 `taskId` 缓存 `ReActAgent` + `stop()` 调 `interrupt()` + 流结束移除缓存）、`ReActAgentWithBuilderRunner`（`ReActAgent.Builder` → `buildReActAgent()`）；Rust `AgentRunner` 现有 `agent_name`/`agent_description`/`stream_events`/`stream_events_incremental`/`stop`，但**无 per-task agent 缓存与自动 interrupt 语义**。`stream_events_incremental` 为 2026-10-28 新增（默认 `None`，由 handler 回放 `stream_events`），是接 core 流式 agent 的挂点 |
| A2A 智能体注册与发现 | `registry.*` | **已完成（2026-10-28）** | `a2a::server::{AgentRegistry, AgentRegistryService, RegistrationReport}`（`agentscope-extensions-a2a-server/src/registry.rs`）；`register(agent_card, transports)` 遍历全部 registry、单点失败隔离；`AgentScopeA2aServer::post_endpoint_ready()` 触发。测试 `register_reaches_every_registry_with_the_served_transports` / `one_failing_registry_never_blocks_the_others` / `no_transport_means_no_registry_call` / `post_endpoint_ready_registers_with_every_registry_once` |
| A2A 传输包装层 | `transport.*` | **已完成（2026-10-28）** | `src/transport.rs`：`TransportWrapper`（`async fn handle_request(body, headers, metadata) -> TransportResponse`，Java 同步版因 Rust handler 为 async 而改 async，已记偏差）、`TransportWrapperBuilder`（去掉 Java 的 `Executor` 参数——流在调用方 async 任务上跑）、`JsonRpcTransportWrapper`+`Builder`、`JsonRpcTransportMetadata`、`DeploymentProperties`+`Builder`（默认 host=本机 IP，端口缺失报错）、`CustomTransportProperties`（由 `Builder::with_custom_transport` 在 build 时 `set_deployment_properties` 后 `to_transport_properties`）。axum `POST /` 已改走 wrapper。测试 8 个（含 `non_streaming_requests_return_one_json_response` / `streaming_requests_return_a_stream_of_json_responses` / `custom_transport_properties_are_resolved_at_build_time`） |
| A2A 服务端引导层 | `AgentScopeA2aServer` | **已完成（2026-10-28）** | `src/server.rs`：`AgentScopeA2aServer` + `Builder`（`agent_card`/`with_transport`/`transport_builder`/`deployment_properties`/`deployment_port`/`with_agent_registry`/`with_custom_transport`/`agent_execute_properties`/`task_registry`），`build()` 归一化 transport→派生 card→建 handler→建 wrapper（失败即丢弃该 transport）；`transport_wrapper`/`require_transport_wrapper`/`agent_card_json`/`post_endpoint_ready`/`handle_request`/`router`/`close`。`A2aServerBuildError::{NoTransportProperties, NoAvailableTransport}` 对齐 Java 两条 `IllegalArgumentException` 文案。`ServiceLoader` → `default_transport_builders()`（偏差：Rust 无 SPI）。测试 7 个 |
| A2A 执行属性 | `executor.AgentExecuteProperties` | **已完成（2026-10-28）** | `src/executor.rs`：`AgentExecuteProperties{complete_with_message, require_inner_message}` + builder；`AgentOutputProjector` 复刻 `generateRequiredEventTypes`/`convertToResponseMessage`（TEXT/THINKING delta 恒投影；TOOL_PROGRESS/TOOL_RESULT_TEXT_DELTA/TOOL_RESULT_DATA_DELTA/HINT_BLOCK 需 `require_inner_message`；`AGENT_RESULT` 仅在该 run 无流式输出时兜底）。**偏差**：Rust `AgentRunner` 默认只有消息级契约，故保留 `AGENT_RESULT` 兜底分支（Java 无此事件形态）。测试 `inner_events_need_require_inner_message` / `agent_result_falls_back_only_when_nothing_was_streamed` / `require_inner_message_gates_tool_output` / `complete_with_message_only_applies_to_non_blocking_runs` |
| agent-protocol workspace 落盘仓库 | `WorkspaceProtocolTaskRepository` | **已完成（2026-10-28）** | `agentscope-extensions-agent-protocol/src/workspace_repository.rs`：复用 harness `TaskRecord` + `WorkspaceManager`（**未另造类型**；harness `TaskRepository` trait 是会话级后台子智能体模型，与协议侧全局 `task_id` 查询语义不同，故 `ProtocolTaskRepository` 保持既有形状并在 tracker 记录理由）；`save/find` 同步 trait 实现用 `futures::executor::block_on` 驱动 workspace 的 async 写读（本地盘 `std::fs` + runtime 无关的 `tokio::sync` 锁，无需 reactor），另提供 `save_async`/`find_async`/`list_async`。接线：`AgentProtocolTaskStore::with_workspace_task_store(..)` 消费 `AgentProtocolProperties::resolved_task_store_path()`。测试 5 个 |
| agui 运行中断入口 | `AguiRequestProcessor.ProcessResult#interrupt(threadId)` | **已完成（2026-10-28）** | `agui::processor::ProcessResult::interrupt(&self, thread_id) -> bool`：经 `AguiStreamableAgent::interrupt_session`（新增 trait 方法，默认 `interrupt()` 并返回 false；`ReActAgent`/`HarnessAgent` 覆写为各自的 `interrupt_session`）。`ProcessResult` 新增 `agent: Option<Arc<dyn AguiStreamableAgent>>`（**偏差**：Rust 在 agent 解析失败时以 `RUN_ERROR` 事件收尾、无 agent 可挂，故为 `Option`；Java 先解析再建惰性 Flux，恒有值）。测试 `interrupt_targets_the_thread_session` / `interrupt_falls_back_for_agents_without_a_session_scope` / `process_exposes_the_resolved_agent_for_interruption` |
| SSE 流式响应 | Spring `SseEmitter` / `Flux` | **已完成（2026-10-28）** | a2a 服务端新增 `A2aRequestHandler::stream_responses(request) -> BoxStream<'static, JsonRpcResponse>`（`agentscope-extensions-a2a-server/src/lib.rs` 的 `StreamPhase` 状态机：首帧 `working`、逐事件 `artifact-update`、终帧 `completed`；`AgentRunner::stream_events_incremental(...)` 有则直连、无则回放缓冲结果）；`is_streaming_method` **仅**覆盖 `message/stream`（Java 的 wrapper 也接受 `tasks/resubscribe`，但 Java 有 per-task 事件队列；Rust handler 无队列，订阅只能用重跑实现，故保持非流式并回 `-32601`，已记偏差）；axum `POST /` 对流式方法返回 `text/event-stream`（每帧 `data: <json-rpc response>`，对齐 Java `Flux<String>` + Spring SSE）。测试 `stream_responses_emits_before_the_run_finishes`（用 channel 驱动，缓冲实现必然挂死）/ `message_stream_relays_the_run_incrementally` / `message_stream_is_served_incrementally_over_a_transport` / `resubscribe_is_not_streamed_and_reports_method_not_found` |
| agui 适配器 + 转换策略 | `agui/adapter/*` | **已完成** | `AguiAgentAdapter`（981 行）/`AguiAdapterConfig`（302 行，12 个配置项与 Java 一一对应）+ 策略层（`context` 603 行/`converters` 1489 行/`enricher` 129 行/`registry` 334 行）；11 个内建 converter 与 Java `strategy/*` 11 个类一一对应 |
| agui 请求处理/续跑 | `agui/processor/*` | **已完成** | `AguiRequestProcessor`（638 行）/`AguiResumeCoordinator`（600 行）/`AgentResolver`；**注**：`AguiRequestProcessorBuilder` 未暴露 Java 的 `adapterFactory` 注入点（Rust 侧 `AguiAgentAdapterFactory` trait 已存在但 processor 未接线） |
| `TOOL_PROGRESS` 事件投影（`707400eb`） | `agui/adapter/strategy/ToolResultEventConverter` | **已完成（2026-10-20，有意不映射）** | core 已发独立 `TOOL_PROGRESS` 事件；AG-UI 侧按 Java 既定行为**显式丢弃**（Java 注释：AG-UI 没有工具进度通道，故丢弃而非透传为原始事件），Rust 用带注释的 no-op 分支对齐，**未新增任何 AG-UI 事件**。原先进度借 `TOOL_RESULT_DATA_DELTA` 透传导致的「进度被当成工具结果」风险已消除 |
| `AguiResume.reason`（`c0d03ccea`） | `AguiMessageConverter#RESUME_PAYLOAD_REASON` | **已完成（2026-10-20）** | `AguiResume` 增 `reason: Option<String>`（`serde(default)` + `skip_serializing_if`，旧客户端缺键可反序列化）；`cancelled` 且带 reason 时，`resume_content` 用它作工具结果文本，并写入 `ConfirmResult.message`；无 reason 时保留历史默认文案 |
| COMPLETED 任务优先取 artifacts（`6e1ee68d7`） | `A2aAgent.call()` / `TaskEvent` 结果提取 | 待办（新细项，2026-10-26 登记） | Java：COMPLETED 任务若带非空 output artifacts，**优先返回 artifacts**；无 artifacts 才回落 status message；失败终态仍保持「status message 优先于残留 artifacts」。Rust：**服务端**侧 artifacts 已存在（`A2aTaskRegistry::add_artifact` + `TaskArtifactUpdateEvent`，2026-10-28）；**客户端**侧 `agentscope-extensions-a2a-client` 仍无 artifacts 概念（`rg -i artifact src/` 零命中），结果提取只认 status/文本。属 A2A 客户端门面（`A2aAgent`）未迁移的一部分，非独立缺口，故不单列 P0 |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A MIME 探测全表 | `Files.probeContentType` | 部分完成 | Rust 侧为受控扩展名表（png/jpg/gif/webp/bmp/mp3/wav/ogg/mp4/webm/mov）；Java 走系统探测 |
| A2A 客户端事件 Handler 类体系 | `agent.event.*` | 部分完成 | 由函数式解码替代，无 Handler 对象继承树；**Java `ClientEventContext` 的 hook（Pre/PostReasoning）与 `AgentEventEmitter` 增量发射语义亦未迁移** |
| agui 工具/常量/异常 | `AguiUtil` 等 | **已完成（2026-10-28）** | `agui::constants`（中断元数据/原因常量）+ `agui::error::{AguiError, AguiResult}`（覆盖 Java `AguiException` 的 `AgentNotFound`/`Encoding` 两个子类语义）；`AguiUtil` 由 `AguiStreamableAgent` trait 替代，`ProcessResult.interrupt` 已迁移（见 §3.2） |
| agui transport 枚举粒度 | `AguiRuntimeContextRequest.Transport`（MVC/WEBFLUX/CUSTOM） | 部分完成 | Rust `AguiTransport` 只有 `Custom`/`Http`，MVC 与 WEBFLUX 合并为 `Http` |
| Java `AguiMessage` 工厂方法族 | `AguiMessage.userMessage/assistantMessage/...` | 部分完成 | Rust `AguiMessage` 有 `user_message`/`user_message_blocks`/`blocks_message`/`reasoning_message`/`with_error`，但**缺** `system_message`/`tool_message` 工厂（字段仍可由 `new`/`blocks_message` 构造） |
| Spring AutoConfiguration | `*AutoConfiguration` | **已完成（设计决定，不迁移）** | Rust 无 DI 容器；配置由各 crate 的 builder/构造参数显式传入 |
| `NetworkUtils` | `a2a-server/utils` | **已完成（设计决定，不迁移）** | 逻辑并入 card/client，无独立工具类必要 |
| `bytes` 依赖 | `Cargo.toml` `[dependencies]` | 部分完成 | `bytes = { workspace = true }` 已声明但**本 crate 源码 0 处使用**（`bytes::` / `use bytes` 均 0 命中）——建议清理或改为实际使用 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **Rust 产物（2026-10-28，crate 已拆分，见 §7）**：
  - `agentscope-extensions-protocol`（facade）1 文件 33 行；
  - `agentscope-extensions-a2a-client` 6 文件 2414 行（`card`/`client`/`error`/`lib`/`message`/`model`）；
  - `agentscope-extensions-a2a-server` 6 文件 3778 行（`lib`/`card`/`server`/`transport`/`registry`/`executor`）；
  - `agentscope-extensions-agent-protocol` 3 文件 2097 行（`lib`/`error`/`workspace_repository`）；
  - `agentscope-extensions-agui` 24 文件 11076 行；
  - `agentscope-extensions-chat-completions-web` 5 文件 1507 行；
  - 合计 **45 文件 / 20905 行**。
- **测试**：**341 个**（`#[test]` 317 + `#[tokio::test]` 24）。分布见 §1 与 §8。
- **验证（2026-10-28 实测，单条命令 -j 2）**：
  ```
  cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client \
    -p agentscope-extensions-a2a-server -p agentscope-extensions-agent-protocol \
    -p agentscope-extensions-agui -p agentscope-extensions-chat-completions-web
  -> 39 / 47 / 29 / 191 / 35 / 0 passed；合计 341 passed; 0 failed; 0 ignored
  ```
- **本批（2026-10-28 P1×7 关闭）交付**：见 §8（Java 依据、改动文件、测试名、反向验证）。
- **历史批次（agui 整包迁移 + 收尾，2026-10-21）**：
  - agui 整包落盘（~8800 行）；**首个子智能体因网络错误中断**，留下 5 个编译错误 + 11 个失败用例且 **tracker 完全未回填**。
  - 汇总方修复 5 个编译错误：①`tool_injection.rs` 的 `AgentTool`→core `Tool`；②core `AgentEventType` **不实现 `Hash`** → `registry.rs` 转换表由 `HashMap` 改 `Vec<(AgentEventType, Arc<dyn AgentEventConverter>)>` + 线性查找；③事件流 `then(async{...}).flatten()` 替代 async `flat_map`（E0277）；④`build_tool_call_interrupt(&result, …)` 补引用（E0308）；⑤补 3 处测试模块 import。
  - 汇总方修复剩余失败用例至全绿（均已在同文件内修正，未删测试/未 `#[ignore]`）：`block_message_id` 无显式 id 时按事件类型推导块类型（`-text`）、`RunFinished.outcome` 与 Java 同为 NON_NULL 语义（断言修正）、ToolResult 数据块换行分隔断言改走两个 data 块、registry native 模式断言改为 START+CONTENT 两事件、`ToolInjection` 复原断言改为「原实例复原」而非「非 external」、`headers().len()` 随大小写敏感存储改为 3、`request_processor` 两个用例补上 agentId 与 active-run 前置条件。
- **会话内根因修复（早期）**：
  - `chat_completions/streaming.rs` E0700、`builder.rs` 单测非法 `Msg`、`agui` 接线（`lib.rs` 缺 `pub mod agui;`）、`event.rs` `DeserializeOwned` 约束、`a2a/message.rs` 断言写错（详见 §4.3）。
- **已知问题**：build 存在若干 unused-import / unused-const 警告（`FunctionCallStream`、`HarnessAgentBuilder`、`SdkResult`、`TextBlock`、`SandboxState`、`WorkspaceSpec`、`ExecResult`、`BUILT_IN_EVENT_TYPES` 等），不影响编译与测试；属既有状态，本批未动。

### 4.2 下一步与阻塞
1. **P0/P1×7 已全部落地**（见 §3.1/§3.2、§8），无 P0 阻塞。
2. **仍为非 1:1 的 P1（明确不在本次 7 项内）**：A2A runner 体系 1:1（`BaseReActAgentRunner` per-task agent 缓存 + 自动 `interrupt`／`ReActAgentWithBuilderRunner`）；`AguiRequestProcessorBuilder` 未暴露 `adapterFactory` 注入点。
3. **P2 长尾**（§3.3）：`A2aAgent` 高层门面 + `A2aAgentConfig`、客户端 `agent.event.*` Handler 体系、`AguiTransport` 枚举粒度、`AguiMessage.system_message/tool_message`、`bytes`/`tracing` 未用依赖、A2A MIME 全表、客户端侧 artifacts 优先提取。
4. **跨模块需求**：本批**未改 core/harness**。`WorkspaceProtocolTaskRepository` 直接复用 harness `WorkspaceManager` 的既有 task-record API（`write_task_record_json`/`read_task_record_json`/`list_task_records_json`），无需新增能力位。`AgentRegistry` 由本 crate 提供，NACOS 等发现后端可直接实现该 trait 复用。
5. **依赖变更**：仅 `crates/agentscope-extensions-a2a-server/Cargo.toml` 增加 `futures`、`tracing`（均为 workspace 依赖，根 `Cargo.toml` 未改）。**注**：`bytes` 仍在本 crate 声明但未使用，清理前需汇总方确认是否影响其它 crate。

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

## 6. 静态复核记录（2026-10-22）

> **已被 §8 部分取代（2026-10-28）**：本节记录的 7 项 P1 缺口现已全部关闭（见 §3.2 与 §8）。保留本节作为历史快照；行数/测试数口径以 §1.1 / §4.1 / §8.2 的实测为准。

### 6.1 复核方式
- **静态复核**：**未执行** `cargo build` / `cargo test`；**未修改任何 Rust 代码**；仅修改本文件 `docs/extensions/PROTOCOL__GAP_TRACKER.md`。
- 手段：遍历 Java 源码树统计文件/行数/类型数；遍历 Rust `src/` 统计文件/行数/`#[test]`/`#[tokio::test]`；逐个 Java 类打开核对 Rust 对应物的**符号是否存在、是否 `pub`、行为是否覆盖**。
- 行数口径：Java 按**非空行**计（与 rollup 口径一致，便于对比）；Rust 按 `Get-Content | Measure-Object -Line`（非空行）计。物理行（含空行/注释）口径另附于 §6.3。
- Rust 测试计数口径：源码内 `#[test]` 与 `#[tokio::test]` 属性行数（与 `cargo test` 的用例数一一对应，无 `#[ignore]`）。

### 6.2 实测统计
**Java（`agentscope-extensions-protocol`，6 个 Maven 模块，207 个 `.java`）**

| 子模块 | 主文件 | 主行(非空) | 测文件 | 测行(非空) | 主类/接口/枚举/record |
|---|---|---|---|---|---|
| a2a-client | 31 | 2896 | 21 | 3500 | 31 / 4 / 0 / 1 |
| a2a-server | 24 | 3238 | 12 | 3942 | 24 / 5 / 0 / 2 |
| agent-protocol | 12 | 1458 | 9 | 1234 | 7 / 4 / 0 / 1 |
| agui | 55 | 8467 | 21 | 10304 | 45 / 5 / 2 / 45 |
| chat-completions-web | 12 | 1851 | 10 | 2274 | 14 / 0 / 0 / 0 |
| **合计** | **134** | **17910** | **73** | **21254** | — |

> 物理行（含空行）口径：主 **19968** / 测 **24814**。

**Rust（`crates/agentscope-extensions-protocol/src`，38 个 `.rs`）**

| 项 | 实测 | rollup 记录 | 结论 |
|---|---|---|---|
| 文件数 | 38 | — | — |
| 行数（非空） | 17248 | — | — |
| 行数（物理行） | 18793 | — | — |
| `#[test]` | 285 | — | — |
| `#[tokio::test]` | 17 | — | — |
| **测试合计** | **302** | 290（rollup / §1.1 / §4.1） | **tracker 旧值偏小 12** |
| agui 目录行数 | 10259 | ~8800（§4.1） | 旧值偏小 |

**逐文件测试分布（Rust）**

| 文件 | 用例 | 文件 | 用例 | 文件 | 用例 |
|---|---|---|---|---|---|
| `a2a/model.rs` | 9 | `agui/converter/message.rs` | 33 | `agui/processor/request_processor.rs` | 9+6=15 |
| `a2a/message.rs` | 16 | `agui/converter/state.rs` | 8 | `agui/processor/resolver.rs` | 3 |
| `a2a/card.rs` | 6 | `agui/converter/tool.rs` | 2 | `agui/processor/resume_coordinator.rs` | 12 |
| `a2a/client.rs` | 9 | `agui/event.rs` | 11 | `agui/registry.rs` | 8 |
| `a2a/server.rs` | 15 | `agui/model.rs` | 17 | `agui/runtime.rs` | 10 |
| `agent_protocol.rs` | 22 | `agui/adapter/agent_adapter.rs` | 5+11=16 | `agui/encoder.rs` | 6 |
| `chat_completions/{builder,converter,model,streaming}.rs` | 8/10/9/8 | `agui/adapter/config.rs` | 4 | `agui/adapter/strategy/converters.rs` | 28 |
| `agui/adapter/strategy/{context,enricher,registry}.rs` | 0/4/8 | `agui/constants.rs` | 1 | `agui/error.rs` | 2 |
| `error.rs` | 2 | | | **合计** | **302** |

### 6.3 逐子模块对齐结论
| 子模块 | 结论 | 说明 |
|---|---|---|
| **a2a-client** | **部分完成** | `MessageConstants`（**8 个 block type 常量 + 10 个 metadata 键**，与 Java `MessageConstants.BlockContent` 逐项一致）、`FixedAgentCardResolver(+Builder)`、`WellKnownAgentCardResolver(+Builder)`、`CardFetcher`/`ReqwestCardFetcher`、`AgentCardResolver` trait **全部存在且语义一致**；16 个 `*Parser` 类收敛为 `part_to_content_block`/`content_block_to_part` 两个公开函数 + 5 个私有辅助，**行为覆盖完整**（含 `StreamingChunkAccumulator` 流式块合并、`MetadataUtil` 的工具元数据保留键覆盖语义）。**缺**：`A2aAgent` 高层门面（Memory/Hook/Middleware/Session/interrupt 全缺）、`A2aAgentConfig`（`clientTransports` + `ClientConfig` 无对应物）、`event/*` Handler 类体系 + `ClientEventContext` 的 hook 与增量发射语义、`LoggerUtil`（设计决定不迁移） |
| **a2a-server** | **部分完成** | `AgentRequestOptions`（taskId/sessionId/userId 三字段 + getter/setter）、`AgentRunner` trait（`agent_name`/`agent_description`/`stream_events`/`stop` **四方法与 Java 接口对齐**）、`ServerMessageConvert`（6 个公开方法覆盖 Java `utils.MessageConvertUtil` 全部静态方法）、`create_agent_card`/`ConfigurableAgentCard`/`TransportProperties`(+Builder)、`A2aTaskRegistry`、`A2aRequestHandler`（5 个 JSON-RPC 方法）、`router()`（`POST /` + `GET /.well-known/agent-card.json`）、`server_constants::IS_STREAM_KEY` **均存在**。**缺**：`AgentScopeA2aServer` 引导层、`BaseReActAgentRunner`/`ReActAgentWithBuilderRunner`、`AgentExecuteProperties`、`AgentRegistry`/`AgentRegistryService`、`TransportWrapper(+Builder)`/`DeploymentProperties`/`CustomTransportProperties`/`JsonRpcTransportMetadata`、`SdkInternals`（SDK 反射，Rust 不适用） |
| **agent-protocol** | **部分完成** | `AgentProtocolConstants`（含 `RESERVED_KEYS`/`RESERVED_PREFIXES`）、`AgentProtocolProperties(+Builder)`（6 个配置项 + `resolved_task_store_path` 与 Java `getTaskStorePath()` 默认值一致）、`AgentRequest`、`RuntimeContextCustomizer`/`FlattenCustomizer`、`AgentFactory`/`FixedAgentFactory`、`AgentProtocolEventBus`/`AgentProtocolTaskEventBus`/`EventSubscription`、`AgentProtocolTaskStore`、`ProtocolTaskRepository`/`InMemoryProtocolTaskRepository`、`ConfirmResult`/`to_confirm_results`/`extract_pending_confirms`/`snapshot_value`/`AWAITING_CONFIRM_SENTINEL`、`router()`（**6 条路由与 Java `@RestController` 的 6 个映射逐一对应**）**全部存在**。**缺**：`WorkspaceProtocolTaskRepository`（落盘；`task_store_path` 已能解析但无消费方） |
| **agui** | **已完成（1 处 API 缺口）** | `src/agui/mod.rs` **确实声明并 re-export 了全部 10 个子模块**（`adapter`/`constants`/`converter`/`encoder`/`error`/`event`/`model`/`processor`/`registry`/`runtime`）；`AguiEvent` **28 个变体与 Java `AguiEvent` 的 28 个 record 一一对应**（Rust 另加 `Unknown` 容忍未知类型；Java 另有 `RunFinishedSuccessOutcome`/`RunFinishedInterruptOutcome`/`Interrupt`/`JsonPatchOperation`，Rust 对应 `RunFinishedOutcome::{Success,Interrupt}` + `Interrupt` + `JsonPatchOperation`）；`AguiEventType` 30 个枚举值与 Java 30 个逐一对应；`strategy/` **11 个内建 converter 与 Java `adapter/strategy/*` 的 11 个类逐一对应**（`AgentLifecycle`/`PermissionConfirm`/`ExternalExecution`/`TextBlock`/`ThinkingBlock`/`ToolCall`/`ToolResult`/`ModelCallUsage`/`Custom`/`Raw`/`Subagent`）+ `BaseEventPropertiesEnricher`；`AguiAdapterConfig` **12 个配置项 + 默认值与 Java 一致**（`MERGE_EXTERNAL_PRIORITY`/`emitStateEvents=true`/`emitToolCallArgs=true`/`emitTokenUsage=false`/`enableReasoning=false`/`emitRunFinishedAfterError=false`/`runTimeout=10min`/`baseEventPropertiesEnricherEnabled=false`/`emitSubagentEventsAsNative=false`）；`AguiAgentRegistry` 7 方法与 Java 一一对应；`AguiEventEncoder` 4 方法（`encode`/`encode_to_json`/`encode_comment`/`keep_alive`）与 Java 一致，**SSE 帧格式 `data: <json>\n\n` 完全对齐**；`AguiRequestProcessor` 的 agentId 解析优先级（path→header→forwardedProps→config→default）与 Java `resolveAgentId` 一致。**缺**：`ProcessResult.interrupt(threadId)` 公开中断入口；`AguiRequestProcessorBuilder` 未暴露 `adapterFactory` 注入点 |
| **chat-completions-web** | **已完成** | Java 11 个类全部覆盖：8 个 model（`ChatCompletionsRequest`/`Response`/`Chunk`/`ChatMessage`/`ChatChoice`/`ToolCall(+FunctionCall)`/`OpenAITool`/`OpenAIToolFunction`/`Usage`）、2 个 converter、1 个 builder、1 个 streaming adapter，Rust 侧 `chat_completions::model` 9 结构体 + `converter` 2 + `builder` 1 + `streaming` 1 + `Usage` 独立结构体 |

### 6.4 新发现缺口
| 优先级 | 缺口 | Java 参考 | Rust 现状 | 建议 |
|---|---|---|---|---|
| **P1** | A2A 服务端引导层缺失 | `a2a-server/.../server/AgentScopeA2aServer.java`（505 行） | 只有 `a2a::server::router()` + `A2aRequestHandler`；`Builder`/`ServiceLoader` transport SPI 发现/`postEndpointReady`/`getAgentCardJson`/`close` 全无 | 补一个 `A2aServer` 门面（聚合 card/handler/registry/transport） |
| **P1** | A2A 执行属性缺失 | `a2a-server/.../executor/AgentExecuteProperties.java`（66 行，源码 0 引用） | 无；`run_task` 恒定只收 `AgentResult` 文本，`requireInnerMessage`/`completeWithMessage` 语义丢失 | 在 `AgentRequestOptions` 上加两个开关或在 handler 侧补参数 |
| **P1** | A2A registry 服务发现缺失 | `a2a-server/.../registry/{AgentRegistry,AgentRegistryService}.java`（108 行） | 无（源码 0 引用） | 补 `AgentRegistry` trait + `AgentRegistryService` |
| **P1** | A2A transport 包装层缺失 | `a2a-server/.../transport/**`（7 类，含 `TransportWrapper`/`TransportWrapperBuilder`/`DeploymentProperties`/`CustomTransportProperties`） | 仅 `a2a::card::TransportProperties`；其余无 | P2 长尾，可延后 |
| **P1** | agui 运行中断入口缺失 | `agui/processor/AguiRequestProcessor.java:111`（`ProcessResult.interrupt`） | `agui::processor::ProcessResult` 无 `interrupt` | 补 `ProcessResult::interrupt(thread_id)`，经 `AguiStreamableAgent` 转发 |
| **P1** | A2A `message/stream` 非增量流 | `a2a-server/.../transport/jsonrpc/JsonRpcTransportWrapper.java`（`Flow.Publisher<StreamingEventKind>`） | Rust `A2aRequestHandler::message_stream` 返回**一次性事件数组**（`vec![TaskStatusUpdateEvent]`），非 SSE/流式增量 | 补 axum SSE 流式响应 |
| **P1** | agent-protocol 落盘仓库缺失 | `agent-protocol/.../WorkspaceProtocolTaskRepository.java`（64 行） | 仅 `InMemoryProtocolTaskRepository`；`task_store_path` 无消费方 | 补 workspace/file 仓库实现 |
| **P2** | `A2aAgent` 高层门面缺失 | `a2a-client/.../agent/A2aAgent.java`（724 行）+ `A2aAgentConfig.java`（71 行） | 只有 `A2aClient`（请求/响应），**未实现 `agentscope_core::Agent`**（无 memory/hook/middleware/session/interrupt） | 若需把远程 A2A 智能体当本地 Agent 用，需补 |
| **P2** | 客户端事件 Handler 体系缺失 | `a2a-client/.../event/*`（6 类） | 函数式 `decode_stream_result`/`parse_sse_stream`；`ClientEventContext` 的 hook/增量发射语义无 | 功能可用但 API 形态不同 |
| **P2** | `AguiTransport` 枚举粒度不足 | `AguiRuntimeContextRequest.Transport`（MVC/WEBFLUX/CUSTOM） | 只有 `Custom`/`Http` | 可保持现状，登记为已知偏差 |
| **P2** | `AguiMessage` 工厂方法缺失 | `AguiMessage.userMessage/systemMessage/toolMessage/...` | 缺 `system_message`/`tool_message` | 补两个便捷构造函数 |
| **P2** | `bytes` 依赖未使用 | — | `Cargo.toml` 已声明，源码 0 引用 | 清理依赖或改用 |
| **P2** | `tracing` 依赖几乎未使用 | Java 侧用 SLF4J 贯穿 a2a | 本 crate `tracing::` 实际调用仅 `agent_protocol.rs:290` 一处 | 可选：补齐 a2a/agui 的日志 |

### 6.5 勘误列表（原文 → 实际）

> **2026-10-29 注**：本节是 2026-10-22 静态复核快照。其中 #6/#7/#10/#19/#22 所指的 P1 缺口已由 §8（2026-10-28）全部关闭，当前状态以 §3.2 / §4.2 为准。
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | `a2a-client 2401 / a2a-server 2824 / agent-protocol 1443 / agui 8325 / chat-completions-web 1851` | `a2a-client 2896 / a2a-server 3238 / agent-protocol 1458 / agui 8467 / chat-completions-web 1851`（合计主 **17910**；rollup 记 16844 已过期） |
| 2 | §1 / §1.1 / §4.1 | `290 test 全绿` / `290 passed` / `**290 个 #[test]**` | **302**（285 `#[test]` + 17 `#[tokio::test]`）；§4.4 的「302 passed」才是正确值 |
| 3 | §2.1 | `a2a::message::{part_to_content_block, content_block_to_part, file_part_to_block, data_part_to_block, media_part, mime_type_from_url}` | 仅 `part_to_content_block`/`content_block_to_part` 为 `pub`；`file_part_to_block`/`data_part_to_block`/`media_part`/`mime_type_from_url` 为**私有** |
| 4 | §2.1 | `a2a::message::MessageConvert`（含 `StreamingChunkAccumulator`） | `StreamingChunkAccumulator` 为**私有 struct**（行为已实现，仅非公开） |
| 5 | §2.1 | `a2a::message::{media_part, mime_type_from_url}` 标「已完成」 | 两者均**私有**，外部不可调用；对外只有两个公开转换函数 |
| 6 | §2.2 | `AgentScopeA2aServer` / `AgentScopeAgentExecutor` 行标「已完成」 | 二者均为**部分完成**（引导层与执行属性/双模式缺失） |
| 7 | §2.2 | `a2a::server::A2aTaskRegistry`（部分） | 准确；但未提及 `AgentRegistry`/`AgentRegistryService` 全缺 |
| 8 | §2.3 | `AgentProtocolController` HTTP 控制器（`/api/tasks/...`） | Java 实际映射为 `/tasks`、`/tasks/{taskId}`、`/tasks/{taskId}/wait`、`/tasks/{taskId}/cancel`、`/tasks/{taskId}/events`、`/tasks/{taskId}/resume`，**无 `/api` 前缀**；Rust `router()` 亦为 `/tasks...` |
| 9 | §2.4 | `agui::adapter::{agent_adapter, config, tool_injection}` | `tool_injection` 已于 §4.4 GAP-17 **整文件删除**，源码中 0 引用 |
| 10 | §2.4 | `AguiUtil` 行标「已完成」 | **部分完成**：`isHarnessAgent`/`asReActAgent` 由 `AguiStreamableAgent` trait 替代，但 `ProcessResult.interrupt` 未迁移 |
| 11 | §2.6 | `Spring SseEmitter → axum SSE / bytes`（部分完成） | `bytes` 在本 crate 源码 0 引用；实际用 `axum::response::sse` + `agent_protocol` 手写 `text/event-stream` 帧 |
| 12 | §2.6 | `AG-UI 协议类型 / OpenTelemetry (agui 依赖) → 无`，状态 **待办** | Java `agentscope-extensions-agui/pom.xml` 实测**仅依赖 `agentscope-core`（provided/optional）**，无 ag-ui/opentelemetry 依赖；该「待办」前提不成立，已改为**设计决定（不迁移）** |
| 13 | §3.1 | `a2a/model.rs`（26595B）/`message.rs`（36298B）/`card.rs`（26384B）/`client.rs`（17943B）/`server.rs`（38558B）/`agent_protocol.rs`（68830B）/`agui/event.rs`（61600B） | 依次为 **27400B / 37200B / 27149B / 18440B / 39601B / 70749B / 63147B**（旧值均偏小） |
| 14 | §3.1 | agui 事件模型「含 **16** 单测」 | `agui/event.rs` 实测 **11** 个测试 |
| 15 | §3.1 | agui 消息/内容模型「**5** 单测」 | `agui/model.rs` 实测 **17** 个测试 |
| 16 | §3.1 | `agui/model.rs`（**671** 行） | 实测 **805** 行 |
| 17 | §3.1 | `agui/adapter/{agent_adapter,config,tool_injection}.rs` | `tool_injection.rs` 已删除 |
| 18 | §3.2 | `AguiAgentAdapter`（**872** 行）/`AguiRequestProcessor`（**626** 行） | 实测 **981** 行 / **638** 行 |
| 19 | §3.3 | agui 工具/常量/异常 行标「已完成」 | 降为**部分完成**（`ProcessResult.interrupt` 未迁移） |
| 20 | §4.1 | agui 整包落盘（~**8800** 行） | agui 目录实测 **10259** 行（38 文件中的 21 个） |
| 21 | §4.2 | P1 缺口笼统列为「A2A runner/registry/transport 的 1:1 补齐 + workspace 仓库」 | 已按 §3.2 逐项列明（新增：服务端引导层、`AgentExecuteProperties`、a2a `message/stream` 增量流、agui `ProcessResult.interrupt`） |
| 22 | §3.2 | `SSE 流式响应` 行标「已完成」 | 降为**部分完成**——a2a 服务端 `message/stream` 返回一次性数组，非 SSE 增量流 |
| 23 | §2.6 | `Spring Web / AutoConfiguration → axum 0.7` | 准确（根 `Cargo.toml` 实测 `axum = { version = "0.7", features = ["json","tokio","http1","query"] }`）；`reqwest` 实测为 0.13（非 tracker 隐含的旧版本） |

### 6.6 复核结论
- **P0**：**0 项**。四条核心链路（a2a wire+转换+卡片+客户端/服务端、agent-protocol 任务生命周期+HTTP、chat-completions-web、agui 整包）在静态层面**均已落地并有对应测试**。
- **P1**：**7 项真实缺口**（A2A 服务端引导层、A2A 执行属性、A2A registry 服务发现、A2A transport 包装层、A2A `message/stream` 非增量流、agent-protocol 落盘仓库、agui `ProcessResult.interrupt`），均为**结构性缺失**而非行为错误。
- **P2**：**6 项长尾**（`A2aAgent` 门面、客户端 Handler 体系、`AguiTransport` 粒度、`AguiMessage` 工厂、未用依赖、日志）。
- **建议 rollup 状态值**：**`部分完成`（保留长尾）**——理由：agui 整包与 chat-completions-web 确已 1:1 落地，但 a2a 的 **服务端引导层 / registry 服务发现 / transport 包装层 / 执行属性 / `message/stream` 增量流** 与 agent-protocol 的 **落盘仓库** 仍为结构性缺失，属「核心链路可用但非全量 1:1」；**不建议标 `已完成`**，也不建议标 `待办`（P0 链路与 302 个测试已就位）。同时 rollup 的 `16844 / 18860` 行数应更新为 **主 17910 / 测 21254**，测试数 `290` 应更新为 **302**。


---

## 7. 2026-10-22 crate 拆分（依赖隔离）

`crates/agentscope-extensions-protocol` 不再承载实现，按 Java 的 Maven 子模块拆为独立 crate：

| Java 模块 | Rust crate | 依赖要点 |
| --- | --- | --- |
| `agentscope-extensions-a2a-client` | `agentscope-extensions-a2a-client` | core, reqwest, uuid, tokio |
| `agentscope-extensions-a2a-server` | `agentscope-extensions-a2a-server` | 依赖 a2a-client, axum, async-trait |
| `agentscope-extensions-agent-protocol` | `agentscope-extensions-agent-protocol` | harness, axum, http |
| `agentscope-extensions-agui` | `agentscope-extensions-agui` | core-only（`HarnessAgent` impl 改为可选 `harness` feature） |
| `agentscope-extensions-chat-completions-web` | `agentscope-extensions-chat-completions-web` | core, futures |
| （父模块） | `agentscope-extensions-protocol` | 仅 facade：feature-gated re-export |

- 错误类型按 crate 拆分：`A2aError`（a2a-client，a2a-server 复用）、`AgentProtocolError`（agent-protocol）；不再有共享 `ProtocolError`。
- 服务端专属的 card 构造（`ConfigurableAgentCard` / `TransportProperties` / `AgentDescriptor` / `create_agent_card`）从 `card.rs` 移到 `agentscope-extensions-a2a-server/src/card.rs`，以解除拆分后的 orphan-rule 冲突（`impl<T: AgentRunner> AgentDescriptor for T`）。
- 测试随文件迁移，拆分后 304 个（原 302 + 2 个错误类型用例）；**2026-10-28 P1×7 关闭后 341 个**（+37）。
- `agui` 的 `token_usage_events_are_opt_in_and_accumulate` 原本依赖 `serde_json/preserve_order` 被 ACP 传递开启；拆分后改为解析 JSON 断言，不再依赖 feature 统一顺序。

---

## 8. 2026-10-28 执行记录 —— P1×7 关闭

> 本批目标：关闭 §6.4 / §6.6 列出的 7 项 P1。代码按归属落到子 crate，facade（`agentscope-extensions-protocol`）只做 re-export，未新增任何实现代码。未改根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**` 或其它扩展 crate。

### 8.1 逐项交付

| # | P1 项 | 归属子 crate / 文件 | Java 依据 | Rust 产物 | 测试 |
|---|---|---|---|---|---|
| 1 | A2A 服务端引导层 | `agentscope-extensions-a2a-server/src/server.rs`（新，656 行） | `a2a-server/.../server/AgentScopeA2aServer.java`（505 行：`Builder`、`ServiceLoader`、`postEndpointReady`、`getAgentCardJson`、`close`） | `AgentScopeA2aServer` + `AgentScopeA2aServerBuilder`（`agent_card`/`with_transport`/`transport_builder`/`deployment_properties`/`deployment_port`/`with_agent_registry`/`with_custom_transport`/`agent_execute_properties`/`task_registry`）；`build()` 归一化 transport → 派生 card → 建 handler → 建 wrapper（失败丢弃该 transport）；`transport_wrapper`/`require_transport_wrapper`/`transport_names`/`agent_card_json`/`post_endpoint_ready`/`handle_request`/`router`/`close`；`A2aServerBuildError::{NoTransportProperties, NoAvailableTransport}` 文案与 Java 两条 `IllegalArgumentException` 逐字对齐 | `build_requires_transport_properties_or_a_deployment_port`、`deployment_properties_synthesize_the_default_jsonrpc_transport`、`configured_transports_without_a_builder_are_rejected`、`custom_builders_can_add_transports`、`handle_request_routes_through_the_named_transport_wrapper`、`message_stream_is_served_incrementally_over_a_transport`、`post_endpoint_ready_registers_with_every_registry_once` |
| 2 | `AgentExecuteProperties` | `agentscope-extensions-a2a-server/src/executor.rs`（新） | `executor/AgentExecuteProperties.java`（66 行）；`AgentScopeAgentExecutor.java`（`generateRequiredEventTypes` / `convertToResponseMessage` / `processTaskBlocking` / `processTaskNonBlocking`） | `AgentExecuteProperties{complete_with_message, require_inner_message}` + builder；`AgentOutputProjector`（required-event-type 表 + `Msg` 投影 + `compacted()`）；接线：`A2aRequestHandler::new_with_properties` / `execute_properties()`，`run_task` 与流式状态机均经 projector | `properties_default_to_java_defaults_and_are_buildable`、`text_and_thinking_deltas_are_always_projected`、`inner_events_need_require_inner_message`、`agent_result_falls_back_only_when_nothing_was_streamed`、`compaction_merges_deltas_of_the_same_message`、`require_inner_message_gates_tool_output`、`complete_with_message_only_applies_to_non_blocking_runs` |
| 3 | `AgentRegistry` / `AgentRegistryService` | `agentscope-extensions-a2a-server/src/registry.rs`（新） | `registry/AgentRegistry.java`（`registryName()` + `register(AgentCard, List<TransportProperties>)`）、`registry/AgentRegistryService.java`（遍历 + 单点失败隔离） | `AgentRegistry` trait + `AgentRegistryError`；`AgentRegistryService`（`registries`/`is_empty`/`register -> RegistrationReport`）；`RegistrationReport{succeeded, failed}` 让 Java 只写日志的成败结果对 Rust 调用方可观测。**NACOS 等发现后端直接实现该 trait 即可复用**（本批未改 nacos crate） | `register_reaches_every_registry_with_the_served_transports`、`one_failing_registry_never_blocks_the_others`、`no_transport_means_no_registry_call`、`post_endpoint_ready_registers_with_every_registry_once` |
| 4 | `TransportWrapper*` / `DeploymentProperties` | `agentscope-extensions-a2a-server/src/transport.rs`（新） | `transport/TransportWrapper.java`、`TransportWrapperBuilder.java`、`DeploymentProperties.java`（host 默认 `NetworkUtils.getLocalIpAddress()`）、`CustomTransportProperties.java`、`transport/jsonrpc/{JsonRpcTransportWrapper,JsonRpcTransportWrapperBuilder,JsonRpcTransportMetadata}.java` | `TransportWrapper`（`async fn handle_request(body, headers, metadata) -> TransportResponse`）、`TransportResponse::{Json, Stream}`（对应 Java 的 `String` / `Flux<String>`）、`TransportWrapperBuilder`（`build` / `build_with_extended_card`）、`JsonRpcTransportWrapper`(+Builder)、`JsonRpcTransportMetadata`、`DeploymentProperties`(+Builder, `local_ip_address()` 用 UDP connect 取本机 IP 且发不出包)、`CustomTransportProperties` trait、`default_transport_builders()`（替代 `ServiceLoader`）。接线：axum `POST /` 改走 `JsonRpcTransportWrapper`；`Builder::with_custom_transport` 在 build 时注入 deployment 并转标准属性 | `jsonrpc_wrapper_declares_the_jsonrpc_transport_type`、`empty_and_malformed_bodies_become_jsonrpc_errors`、`non_streaming_requests_return_one_json_response`、`streaming_requests_return_a_stream_of_json_responses`、`streaming_method_detection_matches_the_a2a_methods`、`deployment_properties_require_a_port_and_default_the_host`、`default_transport_builders_expose_jsonrpc_only`、`send_helpers_parse_a_raw_jsonrpc_body`、`custom_transport_properties_are_resolved_at_build_time` |
| 5 | `message/stream` SSE 增量 | `agentscope-extensions-a2a-server/src/{lib,transport}.rs` | `transport/jsonrpc/JsonRpcTransportWrapper.java#handleStreamRequest`（`Flow.Publisher` → `Flux<String>`，每响应一帧） | `A2aRequestHandler::stream_responses(&self, request) -> BoxStream<'static, JsonRpcResponse>`：`StreamPhase::{Start, Running, Done}` + `futures::stream::unfold` 状态机（首帧 `working`(final=false) → 每个投影事件一帧 `artifact-update`(append=true, lastChunk=false) → 终帧 `completed`(final=true)；失败为 `failed` + 文本 message）。`AgentRunner::stream_events_incremental(...) -> Option<BoxStream<..>>` 是接 core 流式抽象的挂点（core `ReActAgent::stream_events` 形态一致）；未实现时回放 `stream_events` 缓冲结果。axum `POST /` 对流式方法返回 `Sse<...>`（`data: <json-rpc response>`） | `stream_responses_emits_before_the_run_finishes`（channel 驱动：在 run 未结束时已收到 artifact 帧，缓冲实现必然挂死）、`message_stream_relays_the_run_incrementally`、`message_stream_is_served_incrementally_over_a_transport`、`streaming_requests_return_a_stream_of_json_responses` |
| 6 | `WorkspaceProtocolTaskRepository` | `agentscope-extensions-agent-protocol/src/workspace_repository.rs`（新） | `agent-protocol/.../WorkspaceProtocolTaskRepository.java`（`writeTaskRecord`/`readTaskRecord` 走专用 `WorkspaceManager`，桶为 `PROTOCOL_AGENT_ID`/`PROTOCOL_SESSION_ID`） | `WorkspaceProtocolTaskRepository`：`new(path)` / `from_manager(Arc<WorkspaceManager>)` / `workspace_manager()` / `record_path()` / `save_async` / `find_async` / `list_async` + 同步 `ProtocolTaskRepository` 实现（`futures::executor::block_on`；空白 id 视为未命中，与 Java `find` 一致）。接线：`AgentProtocolTaskStore::with_workspace_task_store(factory, properties, customizers)` 消费 `AgentProtocolProperties::resolved_task_store_path()`，使 `task_store_path` 首次真正生效 | `records_round_trip_through_the_workspace_bucket`、`save_upserts_and_list_returns_every_record`、`a_missing_or_blank_id_yields_none`、`the_sync_trait_impl_persists_and_reads`、`a_second_repository_over_the_same_path_sees_the_records` |
| 7 | `ProcessResult.interrupt(threadId)` | `agentscope-extensions-agui/src/processor/request_processor.rs` + `adapter/agent_adapter.rs` | `agui/processor/AguiRequestProcessor.java:111`（`AguiUtil.asReActAgent(agent)` → `interrupt(RuntimeContext(sessionId=threadId))`，否则 `agent.interrupt()`） | `ProcessResult::interrupt(&self, thread_id) -> bool`；新增 `AguiStreamableAgent::interrupt_session(&str) -> bool`（默认 `interrupt()` 并返回 false，`ReActAgent`/`HarnessAgent` 各自覆写为同名固有方法）；`ProcessResult` 新增 `agent: Option<Arc<dyn AguiStreamableAgent>>`，`process()` 在成功路径填充 | `interrupt_targets_the_thread_session`、`interrupt_falls_back_for_agents_without_a_session_scope`、`process_exposes_the_resolved_agent_for_interruption` |

### 8.2 命令与实测

```
cargo test -p agentscope-extensions-protocol -p agentscope-extensions-a2a-client \
  -p agentscope-extensions-a2a-server -p agentscope-extensions-agent-protocol \
  -p agentscope-extensions-agui -p agentscope-extensions-chat-completions-web -j 2
```

| crate | 改前 | 改后 | 结果 |
|---|---|---|---|
| `agentscope-extensions-a2a-client` | 39 | 39 | 0 failed |
| `agentscope-extensions-a2a-server` | 15 | **47** | 0 failed（15 旧测试全保留） |
| `agentscope-extensions-agent-protocol` | 24 | **29** | 0 failed |
| `agentscope-extensions-agui` | 188 | **191** | 0 failed |
| `agentscope-extensions-chat-completions-web` | 35 | 35 | 0 failed |
| `agentscope-extensions-protocol`（facade） | 0 | 0 | 无测试（仅 re-export） |
| **合计** | 304 | **341** | **341 passed / 0 failed / 0 ignored** |

> 未删任何既有测试、未放宽既有断言、未 `#[ignore]`。`message_stream_returns_a_final_status_update` 因语义变化（一次性数组 → 增量序列）被改写为 `message_stream_relays_the_run_incrementally`，断言由「长度恒为 1」加强为「首帧 working / 中段 artifact / 末帧 completed」；`ProcessResult` 新增字段使两个既有测试构造处补 `agent` 字段（断言未动）。

### 8.3 反向验证（回退实现 → 确认 FAILED → 恢复）

| # | 回退点 | 期望失败用例 | 实测 |
|---|---|---|---|
| 1 | `executor.rs` 把 `ToolResultTextDelta(data) if self.require_inner_message` 改为 `if true \|\| ...`（内层事件恒投影） | `tests::require_inner_message_gates_tool_output` | FAILED（0 passed / 1 failed，断言 artifacts 数量 1 vs 2） |
| 2 | `request_processor.rs` 把 `Some(agent) => agent.interrupt_session(thread_id)` 改为 `{ agent.interrupt(); false }`（丢掉 session 定位） | `processor::request_processor::tests::interrupt_targets_the_thread_session` | FAILED（sessions 记录为空） |
| 3 | `workspace_repository.rs` 在 `save_async` 里提前 `return Ok(())`（不落盘） | `records_round_trip_through_the_workspace_bucket` / `save_upserts_and_list_returns_every_record` / `the_sync_trait_impl_persists_and_reads` / `a_second_repository_over_the_same_path_sees_the_records` | FAILED（1 passed / 4 failed） |
| 4 | `transport.rs` 把 `is_streaming_method` 改为只匹配 `never/stream`（流式判定失效） | `transport::tests::streaming_requests_return_a_stream_of_json_responses` / `streaming_method_detection_matches_the_a2a_methods` | FAILED（4 passed / 2 failed） |

四处回退均已恢复，`Select-String` 复核四个文件无残留（`never/stream` / `if true ||` / `agent.interrupt(); false` / 提前 `return Ok(())` 均 0 命中），恢复后重跑全绿。

### 8.4 偏差与设计决定（本批新增）

1. **`ServiceLoader` → 显式 builder 列表**：Rust 无 SPI 扫描，`default_transport_builders()` 提供 JSON-RPC，其它 transport 通过 `Builder::transport_builder(..)` 注册（重名后者覆盖前者，对齐 Java `Collectors.toMap` 的 merge 语义）。
2. **`TransportWrapper::handle_request` 改 async**：Java 同步是因为其 SDK 非流式 handler 同步；Rust `A2aRequestHandler` 为 async。流式半部仍同步产出 `BoxStream`。`TransportWrapperBuilder::build` 不再收 `Executor`（Rust 的流跑在调用方 async 任务上）。
3. **`AGENT_RESULT` 兜底分支**：Java 的 required-event-type 表只有块级 delta；Rust `AgentRunner` 默认只有消息级契约，故 `AgentOutputProjector` 在该 run 无任何流式输出时才投影 `AGENT_RESULT`（否则会与 delta 重复）。
4. **`ProcessResult.agent` 为 `Option`**：Rust 在 agent 解析失败时以 `RUN_ERROR` 事件收尾（不返回 Err），该路径无 agent 可挂；Java 先解析再建惰性 `Flux`，恒有值。
5. **`WorkspaceProtocolTaskRepository` 的同步/异步双面**：`ProtocolTaskRepository` 是同步 trait（`submit`/`snapshot` 链路为同步），而 harness `WorkspaceManager` 的 task-record API 是 async → trait impl 用 `futures::executor::block_on` 驱动（本地盘走 `std::fs` + runtime 无关的 `tokio::sync` 锁，无需 reactor），另暴露 `save_async`/`find_async`/`list_async`。
6. **`tasks/resubscribe` 不做流式**：Java transport wrapper 同时接受 `message/stream` 与 `tasks/resubscribe`，因为其 SDK 保留 per-task 事件队列；Rust handler 无队列（事件一过即落成 artifact），实现订阅只能重跑 agent，故 `is_streaming_method` 只认 `message/stream`，`tasks/resubscribe` 走普通分发并回 `-32601`。
7. **`close()` 为 no-op**：Java 用于 join handler 的队列线程；Rust handler 无后台线程。

### 8.5 未完成（如实记录）

| 项 | 优先级 | 原因 |
|---|---|---|
| `BaseReActAgentRunner` / `ReActAgentWithBuilderRunner`（per-task agent 缓存 + `stop()` 触发 `interrupt()` + 流结束移除缓存） | P1（不在本批 7 项内） | 需要 harness/core 侧「按 task 缓存 agent 并中断」的装配约定；本批已提供 `stream_events_incremental` 挂点，可与该 runner 一并落地 |
| `tasks/resubscribe` 事件重放 | P2 | 需要 per-task 事件队列（Java 由 SDK 的 `QueueManager` 提供）；本批只能回 `-32601`，见 §8.4 第 6 条 |
| `AguiRequestProcessorBuilder` 暴露 `adapterFactory` | P1 长尾 | 需先定义 Rust 侧 factory 与 processor 的持有关系（`AguiAgentAdapterFactory` trait 已存在，未接线） |
| `A2aAgent` 高层客户端门面 + `A2aAgentConfig` + 客户端 `agent.event.*` Handler 体系 + 客户端 artifacts 优先 | P2 | 需要在 a2a-client 侧实现 core `Agent` trait（memory/hook/middleware/session/interrupt），本批未触及 |
| `bytes` 未用依赖清理 | P2 | 需汇总方确认是否影响其它 crate（本批未动 `Cargo.toml` 的既有依赖项，只新增 `futures`/`tracing`） |

> **注意**：本次执行期间 `crates/agentscope-extensions-model` 被另一会话并发编辑，曾导致依赖链暂时无法编译（`DEFAULT_CONVERSATION_HISTORY_PROMPT` / `dashscope` 非穷尽 match）；本批未修改该 crate，等待其稳定后重跑，最终 6 crate 全绿。

## 2026-10-29 执行记录（§1.1 跳模块项 ④）

| 项 | 变更 | 验证 |
|---|---|---|
| `AgentCard` 补 `documentationUrl` | `agentscope-extensions-a2a-client/src/model.rs` 新增 `documentation_url: Option<String>`（serde `documentationUrl`，未设置时不输出 `null`）；`AgentCard::new` 默认 `None` | 新用例 `model::tests::agent_card_serializes_documentation_url`；a2a-client **40 passed** |
| `TransportProperties` 下沉 | 新增 `a2a-client/src/transport_properties.rs`（`TransportProperties` + Builder + `effective_port` / `normalized_path` / `to_url` / `to_agent_interface`）；`a2a-server/src/card.rs` 删除本地实现改为 `pub use`，公共路径 `a2a::card::TransportProperties` 不变；随迁移一并删除已无用的 `card_converter::{HTTPS,HTTP}` 常量 | a2a-client 40 / a2a-server 47 / nacos 70 全绿 |
| card 转换接线 | `create_agent_card` 现在把 `ConfigurableAgentCard.documentation_url` 传入 wire card（此前丢弃） | 同上 |

**反向验证**：去掉 `documentationUrl` 的 serde rename → `agent_card_serializes_documentation_url` FAILED（39 passed / 1 failed）；恢复后 40 passed。

**遗留说明**：NACOS 仍保留自己的必填版 `a2a::registry::TransportProperties`（`host: String` / `port: i32`），因其来自 `AgentRegistry::register` 的必填语义，与共享版的 `Option` 形态不同（见 `NACOS__GAP_TRACKER.md` §4.2-6）。
