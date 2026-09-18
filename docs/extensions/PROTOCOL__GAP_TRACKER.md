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
| Java 规模（实测行数） | a2a-client 2401 / a2a-server 2824 / agent-protocol 1443 / agui 8325 / chat-completions-web 1851（主代码，不含测试） |
| Rust crate | `crates/agentscope-extensions-protocol` |
| 迁移波次 | A |
| 当前状态 | **部分完成** — a2a（client+server+card+model+message）、agent-protocol、chat-completions-web 已落地并测试全绿；agui **部分迁移**（event + model 已落地，adapter/converter/encoder/processor/registry/runtime 未迁移） |

### 1.1 验证基线（最近一次实测）
```
cargo build -p agentscope-extensions-protocol   -> Finished (0 errors)
cargo test  -p agentscope-extensions-protocol   -> 130 passed; 0 failed; 0 ignored
```

## 2. Java 清单与依赖映射

### 2.1 a2a-client（`io.agentscope.core.a2a.agent.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `agent.message.MessageConstants` | 元数据键与 block 类型常量 | `a2a::message::MessageConstants` | 已完成 |
| `agent.message.*ParserRouter`（Text/File/Data/Thinking/ToolUse/ToolResult/Hint 等） | Part ↔ ContentBlock 解析路由 | `a2a::message::{part_to_content_block, content_block_to_part, file_part_to_block, data_part_to_block, media_part, mime_type_from_url}` | 已完成 |
| `agent.utils.MessageConvertUtil` | Msg ↔ A2A Message 转换 + 流式分块累积 | `a2a::message::MessageConvert`（含 `StreamingChunkAccumulator`） | 已完成 |
| `agent.card.AgentCardResolver` | AgentCard 解析接口 | `a2a::card::AgentCardResolver` (trait) | 已完成 |
| `agent.card.FixedAgentCardResolver` (+Builder) | 固定卡片解析 | `a2a::card::{FixedAgentCardResolver, FixedAgentCardResolverBuilder}` | 已完成 |
| `agent.card.WellKnownAgentCardResolver` | `/.well-known/agent-card.json` 拉取 | `a2a::card::{WellKnownAgentCardResolver, WellKnownAgentCardResolverBuilder, CardFetcher, ReqwestCardFetcher}` | 已完成 |
| `agent.A2aAgent` / `A2aAgentConfig` | 远程 A2A 智能体与配置 | `a2a::client::{A2aClient, SendResult, JsonRpcTransport, ReqwestJsonRpcTransport}` | 部分完成（客户端与传输已实现；高层 Agent 门面未迁移） |
| `agent.event.*`（ClientEventContext/Handler/Router、Message/Task/TaskUpdate EventHandler） | 流式事件处理 | `a2a::client::{decode_stream_result, parse_sse_stream, is_terminal_state, is_final_status}` + `a2a::model::StreamEvent` | 部分完成（事件解码已实现，Java 的 Handler 类体系未 1:1 迁移） |
| `agent.message.BaseMediaBlockParser` 等 | Source→FilePart MIME 推导 | `a2a::message::{media_part, mime_type_from_url}` | 已完成 |

### 2.2 a2a-server（`io.agentscope.core.a2a.server.*`）
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `AgentScopeA2aServer` / `request.AgentScopeA2aRequestHandler` | JSON-RPC 请求处理 | `a2a::server::{A2aRequestHandler, router}` | 已完成 |
| `executor.AgentScopeAgentExecutor` | 任务执行编排 | `a2a::server::{AgentRunner, A2aTaskRegistry}` | 已完成（以 trait + 注册表形式） |
| `executor.runner.{AgentRunner, BaseReActAgentRunner, ReActAgentWithBuilderRunner, AgentRequestOptions}` | runner 体系 | `a2a::server::AgentRequestOptions` + `AgentRunner` trait | 部分完成 |
| `server.utils.MessageConvertUtil` | 服务端 Msg↔A2A 转换 | `a2a::server::ServerMessageConvert` | 已完成 |
| `card.AgentScopeAgentCardConverter` | AgentDescriptor→AgentCard | `a2a::card::{AgentDescriptor, create_agent_card, ConfigurableAgentCard, TransportProperties}` | 已完成 |
| `registry.{AgentRegistry, AgentRegistryService}` | 智能体注册/发现 | `a2a::server::A2aTaskRegistry`（部分）| 部分完成 |
| `transport.*` / `transport.jsonrpc.*` | JSON-RPC 传输与包装 | `a2a::client::JsonRpcTransport` + `a2a::server::router` | 部分完成 |
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
| `WorkspaceProtocolTaskRepository` / `ProtocolTaskRepository` | 任务持久化仓库 | `agent_protocol::{ProtocolTaskRepository, InMemoryProtocolTaskRepository}` | 部分完成（内存仓库；workspace 落盘仓库未迁移） |
| `AgentProtocolController` | HTTP 控制器（`/api/tasks/...`） | `agent_protocol::router` (axum) | 已完成 |
| `AgentProtocolAutoConfiguration` | Spring 自动装配 | 不适用（Rust 无 Spring） | 设计决定（不迁移）|

### 2.4 agui（`io.agentscope.core.agui.*`）— **部分迁移**
| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `event.AguiEvent` / `AguiEventType` / `AguiEvents` | AG-UI 事件模型（~60KB） | `agui::event::{AguiEvent, AguiEventType, AguiEvents, JsonPatchOperation, Interrupt, RunFinishedOutcome}` | 已完成 |
| `model.AguiMessage` / `AguiToolCall` / `AguiFunctionCall` | 消息与工具调用模型 | `agui::model::{AguiMessage, AguiToolCall, AguiFunctionCall}` | 已完成 |
| `model.{MessageContent, InputContent*, InputContentSource*}` | content 联合类型与输入部件 | `agui::model::{MessageContent, InputContent, InputContentSource}` | 已完成 |
| `model.{RunAgentInput, AguiContext, AguiResume, AguiTool, ToolMergeMode}` | 请求/上下文/续跑模型 | 无 | **待办 P0** |
| `converter.{AguiMessageConverter, AguiStateConverter, AguiToolConverter}` | 核心转换 | 无 | **待办 P0** |
| `adapter.{AguiAgentAdapter, AguiAdapterConfig, ToolInjection}` + `adapter.strategy.*` | 智能体适配与事件转换策略 | 无 | **待办 P1** |
| `processor.{AguiRequestProcessor, AguiResumeCoordinator}` | 请求处理/断点续跑 | 无 | **待办 P1** |
| `encoder.AguiEventEncoder` | SSE 编码 | 无 | **待办 P1** |
| `registry.AguiAgentRegistry` / `runtime.*` | 注册与运行时上下文 | 无 | **待办 P1** |
| `AguiUtil` / `AguiInterruptConstants` / `AguiException` | 工具/常量/异常 | 无 | **待办 P2** |

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
| Spring `SseEmitter` | SSE 流 | `axum` SSE / `bytes` | 部分完成 |
| `HttpClient` (Java 11) | A2A 客户端 | `reqwest`（workspace）+ `JsonRpcTransport` trait | 已完成 |
| `java.net.URL` + `Files.probeContentType` | URL 扩展名→MIME | `mime_type_from_url`（内置扩展名表） | 已完成（受控子集） |
| `java.util.UUID` | 生成文件名 | `uuid`（workspace） | 已完成 |
| AG-UI 协议类型 / OpenTelemetry (`agui` 依赖) | agui 事件 | 无 | **待办** |

## 3. 详细缺口清单

### 3.1 P0（核心链路）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A wire 模型 | `io.a2a.spec.*` | 已完成 | `a2a/model.rs`（26595B） |
| A2A Msg↔Part 转换 | `a2a-client/.../message/*` | 已完成 | `a2a/message.rs`（36298B），16 单测 |
| A2A 卡解析 | `a2a-client/.../card/*` | 已完成 | `a2a/card.rs`（26384B），6 单测 |
| A2A 客户端 | `a2a-client/.../A2aAgent` | 已完成 | `a2a/client.rs`（17943B），9 单测 |
| A2A 服务端 + JSON-RPC 路由 | `a2a-server/*` | 已完成 | `a2a/server.rs`（38558B），15 单测 |
| agent-protocol 任务生命周期 + HTTP 路由 | `agent-protocol/*` | 已完成 | `agent_protocol.rs`（68830B），22 单测 |
| chat-completions-web 模型/转换/构造/流式 | `chat-completions-web/*` | 已完成 | `chat_completions/`（model/converter/builder/streaming） |
| agui 事件模型 | `agui/event/*`（`AguiEvent` 59744B） | **已完成** | `agui/event.rs`（61600B），含 16 单测（事件类型名/序列化/容忍未知类型） |
| agui 消息/内容模型 | `agui/model/*` | **已完成** | `agui/model.rs`（`AguiMessage`/`MessageContent`/`InputContent`/`InputContentSource`/`AguiToolCall`/`AguiFunctionCall`），5 单测 |
| agui 转换/适配/编码/处理/注册 | `agui/{converter,adapter,encoder,processor,registry,runtime}/*` | **待办 P1** | 整包未迁移（`AguiAgentAdapter` 33423B、`AguiMessageConverter` 23830B 等） |
| agui 请求/上下文/续跑模型 | `agui/model/{RunAgentInput,AguiContext,AguiResume,...}` | **待办 P0** | 依赖 converter/processor |

### 3.2 P1（重要）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A 服务端 runner 体系 1:1 | `executor.runner.*` | 部分完成 | 以 `AgentRunner` trait + `A2aTaskRegistry` 抽象；`BaseReActAgentRunner`/`ReActAgentWithBuilderRunner` 未 1:1 |
| A2A 智能体注册与发现 | `registry.*` | 部分完成 | `A2aTaskRegistry` 覆盖任务；AgentRegistry 服务发现未迁移 |
| A2A 传输包装层 | `transport.*` | 部分完成 | 仅 JSON-RPC over HTTP；`TransportWrapper`/`DeploymentProperties` 未迁移 |
| agent-protocol workspace 落盘仓库 | `WorkspaceProtocolTaskRepository` | 部分完成 | 仅 `InMemoryProtocolTaskRepository` |
| SSE 流式响应 | Spring `SseEmitter` | 部分完成 | axum router 已建；完整 SSE 编码需与 agui/`streaming` 一并补齐 |
| agui 适配器 + 转换策略 | `agui/adapter/*` | 待办 | 依赖 agui converter 先落地 |
| agui 请求处理/续跑 | `agui/processor/*` | 待办 | `AguiResumeCoordinator` |

### 3.3 P2（增强）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| A2A MIME 探测全表 | `Files.probeContentType` | 部分完成 | Rust 侧为受控扩展名表（png/jpg/gif/webp/bmp/mp3/wav/ogg/mp4/webm/mov）；Java 走系统探测 |
| A2A 客户端事件 Handler 类体系 | `agent.event.*` | 部分完成 | 由函数式解码替代，无 Handler 对象继承树 |
| agui 工具/常量/异常 | `AguiUtil` 等 | 待办 | 低优先 |
| Spring AutoConfiguration | `*AutoConfiguration` | 设计决定（不迁移） | Rust 无 DI 容器 |
| `NetworkUtils` | `a2a-server/utils` | 设计决定（不迁移） | 逻辑并入 card/client |

## 4. 项目状态与下一步

### 4.1 当前状态
- **Rust 产物**：`src/lib.rs` + `src/error.rs` + `src/agent_protocol.rs` + `src/chat_completions/*` + `src/agui/{mod,event,model}.rs` + `src/a2a/{mod,model,message,card,client,server}.rs`。
- **测试**：130 个 `#[test]`。
- **验证（实测）**：
  ```
  cargo build -p agentscope-extensions-protocol  -> Finished, 0 error
  cargo test  -p agentscope-extensions-protocol  -> 130 passed; 0 failed; 0 ignored
  ```
- **会话内根因修复**：
  - `chat_completions/streaming.rs` E0700：`stream()` 闭包捕获 `&self` 导致生命周期泄露至返回的 `impl Stream` → 改为在闭包内复制 `Copy` 适配器。
  - `chat_completions/builder.rs` 单测构造非法 `Msg`（User + ToolUse）触发 core `validate_role_content` panic；改用命中同一分支的合法 role（仅测体，Java `Msg.builder` 无此校验属 core/Java 差异）。
  - `agui` 接线：`lib.rs` 缺 `pub mod agui;` 与 `agui/model.rs` → 补齐 `model.rs`+`mod.rs`（忠实映射 Java `agui/model`）并在 `lib.rs` 声明。
  - `agui/event.rs` 的 `parse_json<'a, T: Deserialize<'a>>` 在 `serde_json::from_value` 上缺 `DeserializeOwned` → 收紧泛型约束。
  - `a2a/message.rs` 1 个测试断言写错（非实现 bug）：详见 §4.3。
- **已知问题**：build 存在若干 unused-import 警告（`FunctionCallStream`、`HarnessAgentBuilder`、`SdkResult`、`TextBlock`、`SandboxState`、`WorkspaceSpec`、`ExecResult` 等），不影响编译与测试。

### 4.2 下一步与阻塞
1. **P0 未落地**：agui 的 `RunAgentInput`/`AguiContext`/`AguiResume`/`AguiTool`/`ToolMergeMode` 与 `converter.*`（`AguiMessageConverter` 23830B 是核心）。建议顺序：模型补齐 → converter → adapter/encoder → processor/registry。
2. **P1 补齐**：A2A runner/registry/transport 的 1:1 补齐、agent-protocol workspace 落盘仓库、完整 SSE。
3. **跨模块需求（需汇总方裁定）**：agui 的事件/续跑协调可能需要 harness 的 agent 事件总线与中断（interrupt/HITL）抽象——若 harness 现有抽象不足，须由汇总方决定扩展 harness 还是在本 crate 内建适配层；**本子智能体未改 harness/core**。
4. 无 workspace 级依赖变更需求（现有 `axum`/`reqwest` 已在本 crate `[dependencies]` 声明）。

### 4.3 失败测试根因（已修复）
- **用例**：`a2a::message::tests::media_part_mime_falls_back_to_primary_type_for_unknown_extension`（`src/a2a/message.rs:765`）。
- **现象**：`assertion failed: part_to_content_block(&part).is_none()`。
- **根因**：**测试断言写错（非实现 bug）**。该测试构造 `AudioBlock(URLSource("https://example.com/sound.unknownext"))`，`mime_type_from_url` 因扩展名未知回退为 `media_type="audio"`，序列化后 `file.mimeType == "audio"`。回程 `part_to_content_block → file_part_to_block` 取 primary type 为 `"audio"`（无 `/` 时返回整串），命中 `TYPE_AUDIO` 分支 → 正常还原为 `AudioBlock`。Java `FilePartParser#getPrimaryType` 对无 `/` 的串返回原串，`switch case MessageConstants.BlockContent.TYPE_AUDIO` 即 `"audio"` 同样还原为 `AudioBlock`，故 **Java 语义下必然可映射回**，原断言 `is_none()` 与 Java 及测试名相悖。
- **修法**：将断言改为断言可还原为 `ContentBlock::Audio` 且 URL 源为原址（与 Java `FilePartParser` 一致）；未删断言、未删功能、未 `#[ignore]`。
- **变更文件**：`crates/agentscope-extensions-protocol/src/a2a/message.rs`（仅测试体）。
- **Java 依据**：`a2a-client/src/main/java/io/agentscope/core/a2a/agent/message/FilePartParser.java`（`getPrimaryType` + `switch ... TYPE_AUDIO`）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-protocol` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-protocol` / `cargo test -p agentscope-extensions-protocol` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-protocol/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

