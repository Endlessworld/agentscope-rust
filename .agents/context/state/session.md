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
- **扩展 harness crate 功能** ✅（30 files changed, 2271 insertions, 156 deletions）
  - 新增 memory 工具：MemorySearchTool / MemoryGetTool / MemorySaveTool（P0 核心功能缺口）
  - 新增 transcript 存储：TranscriptStore trait + FilesystemTranscriptStore + TranscriptRef（P1）
  - 新增中间件：AtPathExpansionMiddleware（@path 扩展）、SandboxLifecycleMiddleware、TranscriptMiddleware
  - 新增文件系统结果类型：EditResult / WriteResult / ReadResult / LsResult / GlobResult / GrepResult 等
  - 更新 react_agent 支持流式事件（BlockLifecycle 状态机、ToolResultStart/TextDelta/End）
  - 更新 ACP 事件映射（ToolResultEnd → ToolCallUpdate）
  - 添加示例：acp_agent_cli.rs（ACP 客户端 REPL）
  - 添加测试：harness_agent_generate_test / harness_at_path_test / harness_memory_tool_test / harness_transcript_test
  - 更新文档：Feature_Gap_Analysis.md / MIGRATION_GAP_TRACKER.md / MIGRATION_PLAN.md

## 下一步建议
1. **P0 核心功能缺口**：
   - 实现 `AgentSpawnTool`（84KB，子代理生成核心工具）
   - 实现 Docker Sandbox 后端（`bollard` 依赖）
2. **P1 残余**：
   - `$/cancel_request` 通知 handler → 触发会话 cancel
   - 权限 HITL（RequestPermissionRequest 往返）
   - 实现 `TeamTool` / `TeamsMiddleware`（多代理团队协调）
   - 实现 `PlanModeTools`（plan_enter/plan_write/plan_exit）
   - 实现 `WaitAsyncResultsTool`（异步结果等待）
3. **P2 增强**：
   - UsageUpdate 通知 / MCP server 对接 / WebSocket-HTTP transport
   - 实现 `SessionSearchTool`（历史对话搜索）
   - 实现 `WebTools`（web_fetch/web_search）
   - 实现 `SkillSecurityScanner`（安全扫描）
   - 实现 `RemoteFilesystem` 完整实现

## 测试基线
- **cargo test -p agentscope-harness --lib**: 145 通过（较上次增加10个，新增memory/transcript/at_path/agent_generate测试）
- **cargo test -p agentscope-harness --test harness_agent_test**: 8 通过
- **cargo check --examples -p agentscope-harness**: 通过（harness_agent + acp_agent_cli 均编译通过）

## 已验证功能
- ACP stdio 全链路（HarnessAgent 底层）：initialize / session/new / session/prompt / session/close
- HarnessAgentBuilder 完整装配（workspace、filesystem/shell tools、compaction、plan mode、memory）
- harness_agent 示例：AcpCodingAgent + skills 加载 + load_skill_through_path 工具
