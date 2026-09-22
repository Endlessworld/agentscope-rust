# harness_agent — ACP CodingAgent over stdio

一个基于 `AcpCodingAgent` 的 **ACP（Agent Client Protocol）agent（server 角色）**，
通过 stdio 与 ACP 客户端（IDE/CLI）通信。每个会话用 `HarnessAgent` 完整装配
（workspace、文件系统工具 `read_file`/`write_file`/`edit_file`/`grep_files`/`glob_files`/`list_files`、
shell 执行工具 `execute`、compaction、plan mode），并加载用户级与项目级 skills。

对齐 Java `HarnessCodingAgent` / Kotlin `AgiHarnessAgent`。
源码位于 `src/acp/acp_harness_agent.rs`，以 `[[bin]]` 目标提供。

## 运行

```bash
# 启动 ACP 服务器（等待 stdio 上的 ACP JSON-RPC 消息）
cargo run -p agentscope-harness --bin acp_harness_agent

# 基础冒烟测试：initialize
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' \
  | cargo run -p agentscope-harness --bin acp_harness_agent
```

真正的交互式 ACP 客户端（IDE/CLI，或配套的 `acp_agent_cli`）才是预期的对端。

## 环境变量

| 变量 | 说明 |
| --- | --- |
| `CODING_MODEL_ID` | 默认模型 id（默认 `gpt-4o`） |
| `OPENAI_BASE_URL` | OpenAI 兼容网关地址（DeepSeek/Kimi/Ollama 等） |
| `OPENAI_API_KEY` | API 密钥 |

## Skills

- 用户级：`$HOME/.agents/skills`（Windows: `C:\Users\<user>\.agents\skills`）
- 项目级：`<workspace>/.agents/skills`

加载后渲染 `<available_skills>` 提示块，并注册 `load_skill_through_path`
工具，供 agent 按需读取 skill 文档/脚本。

## 工作方式

1. `build_agent(cwd)` 按 `models.json` 解析出的协议构建模型
   （`chat/completions`、`responses` 或 `messages`）。
2. 加载 skills 并渲染 `<available_skills>` 提示块，拼接到系统提示词。
3. 用 `HarnessAgent::builder()` 装配 workspace + 文件系统工具 + shell + compaction。
4. 注册 `load_skill_through_path` 工具。
5. 用 `AcpCodingAgent::builder().model_factory(build_agent)` 构建 ACP agent。
6. `run_stdio()` 启动 ACP 服务器，处理 initialize / new_session / prompt /
   set_config_option / set_mode / close / list / load / resume / delete 请求。
