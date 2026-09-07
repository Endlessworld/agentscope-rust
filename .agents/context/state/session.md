# 会话状态

## 当前目标
修复 ACP agent 执行 git 命令"超时"问题 —— 已全部完成并端到端验证通过。

## 已完成（未提交）
1. **LLM 400 修复**：`agentscope-core/src/model/openai.rs` — base_url 含 opencode.ai 时自动注入 `x-opencode-session` 头（provider "Go" 必需）。
2. **shell 超时根因修复**：`agentscope-harness/src/filesystem/sandbox.rs` — `execute()` 子进程 stdin 设为 `Stdio::null()`。此前 pwsh 继承 agent 的 ACP stdio 管道（client 持有不关闭），等待输入不退出导致 wait() 挂起超时。
3. **yolo 模式验证**：`examples/repro_acp_driver.rs` — session/new 后发 `session/set_mode`（modeId=yolo）跳过权限审核，验证生效。
4. **端到端验证通过**：driver → acp_harness_agent → LLM → execute 工具 → `git status --short` Exit code 0 → stopReason=end_turn。
5. 此前会话：ACP 权限确认机制（ConfirmHandler/PermissionRuleStore/模式映射 plan/accept_edits/yolo）、进程树 taskkill、管道防死锁。

## 经验教训
- agent 进程内 spawn 子进程必须显式 `stdin(null)`，绝不能继承 stdio 管道。
- 复现示例（控制台 stdin）无法暴露该问题；必须走真实 ACP stdio 驱动复现。
- 日志文件 .agentscope/log/acp-agent.log 的 handler 字段有转义爆炸噪音，grep 时需过滤。

## 下一步（可选）
- 用户确认后 git 提交；清理 driver 中遗留的权限自动回复逻辑（yolo 下已不需要）。
