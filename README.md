# AgentScope Rust

AgentScope（Java）的 Rust 实现：把 `io.agentscope.core` 与 `io.agentscope.harness` 迁移为
原生异步 Rust 库（Tokio + serde + async-openai），并在其上实现可用的 **ACP 编程智能体**。

- **目标**：与 Java 参考实现功能对齐（API 语义、事件/状态 JSON、工具治理行为）
- **技术栈**：Rust 2021 · Tokio · serde · async-openai · rust-mcp-sdk · rusqlite
- **进度**：见 [`docs/MIGRATION_GAP_TRACKER.md`](docs/MIGRATION_GAP_TRACKER.md)

## 核心能力

- **ReAct 智能体运行时**：reasoning→acting 循环、流式事件（31 种 `AgentEvent`）、结构化输出、串行门与优雅关闭。
- **模型层**：`Model` trait / `ModelRegistry` 等抽象在 `agentscope-core`；全部具体 provider（openai / responses / anthropic / dashscope / gemini / ollama）实现在 `agentscope-extensions-model`。
- **工具系统**：`Tool` / `Toolkit` / `ToolGroup` / `ToolExecutor`（超时、重试、并行、JSON Schema 校验、挂起语义）+ MCP 工具桥。
- **Harness 运行时**：`HarnessAgent` 一键装配文件系统工具、shell、压缩（compaction）、plan mode 与 skills。
- **ACP Coding Agent**：stdio 上实现 ACP 协议（initialize / session.* / prompt / set_mode / cancel），含权限确认（HITL）与运行时模型切换。
- **沙箱与文件系统**：Local/Docker 沙箱、快照与 Lease、Local/Overlay/Composite/Sandbox/Remote 文件系统。
- **多智能体**：子智能体声明与动态 spawn、后台任务仓库（本地/远程）、消息总线、Gateway 与通道路由、记忆/技能巩固。

## 仓库结构

```text
agentscope-rust/
├── Cargo.toml                  # workspace：core + harness + extensions-*
├── crates/
│   ├── agentscope-core/              # 公共抽象 + 核心实现（仅接口/运行时，不依赖其它模块）
│   ├── agentscope-harness/           # 增强运行时（workspace / ACP / 沙箱 / skill / subagent…），最终使用者
│   └── agentscope-extensions-*/      # 供应商 / 平台适配（model / rag / sandbox / channel / mem / store…）
├── docs/                       # 迁移方案与进度跟踪
└── .agents/context/            # 跨会话项目上下文（画像 / 状态 / ADR / 经验）
```

## 模块依赖规则

- **`agentscope-core`**：公共抽象与核心实现，**如无必要不得引入其它模块 crate 依赖**（仅第三方依赖）。
- **`agentscope-extensions-*`**：供应商 / 平台适配实现。一般只允许依赖 `agentscope-core`；如无必要尽量不引用其它模块。确需跨 extension 引用时，先在汇总 tracker 记「跨模块需求」并获裁定。
- **`agentscope-harness`**：最终使用者 / 装配层，可以引入其它任意模块（core 与 extensions-*），负责把 provider 适配、运行时装配成完整应用。
- **禁止循环引用**：模块依赖图必须保持 DAG。出现循环时由汇总方裁定归属（通常把公共抽象下沉到 core）。

### agentscope-core 模块

| 模块 | 内容 |
| --- | --- |
| `agent` | `ReActAgent` + builder、生命周期、流式选项、`RuntimeContext` |
| `model` | `Model` / `ModelRegistry` SPI、DTO（`GenerateOptions` / `ChatResponse` / `ToolSchema`…）、上下文窗口 |
| `message` | `Msg` / `MsgRole`、9 类 `ContentBlock`、`Source`、`ChatUsage` |
| `tool` | Tool / Toolkit / ToolGroup / Executor / Validator、MCP 桥、`TodoWriteTool` |
| `middleware` | 5 钩子洋葱链 + `TaskReminderMiddleware` |
| `event` | `AgentEvent`（31 变体）+ `AgentEventEmitter` |
| `state` | `AgentState`、`InMemory` / `JsonFile` Store（与 Java 状态文件互通） |
| 其他 | `error`、`memory`、`permission`、`interruption`、`shutdown`、`formatter`、`credential`、`tracing` |

### agentscope-harness 模块

| 模块 | 内容 |
| --- | --- |
| `acp` | ACP 协议 CodingAgent（stdio）、会话生命周期、权限确认、配置/模式、动态模型切换 |
| `agent` | `HarnessAgent` 与 builder（文件系统 + 工具 + 中间件装配） |
| `agent::sandbox` | Local/Docker 沙箱、快照、Lease/Guard、WorkspaceSpec、容器复用与重启恢复 |
| `agent::skill` | Open Agent Skills：加载 / 运行时 / 安全扫描 / curator / prompt |
| `agent::subagent` | 子智能体声明、动态 spawn、后台任务（本地/远程）与完成回调 |
| `agent::gateway` · `channel` · `bus` | 多智能体网关、通道路由（含 ChatUI）、消息总线、唤醒调度 |
| `agent::coordination` | 周期门（Local / Store-backed）与运行时协调 |
| `agent::memory` | 对话压缩、刷新、巩固、驱逐与会话记忆 |
| `agent::team` · `agent::transcript` | 团队协作与对话转录 |
| `agent::workspace` | 工作区命名空间、路径规范化、SQLite 索引 |
| `filesystem` | Local / Overlay / Composite / Sandbox / Remote / Baked 文件系统 |
| `tool` | 文件系统、shell、web、memory、session、task、team、plan mode 等工具 |

## 环境要求

- **Rust** ≥ 1.80（`rust-version`，见根 `Cargo.toml`）
- **可选工具**：`docker` CLI（Docker 沙箱后端）、`pwsh`（Windows 下的沙箱 shell）。
- **网络**：全部依赖从 crates.io 拉取，**无外部 path 依赖**（ACP Rust SDK `agent-client-protocol`
  2.2.0 亦来自 registry，声明在 `crates/agentscope-extensions-acp/Cargo.toml`；
  `agentscope-harness` 已不再依赖该 SDK）。

## 构建与测试

```bash
cargo build  --workspace                 # 构建
cargo test   --workspace                 # 单元 + 集成测试
cargo clippy --workspace --all-targets   # 静态检查
# 构建 harness 与 ACP 二进制（cli + acp_harness_agent）
cargo build --profile embedded -p agentscope-harness -p agentscope-extensions-acp --no-default-features --features lite --bins
# release / embedded 构建两个 ACP 二进制（Windows 便捷脚本）
build-acp-release.bat
build-acp-embedded.bat
```

体积优化（嵌入式场景）：

```bash
# 构建 harness 与 ACP 二进制（cli + acp_harness_agent）
cargo build --profile embedded -p agentscope-harness -p agentscope-extensions-acp --no-default-features --features lite --bins
```

- 默认 feature `rusqlite`：启用工作区 SQLite 索引；
- `lite`：去掉 rusqlite（省去 bundled SQLite C 引擎），索引降级为目录扫描；
- `embedded` profile：`opt-level="z"` + fat LTO + strip。

> 若编译报 `os error 112`（磁盘空间不足），先删除 `target/debug/incremental` 再重试。

## 示例

`agentscope-core`（`tool_agent_demo` 用脚本化模型，**无需 API Key**）与 `agentscope-extensions-model`（真实 provider）：

```bash
cargo run -p agentscope-core --example tool_agent_demo       # ReAct + 工具调用 e2e
cargo run -p agentscope-extensions-model --example chat_demo # 真实模型 + SSE 流式
```

`chat_demo` 通过环境变量取凭据（`OPENAI_API_KEY` / `OPENAI_BASE_URL` / `AGENTSCOPE_MODEL`）。

最小内嵌用法：

```rust
use std::sync::Arc; // 另需 serde_json
use agentscope_core::agent::ReActAgent;
use agentscope_core::message::{JsonMap, Msg, ToolResultBlock};
use agentscope_core::tool::{FunctionTool, ToolExecutionContext, Toolkit};

let toolkit = Toolkit::new();
toolkit.register_tool(Arc::new(FunctionTool::new(
    "add",
    "Adds two integers.",
    serde_json::json!({"type":"object","properties":{
        "a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
    Box::new(|args: JsonMap, _ctx: ToolExecutionContext| {
        let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
        let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(ToolResultBlock::text(format!("{} + {} = {}", a, b, a + b)))
    }),
)));

let agent = ReActAgent::builder()
    .name("tool-agent")
    .model(model)        // Arc<dyn Model>
    .toolkit(toolkit)
    .max_iters(3)
    .build()?;

let reply = agent.call(vec![Msg::user("What is 3 + 4?")]).await?;
println!("{}", reply.text_content());
```

完整可运行版本见 `crates/agentscope-core/examples/`。

## ACP 编程智能体

```bash
# ACP 服务端：等待 ACP 客户端（IDE/CLI）通过 stdio 接入
cargo run -p agentscope-extensions-acp --bin acp_harness_agent

# 交互式 REPL 客户端：自动拉起上面的 agent 子进程
cargo run -p agentscope-extensions-acp --bin acp_agent_cli
```

模型集中在 `~/.agentscope/models.json`（首次运行自动生成，含默认 provider 与模型列表；解析为宽松模式，允许 `//`、`/* */` 注释与尾随逗号，文件已存在时不会被覆写），
环境变量可临时覆盖配置：

| 变量 | 说明 |
| --- | --- |
| `CODING_PROVIDER` | 覆盖 `default_provider` |
| `CODING_MODEL_ID` | 覆盖 `default_model` |
| `OPENAI_API_KEY` | 覆盖 provider 的 API Key |
| `OPENAI_BASE_URL` | 覆盖 provider 的 base URL |
| `ACP_AGENT_EXE` | 指定 CLI 要拉起的 agent 可执行文件路径 |

REPL slash 命令：`/model <id>` 切换模型、`/models` 列出模型、`/quit` 退出。

完整清单（含 ACP 运行时、模型目录、扩展与测试变量，含默认值）见下文 [环境变量](#环境变量)。

## 环境变量

约定：布尔型统一接受 `1/true/on/yes`（开）与 `0/false/off/no`（关），大小写不敏感；数值型遇到非法值一律回落默认（不会静默关掉功能）。

### 模型与 provider

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `CODING_PROVIDER` | `~/.agentscope/models.json` 的 `default_provider` | ACP：覆盖默认 provider |
| `CODING_MODEL_ID` | 同上的 `default_model` | ACP：覆盖默认模型 id |
| `OPENAI_API_KEY` | 配置文件中的 `api_key` | 覆盖 provider 的 api_key（启动时会打 warn 提示它覆盖了配置） |
| `OPENAI_BASE_URL` | 配置文件中的 `base_url` | 覆盖 provider 的 base URL |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` | - | Anthropic 凭据：api key 优先，其次 auth token |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Anthropic 网关 |
| `DASHSCOPE_API_KEY` | - | DashScope 凭据（缺失即报错） |
| `DASHSCOPE_BASE_URL` | `https://dashscope.aliyuncs.com` | DashScope 网关 |
| `GEMINI_API_KEY` | - | Gemini 凭据（缺失即报错） |
| `GEMINI_BASE_URL` | `https://generativelanguage.googleapis.com` | Gemini 网关 |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama 地址 |
| `DEEPSEEK_API_KEY` | - | harness `web_fetch` / `web_search` 走 DeepSeek 抓取通道时的 Bearer 密钥 |
| `TYPESAFE_API_KEY` / `JEV_API_KEY` | - | judge 扩展 `jev` 的凭据（前者优先） |

### 模型目录（models.dev catalog）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_MODEL_CATALOG` | 开 | 设为 `0` 即不加载目录，模型能力与上下文窗口回落内置前缀表 |
| `AGENTSCOPE_MODEL_CATALOG_API` | 开 | 是否使用详细来源 `api.json` |
| `AGENTSCOPE_MODEL_CATALOG_DIR` | `<home>/.agentscope/model-catalog` | 本地缓存目录 |
| `AGENTSCOPE_MODEL_CATALOG_URL` | `https://models.dev/models.json?type=all` | 平铺来源 URL |
| `AGENTSCOPE_MODEL_CATALOG_API_URL` | `https://models.dev/api.json` | 详细来源 URL |
| `AGENTSCOPE_MODEL_CATALOG_TTL_HOURS` | `24` | 缓存有效期（小时） |
| `AGENTSCOPE_MODEL_CATALOG_COLD_BLOCK_MS` | `0` | 冷启动时阻塞等待目录刷新的上限（毫秒） |

### ACP 智能体（`acp_harness_agent` / `acp_agent_cli`）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `ACP_AGENT_EXE` | 与 CLI 同目录的 `acp_harness_agent` | CLI 拉起的 agent 可执行文件 |
| `ACP_SESSION_ID` | 新建会话 | CLI 打开的会话 id |
| `ACP_SESSION_REPLAY` | 关 | 设置任意非空值即重放该会话历史 |
| `AGENTSCOPE_LOG_DIR` | `<cwd>/.agentscope/log` | 日志目录，写 `acp-agent.log` |
| `RUST_LOG` | `info,acp_harness_agent=debug,agentscope_harness=debug` | tracing 过滤器（如 `RUST_LOG=trace`） |
| `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | tokio worker 线程数（每线程一份栈与分配器缓存） |
| `AGENTSCOPE_ACP_MEM_REPORT` | 关 | 每个 turn 额外记录序列化后的 `AgentState` 体积 |
| `AGENTSCOPE_ACP_MEM_COLLECT` | 开 | 每个 turn 后向分配器归还内存；`=0` 关闭（unix 有效，Windows 为 no-op） |
| `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS` | 关 | 把每次模型请求体（`request/prepared`）写进会话 journal，排查用；单次可达数 MB |
| `AGENTSCOPE_ACP_COMPACTION_WINDOW` | `256000` | 对话压缩使用的窗口属性（tokens），**与所选模型的窗口无关**；触发点为 `trigger_for_window(该值)`，默认 `218419` |
| `AGENTSCOPE_SESSION_RETENTION` | ACP 二进制 `12h`；库默认 30 天 | 会话 journal 保留窗口；`off/0/false/no` 关闭清理；裸数字按天，支持 `m`/`h`/`d` 后缀 |
| `AGENTSCOPE_JOURNAL_RETENTION_DAYS` | `min(会话保留天数, 30)` | 空闲会话目录（journal）按天淘汰 |

### 工具输出

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_OUTPUT_COMPACT` | 开 | 关闭 shell / rg 输出的重复行折叠（`0/false/off/no`） |

### 扩展（沙箱 / 对象存储 / 服务发现 / 控制面）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `E2B_SANDBOX_URL` | 按 sandbox id 生成通配域名 | E2B envd 数据面网关 |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` | - | OSS(S3) 凭据，仅当启用 `env_credentials` 时读取 |
| `AGENTSCOPE_NACOS_SKILL_VERSION` / `AGENTSCOPE_NACOS_SKILL_LABEL` | - | Nacos skill 版本 / 标签（等价属性键 `agentscope.nacos.skill.version` 与 `agentscope.nacos.skill.label`） |
| `NACOS_A2A_AGENT_<TRANSPORT>_<ATTR>` | - | A2A 传输属性；`ATTR` 取 `HOST`/`PORT`/`PATH`/`PROTOCOL`/`QUERY`/`SUPPORTTLS` |
| `CONTROL_PLANE_HTTP` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` | 必填 | control-plane store 工厂 |
| `CONTROL_PLANE_NAMESPACE` | `default` | control-plane 命名空间 |

### 测试与示例

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_TEST_DOCKER_IMAGE` | 内置默认镜像 | Docker 沙箱集成测试使用的镜像（无 daemon 时整组跳过） |
| `AGENTSCOPE_MODEL` | 示例内默认模型 | 仅 `crates/*/examples/` 下的示例程序读取 |

### 只读取、无需设置的系统变量

`HOME` / `USERPROFILE`（定位 `~/.agentscope`）、`LANG`（系统提示中的语言，缺省 `zh`）、`HOSTNAME` / `COMPUTERNAME`（control-plane 实例 id 的默认值）。

## 文档

| 路径 | 说明 |
| --- | --- |
| `docs/MIGRATION_PLAN.md` | 迁移方案：可行性、依赖映射、技术难点、测试策略 |
| `docs/MIGRATION_GAP_TRACKER.md` | 迁移进度与缺口清单（单一事实来源） |
| `docs/ACP_AGENT_PLAN.md` | ACP CodingAgent 方案与架构设计 |
| `docs/ACP_AGENT__GAP_TRACKER.md` | ACP CodingAgent 进度跟踪 |
| `crates/agentscope-extensions-acp/README.md` | ACP 运行说明（skills / 环境变量） |
| `.agents/context/` | 跨会话项目上下文（项目画像、当前状态、ADR、经验教训） |

## 已知限制

- 迁移仍在进行（以 core + harness 为主）：`core.hook` 因 Java 侧已废弃而**不迁移**；`core.rag` 尚未开始。
- Docker 沙箱走 `docker` CLI；本机无 daemon 时相关集成测试自动跳过（真实容器 e2e 未验证）。
- 暂无 Java 风格的 `@Tool` 过程宏，工具 schema 目前手写（`FunctionTool`）。
- 仓库尚无 `LICENSE` 文件（`Cargo.toml` 中声明 Apache-2.0）。

## 许可证

Apache-2.0（见 `Cargo.toml`）

