# ACP 协议 CodingAgent（Rust 版）— 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 ACP CodingAgent 实现工作的**单一事实来源**。任何会话开工前必须先读本文件；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。以本文件 + `git status`/`cargo build` 实际验证为准。
> **对照基准**：`docs/ACP_AGENT_PLAN.md`（方案）+ 本文件 + `crates/*`（实际代码）。
> **参考**：Kotlin `AgiHarnessAgent.kt`、Java `HarnessCodingAgent.java`、ACP Rust SDK（`E:\local-github\agentclientprotocol\rust-sdk`）。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 更新为最新验证结果。

## 1. 全局状态总览

| 模块 | 说明 | 实际状态 | 关键结论 |
|---|---|---|---|
| acp.coding_agent | AcpCodingAgent：Agent builder + 处理器装配 + 会话注册表 | 已完成 | `role::acp::Agent` builder，10 个 request handler |
| acp.session | AcpAgentSession：单会话状态 + prompt 驱动 ReActAgent | 已完成 | `run_prompt` 流式 + 取消（AtomicBool） |
| acp.config | 会话配置（model/mode）、SessionConfigOption 构造 | 已完成 | select 构造 + 3 单测 |
| acp.event | AgentEvent → SessionUpdate 转换 | 已完成 | 文本/思考 delta + 工具调用，4 单测 |
| 依赖 | `agent-client-protocol` path 依赖已声明 | 已完成 | harness Cargo.toml 已声明 v2.0.0 |

## 2. 详细缺口清单

### 2.1 依赖与骨架（P0）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| `agent-client-protocol` path 依赖 | ✅ 已完成 | harness Cargo.toml 已声明，编译通过 |
| `acp/` 模块目录 + mod.rs | ✅ 已完成 | `crates/agentscope-harness/src/acp/`（mod/coding_agent/session/config/event） |
| `AcpCodingAgent::builder()` + InitializeRequest 处理器 | ✅ 已完成 | 声明 loadSession/list/delete/resume/close + promptCapabilities(embeddedContext) + mcp |
| `run_stdio()` 便捷方法 | ✅ 已完成 | `Stdio::new()` + `connect_to`；stdio 握手验证通过 |

### 2.2 会话生命周期（P0）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| NewSessionRequest → AcpAgentSession + NewSessionResponse | ✅ 已完成 | 含 modes（SessionModeState）+ config_options（model/mode select）；stdio 验证通过 |
| CloseSessionRequest | ✅ 已完成 | 取消活动请求 + 移除会话；stdio 验证 `{}` |
| ListSessions / LoadSession / ResumeSession / DeleteSession | ✅ 已完成 | 均已注册 handler；ListSessions 返回 SessionInfo 列表 |
| 会话注册表 | ✅ 已完成 | `Arc<Mutex<HashMap<String, AcpAgentSession>>>` |

### 2.3 Prompt 核心（P0）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| PromptRequest → Vec<Msg> → ReActAgent.stream_events | ✅ 已完成 | ContentBlock→Msg::user；stream_events + tokio::select 取消 |
| 事件转换（Text/Thinking/ToolCall） | ✅ 已完成 | map_delta_event（TextBlockDelta→AgentMessageChunk、ThinkingBlockDelta→AgentThoughtChunk）、map_tool_event（ToolCallStart/End→ToolCall/ToolCallUpdate） |
| PromptResponse(StopReason::EndTurn) | ✅ 已完成 | stdio 验证 `stopReason:"end_turn"` + AgentMessageChunk 流式通知 |
| 错误处理（Refusal） | ✅ 已完成 | 会话不存在→Refusal；流错误→Refusal；取消→Cancelled |

### 2.4 配置与取消（P1）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| SetSessionConfigOptionRequest（model/mode） | ✅ 已完成 | 更新 SessionConfig.model/mode + 返回新 config_options |
| SetSessionModeRequest | ✅ 已完成 | 更新 mode |
| `$/cancel_request` → 取消 + StopReason::Cancelled | ⚠️ 部分 | 会话内 cancel()（AtomicBool）已实现；`$/cancel_request` 通知 handler 未注册（需 on_receive_notification + CancelRequestNotification） |

### 2.5 权限 HITL（P2 增强）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| RequestPermissionRequest 往返 | 待办 | 工具调用前 Ask 决策 |

### 2.6 示例与测试（P1）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| examples/acp_agent_demo.rs（脚本化 Model） | ✅ 已完成 | 脚本化 Model + add 工具；stdio 运行验证通过 |
| 单元测试（事件转换/配置） | ✅ 已完成 | event.rs 4 单测 + config.rs 3 单测，共 7 个通过 |
| 协议集成（stdio 握手+prompt） | ✅ 已完成 | groovy 驱动二进制：initialize→new_session→prompt(AgentMessageChunk)→end_turn→close 全链路验证 |

## 3. 项目状态与下一步
### 3.1 当前状态
- 方案文档 `docs/ACP_AGENT_PLAN.md` 已创建。
- **ACP CodingAgent 核心已实现并通过 stdio 端到端验证**（initialize/new_session/prompt/close 全链路）。
- 测试：harness lib 135 通过（含 acp 7 个新单测，原 128）。
- `cargo build -p agentscope-harness` 通过；`cargo build -p agentscope-core` 通过。
- **注意**：`../crates/agentscope-harness/examples/acp_harness_agent.rs` 为既有损坏示例（AgentSkillBuilder 等错误），与本次改动无关，未处理。

### 3.2 下一步建议
1. **Phase 1**：建 `acp/` 模块 + Initialize 处理器 + connect_stdio，编译通过
2. **Phase 2**：会话生命周期（new/close/list/load/resume/delete）
3. **Phase 3**：Prompt 核心（ReAct 驱动 + 事件转换）
4. **Phase 4**：配置与取消
5. **Phase 6**：示例与测试

## 4. 执行状态约定（所有会话必须遵守）
- 开工前：读本文件 + `git status` + 确认 `cargo build` 基线。
- 改动中：保持每阶段 `cargo build`/`cargo test` 通过。
- 完工后：把对应条目 `执行状态` 改为 `已完成`，更新 `交接信息`（验证结果）。
- 涉及 SDK 参考时：以 `E:\local-github\agentclientprotocol\rust-sdk` 源码与 cargo registry 中 `agent-client-protocol-schema-1.6.0` 为准。
