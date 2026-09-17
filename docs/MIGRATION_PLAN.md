# AgentScope Java → Rust 迁移方案（精简版）

> **定位**：本文件仅承载**方案**（可行性、依赖映射、技术难点、测试策略、结论）。
> **进度与缺口清单**：见 `MIGRATION_GAP_TRACKER.md`（跨会话单一事实来源，含逐项执行状态与交接信息）。
> Java 源码目录: `[D:\IdeaProjects](E:\local-github))\agentscope-java`　目标模块: `agentscope-core` + `agentscope-harness`
> 目标目录: `[D:\IdeaProjects](E:\local-github)\agentscope-rust`　参考: `[D:\IdeaProjects](E:\local-github)\rust-mcp-sdk`

## 1. 可行性分析结论

**结论：完全可以，且建议分两步走——先迁移 core + harness 两个核心模块，再扩展其它模块。**

### 1.1 为什么可行
| 维度 | 评估 |
|------|------|
| 语言范式 | Java OOP + 反射 + 响应式(Reactor) → Rust 等价映射（trait + async/Stream + 宏）|
| 并发模型 | Reactor/异步 → tokio async/await（更贴合，性能更好）|
| 序列化 | Jackson → serde/serde_json（能力对齐，含派生宏）|
| JSON Schema | victools → schemars（功能对齐）|
| MCP | `io.modelcontextprotocol.sdk:mcp` → `rust-mcp-sdk`（本地有源码）|
| HTTP/WebSocket | OkHttp → reqwest + tokio-tungstenite |
| 日志 | SLF4J → tracing |
| 测试 | JUnit + Mockito → tokio::test + mock 库 |

### 1.2 难点（非障碍）
- **反射/注解驱动的 Tool 系统**：Java 用 `@Tool` 反射生成 schema；Rust 用 `proc-macro`（参考 rust-mcp-macros 的 `MCPToolSchema`）。
- **响应式背压**：Flux/Mono → `futures::Stream` + `async-stream`。
- **树结构状态模型**：消息/事件层级用 enum + serde tag 天然表达。

## 2. 规模

| 模块 | 主代码文件 | 主代码行 | 测试文件 | 测试行 |
|------|-----------|---------|---------|--------|
| agentscope-core | 301 | 50,854 | 182 | 47,064 |
| agentscope-harness | 217 | 39,086 | 100 | 19,519 |
| **合计** | **518** | **~89,940** | **282** | **~66,583** |

当前迁移进度与各模块对齐状态：**见 `MIGRATION_GAP_TRACKER.md`**。

## 3. 依赖映射表（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|-----------|------|-----------|------|
| reactor-core (Flux/Mono) | 响应式流 | `tokio`, `futures`, `async-stream` | 流式生成用 async Stream |
| jackson-databind / annotations | JSON | `serde`, `serde_json` | 派生宏对齐注解 |
| jackson-datatype-jsr310 | 时间 | `chrono` | serde feature |
| jackson-dataformat-yaml | YAML | `serde_yaml` | |
| victools jsonschema-generator | JSON Schema 生成 | `schemars` | JsonSchema derive |
| networknt json-schema-validator | 校验 | `jsonschema` | 已接入 ToolValidator |
| io.modelcontextprotocol.sdk:mcp | MCP 客户端/服务端 | `rust-mcp-sdk` (+ rust-mcp-transport) | 本地有源码 |
| okhttp | HTTP/WS 客户端 | `async-openai`（底层 reqwest + eventsource-stream） | 见 §4.2；依赖走**远程 crates.io** v0.41.3，`D:\IdeaProjects\async-openai` 仅作源码参考 |
| slf4j | 日志 | `tracing` + `tracing-subscriber` | |
| tree-sitter (bash) | 命令安全分析 | `tree-sitter` + `tree-sitter-bash` | Rust 原生更佳 |
| opentelemetry | 可观测性 | `opentelemetry` + `tracing-opentelemetry` | |
| commons-compress | tar 解压 | `tar` + `flate2` | zip-slip 防护 |
| sqlite-jdbc | 工作区索引 | `rusqlite` (bundled) | |
| jedis / aliyun-oss-sdk-oss / kubernetes-client (可选) | 快照/沙箱后端 | `redis` / `aliyun-oss-rust-sdk` / `kube` | 可先留 trait 接口 |

> **模型请求统一改用开源库 `async-openai`**（v0.41.3，MIT）。不再手写 `HttpTransport`/具体 provider（原 tracker 2.2/2.3 手写方案作废）。
> **依赖来源约定**：`async-openai` 一律通过**远程 crates.io 仓库**引入（`async-openai = { version = "0.41", features = [...] }`）。本地 `D:\IdeaProjects\async-openai` 目录**仅供源码参考**，不做 path 依赖。

## 4. 技术难点与对策

### 4.1 Tool 系统（反射 → 宏）
- 定义 `Tool` trait：`name()` / `description()` / `parameters_schema()` / `call(&self, input, ctx) -> Result<ToolResultBlock>`。
- 用 `proc-macro` 从 struct 字段生成 `input_schema`；函数式工具用闭包包裹成 trait object。
- 治理层（分组/校验/执行/超时/重试）已按 `MIGRATION_GAP_TRACKER.md` 2.1 落地。

### 4.2 模型 HTTP（改用 async-openai 开源库）
用开源库 `async-openai`（v0.41.3，MIT，**远程 crates.io 依赖**）实现 `Model` trait 的 `stream`，**不再手写 transport/provider**。本地 `D:\IdeaProjects\async-openai` 仅作源码参考。
- **能力**：Chat Completions + SSE 流式、builder 模式、指数退避重试、限流、tower 中间件。
- **凭据**：自动读 `OPENAI_API_KEY` / `OPENAI_BASE_URL`（补 tracker 7.2 凭据加载）。
- **对接本框架**：`Model::stream` 内部用 `async-openai` 的 `chat().create_stream()`，把 `CreateChatCompletionStreamResponse` 的 delta 映射为 `ChatResponse`/`ContentBlock`。
- **OpenAI 兼容 provider**（DeepSeek/Kimi/GLM/Ollama 等）：用 `OpenAIConfig` 自定义 `base_url` + BYOT（`serde_json::Value`）规避请求/响应结构差异。
- **feature**：`chat-completion`（+ `chat-completion-types`）、`byot`、`middleware`。
- **引入方式**：`../Cargo.toml` 加 `async-openai = { version = "0.41", default-features = false, features = ["chat-completion", "chat-completion-types", "byot", "middleware", "rustls"] }`（走远程 crates.io）。
- **版本注意**：该库用 `reqwest 0.13`；若与 workspace 的 `reqwest 0.12` 冲突，考虑在其 feature 下统一或降级。

### 4.3 响应式流（Flux/Mono → async Stream）
- 单值：`Mono<T>` → `async fn -> Result<T>`。
- 多值流式：`Flux<ContentBlock>` → `impl Stream<Item=Result<ContentBlock>>`。
- 事件：`Flux<AgentEvent>` → `mpsc::Receiver<AgentEvent>` 或 `broadcast`。

### 4.4 事件层级（多子类 → 枚举）
20+ 事件类映射为一个带 payload 的 enum，用 serde tag 保持 JSON 兼容，与 Java 端字段对齐以保证跨语言状态文件可读。已实现 31/31 变体。

### 4.5 MCP 集成（使用 rust-mcp-sdk）
- 客户端：`rust_mcp_sdk::mcp_client::{ClientRuntime, McpClientOptions, ClientHandler}`。
- `rust_mcp_transport` 提供 stdio/sse 传输；`rust_mcp_macros::MCPToolSchema` 生成 schema；`rust_mcp_extra` 提供 SessionStore/EventStore。
- 建议 path 依赖：`rust-mcp-sdk = { path = "../../rust-mcp-sdk/crates/rust-mcp-sdk" }`。

### 4.6 沙箱与文件系统
- `kubernetes-client`/docker → Rust `kube` / `bollard`；tar 解压 → `tar` + `flate2`（zip-slip 防护）；本地 fs → `tokio::fs` / `walkdir` / `glob`。

### 4.7 状态序列化兼容
Rust serde 字段名/tag 与 Java Jackson 对齐，实现 `JsonFileAgentStateStore` 等价，保证老状态文件可直接加载。

### 4.8 优雅关闭
Java JVM 钩子 → tokio `CancellationToken` + `ctrl_c` + Drop 钩子实现 `GracefulShutdownManager`（已实现状态机）。

## 5. 测试策略
| 类型 | 方案 |
|------|------|
| 单元测试 | 每个模块内 `#[cfg(test)]` + `tokio::test` |
| JSON 兼容 | golden 测试：Java 端 JSON fixture 断言 serde 往返一致 |
| Mock 模型 | trait 注入假 Model |
| 集成 | ReAct 最小循环 + 工具调用 e2e |
| 属性测试 | `proptest` 验证序列化不变量 |
| 覆盖率 | `cargo-llvm-cov` |

## 6. 关于"完全重构"的整体结论
- **核心两个模块（core + harness）**：可行，约 90K 行主代码，按 7 阶段推进。
- **其余模块**（extensions、spring-boot-starters、distribution）：extensions 多为协议/存储适配，Rust 可自然映射；spring-boot-starters 依赖 JVM 生态，不适合用 Rust 重构，作"可选/后期"项。
- **建议**：先交付可独立运行的 `agentscope-rust`（core+harness），验证 API 与功能等价后再扩展。
- **里程碑/路线图**：见 `MIGRATION_GAP_TRACKER.md` 第 6 节。
