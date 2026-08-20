# harness_agent — CLI Coding Agent

一个基于 `HarnessAgent` + `ReActAgent` 的命令行 coding agent 示例（对齐 Java 端
`AgiHarnessAgentConsoleDemo`）：在指定 workspace 内组合文件系统工具
（`read_file`/`write_file`/`edit_file`/`grep_files`/`glob_files`/`list_files`）
与 shell 执行工具（`execute`），驱动 ReAct 推理循环完成编码任务。

支持 **一次性任务** 与 **交互式 REPL** 两种模式，带 ANSI 色彩渲染
（thinking / agent 文本 / 工具调用 / 工具结果）与 token 用量汇总。

## 运行

```bash
# 一次性任务
cargo run -p agentscope-harness --example harness_agent -- "列出当前目录文件"

# 交互式 REPL（未提供 TASK 时默认进入）
cargo run -p agentscope-harness --example harness_agent -- --interactive

# 指定模型 / 工作目录（API 地址与密钥通过环境变量 OPENAI_BASE_URL / OPENAI_API_KEY 提供）
cargo run -p agentscope-harness --example harness_agent -- \
  --model gpt-4o \
  --workspace ./my-project \
  "给 README.md 添加安装说明"

# 沙箱模式（文件系统操作被限制在 workspace 内）
cargo run -p agentscope-harness --example harness_agent -- \
  --sandbox --workspace ./my-project "修复 src/main.rs 的编译错误"
```

## 交互式 REPL 命令

| 命令 | 说明 |
| --- | --- |
| `/exit` | 退出 Demo |
| `/help` | 显示帮助 |
| `/model` | 查看当前模型 |
| `/model <id>` | 切换到指定模型（保留工具） |
| `/workspace` | 查看当前工作目录 |
| 其他 | 作为 prompt 发送给 Agent |

## 环境变量

| 变量 | 说明 |
| --- | --- |
| `AGENTSCOPE_MODEL` | 默认模型 id（默认 `gpt-4o`） |
| `OPENAI_BASE_URL` | OpenAI 兼容网关地址（DeepSeek/Kimi/Ollama 等） |
| `OPENAI_API_KEY` | API 密钥 |

## 参数

```
Usage: harness_agent [OPTIONS] [TASK]

Options:
  --interactive       进入交互式 REPL（未提供 TASK 时默认）
  --model <NAME>      模型 id（env: AGENTSCOPE_MODEL，默认 gpt-4o）
  --workspace <DIR>   工作目录（默认当前目录）
  --sandbox           将文件系统操作限制在 workspace 内
  --max-iters <N>     最大推理迭代次数（默认 10）
  --no-shell          不注册 shell 执行工具
  --quiet             关闭进度输出
  --system <PROMPT>   自定义系统提示词
  --help              显示帮助
```

## 工作方式

1. 用 `OpenAiChatModel` 构建模型（兼容任意 OpenAI 协议网关）。
2. 用 `LocalFilesystem` 包装 workspace（可选用 `Sandboxed` 模式隔离）。
3. 用 `HarnessAgent::builder()` 组装文件系统工具 + cwd。
4. 在 harness 的 toolkit 上追加 `ShellExecuteTool`（`execute`）。
5. 用 `ReActAgent` 驱动 推理→行动 循环，订阅 `AgentEvent` 输出进度。
6. 渲染最终回复（thinking / 文本 / 工具调用 / 结果）与 token 用量汇总。
