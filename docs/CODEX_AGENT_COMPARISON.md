# Codex 智能体实现 vs AgentScope-Rust 功能对比

> 对比对象：`E:\local-github\codex`（Codex CLI / codex-rs） ↔ `E:\local-github\agentscope-rust`
> 方法：源码遍历 + rg 精确检索（只读分析；未编译、未使用子智能体）
> 结论快照：2026-09-17

## 0. 摘要（TL;DR）

1. **同为 Rust+Tokio 的编码智能体，定位不同**：Codex 是「单一产品形态的端到端智能体」（账号体系、模型托管、插件市场、遥测、TUI、app-server、云端任务）；AgentScope-Rust 是「通用智能体框架 + Java 参考实现的 Rust 迁移 + ACP 协议宿主」。
2. **规模差一个数量级**：Codex `codex-rs` ≈ 110 个 crate，仅 `core` 主源码 ≈ 2.5 万行、`app-server` ≈ 15.5 万行；AgentScope-Rust 只有 2 个 crate ≈ 4.35 万行。
3. **运行时内核同构**：都是「采样 → 工具调用 → 结果回灌 → 再采样」。Codex 抽象为 `SessionTask`（Regular/Compact/Review/UserShell 四类）+ `Turn`/`Step`；AgentScope 是 `ReActAgent` + `max_iters` + 中间件链。
4. **工具栈取向相反**：Codex「少而深」——`exec_command`/`write_stdin` 常驻进程、`apply_patch` 补丁 DSL、`tool_search` 延迟加载、`code_mode` 以代码调用工具；AgentScope「多而浅」——read/write/edit/list/glob/grep + shell + web + memory + plan + team + subagent。
5. **权限与沙箱是最大代差**：Codex 有 execpolicy 规则引擎 + guardian LLM 复核 + 4 平台 OS 级沙箱（seatbelt/landlock/bwrap/Windows）+ 网络代理；AgentScope 仅应用层 `PermissionEngine`（5 模式 + 规则）+ Local/Docker 沙箱 + 快照/租约，**无 OS 级隔离、无策略 DSL、无二次复核**。
6. **上下文治理思路不同**：Codex 用「类型化 ContextFragment + world_state 快照 + 本地/远程压缩 + rollout 预算 + token 预算工具」；AgentScope 用「中间件式 compaction + tool_result 淘汰 + 会话记忆固化」。
7. **多智能体**：Codex 有完整父子拓扑（`AgentGraphStore`）、深度限制、V1/V2 双代工具面；AgentScope 有后台任务仓库 + 消息总线/网关/通道路由，**通信模型更完备，但缺父子拓扑与深度/配额治理**。
8. **协议**：Codex 自定义 `app-server` JSON-RPC（200+ 方法）+ MCP + TS SDK；AgentScope 实现 **ACP**（IDE 标准）+ MCP，无独立 GUI/SDK。

## 1. 范围、方法与口径

**对比轴**：智能体运行时循环、会话/线程模型、工具系统、权限审批、沙箱执行、上下文与压缩、记忆、技能、钩子/插件/中间件、子智能体与多智能体、协议与前端、持久化、模型层。

**方法**：
- 结构统计：按目录统计非测试 `.rs` 文件数与行数（`Get-Content | Measure-Object -Line`，排除 `*_tests.rs`）。
- API 轮廓：`rg` 抓取 `pub fn / pub struct / pub enum / pub trait / pub const`。
- 关键流程：阅读 `session/turn.rs`、`tasks/regular.rs`、`agent/control/spawn.rs`、`react_agent.rs`、`harness_agent.rs`、`acp/*` 等。

**局限（务必知晓）**：
- Codex 处于活跃演进中：V1/V2 多智能体并存、大量 feature flag（`features` crate）与 `legacy_*`，本报告是其**快照式结论**，不代表全部条件分支。
- 行数统计含内联测试模块，仅用于量级比较，不用于精确度量。
- 未编译、未运行任何一侧代码；结论来自源码与协议/文档定义。

## 2. 总体画像

| 维度 | Codex（`codex-rs`） | AgentScope-Rust |
| --- | --- | --- |
| 定位 | 产品级编码智能体（CLI/TUI/IDE/云端） | 通用框架 + Java 迁移 + ACP 宿主 |
| 构建 | Bazel + Cargo + pnpm + TS CLI | Cargo workspace |
| crate 数 | ≈110 | 2（core / harness） |
| 主源码量级 | core ≈2.5 万行；app-server ≈15.5 万行 | core 1.13 万行 + harness 3.21 万行 = 4.35 万行 |
| 核心抽象 | Session / Turn / Step / SessionTask | Agent(ReAct) / MiddlewareChain |
| 扩展机制 | hooks + plugins + MCP + skills + execpolicy + features | middleware + skills + MCP + subagent + gateway |
| 前端接入 | app-server(JSON-RPC) / TUI / TS SDK | ACP(stdio) / 库 API |
| 沙箱 | OS 级 4 平台 | 应用级 Local/Docker + 快照 |
| 持久化 | SQLite(state) + JSONL(rollout) + thread-store | 内存/JSON store + 工作区 SQLite 索引 |
| 测试基线 | 快照测试 + 集成测试（`core/src/session/tests.rs` 单文件 46 万字符） | core 127 + harness 318 = 445 |

## 3. 运行时内核

### 3.1 主循环

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 入口 | `tasks/regular.rs::RegularTask::run` → `session/turn.rs::run_turn`（2929 行） | `agent/react_agent.rs::call` / `stream_events` |
| 循环终止 | 模型不再请求 function call / 有 pending input / terminal error | `max_iters`（默认 `DEFAULT_MAX_ITERS = 10`） |
| 越限语义 | 无 max_iters 概念（以 turn 完成度判定） | `AgentEvent::ExceedMaxIters` |
| 子层 | Turn → Step（`step_context` / `step_settings` / `step_activation`） | 无显式 step 层 |
| 任务类型 | `SessionTask` trait：Regular / Compact / Review / UserShell | 单一 ReAct 循环 |
| 采样重试 | `responses_retry.rs` + `compact_model_fallback.rs` | OpenAI provider 内部重试/限流 |
| 中断 | `CancellationToken` 子树 + `GRACEFULL_INTERRUPTION_TIMEOUT_MS=100` | `interrupt()` / `interrupt_session()` + `RequestStop` 事件 + 优雅关闭 |
| 工具并发 | `tools/orchestrator.rs` + `tools/parallel.rs`（29KB） | `ToolExecutor`（超时/重试/并行） |
| 恢复 | `enable_pending_tool_recovery`（待确认工具恢复） | 同名能力存在于 ReActAgent builder |

**结论**：内核同构。Codex 多出「任务类型化（用户 shell 命令、review、compact 也是 task）」与「Step 层」；AgentScope 多出「迭代上限 + 越限事件」这种更适合作为库/嵌入式场景的硬边界。

### 3.2 会话与线程

- **Codex**：`Session`（`session/session.rs` 92KB）+ `TurnContext`（52KB，本轮配置快照）+ `ThreadManager` + `AgentControl`（活体 agent 注册表）+ `codex_thread.rs`。ThreadId / SessionId 分离；线程是**一等持久实体**，支持 fork / resume / archive / revert / search（`thread-store` crate）。
- **AgentScope-Rust**：`ReActAgent` 持有 `AgentState`（按 session 隔离的对话上下文）+ `RuntimeContext`；`HarnessAgent` 聚合 workspace manager / plan mode / sandbox / memory / skill / subagent。会话状态可落 `InMemory` / `JsonFile` store（与 Java 状态文件互通）。

**差距**：无线程 fork/revert/search/分节（sections）语义；无“活体 agent 注册表”这一层（部分被 gateway/subagent manager 覆盖）。

### 3.3 事件流

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 载体 | `protocol` crate 的 `EventMsg` / `TurnItem` | `event::AgentEvent`（31 变体） |
| 粒度 | 业务化：`item/started`、`item/completed`、`plan/delta`、`TerminalInteraction`… | 块级三段式：`TextBlock{Start,Delta,End}`、`ThinkingBlock*`、`DataBlock*`、`ToolResult*` |
| 人机交互 | 审批/用户输入走 `request_user_input` / `request_permissions` 工具体 + serverRequest | `RequireUserConfirm` / `UserConfirmResult` / `RequireExternalExecution` / `ExternalExecutionResult` 事件 |
| 子智能体 | `SubAgentActivity` / inter-agent 消息事件 | `SubagentExposed` 事件 |

## 4. 工具系统

### 4.1 Codex（`core/src/tools/`）

- **执行**：`exec_command` + `write_stdin`。底层 `unified_exec/`（`process_manager.rs` 75KB、`process.rs`、`async_watcher`、`head_tail_buffer`、`shell_snapshot`、`stdin_approval`）——常驻进程、PTY、异步输出监视、stdin 触发审批。
- **文件编辑**：`apply_patch`（自定义补丁 DSL，`runtimes/apply_patch.rs`）。
- **读/媒体**：`view_image`、`read_mcp_resource`、`list_mcp_resources`、`list_mcp_resource_templates`。
- **交互/计划**：`update_plan`、`request_user_input`、`request_permissions`、`send_message_to_user_async`。
- **发现**：`tool_search`（按需暴露工具/延迟加载）、`web_search`、`list_available_plugins_to_install`、`request_plugin_install`。
- **多智能体**：V2 `spawn_agent/wait_agent/send_message/list_agents/interrupt_agent/followup_task`；V1 `close_agent/resume_agent/send_input`。
- **其他**：`sleep`、`current_time`、`get_context_remaining`、`new_context_window`、`code_mode.execute`、扩展工具（`extension_tools`）。
- **注册/路由**：`registry.rs`（31KB）、`router.rs`、`spec_plan.rs`（59KB）、`orchestrator.rs`、`parallel.rs`、`executed_tool_calls/`；`ToolName { namespace, name }`，MCP 工具以 `mcp__<server>__<tool>` 命名空间化。

### 4.2 AgentScope-Rust

- **core**：`Tool` / `Toolkit` / `ToolGroup` / `ToolExecutor`（超时、重试、并行、JSON Schema 校验、**挂起语义 suspend**）+ MCP 桥 + `TodoWriteTool` + `coding` / `dangerous` / `file` / `meta` / `schema_only`。
- **harness**：`read_file` / `write_file` / `edit_file` / `list_files` / `glob_files` / `grep_files`、`execute`（shell）、`plan_enter` / `plan_write` / `plan_exit`、`web_search` / `web_fetch`、`memory_get` / `memory_save` / `memory_search`、`session_search`、`agent_spawn` / `agent_generate` / `task` / `wait_async` / `team`、`LoadSkillTool`。

### 4.3 差异

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 工具数量 | 约 25 个（含 V1/V2 双份） | 约 25 个，但粒度更细（fs 拆为 6 个） |
| 命令执行 | 常驻进程 + stdin 交互 + PTY 尺寸 | 一次性 `execute`（命令 + 超时） |
| 编辑 | 补丁 DSL（可表达多文件/上下文行） | 字符串替换（old/new + replace_all + 批量 operations） |
| 工具发现 | `tool_search` 动态加载，避免 schema 膨胀 | 无；靠 `ToolGroup` 静态分组 |
| 命名空间 | 有（`ToolName.namespace`，MCP 自动前缀） | 无（平铺工具名） |
| 上下文自省 | 模型可查/新建上下文窗口（`get_context_remaining`/`new_context_window`） | 无 |

## 5. 权限与审批

### 5.1 Codex（四层防线）

1. **策略枚举**（`protocol` crate）：
   - `AskForApproval { UnlessTrusted, OnRequest(默认), Granular{...}, Never }`
   - `SandboxPolicy { DangerFullAccess, ReadOnly{network_access}, ExternalSandbox{...}, WorkspaceWrite{...} }`
   - `permission_profile_intersection` / `permission_profile_snapshot`（配置快照可持久校验）
2. **命令策略 DSL**（`execpolicy` crate，2.6K 行）：`parser` / `rule` / `decision` / `amend` / `executable_name`，支持 allow / prompt / **forbidden**；自带 CLI `execpolicycheck` 与 `sandbox_migration`。
3. **审批执行层**：`tools/approvals.rs`（33KB）+ `tools/network_approval.rs`（48KB）+ `tools/sandboxing.rs`；审批结果可沉淀为规则（`approved_command_prefix_saved`、`network_rule_saved`、`compact_permissions`）；`with_additional_permissions` / `require_escalated` 是模型显式升权入口。
4. **LLM 复核（guardian）**：高风险动作交给一个独立复核会话（`guardian/review_session.rs` 38KB、`prompt.rs`、`approval_request.rs`、`request_budget`/`input_budget`、`coverage`、`feedback`）——**用模型审模型**，且带预算与证据收集。

### 5.2 AgentScope-Rust（单层策略）

- `PermissionMode { Default, AcceptEdits, Explore, Bypass, DontAsk }`
- `PermissionBehavior { Allow, Deny, Ask, Passthrough }` + `PermissionRule`（工具级 allow/deny/ask）
- `PermissionEngine.check_permission` + `PermissionContextState`（附加工作目录、建议规则、decision reason、updated input）
- 交互层：`RequireUserConfirm` / `UserConfirmResult` 事件 + `ConfirmHandler`；ACP 侧 `acp/permission_store.rs` 持久化规则

### 5.3 差距判定

| 能力 | Codex | AgentScope-Rust | 差距 |
| --- | --- | --- | --- |
| 审批策略模型 | 4 种模式 + 细粒度 Granular 子开关 | 5 种模式（无子开关） | 中 |
| 命令级策略引擎 | execpolicy DSL + CLI | 无 | **大** |
| 审批结果沉淀为规则 | 前缀规则 / 网络规则自动保存 | 有规则结构，但无自动提炼 | 中 |
| LLM 二次复核 | guardian（带预算/证据） | 无 | **大** |
| 网络审批 | `network_access` 策略 + `network-proxy` crate | 无 | **大** |
| 权限配置快照/交集 | 有 | 无 | 小 |

## 6. 沙箱与执行

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 隔离层级 | **OS 级**：seatbelt(macOS SBPL) / landlock / bwrap / Windows | 应用级：本地直执行 or Docker 容器 |
| 代码位置 | crate `sandboxing`（manager/spawn/policy_transforms/denial/violation/terminal_queries）+ `linux-sandbox` / `mxc-sandbox` / `windows-sandbox-rs` / `windows-sandbox-service` | `harness/agent/sandbox/`（manager/lease/guard/key/context/layout/workspace_spec/state/snapshot/docker） |
| 违规上报 | `denial.rs` / `violation.rs`（结构化拒绝事件） | 无结构化违规上报 |
| 网络隔离 | 策略位 + `network-proxy` 代理层 | 依赖容器网络配置 |
| 进程硬化 | `process-hardening`、`shell-escalation`（含 zsh fork unix_escalation 35KB） | 无 |
| 实例生命周期 | 沙箱跟环境绑定（环境选择 `environment_selection`） | Lease + Guard + IsolationKey + 可插拔 StateStore |
| 快照/恢复 | rollout 重建 + `shell_snapshot`（shell 环境快照） | `SandboxSnapshot`（Local/Remote/Noop + Spec）+ Docker 启停重建 |

**结论**：两侧都做了「沙箱抽象 + 生命周期管理」，但 Codex 的落地深度在 **系统调用/内核机制层面**，AgentScope 在 **容器 + 快照层面**。AgentScope 的 `SandboxSnapshot` trait 化（Local/Remote/Noop + Spec 工厂）设计上并不弱，缺的是 OS 级后端与违规上报。

## 7. 上下文管理与压缩

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 注入载体 | `context/` **55+ 个类型化片段**（实现 `ContextualUserFragment` / developer fragment） | 拼装 sys prompt + 中间件在 `on_agent`/`on_reasoning` 注入 |
| 环境快照 | `context/world_state/`：`environment`(20KB)、`agents_md`、`permissions`、`tools`、`collaboration_mode`、`multi_agent_mode`、`context_window_guidance` | `middleware/workspace_context.rs`、`at_path.rs`、`environment_memory` |
| 历史管理 | `context_manager/history.rs`（52KB）+ `normalize.rs`（17KB）+ `updates.rs` | `AgentState` + `memory/session.rs` |
| 压缩 | `compact.rs` + `compact_remote_v2*`（服务端压缩）+ `compact_token_budget` + `auto_compact_window` + `compact_model_fallback`；压缩作为 **CompactTask** | `middleware/conversation_compaction.rs` + `memory/compaction.rs` |
| 预算 | `rollout_budget` + `token_budget_context` + `post_sampling_token_estimate` | `max_context_tokens` + `model/context_windows.rs` |
| 结果堆大小 | 未单独拆（含于历史/输出限制） | `tool_result_eviction`（显式的大工具结果淘汰） |
| 模型自省 | `get_context_remaining` / `new_context_window` 工具 | 无（仅内部阈值） |
| 提醒类注入 | `time_reminder`、`current_time`、`image_resize_notice`、`unsupported_media`、`turn_aborted` 等 | `TaskReminderMiddleware`、`tool_call_sanitizer` |

**结论**：Codex 的上下文是「**声明式片段集合 + 每轮重算 world_state**」，强在可审计、可测试（有大量快照测试）；AgentScope 是「**命令式中间件包装**」，强在扩展点少而清晰，但在「有哪些东西进了 prompt」上不够可枚举。

## 8. 记忆系统

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 存储 | `state` crate：SQLite `memories` 表 + `memory_versions` + `memory_readiness` | 工具直写 + store（内存/JSON）+ workspace SQLite 索引 |
| 写入策略 | `memories/write`：**两阶段**（phase1 提炼 → phase2 整合）、`guard.rs` 保护、`rollout_input` 作为输入源 | `memory_save` 工具 + `memory/consolidator.rs` 固化 |
| 读取 | `memories/read`：`citations.rs`（**可溯源引用**）+ `usage.rs`（使用统计） | `memory_get` / `memory_search` / `session_search` |
| 维护 | `extensions/ad_hoc.rs`、`extensions/prune.rs` + `memory_readiness` | `memory/eviction.rs`、`memory/flush.rs` + `memory_maintenance` 中间件 |
| 对外接口 | app-server `memory/status`、`memory/reset` | 库 API + 工具 |
| 来源约束 | `rollout_input.rs`（仅从会话日志提炼） | 无明确来源约束 |

## 9. 技能系统

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| crate/目录 | `skills`（interface/loading/parser/selection/mentions/name_counts/model，10 文件/1.3K 行） | `harness/agent/skill/`（13 文件：repository/loader/runtime/load_tool/prompt/curator/promoter/security/filters/stager/manage/shell_path/model） |
| 发现与选择 | `selection.rs` + `mentions.rs`（`@skill` 提及）+ `name_counts` | `SkillRepository` trait + `SkillCatalogSnapshot` + `LoadSkillTool` |
| 依赖 | `mcp_skill_dependencies`（技能声明的 MCP 依赖） | 无 |
| 安全 | 无内置扫描 | `security.rs`：6 类 18 规则扫描 + 安装策略；`filters.rs`：Canary / Environment 过滤 + NotifyAndWait 门控 |
| 使用度量 | `skill_snapshots`（core-plugins） | `SkillUsageStore`（含 environments） |
| 演进 | 无 | `curator`（审计+策展）+ `promoter`（晋升/评审）+ `SkillAuditLog` |
| 热更新 | `skills_watcher`（app-server）+ `skills/changed` 通知 | 无 watcher |

**结论**：这是 AgentScope-Rust **明显领先**的一项——技能治理（安全扫描、金丝雀、环境过滤、审计、晋升）比 Codex 完备；Codex 胜在发现体验（提及、依赖、热更新通知）。

## 10. 钩子 / 中间件 / 插件

### 10.1 Codex：hooks + plugins（进程外可扩展）

- **hooks crate**（28 文件 / 10.8K 行），**9 个事件**：`session_start`、`session_end`、`user_prompt_submit`、`pre_tool_use`、`post_tool_use`、`stop`、`compact`、`interrupt`、`permission_request`。
- 执行引擎：`engine/discovery.rs`（发现）、`dispatcher.rs`（分发）、`command_runner.rs`（外部命令）、`mcp_runner.rs`（MCP 工具型钩子）、`output_parser.rs` + `schema_loader.rs`（带 schema 的输出）+ `output_spill.rs`（大输出落盘）。
- **plugin crate**：`manifest.rs`、`provider.rs`、`bundled_hooks.rs`（插件可携带钩子）。
- **core-plugins crate**（56 文件 / 23K 行）：插件市场（`marketplace_add/remove/upgrade`）、`npm_source`、`remote_bundle`、`plugin_bundle_archive`、`installed_marketplaces`、`git_policy`、`toggles`、`discoverable`、`recommended_plugin_install`、`skill_snapshots`、`startup_sync`。
- **模型自助装插件**：`request_plugin_install` + `list_available_plugins_to_install` 工具 + app-server `plugin/*`、`marketplace/*` 方法（人机均可）。

### 10.2 AgentScope-Rust：middleware（进程内洋葱链）

- **core**：`Middleware` trait 5 钩子：`on_agent` / `on_reasoning` / `on_acting` / `on_model_call` / `on_system_prompt`（可改系统提示）。
- **harness 内置 16 个中间件**：`conversation_compaction`、`tool_result_eviction`、`memory_flush`、`memory_maintenance`、`sandbox_lifecycle`、`state_checkpoint`、`skill_middleware`、`subagents`、`dynamic_subagents`、`teams`、`workspace_context`、`at_path`、`inbox`、`agent_trace`、`async_tool`、`tool_call_sanitizer`。会话历史不再由中间件写入（原 `transcript` 中间件已于 2026-10-21 删除，改由 core `SessionRecorder` 写 journal）。

### 10.3 差异判定

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 扩展粒度 | 进程外命令/MCP（多语言、可独立部署） | 进程内 Rust trait 实现（需重编译） |
| 生命周期事件 | 9 个（含 session/compact/permission） | 5 个（agent/reasoning/acting/model/sysprompt） |
| 分发/沙箱 | 有 schema 校验、输出落盘、钩子自身有权限 | 无 |
| 插件生态 | 完整（市场、安装、版本、远程 bundle） | 无插件包体系 |
| 内置业务逻辑 | 分散在 session/tools | 统一在 middleware/（更内聚） |

## 11. 子智能体与多智能体

### 11.1 Codex

- **工具面（两代并存）**：V2 = `spawn_agent` / `wait_agent` / `send_message` / `list_agents` / `interrupt_agent` / `followup_task`；V1 = `close_agent` / `resume_agent` / `send_input`（`tools/handlers/multi_agents*/`）。
- **控制平面**：`core/src/agent/`（`AgentControl`、`registry.rs`、`role.rs`、`child_config.rs`、`status.rs`、`control/{spawn(58KB), delivery, execution, interrupt, legacy, residency, service_tier, user_authorization}`）。
- **拓扑存储**：crate `agent-graph-store`——“storage-neutral parent/child topology”（`AgentGraphStore` trait / `LocalAgentGraphStore` / `ThreadSpawnEdgeStatus`）。
- **角色**：crate `agent-roles`——从文件解析 agent 角色（`AgentRoleConfig` / `load_agent_roles`），类似“子智能体定义文件”；`agent-identity` 另外管理 agent 身份。
- **治理**：`exceeds_thread_spawn_depth_limit` / `next_thread_spawn_depth`（深度限制）、`UserAuthorization`（用户授权）、`residency`（驻留策略）、`service_tier`。
- **消息注入**：inter-agent 消息/子智能体完成通知作为**类型化 context 片段**进入下一轮（`inter_agent_message`、`subagent_notification`、`sub_agent_activity` 事件）。

### 11.2 AgentScope-Rust

- **工具面**：`agent_spawn`（前台/后台、agent 名、上下文、workspace、task_id）、`agent_generate`、`task`、`wait_async`、`team`。
- **子智能体**：`agent/subagent/`（`entry` / `loader` / `manager` + `protocol/{codec,event}` + `task/{background,delivery,protocol_client,record,repository,spec,status,workspace}`）——含**后台任务仓库与远程执行客户端（SSE）**。
- **通信/编排**：`gateway/`（`subagent_bridge`、`subagent`、`session_turn_gate`、`wakeup_dispatcher`、`task_bridge`、`channel/{router,chatui,dm_scope,peer,binding}`）+ `bus/`（`MessageBus` / `WorkspaceMessageBus` / `async_tool`）+ `coordination/`（`PeriodicGate` Local/Store-backed）。

### 11.3 差异判定

| 能力 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 父子拓扑与查询 | ✅ `AgentGraphStore` | ❌（仅 manager 内列表） |
| 深度/数量限制 | ✅ `exceeds_thread_spawn_depth_limit` | ❌ |
| 角色定义文件 | ✅ `agent-roles` | 部分（`SubagentDeclaration` / loader） |
| 中断/恢复单个子体 | ✅ `interrupt_agent` / `resume_agent` | 部分（`wait_async` + manager） |
| 后台任务持久化 | ✅ thread-store + rollout | ✅ `TaskRepository`（本地/远程） |
| 子体结果回流 | 类型化 context 注入 | 后台任务回调 + gateway |
| 消息总线/通道 | ❌（仅 agent 间消息） | ✅ `bus` + `channel`（ChatUI/DM/Router） |
| 多团队协作 | ❌ | ✅ `team` + 会话 journal（`SessionLog`） |

**结论**：两者互补。AgentScope 的「消息总线 + 通道路由 + 团队」是 Codex 没有的；Codex 的「拓扑 + 深度治理 + 授权/驻留/服务级」是 AgentScope 缺的。建议优先补 **子智能体拓扑与深度限制**（成本低、防御价值高）。

## 12. 协议与前端接入

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 主协议 | 自定义 `app-server` JSON-RPC（请求+通知+serverRequest） | **ACP**（agent-client-protocol，stdio） |
| 规模 | `app-server` 264 文件/15.5 万行；`app-server-protocol` 3.26 万行、**200+ 方法** | `acp/` 6 文件/2.3 千行 |
| 方法域 | `thread/*`（start/fork/resume/archive/revert/search/sections）、`fs/*`、`plugin/*`、`marketplace/*`、`process/*`（spawn/kill/resizePty）、`account/*`、`config/*`、`skill*/`、`hook*/`、`memory/*`、`collaborationMode/*` | `initialize`、`session/new|load|prompt|cancel|set_mode|set_model`、`fs/*`（由客户端反向提供）、`terminal/*`、权限请求 |
| 客户端 | TUI（ratatui）、IDE 插件、TS SDK（`export.rs` 生成） | 任意 ACP 客户端（Zed 等） |
| 传输 | stdio / in-process / socket | stdio（`run_stdio`）/ 自定义 transport |
| MCP | 客户端（`codex-mcp`/`rmcp-client`）+ hook MCP executor + app MCP routing | 客户端（`tool/mcp.rs` + `mcp_client_manager`），ACP 会话可注入 MCP server 配置 |
| 额外面 | realtime WebRTC 语音、云端任务、账号/计费 | 无 |

**结论**：协议上两者不可直接互操作：Codex 是「服务端中心化 + 客户端薄」，AgentScope 是「遵循行业标准 ACP + 把 fs/terminal 反向托管给客户端」。AgentScope 若要做 IDE 生态，ACP 选择是对的；若要做「多客户端 + 服务化」，需补一层 RPC 服务端。

## 13. 持久化与状态

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 主存储 | `state` crate（SQLite `log_db` + `migrations` + `runtime/*`）+ `thread-store`（本地文件+元数据） | core `state`（`AgentState` + `InMemory`/`JsonFile` store） |
| 事件日志 | `rollout`（JSONL 记录器 + 压缩 + 逆序扫描 + 写锁 + 维护 + 参考索引） | 无追加日志（仅状态快照） |
| 线程/会话操作 | create/list/read/archive/delete/fork/revert/search/sections/move | 无 |
| 上下文历史 | `history` crate（`retained_context` / `reconciled_retained_context` / `compaction_checkpoint`） | 上下文在内存 + 压缩后写回 |
| 恢复 | `session/daemon_recovery`、`state_db_bridge`、`rollout_migration`（含回滚计划） | `subagent/task/repository` 可恢复；无进程级崩溃恢复 |
| 工作区索引 | `file-search`（fuzzy） + `file-watcher` | `workspace/index.rs`（SQLite 路径索引，可降级为扫描） |

**差距**：Codex 是**追加日志（event-sourcing）+ 可回放**，AgentScope 是**状态快照**。前者可支持 fork/revert/回放式调试，后者实现简单但不可回溯。

## 14. 模型层

| 维度 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 抽象 | `model-provider` crate（`ModelProvider` + auth + endpoint） | `Model` trait + `ModelRegistry` |
| 供应商 | OpenAI Responses + **Amazon Bedrock**（catalog/runtime/auth_refresh/mantle）+ ollama + lmstudio | OpenAI 兼容（async-openai），复盖 DeepSeek/Kimi/GLM/Ollama 网关 |
| 模型元数据 | `models-manager`（预设、缓存、`model/list`、collaboration mode presets） | `model/context_windows.rs` + `GenerateOptions` |
| 推理控制 | reasoning effort（`session/reasoning_effort.rs`）、service tier | `thought_level`（ACP 暴露）/ `GenerateOptions` |
| 服务端能力位 | `provider.capabilities()`（如 remote_compaction 支持级别） | 无 |
| 凭证 | `keyring-store` / `secrets` / `chatgpt` / `aws-auth` | `credential` 模块 + 环境变量 |

## 15. 关键差异汇总与可借鉴清单

### 15.1 Top 10 差距（按「防御价值 ± 实现成本」排序）

| # | 差距 | Codex 做法 | 建议优先级 | 备注 |
| --- | --- | --- | --- | --- |
| 1 | 命令级策略引擎 | `execpolicy` DSL（allow/prompt/forbidden）+ CLI | **P0** | 不依赖平台，纯逻辑，可直接对齐 |
| 2 | OS 级沙箱后端 | seatbelt/landlock/bwrap/Windows | P2 | 平台相关工作量大，建议先抽 trait |
| 3 | 子智能体拓扑与深度限制 | `AgentGraphStore` + `exceeds_thread_spawn_depth_limit` | **P0** | 成本低，防递归爆炸 |
| 4 | 工具发现（延迟加载） | `tool_search` 按需暴露 | P1 | 工具数增长后 schema 成本会失控 |
| 5 | 常驻 shell 进程 | `unified_exec`（write_stdin/PTY/异步 watcher） | P1 | 对交互式命令体验提升大 |
| 6 | 上下文可枚举化 | 55+ 类型化 fragment + world_state | P1 | 可测试性与可观测性 |
| 7 | 追加日志 + 线程 fork/revert/search | `rollout` + `thread-store` | P2 | 与 Java 兼容目标可能冲突，需权衡 |
| 8 | 钩子事件面（9 个）与进程外钩子 | `hooks` crate + 输出 schema/落盘 | P2 | 进程内中间件已能满足大部分场景 |
| 9 | 模型能力位自省 | `provider.capabilities()`（remote_compaction 等） | P2 | 多供应商适配时会用到 |
| 10 | 审批 LLM 复核（guardian） | 带预算/证据的复核会话 | P3 | 依赖模型成本与产品形态 |

### 15.2 AgentScope-Rust 领先/独有的能力（Codex 无）

- **技能治理**：安全扫描（6 类 18 规则）、Canary/环境过滤、审计日志、策展与晋升。
- **多通道网关**：`channel/{chatui,dm_scope,peer,router}` + `bus` + `PeriodicGate` 唤醒调度。
- **团队协作与转录**：`team` + 会话 journal（`SessionLog`）。
- **文件系统抽象谱系**：Local / Overlay / Composite / Sandbox / **Remote** / Baked（Codex 是单一本地 FS + 进程）。
- **Java 生态互通**：状态文件格式与事件/API 语义对齐（迁移项目的核心目标）。
- **可嵌入/可裁剪**：`lite` feature（去 rusqlite）+ `embedded` profile（极端体积优化）——Codex 无此轴。

### 15.3 不建议直接照搬的

- `app-server` 200+ 方法：与本项目的 ACP 定位重叠且相互竞争，照搬会造成双协议维护。
- 插件市场体系（marketplace / npm source / remote bundle）：强绑商业生态。
- realtime WebRTC 语音、云端任务、账号/计费：超出「框架 + 迁移对齐」范围。
- V1/V2 双代多智能体并存：属于过渡期技术债，不应作为设计目标。

## 16. 附录

### 16.1 统计口径

- 文件/行数：`Get-ChildItem -Recurse -Filter *.rs`（排除 `*_tests.rs`）+ `Get-Content | Measure-Object -Line`。
- 两侧均**未编译、未运行**；结论来自源码结构与公开 API 轮廓。
- 行数仅作量级参考（含内联测试、空行、注释）。

### 16.2 关键路径索引

| 主题 | Codex | AgentScope-Rust |
| --- | --- | --- |
| 主循环 | `codex-rs/core/src/session/turn.rs` | `crates/agentscope-core/src/agent/react_agent.rs` |
| 任务抽象 | `codex-rs/core/src/tasks/` | —（单一 ReAct） |
| 工具注册 | `codex-rs/core/src/tools/{registry,router,spec_plan}.rs` | `crates/agentscope-core/src/tool/` |
| 权限 | `codex-rs/execpolicy/` + `core/src/tools/approvals.rs` + `core/src/guardian/` | `crates/agentscope-core/src/permission/mod.rs` |
| 沙箱 | `codex-rs/sandboxing/src/` | `crates/agentscope-harness/src/agent/sandbox/` |
| 上下文 | `codex-rs/core/src/context/` + `context_manager/` | `crates/agentscope-harness/src/agent/middleware/` |
| 多智能体 | `codex-rs/core/src/agent/` + `agent-graph-store/` | `crates/agentscope-harness/src/agent/{subagent,gateway,bus,team}/` |
| 协议 | `codex-rs/app-server-protocol/` | `crates/agentscope-extensions-acp/` |
| 持久化 | `codex-rs/{state,thread-store,rollout,history}/` | `crates/agentscope-core/src/state/` + `harness/src/agent/workspace/index.rs` |

### 16.3 一句话结论

> Codex 是「产品化跑通 + 平台级安全」的工业级单体；AgentScope-Rust 是「抽象干净 + 可嵌入 + 可裁剪」的框架，在技能治理、网关与文件系统抽象上超出 Codex，在命令策略、OS 级沙箱、上下文可枚举性与持久化回溯上存在明确缺口。下一步最高性价比的是：**execpolicy 类规则引擎**、**子智能体拓扑/深度限制**、**工具延迟加载**。

