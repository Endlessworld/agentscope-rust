# ACP 协议 CodingAgent（Rust 版）— 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 ACP CodingAgent 实现工作的**单一事实来源**。任何会话开工前必须先读本文件；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。以本文件 + `git status`/`cargo build` 实际验证为准。
> **对照基准**：`docs/ACP_AGENT_PLAN.md`（方案）+ 本文件 + `crates/*`（实际代码）。
> **参考**：Kotlin `AgiHarnessAgent.kt`、Java `HarnessCodingAgent.java`、ACP RDF 文档、ACP Rust SDK 源码（本地 `D:\IdeaProjects\agentclientprotocol\rust-sdk`，**tag `v5.0.0`**；旧路径 `E:\local-github\...` 已失效）。SDK 大版本升级后的复核结论见 **§6**。

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
| 依赖 | `agent-client-protocol` 依赖已声明 | 待升级 | `crates/agentscope-extensions-acp/Cargo.toml:19` 仍 pin crates.io **v2.2.0** + 6 个 unstable feature，传递 `agent-client-protocol-schema` **1.9.1**；上游已到 **5.0.0 / schema 1.15.1**，升级任务与新增能力见 **§6** |
| sdk.ability | SDK 能力盘点（2.2.0 一轮 + 5.0.0 一轮） | 已完成（2.2.0） / 待办（5.0.0） | 2.2.0 轮：§2.7.2 所有「待办（不包含暂缓）」项已接线（Notices、Compaction、Plan 操作、Providers、MCP-over-ACP、`PromptResponse.usage`、分页/元数据、CLI 会话恢复）。5.0.0 轮：新增 6 项 v1 能力 + MCP 破坏性改造 + 7 项升级阻断，见 **§6**（`ACP-U01..U08` / `ACP-33..ACP-45`） |

> 2026-10-31 全量对齐复核：本 crate 已独立为 `crates/agentscope-extensions-acp`（17 个 `src/*.rs`），且 **Java 侧无 ACP 模块**（`rg -il 'acp' …/agentscope-java -g '*.java'` exit 1）。本轮以「ACP 规范 + SDK 2.2.0 能力面」为基线逐个文件核对，结论见 **§3.5**（7 项缺口/待裁定，其中 1 项 P0）与 **§4.3**（7 项 Rust 原生设计决定 + 8 项已知差异）。

## 2. 详细缺口清单

### 2.1 依赖与骨架（P0）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| `agent-client-protocol` 依赖 | ✅ 已完成 | 已由 path 依赖（`rust-sdk/src/agent-client-protocol` v2.0.0 + `unstable_tool_call_name`）改为 crates.io `agent-client-protocol = { version = "2.2.0" }`（无 feature），编译通过；能力盘点见 §2.7 |
| `acp/` 模块目录 + mod.rs | ✅ 已完成 | `crates/agentscope-extensions-acp/`（mod/coding_agent/session/config/event） |
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
| `$/cancel_request` → 取消 + StopReason::Cancelled | ✅ 已完成 | 利用 SDK `Responder::cancellation()` marker：PromptRequest handler 取 `responder.cancellation()` 传给 `run_prompt_cancellable`，SDK 收到 `$/cancel_request` 自动置位 marker；handler 用 `cx.spawn` 把 prompt 移出 dispatch loop（否则长 handler 阻塞 loop 导致取消通知无法处理）；`run_prompt_cancellable` 在 select 中监听 `cancellation.cancelled()` 返回 Cancelled。e2e 测试 `acp_cancel_test.rs`（进程内 Channel::duplex + never-ending model）验证通过；`acp_event_flow_test` / `acp_stdio_thought_test`（真实 model）均无回归 |

### 2.5 权限 HITL（P2 增强）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| RequestPermissionRequest 往返 | 待办 | 工具调用前 Ask 决策 |

### 2.6 入口与测试（P1）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| `src/acp_harness_agent.rs`（stdio ACP agent，`[[bin]]` 目标） | ✅ 已完成 | HarnessAgent 全装配（文件系统/shell/compaction/plan mode/skills）+ 真实 stdio 运行验证通过 |
| 单元测试（事件转换/配置） | ✅ 已完成 | event.rs 4 单测 + config.rs 3 单测，共 7 个通过 |
| 协议集成（stdio 握手+prompt） | ✅ 已完成 | groovy 驱动二进制：initialize→new_session→prompt(AgentMessageChunk)→end_turn→close 全链路验证 |
| **取消 e2e（$/cancel_request）** | ✅ 已完成 | `acp_cancel_test.rs`：进程内 `Channel::duplex` transport + never-ending 脚本化 model，client 发 prompt 后 `SentRequest::cancel()`，断言 agent 返回 `StopReason::Cancelled`（0.33s，无需真实 model） |

### 2.7 SDK 能力盘点（`agent-client-protocol` v2.2.0 / `-schema` 1.9.1）

> **依据**：crates.io `agent-client-protocol-2.2.0`（CHANGELOG 2.2.0）与 `agent-client-protocol-schema-1.9.1` 源码；本地 `E:\local-github\agentclientprotocol\rust-sdk`（tag `v2.2.0`）与 schema 仓库 CHANGELOG（1.8.0 / 1.9.0 / 1.9.1）。
> **「未使用」判定**：`rg` 在 `crates/agentscope-harness/**` 零命中，或 feature 未声明（harness 依赖为 `agent-client-protocol = { version = "2.2.0" }`，**未开启任何 feature**）故类型未编译。

#### 2.7.1 本次升级（SDK 2.1.0 → 2.2.0，schema 1.7.0 → 1.9.1）带来的变化

| 变化 | 类型 | 本项目影响 |
|---|---|---|
| `unstable_tool_call_name` 特性**被移除**（schema 1.8.0 把 `ToolCall.name` / `ToolCallUpdateFields.name` 稳定进 v1） | 破坏性（feature 层面） | ✅ 已适配：Cargo.toml 不再声明该 feature；`event.rs` 一直用 `ToolCall::new(..).name(..)`，升级后无需改代码 |
| **新增 `unstable_session_notices`**（schema 1.9.0 引入该能力） | 新增（unstable） | ❌ 未使用 → 见 2.7.2 第 1 条 |
| `wasm_js` 特性（JS 宿主 `wasm32-unknown-unknown` 改用 Web Crypto 作 UUID 随机源） | 新增（可选） | ❌ 未使用（非 wasm 目标） |
| schema 1.9.1：**可默认化 payload 接受 `null`** | 兼容性修复 | ✅ 自动获得（`LoadSessionResponse`/`ResumeSessionResponse`/`NewSessionResponse` 等传 `null` 不再报错） |
| schema 1.9.1：JSON-RPC `Response.result` **要求键存在**（即便 `result` 反序列化接受 null） | 兼容性修复 | ✅ 自动获得 |
| SDK `AcpAgent` shutdown drain 超时仍保留已捕获 stderr（#365） | 缺陷修复 | ✅ 自动获得（只影响子进程 stderr 诊断，不影响协议交互） |
| v2 文档 / 样例 / 测试（#362 / #363 / #364） | 文档 | 无影响 |

**范围说明**：2.2.0 的 SDK crate 源码改动仅 `acp_agent.rs`（stderr 捕获）与 `lib.rs`（wasm 文档），**未新增其它公开 API**；新增能力全部来自 schema 1.9.0（session notices）。

#### 2.7.2 升级后 SDK 已提供、但本项目未接线

`引入版本` 指该能力最早出现的版本（`≤1.7.0` 表示升级前即存在，只是从未接线）。

| 缺口项 | 引入版本 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| **Session Notices**（`SessionUpdate::Notice` / `NoticeSeverity{Info,Warning,Error,Other(_)}` / `Notice{severity,title,description,_meta}` / `ClientSessionCapabilities.notices`） | schema 1.9.0（SDK `unstable_session_notices`） | ✅ 已完成 | feature 已开启（harness `Cargo.toml`）。`acp/support.rs::ClientSupport` 在 `initialize` 记录 client 能力；`session.rs::advisory_notification` **按 `session.notices` 门控**：广告则发 `SessionUpdate::Notice`（如模型切换失败/MCP 桥接失败），否则降级为普通 agent message（协议要求）。
| `PromptResponse.usage`（turn 级 token 用量） | ≤ schema 1.7.0（`unstable_end_turn_token_usage`） | ✅ 已完成 | feature 已开启。`session.rs::turn_usage` 每轮重置并累加，`turn_usage_to_response` 映射为协议 `Usage`（total/input/output + cached_read）并在 `coding_agent.rs` 的 `PromptResponse::new(stop).usage(usage)` 回传；空轮返回 `None`（不发伪造 0）。`UsageUpdate`（上下文窗口）仍照旧推送 |
| **Plan 操作**：`SessionUpdate::PlanUpdate` / `PlanRemoved` + `PlanId` 存续式计划 | ≤ schema 1.7.0（`unstable_plan_operations`） | ✅ 已完成 | feature 已开启。`ClientSupport::supports_plan_operations()` 读 `ClientCapabilities.plan`；`event.rs::ToolEventMapper::with_plan_operations` 在开启时改发 ID 寻址的 `PlanUpdate`（`planId="todos"`），空 todo 列表发 `PlanRemoved`；未广告则仍用旧式 `SessionUpdate::Plan` |
| **上下文压缩通知**：`SessionUpdate::CompactionUpdate` / `CompactionSummaryChunk` + `ClientSessionCapabilities.compaction` | ≤ schema 1.7.0（`unstable_session_compaction`） | ✅ 已完成 | feature 已开启。`ConversationCompactionMiddleware` 压缩后发一条 `Custom` 标记事件（`kind=compaction`）；`session.rs` 识别后在 client 广告 `session.compaction` 时转换为 `InProgress → SummaryChunk → Completed`（`event.rs::compaction_notifications`），未广告则静默 |
| **Provider 配置方法**：`providers/list` / `providers/set` / `providers/disable` + `AgentCapabilities.providers` | ≤ schema 1.7.0（`unstable_llm_providers`） | ✅ 已完成 | feature 已开启。新增 `acp/provider.rs`（`ProviderRegistry`/`ProviderDescriptor`）；`coding_agent.rs` 注册三个 handler，且仅当 registry 非空时在 `initialize` 声明 `AgentCapabilities.providers`；未知/必填 provider 返回 `invalid_params` 错误。registry 为空时（默认）provider 能力不广告 |
| **ACP v2 协议**：`Agent::v2()` / `Client::v2()` / `V2ConnectionTo` / `session/fork` / `V2SessionBuilder` 等 | ≤ schema 1.7.0（`unstable_protocol_v2`） | 待办（暂缓） | 全部走 stable v1。`rg "\.v2\(\)|V2ConnectionTo"` 0 命中 |
| **Proxy / Conductor 与协议路由**：`Proxy`、`ProxyProtocolRouter`、`AgentProtocolRouter`、`ClientProtocolConnector` | SDK ≥2.0.0 | 待办（暂缓） | `rg "Proxy|protocol_router|AgentProtocolRouter"` 0 命中 |
| **MCP-over-ACP**：`McpServer::Acp(McpServerAcp)` + `mcp/connect` / `mcp/message` / `mcp/disconnect` + `McpCapabilities.acp` | ≤ schema 1.7.0（`unstable_mcp_over_acp`） | ✅ 已完成 | feature 已开启；`coding_agent.rs` 声明 `.acp(true)`。新增 `acp/mcp_over_acp.rs::McpAcpConnection`（connect → `tools/list`/`tools/call` → disconnect；纯函数 `parse_tools_list`/`render_tool_result` 离线可测）；`session.rs::apply_mcp_servers` 新增 `McpServer::Acp` 分支，连接后把服务端工具桥接进本会话 `Toolkit`（`bridge_tools`）。方向：**client 声明 server、agent 发 `mcp/message`** |
| **SDK 内建 MCP *server* 运行时**：`agent_client_protocol::mcp_server`（`McpServerConnect` / `McpServer::new` / `McpConnectionTo`） | ≤ SDK 1.3.0 | 设计决定（不迁移） | 本项目 MCP **client** 走 `rust-mcp-sdk`（`agent/mcp_client_manager.rs`）；SDK 的 mcp_server 半边与本仓职责重叠，不引入 |
| **Client 侧能力与请求**：fs（`fs/read_text_file` / `fs/write_text_file`）、terminal（`terminal/create|output|wait_for_exit|kill|release`） | ≤ schema 1.7.0 | 设计决定（不迁移） | CLI 现已在 `initialize` 构造 `ClientCapabilities`（notices/compaction/plan/elicitation），但**仍不广告 fs/terminal**：agent 自有本地工具与 shell 工具，无需通过客户端执行；工具审批走 `session/request_permission`（与此无关，见 §2.5）。原本同列的 elicitation 已落地，见下条与 §2.7.2a |
| **Elicitation 表单推送（`ask_user`）**：`elicitation/create` + `ElicitationSchema` / `ElicitationAction` / `ElicitationContentValue` | ≤ schema 1.7.0（1.7.0 稳定） | ✅ 已完成（2026-10-19） | agent 向 client 推送表单，以 LLM 工具 `ask_user` 暴露（对齐 MCP elicitation 模型）。新增 `acp/elicitation.rs`（`AskUserTool` + `ElicitationClient` seam + `ElicitationSlot`）；`session.rs` 把 live 连接写入 slot；`coding_agent.rs` 新增 `SessionBuild` + `session_factory` 以便**在 build 前**按能力注入工具；`acp_harness_agent.rs` 用 `session_factory` 注入（子 agent 因此继承）；`acp_agent_cli.rs` 广告 `elicitation.form` 并渲染/回填表单。纯函数映射见 §2.7.2a |
| **Elicitation 表单渲染（Form mode）** | ≤ schema 1.7.0（1.7.0 稳定） | 设计决定（不迁移） | 本项目**未接入**：`rg "elicitation|Elicitation"` 在 `crates/agentscope-harness/**` 0 命中，CLI 的 `ClientCapabilities` 构造（`acp_agent_cli.rs:718`）只设 `session{notices,compaction}` + `plan`，**未设 `elicitation`**；agent 侧亦未注册 `elicitation/create` handler。若接入，`ElicitationPropertySchema` 已覆盖完整表单控件（字段→控件映射见 §2.7.2a）；当前 agent 只能自建本地工具/走 `session/request_permission` |
| **会话元数据与分页**：`ListSessionsRequest.cursor` / `ListSessionsResponse.next_cursor`、`SessionInfo.title` / `updated_at` / `additional_directories`、`NewSessionRequest.additional_directories` | ≤ schema 1.7.0 | ✅ 已完成 | `coding_agent.rs::session_infos` 按 id 排序分页（PAGE_SIZE=50，`cursor` 为上一页末 id），回 `next_cursor`；`SessionInfo` 用真实 `cwd`/`additional_directories`/`title`/`updated_at`（`AcpAgentSession::session_info`，不再硬编码 `"."`）。`additional_directories` 从 `session/new`、`session/load`、`session/resume` 请求写入（`set_additional_directories`） |
| **未发送的 `SessionUpdate` 变体**：`AvailableCommandsUpdate` / `CurrentModeUpdate` / `ConfigOptionUpdate` / `SessionInfoUpdate` | ≤ schema 1.7.0 | ✅ 已完成（除 AvailableCommandsUpdate） | `set_mode`（`coding_agent.rs`）现在同推 `CurrentModeUpdate`；`set_config_option` 同推 `ConfigOptionUpdate`（完整 config options）；每轮 prompt 推 `SessionInfoUpdate`（`updated_at`，`session.rs::session_info_update`）。`AvailableCommandsUpdate` 未接（无命令目录来源，属设计决定） |
| **客户端会话恢复 builder**：stable `load_session*` / `resume_session*` → `RestoredSession` | SDK 2.1.0 | ✅ 已完成 | `acp_agent_cli.rs` 现读 `ACP_SESSION_ID`：非空时用 `cx.load_session(id, cwd)`（`ACP_SESSION_REPLAY` 置位时）或 `cx.resume_session(id, cwd)` 的 `.block_task().start_session().into_session()` 恢复；空则仍 `build_session(..).start_session()`。CLI 同时在 `initialize` 广告 `ClientCapabilities`（notices/compaction/plan），并渲染 `Notice`/`CompactionUpdate`/`CurrentModeUpdate`/`ConfigOptionUpdate`/`PlanUpdate`/`PlanRemoved` |
| **SDK 杂项工具**：`util::json_cast`、`MatchDispatchFrom`、`TypeNotification`、`JsonRpcBatch`、`MetaCapability`（`_meta.symposium` 能力协商）、`DynConnectTo`、`AcpAgent::with_debug` / `AcpAgentConfig` / `LineDirection`、NES（`agent-client-protocol` 2.2.0 的 `unstable` 聚合未转发 schema 的 `unstable_nes`） | ≤ SDK 1.3.0 | 设计决定（不迁移） | 全部 0 命中。**2026-10-16 措辞更正**：`LineDirection`（`acp_agent.rs:28`）是 **native-only stdio 行级调试回调**的参数（`AcpAgent::with_debug` / `Stdio::with_debug`，唯一消费者是 conductor 的 `DebugLogger`，标 Stdin/Stdout/Stderr 方向），与「嵌入式」无关 —— lib.rs 里说的 embedder 指「自带 runtime+transport 的宿主（含 wasm）」，正因 `AcpAgent`/`Stdio` 依赖子进程+阻塞线程，二者连同 `LineDirection` 在 wasm target 上不导出；原「本项目非嵌入式（`LineDirection`）」的因果表述有误，已改。`MetaCapability`（`capabilities.rs:31`）是**代理链** `_meta.symposium` 能力协商原语（`add/has/remove_meta_capability`，用于 Conductor↔Proxy 的 `proxy=true` 双向握手；`McpAcpTransport` 的 impl 已于 SDK 移除）。二者本项目均无对应角色（不做 Conductor/Proxy、也不用 debug 回调） |

#### 2.7.2a Elicitation Form 模式的表单渲染能力（✅ 2026-10-19 已接入）

> **依据**：`agent-client-protocol-schema` v1 `elicitation.rs` + 协议文档 `docs/protocol/v1/elicitation.mdx`（对应 MCP 2026-07-28 RC 的 elicitation）。

Form 模式用受限 JSON Schema（扁平 `object`）描述请求内容，`ElicitationPropertySchema` 变体逐一对应用户控件：

| Schema 类型（`ElicitationPropertySchema` 变体） | 关键字段 | 对应 UI 控件 |
|---|---|---|
| `String(StringPropertySchema)` | `minLength` / `maxLength` / `pattern` / `default` | 文本框；`format` 为 `email` / `uri` / `date` / `date-time` 时对应专用输入（邮箱 / URL / 日期 / 日期时间） |
| `String` + `enum` / `oneOf` | `EnumOption{value,title,description}` | 单选下拉 / 单选组（`enum` 与带标题的 `oneOf`） |
| `Number(NumberPropertySchema)` | `minimum` / `maximum` / `default` | 数字（浮点）输入，含 min/max 约束 |
| `Integer(IntegerPropertySchema)` | `minimum` / `maximum` / `default` | 数字（整数）输入，含 min/max 约束 |
| `Boolean(BooleanPropertySchema)` | `default` | 复选框 / 开关 |
| `Array(MultiSelectPropertySchema)` | `items`（`MultiSelectItems`）/ `minItems` / `maxItems` / `default` | 多选控件；`items` 三态：`String(StringMultiSelectItems)` = 纯字符串多选、`Titled(TitledMultiSelectItems)` = 带人类可读标签的多选、`Other(OtherMultiSelectItems)` = 未知判别的透传 |
| `Other(OtherElicitationPropertySchema)` | 保留 `type` 与其余字段 | **不得**当已知控件渲染（客户端须原样保留以便存储/回放/代理转发） |

**本项目落地（2026-10-19）**：
- agent 侧：`acp/elicitation.rs` 把该受限 schema 作为 LLM 工具 `ask_user` 暴露——模型给出 `message` + 扁平 `properties`（+ 可选 `required` / `title` / `description`），工具把它**逐字段映射**为 `ElicitationPropertySchema`（`enum` → 无标题单选、`oneOf:[{const,title,description}]` → 有标题单选、`items.enum` → 纯字符串多选、`items.oneOf`（线上写作 `anyOf`）→ 有标题多选）；不认识的 `type` / `format` / 组合一律**拒绝**（返回工具错误，让模型改述），不伪造控件。scope 用 `ElicitationSessionScope`，并把触发它的 `tool_call_id` 带上。
- 回传：`ElicitationAction::{Accept,Decline,Cancel}` → `ToolResultBlock`；accept 的 `content`（`ElicitationContentValue`）转成 JSON 对象文本回灌给模型；decline/cancel/传输失败各有明确文案，保证模型能安全兜底。
- client 侧：`acp_agent_cli.rs` 在 `initialize` 广告 `elicitation.form`，并在 `read_update` 的 dispatch 上用 `if_request::<CreateElicitationRequest>` 渲染表单（逐字段提示、展示 `default`、支持 `/decline` `/cancel`、空行取默认值）、回填 `ElicitationContentValue`。
- **能力门控（协议硬约束）**：`ClientSupport::supports_elicitation_form()` 在**创建会话、构建 agent 之前**判定；未广告时 `ask_user` **根本不注册**（模型看不到该工具），符合「不得请求未广告模式」。
- **子 agent 继承**：工具在 `HarnessAgent::build()` **之前**注册进 base toolkit，故 `SubagentBlueprint` 的 `toolkit.copy()` 快照含该工具 → 子 agent 也可推表单（主 agent 与所有叶 agent 共享同一 slot）。
- 协议约束遵守：`form` 模式**禁止**索取密码/密钥/token（工具描述已明示，模型侧提示走纯文本/URL）；值校验在映射层拒绝非法 schema。URL 模式未实现（CLI 只广告 form）。

#### 2.7.3 结论

- **2026-10-16 接线波次**：§2.7.2 中所有「待办（不包含暂缓）」项均已实现，harness 依赖改为逐个开启 6 个 unstable feature（`unstable_session_notices` / `unstable_session_compaction` / `unstable_plan_operations` / `unstable_llm_providers` / `unstable_end_turn_token_usage` / `unstable_mcp_over_acp`），**不**开聚合 `unstable`（它会连带开启暂缓的 `unstable_protocol_v2` / `unstable_session_fork`）。
- 仍为「设计决定（不迁移）」的：SDK 内建 MCP server 运行时、client 侧 fs/terminal（elicitation 已于 2026-10-19 接入，见 §2.7.2a）、SDK 杂项工具。仍为「待办（暂缓）」的：ACP v2 协议与 Proxy/Conductor 路由。`AvailableCommandsUpdate` 未接（无命令目录来源，无产品需求）。
- 协议合规要点：所有新增通知均**按 client 能力门控**（未广告则降级或静默），避免向不支持的客户端发送非能力更新。

> **⚠️ 本节基线已过期**：以上三节依据的是 SDK **2.2.0 / schema 1.9.1**。上游已发布 **5.0.0 / schema 1.15.1**（2026-11-02），v1 协议面新增 7 个方法、删除 13 个方法（document 族、NES、`mcp/connect` / `mcp/disconnect`），并有 3 项 Rust API 破坏性变更。**升级项与新增能力缺口统一见 §6**（`ACP-U01..U08` 升级阻断、`ACP-33..ACP-45` 新能力、`ACP-D08..D14` 设计决定、§6.7 批次计划）。本节保留为 2.2.0 轮的历史记录，不再更新。

## 3. 项目状态与下一步
### 3.1 当前状态
- 方案文档 `docs/ACP_AGENT_PLAN.md` 已创建。
- **ACP CodingAgent 核心已实现并通过 stdio 端到端验证**（initialize/new_session/prompt/close 全链路）。
- **`$/cancel_request` 取消已实现并通过进程内 e2e 验证**（`acp_cancel_test.rs`，无需真实 model）。
- **SDK v2.2.0 未接线能力已全部补完（除暂缓项）**：Session Notices、Compaction 通知、Plan 操作、Provider 方法、`PromptResponse.usage`、`CurrentModeUpdate`/`ConfigOptionUpdate`/`SessionInfoUpdate`、会话分页与元数据、MCP-over-ACP、CLI 会话恢复。详见 §2.7.2。
- **Elicitation 表单推送（`ask_user`）已实现并通过进程内 e2e 验证**（`acp_elicitation_test.rs`，2 passed）：`ask_user` → `elicitation/create`（含 `sessionId`/`toolCallId`/`requestedSchema`）→ client accept → 答案回灌模型；并含「未广告 elicitation 时不推送表单」的反向用例。
- 测试（2026-10-19 实测）：`cargo test -p agentscope-harness --lib` = **330 passed / 0 failed**；`acp_elicitation_test`（2）/ `acp_cancel_test`（1）/ `acp_event_flow_test`（1）/ `acp_session_resume_test`（1）四个集成测试全绿。`cargo build --workspace` = 0。
- `cargo build -p agentscope-harness` 通过（含两个 `[[bin]]`）；新增代码 **0 新增编译告警**（剩余 6 个告警均为既有）。
- 本次改动均只碰 `crates/agentscope-harness/**` 与 `docs/ACP_AGENT__GAP_TRACKER.md`；未改根 `Cargo.toml`/core/其它 crate。

### 3.2 下一步建议
1. **Phase 1**：建 `acp/` 模块 + Initialize 处理器 + connect_stdio，编译通过 ✅
2. **Phase 2**：会话生命周期（new/close/list/load/resume/delete）✅
3. **Phase 3**：Prompt 核心（ReAct 驱动 + 事件转换）✅
4. **Phase 4**：配置与取消 ✅（`$/cancel_request` 已完成）
5. **Phase 5**：权限 HITL（RequestPermissionRequest 往返，Ask 决策）✅
6. **Phase 6**：入口与测试 ✅
7. **Phase 7**：SDK v2.2.0 未接线能力接入（Notices/Compaction/Plan/Providers/MCP-over-ACP/usage/分页）✅（除暂缓项）
8. **待裁定/暂缓**：ACP v2 协议与 Proxy/Conductor 路由；`AdditionalDirectories` 是否需真正参与工具根目录约束（现仅回读展示）

## 3.5 2026-10-31 ACP 复核清单（唯一编号）

> **2026-10-31 合并更正（dev `5883c98`）**：`a_cancelled_turn_does_not_block_the_next_prompt` 的永久挂起**已修**，真因是用例硬编码 `sleep(400ms)` 后取消、与冷会话首次模型调用（0.7–1.0s）竞态；改为等模型调用计数 ≥1 再取消。故 **ACP-01 由 P0 降为 P1**：残留风险仅限「provider 永不返回且客户端永不取消」时 `running` 无上限（有意不加粗粒度超时，避免误杀健康长任务）。**【2026-10-31 再补**：只要是取消/关闭路径（`session/cancel`、`$/cancel_request`、`session/close`、LRU 淘汰）现在都能立即终止 `running` 中的 turn，包括“关闭后不再等 10s 结算宽限”与“排队中的 prompt 不再丢 cancel”；仅剩“客户端什么都不发且 provider 永不返回”的纯静默悬挂，按裁定不处理。见 ACP-01 行】**

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`ACP-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `ACP-DNN` / `ACP-XNN` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
>
> **Java 基线为零命中（本轮亲自复核）**：`rg -il 'acp' D:\IdeaProjects\agentscope-java -g '*.java'` → exit 1（无任何 `*.java` 含 ACP）；`Get-ChildItem -Recurse -Directory -Filter '*acp*'` → 0 个目录。因此本模块的「对齐」对象是 **ACP 协议/RFD 规范 + `agent-client-protocol` 2.2.0 能力面 + 本仓 `docs/ACP_AGENT_PLAN.md`**，不是 Java 代码；凡「Java 无对应」均属 **Rust 原生能力，不算迁移缺口**（见 §4.3.1）。
>
> 规模：`crates/agentscope-extensions-acp` 17 个 `src/*.rs`（≈9.5k 行）+ 6 个 `tests/*.rs`；源码内联测试 `#[test]`/`#[tokio::test]` 共 **75** 条，集成测试 **12** 条（`rg -c '#\[(test|tokio::test)\]'` 统计，非运行结果）。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| ACP-01 | P1 | **prompt 循环对 `running` 状态无上限等待**：`$/cancel_request` 未送达 agent 侧时，`session/prompt` 永不返回（生产表现为「输出中断后再说 continue 无任何响应」）。admission 看门狗只认 `queued`，`running` 没有任何上限 | 无（Rust 原生；此前在 `docs/extensions/HARNESS__GAP_TRACKER.md` 的 HARNESS-15 登记，本 crate 已独立成 crate，故迁到本条） | `session.rs:1849` 仅 `current.status == "queued"` 才触发 `ADMISSION_TIMEOUT`（`session.rs:1793`，300s）；`session.rs:1845` 又在**任何状态变化**时重置该 deadline，故 `queued→running` 后再无截止时间。内层 select 只有两处能跳出等待：`cancel_epoch` 比对（`session.rs:1832`）与 SDK `cancellation.cancelled()`（`session.rs:1880-1892`），两者都要求取消**已送达**。`cancel_and_wait`（`session.rs:1351`）与 `is_closed()`（`session.rs:1366`）存在于外层 admission 等待（`session.rs:1586-1612`）但**不在内层 select 里**。用例：`tests/acp_interrupted_turn_test.rs:62 a_cancelled_turn_does_not_block_the_next_prompt`（Windows 永久挂起；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §4.2-1 记 `f81c8b1` 即复现） | 已完成 | ①把 `admission_deadline` 推广为「**无进展上限**」：progress = 收到任一 `AgentEvent` 或 `status` 变化（事件驱动重置，默认 30min，`AGENTSCOPE_ACP_TURN_TIMEOUT` 可调）；超时先 `agent.interrupt_run(run_id)`，10s 宽限后 `abandon_turn`，发一条 `[turn timed out]` chunk 并返 `StopReason::Refusal`；②内层 select 增 `closed` 轮询臂（`is_closed()` 已有），使 `session/close`/LRU 淘汰能无条件跳出；③prompt handler（`coding_agent.rs:583-640`）另建 turn 级取消令牌，由 `AcpAgentSession::cancel()` 与 `$/cancel_request` 共同触发，让两条取消路径收敛。验收：该用例单跑 + `cargo test -p agentscope-extensions-acp -j 2` 各 8 次全绿；`EXTENSIONS_GAP_TRACKER.md:115` 的 `--skip a_cancelled_turn_does_not_block_the_next_prompt` （以上为原计划，① 保留为「不做」）。**【2026-10-31 落地：②内层 select 增 `closed` 轮询臂（并入 `cancelled` 分支），`closed` 为真时直接 `abandon_turn`、不再等 10s 结算宽限——`session/close`/LRU 淘汰不再被挂死的 provider 拖住（`coding_agent.rs` 的 `CloseSessionRequest` 同步改为先 `close()` 再 `cancel_and_wait()`：否则宽限仍会在 closed 置位前发生）；③turn 代次改为在 admission 等待**前**捕获，并加 `cancel_epoch_moved` 轮询臂，排队中的 prompt 也能被 session 级 `cancel()` 立即终止（此前排队期间收到的 cancel 被静默丢弃、该轮照跑）；①无进展看门狗按裁定**不做**（粗粒度超时误杀健康长任务）。代码：`session.rs::run_prompt_inner` / `cancel_epoch_moved`；测试：`tests/acp_session_cancel_test.rs::closing_a_session_stops_a_running_turn_instead_of_stalling_it`、`::a_cancel_while_a_prompt_is_queued_stops_it_before_it_runs`（修复前：前者 3s 超时失败、后者等满 10s 后模型被调用两次）】** |
| ACP-02 | P1 | **MCP 连接生命周期未收尾**：`mcp/disconnect` 从不发送；stdio/SSE/HTTP MCP 子进程不断开；桥接进 toolkit 的工具从不摘除（会话关闭后工具仍指向死连接） | 无（Rust 原生） | `mcp_over_acp.rs:163` 定义 `McpAcpConnection::disconnect()`，全 crate `rg 'disconnect\('` 仅命中定义与模块文档；`session.rs:434`（字段）+ `session.rs:1178`（push）只增不减，`coding_agent.rs` 的 `CloseSessionRequest`（`:734`）/`DeleteSessionRequest`（`:840`）/LRU 淘汰（`:310-338`）均不 drain。harness 侧 `McpClientManager::disconnect`（`crates/agentscope-harness/src/agent/mcp_client_manager.rs:308`）在 acp crate 内零调用（`rg 'mcp_manager\.'` 只命中 `session.rs:1085`/`session.rs:1097`），`mcp_client_manager.rs` 无 `impl Drop`（零命中） | 待办 | ①`AcpAgentSession::release_mcp(&self)`：遍历 `mcp_acp_connections` 逐个 `disconnect()` 并 `Toolkit::remove_tool(<server>__<tool>)`；②在 `CloseSessionRequest`/`DeleteSessionRequest`/`register_session` 淘汰分支调用；③对 stdio/sse/http 记 `client_id` 后调 `mcp_manager.disconnect(client_id)`；验收：新增离线 `tests/acp_mcp_disconnect_test.rs`（in-process `Channel::duplex` + 假 MCP client）断言关闭/删除会话后客户端收到 `mcp/disconnect`、toolkit 不再含该服务器工具 |
| ACP-03 | P1 | **`session/load` 不回放历史**：`AgentCapabilities.load_session(true)` 已广告，但 handler 只回 `modes`/`config_options`，一条 `session/update` 都不发（客户端重启后 UI 为空、agent 却带着上下文） | 无（Rust 原生；ACP v1 `loadSession` 要求用 `session/update` 重放整段会话） | `coding_agent.rs:240 .load_session(true)`；handler `coding_agent.rs:784-811` 的第三个参数是 `_cx`（**连接未被使用，物理上无法回放**），响应只有 `modes(...).config_options(...)`；`rg 'UserMessageChunk'` 在 crate 内零命中，`rg 'session_transcript'` 与 `rg 'inspect_session'` 在 crate 内亦零命中（harness 侧能力存在：`harness_agent.rs:277 session_transcript`、`:252 inspect_session`） | 已完成 | 二选一：**(A 首选)** handler 改用 `cx`，先 `agent.inspect_session(&ctx).await` 取投影，按轮次发 `UserMessageChunk`/`AgentMessageChunk`（工具调用发 `ToolCall`+`ToolCallUpdate`）再响应；验收：新增 `tests/acp_load_replay_test.rs`（离线 model）断言 `session/load` 后收到的 chunk 文本序列等于写入时 transcript。**(B)** 若产品决定不回放，则必须 `load_session(false)` 并把 `session/load` 改为 `method_not_found`，避免客户端按协议期待重放 【裁定 2026-10-31：A：handler 改用 cx + agent.inspect_session 回放 chunk】。**【2026-10-31 落地（A）**：`coding_agent.rs` 的 `LoadSessionRequest` handler 改用 `cx`，回答前逐条 `cx.send_notification` 回放；`session.rs` 新增 `AcpAgentSession::replay_notifications()`：先 `HarnessAgent::inspect_session` 作为「已提交 journal 可读」闸门（其投影只含 `state_json`、无消息列表），再按 `HarnessAgent::session_transcript` 的消息顺序发 `UserMessageChunk` / `AgentMessageChunk`（每条历史消息一个 `history-<id>` 的 `messageId` 便于客户端聚合），`ToolUse` 发已完成的 `ToolCall`（带 `raw_input`、`status=Completed`）。验收测试：`tests/acp_load_replay_test.rs::session_load_replays_the_conversation_as_chunks`（脚本化 model，断言 `session/load` 后先收到用户消息、随后收到 agent 回答）】** |
| ACP-04 | P1 | **`session/delete` 不删持久状态**：只从内存注册表移除，journal 与 `.agentscope/acp-sessions/<id>.json` 仍在 → `session/list` 仍列出该会话、`session/load` 仍能成功（删除语义形同「关闭」） | 无（Rust 原生） | `coding_agent.rs:840-850`：`cancel_and_wait` → `session.close()` → `remove_session`，无任何存储删除；`session.rs:451-457 session_config_path` 指向的 sidecar 亦无删除路径。根因在跨 crate：`SessionLogStore`（`crates/agentscope-core/src/session/log.rs:199-214`）只有 `open`/`list`，**没有 `delete`**（`rg 'fn delete' crates/agentscope-core/src/session` 零命中） | 待办 | 【阻塞：等待 core/harness 批次1/2 的 `SessionLogStore::delete`】跨模块需求（记 §4.2 类）：给 core `SessionLogStore` 增 `async fn delete(&self, key, context) -> Result<()>`（默认实现返 unsupported），`WorkspaceSessionLogStore`/`StoreSessionStorage` 实现之；acp 侧在 `DeleteSessionRequest` 调 `store.delete(...)` + `std::fs::remove_file(config_path)`。若 core 暂不扩 trait，则 acp 侧只能降级为「标记 tombstone」，需产品确认；验收：删除后 `session/list` 不再列出、`session/load` 返回 `invalid_params` 【裁定 2026-10-31：A：core SessionLogStore::delete 默认 Err(unsupported) + acp 调 delete + 删 sidecar】 |
| ACP-05 | P2 | 集成测试依赖**预构建 bin**：`acp_stdio_thought_test` 断言 `target/<profile>/acp_harness_agent.exe` 已存在，`cargo test --test acp_stdio_thought_test` 单独跑会 panic | 无（Rust 原生） | `tests/acp_stdio_thought_test.rs:50-72 agent_exe()`（回退到 `current_exe()` 的 profile 目录并 `assert!(candidate.exists())`） | 待办 | 改为 `env!("CARGO_BIN_EXE_acp_harness_agent")`（cargo 保证先构建该 bin），保留 `ACP_AGENT_EXE` 覆写；验收：清空 target 后直接 `cargo test -p agentscope-extensions-acp --test acp_stdio_thought_test` 通过。**顺带更正历史口径**：该测试**已不再是 live 模型 flaky**——`tests/acp_stdio_thought_test.rs:96-199` 起内置脚本化 OpenAI 兼容 `TcpListener`+SSE（固定吐 `reasoning_content`+`content`），并把 `HOME`/`USERPROFILE`/`AGENTSCOPE_LOG_DIR` 重定向到临时目录；`MEMORY.md` 与旧 HARNESS-16 条目所述「依赖 provider 返回 reasoning」已失效 |
| ACP-06 | P2 | **prompt 错误映射不符合协议**：未知 `sessionId` 回 `PromptResponse(Refusal)`（协议要求 JSON-RPC `invalid_params`）；`cx.spawn` 失败时 `responder` 被丢弃，客户端收不到任何响应（静默挂起） | 无（Rust 原生） | `coding_agent.rs:589-593`（`None => return responder.respond(PromptResponse::new(StopReason::Refusal))`）；`coding_agent.rs:634 cx.spawn(...)?` 的 `?` 在 spawn 失败时把 `responder` 随闭包一同丢弃 | 待办 | 未知会话改为 `responder.respond_with_error(Error::invalid_params().data("unknown session"))`；`cx.spawn` 失败改为 `respond_with_error(Error::internal_error())`；验收：新增 2 条集成断言（未知 sessionId → JSON-RPC error；spawn 失败路径用注入型 `cx` 不可得时至少单测 `describe_error_mapping`） |
| ACP-07 | P2 | **探活/列举为一次性构建完整 `HarnessAgent`**：`session/load` / `session/resume` 的前缀判断每次都跑一次 `session_factory`（SQLite 打开 + workspace 初始化 + skills 扫描），`session/list` 另为每个 cwd 建并缓存一个「catalog agent」（上限 4） | 无（Rust 原生） | `coding_agent.rs:361-388 session_exists()`（`factory(...)` 只为拿 `session_log(&ctx).head()`）；`coding_agent.rs:462-495 catalog_agent()`；`MAX_CATALOG_AGENTS` = `coding_agent.rs:44` | 待办 | `session_exists` 改为直接用 `session_log_store`（`HarnessAgent::list_session_keys` 同源）或 `WorkspaceSessionLogStore::open(...).head()` 探活，不建 agent；`catalog_agent` 仅在确有需要时 lazy 建；验收：`session/load` 冷路径不再出现一次 `HarnessAgent::build`（可在 `session_factory` 内加计数断言） |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | 参考（ACP 规范 / 计划文档） | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| ACP-20 | `session/new`：`SessionId` 用 UUID（避免毫秒碰撞）、返回 `modes`+`config_options`、桥接请求内 MCP 服务器 | `ACP_AGENT_PLAN.md` §4 Phase 2 | `coding_agent.rs:557-580`；`tests/acp_event_flow_test.rs`（3 个用例） | 已完成 |
| ACP-21 | prompt 生命周期：幂等键 + 提交重试、收据轮询、`interrupted` 自动续跑（≤2）、`failed`/`rejected`/非终态各自的 `StopReason` 与错误 chunk | `ACP_AGENT_PLAN.md` §4 Phase 3 | `session.rs:1576-2236`（`prompt_idempotency_key` `session.rs:524`；`MAX_INTERRUPTED_RESUMES` `session.rs:1804`；`interrupted` 分支 `session.rs:2147`）；`tests/acp_interrupted_turn_test.rs` 3 个用例（含 `a_forced_interrupt_is_continued_without_discarding_the_prompt`、`a_failed_turn_reports_the_error_and_does_not_wedge_the_session`） | 已完成 |
| ACP-22 | 权限 HITL：`session/request_permission` 四选项（ALLOW_ONCE/ALWAYS/REJECT_ONCE/ALWAYS）、always 规则落盘 `{cwd}/.agentscope/permissions.json`、按 mode 叠加 permission context | `ACP_AGENT_PLAN.md` §4 Phase 5 | `session.rs:174-201`（选项）、`session.rs:219-313 acp_confirm_handler`、`session.rs:202-217 always_rule_for`、`session.rs:927-950 apply_mode`、`permission_store.rs:44-70 add()`（tmp+rename 原子写）；测试 `permission_store.rs` 3 条（`add_then_load_roundtrips` 等）+ 端到端 `tests/acp_elicitation_test.rs:297-312`（客户端应答后工具继续执行） | 已完成 |
| ACP-23 | MCP-over-ACP：`mcp/connect` → `tools/list` / `tools/call` → 桥接 `McpTool` 进会话 toolkit；client 声明 `McpServer::Acp` 且 `McpCapabilities.acp(true)` | SDK `unstable_mcp_over_acp`；`mcp_over_acp.rs` 模块文档 | `coding_agent.rs:247 .mcp_capabilities(...acp(true))`；`session.rs:1013-1135 apply_mcp_servers` 的 `McpServer::Acp` 分支 → `session.rs:1138-1210 apply_acp_mcp_server`；`mcp_over_acp.rs:99-121 connect`、`:127-158 request/call_tool`、`:180-260 bridge_tools`；单测 `mcp_over_acp.rs` 4 条（`parses_tools_list_and_skips_malformed_entries` 等）。**注意**：连接收尾缺失，见 ACP-02 | 已完成（收尾见 ACP-02） |
| ACP-24 | compaction 通知：`AgentEvent::Custom(kind=compaction)` → `CompactionUpdate(InProgress)` → `CompactionSummaryChunk` → `CompactionUpdate(Completed)`，仅在 client 广告 `session.compaction` 时发送 | SDK `unstable_session_compaction` | `event.rs:87-122 compaction_notifications`、`event.rs:124-148 compaction_marker`、`session.rs:1952-1980`（`if self.support.supports_compaction()`）；门控 `support.rs:75-83`；测试 `event.rs:1053 compaction_notifications_go_in_progress_then_completed`、`event.rs:1077 compaction_marker_is_recognized_and_other_custom_events_are_not`、`support.rs:session_sub_capabilities_are_detected_independently` | 已完成 |
| ACP-25 | 取消：`$/cancel_request`（`Responder::cancellation()`，handler 用 `cx.spawn` 移出 dispatch loop）与协议通知 `session/cancel`（`CancelNotification` → `session.cancel()`）双路径；按 turn 代次（`cancel_epoch`）只取消当轮 | ACP v1 `session/cancel` + `$/cancel_request` | `coding_agent.rs:599 cancellation = responder.cancellation()`、`:605-637 cx.spawn(...)`、`:646-654 on_receive_notification<CancelNotification>`；`session.rs:1332-1336 cancel()`、`:1621-1622 epoch 捕获`、`:1832 epoch 比对`；测试 `tests/acp_cancel_test.rs` 2 条（`cancel_request_stops_prompt_with_stop_reason_cancelled`、`session_cancel_notification_stops_the_prompt`）。**注意**：送达失败时的兜底缺失，见 ACP-01 | 已完成（兜底见 ACP-01） |
| ACP-26 | 6 个 unstable feature 逐个开启（不启聚合 `unstable`，以免连带 `protocol_v2`/`session_fork`） | SDK 2.2.0 | `Cargo.toml:10-24`（`unstable_session_notices` / `unstable_session_compaction` / `unstable_plan_operations` / `unstable_llm_providers` / `unstable_end_turn_token_usage` / `unstable_mcp_over_acp`） | 已完成 |
| ACP-27 | stdio 传输与真实二进制入口（`run_stdio()` + 两个 `[[bin]]`） | `ACP_AGENT_PLAN.md` §4 Phase 1/6 | `coding_agent.rs:497-505 run_stdio`；`Cargo.toml:44-51`（`acp_agent_cli`/`acp_harness_agent`）；测试 `tests/acp_stdio_thought_test.rs:200 stdio_agent_streams_thinking_to_client`（脚本化 SSE，断言 THOUGHT 与 MESSAGE 双到达） | 已完成 |
| ACP-28 | 能力门控 + provider/support 模块：`ClientSupport` 记录 `initialize` 能力，notice/compaction/plan/elicitation 全部按广告位判定；`providers/list` / `providers/set` / `providers/disable` 仅在 registry 非空时广告 | SDK `unstable_llm_providers` 等 | `support.rs:16-124`（`supports_notices`/`supports_compaction`/`supports_plan_operations`/`supports_elicitation_form`/`is_intellij_2026`），`support.rs` 5 条单测；`provider.rs:15-160`（`ProviderRegistry::{list,set,disable,current}` + `required` 保护），3 条单测；`coding_agent.rs:543-549`（非空才加 `ProvidersCapabilities`）、`:858-888` 三 handler | 已完成 |
| ACP-29 | `PromptResponse.usage`：每轮重置累加，空轮返回 `None`；`UsageUpdate`（上下文窗口）在每次 `ModelCallEnd` 后推送 | SDK `unstable_end_turn_token_usage` | `session.rs:329-343 turn_usage_to_response`、`:2255-2268 accumulate_turn_usage/turn_usage_response`、`:2270-2282 usage_notification`、`:2013-2040`（`ModelCallEnd` 分支）；`coding_agent.rs:629 PromptResponse::new(stop).usage(usage)`；测试 `session.rs:2362 an_empty_turn_reports_no_usage`、`:2367 turn_usage_maps_to_the_protocol_shape` | 已完成 |
| ACP-30 | 自定义扩展方法 `_agentscope/session/respond`（回答 `external_execution`/`confirmation` 挂起请求并续跑）——ACP 标准面没有「客户端回答」入口，属协议缺口的窄化扩展 | ACP 扩展命名规则（`_` 前缀） | `ext.rs:21 RESPOND_METHOD`、`ext.rs:38-66 to_session_answer`（输出/批准互斥校验）、`ext.rs:100-140 JsonRpcMessage 实现`；接线 `coding_agent.rs:723-727` → `:910-941 handle_ext_respond`；6 条单测（`the_method_is_namespaced_as_an_extension`、`an_ambiguous_answer_is_rejected` 等） | 已完成 |
| ACP-31 | elicitation（`ask_user`）：仅在 client 广告 `elicitation.form` 时**注册工具**（构建前注入，子 agent 继承），accept/decline/cancel 映射回 `ToolResultBlock` | ACP `/ elicitation` v1 | `elicitation.rs`（`AskUserTool` + `ElicitationClient` seam + `ElicitationSlot`，6 条单测）；门控 `acp_harness_agent.rs:1072-1086`；连接绑定 `session.rs:952-966 set_client`；端到端 `tests/acp_elicitation_test.rs` 2 条（含「未广告不推送」反向用例） | 已完成 |
| ACP-32 | 客户端取消/关闭的清理链：`CloseSession` 先 `cancel_and_wait(15s)` 再 `close()`（防止晚到的 prompt 把会话写回注册表）；进程退出前排空所有会话 | ACP 生命周期约定 | `coding_agent.rs:734-748`（close）、`:840-849`（delete，含 `cancel_and_wait(10s)`）；`session.rs:1339-1349 close()`（置 `closed` + 取消 + 释放 dispatcher）、`coding_agent.rs:299-305 register_session` 拒绝复活已关闭会话；`acp_harness_agent.rs:1264-1330`（信号/EOF 排空） | 已完成 |

## 4. 执行状态约定（所有会话必须遵守）
- 开工前：读本文件 + `git status` + 确认 `cargo build` 基线。
- 改动中：保持每阶段 `cargo build`/`cargo test` 通过。
- 完工后：把对应条目 `执行状态` 改为 `已完成`，更新 `交接信息`（验证结果）。
- 涉及 SDK 参考时：以本地 `D:\IdeaProjects\agentclientprotocol\rust-sdk`（**tag `v5.0.0`**）源码与 cargo registry 中 `agent-client-protocol-5.0.0` / `agent-client-protocol-schema-1.15.1` 为准；升级前实现仍以 `2.2.0` / `1.9.1` 为准（两套口径并存期间，条目里必须写明是哪一套）。旧的 `schema-1.6.0` 口径已作废。

---

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> **前提**：Java 侧**没有 ACP 模块**（本轮复核：`rg -il 'acp' D:\IdeaProjects\agentscope-java -g '*.java'` exit 1；`*acp*` 目录 0 个）。因此 4.3.1 全部为 **Rust 原生能力，非迁移缺口**（对标的不是 Java，而是 ACP 规范 /`agent-client-protocol` 2.2.0 的能力面）；4.3.2 记录 Rust 与 **Java harness 交互面** 的差异（同一能力两侧都有，但接法与语义不同）。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| ACP-D01 | ACP v2 协议与 `session/fork`（`unstable_protocol_v2` / `unstable_session_fork`） | 无（Java 无 ACP 模块） | 两个 feature **故意不开**：`Cargo.toml:10-19` 逐个开启 6 个 unstable feature 并注明「不用聚合 `unstable`，因为它还含本项目明确暂缓的 `unstable_protocol_v2` / `unstable_session_fork`」；`rg '\.v2\(\)'`、`V2ConnectionTo`、`V2SessionBuilder` 三个模式在 crate 内均零命中 | 仅说 stable v1；v2-only 客户端或需要 fork 的客户端用不了。启用成本：开 feature + 重跑全部集成测试 |
| ACP-D02 | Proxy / Conductor / 多协议路由（`Proxy`、`ProxyProtocolRouter`、`AgentProtocolRouter`） | 无 | `rg 'Proxy'` / `protocol_router` / `AgentProtocolRouter` 在 crate 内均零命中；本 crate 只做「进程内 ACP Agent 服务端」（`coding_agent.rs:509 run_with_transport` 单一 transport） | 不能做 ACP↔其他协议的网关/代理；多客户端共享一个 agent 需宿主另做 |
| ACP-D03 | SDK 内建 MCP *server* 运行时（`agent_client_protocol::mcp_server` / `McpServerConnect`） | 无 | 本仓 MCP **client** 已由 `rust-mcp-sdk` 承担（`crates/agentscope-harness/src/agent/mcp_client_manager.rs`，acp 侧只调 `create_client`/`connect`，见 `session.rs:1085`/`1097`）；SDK 的 mcp_server 半边与本仓职责重叠（`McpServerConnect` / `mcp_server::` 在 crate 内零命中） | 不对外提供 ACP MCP server；只作为 ACP 客户端声明的 MCP 服务器的消费方 |
| ACP-D04 | 客户端侧文件系统与终端（`fs/read_text_file`、`fs/write_text_file`、`terminal/create` 等六个 `terminal/*` 方法） | 无 | `ReadTextFile` / `WriteTextFile` / `CreateTerminal` / `TerminalOutput` / `WaitForTerminalExit` / `KillTerminal` 六个符号在 `crates/agentscope-extensions-acp` 零命中（全仓 `rg` 亦零命中）；agent 自带本地 fs/shell 工具（`acp_harness_agent.rs:935 build_agent` 装配 filesystem + shell），并且 `initialize` 的 `ClientCapabilities` 也不广告这两组能力 | 工具读写文件不走 IDE 的 fs 代理，因此 IDE 侧「未保存缓冲区」内容不可见；换来的好处是工具审批/权限/沙箱完全在 agent 自身策略内（不经客户端绕行） |
| ACP-D05 | `SessionUpdate::AvailableCommandsUpdate`（斜线命令目录） | 无 | `rg 'AvailableCommandsUpdate'` 在 crate 内零命中；无命令目录来源（本 agent 的「命令」是工具与 skills，不映射为 ACP 命令） | IDE 侧无法用 `/` 列出 agent 命令；skills 仍可经自然语言/工具触发 |
| ACP-D06 | elicitation 的 URL 模式（仅实现 form 模式） | 无 | `support.rs:104-113 supports_elicitation_form()` 只认 `elicitation.form`；`rg 'ElicitationUrlCapabilities'` 在 `src/` 内只出现在 `support.rs` 的**测试**（用于验证 URL-only 不开 form）；CLI 只广告 `form` | 无法让客户端开浏览器/外部页面收集输入；form 模式已覆盖 `ask_user` 全部需求 |
| ACP-D07 | SDK 杂项能力：`MetaCapability`（`_meta.symposium` 代理链协商）、NES、`AcpAgent::with_debug`/`LineDirection`、`JsonRpcBatch`、`DynConnectTo` | 无 | 与 §2.7.2 同一条目；本 crate `MetaCapability` / `LineDirection` / `JsonRpcBatch` / `DynConnectTo` / `with_debug` 全部零命中，两类角色的宿主都不存在（不做 Conductor/Proxy；不做行级 debug 日志回调，日志走 `acp_harness_agent.rs:1097 init_logging` 写文件） | 无协议能力损失；缺的是调试/互联互通工具链 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| ACP-X01 | compaction 通知通路 | `ConversationCompactionMiddleware` 压缩完成后发一条 band-in 事件 `AgentEvent::Custom{kind:"compaction", summary}`（`event.rs:124-148 compaction_marker`），`session.rs:1952-1980` 识别后在客户端广告 `session.compaction` 时转成 `InProgress → SummaryChunk → Completed`（`event.rs:87-122`） | Java 侧压缩用回调/中间件直接通知；Rust 改为**经事件总线**，中间件不感知 ACP | 中间件零 ACP 依赖，可用纯函数单测（`event.rs:1053`/`:1077`）；能力门控集中在 session 层一处，未广告时静默（协议要求） |
| ACP-X02 | 工具审批回环（HITL） | core `ConfirmHandler` **内联 await**：`session.rs:982-1010 install_confirm_handler` → `session.rs:219-313 acp_confirm_handler`（发 `session/request_permission` 并映射四选项）；外部工具/未完成的 `external_execution` 则走窄化扩展方法 `_agentscope/session/respond`（`ext.rs:21` + `coding_agent.rs:910-941`） | Java 用 `GenerateReason::PERMISSION_ASKING` + `METADATA_CONFIRM_RESULTS` 的 32 轮上限恢复；Rust core 无此路径，改内联 await（无轮次上限） | 语义等价、不占模型轮次；ACP 没有「客户端回答挂起工具」的标准入口，故用 `_` 前缀扩展而非拓宽标准面（`ext.rs` 模块文档说明） |
| ACP-X03 | 权限请求失败/未知选项的处理 | 传输失败 → **allow**（`session.rs:290-306`，注释明确「mirror the Java behaviour of auto-approving when ClientSessionOperations is absent」）；未知 `option_id` → allow 并 `tracing::warn!`（`session.rs:277-289`）；`RequestPermissionOutcome::Cancelled` → deny | Java harness 在「无 ClientSessionOperations」时自动批准；Rust 在「有 client 但请求失败」时也归到同一类 fail-open | 避免 IDE 断连一次就让工具永久卡住/整轮失败；风险已被 `warn!` 显式化。**如果安全 posture 要求 fail-closed，此处是唯一需要改的点**（改一行 `ConfirmResult::denied`） |
| ACP-X04 | 思考等级下发 | `session.rs:315-327 thought_level_params`：`auto`/空 → 不发；`off/disabled/none` → `reasoning_effort:"none"`；其余原样；**永不**写 `enable_thinking`/`thinking_budget` | Java/DashScope 原生 API 历史上用 `thinking_budget`；Rust 统一收敛到 `reasoning_effort` | 严格 OpenAI 兼容网关会把未知字段当 400（注释举 Go/Zen 网关 `json: unknown field "thinking_budget"`）；ACP 侧也因此可用任意模型 catalog 而不分厂商 |
| ACP-X05 | 事件背压 | 有界队列 4096（`event_queue.rs:22`），溢出**丢最新**并计数（`event_queue.rs:60-90`），`AgentResult` 永不丢（`event_queue.rs:63`）；轮末打 `[some streamed output was dropped…]`（`session.rs:2203-2220`） | Java harness 无 ACP 推送面（无此问题）；Rust 旧实现用无界 channel，慢客户端会无限涨内存 | 慢 IDE 不能把 agent 拖进 OOM；丢包被计数并**告诉用户**，不静默截断（`dropped_deltas` 同时进 `tracing::warn!`） |
| ACP-X06 | 会话注册表容量与会话列表 | `max_sessions` 上限，满则淘汰最旧**空闲**会话、优先保留运行中会话（`coding_agent.rs:293-338`）；`session/list` 合并「内存注册表 + 持久 session 索引」并去重、按 `updated_at` 倒序、`PAGE_SIZE=50` 游标分页（`coding_agent.rs:390-460`、`sort_session_infos` `:945`） | Java harness 无 ACP 会话注册表（无上限、无游标分页） | 每个会话持有一整套 `HarnessAgent`（toolkit+SQLite），无上限会成倍放大内存（注释即为嵌入式优化动机）；过期会话仍能从持久索引发现（`list_session_keys`） |
| ACP-X07 | journal 保留与录制默认值 | 宿主默认把 `AGENTSCOPE_SESSION_RETENTION` 设为 **12h**（`retention.rs:19-30`，仅当环境变量缺失时）；`request/prepared` 全量请求负载默认**不记**（`recording.rs:28-36`，`AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS=1` 可恢复） | core 默认保留 30 天；Java harness 默认记录完整请求负载 | 模块注释给出实测：事件日志每次 model call 存一份全上下文 checkpoint，**单会话 1 小时 700MB**，冷启动要 hydrate；两者都只是默认值，可环境变量覆盖，无功能损失 |
| ACP-X08 | 单会话 prompt 串行化 | `prompt_lock` + turn 代次：晚到的 follow-up **排队**而不是被拒（`session.rs:1586-1612`）；只对 `turn_unfinished` 这类真拒绝给用户文案（`session.rs:2102-2119`） | Java 侧并发 turn 直接拒（`turn_unfinished`）；Rust 旧实现同样会拒，导致「Stop 后马上再发」丢输入 | 客户端（IDE）确实会在上一响应未到时发 follow-up；排队 + 代次让取消只作用于当轮（`session.rs:1332-1336` / `:1832`），不会误杀新轮 |

## 5. 2026-10-22 crate 迁移

`src/acp/` 已整体迁至 `crates/agentscope-extensions-acp/`（未做 trait 抽象，仍是 `HarnessAgent` 专用）：

- `agentscope-harness` 删除 `pub mod acp`、`[[bin]]` 与 `agent-client-protocol` / `tracing-subscriber` / `tokio/signal` 依赖。
- 两个 bin：`cargo run -p agentscope-extensions-acp --bin acp_harness_agent|acp_agent_cli`。
- 5 个 `acp_*` 集成测试与 46 个内联测试随 crate 迁移；`ACP_CONTEXT_WINDOW_TOKENS` 被 harness 测试引用，已在测试内本地化为常量，避免 dev-dependency 环。
- feature 透传：`default = ["rusqlite"]` → `agentscope-harness/rusqlite`；`lite` 对应 harness `lite`。`cargo check -p agentscope-extensions-acp --no-default-features --features lite --bins` 通过。

---

## 6. 2026-11-03 SDK 复核：`agent-client-protocol` 2.2.0 → 5.0.0（schema 1.9.1 → 1.15.1）

> **触发**：上游 SDK 大版本更新。本地源码 `D:\IdeaProjects\agentclientprotocol\rust-sdk` 当前 `HEAD = 050cd94`、最新 tag **`v5.0.0`（2026-11-02）**；版本序列 `2.2.0 → 3.0.0 → 3.6.0 → 5.0.0`（**没有 4.x**，`1b1399a chore: release v5.0.0`）。
> **本仓基线**：`crates/agentscope-extensions-acp/Cargo.toml:19` 仍 pin `agent-client-protocol = { version = "2.2.0", features = [6 个 unstable] }`，传递 schema **1.9.1**；5.0.0 精确 pin schema **`=1.15.1`**（`agent-client-protocol-5.0.0/Cargo.toml:345` 区）。
> **证据来源**（本轮逐个读过，不转述二手资料）：cargo registry 中 `agent-client-protocol-2.2.0` / `-5.0.0`、`agent-client-protocol-schema-1.9.1` / `-1.15.1` 源码（`git diff --no-index` 逐文件比对）；`rust-sdk/md/` 下 `migration-v5.md`、`migration-v3.md`、`migration-stateless-mcp.md`、`migration-connection-drivers.md`、`migration-config-api.md`、`features.md`、`versioning.md`、`protocol-v2.md`、`protocol-v2-quickstart.md`、`http-transport.md`、`transport-architecture.md`、`request-cancellation.md`、`ordered-application-dispatch.md`、`sending-requests.md`、`logging-privacy.md`、`session-operation-coordination.md`、`conductor.md`、`mcp-bridge.md`、`mcp-over-acp.md`；`src/agent-client-protocol/CHANGELOG.md`。
> **方法**：①`rg 'METHOD_NAME: &str'` 枚举两版 v1 协议方法求差集；②`rg '^pub (struct|enum|fn)'` 求类型差集；③`git diff --no-index` 看字段级变化；④在本 crate 内 `rg` 判定「零命中 = 未接线」。
> **编号**：本节新增三组前缀——`ACP-U**`（升级阻断项，必做，非新特性）、`ACP-33+`（新能力缺口，接续 §3.5 的 `ACP-32` 单调递增）、`ACP-D08+`（设计决定，接续 §4.3.1 的 `ACP-D07`）。历史编号一律不复用。

### 6.1 版本与工程面事实（升级前必须知道）

| # | 项 | 现状（2.2.0 / schema 1.9.1） | 新基线（5.0.0 / schema 1.15.1） | 对本项目的影响 |
|---|---|---|---|---|
| 1 | crate 版本 | `agent-client-protocol 2.2.0` | **`5.0.0`**（全部 SDK crate 同版本号；无 4.x） | 跨两个语义化大版本，必须按 6.3 逐项改 |
| 2 | schema 版本 | 传递 `agent-client-protocol-schema 1.9.1` | 精确 `=1.15.1` | v1 协议面 +7 / −13 个方法（6.2.1） |
| 3 | edition / MSRV | schema 1.9.1 已 `edition 2024` / `rust-version 1.88`；本 workspace `edition 2021` / `rust-version 1.85.0`（根 `Cargo.toml:40,47`） | 5.0.0 同 `edition 2024` / MSRV `1.88` | 我们 crate 可继续 `edition 2021`（依赖可混用 edition），但**声明的 `rust-version` 必须 ≥1.88**，否则与实际下限不符；本机 stable 1.97.1 满足要求 |
| 4 | 默认 feature | `default = []`（3.0 起），但**原生传输在 native 平台无条件编译** | `default = []` 且**原生传输改为 opt-in**：`stdio` → `Stdio`；`process` → `AcpAgent` / `AcpAgentConfig`；`schemars` → `McpTool`/`McpToolRegistry`/`tool_fn` | **不加即编译失败**：`Stdio::new()`（`coding_agent.rs:591`）需要 `stdio`；`AcpAgent::from_args` / `AcpAgent::new`（`acp_agent_cli.rs:79,91`、`tests/acp_stdio_thought_test.rs:220`）需要 `process` |
| 5 | unstable feature 删除 | `unstable_session_notices`、`unstable_session_compaction` | 二者**转正为稳定**（schema 1.11 起），同名 feature **不复存在** | Cargo.toml 必须删掉这两个 feature 名（否则 cargo 报 `feature ... does not exist`）；`support.rs`/`session.rs`/`event.rs` 的 Notice/Compaction 代码**零改动** |
| 6 | unstable feature 删除 | `unstable_nes` | **NES 整体移除**：`v1/nes.rs`（3356 行 / 55 个公开类型）、`nes/*` 6 个方法、`ClientCapabilities.nes`、`AgentCapabilities.nes`、`ClientNesCapabilities` 全部消失，**无替代物** | 本 crate 从未使用（相关符号零命中），仅需确认不误开该 feature |
| 7 | unstable feature 新增 | — | schema 侧新增 `unstable_auth_status`、`unstable_session_archive`、`unstable_session_set_title`、`unstable_session_status`、`unstable_session_system_prompt`、`unstable_subagents`、`unstable_proxy_chains`（`schema-1.15.1/Cargo.toml:59-85`） | 逐一评估见 6.4（`ACP-33..ACP-40`） |
| 8 | `unstable` 聚合 | 含 notices/compaction/nes 等 | **SDK 侧 `unstable` = 11 项**：auth_status、end_turn_token_usage、llm_providers、mcp_over_acp、plan_operations、session_archive、session_fork、session_set_title、session_status、session_system_prompt、subagents。两个注意点：①**不含 `unstable_proxy_chains`** —— SDK 直接把 schema 依赖写成 `features = ["unstable_proxy_chains"]`（`agent-client-protocol-5.0.0/Cargo.toml:344-347`），即 proxy schema **恒开**，不可关；②**不含 `unstable_protocol_v2`**（schema 侧 `unstable` 有 12 项，比 SDK 多 proxy_chains） | 继续「逐个开启、不开聚合」的既有策略（`ACP-D02` 不变）；proxy 恒开不影响我们（不实现 handler 即可） |
| 9 | proxy chains | `_proxy/*`（实验） | `proxy/initialize` / `proxy/successor`；SDK **无条件**开启 schema 的 `unstable_proxy_chains`（`agent-client-protocol-5.0.0/Cargo.toml:96-98` 注释） | 不实现即无影响；若将来实现，方法名**无回退**，`_proxy/*` 客户端不再工作 |
| 10 | 连接/构建器模型 | `Agent.builder()` / `Builder::new()`（隐式 v1） | `Builder` 按协议版本参数化：`Agent.v1()`（v1 语义 + typed handle）、`Agent.v2()`、`Agent.protocol_router()`（v1/v2 双栈）；裸 `Builder::new(role)` 变成 **raw 无 guard** | 见 `ACP-U03`；**误用 raw `Builder::new(Agent)` 不会编译错误，只会静默丢掉 initialize 协商守卫**——这是最危险的静默退化 |
| 11 | 错误模型 | `Error` 带 v1 语义（`session_not_found` 等构造器） | 根 `Error` **版本中立**；`ErrorCode` 变 newtype；v1 专有错误在 `v1::schema::Error`（新增 `session_not_found()` = `-32002` + `data.sessionId`） | 见 `ACP-U05` |
| 12 | 文档与工具链 | — | 新增 `agent-client-protocol-http`（HTTP/WS 传输）、`-polyfill`、`-rmcp`、`-conductor`、`-testy`、`-trace-viewer`、`-cookbook`、`-derive` | 见 6.4 与 6.5 的取舍 |

### 6.2 协议面增删（v1，逐项实测）

#### 6.2.1 v1 方法增删（`rg 'METHOD_NAME: &str'` 差集）

| 类型 | 方法 | 归属 | 说明 |
|---|---|---|---|
| **新增** | `auth/status` | agent | 登录态只读探测（`AuthStatusRequest/Response`、`AuthState`、`AuthAccount`、`AuthStatusUpdateNotification`），能力位 `AgentAuthCapabilities.status` |
| **新增** | `session/archive`、`session/unarchive` | agent | 归档 = 从 `session/list` 隐藏但保留历史；`ListSessionsRequest.archived: SessionArchiveFilter`、`SessionInfo.archived` |
| **新增** | `session/set_title` | agent | 客户端改标题（我们已有 `SessionInfo.title` 与 `set_title`，但只能 agent 侧设置） |
| **新增** | `session/status` | agent | 查会话是否仍存活：`SessionStatus{open, saved, Other}`，**不是** running/idle |
| **新增** | `proxy/initialize`、`proxy/successor` | proxy | 代理链（RFD proxy chains 清理后语义），纯 agent 不必实现 |
| **删除** | `document/didOpen|didChange|didClose|didSave|didFocus` | client | 1.9.1 的文档同步族整体移除 |
| **删除** | `nes/start|suggest|accept|reject|close` | — | NES（next-edit-suggestion）整体移除，无替代物 |
| **删除** | `mcp/connect`、`mcp/disconnect` | agent→client | **破坏性**：MCP-over-ACP 改为 request-scoped（见 6.2.4） |
| 不变（28） | `initialize`、`authenticate`、`logout`、`elicitation/create`、`fs/read_text_file`、`fs/write_text_file`、`mcp/message`、`providers/list|set|disable`、`session/{new|prompt|cancel|load|list|resume|close|delete|fork|set_mode|set_config_option|request_permission}`、`terminal/{create|output|wait_for_exit|kill|release}`、`$/cancel_request` | — | 本项目的 12 个 handler + CLI 客户端全部落在这一栏，**签名与语义均未变** |

#### 6.2.2 `SessionUpdate` 变体增删

| 变化 | 变体 | feature | 说明 |
|---|---|---|---|
| 新增 | `SubagentUpdate` | `unstable_subagents` | 父会话宣告/更新子会话：`session_id`、`title`、`description`、`capabilities`、`state`、`cwd`、`additional_directories`，全部 `MaybeUndefined` 三态补丁（省略=不变、`null`=清除、值=整体替换） |
| 新增 | `SessionMessage` | `unstable_subagents` | 跨会话消息 **upsert**：`message_id` + sender/recipient 的 session/message id（参与方可后到） |
| 新增 | `SessionMessageChunk` | `unstable_subagents` | 上述消息的内容块增量（只有 `message_id` + `content`） |
| 转正 | `Notice`、`CompactionUpdate`、`CompactionSummaryChunk` | 无（稳定） | 我们已在用，**只需删 Cargo feature 名** |
| 不变 | `UserMessageChunk`、`AgentMessageChunk`、`AgentThoughtChunk`、`ToolCall`、`ToolCallUpdate`、`Plan`、`PlanUpdate`、`PlanRemoved`、`AvailableCommandsUpdate`、`CurrentModeUpdate`、`ConfigOptionUpdate`、`SessionInfoUpdate`、`UsageUpdate` | — | `AvailableCommandsUpdate` 我们仍未接（`ACP-D05` 不变） |

#### 6.2.3 能力位（capabilities）变化

| 结构 | 新增 | 删除 | 备注 |
|---|---|---|---|
| `AgentCapabilities` | `auth.status`（`AgentAuthCapabilities.status`） | `nes`、`position_encoding` | `prompt_capabilities` / `mcp_capabilities`（http/sse/acp 三项）**形状未变** |
| `SessionCapabilities` | `system_prompt`、`archive`、`status`、`title`；`fork` 由「文档级 unstable」变真 `cfg` 门控 | — | `list`/`delete`/`additional_directories`/`resume`/`close` 未变（我们已在用前 5 个，`additional_directories` **仍未广告** → 见 `ACP-37`） |
| `ClientCapabilities` | `subagents`（`SubagentCapabilities`） | `nes`、`position_encodings` | `fs`/`terminal`/`session{notices,compaction,config_options}`/`plan`/`elicitation` 未变 |
| `ClientSessionCapabilities` | — | — | `notices`/`compaction` 由门控转稳定 |
| `McpCapabilities` | — | — | `http`/`sse`/`acp` 三项未变 |

#### 6.2.4 MCP-over-ACP：从「连接式」改为「请求式」（唯一破坏性协议改造）

| 维度 | 2.2.0（我们现在的实现） | 5.0.0 |
|---|---|---|
| 会话声明 | `McpServer::Acp(McpServerAcp{name, server_id})` | 不变 |
| 建连 | `mcp/connect(server_id)` → `ConnectMcpResponse{connection_id: McpConnectionId}` | **方法删除**；无建连步骤（只支持 MCP 2026-07-28，**无 initialize 前置**） |
| 单次调用 | `mcp/message{connection_id, method, params}` | `mcp/message{server_id: McpServerAcpId, request_id: McpRequestId, method, params}`，`request_id` 由调用方生成、跨代理稳定，与 ACP 外层 id 无关 |
| 收尾 | `mcp/disconnect(connection_id)` | **方法删除**；请求结束即结束 |
| 响应体 | `MessageMcpResponse(RawValue)`（内层结果原样） | untagged `MessageMcpResponse::{Result{result}, Error{error: McpError{code,message,data,extra}}}`——**内层 MCP 错误不再借 ACP 错误通道** |
| 通知 | 依附连接 | 依附 `request_id` 所属操作（订阅确认/流式更新） |
| 我们的代码 | `mcp_over_acp.rs:19` 引入 `ConnectMcpRequest`/`DisconnectMcpRequest`/`McpConnectionId`；`:97` 字段 `connection_id`；`:105 connect()`；`:163 disconnect()` | 三个类型已不存在 → 必须重写该模块（`ACP-U04`） |

#### 6.2.5 其他 Rust API 面变化（可能间接影响我们）

- `v1::schema` 成为 v1 消息类型的**唯一路径**（`SDK 5.0.0: src/v1.rs:58` 注释原文「This is the only path to them」）；根 `schema` 仅保留版本中立项（`ProtocolVersion`、`RequestId` 等）。
- `v1/rpc.rs` 新增：`RequestId{Null|Number|Str}`、`Request`、`Response<R,E>` 从 `crate::rpc` 迁入 v1（**Rust API 破坏点**，旧 `v1::Response<T>` 别名被删）；我们未直接使用。
- 根 `Responder<T, P = Raw>` / `SentRequest<T, P>` / `Dispatch<Req, Notif, P = Raw>` 默认 `Raw`；v1 回调拿到的是 `V1` 实例（`v1::Responder` 等别名，`SDK 5.0.0: src/v1.rs:78-94`）。我们的 `Responder` / `Dispatch` / `Handled` / `UntypedMessage` 显式标注处必须改用 `v1::` 别名。
- `ConnectionTo::terminated() -> ConnectionTermination`（`jsonrpc.rs:3623`，含 `Dropped`）；但 `v1::ConnectionTo` 只有 `termination()` / `is_incoming_closed()`（`v1_connection.rs:68,81`），且**故意不实现 `Deref`**。
- batch 支持**移除**（`TransportFrame::Batch` 不复存在，`TransportFrame` 改 `non_exhaustive`）。
- `Responder` 被 drop 而没响应 → 自动回错（取消中 `-32800`，否则 `-32603`），仅在连接仍可写时发送。

### 6.3 升级阻断项（必做，`ACP-U**`）

| 编号 | 优先级 | 项（可执行描述） | 证据（新基线 / 我们的代码） | 改法 | 验收 |
|---|---|---|---|---|---|
| ACP-U01 | P0 | **feature 与 MSRV 对齐**：加 `stdio`（`Stdio`）、`process`（`AcpAgent`）、删两个已转正的 feature 名、`rust-version` ≥1.88 | `SDK 5.0.0: Cargo.toml:33-82`、`src/lib.rs:251-279`（`Stdio`/`AcpAgent` 均 cfg 门控）；`crates/agentscope-extensions-acp/Cargo.toml:19-31`；根 `Cargo.toml:47` | `[dependencies]` 改 `features = ["stdio","process","unstable_session_notices"?→删,"unstable_session_compaction"?→删,"unstable_plan_operations","unstable_llm_providers","unstable_end_turn_token_usage","unstable_mcp_over_acp"]`；`rust-version = "1.88.0"`（workspace 级） | `cargo check -p agentscope-extensions-acp --no-default-features --features lite --bins` 与默认构建均通过；`cargo tree -e features -p agentscope-extensions-acp` 显示 `stdio`/`process` |
| ACP-U02 | P0 | **schema 命名空间迁移**：`agent_client_protocol::schema::v1::X` → `agent_client_protocol::v1::schema::X`；版本中立项（`ProtocolVersion`）留在 `schema::` | `SDK 5.0.0: src/v1.rs:58`（唯一路径）、`src/lib.rs:205`；本 crate **75 处 / 19 文件**（`src/` 10 文件 + `tests/` 9 文件） | 机械替换（含 tests）。建议一次 `rg -l 'schema::v1' \| % { ... }` 批量改，再逐个编译错误收尾 | 编译通过；`rg 'schema::v1' crates/agentscope-extensions-acp` 零命中 |
| ACP-U03 | P0 | **服务端入口改为 `Agent.v1()`**；禁止用裸 `Builder::new(Agent)` | `SDK 5.0.0: src/role/acp.rs:393-402`；`src/jsonrpc.rs:941-967`（`Builder::new(role)` = raw、**无 wire guard**）；我们 `coding_agent.rs:626-627` 用 `Agent.builder()`（该 API 在 5.0.0 不存在） | `Agent.v1().on_receive_request(...)`；`run_with_transport` 的泛型参数由根 `ConnectionTo<Agent>` 改为 `v1::ConnectionTo<Agent>`；CLI 侧同理用 `v1::` 别名 | 编译通过；`tests/acp_stdio_thought_test.rs` 的 initialize 协商回归（旧版本号客户端仍能握手） |
| ACP-U04 | P0 | **重写 MCP-over-ACP 客户端**：删除 connect/disconnect/connectionId，改 request-scoped `mcp/message(serverId, requestId, method, params)` + 内层 `Result|Error` 解析 | `schema 1.15.1: src/v1/mcp.rs:23,76,138,160`（`McpError`/`MessageMcpResponse`/`McpRequestId`/`MessageMcpRequest`）；本 crate `mcp_over_acp.rs:19` 仍 import 已删类型，`:97`/`:105`/`:163` 用 `McpConnectionId`/`connect()`/`disconnect()` | `McpAcpConnection{session_id, server_id}` 只保 id；`call_tool` 每次生成 `McpRequestId`（`Uuid`）并解析 `MessageMcpResponse`：`Result` → 现有 `render_tool_result`，`Error(McpError)` → 转成工具错误文本（**不要**抛 ACP 错误）；`apply_acp_mcp_server` 不再“连接”，只在会话内登记 server 元数据 | 新增离线单测：`parses_tool_result_and_inner_error`、`request_ids_are_unique_per_call`；`tests/acp_*` 全绿 |
| ACP-U05 | P0 | **错误类型版本中立化**：根 `Error` 不再有 v1 专有构造器；`ErrorCode` 变 newtype；需要具体错误码时走 `v1::schema::Error` 的转换 | `SDK 5.0.0: src/error.rs:311-323`（`method_not_found`/`invalid_params`/`internal_error` 仍在）；`schema 1.15.1: src/v1/error.rs:131`（`session_not_found` = `-32002` + `data.sessionId`） | 现有 `Error::invalid_params()` / `internal_error()` 调用**可保持**；`ACP-06` 那条“未知 session 应回 `invalid_params`”改为回 `v1::schema::Error::session_not_found(session_id)`（更合规） | 新增断言：未知 sessionId 的 `session/prompt` 返回 `-32002` 且 `data.sessionId` 命中 |
| ACP-U06 | P1 | **wire 行为差异回归**：①`MatchDispatch::otherwise_ignore` 未匹配请求现在回 `-32601`（此前静默）；②responder 被 drop 会自动回错；③batch 移除 | `SDK 5.0.0: src/util/typed.rs:416`、`src/jsonrpc.rs:4635-4673`；我们用 `otherwise_ignore()` **6 处 / 3 文件**（`tests/acp_elicitation_test.rs:191,324`、`tests/acp_interrupted_turn_test.rs:161,275,389`、`tests/acp_load_replay_test.rs:163`） | 测试里对手工构造的 `channel.*` 请求保留 `otherwise_ignore`，但断言“未处理请求不得导致客户端报错”；确认没有依赖“静默丢弃”的逻辑 | 4 个测试文件全绿；`acp_agent_cli` 收到 `-32601` 时不 panic |
| ACP-U07 | P1 | **测试/工具链 API 适配**：`into_channel_and_future` 返回 `(Channel, Option<ConnectionDriver>)`；`Channel` 非穷尽；`TransportFrame` 非穷尽 | `SDK 5.0.0: src/component.rs:376`、`src/jsonrpc.rs`（`Channel::duplex` 保留） | 我们全仓 `into_channel_and_future` 零命中，`Channel::duplex()` 继续可用；只需确认没有 `TransportFrame` 穷尽 match | `cargo test -p agentscope-extensions-acp -j 2` 全绿 |
| ACP-U08 | P2 | **`cargo fmt`/clippy 基线**：SDK 自身改 `edition 2024` 不影响我们的格式化，但 `rust-version` 变更会进 `Cargo.lock` | 根 `rust-toolchain.toml`=stable 1.97.1 | 升级后重跑 `cargo fmt --all -- --check` 与既有 clippy 口径（仓库无 `-D warnings` 门禁，见 MEMORY） | 两条命令 exit 0（`fmt` 必须 0） |

### 6.4 新能力缺口（`ACP-33+`，接续 §3.5 单调递增）

#### 6.4.1 协议能力（可落地）

| 编号 | 优先级 | 能力 | 协议依据（1.15.1） | 本仓现状（`rg` 实测） | 落地方案 + 验收 |
|---|---|---|---|---|---|
| ACP-33 | P1 | `session/set_title`：客户端改会话标题 | `v1/session.rs`（`SetSessionTitleRequest{title: String}` 必填非 null，空串/不合法 → `-32602`）；能力 `AgentSessionCapabilities.title` | 我们只能 agent 侧设置：`session.rs:777` 有 `set_title`（内部 `SessionInfoUpdate`），**无 handler、无能力广告**（`SetSessionTitleRequest` 零命中） | `coding_agent.rs` 注册 handler → 校验非空 → 更新 `SessionInfo.title` → 广播 `SessionInfoUpdate` → 返回 `{}`；`session_capabilities.title=true`；单测：空标题 → `-32602`、正常设置后 `list_sessions` 可见新标题 |
| ACP-34 | P1 | `session/status`：查询会话是否仍存活 | `v1/session.rs`：`SessionStatus{open, saved, Other}`（**不是** running/idle；未知字符串按 `saved`）；未知 session → `-32002` + `data.sessionId`（`Error::session_not_found`）；能力 `AgentSessionCapabilities.status` | 零命中（`SessionStatusCapabilities`/`session/status` 均 0） | `AcpCodingAgent` 注册 handler：`state.sessions` 命中 → `open`，仅存在于持久化存储 → `saved`，否则 `session_not_found`；`saved` 分支依赖持久化查询，与 `ACP-04`（`session/delete` 未删持久状态）同属存储层缺口，两者宜同批做 |
| ACP-35 | P1 | `auth/status`：登录态只读探测 | `v1/auth.rs`：`AuthStatusRequest/Response`、`AuthState{NotRequired, Required, Ready, Other}`、`AuthAccount`（仅供展示）、`AuthStatusUpdateNotification`；能力 `AgentAuthCapabilities.status`；**不得**用 `-32000 auth_required` 拒绝 | 零命中；我们 `authenticate` 直接返回成功（无真实凭据） | 注册 handler 恒返 `AuthState::NotRequired`；`auth_status.capabilities.status = true`；**不实现** `auth/status_update` 推送（无状态变化源）；单测：`authenticate` 前后均 `not_required` |
| ACP-36 | P2 | `session/archive` / `session/unarchive` + `session/list` 归档过滤 | `v1/session.rs`：`SessionArchiveFilter{Exclude(默认)|Include|Only}`、`SessionInfo.archived: Option<bool>`；**声明归档能力后每条 list 结果必须带 `archived`**；活跃会话归档须先按 `session/close` 语义释放；非法过滤值 → `-32602` | 零命中（`ArchiveSessionRequest`/`session/archive` 均 0）；`session/list` 已实现（分页 + cwd） | `AcpStore` 增 `archived: bool`（内存 + 持久化两层，注意 SQLite 侧 schema 迁移）；list 按 filter 过滤并填 `archived`；归档活跃会话 → 先走现有 close 路径释放 `AcpAgentSession`；单测 4 条（archive→list 默认不含、Only 只含、unarchive 恢复、活会话归档释放） |
| ACP-37 | P1 | **`additional_directories` 能力广告**（不是新方法，是漏广告） | `AgentSessionCapabilities.additional_directories: bool`（1.9.1 已有） | 代码已处理：`session.rs` 的 `set_additional_directories` + `additional_directories` 1 处使用；但 `session_capabilities` **未广告** → 客户端合规地不会发 | 在 `coding_agent.rs` 的 session capabilities 里加 `additional_directories = true`；验收：`initialize` 响应的 `agentCapabilities.sessionCapabilities.additionalDirectories == true` |
| ACP-38 | P1 | `session/new` / `session/fork` 的 **systemPrompt** 支持 | `v1/session.rs`：`SystemPrompt{mode, content}`、`SystemPromptMode{Append(默认)|Override|Other}`；能力三层门控：agent 未广告 → client 发了必须报错（`invalid_params` 语义），不支持某个 mode 也要报错而**不能降级为 append** | 零命中（`SystemPrompt` 0 处） | 第一阶段只做 `Append`：拼接到 system prompt 末尾 —— 注入点在 `acp_harness_agent.rs:816 build_system_prompt()` → `:1041 .sys_prompt(...)`；`Override` 需产品决策（会绕过我们的编码规则/工具说明）→ 暂不做，收到即返回不支持模式错误；能力 `session_capabilities.system_prompt = true`（仅当 Append 可用 **且** 由 `session_factory` 构建，否则不广告、收到即报错）；单测：append 生效、override → 错误、未广告时收到 → 错误 |
| ACP-39 | P2 | `unstable_subagents`：向客户端上报子智能体 | `v1/session.rs`：`SessionUpdate::SubagentUpdate`（字段全 `MaybeUndefined` 三态：省略=不变、`null`=清除、值=替换）；`SessionMessage`/`SessionMessageChunk`（message upsert / 增量）；`SubagentCapabilities` 在 **client 侧** `ClientCapabilities.subagents`；v2 另有 `StateUpdate`/`IdleStopReason` | 零命中（`SubagentUpdate`/`SubagentCapabilities`/`SessionMessageChunk` 均 0）；harness 侧子智能体存在（`agent_spawn`/`agent_send`），但**子 agent 事件不回灌父流**（MEMORY 记 `abf3c35a3` 阻塞：`core::event::AgentEventEmitter` 无使用点） | **前置阻塞**（已解除，见 §6.9 `ACP-39`：改用出带通道，不做父流回灌）：先实现「子智能体事件回灌父流」（core/event 的 `FORWARDING_CONTEXT_KEY` 接缝），再映射为 `SubagentUpdate`（`session_id`/`title`/`state`）+ 是否发 `SessionMessage`（需产品决定是否把子 agent 消息内容透给客户端）；仅当 `client_capabilities.subagents` 存在才发送；不实现 `requires_action` 传播（无 HITL 子流程） |
| ACP-40 | P2 | `unstable_session_fork` 的实际接线 | 1.9.1 已有方法；1.15.1 请求新增 `systemPrompt` 字段，能力位由文档级改为真 `cfg` 门控 | 未启用 feature（`ACP-D01` 明确暂缓），零 handler | 见 `ACP-D13`：暂不实现（理由：会话存储无 fork 原语 + 产品未要求）。若要做，必须先给 `AcpStore` 加「复制会话历史」原语，再叠加 `ACP-38` 的 systemPrompt 语义 |
| ACP-41 | —（已对齐） | `$/cancel_request`：**单请求**取消（与 `session/cancel` 严格区分） | v1 稳定方法；`Responder::cancellation()` 返回可 await 的取消信号；`session/cancel` 是「取消该会话当前 turn」，`$/cancel_request` 是「取消某个在飞请求 id」 | **已实现**：`session.rs:1464 run_prompt_cancellable` 已同时监听 SDK 的 per-request cancellation marker（`coding_agent.rs:695-702` 有注释说明），`tests/acp_cancel_test.rs:51` 断言 `StopReason::Cancelled` | 无需新做；升级后只需回归（5.0.0 把 `cancellation()` 变成公开 API，可选重构为直接 await）；验收：`cargo test -p agentscope-extensions-acp acp_cancel` 全绿 |

#### 6.4.2 运行时 / 工程面（非协议）

| 编号 | 优先级 | 项 | 新基线事实 | 本仓现状 | 建议 |
|---|---|---|---|---|---|
| ACP-42 | P1 | 连接终止统一用 `terminated()` / `termination()` 做清理 | 根 `ConnectionTo::terminated() -> ConnectionTermination{Dropped, ...}`（`jsonrpc.rs:3623`）；`v1::ConnectionTo` 只有 `termination()` / `is_incoming_closed()`，**故意不 Deref** | 依赖 stdio 读端 EOF 自然退出，无显式终止处理 | 在所有 `forwarder` / 事件泵里 await `termination()`，用于 flush `event_queue`、释放 `AcpAgentSession`、写 checkpoint；避免「客户端断开后 agent 仍在跑」 |
| ACP-43 | P1 | 日志脱敏口径 | 普通日志只记错误 code + 固定 reason（不含 message/params）；TRACE 记全量行但 `env`/`headers` 的值替换为 `[REDACTED]`；`AcpAgent::with_debug` 回调**不脱敏** | 我们自写 `tracing` 日志，未统一口径 | 关掉对 request/response 全量 body 的 `debug!`；若用 `AcpAgent::with_debug`（CLI）明确它可能打印令牌 |
| ACP-44 | P2 | 并发模型事实对齐 | 每连接**单 dispatch loop**（handler 跑完才取下一条）；`spawn` 才并发；同连接所有 session 的 update 天然串行；内部全 `mpsc::unbounded`（**无背压**），仅 parked 消息队列限 1024、溢出丢最旧；ordered callback 里 await 其它入站请求 = **死锁** | 我们把长任务丢后台队列（`event_queue.rs`），符合「不要在 callback 里 await」 | 文档化 + 检查 `coding_agent` 的每个 handler 都不在 callback 内 await 别的入站请求；跨会话公平性靠我们自己的 FIFO 队列 |
| ACP-45 | P2 | 优雅关闭 | `request_finish()` 幂等、**无隐式超时**；须 `request_finish()` + 继续 pump + 自建超时；`ConnectionDriver::with_finish` / `request_finish()` | 无 | agent 收到 SIGINT（CLI 已处理）→ `roadmap`：先停止接新请求，再 `request_finish()`，最后带 5s 超时强制退出 |

### 6.5 设计决定（`ACP-D**`，接续 §4.3.1）

| 编号 | 决定 | 理由 | 若将来重开的前置条件 |
|---|---|---|---|
| ACP-D08 | **不实现 proxy chains**（`proxy/initialize`、`proxy/successor`）；不开 `unstable_proxy_chains` | 我们是终端用户直连的 coding agent，没有「agent 被另一个 ACP 客户端通过代理链调度」的部署形态；5.0.0 方法名从 `_proxy/*` 改为 `proxy/*` 且无兼容回退，实现了反而要跟不稳定语义 | 出现「ACP 网关 / 多级客户端」需求时，按 `md/proxying-acp.md` 一次做对（`Proxy`/`ProxyProtocolRouter` 现成） |
| ACP-D09 | **不实现 protocol v2**；`unstable_protocol_v2` 继续不开 | v2 是「另一套协议」——v2 删 batch、无 `session/load`、无 `session/set_mode`、prompt 先回 `messageId` 再后台执行、能力结构改成必填 `info`、client 侧无 `fs`/`terminal`、`McpServerSse` 删除、错误码 `-32002` 改 `session_not_found(id)`；开启后 `ProtocolVersion::LATEST` 直接消失，且 v2 连接不能 `into_protocol::<V1>`，两套 handler 需并存 | 客户端生态明确要求 v2 时再用 `Agent.protocol_router().with_v1().with_v2()`（SDK 已代劳 initialize 版本协商与降级，但 handler 全集需自己写） |
| ACP-D10 | **不引入 HTTP / WebSocket 传输**，保持 stdio 主干 | 目标是本地 IDE 集成；`agent-client-protocol-http` 的 `AcpHttpServer` 暴露 `axum::Router`、每连接调一次 factory、`Acp-Connection-Id` 头、未确认会话流上限 16、请求体 16 MiB，**SDK 不含认证** → 自建网关要自己做鉴权/限流，收益不抵成本 | 需要「远程/多客户端接入」时再评估，且必须自带认证层 |
| ACP-D11 | **Conductor / polyfill / rmcp / testy 不作为运行时依赖** | Conductor 是开发期「多 agent 联调服务」（CLI `conductor agent "p1" "agent" --trace/--serve`，**没有 mcp 子命令**）；polyfill 用旧模型 API 模拟 `tool_call` 更新；rmcp 是「把 MCP server 桥成 ACP 工具」的适配层。三者都会显著增大 embedded 产物体积（当前 `acp_harness_agent.exe` 9.28 MB 是硬指标） | 只在宿主项目里按需引入（dev-dependencies 或独立 bin） |
| ACP-D12 | **NES 移除无需任何动作** | 我们从未使用 NES（符号零命中），5.0.0 直接删掉 `v1/nes.rs` 与 `client_capabilities.nes` | — |
| ACP-D13 | **`session/fork` 继续暂缓**（细化 `ACP-D01`） | 1.15.1 只是给请求加了 `systemPrompt` 字段，能力位从文档级变真 `cfg`；但实现 fork 需要 `AcpStore` 能复制「消息历史 + 会话元数据 + checkpoint」，我们目前只有单条会话持久化 | 先做 `ACP-36` 的存储层抽象（archive 已需要 schema 迁移），再评估 fork |
| ACP-D14 | **不做「把 agent 工具暴露成 MCP server」**（`md/mcp-bridge.md` 方向） | 我们是 ACP agent，客户端负责挂 MCP server；反向桥接会引入 rmcp 依赖与工具语义二次映射（我们的 `Tool` ↔ MCP schema），且没有客户端需求 | 出现「别的 MCP 客户端想复用我们的编码工具」需求时单独评估 |

### 6.6 存量条目联动（必须同步改口径）

| 存量条目 | 原结论 | 新口径 |
|---|---|---|
| §1 `依赖` 行 | 「v2.2.0 已声明」 | 改为「待升级」，指向 §6.3 |
| §2.7「SDK 能力盘点」 | 以 2.2.0/1.9.1 为基线，列「待办（不含暂缓）」 | 2.2.0 那一栏保留为历史；新增 5.0.0 栏，逐项指向 `ACP-33..ACP-41` 与 `ACP-D08..D14` |
| `ACP-01`（配置选项）等 12 个 handler | 已完成 | 不受影响，但 `session_capabilities` 广告字段需补 `title`/`status`/`archive`/`system_prompt`/`additional_directories`（随 `ACP-33..ACP-38` 分批） |
| `ACP-02`（协议演进跟踪） | 「暂缓 ACP v2 / Proxy」 | 补 §6.2 的方法增删表 + `ACP-D08`/`ACP-D09` 两条正式决定 |
| `ACP-06`（未知 session 错误码） | 回 `invalid_params` | 改用 `v1::schema::Error::session_not_found(id)`（`-32002` + `data.sessionId`），见 `ACP-U05` |
| `ACP-D01`（unstable feature 逐个开） | 6 个 feature 清单 | 清单变化：删 `unstable_session_notices`、`unstable_session_compaction`；新增评估项见 6.1#7；**加 `stdio`/`process`（非 unstable，但必开）** |
| `ACP-D02`（不用聚合 `unstable`） | 不变 | 仍成立（聚合里新增了 6 项，其中 `unstable_protocol_v2` 仍不在内） |

### 6.7 建议批次（每批独立可编译、可回滚）

| 批次 | 内容 | 涉及文件 | 验收命令 |
|---|---|---|---|
| **B0 升级** ✅ 已完成（2026-11-03） | `ACP-U01..U08`（纯迁移，零新特性） | `Cargo.toml`（crate + workspace `rust-version`）、`src/` 10 个含 `schema::v1` 的文件、`tests/` 9 个文件 | `cargo check -p agentscope-extensions-acp --all-targets -j 2`；`cargo test -p agentscope-extensions-acp -j 2`；ACP 握手冒烟（stdin 喂 `initialize`，校验 stdout 含 `protocolVersion`） |
| **B1 只读能力** ✅ 已完成（2026-11-03，`ACP-33/34/35/37`） | `ACP-33`（set_title）、`ACP-34`（status）、`ACP-35`（auth/status）、`ACP-37`（additional_directories 广告） | `coding_agent.rs`、`session.rs`、`config.rs`（capabilities 构造） | 新增单测 + `initialize` 能力位断言 |
| **B2 存储与生命周期** ✅ 已完成（`ACP-36`、`ACP-42`） | `ACP-36`（archive/unarchive + list filter + `archived`）、`ACP-42`（termination 清理） | `session.rs`、`event_queue.rs`、`acp_harness_agent.rs` | 归档 4 条单测；断开连接后无泄漏（`mem.rs` 探针可选） |
| **B3 提示与取消** ✅ 已完成（`ACP-38`、`ACP-41` 回归、`ACP-45` 原已存在） | `ACP-38`（systemPrompt append）、`ACP-41`（`$/cancel_request`）、`ACP-45`（优雅关闭） | `acp_harness_agent.rs`、`session.rs`、`coding_agent.rs` | cancel 竞态测试（先 prompt 后 cancel）；append 生效测试 |
| **B4（可选，有前置）** ✅ `ACP-39` 已完成（2026-11-04）；`ACP-40` 仍暂缓 | `ACP-39`（subagents 上报）、`ACP-40`（fork，仍暂缓） | core `agent_traits.rs`/`react_agent.rs`、harness `agent_spawn.rs`/`harness_agent.rs`、ACP `event.rs`/`session.rs`/`coding_agent.rs`/`support.rs` | `cargo test -p agentscope-extensions-acp`；`tests/acp_subagent_test.rs`（有/无能力两种客户端） |

### 6.8 升级坑清单（实测/读码得到，勿踩）

1. **`Agent.builder()` 在 5.0.0 不存在**，换成 `Agent.v1()`；但**裸 `Builder::new(Agent)` 仍能编译**（raw 模式），代价是丢掉 wire guard —— initialize 不校验版本、未初始化请求不被拒。必须用 `Agent.v1()`。
2. **`stdio` / `process` 不加就是一堆 `cannot find type`**，错误信息不会提示「请开 feature」，容易误判为 SDK 版本不对。
3. `unstable_session_notices` / `unstable_session_compaction` 两个 feature 名删掉后，代码里**没有任何 `cfg(feature="unstable_")`**（本 crate 实测只有 `alloc-stats`），所以这两个名字只出现在 `Cargo.toml` 注释里 → 记得把注释一起改，否则注释误导后来者。
4. `schema::v1::X → v1::schema::X` 有 **75 处**；`rg 'schema::v1'` 改完后，`RequestId`/`ProtocolVersion` 这类**版本中立项**仍在 `schema::` 下，不要一起替换。
5. `Dispatch` / `Handled` / `UntypedMessage` / `Responder` 在根上是泛型默认 `Raw`，v1 语义需显式 `v1::` 别名（`acp_agent_cli.rs:51`、`:962`）。
6. `MatchDispatch::otherwise_ignore` 语义变化：未匹配请求从「静默丢弃」变成回 `-32601`；我们的 6 处测试用它构造「只关心 elicitation」的场景，升级后客户端可能收到额外错误响应。
7. responder 被 drop 会自动回错（取消中 `-32800`，否则 `-32603`）——异步 handler 里 `?` 早退不再「静默无响应」，客户端行为可能变化。
8. `v1::ConnectionTo` 没有 `Deref` 到根 `ConnectionTo`：需要 `terminated()` 的地方只能拿根连接。
9. MCP 改造后**没有 initialize 握手**（只支持 MCP 2026-07-28）——如果现有 `mcp_over_acp.rs` 里发过 `initialize`，升级后会被服务端当非法方法。
10. 升级顺序建议：先只改 `Cargo.toml` 跑 `cargo check` 收集全部错误，再一次性改命名空间，最后修 MCP —— 反过来会因为错误量太大淹没有效信息。

### 6.9 执行记录（B0 / B1 / B2 片段 / B3 的 ACP-38 / B4 的 ACP-39，2026-11-03 ~ 11-04）

| 编号 | 执行状态 | 交接信息（验证结果） |
|---|---|---|
| `ACP-U01..U08`（B0） | 已完成 | workspace `rust-version` → `1.88.0`；crate features 加 `stdio`/`process` 与 5 个 `unstable_*`；`schema::v1::X → v1::schema::X` 全量改写（src 10 + tests 9）；`cargo check -p agentscope-extensions-acp --all-targets` exit 0；`cargo test -p agentscope-extensions-acp -j 2` 全绿 |
| `ACP-37` | 已完成 | `coding_agent.rs` 的 `SessionCapabilities` 增 `additional_directories(SessionAdditionalDirectoriesCapabilities::new())`；测试 `acp_capabilities_test.rs::initialize_advertises_the_new_capabilities_and_they_work` 断言 `session.additional_directories.is_some()` |
| `ACP-33` | 已完成 | `session/set_title` handler：trim + 空白 → `-32602`，成功后写 `SessionInfo.title` 并广播 `SessionInfoUpdate`；能力位 `session.title`；测试断言 `list_sessions` 可见新标题、空标题报 `INVALID_PARAMS` |
| `ACP-34` | 已完成 | `session/status` handler + `session_status()`：注册表命中 → `Open`，仅持久化 → `Saved`，否则 `session_not_found`（`-32002`）；能力位 `session.status`；测试 `a_session_that_is_only_persisted_reports_saved`（重启后 `Saved` → `session/load` 后 `Open`） |
| `ACP-35` | 已完成 | `auth/status` handler 恒返 `AuthState::NotRequired`（无真实凭据，不实现 `auth/status_update`）；能力位 `auth.status`；测试在 `initialize` 后直接断言 |
| `ACP-36` | 已完成 | 归档状态落 `.agentscope/acp-sessions/<id>.archived` 标记文件（`session.rs::is_archived_on_disk` / `set_archived_on_disk`），不动 harness 侧 journal schema；`session/list` 按 `SessionArchiveFilter`（Exclude 默认 / Only / Include）在**分页前**过滤，且每条结果都带 `archived`；`Other(_)` 过滤值 → `-32602`；活跃会话归档按 `session/close` 语义释放（`close()` + `cancel_and_wait(15s)` + `remove_session`）；持久化-only 会话的 unarchive 靠 `session_workspace()` 探测（cwd 来源＝注册表 + 进程内见过的 workspace 集合 `known_workspaces`，由 `session/new|load|resume|list` 填充）。测试：`archiving_a_live_session_hides_it_until_it_is_unarchived`、`archive_state_survives_a_restart`、`archiving_a_session_that_never_prompted_keeps_it_resolvable` |
| `ACP-38` | 已完成 | `coding_agent.rs::system_prompt_text()` 只接受 `SystemPromptMode::Append`（`Override`/其它 → `-32602` + `data.supportedModes`，不降级）；文本块以空行拼接、空白内容报错；`acp_harness_agent.rs::with_client_system_prompt()` 把客户端文本接在默认 system prompt **之后**（编码规则/工具说明保留在前）；能力位 `session.system_prompt` **仅在 `session_factory` 存在时**广告（`model_factory` 无传递通道，故不广告且收到即 `-32602`）；`session/load|resume` 命中**已存活**会话时 `systemPrompt` 无法叠加（会丢掉正在跑的轮次），记 WARN 后忽略。测试：单测 `coding_agent::tests::a_client_system_prompt_is_only_accepted_in_append_mode`、`tests::a_client_system_prompt_is_appended_after_our_instructions`（bin），集成 `acp_capabilities_test` 两条（`model_factory`：能力位 `is_none()` + 带 prompt 的 `session/new` 报 `INVALID_PARAMS`；`session_factory`：能力位 `is_some()` + `SessionBuild.system_prompt` 真的传到工厂 + `override` 被拒且不建会话） |
| `ACP-42` | 已完成 | `AcpCodingAgent::shutdown_sessions()`：`run_with_transport` 的 `connect_to(...).await` 返回后先停 wakeup 循环，再 drain 会话（`close()` + `cancel_and_wait(5s)`）并清空 `session/load|list` 的 catalog agent 缓存。事件队列不需额外 flush：队列是**每轮**新建的（`session.rs::run_prompt_inner`），轮次返回时 sink 已 drop。`connect_to` 本身就在 termination 时返回，所以没有额外 await `termination()`（避免两套清理路径）。测试：`coding_agent::tests::a_terminated_connection_releases_its_sessions`（反向验证：注释掉 `session.close()` → FAILED） |
| `ACP-43` | 已完成（现状已满足） | `rg 'debug!'` 审查：本 crate 的 debug 日志只记路径/字节数/规则条数/会话 id，**无** request/response 全量 body；全程未用 `AcpAgent::with_debug`，不存在未脱敏的令牌打印 |
| `ACP-44` | 已完成（审查结论） | 每个 handler 只做轻量工作：`session/prompt` 一律 `cx.spawn`，`session/request_permission` 只在 spawn 出去的轮次内 await；handler 内没有 await 其它入站请求，不存在单 dispatch loop 死锁风险 |
| `ACP-45` | 已完成（HEAD 已有，非本批新增） | `acp_harness_agent.rs::spawn_shutdown_handler`（SIGINT/SIGTERM）→ `drain_sessions`：`interrupt()` → 20s 有界等待 `is_running`/`active_request_count` 归零 → `cancel()+close()` → `sleep(250ms)` → `exit(130)`；stdin EOF 路径同样先 drain 再返回。**有意不用** `request_finish()`：信号路径进程即将退出、EOF 路径客户端已走，没有可礼貌告别的对端 |
| `ACP-39` | 已完成（2026-11-04，B4） | **前置方案改写**：不做「子事件回灌父流」，改为**一条独立的出带通道**——子代理在父 turn 返回后仍在跑，回灌父流只能在下一次 reasoning 前排队。落地分四段：①core `CallableAgent::call_streaming(messages, sink)`（默认忽略 sink = `call`），`ReActAgent::call` 改为**边产出边转发**给 subscriber（原先 `collect()` 后整批转发，UI 上等于没有流式），并新增 `ObservableAgent::unsubscribe`（`HarnessAgent::call_streaming` 在 run 结束后清除单槽 subscriber，避免持久 `agent_key` 把后一轮事件发给已退役的通道）；②harness `AgentSpawnTool::with_event_sink(UnboundedSender<(child_session, AgentEvent)>)`：spawn 时**先**把 `SubagentExposed` 写进该通道（保证公告早于子任务任何事件）再 `tokio::spawn`，`run_child_turn` 有 sink 走 `call_streaming`、无 sink 保持 `call`（向后兼容），结束（含 repair 失败）一律补一条 `Custom{kind:"subagent_finished", outcome, error?}` 终态；③ACP `Session` 持有接收端，首次 `set_client`（连接就绪）启动**常驻转发任务**，`event::SubagentTracker` 按 child 维护 `ToolEventMapper`/已公告/运行中状态，帧映射为 `subagent_update`（外层父会话，含 `title`/`description`/`capabilities.cancel {}`/`state running|idle`）+ 子会话 chunk（外层子会话，`messageId = sub-<child>`，绝不复用父 turn 的 id；非流式的一次性回复由 `AgentResult` 补一条、已流式的不重复；出错先发 `notice` 再 idle）；④门控：`support.subagents.is_some()` 为假时零帧（含子会话 chunk），`session/cancel` 参数为子会话 id 时经 `AcpCodingAgent.subagent_children`（child→parent，仅 running 期登记）路由到 `HarnessAgent::spawn_tool().cancel_subagent()`（只 `interrupt()`，**不 abort 后台任务**——abort 会留下 unknown tool outcome 让该 `agent_key` 之后不可用），`session/close` 中断该会话全部子代理，`session/list|status|load` 屏蔽 `sub-*` 前缀。**未做**：`requires_action`（阶段 6，子代理内权限/elicitation 弹层）、`session/load` 回放子代理历史、`session_message`（三者均为方案中的可选/明确不做项）。测试：`event.rs` 7 条（帧形状/外层区分/公告先到/idle+notice/门控/非流式回复/流式回复不重复）、`support.rs` 2 条、`agent_spawn.rs` 3 条（流式+终态、无 sink 保持阻塞、按子会话取消只命中该子代理）、`tests/acp_subagent_test.rs` 2 条（真实 Channel 传输：有 `subagents` 能力收到 3 帧且顺序/外层 id/cancelable 正确；无能力零帧）。反向验证：`supports_subagents` 恒 true → 静默用例 FAILED；去掉公告先到 → `a_child_frame_is_always_preceded_by_an_announcement` FAILED |

**验证命令（本轮实测）**

- `cargo check -p agentscope-extensions-acp --all-targets -j 2` → exit 0（本 crate 零警告；harness 侧 4 条 `unused import` 为 HEAD 既有，未纳入本批）
- `cargo test -p agentscope-extensions-acp -j 2` → 全部 target `ok`（lib **85 passed**，含新增 `mcp_over_acp::tests::unwraps_a_result_and_surfaces_an_inner_error_as_a_tool_failure` / `every_request_gets_its_own_id`；`acp_capabilities_test` **5 passed**；bin `acp_harness_agent` **5 passed**；其余 target 均 0 failed）
- `cargo fmt --all -- --check` → exit 0
- 反向验证（均已还原）：①`SessionArchiveFilter::Exclude` 改成不过滤 → `archiving_a_live_session_hides_it_until_it_is_unarchived` / `archive_state_survives_a_restart` 双双 FAILED（`:326:17`、`:440:17`）；②`system_prompt_text` 的 mode 校验短路（`if false && ...`）→ `a_client_system_prompt_is_only_accepted_in_append_mode` + `a_client_system_prompt_reaches_the_session_factory_and_override_is_rejected` FAILED；③`with_client_system_prompt` 改成只返回客户端文本 → `a_client_system_prompt_is_appended_after_our_instructions` FAILED；④`shutdown_sessions` 去掉 `session.close()` → `a_terminated_connection_releases_its_sessions` FAILED；⑤systemPrompt 能力改为无条件广告 → `initialize_advertises_the_new_capabilities_and_they_work` FAILED（`:102`）；⑥`response_payload` 把内层 MCP `Error` 当成功返回 → `unwraps_a_result_and_surfaces_an_inner_error_as_a_tool_failure` FAILED

**已知限制**：`AcpCodingAgent::model_factory` 路径拿不到 `systemPrompt`（该工厂签名只有 cwd + session id）→ 该配置下**不广告**该能力，且带 `systemPrompt` 的 `session/new|load|resume` 直接 `-32602`（不再静默丢弃），已在 rustdoc 注明；生产二进制走 `session_factory`，行为正确。

**本轮收尾清理**：删掉 `AcpAgentSession::mcp_acp_connections`（无握手协议下已无读取点，纯只写 keep-alive）；`is_archived_on_disk`/`set_archived_on_disk` 收为 `pub(crate)`；`shutdown_sessions` 改 `futures::future::join_all` 并发释放（每条仍 5s 有界）；MCP-over-ACP 内层错误改用 `Error::new(INTERNAL_ERROR, msg)`（信息进 `message` 而非 `data`，客户端可见）。

**下一步（B4，可选）**：`ACP-40`（fork，仍暂缓，见 `ACP-D13`）。`ACP-39` 的收尾项（stop reason / requires_action / 嵌套 / 回放 / `_meta` / 子会话 status）已于 2026-11-05 补齐，见下节。B0–B3 已全部落地；`ACP-U01..U08` 与 `ACP-33..ACP-38`、`ACP-41..ACP-45` 均有结论。

#### 2026-11-05 `ACP-39` 收尾：子代理上报的协议细节

前端 `subagent_update` 已就绪，本轮只补 Rust 侧缺口（不改前端）。协议约束（来自 `agent-client-protocol-schema-1.15.1`）：

| 项 | 结论 | 落地位置 |
|---|---|---|
| 结束/失败/取消必须发 `subagent_update` 且带 `stopReason` | ✅ 已完成（原实现只发裸 `idle`，卡片会永远「运行中」） | `event.rs::stop_reason_for`（`success→end_turn`、`cancelled→cancelled`、`error→IdleStopReason::Error{error:{code,message}}`）+ `subagent_idle(..., stop_reason, meta)` |
| `requires_action` 与 `session/request_permission` 成对 | ✅ 已完成 | `event.rs::subagent_requires_action`；`session.rs::SubagentStateNotice::{requires_action,running}` 在 `acp_confirm_handler` 内包住 `client.request_permission`（答复后无论允许/拒绝都回 `running`，终态由 turn 结束上报） |
| 子会话 `plan`/`plan_update` 的外层 sessionId = 子会话 id | ✅ 已完成 | 子 `SubagentTracker` 改用 `with_plan_operations(support.supports_plan_operations())`，与父 turn 同一形态（此前子 mapper 走 legacy `Plan`，ID 化计划会丢 id） |
| `request_permission.sessionId` 必须是子会话 id | ✅ 已完成（原实现子代理 `ask` 无 handler，卡片无法归属） | `SubagentRuntimeHook` 注入链：`SessionBuild.client_slot`/`permission_store` → `build_agent` → 子代理 blueprint → `agent_spawn` 调 `on_child_ready` → 装 `acp_confirm_handler(child_session, ...)` |
| 嵌套 spawn 的外层 sessionId = 直接父会话 | ✅ 已完成 | harness：`HarnessAgentBuilder::subagent_nesting(depth, max)`；blueprint 携带 `subagent_events`/`subagent_runtime_hook`/`subagent_declarations`/`subagent_depth`/`max_subagent_depth`；`LeafFactory::build` 仅当 `depth > max` 才 `.as_leaf_subagent()`。帧：`SubagentFrame{parent_session, child_session, event}` 取代裸元组；`CallableAgent::set_host_session_id` 让中间子代理给自己的子代理打父标记（ACP 在 `on_child_ready` 里调用）。ACP 侧 `build_agent` 用 `subagent_nesting(0, 1)`（一层嵌套，孙代理是叶子）。取消：`AcpAgentSession.subagent_agents`（`on_child_ready` 登记的树内全部子代理）兜底嵌套子代理的 `session/cancel`——根的 spawn tool 只认直接子代 |
| `_meta.startedAt/completedAt`、`_meta.model/role` | ✅ 已完成 | `SubagentMeta` 随 `subagent_started`（model/role 来自 exposure 的 `model`/`role`）与 `subagent_idle`（completedAt）下发 |
| `session/load` 回放历史 `subagent_update` | ✅ 已完成 | `session.rs::replay_subagent_cards`：`replay_notifications` 末尾按 `TaskRepository::list_tasks` 回放**已终态**任务（`agent_id`→role、`created_at`→startedAt、`last_checked_at`→completedAt，`Completed→success`/`Cancelled→cancelled`/其它→`error`），子会话 id 由 task id 派生（`sub-<task_id>`）故重开幂等。挂起/运行中的任务**不回放**（无 runner、无结果，报任一状态都是编的） |
| `session/status` 支持子会话 id | ✅ 已完成（窄口径） | `coding_agent.rs::session_status`：注册表 → `subagent_children`（仅 running 期登记）→ 持久化；`session/list|load|delete` 仍屏蔽 `sub-*`（子会话转写属于父会话，不应能被单独操作） |

测试：`event.rs` +6（`_meta` model/role、requires_action↔running 配对、cancel/error stop reason、嵌套外层 id、idle 的 `_meta`）、harness +3（`materialize` 嵌套深度两条、`agent_spawn` 帧携带父会话）、`tests/acp_subagent_test.rs` +2（嵌套帧外层 = 中间子代理；存活子会话 `session/status` = `open`）。反向验证：`subagent_children` 判断短路 → `a_live_child_is_reported_as_an_open_session` FAILED；转发器忽略 `parent_session` → `a_nested_child_is_reported_under_the_subagent_that_spawned_it` FAILED；`LeafFactory` 强制 `.as_leaf_subagent()` → `a_child_allowed_to_nest_keeps_its_own_spawn_tool` FAILED。

**已知限制**：①嵌套默认只开一层（`subagent_nesting(0, 1)`），需要更多层由嵌入方自行设置上限；②`session/load` 回放的 role 只有 `agent_id`（模型名不在任务记录里）、无 `capabilities`/`requires_action`（回放时子代理已不在跑）；③子会话 status 只在进程内 running 期可见，重启后 `sub-*` 一律 `-32002`（与 `session/list` 屏蔽一致）。

#### 2026-11-04 会话生命周期修复（creation record + known_workspaces）

三个外部报障的根因都是「会话在磁盘上没有任何痕迹」：`session/new` 不写 journal（没有 prompt 就没有 turn），config sidecar 又只在用户改过 model/mode 时才落盘，归档标记是**唯一**的磁盘痕迹——一删就什么都不剩，而 `session_workspace()` 只能按「注册表 cwd + `session/list` 建过的索引 agent」去猜 cwd。

| 缺陷 | 修复 | 守护测试 |
|---|---|---|
| `session/unarchive` 非幂等：磁盘-only 会话首次 unarchive 删掉标记后，第二次 `-32002` | `AcpAgentSession::new` 在创建时即 `persist_config()`，把 config sidecar 当作**creation record**（`session.rs::is_recorded_on_disk` / `recorded_session_ids`）；`session_workspace()` 的探测条件改为「creation record **或** archive marker 或 journal 键」，unarchive 因此天然幂等 | `archiving_a_session_that_never_prompted_keeps_it_resolvable`（连续两次 unarchive） |
| 归档后 `session/status`/`session/resume` 对「建了但从未 prompt」的会话一律 `-32002` | 新增 `AcpCodingAgent::known_workspaces: Mutex<Vec<PathBuf>>`（只存路径，由 `register_session`/`catalog_agent` 填充），`session_workspace()` 不再只依赖 `catalog_agents.keys()`：归档释放会话后 cwd 仍在集合里；`session_exists()` 先查 creation record，再探 journal head | 同上（归档后 `status == Saved`）+ `a_session_created_without_a_prompt_survives_a_restart` |
| `session/list` 只列 journal 持久化的会话，`session/new` 后未发消息的会话（含 `archived: include`）不出现 | `session_infos()` 在 journal 键之外合并 `recorded_session_ids(cwd)`（`BTreeSet` 去重），每条仍按 marker 报 `archived`；`session/delete` 新增 `session.rs::forget_session_on_disk`（删 record + marker），否则被删的未 prompt 会话会重新出现在 list 里 | `a_session_created_without_a_prompt_survives_a_restart`（默认过滤 + `Include`）、`deleting_a_session_created_without_a_prompt_hides_it_again` |

反向验证（均已还原）：①注释掉 `session.persist_config()` → 前两个测试 FAILED（`:665`、`:728`）；②去掉 `register_session` 的 `note_workspace()` → `archiving_a_session_that_never_prompted_keeps_it_resolvable` FAILED（`:665`，归档后 status 报 `-32002`）。

**注意（行为变更）**：`session/load|resume` 也会在 workspace 里留下 creation record；`session/delete` 现在会删掉 record 与 archive marker（journal 仍由 harness 拥有，未动）。`known_workspaces` 只增长路径串，不做上限（`catalog_agents` 仍限 `MAX_CATALOG_AGENTS = 4`）。

**验证（2026-11-04）**：`cargo test -p agentscope-extensions-acp -j 2 --no-fail-fast` → **14 targets 全 ok / EXITCODE=0**（lib 85、`acp_capabilities_test` **8**）；`cargo fmt --all -- --check` exit 0；`cargo check -p agentscope-extensions-acp --all-targets` exit 0；`cargo check -p agentscope-extensions-acp --no-default-features --features lite --bins` exit 0。

#### 2026-11-04（续）`session/delete` 真正删除 journal + 归档还原后可续聊

`SessionLogStore::delete` 一直只有「不支持」的默认实现，`session/delete` 因此只清注册表：跑过至少一轮的会话被删后仍能被 `list_session_keys` 发现、仍能被 `session/load` 读回（「删了还在」）。补链条：

| 层 | 变更 |
|---|---|
| harness `agent/session/storage.rs` | `WorkspaceSessionLogStore::delete`：对 `key.storage_path()` 与 `key.legacy_storage_path()` 各删一次子树（旧版本写在 `agents/...`，留下会被再次发现）；不存在则跳过，重复删除为 no-op |
| core `session/in_memory.rs` | `InMemorySessionLogStore::delete`：从 `stores` 移除该 key（原先只有 unsupported 默认） |
| harness `agent/harness_agent.rs` | 新增 `HarnessAgent::delete_session(&RuntimeContext)`：`session_key(context)` + `store.delete(&key)`，与 `list_session_keys`/`session_log` 用同一 key 解析 |
| acp `coding_agent.rs` | `session/delete` 新增 `forget_session()`：活跃会话用**该会话自己的** `agent`（turn 写入用的同一 agent 身份），否则用 `catalog_agent(cwd)`；`delete_session` 失败只 WARN（不因后端不支持删除而回滚已完成的协议调用），随后 `forget_session_on_disk`（record + marker） |

新增测试：`deleting_a_persisted_session_removes_its_history`（跨重启：list 可见 → delete → list 消失 + `session/load` 报 `-32002`）、harness 单测 `deleting_a_session_removes_the_journal_and_its_legacy_copy`、`an_unarchived_session_replays_its_history_and_keeps_going`（归档 → 重启 → `list(include)` → unarchive → load 回放出 `USER:hello` → 再发一轮）。

**归档/还原语义（已确认支持）**：归档只写 marker + 按 `session/close` 释放（journal 保留）、unarchive 只删 marker；还原后用 `session/load`（回放全部历史）或 `session/resume`（不重放）重新打开，会话继续写**同一个** journal，历史与后续轮次连续。反向验证（已还原）：把 `WorkspaceSessionLogStore::delete` 临时改成 no-op → `deleting_a_persisted_session_removes_its_history` FAILED（`:911`，删除后仍被 list）。

**前提（协议限制，非缺陷）**：`session/status|archive|unarchive` 都不带 `cwd`，而 record/marker/journal 全是 workspace 内状态；进程重启后必须先有一次带该 `cwd` 的 `session/list`（归档会话要带 `archived: include/only`）才能定位 workspace，否则按「未知会话」返回 `-32002`。IDE 恢复历史列表时天然满足这个顺序。

**验证（续）**：`cargo test -p agentscope-core --lib` → **466 passed / 3 ignored**；`cargo test -p agentscope-harness --lib` → **625 passed**（基线 624）；`cargo test -p agentscope-extensions-acp -j 2 --no-fail-fast` → **14 targets 全 ok**（`acp_capabilities_test` **10**）；`cargo fmt --all -- --check` exit 0；acp `--all-targets` / `--no-default-features --features lite --bins` 均 exit 0。
