# Java 版（agentscope-java）未关闭 Bug 清单

> 数据来源：GitHub REST API `repo:agentscope-ai/agentscope-java is:issue is:open label:bug`
> 归档时间：2026-11-16（本地 Java 仓库 HEAD `30dc84b59`，分支 `main`）
> 统计口径：**state = open 且带 `bug` 标签的 issue**（不含 PR、不含已关闭 issue）
> 分组规则：每个 issue 按其**第一个 `area/*` 标签**归入唯一主模块，其余 area 标签列入「附注」的交叉标记。

总计：**312** 个未关闭 bug。

## 1. 分布统计

### 1.1 按主模块

| 主模块（主标签） | 未关闭 Bug 数 | 占比 |
| --- | ---: | ---: |
| `area/core/agent` | 70 | 22.4% |
| `area/harness` | 70 | 22.4% |
| `(unlabeled)` | 52 | 16.7% |
| `area/core/tool` | 36 | 11.5% |
| `area/core/memory` | 22 | 7.1% |
| `area/ext/integration` | 16 | 5.1% |
| `area/core/model` | 10 | 3.2% |
| `area/extensions` | 9 | 2.9% |
| `area/core` | 6 | 1.9% |
| `area/ext/memory` | 5 | 1.6% |
| `area/build` | 4 | 1.3% |
| `area/examples` | 4 | 1.3% |
| `area/ext/rag` | 4 | 1.3% |
| `area/docs` | 3 | 1% |
| `area/ext/spring-boot` | 1 | 0.3% |
| **合计** | **312** | **100%** |

### 1.2 按创建月份

| 创建月份 | 数量 |
| --- | ---: |
| 2025-12 | 1 |
| 2026-01 | 6 |
| 2026-02 | 4 |
| 2026-03 | 8 |
| 2026-04 | 19 |
| 2026-05 | 18 |
| 2026-06 | 58 |
| 2026-07 | 64 |
| 2026-08 | 49 |
| 2026-09 | 46 |
| 2026-10 | 27 |
| 2026-11 | 12 |

### 1.3 附加标记

| 附加标记 | 数量 |
| --- | ---: |
| `help wanted` | 5 |
| `good first issue` | 4 |
| `wait-for-response` | 1 |
| `enhancement` | 2 |
| `dependencies` | 2 |

## 2. 未关闭 Bug 清单（按主模块分组）

列说明：`Issue` 为 GitHub issue 编号，标题可直点；`创建 / 更新` = 首报 / 最后一次活动日期；`评论` = 讨论条数。

### 2.1 `area/core/agent` — 70 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3353 | [Malformed streaming tool calls silently degrade to "Tool not found: null" and NPE in parallel mode](https://github.com/agentscope-ai/agentscope-java/issues/3353) | 2026-09-29 | 2026-09-29 | 1 | 交叉: area/core/tool |
| #3320 | [Tool input schema is validated after permission confirmation — users are asked to confirm calls that will fail validation](https://github.com/agentscope-ai/agentscope-java/issues/3320) | 2026-09-27 | 2026-09-27 | 2 | 交叉: area/core/tool |
| #3315 | [The message of an ASK PermissionDecision is dropped and never reaches the confirmation (RequireUserConfirmEvent / AG-UI interrupt)](https://github.com/agentscope-ai/agentscope-java/issues/3315) | 2026-09-27 | 2026-09-30 | 4 | 交叉: area/ext/integration |
| #3308 | [Sub-agents inherit the parent's client-declared frontend tools; calling one silently loses the sub-task](https://github.com/agentscope-ai/agentscope-java/issues/3308) | 2026-09-26 | 2026-09-29 | 3 | 交叉: area/harness |
| #3294 | [User messages sent together with a permission-HITL resume are silently dropped](https://github.com/agentscope-ai/agentscope-java/issues/3294) | 2026-09-25 | 2026-09-28 | 5 |  |
| #3135 | [ASKING 状态会被持久化到状态存储中](https://github.com/agentscope-ai/agentscope-java/issues/3135) | 2026-09-14 | 2026-11-05 | 5 | 交叉: area/harness |
| #3102 | [ReActAgent 调用不存在的工具时直接退出 ReAct 循环，未给模型重试机会](https://github.com/agentscope-ai/agentscope-java/issues/3102) | 2026-09-11 | 2026-09-14 | 3 | 交叉: area/core/model |
| #3065 | [Bug: ToolEmitter progress can be delivered to the wrong concurrent request](https://github.com/agentscope-ai/agentscope-java/issues/3065) | 2026-09-09 | 2026-10-03 | 4 | 交叉: area/core/tool |
| #3043 | [: Message: Invalid tool result IDs: [call_01_w7wlz4ch9Hiw4Um74zwv8117]. Expected: [call_00_OugZmGqpB71ykFO5IjxM8342]](https://github.com/agentscope-ai/agentscope-java/issues/3043) | 2026-09-08 | 2026-10-23 | 3 | 交叉: area/ext/integration |
| #2915 | [AgentSpawnTool retains every non-persistent spawned Agent indefinitely](https://github.com/agentscope-ai/agentscope-java/issues/2915) | 2026-08-31 | 2026-10-01 | 3 | 交叉: area/harness |
| #2870 | [ACCEPT_EDITS ignores working directories — in-scope edit tools always ask](https://github.com/agentscope-ai/agentscope-java/issues/2870) | 2026-08-27 | 2026-10-22 | 2 |  |
| #2732 | [DockerSandbox：uploadFiles 把 base64 文件内容拼进 docker exec 的单个 argv，大文件必报「参数列表过长」(error=7)，导致 ToolResultEvictionMiddleware 静默失效](https://github.com/agentscope-ai/agentscope-java/issues/2732) | 2026-08-16 | 2026-08-18 | 1 |  |
| #2702 | [onError 路径（超时/LLM 异常）下 AgentState 也不持久化，#2461 是否可一并覆盖？](https://github.com/agentscope-ai/agentscope-java/issues/2702) | 2026-08-14 | 2026-11-12 | 3 |  |
| #2681 | [agentscope.agent.event.emitter   和 AsyncToolMiddleware](https://github.com/agentscope-ai/agentscope-java/issues/2681) | 2026-08-13 | 2026-09-27 | 2 | 交叉: area/core/tool |
| #2672 | [middleware 工具重写失败](https://github.com/agentscope-ai/agentscope-java/issues/2672) | 2026-08-12 | 2026-08-13 | 2 |  |
| #2652 | [onSystemPrompt middleware executes on a Reactor boundedElastic scheduler thread with an uncontrollable TCCL, breaking TCCL-based resource loading (FreeMarker / hutool SPI) in Spring Boot fat-jar deployments](https://github.com/agentscope-ai/agentscope-java/issues/2652) | 2026-08-10 | 2026-08-11 | 2 | 交叉: area/ext/spring-boot |
| #2641 | [ModelCallEndEvent in Middleware lacks complete response info (status_code) for sensitive content filtering](https://github.com/agentscope-ai/agentscope-java/issues/2641) | 2026-08-10 | 2026-08-11 | 0 | 交叉: area/core/model |
| #2610 | [AgentScope HarnessAgent interrupt() API 设计缺陷](https://github.com/agentscope-ai/agentscope-java/issues/2610) | 2026-08-07 | 2026-09-02 | 1 | 交叉: area/core/memory, area/build, area/harness |
| #2570 | [模型返回的内容没结构化会工具的调用](https://github.com/agentscope-ai/agentscope-java/issues/2570) | 2026-08-05 | 2026-08-11 | 0 | 交叉: area/examples |
| #2539 | [enablePendingToolRecovery mutates the shared ReActAgent.Builder.hooks on every build(), racing under concurrent A2A requests (AIOOBE / NPE during ReActAgent construction)](https://github.com/agentscope-ai/agentscope-java/issues/2539) | 2026-08-03 | 2026-08-11 | 4 | 交叉: area/ext/integration, area/ext/spring-boot, area/build |
| #2536 | [配置 AgentStateStore 时 stateCache 只写不读却无界驻留，建议提供失效策略或不填充](https://github.com/agentscope-ai/agentscope-java/issues/2536) | 2026-08-03 | 2026-08-11 | 2 | 交叉: area/docs |
| #2535 | [setPermissionMode 读取未回源的进程内 stateCache 并整体写回，多副本下会静默覆盖更新的 AgentState](https://github.com/agentscope-ai/agentscope-java/issues/2535) | 2026-08-03 | 2026-08-11 | 0 | 交叉: area/core/memory, area/harness |
| #2506 | [WakeupDispatcher 可能会丢弃子智能体的透传消息](https://github.com/agentscope-ai/agentscope-java/issues/2506) | 2026-07-31 | 2026-08-11 | 1 |  |
| #2486 | [Per-path lock maps grow without bound — no eviction in SkillBox, WorkspaceManager and LocalFilesystem](https://github.com/agentscope-ai/agentscope-java/issues/2486) | 2026-07-29 | 2026-08-11 | 2 | 交叉: area/core/tool, area/harness |
| #2478 | [Streaming retry re-subscribes the model and duplicates already-delivered chunks](https://github.com/agentscope-ai/agentscope-java/issues/2478) | 2026-07-29 | 2026-08-11 | 1 | 交叉: area/core/memory |
| #2446 | [edit_file hangs indefinitely when old_string is empty](https://github.com/agentscope-ai/agentscope-java/issues/2446) | 2026-07-28 | 2026-08-11 | 3 |  |
| #2420 | [skill热加载问题](https://github.com/agentscope-ai/agentscope-java/issues/2420) | 2026-07-27 | 2026-08-11 | 2 | 交叉: area/core/tool, area/harness |
| #2419 | [skill热加载问题](https://github.com/agentscope-ai/agentscope-java/issues/2419) | 2026-07-27 | 2026-08-11 | 1 | 交叉: area/core/tool, area/harness |
| #2408 | [Cancelling a parent run does not stop a sync-spawned HarnessAgent subagent (interruptAgent no-op + wrong ctx)](https://github.com/agentscope-ai/agentscope-java/issues/2408) | 2026-07-27 | 2026-11-12 | 3 | 交叉: area/core/tool, area/harness |
| #2400 | [AnthropicChatModel 将 ThinkingBlock 作为普通 text 传回 API，导致 400 错误](https://github.com/agentscope-ai/agentscope-java/issues/2400) | 2026-07-27 | 2026-08-11 | 2 | 交叉: area/core/model, area/core/memory, area/build; 标记: help wanted |
| #2362 | [缺少「终止工具」的干净实现：onActing 停止会产生孤儿 tool_result，onModelCall/onReasoning 停止会丢失已落库记忆](https://github.com/agentscope-ai/agentscope-java/issues/2362) | 2026-07-22 | 2026-09-22 | 1 | 交叉: area/core/memory, area/core/tool, area/docs, area/build |
| #2334 | [[Feature]: 希望写入记忆能异步进行](https://github.com/agentscope-ai/agentscope-java/issues/2334) | 2026-07-21 | 2026-08-11 | 1 | 交叉: area/core/memory, area/harness; 标记: enhancement |
| #2324 | [harness stop earler 智能体streamEvent过早结束](https://github.com/agentscope-ai/agentscope-java/issues/2324) | 2026-07-20 | 2026-08-11 | 1 | 交叉: area/harness |
| #2314 | [[2.0.0] HarnessAgent 并发调用时 WorkspaceSkillRepository 可能串用其他会话的 RuntimeContext](https://github.com/agentscope-ai/agentscope-java/issues/2314) | 2026-07-20 | 2026-08-11 | 2 | 交叉: area/core/memory, area/core/tool, area/extensions, area/harness |
| #2303 | [RemoteSandboxSnapshot loses RemoteSnapshotClient after JSON deserialization, only DockerSandboxClient handles the re-binding](https://github.com/agentscope-ai/agentscope-java/issues/2303) | 2026-07-19 | 2026-11-11 | 5 |  |
| #2279 | [Flux.create sink.complete() 延迟 30+ 秒才传播到下游](https://github.com/agentscope-ai/agentscope-java/issues/2279) | 2026-07-18 | 2026-09-09 | 3 |  |
| #2261 | [ReActAgent.stateCache grows unboundedly — no eviction mechanism for long-running singleton HarnessAgent](https://github.com/agentscope-ai/agentscope-java/issues/2261) | 2026-07-17 | 2026-07-27 | 1 |  |
| #2200 | [Tool groups activated at build time are lost on first call for new sessions](https://github.com/agentscope-ai/agentscope-java/issues/2200) | 2026-07-14 | 2026-09-27 | 7 | 交叉: area/core/tool |
| #2191 | [无法实现ClaudeCode的AI思考过程中的纠偏机制，ClaudeCode是中断新建Turn机制；AgentScope2.0.0 中断恢复（interrupt resume）遗留 orphan ToolUseBlock，导致 OpenAI/Anthropic 请求 400](https://github.com/agentscope-ai/agentscope-java/issues/2191) | 2026-07-14 | 2026-07-15 | 2 |  |
| #2074 | [coordinator 模式下无法 cancel subagents](https://github.com/agentscope-ai/agentscope-java/issues/2074) | 2026-07-09 | 2026-07-11 | 2 |  |
| #1997 | [streamEvents cancellation does not guarantee AgentState checkpoint for resumable sessions](https://github.com/agentscope-ai/agentscope-java/issues/1997) | 2026-07-03 | 2026-07-11 | 0 |  |
| #1953 | [harnessAgent编排:业务Tool被HarnessAgent跳过子agent直接调用工具，设置工具组父不可见子可用无效](https://github.com/agentscope-ai/agentscope-java/issues/1953) | 2026-06-28 | 2026-07-11 | 0 |  |
| #1948 | [io.agentscope.core.event.AgentEvent Deserialize error](https://github.com/agentscope-ai/agentscope-java/issues/1948) | 2026-06-27 | 2026-07-11 | 0 |  |
| #1941 | [ReActAgent.call() does not trigger onAgent middleware hook](https://github.com/agentscope-ai/agentscope-java/issues/1941) | 2026-06-26 | 2026-07-11 | 1 |  |
| #1910 | [Plan Mode 的“白名单机制”和“权限确认机制”没有联动](https://github.com/agentscope-ai/agentscope-java/issues/1910) | 2026-06-25 | 2026-11-04 | 3 |  |
| #1779 | [Plan 完成态 hint 在最后一步与 finish_plan 同轮调用时无法触达](https://github.com/agentscope-ai/agentscope-java/issues/1779) | 2026-06-16 | 2026-06-17 | 1 |  |
| #1741 | [关闭不了sse。](https://github.com/agentscope-ai/agentscope-java/issues/1741) | 2026-06-13 | 2026-06-13 | 0 |  |
| #1715 | [ReactAgent模式下skill无法被激活，HarnessAgent模式下skill绑定的工具无法被激活](https://github.com/agentscope-ai/agentscope-java/issues/1715) | 2026-06-11 | 2026-06-12 | 0 | 交叉: area/core/tool |
| #1695 | [ToolResultEvictionMiddleware does not evict](https://github.com/agentscope-ai/agentscope-java/issues/1695) | 2026-06-10 | 2026-10-03 | 1 |  |
| #1681 | [streamEvents() 在绑定 RuntimeContext 之前就装配了 onAgent 中间件链 —— 导致长期记忆与会话落盘写入匿名 _default 命名空间](https://github.com/agentscope-ai/agentscope-java/issues/1681) | 2026-06-09 | 2026-06-09 | 0 | 交叉: area/harness |
| #1675 | [v2版本：call() 方法确实不执行 Middleware,v1的钩子系统，v2切换到Middleware发现没生效](https://github.com/agentscope-ai/agentscope-java/issues/1675) | 2026-06-08 | 2026-06-09 | 0 |  |
| #1672 | [在MiddlewareBase的onAgent中,agent参数没有getRuntimeContext](https://github.com/agentscope-ai/agentscope-java/issues/1672) | 2026-06-08 | 2026-06-08 | 4 |  |
| #1623 | [MEMORY.md 创建为default](https://github.com/agentscope-ai/agentscope-java/issues/1623) | 2026-06-05 | 2026-06-05 | 0 | 交叉: area/core/memory |
| #1606 | [ReActAgent.Builder.build() toolkit deep copy causes dynamically-registered middleware tools to be missing from the agent's actual toolkit](https://github.com/agentscope-ai/agentscope-java/issues/1606) | 2026-06-04 | 2026-06-11 | 6 | 交叉: area/harness |
| #1556 | [ToolExecutor异常逃逸与PendingToolRecoveryHook失效问题](https://github.com/agentscope-ai/agentscope-java/issues/1556) | 2026-05-31 | 2026-06-01 | 0 | 交叉: area/core/tool |
| #1555 | [PendingToolRecoveryHook always skips recovery for multi-turn ReActAgent (Supervisor) due to false positive userProvidedResults check](https://github.com/agentscope-ai/agentscope-java/issues/1555) | 2026-05-31 | 2026-06-01 | 2 |  |
| #1446 | [Qwen3.6-27B响应stop_reason=248044导致ReActAgent假死，不能未正确结束](https://github.com/agentscope-ai/agentscope-java/issues/1446) | 2026-05-20 | 2026-05-28 | 0 |  |
| #1403 | [Hook message extraction conflict when using long-term memory and RAG together](https://github.com/agentscope-ai/agentscope-java/issues/1403) | 2026-05-15 | 2026-05-28 | 4 |  |
| #1265 | [Annotation in PendingToolRecoveryHook wrong](https://github.com/agentscope-ai/agentscope-java/issues/1265) | 2026-04-22 | 2026-05-28 | 2 |  |
| #1217 | [AgentScope Java 的 ReActAgent 框架未按LLM提示词返回要求输出](https://github.com/agentscope-ai/agentscope-java/issues/1217) | 2026-04-14 | 2026-05-28 | 0 |  |
| #1211 | [io.agentscope.core.agent.StreamOptions.Builder#incremental（累计模式）在 tool call 后无法正确累加内容](https://github.com/agentscope-ai/agentscope-java/issues/1211) | 2026-04-13 | 2026-05-28 | 2 |  |
| #1203 | [updateToolGroups() does not take effect on existing ReActAgent instance](https://github.com/agentscope-ai/agentscope-java/issues/1203) | 2026-04-12 | 2026-05-28 | 4 |  |
| #1117 | [block()/blockFirst()/blockLast() are blocking, which is not supported in thread reactor-http-nio-3](https://github.com/agentscope-ai/agentscope-java/issues/1117) | 2026-04-03 | 2026-05-28 | 2 |  |
| #1036 | [interrupt don't work](https://github.com/agentscope-ai/agentscope-java/issues/1036) | 2026-03-25 | 2026-05-28 | 0 |  |
| #938 | [主智能体不会订阅子智能体的流式事件，导致主智能体输出完毕了，子智能体还在流式输出，即主智能体的回答没有使用子智能体的流式内容](https://github.com/agentscope-ai/agentscope-java/issues/938) | 2026-03-12 | 2026-08-24 | 4 |  |
| #918 | [agentscope agent as tools模式下流式场景报错：Cannot add messages without tool results when pending tool calls exist.](https://github.com/agentscope-ai/agentscope-java/issues/918) | 2026-03-11 | 2026-08-24 | 9 |  |
| #846 | [ReActAgent 在流式输出（stream）时存在严重的文本块内容累积重复问题](https://github.com/agentscope-ai/agentscope-java/issues/846) | 2026-03-01 | 2026-05-28 | 7 |  |
| #680 | [Agent As Tool + A2A形式调用远程子智能体时，Msg中的metadata信息丢失。](https://github.com/agentscope-ai/agentscope-java/issues/680) | 2026-01-29 | 2026-08-24 | 6 | 交叉: area/ext/integration |
| #618 | [planNoteBook在finish_plan后没有按预期的输出总结](https://github.com/agentscope-ai/agentscope-java/issues/618) | 2026-01-21 | 2026-06-02 | 1 |  |
| #459 | [SubAgent registered as Tool fails when inner Tool call is blocked by Human-in-the-Loop](https://github.com/agentscope-ai/agentscope-java/issues/459) | 2026-01-06 | 2026-05-28 | 4 |  |

### 2.2 `area/harness` — 70 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3421 | [Team wakeups cross namespace boundaries for same-named members](https://github.com/agentscope-ai/agentscope-java/issues/3421) | 2026-10-05 | 2026-10-15 | 1 |  |
| #3415 | [HarnessAgent带沙箱运行，结束后会报错](https://github.com/agentscope-ai/agentscope-java/issues/3415) | 2026-10-04 | 2026-10-04 | 1 |  |
| #3388 | [In sandbox mode, shell writes to AGENTS.md / skills/ / MEMORY.md override host definitions and persist across calls](https://github.com/agentscope-ai/agentscope-java/issues/3388) | 2026-10-02 | 2026-10-04 | 3 |  |
| #3378 | [Enumeration surfaces: relative and ..-bearing paths bypass the namespace anchor (sibling-tenant access)](https://github.com/agentscope-ai/agentscope-java/issues/3378) | 2026-10-01 | 2026-10-01 | 2 |  |
| #3266 | [memory_search returns unbounded results — no maxResults, no per-line truncation, while excluded from tool-result eviction](https://github.com/agentscope-ai/agentscope-java/issues/3266) | 2026-09-23 | 2026-09-23 | 2 |  |
| #3258 | [Remote-spec UNRESTRICTED default backend passes arbitrary absolute paths through (read_file("/etc/passwd") reaches the host)](https://github.com/agentscope-ai/agentscope-java/issues/3258) | 2026-09-22 | 2026-09-26 | 3 |  |
| #3253 | [CompositeFilesystem root ls/grep escape the workspace under the UNRESTRICTED default backend (list_files(".") enumerates the drive root)](https://github.com/agentscope-ai/agentscope-java/issues/3253) | 2026-09-22 | 2026-09-23 | 4 |  |
| #3197 | [Redis 模式下 glob 和 ls 操作被本地索引误导，无法正确发现 Redis 中实际存储的记忆文件。](https://github.com/agentscope-ai/agentscope-java/issues/3197) | 2026-09-18 | 2026-11-12 | 5 |  |
| #3178 | [Declared/general-purpose subagents never inherit MCP tools (inherited toolkit captured from b.toolkit before MCP registration)](https://github.com/agentscope-ai/agentscope-java/issues/3178) | 2026-09-17 | 2026-09-20 | 9 |  |
| #3171 | [HarnessAgent.streamEvents() does not recover from context overflow while call() does](https://github.com/agentscope-ai/agentscope-java/issues/3171) | 2026-09-16 | 2026-09-16 | 1 |  |
| #3088 | [HarnessAgent memory timezone error](https://github.com/agentscope-ai/agentscope-java/issues/3088) | 2026-09-10 | 2026-09-12 | 3 |  |
| #3059 | [Memory flush / consolidation / transcript block completion of the agent event stream (concatWith instead of fire-and-forget), contradicting the documentation](https://github.com/agentscope-ai/agentscope-java/issues/3059) | 2026-09-09 | 2026-09-12 | 4 |  |
| #3042 | [Cross-backend CompositeFilesystem.move rewrites file bytes and deletes the source](https://github.com/agentscope-ai/agentscope-java/issues/3042) | 2026-09-08 | 2026-09-15 | 2 |  |
| #2985 | [上下文压缩失效导致agent运行失败](https://github.com/agentscope-ai/agentscope-java/issues/2985) | 2026-09-04 | 2026-09-13 | 1 |  |
| #2982 | [2.0.0版本HarnessAgent配置了MemoryConfig.FlushTrigger和consolidationMinGap，但是第一次运行时总会触发consolidation](https://github.com/agentscope-ai/agentscope-java/issues/2982) | 2026-09-04 | 2026-11-12 | 2 |  |
| #2918 | [当前 main 默认 ObjectStoreTranscriptStore 的 segment key 不匹配 RemoteFilesystemSpec 路由，session 历史无法写入远程/MySQL](https://github.com/agentscope-ai/agentscope-java/issues/2918) | 2026-08-31 | 2026-10-01 | 5 |  |
| #2849 | [`SandboxConfigurationException: No active sandbox` with concurrent sessions on a shared agent — the per-call binding fix (#2490) is partial; the legacy `volatile sandbox` fallback still races](https://github.com/agentscope-ai/agentscope-java/issues/2849) | 2026-08-26 | 2026-09-14 | 1 |  |
| #2845 | [Same-isolation-key concurrent calls race on sandbox state slot](https://github.com/agentscope-ai/agentscope-java/issues/2845) | 2026-08-26 | 2026-09-24 | 1 |  |
| #2792 | [Subagent completion notification lacks global progress; SubagentsMiddleware.wireMessageBus is unreachable code](https://github.com/agentscope-ai/agentscope-java/issues/2792) | 2026-08-20 | 2026-10-26 | 3 |  |
| #2791 | [WaitAsyncResultsTool waits on the inbox notification chain instead of task state, stalling the main agent when any link breaks](https://github.com/agentscope-ai/agentscope-java/issues/2791) | 2026-08-20 | 2026-09-25 | 1 |  |
| #2758 | [k8s沙箱WorkspaceManager.listKnowledgeFiles报错](https://github.com/agentscope-ai/agentscope-java/issues/2758) | 2026-08-18 | 2026-08-26 | 3 |  |
| #2688 | [WorkspaceSkillRepository contextSupplier有并发的bug](https://github.com/agentscope-ai/agentscope-java/issues/2688) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2686 | [subAgent](https://github.com/agentscope-ai/agentscope-java/issues/2686) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2683 | [<agent-id>/knowledge文件夹下的文件读取不到](https://github.com/agentscope-ai/agentscope-java/issues/2683) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2679 | [用户隔离下的skill使用bug。](https://github.com/agentscope-ai/agentscope-java/issues/2679) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2664 | [`.skills-cache` is never cleaned up when skill repositories return an empty collection; HarnessAgent still reports installed skills](https://github.com/agentscope-ai/agentscope-java/issues/2664) | 2026-08-11 | 2026-08-17 | 4 |  |
| #2644 | [harnessAgent sanbox shell tool prompt not inconsistent of system prompt](https://github.com/agentscope-ai/agentscope-java/issues/2644) | 2026-08-10 | 2026-08-12 | 1 |  |
| #2581 | [SSE连接中断(类似于浏览器刷新造成中断)，走不到 agent_state 持久化](https://github.com/agentscope-ai/agentscope-java/issues/2581) | 2026-08-06 | 2026-08-11 | 4 |  |
| #2448 | [OverlayFilesystem.grep keeps stale lower-layer matches after upper edits](https://github.com/agentscope-ai/agentscope-java/issues/2448) | 2026-07-28 | 2026-08-11 | 3 |  |
| #2298 | [HarnessAgent的agentName 参数 影响远程问价系统的session卸载](https://github.com/agentscope-ai/agentscope-java/issues/2298) | 2026-07-19 | 2026-07-29 | 2 |  |
| #2271 | [window系统下执行任何命令工具 execute 总是出错](https://github.com/agentscope-ai/agentscope-java/issues/2271) | 2026-07-17 | 2026-07-29 | 1 | 标记: good first issue |
| #2266 | [AI创建的文件实际路径和返回给用户看到的不一致](https://github.com/agentscope-ai/agentscope-java/issues/2266) | 2026-07-17 | 2026-07-29 | 2 |  |
| #2251 | [LocalFilesystemPersonalAssistantExampleTest flaky JUnit @TempDir cleanup race with session-tree-mirror thread](https://github.com/agentscope-ai/agentscope-java/issues/2251) | 2026-07-16 | 2026-07-19 | 1 |  |
| #2216 | [AgentRun tar extract failed: tar: /home/agentscope/workspace: Cannot open: No such file or directory](https://github.com/agentscope-ai/agentscope-java/issues/2216) | 2026-07-15 | 2026-07-16 | 1 |  |
| #2182 | [使用DistributedStore和filesystem仍然本地会创建.agentscope/workspace/.index/workerspace.db和alice文件](https://github.com/agentscope-ai/agentscope-java/issues/2182) | 2026-07-13 | 2026-09-11 | 1 |  |
| #2181 | [MarketplaceStager filesystem 抽象层  工作区下公共技能无法使用](https://github.com/agentscope-ai/agentscope-java/issues/2181) | 2026-07-13 | 2026-07-29 | 1 |  |
| #2162 | [Background task cancellation lacks execution-stop acknowledgement](https://github.com/agentscope-ai/agentscope-java/issues/2162) | 2026-07-12 | 2026-07-29 | 1 |  |
| #2159 | [Dynamic subagent notifications drop FAILED/CANCELLED payloads](https://github.com/agentscope-ai/agentscope-java/issues/2159) | 2026-07-12 | 2026-07-29 | 1 |  |
| #2147 | [Harness close can return before SessionTree mirror finishes using distributed store](https://github.com/agentscope-ai/agentscope-java/issues/2147) | 2026-07-12 | 2026-07-29 | 2 |  |
| #2146 | [Distributed replicas can lose concurrent turns for the same agent session](https://github.com/agentscope-ai/agentscope-java/issues/2146) | 2026-07-12 | 2026-07-29 | 2 |  |
| #2145 | [ToolResultEvictionMiddleware leaves full tool output in Redis AgentState](https://github.com/agentscope-ai/agentscope-java/issues/2145) | 2026-07-12 | 2026-07-29 | 3 |  |
| #2143 | [PostgresBaseStore UPSERT_SQL has an extra comma and fails updates](https://github.com/agentscope-ai/agentscope-java/issues/2143) | 2026-07-11 | 2026-07-29 | 3 |  |
| #2135 | [DockerSandbox corrupts complex sh -c commands on Windows](https://github.com/agentscope-ai/agentscope-java/issues/2135) | 2026-07-11 | 2026-07-29 | 3 |  |
| #1985 | [agentscope-builder模块本地运行skills加载报错](https://github.com/agentscope-ai/agentscope-java/issues/1985) | 2026-07-02 | 2026-07-11 | 0 |  |
| #1772 | [沙盒执行命令的问题](https://github.com/agentscope-ai/agentscope-java/issues/1772) | 2026-06-16 | 2026-06-16 | 0 |  |
| #1769 | [SessionTree.flush + offloadToSessionTree 导致 session JSONL 文件内容重复膨胀](https://github.com/agentscope-ai/agentscope-java/issues/1769) | 2026-06-15 | 2026-07-13 | 3 |  |
| #1729 | [](https://github.com/agentscope-ai/agentscope-java/issues/1729) | 2026-06-12 | 2026-06-13 | 0 |  |
| #1702 | [In sandbox mode, the tool cannot obtain type-safe custom context types.](https://github.com/agentscope-ai/agentscope-java/issues/1702) | 2026-06-10 | 2026-06-11 | 0 |  |
| #1700 | [HarnessGateway.runSubagentStream 未向暴露的子 Agent 传递 RuntimeContext / MsgContext](https://github.com/agentscope-ai/agentscope-java/issues/1700) | 2026-06-10 | 2026-06-11 | 0 |  |
| #1683 | [HarnessAgent does not forward per-session interrupt methods from ReActAgent](https://github.com/agentscope-ai/agentscope-java/issues/1683) | 2026-06-10 | 2026-06-29 | 1 |  |
| #1650 | [Never use workspace file](https://github.com/agentscope-ai/agentscope-java/issues/1650) | 2026-06-06 | 2026-06-07 | 1 |  |
| #1644 | [streamEvents() throws NPE when generateOptions is not set on HarnessAgent](https://github.com/agentscope-ai/agentscope-java/issues/1644) | 2026-06-06 | 2026-06-07 | 0 |  |
| #1635 | [为什么harness-agent输出的文件会多一层userid的目录](https://github.com/agentscope-ai/agentscope-java/issues/1635) | 2026-06-05 | 2026-06-05 | 0 |  |
| #1632 | [agentscope-dataagent — per-user workspace not persisted single-node (UserSandboxRegistry hardcodes NoopSnapshotSpec, bypasses snapshot persist/restore)](https://github.com/agentscope-ai/agentscope-java/issues/1632) | 2026-06-05 | 2026-06-05 | 1 |  |
| #1626 | [agentscope-dataagent — session REST endpoints read the sandbox-backed workspace outside a call context (500 "No active sandbox")](https://github.com/agentscope-ai/agentscope-java/issues/1626) | 2026-06-05 | 2026-06-05 | 0 |  |
| #1619 | [同一个HarnessAgent，不同的sessionId，不同的userId，上下文能互相访问到](https://github.com/agentscope-ai/agentscope-java/issues/1619) | 2026-06-05 | 2026-06-10 | 9 |  |
| #1609 | [BaseSandboxFilesystem.edit() generates broken python3 -c command with literal \n](https://github.com/agentscope-ai/agentscope-java/issues/1609) | 2026-06-04 | 2026-06-05 | 0 |  |
| #1605 | [abstractFilesystem() with user-registered SandboxLifecycleMiddleware should participate in wrappedCall lifecycle](https://github.com/agentscope-ai/agentscope-java/issues/1605) | 2026-06-04 | 2026-06-05 | 0 |  |
| #1602 | [HarnessAgent Skill加载工具load_skill_through_path提示Error: Tool not found: load_skill_through_path](https://github.com/agentscope-ai/agentscope-java/issues/1602) | 2026-06-04 | 2026-06-10 | 6 |  |
| #1587 | [AgentScope-Java V2 版本，workspace中存在的文件，但是read_file 工具读取不到](https://github.com/agentscope-ai/agentscope-java/issues/1587) | 2026-06-03 | 2026-06-04 | 0 |  |
| #1584 | [Failed to deserialize Docker sandbox state](https://github.com/agentscope-ai/agentscope-java/issues/1584) | 2026-06-03 | 2026-06-05 | 1 |  |
| #1578 | [声明的子 Agent 不会继承父 Agent hooks](https://github.com/agentscope-ai/agentscope-java/issues/1578) | 2026-06-03 | 2026-06-04 | 0 |  |
| #1577 | [主agent停止子agent不停](https://github.com/agentscope-ai/agentscope-java/issues/1577) | 2026-06-03 | 2026-06-04 | 0 |  |
| #1560 | [[harness][sandbox] SandboxBackedFilesystem.uploadFiles fails with CreateProcess error=206 / E2BIG because file content is base64-embedded into the docker exec command line](https://github.com/agentscope-ai/agentscope-java/issues/1560) | 2026-06-01 | 2026-08-02 | 2 |  |
| #1458 | [LocalFilesystem.glob 使用 **/*.md 无法匹配目录直接子文件，导致 MemoryConsolidator consolidation 失效](https://github.com/agentscope-ai/agentscope-java/issues/1458) | 2026-05-21 | 2026-05-28 | 4 |  |
| #1453 | [How can I get HarnessAgent's subAgent stream event content?](https://github.com/agentscope-ai/agentscope-java/issues/1453) | 2026-05-21 | 2026-05-28 | 0 |  |
| #1451 | [HarnessAgent subagent task_output may be interrupted by default tool execution timeout](https://github.com/agentscope-ai/agentscope-java/issues/1451) | 2026-05-20 | 2026-05-28 | 0 |  |
| #1441 | [Description (Memory fails to load due to inconsistent Workspace paths)](https://github.com/agentscope-ai/agentscope-java/issues/1441) | 2026-05-19 | 2026-05-28 | 1 |  |
| #1440 | [当前harnessagent 对话过程中，调用harnessagent.interrupt 方法后，会话记忆没有保存](https://github.com/agentscope-ai/agentscope-java/issues/1440) | 2026-05-19 | 2026-05-28 | 0 |  |
| #1396 | [Command Execution Fails on Windows with "Cannot run program 'sh'" Error](https://github.com/agentscope-ai/agentscope-java/issues/1396) | 2026-05-15 | 2026-11-11 | 8 | 标记: good first issue, help wanted |

### 2.3 `(unlabeled)` — 52 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #4207 | [SubAgentTool derives the same tool name for agent names that differ only by case or spacing — registration fails with a duplicate-tool error](https://github.com/agentscope-ai/agentscope-java/issues/4207) | 2026-11-16 | 2026-11-16 | 0 |  |
| #4205 | [ModelContextWindows.lookup folds model names with the default locale — upper-case "I" breaks lookup under tr-TR](https://github.com/agentscope-ai/agentscope-java/issues/4205) | 2026-11-15 | 2026-11-15 | 0 |  |
| #4203 | [TruncateArgsConfig clips tool arguments to a hardcoded 20 chars instead of maxArgLength](https://github.com/agentscope-ai/agentscope-java/issues/4203) | 2026-11-15 | 2026-11-15 | 1 |  |
| #4140 | [CompositeFilesystem.sessionStorage() 硬编码 routeForPath("agents/")，Native Session Log 永远写不进 RemoteFilesystemSpec 的 sessions 路由](https://github.com/agentscope-ai/agentscope-java/issues/4140) | 2026-11-12 | 2026-11-15 | 3 |  |
| #4104 | [Returning one external tool result redispatches suspended sibling calls](https://github.com/agentscope-ai/agentscope-java/issues/4104) | 2026-11-10 | 2026-11-15 | 2 |  |
| #3975 | [WorkspaceAsyncToolRegistry by-id terminal updates silently no-op (double-slash record path from ls trailing slashes)](https://github.com/agentscope-ai/agentscope-java/issues/3975) | 2026-11-04 | 2026-11-15 | 1 |  |
| #3973 | [InboxMiddleware.onReasoning error-handling gaps: fire-and-forget markTimeout, Mono.zip fail-fast, null-payload NPE](https://github.com/agentscope-ai/agentscope-java/issues/3973) | 2026-11-04 | 2026-11-05 | 1 |  |
| #3972 | [InboxMiddleware stale async-tool hint misreports still-running long tasks as "process crash or timeout"](https://github.com/agentscope-ai/agentscope-java/issues/3972) | 2026-11-04 | 2026-11-04 | 0 |  |
| #3971 | [InboxMiddleware: drained inbox hints are silently lost when agent state cannot be resolved](https://github.com/agentscope-ai/agentscope-java/issues/3971) | 2026-11-04 | 2026-11-15 | 2 |  |
| #3968 | [A concurrent sandbox call can fail during inbox checks after another session finishes](https://github.com/agentscope-ai/agentscope-java/issues/3968) | 2026-11-04 | 2026-11-04 | 0 |  |
| #3966 | [沙箱文件系统下，读取文件内容为base64出现含\n](https://github.com/agentscope-ai/agentscope-java/issues/3966) | 2026-11-04 | 2026-11-15 | 1 |  |
| #3937 | [在 `release/2.1.0-BETA1` 上，只要托管会话的 owner id 含冒号（控制台创建的 Agent 默认就是这种），](https://github.com/agentscope-ai/agentscope-java/issues/3937) | 2026-11-02 | 2026-11-02 | 0 |  |
| #3891 | [agentservice的chat界面，无法展示返回的消息](https://github.com/agentscope-ai/agentscope-java/issues/3891) | 2026-10-31 | 2026-11-01 | 1 |  |
| #3866 | [agentscope2.0.2和spring ai2.0.4在一个项目中都实现结构化输出的场景，底层依赖包不兼容](https://github.com/agentscope-ai/agentscope-java/issues/3866) | 2026-10-30 | 2026-10-30 | 0 |  |
| #3816 | [会话 journal 重放与体积放大](https://github.com/agentscope-ai/agentscope-java/issues/3816) | 2026-10-27 | 2026-10-28 | 4 |  |
| #3815 | [Cancelling native session flush interrupts storage I/O; cancellation-safe flush exposes initialization writer cleanup gap](https://github.com/agentscope-ai/agentscope-java/issues/3815) | 2026-10-27 | 2026-11-12 | 4 |  |
| #3780 | [Harness compaction gaps: v2.0.4 streaming overflow recovery and custom flushPrompt propagation](https://github.com/agentscope-ai/agentscope-java/issues/3780) | 2026-10-26 | 2026-11-12 | 1 |  |
| #3768 | [SessionBridge.sessions() 持锁同步加载 agent 状态，会阻塞事件摄取与上下文推送](https://github.com/agentscope-ai/agentscope-java/issues/3768) | 2026-10-26 | 2026-11-05 | 2 |  |
| #3733 | [ControlPlane Http 连接后约 45 秒被标记为不健康，并且永远不会被调度](https://github.com/agentscope-ai/agentscope-java/issues/3733) | 2026-10-23 | 2026-10-26 | 3 |  |
| #3683 | [agentscope-extensions-jdbc 依赖缺少JdbcAgentSkillRepository类](https://github.com/agentscope-ai/agentscope-java/issues/3683) | 2026-10-21 | 2026-10-21 | 1 |  |
| #3671 | [自定义 AbstractFilesystem 包装器导致 ShellExecuteTool 不注册](https://github.com/agentscope-ai/agentscope-java/issues/3671) | 2026-10-20 | 2026-11-15 | 1 |  |
| #3667 | [RemoteFilesystem.exists() 全量扫描 namespace 并物化 value_json，导致新会话首次 append 时 OOM](https://github.com/agentscope-ai/agentscope-java/issues/3667) | 2026-10-20 | 2026-10-20 | 1 |  |
| #3651 | [是否需要重新构建智能体](https://github.com/agentscope-ai/agentscope-java/issues/3651) | 2026-10-19 | 2026-10-23 | 1 |  |
| #3650 | [ReActAgent 在摘要无可见产出时以空 Mono<Msg> 静默结束，调用方无法区分「无输出」与「正常完成」](https://github.com/agentscope-ai/agentscope-java/issues/3650) | 2026-10-19 | 2026-10-22 | 4 |  |
| #3647 | [会话列表接口对每个会话都做一次完整 journal 重放 N 个会话 = N 次全量状态重建，耗时达数秒](https://github.com/agentscope-ai/agentscope-java/issues/3647) | 2026-10-19 | 2026-11-12 | 7 |  |
| #3644 | [DockerSandbox container name is deterministic while liveness is keyed by containerId — a state/daemon divergence wedges a sandbox session permanently (docker run exit=125 on every turn)](https://github.com/agentscope-ai/agentscope-java/issues/3644) | 2026-10-18 | 2026-10-31 | 2 |  |
| #3612 | [会话历史存储换代后缺少删除原生会话(journal)的官方 API](https://github.com/agentscope-ai/agentscope-java/issues/3612) | 2026-10-16 | 2026-11-05 | 4 |  |
| #3611 | [Docker部署agentscope-service启动后日志报错](https://github.com/agentscope-ai/agentscope-java/issues/3611) | 2026-10-16 | 2026-10-19 | 2 |  |
| #3599 | [根据文档docker部署service遇到问题](https://github.com/agentscope-ai/agentscope-java/issues/3599) | 2026-10-16 | 2026-10-16 | 1 |  |
| #3567 | [会话历史存储后端换代（#3441 / JEV）后存在两处遗留问题：原生会话(journal)缺少删除 API、旧会话无迁移方案且与兼容模式互斥](https://github.com/agentscope-ai/agentscope-java/issues/3567) | 2026-10-14 | 2026-10-16 | 6 |  |
| #3558 | [agentscope-service与managed agent后台报违反唯一键](https://github.com/agentscope-ai/agentscope-java/issues/3558) | 2026-10-14 | 2026-10-18 | 2 |  |
| #3510 | [Shared MemoryConsolidator watermark skips unprocessed ledgers across isolated users](https://github.com/agentscope-ai/agentscope-java/issues/3510) | 2026-10-11 | 2026-10-11 | 1 |  |
| #3475 | [ActionObservations.emitSafely swallows event-delivery exception (no message, no stack trace)](https://github.com/agentscope-ai/agentscope-java/issues/3475) | 2026-10-10 | 2026-10-10 | 0 |  |
| #3433 | [短期记忆Memory Flush 时间格式如何修改默认不是utc时间 — 2026-10-07T08:15:40.992228Z](https://github.com/agentscope-ai/agentscope-java/issues/3433) | 2026-10-07 | 2026-10-22 | 3 |  |
| #3426 | [Harness智能体装配技能运行在new LocalFilesystemSpec().isolationScope(isolationScope)时路径会存在问题](https://github.com/agentscope-ai/agentscope-java/issues/3426) | 2026-10-06 | 2026-10-06 | 0 |  |
| #3262 | [Windows 宿主上 write_file / edit_file 必然失败：命令中的双引号在传参往返中被丢弃](https://github.com/agentscope-ai/agentscope-java/issues/3262) | 2026-09-23 | 2026-10-23 | 5 |  |
| #3192 | [Sandbox mode system prompt is inconsistent with the container filesystem: host workspace path injected and sandbox root hard-coded to /workspace](https://github.com/agentscope-ai/agentscope-java/issues/3192) | 2026-09-18 | 2026-09-22 | 4 |  |
| #3161 | [](https://github.com/agentscope-ai/agentscope-java/issues/3161) | 2026-09-15 | 2026-09-15 | 1 |  |
| #3063 | [Class org.redisson.api.RScript$ReturnType does not have member field 'org.redisson.api.RScript$ReturnType LONG'](https://github.com/agentscope-ai/agentscope-java/issues/3063) | 2026-09-09 | 2026-10-27 | 4 |  |
| #2979 | [2.0.2版本 k8s sandbox  每次调用后都会删除claim，下一次调用resume不到卡30秒后会重新创建](https://github.com/agentscope-ai/agentscope-java/issues/2979) | 2026-09-04 | 2026-09-16 | 2 |  |
| #2977 | [fix(aistio): agent_shares table is never created — Agent Share API fails with "relation agent_shares does not exist", and console Agent detail tabs are hidden because tierForCurrentUser is never  returned](https://github.com/agentscope-ai/agentscope-java/issues/2977) | 2026-09-04 | 2026-09-13 | 2 |  |
| #2962 | [Zero-chunk model completion ends the ReAct run as silent success - no error, no retry, no fallback](https://github.com/agentscope-ai/agentscope-java/issues/2962) | 2026-09-03 | 2026-09-16 | 2 |  |
| #2959 | [agentscope-dependencies-bom pins MCP SDK 0.17.0, hit by java-sdk#748's 202+text/plain empty-response bug](https://github.com/agentscope-ai/agentscope-java/issues/2959) | 2026-09-03 | 2026-09-13 | 1 | 标记: dependencies |
| #2950 | [目前如果开启多个子Agent并行跑，可能会出现一些问题](https://github.com/agentscope-ai/agentscope-java/issues/2950) | 2026-09-03 | 2026-09-08 | 2 | 标记: enhancement |
| #2937 | [Windows NTFS: session key containing ':' causes InvalidPathException in WorkspaceManager task/session file paths](https://github.com/agentscope-ai/agentscope-java/issues/2937) | 2026-09-02 | 2026-09-24 | 5 |  |
| #2924 | [DockerSandbox.doExec() strips nested double quotes on Windows](https://github.com/agentscope-ai/agentscope-java/issues/2924) | 2026-09-01 | 2026-09-13 | 2 |  |
| #2829 | [ExecutionConfig retry settings are ignored for tool failures](https://github.com/agentscope-ai/agentscope-java/issues/2829) | 2026-08-24 | 2026-09-25 | 2 |  |
| #2821 | [开启长期记忆后，每次对话完成，会同步卡在那里等待，能不能做成异步的](https://github.com/agentscope-ai/agentscope-java/issues/2821) | 2026-08-24 | 2026-08-25 | 8 |  |
| #2790 | [TaskRepository.cancelTask cannot stop an already-running async subagent](https://github.com/agentscope-ai/agentscope-java/issues/2790) | 2026-08-20 | 2026-08-20 | 1 |  |
| #2789 | [TaskRepository.cancelTask 无法停止正在执行的异步 subagent —— cancelRequested 在 agent 执行循环内没有检查点](https://github.com/agentscope-ai/agentscope-java/issues/2789) | 2026-08-20 | 2026-08-20 | 0 |  |
| #2773 | [AguiResumeCoordinator loses HITL interrupt state across instances](https://github.com/agentscope-ai/agentscope-java/issues/2773) | 2026-08-19 | 2026-08-19 | 1 |  |
| #2772 | [非沙箱模式下,分布式部署机器workspace使用共享磁盘挂载会导致.skill-cache竞争](https://github.com/agentscope-ai/agentscope-java/issues/2772) | 2026-08-19 | 2026-08-20 | 1 |  |

### 2.4 `area/core/tool` — 36 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3301 | [Enum tool parameters with @JsonProperty/@JsonValue: schema lists constant names but conversion expects the renamed values](https://github.com/agentscope-ai/agentscope-java/issues/3301) | 2026-09-26 | 2026-10-23 | 4 |  |
| #3243 | [[Question] What is the best practice for obtaining permission suggestedRules in the ReActAgent HITL flow?](https://github.com/agentscope-ai/agentscope-java/issues/3243) | 2026-09-22 | 2026-10-03 | 4 | 交叉: area/docs |
| #3167 | [updateToolGroups() does not take effect on existing harnessAgent.getToolkit() instance](https://github.com/agentscope-ai/agentscope-java/issues/3167) | 2026-09-16 | 2026-10-28 | 3 |  |
| #2670 | [AsyncToolExample通过openai使用mimo模型时候执行工具超时还是阻塞了主线程](https://github.com/agentscope-ai/agentscope-java/issues/2670) | 2026-08-12 | 2026-08-12 | 0 |  |
| #2653 | [HarnessAgent  (Skill + ToolGroup: on-demand tool disclosure) not work](https://github.com/agentscope-ai/agentscope-java/issues/2653) | 2026-08-10 | 2026-08-11 | 1 | 交叉: area/harness |
| #2441 | [SkillManageTool的实现里面使用的是WorkspaceSkillRepository，这个是对本地文件的操作，如选择数据库这个实现只能存在本地。](https://github.com/agentscope-ai/agentscope-java/issues/2441) | 2026-07-28 | 2026-08-11 | 1 | 交叉: area/harness |
| #2438 | [当询问有哪些技能的时候，会把对应的ToolKit里面注册的工具一并带出。](https://github.com/agentscope-ai/agentscope-java/issues/2438) | 2026-07-28 | 2026-08-11 | 1 |  |
| #2407 | [MysqlSkillRepository 和 PostgresSkillRepository 表名和库名无法自定义传参](https://github.com/agentscope-ai/agentscope-java/issues/2407) | 2026-07-27 | 2026-09-21 | 5 |  |
| #2312 | [OnActing模式下，沙箱内应用工作空间的路径和宿主机挂载路径不一致时，会在用户工作空间下创建一个嵌套的工作目录，OnAgent模式下正常](https://github.com/agentscope-ai/agentscope-java/issues/2312) | 2026-07-20 | 2026-08-11 | 1 | 交叉: area/extensions, area/build, area/harness |
| #2169 | [技能已经激活，但绑定的工具没有权限](https://github.com/agentscope-ai/agentscope-java/issues/2169) | 2026-07-13 | 2026-07-29 | 5 |  |
| #2091 | [Session files are still created in workspace with RemoteFilesystem](https://github.com/agentscope-ai/agentscope-java/issues/2091) | 2026-07-09 | 2026-07-11 | 1 |  |
| #2065 | [LocalFilesystemSpec falls back to user.dir as project root, leaking JVM working directory to Agent reads](https://github.com/agentscope-ai/agentscope-java/issues/2065) | 2026-07-09 | 2026-07-11 | 0 |  |
| #2003 | [McpClientBuilder.streamableHttpTransport does not expose resumableStreams option — hangs on SSE-disabled MCP servers](https://github.com/agentscope-ai/agentscope-java/issues/2003) | 2026-07-03 | 2026-07-11 | 0 |  |
| #1906 | [当skill目录下存在其他文件时，使用会存在问题](https://github.com/agentscope-ai/agentscope-java/issues/1906) | 2026-06-25 | 2026-07-11 | 0 |  |
| #1898 | [我让智能体删除 .agentscope/workspace，他真的能execute工具给删除，怎么整？或者写一个python脚本，也能删除workspace下其他userid的文件，根本防不住啊](https://github.com/agentscope-ai/agentscope-java/issues/1898) | 2026-06-24 | 2026-11-11 | 3 |  |
| #1896 | [SandboxBackedFilesystem 并发多用户不安全 &&框架后台任务用 RuntimeContext.empty() 写文件系统，破坏多用户路由](https://github.com/agentscope-ai/agentscope-java/issues/1896) | 2026-06-24 | 2026-07-11 | 0 |  |
| #1775 | [read_file读取project目录的时候，没有拼接userId](https://github.com/agentscope-ai/agentscope-java/issues/1775) | 2026-06-16 | 2026-09-23 | 4 | 交叉: area/harness; 标记: wait-for-response |
| #1705 | [中文 Skill 名路径编码异常](https://github.com/agentscope-ai/agentscope-java/issues/1705) | 2026-06-11 | 2026-06-12 | 0 |  |
| #1696 | [Title: Feature request: allow optional display metadata in todo_write tasks](https://github.com/agentscope-ai/agentscope-java/issues/1696) | 2026-06-10 | 2026-11-05 | 1 |  |
| #1689 | [并行执行，openapi协议](https://github.com/agentscope-ai/agentscope-java/issues/1689) | 2026-06-10 | 2026-06-10 | 0 |  |
| #1674 | [glob_files tool description advertises **/*.java pattern but BaseSandboxFilesystem implementation doesn't support it](https://github.com/agentscope-ai/agentscope-java/issues/1674) | 2026-06-08 | 2026-07-28 | 2 |  |
| #1633 | [升级2.0版本后，在流式输出中事件无法正常获取到工具的完整输出结果。](https://github.com/agentscope-ai/agentscope-java/issues/1633) | 2026-06-05 | 2026-06-05 | 0 |  |
| #1569 | [Deduplicate repeated load_skill_through_path calls for already-active skills](https://github.com/agentscope-ai/agentscope-java/issues/1569) | 2026-06-02 | 2026-08-30 | 1 |  |
| #1442 | [Improve skill YAML frontmatter parsing tolerance for unquoted colons in scalar values](https://github.com/agentscope-ai/agentscope-java/issues/1442) | 2026-05-19 | 2026-05-28 | 2 |  |
| #1438 | [SKILL.md 元数据区通过多行定义数组类型数据，执行技能是报错：sequence entries are not allowed here  in 'string', line 6, column 18:](https://github.com/agentscope-ai/agentscope-java/issues/1438) | 2026-05-19 | 2026-05-28 | 1 |  |
| #1306 | [工具组动态激活后not available](https://github.com/agentscope-ai/agentscope-java/issues/1306) | 2026-04-27 | 2026-05-28 | 3 |  |
| #1216 | [执行工具调用是，解析的参数发生异常（When executing the tool call, an exception occurred in the parsed parameters）](https://github.com/agentscope-ai/agentscope-java/issues/1216) | 2026-04-14 | 2026-05-28 | 7 |  |
| #1212 | [ShellCommandTool 没有触发 approvalCallback](https://github.com/agentscope-ai/agentscope-java/issues/1212) | 2026-04-13 | 2026-05-28 | 2 |  |
| #913 | [mcp服务在docker环境部署使用时，会出现报错：java.lang.IllegalStateException: java.lang.IllegalStateException: No default JsonSchemaValidatorSupplier implementation found](https://github.com/agentscope-ai/agentscope-java/issues/913) | 2026-03-10 | 2026-07-12 | 2 | 标记: help wanted |
| #912 | [mcp服务在使用时，会偶现报错：.a.core.tool.mcp.McpSyncClientWrapper   : Failed to initialize MCP client: gaode-mcp java.lang.RuntimeException: Client failed to initialize by explicit API call](https://github.com/agentscope-ai/agentscope-java/issues/912) | 2026-03-10 | 2026-05-28 | 1 |  |
| #885 | [Failed to initialize MCP client: business-knowledge-query](https://github.com/agentscope-ai/agentscope-java/issues/885) | 2026-03-06 | 2026-09-28 | 1 | 标记: help wanted |
| #833 | [使用 sse 协议连接 mcp 服务器时，启动报错：onErrorComplete method did not exist](https://github.com/agentscope-ai/agentscope-java/issues/833) | 2026-02-26 | 2026-05-28 | 0 |  |
| #770 | [Error: Access denied: The file path '.' is outside the allowed base directory](https://github.com/agentscope-ai/agentscope-java/issues/770) | 2026-02-10 | 2026-05-28 | 1 |  |
| #755 | [MPC通过http访问如何忽略证书了，目前貌似没办法添加](https://github.com/agentscope-ai/agentscope-java/issues/755) | 2026-02-07 | 2026-05-28 | 0 |  |
| #665 | [When a SubAgent uses the MCP tools from the MainAgent, there is a certain probability of errors occurring.](https://github.com/agentscope-ai/agentscope-java/issues/665) | 2026-01-26 | 2026-05-28 | 1 |  |
| #626 | [使用@Tool定义工具时如果有其他注解无法注册到智能中](https://github.com/agentscope-ai/agentscope-java/issues/626) | 2026-01-22 | 2026-07-06 | 5 |  |

### 2.5 `area/core/memory` — 22 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #2701 | [当传入图片url或者base64数据时，agent_sessions表中没有存储对应的数据](https://github.com/agentscope-ai/agentscope-java/issues/2701) | 2026-08-14 | 2026-08-15 | 0 |  |
| #2694 | [CompactionMiddleware 自建 MemoryFlushManager，绕过 disableMemoryHooks()——「记忆已关闭」时仍会写记忆文件并额外调用 LLM](https://github.com/agentscope-ai/agentscope-java/issues/2694) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2693 | [pruneOldSessions 的 glob 路径层级与路由前缀不匹配，远程 session 数据永远扫不到（sessionRetentionDays 静默失效）](https://github.com/agentscope-ai/agentscope-java/issues/2693) | 2026-08-13 | 2026-08-15 | 1 |  |
| #2692 | [WorkspaceIndex 快速路径返回空 mtime，使 MemoryConsolidator 的水位线与 pruneOldSessions 同时失效（且无法关闭）](https://github.com/agentscope-ai/agentscope-java/issues/2692) | 2026-08-13 | 2026-08-25 | 3 |  |
| #2691 | [配了远程 store 后 session 仍写本地工作副本，且该副本的路径与路由下层不一致、无人可读](https://github.com/agentscope-ai/agentscope-java/issues/2691) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2690 | [CompositeFilesystem 的路由前缀会劫持同名的宿主挂载点（/knowledge、/skills…），静默读不到文件](https://github.com/agentscope-ai/agentscope-java/issues/2690) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2566 | [compaction summary leaks host filesystem path into the agent prompt (invalid inside Docker sandbox)](https://github.com/agentscope-ai/agentscope-java/issues/2566) | 2026-08-05 | 2026-08-11 | 1 | 交叉: area/extensions, area/build |
| #2564 | [Sandbox is created on every agent call even for pure chat turns](https://github.com/agentscope-ai/agentscope-java/issues/2564) | 2026-08-05 | 2026-08-12 | 4 | 交叉: area/extensions |
| #2517 | [harness: ProjectAwareOverlay/projectWritable design has structural inconsistencies (dual-root routing)](https://github.com/agentscope-ai/agentscope-java/issues/2517) | 2026-08-01 | 2026-08-11 | 3 | 交叉: area/core/tool, area/build, area/harness |
| #2501 | [read_file cannot read workspace/knowledge under IsolationScope.USER despite docs defining knowledge/ as a shared static asset](https://github.com/agentscope-ai/agentscope-java/issues/2501) | 2026-07-31 | 2026-08-11 | 1 | 交叉: area/docs, area/build, area/harness |
| #2421 | [DaytonaSandboxClient: Using snapshot to create sandbox fails because cpu/memory/disk default values are always sent](https://github.com/agentscope-ai/agentscope-java/issues/2421) | 2026-07-27 | 2026-08-11 | 2 | 交叉: area/extensions, area/build |
| #2329 | [IsolationScope (SESSION/USER) does not apply to execute_shell_command working_directory](https://github.com/agentscope-ai/agentscope-java/issues/2329) | 2026-07-21 | 2026-08-11 | 2 | 交叉: area/core/tool, area/build, area/harness |
| #2327 | [PostgresAgentStateStore存储记忆有问题](https://github.com/agentscope-ai/agentscope-java/issues/2327) | 2026-07-20 | 2026-08-11 | 2 | 交叉: area/extensions |
| #2276 | [harnessAgent编排模式,Memory 的 `concatWith` 后置钩子阻塞了 `call()` Flux complete处理，导致编排长时间等待？是否有其他方案](https://github.com/agentscope-ai/agentscope-java/issues/2276) | 2026-07-18 | 2026-07-29 | 2 |  |
| #2225 | [MemoryConsolidator blocks agent call via `concatWith` + `.block()`](https://github.com/agentscope-ai/agentscope-java/issues/2225) | 2026-07-15 | 2026-07-15 | 0 |  |
| #2173 | [使用DistributedStore的方式，数据库中的agentscope_sessions中的state_data字段会一直叠加](https://github.com/agentscope-ai/agentscope-java/issues/2173) | 2026-07-13 | 2026-07-29 | 1 |  |
| #1969 | [前端手动压缩消息无效果](https://github.com/agentscope-ai/agentscope-java/issues/1969) | 2026-06-30 | 2026-07-11 | 3 |  |
| #1936 | [发送 ”这是什么 @world.txt“，历史记录会自动拼接<attached_file></attached_file>](https://github.com/agentscope-ai/agentscope-java/issues/1936) | 2026-06-26 | 2026-07-11 | 2 |  |
| #1740 | [compressIfNeeded() 是单次执行的——任何一个策略成功后就退出。对于 大于maxtoken 的上下文，需要反复循环直到 token 降到阈值以下。](https://github.com/agentscope-ai/agentscope-java/issues/1740) | 2026-06-13 | 2026-06-13 | 0 |  |
| #1262 | [Multiple system messages caused by StaticLongTermMemoryHook leads to LLM request failure](https://github.com/agentscope-ai/agentscope-java/issues/1262) | 2026-04-21 | 2026-05-28 | 0 |  |
| #1094 | [打开长期记忆后，输出完成会有延时](https://github.com/agentscope-ai/agentscope-java/issues/1094) | 2026-04-01 | 2026-06-13 | 2 |  |
| #1093 | [1.0.11 AgentScopeJvmShutdownHook conflict with spring](https://github.com/agentscope-ai/agentscope-java/issues/1093) | 2026-04-01 | 2026-05-28 | 5 | 标记: good first issue |

### 2.6 `area/ext/integration` — 16 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #2911 | [agentscope-copilotkit中connect历史消息重放时重复激活前端中断UI](https://github.com/agentscope-ai/agentscope-java/issues/2911) | 2026-08-30 | 2026-09-14 | 1 | 交叉: area/examples |
| #2736 | [A2A的spring-boot模块，一个服务只支持一个agent?](https://github.com/agentscope-ai/agentscope-java/issues/2736) | 2026-08-17 | 2026-08-17 | 0 | 交叉: area/ext/spring-boot |
| #2651 | [使用ssh访问git skill仓库报错](https://github.com/agentscope-ai/agentscope-java/issues/2651) | 2026-08-10 | 2026-08-11 | 2 |  |
| #2507 | [a2a sse接口事件超过1000的时候会导致卡住](https://github.com/agentscope-ai/agentscope-java/issues/2507) | 2026-07-31 | 2026-11-03 | 3 |  |
| #2265 | [使用agentscope-extensions-channel-dingtalk这个模块，初始化钉钉client的时候报错了](https://github.com/agentscope-ai/agentscope-java/issues/2265) | 2026-07-17 | 2026-07-18 | 0 |  |
| #2102 | [使用自定义工作流CompiledGraph， agui无法返回事件流，REASONING_MESSAGE_CONTENT和TEXT_MESSAGE_CONTENT等重要的事件流类型没有](https://github.com/agentscope-ai/agentscope-java/issues/2102) | 2026-07-10 | 2026-07-13 | 6 |  |
| #2070 | [AgentRun MCP initialize 创建新沙箱实例，不复用 CreateSandbox 创建的带 OSS/NAS 挂载的实例](https://github.com/agentscope-ai/agentscope-java/issues/2070) | 2026-07-09 | 2026-07-11 | 1 |  |
| #1907 | [`AgentRunDataPlaneHttp` 响应 JSON `data` 信封未解包](https://github.com/agentscope-ai/agentscope-java/issues/1907) | 2026-06-25 | 2026-07-11 | 0 |  |
| #1821 | [A2A streaming BufferingTube overflow with parallel tool calls](https://github.com/agentscope-ai/agentscope-java/issues/1821) | 2026-06-18 | 2026-06-19 | 0 |  |
| #1795 | [oss 模块是否考虑使用 awssdk 目前阿里云的 sdk 各种不兼容 minio 都存不进去](https://github.com/agentscope-ai/agentscope-java/issues/1795) | 2026-06-17 | 2026-06-18 | 1 |  |
| #1790 | [单技能仓库（无 skills 子目录）无法加载技能](https://github.com/agentscope-ai/agentscope-java/issues/1790) | 2026-06-17 | 2026-06-29 | 2 |  |
| #1455 | [a2aclient请求404](https://github.com/agentscope-ai/agentscope-java/issues/1455) | 2026-05-21 | 2026-05-28 | 1 |  |
| #1381 | [引入AgentScope A2a后nacos需要重复配置问题](https://github.com/agentscope-ai/agentscope-java/issues/1381) | 2026-05-12 | 2026-05-28 | 0 |  |
| #1378 | [AgentScope Java A2A Streaming 重复输出问题反馈](https://github.com/agentscope-ai/agentscope-java/issues/1378) | 2026-05-11 | 2026-08-24 | 7 |  |
| #750 | [When integrating with agentscope-studio in a web service, after the request ends, the observed request in agentscope-studio remains in the running state.](https://github.com/agentscope-ai/agentscope-java/issues/750) | 2026-02-06 | 2026-05-28 | 1 |  |
| #321 | [[Feature]:For the agent.call call to a2a, if there are tool calls inside, after returning the tool execution results, it does not call the large model again, directly returns the tool call informatio](https://github.com/agentscope-ai/agentscope-java/issues/321) | 2025-12-23 | 2026-05-28 | 2 |  |

### 2.7 `area/core/model` — 10 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3147 | [help, call llm timeout，what's the problem?](https://github.com/agentscope-ai/agentscope-java/issues/3147) | 2026-09-14 | 2026-09-14 | 1 |  |
| #2700 | [无法下载的图片url会导致请求出错DashScope API error: <400> InternalError.Algo.InvalidParameter: Failed to download multimodal content](https://github.com/agentscope-ai/agentscope-java/issues/2700) | 2026-08-13 | 2026-08-15 | 0 |  |
| #2538 | [harnessAgent 配置ossAmount，agentRun沙箱销毁时，并没有把工作空间备份到oss中](https://github.com/agentscope-ai/agentscope-java/issues/2538) | 2026-08-03 | 2026-08-11 | 1 | 交叉: area/core/tool, area/build, area/harness |
| #2512 | [AG-UI TEXT_MESSAGE events interleave when multiple agent_spawn run concurrently with parallel=true](https://github.com/agentscope-ai/agentscope-java/issues/2512) | 2026-07-31 | 2026-09-25 | 3 | 交叉: area/core/tool, area/ext/integration, area/ext/spring-boot, area/build |
| #1944 | [DashScope使用DeepSeek缓存命中过低](https://github.com/agentscope-ai/agentscope-java/issues/1944) | 2026-06-27 | 2026-07-31 | 1 |  |
| #1568 | [dashscope报错'dict object' has no attribute 'name'](https://github.com/agentscope-ai/agentscope-java/issues/1568) | 2026-06-02 | 2026-08-07 | 3 |  |
| #1309 | [Bug Report: stream=true 模式下 tool call 参数 JSON 不完整导致工具调用失败](https://github.com/agentscope-ai/agentscope-java/issues/1309) | 2026-04-27 | 2026-05-28 | 0 |  |
| #1287 | [OpenAIChatModel Http2 有时候调用不通大模型，建议设置自动降级功能，阿里云的服务器默认会对Http2进行限流](https://github.com/agentscope-ai/agentscope-java/issues/1287) | 2026-04-25 | 2026-05-28 | 0 |  |
| #1284 | [DashScopeMessageConverter does not preserve reasoning_content from ThinkingBlock in assistant messages](https://github.com/agentscope-ai/agentscope-java/issues/1284) | 2026-04-25 | 2026-05-28 | 0 |  |
| #1237 | [1.0.11 流式模式下 ToolUseBlock name 为 null](https://github.com/agentscope-ai/agentscope-java/issues/1237) | 2026-04-17 | 2026-05-28 | 0 |  |

### 2.8 `area/extensions` — 9 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3310 | [Cron scheduler silently falls back to GMT for invalid or offset zone IDs](https://github.com/agentscope-ai/agentscope-java/issues/3310) | 2026-09-26 | 2026-10-23 | 2 |  |
| #3248 | [MysqlSkillRepository builds SQL with unquoted identifiers — reserved-word database names break, and the accepted identifier set diverges from MysqlAgentStateStore](https://github.com/agentscope-ai/agentscope-java/issues/3248) | 2026-09-22 | 2026-10-21 | 2 |  |
| #3213 | [PostgresAgentStateStore executes ALTER TABLE even when createIfNotExist=false](https://github.com/agentscope-ai/agentscope-java/issues/3213) | 2026-09-20 | 2026-09-24 | 4 | 交叉: area/ext/memory |
| #3153 | [aistio: collaboration tool names use dots and still break OpenAI-compatible models (HTTP 400)](https://github.com/agentscope-ai/agentscope-java/issues/3153) | 2026-09-15 | 2026-09-19 | 3 |  |
| #3132 | [HttpSelfRegistration.close() skips deregistration when a heartbeat re-register cycle is in flight (flaky HttpSelfRegistrationTest on CI)](https://github.com/agentscope-ai/agentscope-java/issues/3132) | 2026-09-13 | 2026-09-16 | 3 |  |
| #2396 | [创建k8s沙箱的时候报错，无法新建工作目录](https://github.com/agentscope-ai/agentscope-java/issues/2396) | 2026-07-26 | 2026-08-11 | 4 |  |
| #2382 | [AgentRun sandbox:](https://github.com/agentscope-ai/agentscope-java/issues/2382) | 2026-07-24 | 2026-08-11 | 1 | 交叉: area/examples |
| #2365 | [使用默认镜像运行沙箱命令，write_file和edit_file工具报错](https://github.com/agentscope-ai/agentscope-java/issues/2365) | 2026-07-23 | 2026-08-11 | 1 |  |
| #2165 | [`ERR 'EVAL' command keys must in same slot` in Redis Cluster](https://github.com/agentscope-ai/agentscope-java/issues/2165) | 2026-07-13 | 2026-07-29 | 1 |  |

### 2.9 `area/core` — 6 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3318 | [Media URLSource conversion reads unbounded responses and mishandles file URIs and MIME hints](https://github.com/agentscope-ai/agentscope-java/issues/3318) | 2026-09-27 | 2026-10-26 | 4 | 交叉: area/extensions |
| #3237 | [SSE 断连导致本轮会话进度整体丢失：AgentState 仅在  call()  正常结束时落盘一次，所有 cancel 路径均无保存钩子](https://github.com/agentscope-ai/agentscope-java/issues/3237) | 2026-09-21 | 2026-09-24 | 6 | 标记: help wanted |
| #3199 | [Base64 tool-result formatting creates nondeterministic temporary paths](https://github.com/agentscope-ai/agentscope-java/issues/3199) | 2026-09-18 | 2026-09-18 | 3 |  |
| #2991 | [JdkHttpTransport SSE streams exhaust the shared boundedElastic scheduler](https://github.com/agentscope-ai/agentscope-java/issues/2991) | 2026-09-05 | 2026-09-15 | 5 |  |
| #2256 | [单例并发问题，目前多用户同时问，总会出现等待](https://github.com/agentscope-ai/agentscope-java/issues/2256) | 2026-07-16 | 2026-07-18 | 3 |  |
| #2254 | [filesystem重写snapshotSpec不生效](https://github.com/agentscope-ai/agentscope-java/issues/2254) | 2026-07-16 | 2026-07-24 | 0 |  |

### 2.10 `area/ext/memory` — 5 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #2005 | [agentscope-extensions-reme:2.0.0-RC4 中的 ReMeClient 跟 Reme-ai 服务端 0.4.0.6 不对应，最高只能对应到 v0.3.1.10 版本](https://github.com/agentscope-ai/agentscope-java/issues/2005) | 2026-07-04 | 2026-07-11 | 1 |  |
| #1801 | [MysqlSession persistence PlanNotebook.currentPlan (Plan @JsonIgnore field not save)](https://github.com/agentscope-ai/agentscope-java/issues/1801) | 2026-06-17 | 2026-06-17 | 1 |  |
| #1358 | [[Question] `AutoContextMemory 当前轮压缩导致 LLM 直接输出内部摘要内容`](https://github.com/agentscope-ai/agentscope-java/issues/1358) | 2026-05-08 | 2026-06-25 | 1 |  |
| #1288 | [Mem0LongTermMemory](https://github.com/agentscope-ai/agentscope-java/issues/1288) | 2026-04-25 | 2026-05-28 | 1 |  |
| #1260 | [AutoContextMemory未按设计的预期执行策略压缩（compressIfNeeded() Strategy 1 cursor/index mismatch and offloadingLargePayload() lastKeep check issues）](https://github.com/agentscope-ai/agentscope-java/issues/1260) | 2026-04-20 | 2026-05-28 | 0 |  |

### 2.11 `area/build` — 4 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #2972 | [Dependency Conflict with Spring AI 2.0.1](https://github.com/agentscope-ai/agentscope-java/issues/2972) | 2026-09-03 | 2026-09-13 | 1 | 标记: dependencies |
| #2738 | [agentscope-service: frontend/src/features/build/ 目录中文件被gitignore中build/排除，导致页面未提交至代码库](https://github.com/agentscope-ai/agentscope-java/issues/2738) | 2026-08-17 | 2026-08-19 | 1 |  |
| #2340 | [HarnessAgent注入的子agent是自行build出来的HarnessAgent，会默认注入自带的general-purpose](https://github.com/agentscope-ai/agentscope-java/issues/2340) | 2026-07-21 | 2026-08-11 | 2 | 交叉: area/harness |
| #1271 | [Dependency conflict with org.springframework.ai 2.0.0-M4 due to com.github.victools version mismatch](https://github.com/agentscope-ai/agentscope-java/issues/1271) | 2026-04-22 | 2026-08-25 | 3 |  |

### 2.12 `area/examples` — 4 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #1725 | [agentscope-builder 无法以 remote/sandbox 模式启动，即使已提供 Redis 环境](https://github.com/agentscope-ai/agentscope-java/issues/1725) | 2026-06-11 | 2026-06-12 | 0 |  |
| #1723 | [`builder.workspace-store.fs-spec` config property documented but never implemented](https://github.com/agentscope-ai/agentscope-java/issues/1723) | 2026-06-11 | 2026-07-12 | 1 |  |
| #1600 | [agentscope-dataagent fails to start — builderBootstrap throws "requires a non-noop snapshotSpec" (default single-node run)](https://github.com/agentscope-ai/agentscope-java/issues/1600) | 2026-06-04 | 2026-06-05 | 1 |  |
| #1567 | [AgentScope Builder新建agent时创建的workspace路径不正确](https://github.com/agentscope-ai/agentscope-java/issues/1567) | 2026-06-02 | 2026-06-02 | 0 |  |

### 2.13 `area/ext/rag` — 4 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #3305 | [TextChunker emits blank chunks into the RAG embedding pipeline](https://github.com/agentscope-ai/agentscope-java/issues/3305) | 2026-09-26 | 2026-10-23 | 2 |  |
| #1516 | [智能体调用RAGFlow，similarityThreshold参数设置失效，始终是0.5](https://github.com/agentscope-ai/agentscope-java/issues/1516) | 2026-05-28 | 2026-06-11 | 1 |  |
| #1095 | [OpenAITextEmbedding 组件存在设计缺陷](https://github.com/agentscope-ai/agentscope-java/issues/1095) | 2026-04-01 | 2026-11-13 | 3 | 标记: good first issue |
| #527 | [百炼知识库的标签过滤条件异常](https://github.com/agentscope-ai/agentscope-java/issues/527) | 2026-01-12 | 2026-05-28 | 1 |  |

### 2.14 `area/docs` — 3 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #2405 | [自定义mcpServer，如果ToolsConfig里没有allow mcpserver里的工具，mcpserver里的工具将不可用](https://github.com/agentscope-ai/agentscope-java/issues/2405) | 2026-07-27 | 2026-08-11 | 1 | 交叉: area/build, area/harness |
| #1883 | [Official example code cannot run directly, documentation is incomplete and poor quality](https://github.com/agentscope-ai/agentscope-java/issues/1883) | 2026-06-23 | 2026-11-12 | 4 |  |
| #933 | [agentscope all-in-one文档描述和实际不符合](https://github.com/agentscope-ai/agentscope-java/issues/933) | 2026-03-12 | 2026-05-28 | 0 |  |

### 2.15 `area/ext/spring-boot` — 1 个

| Issue | 标题 | 创建 | 更新 | 评论 | 附注 |
| ---: | --- | --- | --- | ---: | --- |
| #1456 | [agentscope-a2a-spring-boot-starter 1.0.12依赖冲突](https://github.com/agentscope-ai/agentscope-java/issues/1456) | 2026-05-21 | 2026-05-28 | 5 |  |

## 3. 阅读说明

- 清单只反映 GitHub 上的 issue 状态，不代表缺陷仍在当前 `main` 上复现；部分条目可能已有未合并的 PR 或仍在讨论中。
- `area/harness`、`area/core/agent`、`area/core/tool` 三个模块占了全部未关闭 bug 的一半以上，是当前缺陷最集中的地方。
- 无 `area/*` 标签的条目归入 `(unlabeled)`，需要人工再分类。
- 标签组合大于两个时（如同时打 harness + core/agent + core/tool），清单只在主模块下出现一次，其余列在「附注」。

