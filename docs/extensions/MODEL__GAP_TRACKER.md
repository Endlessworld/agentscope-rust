# MODEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MODEL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-model\`
> **Java 子模块**：anthropic, dashscope, gemini, ollama, openai, **openai-official**, e2e-tests（7 个；旧版漏记 `openai-official`）
> **Rust 目标**：`crates/agentscope-extensions-model/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-model` |
| Java 子模块 | anthropic, dashscope, gemini, ollama, openai, openai-official, e2e-tests（7 个） |
| Java 规模（实测行数，`wc -l` 口径） | 主代码 33819 行 / 测试 63416 行（anthropic 3171/6259 · dashscope 8791/13933 · gemini 3071/5769 · ollama 5217/5711 · openai 9853/15793 · openai-official 3716/7395 · e2e-tests 0/8556） |
| Java 规模（去空行口径，rollup 使用） | 主代码 30058 行 / 测试 54660 行（= 33819 / 63416 扣除空行；**与上一行同源同文件，仅统计口径不同**，跨会话引用时勿混用） |
| Rust crate | `crates/agentscope-extensions-model` |
| 迁移波次 | A |
| 当前状态 | **部分完成（provider 主链路）** — `common`（HTTP seam + error/capabilities/usage/diagnostics/media/model_map）+ `ollama` + `dashscope` + `anthropic` + `gemini` + `openai` / `responses` / `anthropic_async` + `relay` 已落地；2026-10-22 静态复核实测 **42 个源文件 / 15004 行 / 267 个测试属性**（262 内联 + 5 集成）。`openai-official`（14 类 / 3716 行，Responses 官方 SDK 路径）**完全未对照**；§3.2 仍有多项待办（见文末静态复核记录） |

### 1.1 验证基线（2026-09-19 实测；2026-09-21 / 2026-10-12 更新；2026-10-22 静态复核）

> ⚠️ **2026-10-22 静态复核**：本节历史 `passed` 数字（198 / 236）为既往会话的编译期结果，本次复核按约定**未执行 `cargo build` / `cargo test`**，仅静态清点测试属性。当前源码静态计数为 **267 个测试属性（262 内联 + 5 集成）**，高于 §6 记录的 236；差额来自 2026-10-12 之后新增的 server-tool / base_url / media 等用例。真实 `passed` 数需下次编译轮确认。
```
（历史记录，未在本次复核中重跑）
cargo build -p agentscope-extensions-model   -> Finished (0 errors；本 crate 0 error/0 warning；输出中的 warning 均来自其它 crate 的既有代码)
cargo test  -p agentscope-extensions-model   -> 198 passed（2026-09-21）/ 236 passed（2026-10-12，231 lib + 5 集成）
（2026-10-22 静态计数，未编译）
测试属性总数 267 = common 34 + ollama 27 + dashscope 39 + anthropic 61 + gemini 58
                + openai 24 + responses 6 + anthropic_async 8 + relay 5 + tests/ 集成 5
```
> 说明：2026-09-21（提交 `d86a578`）core 的 `model/{openai,responses,anthropic}.rs` 整体迁入本 crate；本次复核已确认 `crates/agentscope-core/src/model/` 现仅剩 **10 个 `.rs` 文件 / 1117 行**（`mod.rs` 30 行 + 9 个模块文件：`cache_policy` 12 / `chat_response` 110 / `context_windows` 90 / `execution_config` 135 / `generate_options` 161 / `model`(trait) 77 / `provider`(SPI+registry) 371 / `tool_choice` 33 / `tool_schema` 98），**`model/openai.rs` / `model/responses.rs` / `model/anthropic.rs` 均已不存在**，叙述成立。该批代码**源自 core 既有实现，非从 Java 逐类重写**。

### 1.2 Rust crate 文件清单（42 个源文件，15004 行；262 个内联单测 + `tests/` 5 个集成测试）

> **行数口径说明（2026-10-22 静态复核补充）**：本表 `src/` 各行数为**物理行数**（含空行，等价 `wc -l`），42 文件合计 **15004**；**去空行口径为 13833**（rollup / 上游统计使用此口径）。表中 `tests/model_extensions_integration.rs` 原记 **117** 行实为**去空行**口径，其物理行数为 **135**——**该行与 `src/` 各行口径不一致**，已在下表更正为 135 并在「职责」列标注。

> **2026-10-22 复核更正**：旧版记「34 个源文件 / 10473 行 / 198 个单测全部为内联、无 `tests/` 目录」，**三点均已被证伪**。实测 `src/` 为 **42 个 `.rs` / 15004 行**；`tests/model_extensions_integration.rs`（117 行 / 5 例）**已存在**；内联测试 262 个。
> 相对旧表的增量文件：`src/relay.rs`(435)、`src/common/{base_url 95, capabilities 203, diagnostics 152, error 602, media 125, model_map 191, usage 75}`、`tests/model_extensions_integration.rs`(117)；`src/openai/mod.rs` 1045→1380、`src/anthropic_async/mod.rs` 653→712、`src/responses/mod.rs` 620→691、`src/lib.rs` 33→51。
| 文件 | 行数 | 职责 | 是否被 `lib.rs` 编译 |
|---|---|---|---|
| `src/lib.rs` | 51 | crate 根；声明 `pub mod anthropic; anthropic_async; common; dashscope; gemini; ollama; openai; relay; responses;` + re-export + 设计裁定文档注释 | — |
| `src/common/mod.rs` | 37 | 子模块 re-export 汇总（transport/error/capabilities/usage/diagnostics/media/model_map/base_url） | 是 |
| `src/common/transport.rs` | 461 | `HttpTransport` trait、`HttpRequestSpec`、`TransportConfig`（connect/read/keepalive/retry）、`ReqwestTransport`、`MemoryTransport`、`decode_stream_line`、`JsonStream`、`SharedTransport`；5 个单测 | 是 |
| `src/common/error.rs` | 602 | `ModelError` + `ErrorKind`（8 种厂商错误体字段归一）、`retryable()`、`parse_stream_value`；10 个单测 | 是 |
| `src/common/capabilities.rs` | 203 | `ModelCapabilities` + `resolve_capabilities`（最长前缀匹配）；4 个单测 | 是 |
| `src/common/usage.rs` | 75 | `merge_non_zero`；3 个单测 | 是 |
| `src/common/diagnostics.rs` | 152 | `ConversionDiagnostic` + `emit`/`warn`/`info`（tracing 落点）+ `Diagnostics`；2 个单测 | 是 |
| `src/common/media.rs` | 125 | `MediaResolver` seam + `set_media_resolver` + `DefaultMediaResolver`（base64/本地/远程拒绝）；2 个单测 | 是 |
| `src/common/model_map.rs` | 191 | `ModelMap`（链式映射 + 环检测 + base-name 回退）、`strip_reasoning_suffix`；4 个单测 | 是 |
| `src/common/base_url.rs` | 95 | `resolve_base_url` / `resolve_base_url_with`（显式 > env > 默认）；4 个单测 | 是 |
| `src/relay.rs` | 435 | 反向编码 `encode_response`（IR → OpenAI Chat/Responses/Claude/Gemini）+ `encode_error` + `BUILTIN_CONVERTERS`（`Quality` + 多跳 `steps`）+ `stream_terminator`；5 个单测 | 是 |
| `src/ollama/mod.rs` | 22 | ollama 子模块声明与 re-export | 是 |
| `src/ollama/dto.rs` | 170 | Ollama wire DTO（9 个 pub 类型，含 embedding DTO） | 是 |
| `src/ollama/options.rs` | 264 | `OllamaOptions` / `ThinkOption`；6 个单测 | 是 |
| `src/ollama/converter.rs` | 457 | `Msg`/`ToolSchema` → Ollama DTO；`ImageResolver` seam；8 个单测 | 是 |
| `src/ollama/parser.rs` | 295 | 响应解析 + 流式增量器；5 个单测 | 是 |
| `src/ollama/model.rs` | 323 | `OllamaChatModel`（impl core `Model`）+ `embed`/`embed_with_options`；5 个单测 | 是 |
| `src/ollama/provider.rs` | 110 | `OllamaModelProvider`（impl `ModelProvider`）；3 个单测 | 是 |
| `src/dashscope/mod.rs` | 33 | DashScope 子模块声明/re-export | 是 |
| `src/dashscope/dto.rs` | 418 | DashScope wire DTO（**18 个 pub 类型** = 15 个对应 Java `dto.*` + Rust 专属 `DashScopeContent`（untagged enum）/ `PublicKeyData` / `CacheControl` 别名）；**0 个单测** | 是 |
| `src/dashscope/converter.rs` | 819 | `Msg`/工具 → DashScope 请求体（含 cache control）；14 个单测 | 是 |
| `src/dashscope/credential.rs` | 127 | `DashScopeCredential` / `EndpointType`；3 个单测 | 是 |
| `src/dashscope/model.rs` | 385 | `DashScopeChatModel`（impl core `Model`）；6 个单测 | 是 |
| `src/dashscope/parser.rs` | 523 | 响应解析 + 流式增量器；11 个单测 | 是 |
| `src/dashscope/provider.rs` | 164 | `DashScopeModelProvider`（impl `ModelProvider`）；5 个单测 | 是 |
| `src/anthropic/mod.rs` | 45 | anthropic 子模块声明/re-export + Java→Rust 文件映射表 | 是 |
| `src/anthropic/dto.rs` | 492 | Anthropic Messages wire DTO（21 个 pub 类型，含 server-tool 变体）；8 个单测 | 是 |
| `src/anthropic/credential.rs` | 144 | `AnthropicCredential` / `AnthropicAuthMode` / 端点与版本常量；5 个单测 | 是 |
| `src/anthropic/converter.rs` | 1147 | `Msg`/`ToolSchema` → Anthropic 请求体（cache_control、多智能体合并、server-tool 回显）；23 个单测 | 是 |
| `src/anthropic/parser.rs` | 526 | 响应解析 + SSE 流式增量器（含 server-tool 结果映射）；10 个单测 | 是 |
| `src/anthropic/model.rs` | 430 | `AnthropicChatModel`（impl core `Model`）；7 个单测 | 是 |
| `src/anthropic/provider.rs` | 218 | `AnthropicModelProvider`（impl `ModelProvider`）；8 个单测 | 是 |
| `src/gemini/mod.rs` | 46 | gemini 子模块声明/re-export + Java→Rust 文件映射表 | 是 |
| `src/gemini/dto.rs` | 485 | Gemini REST wire DTO（20 个 pub 类型）；7 个单测 | 是 |
| `src/gemini/credential.rs` | 108 | `GeminiCredential` / 端点构造函数 / API 常量；4 个单测 | 是 |
| `src/gemini/converter.rs` | 839 | `Msg`/`ToolSchema` → Gemini 请求体（thought signature、server tools、多智能体合并）；21 个单测 | 是 |
| `src/gemini/parser.rs` | 591 | 响应解析 + 流式增量器（thoughtSignature 持久化）；11 个单测 | 是 |
| `src/gemini/model.rs` | 397 | `GeminiChatModel`（impl core `Model`）；7 个单测 | 是 |
| `src/gemini/provider.rs` | 216 | `GeminiModelProvider`（impl `ModelProvider`）；8 个单测 | 是 |
| `src/openai/mod.rs` | 1380 | `OpenAiChatModel` / `OpenAiProvider` / `default_registry`（`async-openai` 0.41）；24 个单测 | 是 |
| `src/responses/mod.rs` | 691 | `OpenAiResponsesModel` / `OpenAiResponsesProvider` / `default_responses_registry`；6 个单测 | 是 |
| `src/anthropic_async/mod.rs` | 712 | `AnthropicMessagesModel` / `AnthropicMessagesProvider`（`async-anthropic` 0.6）；8 个单测 | 是 |
| `tests/model_extensions_integration.rs` | 135 | 5 个集成测试（resolver 注入 / 错误归一 / 四协议反向编码 / 路由质量 / 能力+用量+命名）；物理 135 行 / 去空行 117 行 | 集成测试目标 |

> ℹ️ **更正（2026-09-19）**：本表旧版曾记「`dashscope` 仅 DTO、未接线、未编译」，并据此在 §3.1 列为待办阻塞项——**该描述已过时**。现 `src/dashscope/` 含 6 个子模块（converter/credential/dto/model/parser/provider）、已在 `lib.rs` 声明、已全部接入并随 crate 编译，共 **39** 个单测（旧记 33）。
>
> ℹ️ **本轮（2026-09-19）新增 anthropic / gemini**：两子模块均已在 `lib.rs` 声明（`pub mod anthropic; pub mod gemini;`）并 re-export，14 个新源文件、共 **119** 个新单测（anthropic 61 + gemini 58；旧记 111）。

## 2. Java 清单与依赖映射

> Java 类清单位于 `agentscope-extensions-model-<子模块>/src/main/java/io/agentscope/extensions/model/<子模块>/`；下表逐类给出 Rust 对应，未迁移者写明「待办 / 设计决定」。
> Java 侧每个子模块另有 `src/main/resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider`（SPI 注册文件，非 Java 类）。

### 2.1 ollama（22 类 / 5217 行）— **部分完成**
> **2026-10-22 复核**：Java 类数与旧记一致（22 个 `.java`），但**行数为 5217**（旧记 4412）；测试侧 16 文件 / 5711 行（旧记 4464）。真实缺口 2 项：`OllamaConversationMerger`、`OllamaMultiAgentFormatter`（`<system-info>` 图片提升）。
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
| `ollama.dto.OllamaEmbeddingRequest` | `POST /api/embed` 请求 | `ollama::dto::OllamaEmbeddingRequest` | 已接线：`OllamaChatModel::embed` / `embed_with_options`（§6.2 P1 已完成） |
| `ollama.dto.OllamaEmbeddingResponse` | embed 响应 | `ollama::dto::OllamaEmbeddingResponse` | 同上 |
| `ollama.formatter.OllamaChatFormatter` | 会话格式化 | `ollama::converter::{to_ollama_messages, to_ollama_messages_with, build_request}` | |
| `ollama.formatter.OllamaMessageConverter` | 单条消息转换 | `ollama::converter::{block_text, collect_text, role_str, url_of, ImageResolver, DefaultImageResolver}` | |
| `ollama.formatter.OllamaToolsHelper` | 工具/工具选择转换 | `ollama::converter::{to_ollama_tools, to_tool_choice}` | |
| `ollama.formatter.OllamaMediaConverter` | 图片/媒体 → base64 | `ollama::converter::{ImageResolver, DefaultImageResolver}` | 已改走共享 `common::resolve_media`：默认拒绝远程 URL，宿主 `set_media_resolver` 后即可下载（§6.2 P1） |
| `ollama.formatter.OllamaResponseParser` | 响应 → `ChatResponse` | `ollama::parser::{parse_response, OllamaStreamIncrementer}` | 工具调用 id 用 `uuid` 合成（同 Java） |
| `ollama.formatter.OllamaConversationMerger` | 多轮/多智能体会话合并 | 无 | **待办**（见 §3.2） |
| `ollama.formatter.OllamaMultiAgentFormatter` | 多智能体格式化 | 部分：`ollama::converter::hint_text` + tool-result 文本抽取 | **待办**（Java 的 `<system-info>` 图片提升仅部分覆盖） |
| `ollama.options.OllamaOptions` | 采样/加载参数 | `ollama::options::OllamaOptions` | 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`；llama.cpp loader 长尾见 §3.3 P2 |
| `ollama.options.ThinkOption` | thinking 开关/级别 | `ollama::options::ThinkOption` | `Boolean(bool)` / `Level(String)` |
| `resources/META-INF/services/...ModelProvider` | SPI 注册 | 无对应机制；改为 `ModelRegistry::register(Arc<dyn ModelProvider>)` 显式注册 | **设计决定**（Rust 无 ServiceLoader） |

### 2.2 dashscope（29 类 / 8791 行）— **部分完成（provider 主链路）**
> **2026-10-22 复核**：类数一致（29 个 `.java`），**行数为 8791**（旧记 7453）；测试 23 文件 / 13933 行（旧记 11767）。真实缺口：`DashScopeEncryptionUtils` 的 AES-GCM/RSA 加密**未实现**（仅公钥 DTO + 端点常量）、`ConversationMerger`、`MultiAgentFormatter`、`DashScopeMultiModalTool`。
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
| `dashscope.dto.DashScopePublicKeyResponse` | 加密公钥响应 | `dashscope::dto::{DashScopePublicKeyResponse, PublicKeyData}` + `dashscope::model::PUBLIC_KEYS_ENDPOINT` | 仅**常量与 DTO**；无公钥拉取调用方（见下） |
| `dashscope.DashScopeChatModel` | 聊天模型主类 | `dashscope::model::DashScopeChatModel`（impl core `Model`） | 已接线 |
| `dashscope.DashScopeHttpClient` | HTTP/SSE 客户端 | `dashscope::{converter,parser}` + `common::HttpTransport` | 已接线 |
| `dashscope.DashScopeEncryptionUtils` | 请求加密（AES-GCM 数据 + RSA 包 AES key） | 无 | **待办**：Rust 侧只有 `PUBLIC_KEYS_ENDPOINT` 常量与 `DashScopePublicKeyResponse` DTO，**没有任何加密/解密实现**（旧版记「已接线」有误） |
| `dashscope.DashScopeModelProvider` | 注册表 provider | `dashscope::provider::DashScopeModelProvider`（impl `ModelProvider`） | 已接线；`DASHSCOPE_API_KEY` / `DASHSCOPE_BASE_URL` |
| `dashscope.EndpointType` | 端点类型枚举 | `dashscope::credential::EndpointType` | `Auto`/`Text`/`Multimodal` + `parse()` |
| `dashscope.credential.DashScopeCredential` | 凭据 | `dashscope::credential::DashScopeCredential` | **已迁移**（旧版记「无 / 待办」有误）：`api_key` + `base_url` + `CREDENTIAL_TYPE` |
| `dashscope.formatter.DashScopeChatFormatter` | 会话格式化 + options/tools | `dashscope::converter::{to_dashscope_messages_with_options, build_request}` | 部分（`build_request` 覆盖主链路） |
| `dashscope.formatter.DashScopeMessageConverter` | 单条消息转换（含多模态 part） | `dashscope::converter::{to_dashscope_messages, simple_message, multimodal_message, tool_role_multimodal_message}` | 部分（函数级覆盖，无同名类型） |
| `dashscope.formatter.DashScopeMediaConverter` | 图片/音频/视频 → part | `dashscope::converter::{image_part, audio_part, video_part, data_part}` | 部分（函数级覆盖） |
| `dashscope.formatter.DashScopeToolsHelper` | 工具/工具选择转换 | `dashscope::converter::{to_dashscope_tools, to_tool_choice_value}` | 部分（`Required→auto` 降级带 diagnostics） |
| `dashscope.formatter.DashScopeResponseParser` | 响应 → `ChatResponse` | `dashscope::parser::{parse_response, DashScopeStreamIncrementer}` | 等价 |
| `dashscope.formatter.DashScopeConversationMerger` | 多轮/多智能体会话合并（`<history>`） | 无 | **待办**（`converter.rs` 内 `merge` 命中均为 options 合并，无会话合并） |
| `dashscope.formatter.DashScopeMultiAgentFormatter` | 多智能体格式化 | 无 | **待办** |
| `dashscope.tool.DashScopeMultiModalTool`（93.5 KB / 29 文件之一） | 多模态工具（TTS/图像/视频等） | 无 | **待办**（本模块最大单文件） |

### 2.3 openai（53 类 / 9853 行）— **部分完成（2026-09-21 自 core 迁入本 crate）**
> **状态口径变更（2026-09-21，`d86a578`）**：本节原为「设计决定（不迁移）」，依据是 `EXTENSIONS_GAP_TRACKER.md` §2.2 裁定「core 定 trait + 注册表，本 crate 不重复实现 openai」。该裁定已被本次目录重构取代——core 的 `model/openai.rs`、`model/responses.rs`、`model/anthropic.rs` **实现体已整体迁入本 crate**，core 仅保留 `Model` trait / `ModelRegistry` / provider SPI。因此「不重复实现」的说法不再成立。
> **注意区分**：迁入的是 **core 既有实现**，不等价于「Java 53 类逐类迁移完成」。Java 侧的 `compat.*`（12 类）、`exception.*`（8 类）、`OpenAIMultiModalTool`、`credential.OpenAICredential` 仍未迁，见下表与 §3.2/§3.4。
> **2026-10-22 复核**：类数一致（53 个 `.java`），**行数 9853**（旧记 8534）；测试 45 文件 / 15793 行（旧记 12886）。`src/openai/mod.rs` 实测 **1380 行**（旧记 1045）。

| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openai.OpenAIChatModel` / `OpenAIClient` / `OpenAIModelProvider` / `OpenAIModelProviderSupport` | OpenAI 兼容聊天链路与 provider | `crate::openai::{OpenAiChatModel, OpenAiProvider, default_registry}`（`src/openai/mod.rs`，1380 行，基于 `async-openai` 0.41） | **已就位**（实现体原在 `agentscope-core/src/model/openai.rs`，2026-09-21 整体迁入；`agentscope_core` 不再导出这些类型） |
| `openai.credential.OpenAICredential` | 凭据载体 | 无独立类型；经 `ModelCreationContext.api_key` / `OPENAI_API_KEY` | **待办/待裁定**（旧表漏记此类；Rust 侧 `openai/mod.rs` 无 credential 符号） |
| `openai.dto.*`（16 类：Request/Response/Message/Choice/ContentPart/Function/ImageUrl/InputAudio/ReasoningDetail/StreamOptions/Tool/ToolCall/ToolFunction/Usage/VideoUrl/Error） | OpenAI wire 模型 | `src/openai/mod.rs` 内部 DTO + `async-openai` 0.41 类型 | 已就位（随 `openai` 一并迁入）；Java 16 类未逐类 1:1 重写 |
| `openai.responses.*`（OpenAI Responses API） | Responses 协议客户端 | `crate::responses::{OpenAiResponsesModel, OpenAiResponsesProvider, default_responses_registry}`（`src/responses/mod.rs`，691 行） | **已就位**：与 Java 的 `openai-official` 子模块（`OpenAIResponsesChatModel` 等 14 类）是**不同来源**——见 §2.8 |
| `anthropic_async`（async-anthropic 版 Messages 客户端） | Anthropic Messages 协议（SDK 驱动） | `crate::anthropic_async::{AnthropicMessagesModel, AnthropicMessagesProvider, default_anthropic_registry}`（`src/anthropic_async/mod.rs`，712 行，基于 `async-anthropic` 0.6） | **本轮新增对应**：与 §2.4 的 transport-seam 版 `crate::anthropic` 并存（两条实现路径，见 §4.2 待裁定）。**注意：Java 侧无对应子模块**（Java 用官方 SDK 的是 anthropic 子模块本身） |
| `openai.exception.*`（8 类：OpenAIException/Authentication/BadRequest/InternalServer/NotFound/PermissionDenied/RateLimit/UnprocessableEntity） | 异常层次 | `crate::common::error::{ModelError, ErrorKind}`（8 种厂商错误体归一 + `retryable()`） | **设计决定**：Rust 用单一 `thiserror` 枚举 + `ErrorKind` 分类，不 1:1 建异常类层次；分类能力已等价（§6.2 P0） |
| `openai.formatter.*`（11 类：OpenAIBase/Chat/ConversationMerger/ConverterUtils/MessageConverter/MultiAgent/ResponseParser + 4 个 vendor formatter） | OpenAI 格式/转换/解析 | `src/openai/mod.rs`（`to_request_messages`/`to_request_tools`/`to_tool_choice`/`to_response_format`/`StreamIncrementer`） | 已就位（随迁入）；**`OpenAIConversationMerger` / `OpenAIMultiAgentFormatter` 无对应**（`<history>` 会话合并与多智能体分组缺失） |
| `openai.tool.OpenAIMultiModalTool` | 多模态工具 | 无 | **待确认**（core 与本 crate 均无对应；归属 MODEL 还是 TOOL 类模块待汇总方裁定，见 §4.2 第 4 条） |
| `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 兼容厂商 formatter + provider | 无专门类型；经 `crate::openai::OpenAiProvider` + `ModelCreationContext.base_url` + core `context_windows.rs` + `common::capabilities`（DeepSeek `reasoning_content` 回传、`max_tokens` 兼容）覆盖 | **部分完成**：DeepSeek 的 `reasoning_content` 往返与 legacy `max_tokens` 已实现并有单测；**GLM/Kimi/MiniMax 的 formatter 差异化（thinking 处理、multi-agent 分组）无对应** |
| `resources/META-INF/services/...ModelProvider`（5 行） | SPI 注册 | `crate::openai::default_registry()` / core `ModelRegistry::register` | |

### 2.4 anthropic（13 类 / 3171 行）— **部分完成（provider 主链路已完成）**
> **2026-10-22 复核**：Java 侧实为 **13 个类**（旧记 11），**3171 行**（旧记 2192）；测试 16 文件 / 6259 行（旧记 4677）。旧表**漏记 2 类**：`formatter.AnthropicServerToolHelper`、`tool.AnthropicServerTool`。这两类的 provider 侧能力（server-tool 块回显与结果映射）在 Rust 已实现，但**没有 Java 侧的工具声明/校验入口**。
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
| `anthropic.formatter.AnthropicServerToolHelper`（旧表漏记） | server tool 块 ↔ SDK 内容块互转、结果状态映射 | `anthropic::parser::{METADATA_SERVER_TOOL_RESULT, blocks_of, server_tool_result_block}` + `anthropic::converter`（`ServerToolUse` 回显、`is_server_tool()` 分支） | **部分完成**：解析侧（`server_tool_use` / `web_search_tool_result` / `code_execution` / `bash_code_execution` / `text_editor_code_execution` / `tool_search` → `ToolResultBlock` + metadata）已实现且有 3 个单测；**Java 侧的 `ToolValidator` 校验与 `ServerToolUseBlockParam` 声明构造无对应** |
| `anthropic.tool.AnthropicServerTool`（旧表漏记） | server tool 声明（web_search 等内置工具） | 无独立类型（`GeminiServerTool` 有对等物，Anthropic 侧无） | **待办**：Rust 无法把「内置 server tool」声明进请求，只能回显服务端已返回的块 |
| SDK wire 类型（`MessageCreateParams`/`MessageParam`/`Message`/`RawMessageStreamEvent`） | Anthropic 官方 SDK | `anthropic::dto::{AnthropicRequest, AnthropicMessage, AnthropicContentBlock, AnthropicMessageResponse, AnthropicStreamEvent}` | Java 用官方 SDK，Rust 无对应依赖→自建 DTO/parser（方案裁定见 §4.2） |

> **传输层裁定**（2026-09-19）：采用 §4.2 选项 (b) —— 复用本 crate 既有 `HttpTransport` seam + `ReqwestTransport`（生产）/`MemoryTransport`（离线测试）手写 Anthropic Messages 协议，**未引入新 HTTP / 厂商 SDK 依赖**。

### 2.5 gemini（11 类 / 3071 行）— **部分完成（provider 全链路已完成）**
> **2026-10-22 复核**：Java 侧实为 **11 个类**（旧记 10），**3071 行**（旧记 2294）；测试 16 文件 / 5769 行（旧记 4113）。旧表**漏记 1 类**：`tool.GeminiServerTool`（内置 server tool 声明）。Rust 侧**有对等物** `gemini::converter::GeminiServerTool`（`GoogleSearch`/`UrlContext`/`CodeExecution`）+ `merge_server_tools` / `build_request_with_server_tools`——**这是 anthropic 侧缺失的那一半**（见 §2.4 `AnthropicServerTool` 行）。其余 10 类映射已核实存在。
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
| `gemini.tool.GeminiServerTool`（旧表漏记） | 内置 server tool 声明（googleSearch / urlContext / codeExecution） | `gemini::converter::GeminiServerTool` + `merge_server_tools` + `build_request_with_server_tools` | **等价**：`to_tool()` 映射到 `google_search`/`url_context`/`code_execution` 键，并置 `includeServerSideToolInvocations`；`GeminiChatModel::with_server_tools` 接线 |
| SDK wire 类型（`Content`/`Part`/`GenerateContentConfig`/`GenerateContentResponse`） | Google GenAI SDK | `gemini::dto::{GeminiRequest, GeminiContent, GeminiPart, GeminiResponse, ...}` | Java 用官方 SDK，Rust 无对应依赖→自建 DTO/parser（方案裁定见 §4.2） |

> **传输层裁定**（2026-09-19）：同 anthropic，采用 §4.2 选项 (b) —— 复用 `HttpTransport` seam 手写 Gemini REST 协议，**未引入 Google SDK / 新依赖**。

### 2.6 e2e-tests（0 主代码 / 36 测试文件 / 8556 行）— **设计决定（不迁移）**
> **2026-10-22 复核**：Java 侧实测 **36 个 `.java` 文件 / 8556 行**（旧记 34 类 / 7136 行）；其中**含 `@Test`/`@ParameterizedTest` 的真正测试类为 17 个**（另有 14 个 `providers/*` 辅助类 + 5 个 util/常量类）。**「必须依赖真实网络/密钥」这一前提已核实成立**：`ProviderFactory` 读取 `OPENAI_API_KEY` / `DASHSCOPE_API_KEY` / `DEEPSEEK_API_KEY` / `GLM_API_KEY` / `GOOGLE_API_KEY` / `ANTHROPIC_API_KEY` / `OPENROUTER_API_KEY`；`E2ETestCondition` 另需 `ENABLE_E2E` 环境变量或系统属性，否则用 `assumeTrue` 跳过。**结论：维持「设计决定（不迁移）」成立**（Rust 侧坚持 `HttpTransport` seam + `MemoryTransport` 离线策略）。
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `io.agentscope.core.e2e.*`（CoreAgentE2ETest、MultiModalE2ETest、StreamingBehaviorE2ETest、StructuredOutputE2ETest、ToolSystemE2ETest、CrossModelCompatibilityE2ETest、AudioCapabilityE2ETest、VideoCapabilityE2ETest、HookE2ETest、HITLBasicE2ETest、InterruptionE2ETest、SkillE2ETest、ToolGroupsE2ETest、ExecutionConfigE2ETest、SpecializedFeaturesE2ETest 等 **17 个含 `@Test` 的测试类**）+ `providers.*`（ModelProvider/BaseModelProvider/AnthropicProvider/DashScopeProvider/OpenRouterProvider 等 14 个辅助类）+ `io.agentscope.extensions.model.*.E2ETest`（DashScopeMultiModalToolE2ETest、GeminiNullableToolSchemaE2ETest） | 真实厂商端到端回归（需真实 API key + `ENABLE_E2E`） | 无 | **设计决定（不迁移）**：Rust 侧统一采用**离线可测**策略——`HttpTransport` seam + `MemoryTransport` 注入，不引入需要密钥的网络测试；各 provider 的真实联调记为 §3.2 待办 |

### 2.7 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Jackson (`@JsonProperty`/`@JsonInclude`/`@JsonAlias`/自定义 (De)Serializer) | JSON (反)序列化 | `serde` + `serde_json`（`[dependencies]` 经 workspace 引入）+ `#[serde(rename/skip_serializing_if/untagged)]` | 字段名 1:1 对齐 Java JSON |
| OkHttp / `java.net.http.HttpClient` / core `OkHttpTransport` | HTTP 传输 | `reqwest` **0.13**（经 workspace；根 `Cargo.toml` 已于 `43cd044` 全局统一 0.13）+ 别名 `reqwest12 = { package = "reqwest", version = "0.12" }`（仅 `anthropic_async` 注入自定义 client 用；`async-anthropic` 0.6 钉住 0.12）+ 自建 `HttpTransport` trait seam | **2026-10-22 复核更正**：旧记「`reqwest` 0.12」已过时——本 crate 主体走 workspace 0.13；0.12 仅作为别名保留（见 §6.3-2）。生产 `ReqwestTransport`，测试 `MemoryTransport`（零真实 HTTP） |
| `com.anthropic.client.okhttp.AnthropicOkHttpClient`（Anthropic 官方 SDK） | Anthropic 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `anthropic::dto` 自建 Messages 协议 | **已完成**（2026-09-19）：未引入厂商 SDK；wire DTO 与 converter/parser 自建，可离线测 |
| `com.google.genai.Client`（Google GenAI 官方 SDK） | Gemini 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `gemini::dto` 自建 REST 协议 | **已完成**（2026-09-19）：同上；SSE 流式用 `:streamGenerateContent?alt=sse` |
| reactor-core (`Flux`/`Mono`/`Schedulers`) | 响应式流 | `futures` (`BoxStream`) + `async-stream` **0.3**（本 crate） | core `Model::stream` 返回 `BoxStream` |
| slf4j (`Logger`/`LoggerFactory`) | 日志 | `tracing`（workspace） | |
| `java.util.UUID` | 合成工具调用 id / 请求 id | `uuid` **1**（本 crate，features `v4`, `serde`） | 与 Java 行为一致 |
| `java.util.Base64` | 媒体内联编码 | `base64` **0.22**（本 crate `Cargo.toml` 已声明） | ⚠️ **2026-10-22 复核**：`base64` crate 在本 crate 源码中**零直接引用**（base64 编码实际由 core `formatter::media_utils::file_to_base64` 与 `agentscope_core` 承担）；声明存在但未被使用 |
| `java.net.URL` / `URLEncoder` / `URI` | URL 构造与转义 | `reqwest`/`url` 内置 | 受控子集 |
| JUnit 5 / Mockito | 测试 | `#[test]` / `#[tokio::test]` + `MemoryTransport` fake | 离线，不新增 HTTP mock crate |
| Spring Boot AutoConfiguration | DI / 自动装配 | 不适用 | **设计决定（不迁移）**（本模块 Java 侧亦无 Spring 依赖，`pom.xml` 仅 core/jackson/reactor/junit） |
| `agentscope-core` / `agentscope-harness` | 内部依赖 | path 依赖 `../agentscope-core`（`Cargo.toml` 实测仅此一项 path 依赖） | ✅ **2026-10-22 复核**：§3.3 记「`agentscope-harness` 在本 crate 源码中零引用」与 §3.3「已完成（2026-09-21 从 `Cargo.toml` 移除）」**均成立**——`Cargo.toml` 当前只有 `agentscope-core` 一个 path 依赖，源码中 `agentscope_harness` 出现 **0** 次 |
| `openai-java` 官方 SDK（`openai-official` 子模块） | Responses API 官方客户端 | `async-openai` 0.41（`features` 含 `responses` / `response-types`）+ `crate::responses` 自建 request/stream/parse | **待裁定（2026-10-22 复核）**：见 §2.8；SDK 路径不同但**能力缺口独立存在** |

### 2.8 openai-official（14 类 / 3716 行）— **未对照（真实缺口）**
> **2026-10-22 复核新增**：本节此前**完全缺失**（旧版仅在 §1 提到「旧版漏记 `openai-official`」）。Java 侧 `agentscope-extensions-model-openai-official` 是**独立 Maven 子模块**，依赖官方 `openai-java` SDK；provider id = **`openai-official`**，模型 id 前缀 = **`openai-official:`**（`OpenAIOfficialModelProvider` 的 `MODEL_ID` 正则 `openai-official:.+`），并有独立 SPI 注册文件。
> ⚠️ **关键区分**：Rust 侧 `crate::responses`（`OpenAiResponsesModel` / `OpenAiResponsesProvider`，provider id = core 常量 `openai-responses` = `"openai-responses"`）**不是**本子模块的等价物——provider id、SDK、id 前缀三者全不同，**不可互相顶替**。
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openaiofficial.OpenAIResponsesChatModel`（415 行） | Responses 协议聊天模型主类 | `responses::OpenAiResponsesModel`（`src/responses/mod.rs`，691 行） | **部分完成**：请求/流式/解析主干有对等物；但 Java 侧 `ResponsesMultiAgentFormatter` 注入、`List<OpenAIServerTool> serverTools` 注入、`additionalBodyParams` 白名单 fail-fast **无对应** |
| `openaiofficial.OpenAIOfficialModelProvider`（118 行） | SPI 注册 provider（前缀 `openai-official:`） | `responses::OpenAiResponsesProvider`（provider id `openai-responses`，`supports()` 返回 `true` 兜底） | **不等价**：provider id 与前缀均不同，Rust 侧无 `openai-official:` 前缀解析 |
| `openaiofficial.ResponsesRequestMapper`（**834 行**，本子模块最大类） | `Msg` → Responses `ResponseCreateParams` 全量映射 | `responses::build_request`（`src/responses/mod.rs:226`） | **部分完成**：system→instructions、user→easy message、tool_use→function_call、tool_result→function_call_output 已覆盖；**多智能体分组 / 多模态图片 / `additionalBodyParams` 白名单 fail-fast / `prompt_cache_key` 等 body 参数**无对应 |
| `openaiofficial.ResponsesMultiAgentFormatter`（355 行） | 多智能体分组（SYSTEM / TOOL_SEQUENCE / AGENT_CONVERSATION / BYPASS）+ `<history>` 包裹 | **无** | **缺失**：Rust `responses` 无任何 `<history>` / 多智能体分组实现（`responses/mod.rs` 全文无 `history`） |
| `openaiofficial.ResponsesStreamingAssembler`（355 行） | SSE 流式装配（reasoning / message / function_call 分桶） | `responses::ResponsesIncrementer`（`:397`） | **部分完成**：text delta / reasoning delta / function_call arguments delta → `ToolUse` 已覆盖（`src/responses/mod.rs:9-13` 文档 + `accumulate`/`into_final`）；**`encryptedContent` / reasoning item 回放 / BYPASS 分支**无对应 |
| `openaiofficial.ResponsesResponseParser`（211 行） | 非流式响应 → `ChatResponse`（reasoning/message/function_call 分桶 + refusal 短路） | `responses::ResponsesIncrementer::into_final` + `status_string` | **部分完成**：非流式与流式共用增量器；**refusal 短路（不可重试异常）**无对应 |
| `openaiofficial.ResponsesHelper`（194 行） | 辅助构造 | 无独立类型 | 功能级可由 `responses` 的 `easy_message` / `to_request_tools` / `to_tool_choice` 覆盖，但**无一一对应物** |
| `openaiofficial.OpenAISdkClientFactory`（139 行） | SDK client 工厂 | 无 | **设计决定候选**：Rust 用 `async-openai` 的 `Client::with_config`，不需工厂层 |
| `openaiofficial.OpenAIOfficialConstants`（70 行） | provider id / `additionalBodyParams` 白名单 / metadata 命名空间键 | 无 | **缺失**：白名单 fail-fast 行为是 Java 侧的实际请求校验，Rust 无 |
| `openaiofficial.OpenAIErrorTranslator`（128 行） | SDK 异常 → 统一异常 | `common::error::ModelError` + `responses::map_openai_error`（`:216`） | **部分完成**：`map_openai_error` 目前只映射 `ApiError`→`Error::HttpStatus` / 其余→`Error::Model`，**未走 `ModelError::from_parts` 的结构化归一**（与 `openai/mod.rs:338` 的实现不一致——后者已用 `ModelError::from_parts` + `ErrorKind::Timeout`/`Network`）。⚠️ **本轮新发现的不一致点** |
| `openaiofficial.OpenAIOfficialModelException`（101 行） | 模块异常 | `agentscope_core::Error` | **设计决定**：不建平行异常体系 |
| `openaiofficial.credential.OpenAIOfficialCredential`（127 行） | 凭据载体 | 无独立类型；经 `ModelCreationContext.api_key` / `OPENAI_API_KEY` | 与 `openai.credential.OpenAICredential` 同处理（§2.3 已记为待办/待裁定） |
| `openaiofficial.tool.OpenAIServerTool`（125 行） | 内置 server tool 声明（web_search / code_interpreter / image_generation / tool_search） | **无** | **缺失**：Rust `responses::to_request_tools`（`:334`）**只构造 `Tool::Function`**，无任何内置 server tool 变体。⚠️ 与 gemini 侧 `GeminiServerTool`（已实现）对比明显 |
| `openaiofficial.tool.ResponsesServerToolHelper`（**544 行**，本子模块第二大类） | server tool 块 ↔ SDK 互转 + 结果状态映射 | **无** | **缺失**：Rust 侧无 server-tool 声明/校验/结果映射 |
| `resources/META-INF/services/...ModelProvider` | SPI 注册（`OpenAIOfficialModelProvider`） | `responses::default_responses_registry` | **设计决定**（Rust 无 `ServiceLoader`），但 provider id 不一致 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 接线 | 全模块 | 已完成 | `src/lib.rs` 声明 `pub mod anthropic; anthropic_async; common; dashscope; gemini; ollama; openai; relay; responses;`（9 个模块）+ re-export；crate 级文档注释已写明与 core 的分工。**2026-10-22 复核**：`lib.rs` 51 行，与声明一致 |
| `Model` trait 复用（不自建平行 trait） | `io.agentscope.core.model.Model` | 已完成 | `ollama::model::OllamaChatModel` 实现 `agentscope_core::Model`（`crates/agentscope-core/src/model/model.rs:12`）；未在 extensions 侧新建 trait。**2026-10-22 复核**：core `model/mod.rs` 仅 re-export trait/SPI/DTO（`Model`/`ModelCapabilities`/`ModelProvider`/`ModelRegistry`/`ModelCreationContext` 等），确无具体实现体 |
| `ModelProvider` / `ModelRegistry` 复用 | `io.agentscope.core.model.spi.ModelProvider` + `ModelRegistry` | 已完成 | `ollama::provider::OllamaModelProvider` 实现 core `ModelProvider`（`crates/agentscope-core/src/model/provider.rs:126`）；注册表为 core `ModelRegistry`（同文件 `:143`，`register` 在 `:154`）。⚠️ **2026-10-22 勘误**：旧记行号 `:78` / `:95` 已过时（core 文件已扩到 371 行）。Java 的 SPI 文件机制不迁移（**设计决定**） |
| 离线可测的 HTTP seam | `OllamaHttpClient` / `DashScopeHttpClient` / `OpenAIClient` / Anthropic SDK / Google GenAI SDK | 已完成 | `src/common/transport.rs`：`HttpTransport` trait + `ReqwestTransport` + `MemoryTransport`；全部 **267** 个测试属性零真实网络（2026-10-22 静态计数） |
| Ollama provider 全链路（格式化→请求→解析→流式） | `OllamaChatModel`/`OllamaHttpClient`/`OllamaModelProvider` + `formatter/*`（7 类） | 已完成 | `build_request`/`to_ollama_messages`/`to_ollama_tools`/`parse_response`/`OllamaStreamIncrementer`；端点 `/api/chat` |
| Ollama DTO serde 对齐 | `ollama.dto.*`（9 类，含 embedding） | 已完成 | `src/ollama/dto.rs`（9 个 pub 类型，与 Java 9 类一一对应）；embedding DTO 已迁且**已有调用方**（`OllamaChatModel::embed`，见 §2.1） |
| 离线单测 | Java 测试 54660 行（去空行）/ 63416 行（`wc -l`） | 已完成（部分覆盖） | **267 个测试属性（262 内联 + 5 集成）**：`common` 34（transport 5 / error 10 / capabilities 4 / usage 3 / model_map 4 / base_url 4 / diagnostics 2 / media 2）、`ollama` 27、`dashscope` 39、`anthropic` 61、`gemini` 58、`openai` 24、`responses` 6、`anthropic_async` 8、`relay` 5、`tests/` 5。**2026-10-22 静态复核更正**：旧记 198 与逐文件拆分均已过时 |
| **`dashscope` provider 全链路** | `dashscope.DashScopeChatModel` / `DashScopeHttpClient` / `DashScopeModelProvider` / `EndpointType` | **已完成** | `src/dashscope/{converter,credential,dto,model,parser,provider}.rs`，共 **39** 个单测；`lib.rs` 已声明 `pub mod dashscope;`。**历史误记更正（2026-09-19）**：旧版曾将此列为「接线缺陷/阻塞项」（称四模块文件缺失且未编译），实测该缺陷不存在 |
| dashscope DTO 接线 | `dashscope.dto.*`（15 类） | 已完成 | `src/dashscope/dto.rs` **18 个** pub 类型（15 个对应 Java + Rust 专属 `DashScopeContent` untagged enum / `PublicKeyData` / `CacheControl` 别名）；含 `DashScopePublicKeyResponse`（凭证加密公钥，**仅 DTO，无拉取调用方**） |
| anthropic provider 基线 | `anthropic.*`（**13** 类） | **已完成** | `src/anthropic/{dto,credential,converter,parser,model,provider,mod}.rs`，**61** 个单测；`lib.rs` 已声明 `pub mod anthropic;`；详见 §2.4 |
| gemini provider 基线 | `gemini.*`（**11** 类） | **已完成** | `src/gemini/{dto,credential,converter,parser,model,provider,mod}.rs`，**58** 个单测；`lib.rs` 已声明 `pub mod gemini;`；详见 §2.5 |
| OpenAI provider | `openai.*`（53 类） | **已完成（2026-09-21 自 core 迁入）** | `src/openai/mod.rs`（`OpenAiChatModel`/`OpenAiProvider`/`default_registry`，**1380 行**）；同批迁入 `responses`（**691 行**）与 `anthropic_async`（**712 行**）。⚠️ 原「设计决定（不迁移）」口径已作废；Java `compat.*`（12 类）/`exception.*`（8 类）/`OpenAIMultiModalTool` 仍未迁（§3.2/§3.4） |
| **`openai-official` 子模块对照** | `openaiofficial.*`（**14 类** / 3716 行） | **待办** | **2026-10-22 复核新增**：Java 侧 `openai-official` 为**独立子模块**（`OpenAIResponsesChatModel` / `ResponsesRequestMapper` / `ResponsesMultiAgentFormatter` / `ResponsesStreamingAssembler` / `ResponsesResponseParser` / `ResponsesHelper` / `OpenAIOfficialModelProvider` / `OpenAIOfficialConstants` / `OpenAIErrorTranslator` / `OpenAIOfficialModelException` / `OpenAISdkClientFactory` / `credential.OpenAIOfficialCredential` / `tool.OpenAIServerTool` / `tool.ResponsesServerToolHelper`），provider id = **`openai-official`**（前缀 `openai-official:`），走 **官方 `openai-java` SDK**。Rust 侧 `crate::responses` 虽同为 Responses 协议，但 provider id = core 常量 **`openai-responses`**（`provider.rs:20`）、基于 `async-openai` 而非官方 Java SDK——**二者不是同一物**，不可互相顶替 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama embedding 调用链 | `ollama.dto.OllamaEmbedding*` + `OllamaHttpClient` 的 `POST /api/embed` | **已完成** | **2026-10-22 复核更正**：旧记「无客户端方法、无调用方、无单测」已过时。实测 `ollama::model::OllamaChatModel::embed`（`:73`）/ `embed_with_options`（`:78`）已接线 `POST {base_url}/api/embed`（常量 `EMBED_ENDPOINT`），并有单测 `embed_posts_to_embed_endpoint`（`model.rs:233`）断言 URL 与 provider。⚠️ 遗留小项：`embed_with_options` 的 `options` 参数当前被显式忽略（`let _ = options;`，预留未接线） |
| Ollama 会话合并 | `ollama.formatter.OllamaConversationMerger` | 待办 | 无 Rust 对应；影响多轮/多智能体长会话的消息合并语义。**2026-10-22 复核**：全 crate 检索 `OllamaConversationMerger` / `<history>` 在 ollama 子模块**零命中**，确认缺失（anthropic/gemini 已有 `merge_conversation` / `merge_to_content` 对应物） |
| Ollama 多智能体格式化 | `ollama.formatter.OllamaMultiAgentFormatter` | 待办 | 仅 `hint_text` + tool-result 文本抽取被覆盖；`<system-info>` 图片提升等未完整对齐。**2026-10-22 复核**：`ollama/converter.rs` 无 `<history>` / multi-agent 分组实现，确认缺失（Java 该类 508 行，为 ollama 子模块第二大类） |
| Ollama 远程图片下载 | `ollama.formatter.OllamaMediaConverter` | 待办 | `DefaultImageResolver` 对 http(s) URL 返回错误，需注入自定义 resolver（doc 注释已标注 `MODEL__GAP_TRACKER.md §3 (P1)`）；未提供内置实现。**2026-10-22 复核**：已改走共享 `common::resolve_media`（`ollama/converter.rs:39`），宿主 `set_media_resolver` 后即可下载；仍**无内置**远程下载实现 |
| anthropic 子模块整体 | `anthropic.*`（**13** 类 / **3171** 行） | **已完成** | cache control / thinking / media / multi-agent formatter 均已落地；传输层自建（§4.2 选项 b）。**剩余 2 项细化缺口见下**（类数/行数已按 2026-10-22 复核更正） |
| anthropic 并行 tool-use 拆分 | `AnthropicMessageConverter#collectSplitToolResults` / `#convertParallelToolCalls` | 待办（非本轮范围） | 本轮将多条 tool_use / tool_result 按“独立 user 消息”处理，但**未实现 Java 的「多 tool_use + 对应多 tool_result 交替拆分」**（Anthropic 要求 assistant(tool_use)→user(tool_result) 严格交替时）。触发条件苛刻（需 tool_use 数==tool_result 数且全部 id 可配），待后续轮次补齐。**2026-10-22 复核**：`anthropic/converter.rs` 有 `disable_parallel_tool_use` 的**请求侧**开关（`:361`），但**响应侧**的 tool_use/tool_result 交替拆分仍零实现——缺口成立 |
| anthropic 多智能体 system 前导文本细节 | `AnthropicMultiAgentFormatter` | 部分完成（非本轮范围） | 分组与 `<history>` 合并已对齐；Java 的 `isFirstAgentMessageGroup` 首组标记已实现，但**系统消息在组内的特殊转换路径较 Java 略简化**（本轮未逐条对齐其 ground-truth 测试） |
| **anthropic 内置 server tool 声明** | `anthropic.tool.AnthropicServerTool`（96 行）+ `formatter.AnthropicServerToolHelper`（333 行） | 待办 | **2026-10-22 复核新发现（真实缺口）**：Rust **解析侧**已能处理 `server_tool_use` / `web_search_tool_result` / `code_execution` / `bash_code_execution` / `text_editor_code_execution` / `tool_search` → `ToolResultBlock` + metadata（`anthropic/parser.rs:76-95`，`METADATA_SERVER_TOOL_RESULT`），**回显侧**也有 `AnthropicContentBlock::ServerToolUse`（`converter.rs:197`）；但**没有「把内置 server tool 声明进请求」的类型**——即 Java `AnthropicServerTool` 的等价物缺失。⚠️ 与 gemini 侧对比：`gemini::converter::GeminiServerTool` **已存在**（`GoogleSearch`/`UrlContext`/`CodeExecution`），anthropic 侧无对等物 |
| gemini 子模块整体 | `gemini.*`（**11** 类 / **3071** 行） | **已完成** | thought signature 持久化、thinking、多模态、multi-agent、内置 server tool 均已落地。**剩余 2 项细化缺口见下**（类数/行数已按 2026-10-22 复核更正） |
| gemini Vertex AI 路径 | `GeminiModelProvider`（`project`/`location`/`vertexAI` + `GoogleCredentials`） | 待办（设计决定：本轮不迁移） | Vertex AI 与 Gemini API 的端点主机与认证完全不同（OAuth2 服务账号）；离线 adapter **明确报错拒绝**而非静默降级（已有单测 `create_rejects_vertex_ai`）。需汇总方裁定是否另建 vertex 适配（见 §4.2）。**2026-10-22 复核**：`gemini/provider.rs:78` 读 `vertexAI` 选项、`:83` 显式报错，测试在 `:165`——叙述成立 |
| gemini 远程图片下载 | `GeminiMediaConverter#readFileAsBytes`（http/https） | 待办（非本轮范围） | Rust 侧拒绝 `http(s)` URL 并回退文本（避免隐式网络 I/O）；仅支持 base64/本地文件。需与 ollama 远程下载一并裁定统一 resolver（见 §3.3）。**2026-10-22 复核**：已改走共享 `common::resolve_media`（`gemini/converter.rs:59`），失败时经 `media_fallback` 回退文本并打 `unsupported_media_type` 诊断（`:78`）；仍**无内置**远程下载实现 |
| dashscope 子模块整体 | `dashscope.*`（29 类 / **8791** 行） | 部分完成 | **2026-10-22 复核更正**：旧记「含 `DashScopeEncryptionUtils` 对应物」**有误**——Rust 侧**没有任何 AES-GCM/RSA 加密实现**，仅 `PUBLIC_KEYS_ENDPOINT` 常量与 `DashScopePublicKeyResponse` DTO；`credential.rs` 只做 API key / base url 承载。`EndpointType`、多模态端点常量（`MULTIMODAL_GENERATION_ENDPOINT`）已落地。见 §2.2。**长尾**：`DashScopeMultiModalTool`（**93.5 KB / 1770 行**，本模块最大单文件）覆盖度待核 |
| **dashscope 会话合并 / 多智能体格式化** | `dashscope.formatter.DashScopeConversationMerger`（316 行）/ `DashScopeMultiAgentFormatter`（406 行） | 待办 | **2026-10-22 复核**：两者在 Rust 侧**零对应物**（anthropic/gemini 各有等价实现，dashscope 无）。直接影响多轮/多智能体长会话语义 |
| OpenAI 兼容厂商差异化行为 | `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 待确认 | 当前仅靠 `crate::openai::OpenAiProvider` 的 base_url + core `context_windows.rs` + `common::capabilities` 覆盖。**2026-10-22 复核**：`capabilities.rs` 的前缀表覆盖 deepseek/glm/zhipu/kimi/moonshot/qwen/llama/mistral/grok/gemma/phi/ollama → `max_tokens` 兼容；`openai/mod.rs` 有 DeepSeek `reasoning_content` 回传（`requires_reasoning_passback` / `inject_reasoning_content`）。**但 GLM/Kimi/MiniMax 各自的 `*Formatter` / `*MultiAgentFormatter` 差异化（thinking 处理、multi-agent 分组）仍无对应**——Java 侧 `compat/*` 与 `formatter/{DeepSeek,GLM}*` 两套并存共 12 类。是否影响功能正确性仍待汇总方裁定（见 §4.2） |
| OpenAI 多模态工具 | `openai.tool.OpenAIMultiModalTool`（**594 行 / 28.5 KB**） | 待确认 | core 与本 crate 均无对应；归属 MODEL 还是 TOOL 类模块未裁定。**2026-10-22 复核**：Java 侧该类体积确认（594 行），非可忽略长尾 |
| **`openai-official` 子模块整体** | `openaiofficial.*`（**14 类 / 3716 行**） | **待办** | **2026-10-22 复核新发现（真实缺口，见 §2.8）**：Java 侧 `openai-official` 为独立子模块，provider id `openai-official`、前缀 `openai-official:`、走官方 `openai-java` SDK，含 **multi-agent formatter**（`ResponsesMultiAgentFormatter`，355 行）、**server tool**（`tool.OpenAIServerTool`：web_search / code_interpreter / image_generation / tool_search）、**请求映射**（`ResponsesRequestMapper`，834 行）、**流式装配**（`ResponsesStreamingAssembler`，355 行）、**错误翻译**（`OpenAIErrorTranslator`）与 **additionalBodyParams 白名单 fail-fast**。Rust 侧 `crate::responses` **仅覆盖协议主干**（request/stream/parse），**多智能体分组、server tool 声明、body-params 白名单均无对应** |
| Java 测试用例逐条对照迁移 | Java 测试 **54660** 行（去空行）/ **63416** 行（`wc -l`；anthropic 6259 / dashscope 13933 / gemini 5769 / ollama 5711 / openai 15793 / openai-official 7395 / e2e 8556） | 待办 | 当前 **267** 个 Rust 测试属性为**功能等价重写**（覆盖 common + ollama + dashscope + anthropic + gemini + 自 core 迁入的 openai/responses/anthropic_async + relay + 集成），未逐条照搬；anthropic/gemini 的 ground-truth formatter 测试（如 `AnthropicChatFormatterGroundTruthTest`、`GeminiPythonConsistencyTest`）尚未逐条对照。**2026-10-22 复核**：旧记「Java 测试 45043 行」及各子模块旧值均已过期，本行已按实测更新 |
| 真实厂商端点联调 | `e2e-tests`（**36 文件 / 17 个含 `@Test` 的测试类** / 8556 行） | 设计决定（不迁移） | 需要真实 API key（7 个厂商 env）+ `ENABLE_E2E`；Rust 侧坚持离线策略，网络层已实现但未对真实端点回归。**2026-10-22 复核**：该前提已核实成立，结论维持不变 |

### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama llama.cpp loader 长尾参数 | `ollama.options.OllamaOptions`（**45.0 KB / 1361 行**，为 ollama 子模块最大单类） | 待办 | Rust 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`（源码 doc 已声明为 P2）。**2026-10-22 复核**：`ollama/options.rs`（264 行）含 `OllamaOptions` / `ThinkOption`（`Boolean`/`Level`）/ `to_options_map` / `merge` / `to_json_value` |
| 统一远程媒体 resolver | `MediaUtils` + 各 `*MediaConverter` | 待办（非本轮范围） | **2026-10-22 复核（部分解决）**：统一 seam **已落地**为 `common::media`（`MediaResolver` trait + `set_media_resolver` / `ensure_media_resolver` / `resolve_media` / `DefaultMediaResolver`），gemini（`converter.rs:59`）与 ollama（`converter.rs:39`）已改走该 seam；anthropic 仍走「远程 URL 透传」路径。**剩余缺口**：**无内置远程下载实现**——宿主需自行注册 resolver（`media.rs` 明确说明 resolver 是**同步** trait，异步下载需宿主预先缓存） |
| 重试 / 退避 / 超时可配置化 | — | 部分完成 | **2026-10-22 复核更正**：旧记「当前无重试；超时未暴露为可配置项」**已过时**。`common/transport.rs` 的 `TransportConfig` 已含 `connect_timeout` / `read_timeout` / `tcp_keepalive` / `max_retries`（默认 2）/ `initial_backoff`（500ms）/ `max_backoff`（8s）/ `backoff_multiplier`（2.0），`ReqwestTransport::post_json` 依 `ModelError::retryable()` + 指数退避重试（`transport.rs:267-283`）；4 个 provider 的 `default_transport()` 已改经 `ReqwestTransport::new()`。**剩余缺口**：超时/重试**未暴露到 `GenerateOptions`**（仍需构造 `TransportConfig` 注入），且**流式不重试**（`end_reason` 语义未引入，见 §6.3-3） |
| 指标 / 可观测性埋点 | slf4j 日志 | 部分完成 | **2026-10-22 复核更正**：旧记「`tracing` 依赖已声明但本 crate 代码未埋点」**已过时**。`common/diagnostics.rs` 已有 `ConversionDiagnostic` + `emit`/`warn`/`info`（`tracing::info!/warn!/error!` 落点），实测 **9 处**调用：`tool_choice_downgraded`（dashscope/ollama）、`tool_choice_omitted`（openai/responses）、`sampling_unsupported`（openai/responses）、`unsupported_media_type`（anthropic ×2 / gemini）。**剩余缺口**：`Diagnostics` 收集器（结构化收集而非仅打日志）在 provider 层**未被使用**（仅 `common` 内部 + `lib.rs` re-export） |
| `agentscope-harness` 依赖清理 | — | **已完成**（2026-09-21） | `crates/agentscope-extensions-model/Cargo.toml` 已移除 `agentscope-harness`（此前 `src/**` 零引用）；见提交 `d86a578`。**2026-10-22 复核**：`Cargo.toml` 现有 path 依赖仅 `agentscope-core`；源码中 `agentscope_harness` 出现 **0** 次——叙述成立 |
| 独立 `tests/` 集成测试 | — | **已完成**（2026-10-12） | **2026-10-22 复核更正**：旧记「当前 198 个单测全部内联…无 `tests/` 目录」**已过时**。`tests/model_extensions_integration.rs` **已存在**（物理 135 行 / 去空行 117 行），含 **5 个集成测试**（resolver 注入 / 错误归一 / 四协议反向编码 / 路由质量 / 能力+用量+命名），验证的是 crate 的**公开导出面**（`use agentscope_extensions_model::{...}`） |
| **`ModelMap` / `strip_reasoning_suffix` 未接线** | — | **待办**（2026-10-22 新发现） | `common/model_map.rs`（191 行）实现了链式映射 + 环检测 + base-name 回退 + `-thinking-N` / `-<effort>` 后缀剥离，但**除 `lib.rs` re-export 与 `tests/` 集成测试外，全 crate 无任何 provider 消费它**（`ModelMap` / `strip_reasoning_suffix` 在 `src/` 中的引用仅 `lib.rs` + `common/mod.rs`）——即「模型名映射 / 推理档位剥离」能力**尚未接入任何 provider 的请求构建** |
| **`base64` / `chrono` / `thiserror` 依赖未使用** | — | **待办**（2026-10-22 新发现，轻量） | `Cargo.toml` 声明了 `base64 0.22` / `chrono`（workspace）/ `thiserror`（workspace），但源码中 `base64::` / `chrono::` / `thiserror` 出现次数均为 **0**。`ModelError` 是**手写 `Display` 实现**（非 `thiserror` derive），媒体 base64 编码由 core `formatter::media_utils::file_to_base64` 承担。可择机清理或改为真用 |

### 3.4 设计决定（不迁移）汇总
| 项 | Java 参考 | 理由 |
|---|---|---|
| OpenAI 全子模块（53 类）的「逐类 1:1 迁移」 | `openai.OpenAIChatModel` / `OpenAIClient` / `dto.*` / `formatter.*` / `compat.*` | **原「不迁移」理由已作废（2026-09-21，`d86a578`）**：实现体已自 core 迁入本 crate（`src/openai/mod.rs`），core 仅留 trait/注册表/SPI。但 Java 53 类仍**未逐类对照迁移**——`compat.*` 12 类、`exception.*` 8 类、`OpenAIMultiModalTool` 仍为开放项（§3.2/§3.4 下表） |
| OpenAI 异常类层次（8 类） | `openai.exception.*` | Rust 以 `agentscope_core::Error`（`thiserror` 枚举）统一表达，不建平行异常继承体系。**2026-10-22 复核**：更细的厂商错误归一改由 `common::error::ModelError` + `ErrorKind`（**14 个分类**：Authentication/Permission/RateLimit/Quota/ContextLength/InvalidRequest/NotFound/ContentFilter/Overloaded/Server/Timeout/Network/Cancelled/Unknown）承担，`retryable()` 覆盖 RateLimit/Overloaded/Server/Timeout/Network——分类能力已等价且更细 |
| SPI `META-INF/services` 注册机制 | 各子模块 `resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider` | Rust 无 `ServiceLoader`；改为显式 `ModelRegistry::register(Arc<dyn ModelProvider>)`。**2026-10-22 复核**：Java 侧该文件共 **6 份**（anthropic / dashscope / gemini / ollama / openai / openai-official 各 1；openai 的为 6 行、其余各 2 行），Rust 侧对应为 `default_registry` / `default_responses_registry` / `default_anthropic_registry` 三个显式构造函数 + `ModelRegistry::register` |
| Spring 自动装配 / DI | 本模块 `pom.xml` 无 Spring 依赖 | **已完成（设计决定，不迁移）**：不适用；Rust 侧由调用方显式构造 provider |
| Ollama 凭据类型 | `ollama.credential.OllamaCredential` | **已完成（设计决定，不迁移）**：本地 Ollama 无鉴权头；凭据经 `ModelCreationContext.api_key`/环境变量表达，不引入平行类型 |
| e2e-tests 真实网络回归（**36 文件 / 17 个含 `@Test` 的测试类 / 8556 行**） | `agentscope-extensions-model-e2e-tests` | **已完成（设计决定，不迁移）**：需真实 API key（7 个厂商 env）+ `ENABLE_E2E`，无法在离线 CI 复现；改为 `HttpTransport` seam + `MemoryTransport` 离线策略，各 provider 的请求/响应/流式分支已有等价单测。**2026-10-22 复核**：已核实 `ProviderFactory` / `E2ETestCondition` 的密钥与开关依赖，结论维持 |
| anthropic / gemini 官方 SDK 类型（`AnthropicOkHttpClient` / `com.google.genai.Client`）不引入 | 两子模块 `pom.xml` | **已完成（设计决定）**：经 §4.2 裁定采用选项 (b)，用 `HttpTransport` + 自建 wire DTO 替代；避免引入新第三方依赖与机器绑定 |
| Gemini Vertex AI（`project`/`location`/OAuth2 credentials） | `GeminiModelProvider` | **已完成（设计决定：本轮不迁移）**：端点为 Vertex AI 专用主机 + OAuth2 服务账号，与 API-key 流程正交；离线 adapter **显式报错拒绝**（单测 `create_rejects_vertex_ai`）避免静默降级。后续若需，另立适配 |
| Anthropic SDK 的 `ProxyConfig` / Gemini SDK 的 `ProxyOptions` | 两子模块构造器 | **已完成（设计决定，不迁移）**：Rust 侧代理由调用方通过 `ReqwestTransport::with_client` 注入 `reqwest::Client`（支持代理/超时/TLS），不在 provider 层重复建模 |
| **`openai-official` 的「官方 SDK 路径」不引入** | `openai-official` 子模块 `pom.xml`（依赖 `openai-java` SDK） | **待裁定（2026-10-22 复核新增）**：Java 侧用官方 `openai-java` SDK；Rust 侧 `crate::responses` 走 `async-openai` 0.41。**是否另开一条 SDK 路径**（与 `anthropic` vs `anthropic_async` 同类问题）需汇总方裁定；但**该子模块的 multi-agent formatter / server tool / body-params 白名单等能力缺口不因 SDK 选型而消失**，应单独立项 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**
  1. **`common` 层**：`HttpTransport` trait（`post_json` / `post_json_stream`）+ `HttpRequestSpec` + `TransportConfig`（connect/read/keepalive/retry）+ `ReqwestTransport`（真实 reqwest，**主体走 workspace 0.13**）+ `MemoryTransport`（离线 fake）+ `decode_stream_line`（NDJSON / SSE `data:` 统一解码）+ `ModelError`/`ErrorKind` + `ModelCapabilities` + `merge_non_zero` + `ConversionDiagnostic` + `MediaResolver` + `ModelMap` + `resolve_base_url`。这是全 crate 的离线可测边界。
  2. **`ollama` 子模块**：`OllamaChatModel`（实现 core `Model`，端点 `/api/chat`，流式与非流式，**含 `embed` / `embed_with_options` → `/api/embed`**）、`OllamaModelProvider`（实现 core `ModelProvider`，`ollama:` 前缀 + 裸 id 兜底 + `OLLAMA_BASE_URL`）、9 个 DTO、`OllamaOptions`/`ThinkOption`、消息/工具转换、响应解析与流式增量器。
  3. **`dashscope` 子模块**：provider 全链路（`converter`/`credential`/`dto`/`model`/`parser`/`provider` 六模块，**39** 个单测）。
  4. **`anthropic` 子模块**：provider 全链路（`dto`/`credential`/`converter`/`parser`/`model`/`provider` 六模块，**61** 个单测）。覆盖：system 提升到顶层 `system`、prompt caching（`cache_control`/TTL，标记 system/末条消息/末个工具）、extended thinking（`thinking.budget_tokens`）、`tool_choice` 四变体与 `disable_parallel_tool_use`、tool_result 拆为独立 user 消息、多模态（base64/本地→base64，远程 URL 透传）、多智能体 `<history>` 合并、**server-tool 块回显与结果映射**、SSE 流式增量器（text/thinking 即时产出，tool 参数分片缓冲，usage 合并 message_start+message_delta）。
  5. **`gemini` 子模块**：provider 全链路（同构六模块，**58** 个单测）。覆盖：REST `generateContent`/`streamGenerateContent?alt=sse`、`generationConfig`（temperature/topP/topK/seed/maxOutputTokens/penalty/thinkingConfig）、`parametersJsonSchema` 工具声明与 `functionCallingConfig`（NONE/ANY/allowedFunctionNames）、tool result 独立 `user` content、`thoughtSignature` 持久化与回放、多模态 `inlineData`、多智能体 `<history>` 合并、**内置 server tool（`GeminiServerTool` / `merge_server_tools`）**、流式增量器。
  6. **`relay` 网关化前置**（2026-10-12）：`encode_response`（IR → OpenAI Chat / Responses / Claude / Gemini）+ `encode_error` + `BUILTIN_CONVERTERS` 路由注册表（8 条边，含 `Quality` 评级与显式多跳 `steps`）+ `find_route` + `stream_terminator`。
- **验证命令与结果**
  - （2026-10-22 静态复核，**未编译**）测试属性静态计数 **267**（262 内联 + 5 集成）；42 个源文件 / 15004 物理行（去空行 13833）。真实 `passed` 数待下次编译轮确认。
  - （2026-10-12 优化轮实测，对标 new-api；详见 §6）`cargo test -p agentscope-extensions-model` → **236 passed / 0 failed**（231 lib + 5 集成）；`cargo check --workspace` → Finished（仅其它 crate 存量 warning）
  - （2026-09-19 实测）`cargo build -p agentscope-extensions-model` → **Finished**（0 error；本 crate 0 error/0 warning）
  - （2026-09-19 实测）`cargo test -p agentscope-extensions-model` → **169 passed / 0 failed / 0 ignored**
  - （2026-09-21 实测，core 实现迁入后）`cargo test -p agentscope-extensions-model` → **198 passed / 0 failed / 0 ignored**；`cargo check --workspace` → **Finished**（0 error，仅其它 crate 存量 warning）
- **已知问题**
  1. ~~`src/ollama/dto.rs` 的 embedding DTO 与 `EMBED_ENDPOINT` 常量无调用方（§3.2）。~~ **已解决**（2026-10-12，`OllamaChatModel::embed` 已接线并有单测）。
  2. ~~`agentscope-harness` 为未使用依赖（§3.3）。~~ **已解决**（2026-09-21 从 `Cargo.toml` 移除，`d86a578`）。
  3. anthropic 并行 tool-use 拆分、gemini Vertex AI 与**内置**远程媒体下载为**非本轮范围**细化缺口（§3.2/§3.3）。
  4. **（2026-10-22 复核新增）`openai-official` 子模块（14 类 / 3716 行）完全未对照**——Rust `crate::responses` 只覆盖 Responses 协议主干，缺 multi-agent formatter / server tool / body-params 白名单（§2.8）。
  5. **（2026-10-22 复核新增）`ModelMap` / `strip_reasoning_suffix` 未被任何 provider 消费**（§3.3）。
  6. **（2026-10-22 复核新增）`base64` / `chrono` / `thiserror` 三个依赖声明但零使用**（§3.3）。
  7. 无阻塞：上次 build/test 全绿；本次为静态复核，未重新编译。

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

## 6. 2026-10-12 优化轮（对标 Go 网关 new-api）

> 参考 `E:\local-github\new-api` 的协议转换/错误/用量/能力设计，在本 crate 内完成一轮“客户端保真度 + 可观测性 + 网关化前置”优化。**全程未改动 core/harness/根 Cargo.toml**；`cargo test -p agentscope-extensions-model` → **236 passed**（231 lib + 5 集成），`cargo check --workspace` → Finished（仅存量 warning）。

### 6.1 新增模块
| 文件 | 职责 |
|---|---|
| `src/common/error.rs` | `ModelError` + `ErrorKind`：归一化 8 种厂商错误体字段（`error.message`/`message`/`msg`/`err`/`errorMsg`/`detail`/`header.message`/`response.error.message`），分类（auth/quota/rate_limit/context_length/…）与 `retryable()`；`parse_stream_value` 处理流内 `error` 帧 |
| `src/common/capabilities.rs` | `ModelCapabilities` + `resolve_capabilities`（最长前缀匹配）：推理模型禁 sampling/penalty/seed；兼容网关用 `max_tokens` 而非 `max_completion_tokens` |
| `src/common/usage.rs` | `merge_non_zero`（后到非零才覆盖，0 不清正） |
| `src/common/diagnostics.rs` | `ConversionDiagnostic{code,path,message,severity}` + `emit`/`warn`/`info`（tracing 落点）+ `Diagnostics` 收集器 |
| `src/common/media.rs` | `MediaResolver` seam + 全局 `set_media_resolver`：「base64 直通 / 本地文件读取 / 远程 URL 显式拒绝」统一实现 |
| `src/common/model_map.rs` | `ModelMap`（链式映射 + 环检测 + base-name 回退）、`strip_reasoning_suffix`（`-thinking-N` / `-<effort>`） |
| `src/relay.rs` | 反向编码器 `encode_response`（IR → OpenAI Chat / Responses / Claude / Gemini）+ `encode_error` + 路由注册表 `BUILTIN_CONVERTERS`（含 `Quality` 与显式多跳 `steps`） |

### 6.2 改动落点（对应 §3 缺口）
- **P0 错误归一化**：`common/transport.rs` 非 2xx 与流内 `error` 帧经 `ModelError`；`openai::map_openai_error` / gemini / anthropic / dashscope / ollama 全部改为结构化错误（§3.2「OpenAI 异常类层次」细化）。
- **P0 超时统一**：`TransportConfig{connect/read/keepalive}`；4 个 provider 的 `default_transport()` 改经 `ReqwestTransport::new()`（此前**零超时**）；`anthropic_async` 的 reqwest client 同步套用（§3.3「超时未可配置」）。
- **P0 重试**：`ReqwestTransport::post_json` 依 `ModelError::retryable()` + 指数退避重试（默认 2 次）；流式不重试（§3.3「当前无重试」部分解决）。
- **P0 usage 合并**：openai / responses / gemini / dashscope / ollama 增量器改用 `merge_non_zero`（anthropic 两条路径原本已正确合并）。
- **P1 能力降级**：openai / responses 的请求构建按 `resolve_capabilities` 过滤参数（§3.2「OpenAI 兼容厂商差异化行为」的运行时实现）。
- **P1 诊断**：`tool_choice` 因无工具被丢弃、`Required→auto` 降级（dashscope/ollama）、媒体回退、sampling 参数被丢弃等一律 `diagnostics::warn/info`；本 crate 首次引入 `tracing` 落点（§3.3「指标/可观测性埋点」）。
- **P1 媒体**：gemini `media_part`/`data_part`、ollama `DefaultImageResolver` 改走共享 `resolve_media`（§3.2/§3.3「远程媒体下载」— 由宿主注入 resolver 后即支持）。
- **P1 端点**：Ollama `embed`/`embed_with_options` 接线 `POST /api/embed`（§3.2「Ollama embedding 调用链」）。
- **P2 反向编码 + 注册表**：`relay.rs`（网关化前置，无需改 core）。
- **P2 收敛**：`anthropic_async` 复用 `TransportConfig` 超时；`anthropic` vs `anthropic_async`、`reqwest 0.12 vs reqwest13` 的取舍记入下方 §6.3。
- **测试**：新增 `tests/model_extensions_integration.rs`（5 例，覆盖 resolver 注入、错误归一化、四协议反向编码、路由质量、能力/用量/命名）；各 provider 内联单测同步扩充（+9）。

### 6.3 仍开放 / 有意保留
> **2026-10-22 静态复核逐条核对结果**：§6.2 声称的 **10 项能力全部能在代码中找到对应符号**（错误归一 / 超时 / 重试 / usage 合并 / 能力降级 / 诊断 / 媒体 resolver / 反向编码 / 路由注册表 / Ollama embedding），逐项定位见文末复核记录 §7.3。
1. **`anthropic` 双实现**：保留 `anthropic`（自建 wire DTO，规范路径，61 测试）与 `anthropic_async`（SDK 路径，712 行 / 8 测试），两者共用超时配置（`anthropic_async/mod.rs:110` 读 `TransportConfig::default()`）；是否下线其一仍待汇总方裁定（§4.2-7）。
2. **`reqwest` 0.12 + 0.13 双栈**：⚠️ **2026-10-22 勘误**——旧记「本 crate 自有 HTTP 走 0.12」**已过时**。根 `Cargo.toml` 已于 `43cd044` 全局统一 `reqwest = 0.13`；本 crate 主体（`common::transport` / 4 个 provider）走 **workspace 0.13**，仅 `anthropic_async` 用别名 `reqwest12 = { package = "reqwest", version = "0.12" }`（因 `async-anthropic` 0.6 钉住 0.12，`Cargo.toml` 已注释说明）。`Cargo.toml` 注释里的「reqwest13」旧称亦已过时。
3. **流式重试**：仅在**首包前**安全；当前实现对流式不做重试（`end_reason` 语义未引入），保持保守。**2026-10-22 复核**：`transport.rs` 的重试循环仅在 `post_json`（非流式）路径，`post_json_stream` 无重试——叙述成立。
4. **反向编码仅覆盖非流式体**：SSE 逐事件编码（Claude `content_block_delta` 等）与真正的 OpenAI 兼容 HTTP 端点尚未落地——`relay.rs` 提供构建块与 `stream_terminator`。**2026-10-22 复核**：`relay.rs` 的 `encode_openai_chat` / `encode_claude_messages` / `encode_gemini` / `encode_openai_responses` 四个编码器均为**整包编码**，`stream_terminator` 只返回协议终止片段（`"[DONE]"` 等），**无逐事件 SSE 编码**——叙述成立。
5. e2e 真实端点回归、`OpenAIMultiModalTool` 归属仍按 §4.2 待裁定。**2026-10-22 新增**：`openai-official` 子模块对照（§2.8）与 `responses::map_openai_error` 的错误归一不一致（§2.8 `OpenAIErrorTranslator` 行）亦待处理。

---

## 7. 静态复核记录（2026-10-22）

### 7.1 复核方式
- **静态核对，未执行 `cargo build` / `cargo test`**（按本轮约定）。所有「已完成 / 已就位」结论均基于**源码符号检索**（`Select-String` 全量扫描 `src/` + `tests/` + `examples/`）与**行数/类数清点**。
- **Java 侧**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-model\`（HEAD = `ed8073259`）；**Rust 侧**：`E:\local-github\agentscope-rust\crates\agentscope-extensions-model\`（HEAD = `71f443c`，crate 目录工作区**无未提交改动**）。
- **行数双口径**：本文档同时给出 **物理行数**（含空行，≈`wc -l`）与**去空行数**（PowerShell `Measure-Object -Line`）。跨会话引用时**必须标明口径**，否则会产生「15004 vs 13833」「33819 vs 30058」这类伪冲突。

### 7.2 实测统计

**Java（`wc -l` 物理行 / 去空行）**
| 子模块 | 主代码 文件/行 | 测试 文件/行 | 去空行 主/测 |
|---|---|---|---|
| anthropic | 13 / 3171 | 16 / 6259 | 2886 / 5413 |
| dashscope | 29 / 8791 | 23 / 13933 | 7819 / 12038 |
| gemini | **11** / **3071** | 16 / **5769** | 2757 / 4925 |
| ollama | 22 / 5217 | 16 / 5711 | 4530 / 4974 |
| openai | 53 / 9853 | 45 / 15793 | 8737 / 13364 |
| openai-official | 14 / 3716 | 15 / 7395 | 3329 / 6569 |
| e2e-tests | 0 / 0 | **36 / 8556**（其中 **17** 个含 `@Test`） | 0 / 7377 |
| **合计** | **142 / 33819** | **151 / 63416** | **26729 / 54660** |

> rollup 记的 **24885 / 45043** 确已过期（那是上一版 Java 快照的行数）。本次实测 **30058 / 54660**（去空行口径，与 rollup 同源）与 **33819 / 63416**（物理行口径）均成立，取决于口径。

**Rust（`agentscope-extensions-model`）**
| 项 | 物理行 | 去空行 |
|---|---|---|
| `src/` **42** 个 `.rs` | **15004** | **13833** |
| `tests/` 1 个 `.rs` | 135 | 117 |
| `examples/chat_demo.rs` | 62 | 55 |
| **测试属性** | **267**（`#[test]` + `#[tokio::test]`：内联 **262** + 集成 **5**） | — |
| `crates/agentscope-core/src/model/` | **10** 个 `.rs` / **1117** 行（`mod.rs` 30 + `cache_policy` 12 / `chat_response` 110 / `context_windows` 90 / `execution_config` 135 / `generate_options` 161 / `model`(trait) 77 / `provider`(SPI+registry) 371 / `tool_choice` 33 / `tool_schema` 98） | — |

> ✅ core 叙述**核实成立**：`crates/agentscope-core/src/model/` 下 **不存在** `openai.rs` / `responses.rs` / `anthropic.rs`；`model/mod.rs` 的文档注释明写「This module only carries the **interface abstraction** (trait, SPI, DTOs). Concrete provider implementations live in `agentscope-extensions-model`」。全 workspace 检索 `OpenAiChatModel` / `OpenAiProvider` / `OpenAiResponsesModel` / `AnthropicMessagesModel` 在 core 中**零定义**（仅 `formatter/base_formatter.rs:6` 的一句文档提及）。

### 7.3 逐子模块对齐结论表
| 子模块 | Java 类数 | Rust 对应物 | 结论 | 真实缺口 |
|---|---|---|---|---|
| **ollama** | 22 / 5217 行 | `src/ollama/*` 7 文件 / 1641 行 | **部分完成** | `OllamaConversationMerger`（116 行）、`OllamaMultiAgentFormatter`（**508 行**）均**零对应**；`OllamaOptions`（1361 行）仅迁子集 |
| **dashscope** | 29 / 8791 行 | `src/dashscope/*` 7 文件 / 2469 行 | **部分完成** | `DashScopeEncryptionUtils`（**AES-GCM + RSA，零实现**）、`DashScopeConversationMerger`（316 行）、`DashScopeMultiAgentFormatter`（406 行）、`DashScopeMultiModalTool`（**1770 行 / 93.5 KB**） |
| **anthropic** | 13 / 3171 行 | `src/anthropic/*` 7 文件 / 3002 行 | **部分完成** | `tool.AnthropicServerTool`（96 行）**无等价物**（gemini 侧已有 `GeminiServerTool`）；响应侧并行 tool-use 交替拆分未实现 |
| **gemini** | 11 / 3071 行 | `src/gemini/*` 7 文件 / 2682 行 | **部分完成（最完整）** | Vertex AI（设计决定拒绝）；**内置**远程媒体下载缺省（seam 已有） |
| **openai** | 53 / 9853 行 | `src/openai/mod.rs` 1380 行（**源自 core 迁入，非逐类重写**） | **部分完成** | `compat.*` 12 类（GLM/Kimi/MiniMax formatter 差异化）、`credential.OpenAICredential`、`tool.OpenAIMultiModalTool`（594 行）、`OpenAIConversationMerger` / `OpenAIMultiAgentFormatter` |
| **responses**（Rust 独有） | — | `src/responses/mod.rs` 691 行 | **已就位（协议主干）** | 无 multi-agent 分组、无 server tool、`map_openai_error` 未走 `ModelError` 结构化归一 |
| **anthropic_async**（Rust 独有） | — | `src/anthropic_async/mod.rs` 712 行 | **已就位（SDK 路径）** | 与 `crate::anthropic` 双实现并存，待裁定（§4.2-7） |
| **openai-official** | **14 / 3716 行** | **无对应物** | **未对照** | 全部 14 类；见 §2.8（multi-agent formatter 355 行 / request mapper 834 行 / server tool 669 行 / streaming assembler 355 行 / body-params 白名单） |
| **e2e-tests** | 0 / 36 文件 / 8556 行 | 无 | **设计决定（不迁移）——核实成立** | 需 7 个厂商 API key + `ENABLE_E2E`（`ProviderFactory` / `E2ETestCondition` 已核实） |

### 7.4 §6「对标 new-api」十项能力符号定位（逐条核实，**全部命中**）
| §6.2 声称 | 对应符号 | 位置 |
|---|---|---|
| 错误归一（8 种厂商错误体字段） | `ModelError::parse` + `extract_message` 的 `PATHS`（`error.message` / `response.error.message` / `header.message` / `error.msg` / `message` / `msg` / `errorMsg` / `error_msg` / `err` / `detail`） | `src/common/error.rs:281-296`、`:456`；`ModelError::parse_stream_value` `:479` || 超时统一 | `TransportConfig{connect_timeout, read_timeout, tcp_keepalive}` + `ReqwestTransport::new()`；4 个 provider 的 `default_transport()` | `transport.rs:30-46`、`:48`、`:204`；`anthropic/model.rs:242`、`dashscope/model.rs:248`、`gemini/model.rs:226`、`ollama/model.rs:219`；`anthropic_async/mod.rs:110-114` |
| 重试 | `TransportConfig{max_retries:2, initial_backoff:500ms, max_backoff:8s, backoff_multiplier:2.0}` + `ModelError::retryable()` + `TransportConfig::backoff` | `transport.rs:37-57`、`:64`、`:267-283`；`error.rs:78-90` |
| usage 合并 | `merge_non_zero` / `merge_into` | `common/usage.rs:7`、`:34`；消费点 `openai/mod.rs:829`、`responses/mod.rs:451`、`gemini/parser.rs:326`、`dashscope/parser.rs:175`、`ollama/parser.rs:156` |
| 能力降级 | `resolve_capabilities`（最长前缀匹配）+ `ModelCapabilities::use_max_completion_tokens` | `common/capabilities.rs:153`、`:122-135`；消费点 `openai/mod.rs:51,393`、`responses/mod.rs:45,279` |
| 诊断 | `ConversionDiagnostic` + `emit`/`warn`/`info`（`tracing::info!/warn!/error!`） | `common/diagnostics.rs:57`、`:81`、`:86`；调用 9 处（见 §3.3） |
| 媒体 resolver | `MediaResolver` + `set_media_resolver` + `resolve_media` + `DefaultMediaResolver` | `common/media.rs:37`、`:70`、`:102`；消费点 `gemini/converter.rs:59`、`ollama/converter.rs:39` |
| 反向编码 | `encode_response` + `encode_error` + `stream_terminator` + 4 个协议编码器 | `relay.rs:152`、`:175`、`:167`、`:237`、`:276`、`:304`、`:328` |
| 路由注册表 | `BUILTIN_CONVERTERS`（8 条边，含 `ConverterQuality` 与显式多跳 `steps`）+ `find_route` | `relay.rs:84-141`、`:144` |
| Ollama embedding | `OllamaChatModel::embed` / `embed_with_options` → `POST {base_url}/api/embed` | `ollama/model.rs:73`、`:78`、`:86`（`EMBED_ENDPOINT`）；单测 `:233` |

> **结论：§6.2 的十项声明全部可在代码中定位到对应符号，叙述成立。** 但 §6.3-2 的 `reqwest` 版本描述已过期（见 §7.5 勘误 3）。

### 7.5 新发现缺口

**P0（阻塞核心链路）**
1. **`openai-official` 子模块（14 类 / 3716 行）完全未对照**——provider id（`openai-official` vs `openai-responses`）、id 前缀、SDK 三者全不同，`crate::responses` 不可顶替。详见 §2.8。

**P1（重要功能）**
2. **`dashscope` 请求加密零实现**——Java `DashScopeEncryptionUtils`（181 行，AES-GCM 数据 + RSA 包 AES key）在 Rust 侧只有 `PUBLIC_KEYS_ENDPOINT` 常量与 `DashScopePublicKeyResponse` DTO，**无任何加密/解密代码**（`dashscope/dto.rs` / `model.rs` 全文无 `aes`/`rsa`/`encrypt`/`decrypt`）。
3. **`dashscope` 会话合并 / 多智能体格式化缺失**（`ConversationMerger` 316 行 + `MultiAgentFormatter` 406 行）——anthropic/gemini 各有等价物，dashscope 无。
4. **`ollama` 会话合并 / 多智能体格式化缺失**（`ConversationMerger` 116 行 + `MultiAgentFormatter` **508 行**）。
5. **`anthropic` 内置 server tool 声明缺失**（`tool.AnthropicServerTool` 96 行）——解析/回显侧已实现，但**无法把内置 server tool 声明进请求**；gemini 侧已有 `GeminiServerTool` 可作模板。
6. **`responses` 多智能体分组 + server tool 声明缺失**（Java `ResponsesMultiAgentFormatter` 355 行 + `OpenAIServerTool` 125 行 + `ResponsesServerToolHelper` 544 行）；`to_request_tools` 仅构造 `Tool::Function`。
7. **`responses::map_openai_error` 未走结构化错误归一**——与 `openai/mod.rs:338` 的 `ModelError::from_parts` + `ErrorKind::Timeout/Network` 实现**不一致**，导致 responses 路径的 429/5xx 无法进入 `retryable()` 判定与统一分类。

**P2（增强/清理）**
8. **`ModelMap` / `strip_reasoning_suffix` 未被任何 provider 消费**——191 行实现（含环检测 + base-name 回退 + 推理档位后缀剥离）在 `src/` 中零调用点，能力「已实现但未接线」。
9. **`base64` / `chrono` / `thiserror` 三依赖声明但零使用**——`ModelError` 是手写 `Display`（非 `thiserror` derive）；base64 编码实际由 core `formatter::media_utils::file_to_base64` 承担。
10. **`Diagnostics` 结构化收集器在 provider 层未被使用**——仅 `common/diagnostics.rs` 内部 + `lib.rs` re-export；provider 侧只调 `warn`/`info` 打日志（9 处）。`ensure_media_resolver`（`media.rs:92`）同样只在 `common` 内部与集成测试中出现。
11. **`embed_with_options` 的 `options` 参数被显式忽略**（`ollama/model.rs:90` `let _ = options;`），per-call options 预留未接线。
12. **`base_url` / `model_map` 的部分 API 无 provider 调用点**——`resolve_base_url` 有 4 个 provider 调用点（`anthropic/provider.rs:99`、`dashscope/provider.rs:90`、`gemini/provider.rs:102`、`openai/mod.rs:157`），但 `resolve_base_url_with`（可注入读取器版本）**仅在 `common/base_url.rs` 内部**；`ModelMap::from_json`（从 JSON 配置构建映射）**仅 `common/model_map.rs:173` 一处**（自身单测）——即「从配置加载模型名映射」的入口**无 provider 消费**。

### 7.6 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 / §1.2 | Java「主 24885 / 测 45043」 | **已过期**。实测 **33819 / 63416**（物理行）或 **30058 / 54660**（去空行）——rollup 的 30058/54660 正确 |
| 2 | §1.2 | `tests/model_extensions_integration.rs` **117 行** | 物理 **135** 行 / 去空行 117 行——与同表 `src/` 各行的物理行口径不一致，已改为 135 |
| 3 | §1.2 | `src/dashscope/dto.rs`「17 个 pub 类型」 | **18 个**（15 Java 对应 + `DashScopeContent` / `PublicKeyData` / `CacheControl`） |
| 4 | §2.5 | gemini「**10 类 / 2294 行**」 | **11 类 / 3071 行**（漏记 `tool.GeminiServerTool`） |
| 5 | §2.5 | gemini 状态「**已完成**」 | 降级为「**部分完成**」（Vertex AI + 内置远程下载为遗留缺口） |
| 6 | §2.6 | e2e-tests「**34 测试类 / 7136 行**」 | **36 文件 / 8556 行**，其中含 `@Test` 的真正测试类 **17** 个 |
| 7 | §2.7 | 「`reqwest` **0.12**（本 crate `Cargo.toml`）」 | 本 crate 主体走 **workspace reqwest 0.13**（根 `Cargo.toml` 已于 `43cd044` 统一）；0.12 仅以别名 `reqwest12` 保留给 `anthropic_async` |
| 8 | §2.7 | 「`base64` **0.22**（本 crate）」 | 声明存在但源码中 **零直接引用**（编码实际由 core `file_to_base64` 承担） |
| 9 | §2.7 | path 依赖「`../agentscope-core`、`../agentscope-harness`」 | 仅 `../agentscope-core`；`agentscope-harness` 已于 2026-09-21 移除（§3.3 的记载正确，此表未同步） |
| 10 | §3.1 | core `ModelProvider` / `ModelRegistry` 行号 `:78` / `:95` | `provider.rs:126` / `:143`（`register` 在 `:154`），文件已扩至 371 行 |
| 11 | §3.1 | 「离线单测 **198 个**」及逐文件拆分 | **267** 个测试属性（262 内联 + 5 集成）；逐模块 common 34 / ollama 27 / dashscope 39 / anthropic 61 / gemini 58 / openai 24 / responses 6 / anthropic_async 8 / relay 5 |
| 12 | §3.1 | 「dashscope DTO 接线：15 个 pub 类型」 | **18 个** |
| 13 | §3.1 | anthropic 基线「11 类 / 58 个单测」 | **13 类 / 61** 个单测 |
| 14 | §3.1 | gemini 基线「10 类 / 53 个单测」 | **11 类 / 58** 个单测 |
| 15 | §3.1 | OpenAI 行「`openai/mod.rs` 1045 行 / `responses` 620 / `anthropic_async` 653」 | **1380 / 691 / 712** |
| 16 | §3.2 | Ollama embedding「**无客户端方法、无调用方、无单测**」 | **已接线**：`embed` / `embed_with_options` + 单测 `embed_posts_to_embed_endpoint`（`ollama/model.rs:73/78/233`） |
| 17 | §3.2 | dashscope 整体「**已完成**；含 `DashScopeEncryptionUtils` 对应物」 | 降级为「**部分完成**」——**无任何加密实现** |
| 18 | §3.2 | anthropic 子模块「11 类 / 2192 行」 | **13 类 / 3171 行** |
| 19 | §3.2 | gemini 子模块「10 类 / 2294 行」 | **11 类 / 3071 行** |
| 20 | §3.2 | Java 测试基线「45043 行（anthropic 4677 / dashscope 11767 / gemini 4113 / ollama 4464 / openai 12886 / e2e 7136）」 | **54660 行（去空行）/ 63416（物理）**：anthropic 6259 / dashscope 13933 / gemini 5769 / ollama 5711 / openai 15793 / openai-official 7395 / e2e 8556 |
| 21 | §3.3 | 「重试/退避/超时：**待办**；当前无重试」 | **部分完成**——`TransportConfig` 已含超时 + `max_retries=2` + 指数退避并已接线；剩余：未暴露到 `GenerateOptions`、流式不重试 |
| 22 | §3.3 | 「指标/可观测性埋点：**待办**；`tracing` 依赖已声明但未埋点」 | **部分完成**——`common/diagnostics.rs` 已有 `tracing` 落点 + 9 处 provider 调用 |
| 23 | §3.3 | 「独立 `tests/` 集成测试：**待办**；当前 198 个单测全部内联、无 `tests/` 目录」 | **已完成**——`tests/model_extensions_integration.rs` 已存在，5 个集成测试 |
| 24 | §3.3 | 「统一远程媒体 resolver：**待办**」 | **部分完成**——统一 seam 已落地（`common::media`）且 gemini/ollama 已改走；剩余：无内置远程下载实现 |
| 25 | §4.1 | 「`ReqwestTransport`（真实 reqwest **0.12**）」 | **0.13**（workspace） |
| 26 | §4.1 | 已知问题 1「embedding DTO 无调用方」 | **已解决**（2026-10-12） |
| 27 | §6.3-2 | 「本 crate 自有 HTTP 走 0.12」 | 主体走 **0.13**；0.12 仅 `anthropic_async` 别名 |
| 28 | §2.3 | 「`openai.responses.*` → `crate::responses`（已就位）」 | **需限定**：与 Java `openai-official` **不是同一物**（provider id / SDK / 前缀均不同）；已在 §2.8 拆开并单列 |

### 7.7 本轮未改动
- **未修改任何 Rust 代码**（本子智能体为静态复核，只读）。
- **未执行** `cargo build` / `cargo test`。
- 本轮唯一的写操作：**本文件** `docs/extensions/MODEL__GAP_TRACKER.md`（原地定点修正 + 追加本节，未重写全文）。



## 7. §4.3 P0 修复记录（2026-10-22，汇总方）

### 7.1 已修：`responses` 错误归一化不一致（§4.3 #12）

- **缺陷**：`responses::map_openai_error` 只做 `Error::HttpStatus{status,message}`，丢掉 `type`/`code`，且未区分 timeout / network；与 `openai/mod.rs` 的同名映射不一致 → 重试与降级判断失效。
- **修复**：改用 `ModelError::from_parts`（provider 标记 `openai-responses`）、`OpenAIError::Reqwest` 按 `is_timeout()` 归类 `Timeout`/`Network`、`StreamError` 归 `Error::Http`。
- **用例**：`api_error_preserves_type_code_and_status`（断言 429 且渲染串含 `type=` / `code=`）。
- **验证**：`cargo test -p agentscope-extensions-model` → **263 + 5 passed / 0 failed**。

### 7.2 新发现的阻塞（影响 TRAINING §4.3 #6 闭环）

- `OpenAiChatModel::stream` 硬编码 `stream: Some(true)`，`GenerateOptions.stream` **从不被读**；且只有 `create_stream_byot` 的 SSE 解析路径。
- 若要支持非流式（Trinity 场景）：`build_request` 需读 `options.stream`，并新增 `chat().create()` 分支 + `CreateChatCompletionResponse` → `ChatResponse` 映射 + 用例。**这是关闭 TRAINING #6 的前置条件。**

### 7.3 仍待办

- `openai-official` 子模块（14 类 / 3716 行）完全未登记（§4.3 #11）。
- dashscope 加密零实现、dashscope 与会话合并、ollama 多智能体、`AnthropicServerTool`、responses multi-agent 分组（§3 P1）。

## 7. P0 修复记录（2026-10-22）— OpenAI Chat Completions 非流式请求路径

> 编号承接上一节（`## 7. §4.3 P0 修复记录（2026-10-22，汇总方）`）的 7.1–7.3，本节从 7.4 起；上一节内容未改动。

### 7.4 已修：`OpenAiChatModel` 支持 `stream=false`（TRAINING §4.3 #6）

- **缺陷**：`build_request` 硬编码 `stream: Some(true)` + `stream_options.include_usage`，`GenerateOptions.stream`（`agentscope-core/src/model/generate_options.rs:17`）从不被读取；`impl Model for OpenAiChatModel::stream` 只有 `create_stream_byot` 的 SSE 路径。任何要求 `stream=false` 的 OpenAI 兼容后端（Trinity-RFT，Java `TrinityModelAdapter.stream(false)`）都会收到相反的协议。
- **改动**（仅 `crates/agentscope-extensions-model/src/openai/mod.rs`）：
  1. `build_request`：`let streaming = options.stream.unwrap_or(true)`；`Some(false)` → `stream: Some(false)` 且 `stream_options: None`；`None` 保持历史默认 `true`（不回归）。
  2. `Model::stream`：先捕获 `options.stream`，请求体（含 `additional_body_params` 合并、`reasoning_content` 回填）构造完成后，`!streaming` 走 `client.chat().create_byot::<_, OpenAiCompletionResponse>(body)`，映射成单个 `ChatResponse`，再 `futures::stream::once(...)` 返回单元素流，trait 形状不变。
  3. 复用/抽取共享逻辑，避免两套字段映射：`usage_from_completion`（流式 accumulator 与 非流式 mapper 共用）、`tool_use_block`（参数解析 + 非 JSON 原文兜底）、`reasoning_text_from`（`reasoning_content`/`reasoning` 字段解析）。非流式响应用自定义 `OpenAiCompletionResponse` 反序列化（与 `OpenAiStreamChunk` 同理），因此 `reasoning_content` 不会被 `CreateChatCompletionResponse` 丢弃。
- **用例**（3 条）：
  - `non_stream_request_sets_stream_false_and_omits_stream_options`：**改前必失败**——`build_request` 硬编码 `Some(true)`，`body["stream"] == false` 断言不成立，且 `stream_options` 一定存在。
  - `non_stream_completion_maps_text_tool_calls_usage_and_reasoning`：**改前必失败**——所需符号 `OpenAiCompletionResponse` / `completion_to_response` 不存在（编译失败）；语义上纯 JSON completion 交给 `create_stream_byot` 只会被当作 SSE 解析，产生不了文本/tool call/usage。现断言 thinking + text + tool call（`call_1` / `get_weather` / `Beijing`）+ usage（10/5/4）+ `finish_reason=tool_calls`。
  - `default_options_keep_streaming_request`：**防回归**（改前即通过，改后继续通过）——`None` 仍为 `stream: true` + `include_usage: true`。
  - 测试全部为离线纯逻辑，不发起真实网络请求（本 provider 直连 `async-openai`，未接入 `common::HttpTransport` seam，故用请求体序列化 + 响应 DTO 反序列化验证）。
- **验证**：
  - `cargo check -p agentscope-extensions-model` → `Finished`（0 error；本 crate 无新增 warning；`base_url.rs::env_lock` 的 dead_code warning 为既有）。
  - `cargo test -p agentscope-extensions-model` → **266 lib + 5 integration = 271 passed / 0 failed**（其中 `openai::` 27 条，含本次新增 3 条）。
- **未覆盖 / 边界**：
  - `ChatCompletionMessageToolCalls::Custom`（OpenAI 自定义工具，非 function）在非流式映射中被跳过：core 只有函数工具模型；现有后端未使用。
  - `n > 1` 只取第一个 choice（与 agent 主链路一致，请求也不会下发 `n`）。
  - `responses` provider 的 `stream: Some(true)` 同样硬编码，本次范围限定 Chat Completions，未改动。
  - 未做真实 Trinity-RFT 端到端联调（环境无该后端）；协议侧已由请求体断言覆盖。

## 8. §4.3 P0 #11 完成记录（2026-10-22）— `openai-official` provider

> 上一节（7.x）内容未改动，本节从 8 起。

### 8.1 结论

- **已交付**：新增 `crates/agentscope-extensions-model/src/openai_official/`（10 个源文件），provider id **`openai-official`**、模型前缀 **`openai-official:`**，通过 `default_openai_official_registry()` 注册并在 `lib.rs` 导出；`Model` 调用链、服务端工具、多智能体 formatter、错误翻译、凭据与请求映射均可编译并有离线测试。
- **未顶替 `openai-responses`**：`crate::responses` 行为未改动；`openai` / `responses` 原有测试保持全绿。
- **协议差异保留**：Rust 侧无官方 `openai-java` SDK，改用本 crate 既有的 `common::HttpTransport` seam（与 `gemini` 适配器同模式）直接说 Responses wire protocol；provider id、前缀、请求字段、server tool、`additionalBodyParams` 白名单、408/409 重试语义均按 Java 复刻。

### 8.2 类级对照表（Java 14 类）

| Java 类 | Rust 对应 | 状态 |
|---|---|---|
| `OpenAIOfficialModelProvider` | `openai_official::provider::OpenAIOfficialModelProvider` + `default_openai_official_registry()` | **已覆盖**：`supports` 接受 `openai-official:<id>` 与 OpenAI 家族 bare id（供 `ModelRegistry` 拆前缀后二次判定）；env 回退 `OPENAI_API_KEY` / `OPENAI_BASE_URL`；解析 `strictTools` / `strictJsonSchema` / `contextWindowSize` / `multiAgentFormatter` / `serverTools` 并**从 options 中剔除**，避免落入 body-params 白名单校验 |
| `OpenAIOfficialConstants` | `openai_official::constants` | **已覆盖**：provider id、前缀、默认 base URL、env 名、10 项 body-params 白名单、`openai.*` 元数据键、可重试状态集合 |
| `OpenAIOfficialModelException` + `OpenAIErrorTranslator` | `openai_official::error`（`OpenAIOfficialModelError` / `translate*`） | **已覆盖（输入形态不同）**：给定 `{status,type,code,message}` 归一为 `ModelError(provider="openai-official")`；408/409/429/5xx 可重试，401/403/404/422 不可重试；空消息回退 `OpenAI API error (HTTP n)`；无状态超时按消息归类为可重试 |
| `OpenAIOfficialCredential` | `openai_official::credential::OpenAIOfficialCredential` | **已覆盖**：`{id,type,api_key,organization,base_url}`，`api_key` 必填、其余可选；`resolve_api_key` / `resolve_base_url` 做显式值优先 + env 回退 + blank 视为缺失 |
| `OpenAISdkClientFactory` | `openai_official::model`（`default_transport` + `build_http_request`） | **部分覆盖**：`maxRetries=0` 语义由 AgentScope 重试层承担；builder 级 additional headers 已注入；**ProxyConfig 未实现**（见 8.4） |
| `OpenAIResponsesChatModel` | `openai_official::model::OpenAIResponsesChatModel` | **已覆盖**：stream / non-stream、配置 options 合并、连接字段覆盖校验（per-request apiKey/baseUrl 必须与构造时一致）、native structured output 默认开启、上下文窗口 |
| `ResponsesRequestMapper` | `openai_official::request` | **已覆盖**：options、`additionalBodyParams` 白名单与映射、rejected fields fail-fast、history（system/user/assistant/tool、reasoning replay、server tool replay、多模态 image/data）、tool 定义（strict / output_schema / defer_loading）、tool_choice |
| `ResponsesResponseParser` | `openai_official::parser::parse_response` | **已覆盖**：reasoning summary/text/encrypted、message/refusal、function_call、server tool items、Thinking→Text→Tool 顺序、response metadata、citations、usage 细分 |
| `ResponsesStreamingAssembler` | `openai_official::parser::ResponsesStreamingAssembler` | **已覆盖**：output_item.added/done、text delta、reasoning summary/text delta、function_call arguments delta、partial image、completed/incomplete、failed/error；终态 refusal + reasoning replay 元数据 + usage |
| `ResponsesHelper` | `openai_official::parser::{extract_response_metadata, extract_usage}` | **已覆盖**：id/status/created_at/completed_at/incomplete_reason/error/citations；input/output/cached/cache_write/reasoning tokens + wall-clock time |
| `ResponsesMultiAgentFormatter` | `openai_official::formatter::ResponsesMultiAgentFormatter` | **已覆盖**：SYSTEM 独立、TOOL_SEQUENCE 直通、AGENT_CONVERSATION 合并 `<history>`、BYPASS 独立；首组 prompt、多轮 agent 名前缀、text/hint/image/data/thinking 处理与失败降级 |
| `OpenAIServerTool` | `openai_official::server_tool::OpenAIServerTool` | **已覆盖**：web_search / code_interpreter / image_generation / tool_search；拒绝 function、mcp、computer、shell、自定义工具与 client-executed tool_search |
| `ResponsesServerToolHelper` | `openai_official::server_tool::ResponsesServerToolHelper` + `HostedToolSearchPairing` | **已覆盖**：merge tools、include 列表、call/result 解码（web search / code interpreter logs+image / image generation base64 / tool_search 配对）、server-tool item 元数据回放、partial image |

### 8.3 测试（离线）

命令与真实结果：

```text
cargo test -p agentscope-extensions-model
→ lib: 327 passed / 0 failed
→ integration: 5 passed / 0 failed
合计 332 passed / 0 failed
```

基线为 266 lib + 5 integration；本次新增 **61** 条（全部内联在 `src/openai_official/*`，无真实网络）。关键用例及**改前失败原因**：

- `provider::tests::registers_and_resolves_in_core_registry` / `registry_resolution_creates_a_model`：改前 `openai-official` 未注册、无 provider 实现，`can_resolve("openai-official:gpt-5")` 必然为 false 且 `resolve_with` 报 “no model provider”。
- `provider::tests::create_applies_builder_options_and_strips_them`：改前无 `create` 实现；且若把 `strictTools` 等键原样留在 `additional_body_params`，请求映射会因白名单 fail-fast。
- `request::tests::system_and_user_messages_map_to_easy_messages` / `assistant_tool_use_becomes_function_call` / `tool_result_becomes_function_call_output_and_joins_text`：改前无 Responses 请求体映射，body 为空。
- `request::tests::server_tools_are_merged_and_include_list_is_added` / `deferred_tool_requires_tool_search_server_tool`：改前无 `OpenAIServerTool` / `apply_includes` / `defer_loading` 校验。
- `error::tests::http_error_preserves_type_code_and_status` / `official_retryable_status_set_includes_408_and_409`：改前无官方 provider 错误归一；通用 `ModelError` 对 409 归类为不可重试，故本组断言 `to_core().is_retryable()==true` 必失败。
- `formatter::tests::system_is_individual_and_agent_turns_merge_into_one_user_message` / `only_first_agent_group_carries_the_prompt` / `bypass_messages_stay_individual` / `tool_sequence_passes_through_unchanged`：改前无分组/合并实现。
- `server_tool::tests::*`：改前无 server tool 解码（web search call+result、code interpreter logs/image、image generation base64、tool_search 配对、raw item 回放）。
- `parser::tests::*` / `model::tests::*`：改前无 Responses 响应/SSE 解析与 transport 调用链；`non_streaming_request_targets_responses_endpoint` 断言 URL `…/responses`、`store=false`、`Authorization: Bearer`，`streaming_uses_sse_and_yields_incremental_blocks` 断言 SSE 增量与 usage。

### 8.4 未覆盖 / 后续待办（精确到方法）

1. **`OpenAISdkClientFactory.applyProxy`**：`ProxyConfig`（HTTP/SOCKS、认证、nonProxyHosts）未接；当前依赖 `ReqwestTransport` 的进程级默认。需在 provider/model 增加 proxy 配置并构造专用 `reqwest::Client`。
2. **`OpenAIResponsesChatModel.isModuleRetryable` / `checkXShouldRetry`**：未读取 `x-should-retry` 响应头；`OpenAIIoException` / `OpenAIInvalidDataException` 的类型级区分在 Rust 侧退化为 transport `Error` + 消息启发式。`408/409/429/5xx` 的 HTTP 重试语义已覆盖。
3. **`ResponsesRequestMapper.buildReasoning` 的 `.validate()`**：`reasoning_effort` / `reasoning.summary|context|mode`、`service_tier`、`prompt_cache_options.mode|ttl` 目前直接透传字符串，未做 Java SDK 的枚举白名单校验。
4. **`OpenAIOfficialCredential.organization`**：字段已保留但不发送 `OpenAI-Organization` 头；Java `OpenAIResponsesChatModel.Builder` 同样未接线，故属对齐现状而非新增缺口。
5. **`ResponsesStreamingAssembler.handleOutputItemDone`**：与 Java 一致，只为 server-tool item 产出 block；普通 `function_call` 的最终参数不在 `output_item.done` 单独落块（由 arguments delta + 终态由上层消费方合并）。
6. **`ResponsesMultiAgentFormatter` 注入方式**：Rust 无 Java 的 `component(...)` 注入，改为 `GenerateOptions.additional_body_params["multiAgentFormatter"]`（`true` 或自定义 prompt 字符串）；`serverTools` 同理接受 JSON 数组。
7. **e2e**：未对真实 OpenAI 端点联调；全部为离线 wire-level 断言。

### 8.5 影响面

仅改动 `crates/agentscope-extensions-model/src/openai_official/**`、`src/lib.rs`（新增 `pub mod` / 导出）与本 tracker。未触碰 `openai` / `responses` 实现、其它 crate、根 `Cargo.toml`，未执行 git 写操作。
