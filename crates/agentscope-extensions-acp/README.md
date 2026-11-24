# agentscope-extensions-acp

把 AgentScope Rust 的 `HarnessAgent` 暴露成一个 **ACP（Agent Client Protocol）编程智能体**：
任何实现了 ACP 的编辑器 / 客户端都能把它当作编码助手用——读改文件、执行 shell、加载 skills、
派发子智能体、自动压缩上下文。附带的 REPL 客户端可以在没有 IDE 的情况下直接验证。

> 状态：`0.2.0`。协议实现基于 [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) 5.0.0（schema 1.15.1，v1 协议线）。

## 两个可执行文件

| 二进制 | 角色 | 说明 |
| --- | --- | --- |
| `acp_harness_agent` | ACP **agent**（server 侧） | 默认 stdio 传输（无参数、不监听端口，由客户端拉起）；`--ws` / `--http [--port 8888]` 时用 SDK 的 HTTP/WS 传输监听 `0.0.0.0`；在 `initialize` 中自报 `agentscope-coding-agent` |
| `acp_agent_cli` | ACP **client**（REPL） | 自动拉起上方的 agent 子进程，用于本地调试 |

两者默认都说 stdio 上的 JSON-RPC，因此可以被任意客户端用「一个命令」接入；
`acp_harness_agent --ws` / `--http` 则换成监听 `ws://0.0.0.0:8888/acp`（HTTP+SSE 共用同一端点，详见下文）。

## 提供什么

- **完整编码循环**：ReAct 推理 + 工具调用，事件流式回传给客户端（消息分片、工具调用状态与位置、计划更新）
- **三种审批模式**：`plan`（只读）/ `accept_edits`（默认，改文件免问、shell 需确认）/ `yolo`（全自动）
- **权限记忆**：用户选择「总是允许 / 总是拒绝」后写入工作区，跨会话生效
- **运行时切换**：`model` 与 `thought_level` 作为 ACP config option 暴露，客户端可随时切换
- **会话持久化**：`session/new|load|resume|list|delete|close`，历史以 journal 为准，重启后恢复；默认走 durable 驱动（跨进程可续跑），也可切到轻量的原生驱动（见「运行模式」）
- **执行中追加输入**：插话（`steer`，折进当前回合而不打断模型调用）、排队（`queue`，准入成 durable Turn，可单独撤回）、暂停 / 继续（`pause` / `resume`，原地保留位置与工具结果）。只有 durable 驱动提供（见「排队、插话与暂停」）
- **上下文压缩**：超过阈值自动压缩，并在客户端支持时推送进度通知；也可主动触发——模型调用 `compact_context` 工具（由用户提示词驱动），或直接发送 `/compact`（宿主立即压缩，不占模型回合）
- **长任务不霸占回合**：`execute` 超过 240 秒自动转后台（执行上限 1800 秒），模型先拿到占位结果继续推理，真实输出在后续 wakeup 回合注入
- **MCP**：支持客户端以 `stdio` / `http` / `sse` 声明的 MCP server，也支持 MCP over ACP（SDK 5.0 的 request-scoped 语义）
- **Skills**：用户级与项目级 skill 目录，按需通过 `load_skill_through_path` 读取
- **子智能体**：内置 `general-purpose`，另可从 `<workspace>/subagents/*.md` 声明

## 运行模式

同一个协议面由两套 turn 驱动实现，客户端无感，差别只在**持久化保证**：

| 驱动 | 何时编译 | 会话历史 | 跨进程续跑 | 崩溃原子 | 多宿主 |
| --- | --- | --- | --- | --- | --- |
| `durable`（默认） | feature `durable`（默认开启） | `<workspace>/.agentscope/durable/` 下的 journal；编译 `durable-sqlite` 时为 `durable.db` | ✅ | ✅ | ✅（依赖共享存储语义；SQLite WAL 不能放网络文件系统） |
| `native` | 任意构建 | `<cwd>/.agentscope/acp-sessions/<session_id>/`（逐条消息 JSON 行 + `turns.json`） | ❌ | ❌（未写完的最后一条会被丢弃） | ❌ |

- **运行时切换**：`AGENTSCOPE_TURN_DRIVER=native` 选原生驱动；未设置、拼错或空值一律保持 durable——两种历史存放位置不可互换，不能靠“猜”切过去。构建里没有 `durable` feature 时只有一个驱动，该变量被忽略。
- **SQLite 后端**：编译 `durable-sqlite` 后默认用 `<workspace>/.agentscope/durable/durable.db`；`AGENTSCOPE_DURABLE_STORE=file` 保留 FileStore，无需重新构建。
- **掉电语义**：FileStore 是“进程崩溃安全、掉电不安全”（不 `fsync`）；重要部署请在文件系统层做快照。
- **插话 / 排队 / 暂停只在 durable 驱动下可用**：它们依赖 durable 的 Turn 准入与控制通道；`native` 构建（含 `lite` 预编译包）遇到这些扩展会回错误，`queue/list` 永远是空。

## 安装

```bash
cargo install agentscope-extensions-acp --version 0.2.0

# 或从源码（仓库根目录）
cargo install --path crates/agentscope-extensions-acp
```

要求 Rust 1.89+。默认 feature 是 `rusqlite` + `durable`：前者是 bundled SQLite（工作区索引），
后者是默认的 durable turn 驱动。按需裁剪：

- `--no-default-features --features lite`：不编 C 引擎，索引降级为目录扫描；同时**不编译 durable**，只剩原生驱动（发布包即此组合）。
- `--features durable-sqlite`：在 durable 之上启用 SQLite 会话后端（默认即 `durable.db`）。
- `--features alloc-stats`：诊断构建，`[mem]` 行报告真实堆占用。

## 快速开始

### 1. 配置模型

**最快的方式**（推荐）：用环境变量，或者跑一次向导 —— 两者都只需要一个 API key。

```bash
export DEEPSEEK_API_KEY=sk-...            # PowerShell: $env:DEEPSEEK_API_KEY='sk-...'
acp_harness_agent                         # 直接就能用，无需任何文件配置
```

或者让向导来问（客户端里有 `--setup` 认证入口时会直接调这个）：

```bash
acp_harness_agent --setup
```

向导从 [models.dev](https://models.dev) 的模型目录里列出 200+ 个 provider，**不问你模型名、不问你
base_url**：选中一个 provider、填一个 API key 即可，写回 `~/.agentscope/models.json`（已存在的其它
provider 原样保留）。已导出的 provider 密钥会被自动识别，那时一个问题都不用回答。

> 目录表（含每个 provider 的网关地址与环境变量名）随 agent 自动下载；全新安装后第一次
> 启动只负责下载，密钥探测从下一次启动起生效。

也可以手写配置。首次运行会自动生成 `~/.agentscope/models.json`（Windows：`%USERPROFILE%\.agentscope\models.json`），
指向一个公开的免费端点。**实际使用建议改成你自己的 provider**：

```json
{
  "default_provider": "main",
  "default_model": "gpt-5.1-codex",
  "providers": {
    "main": {
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-...",
      "models": {
        "completions": ["gpt-4.1", "deepseek-chat"],
        "responses": ["gpt-5.1-codex"],
        "messages": ["claude-sonnet-4-5"]
      }
    }
  }
}
```

`base_url` 只写到版本前缀（`.../v1`），端点后缀由协议分组决定，**不要写进 `base_url`**。
`models` 也接受扁平数组（按模型名前缀自动归组）：

```json
"models": ["gpt-5.1-codex", "claude-sonnet-4-5", "deepseek-chat"]
```

文件允许 `//`、`/* */` 注释与尾随逗号；解析失败时**不会覆写**你的文件，只会告警并回退到内置默认。

### 2. 接到你的 ACP 客户端

在客户端的「自定义 agent」配置里把命令指向 `acp_harness_agent`（或它的绝对路径），不需要参数。
客户端会把工作目录（cwd）作为 workspace 传过来，agent 在该目录下读写文件、执行命令。

想先确认能不能通，可以手动跑一次握手（`protocolVersion` 是整数，当前稳定值 `1`）：

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
  | acp_harness_agent
```

返回里带 `agentInfo.name = "agentscope-coding-agent"` 就说明握手成功。报文格式见
[用各语言客户端 SDK 接入](#用各语言客户端-sdk-接入)。

### 3. 或者用内置 REPL

```bash
acp_agent_cli
```

REPL 会在当前目录拉起 agent 并开始对话。斜杠命令：

| 命令 | 作用 |
| --- | --- |
| `/model <id>` | 运行时切换模型 |
| `/models` | 列出 `models.json` 中的模型（按 provider 分组） |
| `/quit` · `/exit` · `Ctrl-D` | 退出 |

启动时可用 `ACP_SESSION_ID=<id>` 打开已有会话，再用 `ACP_SESSION_REPLAY=1` 重放其历史。

### 4. 或者自己监听一个端口（WebSocket / HTTP+SSE）

默认是 stdio，由客户端把你当子进程拉起。想让它当一个常驻服务时加 `--ws` 或 `--http`：

```bash
acp_harness_agent --ws                  # ws://0.0.0.0:8888/acp
acp_harness_agent --http                # http://0.0.0.0:8888/acp（同一条监听）
acp_harness_agent --ws --port 9000      # 指定端口（1-65535）
```

两个开关是**同一条监听**：传输用的是 SDK 自带的 `agent-client-protocol-http`
（`AcpHttpServer`），它在同一个 `/acp` 端点上既接 WebSocket 升级，也接 HTTP + SSE。
握手、分帧、会话流路由都由 SDK 负责，本 crate 只提供「一条连接一份 agent」的工厂。

- **绑 `0.0.0.0`**，局域网 / 其它机器上任意能访问该端口的客户端都能连；服务不退出，
  连接断开只排空并释放那条连接自己的会话；
- WebSocket：**一条文本帧 = 一条 JSON-RPC 消息**；
- HTTP + SSE（客户端按顺序走四步）：

  | 请求 | 作用 |
  | --- | --- |
  | `POST /acp`（`Content-Type: application/json`，body 为 `initialize`） | 建连接，返回 `200` + JSON-RPC 响应体 + `Acp-Connection-Id` 响应头 |
  | `GET /acp`（`Accept: text/event-stream`，带 `Acp-Connection-Id`） | 该连接上 agent→client 的消息流；每条连接只能有一条 |
  | `POST /acp`（带 `Acp-Connection-Id`） | 发消息，返回 `202 Accepted`；响应从 SSE 流回来 |
  | `DELETE /acp`（带 `Acp-Connection-Id`） | 关闭连接（`202`） |

  会话级另有一条 `Acp-Session-Id` 头可用的 SSE 流（`session/new` 之后按会话订阅）。
- `GET /health` 返回 `ok`；浏览器来源不设限（**协议本身没有认证**，暴露面由绑定的网卡决定）；
- `--port` 也可以单独给（隐含监听），`--port=9000` 与 `--port 9000` 等价；
- 监听地址会打到 stderr：`[serve] ws://0.0.0.0:8888/acp · http://0.0.0.0:8888/acp`。

最小 HTTP 客户端（curl，可直接跑通 `initialize` + `session/new`）：

```bash
curl -s -D - -X POST http://127.0.0.1:8888/acp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}'
# → 200，响应头里有 acp-connection-id: <id>
curl -N http://127.0.0.1:8888/acp -H 'Accept: text/event-stream' -H 'Acp-Connection-Id: <id>' &
curl -s -X POST http://127.0.0.1:8888/acp -H 'Content-Type: application/json' \
  -H 'Acp-Connection-Id: <id>' -d '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}'
# → 202 Accepted；session/new 的结果从上面那条 SSE 流里回来
```

## 会话模式与权限

| 模式 | 行为 |
| --- | --- |
| `plan` | 只读探索：只自动允许读取类工具，写文件 / 执行命令一概拒绝 |
| `accept_edits`（默认） | `write_file` / `edit_file` / `todo_write` 自动允许；`execute`（shell）**每次询问** |
| `yolo` | 跳过全部权限检查（慎用） |

- 权限询问走 ACP 的 `session/request_permission`，选项包含一次性允许 / 总是允许 / 总是拒绝
- 选「总是」后规则写入 `<workspace>/.agentscope/permissions.json`，后续会话直接生效
- 想重置就去删这个文件，或删除对应条目

## ACP 能力

### 处理的方法

| 方法 | 说明 |
| --- | --- |
| `initialize` | 协商协议版本与客户端能力，返回 agent 能力与身份 |
| `session/new` | 新建会话，返回 `modes` 与 `config_options`；同时桥接客户端声明的 MCP server |
| `session/prompt` | 跑一轮对话；支持图像输入（按所选模型能力）、客户端 `embedded_context`。它同时是队列的驱动器：先把该会话遗留的排队 Turn 跑完，自己的消息跑完后再排空一次（执行期间排进来的消息跟随同一个请求） |
| `session/cancel` | 停止：取消当前 turn（`StopReason::Cancelled`）**并清空整个队列**；参数是子会话 id 时改为中断该子代理 |
| `session/set_mode` | 切换 `plan` / `accept_edits` / `yolo` |
| `session/set_config_option` | 切换 `model` / `thought_level` |
| `session/load` · `session/resume` | 从 journal 恢复会话 |
| `session/list` · `session/delete` · `session/close` | 会话管理 |
| `session/set_title` | 客户端改会话标题（空白标题回 `-32602`） |
| `session/status` | 会话存活探测：`open`（本进程持有）/ `saved`（仅磁盘有）/ `-32002` |
| `session/archive` · `session/unarchive` | 归档（默认从 `session/list` 隐藏，`archived` 过滤可选 `only`/`include`） |
| `auth/status` | 真实登录态：有真实凭据（含环境变量）为 `authenticated`，一处也没有为 `unauthenticated` |
| `providers/list` · `providers/set` · `providers/disable` | 模型供应商管理；`acp_harness_agent` 默认开启，模型表与展示信息走 `_meta`（见下节） |
| `_acp_extends/providers/remove` · `default` · `catalog` · `probe` | 删供应商 / 改默认 / models.dev 目录 / 探测端点（ACP 扩展） |
| `_acp_extends/session/respond` | 私有扩展：回答被挂起的结构化提问 |
| `_acp_extends/session/steer` | 插话：把一条消息折进当前活跃 Turn（不新建 Turn、不排队），回 `{turnId}`；没有活跃 Turn 时 `data` 为 `no_active_turn` |
| `_acp_extends/session/queue` | 排队：把消息准入成一个 durable Turn，回 `{turnId, position}`（`position` 从 1 起） |
| `_acp_extends/session/queue/list` | 列出未收尾的 Turn：`{items: [{turnId, position, status}]}`；不回消息正文 |
| `_acp_extends/session/queue/remove` | 撤回还没开跑的 Turn：`{turnId, removed}`；已开始/已收尾的回 `removed: false` |
| `_acp_extends/session/pause` | 原地暂停运行中的 Turn：`{turnId, status}`（`pausing` → `paused`），保留位置与工具结果 |
| `_acp_extends/session/resume` | 继续一个暂停的 Turn：请求直到 Turn 收尾才返回，期间照常推送 `session/update`，回 `{turnId, stopReason}` |

### 按客户端能力门控

只有在客户端声明了相应能力时才会发送：`session/notice`（会话提示）、压缩进度、plan 操作通知、
`end_turn` 的 token usage、MCP over ACP。没声明就不会收到，也不会报错。

### 排队、插话与暂停（`_acp_extends/session/*`）

执行中客户端有三种「加话」的方式，语义不同、不能互相替代（都只在 durable 驱动下可用）：

| 手段 | 方法 | 效果 | 打断在途的模型调用 / 工具执行 |
| --- | --- | --- | --- |
| 插话 | `_acp_extends/session/steer` | 文本作为一个 Signal 折进**当前** Turn，在下一个推理边界进上下文 | ❌ 照常跑完 |
| 排队 | `_acp_extends/session/queue` | 新准入一个 Turn（`Queued`），当前 Turn 收尾后按 FIFO 跑 | ❌ 等本轮结束 |
| 暂停 | `_acp_extends/session/pause` | 运行中的 Turn 在最近的边界停下，保留位置与工具结果，等 `resume` | ✅ 唯一会立即停下的手段 |

「先暂停、再插话、再继续」是唯一能让 agent 提前改变方向的组合：单独的插话不会打断正在跑的调用。

时序与契约：

- **每个 Turn 推的 `session/update` 都带 `_meta.turnId`**：一个 `session/prompt` 请求可能先后跑多个 Turn（排队的跟着它一起跑），客户端按 `turnId` 把流式内容绑到正确的消息气泡。
- **prompt 是唯一的驱动器**：请求入口先排空遗留的 `Queued` Turn（FIFO；进程崩溃后残留的也先跑），再跑自己的消息，收尾后再排空一次——执行期间排进来的消息跟随同一个请求，不会留下没人驱动的队列。
- **队列是 durable 的**：`queue` 一返回 Turn 就已写进 store，崩溃/重启后仍是 `Queued`，可被 `queue/list` 看到、被 `queue/remove` 撤回（撤回按 `Queued` 状态判定，已开跑的 Turn 不会被误停）。
- **Stop = 全停**：`session/cancel` 取消运行中的 Turn 之后还会逐个取消 `Queued` 的（请求 id `stop-<turnId>`，可重放），紧接着的重投不会和在飞的取消打架。被取消的排队消息载荷不可恢复，**正文由客户端保留**，在下一次 prompt 之前用 `queue` 按原顺序重投。
- **暂停不是取消**：ACP 没有 `paused` 停止原因，`session/prompt` 仍以 `cancelled` 结束，但同时带 `_meta.paused = true`；客户端应以此区分暂停与真正的取消。
- **暂停不切断已开始的答复**：已经在流式输出的模型消息会写完再停（只有连续 3 秒无产出的卡死流才会被切断），所以继续时不会重复生成一段。
- **`resume` 等到 Turn 收尾才回**：它占和 prompt 同一把锁，逐条推送 `session/update`，最后回 `{turnId, stopReason}`——fire-and-forget 的继续会让客户端分不清「还在跑」和「已经结束」。
- **暂停落在答复已生成完之后**（finalization 边界）：`resume` 只补收尾，不再问模型；这期间插的话不会进上下文（已知边界）。

无 durable 的构建（`lite` / `native`）：`steer` / `queue` / `queue/remove` / `pause` / `resume` 一律回错误（提示本构建没有 durable 驱动），`queue/list` 返回空列表，`session/cancel` 没有队列可清。

### 子代理进度（`subagent_update`，`clientCapabilities.subagents`）

客户端的 `clientCapabilities` 里带 `subagents: {}` 时，本 agent 会把 `agent_spawn` 起的子代理
运行过程实时上报（不稳定特性，需要 SDK 的 `unstable_subagents`）：

| 帧 | 外层 `sessionId` | 内容 |
| --- | --- | --- |
| `subagent_update` | 父会话 | `sessionId`（子会话）、`title`/`description`、`capabilities.cancel: {}`、`state.state = running`；`_meta.startedAt`/`_meta.model`/`_meta.role` |
| `agent_message_chunk` / `agent_thought_chunk` / `tool_call*` | **子会话** | 子代理的输出，`messageId` 形如 `sub-<child>`，与父 turn 的消息 id 不同 |
| `subagent_update` | 父会话 | `state.state = requires_action`（子代理要审批时，紧跟一条发往**子会话**的 `session/request_permission`） |
| `subagent_update` | 父会话 | `state.state = idle` + `stopReason`（`end_turn` / `cancelled` / `error`，失败时先发一条 `notice`）；`_meta.completedAt`；子会话随后仍可用 `agent_send` 继续 |

约束：

- **公告先到**：子会话的任何一条 chunk 之前必定先有一条 `subagent_update`（客户端按外层 id 认领子会话）。
- 非流式的一次性回复会在 `AgentResult` 时补成一条 `agent_message_chunk`（已流式的内容不会重复）。
- 客户端没声明 `subagents` 时**完全静默**（不发 `subagent_update`，也不发子会话 chunk），退回普通 tool_call 形态。
- `session/cancel` 发到子会话 id 会中断该子代理（不会改动父 turn）；`session/close` 会中断该会话下所有子代理。
- 子代理内的权限询问以**子会话 id** 发起（`session/request_permission.sessionId` = 子会话），客户端据此把审批卡挂到子代理卡片上；答复后立刻回 `running`，终态仍由 turn 结束上报。
- 子代理在启动前会继承父会话**当前**的权限模式，并加载 `permissions.json` 里已持久化的 always allow / always deny 规则：`accept_edits` 下子代理执行 shell 同样会弹审批，用户在子代理弹窗里选的 “always allow” 对后续子代理同样生效（同一个 store）。
- 子会话的 `plan`/`plan_update` 外层 `sessionId` 也是子会话 id，与父 turn 形态一致。
- 嵌套：子代理内部再 spawn 时，`subagent_update` 的外层 `sessionId` 是**直接父会话**（即中间那个子代理）。嵌套层数由 `AcpCodingAgent::with_subagent_nesting(n)` 配置（默认 `1`，经 `SessionBuild.subagent_nesting` 传给 builder）：`0` 时每个子代理都是叶子，`1` 时子代理可再 spawn 一层（孙代理为叶子）；`session/cancel` 用子会话 id 会在整棵树上找到并中断对应子代理。
- `session/load` 会把历史上**已终态**的子代理回放成 `started` + `idle` 两张卡：子会话 id 用记录里持久化的 `sub_session_id`（即实时的 `sub-<uuid>`，重开幂等、不会与实时卡片错位）；同一子代理的多次 `agent_send` 只回放一张卡（时间跨度取首轮创建到最后一次更新，状态取最近一轮）；嵌套子代理按记录的父会话 id 回放到对应外层；挂起/运行中的任务不回放。
- 子会话（`sub-*`）不出现在 `session/list` / `session/load` 里；`session/status` 只对进程内仍在运行的子代理返回 `open`，重启后与其他未知 id 一样报错。

认证方法不门控：`initialize` 总是返回一条 terminal 类型的认证方法（id `setup`，`args: ["--setup"]`），
客户端据此在自己的终端里拉起 `acp_harness_agent --setup`。注册表校验发的是空 `clientCapabilities`
并要求 `authMethods` 非空，所以这里不能按 `auth.terminal` 门控；不支持终端启动的客户端只是不展示它。

### 客户端 `systemPrompt`（仅 append）

`session/new|load|resume` 可带 `systemPrompt`，但**只支持 `append`**：文本接在内置提示词之后，
编码规则与工具说明仍在前；`override` 及其他 mode 一律 `-32602`（不降级）。该能力**只在用
`session_factory` 构建 agent 时声明**——`model_factory` 的签名拿不到这段文本，此时不声明，
且带 `systemPrompt` 的请求会被拒绝而不是被静默丢弃。

### `thought_level`

`auto`（不发任何思考参数，用 provider 默认）/ `disabled` / `low` / `medium` / `high` / `max`，
实际下发为 `reasoning_effort`。模型不支持推理时，该选项不展示。

## 用各语言客户端 SDK 接入

ACP 就是 stdio 上的 JSON-RPC：把 `acp_harness_agent` 当子进程拉起，接它的 stdin / stdout 即可。
不想自己造轮子的话，用官方 SDK：

| 语言 | 包 | 备注 |
| --- | --- | --- |
| Rust | [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) | 与本 crate 同一个 SDK；现成客户端实现见本仓库 [`src/acp_agent_cli.rs`](src/acp_agent_cli.rs) |
| TypeScript | `@agentclientprotocol/sdk`（npm） | 默认入口是 v1；另有 `/unstable` 与 `/experimental/v2` |
| Python | `agent-client-protocol`（PyPI，代码里 `import acp`） | 基于 asyncio |
| Java | `com.agentclientprotocol:acp-core`（Maven Central，需 Java 17+） | 自 0.18 起不再内置 JSON，需另加 `acp-json-jackson2` 或 `acp-json-jackson3` |
| Kotlin | `acp-kotlin`（JVM） | 其他 target 上游仍在推进 |

### 客户端要实现的回调

本 agent 只会用到客户端的下面三个回调，其余全部在 agent 进程内完成：

| 方向 | 方法 | 是否必需 |
| --- | --- | --- |
| agent → client | `session/update`（通知） | **必需**：所有流式输出都走这里 |
| agent → client | `session/request_permission` | **必需**：`accept_edits` / `plan` 模式下的确认；不实现会卡住本轮对话 |
| agent → client | `elicitation/create` | 可选：`ask_user` 工具用；直接返回 `decline` 即可降级 |
| agent → client | `fs/read_text_file` · `fs/write_text_file` · `terminal/*` | **不会调用**：文件读写与 shell 都在 agent 进程内完成，这些回调可以留空 |

`session/update` 可能出现的变体（按需渲染，不认识的忽略即可）：`agent_message_chunk`、`agent_thought_chunk`、
`user_message_chunk`、`tool_call`、`tool_call_update`、`plan`、`plan_update`、`plan_removed`、
`current_mode_update`、`config_option_update`、`session_info_update`、`usage_update`、`notice`、
`compaction_update`、`compaction_summary_chunk`。

### 最小握手序列

```jsonc
// → 客户端
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
// ← agent（agentInfo 会自报 agentscope-coding-agent）
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{}}}

// → 新建会话（cwd 就是 agent 的工作目录）
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/path/to/project","mcpServers":[]}}
// ← 返回 sessionId + modes + configOptions（下方均省略细节）
{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-..."}}

// → 跑一轮
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"session-...","prompt":[{"type":"text","text":"这个仓库的 CI 为什么红了？"}]}}
// ← 期间不停推送 session/update 通知
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session-...","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"我先看下 CI 配置"}}}}
// ← 本轮结束
{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}
```

### Python（asyncio）

```python
import asyncio, asyncio.subprocess as subprocess, os
from acp import PROTOCOL_VERSION, Client, connect_to_agent
from acp.schema import AgentMessageChunk, ClientCapabilities, Implementation, TextContentBlock

# `Client` 的回调需要全部实现；与本 agent 无关的（fs/*、terminal/*）按上游示例
# `raise RequestError.method_not_found(...)` 即可，这里只列真正用得上的两个。
class MyClient(Client):
    async def session_update(self, session_id, update, **kwargs):
        if isinstance(update, AgentMessageChunk):
            print(update.content.text, end="", flush=True)

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        # 生产实现：把 options 渲染给用户，按选择构造 RequestPermissionResponse
        raise NotImplementedError("render permission options")

async def main():
    proc = await asyncio.create_subprocess_exec(
        "acp_harness_agent",           # 或绝对路径 / ACP_AGENT_EXE
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )
    conn = connect_to_agent(MyClient(), proc.stdin, proc.stdout)
    await conn.initialize(
        protocol_version=PROTOCOL_VERSION,
        client_capabilities=ClientCapabilities(),
        client_info=Implementation(name="my-client", title="My Client", version="0.1.0"),
    )
    session = await conn.new_session(mcp_servers=[], cwd=os.getcwd())
    await conn.prompt(session_id=session.session_id,
                      prompt=[TextContentBlock(text="列一下 src/ 的结构")])

asyncio.run(main())
```

### TypeScript

```ts
import * as acp from "@agentclientprotocol/sdk";

await acp
  .client({ name: "my-client" })
  .connectWith(agentStdio, async (ctx) => {          // agentStdio：子进程的 stdin/stdout 双向流
    await ctx.request(acp.methods.agent.initialize, {
      protocolVersion: 1,
      clientCapabilities: {},
    });
    const { sessionId } = await ctx.request(acp.methods.agent.session.new, {
      cwd: process.cwd(),
      mcpServers: [],
    });
    await ctx.request(acp.methods.agent.session.prompt, {
      sessionId,
      prompt: [{ type: "text", text: "列一下 src/ 的结构" }],
    });
  });
```

> 子进程与流的搭建各客户端不同（`child_process` + stdio 双工封装），这里只保留协议调用骨架；
> 可运行版本见上游 [`typescript-sdk/examples`](https://github.com/agentclientprotocol/typescript-sdk/tree/main/src/examples)。

### Java

```xml
<dependency>
  <groupId>com.agentclientprotocol</groupId>
  <artifactId>acp-core</artifactId>
  <version>0.80.0</version>
</dependency>
<dependency>
  <groupId>com.agentclientprotocol</groupId>
  <artifactId>acp-json-jackson2</artifactId>
  <version>0.80.0</version>
</dependency>
```

`acp-core` 提供 Client 与 Agent 两侧，支持 stdio / WebSocket / Streamable HTTP 传输；
连接本 agent 时用 stdio 传输指向 `acp_harness_agent`。可运行示例见上游
[`java-sdk/examples`](https://github.com/agentclientprotocol/java-sdk/tree/main/examples)。

### Kotlin

用 `acp-kotlin`（JVM）。stdio 传输 + 外部进程的完整客户端示例见上游
[`kotlin-sdk` 的 client sample](https://github.com/agentclientprotocol/kotlin-sdk/tree/master/samples/kotlin-acp-client-sample/src/main/kotlin/com/agentclientprotocol/samples)。

### Rust

本仓库的 [`src/acp_agent_cli.rs`](src/acp_agent_cli.rs) 就是一个可用的 Rust 客户端：
用 `AcpAgent::from_args([...])` 拉起子进程，`Client` 侧处理 `session/update`、权限与 elicitation，
再通过 `AcpAgent` 发 `session/prompt`。需要更完整的写法时看上游
[`rust-sdk` 的 client 示例](https://github.com/agentclientprotocol/rust-sdk)。

### 版本对齐

- `protocolVersion` 是**整数**，当前稳定值 `1`（v2 仍是需显式启用的草稿）
- 本 crate 基于 `agent-client-protocol` **5.0.0**（schema **1.15.1**）；SDK 的大版本升级已完成，
  `initialize` / `session/*` 的线格式与 v1 客户端兼容
- 5.0 的 v1 方法面相对 2.2 **新增** `auth/status`、`session/set_title`、`session/status`、
  `session/archive|unarchive`、`proxy/*`；**删除** `document/*`、`nes/*`、`mcp/connect|disconnect`。
  本项目只实现 agent 侧方法，`proxy/*` 不实现；MCP over ACP 改为 request-scoped（`mcp/message`
  带 `serverId` + `requestId`，不再有建连/断连）
- 本 agent 在 `initialize` 中**原样回显**客户端声明的版本，不主动降级也不拒绝——请传 `1`
- 不稳定特性（plan 操作、`end_turn` usage、MCP over ACP、`providers/*`、`_acp_extends/providers/*`、
  `subagent_update`）全部按**客户端声明的能力**门控：客户端没声明就不会收到，也不会报错。
  session notices 与压缩进度在 schema 1.11 起已转正，不再需要单独的 feature 门控。
- 执行中追加输入（`_acp_extends/session/steer|queue|pause|resume`）是**本 agent 的私有扩展**（`_` 前缀）：
  标准客户端不调用它们就没有任何影响，普通 `session/prompt` 流程照旧。

## 模型供应商管理（`providers/*`）

`acp_harness_agent` 把 `~/.agentscope/models.json` 当作**唯一事实来源**：设置页、`--setup` 向导与
`providers/*` 都读写同一个文件。改完立即生效：新会话拿到新列表，活着的会话收到
`config_option_update`（会话为空闲也能收到：`session/new` / `load` / `resume` 时就绑定了连接）。

### 能力声明

`initialize` 的 `agentCapabilities.providers` 出现即表示支持（空对象 `{}` 就够）。
`acp_harness_agent` 默认开启（`AcpCodingAgent::builder().manage_providers(true)`）；
以库方式自建 agent 时必须显式打开，否则这些方法回 `-32601`。

| 方法 | 语义 |
| --- | --- |
| `providers/list` | 每个 provider 一条；密钥只给掩码，非密钥信息进 `_meta` |
| `providers/set` | 新增/改写一个 provider；密钥从 `headers` 的 `Authorization: Bearer …` 或 `x-api-key` 取 |
| `providers/disable` | 置 `disabled: true`，保留密钥与模型表，可再启用 |

`_meta` 字段（全部可选，客户端画 UI 用）：

| 字段 | 含义 |
| --- | --- |
| `label` | 展示名（缺省等于 provider 键） |
| `models` | 按端点协议分组的模型表 `{completions, messages, responses}` |
| `apiKeyMasked` · `hasApiKey` | 掩码后的密钥 / 是否已配 |
| `disabled` | 是否停用（与 `current` 为空同义） |
| `readOnly` · `envVar` | 由环境变量提供：改环境变量才生效，删改会报错 |
| `isDefault` · `defaultModel` | 是否默认 provider / 新会话用哪个模型 |

`set` 的局部更新约定（标准面没有的字段都走 `_meta`）：

- `baseUrl` 传空串 = **不改地址**，只做启用/停用这类局部更新；**新建**条目时必须给地址
  （否则会落下一份连不通、也没法 `probe` 的配置）
- 不带 `headers` = 不改密钥；`_meta.models` 缺省 = 不改模型表
- 模型表的协议归属决定端点：`completions` → `POST {base_url}/chat/completions`，
  `messages` → `POST {base_url}/messages`，`responses` → `POST {base_url}/responses`

### 扩展方法（`_acp_extends/providers/*`）

标准面没有删除、默认项与“探测端点”，这几件事走 ACP 扩展（方法名以 `_` 开头，不支持的客户端不会误调）：

| 方法 | 入参 | 返回 |
| --- | --- | --- |
| `_acp_extends/providers/remove` | `{providerId}` | `{}`；环境变量提供的 provider 会报错 |
| `_acp_extends/providers/default` | `{providerId, model?}` | `{}`；`model` 缺省取该 provider 第一个模型 |
| `_acp_extends/providers/catalog` | `{providerId?}` | `{providers: [{id, name, baseUrl, protocol, env, modelCount, models?}]}`（models.dev，最后一项仅给了 `providerId` 才有） |
| `_acp_extends/providers/probe` | `{providerId?, baseUrl?, apiKey?, protocol?}` | `{ok, models?, protocol?, error?}`，实际打 `GET {baseUrl}/models` |

四个方法共用一个入参形状（`camelCase`，用不到的字段省略）；错误一律 `-32602`，原囚放 `data`。

### `models.json` 的字段

provider 级可选字段：

| 字段 | 含义 |
| --- | --- |
| `label` | 展示名 |
| `disabled` | 停用：不进模型列表，也不参与默认模型解析 |
| `env_var` | 该 provider 由环境变量提供（只读；启动时补齐到内存，**不写盘**） |

行为约定：

- 删除默认 provider 时自动回退到剩下的第一个；一个都没有就用内置默认
- 活会话选中的模型被删/改名时，会话回落到新列表的第一个（否则客户端会收到一个不在
  选项里的 `currentValue`，下拉显示为空）；已建好的模型实例在下个 prompt 前照旧可用
- 环境变量提供的 provider 改不了、删不掉（`readOnly`）：从磁盘单独读会把它们当成新条目写进
  `models.json`，从此不再从环境变量接管，所以每次改都从「文件 + 环境变量」的同一份视图出发
- 多个 provider 下重复的裸模型 id 在模型下拉里会加上 `provider/` 前缀（避免歧义），
  单 provider 或 id 不重复时保持原样
- 文件损坏/解析失败时**不覆写**：告警后退到内置默认，改回文件即可恢复

## 工具、skills 与子智能体

主 agent 默认工具箱（受模式与权限约束）：

- 文件：`read_file`、`write_file`、`edit_file`、`grep_files`、`glob_files`、`list_files`
- 执行：`execute`（shell，按模式决定是否询问）
- 网络：`web_fetch`、`web_search`（`web_search` 需 `DEEPSEEK_API_KEY`）
- 记忆与检索：`memory_get`、`memory_save`、`memory_search`、`session_search`
- 计划：`plan_enter`、`plan_write`、`plan_exit`
- 子智能体：`agent_spawn`、`agent_send`、`agent_list`、`task_output`、`task_cancel`、`task_list`
- 技能：`load_skill_through_path`

**Skills 目录**

- 用户级：`~/.agents/skills`（Windows：`C:\Users\<user>\.agents\skills`）
- 项目级：`<workspace>/.agents/skills`

加载后会渲染成 `<available_skills>` 提示块，agent 用 `load_skill_through_path` 按需读全文。

**子智能体**

内置 `general-purpose`（同模型同工具集，不能再 spawn）。额外的声明式智能体放在
`<workspace>/subagents/*.md`，用 front-matter 指定 `tools` / `steps` / `workspace.mode` / `model` 等。

## 环境变量

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `CODING_PROVIDER` | `models.json` 的 `default_provider` | 临时覆盖 provider |
| `CODING_MODEL_ID` | 同上的 `default_model` | 临时覆盖模型 |
| `OPENAI_API_KEY` | provider 里的 `api_key` | 覆盖密钥（启动时会打 warn，避免与配置冲突后难查 401） |
| `OPENAI_BASE_URL` | provider 里的 `base_url` | 覆盖网关 |
| `AGENTSCOPE_MODELS_JSON` | `~/.agentscope/models.json` | 覆盖模型配置文件路径（测试与多配置并存） |
| `LANG` | `zh` | 系统提示语言 |
| `AGENTSCOPE_LOG_DIR` | `<cwd>/.agentscope/log` | 日志目录（写 `acp-agent.log`） |
| `RUST_LOG` | `info,acp_harness_agent=debug,agentscope_harness=debug` | tracing 过滤器 |
| `AGENTSCOPE_TURN_DRIVER` | `durable` | `native` 切到原生驱动；其它值一律保持 durable（仅编译了 `durable` 时有意义） |
| `AGENTSCOPE_DURABLE_STORE` | 编译 `durable-sqlite` 时 `sqlite` | 设为 `file` 时改用 FileStore（未编译 `durable-sqlite` 时只有 FileStore） |
| `AGENTSCOPE_ACP_STALL_CANCEL_SECS` | `2700`（45 分钟） | durable 驱动下，`running` turn 无任何 durable 进展时的硬上限；`0`/`off` 关闭。高于所有单工具上限（shell 30 分钟），只用于真卡死的 run |
| `AGENTSCOPE_ACP_COMPACTION_WINDOW` | 跟随所选模型（未知时 `256000`） | 显式覆盖压缩窗口（tokens）。不设时按模型自己的上下文窗口推导触发点（1M 窗口约 `888192`）；设置后可用于封顶或提高触发点 |
| `AGENTSCOPE_SESSION_RETENTION` | 二进制 `12h`（库默认 30 天） | 会话 journal 保留期；`off` 关闭清理；支持 `m`/`h`/`d` |
| `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | tokio worker 数 |
| `AGENTSCOPE_ACP_MEM_TRIM_IDLE` | `3` 秒 | 空闲多久后归还工作集；`0` 关闭 |
| `AGENTSCOPE_ACP_MEM_REPORT` | 关 | 每 turn 额外打印 `AgentState` 序列化体积 |
| `AGENTSCOPE_ACP_MEM_COLLECT` | 开 | 每 turn 后向分配器归还内存（仅 Unix 有效，Windows 为 no-op） |
| `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS` | 关 | 把模型请求体写进 journal（排查用，单次可达数 MB） |
| `ACP_AGENT_EXE` | 与 CLI 同目录的 `acp_harness_agent.exe` | 仅 `acp_agent_cli`：指定要拉起的 agent |
| `ACP_SESSION_ID` / `ACP_SESSION_REPLAY` | - | 仅 `acp_agent_cli`：打开指定会话 / 重放历史 |

provider、模型目录、扩展等其余变量见仓库根 [`docs/ENVIRONMENT.md`](../../docs/ENVIRONMENT.md)。

## 磁盘布局

以客户端传来的 cwd（即 workspace）为根：

```text
~/.agentscope/models.json            # 模型与 provider 配置
~/.agents/skills/                    # 用户级 skills
<workspace>/.agents/skills/          # 项目级 skills
<workspace>/.agentscope/permissions.json   # 「总是允许/拒绝」记忆
<workspace>/.agentscope/workspace/   # 会话 journal 与工作区索引
<workspace>/.agentscope/log/         # 日志
<workspace>/subagents/*.md           # 声明式子智能体
```

## 故障排查

| 现象 | 排查 |
| --- | --- |
| `model '<id>' not found in any provider of ~/.agentscope/models.json` | 模型 id 必须写在某个 provider 的 `completions`/`responses`/`messages` 列表中 |
| 401 / 403 | 先看启动日志：`OPENAI_API_KEY` 会**覆盖**配置文件，残留的 shell 变量常是真凶 |
| 端点 404 | `base_url` 写到了具体端点，或模型放错了协议分组 |
| `acp_agent_cli` 报 `agent executable not found` | 用 `ACP_AGENT_EXE` 指定 `acp_harness_agent` 路径 |
| 配置改了不生效 | 文件解析失败时会静默回退内置默认（并告警）；检查注释外的语法与 UTF-8 BOM |
| 首次启动变慢 | 正在拉 models.dev 模型目录；`AGENTSCOPE_MODEL_CATALOG=0` 可关闭 |

## 已知限制

- 0.x 预发布：协议表面会随 `agent-client-protocol` 的不稳定特性调整
- 预编译包（`embedded` profile + `lite`）**只带原生驱动**：没有 durable 的跨进程续跑与崩溃原子性，
  也没有插话 / 排队 / 暂停；需要这些能力时用 `cargo xtask acp build durable|sqlite` 自建
- `acp_agent_cli` 默认只按 `acp_harness_agent.exe` 找同级二进制，非 Windows 需设 `ACP_AGENT_EXE`
- 传输有 stdio 与监听（WebSocket / HTTP+SSE，`--ws` 或 `--http`，同一端点 `/acp`）两种；后者没有认证
- 音频输入未实现（`prompt_capabilities.audio = false`）
- 全自动模式（`yolo`）不做任何确认，请在可信工作区使用
- durable 的 FileStore 不做 `fsync`（进程崩溃安全、掉电不安全）；SQLite 后端是单机预览，
  WAL 不能放网络文件系统，多宿主请用 Postgres 记录存储（`agentscope-durable-jdbc`）
- 暂停落在最终答复已生成完之后（finalization 边界）时，`resume` 只补收尾、不再问模型；
  这期间插的话不会进上下文
- 后台化的长工具是**进程内** spawn：ACP 进程退出，后台命令与其结果一起消失

## 作为库使用

不想发二进制时，可以直接把 agent 嵌进已有进程：

```rust
use agentscope_extensions_acp::AcpCodingAgent;

let agent = AcpCodingAgent::builder()
    .model_factory(build_agent)   // fn(cwd, session_id) -> 装配好的 HarnessAgent
    .manage_providers(true)       // 开启 providers/* 管理（二进制默认开启）
    .build();
agent.run_stdio().await?;         // 在当前 stdio 上服务 ACP 客户端
```

需要接收客户端 `systemPrompt`（append 模式）时必须改用 `session_factory`——`model_factory`
的签名拿不到这段文本，此时能力不声明且请求会被拒绝。完整的装配示例（模型解析、skills、
compaction、子智能体）见 [`src/acp_harness_agent.rs`](src/acp_harness_agent.rs)。

## 开发

```bash
cargo test -p agentscope-extensions-acp                    # 单元测试（全离线）
cargo build -p agentscope-extensions-acp --bins            # 两个可执行文件（默认 durable + FileStore）

# 三个发布变体，产物在 target/acp/<profile>/<variant>/
cargo xtask acp build native  --profile release            # 原生驱动，无 durable
cargo xtask acp build durable --profile release            # durable + FileStore
cargo xtask acp build sqlite  --profile release            # durable + SQLite
cargo xtask acp build all     --profile embedded           # 体积优先，需 nightly + rust-src
cargo xtask acp upx sqlite    --profile embedded           # UPX 打包 + 完整性校验
```

变体只差 feature 组合：`native` = `rusqlite`/`lite`，`durable` = `+durable`，
`sqlite` = `+durable-sqlite`（隐含 `durable`，两个驱动都编进去，由 `AGENTSCOPE_TURN_DRIVER` 选）。
两个二进制必须同目录（`acp_agent_cli` 从自身目录 spawn `acp_harness_agent`）。

诊断构建：`cargo build -p agentscope-extensions-acp --features alloc-stats` 让 `[mem]` 行报告真实堆占用。

模块划分：`coding_agent`（ACP 请求分发与能力声明）、`session`（协议状态 + durable 驱动）、
`turn_driver`（驱动接缝）、`native_driver` + `session_history`（无 durable 的驱动与轻量历史）、
`ext`（`_acp_extends/*` 私有扩展：应答 / 插话 / 队列 / 暂停）、
`config`（modes / config options）、`elicitation`（结构化提问）、`mcp_over_acp`、`deferred_effects`、
`event_queue`、`provider` + `providers_admin`（`providers/*` 支持）、`mem`（线程与内存诊断）、
`retention`、`recording`。

## 许可证

Apache-2.0
