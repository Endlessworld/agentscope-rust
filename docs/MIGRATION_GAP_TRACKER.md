# AgentScope Java → Rust 迁移 — 跨会话进度跟踪文件（Gap Tracker）

> 用途：本文件是**所有迁移会话共享的单一事实来源**。任何会话在开始工作前必须先读本文件；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件与 `git status`/`cargo build` 实际验证为准。
> 对照基准：`MIGRATION_PLAN.md`（方案）+ 本文件（已整合原 worker_output 8 份分析报告）+ `crates/*`（实际代码）。
> 覆盖统计截至：ACP 权限确认/取消/动态模型切换全套 + 嵌入式内存优化（lite feature + 对话压缩）+ 文件批量读写 + 沙箱 pwsh 修复后（b5e1635 之后 10 个提交已反映）。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- **对齐度**：由分析报告给出（可能滞后于代码，见各条目实际状态）。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 更新为最新验证结果。

## 1. 全局状态总览（对齐度汇总）

| 模块/包 | Java 规模 | Rust 实际状态 | 对齐度(报告) | 关键结论 |
|---|---|---|---|---|
| core.message | 23 类 | 6 文件完整 | ≈90% | 9 内容块+Source+State 枚举齐全；4 个消息子类折叠进 Msg+MsgRole |
| core.model | 18 类 | 10 文件（原 11） | ≈80% | 数据层 + **真实 provider（async-openai）** + ModelRegistry/ModelContextWindows；可发真实 HTTP/SSE。**2026-09-21（`d86a578`）**：`openai`/`responses`/`anthropic` provider 实现已迁至 `agentscope-extensions-model/src/{openai,responses,anthropic_async}/mod.rs`，core 仅保留 trait / 注册表 / provider SPI |
| core.credential | 6 类 | 3 文件 | ≈90% | DeepSeek/Kimi/XAI 均纯凭据元数据，与 Java 同水位 |
| core.formatter | 6 类 | 4 文件 | ≈95% | ResponseFormat/JsonSchema + MediaUtils + **base_formatter（extract_text/has_media/role_label/convert_tool_result/save_base64/should_bypass 全套）**；openai.rs 已复用 |
| core.agent | 13 类 | 6 文件（ReActAgent + lifecycle/stream_options） | ≈5%→大幅提升 | ReAct 循环 + stream_events + 结构化调用 + 串行门/优雅关闭守卫已实现；缺 AgentBase 全生命周期细节（hooks/per-session 属性袋） |
| core.event | 35 类 | 4 文件 | ≈95% | 31/31 事件变体齐全 + AgentEventEmitter/ConfirmResult 已补 |
| core.middleware | 7 类 | 4 文件 | ≈95% | 5 钩子洋葱链完整 + TaskReminderMiddleware 已补 |
| core.hook | 21 类 | **0 文件** | 0%（设计决定） | Java 已 @Deprecated，走 2.0 middleware 架构，不迁移 |
| core.tool | 38 类 | 20 文件 | ≈80% | 治理层 + 结果转换/消息构造/挂起语义/危险路径/注册 builder + meta（ToolCallParam/ExtendedModel/reset_equipped_tools/**ToolkitAware rebindToolkit**）+ 注册元数据（extendedModel/mcpClient/subAgent）+ TodoWriteTool 已补；余 **@Tool proc-macro**（最大开发体验缺口）；ContextStore 属性袋为设计决定（走 RuntimeContext.metadata） |
| core.state | 13 类 | 6 文件 | ≈85% | 核心 + ListHashUtil/SessionInfo 已补；缺 LegacyStateLoader/State 标记接口/readFileCache LRU |
| core.memory | 8 类 | 1 文件 | ≈85% | StateBackedMemory 已完整实现 Memory trait（add/get/delete/clear/save_to/load_from）；缺 AgentStateMemoryView（活跃 API）；LongTermMemory 系列已 deprecated |
| core.permission | 8 类 | 2 文件 | ≈95% | 流程高度对齐；ConfirmHandler 用户交互确认 + PermissionRuleStore 持久化已补；余 tool 自身规则建议生成 |
| core.interruption | 3 类 | 1 文件 | ≈100% | 完全对齐 |
| core.shutdown | 10 类 | 1 文件 | ≈45% | 状态机在；缺 per-request 跟踪/中间件/钩子 |
| core.tracing | 4 类 | 1 文件 | ≈15% | 仅日志初始化；缺 OtelTracingMiddleware |
| core.rag | 4 类 | **0 文件** | 0% | 完全缺失 |
| core.skill | 10 类 | **0 文件(core)** | 0% | core 无 skill；harness 有同名模块 |
| core.util | 8 类 | 无 util 模块 | ≈30% | 功能散落 serde_json；缺 ExceptionUtils/TypeUtils |
| core.exception | 1 类 | error.rs | ≈100% | CompositeAgentException+AgentExceptionInfo 已补 |
| core.workspace | 占位 | 无 | 0%（双方均未落地） | 占位对齐 |
| harness.agent | — | harness_agent.rs | 已实装 | 完整 builder（disable_* 开关/agent_tracing_log/message_bus/generate_options_defaults）+ model()/filesystem()/toolkit()/middleware() 访问器；ACP coding agent 全套已落地；**#2600/#2499 per-session interrupt/clear_context 已加（interrupt/interrupt_session/clear_context/clear_context_for）**；**D2 已补 13 项 Java builder setter（见 §3.14 D2）** |
| harness.filesystem | — | 6 文件 | 高 | Local/Overlay/Composite/Sandbox/Remote 完整 |
| harness.tool | — | 4 文件 | 高 | FilesystemTool/ShellExecute/FuzzyTextMatcher |
| harness.gateway | 29 类 | 8 文件 + channel/(7) | ≈98% | HarnessGateway/GatewayBootstrap/SessionTurnGate/WakeupDispatcher/MsgContext/SubagentRegistry 完整；channel/* 7 层路由 + chatui 已建（本轮） |
| harness.memory | 10 类 | 6 文件 | ≈95% | compaction+flush+consolidation+eviction+session+MemoryConfig 完整 |
| harness.bus | 6 类 | 4 文件 | ≈90% | MessageBus（Mode A/C/D）+ WorkspaceMessageBus + AsyncToolRegistry 系完整；Mode D 降级轮询 |
| harness.sandbox | — | sandbox/ 模块 | 高 | Sandbox/SandboxClient/SandboxManager(4 级获取策略)/Guard/Lease/StateStore 完整；Windows pwsh -EncodedCommand（UTF-16LE→Base64）+ 子进程 stdin 置空防挂起 + 进程树管道死锁修复；**docker 后端已接（docker CLI 实现）**；**snapshot/layout/helpers 已接（见 §3.6）** |
| harness.acp | — | `crates/agentscope-extensions-acp/`（2026-10-22 从 harness 抽出） | 新增模块 | AcpCodingAgent 包装 core ReActAgent（镜像 Kotlin AgiAgent/AgiAgentSession）；AcpAgentSession 权限确认/取消/流式/动态模型切换/per-session headers 全套落地 |
| harness.middleware | 18+ | 11 文件 | 高 | 3 系统提示转换 + Inbox + AsyncTool + MemoryFlush + MemoryMaintenance + ToolResultEviction + SubagentsMiddleware + DynamicSubagentsMiddleware + **AgentTraceMiddleware（本轮）** + 配套 TaskTool/SubagentEntry/DefaultAgentManager/AgentSpecLoader；**WorkspaceContext/SandboxLifecycle 已接** |
| harness.subagent | — | subagent/task/ 目录 | task 仓库已建 | BackgroundTask/TaskRepository/WorkspaceTaskRepository 完整；Local/Adopted 动态 spawn 已实现；**Remote（agent-protocol）任务 + 维护循环（heartbeat/sweep）已接线（见 §3.14 D6）** |
| harness.mcp | — | mcp.rs + mcp_client_manager.rs | 高 | McpClientManager（rust-mcp-sdk v1.0.1 stdio）+ McpServerRegistrar 已实现；**SSE + Streamable HTTP 已接（WS 未做）** |
| harness.skill | — | 7 文件 | 高 | curator/runtime/stager/prompt 完整，LLM 模式待接 |
| harness.workspace | — | 7 文件 | 高 | constants/path_normalizer/plan/manager/index/namespace 已实现；**完整版 WorkspaceManager（两层次读写 + 命名空间 + SQLite 索引 + session/task/memory/skill 操作）本轮完成** |


## 2. agentscope-core 详细缺口清单（按模块）

### 2.1 tool（优先级 P0）— 治理层本轮已补，剩余中高层缺口
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| ToolExecutor 已补（timeout/retry/parallel/外部短路/激活校验/schema 校验/预设覆盖） | ToolExecutor | ✅ 已完成 | group/config/validator/executor.rs 已建，7 个治理测试通过 |
| ToolGroupManager 已补 | ToolGroupManager/ToolGroup/ToolGroupScope | ✅ 已完成 | group.rs；含 META/EXTERNAL、替换语义、孤儿回收 |
| ToolValidator 已补 | ToolValidator | ✅ 已完成 | validator.rs，jsonschema DRAFT-2020-12 |
| ToolkitConfig 已补 | ToolkitConfig | ✅ 已完成 | config.rs；parallel/execution_config/allow_tool_deletion |
| **ToolRegistration 流式 builder** | Toolkit$ToolRegistration | ✅ 已完成 | registration.rs + `Toolkit::register()`；tool/group/presetParameters/apply/**extendedModel（ExtendedSchemaTool 合并 schema）/mcp_client/sub_agent** 已实现；enableTools 语义已并入 group 管理（见下条） |
| **MetaToolFactory + reset_equipped_tools** | MetaToolFactory | ✅ 已完成 | meta.rs：`Toolkit::register_meta_tools()`/`reset_equipped_tools()`；ToolGroupManager 新增 `get_meta_group_names`/`validate_group_exists`/`get_notes`/`replace_meta_active_groups`；EXTERNAL 组不受影响（6 个集成测试过） |
| **ToolDangerousPathConstants + isDangerousPath** | ToolDangerousPathConstants/ToolBase | ✅ 已完成 | dangerous.rs：DEFAULT_DANGEROUS_FILES/DIRS/COMMANDS 常量 + `is_dangerous_path`（目录段/文件尾/tilde） + `is_dangerous_command`；harness shell 自检接入仍待办（见 3.2） |
| **ToolSuspendException + suspended 语义** | ToolSuspendException | ✅ 已完成 | suspend.rs + `Error::ToolSuspended`；executor 将 suspend 转 `ToolResultBlock::suspended`（id/name 回填）；ReAct 循环检测到 suspended 即返回 `GenerateReason::ToolSuspended`（单元测试过） |
| **ToolRegistry + RegisteredToolFunction 元数据** | ToolRegistry/RegisteredToolFunction | ✅ 已完成（本轮补齐） | registry 逻辑折入 toolkit；preset/extendedModel/mcpClientName/subAgent 元数据已补（`Toolkit::set_mcp_client_name/mcp_client_name/set_sub_agent_name/sub_agent_name`，copy() 一并复制）；enableTools 语义并入 group 管理 |
| **ToolResultConverter / DefaultToolResultConverter** | ToolResultConverter | ✅ 已完成 | result.rs；trait `convert(Option<&Value>)`；null→"null"、String 直通、其余紧凑 JSON（Void→"Done" 因 Rust 无 void 返回略过） |
| **ToolResultMessageBuilder** | ToolResultMessageBuilder | ✅ 已完成 | result.rs；`build_tool_result_msg` 用原 call id/name 回填 + agent name（集成测试过） |
| **ToolEmitter / DefaultToolEmitter** | ToolEmitter | ✅ 已完成 | emitter.rs：`ToolEmitter`/`DefaultToolEmitter`/`NoOpToolEmitter`/`emit_to`；`ToolExecutionContext.emitter` 已接入 ReActAgent 执行上下文 |
| **ToolCallParam** | ToolCallParam | ✅ 已完成 | meta.rs：builder+访问器+`ToolCallParam::from`；agent 适配为 agent_id 字符串、runtimeContext 为扁平 JSON |
| **ToolExecutionContextProvider / ContextStore** | ContextStore/DefaultContextStore | 设计决定（不迁移） | Java 已废弃→RuntimeContext；Rust 属性袋语义由 `RuntimeContext.metadata: JsonMap` + `state: Option<Arc<Mutex<AgentState>>>` 承担，不单建类型化属性袋（如需可后续加 typed map） |
| **ToolParam/@Tool 注解 + proc-macro** | Tool/@ToolParam | 待办 | **与已引入的 `rust-mcp-sdk` macros 区分**：SDK 的 `macros` feature 转引 `rust-mcp-macros`（#[mcp_tool]/#[mcp_resource]/#[mcp_elicit]），生成的是 `rust_mcp_schema::Tool`（MCP server 协议工具声明），**不是 agent 工具**。本项缺口是 agentscope 自身的 `@Tool` 宏——生成 `FunctionTool`（agent 本地工具，供 ReActAgent 调用）。当前工具注册全靠手写 `FunctionTool`，**这是 core 最大开发体验缺口**；可参考 rust-mcp-macros 的实现模式（属性解析 + type_to_json_schema + quote 生成）新建独立的 agentscope proc-macro crate。实现后 MCP server 侧可复用 SDK 宏 + 桥接 |
| **ToolkitAware / ExtendedModel** | ToolkitAware/ExtendedModel | ✅ 已完成 | ExtendedModel + SimpleExtendedModel 已实现（meta.rs，含冲突检测/required 合并）；**ToolkitAware rebindToolkit 深拷贝重绑已补：`Tool::rebind_toolkit` 钩子 + `FunctionTool::toolkit_aware(factory)`，`Toolkit::copy()` 对 aware 工具重绑到新副本；顺带修复 `ToolGroupManager::clone` 共享状态的既有 bug（新增 `copy()` 深拷贝，Inner derive Clone）；3 个新集成测试过** |
| **McpClientManager** | McpClientManager | ✅ 已完成 | 已在 harness 实现（mcp_client_manager.rs + McpServerRegistrar，见 2.14）；core 侧 `McpTool` e2e 接入 ReActAgent 执行链路仍待办（见 7.5：MCP 连通验收） |
| **并行安全分区（isConcurrencySafe 独占槽）** | ToolExecutor.execute_all | ✅ 已完成 | executor.rs `execute_all` 已实现：并行模式下将连续 concurrency-safe 工具并发执行（`run_safe_batch` + join_all，保持声明顺序），不安全/未知工具各自形成独立串行槽；`Toolkit::is_concurrency_safe_tool` 查询；4 个新测试覆盖（unsafe 串行 peak=1、safe 并发 peak>=2、混合分区保序、顺序模式） |
| **TodoWriteTool** | TodoTools | ✅ 已完成 | tool/todo.rs：`TodoWriteTool`（基于任务清单全量替换）+ `render`/`parse_state`/`normalize_priority` 辅助；`ReActAgentBuilder::build` 默认注册；调用后同步 `AgentState.tasks_context` 与 `AgentCore.tasks`（`Arc<Mutex<TaskContextState>>`）；单测过 |

### 2.2 model（优先级 P0）— 最大运行缺口：无法真正发 HTTP
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Model trait / ChatModelBase 能力位 | Model/ChatModelBase | ✅ 已完成 | model.rs；纯抽象，无实现者 |
| GenerateOptions(25字段)/ChatResponse/ChatUsage/ExecutionConfig/ToolChoice/ToolSchema/CachePolicy | 同名 | ✅ 已完成 | 字段级对齐 |
| **ModelRegistry** | ModelRegistry(428行) | ✅ 已完成 | provider.rs；register/canResolve/resolve_with（`provider:model` 拆分，无前缀时按序询问） |
| **ModelCreationContext** | ModelCreationContext | ✅ 已完成 | provider.rs；apiKey/baseUrl/endpointPath/stream/enableThinking/cachePolicy/cacheId/options |
| **ModelContextWindows** | ModelContextWindows | ✅ 已完成 | context_windows.rs；最长前缀匹配表（gpt/deepseek/glm/qwen/kimi/claude/...） |
| **ModelProviderSupport** | ModelProviderSupport | ✅ 已完成 | provider.rs；firstNonBlank/intOption/findAssignableComponent 桩 |
| **ModelUtils** | ModelUtils | ✅ 已完成 | provider.rs；ensure_default_execution_config |
| **StructuredOutputReminder** | StructuredOutputReminder | ✅ 已完成 | provider.rs；TOOL_CHOICE/PROMPT 枚举 |
| **ModelProvider SPI** | spi.ModelProvider | ✅ 已完成 | provider.rs；providerId/supports/create；静态注册替代 ServiceLoader |
| **真实 provider 接入（用 async-openai）** | agentscope-extensions-model-* | ✅ 已完成 | **`async-openai` v0.41.3（远程 crates.io，MIT）**。`OpenAiChatModel::stream` 内调 `chat().create_stream()`，SSE delta→`ChatResponse`/`ContentBlock`（含流式 tool_call 分片按 index 聚合）；OpenAI 兼容 provider 用 `OpenAIConfig` 改 base_url+key。见方案 §4.2。**live 真实 HTTP 验证待 key（见 §7.3）**。**2026-09-21（`d86a578`）实现已迁至 `crates/agentscope-extensions-model/src/openai/mod.rs`**（core 仅留 trait/注册表/SPI） |

### 2.3 model.transport（优先级 P1）— 手写方案作废，改用 async-openai
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| HttpTransport/HttpRequest/HttpResponse | HttpTransport | 设计决定（不迁移） | 手写 transport 作废；由 `async-openai` 内部（reqwest + eventsource-stream）提供 |
| HttpTransportFactory | HttpTransportFactory | 设计决定（不迁移） | 同上，`async-openai::Client` 内置 |
| JdkHttpTransport/OkHttpTransport | 同名 | 设计决定（不迁移） | 同上 |
| WebSocketTransport 子包 | WebSocketTransport/JdkWebSocketTransport | 待办 | 若需 realtime/WS，`async-openai` 已带 `realtime` feature（tokio-tungstenite） |
| **async-openai 接入层** | agentscope-extensions-model-* | ✅ 已完成 | `crates/agentscope-extensions-model/src/openai/mod.rs`（2026-09-21 自 `agentscope-core/src/model/openai.rs` 迁入）；`impl Model for OpenAiChatModel`：内部 `chat().create_stream()`，delta→ContentBlock 映射；feature `chat-completion`+`byot`+`middleware`；`OpenAiProvider` 注册进 `default_registry()` |


### 2.4 agent（优先级 P0）— ReAct 循环已实现，缺生命周期治理细节
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Agent/CallableAgent/ObservableAgent trait | 同名 | ✅ 已完成 | agent_traits.rs |
| ReActAgent reasoning→acting 循环 | ReActAgent(4727行) | ✅ 已完成 | react_agent.rs；含 maxIters/permission/middleware/事件 |
| RuntimeContext | RuntimeContext | ✅ 已完成 | 有 agent_name/user_input/memory/toolkit/metadata/cwd/**response_format**/**session_id/user_id/agent_id**（with_*/访问器）；**已含 `state: Option<Arc<Mutex<AgentState>>>`（agentState 绑定）+ `delta_tx` 增量事件通道（逐 token 渲染）**；asToolExecutionContext/resolveAgentState 便捷方法为设计决定不单建（直接以 RuntimeContext 本体充当执行上下文） |
| **AgentBase 生命周期**（serialization gate、per-call 串行 callGates、graceful shutdown guard、hooks 分发） | AgentBase(1040行) | 部分 | `CallGate`（按 key FIFO 串行，lifecycle.rs）+ `ExecutionGuard`（acquire/release 优雅关闭，register/unregister request）已实现并接入 ReActAgent.call；**缺 hooks 分发**（走 middleware，AgentBase 抽象类本身不迁移） |
| **CallableAgent 结构化 overloads** | call(List,Class/JsonNode) | ✅ 已完成 | `call_with_schema` + 便捷 `call_single`/`call_text`；native response_format(json_schema) 路径 + **fallback（generate_response 工具）** 均已实现：`call_with_schema` 按 `supports_native_structured_output` 分派到 `call_native_structured`/`call_fallback_structured`（后者注入 `generate_response` 工具循环，含 `<structured-output-instruction>`），单测过 |
| **StreamableAgent / streamEvents / StreamOptions** | StreamableAgent/StreamOptions | ✅ 已完成 | `ReActAgent::stream_events(msgs, StreamOptions)` 返回过滤后的 `EventStream`；`StreamOptions`（EventCategory 过滤 + incremental + includeReasoningChunk/Result/ActingChunk/SummaryChunk/Result）+ `EventCategory::of` 映射 31 事件；v1 `StreamableAgent`/粗粒度 `Event` 为设计决定不迁移 |
| **ObservableAgent observe（接收不回复）** | ObservableAgent | 待办 | 多智能体协作；当前 `subscribe(sink)` 是单向事件分发，缺 Java observe(observer) 的 observer 注册语义；依赖 SubagentEventBus 中继 |
| **Event/EventSource/EventType（v1 粗粒度，已废弃）** | Event/EventSource/EventType | 设计决定 | Java @Deprecated；不迁移，用新版 AgentEvent |
| **ReasoningContext 完善**（ThinkingAccumulator/ToolCallsAccumulator/usage） | ReasoningContext | 部分 | 已有 text/thinking/tool_calls/usage 累积；缺 per-chunk 即时事件 |
| **SubagentEventBus** | SubagentEventBus | 待办 | Reactor Context 中继子 agent 事件 |

### 2.5 event（优先级 P0）— 高度对齐
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AgentEvent enum + AgentEventType + EventData | 同名 | ✅ 已完成 | 31/31 变体 + wire name + legacy alias |
| **AgentEventEmitter** | AgentEventEmitter | ✅ 已完成 | event/emitter.rs：发射器（emit_agent_event/emit），经 AgentEventEmitter 接口下发事件 |
| **ConfirmResult** | ConfirmResult | ✅ 已完成 | event/emitter.rs：HITL 确认结果 record（同意/拒绝/补充/重试枚举 + 加载器）；为 PermissionBehavior::Ask 预留 |

### 2.6 middleware（优先级 P0）— 高对齐
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Middleware trait（5 钩子）+ MiddlewareChain + 4 输入类型 | MiddlewareBase/MiddlewareChain/AgentInput/ReasoningInput/ModelCallInput/ActingInput | ✅ 已完成 | middleware.rs/input.rs |
| **TaskReminderMiddleware** | TaskReminderMiddleware | ✅ 已完成 | middleware/task_reminder.rs：`on_reasoning` 注入 `<system-reminder>`、`on_system_prompt` 注入 GROUNDING；经 `Arc<Mutex<TaskContextState>>` 共享任务状态（builder 内创建，与 TodoWriteTool 共同持有） |

### 2.7 hook（优先级 P2）— 设计决定
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Hook 体系全套 | Hook/HookEvent/.../recorder/JsonlTraceExporter | 设计决定 | Java 全部 @Deprecated(forRemoval,2.0)，被 middleware 替代；Rust 走 2.0 新架构，不迁移 |

### 2.8 formatter（优先级 P1）— 薄弱
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| ResponseFormat/JsonSchema | 同名 | ✅ 已完成 | 数据类型；JsonSchema 缺 description 字段 |
| **Formatter / AbstractBaseFormatter** | Formatter/AbstractBaseFormatter | ✅ 已完成 | `formatter/base_formatter.rs`：`extract_text_content`（过滤 ThinkingBlock + 展开 Hint + 扁平化 ToolResult）、`extract_text_content_from_block`、`has_media_content`、`format_role_label`、`convert_tool_result_to_string`（media 块→URL/临时文件引用，单/多条目格式对齐）、`save_base64_data_to_temp_file`（MIME→扩展名+临时文件）、`should_bypass_history`（bypassMultiagentHistoryMerge 元数据）；8 单测通过；**openai.rs 的 to_request_messages 已重构复用该层**（System/User 用 extract_text_content，Tool 用 convert_tool_result_to_string） |
| **MediaUtils** | MediaUtils(452行) | ✅ 已完成 | formatter/media_utils.rs：is_local_file/is_file_exists/get_extension/determine_media_type/resolve_mime_type/validate_image|audio|video_extension/determine_audio_format/infer_audio_format_from_media_type/file_to_base64/to_file_protocol_url/url_to_protocol_url + async download_url_to_base64/url_to_base64_data_url（reqwest 限时）；9 单测；urlToRgbaImage/urlToInputStream 因需 image 解码略过 |
| **FormatterException** | FormatterException | ✅ 已完成 | 映射通用 `Error`（`Error::Other`/`Error::Io`/`Error::HttpStatus`），不单独建异常类型 |

### 2.9 state（优先级 P1）— 核心全齐，辅助缺
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AgentState + 子上下文 + 3 种 Store | 同名 | ✅ 已完成 | agent_state/context/store.rs |
| **LegacyStateLoader** | LegacyStateLoader | 待办 | v1→v2 状态迁移 |
| **ListHashUtil** | ListHashUtil | ✅ 已完成 | state/list_hash.rs：compute_hash（采样 0/1/4/1/2/3/4/末尾，DefaultHasher 十六进制）、has_changed、needs_full_rewrite（缩小/缺 hash/前缀变化判定）；6 单测 |
| **SessionInfo** | SessionInfo | ✅ 已完成 | state/session_info.rs：session_id/size/last_modified/component_count + Display + camelCase serde；2 单测 |
| **State 标记接口** | State | 待办 | 类型化 get(Class<T>) |
| **ToolContextState.readFileCache LRU + mtime 校验** | ToolContextState | 待办 | tool/file.rs 未接入缓存 |

### 2.10 memory（优先级 P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Memory/InMemoryMemory/StateBackedMemory | 同名 | ✅ 已完成 | memory/mod.rs |
| **AgentStateMemoryView** | AgentStateMemoryView | 待办 | 活跃 API；StateBackedMemory 已完整实现 Memory trait 但仍直接读写 context，缺视图语义（按 user/session 视角切片） |
| LongTermMemory 系列 / StaticLongTermMemoryHook | LongTermMemory/LongTermMemoryMode/LongTermMemoryTools | 设计决定 | Java 全部 @Deprecated(forRemoval,2.0)，不迁移 |

### 2.11 permission（优先级 P1）— 高对齐
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 枚举/规则/引擎 + 流程 | PermissionMode/Behavior/Rule/Decision/Context/Engine | ✅ 已完成 | deny→ask→tool→allow→bypass→default 一致 |
| **tool 自身规则集成**（checkPermissions/generateSuggestions/危险路径） | ToolBase | 待办 | 当前 tool_check 仅做 EXPLORE/ACCEPT_EDITS 只读判断 |
| **ask 决策带 suggested_rules** | ToolBase.generateSuggestions | 待办 | 当前 ask 决策不带 |
| **ConfirmHandler 用户交互确认** | — | ✅ 已完成 | ReActAgent::set_confirm_handler（含 PermissionRule/ConfirmHandler 类型）；ACP 模式映射 plan/accept_edits/yolo；PermissionRuleStore 持久化规则到磁盘 |

### 2.12 shutdown（优先级 P1）— 状态机在，周边缺
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 状态机+单例+active 计数 | GracefulShutdownManager | ✅ 已完成 | shutdown/mod.rs；含 perform/await/ensure |
| **per-request 粒度跟踪 / ActiveRequestContext** | ActiveRequestContext | 待办 | 当前仅原子计数 |
| **GracefulShutdownMiddleware** | GracefulShutdownMiddleware | 待办 | Rust middleware 层无 shutdown 集成 |
| **ShutdownStateSaver / bindStateSaver** | ShutdownStateSaver | 待办 | |
| **AgentScopeJvmShutdownHook（信号处理）** | AgentScopeJvmShutdownHook | 待办 | Rust 无 JVM；可 tokio ctrl_c + CancellationToken |
| **超时强制中断监控循环 / resetForTesting** | GracefulShutdownManager | 待办 | |

### 2.13 tracing（优先级 P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| init_tracing / init_tracing_with_filter | — | ✅ 已完成 | tracing.rs |
| **OtelTracingMiddleware** | OtelTracingMiddleware | 待办 | 需 opentelemetry + tracing-opentelemetry |
| Tracer/TracerRegistry/NoopTracer | 同名 | 设计决定 | Java @Deprecated，不迁移 |


### 2.14 MCP（core.tool.mcp + harness mcp，优先级 P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| McpTool（handler 注入 + not_connected fallback） | — | ✅ 已完成 | tool/mcp.rs |
| harness McpServerConfig/Registrar 声明 | — | ✅ 已完成 | agent/mcp.rs（注册为 stub） |
| **rust-mcp-sdk path 依赖接入** | McpClientManager | ✅ 已完成 | 已接入 `rust-mcp-sdk` v1.0.1 path 依赖；`McpClientManager` 实现客户端生命周期管理（create/connect/disconnect/list_tools/call_tool）；`McpClientInstance` 管理连接状态和工具列表 |
| **McpServerRegistrar 真实实现** | harness Registrar | ✅ 已完成 | `McpServerRegistrar` 已实现：使用 `McpClientManager` 管理连接；`register()` 方法异步创建客户端并连接；新增 `disconnect_all()`、`registered_servers()` 辅助方法；**#2877 注册结果上报已加（McpServerRegistrationListener + Success/Failed/Skipped 结果，with_listener 注入）** |
| **McpTool e2e 接入 ReActAgent** | McpTool 被 agent 调用 | ✅ 已完成 | `McpClientManager::connect()` 现会在握手后调用 `request_tool_list()` 填充 `client.tools`（此前空列表）；新增 `find_client_id_by_server`/`call_tool_by_server` 辅助；`bridge_mcp_tools_to_toolkit()` 把已连接 server 的工具注册为 core `McpTool`（**core `McpTool` 新增 `async_handler`（`with_async_handler`）**，handler 直接 await `call_tool_by_server`，避免在 tokio 运行时内 `block_on` panic）；ACP 会话经 `AcpAgentSession::apply_mcp_servers` 桥接。**真实 e2e 通过**：`harness_mcp_bridge_test` 拉起 Node stdio MCP server → 发现 → 桥接 → 调用，输出回显验证（2 测试） |
| **MCP SSE/HTTP 传输** | McpClientManager 传输层 | ✅ 已完成 | stdio 已通；新增 SSE（`ClientSseTransport`，URL 含 `/sse`）与 Streamable HTTP（`ClientStreamableTransport`，其余 URL）双传输，支持自定义 headers；依赖改为直接引入 `rust-mcp-transport` 的 `sse`/`streamable-http` feature（避开 rust-mcp-sdk 1.0.1 的 `sse`/`streamable-http` 会编译损坏的 server 侧 `mcp_http` 模块）；`McpServerConfig` 增 `headers`；ACP `apply_mcp_servers` 递归匹配 `McpServer::Http`/`Sse`，capabilities 声明 http/sse=true；2 单测（URL 路由 + config builder） |

### 2.15 其余 core 缺失模块（优先级 P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **rag 全套** | GenericRAGHook/Knowledge/KnowledgeRetrievalTools/RAGMode | 待办 | core 无 rag 模块 |
| **util 工具类** | ExceptionUtils/TypeUtils/JsonUtils | 待办 | ExceptionUtils 可映射 error.rs；TypeUtils 类型反射辅助 |
| **CompositeAgentException** | CompositeAgentException+AgentExceptionInfo | ✅ 已完成 | error.rs：`AgentExceptionInfo`（agent_id/agent_name/throwable_message）+ `CompositeAgentException`（message+causes，Display 复刻 Java getMessage 拼装）+ `Error::CompositeAgent` 变体；2 单测 |
| core **workspace** 契约 | WorkspaceBase/LocalWorkspace | 设计决定 | Java 仅 package-info 占位；Rust harness 已有 workspace 模块 |

## 3. agentscope-harness 详细缺口清单（按模块）

### 3.1 filesystem（优先级 P0）— 高对齐
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Filesystem trait + 6 实现 | LocalFilesystem/OverlayFilesystem/CompositeFilesystem/SandboxBackedFilesystem/RemoteFilesystem | ✅ 已完成 | filesystem/*.rs |
| **RemoteFilesystem 真实后端** | RemoteFilesystem | ✅ 已完成（本轮） | `remote/mod.rs` + `remote/store.rs`：完整 BaseStore 抽象（get/put/putIfVersion(CAS)/search/delete）+ InMemoryStore（版本递增、命名空间 \0 前缀）+ NamespaceFactory/StaticNamespace/DefaultNamespace + StoreItem；RemoteFilesystem 实现 ls/read(offset-limit)/write(CAS 创建)/edit(5 次 CAS 重试)/grep/glob/exists/delete/move，convertItemToFileData 支持 content 字符串或字符串数组；13 单测过 |
| **FilesystemUtils** | FilesystemUtils | ✅ 已完成（本轮） | `filesystem/util.rs`：get_file_type（二进制扩展名表）/count_occurrences/perform_string_replacement（不存在/歧义报错）/shell_quote；7 单测过 |
| **BakedContextFilesystem** | BakedContextFilesystem | ✅ 已完成（本轮） | `filesystem/baked.rs`：包装 `Arc<dyn Filesystem>` 并在每次委托时注入固定 RuntimeContext（workspaceFor user/session 越权视图）；2 单测过 |

### 3.2 tool（优先级 P0）— 高对齐
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| FilesystemTool/ShellExecuteTool/FuzzyTextMatcher | 同名 | ✅ 已完成 | tool/*.rs；**read/write/edit 均支持批量**（file_paths/operations 数组）；ShellExecuteTool 执行 trace（命令/超时/耗时）已加；**#2832 grep/glob limit 上界（默认 100/200/硬上限 1000）+ 截断提示**；**#2413 list_files 空结果改 "Empty directory" + local/ls 区分 not-exists/not-a-directory 报错** |
| **DANGEROUS_COMMANDS shell 自检** | ToolDangerousPathConstants | ✅ 已完成 | ShellExecuteTool::call 已接入 core `is_dangerous_command`；rm -rf/sudo rm/dd/mkfs/chmod 777 等直接返回 error 块（测试通过） |
| **SessionSearchTool**（session_search/session_list/session_history） | SessionSearchTool | ✅ 已完成（本轮） | tool/session_search.rs：基于 WorkspaceManager.list_session_log_files + SessionTree JSONL；search 支持可选 agentId 过滤 + maxResults、list 优先 sessions.json store 索引、history 取最后 N 条消息（500 字符截断）；7 集成测试过 |
| **WebTools**（web_fetch/web_search） | WebTools | ✅ 已完成（本轮） | tool/web_tools.rs：reqwest 实现，fetch 仅 http/https + max_chars 截断；有 `DEEPSEEK_API_KEY` 时 fetch/search 均走 DeepSeek（`/v1/tools/fetch` 返回 Markdown、`/v1/tools/search`），无 key 时 fetch 回落免 key 裸 GET（HTML 正文经内置 `html_to_text` 降为纯文本，无三方解析库）；register_web_tools 一键注册；测试过（html_to_text/body_of/truncate 纯函数单测 6 + 无 key 分支 + 参数校验；live HTTP 未验） |

### 3.3 gateway（优先级 P1）— 差距最大（5%）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Channel/Gateway mpsc 事件通道 | Channel | ✅ 已完成 | 43 行；语义是事件 mpsc 而非 Java 外发通道 |
| **Gateway 接口（bindMainAgent/run/runSubagent/...）** | Gateway | ✅ 已完成 | gateway/harness_gateway.rs：`GatewayAgent` trait + `HarnessGateway`（多 agent 注册表 + canonicalKey→sessionId + SessionTurnGate 互斥 + exposeSubagent/revoke + runWakeup + isSessionRunning + deliver_to_session） |
| **HarnessGateway 完整** | HarnessGateway(589行) | ✅ 已完成 | harness_gateway.rs（430 行）；含 ChannelManager/OutboundAddress/GatewayTurnContext/SubagentMaterializer；resolve_agent fallback 链路；集成测试 `gateway_emits_to_channel` 通过 |
| **GatewayBootstrap / ChannelManager** | GatewayBootstrap/ChannelManager | ✅ 已完成 | gateway_bootstrap.rs：builder（agent/main_agent/message_bus/channel_manager/build）+ ChannelManager（register/get/deliver） |
| **SessionTurnGate / WakeupDispatcher** | SessionTurnGate/WakeupDispatcher | ✅ 已完成 | session_turn_gate.rs（公平互斥，4 单测）+ wakeup_dispatcher.rs（唤醒分发，1 单测）；**本轮补 `TurnBusyException`（gate_key + Display/Error）与非阻塞 `try_acquire`** |
| **MsgContext / SessionIdUtils** | MsgContext/SessionIdUtils | ✅ 已完成 | msg_context.rs：canonicalKey + deterministic_hash（3 单测） |
| **SubagentRegistry/Record/Materializer/GatewayBridge** | SubagentRegistry 系 | ✅ 已完成 | subagent.rs：SubagentRecord/SubagentRegistry/InMemorySubagentRegistry（3 单测）+ harness_gateway 的 SubagentMaterializer/GatewayBridge；**本轮补 `StoreBackedSubagentRegistry`（BaseStore namespace ["subagents","exposed"]，TTL 惰性回收 + revokeByParentSession，2 单测）** |
| **SubagentGatewayBridge** | SubagentGatewayBridge | ✅ 已完成（本轮） | `gateway/subagent_bridge.rs`：`SubagentGatewayBridge` trait（expose→ExposeResult{subagent_id}）+ `GatewaySubagentBridge`（委托 HarnessGateway::expose_subagent）；1 集成测试（expose→run_subagent e2e）过 |
| **channel/* 全套** | ChannelRouter(7层)/ChannelConfig/Binding/DmScope/InboundMessage/OutboundAddress/Peer/PeerKind/RouteResult | ✅ 已完成 | `gateway/channel/` 新建：peer.rs(PeerKind/Peer) + dm_scope.rs(DmScope) + binding.rs(ChannelBinding) + config.rs(ChannelConfig/Builder) + inbound.rs(InboundMessage/Builder) + router.rs(ChannelRouter 7 层路由 + RouteResult) + channel.rs(Channel trait/ChannelFactory)；OutboundAddress 升级为完整版(channel_id/account_id/to/thread_id)；7 层(explicit/peer/parentPeer/guild+roles/guild/team/account/channel→default)；router 8 单测 + peer 2 单测通过 |
| **channel/chatui/** | ChatUiChannel/Request/Envelope/SendOptions | ✅ 已完成 | `gateway/channel/chatui.rs`：ChatUiChannel(create/with_config/per_peer/with_gateway) + ChatUiRequest + SendOptions + OutboundEnvelope；dispatch/preview_route/send/send_peer/send_to_subagent/send_request/poll_outbound；实现 Channel trait + ChannelSink(缓冲投递)；streaming 待接 gateway runStream(见 7.1) |

### 3.4 memory（优先级 P1）— compaction 完整，两条 LLM 流水线缺
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| compaction 全链路（TokenCounter/Config/ConversationCompactor/Prune/Truncate） | 同名 | ✅ 已完成 | ~90%；flush/offload 钩子被略过；**#3009 thinking block token 已按内容估算 + ToolResult 内嵌套 thinking 递归计数** |
| session（SessionEntry/SessionTree/FreshnessEvaluator） | 同名 | ✅ 已完成 | ~90% 结构；SessionTree 无文件 IO/remote 镜像/deferred flush |
| **MemoryFlushManager** | MemoryFlushManager | ✅ 已完成 | memory/flush.rs（403 行）：LLM 抽取→今日 ledger（memory/YYYY-MM-DD.md），NO_REPLY 兜底，跳过 internal context；3 单测通过 |
| **MemoryConsolidator** | MemoryConsolidator | ✅ 已完成 | memory/consolidator.rs（305 行）：定时合并每日 ledger→MEMORY.md；2 单测通过 |
| **ToolResultEvictionConfig** | ToolResultEvictionConfig | ✅ 已完成 | memory/eviction.rs（165 行）：单超大结果落盘+占位符（head/tail/path）；2 单测通过 |
| **MemoryConfig 补全**（FlushTrigger/prompt/保留天数） | MemoryConfig | ✅ 已完成 | memory/mod.rs：MemoryConfig/FlushMode/FlushTrigger + 4 单测；3 条 LLM 流水线（flush/consolidation/compaction）配置统一 |

### 3.5 bus（优先级 P1）— 已完成
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **MessageBus** | MessageBus(267行) | ✅ 已完成 | bus/message_bus.rs；Mode A drain / Mode C replay + inbox*/enqueue_wakeup/session* 域助手（默认方法） |
| **WorkspaceMessageBus** | WorkspaceMessageBus | ✅ 已完成 | bus/workspace_message_bus.rs；基于 Filesystem trait，queue/log 文件布局；Mode D 降级为 3s 轮询（unbounded receiver） |
| **AsyncToolRegistry / WorkspaceAsyncToolRegistry / AsyncToolRecord** | 同名 | ✅ 已完成 | bus/async_tool.rs；{root}/{session}/{id}.json；register/complete/fail/findStale/markTimeout |

### 3.6 sandbox（优先级 P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| ExecResult/WorkspaceSpec/DefaultWorkspaceSpecApplier | 同名 | ✅ 已完成 | sandbox.rs |
| **Sandbox trait + SandboxClient + SandboxManager** | Sandbox/SandboxClient/SandboxManager | ✅ 已完成（本轮） | sandbox/ 目录模块：Sandbox trait (start/stop/shutdown/exec/persist_workspace/hydrate_workspace)、SandboxClient trait (create/resume/delete/serialize_state/deserialize_state)、SandboxManager (4优先级获取策略：external sandbox > external state > persisted state > new create)、SandboxContext (builder模式)、SandboxState (Docker/Local/Remote enum)、SandboxIsolationKey、SandboxExecutionGuard (并发隔离)、SandboxLease (执行租约)、InMemorySandboxStateStore；25 个测试通过 |
| **SandboxLifecycleMiddleware** | SandboxLifecycleMiddleware | 待办 | 依赖 SandboxManager + SandboxBackedFilesystem |
| **docker 后端** | DockerSandbox | ✅ 已完成（本轮） | `sandbox/docker.rs`：DockerSandboxClient + DockerSandbox，**走 `docker` CLI（不引 bollard，与 Java 参考实现一致，见 ADR）**；含容器生命周期（inspect/复用/重启/新建）、`docker exec -w <root> <id> sh -c` 执行（512KB/流截断、超时返回 -1）、tar 持久化/回灌（stdin 管道 + 双流并发 drain + 超时 taskkill 进程树）、shutdown（stop --time + rm --force，container_owned=false 跳过）、start 4 分支（A 保留/ B 降 D / D 新建，C 待 snapshot）；WorkspaceSpec 经 `docker exec` 物化（base64 写文件，mkdir -p）；DockerSandboxState 扩展 container_name/owned/memory/cpu/ports/network/additional_run_args/volumes/workspace_spec（旧 JSON 可加载，缺省 owned=true）；SandboxClientOptions 增 workspace_root/exposed_ports/additional_run_args/volumes |
| **snapshot 持久化** | SnapshotSpec | ✅ 已完成（本轮） | `sandbox/snapshot.rs`：SandboxSnapshot trait（persist/restore/is_restorable/id/type/is_persistence_enabled）+ LocalSandboxSnapshot（原子 temp+rename、id 路径安全校验）+ RemoteSandboxSnapshot/RemoteSnapshotClient + NoopSandboxSnapshot + 工厂 trait `SandboxSnapshotSpec` 与 Local/Remote/Noop 三个 spec；原 `SandboxSnapshotSpec` 结构体已升级为 trait，`SandboxContext`/`SandboxClient::create` 改用 `Arc<dyn SandboxSnapshotSpec>`；8 单测过。**Docker 已接线**：create 时按 session_id `spec.build()` 并 `with_snapshot`；`stop()` 在 persistence enabled 时 `persist_workspace()`→snapshot.persist（noop 跳过归档）；`start()` 分支 A 保留 / 分支 B·C snapshot 可恢复则 hydrate+重放 spec / 否则分支 D 全新初始化；新增 2 单测（无 daemon：绑定/noop/无归档降级 + noop stop 不归档） |
| **Sandbox Layout entries** | WorkspaceEntry/DirEntry/FileEntry/GitRepoEntry/BindMountEntry/Local*Entry/WorkspaceProjectionEntry | ✅ 已完成（本轮） | `sandbox/layout.rs`：7 类 entry + `WorkspaceEntry` 内部 tag 枚举（type=file/dir/local_file/local_dir/git_repo/workspace_projection/bind_mount）+ `LayoutSpec`（root/entries/environment）+ `to_workspace_spec()` 扁平化；7 单测过 |
| **Sandbox helpers** | WorkspaceMountSupport/WorkspaceArchiveExtractor/WorkspaceProjectionApplier/SessionSandboxStateStore | ✅ 已完成（本轮） | `sandbox/helpers.rs`：bindMountRelativePaths/hasBindMounts/containerMountPath/tarExcludeArgs/topLevelBindMounts/normalizedHostPath；extract_tar_bytes（最小 ustar 解析 + Zip-Slip 绝对路径/`..`/NUL/root 逃逸防护）；ProjectionPayload（SHA-256 hash + 最小 ustar 归档 + fileCount）；6 单测过。SessionSandboxStateStore 语义由既有 `SandboxStateStore` 承担 |

### 3.7 middleware（优先级 P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| CompactionMiddleware/PlanModeMiddleware/AtPathExpansionMiddleware | 同名 | ✅ 已完成 | 系统提示词转换；middleware.rs 已重构为 middleware/ 目录模块 |
| **InboxMiddleware** | InboxMiddleware | ✅ 已完成 | middleware/inbox.rs：on_reasoning 排空 inbox + stale async tools（find_stale/mark_timeout）→ 注入 HintBlock 到 reasoning 输入 + 发 HintBlock 事件；`once(fut).flatten()` 组合流；4 集成测试通过 |
| **AsyncToolMiddleware** | AsyncToolMiddleware | ✅ 已完成 | middleware/async_tool.rs：on_acting 超时后台化——spawn 后台任务执行 next + watch 信号竞争 offload_timeout；超时注册 async record + 发 placeholder ToolResult 事件，后台完成后 inbox 回投 hint + enqueue_wakeup；依赖 RuntimeContext.session_id；集成测试通过 |
| **MemoryFlushMiddleware** | MemoryFlushMiddleware | ✅ 已完成 | middleware/memory_flush.rs：on_agent 尾部执行 LLM 记忆抽取（MemoryFlushManager::flush_memories）+ 消息 offload（JSONL 按 msg id 幂等）；FlushTrigger 节流（进程级 SHARED_LAST_FLUSH_AT）；`once(tail).map(iter).flatten()` 尾部流；2 内联单测通过 |
| **MemoryMaintenanceMiddleware** | MemoryMaintenanceMiddleware | ✅ 已完成 | middleware/memory_maintenance.rs：on_agent 尾部节流执行 ①过期每日记忆归档 memory/archive ②LLM 合并（MemoryConsolidator）③清理过期 *.log.jsonl；glob 用 `{MEMORY_DIR}/*.md`、`{AGENTS_DIR}/**/*.log.jsonl`（根路径，匹配 root 相对路径）；2 内联单测通过 |
| **ToolResultEvictionMiddleware** | ToolResultEvictionMiddleware | ✅ 已完成 | middleware/tool_result_eviction.rs：on_reasoning 将超限 ToolResultBlock 落盘 {eviction_path}/{agent}/{toolCallId}-{hash}，原位替换为 head+tail 占位符 + EVICTED_METADATA_KEY 防递归驱逐；3 内联单测通过 |
| **SubagentsMiddleware** | SubagentsMiddleware | ✅ 已完成 | middleware/subagents.rs：on_reasoning 注入 ①subagents 系统提示节（render_subagent_section，过滤 hidden/Primary）②Async tasks 摘要（build_task_summary，≤10 条）③待交付 <system-reminder>（build_delivery_reminder，≤10 条），交付后 mark_delivered；公共辅助 prepend_to_system_message；依赖 WorkspaceTaskRepository（find_pending_deliveries/list_tasks）；配套 SubagentDeclaration 扩展（mode/hidden/persist_session/url/headers）+ SubagentEntry/SubagentFactory/DefaultAgentManager + TaskTool（task_output/task_cancel/task_list）；9 集成测试通过 |
| **DynamicSubagentsMiddleware** | DynamicSubagentsMiddleware | ✅ 已完成（已滞后） | middleware/dynamic_subagents.rs：on_reasoning 每步重新解析两层（Layer2 本地 subagents/ 目录 + Layer1 Filesystem glob subagents/*.md），动态覆盖静态同名；可选 factory_builder 物化 + DefaultAgentManager 刷新 + extra_tools；`load_from_directory`/`load_from_filesystem`/`parse`（AgentSpecLoader .md front-matter）；10 测试（4 单测 loader + 5 集成 + 1 含 middleware）通过；**本会话修复其 1 个失败测试（NoopFactory.create unreachable → DummyAgent 返回）** |
| **AgentTraceMiddleware** | AgentTraceMiddleware | ✅ 已完成（本轮） | middleware/agent_trace.rs：INFO 概要日志（PRE/POST_CALL、PRE/POST_REASONING、PRE_ACTING/POST_ACTING）+ DEBUG 详情；用 `WithDone` 流包装器在阶段流完成时触发回调；ReasoningTrace 累积 TextBlockDelta delta + ToolCallStart；ActingTrace 观察 ToolResultStart/TextDelta/End（toolCallId→name，result_len/state）；`AgentTraceMiddleware::new`/`with_debug`；6 单测 + 2 集成通过；**注：on_agent 的 POST_CALL 用事件流观察近似（Rust 无 resolveAgentState），on_reasoning/on_acting 完整等价**；**注：这些细分事件当前在 core 无实际发射点（类型齐全），中间件就绪待事件接入** |
| **WorkspaceContextMiddleware** | WorkspaceContextMiddleware | ✅ 已完成（本轮） | middleware/workspace_context.rs：core `Middleware::on_system_prompt` 为同步（无法异步读文件）→ 改用 `on_agent` 拦截（每次 call 运行一次，匹配 Java 语义），async 构建 section 后追加到首条 system 消息（复用 `subagents::prepend_to_system_message`）；含 workspace 段落（Java "Other" legacy 单行分支）+ session context 节（日期/OS/arch/temp）+ session dynamic part（AgentStateStore ID + environment memory）+ loaded context + XML context + additional context files + knowledge block + token 预算截断（estimate len/4、`chars().take` 防 UTF-8 边界 panic）；10 内联测试通过 |
| **SandboxLifecycleMiddleware** | SandboxLifecycleMiddleware | 待办 | 依赖 SandboxManager + SandboxBackedFilesystem（sandbox 骨架） |

### 3.8 subagent（优先级 P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| SubagentDeclaration 数据类 | SubagentDeclaration | ✅ 已完成 | |
| **task 仓库 / 动态 spawn** | SubagentTask/SubagentTaskRepository | ✅ 已完成 | `subagent/task/` 目录：background/status/spec/delivery/record/repository/workspace；TaskRunSpec Local/Remote/Adopted；WorkspaceTaskRepository 基于 Filesystem（`tasks/<session>.json`）+ 内存覆盖层 + 推送交付 API；5 集成测试通过 |
| **Subagent task 接线**（DefaultSubagentMaterializer + TaskCompletionBridge） | SubagentMaterializer + GatewayBridge | ✅ 已完成（本轮） | gateway/task_bridge.rs：DefaultSubagentMaterializer（从 DefaultAgentManager 工厂注册表重建 GatewayAgent）+ CallableAgentBridge（CallableAgent→GatewayAgent 适配器）+ TaskCompletionBridge（completion_callback → MessageBus.enqueue_wakeup 桥接）+ wire_task_completion 便捷函数 + create_completion_callback；DefaultAgentManager 新增 get_factory 方法；3 内联测试（materializer create/unknown/bridge wakeup）通过 |

### 3.9 mcp / workspace / skill
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| harness mcp 声明 + Registrar | McpServerConfig/Registrar | ✅ 已完成 | 注册 stub 待接 |
| **完整版 WorkspaceManager** | WorkspaceManager(1032行) | ✅ 已完成（本轮） | workspace/manager.rs：两层次读（filesystem 优先 + 本地 fallback）、写经 Filesystem、命名空间 resolve_runtime_data_path、逐路径锁（read-merge-write 原子）、append/write 本地原子写（temp+rename）、session index upsert、task record JSON map 读写、sweep marker、skill draft/move、listMemory/listSessionLog/toWorkspaceRelativeString；**限制**：writeDraftSkillFile 的 overlay-lower 跨副本直写因 Rust Filesystem 无 downcast 略过（走标准路径） |
| **WorkspaceIndex（SQLite）** | WorkspaceIndex(372行) | ✅ 已完成（本轮） | workspace/index.rs：rusqlite(bundled)，files 表（path PK/size/mtime/present_local）+ index_state；is_indexable（agents//, memory/）、upsert/upsert_from_local_file/remove/rename/exists/list_by_prefix/has_prefix/rebuild_from_disk；**Connection 非 Sync → 用 Mutex 包裹使 Arc 共享合法**；5 单测过 |
| **NamespaceFactory** | NamespaceFactory | ✅ 已完成（本轮） | workspace/namespace.rs：trait `get_namespace(&RuntimeContext)` + NoNamespace + UserSessionNamespace（user_id/session_id → `users/<uid>/sessions/<sid>` 元组） |
| WorkspaceConstants/PathNormalizer/PlanModeManager | 同名 | ✅ 已完成 | workspace/*.rs |
| skill 全模块 | curator/runtime/stager/prompt/shell_path | ✅ 已完成 | 7 文件；LLM umbrella 模式待接 |
| **SkillSecurityScanner** | SkillSecurityScanner(15KB) | ✅ 已完成（本轮） | `skill/security.rs`：6 类检测（EXFILTRATION/INJECTION/DESTRUCTIVE/PERSISTENCE/NETWORK/OBFUSCATION）18 条 regex 规则（regex crate）、Finding/ScanResult/report_text、scan(skill_md+resources)/scan_single_file/should_allow（trust×verdict 安装策略）；8 单测过 |
| **Curator 过滤器 + Backend** | CanaryFilter/EnvironmentFilter/NotifyAndWaitGate/NotificationSink/FilesystemSkillUsageBackend | ✅ 已完成（本轮） | `skill/filters.rs`：`SkillUsageBackend` trait + FilesystemSkillUsageBackend（JSON sidecar，loadAll/get/mutate/replaceAll）；CanaryFilter（Java String.hashCode 稳定分桶，agent-created 限定，0/100 边界）；EnvironmentFilter（sidecar environments 匹配，无 sidecar 放行）；NotificationSink trait + Noop/Logging；NotifyAndWaitGate（写 .review_request.json + 触发 sinks，返回 NeedsReview=Defer）；SkillUsageRecord 新增 environments 字段 + SkillUsageStore.get；6 单测过。curator.rs 顶部 re-export 保持 `skill.curator` 包布局一致 |
| **SkillRuntime 完整性** | SkillRuntime | ✅ 已完成（本轮） | `skill/runtime.rs`：新增 `SkillCatalogSnapshot`（ids + filesRoots 的 serde 投影，作为 load tool 授权边界）；`SkillRuntime::install(catalog, &mut RuntimeContext)` 将快照写入 `RuntimeContext.metadata[agentscope_skill_catalog]`、`catalog_snapshot_for(ctx)` 读取、`prepare_toolkit(toolkit, snapshot)` 幂等注册限定 catalog 的 `LoadSkillTool`（`LoadSkillTool::from_snapshot` + `TOOL_NAME` 常量）；保留 legacy context-free catalog；5 单测过。**注**：Rust 中间件拿到的是不可变 `&RuntimeContext`，故 per-call 绑定需宿主持有可变 context（Java 的可变 RuntimeContext 语义；core 中间件可变 context 缺口已在 runtime.rs 文档注明） |


### 3.10 acp（优先级 P1）— 新增模块，全链路落地
| 缺口项 | Java/Kotlin 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AcpCodingAgent（包装 ReActAgent） | Kotlin AgiAgent / Java HarnessCodingAgent | ✅ 已完成 | `crates/agentscope-extensions-acp/src/coding_agent.rs`；crate `lib.rs` 导出 AcpAgentSession/AcpCodingAgent/AcpCodingAgentBuilder |
| AcpAgentSession（权限确认/取消/流式） | Kotlin AgiAgentSession | ✅ 已完成 | `crates/agentscope-extensions-acp/src/session.rs`：ConfirmHandler 用户交互确认 + PermissionRuleStore 持久化 + 模式映射（plan/accept_edits/yolo）；$/cancel_request 取消（SDK Responder cancellation marker + run_with_transport 可注入 transport，进程内 Channel::duplex 取消 e2e） |
| 流式输出与工具调用流 | — | ✅ 已完成 | acp/event.rs：思考/工具调用带 message_id、逐 token 增量流式、兼容 reasoning 字段；工具参数与位置随通知推送、ToolCall 创建延迟至参数就绪（Pending→raw_input→Completed，raw_output+content） |
| 动态模型切换 + per-session headers | — | ✅ 已完成 | session.rs：model_factory/model_builder 回调（签名含 sessionId）；模型切换失败通知客户端；x-opencode-session 由外部注入绑定 ACP sessionId（core 硬编码已移除）；思考等级映射（auto 默认不传；显式等级统一只传官方 `reasoning_effort`，不传 `enable_thinking`/`thinking_budget`——严格网关会以 `unknown field "thinking_budget"` 400 拒绝）；UsageUpdate 为当前上下文 token 计数；上下文窗口推送 |
| 配置与日志 | — | ✅ 已完成 | acp/config.rs：会话配置 Arc<Mutex> 共享防克隆丢失；AGENTSCOPE_LOG_DIR 固定日志目录 + ACP agent 文件日志 |
| **ACP 客户端 MCP 支持（mcp_servers 声明 → McpClientManager 桥接）** | ACP MCP 能力 | ✅ 已完成 | `AcpAgentSession` 新增 `apply_mcp_servers(&[McpServer])`：解析 stdio 声明（`McpServer::Stdio`，HTTP/SSE 跳过告警）、经 `McpClientManager` 连接 + `bridge_mcp_tools_to_toolkit` 注册到会话 Toolkit；`NewSessionRequest`/`LoadSessionRequest`/`ResumeSessionRequest` 的 `mcp_servers` 均已接入（幂等去重）；会话级动态注入（已存在会话再次 load/resume 补桥接） |
| 已知 flaky | — | ⚠️ 注意 | acp_stdio_thought_test 全量跑时首轮失败/卡顿（stdio 时序），单独复跑通过 |

### 3.11 coordination（优先级 P2）— 本轮新增
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **PeriodicGate / LocalPeriodicGate / StoreBackedPeriodicGate** | coordination/* | ✅ 已完成（本轮） | `agent/coordination/mod.rs`：`PeriodicGate` trait（try_claim(name,min_gap)）；`LocalPeriodicGate` 进程级共享 map（OnceLock+Mutex，等价 Java static ConcurrentHashMap+CAS；含 cleanupStaleEntries / new_isolated / 测试种子钩子）；`StoreBackedPeriodicGate` 基于 `BaseStore::put_if_version` CAS（namespace ["coordination","periodic"]，5 次重试）；6 单测过 |

### 3.12 subagent protocol / remote（优先级 P2）— 本轮新增
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **RemoteEventType / RemoteAgentEvent / RemotePendingConfirm / RemoteConfirmDecision / RemoteStreamDetail** | protocol/* | ✅ 已完成（本轮） | `subagent/protocol/event.rs`：SCREAMING_SNAKE wire 枚举（含 AGENT_EVENT）、camelCase + NON_NULL DTO、HITL 确认 DTO、detail 三档（status/full/verbose，wire_value/includes/parse）；5 单测过 |
| **RemoteEventCodec** | RemoteEventCodec | ✅ 已完成（本轮） | `subagent/protocol/codec.rs`：`from_agent_event`（7 类专用 wire 类型 + payload 全量序列化 + tool_calls→pendingConfirms）、`to_agent_event`（payload 优先还原；否则按 flat 字段重建 AgentEvent；taskId 注入 metadata）、`matches_detail(±event)`；8 单测过 |
| **AgentProtocolTaskClient / RemoteTarget / RemoteTaskStatus / RemoteSubmitContext / RemoteSubagentTransport / AgentProtocolTransport** | task/* | ✅ 已完成（本轮） | `subagent/task/protocol_client.rs`：reqwest 客户端实现 submitTask(POST /tasks)/getStatus(GET /tasks/{id})/waitForResult(GET /tasks/{id}/wait)/cancelTask/resumeTask + SSE open_event_stream（bytes_stream + id/data 解析 + last-event-id，404/405 降级 no-op）；RemoteSubmitContext(builder+to_map) / RemoteTaskStatus 断言 / RemoteSubagentTransport trait + AgentProtocolTransport；6 单测过（含 SSE 解析）。真实 HTTP e2e 未验（无服务端） |

### 3.13 迁移清单（原 `docs/MIGRATION_GAP_TRACKER.md`，M/T/S/B/G/R/D）

> 已从 `docs/MIGRATION_GAP_TRACKER.md` 合并入本文件（该文件随后删除）。状态图例：❌ 缺失 / 🟡 桩 / 🟠 部分 / ✅ 完成。

**Missing Modules**
| # | Java module | Rust target | Status | Priority |
|---|---|---|---|---|
| M1 | `agent/team/` | `src/agent/team/` | ✅（LocalTeamClient 进程内 CAS；store-backed 分布式实现待办） | P1 |
| M2 | `agent/coordination/` | `src/agent/coordination/` | ✅ | P2 |
| M3 | `agent/transcript/` | `src/agent/transcript/` | ⛔ 已删除（2026-10-21）——由 `core/session` journal 取代 | P1 |
| M4 | `agent/middleware/TranscriptMiddleware` | `src/agent/middleware/transcript.rs` | ⛔ 已删除（2026-10-21）——由 `SessionRecorder` 取代 | P1 |
| M5 | `agent/middleware/TeamsMiddleware` | `src/agent/middleware/teams.rs` | ✅（系统提示队伍节 + 每轮 mailbox drain；wakeup 注入待 gateway runStream） | P1 |
| M6 | `agent/middleware/SandboxLifecycleMiddleware` | `src/agent/middleware/sandbox_lifecycle.rs` | ✅ | P1 |

**Missing Tools**
| # | Java Tool | Rust target | Status | Priority |
|---|---|---|---|---|
| T1 | `AgentSpawnTool` | `src/tool/agent_spawn.rs` | 🟠 薄实切面：fg/bg spawn + 权限最小化 + plan-mode 传播 + end_spawn + **远程目标（本轮接线 `TaskRunSpec::Remote`）**；流式事件待办 | P0 |
| T2 | `AgentGenerateTool` | `src/tool/agent_generate.rs` | ✅ | P2 |
| T3 | `TeamTool` | `src/tool/team.rs` | ✅（TeamClient 10 ops + 角色裁剪；submitPlan/approvePlan 待 plan 桥） | P1 |
| T4 | `MemorySearchTool` | `src/tool/memory_search.rs` | ✅ | P0 |
| T5 | `MemoryGetTool` | `src/tool/memory_get.rs` | ✅ | P0 |
| T6 | `MemorySaveTool` | `src/tool/memory_save.rs` | ✅ | P0 |
| T7 | `SessionSearchTool` | `src/tool/session_search.rs` | ✅ | P2 |
| T8 | `PlanModeTools` | `src/tool/plan_mode.rs` | ✅ | P1 |
| T9 | `WaitAsyncResultsTool` | `src/tool/wait_async.rs` | ✅ | P1 |
| T10 | `ProposeSkillTool` | `src/agent/skill/load_tool.rs` | ✅ | P2 |
| T11 | `SkillManageTool` | `src/agent/skill/manage.rs` | ✅ | P2 |
| T12 | `WebTools` | `src/tool/web_tools.rs` | ✅ | P2 |

**Stub / Incomplete Files**
| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| S1 | `RemoteFilesystem` | `src/filesystem/remote/` | ✅ | P2 |
| S2 | `remote/store/`（BaseStore/InMemoryStore/NamespaceFactory/StoreItem） | `src/filesystem/remote/store.rs` | ✅ | P2 |
| S3 | `BakedContextFilesystem` | `src/filesystem/baked.rs` | ✅ | P2 |
| S4 | `ProjectAwareOverlay` | `src/filesystem/overlay.rs` | ✅ | P2 |
| S5 | `RoutedSandboxFilesystem` | `src/filesystem/sandbox.rs` | ✅ | P2 |
| S6 | `LocalFilesystemWithShell` | `src/filesystem/local.rs` | ✅ | P2 |
| S7 | Filesystem result types | `src/filesystem/result.rs` | ✅ | P1 |
| S8 | `FilesystemUtils` | `src/filesystem/util.rs` | ✅ | P2 |
| S9 | `SkillSecurityScanner` | `src/agent/skill/security.rs` | ✅ | P2 |
| S10 | Curator filters | `src/agent/skill/filters.rs` | ✅ | P2 |

**Sandbox**
| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| B1 | Docker Sandbox backend | `src/agent/sandbox/docker.rs` | ✅（docker CLI：create/resume/exec/persist/hydrate/start 4 分支 A/B·C/D/shutdown；snapshot 已接线） | P0 |
| B2 | Sandbox Snapshot（Local/Remote/Noop） | `src/agent/sandbox/snapshot.rs` | ✅ | P2 |
| B3 | Sandbox Layout entries | `src/agent/sandbox/layout.rs` | ✅ | P2 |
| B4 | Sandbox helpers | `src/agent/sandbox/helpers.rs` | ✅（SessionStateStore 由 `SandboxStateStore` 承担） | P2 |

**Gateway**
| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| G1 | `ChannelFactory` | `src/agent/gateway/channel/channel.rs` | ✅ | P2 |
| G2 | `SubagentGatewayBridge` | `src/agent/gateway/subagent_bridge.rs` | ✅ | P2 |
| G3 | `SessionIdUtils` | `src/agent/gateway/msg_context.rs` | ✅ | P2 |
| G4 | `TurnBusyException` | `src/agent/gateway/session_turn_gate.rs` | ✅ | P2 |
| G5 | `LocalSessionTurnGate`（CAS） | `src/agent/gateway/session_turn_gate.rs` | ✅ | P2 |
| G6 | `StoreBackedSubagentRegistry` | `src/agent/gateway/subagent.rs` | ✅ | P2 |

**Subagent Protocol / Remote**
| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| R1 | `RemoteEventCodec` | `src/agent/subagent/protocol/codec.rs` | ✅ | P2 |
| R2 | `RemoteAgentEvent` / `RemoteEventType` | `src/agent/subagent/protocol/event.rs` | ✅ | P2 |
| R3 | `AgentProtocolTaskClient` | `src/agent/subagent/task/protocol_client.rs` | ✅ | P2 |

**Other Details**
| # | Item | Rust target | Status | Priority |
|---|---|---|---|---|
| D1 | `AtPathExpansionMiddleware` | `src/agent/middleware/at_path.rs` | ✅ | P2 |
| D2 | `HarnessAgent.Builder` auto-wiring | `src/agent/harness_agent.rs` | 🟠 Partial — 2026-09-17 已补 13 项 setter，余见 §3.14 | P2 |
| D3 | `SkillRuntime` completeness | `src/agent/skill/runtime.rs` | ✅ | P2 |
| D4 | `WorkspaceSpecApplier`（+ProjectionApplier/MountSupport/ArchiveExtractor） | `src/agent/sandbox/` | ✅ | P2 |
| D5 | `SubagentDeclaration` complexity | `src/agent/subagent/{mod.rs,loader.rs}` | ✅（P0/P1 全闭；残余见 §3.14） | P2 |
| D6 | `WorkspaceTaskRepository` complexity | `src/agent/subagent/task/{repository,record,workspace}.rs` | 🟠 Partial — 见 §3.14（字段/CAS 守卫/传输抽象/维护循环/持久化 CAS/Remote 生产接线均已补；仅 `when_complete` 未做） | P2 |

**Progress Summary（原 docs 文件口径）**
| Phase | Items | Done |
|---|---|---|
| P0 | T1, T4, T5, T6, B1 | 5/5（T1 partial） |
| P1 | M1, M3, M4, M5, M6, T3, T8, T9, S7 | 9/9 ✅ |
| P2 | M2, T2, T7, T10, T11, T12, S1–S10, B2–B4, G1–G6, R1–R3, D1–D6 | 32/34（D2/D6 partial） |

### 3.14 缺口核对记录（D2/D5/D6 复盘，原 docs 文件 §D2/§D5/§D6）

> 复核时间：2026-09-17；对象为工作区当前代码。

#### D5 — `worker_result_e7714892.txt`（SubagentDeclaration）→ 已完成

已关闭（`src/agent/subagent/mod.rs`、`loader.rs`、`protocol/event.rs`）：
- ✅ 新增字段 `inherit_parent_permissions`(默认 true)、`enable_pending_tool_recovery`(tri-state)、`remote_streaming`、`remote_stream_detail`、`remote_ask_policy`、`remote_context_attributes`。
- ✅ 新枚举 `RemoteAskPolicy { Deny, Propagate }`（SCREAMING_SNAKE_CASE），已在 `agent/mod.rs`、`lib.rs` 导出。
- ✅ YAML 解析 `enable_pending_tool_recovery` / `enablePendingToolRecovery` 双别名并写入声明。
- ✅ P1 日志：description 缺失、`workspace.path`+body 冲突、未知 `workspace.mode`、未知 `mode` 枚举值均 `tracing::warn!`。
- ✅ `RemoteStreamDetail` 补 `Serialize`/`Deserialize`（含 serde 单测）。
- ✅ 便捷 API `has_definition_workspace()` / `is_remote_streaming()` / `remote_stream_detail_or_default()`。

残余（P2，未做）：
- ⚠️ `SubagentDeclaration::new()` 的 `steps` 仍为 `None`（`loader::parse` 已默认 `Some(10)`，仅程序化构建无默认值）。
- ⚠️ `SubagentSpecGenerator`（LLM 驱动 spec 生成）完全缺失。
- ⚠️ 无 `load_from_file` 单文件入口（可用 `parse()` 替代）；Java deprecated `getMaxIters()` 未实现（可接受）。

#### D6 — `worker_result_ae6624a8.txt`（WorkspaceTaskRepository）→ 仅 `when_complete` 未做

已关闭：
- ✅ `TaskRecord` 新增 `awaiting_confirm` / `pending_confirms` / `last_event_seq`（camelCase serde + 缺省兼容单测）；新增 `RemotePendingConfirm` DTO（`subagent/protocol/event.rs`）。
- ✅ `update_record_status` CAS 守卫：终态不可变 + `cancel_requested` 拒绝非终态写入（2 单测）。
- ✅ `mark_delivered` 幂等（首次写入生效 + 单测）；`find_pending_deliveries` 过滤/排序与 Java 一致。
- ✅ 传输抽象落地：`RemoteSubagentTransport` trait + `AgentProtocolTransport`（submit/status/wait_for_result/cancel/resume + SSE）、`AgentProtocolTaskClient`、`RemoteSubmitContext*`、`RemoteTaskStatus::is_awaiting_confirm()`。
- ✅ `WorkspaceTaskRepository::set_completion_callback` + `gateway/task_bridge.rs` 接线。
- ✅ `TaskRepository` trait 声明 `set_completion_callback` / `shutdown`（默认 no-op，`subagent/task/repository.rs:91,94`），导出 `SUPPRESS_COMPLETION_CALLBACK`（`repository.rs:21`）。
- ✅ `put_task` 的 `TaskRunSpec::Remote` 分支接 `AgentProtocolTransport`：`run_remote_task` → `poll_remote_until_done`（200ms→5s 指数退避，awaiting_confirm 期间 750ms 轮询），`cancel_task` 增加远程取消分支（`subagent/task/workspace.rs`）。
- ✅ `update_awaiting_confirm()` 已实现（终态 no-op + 去抖）。
- ✅ `heartbeat()`（30s）与 `sweep_orphaned_tasks()`（5min 扫描 / 10min 判死）已实现，并由 `ensure_maintenance()` 随首次 `put_task` 启动（PeriodicGate 节流 + 首扫抖动；`shutdown()` 时 abort）。
- ✅ 持久化 CAS（2026-09-17）：`TaskRecord` 新增单调 `version`；`update_record_status` 改为乐观并发循环（重读→比对 `version`→重试，`RECORD_CAS_RETRIES=5`），`cancel_task` / `mark_delivered` / `update_awaiting_confirm` 写入时 bump version；新增 version/CAS 单测。
- ✅ `TaskRunSpec::Remote` 生产接线（2026-09-17）：`AgentSpawnTool` 对带 `url` 的声明后台 spawn 时构造 `TaskRunSpec::Remote`（`tool/agent_spawn.rs::remote_spec()` 映射 streaming/detail/context attributes）；前台远程目标明确报错要求 `background=true`；2 单测。

仍缺口：
- ❌ `BackgroundTask::when_complete`（Rust 走 `set_done` + Future 模型，暂不需要）。
- ⚠️ CAS 为文件层「重读-比对-重写」的乐观并发（非 FS 级原子）；`write_records` 仍为 delete-then-write，跨进程强一致需 store-backed 文件系统。

#### D2 — `worker_result_d90adbd7.txt`（HarnessAgent Builder）→ 报告作废，缺口已重新推导

- 原报告称 Java `HarnessAgentBuilderSupport` 有 62 setter、build() 位于 2358-2987 行，Rust `HarnessAgent.Builder` 有 68 方法（`parse_agent_map` / `heartbeat_config` / `activityTrackerConfig` 等）。实测不成立：真实 `HarnessAgentBuilderSupport.java` 是第 71 行的 package-private 小助手类；Java 真实 builder 在 `HarnessAgent.java`（2989 行 / 98 个 setter）；Rust `HarnessAgentBuilder` 仅 51 字段 / 53 个 `pub fn`。
- 原报告列出的 16 个“已确认字段”（`defaultToolProperties`、`toolPropertyOverride`、`disabledTools`、`allowedTools`、`additionalEnvironment`、`dbConfig`、`codexExecutionMode`、`maxAgentContextTokens`、`fileContextMaxFiles`、`perFileMaxTokens`、`extraFileAgentNames`、`logLevelOverrides`、`__MYSELF__`、`heartbeatConfig`、`activityTrackerConfig`、`parse_agent_map`）在 Java 与 Rust 两仓库中 grep 均 **0 命中**，全部为幻觉。
- 已补（2026-09-17，`src/agent/harness_agent.rs`；均有 builder setter + getter + build 接线/单测）：
  - ✅ `agentId(String)`（默认 `name`；驱动 ReActAgent `id` 与 task 仓库作用域）
  - ✅ `description(String)`（转发 ReActAgent）
  - ✅ `checkRunning(bool)`（存储 + getter）
  - ✅ `enablePendingToolRecovery(bool)`（转发 ReActAgent）
  - ✅ `enableMetaTool(bool)`（注册 `reset_equipped_tools` 元工具）
  - ✅ `enableTaskList(bool)`（映射 ReActAgent `todo_tool`）
  - ✅ `useLegacyXmlWorkspaceContext(bool)`（存储 + getter）
  - ✅ `asyncToolTimeout(Duration)`（存储 + getter）
  - ✅ `environment(String)`（默认 `"prod"`）
  - ✅ `taskRepository(repo)`（覆盖默认 `WorkspaceTaskRepository`）
  - ~~✅ `transcriptStore` + `disableTranscript()` + `transcriptTenant(...)`（接线 `TranscriptMiddleware`，默认 root `.agentscope/transcripts`）~~ —— **已废弃（2026-10-21）**：`TranscriptMiddleware` / `agent/transcript/` / `.agentscope/transcripts` 均已删除，会话历史统一由 `SessionLog`（journal）承载，对应 builder 旋钮不复存在。见 `docs/harness-session-context-migration.md`。
  - ✅ `teamsMode(TeamClient, TeamContext)`（注册角色裁剪 `team` 工具 + `TeamsMiddleware`；`TeamContext` 增 `member` 字段与 `new()`）
  - ✅ `disableMemoryTools()` / `disableWebTools()` / `webHttpClient(HttpClient)`（默认注册 `memory_search/get/save` + `web_fetch/web_search`，可禁用，客户端可注入）
- 仍缺口：
  - ❌ P0 `toolsConfig(ToolsConfig)` + `disableToolsConfig()`（Rust 无 `ToolsConfig` 类型）
  - ❌ P0 `filesystem(LocalFilesystemSpec/RemoteFilesystemSpec/SandboxFilesystemSpec)`（Rust 仅有 `filesystem(Arc<dyn Filesystem>)`，无 spec 便捷构造）
  - ❌ P1 `artifactDeliveryTarget(...)`、`subagentFactory(name, fn)`、`modelResolver(fn)`、`externalSubagentTool(...)`、`filesystemRoute(...)`、`mcpServerRegistrationListener`（无对应子系统）
  - ❌ P2 `registerExternalSchemas(...)`（Rust `Toolkit` 无 `register_schemas`）
- Rust 侧另有多项 Java 无对应（`cwd`、`session_id`/`user_id`、`conversation_compaction`、`confirm_handler`，以及 `register_filesystem_tools`/`register_shell_tool` 的 disable→enable 语义差异）。

### 3.15 新漂移复评（2026-10-26，harness / core 侧）

> 来源：`docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md` §4。
> Java 基线：`16cb024ae`（`main`，已用 `git rev-parse HEAD` 确认）；旧基线 `ed8073259`。
> 该复评初稿的 11 条 harness 侧缺口逐条回源码核实：**11 条成立**（4.4 的报告措辞不准确，
> 但确有一处真实的发布时机缺陷，已按方案 B 修复）；另更正 1 处行号（4.8）。

| # | 模块 | 缺口 | 执行状态 | 交接信息/说明 |
|---|---|---|---|---|
| 4.2 | subagent.task | orphan sweep 不按原始隔离域写回 | ✅ 已完成（`c7ffa4f`） | `task/workspace.rs`：新增持久化 scope 注册表 `harness/task-scopes/<agentId>/<key>.json`（`namespace`/`user_id`/`session_id`，`.tmp`+`move_path` 原子写）；`scopeKey` 与 Java 一致（各段 base64url 无填充 + `.` 拼接，超长退化 SHA-256 hex，无新依赖）；默认域不注册；`sweep_once` 改遍历「默认域 + 已注册域」逐域读写的；`local_contexts` 降级为进程内快路径，写回时校验域一致 |
| 4.3 | skills | frontmatter 闭合围栏未要求独占一行 | ✅ 已完成（`95d40f2`，见 `SKILLS__GAP_TRACKER.md` P1-11） | `git.rs::split_frontmatter` 不再 `trim()` 行首：缩进的 `  ---` 属块标量正文；行尾空白仍允许 |
| 4.4 | context | 溢出压缩后重试未判「输入已在上下文」 | ✅ 已完成（本轮） | 报告的**两条都不成立**：「输入重复注入」不存在（`on_model_call` 内联改写，无二次追加路径）；「缺 isInContext 判定」也不适用（重试不单独传 input）。进一步核对还推翻了本表上一版的「P0 判定」：`reasoning_phase` 在每轮模型调用前已把整份本地 state 发布到共享 state，调用后又由 `adopt_shared_context_compaction`（本轮更名为 `adopt_published_context`）取回压缩结果，所以「丢本轮输入 / 压缩不落盘」并不成立。**真正残留的缺陷**是发布时机：`before_step` 把本轮 steer/inject 指引折进**本地** context 是在那次发布**之后**，于是溢出压缩读到的是「缺本轮指引」的历史 → 重试请求与随后的落盘都会丢指引。修法（方案 B 定向交接）：① core 在 `before_step` 之后、模型调用之前只交接 `context` 字段（`publish_context`），调用后按「与交接时是否一致」精确取回中间件的改写（`adopt_published_context`，替换原「变短/换头」启发式）；② `compact_overflow` 的历史源从共享快照改为**本次实际请求** `messages`，避免重复注入被 `tool_result_eviction` 等 `on_reasoning` 中间件已从请求中移除的内容。测试：`session_step_boundary::guidance_folded_at_the_step_boundary_is_visible_in_the_shared_state`、`context_middleware::the_fallback_compacts_the_request_it_was_handed`、`context_middleware::the_agent_keeps_the_current_turn_and_persists_the_compaction`（三者均已做反向验证） |
| 4.5 | middleware.inbox | 提示注入形态与 Java 相反 | ✅ 已完成（`6fbc768`） | 每条 hint → 独立 `MsgRole::User` 通知（溯源句 + `synthetic`/`reminder_kind=background_event`/`source`/`hint_id` 元数据），并写回 `AgentState.context`；锁不跨 await |
| 4.6 | middleware.at_path | `@path` 展开形态与截断提示 | ✅ 已完成（`7223b25`） | 追加独立预览块（保留原 blocks）、`<attached_file path="...">`、中和 `</attached_file>`、按行截断 1000 行 + Java 提示语、单条消息 64k 字符预算、仅展开 user 消息；`collect_text` 按 Java `getTextContent()` 用 `\n` 连接 |
| 4.7 | skill.prompt | `<available_skills>` 默认仍 full-metadata | ✅ 已完成（`dbc3f02`） | `expose_all_metadata` 默认 `false`；slim 视图 `name`/`description` 取自 `AgentSkill` 字段且无条件输出（对齐 Java `metadataView`）；usage 文案换 Java 精简版；full 视图仍可 `with_parts(..., true)` |
| 4.8 | middleware.plan_mode | 无 PLAN MODE 横幅、无 model-call 刷新 | ✅ 已完成（`e531dd1`） | 新建 `middleware/plan_mode.rs`：`PLAN_BANNER_TEMPLATE` / `BUILD_MODE_PLAN_HINT` / `PLAN_EXTRA_TOOLS_HINT`、`PlanModeConfig::with_manager`（持 `Arc<PlanModeManager>`，支持运行中 `plan_enter`/`plan_exit` 翻转）、`on_model_call` 每次模型边界重算、`active_points() = OnSystemPrompt + OnModelCall`。**行号更正**：报告写的 `harness_agent.rs:1889-1892` 实为 **`:1900-1913`**（`plan: String::new()` 在 `:1909`）。**后续修复**：见下表后的「本轮顺带修复」——`plan_enter` 中途翻转此前会被下一轮 state 发布覆盖 |
| 4.9 | subagent | 子智能体无法独立配置 compaction | ✅ 已完成（`f6f2a67`） | `SubagentDeclaration` 新增 `compaction: Option<CompactionConfig>` / `disable_compaction`；优先级对齐 Java：声明级禁用 → 声明级配置 → 父级禁用 → 父级配置（父级禁用时声明级仍可单独启用） |
| 4.10 | context.render | 渲染时转义正文；AGENTS.md 无上限 | ✅ 已完成（`37e38cd`） | (a) 正文不再 `escape()`，改为 `neutralize_closing()`（只中和自身闭合标签 + 运行时外框 `</HARNESS_CONTEXT>`），属性值仍转义；(b) 新增 `MAX_AGENTS_MD_BYTES = 102_400` 与按 char boundary 回退的 `cap_agents_md()` |
| 4.11 | memory.compaction | 摘要提示远简于 Java | ✅ 已完成（`dbc3f02`） | `DEFAULT_SUMMARY_PROMPT` 补齐至 Java 的 7 段（新增 `USER PREFERENCES AND CORRECTIONS`、`COMMANDS AND ERRORS`、`objective_information`）与 `<extraction_rules>`/`<secrecy>`（逐字保留、superseded 标注、链式合并为一份）；`{messages}` 占位符保留 |

**本轮顺带修复（复评外，验证 4.4 时发现）**：

| # | 缺口 | 说明 |
|---|---|---|
| S1 | 回合内 `plan_enter` / `plan_exit` 被下一轮 state 发布覆盖 | 工具持共享 `Arc<Mutex<AgentState>>` 并直接改 `plan_mode_context`，而循环跑在私有克隆上；`reasoning_phase` 每次发布 `*shared = state.clone()` 会把刚翻转的标志写回旧值 → 后续请求既拿不到 PLAN 横幅，core 的 acting 拦截也退回 BUILD（等于 `plan_enter` 只对下一次 `call` 生效，且本回合内完全无效）。修法：发布前先把共享侧 `plan_mode_context` 取回本地（该字段只由工具写，共享侧始终是权威）。测试：`harness_plan_wait_tool_test::plan_enter_mid_turn_reaches_the_next_request`（已反向验证：去掉取回后第二个请求无 PLAN 横幅） |
| S2 | 刚观察到 `interrupted` 就 `resume` 会偶发失败 | 运行的收据在 `finish_with_error` 时就写成 `interrupted`，而 `active_run` 要到随后清空 → 宿主观测到该状态立刻续跑会撞上 `require_idle`，报“Wait for the current execution to stop / could not be continued”。全量测试并发压力下可复现（`acp_interrupted_turn_test::a_forced_interrupt_is_continued_without_discarding_the_prompt`）。修法：`resume` 改为等待上一轮释放槽位（`wait_for_idle`，上限 5s，超时才报原错误）；`respond_all` 仍用即时 `require_idle`（运行中应答本就非法）。测试：`session::session::tests::a_continuation_waits_for_the_previous_run_to_release_its_slot`、`a_run_that_never_stops_still_fails_the_continuation`（已反向验证：把 helper 换回即时失败 → 前者 FAILED） |

**同批复评的 core 侧缺口（C1/C2 本轮已修复，C3 仍待办）**：

| # | 模块 | 缺口 | 执行状态 | 交接信息/说明 |
|---|---|---|---|---|
| C1 | core.agent | tool-call 参数完整性语义相反 | ✅ 已完成（本轮） | `1ac8c8304`/`131c3bd9a`。Java 侧 `ToolCallsAccumulator` 是**provider 无关**的，因此修法放在 core 统一收口而不是只改 anthropic parser：`ReasoningContext` 记录 `finish_reason`（`length`/`max_tokens`，大小写不敏感），`build_final_message` 对**所有 provider** 的 tool call 调 `finalize_tool_call_arguments`。`content`（累积原始片段）是权威来源：合法 JSON 对象只补 `input` 缺失键（已解析值优先）；非空但非 JSON 对象 → 清空 `input` + `ToolUseBlock::METADATA_MALFORMED_ARGUMENTS`（`malformedArguments`）；raw 为空 + 输出被截断 + `input` 为空 → 同样标记。截断时 assistant msg 写 `metadata_keys::OUTPUT_LIMIT_REACHED`（`_output_limit_reached`）。acting 在 `run_acting_calls` 最前面拦截 malformed：发 `ToolResultStart` → 提示文本 → ERROR 结果 → `ToolResultEnd(Error)`，**不经过** plan/permission/middleware/executor（对齐 Java `writeMalformedArgumentResults`），提示语常量 `OUTPUT_LIMIT_TOOL_ARGUMENTS` / `INVALID_TOOL_ARGUMENTS` 逐字对齐 Java。测试：`react_agent::tests::malformed_tool_arguments_are_finalized_with_the_java_rules`、`react_agent::tests::truncated_tool_call_is_rejected_without_executing_and_pairs_its_result`（含 sibling 正常执行、事件、第二请求可见错误结果、assistant 截断元数据断言）；`anthropic::parser::tests::malformed_tool_arguments_keep_raw_content_for_the_core_to_reject`（原 `..._degrade_to_empty_input` 改名并新增 raw 片段保留断言）。反向验证：去掉 acting 拦截分支 → handler 执行次数 2（期望 1）；去掉 `finalize_tool_call_arguments` 调用 → 两个用例均 FAILED |
| C2 | core.agent | progressive tool events | ✅ 已完成（本轮） | `db0b67eb8`。**核对后修正了复评初稿的判断**：Rust acting 一直是逐工具内联发 `ToolResultStart/TextDelta/End`（模型侧 `ToolCallStart/Delta/End` 也是流式即时发），并不存在「按批聚合后才发」；真实差异是 `ToolkitConfig.parallel` 默认 `false` 且 acting 阶段**完全忽略该配置**，工具永远串行 → 快的工具不可能在慢的兄弟完成前可见。修法：`run_acting_calls` 改索引循环，`parallel` 开启时把「连续 + 并发安全 + 已批准」的调用交给 `run_parallel_batch` 并发执行，每个调用在**自身真实开始/结束时**发事件（完成序），`join_all` 后按**声明序** `bind_result` 落盘、整批统一结算（对齐 Java 的 callback + 有序聚合）；`Ask`/`Deny`/plan-denied/malformed/非并发安全工具仍走串行槽位。`ToolkitConfig` 默认值改为 `parallel: true` 对齐 Java `new Toolkit()`。测试：`react_agent::tests::a_fast_tool_is_visible_while_a_slower_earlier_tool_is_still_running`（GatedTool 门控：fast end 先于 slow end，落盘序仍为声明序）、`react_agent::tests::serial_mode_emits_start_when_execution_begins`（`parallel(false)` 下事件序 `start:slow → end:slow → start:fast → end:fast`）。反向验证：禁用并行分支 → 前者 5s 超时 FAILED |
| C3 | core.tool | `@Tool` proc-macro / `additionalProperties` | ⏳ 待办 | `7976a20d7`/`1aeb863c4`：无 `@Tool` 宏（既有 P0）；tool schema 无 `additionalProperties` 落点 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **测试基线**：`cargo test -p agentscope-harness` **lib 266 + 集成全绿**（本轮 P2 批量落地新增单测 76 条；core 未改动，仍 127）。**注意**：工作区 `test_all.txt`（2026-09-17 15:14）在 `acp_stdio_thought_test` 处因外部 provider `HTTP 403`（OpenCode 免费额度限制，非代码缺陷）中断，该次记录未跑完，需在有可用 provider 时复跑以确认全绿。
- **本轮（P2 批量落地，见 §3.13 P2 清单）**：
  - M2 coordination（PeriodicGate/Local/StoreBacked CAS）
  - S1 RemoteFilesystem 真实后端 + S2 remote/store（BaseStore/InMemory/NamespaceFactory/StoreItem）+ S8 FilesystemUtils + S3 BakedContextFilesystem
  - S9 SkillSecurityScanner（6 类 18 规则）+ S10 curator filters/backends（Canary/Environment/NotifyAndWaitGate/NotificationSink/FilesystemSkillUsageBackend）
  - B2 sandbox snapshot（Local 原子写/Remote/Noop + SandboxSnapshotSpec 由结构体升级为工厂 trait）+ B3 layout entries + B4 helpers（MountSupport/ArchiveExtractor 防 Zip-Slip/ProjectionApplier）
  - G2 SubagentGatewayBridge + G4 TurnBusyException + G6 StoreBackedSubagentRegistry（G1/G3/G5 已存在，核对后确认）
  - R1 RemoteEventCodec + R2 RemoteAgentEvent/RemoteEventType + R3 AgentProtocolTaskClient/Transport（含 SSE）
  - T7/T12/T2/T10/T11 经核对已在更早提交完成（b5e1635）
- **本轮（B1 Docker 沙箱后端）**：新增 `src/agent/sandbox/docker.rs`（DockerSandboxClient/DockerSandbox，docker CLI 实现，不引 bollard —— 与 Java 参考一致）；`DockerSandboxState` 补齐 container_name/container_owned/memory/cpu/ports/network/additional_run_args/volumes/workspace_spec（serde default 兼容旧 JSON）；`WorkspaceSpec`/`CopySpec` 加 serde；`SandboxClientOptions` 增 workspace_root/exposed_ports/additional_run_args/volumes；start 实现 Java 4 分支 A/B·C/D（C 已由 snapshot 接线：stop 持久化、start 可恢复则 hydrate+重放 spec）；新增 11 单测 + `tests/harness_docker_sandbox_test.rs`（无 docker daemon 自动跳过；本机无 docker，本轮仅验证跳过分支）
- **本轮（MCP 工具接入 ReActAgent + ACP 客户端 MCP 支持）**：`McpClientManager::connect()` 握手后 `request_tool_list()` 填充 tools；core `McpTool` 新增 `async_handler`（`with_async_handler`）解决 tokio 运行时内 `block_on` panic；`bridge_mcp_tools_to_toolkit` 把 MCP 工具注册为 `McpTool`；ACP `AcpAgentSession::apply_mcp_servers` 解析 NewSession/LoadSession/ResumeSession 的 `mcp_servers` 桥接到会话 Toolkit；`harness_mcp_bridge_test` 真实 e2e（Node stdio MCP server 发现→桥接→调用）通过
- **缺口核验（codegraph 对账）**：经 codegraph 对账，以下 Tracker 旧条目已滞后于代码——① `McpClientManager` 已在 harness 实现（非待办），核心待办是 **McpTool e2e 接入 ReActAgent**；② `RuntimeContext` 已含 `state`（agentState 绑定）+ `delta_tx` 增量事件通道；③ `StateBackedMemory` 已完整实现 Memory trait；④ enableTools 语义已并入 group 管理。
- **ACP 全套落地（b5e1635 后 10 个提交）**：ed87c57 流式修复 → d736e87 工具调用流增强 → b33e133 文件批量读写 → 6caf10c 嵌入式内存优化（lite feature + 对话压缩 + pwsh EncodedCommand）→ 684839e 取消 → abd29f7 权限确认+沙箱进程树修复 → ba7b73b 上下文 → ccba968 思考等级/动态模型切换/技能双目录 → 321d384 会话配置共享/UsageUpdate/CLI 仪表 → 833f280 日志/请求头/ShellExecute trace
- **嵌入式内存优化**：lite feature（`--no-default-features --features lite` 去掉 rusqlite bundled）+ 对话压缩中间件（conversation_compaction.rs，超触发点重写为 [summary]+preserved_tail）+ 技能/索引惰性加载 + 会话上限淘汰 + grep 正则/glob 相对路径 + Windows pwsh EncodedCommand
- **clippy 基线**：core 16 条既有警告；harness 既有若干；**本轮新增文件 0 条**（已逐项修正 unused import / div_ceil / &Path / redundant closure 等）
- **磁盘注意**：E: 盘紧张（本轮曾出现 link.exe exit 1180/1318）；全量 `cargo test` 前若报 os error 112/链接失败，先 `Remove-Item -Recurse -Force target/debug/incremental`，并可用 `-j 2` 降低并发
- **关键文件**：`MIGRATION_PLAN.md`（方案）、`MIGRATION_GAP_TRACKER.md`（本文件）
- **依赖说明**：`async-openai` v0.41.3、`rust-mcp-sdk` v1.0.1（远程 crates.io）、`rusqlite` v0.31（bundled，lite feature 可去）
- **已验证功能**：async-openai 自定义 base URL 生效（chat_demo 使用 https://opencode.ai/zen/go/v1 成功）；ACP 权限确认/取消/动态模型切换全套 + 对话压缩 + 批量文件读写已通过测试

### 4.2 下一步建议（按优先级）
1. **P0 已完成**：model 层真实 provider 接入（async-openai）已验证（7.3 full）；ACP coding agent 全套（权限确认/取消/流式/动态模型切换）已落地；**B1 docker 沙箱后端（2026-09-17，`sandbox/docker.rs`，docker CLI 实现）**
2. **P0 待办（core 开发体验）**：**`@Tool` proc-macro**——生成 agentscope `FunctionTool`（agent 本地工具宏）；**注意与已引入的 `rust-mcp-sdk` 宏区分**（SDK 宏 `rust-mcp-macros` 生成 `rust_mcp_schema::Tool` 协议声明，非 agent 工具）。当前工具注册全靠手写 `FunctionTool`；新建 agentscope 自己的 proc-macro crate，参考 rust-mcp-macros 模式（属性解析 + type_to_json_schema + quote 生成）
3. **P1 已完成（本轮）**：
   - **McpTool e2e 接入 ReActAgent** ✅（McpClientManager 工具发现 + `bridge_mcp_tools_to_toolkit` → Toolkit；`harness_mcp_bridge_test` 真实 e2e 通过，见 §2.14）
   - **ACP 客户端 MCP 支持** ✅（NewSession/LoadSession/ResumeSession 的 `mcp_servers` → `apply_mcp_servers` → 会话 Toolkit，见 §3.10）
   - ACP 稳定性：`acp_stdio_thought_test` 早前全量跑通过；工作区 `test_all.txt`（15:14）该用例因外部 provider 403 失败，属环境问题，非 flaky
4. **P1 剩余待办**：
   - ~~SandboxLifecycleMiddleware~~ ✅（已存在）+ **docker 后端（本轮完成，见 §3.6）**
   - ~~RemoteFilesystem 真实后端~~ ✅（本轮完成，见 §3.1）
   - ~~MCP SSE/WS 传输待接~~ ✅（SSE + Streamable HTTP 已接，见 §2.14）
   - shutdown 精细化（GracefulShutdownMiddleware / per-request 跟踪 / ctrl_c 信号）
5. **P2 本轮完成**（§3.13 P2 清单，2026-09-17）：M2 coordination、S1–S3/S8 RemoteFilesystem+store+utils+baked、S9/S10 安全扫描+curator 过滤器、B2–B4 snapshot/layout/helpers、G2/G4/G6 gateway、R1–R3 subagent protocol+HTTP client。
6. **P2 剩余（部分）**（2026-09-17 复核，详见 §3.14）：
   - D2 `HarnessAgent.Builder` 自动装配：**2026-09-17 已补 13 项 setter**（agentId/description/checkRunning/enablePendingToolRecovery/enableMetaTool/enableTaskList/useLegacyXmlWorkspaceContext/asyncToolTimeout/environment/taskRepository/~~transcript*~~（已随 `TranscriptMiddleware` 删除，2026-10-21）/teamsMode/disableMemoryTools+disableWebTools+webHttpClient）；剩余 `toolsConfig`+`disableToolsConfig`、`filesystem(Local/Remote/SandboxFilesystemSpec)`、`artifactDeliveryTarget`、`subagentFactory`、`modelResolver`、`externalSubagentTool`、`filesystemRoute`、`mcpServerRegistrationListener`、`registerExternalSchemas`（无对应子系统，见 §3.14 D2）
   - ~~D3 `SkillRuntime` 完整性~~ ✅（2026-09-17，context 快照绑定 + prepare_toolkit 授权边界，见 §3.9）
   - D5 `SubagentDeclaration` 残余：`SubagentSpecGenerator`（LLM spec 生成）缺失、`SubagentDeclaration::new()` 的 `steps` 无默认、无 `load_from_file`
   - D6 `WorkspaceTaskRepository` 残余：仅 `BackgroundTask::when_complete` 未做（Rust 走 `set_done`+Future）；**维护循环、远程轮询、update_awaiting_confirm、trait lifecycle、持久化 version CAS、`TaskRunSpec::Remote` 生产接线均已于 2026-09-17 落地**，见 §3.14 D6
   - ~~Docker start 分支 C 接线~~ ✅（2026-09-17，stop 持久化 + start A/B·C/D 分支，见 §3.6）
7. **P2 增强可选**：
   - Observability 增强（OtelTracingMiddleware）
   - RAG 模块（全新模块）
   - Agent 协作（ObservableAgent observe、SubagentEventBus）
   - AgentStateMemoryView（视图语义）/ LegacyStateLoader（v1→v2 迁移）

> 下一步开工前请先读 `MIGRATION_GAP_TRACKER.md` 4.1/4.2，并确认 `cargo test -p agentscope-harness` 全绿（lib 266 + 集成）。

## 5. 执行状态约定（所有会话必须遵守）
- 开工前：读本文件 + `git status` + 确认 `cargo build`/`cargo test` 基线。
- 改动中：保持每阶段 `cargo build`/`cargo test` 通过（增量可编译验证）。
- 完工后：把对应条目的 `执行状态` 改为 `已完成`，更新 `交接信息`（验证结果），并把 `下一步建议` 归档到 `4.1 上次会话`。
- 涉及 Java 参考时：以 `E:\local-github\agentscope-java\agentscope-harness\src\main\java\io\agentscope\harness` 源码为准（`D:\IdeaProjects\agentscope-java` 为旧路径）。
- 不迁移项（hook/Event v1/LongTermMemory/Tracer 等 Java 已 @Deprecated）一律标注 `设计决定（不迁移）` 而非 `待办`。

## 6. 优先级路线图（Roadmap）
| 里程碑 | 范围 | 建议并行度 |
|---|---|---|
| M1+（当前） | model 层可发 HTTP + agent 生命周期 | 2 个 worker |
| M3+ | tool 中高层 + MCP 接入 | 1-2 个 worker |
| M4/M5 | harness bus/gateway/middleware/sandbox | 2 个 worker |
| M6 | 全量测试对齐、API 打磨 | 1 个 worker |

## 7. 框架可用性验收清单（"基本可用"的最终标准）

> 本节回答"全部 P0/P1 项完成后，框架是否就能开箱即用"。**代码完整 ≠ 可用**——即使上面所有 Java 类都有 Rust 等价物、能编译、能测，仍需以下端到端能力真正"通电"，框架才算可用。
> **原则**：本节各项是**独立的可执行验收**，与前面章节的"逐类对齐"互补。完成前，不要宣称框架"可用"。

### 7.1 端到端可运行 demo（最关键，当前盲区）
| 验收项 | 说明 | 执行状态 | 交接信息 |
|---|---|---|---|
| **example/bin 入口** | 新增 `examples/`（或 `crates/*/examples/`），把 `Model + ReActAgent + Toolkit + 至少一个工具` 串成命令行程序 | ✅ 已完成 | 两个入口已建：`../crates/agentscope-core/examples/chat_demo.rs`（Model 真实 provider 串通，需 key）；`../crates/agentscope-core/examples/tool_agent_demo.rs`（**ReActAgent + Toolkit + add 工具**，脚本化 Model 无需 key）。tool_agent_demo 运行验证输出 `final reply: The sum of 3 and 4 is 7.`，证明 Model→Toolkit→工具→结果回填→最终回复 e2e 链路 ✓ |
| **完整对话跑通** | 命令行发起一次用户消息 → 模型响应（含工具调用 → 工具结果 → 最终回复）全程走通 | 进行中 | 脚本化 Model 版已跑通（tool_agent_demo，无需 key）；真实 provider 版待 key（chat_demo 可跑，缺 key 时用脚本化 Model 兜底） |

### 7.2 凭据 / 配置加载机制（当前盲区）
| 验收项 | 说明 | 执行状态 | 交接信息 |
|---|---|---|---|
| **凭据加载** | `async-openai` 自动读 `OPENAI_API_KEY`/`OPENAI_BASE_URL`；DeepSeek/Kimi 等自定义 provider 用 `OpenAIConfig` 注入 key/base_url | ✅ 已完成 | `OpenAIConfig::new()` 内置 env 读取；`OpenAiChatModel::new/from_context` 可显式注入 key/base_url；credential 纯数据类用于跨 provider 映射 |
| **模型 ID 解析** | 解析 `provider:model` 形式（如 `openai:gpt-4o`）→ 选择 provider + model | ✅ 已完成 | `ModelRegistry::resolve_with`（见 2.2）仍属 core；但 `default_registry()` 自 2026-09-21（`d86a578`）起位于 **`agentscope_extensions_model::openai::default_registry`**，core 不再导出（另有 `responses::default_responses_registry` / `anthropic_async::default_anthropic_registry`） |

### 7.3 真实 provider 打通（用 async-openai，不再手写）
| 验收项 | 说明 | 执行状态 | 交接信息 |
|---|---|---|---|
| **至少一个真实 provider** | 用 `async-openai` 包装实现 `Model::stream`：内部 `chat().create_stream()` 发真实 HTTP/SSE，delta→`ChatResponse`/`ContentBlock`。建议先 **OpenAI**（读 `OPENAI_API_KEY`）或 **Ollama**（本地，改 base_url） | ✅ 已完成 | 接入层已实现（`OpenAiChatModel`，见 2.3），单测 ✅；**live 真实 HTTP/SSE 验证通过**（chat_demo 使用自定义 base URL https://opencode.ai/zen/go/v1 成功发送请求并获得回复） |
| **带回退与错误映射** | async-openai 内置限流指数退避；429/5xx 重试、超时、错误→ `Error::HttpStatus/Model` 映射 | 待办 | 复用 ExecutionConfig（已实现）+ async-openai 内置重试 |

### 7.4 核心能力端到端验证
| 验收项 | 说明 | 执行状态 | 交接信息 |
|---|---|---|---|
| **工具调用 e2e** | 真实 provider 下，模型发起工具调用 → 框架执行 → 结果回填 → 继续 | 待办 | 依赖 7.3；Toolkit 治理层已就绪 |
| **结构化输出 e2e** | 走通 `generate_response` 工具（fallback）或原生 `response_format`（native）任一路径 | 部分 | **native 路径已就绪**：`CallableAgent::call_with_schema` 设 `response_format(json_schema)` + 解析 reply 存结构化元数据（单测过）；fallback（`generate_response` 工具）仍待补 |
| **状态持久化 e2e** | 用 `JsonFileAgentStateStore` 存会话，重启后从磁盘恢复并续接 | 部分 | store.rs 已实现；新增集成测试 `json_file_store_survives_restart` 验证"新 store 实例从磁盘恢复已保存的 AgentState + list + session 列表"闭环（通过）；demo 接线仍待补 |

### 7.5 集成连通性
| 验收项 | 说明 | 执行状态 | 交接信息 |
|---|---|---|---|
| **MCP 连通** | 接一个本地 stdio MCP server，其 tool 能被 ReActAgent 调用 | ✅ 已完成 | `harness_mcp_bridge_test` 拉起本地 Node stdio MCP server → `McpClientManager` 连接发现 → `bridge_mcp_tools_to_toolkit` 注册为 core `McpTool` → 调用回显验证通过；ACP 会话亦经 `apply_mcp_servers` 接入（见 §2.14/§3.10） |
| **harness 端到端** | HarnessAgent + FilesystemTool + 工作区操作跑通一次 | 部分 | harness 有测试（74 个通过），但无真实 model 驱动 e2e |

### 7.6 可用性验收结论判定
- **达成"基本可用"的标志**：7.1 + 7.2 + 7.3（任一 provider）三项全绿，7.4 任一项 e2e 跑通。
- **达成"完全可用"的标志**：7.4 全部 + 7.5 全部。
- **判定方法**：新建 `examples/demo.rs`（或 cargo run），人工/脚本执行并确认输出符合预期；不以"编译通过 + 单测通过"替代 e2e。
- 全部验收项完成后：在本节把对应 `执行状态` 改为 `已完成`，并在 `4.1 上次会话` 记录"框架已基本可用"的验证过程。
