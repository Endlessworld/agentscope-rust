# ACP CodingAgent（Rust 版）— v1 / v2 双版本能力跟踪（Gap Tracker）

> **用途**：本文件是 ACP CodingAgent 实现工作的**单一事实来源**。任何会话开工前必须先读本文件；完成后必须更新对应行的「状态」与「证据」。
> **更新日期**：2026-11-25
> **基线**：`agent-client-protocol` **5.6.0**（schema **1.22.0**，含 `-http` 5.6.0 `server`）；服务端 `crates/agentscope-extensions-acp`，客户端 `D:\IdeaProjects\acp-agent-ui`（`@agentclientprotocol/sdk` **2.12.0**）。
> **分支**：服务端与客户端均为 `feature/acp-v2`。
> **历史**：本文件于 2026-11-25 按「v1 / v2 双版本」重写；v1 时代的条目（`ACP-01..ACP-45`、`ACP-D01..D14`、`ACP-U01..U08`）在 **§7** 保留编号索引与结论，逐条证据见 git 历史（`git log -p -- docs/ACP_AGENT__GAP_TRACKER.md`）。

## 0. 约定

- **状态**：`未开始` / `进行中` / `已完成` / `不适用`（该协议版本没有这个方法）/ `设计决定（不做）`。
- **版本列**：
  - **v1** = `ProtocolVersion::V1` 图；`acp-v2` feature **关闭**时它就是唯一图（v1-only 构建）。默认双栈构建下，它仍是 `protocolVersion != 2` 的兵底路径。
  - **v2** = `unstable_protocol_v2` 下的 v2 图；`acp-v2` **默认开启**，经 `Agent.protocol_router().with_v1().with_v2()` 由 `initialize.protocolVersion` 选路。
- **编号**：`ACP-NN` / `ACP-DNN` / `ACP-U NN` 是 v1 时代历史编号，**不复用**；v2 落地专项从 `ACP2-01` 起单调递增。
- **完工标准**：状态改 `已完成`，同时在「证据」列写**可复现的命令或文件:行**；协议类改动必须有集成测试守护。

## 1. 版本总览

| 维度 | v1 | v2 | 目标 |
|---|---|---|---|
| 规范地位 | 稳定（schema 1.22.0 的 v1 面） | **unstable**（`unstable_protocol_v2`） | v2 全量能力 + v1 全部自定义扩展 |
| 服务端默认 | 关闭 v2 后唯一图 | **双栈，`acp-v2` 默认开启** | 先双栈，再切默认（✔ 已完成），最后删 v1 |
| 方法面 | 16 个规范方法 + 12 个私有扩展 | 21 个规范方法；**12 个扩展全部缺失** | 两版本功能等价 |
| 轮次结束 | `PromptRequest` 响应带 `stopReason` | `session/prompt` 只回 `{messageId}`，靠 `state_update(idle+stopReason)` | 已对齐 |
| 客户端侧方法 | fs / terminal / permission / elicitation / update | **仅 permission / update / elicitation / mcp / auth-status** | v2 无客户端 fs/terminal（agent 自己读写） |
| MCP 传输 | stdio / http / sse / **acp** | stdio / http / **acp**（无 sse） | v2 的 acp 待打通 |

## 2. 服务端能力矩阵（`crates/agentscope-extensions-acp`）

> 列含义：能力 | v1 | v2 | 编号 | 证据 / 备注

### 2.1 握手与传输

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| `initialize` 握手 | 已完成 | 已完成 | — | v2：`v2/mod.rs:59-93`（回 `protocolVersion/info/capabilities/authMethods`） |
| 协议版本协商与路由 | 已完成 | 已完成 | — | `coding_agent.rs::into_service`：`acp-v2` 下 `protocol_router().with_v1().with_v2()`；关闭时 v1 图直连（零路由开销） |
| stdio 传输 | 已完成 | 已完成 | — | 传输与协议版本解耦 |
| WebSocket / HTTP+SSE | 已完成 | 已完成 | — | `agent-client-protocol-http 5.6.0`：`/acp` 同时接 WS 升级与 `POST/GET`，另有 `/health` |
| `$/cancel_request` 单请求取消 | 已完成 | 已完成 | — | 同 SDK 机制（`Responder::cancellation()`） |
| 连接终止清理 | 已完成 | 已完成 | — | `shutdown_sessions()`；ws/http 模式每连接各自建图与 agent |

### 2.2 会话生命周期

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| `session/new` | 已完成 | 已完成 | — | v2 额外推一条**无 stopReason** 的 `state_update(idle)` 就绪快照 |
| `session/prompt` | 已完成 | 已完成 | — | v2 立即回 `{messageId}`，进度走 `session/update` |
| `session/cancel`（通知） | 已完成 | 已完成 | — | 取消当前 Turn + 清空排队；v2 为 notification |
| `session/close` | 已完成 | 已完成 | — | 先 `cancel_and_wait` 再释放 |
| `session/delete` | 已完成 | 已完成 | — | 删 journal + creation record + archive marker |
| `session/list`（cwd / 分页 / 归档过滤） | 已完成 | 已完成 | — | v2 必须带 `cwd` 才合并磁盘历史 |
| `session/status` | 已完成 | 已完成 | — | `open` / `saved`；子会话 running 期可见 |
| `session/set_title` | 已完成 | 已完成 | — | 空标题 → `-32602` |
| `session/set_config_option` | 已完成 | 已完成 | — | v2：`configId` + flatten `{type,value}` |
| `session/archive` / `unarchive` | 已完成 | 已完成 | — | 标记文件；活跃会话归档先按 close 释放 |
| `session/resume` | 已完成 | 已完成 | ACP2-08 | v2 支持 `replayFrom:{type:'start'}`；**v2 拒绝 running 会话**（v1 `load` 不拒） |
| `session/load` | 已完成 | 不适用 | ACP2-08 | v2 无此方法，语义由 `resume + replayFrom` 承担 |
| `session/set_mode` | 已完成 | 不适用 | — | v2 无此方法；mode 是 config option（`configId='mode'`），服务端 `apply_config_option` 已支持 |
| `session/fork` | 设计决定（不做） | 未开始 | ACP2-04 | v1 从未实现（`ACP-D13`）；v2 已注册但恒 `-32602`（`v2/mod.rs:576-585`） |
| `session/options` | 不适用 | 已完成 | ACP2-03 | 快照查询：带 `sessionId` 回该会话当前选项，不带回默认值；未知会话 `-32002`；capability `session.options` 已广告 |
| `additionalDirectories` | 已完成 | 已完成 | — | 双版本都声明并校验目录存在性 |
| `systemPrompt`（append） | 已完成 | 已完成 | — | 仅 `Append`；`Override` → `-32602` |

### 2.3 轮次与事件

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| 轮次结束判定 | 响应带 `stopReason` | `state_update` 的 `idle + stopReason` | — | 跨机实测：`{state:'idle', stopReason:'end_turn'}` |
| 全部 `SessionUpdate` 变体映射 | 已完成 | 已完成 | — | `v2/convert.rs` 覆盖 chunk / tool / plan(3) / commands / mode / config / sessionInfo / usage / notice / compaction(2) / subagent / message(2) |
| turn 级 usage | 已完成 | 已完成 | — | v2 走 `state_update.usage`（字段 `inputTokens/outputTokens/totalTokens/thoughtTokens/cachedReadTokens/cachedWriteTokens`） |
| 上下文窗口 usage | 已完成 | 已完成 | — | `UsageUpdate{used,size,cost?}` |
| notices / compaction | 已完成 | 已完成 | — | v2 无 client 门控（基线能力），转换层恒发 |
| plan operations（ID 化） | 已完成 | 已完成 | — | `PlanUpdate`/`PlanRemoved` 双版本同形 |
| 子代理上报（含嵌套 / stopReason） | 已完成 | 已完成 | — | `subagent_update` 转换存在；v2 外层 sessionId = 直接父会话 |
| 事件背压（4096 有界、丢最新） | 已完成 | 已完成 | — | 与协议版本无关 |
| **暂停（paused）** | 已完成（`_meta.paused=true`） | 已完成 | ACP2-02 | v2 走 `state_update(idle{stopReason:cancelled, _meta.paused:true})`：idle=无前台工作，cancelled+meta 表示可恢复暂停 |
| **排队（queue）** | 已完成（扩展方法） | 已完成 | ACP2-01 | v2 无协议位置，保留扩展方法（v2 图已注册 queue/queue-list/queue-remove） |
| **插话（steer）** | 已完成（扩展方法） | 已完成 | ACP2-01 | 同上 |

### 2.4 权限与 elicitation

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| `session/request_permission` | 已完成 | 已完成 | — | v2 `subject{type:'tool_call'|'command'}` ↔ v1 `toolCall`；选项同形 |
| 权限四选项 + always 落盘 | 已完成 | 已完成 | — | 语义在 session 层，与版本无关 |
| `elicitation/create`（form） | 已完成 | 已完成 | ACP2-09 | 服务端 `client_capabilities_to_v1` 目前只映射 `form` |
| `elicitation/create`（url） | 设计决定（不做，`ACP-D06`） | 未开始 | ACP2-09 | v2 客户端广告 `{form,url}` 时 url 被丢弃 |
| `elicitation/complete` 通知 | 已完成 | 已完成 | — | URL 型表单在浏览器完成 |

### 2.5 MCP

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| stdio MCP server | 已完成 | 已完成 | — | `v2::McpCapabilities.stdio` 恒真 |
| http MCP server | 已完成 | 已完成 | — | |
| sse MCP server | 已完成 | 不适用 | — | v2 schema 删除 `McpServerSse`；客户端需过滤 |
| **MCP-over-ACP（`mcp/message`）** | 已完成 | 已完成 | ACP2-07 / ACP2-14 | 服务端：`AcpTransport{V1,V2}`，`session.rs::connect_mcp_acp` 为 v2 构造 `McpAcpConnection::new_v2`；客户端：v2 连接注册 `mcp/message`（SDK 要求传 params parser）并在 `_connectV2` 里 `_createClientMcp(conn.agent)` |
| **`session.mcp.acp` 广告** | 已完成 | 一致 | ACP2-07 / ACP2-14 | 桥接在 v2 真正可用，`convert.rs` 的 `acp` 映射因此诚实（测试 `acp_v2_mcp_test` 断言广告 + 真实 `tools/list` 往返）。客户端：TS SDK 2.12 的 v2 `McpCapabilities` 无 `acp` 字段会丢弃，`convert.rs` 同时镜射 `session.mcp._meta["io.animus/mcpAcp"]=true`，前端以 `acp ?? _meta` 判定 |
| **v2 能力广告可见性** | 已完成 | 一致 | ACP2-15 | 服务端把 unstable 能力镜射到 `_meta`：`capabilities._meta["io.animus/caps"]=["providers","authStatus"]`、`session._meta["io.animus/sessionCaps"]=["delete","additionalDirectories","systemPrompt","fork","archive","status","title","options"]`。原因：TS SDK 2.12 用 zod 白名单解析，`options`/`archive`/`status`/`title`/`auth.status`/`providers` 全被丢弃；客户端 `readV2AgentCaps` 合并 `_meta` 与标准字段两路来源 |
| MCP 连接收尾（`mcp/disconnect`） | 待办（`ACP-02`） | 未开始 | — | v1 遗留缺口 |

### 2.6 认证与供应商

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| 登录（v1 `authenticate` / v2 `auth/login`） | 已完成 | 已完成 | — | 无真实凭据，恒成功 / 恒 `terminal` 方法 |
| 登出（v2 `auth/logout`） | 不适用 | 已完成 | — | |
| 登录态探测（v1 `auth/status` / v2 `auth/status`） | 已完成 | 已完成 | — | 恒 `not_required` |
| `providers/list` / `set` / `disable` | 已完成 | 已完成 | — | 读写 `models.json`，registry 非空才广告 |
| `providers` 扩展：remove / default / catalog / probe | 已完成 | 已完成 | ACP2-01 | `_acp_extends/providers/*` 共用一个协议无关 handler（`handle_providers_ext`），v2 图已注册；未开 `manage_providers` 时回 `-32601` |

### 2.7 私有扩展（`_acp_extends/*`，共 12 个）

> 这些方法**不在 ACP 规范内**，v2 永远不会「自带」；必须自己在新图上注册。

| 方法 | 功能 | v1 | v2 | 编号 |
|---|---|---|---|---|
| `_acp_extends/session/steer` | 执行中插话，不新建 Turn | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/session/queue` | 把消息排进当前会话队列 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/session/queue/list` | 列出未完成 Turn（位置/状态） | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/session/queue/remove` | 从队列移除 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/session/pause` | 暂停当前 Turn（pausing → paused） | 已完成 | 已完成 | ACP2-01 / ACP2-02 |
| `_acp_extends/session/resume` | 继续暂停的 Turn | 已完成 | 已完成 | ACP2-01 / ACP2-02 |
| `_acp_extends/session/respond` | HITL 批量应答（`answers: map`） | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/fs/list` | 远端工作区目录浏览 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/providers/remove` | 删除供应商 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/providers/default` | 设默认供应商 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/providers/catalog` | 供应商目录 | 已完成 | 已完成 | ACP2-01 |
| `_acp_extends/providers/probe` | 供应商连通性探测 | 已完成 | 已完成 | ACP2-01 |

### 2.8 v2 独有特性

| 方法 / 特性 | 规范位置 | v2 状态 | 编号 | 说明 |
|---|---|---|---|---|
| `session/options` | `AGENT_METHOD_NAMES.session_options` | 已完成 | ACP2-03 | 快照查询（config options / commands），无副作用；未知 `sessionId` → `-32002`；capability `session.options` 已广告 |
| `session/fork` | `unstable_session_fork` | 未开始 | ACP2-04 | 需存储层「复制会话历史 + checkpoint」原语 |
| `tool_call/cancel` | v2 基线 | 已完成（规范语义） | ACP2-05 | 规范：只有 tool call 广告 `capabilities.cancel` 时客户端才可发，且对未知/不可取消回 `-32602`——我们从不广告（ReAct 无单工具取消原语），因此 `-32602` 就是规定答案 |
| `terminal/cancel` | v2 基线 | 已完成（规范语义） | ACP2-06 | 同理：agent 不托管终端、从不广告可取消终端，任何 id 都回 `-32602` |
| `mcp/message` | `unstable_mcp_over_acp` | 已完成 | ACP2-07 | 见 2.5：`AcpTransport{V1,V2}` 发送端抽象 |
| `StateUpdate`（idle/running/requires_action） | v2 基线 | 已完成 | — | `v2/mod.rs` 的 `send_update` |

## 3. 客户端能力矩阵（`D:\IdeaProjects\acp-agent-ui`）

| 能力 | v1 | v2 | 编号 | 证据 / 备注 |
|---|---|---|---|---|
| 协议协商（auto / v1 / v2） | 已完成 | 已完成 | — | `getAcpProtocol()`；`?protocol=` 覆盖；auto 失败回退一次 v1 |
| v2 连接层 | 不适用 | 已完成 | — | `src/services/acp/v2/client.ts`（`AcpV2Connection`） |
| 事件映射到 `EventHandler` | 已完成 | 已完成 | — | 16 种 update；`configOptionUpdate` 的 `configId→id` |
| 轮次结算 | 已完成 | 已完成 | — | 只认 `idle + stopReason`；就绪 idle 被忽略 |
| 会话 new / prompt / cancel / close / delete / list / status / title / config / archive / resume | 已完成 | 已完成 | — | `acpClient.ts` 双分支 |
| 插话 / 排队 / 暂停 / 继续 | 已完成 | 已完成 | ACP2-10 | 统一走 `this._v2 ?? this._ctx`；v2 图注册了同名扩展方法 |
| 远端目录浏览（`fs/list`） | 已完成 | 已完成 | ACP2-10 | 同上（`listRemoteDirectories`） |
| 供应商 remove / default / catalog / probe | 已完成 | 已完成 | ACP2-10 | 同上四件套 |
| `session/options`（v2 独有） | 不适用 | 已完成 | ACP2-03 | `AcpClientManager.sessionOptions(cwd, acpSessionId?)`，`configId→id` 归一 |
| 暂停标记透传 | 已完成 | 已完成 | ACP2-02 | `_handleV2State` 把 `_meta` 随 `onPromptResponse` 透传（与 v1 同形） |
| MCP-over-ACP（声明 `McpServerAcp`） | 已完成 | 已完成 | ACP2-07 / ACP2-14 | 服务端 v2 桥接完成；客户端 v2 已注册 `mcp/message`（带 parser）并在 `_connectV2` 声明 `_clientMcp`（冒烟：`tools/list` 往返成功） |
| SSE MCP server | 已完成 | 不适用 | — | v2 创建会话时过滤 `type==='sse'` |
| fork | 未实现 | 未实现 | ACP2-04 | 本地直接报错 |
| 会话历史（list + replay 打开） | 已完成 | 已完成 | ACP2-08 | v2 用 `resume + replayFrom:{type:'start'}` |

## 4. 语义差异与处置

| # | 差异 | v1 | v2 | 处置 | 编号 |
|---|---|---|---|---|---|
| 1 | 轮次结束信号 | prompt 响应即结束 | `state_update(idle+stopReason)` | 已对齐（客户端只认带 stopReason 的 idle） | — |
| 2 | 恢复运行中的会话 | `load` 不检查 running | `resume` 报 `session still has foreground work` | 统一为「拒绝并提示」或「允许附着」，需定策略 | ACP2-08 |
| 3 | 历史回放时机 | `load` 总是回放 | 仅 `replayFrom.start` 才回放 | 客户端固定传 `replayFrom` | ACP2-08 |
| 4 | 暂停表达 | `stopReason=cancelled` + `_meta.paused` | 无对应位 | **已处置**：`state_update(idle{stopReason:cancelled, _meta.paused:true})` | ACP2-02 |
| 5 | 队列呈现 | `queue/list` | 无 | **已处置**：v2 图注册同一扩展方法 | ACP2-01 |
| 6 | 模式切换 | `session/set_mode` | `set_config_option(configId='mode')` | 客户端改走 config option | — |
| 7 | 未知会话错误 | `-32002` + `data.sessionId` | 同 v2 `session_not_found` | 已对齐 | — |
| 8 | 客户端 fs / terminal | 存在（我们未使用，`ACP-D04`） | 移除 | 无影响；客户端命令执行走客户端 MCP 工具 | — |
| 9 | elicitation URL | 未实现（`ACP-D06`） | 客户端会广告 `url` | 服务端补映射或明确忽略 | ACP2-09 |

## 5. v2 缺口登记（`ACP2-NN`）

| 编号 | 优先级 | 项 | 现状证据 | 落地方案 | 验收 |
|---|---|---|---|---|---|
| **ACP2-01** | P0 | 12 个 `_acp_extends/*` 在 v2 图注册 | ✅ 已完成：`v2/mod.rs` 注册 9 个共享 handler（覆盖 12 个方法），handler 已泛型化为 `P: Protocol` | 在 v2 图追加同批 handler（方法名不变；`cx` 类型为 `v2::ConnectionTo<Client>`，需按需转 `MessageRequest`）；`respond`/`queue` 等只依赖 `AcpAgentSession`，与版本无关 | 新增 `tests/acp_v2_ext_test.rs`：12 个方法各自往返成功；`cargo test -p agentscope-extensions-acp --features acp-v2` 全绿 |
| **ACP2-02** | P0 | 暂停语义映射到 v2 状态机 | ✅ 已完成：`state_update(idle{stopReason:cancelled,_meta.paused:true})` | 暂停时发 `state_update(requires_action?)` 或 `idle` + `_meta.paused`；恢复后发 `running`；需与客户端约定单一规则并写测试 | 集成测试：pause → 收到暂停态；resume → 收尾 `idle(stopReason)` |
| **ACP2-03** | P1 | `session/options` 未实现 | ✅ 已完成：handler + capability `session.options` 已广告 | 注册 handler：未知会话 → `session_not_found`；否则回当前 `configOptions` + `availableCommands`（无副作用） | 单测 + 集成：`session/new` 后调用，返回 3 个 config option |
| **ACP2-04** | P1 | `session/fork` 恒 `-32602` | `v2/mod.rs:576-585` | 给会话存储加「复制历史 + checkpoint」原语；fork 时复制到新 sessionId 并回 `configOptions`；无原语前保持 `-32602` 且**不广告** | 集成：fork 后新会话 `session/list` 可见、`resume` 回放出源会话历史 |
| **ACP2-05** | P1 | `tool_call/cancel` 恒 `-32602` | ✅ 已完成（规范语义）：不广告 `capabilities.cancel` → `-32602` 就是规定答案 | 会话内维护 `toolCallId → CancellationToken` 注册表；工具执行前登记，`tool_call/cancel` 命中则中断该工具并回 `{}`；未命中回 `-32602` | 集成：长跑工具被取消后该 Turn 以 `cancelled` 收尾 |
| **ACP2-06** | P1 | `terminal/cancel` 恒 `-32602` | ✅ 已完成（规范语义）：不托管终端、不广告可取消 → `-32602` | v2 的 terminal 由 agent 侧 `execute` 创建：登记 terminalId → 子进程句柄；cancel 时 kill 并回 `{}` | 集成：启动长命令 → cancel → 进程退出 |
| **ACP2-07** | P1 | MCP-over-ACP 在 v2 不可用 + 广告不一致 | ✅ 已完成：`AcpTransport{V1,V2}` + `McpAcpConnection::new_v2`（`mcp_over_acp.rs`） | ①把 `McpAcpConnection` 的发送端抽象（v1/v2 各一实现，v2 发 `mcp/message`）；②或先修广告（v2 不宣称 `session.mcp.acp`）。**目标：实现，不是隐藏** | 集成：v2 客户端声明 Acp server → 工具可用；`initialize` 广告与实际一致 |
| **ACP2-08** | P1 | `load`/`resume` 语义对齐 | `v2/mod.rs:510-530`（running 拒绝） | 明确「运行中会话是否允许重开」；客户端固定带 `replayFrom`；文档写清差异 | 集成：running 会话 resume 的行为与 v1 对齐或有明确错误码 |
| **ACP2-09** | P2 | elicitation URL 能力映射 | `convert.rs:117-127` 只映射 form | 补 `url`；服务端 URL 型 elicitation 走 `elicitation/complete` | 单测：客户端广告 url 时 `supports_elicitation_url()` 为真 |
| **ACP2-10** | P1 | 客户端 v2 分支补全扩展与 v2 独有 | ✅ 已完成：`acpClient.ts` 统一 `this._v2 ?? this._ctx`，新增 `sessionOptions`；`pnpm typecheck`/`build` EXIT=0 | 逐项接：steer/queue/pause/resume、fs/list、providers 扩展、session/options、fork | `pnpm typecheck` + 真机 v2 冒烟覆盖 12 个扩展 |
| **ACP2-11** | P2 | 集成测试双跑 | 20 个集成测试中 **19 个**用 v1 客户端（`rg -l 'ProtocolVersion::V1|v1::' tests`） | 关键场景（pause/queue/load/subagent/compact/auth）各加 v2 版本或用参数化客户端 | `cargo test --features acp-v2` 下两套客户端都跑过 |
| **ACP2-12** | P3 | 移除 v1 | 双栈并存 | `acp-v2` 默认开启 → 观察一个迭代 → 删 v1 图、`ClientConnection::V1`、`ext.rs` v1 类型、客户端 v1 分支；SDK v2 转正后再执行 | `git grep 'with_v1\|ProtocolVersion::V1'` 仅剩历史文档 |
| **ACP2-13** | P1 | 能力-方法契约测试 | 现在只靠人工核对 | 每个广告的能力必须存在能应答的方法且不恒 `-32602`；表驱动测试 | 新增 `tests/acp_v2_contract_test.rs` |
| **ACP2-15** | P1 | 能力广告可见性 | 客户端只能看到 `session.{prompt,mcp,delete,additionalDirectories,systemPrompt}`；`archive/status/title/options/fork`、`auth.status`、`providers` 被 SDK 丢弃（原始 JSON 里都在） | 服务端在 `v2/mod.rs` 的 initialize handler 里镜射 `_meta`（`io.animus/caps`、`io.animus/sessionCaps`）；客户端 `readV2AgentCaps` 用 `_meta` 兜底 | 真机冒烟：`sessionArchive/sessionStatus/sessionTitle/authStatus/providers=true`、`sessionFork=false` |

## 6. 路线图与验收

| 批次 | 内容 | 验收命令 |
|---|---|---|
| **V2-P0** | `ACP2-01`（12 个扩展注册）、`ACP2-13`（契约测试）、`ACP2-07` 的广告修正 | `cargo test -p agentscope-extensions-acp --features acp-v2` 全绿 |
| **V2-P1** | `ACP2-02`（暂停映射）、`ACP2-03`（options）、`ACP2-05/06`（工具与终端取消）、`ACP2-07`（MCP 真实现）、`ACP2-08`、`ACP2-10`（客户端） | 同上 + 客户端 `pnpm typecheck` / 真机冒烟 |
| **V2-P2** | `ACP2-04`（fork）、`ACP2-09`（url elicitation）、`ACP2-11`（测试双跑） | 回归 + 双客户端测试 |
| **V2-P3** | `ACP2-12`（切默认 / 删 v1） | 全量 `cargo test --workspace -j 2 --no-fail-fast` EXIT=0 |

**统一验证口径**：`cargo test -p agentscope-extensions-acp`（默认）与 `--features acp-v2` 都必须 `EXIT=0` 且无 `failures:` 段；客户端 `pnpm typecheck` / `pnpm build` 均 EXIT=0。

## 7. v1 时代历史编号索引（不复用）

> 逐条证据见 git 历史；此处只保留编号、主题与最终结论，供追溯。

**缺口项 `ACP-01..ACP-45`**

| 编号 | 主题 | 结论 |
|---|---|---|
| ACP-01 | `running` 状态无上限等待 | 已完成（`closed`/`cancel_epoch` 轮询臂；无进展看门狗按裁定不做） |
| ACP-02 | MCP 连接生命周期未收尾 | **仍待办**（v1 遗留） |
| ACP-03 | `session/load` 不回放历史 | 已完成（`replay_notifications`） |
| ACP-04 | `session/delete` 不删持久状态 | 已完成（`SessionLogStore::delete` + `forget_session`） |
| ACP-05 | 集成测试依赖预构建 bin | 待办 |
| ACP-06 | prompt 错误映射（未知会话应 `-32002`） | 待办 |
| ACP-07 | 探活时构建完整 `HarnessAgent` | 待办 |
| ACP-20..32 | 会话/prompt/权限/MCP/compaction/取消/feature/stdio/门控/usage/respond/elicitation/清理链 | 全部已完成 |
| ACP-33..39 | set_title / status / auth-status / archive / additional_directories / systemPrompt / subagents | 全部已完成 |
| ACP-40 | `session/fork` | 暂缓（`ACP-D13`）；本轮改为 **ACP2-04** 推进 |
| ACP-41..45 | `$/cancel_request` / termination / 日志脱敏 / 并发模型 / 优雅关闭 | 已完成 |

**设计决定 `ACP-D01..D14`**：`D01`（逐个开 unstable）、`D02`（不用聚合 `unstable`）、`D03`（不用 SDK 内建 MCP server）、`D04`（不实现客户端 fs/terminal）、`D05`（`/compact` 命令目录）、`D06`（不做 url elicitation，**本轮改为 ACP2-09**）、`D07`（不做 MetaCapability/NES/debug 回调）、`D08`（不做 proxy chains）、**`D09`（不实现 protocol v2 —— 已作废，由本轮 v2 落地取代）**、`D10`（不引入 HTTP 传输 —— 已作废，已落地 ws/http）、`D11`（Conductor/polyfill/rmcp/testy 不作运行时依赖）、`D12`（NES 移除无需动作）、**`D13`（fork 暂缓 —— 由 ACP2-04 取代）**、`D14`（不做 agent 工具→MCP server 反桥）。

**升级阻断 `ACP-U01..U08`**（SDK 2.2.0→5.0.0）：全部已完成（feature/MSRV、`schema::v1→v1::schema`、`Agent.v1()`、MCP request-scoped 重写、错误类型版本中立、wire 行为回归、测试工具链、fmt 基线）。

## 8. 变更日志

| 日期 | 变更 |
|---|---|
| 2026-11-25 | 重写为 v1/v2 双版本结构；新增 `ACP2-01..ACP2-13`；`ACP-D09`/`ACP-D10`/`ACP-D13` 因 v2 落地作废或转由 `ACP2-*` 承接；基线升至 SDK 5.6.0 / schema 1.22.0 |
| 2026-11-25 | 服务端 `ACP2-01/02/03/05/06/07` 落地：12 个扩展方法注册进 v2 图（`handle_*` 泛型化）、`session/options` 实现并广告、暂停映射为 `idle+_meta.paused`、MCP-over-ACP 增加 v2 发送端、`tool_call/terminal cancel` 明确为规范语义；新增 `tests/acp_v2_ext_test.rs`、`tests/acp_v2_mcp_test.rs`、`session/options` 集成测试；两套全量 EXIT=0 |
| 2026-11-25 | 客户端 v2 MCP 接入（ACP2-14）：`v2/client.ts` 注册 `mcp/message`（v2 方法表无此方法，必须传 params parser）并暴露 `agent`；`_connectV2` 调用 `_createClientMcp`；`readV2AgentCaps` 以 `mcp.acp ?? _meta["io.animus/mcpAcp"]` 判定；服务端 `convert.rs` 在 `session.mcp._meta` 镜射 acp 标记（TS SDK 2.12 反序列化丢弃 `acp`）。真机冒烟：v2 建会话 + acp server → `tools/list` 往返成功；客户端 `typecheck`/`build` EXIT=0 |
| 2026-11-25 | 客户端抽象层收敛（阶段 1）：新增 `AcpChannel` 会话语义方法（`sessionNew/sessionLoad/sessionResume/sessionFork/sessionClose/sessionList/sessionPrompt/sessionSetConfigOption/sessionOptions` + `providersList`），`acpClient.ts` 的 `if (this._v2)` 22→8→**0**、`this._v2` 67→4（仅剩连接句柄），删除 8 个 `_*V2` 私有方法；v2 专属差异（SSE 过滤、`include`→`null`、`configId`→`id` 归一化、fork 抛错）全部收进 `V2Channel`。另修 ACP2-15 能力可见性（`_meta` 镜射 + 客户端兜底）。`pnpm typecheck`/`build` EXIT=0；服务端默认特性与 `--features acp-v2` 各 25 target EXIT=0 |
| 2026-11-25 | **双栈改为默认**：`acp-v2` 加入 `default` features（`default = ["rusqlite","durable","acp-v2"]`），`xtask` 三个变体（native/durable/sqlite）因用 `--no-default-features` 而显式补 `acp-v2`。同一进程实测混连自适应（18899）：`protocolVersion:1` → `1`+`agentCapabilities`+`agentInfo`；`protocolVersion:2` → `2`+`capabilities`+`info`；第三条再发 v1 仍回 `1`。记录关键差异：v2 `initialize` 用 `info`/`capabilities`，v1 用 `clientInfo`/`clientCapabilities`（混用报 `-32602 missing field info`）。客户端 `getAcpProtocol` 增加设置未初始化兑底，默认仍为 `auto`（优先 v2，不支持才回退 v1）。默认特性回归 25 target / 0 failures / EXIT=0；`pnpm typecheck`/`build` EXIT=0 |
