# AgentScope Rust

AgentScope 的**原生异步 Rust 实现**。以 `agentscope-core`（智能体运行时与公共抽象）和
`agentscope-harness`（可直接使用的装配层）为骨架，外加 27 个 `agentscope-extensions-*`
适配 crate，覆盖模型 provider、协议接入、持久化、RAG、沙箱、平台通道与可观测性；
仓库同时提供开箱即用的 **ACP 编程智能体**（`acp_harness_agent` / `acp_agent_cli`）。

- **基线**：对齐 Java 参考实现（`io.agentscope.core` / `io.agentscope.harness` 及其 extension 模块）
  的 API 语义、事件/状态 JSON 与工具治理行为
- **技术栈**：Rust 2021（MSRV 1.85）· Tokio · serde · reqwest/rustls · async-openai / async-anthropic ·
  rust-mcp-sdk · axum · rusqlite
- **规模**：29 个 crate（2 个骨架 + 27 个扩展），线性依赖，无循环
- **进度**：`docs/MIGRATION_GAP_TRACKER.md`、`docs/extensions/EXTENSIONS_GAP_TRACKER.md`
  （各模块另有独立 tracker，为单一事实来源）

## 核心能力

**运行时（core）**

- ReAct 循环、流式事件（32 种 `AgentEvent`）、结构化输出、串行门与优雅关闭
- run 级控制（`AgentRun` / `RunControl`）、中断恢复与 pending tool 修复
- 双轨会话：`SessionLogStore`（journal）+ 事件日志，可换后端（本地 / SQLite / BaseStore）

**模型层**

- `Model` / `ModelRegistry` 等抽象在 core，全部具体 provider 在 `agentscope-extensions-model`
  （OpenAI Chat / Responses、Anthropic、DashScope、Gemini、Ollama、DeepSeek 兼容）
- 统一 `HttpTransport` 缝：单测零真实网络；模型重试与上下文窗口按目录（models.dev）解析

**工具与状态**

- `Tool` / `Toolkit` / `ToolGroup` / `ToolExecutor`（超时、重试、并发、JSON Schema 校验、挂起语义）
- MCP 工具桥、文件 / shell / web / memory / session / task / plan mode 等内置工具
- `AgentState` + 版本化 CAS（core 的 `AgentStateStore` trait），状态文件与 Java 侧互通

**装配层（harness）**

- `HarnessAgent` 一键装配 workspace、文件系统、工具、中间件、skills、子智能体与记忆管线
- 沙箱（Local / Docker）与快照 Lease；Local / Overlay / Composite / Sandbox / Remote 文件系统
- 多智能体：声明式 + 动态 spawn、后台任务仓库、消息总线、gateway 与通道路由、team 协作
- 记忆管线：对话压缩（compaction）、刷新、巩固、驱逐与会话记忆

**生态（extensions）**

- 协议：ACP、A2A（client / server）、Agent Protocol、AG-UI、OpenAI 兼容 Chat Completions
- 存储：JDBC / MySQL / PostgreSQL / MongoDB / Redis / COS / OSS
- 能力：RAG、长期记忆、skills 仓库、定时调度、影子训练、Studio 遥测、judge 决策 API

## Crate 地图

### 骨架

| crate | 职责 |
| --- | --- |
| `agentscope-core` | 运行时与公共抽象：`ReActAgent`、`Msg`/`ContentBlock`、`AgentEvent`、`Model`/`ModelRegistry` SPI、`Tool`/`Toolkit`/`ToolExecutor`、`Middleware`、`AgentState`/`AgentStateStore`、session journal、权限 / 中断 / 关闭、formatter、credential、tls |
| `agentscope-harness` | 装配层：`HarnessAgent` 与 builder、workspace + 文件系统、沙箱、skills、子智能体与任务、记忆管线、gateway / 通道 / 总线、MCP 客户端、SQLite 工作区索引 |

### 协议与接入

| crate | 职责 |
| --- | --- |
| `agentscope-extensions-acp` | ACP 编程智能体：stdio 服务端 `acp_harness_agent` + REPL 客户端 `acp_agent_cli`，含权限确认（HITL）、会话压缩、动态模型切换 |
| `agentscope-extensions-a2a-client` | A2A 客户端：AgentCard 解析、JSON-RPC、`Msg` ⇄ A2A 转换、`A2aAgent` 门面 |
| `agentscope-extensions-a2a-server` | A2A 服务端：runner、任务注册表、JSON-RPC handler、registry 服务 |
| `agentscope-extensions-agent-protocol` | Agent Protocol：`/tasks` HTTP+SSE 任务生命周期与控制面 |
| `agentscope-extensions-agui` | AG-UI 事件分类、转换器、SSE 编码与请求处理 |
| `agentscope-extensions-chat-completions-web` | OpenAI 兼容 Chat Completions 请求 / 响应 / 流式适配 |
| `agentscope-extensions-protocol` | 上述协议的总装 crate（feature：`a2a` / `agent-protocol` / `agui` / `chat-completions-web`，默认全开） |

### 模型

| crate | 职责 |
| --- | --- |
| `agentscope-extensions-model` | 全部 provider 实现（OpenAI Chat / Responses、Anthropic、DashScope、Gemini、Ollama、DeepSeek 兼容），统一传输缝 + 离线测试替身 |

### 存储

| crate | 职责 |
| --- | --- |
| `agentscope-extensions-jdbc` | 多方言 JDBC 抽象：SQLite / Postgres / MySQL executor、原生锁会话、版本化 state |
| `agentscope-extensions-mysql` · `-postgresql` · `-mongodb` · `-redis` | 分布式存储：`AgentStateStore`、`BaseStore`、沙箱快照、执行锁 |
| `agentscope-extensions-cos` · `-oss` | 对象存储：腾讯云 COS / S3 兼容（分片上传、预签名 URL） |

### 能力扩展

| crate | 职责 |
| --- | --- |
| `agentscope-extensions-rag` | RAG：simple / dify / ragflow / haystack / bailian 知识库、embedding provider、文档 reader |
| `agentscope-extensions-mem` | 长期记忆：Mem0 / 百炼 / ReMe |
| `agentscope-extensions-sandbox` | 远程沙箱：AgentRun / Daytona / E2B / Kubernetes |
| `agentscope-extensions-skills` | Skill 仓库后端：git / MySQL / PostgreSQL（适配 harness `SkillRepository`） |
| `agentscope-extensions-scheduler` | 定时执行：Quartz 风格调度 + XXL-Job handler 注册 |
| `agentscope-extensions-training` | 影子流量训练：策略选择、奖励计算、Trinity backend、runner |
| `agentscope-extensions-channel` | 平台通道：钉钉 / 飞书 / 企业微信 / 个人微信 / GitHub / GitLab（回调 + 长连接） |
| `agentscope-extensions-nacos` | Nacos：A2A 注册发现、prompt 订阅、只读 skill 仓库 |
| `agentscope-extensions-higress` | Higress AI 网关工具检索（`x_higress_tool_search`） |
| `agentscope-extensions-controlplane` | control-plane 数据面 SDK：会话快照、事件流、有效上下文 |
| `agentscope-extensions-studio` | AgentScope Studio 集成（REST + Socket.IO）+ OpenTelemetry GenAI 遥测 |
| `agentscope-extensions-judge` | 校准式决策 API：TypeSafe JEV（概率分布）+ OpenAI Decisions |

## 架构与依赖规则

```text
agentscope-rust/
├── Cargo.toml                     # workspace：版本 / 依赖 / profile 统一在此
├── crates/
│   ├── agentscope-core/           # 运行时与公共抽象（不依赖任何其它 workspace crate）
│   ├── agentscope-harness/        # 装配层（依赖 core + extensions-model）
│   └── agentscope-extensions-*/   # 27 个适配 crate
├── docs/                          # 迁移方案、进度 tracker、环境变量、设计记录
│   └── extensions/                # 每个模块的迁移 tracker
└── LICENSE · README.md
```

- **`agentscope-core`**：只允许依赖第三方 crate。公共抽象若被多个模块需要，下沉到这里。
- **`agentscope-extensions-*`**：一般依赖 `core`（多数还需要 `harness` 的 `BaseStore` /
  `Sandbox` / `SkillRepository` 等 trait），少数依赖其它扩展（如 `mysql` → `jdbc`、`nacos` → `a2a-*`）。
  确需新增跨模块引用时，先在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 记「跨模块需求」并获裁定。
- **`agentscope-harness`**：装配层，不反向依赖任何 `extensions-*`（`extensions-model` 除外）。
- **禁止循环引用**：依赖图必须保持 DAG；出现循环时把公共抽象下沉到 core。

## 快速开始

### 从 crates.io 引入

workspace 已具备发布元数据，首发版本为预发布 `0.1.0-alpha.1`：

```toml
[dependencies]
agentscope-core = "0.1.0-alpha.1"
agentscope-harness = "0.1.0-alpha.1"
```

> 预发布版本不会被 `cargo add` 默认选中，需显式写出（或用 `--precise`）。

### 从源码构建

```bash
cargo build  --workspace                 # 构建
cargo test   --workspace                 # 单元 + 集成测试（默认全离线）
cargo clippy --workspace --all-targets    # 静态检查
```

- **Rust** ≥ 1.85（见根 `Cargo.toml` 的 `rust-version`）
- **可选工具**：`docker` CLI（Docker 沙箱后端与相关集成测试）、`pwsh`（Windows 下的沙箱 shell）
- 外部依赖全部来自 crates.io；workspace 内部通过 `path` 互相引用（发布时由 `version` 生效）

### 示例

```bash
cargo run -p agentscope-core --example tool_agent_demo        # 脚本化模型，无需 API Key
cargo run -p agentscope-extensions-model --example chat_demo  # 真实 provider + SSE
cargo run -p agentscope-harness --example session_journal_memory_probe
```

`chat_demo` 通过环境变量取凭据（`OPENAI_API_KEY` / `OPENAI_BASE_URL` / `AGENTSCOPE_MODEL`）。

### 最小内嵌用法

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
    .model(model)        // Arc<dyn Model>，如 agentscope_extensions_model 的 OpenAiChatModel
    .toolkit(toolkit)
    .max_iters(3)
    .build()?;

let reply = agent.call(vec![Msg::user("What is 3 + 4?")]).await?;
println!("{}", reply.text_content());
```

完整可运行版本见 `crates/agentscope-core/examples/`。

## ACP 编程智能体

```bash
# ACP 服务端：等待 ACP 客户端（IDE / CLI）通过 stdio 接入
cargo run -p agentscope-extensions-acp --bin acp_harness_agent

# 交互式 REPL 客户端：自动拉起上面的 agent 子进程
cargo run -p agentscope-extensions-acp --bin acp_agent_cli
```

不想自己编译的话，[Releases](https://github.com/Endlessworld/agentscope-rust/releases) 提供
Windows x64 预编译包（embedded profile + `lite` features，压缩包约 6 MB、解压后约 12 MB）：
`acp_harness_agent.exe`（agent）与 `acp_agent_cli.exe`（REPL）。产物由
`.github/workflows/release-acp.yml` 在打 tag 时构建，发布前会跑一次 ACP 握手冒烟测试。

模型集中在 `~/.agentscope/models.json`（首次运行自动生成；宽松解析，允许 `//`、`/* */`
注释与尾随逗号，已存在时不覆写）。最常用的覆盖变量：

| 变量 | 说明 |
| --- | --- |
| `CODING_PROVIDER` | 覆盖 `default_provider` |
| `CODING_MODEL_ID` | 覆盖 `default_model` |
| `OPENAI_API_KEY` / `OPENAI_BASE_URL` | 覆盖 provider 的凭据与网关 |
| `RUST_LOG` | tracing 过滤器（默认 `info,acp_harness_agent=debug,agentscope_harness=debug`） |
| `AGENTSCOPE_SESSION_RETENTION` | 会话 journal 保留窗口（二进制默认 `12h`） |

完整清单（provider、模型目录、ACP 运行时、扩展、测试变量，含默认值）见
[`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md)。

REPL slash 命令：`/model <id>` 切换模型、`/models` 列出模型、`/quit` 退出。

会话模式（`plan` / `accept_edits` / `yolo`）、权限持久化、支持的 ACP 方法与扩展、
工具与 skills、排障，以及 Rust / TypeScript / Python / Java / Kotlin 客户端接入示例，
见 [`crates/agentscope-extensions-acp/README.md`](crates/agentscope-extensions-acp/README.md)。

## 构建、测试与体积

```bash
# 嵌入式最小集：去掉 rusqlite（省掉 bundled SQLite C 引擎），索引降级为目录扫描
cargo build --profile embedded -p agentscope-harness -p agentscope-extensions-acp \
    --no-default-features --features lite --bins

# Windows 便捷脚本（release / embedded 两个 ACP 二进制）
build-acp-release.bat
build-acp-embedded.bat
```

- 默认 feature `rusqlite`：启用工作区 SQLite 索引；`lite` 关闭它
- `embedded` profile：`opt-level="z"` + fat LTO + `strip`（详见 `docs/EMBEDDED_SIZE.md`）

> 若编译报 `os error 112`（磁盘空间不足），先清 `target/debug/incremental` 与 `target/debug/deps/*.pdb` 再重试。

## 文档

| 路径 | 说明 |
| --- | --- |
| `docs/MIGRATION_PLAN.md` | 迁移方案：可行性、依赖映射、技术难点、测试策略 |
| `docs/MIGRATION_GAP_TRACKER.md` | core / harness 迁移进度与缺口清单 |
| `docs/extensions/EXTENSIONS_MIGRATION_PLAN.md` | 27 个扩展 crate 的迁移编排与回执约定 |
| `docs/extensions/*__GAP_TRACKER.md` | 各扩展模块的进度 tracker（单一事实来源） |
| `docs/ENVIRONMENT.md` | 全部环境变量参考 |
| `docs/ACP_AGENT_PLAN.md` · `docs/ACP_AGENT__GAP_TRACKER.md` | ACP 编程智能体的设计与进度 |
| `crates/agentscope-extensions-acp/README.md` | ACP 运行说明（skills / 环境变量） |
| `.agents/context/` | 跨会话项目上下文（画像、状态、ADR、经验教训） |

## 已知限制

- **迁移仍在收尾**：以各模块 tracker 为准。`core.hook` 因 Java 侧已废弃而**不迁移**；
  部分扩展仍有 P2 长尾（如 AISTIO 的 gRPC 传输、e2b 的 snapshot prune）。
- **真实后端需要外部服务**：MySQL / PostgreSQL / MongoDB / Redis / 对象存储的集成测试需
  自备服务或 Docker，默认跳过；单元测试全部离线（传输均经 trait 注入替身）。
- **Docker 沙箱**走 `docker` CLI，本机无 daemon 时相关测试自动跳过。
- 暂无 Java 风格的 `@Tool` 过程宏，工具 schema 目前手写（`FunctionTool`）。
- 部分运行时优化仅在 Unix 生效（如 `AGENTSCOPE_ACP_MEM_COLLECT` 在 Windows 为 no-op）。

## 许可证

Apache-2.0，见 [`LICENSE`](LICENSE)。
