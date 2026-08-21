# 当前会话状态

## 当前目标
- 完善 agentscope-rust，完整复刻 Java 的 HarnessAgent/CodingAgent 能力（HarnessAgent 已完善，AcpCodingAgent 默认工厂已优化，harness_agent 示例已改为 ACP stdio 服务器）

## 进行中任务
- 无（HarnessAgent 完善 + AcpCodingAgent 默认工厂 + harness_agent 示例改造 三个子任务已完成）

## 已完成任务
- **完善 HarnessAgent/HarnessAgentBuilder** ✅（`agent/harness_agent.rs` 654 行）
  - Builder 新增 25+ 能力：workspace/environment_memory/max_context_tokens/additional_context_file/
    memory/compaction/tool_result_eviction/plan_mode/disable_* 系列/register_* 工具/message_bus/subagent 等
  - HarnessAgent 新增访问器：workspace_manager/plan_mode_manager/memory_config/path_normalizer
- **优化 AcpCodingAgent 默认工厂** ✅（`acp/coding_agent.rs`）
  - 修复 import：crate::memory/crate::middleware → crate::CompactionMiddlewareConfig（根 re-export）
  - 新增 default_model_factory（读 CODING_MODEL_ID/OPENAI_API_KEY/OPENAI_BASE_URL，完整装配 HarnessAgent）
  - AcpCodingAgentBuilder::build() 未提供 factory 时回退默认 factory（向后兼容）
- **harness_agent 示例改为 ACP stdio 服务器** ✅（`examples/harness_agent.rs` 327 行）
  - 从 CLI REPL 改为基于 `AcpCodingAgent` 的 ACP agent（server 角色），通过 stdio 通信
  - `build_agent(cwd)` 用 HarnessAgent 完整装配（OpenAiChatModel 读 CODING_MODEL_ID/OPENAI_API_KEY/OPENAI_BASE_URL、
    workspace、filesystem/shell tools、compaction 16000、max_iters 50、sys_prompt 含 skills 块）
  - 保留并复用原有 skill 加载能力：用户级 $HOME/.agents/skills + 项目级 <workspace>/.agents/skills，
    渲染 <available_skills>，注册 load_skill_through_path 工具（LoadSkillTool，按 source 解析宿主目录）
  - 修复过程中：AgentSkillBuilder::new() → AgentSkillBuilder::default()（无 new 关联函数）；
    bytes move 借用错误（先取 len 再 move）；移除未用 mut
  - 同步更新 `examples/README.md` 为 ACP stdio 用法

## 下一步建议
1. **P1 残余**：
   - `$/cancel_request` 通知 handler → 触发会话 cancel
   - 权限 HITL（RequestPermissionRequest 往返）
2. **P2 增强**：
   - UsageUpdate 通知 / MCP server 对接 / WebSocket-HTTP transport

## 测试基线
- **cargo test -p agentscope-harness --lib**: 135 通过
- **cargo test -p agentscope-harness --test harness_agent_test**: 8 通过
- **cargo check --examples -p agentscope-harness**: 通过（harness_agent + acp_agent_demo 均编译通过）

## 已验证功能
- ACP stdio 全链路（HarnessAgent 底层）：initialize / session/new / session/prompt / session/close
- HarnessAgentBuilder 完整装配（workspace、filesystem/shell tools、compaction、plan mode、memory）
- harness_agent 示例：AcpCodingAgent + skills 加载 + load_skill_through_path 工具
