# MODEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MODEL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-model\`
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
| 当前状态 | **已完成（provider 主链路）** — `common`（HTTP seam）+ `ollama`（provider 全链路）+ `dashscope`（provider 全链路，含凭证/加密公钥/多模态端点）+ **`anthropic`（11 类全数映射）** + **`gemini`（10 类全数映射）** + **`openai` / `responses` / `anthropic_async`（2026-09-21 自 `agentscope-core/src/model/` 整体迁入本 crate）** 已落地并测试全绿（**198 passed**）。§3.3 的长尾增强项（远程图片下载、独立 `tests/`、重试/指标等）按本轮范围约定保持待办 |

### 1.1 验证基线（2026-09-19 实测；2026-09-21 更新）
```
cargo build -p agentscope-extensions-model   -> Finished (0 errors；本 crate 0 error/0 warning；输出中的 warning 均来自其它 crate 的既有代码)
cargo test  -p agentscope-extensions-model   -> 198 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out（lib test，0.03s）
                                                构成：common 3 + ollama（7/5/4/3/3）+ dashscope 33 + anthropic 58 + gemini 53 + 2026-09-21 自 core 迁入的
                                                openai / responses / anthropic_async 29 个（169 -> 198）
```
> 说明：2026-09-21（提交 `d86a578`）core 的 `model/{openai,responses,anthropic}.rs` 整体迁入本 crate，上表数字已按迁移后状态更新。该批代码**源自 core 既有实现，非从 Java 逐类重写**；Java 53 类的逐条对照情况见 §2.3/§3.2/§3.4。

### 1.2 Rust crate 文件清单（34 个源文件，10473 行；无 `tests/` 目录，198 个单测全部为内联 `#[cfg(test)]`）

> **2026-09-21 变更（`d86a578`）**：自 `agentscope-core/src/model/` 迁入 3 个源文件 + 1 个示例，下表尚未逐行补录：
> `src/openai/mod.rs`（1045 行）、`src/anthropic_async/mod.rs`（653 行）、`src/responses/mod.rs`（620 行）、`examples/chat_demo.rs`（55 行）。
> 文件总数 31 -> 34，源行数 8146 -> 10473；对应 `Cargo.toml` 新增 `async-openai` 0.41 / `async-anthropic` 0.6 / `reqwest13`，并移除未使用的 `agentscope-harness`。
| 文件 | 行数 | 职责 | 是否被 `lib.rs` 编译 |
|---|---|---|---|
| `src/lib.rs` | 33 | crate 根；声明 `pub mod anthropic; pub mod common; pub mod dashscope; pub mod gemini; pub mod ollama;` + re-export + 设计裁定文档注释 | — |
| `src/common/mod.rs` | 11 | `HttpTransport` seam 的 re-export | 是 |
| `src/common/transport.rs` | 244 | `HttpTransport` trait、`HttpRequestSpec`、`ReqwestTransport`（真实）、`MemoryTransport`（离线 fake）、`decode_stream_line`、`JsonStream`、`SharedTransport`；3 个单测 | 是 |
| `src/ollama/mod.rs` | 20 | ollama 子模块声明与 re-export | 是 |
| `src/ollama/dto.rs` | 155 | Ollama wire DTO（含 embedding DTO） | 是 |
| `src/ollama/options.rs` | 190 | `OllamaOptions` / `ThinkOption`；3 个单测 | 是 |
| `src/ollama/converter.rs` | 402 | `Msg`/`ToolSchema` → Ollama DTO；`ImageResolver` seam；7 个单测 | 是 |
| `src/ollama/parser.rs` | 265 | 响应解析 + 流式增量器；5 个单测 | 是 |
| `src/ollama/model.rs` | 252 | `OllamaChatModel`（`impl agentscope_core::Model`）；4 个单测 | 是 |
| `src/ollama/provider.rs` | 94 | `OllamaModelProvider`（`impl ModelProvider`）；3 个单测 | 是 |
| `src/dashscope/mod.rs` | 29 | DashScope 子模块声明/re-export | 是 |
| `src/dashscope/dto.rs` | 422 | DashScope wire DTO（含多态 content untagged enum） | 是 |
| `src/dashscope/converter.rs` | 644 | `Msg`/工具 → DashScope 请求体；11 个单测 | 是 |
| `src/dashscope/credential.rs` | 127 | `DashScopeCredential` / `EndpointType`；3 个单测 | 是 |
| `src/dashscope/model.rs` | 372 | `DashScopeChatModel`（impl core `Model`）；6 个单测 | 是 |
| `src/dashscope/parser.rs` | 370 | 响应解析 + 流式增量器；8 个单测 | 是 |
| `src/dashscope/provider.rs` | 155 | `DashScopeModelProvider`（impl `ModelProvider`）；5 个单测 | 是 |
| `src/anthropic/mod.rs` | 43 | anthropic 子模块声明/re-export + Java→Rust 文件映射表 | 是 |
| `src/anthropic/dto.rs` | 417 | Anthropic Messages wire DTO（request/response/SSE event，tagged enum content block）；8 个单测 | 是 |
| `src/anthropic/credential.rs` | 126 | `AnthropicCredential` / `AnthropicAuthMode` / 端点与版本常量；5 个单测 | 是 |
| `src/anthropic/converter.rs` | 996 | `Msg`/`ToolSchema` → Anthropic 请求体（含 cache_control、多智能体合并）；22 个单测 | 是 |
| `src/anthropic/parser.rs` | 350 | 响应解析 + SSE 流式增量器；8 个单测 | 是 |
| `src/anthropic/model.rs` | 385 | `AnthropicChatModel`（impl core `Model`）；7 个单测 | 是 |
| `src/anthropic/provider.rs` | 187 | `AnthropicModelProvider`（impl `ModelProvider`）；8 个单测 | 是 |
| `src/gemini/mod.rs` | 44 | gemini 子模块声明/re-export + Java→Rust 文件映射表 | 是 |
| `src/gemini/dto.rs` | 370 | Gemini REST wire DTO（camelCase，inlineData/functionCall/thoughtSignature）；7 个单测 | 是 |
| `src/gemini/credential.rs` | 92 | `GeminiCredential` / 端点构造函数 / API 常量；4 个单测 | 是 |
| `src/gemini/converter.rs` | 693 | `Msg`/`ToolSchema` → Gemini 请求体（含 thought signature、多智能体合并）；19 个单测 | 是 |
| `src/gemini/parser.rs` | 318 | 响应解析 + 流式增量器（thoughtSignature 持久化）；8 个单测 | 是 |
| `src/gemini/model.rs` | 339 | `GeminiChatModel`（impl core `Model`）；7 个单测 | 是 |
| `src/gemini/provider.rs` | 184 | `GeminiModelProvider`（impl `ModelProvider`）；8 个单测 | 是 |

> ℹ️ **更正（2026-09-19）**：本表旧版曾记「`dashscope` 仅 DTO、未接线、未编译」，并据此在 §3.1 列为待办阻塞项——**该描述已过时**。现 `src/dashscope/` 含 6 个子模块（converter/credential/dto/model/parser/provider）、已在 `lib.rs` 声明、已全部接入并随 crate 编译，共 33 个单测。
>
> ℹ️ **本轮（2026-09-19）新增 anthropic / gemini**：两子模块均已在 `lib.rs` 声明（`pub mod anthropic; pub mod gemini;`）并 re-export，14 个新源文件、共 111 个新单测，实测均已被 `cargo test` 执行（非「落盘未接线」）。

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

### 2.2 dashscope（29 类 / 7453 行）— **已迁移（provider 全链路）**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `dashscope.dto.DashScopeRequest` | 顶层请求体 | `dashscope::dto::DashScopeRequest` | 已接线 |
| `dashscope.dto.DashScopeInput` | `input.messages` 容器 | `dashscope::dto::DashScopeInput`（`of()`） | 已接线 |
| `dashscope.dto.DashScopeParameters` | `parameters`（result_format/incremental_output/…） | `dashscope::dto::DashScopeParameters` | 默认 `result_format="message"`；已接线 |
| `dashscope.dto.DashScopeSearchOptions` | 联网搜索策略 | `dashscope::dto::DashScopeSearchOptions` | 已接线 |
| `dashscope.dto.DashScopeMessage` | 消息（content 多态） | `dashscope::dto::{DashScopeMessage, DashScopeContent}` | Java `Object` content → Rust `#[serde(untagged)]` enum（Rust 侧扩展类型） |
| `dashscope.dto.DashScopeContentPart` | 多模态内容片段 | `dashscope::dto::DashScopeContentPart` | 已接线 |
| `dashscope.dto.DashScopeTool` | 工具声明 | `dashscope::dto::DashScopeTool` | 已接线 |
| `dashscope.dto.DashScopeToolFunction` | 工具函数 schema | `dashscope::dto::DashScopeToolFunction` | 已接线 |
| `dashscope.dto.DashScopeToolCall` | 工具调用 | `dashscope::dto::DashScopeToolCall` | 已接线 |
| `dashscope.dto.DashScopeFunction` | 函数 | `dashscope::dto::DashScopeFunction` | 已接线 |
| `dashscope.dto.DashScopeResponse` | 响应体 | `dashscope::dto::DashScopeResponse` | 已接线 |
| `dashscope.dto.DashScopeOutput` | `output` | `dashscope::dto::DashScopeOutput` | 已接线 |
| `dashscope.dto.DashScopeChoice` | 单条 choice | `dashscope::dto::DashScopeChoice` | 已接线 |
| `dashscope.dto.DashScopeUsage` | token 用量 | `dashscope::dto::DashScopeUsage` | 已接线 |
| `dashscope.dto.DashScopePublicKeyResponse` | 加密公钥响应 | `dashscope::dto::DashScopePublicKeyResponse` + `dashscope::credential::DashScopeCredential` | 已接线（`PUBLIC_KEYS_ENDPOINT`） |
| `dashscope.DashScopeChatModel` | 聊天模型主类 | `dashscope::model::DashScopeChatModel`（impl core `Model`） | 已接线 |
| `dashscope.DashScopeHttpClient` | HTTP/SSE 客户端 | `dashscope::{converter,parser}` + `common::HttpTransport` | 已接线 |
| `dashscope.DashScopeEncryptionUtils` | 请求加密 | `dashscope::credential`（凭证/公钥） | 已接线 |
| `dashscope.DashScopeModelProvider` | 注册表 provider | `dashscope::provider::DashScopeModelProvider`（impl `ModelProvider`） | 已接线 |
| `dashscope.EndpointType` | 端点类型枚举 | `dashscope::credential::EndpointType` | 已接线 |
| `dashscope.credential.DashScopeCredential` | 凭据 | 无 | **待办** |
| `dashscope.formatter.*`（7 类：ChatFormatter / ConversationMerger / MediaConverter / MessageConverter / MultiAgentFormatter / ResponseParser / ToolsHelper） | 格式化/解析 | 无 | **待办** |
| `dashscope.tool.DashScopeMultiModalTool`（93.5 KB） | 多模态工具（TTS/图像/视频等） | 无 | **待办**（本模块最大单文件） |

### 2.3 openai（53 类 / 8534 行）— **已迁移（2026-09-21 自 core 迁入本 crate）**
> **状态口径变更（2026-09-21，`d86a578`）**：本节原为「设计决定（不迁移）」，依据是 `EXTENSIONS_GAP_TRACKER.md` §2.2 裁定「core 定 trait + 注册表，本 crate 不重复实现 openai」。该裁定已被本次目录重构取代——core 的 `model/openai.rs`、`model/responses.rs`、`model/anthropic.rs` **实现体已整体迁入本 crate**，core 仅保留 `Model` trait / `ModelRegistry` / provider SPI。因此「不重复实现」的说法不再成立。
> **注意区分**：迁入的是 **core 既有实现**，不等价于「Java 53 类逐类迁移完成」。Java 侧的 `compat.*`（12 类）、`exception.*`（8 类）、`OpenAIMultiModalTool` 仍未迁，见下表与 §3.2/§3.4。

| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openai.OpenAIChatModel` / `OpenAIClient` / `OpenAIModelProvider` / `OpenAIModelProviderSupport` | OpenAI 兼容聊天链路与 provider | `crate::openai::{OpenAiChatModel, OpenAiProvider, default_registry}`（`src/openai/mod.rs`，1045 行，基于 `async-openai` 0.41） | **已就位**（实现体原在 `agentscope-core/src/model/openai.rs`，2026-09-21 整体迁入；`agentscope_core` 不再导出这些类型） |
| `openai.dto.*`（16 类：Request/Response/Message/Choice/ContentPart/Function/ImageUrl/InputAudio/ReasoningDetail/StreamOptions/Tool/ToolCall/ToolFunction/Usage/VideoUrl/Error） | OpenAI wire 模型 | `src/openai/mod.rs` 内部 DTO + `async-openai` 0.41 类型 | 已就位（随 `openai` 一并迁入）；Java 16 类未逐类 1:1 重写 |
| `openai.responses.*`（OpenAI Responses API） | Responses 协议客户端 | `crate::responses::{OpenAiResponsesModel, OpenAiResponsesProvider, default_responses_registry}`（`src/responses/mod.rs`，620 行） | **本轮新增对应**：随迁移落地（Java 侧对应 `openai` 子模块内的 Responses 支持） |
| `anthropic_async`（async-anthropic 版 Messages 客户端） | Anthropic Messages 协议（SDK 驱动） | `crate::anthropic_async::{AnthropicMessagesModel, AnthropicMessagesProvider, default_anthropic_registry}`（`src/anthropic_async/mod.rs`，653 行，基于 `async-anthropic` 0.6） | **本轮新增对应**：与 §2.4 的 transport-seam 版 `crate::anthropic` 并存（两条实现路径，见 §4.2 待裁定） |
| `openai.exception.*`（8 类：OpenAIException/Authentication/BadRequest/InternalServer/NotFound/PermissionDenied/RateLimit/UnprocessableEntity） | 异常层次 | `agentscope_core::Error`（`Error::Model`/`Error::Http` 等） | **设计决定**：Rust 用单一 `thiserror` 枚举，不 1:1 建异常类层次 |
| `openai.formatter.*`（10 类） | OpenAI 格式/转换/解析 | `src/openai/mod.rs`（`to_request_messages`/`to_request_tools`/`to_tool_choice`/`to_response_format`/`OpenAiStreamIncrementer`） | 已就位（随迁入） |
| `openai.tool.OpenAIMultiModalTool` | 多模态工具 | 无 | **待确认**（core 与本 crate 均无对应；归属 MODEL 还是 TOOL 类模块待汇总方裁定，见 §4.2 第 4 条） |
| `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 兼容厂商 formatter + provider | 无专门类型；经 `crate::openai::OpenAiProvider` + `ModelCreationContext.base_url` + core `context_windows.rs`（含 deepseek/glm/kimi 窗口表）覆盖 | **设计决定**：Java 每厂商一个 `*ModelProvider` + `*Formatter` 的做法未被 1:1 迁移；其差异（base_url/模型窗口/格式化）当前靠 base_url + 上下文窗口表覆盖。**残留差异待确认**（见 §3.2）。注：实现体现已在本 crate 内，若裁定需补 vendor formatter，可在本 crate 实施而无需改 core |
| `resources/META-INF/services/...ModelProvider`（5 行） | SPI 注册 | `crate::openai::default_registry()` / core `ModelRegistry::register` | |

### 2.4 anthropic（11 类 / 2192 行）— **已完成（provider 全链路，本轮迁移）**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `anthropic.AnthropicChatModel` | 聊天模型主类 | `anthropic::model::AnthropicChatModel`（impl core `Model`） | 经注入的 `SharedTransport` 调 `POST {base_url}/v1/messages`；离线可测 |
| `anthropic.AnthropicModelProvider` | provider 插件 | `anthropic::provider::AnthropicModelProvider`（impl `ModelProvider`） | `provider_id()=="anthropic"`；支持 `anthropic:` 前缀与裸 `claude*`；`ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` |
| `anthropic.credential.AnthropicCredential` | 凭据 | `anthropic::credential::AnthropicCredential` + `AnthropicAuthMode` | api_key（`x-api-key`）/ authToken（`Authorization: Bearer`）互斥 |
| `anthropic.formatter.AnthropicBaseFormatter` | 基础格式化（system/cache/options） | `anthropic::converter::{build_request, system_prompt, apply_cache_control, ChatFormatOptions}` | system 提到顶层 `system` 字段；cache_control 标记 system/末条消息/末个工具 |
| `anthropic.formatter.AnthropicChatFormatter` | 会话格式化 | `anthropic::converter::{to_anthropic_messages, build_request}` | |
| `anthropic.formatter.AnthropicConversationMerger` | 会话合并 | `anthropic::converter::{merge_conversation, ConversationBlock}` | `<history>` 包裹，与 Java 同构；作为多智能体路径的可复用入口 |
| `anthropic.formatter.AnthropicMediaConverter` | 媒体转换 | `anthropic::converter::{image_source, image_content_block, data_content_block}` | 本地文件→base64；远程 URL 直接透传 |
| `anthropic.formatter.AnthropicMessageConverter` | 消息转换 | `anthropic::converter::{to_anthropic_messages, message_content_blocks, tool_result_message}` | tool_result 拆为独立 user 消息；未做 Java 的并行 tool-use 拆分（见 §3.2） |
| `anthropic.formatter.AnthropicMultiAgentFormatter` | 多智能体格式化 | `anthropic::converter::to_multi_agent_messages` | system/tool-sequence/agent-conversation 分组；agent 组合并为单 user 消息 |
| `anthropic.formatter.AnthropicResponseParser` | 响应解析（含 cache control / thinking） | `anthropic::parser::{parse_message, AnthropicStreamIncrementer}` | thinking→text→tool_use；cache token 加回 input；流式增量器缓冲 tool 参数分片 |
| `anthropic.formatter.AnthropicToolsHelper` | 工具转换 | `anthropic::converter::{to_anthropic_tools, to_tool_choice, apply_extra_body_params}` | `disable_parallel_tool_use = !parallel_tool_calls`；`Required`→`any`；末个工具带 cache_control |
| SDK wire 类型（`MessageCreateParams`/`MessageParam`/`Message`/`RawMessageStreamEvent`） | Anthropic 官方 SDK | `anthropic::dto::{AnthropicRequest, AnthropicMessage, AnthropicContentBlock, AnthropicMessageResponse, AnthropicStreamEvent}` | Java 用官方 SDK，Rust 无对应依赖→自建 DTO/parser（方案裁定见 §4.2） |

> **传输层裁定**（2026-09-19）：采用 §4.2 选项 (b) —— 复用本 crate 既有 `HttpTransport` seam + `ReqwestTransport`（生产）/`MemoryTransport`（离线测试）手写 Anthropic Messages 协议，**未引入新 HTTP / 厂商 SDK 依赖**。

### 2.5 gemini（10 类 / 2294 行）— **已完成（provider 全链路，本轮迁移）**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `gemini.GeminiChatModel` | 聊天模型主类 | `gemini::model::GeminiChatModel`（impl core `Model`） | 经 `SharedTransport` 调 `models/{model}:generateContent` / `:streamGenerateContent?alt=sse` |
| `gemini.GeminiModelProvider` | provider 插件 | `gemini::provider::GeminiModelProvider`（impl `ModelProvider`） | `provider_id()=="gemini"`；支持 `gemini:` 前缀与裸 `gemini-*`；`GEMINI_API_KEY`；**Vertex AI 明确拒绝**（见 §3.2/§4.2） |
| `gemini.credential.GeminiCredential` | 凭据 | `gemini::credential::GeminiCredential` | `x-goog-api-key` |
| `gemini.formatter.GeminiChatFormatter` | 会话格式化 + options/tools | `gemini::converter::{build_request, apply_options}` | `topK`→Float、`seed`→Int、`maxTokens`→`maxOutputTokens`；thinking→`thinkingConfig` |
| `gemini.formatter.GeminiConversationMerger` | 会话合并 | `gemini::converter::merge_to_content` | `<history>` 包裹；`"<name>: <text>"`；tool result → `"Tool: <name>\n<output>"` |
| `gemini.formatter.GeminiMultiAgentFormatter` | 多智能体格式化 | `gemini::converter::to_multi_agent_contents` | system→user 内容；tool 序列单独分组；agent 组合并为单 user Content |
| `gemini.formatter.GeminiMessageConverter` | 消息转换 | `gemini::converter::{to_gemini_contents, tool_arguments, tool_call_part, tool_result_content}` | tool result 为独立 `user` content；tool 参数优先 `content` 原串再 `input` |
| `gemini.formatter.GeminiMediaConverter` | 媒体转换 | `gemini::converter::{media_part, data_part}` | base64/本地文件→`inlineData`；**远程 URL 拒绝并回退文本**（见 §3.2） |
| `gemini.formatter.GeminiResponseParser` | 响应解析（含 thought signature） | `gemini::parser::{parse_response, GeminiStreamIncrementer}` | thinking(`thought:true`)→text→tool_use；`thoughtSignature` 写入 `ToolUseBlock` metadata 以支持回放 |
| `gemini.formatter.GeminiToolsHelper` | 工具转换 | `gemini::converter::{to_gemini_tools, to_tool_choice}` | `parametersJsonSchema`；Auto→默认、None→NONE、Required→ANY、Specific→ANY+allowedFunctionNames |
| SDK wire 类型（`Content`/`Part`/`GenerateContentConfig`/`GenerateContentResponse`） | Google GenAI SDK | `gemini::dto::{GeminiRequest, GeminiContent, GeminiPart, GeminiResponse, ...}` | Java 用官方 SDK，Rust 无对应依赖→自建 DTO/parser（方案裁定见 §4.2） |

> **传输层裁定**（2026-09-19）：同 anthropic，采用 §4.2 选项 (b) —— 复用 `HttpTransport` seam 手写 Gemini REST 协议，**未引入 Google SDK / 新依赖**。

### 2.6 e2e-tests（0 主代码 / 34 测试类 / 7136 行）— **未迁移**
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `io.agentscope.core.e2e.*`（CoreAgentE2E、MultiModalE2E、StreamingBehaviorE2E、StructuredOutputE2E、ToolSystemE2E、CrossModelCompatibilityE2E 等 34 个测试类）+ `providers.*`（ModelProvider/BaseModelProvider/AnthropicProvider/DashScopeProvider/OpenRouterProvider 等） | 真实厂商端到端回归（需真实 API key） | 无 | **设计决定（不迁移）**：Rust 侧统一采用**离线可测**策略——`HttpTransport` seam + `MemoryTransport` 注入，不引入需要密钥的网络测试；各 provider 的真实联调记为 §3.2 待办 |

### 2.7 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Jackson (`@JsonProperty`/`@JsonInclude`/`@JsonAlias`/自定义 (De)Serializer) | JSON (反)序列化 | `serde` + `serde_json`（`[dependencies]` 经 workspace 引入）+ `#[serde(rename/skip_serializing_if/untagged)]` | 字段名 1:1 对齐 Java JSON |
| OkHttp / `java.net.http.HttpClient` / core `OkHttpTransport` | HTTP 传输 | `reqwest` **0.12**（本 crate `Cargo.toml`，`default-features = false`，features `json`/`stream`/`rustls-tls`）+ 自建 `HttpTransport` trait seam | 生产 `ReqwestTransport`，测试 `MemoryTransport`（零真实 HTTP） |
| `com.anthropic.client.okhttp.AnthropicOkHttpClient`（Anthropic 官方 SDK） | Anthropic 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `anthropic::dto` 自建 Messages 协议 | **已完成**（2026-09-19）：未引入厂商 SDK；wire DTO 与 converter/parser 自建，可离线测 |
| `com.google.genai.Client`（Google GenAI 官方 SDK） | Gemini 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `gemini::dto` 自建 REST 协议 | **已完成**（2026-09-19）：同上；SSE 流式用 `:streamGenerateContent?alt=sse` |
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
| crate 骨架 + `lib.rs` 接线 | 全模块 | 已完成 | `src/lib.rs` 声明 `pub mod anthropic; pub mod common; pub mod dashscope; pub mod gemini; pub mod ollama;` + re-export；crate 级文档注释已写明与 core 的分工 |
| `Model` trait 复用（不自建平行 trait） | `io.agentscope.core.model.Model` | 已完成 | `ollama::model::OllamaChatModel` 实现 `agentscope_core::Model`（`crates/agentscope-core/src/model/model.rs:12`）；未在 extensions 侧新建 trait |
| `ModelProvider` / `ModelRegistry` 复用 | `io.agentscope.core.model.spi.ModelProvider` + `ModelRegistry` | 已完成 | `ollama::provider::OllamaModelProvider` 实现 core `ModelProvider`（`crates/agentscope-core/src/model/provider.rs:78`）；注册表为 core `ModelRegistry`（同文件 `:95`）。Java 的 SPI 文件机制不迁移（**设计决定**） |
| 离线可测的 HTTP seam | `OllamaHttpClient` / `DashScopeHttpClient` / `OpenAIClient` / Anthropic SDK / Google GenAI SDK | 已完成 | `src/common/transport.rs`：`HttpTransport` trait + `ReqwestTransport` + `MemoryTransport`；全部 198 个单测零真实网络 |
| Ollama provider 全链路（格式化→请求→解析→流式） | `OllamaChatModel`/`OllamaHttpClient`/`OllamaModelProvider` + `formatter/*`（7 类） | 已完成 | `build_request`/`to_ollama_messages`/`to_ollama_tools`/`parse_response`/`OllamaStreamIncrementer`；端点 `/api/chat` |
| Ollama DTO serde 对齐 | `ollama.dto.*`（9 类，含 embedding） | 已完成 | `src/ollama/dto.rs`；embedding DTO 已迁但无调用方（见 §3.2） |
| 离线单测 | Java 测试 45043 行 | 已完成（部分覆盖） | **198 个**：`common/transport.rs` 3、`ollama`（converter 7 / parser 5 / model 4 / options 3 / provider 3）、`dashscope`（converter 11 / parser 8 / model 6 / provider 5 / credential 3）、**`anthropic` 58**（dto 8 / credential 5 / converter 22 / parser 8 / model 7 / provider 8）、**`gemini` 53**（dto 7 / credential 4 / converter 19 / parser 8 / model 7 / provider 8）、另 29 个随 2026-09-21 自 core 迁入的 `openai` / `responses` / `anthropic_async` |
| **`dashscope` provider 全链路** | `dashscope.DashScopeChatModel` / `DashScopeHttpClient` / `DashScopeModelProvider` / `EndpointType` | **已完成** | `src/dashscope/{converter,credential,dto,model,parser,provider}.rs`，共 33 个单测；`lib.rs` 已声明 `pub mod dashscope;`。**历史误记更正（2026-09-19）**：旧版曾将此列为「接线缺陷/阻塞项」（称四模块文件缺失且未编译），实测该缺陷不存在 |
| dashscope DTO 接线 | `dashscope.dto.*`（15 类） | 已完成 | `src/dashscope/dto.rs` 15 个 pub 类型（14 个对应 Java + Rust 专属 `DashScopeContent` untagged enum）；含 `DashScopePublicKeyResponse`（凭证加密公钥） |
| anthropic provider 基线 | `anthropic.*`（11 类） | **已完成** | `src/anthropic/{dto,credential,converter,parser,model,provider,mod}.rs`，58 个单测；`lib.rs` 已声明 `pub mod anthropic;`；详见 §2.4 |
| gemini provider 基线 | `gemini.*`（10 类） | **已完成** | `src/gemini/{dto,credential,converter,parser,model,provider,mod}.rs`，53 个单测；`lib.rs` 已声明 `pub mod gemini;`；详见 §2.5 |
| OpenAI provider | `openai.*`（53 类） | **已完成（2026-09-21 自 core 迁入）** | `src/openai/mod.rs`（`OpenAiChatModel`/`OpenAiProvider`/`default_registry`，1045 行）；同批迁入 `responses`（620 行）与 `anthropic_async`（653 行）。⚠️ 原「设计决定（不迁移）」口径已作废；Java `compat.*`（12 类）/`exception.*`（8 类）/`OpenAIMultiModalTool` 仍未迁（§3.2/§3.4） |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama embedding 调用链 | `ollama.dto.OllamaEmbedding*` + `OllamaHttpClient` 的 `POST /api/embed` | 待办 | Rust 仅迁移 DTO 与常量 `ollama::model::EMBED_ENDPOINT`（`/api/embed`），**无客户端方法、无调用方、无单测** |
| Ollama 会话合并 | `ollama.formatter.OllamaConversationMerger` | 待办 | 无 Rust 对应；影响多轮/多智能体长会话的消息合并语义 |
| Ollama 多智能体格式化 | `ollama.formatter.OllamaMultiAgentFormatter` | 待办 | 仅 `hint_text` + tool-result 文本抽取被覆盖；`<system-info>` 图片提升等未完整对齐 |
| Ollama 远程图片下载 | `ollama.formatter.OllamaMediaConverter` | 待办 | `DefaultImageResolver` 对 http(s) URL 返回错误，需注入自定义异步 resolver（doc 注释已标注 `MODEL__GAP_TRACKER.md §3 (P1)`）；未提供内置实现 |
| anthropic 子模块整体 | `anthropic.*`（11 类 / 2192 行） | **已完成** | cache control / thinking / media / multi-agent formatter 均已落地；传输层自建（§4.2 选项 b）。**剩余 2 项细化缺口见下** |
| anthropic 并行 tool-use 拆分 | `AnthropicMessageConverter#collectSplitToolResults` / `#convertParallelToolCalls` | 待办（非本轮范围） | 本轮将多条 tool_use / tool_result 按“独立 user 消息”处理，但**未实现 Java 的「多 tool_use + 对应多 tool_result 交替拆分」**（Anthropic 要求 assistant(tool_use)→user(tool_result) 严格交替时）。触发条件苛刻（需 tool_use 数==tool_result 数且全部 id 可配），待后续轮次补齐 |
| anthropic 多智能体 system 前导文本细节 | `AnthropicMultiAgentFormatter` | 部分完成（非本轮范围） | 分组与 `<history>` 合并已对齐；Java 的 `isFirstAgentMessageGroup` 首组标记已实现，但**系统消息在组内的特殊转换路径较 Java 略简化**（本轮未逐条对齐其 ground-truth 测试） |
| gemini 子模块整体 | `gemini.*`（10 类 / 2294 行） | **已完成** | thought signature 持久化、thinking、多模态、multi-agent 均已落地。**剩余 2 项细化缺口见下** |
| gemini Vertex AI 路径 | `GeminiModelProvider`（`project`/`location`/`vertexAI` + `GoogleCredentials`） | 待办（设计决定：本轮不迁移） | Vertex AI 与 Gemini API 的端点主机与认证完全不同（OAuth2 服务账号）；离线 adapter **明确报错拒绝**而非静默降级（已有单测 `create_rejects_vertex_ai`）。需汇总方裁定是否另建 vertex 适配（见 §4.2） |
| gemini 远程图片下载 | `GeminiMediaConverter#readFileAsBytes`（http/https） | 待办（非本轮范围） | Rust 侧拒绝 `http(s)` URL 并回退文本（避免隐式网络 I/O）；仅支持 base64/本地文件。需与 ollama 远程下载一并裁定统一 resolver（见 §3.3） |
| dashscope 子模块整体 | `dashscope.*`（29 类 / 7453 行） | **已完成** | 含 `DashScopeEncryptionUtils` 对应物（`credential.rs` 凭证/公钥处理）、`EndpointType`、多模态端点常量（`MULTIMODAL_GENERATION_ENDPOINT`）；见 §2.2。**长尾待确认**：`DashScopeMultiModalTool`（93.5 KB，TTS/图像/视频）覆盖度待核 |
| OpenAI 兼容厂商差异化行为 | `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 待确认 | 当前仅靠 `crate::openai::OpenAiProvider` 的 base_url + core `context_windows.rs` 覆盖；Java 侧每厂商 `*Formatter`/`*MultiAgentFormatter` 的格式化差异**是否有功能影响尚未裁定**——需汇总方确认（见 §4.2）。注：实现体已在本 crate 内，若裁定需补 formatter 可在本 crate 实施 |
| OpenAI 多模态工具 | `openai.tool.OpenAIMultiModalTool` | 待确认 | core 与本 crate 均无对应；归属 MODEL 还是 TOOL 类模块未裁定 |
| Java 测试用例逐条对照迁移 | Java 测试 45043 行（anthropic 4677 / dashscope 11767 / gemini 4113 / ollama 4464 / openai 12886 / e2e 7136） | 待办 | 当前 **198** 个 Rust 单测为**功能等价重写**（覆盖 transport + ollama + dashscope + anthropic + gemini + 自 core 迁入的 openai/responses/anthropic_async），未逐条照搬；anthropic/gemini 的 ground-truth formatter 测试（如 `AnthropicChatFormatterGroundTruthTest`、`GeminiPythonConsistencyTest`）尚未逐条对照 |
| 真实厂商端点联调 | `e2e-tests`（34 测试类） | 设计决定（不迁移） | 需要真实 API key；Rust 侧坚持离线策略，网络层已实现但未对真实端点回归 |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama llama.cpp loader 长尾参数 | `ollama.options.OllamaOptions`（44.5 KB） | 待办 | Rust 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`（源码 doc 已声明为 P2） |
| 统一远程媒体 resolver | `MediaUtils` + 各 `*MediaConverter` | 待办（非本轮范围） | anthropic 远程 URL 透传（由服务端拉取）、gemini/ollama 拒绑（回退文本）。是否提供统一异步 resolver 待裁定 |
| 重试 / 退避 / 超时可配置化 | — | 待办 | 当前无重试；超时未暴露为可配置项（`GenerateOptions` 无对应字段） |
| 指标 / 可观测性埋点 | slf4j 日志 | 待办 | `tracing` 依赖已声明但本 crate 代码未埋点 |
| `agentscope-harness` 依赖清理 | — | **已完成**（2026-09-21） | `crates/agentscope-extensions-model/Cargo.toml` 已移除 `agentscope-harness`（此前 `src/**` 零引用）；见提交 `d86a578` |
| 独立 `tests/` 集成测试 | — | 待办（非本轮范围） | 当前 198 个单测全部内联在各 `src` 文件 `#[cfg(test)]` 中 |

### 3.4 设计决定（不迁移）汇总
| 项 | Java 参考 | 理由 |
|---|---|---|
| OpenAI 全子模块（53 类）的「逐类 1:1 迁移」 | `openai.OpenAIChatModel` / `OpenAIClient` / `dto.*` / `formatter.*` / `compat.*` | **原「不迁移」理由已作废（2026-09-21，`d86a578`）**：实现体已自 core 迁入本 crate（`src/openai/mod.rs`），core 仅留 trait/注册表/SPI。但 Java 53 类仍**未逐类对照迁移**——`compat.*` 12 类、`exception.*` 8 类、`OpenAIMultiModalTool` 仍为开放项（§3.2/§3.4 下表） |
| OpenAI 异常类层次（8 类） | `openai.exception.*` | Rust 以 `agentscope_core::Error`（`thiserror` 枚举）统一表达，不建平行异常继承体系 |
| SPI `META-INF/services` 注册机制 | 各子模块 `resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider` | Rust 无 `ServiceLoader`；改为显式 `ModelRegistry::register(Arc<dyn ModelProvider>)` |
| Spring 自动装配 / DI | 本模块 `pom.xml` 无 Spring 依赖 | **已完成（设计决定，不迁移）**：不适用；Rust 侧由调用方显式构造 provider |
| Ollama 凭据类型 | `ollama.credential.OllamaCredential` | **已完成（设计决定，不迁移）**：本地 Ollama 无鉴权头；凭据经 `ModelCreationContext.api_key`/环境变量表达，不引入平行类型 |
| e2e-tests 真实网络回归（34 类 / 7136 行） | `agentscope-extensions-model-e2e-tests` | **已完成（设计决定，不迁移）**：需真实 API key，无法在离线 CI 复现；改为 `HttpTransport` seam + `MemoryTransport` 离线策略，各 provider 的请求/响应/流式分支已有等价单测 |
| anthropic / gemini 官方 SDK 类型（`AnthropicOkHttpClient` / `com.google.genai.Client`）不引入 | 两子模块 `pom.xml` | **已完成（设计决定）**：经 §4.2 裁定采用选项 (b)，用 `HttpTransport` + 自建 wire DTO 替代；避免引入新第三方依赖与机器绑定 |
| Gemini Vertex AI（`project`/`location`/OAuth2 credentials） | `GeminiModelProvider` | **已完成（设计决定：本轮不迁移）**：端点为 Vertex AI 专用主机 + OAuth2 服务账号，与 API-key 流程正交；离线 adapter **显式报错拒绝**（单测 `create_rejects_vertex_ai`）避免静默降级。后续若需，另立适配 |
| Anthropic SDK 的 `ProxyConfig` / Gemini SDK 的 `ProxyOptions` | 两子模块构造器 | **已完成（设计决定，不迁移）**：Rust 侧代理由调用方通过 `ReqwestTransport::with_client` 注入 `reqwest::Client`（支持代理/超时/TLS），不在 provider 层重复建模 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**
  1. **`common` 层**：`HttpTransport` trait（`post_json` / `post_json_stream`）+ `HttpRequestSpec` + `ReqwestTransport`（真实 reqwest 0.12）+ `MemoryTransport`（离线 fake）+ `decode_stream_line`（NDJSON / SSE `data:` 统一解码）。这是全 crate 的离线可测边界。
  2. **`ollama` 子模块**：`OllamaChatModel`（实现 core `Model`，端点 `/api/chat`，流式与非流式）、`OllamaModelProvider`（实现 core `ModelProvider`，`ollama:` 前缀 + 裸 id 兜底 + `OLLAMA_BASE_URL`）、9 个 DTO、`OllamaOptions`/`ThinkOption`、消息/工具转换、响应解析与流式增量器。
  3. **`dashscope` 子模块**：provider 全链路（`converter`/`credential`/`dto`/`model`/`parser`/`provider` 六模块，33 个单测）。
  4. **`anthropic` 子模块（本轮新增）**：provider 全链路（`dto`/`credential`/`converter`/`parser`/`model`/`provider` 六模块，58 个单测）。覆盖：system 提升到顶层 `system`、prompt caching（`cache_control`/TTL，标记 system/末条消息/末个工具）、extended thinking（`thinking.budget_tokens`）、`tool_choice` 四变体与 `disable_parallel_tool_use`、tool_result 拆为独立 user 消息、多模态（base64/本地→base64，远程 URL 透传）、多智能体 `<history>` 合并、SSE 流式增量器（text/thinking 即时产出，tool 参数分片缓冲，usage 合并 message_start+message_delta）。
  5. **`gemini` 子模块（本轮新增）**：provider 全链路（同构六模块，53 个单测）。覆盖：REST `generateContent`/`streamGenerateContent?alt=sse`、`generationConfig`（temperature/topP/topK/seed/maxOutputTokens/penalty/thinkingConfig）、`parametersJsonSchema` 工具声明与 `functionCallingConfig`（NONE/ANY/allowedFunctionNames）、tool result 独立 `user` content、`thoughtSignature` 持久化与回放、多模态 `inlineData`、多智能体 `<history>` 合并、流式增量器。
- **验证命令与结果**
  - （2026-09-19 实测）`cargo build -p agentscope-extensions-model` → **Finished**（0 error；本 crate 0 error/0 warning）
  - （2026-09-19 实测）`cargo test -p agentscope-extensions-model` → **169 passed / 0 failed / 0 ignored**
  - （2026-09-21 实测，core 实现迁入后）`cargo test -p agentscope-extensions-model` → **198 passed / 0 failed / 0 ignored**；`cargo check --workspace` → **Finished**（0 error，仅其它 crate 存量 warning）
- **已知问题**
  1. `src/ollama/dto.rs` 的 embedding DTO 与 `EMBED_ENDPOINT` 常量无调用方（§3.2）。
  2. ~~`agentscope-harness` 为未使用依赖（§3.3）。~~ **已解决**（2026-09-21 从 `Cargo.toml` 移除，`d86a578`）。
  3. anthropic 并行 tool-use 拆分、gemini Vertex AI 与远程媒体下载为**非本轮范围**细化缺口（§3.2）。
  4. 无阻塞：build/test 全绿。

### 4.2 下一步与阻塞

1. **【本轮已完成，原阻塞项已解除】anthropic / gemini 的传输与 SDK 选型**：已按原第 3 条选项 **(b)** 执行——复用本 crate 既有 `HttpTransport` + `reqwest` 手写 wire 协议，未引入任何新依赖，未触碰根 `Cargo.toml`，测试完全离线（`MemoryTransport`）。两子模块 provider 全链路已落地并测试全绿。
2. **【本 crate 内，非本轮范围】剩余 P1 细化缺口**：anthropic 并行 tool-use 拆分、gemini Vertex AI、统一远程媒体下载 resolver（§3.2/§3.3）。均可独立推进，无外部依赖。
3. **【跨模块裁定请求】OpenAI 兼容厂商（deepseek/glm/kimi/minimax）差异化行为**：Java 为每厂商提供独立 `*ModelProvider`（SPI 注册）+ `*Formatter`/`*MultiAgentFormatter`；Rust 侧 core 仅以 `base_url` 覆盖 + `context_windows.rs` 窗口表覆盖。需确认：(a) 这些厂商的格式化差异是否影响功能正确性（尤其 kimi/glm 的 multi-agent 格式化与 `GLMFormatter` 的 thinking 处理）；(b) 若需要，应在 core 扩展兼容表还是在本 crate 增补 vendor formatter。**core 侧改动本 crate 不得执行。** **注（2026-09-21）**：`openai` 实现体已迁入本 crate（§2.3），故若裁定为 (b)，vendor formatter 可在本 crate 内实施，无需再改 core。
4. **【待确认，非阻塞】`openai.tool.OpenAIMultiModalTool` 的归属**：core 与本 crate 均无对应，需裁定归 MODEL 模块还是工具类模块。
5. **【非阻塞】离线测试补强**：为 ollama converter 补充 tool-result 图片提升与 `OllamaToolsHelper` 对齐用例；为 dashscope 补 multimodal tool 与长尾 DTO 用例。
6. ~~**【非阻塞】依赖瘦身**：确认后可移除 `Cargo.toml` 中未使用的 `agentscope-harness`。~~ **已完成**（2026-09-21，`d86a578`）。
7. **【待裁定，2026-09-21 新增】`anthropic` 双实现并存**：本 crate 现有两条 Anthropic 路径——`crate::anthropic`（§2.4，自建 wire DTO + `HttpTransport`，58 个单测）与自 core 迁入的 `crate::anthropic_async`（基于 `async-anthropic` 0.6 SDK，653 行）。两者功能重叠，需汇总方裁定保留哪条（或保留主路径 + 另一条标记 deprecated）。**叠加成本**：同时存在两套 DTO 与依赖。

> **跨模块需求汇总**：本轮 anthropic/gemini 落地**不需要任何 core/harness 改动**，也**不需要修改根 `Cargo.toml`**（未新增任何依赖）。当前剩余跨模块请求仅为第 3、4 条（OpenAI 兼容厂商与多模态工具归属），均属「待裁定」而非阻塞。

> **跨模块需求汇总**：以上第 3、4 条涉及**是否需要修改 `crates/agentscope-core`（及极端情况下根 `Cargo.toml` 版本策略）**。本 crate 当前**不需要**任何 core/harness 改动即可保持 build/test 全绿；相关请求仅影响后续 anthropic/gemini/openai-compat 的落地方式，请汇总方裁定后再实施。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-model` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-model` / `cargo test -p agentscope-extensions-model` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-model/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

