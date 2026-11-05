# ACP 协议 CodingAgent（Rust 版）实现方案

> **定位**：本文档承载**方案**（可行性、依赖映射、技术难点、测试策略、架构设计）。
> **进度与缺口清单**：见 `docs/ACP_AGENT__GAP_TRACKER.md`（跨会话单一事实来源）。
> **参考**：Kotlin `AgiHarnessAgent.kt`（ACP AgentSession 实现）、Java `HarnessCodingAgent.java`（Harness Agent 装配）、ACP RDF 文档、ACP Rust SDK（本地 `D:\IdeaProjects\agentclientprotocol\rust-sdk`，**tag `v5.0.0`**；旧路径 `E:\local-github\...` 已失效）。
> **SDK 基线**：方案正文中的 `schema::v1::` 路径与 `Agent.builder()` 等 API 按 **2.2.0** 口径书写（现行代码即此口径）；**5.0.0 的升级映射见 §7**，两套口径的差异与批次计划见 `docs/ACP_AGENT__GAP_TRACKER.md` §6。
> **目标**：在 `agentscope-harness` crate 内实现功能完整的 ACP 协议 CodingAgent（Rust 版），包装核心 `ReActAgent`，通过 stdio 与 ACP 客户端（IDE/CLI）通信。

## 1. 背景与目标

### 1.1 现状
- `agentscope-extensions-acp` 声明 `agent-client-protocol = { version = "2.2.0", features = [6 个 unstable feature] }`（**crates.io 依赖**，无 path 依赖）；`agentscope-harness` 已不再依赖该 SDK。
  > 2026-10-29 更正：原记「harness 声明 path 依赖 v2.0.0、代码未使用」已失效——SDK 代码整体从 harness 迁到 `agentscope-extensions-acp`，依赖于 2026-10-16 改为 crates.io（见 `docs/ACP_AGENT__GAP_TRACKER.md` §2）。
  > **2026-11-03 更新**：上游已到 **5.0.0 / schema 1.15.1**（我们仍 2.2.0 / 1.9.1）。升级不是简单换版本号：原生传输改成 opt-in feature（`stdio`/`process`）、schema 路径翻转为 `v1::schema::*`、服务端入口改为 `Agent.v1()`、MCP-over-ACP 从「连接式」改为「请求式」、两个 unstable feature 转正、NES 整体移除。详见 Tracker §6.3（`ACP-U01..U08`）。
- 核心 `ReActAgent`（`agentscope-core`）已实现 reasoning→acting 循环、tool 治理层、事件流、结构化输出等。
- harness `HarnessAgent` 已装配 Filesystem + 工具 + middleware，但**无 call()/ReAct**，仅装配器。

### 1.2 目标
实现一个 **ACP Agent（服务端角色）**，通过 stdio transport 与 ACP Client 通信，处理：
- `initialize` — 协商协议版本与能力
- `session/new`、`session/load`、`session/resume`、`session/list`、`session/close`、`session/delete`
- `session/prompt` — 核心：驱动 ReActAgent 循环，流式发送 `SessionUpdate`（文本/思考/工具调用/用量）并返回 `PromptResponse`
- `session/set_mode`、`session/set_config_option` — 会话配置
- `session/set_title`、`session/status`、`session/archive`/`session/unarchive`、`auth/status` — schema 1.15.1 新增的只读/元数据方法与能力位（见 §7.3）
- 取消语义分两层：`session/cancel` 通知中断当前 turn；`$/cancel_request` 取消**某个在飞请求 id**（两者不可混用一个 flag；**均已实现**，见 Tracker `ACP-41`）
- 权限 HITL（`requestPermissions`）— 可选增强

### 1.3 非目标（本轮）
- MCP server 对接（依赖 `unstable_mcp_over_acp`）：连接式实现已落地；SDK 5.0.0 把该协议改为请求式（`mcp/message{serverId, requestId}`，删除 `mcp/connect`/`mcp/disconnect`），迁移见 Tracker `ACP-U04`
- WebSocket/HTTP transport（本轮 stdio；SDK 5.0.0 提供 `agent-client-protocol-http`，评估结论「暂不引入」，见 `ACP-D10`）
- ACP 协议 v2（`unstable_protocol_v2`，后续；注意它**不在**聚合 `unstable` feature 内，需显式开启）
- proxy chains（`proxy/initialize`、`proxy/successor`）：纯 agent 不必实现（`ACP-D08`）
- `session/fork`：需会话存储「复制历史」原语，暂缓（`ACP-D13`）
- `unstable_subagents` 上报：✅ 已实现（`ACP-39`，出带通道 + `subagent_update`），见 `docs/ACP_AGENT__GAP_TRACKER.md` §6.9；收尾项（`stopReason` / `requires_action` / 子会话 id 归属 / 嵌套 / `session/load` 回放 / `_meta`）于 2026-11-05 补齐（同节 `2026-11-05` 小节）

## 2. 依赖与 API 映射（Kotlin/Java → Rust）

| Kotlin/Java 参考 | Rust 等价 | 说明 |
|---|---|---|
| `AgentSupport` / `AgentSession` | `agent_client_protocol::role::acp::Agent` + `Agent.v1()`（2.2.0 口径为 `Agent.builder()`） | 服务端角色；**5.0.0 中 `builder()` 已不存在** |
| `StdioTransport` | `agent_client_protocol::Stdio`（需 feature `stdio`） | stdio transport；2.2.0 下无条件可用 |
| `AgentInfo`（能力声明） | `v1::schema::InitializeResponse.agent_capabilities()`（2.2.0 口径 `schema::v1::...`） | 能力协商 |
| `SessionUpdate.AgentMessageChunk` | `v1::schema::SessionUpdate::AgentMessageChunk(ContentChunk)` | 文本流 |
| `SessionUpdate.AgentThoughtChunk` | `SessionUpdate::AgentThoughtChunk` | 思考流 |
| `SessionUpdate.ToolCall/Update` | `SessionUpdate::ToolCall` / `ToolCallUpdate` | 工具进度 |
| `SessionUpdate.UsageUpdate` | `SessionUpdate::UsageUpdate` | 用量 |
| `SessionUpdate.SubagentUpdate` | `SessionUpdate::SubagentUpdate`（`unstable_subagents`） | 子会话上报（已接，见 `ACP-39`） |
| `PromptResponse(StopReason.END_TURN)` | `v1::schema::PromptResponse::new(StopReason::EndTurn)` | 回合结束 |
| `ClientSessionOperations.requestPermissions` | `v1::schema::RequestPermissionRequest`（客户端发） | 权限 HITL |
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
ACP Client ──stdio──> AcpCodingAgent (Agent.v1()；2.2.0 口径 Agent.builder())
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
- **2026-11-03 追加**：SDK 已跨到 5.0.0，方案里的 `schema::v1::*` 路径与 `Agent.builder()` 都需迁移；新增 §7 给出升级映射与增补能力的设计。

> 进度与缺口清单见 `docs/ACP_AGENT__GAP_TRACKER.md`。

---

## 7. SDK 5.0.0 升级与能力增补（2026-11-03）

> 触发：上游 `agent-client-protocol` 2.2.0 → **5.0.0**（schema 1.9.1 → **1.15.1**，2026-11-02 发布，无 4.x）。逐条缺口与批次见 Tracker §6；本节只讲**怎么做**（迁移映射、难点、设计与测试）。

### 7.1 API 迁移映射（2.2.0 → 5.0.0）

| 维度 | 2.2.0（现行） | 5.0.0 | 处理方式 |
|---|---|---|---|
| 依赖 | `agent-client-protocol = "2.2.0"` + 6 个 unstable feature | `"5.0.0"`；原生传输 opt-in | 加 `stdio`、`process`；删 `unstable_session_notices`、`unstable_session_compaction`（已转正）；保留 `unstable_plan_operations`/`unstable_llm_providers`/`unstable_end_turn_token_usage`/`unstable_mcp_over_acp` |
| MSRV | workspace `rust-version = "1.85.0"` | schema/SDK 均要求 1.88 | workspace 改 `1.88.0`（本机 stable 1.97.1） |
| schema 路径 | `agent_client_protocol::schema::v1::X`（75 处） | `agent_client_protocol::v1::schema::X`（**唯一路径**） | 机械替换；`ProtocolVersion` 等版本中立项仍留 `schema::` |
| 服务端入口 | `Agent.builder()` | `Agent.v1()` | 必须改；裸 `Builder::new(Agent)` 虽可编译但**丢掉 v1 wire guard** |
| v1 侧类型 | 根 `SessionBuilder` / `ActiveSession` / `SessionMessage` | `v1::SessionBuilder` / `v1::ActiveSession` / `v1::SessionMessage` | 加 `v1::` 前缀 |
| 响应器/分发 | `Responder<T>`、`Dispatch<Req, Notif>`（隐式 v1） | 根类型默认 `P = Raw`；v1 用 `v1::Responder` / `v1::Dispatch` | 显式标注处改用 v1 别名 |
| 请求 id | `schema::v1::RequestId` | `RawJsonRpcId`（根）；v1 侧 `v1::RequestId` | 跟随新路径 |
| 连接驱动 | `into_channel_and_future() -> (Channel, Future)` | `-> (Channel, Option<ConnectionDriver>)` | 本仓未使用；将来用需处理 `Option` |
| 错误 | 根 `Error` 带 v1 语义（`session_not_found` 等） | 根 `Error` 版本中立；`ErrorCode` 为 newtype；v1 专有错误在 `v1::schema::Error` | `invalid_params`/`internal_error` 保留；未知会话改用 `v1::schema::Error::session_not_found(id)`（`-32002` + `data.sessionId`） |
| MCP-over-ACP | `connect` → `McpConnectionId` → `mcp/message{connection_id}` → `disconnect` | 无 connect/disconnect；`mcp/message{serverId, requestId, method, params}`；响应 `Result|Error(McpError)` | 重写 `mcp_over_acp.rs`（只支持 MCP 2026-07-28，无 initialize） |
| batch | `TransportFrame::Batch` | 移除，`TransportFrame` 非穷尽 | 本仓未使用 |
| 宏 | `on_receive_request!` 等 | **逐字未变** | 不用改 |

### 7.2 技术难点与风险

1. **静默退化**（最高风险）：`Builder::new(Agent)` 在 5.0.0 变成 raw 模式——initialize 不校验版本、未初始化请求不禁用。编译零告警，只有运行时表现改变。缓解：greps 断言 `rg 'Builder::new\(' crates/agentscope-extensions-acp` 为空，只用 `Agent.v1()`。
2. **MCP 协议形态变更**：不只是改名——没有握手、`requestId` 由我们生成且需跨代理稳定、内层错误有自己的 `McpError` 通道。缓解：把 `mcp_over_acp.rs` 当新模块重写，用 `McpRequestId(Uuid)` 每次调用新建，错误转成工具错误文本。
3. **wire 行为差异**：`otherwise_ignore` 未匹配请求从静默改为回 `-32601`；responder 被 drop 会自动回错。缓解：先跑现有 3 个相关测试文件（`acp_elicitation_test` / `acp_interrupted_turn_test` / `acp_load_replay_test`，共 6 处 `otherwise_ignore`），观察客户端侧是否出现新错误。
4. **无背压 / 单 dispatch loop**：SDK 每连接一个 dispatch loop，内部全 `unbounded` 队列，parked 队列 1024 上限丢最旧；在 ordered callback 里 await 其它入站请求会死锁。缓解：长任务继续丢我们自己的 `event_queue`，handler 内不 await 其它入站调用。
5. **子智能体可见性**：✅ `ACP-39` 已解决。做法是把子代理事件送到**独立出带通道**（`AgentSpawnTool::with_event_sink` + `CallableAgent::call_streaming`），而不是回灌父流——子代理在父 turn 返回后仍在跑，回灌父流只能等到下一次 reasoning 才排队发送。ACP 侧常驻转发任务把它映射成 `subagent_update` + 子会话 chunk。协议细节（`2026-11-05`）：终态必须带 `stopReason`（`end_turn`/`cancelled`/`error`），审批前发 `requires_action`、答复后回 `running`，审批与 `plan_update` 的外层 `sessionId` 用子会话 id，嵌套时 `subagent_update` 外层是直接父会话（帧结构改为 `SubagentFrame{parent_session, child_session, event}`，中间子代理靠 `CallableAgent::set_host_session_id` 拿到自己的会话 id），`session/load` 按 `TaskRepository` 回放已终态的子代理卡片。
6. **fork 缺存储原语**：`session/fork` 需要复制会话历史+元数据，`AcpStore` 目前只有单会话持久化 → `ACP-D13` 暂缓。

### 7.3 能力增补设计（按可落地性排序）

| 能力 | 协议点 | 设计要点 | 能力位 | 风险 |
|---|---|---|---|---|
| 会话标题 | `session/set_title` | handler 校验非空 → 更新 `SessionInfo.title` → 广播 `SessionInfoUpdate` | `sessionCapabilities.title = true` | 低 |
| 会话存活 | `session/status` | `open`（内存命中）/`saved`（仅持久化）/`session_not_found`（`-32002`+`data.sessionId`）；未知字符串按 `saved` | `sessionCapabilities.status = true` | 低 |
| 登录态 | `auth/status` | 恒返 `AuthState::NotRequired`；不发 `auth/status_update` | `agentCapabilities.auth.status = true` | 低（**不得**用 `-32000 auth_required` 拒绝） |
| 目录能力广告 | （无新方法） | 把已实现的 `additional_directories` 补进 session capabilities | `sessionCapabilities.additionalDirectories = true` | 低（漏广告导致客户端不发） |
| 归档 | `session/archive`/`unarchive` | 存储加 `archived` 字段；`session/list` 按 `SessionArchiveFilter` 过滤且**每条带 `archived`**；归档活跃会话先按 close 语义释放 | `sessionCapabilities.archive = true` | 中（SQLite schema 迁移） |
| 系统提示 | `session/new` / `session/fork` 的 `systemPrompt` | 只做 `Append`（拼到 system prompt 末尾）；`Override`/未知 mode **报错不降级**；未广告却收到 → 报错 | `sessionCapabilities.systemPrompt = true`（仅当 `session_factory` 构建时广告；`model_factory` 无传递通道 → 不广告且收到即报错） | 中（Override 会绕过编码规则 → 产品决策） |
| 请求级取消 | `$/cancel_request` | **已完成**：`run_prompt_cancellable` 同时监听 session `AtomicBool` 与 SDK per-request 取消 marker | 无能力位 | 低（升级后回归即可） |
| 连接清理 | （无新方法） | await `termination()` 做 flush/释放/checkpoint | — | 低 |
| 子会话上报 | `SessionUpdate::SubagentUpdate` | 前置：实现事件回灌；再映射 `session_id`/`title`/`state`（`MaybeUndefined` 三态） | client 侧 `ClientCapabilities.subagents` | 高（阻塞） |

### 7.4 分阶段实施（与 Tracker §6.7 的 B0–B4 一一对应）

| 阶段 | 目标 | DoD | 进度 |
|---|---|---|---|
| **B0 升级** | 只看「能编译、能跑」：feature/MSRV、`v1::schema`、`Agent.v1()`、MCP 重写、错误码 | `cargo check --all-targets` + `cargo test -p agentscope-extensions-acp` 全绿；stdio 握手冒烟通过；**零新告警** | ✅ 已完成（2026-11-03） |
| **B1 只读能力** | set_title / status / auth-status / additional_directories 广告 | 单测：空标题 `-32602`、未知会话 `-32002`、`initialize` 能力位断言 | ✅ 已完成（`acp_capabilities_test.rs` 4 条） |
| **B2 存储与生命周期** | archive/unarchive + list 过滤 + termination 清理 | 4 条归档单测；断开后无会话泄漏 | ✅ 已完成（`ACP-36` 2 条测试 + `ACP-42` 1 条单测） |
| **B3 提示与取消** | systemPrompt append / 取消路径回归 / 优雅关闭 | append 生效测试；`acp_cancel_test` 仍绿 | ✅ 已完成（append 单测 + 集成；`$/cancel_request` 回归绿；优雅关闭 HEAD 已有） |
| **B4 可选** | subagents 上报（`ACP-39`）/ fork（`ACP-40`，仍暂缓） | `ACP-39`：出带通道 + `subagent_update` 映射 + 能力门控 + 子会话取消；`cargo test -p agentscope-extensions-acp` 全绿 | ✅ `ACP-39` 已完成（2026-11-04，`tests/acp_subagent_test.rs` 2 条 + 单测 10 条）；收尾项 2026-11-05 全部补齐（集成测试 4 条）；`ACP-40` 未做 |

### 7.5 测试策略增补

| 类型 | 新增用例 |
|---|---|
| 迁移回归 | `initialize` 协商（旧 protocolVersion 客户端仍握手）、未初始化请求被拒（证明 wire guard 生效） |
| MCP | `mcp_over_acp` 离线单测：内层 `Result` 解析、内层 `Error(McpError)` 转工具错误、`requestId` 每次唯一 |
| 能力位 | 一次 `initialize` 断言 `title`/`status`/`archive`/`systemPrompt`/`additionalDirectories` 与实现一致（避免「广告了没实现」） |
| 取消 | `session/prompt` 在飞时发 `$/cancel_request` → 取消码结束且会话可复用；与 `session/cancel` 语义不串 |
| 归档 | archive → 默认 list 不含；`Only` 只含；unarchive 恢复；活跃会话归档先释放 |
