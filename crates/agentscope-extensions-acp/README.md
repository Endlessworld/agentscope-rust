# harness_agent — ACP CodingAgent over stdio

一个基于 `AcpCodingAgent` 的 **ACP（Agent Client Protocol）agent（server 角色）**，
通过 stdio 与 ACP 客户端（IDE/CLI）通信。每个会话用 `HarnessAgent` 完整装配
（workspace、文件系统工具 `read_file`/`write_file`/`edit_file`/`grep_files`/`glob_files`/`list_files`、
shell 执行工具 `execute`、compaction、plan mode），并加载用户级与项目级 skills。

对齐 Java `HarnessCodingAgent` / Kotlin `AgiHarnessAgent`。
crate 位于 `crates/agentscope-extensions-acp/`，两个入口以 `[[bin]]` 目标提供：
服务端 `src/acp_harness_agent.rs`、测试客户端 `src/acp_agent_cli.rs`。

## 运行

```bash
# 启动 ACP 服务器（等待 stdio 上的 ACP JSON-RPC 消息）
cargo run -p agentscope-extensions-acp --bin acp_harness_agent

# 基础冒烟测试：initialize
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' \
  | cargo run -p agentscope-extensions-acp --bin acp_harness_agent
```

真正的交互式 ACP 客户端（IDE/CLI，或配套的 `acp_agent_cli`）才是预期的对端。

## 环境变量

| 变量 | 说明 |
| --- | --- |
| `CODING_MODEL_ID` | 默认模型 id（默认 `gpt-4o`） |
| `CODING_PROVIDER` | 覆盖 `models.json` 的 `default_provider` |
| `OPENAI_BASE_URL` | OpenAI 兼容网关地址（DeepSeek/Kimi/Ollama 等） |
| `OPENAI_API_KEY` | API 密钥 |
| `AGENTSCOPE_ACP_COMPACTION_WINDOW` | 对话压缩窗口属性（tokens，默认 `256000`，与模型窗口无关，触发点 `218419`） |
| `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS` | `=1` 时把每次模型请求体写进会话 journal（排查用，默认关） |
| `AGENTSCOPE_ACP_WORKER_THREADS` | tokio worker 线程数（默认 `min(ncpu, 8)`） |
| `AGENTSCOPE_ACP_MEM_REPORT` / `AGENTSCOPE_ACP_MEM_COLLECT` | 每 turn 打印序列化 `AgentState` 体积 / 归还内存（默认关 / 开） |
| `AGENTSCOPE_SESSION_RETENTION` | 会话 journal 保留窗口（本二进制默认 `12h`；`off` 关闭清理） |
| `AGENTSCOPE_LOG_DIR` | 日志目录（默认 `<cwd>/.agentscope/log`） |

完整清单（含 provider、模型目录、扩展与测试变量及默认值）见仓库根 [`README.md`](../../README.md#环境变量)。

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
