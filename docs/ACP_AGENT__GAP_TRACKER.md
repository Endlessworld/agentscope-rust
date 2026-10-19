# ACP 协议 CodingAgent（Rust 版）— 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 ACP CodingAgent 实现工作的**单一事实来源**。任何会话开工前必须先读本文件；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。以本文件 + `git status`/`cargo build` 实际验证为准。
> **对照基准**：`docs/ACP_AGENT_PLAN.md`（方案）+ 本文件 + `crates/*`（实际代码）。
> **参考**：Kotlin `AgiHarnessAgent.kt`、Java `HarnessCodingAgent.java`、ACP RDF 文档 (E:\local-github\agentclientprotocol\agent-client-protocol)、ACP Rust SDK 源码（`E:\local-github\agentclientprotocol\rust-sdk`）。

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
| 依赖 | `agent-client-protocol` 依赖已声明 | 已完成 | harness Cargo.toml 已声明 crates.io **v2.2.0**（无 feature），传递 `agent-client-protocol-schema` **1.9.1**；SDK 提供但本项目未接线的能力见 §2.7 |
| sdk.ability | SDK v2.2.0 能力盘点 | 已完成 | §2.7.2 中所有「待办（不包含暂缓）」项已接线：Session Notices、Compaction 通知、Plan 操作、Providers、MCP-over-ACP、`PromptResponse.usage`、分页/元数据、CLI 会话恢复；仍暂缓 ACP v2 与 Proxy/路由 |

## 2. 详细缺口清单

### 2.1 依赖与骨架（P0）
| 缺口项 | 执行状态 | 交接信息/说明 |
|---|---|---|
| `agent-client-protocol` 依赖 | ✅ 已完成 | 已由 path 依赖（`rust-sdk/src/agent-client-protocol` v2.0.0 + `unstable_tool_call_name`）改为 crates.io `agent-client-protocol = { version = "2.2.0" }`（无 feature），编译通过；能力盘点见 §2.7 |
| `acp/` 模块目录 + mod.rs | ✅ 已完成 | `crates/agentscope-harness/src/acp/`（mod/coding_agent/session/config/event） |
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
| `src/acp/acp_harness_agent.rs`（stdio ACP agent，`[[bin]]` 目标） | ✅ 已完成 | HarnessAgent 全装配（文件系统/shell/compaction/plan mode/skills）+ 真实 stdio 运行验证通过 |
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

## 4. 执行状态约定（所有会话必须遵守）
- 开工前：读本文件 + `git status` + 确认 `cargo build` 基线。
- 改动中：保持每阶段 `cargo build`/`cargo test` 通过。
- 完工后：把对应条目 `执行状态` 改为 `已完成`，更新 `交接信息`（验证结果）。
- 涉及 SDK 参考时：以 `E:\local-github\agentclientprotocol\rust-sdk`（tag `v2.2.0`）源码与 cargo registry 中 `agent-client-protocol-2.2.0` / `agent-client-protocol-schema-1.9.1` 为准（旧的 `schema-1.6.0` 口径已作废）。
