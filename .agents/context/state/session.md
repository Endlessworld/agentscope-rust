# 当前会话状态

## 当前目标
- 重构 ACP CodingAgent 底层：由直接包装 `ReActAgent` 改为包装 `HarnessAgent`（已完成）

## 进行中任务
- 无（本轮完成 ACP CodingAgent 底层切换为 HarnessAgent）

## 已完成任务
- **HarnessAgent 增加 ReAct 驱动能力** ✅
  - `agent/harness_agent.rs`：嵌入 `ReActAgent`（含 model/toolkit/middleware/sys_prompt/max_iters），新增 `stream_events()` 驱动入口 + `agent()` getter
  - `HarnessAgentBuilder` 新增 `sys_prompt()`/`max_iters()`；model 未配置时 agent=None（装配-only 模式），`stream_events` 返回错误流
  - `agentscope-core` `MiddlewareChain` 派生 `Clone`（供 HarnessAgent build 时克隆给内部 ReActAgent）
- **ACP 底层切换为 HarnessAgent** ✅
  - `acp/coding_agent.rs`：`model_factory` 签名 `Fn(PathBuf)->ReActAgent` → `Fn(PathBuf)->HarnessAgent`
  - `acp/session.rs`：`AcpAgentSession.agent` 由 `Arc<ReActAgent>` → `Arc<HarnessAgent>`
  - `examples/acp_agent_demo.rs`：`build_agent` 改为构造 `HarnessAgent`（add 工具注册到 harness toolkit，内部 ReActAgent 共享 toolkit）
- **stdio 端到端验证** ✅（initialize / session/new / session/prompt / session/close 全通过）

## 下一步建议
1. **P1 残余**：
   - `$/cancel_request` 通知 handler（on_receive_notification + CancelRequestNotification）→ 触发会话 cancel
   - 权限 HITL（RequestPermissionRequest 往返）— 需在 ReActAgent 侧支持 Ask 决策回调
2. **P2 增强**：
   - UsageUpdate 通知（当前 PromptResponse 未带 usage，需 unstable_end_turn_token_usage feature）
   - MCP server 对接（unstable_mcp_over_acp）
   - WebSocket/HTTP transport
3. **既有问题**：`examples/harness_agent.rs` 为损坏示例（AgentSkillBuilder 等），非本次改动引入，未处理

## 测试基线
- **cargo test -p agentscope-harness --lib**: 135 通过
- **cargo test -p agentscope-harness --test harness_agent_test**: 6 通过
- **cargo build -p agentscope-harness / -p agentscope-core / --example acp_agent_demo**: 通过
- **注意**：`cargo build -p agentscope-harness --examples` 会因既有损坏示例 harness_agent.rs 失败（非本次改动引入）

## 已验证功能
- ACP stdio 全链路（HarnessAgent 底层）：initialize / session/new / session/prompt（流式通知）/ session/close
- HarnessAgent 嵌入 ReActAgent 的装配与驱动
