# agentscope-extensions-acp

把 AgentScope Rust 的 `HarnessAgent` 暴露成一个 **ACP（Agent Client Protocol）编程智能体**：
任何实现了 ACP 的编辑器 / 客户端都能把它当作编码助手用——读改文件、执行 shell、加载 skills、
派发子智能体、自动压缩上下文。附带的 REPL 客户端可以在没有 IDE 的情况下直接验证。

> 状态：`0.1.0-alpha.1`（预发布）。协议实现基于 [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) 2.2。

## 两个可执行文件

| 二进制 | 角色 | 说明 |
| --- | --- | --- |
| `acp_harness_agent` | ACP **agent**（server 侧） | stdio 传输；无参数、不监听端口；由客户端拉起，在 `initialize` 中自报 `agentscope-coding-agent` |
| `acp_agent_cli` | ACP **client**（REPL） | 自动拉起上方的 agent 子进程，用于本地调试 |

两者都只会说 stdio 上的 JSON-RPC，因此可以被任意客户端用「一个命令」接入。

## 提供什么

- **完整编码循环**：ReAct 推理 + 工具调用，事件流式回传给客户端（消息分片、工具调用状态与位置、计划更新）
- **三种审批模式**：`plan`（只读）/ `accept_edits`（默认，改文件免问、shell 需确认）/ `yolo`（全自动）
- **权限记忆**：用户选择「总是允许 / 总是拒绝」后写入工作区，跨会话生效
- **运行时切换**：`model` 与 `thought_level` 作为 ACP config option 暴露，客户端可随时切换
- **会话持久化**：`session/new|load|resume|list|delete|close`，历史以 journal 为准，重启后恢复
- **上下文压缩**：超过阈值自动压缩，并在客户端支持时推送进度通知
- **MCP**：支持客户端以 `stdio` / `http` / `sse` 声明的 MCP server，也支持 MCP over ACP
- **Skills**：用户级与项目级 skill 目录，按需通过 `load_skill_through_path` 读取
- **子智能体**：内置 `general-purpose`，另可从 `<workspace>/subagents/*.md` 声明

## 安装

```bash
# 预发布版本必须显式指定
cargo install agentscope-extensions-acp --version 0.1.0-alpha.1

# 或从源码（仓库根目录）
cargo install --path crates/agentscope-extensions-acp
```

要求 Rust 1.85+。默认 feature 带 `rusqlite`（bundled SQLite，用于工作区索引）；
不想编 C 引擎可用 `--no-default-features --features lite`（索引降级为目录扫描）。

## 快速开始

### 1. 配置模型

首次运行会自动生成 `~/.agentscope/models.json`（Windows：`%USERPROFILE%\.agentscope\models.json`），
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
| `session/prompt` | 跑一轮对话；支持图像输入（按所选模型能力）、客户端 `embedded_context` |
| `session/cancel` | 取消当前 turn（`StopReason::Cancelled`） |
| `session/set_mode` | 切换 `plan` / `accept_edits` / `yolo` |
| `session/set_config_option` | 切换 `model` / `thought_level` |
| `session/load` · `session/resume` | 从 journal 恢复会话 |
| `session/list` · `session/delete` · `session/close` | 会话管理 |
| `providers/list` · `providers/set` · `providers/disable` | 仅在配了多个 provider 时声明（ACP 不稳定特性） |
| `_agentscope/session/respond` | 私有扩展：回答被挂起的结构化提问 |

### 按客户端能力门控

只有在客户端声明了相应能力时才会发送：`session/notice`（会话提示）、压缩进度、plan 操作通知、
`end_turn` 的 token usage、MCP over ACP。没声明就不会收到，也不会报错。

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
- 本 crate 基于 `agent-client-protocol` 2.2；上游已到 3.6，两边都在 v1 协议线上，
  `initialize` / `session/*` 的线格式一致，握手兼容
- 本 agent 在 `initialize` 中**原样回显**客户端声明的版本，不主动降级也不拒绝——请传 `1`
- 不稳定特性（plan 操作、session notices、压缩进度、`end_turn` usage、MCP over ACP、`providers/*`）
  全部按**客户端声明的能力**门控：客户端没声明就不会收到，也不会报错

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
| `LANG` | `zh` | 系统提示语言 |
| `AGENTSCOPE_LOG_DIR` | `<cwd>/.agentscope/log` | 日志目录（写 `acp-agent.log`） |
| `RUST_LOG` | `info,acp_harness_agent=debug,agentscope_harness=debug` | tracing 过滤器 |
| `AGENTSCOPE_ACP_COMPACTION_WINDOW` | `256000` | 压缩窗口（tokens），**与所选模型窗口无关**，实际触发点 `218419` |
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

- 预发布：协议表面会随 `agent-client-protocol` 的不稳定特性调整
- `acp_agent_cli` 默认只按 `acp_harness_agent.exe` 找同级二进制，非 Windows 需设 `ACP_AGENT_EXE`
- 仅支持 stdio 传输（不提供 TCP / HTTP 上的 ACP）
- 音频输入未实现（`prompt_capabilities.audio = false`）
- 全自动模式（`yolo`）不做任何确认，请在可信工作区使用

## 作为库使用

不想发二进制时，可以直接把 agent 嵌进已有进程：

```rust
use agentscope_extensions_acp::AcpCodingAgent;

let agent = AcpCodingAgent::builder()
    .model_factory(build_agent)   // fn(cwd, session_id) -> 装配好的 HarnessAgent
    .build();
agent.run_stdio().await?;         // 在当前 stdio 上服务 ACP 客户端
```

完整的装配示例（模型解析、skills、compaction、子智能体）见 [`src/acp_harness_agent.rs`](src/acp_harness_agent.rs)。

## 开发

```bash
cargo test -p agentscope-extensions-acp                    # 单元测试（全离线）
cargo build -p agentscope-extensions-acp --bins            # 两个可执行文件
cargo build -p agentscope-extensions-acp --no-default-features --features lite --bins
cargo build -p agentscope-extensions-acp --features alloc-stats  # 诊断：[mem] 行报告真实堆占用
```

模块划分：`coding_agent`（ACP 请求分发与能力声明）、`session`（会话状态机与事件流）、
`config`（modes / config options）、`elicitation`（结构化提问）、`mcp_over_acp`、`mem`（线程与内存诊断）、
`provider`（`providers/*` 支持）。

## 许可证

Apache-2.0
