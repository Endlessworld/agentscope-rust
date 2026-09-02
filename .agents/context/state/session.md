# 会话状态

## 当前目标
实现 ACP `$/cancel_request` 取消支持（GAP_TRACKER 2.4 中唯一剩余 P1 缺口）——已完成。

## 已完成（本会话）
- `AcpAgentSession` 新增 `run_prompt_cancellable`（Option<RequestCancellation>）+ 共享 `run_prompt_inner`
- PromptRequest handler 取 `responder.cancellation()` 传给 `run_prompt_cancellable`；
  **用 `cx.spawn` 把 prompt 移出 dispatch loop**（关键：长 handler 阻塞 loop 会导致 SDK 无法处理 `$/cancel_request` 更新 marker）
- `run_prompt_inner` 的 select 中监听 `cancellation.cancelled()` → 返回 `StopReason::Cancelled`
- `AcpCodingAgent` 新增 `run_with_transport`（transport 可注入），`run_stdio` 委托之
- 新 e2e 测试 `tests/acp_cancel_test.rs`：进程内 `Channel::duplex` + never-ending 脚本化 model，client 发 prompt 后 `SentRequest::cancel()`，断言 `StopReason::Cancelled`（0.33s，无需真实 model）

## 验证结果
- `cargo test -p agentscope-harness`：lib 149 + 全部集成测试通过（含 acp_cancel 1、acp_event_flow 1、acp_stdio_thought 真实 model 1）
- `cargo build -p agentscope-harness` + `--example acp_harness_agent` 通过
- 真实模型 stdio e2e（acp_stdio_thought_test）无回归

## 下一步
ACP 剩余 P2 增强：Phase 5 权限 HITL（RequestPermissionRequest 往返，Ask 决策）。
harness 其余 P2：SandboxLifecycleMiddleware 已实现（GAP_TRACKER 标注过时）；docker 后端、snapshot 持久化、RemoteFilesystem 真实后端、OtelTracingMiddleware、RAG 模块等。

## 关键产物
- crates/agentscope-harness/src/acp/session.rs（run_prompt_cancellable/run_prompt_inner）
- crates/agentscope-harness/src/acp/coding_agent.rs（run_with_transport + spawn 重构）
- crates/agentscope-harness/tests/acp_cancel_test.rs（新 e2e）
- docs/ACP_AGENT__GAP_TRACKER.md（2.4 已标 ✅）