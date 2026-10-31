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

> **2026-10-31 合并更正（dev `5883c98`）**：`a_cancelled_turn_does_not_block_the_next_prompt` 的永久挂起**已修**，真因是用例硬编码 `sleep(400ms)` 后取消、与冷会话首次模型调用（0.7–1.0s）竞态；改为等模型调用计数 ≥1 再取消。故 **ACP-01 由 P0 降为 P1**：残留风险仅限「provider 永不返回且客户端永不取消」时 `running` 无上限（有意不加粗粒度超时，避免误杀健康长任务）。

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
| ACP-01 | P1 | **prompt 循环对 `running` 状态无上限等待**：`$/cancel_request` 未送达 agent 侧时，`session/prompt` 永不返回（生产表现为「输出中断后再说 continue 无任何响应」）。admission 看门狗只认 `queued`，`running` 没有任何上限 | 无（Rust 原生；此前在 `docs/extensions/HARNESS__GAP_TRACKER.md` 的 HARNESS-15 登记，本 crate 已独立成 crate，故迁到本条） | `session.rs:1849` 仅 `current.status == "queued"` 才触发 `ADMISSION_TIMEOUT`（`session.rs:1793`，300s）；`session.rs:1845` 又在**任何状态变化**时重置该 deadline，故 `queued→running` 后再无截止时间。内层 select 只有两处能跳出等待：`cancel_epoch` 比对（`session.rs:1832`）与 SDK `cancellation.cancelled()`（`session.rs:1880-1892`），两者都要求取消**已送达**。`cancel_and_wait`（`session.rs:1351`）与 `is_closed()`（`session.rs:1366`）存在于外层 admission 等待（`session.rs:1586-1612`）但**不在内层 select 里**。用例：`tests/acp_interrupted_turn_test.rs:62 a_cancelled_turn_does_not_block_the_next_prompt`（Windows 永久挂起；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §4.2-1 记 `f81c8b1` 即复现） | 待办 | ①把 `admission_deadline` 推广为「**无进展上限**」：progress = 收到任一 `AgentEvent` 或 `status` 变化（事件驱动重置，默认 30min，`AGENTSCOPE_ACP_TURN_TIMEOUT` 可调）；超时先 `agent.interrupt_run(run_id)`，10s 宽限后 `abandon_turn`，发一条 `[turn timed out]` chunk 并返 `StopReason::Refusal`；②内层 select 增 `closed` 轮询臂（`is_closed()` 已有），使 `session/close`/LRU 淘汰能无条件跳出；③prompt handler（`coding_agent.rs:583-640`）另建 turn 级取消令牌，由 `AcpAgentSession::cancel()` 与 `$/cancel_request` 共同触发，让两条取消路径收敛。验收：该用例单跑 + `cargo test -p agentscope-extensions-acp -j 2` 各 8 次全绿；`EXTENSIONS_GAP_TRACKER.md:115` 的 `--skip a_cancelled_turn_does_not_block_the_next_prompt` 可删除 |
| ACP-02 | P1 | **MCP 连接生命周期未收尾**：`mcp/disconnect` 从不发送；stdio/SSE/HTTP MCP 子进程不断开；桥接进 toolkit 的工具从不摘除（会话关闭后工具仍指向死连接） | 无（Rust 原生） | `mcp_over_acp.rs:163` 定义 `McpAcpConnection::disconnect()`，全 crate `rg 'disconnect\('` 仅命中定义与模块文档；`session.rs:434`（字段）+ `session.rs:1178`（push）只增不减，`coding_agent.rs` 的 `CloseSessionRequest`（`:734`）/`DeleteSessionRequest`（`:840`）/LRU 淘汰（`:310-338`）均不 drain。harness 侧 `McpClientManager::disconnect`（`crates/agentscope-harness/src/agent/mcp_client_manager.rs:308`）在 acp crate 内零调用（`rg 'mcp_manager\.'` 只命中 `session.rs:1085`/`session.rs:1097`），`mcp_client_manager.rs` 无 `impl Drop`（零命中） | 待办 | ①`AcpAgentSession::release_mcp(&self)`：遍历 `mcp_acp_connections` 逐个 `disconnect()` 并 `Toolkit::remove_tool(<server>__<tool>)`；②在 `CloseSessionRequest`/`DeleteSessionRequest`/`register_session` 淘汰分支调用；③对 stdio/sse/http 记 `client_id` 后调 `mcp_manager.disconnect(client_id)`；验收：新增离线 `tests/acp_mcp_disconnect_test.rs`（in-process `Channel::duplex` + 假 MCP client）断言关闭/删除会话后客户端收到 `mcp/disconnect`、toolkit 不再含该服务器工具 |
| ACP-03 | P1 | **`session/load` 不回放历史**：`AgentCapabilities.load_session(true)` 已广告，但 handler 只回 `modes`/`config_options`，一条 `session/update` 都不发（客户端重启后 UI 为空、agent 却带着上下文） | 无（Rust 原生；ACP v1 `loadSession` 要求用 `session/update` 重放整段会话） | `coding_agent.rs:240 .load_session(true)`；handler `coding_agent.rs:784-811` 的第三个参数是 `_cx`（**连接未被使用，物理上无法回放**），响应只有 `modes(...).config_options(...)`；`rg 'UserMessageChunk'` 在 crate 内零命中，`rg 'session_transcript'` 与 `rg 'inspect_session'` 在 crate 内亦零命中（harness 侧能力存在：`harness_agent.rs:277 session_transcript`、`:252 inspect_session`） | 待办 | 二选一：**(A 首选)** handler 改用 `cx`，先 `agent.inspect_session(&ctx).await` 取投影，按轮次发 `UserMessageChunk`/`AgentMessageChunk`（工具调用发 `ToolCall`+`ToolCallUpdate`）再响应；验收：新增 `tests/acp_load_replay_test.rs`（离线 model）断言 `session/load` 后收到的 chunk 文本序列等于写入时 transcript。**(B)** 若产品决定不回放，则必须 `load_session(false)` 并把 `session/load` 改为 `method_not_found`，避免客户端按协议期待重放 【裁定 2026-10-31：A：handler 改用 cx + agent.inspect_session 回放 chunk】 |
| ACP-04 | P1 | **`session/delete` 不删持久状态**：只从内存注册表移除，journal 与 `.agentscope/acp-sessions/<id>.json` 仍在 → `session/list` 仍列出该会话、`session/load` 仍能成功（删除语义形同「关闭」） | 无（Rust 原生） | `coding_agent.rs:840-850`：`cancel_and_wait` → `session.close()` → `remove_session`，无任何存储删除；`session.rs:451-457 session_config_path` 指向的 sidecar 亦无删除路径。根因在跨 crate：`SessionLogStore`（`crates/agentscope-core/src/session/log.rs:199-214`）只有 `open`/`list`，**没有 `delete`**（`rg 'fn delete' crates/agentscope-core/src/session` 零命中） | 待办 | 跨模块需求（记 §4.2 类）：给 core `SessionLogStore` 增 `async fn delete(&self, key, context) -> Result<()>`（默认实现返 unsupported），`WorkspaceSessionLogStore`/`StoreSessionStorage` 实现之；acp 侧在 `DeleteSessionRequest` 调 `store.delete(...)` + `std::fs::remove_file(config_path)`。若 core 暂不扩 trait，则 acp 侧只能降级为「标记 tombstone」，需产品确认；验收：删除后 `session/list` 不再列出、`session/load` 返回 `invalid_params` 【裁定 2026-10-31：A：core SessionLogStore::delete 默认 Err(unsupported) + acp 调 delete + 删 sidecar】 |
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
- 涉及 SDK 参考时：以 `E:\local-github\agentclientprotocol\rust-sdk`（tag `v2.2.0`）源码与 cargo registry 中 `agent-client-protocol-2.2.0` / `agent-client-protocol-schema-1.9.1` 为准（旧的 `schema-1.6.0` 口径已作废）。

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
