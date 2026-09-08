# 当前会话状态

## 本轮已完成（ACP 会话跨重启恢复接线）
- **example**：`examples/acp_harness_agent.rs::build_agent` 增加
  `.session_id(session_id)` + `.state_store(Arc::new(JsonFileAgentStateStore::new(
  {cwd}/.agentscope/state)))`，新增 `use agentscope_core::state::JsonFileAgentStateStore;`。
- **新测试**：`tests/acp_session_resume_test.rs`（进程内 Channel::duplex，重启后 session/load 续接）。
- 详见 history/learnings.md「ACP 会话恢复语义」。

## 验证结果
- `acp_session_resume_test`：1 passed（新增）
- `acp_cancel_test`：1 passed；`acp_event_flow_test`：1 passed
- `state_checkpoint_test`：2 passed；harness lib：165 passed
- `cargo build -p agentscope-harness --example acp_harness_agent`：通过
