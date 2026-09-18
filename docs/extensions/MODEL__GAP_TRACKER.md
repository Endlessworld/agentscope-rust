# MODEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MODEL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-model\`
> **Java 子模块**：anthropic, dashscope, gemini, ollama, openai, e2e-tests
> **Rust 目标**：`crates/agentscope-extensions-model/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-model` |
| Java 子模块 | anthropic, dashscope, gemini, ollama, openai, e2e-tests |
| Java 规模（实测行数） | 主代码 24885 行 / 测试 45043 行（anthropic 2192 / dashscope 7453 / gemini 2294 / ollama 4412 / openai 8534；e2e-tests 测试 7136） |
| Rust crate | `crates/agentscope-extensions-model` |
| 迁移波次 | A |
| 当前状态 | **部分完成** — `common`（HTTP seam）+ `ollama`（provider 全链路）已落地并测试全绿；`dashscope` 仅有 DTO 且**未接线**（详见 §3.1）；`anthropic` / `gemini` **尚未迁移**；`openai` 依裁定不迁移（core 已提供） |

### 1.1 验证基线（最近一次实测，汇总方执行；本次回填复跑 build 确认）
```
cargo build -p agentscope-extensions-model   -> Finished (0 errors；本 crate 0 error，仅有无关 crate 的既有 warning)
cargo test  -p agentscope-extensions-model   -> 25 passed; 0 failed; 0 ignored
cargo test  --workspace                      -> 1485 passed; 0 failed
```

### 1.2 Rust crate 文件清单（12 个源文件，约 2016 行；无 `tests/` 目录，25 个单测全部为内联 `#[cfg(test)]`）
| 文件 | 行数 | 职责 | 是否被 `lib.rs` 编译 |
|---|---|---|---|
| `src/lib.rs` | 25 | crate 根；仅声明 `pub mod common; pub mod ollama;` + re-export + 设计裁定文档注释 | — |
| `src/common/mod.rs` | 11 | `HttpTransport` seam 的 re-export | 是 |
| `src/common/transport.rs` | 244 | `HttpTransport` trait、`HttpRequestSpec`、`ReqwestTransport`（真实）、`MemoryTransport`（离线 fake）、`decode_stream_line`、`JsonStream`、`SharedTransport`；3 个单测 | 是 |
| `src/ollama/mod.rs` | 20 | ollama 子模块声明与 re-export | 是 |
| `src/ollama/dto.rs` | 155 | Ollama wire DTO（含 embedding DTO） | 是 |
| `src/ollama/options.rs` | 190 | `OllamaOptions` / `ThinkOption`；3 个单测 | 是 |
| `src/ollama/converter.rs` | 402 | `Msg`/`ToolSchema` → Ollama DTO；`ImageResolver` seam；7 个单测 | 是 |
| `src/ollama/parser.rs` | 265 | 响应解析 + 流式增量器；5 个单测 | 是 |
| `src/ollama/model.rs` | 252 | `OllamaChatModel`（`impl agentscope_core::Model`）；4 个单测 | 是 |
| `src/ollama/provider.rs` | 94 | `OllamaModelProvider`（`impl ModelProvider`）；3 个单测 | 是 |
| `src/dashscope/mod.rs` | 23 | DashScope 子模块声明/re-export | **否**（`lib.rs` 未声明 `pub mod dashscope;`） |
| `src/dashscope/dto.rs` | 335 | DashScope wire DTO（15 个 pub 类型） | **否**（随 `dashscope` 一起未编译） |

> ⚠️ **`dashscope` 接线缺陷（本次回填实测发现，未修改代码）**：`src/dashscope/mod.rs` 声明了 `pub mod converter; pub mod model; pub mod parser; pub mod provider;` 并 `pub use` 其符号，但该目录下**只有 `dto.rs` 与 `mod.rs` 两个文件**（见 §3.1 P0-6）。因此该子模块当前既不能被编译、也不能被声明进 `lib.rs`，必须先补齐实现再接线。

## 2. Java 清单与依赖映射

> Java 类清单位于 `agentscope-extensions-model-<子模块>/src/main/java/io/agentscope/extensions/model/<子模块>/`；下表逐类给出 Rust 对应，未迁移者写明「待办 / 设计决定」。
> Java 侧每个子模块另有 `src/main/resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider`（SPI 注册文件，非 Java 类）。

### 2.1 ollama（22 类 / 4412 行）— **已迁移**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `ollama.OllamaChatModel` | 聊天模型主类（流式/非流式、工具调用） | `ollama::model::OllamaChatModel`（`impl agentscope_core::Model`） | 通过注入的 `SharedTransport` 调 `POST {base_url}/api/chat`，离线可测 |
| `ollama.OllamaHttpClient` | HTTP/NDJSON 客户端 | `common::transport::{HttpTransport, ReqwestTransport, MemoryTransport}` + `ollama::model::default_transport` | 泛化为全 crate 共用 seam，未逐方法照搬 |
| `ollama.OllamaModelProvider` | 注册表 provider 插件 | `ollama::provider::OllamaModelProvider`（`impl agentscope_core::ModelProvider`） | `provider_id()=="ollama"`；支持 `ollama:` 前缀与裸 id 兜底；`OLLAMA_BASE_URL` 环境变量 |
| `ollama.credential.OllamaCredential` | 凭据载体 | 无独立类型；经 `ModelCreationContext.api_key` / 环境变量 | **设计决定**：本地 Ollama 无鉴权头，不引入平行凭据类型 |
| `ollama.dto.OllamaRequest` | 请求体 | `ollama::dto::OllamaRequest` | `build_request` 构造 |
| `ollama.dto.OllamaResponse` | 响应体 | `ollama::dto::OllamaResponse` | 含 `total_duration`/`eval_count` 等 |
| `ollama.dto.OllamaMessage` | 消息 | `ollama::dto::OllamaMessage` | `new(role, content)` |
| `ollama.dto.OllamaTool` | 工具声明 | `ollama::dto::OllamaTool` | |
| `ollama.dto.OllamaToolFunction` | 工具函数 schema | `ollama::dto::OllamaToolFunction` | |
| `ollama.dto.OllamaFunction` | 函数 | `ollama::dto::OllamaFunction` | |
| `ollama.dto.OllamaToolCall` | 工具调用 | `ollama::dto::OllamaToolCall` | |
| `ollama.dto.OllamaEmbeddingRequest` | `POST /api/embed` 请求 | `ollama::dto::OllamaEmbeddingRequest` | **DTO 已迁移但无调用方**（详见 §3.2） |
| `ollama.dto.OllamaEmbeddingResponse` | embed 响应 | `ollama::dto::OllamaEmbeddingResponse` | 同上 |
| `ollama.formatter.OllamaChatFormatter` | 会话格式化 | `ollama::converter::{to_ollama_messages, to_ollama_messages_with, build_request}` | |
| `ollama.formatter.OllamaMessageConverter` | 单条消息转换 | `ollama::converter::{block_text, collect_text, role_str, url_of, ImageResolver, DefaultImageResolver}` | |
| `ollama.formatter.OllamaToolsHelper` | 工具/工具选择转换 | `ollama::converter::{to_ollama_tools, to_tool_choice}` | |
| `ollama.formatter.OllamaMediaConverter` | 图片/媒体 → base64 | `ollama::converter::{ImageResolver, DefaultImageResolver}` | 远程 http(s) URL **不在此处下载**，需注入自定义 resolver（见 §3.2 P1） |
| `ollama.formatter.OllamaResponseParser` | 响应 → `ChatResponse` | `ollama::parser::{parse_response, OllamaStreamIncrementer}` | 工具调用 id 用 `uuid` 合成（同 Java） |
| `ollama.formatter.OllamaConversationMerger` | 多轮/多智能体会话合并 | 无 | **待办**（见 §3.2） |
| `ollama.formatter.OllamaMultiAgentFormatter` | 多智能体格式化 | 部分：`ollama::converter::hint_text` + tool-result 文本抽取 | **待办**（Java 的 `<system-info>` 图片提升仅部分覆盖） |
| `ollama.options.OllamaOptions` | 采样/加载参数 | `ollama::options::OllamaOptions` | 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`；llama.cpp loader 长尾见 §3.3 P2 |
| `ollama.options.ThinkOption` | thinking 开关/级别 | `ollama::options::ThinkOption` | `Boolean(bool)` / `Level(String)` |
| `resources/META-INF/services/...ModelProvider` | SPI 注册 | 无对应机制；改为 `ModelRegistry::register(Arc<dyn ModelProvider>)` 显式注册 | **设计决定**（Rust 无 ServiceLoader） |

### 2.2 dashscope（29 类 / 7453 行）— **部分迁移（仅 DTO，且未接线）**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `dashscope.dto.DashScopeRequest` | 顶层请求体 | `dashscope::dto::DashScopeRequest` | 未接线 |
| `dashscope.dto.DashScopeInput` | `input.messages` 容器 | `dashscope::dto::DashScopeInput`（`of()`） | 未接线 |
| `dashscope.dto.DashScopeParameters` | `parameters`（result_format/incremental_output/…） | `dashscope::dto::DashScopeParameters` | 默认 `result_format="message"`；未接线 |
| `dashscope.dto.DashScopeSearchOptions` | 联网搜索策略 | `dashscope::dto::DashScopeSearchOptions` | 未接线 |
| `dashscope.dto.DashScopeMessage` | 消息（content 多态） | `dashscope::dto::{DashScopeMessage, DashScopeContent}` | Java `Object` content → Rust `#[serde(untagged)]` enum（Rust 侧扩展类型） |
| `dashscope.dto.DashScopeContentPart` | 多模态内容片段 | `dashscope::dto::DashScopeContentPart` | 未接线 |
| `dashscope.dto.DashScopeTool` | 工具声明 | `dashscope::dto::DashScopeTool` | 未接线 |
| `dashscope.dto.DashScopeToolFunction` | 工具函数 schema | `dashscope::dto::DashScopeToolFunction` | 未接线 |
| `dashscope.dto.DashScopeToolCall` | 工具调用 | `dashscope::dto::DashScopeToolCall` | 未接线 |
| `dashscope.dto.DashScopeFunction` | 函数 | `dashscope::dto::DashScopeFunction` | 未接线 |
| `dashscope.dto.DashScopeResponse` | 响应体 | `dashscope::dto::DashScopeResponse` | 未接线 |
| `dashscope.dto.DashScopeOutput` | `output` | `dashscope::dto::DashScopeOutput` | 未接线 |
| `dashscope.dto.DashScopeChoice` | 单条 choice | `dashscope::dto::DashScopeChoice` | 未接线 |
| `dashscope.dto.DashScopeUsage` | token 用量 | `dashscope::dto::DashScopeUsage` | 未接线 |
| `dashscope.dto.DashScopePublicKeyResponse` | 加密公钥响应 | 无 | **待办**（配合 `DashScopeEncryptionUtils`） |
| `dashscope.DashScopeChatModel` | 聊天模型主类 | 无 | **待办**（`dashscope::model` 仅在 `mod.rs` 中被声明，文件不存在） |
| `dashscope.DashScopeHttpClient` | HTTP/SSE 客户端 | 无 | **待办**（`dashscope::converter`/`parser` 同上） |
| `dashscope.DashScopeEncryptionUtils` | 请求加密 | 无 | **待办** |
| `dashscope.DashScopeModelProvider` | 注册表 provider | 无 | **待办**（`dashscope::provider` 同上） |
| `dashscope.EndpointType` | 端点类型枚举 | 无 | **待办** |
| `dashscope.credential.DashScopeCredential` | 凭据 | 无 | **待办** |
| `dashscope.formatter.*`（7 类：ChatFormatter / ConversationMerger / MediaConverter / MessageConverter / MultiAgentFormatter / ResponseParser / ToolsHelper） | 格式化/解析 | 无 | **待办** |
| `dashscope.tool.DashScopeMultiModalTool`（93.5 KB） | 多模态工具（TTS/图像/视频等） | 无 | **待办**（本模块最大单文件） |

### 2.3 openai（53 类 / 8534 行）— **设计决定（不迁移）**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openai.OpenAIChatModel` / `OpenAIClient` / `OpenAIModelProvider` / `OpenAIModelProviderSupport` | OpenAI 兼容聊天链路与 provider | `agentscope_core::model::openai::{OpenAiChatModel, OpenAiProvider, default_registry}`（core，`model/openai.rs`，32 KB，基于 `async-openai`） | **已由 core 覆盖**，本 crate 重复实现会违反 `EXTENSIONS_GAP_TRACKER.md` §2.2 裁定 |
| `openai.dto.*`（16 类：Request/Response/Message/Choice/ContentPart/Function/ImageUrl/InputAudio/ReasoningDetail/StreamOptions/Tool/ToolCall/ToolFunction/Usage/VideoUrl/Error） | OpenAI wire 模型 | core `model/openai.rs` 内部 DTO + `async-openai` 类型 | 同上 |
| `openai.exception.*`（8 类：OpenAIException/Authentication/BadRequest/InternalServer/NotFound/PermissionDenied/RateLimit/UnprocessableEntity） | 异常层次 | `agentscope_core::Error`（`Error::Model`/`Error::Http` 等） | **设计决定**：Rust 用单一 `thiserror` 枚举，不 1:1 建异常类层次 |
| `openai.formatter.*`（10 类） | OpenAI 格式/转换/解析 | core `model/openai.rs`（`to_request_messages`/`to_request_tools`/`to_tool_choice`/`to_response_format`/`OpenAiStreamIncrementer`） | 已覆盖 |
| `openai.tool.OpenAIMultiModalTool` | 多模态工具 | 无 | **待确认**（core 无对应；是否需要在 MODEL 或 TOOL 侧补齐待汇总方裁定） |
| `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 兼容厂商 formatter + provider | 无专门类型；经 core `OpenAiProvider` + `ModelCreationContext.base_url` + `context_windows.rs`（含 deepseek/glm/kimi 窗口表）覆盖 | **设计决定**：Java 每厂商一个 `*ModelProvider` + `*Formatter` 的做法未被 1:1 迁移；其差异（base_url/模型窗口/格式化）当前靠 base_url + 上下文窗口表覆盖。**残留差异待确认**（见 §3.2） |
| `resources/META-INF/services/...ModelProvider`（5 行） | SPI 注册 | core `default_registry()` / `ModelRegistry::register` | |

### 2.4 anthropic（11 类 / 2192 行）— **未迁移**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `anthropic.AnthropicChatModel` | 聊天模型主类 | 无 | **待办 P1** |
| `anthropic.AnthropicModelProvider` | provider 插件 | 无 | **待办 P1** |
| `anthropic.credential.AnthropicCredential` | 凭据 | 无 | **待办**（可复用 `ModelCreationContext.api_key`） |
| `anthropic.formatter.AnthropicBaseFormatter` | 基础格式化 | 无 | **待办** |
| `anthropic.formatter.AnthropicChatFormatter` | 会话格式化 | 无 | **待办** |
| `anthropic.formatter.AnthropicConversationMerger` | 会话合并 | 无 | **待办** |
| `anthropic.formatter.AnthropicMediaConverter` | 媒体转换 | 无 | **待办** |
| `anthropic.formatter.AnthropicMessageConverter` | 消息转换 | 无 | **待办** |
| `anthropic.formatter.AnthropicMultiAgentFormatter` | 多智能体格式化 | 无 | **待办** |
| `anthropic.formatter.AnthropicResponseParser` | 响应解析（含 cache control / thinking） | 无 | **待办** |
| `anthropic.formatter.AnthropicToolsHelper` | 工具转换 | 无 | **待办** |

> Anthropic 使用官方 SDK（`com.anthropic.client.okhttp.AnthropicOkHttpClient`），Rust 侧需 `reqwest` 手写或引入第三方 crate——**属 §4.2 待裁定项**。

### 2.5 gemini（10 类 / 2294 行）— **未迁移**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `gemini.GeminiChatModel` | 聊天模型主类 | 无 | **待办 P1** |
| `gemini.GeminiModelProvider` | provider 插件 | 无 | **待办 P1** |
| `gemini.credential.GeminiCredential` | 凭据 | 无 | **待办** |
| `gemini.formatter.{GeminiChatFormatter, GeminiConversationMerger, GeminiMediaConverter, GeminiMessageConverter, GeminiMultiAgentFormatter, GeminiResponseParser, GeminiToolsHelper}`（7 类） | 格式化/转换/解析 | 无 | **待办 P1** |

### 2.6 e2e-tests（0 主代码 / 34 测试类 / 7136 行）— **未迁移**
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `io.agentscope.core.e2e.*`（CoreAgentE2E、MultiModalE2E、StreamingBehaviorE2E、StructuredOutputE2E、ToolSystemE2E、CrossModelCompatibilityE2E 等 34 个测试类）+ `providers.*`（ModelProvider/BaseModelProvider/AnthropicProvider/DashScopeProvider/OpenRouterProvider 等） | 真实厂商端到端回归（需真实 API key） | 无 | **设计决定（不迁移）**：Rust 侧统一采用**离线可测**策略——`HttpTransport` seam + `MemoryTransport` 注入，不引入需要密钥的网络测试；各 provider 的真实联调记为 §3.2 待办 |

### 2.7 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Jackson (`@JsonProperty`/`@JsonInclude`/`@JsonAlias`/自定义 (De)Serializer) | JSON (反)序列化 | `serde` + `serde_json`（`[dependencies]` 经 workspace 引入）+ `#[serde(rename/skip_serializing_if/untagged)]` | 字段名 1:1 对齐 Java JSON |
| OkHttp / `java.net.http.HttpClient` / core `OkHttpTransport` | HTTP 传输 | `reqwest` **0.12**（本 crate `Cargo.toml`，`default-features = false`，features `json`/`stream`/`rustls-tls`）+ 自建 `HttpTransport` trait seam | 生产 `ReqwestTransport`，测试 `MemoryTransport`（零真实 HTTP） |
| `com.anthropic.client.okhttp.AnthropicOkHttpClient`（Anthropic 官方 SDK） | Anthropic 传输 | 无 | 随 anthropic 子模块一并**待办** |
| reactor-core (`Flux`/`Mono`/`Schedulers`) | 响应式流 | `futures` (`BoxStream`) + `async-stream` **0.3**（本 crate） | core `Model::stream` 返回 `BoxStream` |
| slf4j (`Logger`/`LoggerFactory`) | 日志 | `tracing`（workspace） | |
| `java.util.UUID` | 合成工具调用 id / 请求 id | `uuid` **1**（本 crate，features `v4`, `serde`） | 与 Java 行为一致 |
| `java.util.Base64` | 媒体内联编码 | `base64` **0.22**（本 crate） | |
| `java.net.URL` / `URLEncoder` / `URI` | URL 构造与转义 | `reqwest`/`url` 内置 | 受控子集 |
| JUnit 5 / Mockito | 测试 | `#[test]` / `#[tokio::test]` + `MemoryTransport` fake | 离线，不新增 HTTP mock crate |
| Spring Boot AutoConfiguration | DI / 自动装配 | 不适用 | **设计决定（不迁移）**（本模块 Java 侧亦无 Spring 依赖，`pom.xml` 仅 core/jackson/reactor/junit） |
| `agentscope-core` / `agentscope-harness` | 内部依赖 | path 依赖 `../agentscope-core`、`../agentscope-harness` | ⚠️ `agentscope-harness` 在本 crate 源码中**零引用**（见 §3.3 P2） |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 接线 | 全模块 | 部分完成 | `src/lib.rs:20-21` 仅 `pub mod common; pub mod ollama;`；crate 级文档注释已写明与 core 的分工 |
| `Model` trait 复用（不自建平行 trait） | `io.agentscope.core.model.Model` | 已完成 | `ollama::model::OllamaChatModel` 实现 `agentscope_core::Model`（`crates/agentscope-core/src/model/model.rs:12`）；未在 extensions 侧新建 trait |
| `ModelProvider` / `ModelRegistry` 复用 | `io.agentscope.core.model.spi.ModelProvider` + `ModelRegistry` | 已完成 | `ollama::provider::OllamaModelProvider` 实现 core `ModelProvider`（`crates/agentscope-core/src/model/provider.rs:78`）；注册表为 core `ModelRegistry`（同文件 `:95`）。Java 的 SPI 文件机制不迁移（**设计决定**） |
| 离线可测的 HTTP seam | `OllamaHttpClient` / `DashScopeHttpClient` / `OpenAIClient` | 已完成 | `src/common/transport.rs`：`HttpTransport` trait + `ReqwestTransport` + `MemoryTransport`；全部 25 个单测零真实网络 |
| Ollama provider 全链路（格式化→请求→解析→流式） | `OllamaChatModel`/`OllamaHttpClient`/`OllamaModelProvider` + `formatter/*`（7 类） | 已完成 | `build_request`/`to_ollama_messages`/`to_ollama_tools`/`parse_response`/`OllamaStreamIncrementer`；端点 `/api/chat` |
| Ollama DTO serde 对齐 | `ollama.dto.*`（9 类，含 embedding） | 已完成 | `src/ollama/dto.rs`；embedding DTO 已迁但无调用方（见 §3.2） |
| 离线单测 | Java 测试 45043 行 | 已完成（部分覆盖） | 25 个：`common/transport.rs` 3、`ollama/converter.rs` 7、`ollama/parser.rs` 5、`ollama/model.rs` 4、`ollama/options.rs` 3、`ollama/provider.rs` 3 |
| **`dashscope` 接线缺陷** | `dashscope.DashScopeChatModel` 等 | **待办（阻塞项）** | `src/dashscope/mod.rs:8-12` 声明 `pub mod converter/model/parser/provider;` 并 `:14-25` `pub use` 其符号，但 `src/dashscope/` 下**只有 `dto.rs` 与 `mod.rs`**——四个模块文件缺失；且 `src/lib.rs` **未声明** `pub mod dashscope;`（故缺陷被掩盖、不影响当前 build）。**裁定：以源码为准补齐真实实现后再接线；禁止凭空造空模块凑数，禁止在跟踪器声称已完成** |
| dashscope DTO 接线 | `dashscope.dto.*`（15 类） | 部分完成 | `src/dashscope/dto.rs` 15 个 pub 类型（14 个对应 Java + Rust 专属 `DashScopeContent` untagged enum）；缺 `DashScopePublicKeyResponse`；文件未被编译（见上一行） |
| anthropic provider 基线 | `anthropic.*`（11 类） | 待办 | 无任何 Rust 对应文件 |
| gemini provider 基线 | `gemini.*`（10 类） | 待办 | 无任何 Rust 对应文件 |
| OpenAI provider | `openai.*`（53 类） | **设计决定（不迁移）** | core 已提供 `agentscope_core::model::openai`（`OpenAiChatModel`/`OpenAiProvider`/`default_registry`），依 `EXTENSIONS_GAP_TRACKER.md` §2.2 裁定本 crate 不重复实现 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama embedding 调用链 | `ollama.dto.OllamaEmbedding*` + `OllamaHttpClient` 的 `POST /api/embed` | 待办 | Rust 仅迁移 DTO 与常量 `ollama::model::EMBED_ENDPOINT`（`/api/embed`），**无客户端方法、无调用方、无单测** |
| Ollama 会话合并 | `ollama.formatter.OllamaConversationMerger` | 待办 | 无 Rust 对应；影响多轮/多智能体长会话的消息合并语义 |
| Ollama 多智能体格式化 | `ollama.formatter.OllamaMultiAgentFormatter` | 待办 | 仅 `hint_text` + tool-result 文本抽取被覆盖；`<system-info>` 图片提升等未完整对齐 |
| Ollama 远程图片下载 | `ollama.formatter.OllamaMediaConverter` | 待办 | `DefaultImageResolver` 对 http(s) URL 返回错误，需注入自定义异步 resolver（doc 注释已标注 `MODEL__GAP_TRACKER.md §3 (P1)`）；未提供内置实现 |
| anthropic 子模块整体 | `anthropic.*`（11 类 / 2192 行） | 待办 | 含 cache control、thinking、media、multi-agent formatter；传输层需自建（Java 用官方 SDK） |
| gemini 子模块整体 | `gemini.*`（10 类 / 2294 行） | 待办 | 含 thought signature 持久化等 Java 侧专项测试 |
| dashscope 子模块整体（除 DTO） | `dashscope.*`（29 类 / 7453 行） | 待办 | 含 `DashScopeEncryptionUtils`、`EndpointType`、`DashScopeMultiModalTool`（93.5 KB，TTS/图像/视频） |
| OpenAI 兼容厂商差异化行为 | `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 待确认 | 当前仅靠 core 的 base_url + `context_windows.rs` 覆盖；Java 侧每厂商 `*Formatter`/`*MultiAgentFormatter` 的格式化差异**是否有功能影响尚未裁定**——需汇总方确认（见 §4.2） |
| OpenAI 多模态工具 | `openai.tool.OpenAIMultiModalTool` | 待确认 | core 无对应；归属 MODEL 还是 TOOL 类模块未裁定 |
| Java 测试用例逐条对照迁移 | Java 测试 45043 行（anthropic 4677 / dashscope 11767 / gemini 4113 / ollama 4464 / openai 12886 / e2e 7136） | 待办 | 当前 25 个 Rust 单测为**功能等价重写**（仅覆盖 ollama + transport），未逐条照搬 |
| 真实厂商端点联调 | `e2e-tests`（34 测试类） | 设计决定（不迁移） | 需要真实 API key；Rust 侧坚持离线策略，网络层已实现但未对真实端点回归 |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama llama.cpp loader 长尾参数 | `ollama.options.OllamaOptions`（44.5 KB） | 待办 | Rust 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`（源码 doc 已声明为 P2） |
| 重试 / 退避 / 超时可配置化 | — | 待办 | 当前无重试；超时未暴露为可配置项 |
| 指标 / 可观测性埋点 | slf4j 日志 | 待办 | `tracing` 依赖已声明但本 crate 代码未埋点 |
| `agentscope-harness` 依赖清理 | — | 待办 | `Cargo.toml` 声明了 `agentscope-harness`，但 `src/**` **零引用**；建议后续确认是否移除（本回填为文档任务，未改代码） |
| 独立 `tests/` 集成测试 | — | 待办 | 当前 25 个单测全部内联在各 `src` 文件 `#[cfg(test)]` 中 |

### 3.4 设计决定（不迁移）汇总
| 项 | Java 参考 | 理由 |
|---|---|---|
| OpenAI 全子模块（53 类） | `openai.OpenAIChatModel` / `OpenAIClient` / `dto.*` / `formatter.*` / `compat.*` | core `agentscope_core::model::openai` 已用 `async-openai` 提供等价能力；`EXTENSIONS_GAP_TRACKER.md` §2.2 已裁定「core 定义 trait + 注册表，本 crate 补 provider 适配，不重复实现 openai」 |
| OpenAI 异常类层次（8 类） | `openai.exception.*` | Rust 以 `agentscope_core::Error`（`thiserror` 枚举）统一表达，不建平行异常继承体系 |
| SPI `META-INF/services` 注册机制 | 各子模块 `resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider` | Rust 无 `ServiceLoader`；改为显式 `ModelRegistry::register(Arc<dyn ModelProvider>)` |
| Spring 自动装配 / DI | 本模块 `pom.xml` 无 Spring 依赖 | 不适用；Rust 侧由调用方显式构造 provider |
| Ollama 凭据类型 | `ollama.credential.OllamaCredential` | 本地 Ollama 无鉴权头；凭据经 `ModelCreationContext.api_key`/环境变量表达，不引入平行类型 |
| e2e-tests 真实网络回归（34 类） | `agentscope-extensions-model-e2e-tests` | 需真实 API key；改为 `HttpTransport` seam + `MemoryTransport` 离线策略 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**
  1. **`common` 层**：`HttpTransport` trait（`post_json` / `post_json_stream`）+ `HttpRequestSpec` + `ReqwestTransport`（真实 reqwest 0.12）+ `MemoryTransport`（离线 fake）+ `decode_stream_line`（NDJSON / SSE `data:` 统一解码）。这是全 crate 的离线可测边界。
  2. **`ollama` 子模块**：`OllamaChatModel`（实现 core `Model`，端点 `/api/chat`，流式与非流式）、`OllamaModelProvider`（实现 core `ModelProvider`，`ollama:` 前缀 + 裸 id 兜底 + `OLLAMA_BASE_URL`）、9 个 DTO、`OllamaOptions`/`ThinkOption`、消息/工具转换（`to_ollama_messages[_with]`/`to_ollama_tools`/`to_tool_choice`）、响应解析与流式增量器。
  3. **`dashscope` 子模块**：仅 DTO 层（`src/dashscope/dto.rs`，15 个 pub 类型，含多态 content 的 `#[serde(untagged)]` 建模）——**未接线、未编译、无测试**。
- **验证命令与结果**（基线由汇总方实测，本次回填复跑 build 确认）
  - `cargo build -p agentscope-extensions-model` → **Finished**（0 error；本 crate 无 error，输出中的 warning 来自其它 crate 的既有代码）
  - `cargo test -p agentscope-extensions-model` → **25 passed / 0 failed / 0 ignored**
  - `cargo test --workspace` → **1485 passed / 0 failed**
- **已知问题**
  1. **`dashscope` 接线缺陷**（§3.1）：`src/dashscope/mod.rs` 引用四个不存在的子模块文件，且 `lib.rs` 未声明该模块——因此缺陷被「未编译」掩盖。须先补齐实现再接线，不得以空模块或删除声明草率收场。
  2. `src/ollama/dto.rs` 的 embedding DTO 与 `EMBED_ENDPOINT` 常量无调用方（§3.2）。
  3. `agentscope-harness` 为未使用依赖（§3.3）。
  4. 无阻塞：build/test 全绿。

### 4.2 下一步与阻塞

1. **【本 crate 内，最高优先】修复 `dashscope` 接线**：`src/dashscope/mod.rs` 已声明 `converter`/`model`/`parser`/`provider` 四模块但文件缺失。建议顺序：(a) 先补 `dashscope::converter`（`Msg`/`ToolSchema` → DashScope DTO，对应 Java `DashScopeMessageConverter`/`DashScopeToolsHelper`/`DashScopeChatFormatter`）与 `dashscope::parser`（响应 + SSE 增量，对应 `DashScopeResponseParser`）；(b) 再补 `dashscope::model`（`impl agentscope_core::Model`，经 `common::HttpTransport` 调 DashScope 端点，对应 `DashScopeChatModel` + `DashScopeHttpClient`）；(c) 最后补 `dashscope::provider`（对应 `DashScopeModelProvider`）；(d) 在 `lib.rs` 加 `pub mod dashscope;` 并 re-export；(e) 配离线单测。**无需改 core/harness。**
2. **【本 crate 内】补齐 P1 provider**：anthropic、gemini 两个子模块尚无任何 Rust 代码。实施前建议先裁定传输层方案（见下条），并按「DTO → converter → parser → model → provider → 离线单测」的既有 ollama 模式推进。
3. **【跨模块裁定请求，阻塞 P1 起点】anthropic / gemini 的传输与 SDK 选型**：Java 侧 anthropic 使用官方 SDK `com.anthropic.client.okhttp.AnthropicOkHttpClient`，而 core 只提供 OpenAI 兼容实现。需汇总方裁定：(a) 引入第三方 crate（如 `anthropic-sdk`/`google-generativeai` 类，需评估维护度与 workspace 版本策略）；(b) 用本 crate 既有 `HttpTransport` + `reqwest` 手写 wire 协议（与 ollama 一致，可完全离线测试，推荐）；(c) 交由 core 统一实现。**若选 (a) 且需写入根 `Cargo.toml` 的 `[workspace.dependencies]`，属跨模块变更，须由汇总方处理——本 crate 不得改根 `Cargo.toml`。**
4. **【跨模块裁定请求】OpenAI 兼容厂商（deepseek/glm/kimi/minimax）差异化行为**：Java 为每厂商提供独立 `*ModelProvider`（SPI 注册）+ `*Formatter`/`*MultiAgentFormatter`；Rust 侧 core 仅以 `base_url` 覆盖 + `context_windows.rs` 窗口表覆盖。需确认：(a) 这些厂商的格式化差异是否影响功能正确性（尤其 kimi/glm 的 multi-agent 格式化与 `GLMFormatter` 的 thinking 处理）；(b) 若需要，应在 core 扩展兼容表还是在本 crate 增补 vendor formatter。**core 侧改动本 crate 不得执行。**
5. **【待确认，非阻塞】`openai.tool.OpenAIMultiModalTool` 的归属**：core 无对应，需裁定归 MODEL 模块还是工具类模块。
6. **【非阻塞】离线测试补强**：为 dashscope DTO 增加 serde 往返测试（一旦接线）；为 ollama converter 补充 tool-result 图片提升与 `OllamaToolsHelper` 对齐用例。
7. **【非阻塞】依赖瘦身**：确认后可移除 `Cargo.toml` 中未使用的 `agentscope-harness`。

> **跨模块需求汇总**：以上第 3、4 条涉及**是否需要修改 `crates/agentscope-core`（及极端情况下根 `Cargo.toml` 版本策略）**。本 crate 当前**不需要**任何 core/harness 改动即可保持 build/test 全绿；相关请求仅影响后续 anthropic/gemini/openai-compat 的落地方式，请汇总方裁定后再实施。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-model` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-model` / `cargo test -p agentscope-extensions-model` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-model/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

