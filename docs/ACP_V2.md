# ACP 协议 v2（草案）双栈接入

> 状态：**服务端已落地并通过测试**（`feature/acp-v2` 分支，`acp-v2` 默认开启，单进程双栈自适应）；客户端适配见 `acp-agent-ui` 仓库同名分支（默认 `auto`，优先 v2、不支持才回退 v1）。
> 适用范围：`crates/agentscope-extensions-acp`。

## 1. 为什么是双栈而不是升级

v2 不是 v1 的小版本扩展，而是另一套协议：`session/prompt` 先回 `messageId` 再后台执行、turn 进度改由 `state_update` 快照表达、没有 `session/load`、没有 mode（改由 config option 承载）、`session/list` 变成必需能力、错误码 `-32002` 语义改为 `session_not_found`。因此 v1 与 v2 必须**共存**，由 `initialize` 协商路由，而不是原地替换。

## 2. 特性门控

```toml
# crates/agentscope-extensions-acp/Cargo.toml
acp-v2 = [
    "agent-client-protocol/unstable_protocol_v2",
    "agent-client-protocol/unstable_session_fork",
]
```

默认**开启**（`default = ["rusqlite", "durable", "acp-v2"]`）：因此**任何**构建产物都同时服务 v1 与 v2，由 `initialize` 协商路由，无需客户端指定构建变体。

需要 v1-only 二进制时可以关掉：`--no-default-features --features "rusqlite,durable"`（或依赖方 `default-features = false`）。关闭时编译产物与行为与引入 v2 前完全一致（v1 图直接作为 service 的 handler，不经过路由层）。

> `unstable_protocol_v2` 会移除下游可见的 `ProtocolVersion::LATEST`。本仓已无该符号引用（全部显式 `V1`/`.v1()`），所以默认开启对 workspace 零破坏。

构建：

```powershell
cargo build -p agentscope-extensions-acp --bins   # 默认即双栈
cargo xtask acp build sqlite                      # xtask 各变体自动带 acp-v2
```

### 2.1 两版 `initialize` 的线上形状不同

路由器靠 `protocolVersion` 选图，但两版的参数/响应字段名也不同，不能共用一份 JSON：

| 位置 | v1 | v2 |
|---|---|---|
| 请求：客户端信息 | `clientInfo` | `info` |
| 请求：客户端能力 | `clientCapabilities` | `capabilities` |
| 响应：智能体信息 | `agentInfo` | `info` |
| 响应：能力 | `agentCapabilities` | `capabilities` |

把 v1 包装的 `initialize` 原样发给 v2 会得到 `-32602 invalid initialize params: missing field \`info\``。客户端必须按目标版本构造请求。

同一个二进制、同一个进程可以同时服务两种客户端（实测，`--serve --port 18899`）：

```
POST initialize {protocolVersion:1, clientInfo, clientCapabilities, ...}
  → protocolVersion=1, agentCapabilities, agentInfo
POST initialize {protocolVersion:2, info, capabilities}
  → protocolVersion=2, capabilities, info=agentscope-coding-agent/0.2.0
  → capabilities._meta = {"io.animus/caps":["providers","authStatus"]}
  → session._meta = {"io.animus/sessionCaps":["delete",...,"options"]}
POST initialize {protocolVersion:1, ...}  （第三条连接）
  → 仍为 protocolVersion=1
```

## 3. 架构

```
into_service()
  └─ #[cfg(feature = "acp-v2")] Agent.protocol_router().with_v1(v1 图).with_v2(v2 图)
       ├─ v1 图：coding_agent.rs（原样保留，零改动语义）
       └─ v2 图：src/v2/mod.rs（handler）+ src/v2/convert.rs（类型转换）
```

- **协议路由**由 SDK 的 `AgentProtocolRouter` 完成：它读第一帧 `initialize`，按 `protocolVersion` 选图；选择后该连接不再改路由。
- **turn 执行复用 v1 机制**：v2 handler 只做协议适配，prompt 仍走 `AcpAgentSession::run_prompt_cancellable`，因此 durable/native 驱动、取消、排队、暂停、子代理等能力对 v2 会话同样生效。
- **转换层**（`convert.rs`）负责：内容块、MCP server、system prompt、config option（`id` → `configId`）、capabilities、tool call update、plan、usage、session info、stop reason、错误码，以及 v2 缺少但 v1 渲染器需要的 `messageId`（`TurnIds`）。
- **MCP-over-ACP 的能力广告**：标准字段是 `capabilities.session.mcp.acp`（Rust schema 1.22 在 `unstable_mcp_over_acp` 下提供）。但 TypeScript SDK 2.12 的 v2 `McpCapabilities` 类型里**没有** `acp`，反序列化时会静默丢弃，JS 客户端因此看不到。所以 `convert.rs` 额外把标记镜射到 `session.mcp._meta["io.animus/mcpAcp"] = true`（`_meta` 所有客户端都会保留）；标准客户端继续读 `acp`，本仓前端以 `acp ?? _meta` 兜底。服务端从未停止广告 `acp`（已用裸 JSON-RPC 抓包验证）。

## 4. 方法矩阵

| v2 方法 | 实现 | 说明 |
|---|---|---|
| `initialize` | ✅ | 回 `protocolVersion`/`info`/`capabilities`/`authMethods`（仅 `terminal` 一种认证方式）；把客户端能力投影成 v1 视图供共享会话逻辑使用 |
| `auth/login` `auth/logout` `auth/status` | ✅ | login 为 no-op（terminal 方式由客户端自己跑命令）；logout 无本地凭证可撤销；status 映射 v1 `AuthState` |
| `providers/list` `providers/set` `providers/disable` | ✅ | 与 v1 同一套 `ProviderRegistry` / `providers_admin` 逻辑（含阻塞线程池） |
| `session/new` | ✅ | 回 `configOptions`（`configId`）+ `availableCommands`，随后推 `state_update(idle)` 就绪快照 |
| `session/prompt` | ✅ | 立即回 `{messageId}`；推 `user_message` 回显 → `state_update(running)` → 正文/工具/计划等 update → `state_update(idle{stopReason, usage})` |
| `session/cancel` | ✅ | 取消当前 turn 并清空排队 |
| `session/close` | ✅ | 关闭会话、等待 turn 收尾（≤15s）、移出注册表 |
| `session/list` | ✅ | 支持 `cwd`、`cursor`、`archived: exclude/only`；**带 `cwd` 才会合并磁盘历史** |
| `session/status` | ✅ | `open` / `saved` |
| `session/set_title` | ✅ | 回 `title`，并推 `session_info_update` |
| `session/set_config_option` | ✅ | 仅接受带 `value` 的 id 形态；回全量 `configOptions` 并推 `config_option_update` |
| `session/archive` `session/unarchive` | ✅ | 与 v1 同一套 marker 语义 |
| `session/delete` | ✅ | 活跃会话先取消再 `forget_session`；否则按 workspace 定位 |
| `session/resume` | ✅ | 已存在会话直接复用（拒绝有前台工作时 resume）；不存在则按 workspace 校验后重建；`replayFrom: start` 触发回放 |
| `session/fork` | ❌ 不广告 | 会话存储无 fork 原语，回 `-32602`；capability 不声明 |
| `session/options` | ✅ | 会话无关快照：带 `sessionId` 回该会话当前选项，不带则回“新会话将拿到的默认选项”；未知会话回 `-32002`。**纯快照，不创建/不恢复任何会话** |
| `tool_call/cancel` `terminal/cancel` | ✅ 规范语义 | 两者都要求被取消对象“当前广告 `capabilities.cancel`”客户端才可发；本实现从不广告（ReAct 循环无单工具取消原语、agent 不托管终端），故按规范对未知/不可取消对象回 `-32602`，而不是 `-32601` |

**暂停（pause）在 v2 的表达**：v1 用 `PromptResponse{stopReason: cancelled, _meta.paused: true}`；v2 的 prompt 只回 `{messageId}`，因此暂停表现为 `state_update(idle{stopReason: cancelled, _meta.paused: true})`——`idle` 表示无前台工作，`cancelled` + `_meta.paused` 区分“可恢复的暂停”与真正的取消。

客户端侧请求：`session/request_permission` 与 `elicitation/create` 通过 `AcpClientHandle` 的 v2 分支桥接回共享会话逻辑；`session/update` 由 v2 渲染路径发出。

## 5. 已知限制

1. **`session/fork` 未实现**：`AcpAgentSession`/durable 存储没有 fork 原语，v2 侧不广告能力，调用回 `-32602`。
2. **无 mode**：v2 客户端通过 config option `mode` 切换，`current_mode_update` 这类 v1 通知不会转发到 v2。
3. **`terminal/*`**：未实现终端托管（v1 也未实现），因此 agent 从不创建终端、也不广告可取消终端；`terminal/cancel` 对任何 id 都回 `-32602`（规范对“未知终端”的答案）。
4. **单工具取消**：tool call 从不广告 `capabilities.cancel`（ReAct 循环没有按工具取消的原语），需要停手请用 `session/cancel` 或 `_acp_extends/session/pause`。
5. **`session/options` 的会话无关快照是“默认值”**：不带 `sessionId` 时按 agent 广告的模型/模式/思考档位组合默认值回答（规范称其为快照，最终以 `session/new` 的响应为准）；带 `sessionId` 时回答该会话的真实当前值。

## 6. 测试

```powershell
cargo test -p agentscope-extensions-acp        # 默认即双栈：全部 v1 套件 + v2 套件（25 个 target）
# v1-only 对照（只用来看 v1 图在无路由时仍全绿）
cargo test -p agentscope-extensions-acp --no-default-features --features "rusqlite,durable"
```

`tests/acp_v2_session_test.rs`（`#![cfg(feature = "acp-v2")]`）覆盖：

- `v2_session_lifecycle_round_trip`：`initialize` → `session/new` → `session/prompt`（校验 messageId、`user_message` 回显、`agent_message_chunk` 正文、`idle{EndTurn}`）→ `set_config_option` → `status` → `set_title` + `list` → `archive`（`archived: only` 命中）+ `unarchive` → 未知会话 `resume` 报错 → `close` + `delete`。
- `v1_client_still_negotiates_through_the_router`：同一 service 上 v1 客户端 `initialize`/`new`/`prompt` 不回归。
