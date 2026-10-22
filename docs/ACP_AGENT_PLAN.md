# ACP 协议 CodingAgent（Rust 版）实现方案

> **定位**：本文档承载**方案**（可行性、依赖映射、技术难点、测试策略、架构设计）。
> **进度与缺口清单**：见 `docs/ACP_AGENT__GAP_TRACKER.md`（跨会话单一事实来源）。
> **参考**：Kotlin `AgiHarnessAgent.kt`（ACP AgentSession 实现）、Java `HarnessCodingAgent.java`（Harness Agent 装配）、ACP RDF 文档 (E:\local-github\agentclientprotocol\agent-client-protocol)、ACP Rust SDK（`E:\local-github\agentclientprotocol\rust-sdk`）。
> **目标**：在 `agentscope-harness` crate 内实现功能完整的 ACP 协议 CodingAgent（Rust 版），包装核心 `ReActAgent`，通过 stdio 与 ACP 客户端（IDE/CLI）通信。

## 1. 背景与目标

### 1.1 现状
- `agentscope-harness` 已声明 `agent-client-protocol = { path = ".../rust-sdk/src/agent-client-protocol" }`（v2.0.0）依赖，但**代码中尚未使用**。
- 核心 `ReActAgent`（`agentscope-core`）已实现 reasoning→acting 循环、tool 治理层、事件流、结构化输出等。
- harness `HarnessAgent` 已装配 Filesystem + 工具 + middleware，但**无 call()/ReAct**，仅装配器。

### 1.2 目标
实现一个 **ACP Agent（服务端角色）**，通过 stdio transport 与 ACP Client 通信，处理：
- `initialize` — 协商协议版本与能力
- `session/new`、`session/load`、`session/resume`、`session/list`、`session/close`、`session/delete`
- `session/prompt` — 核心：驱动 ReActAgent 循环，流式发送 `SessionUpdate`（文本/思考/工具调用/用量）并返回 `PromptResponse`
- `session/set_mode`、`session/set_config_option` — 会话配置
- `$/cancel_request` 通知 — 取消进行中的请求
- 权限 HITL（`requestPermissions`）— 可选增强

### 1.3 非目标（本轮）
- MCP server 对接（依赖 `unstable_mcp_over_acp`，后续）
- WebSocket/HTTP transport（本轮 stdio）
- ACP 协议 v2（`unstable_protocol_v2`，后续）

## 2. 依赖与 API 映射（Kotlin/Java → Rust）

| Kotlin/Java 参考 | Rust 等价 | 说明 |
|---|---|---|
| `AgentSupport` / `AgentSession` | `agent_client_protocol::role::acp::Agent` + `Builder` 处理器 | 服务端角色 |
| `StdioTransport` | `agent_client_protocol::Stdio` | stdio transport |
| `AgentInfo`（能力声明） | `schema::v1::InitializeResponse.agent_capabilities()` | 能力协商 |
| `SessionUpdate.AgentMessageChunk` | `schema::v1::SessionUpdate::AgentMessageChunk(ContentChunk)` | 文本流 |
| `SessionUpdate.AgentThoughtChunk` | `SessionUpdate::AgentThoughtChunk` | 思考流 |
| `SessionUpdate.ToolCall/Update` | `SessionUpdate::ToolCall` / `ToolCallUpdate` | 工具进度 |
| `SessionUpdate.UsageUpdate` | `SessionUpdate::UsageUpdate` | 用量 |
| `PromptResponse(StopReason.END_TURN)` | `schema::v1::PromptResponse::new(StopReason::EndTurn)` | 回合结束 |
| `ClientSessionOperations.requestPermissions` | `schema::v1::RequestPermissionRequest`（客户端发） | 权限 HITL |
| `LocalAgent`（ReActAgent 包装） | `agentscope_core::agent::ReActAgent` | 核心循环 |
| `HarnessAgent` 装配 | `agentscope_harness::agent::HarnessAgent` | 装配 Filesystem+工具 |

## 3. 架构设计

### 3.1 模块划分（新增 `crates/agentscope-extensions-acp/`）
```
acp/
├── mod.rs            # 模块导出
├── coding_agent.rs   # AcpCodingAgent：Agent builder + 会话注册表 + 处理器装配
├── session.rs        # AcpAgentSession：单会话状态 + prompt 驱动 ReActAgent + 事件→SessionUpdate
├── config.rs         # 会话配置（model/mode）、SessionConfigOption 构造
└── event.rs          # AgentEvent → SessionUpdate 转换
```

### 3.2 数据流
```
ACP Client ──stdio──> AcpCodingAgent (Agent.builder())
                        │  on_receive_request 分发:
                        │   InitializeRequest  → InitializeResponse(能力)
                        │   NewSessionRequest  → 建 AcpAgentSession + NewSessionResponse
                        │   PromptRequest      → 驱动 ReActAgent.stream_events()
                        │                        ├─ AgentEvent → SessionUpdate 通知（cx.send_notification）
                        │                        └─ 收尾 → PromptResponse
                        │   SetSessionConfigOptionRequest → 更新会话配置
                        │   CloseSessionRequest → 清理会话
                        └─ on_receive_notification:
                             CancelRequestNotification → 取消进行中请求
```

### 3.3 会话状态（`AcpAgentSession`）
- `session_id: SessionId`
- `cwd: PathBuf`
- `agent: ReActAgent`（按 cwd/配置构建）
- `config: SessionConfig`（model、mode 等）
- `active_request: Option<CancellationToken>`（用于取消）
- `usage: ChatUsage`（累计）

## 4. 实现步骤（分阶段）

### Phase 1：骨架与初始化
- [ ] 建 `acp/` 模块目录与 `mod.rs`
- [ ] `AcpCodingAgent::builder()` → `Agent.builder()`，注册 `InitializeRequest` 处理器，声明能力（loadSession/list/resume/close、promptCapabilities、mcpCapabilities 等）
- [ ] `connect_stdio()` 便捷方法（`Stdio::new()` + `connect_to`）
- [ ] 编译通过

### Phase 2：会话生命周期
- [ ] `NewSessionRequest` → 建 `AcpAgentSession`，返回 `NewSessionResponse`（含 modes/config_options）
- [ ] `CloseSessionRequest`、`ListSessionsRequest`、`LoadSessionRequest`、`ResumeSessionRequest`、`DeleteSessionRequest`
- [ ] 会话注册表（ConcurrentHashMap 等价 → `Mutex<HashMap>`）

### Phase 3：Prompt 核心（ReAct 驱动）
- [ ] `PromptRequest` → 将 ContentBlock 转为 `Vec<Msg>`，调用 `ReActAgent.stream_events`
- [ ] 事件转换：TextBlockDelta→AgentMessageChunk、ThinkingBlockDelta→AgentThoughtChunk、ToolCallStart/End→ToolCall/ToolCallUpdate、AgentEnd→收尾
- [ ] 返回 `PromptResponse(StopReason::EndTurn)`
- [ ] 错误处理：异常→`StopReason::Refusal` + AgentMessageChunk 错误文本

### Phase 4：配置与取消
- [ ] `SetSessionConfigOptionRequest`（model/mode）→ 更新会话配置 + `ConfigOptionUpdate` 通知
- [ ] `SetSessionModeRequest`、`SetSessionModelRequest`
- [ ] `$/cancel_request` 通知 → 取消进行中请求 → `StopReason::Cancelled`

### Phase 5：权限 HITL（增强）
- [ ] 工具调用前，若会话权限为 Ask → 发 `RequestPermissionRequest` 给客户端，等待 `RequestPermissionResponse`
- [ ] 依据 outcome 决定批准/拒绝工具调用

### Phase 6：入口与测试
- [ ] 单元测试：会话生命周期、Prompt 事件转换、配置更新
- [ ] 集成：用 `agent-client-protocol-test` 的 mock transport 走通 initialize→new_session→prompt→close

## 5. 测试策略
| 类型 | 方案 |
|---|---|
| 单元测试 | `#[cfg(test)]` + `tokio::test`，覆盖事件转换、配置构造、会话注册表 |
| 协议集成 | `agent-client-protocol-test` mock transport 或内存 channel 走通握手+prompt |
| e2e | `tests/acp_cancel_test.rs` 用脚本化 Model（无需 key）在进程内跑通 stdio 链路；`tests/acp_stdio_thought_test.rs` 用真实模型跑通 stdio |
| 真实 provider | 复用 `agentscope_extensions_model::OpenAiChatModel`（需 key，可选）。**2026-09-21 起** provider 实现位于 `agentscope-extensions-model`，core 仅留 trait / 注册表 / SPI |

## 6. 结论
- **可行**：SDK 已提供完整的 Agent（服务端）builder 与 schema 类型；核心 ReActAgent 已就绪，只需做「ACP 协议层 ↔ 核心事件层」的桥接。
- **主要工作**：事件转换（AgentEvent→SessionUpdate）、会话状态管理、prompt 驱动循环、配置与取消。
- **风险**：SDK 的 async-closure 宏（`on_receive_request!()`）需按示例正确传参；schema 类型字段较多，需精确匹配。

> 进度与缺口清单见 `docs/ACP_AGENT__GAP_TRACKER.md`。
