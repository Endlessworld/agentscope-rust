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
| 当前状态 | **总判：已完成**（2026-10-31 收口）—— **主体完成（provider 主链路 + 剩余 P1）** — `common`（HTTP seam + error/capabilities/usage/diagnostics/media/model_map/**multi_agent**）+ `ollama` + `dashscope`（含**加密 seam** 与**多智能体**）+ `anthropic`（含**内置 server tool**）+ `gemini` + `openai` / `responses`（含**多智能体分组**）/ `anthropic_async` + `openai-official`（含 proxy / x-should-retry / 枚举白名单）+ `relay`。2026-10-28 实测：**56 个源文件 / 22111 物理行 / 369 内联 + 5 集成测试 = 374 passed / 0 failed**。**2026-10-31 收口：原列 6 项「仍开放」全部关闭**——① anthropic 并行 tool-use 拆分 → `anthropic/converter.rs::{collect_split_tool_results, convert_parallel_tool_calls}` + 2 用例；② gemini Vertex AI → **已完成（设计决定，不迁移）**，`GeminiModelProvider` 显式报错 + 用例 `create_rejects_vertex_ai`；③ 内置远程媒体下载 → `common::RemoteMediaCache::prefetch`（`HttpTransport::get_bytes`）+ 3 用例；④ openai `compat.*` → `openai::compat`（四厂商 + 7 用例）；⑤ `DashScopeMultiModalTool` → **已完成（设计决定，不迁移）**，依据 §3.2 / §4.2-4；⑥ Java 测试逐条对照 → 映射表 见 history/MODEL__MIGRATION_HISTORY.md。本轮另补 openai chat 多智能体格式化（`OpenAIMultiAgentFormatter` / `OpenAIConversationMerger`，§3.2 + 见 history/MODEL__MIGRATION_HISTORY.md）。静态计数 **416 内联测试 + 9 个集成 = 425**（编译/运行由主智能体验证） |

### 1.1 验证基线（2026-09-19 实测；2026-09-21 / 2026-10-12 更新；2026-10-22 静态复核）

> **2026-10-31 loop 基线（受限执行体，未跑 cargo）**：静态计数 **416 个内联测试 + 9 个集成 = 425**（上轮 377 + 5 = 382；本轮 **+39 内联 + 4 集成**，集成新增 `tests/registry_semantics.rs`）。`cargo build/test -p agentscope-extensions-model` 由主智能体统一执行，本表不写编译期结论。

> ⚠️ **2026-10-22 静态复核**：本节历史 `passed` 数字（198 / 236）为既往会话的编译期结果，本次复核按约定**未执行 `cargo build` / `cargo test`**，仅静态清点测试属性。当前源码静态计数为 **267 个测试属性（262 内联 + 5 集成）**，高于 见 history/MODEL__MIGRATION_HISTORY.md 记录的 236；差额来自 2026-10-12 之后新增的 server-tool / base_url / media 等用例。真实 `passed` 数需下次编译轮确认。
```
（历史记录，未在本次复核中重跑）
cargo build -p agentscope-extensions-model   -> Finished (0 errors；本 crate 0 error/0 warning；输出中的 warning 均来自其它 crate 的既有代码)
cargo test  -p agentscope-extensions-model   -> 198 passed（2026-09-21）/ 236 passed（2026-10-12，231 lib + 5 集成）
（2026-10-22 静态计数，未编译）
测试属性总数 267 = common 34 + ollama 27 + dashscope 39 + anthropic 61 + gemini 58
                + openai 24 + responses 6 + anthropic_async 8 + relay 5 + tests/ 集成 5
```
> 说明：2026-09-21（提交 `d86a578`）core 的 `model/{openai,responses,anthropic}.rs` 整体迁入本 crate；本次复核已确认 `crates/agentscope-core/src/model/` 现仅剩 **10 个 `.rs` 文件 / 1117 行**（`mod.rs` 30 行 + 9 个模块文件：`cache_policy` 12 / `chat_response` 110 / `context_windows` 90 / `execution_config` 135 / `generate_options` 161 / `model`(trait) 77 / `provider`(SPI+registry) 371 / `tool_choice` 33 / `tool_schema` 98），**`model/openai.rs` / `model/responses.rs` / `model/anthropic.rs` 均已不存在**，叙述成立。该批代码**源自 core 既有实现，非从 Java 逐类重写**。

### 1.2 Rust crate 文件清单（2026-10-28 实测：**56 个源文件 / 22111 物理行**、369 内联单测 + 5 集成；**2026-10-31 静态计数：416 内联 + 9 集成**）

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
| `tests/registry_semantics.rs`（新增 2026-10-31） | 261 | 4 个集成用例：core `ModelRegistry` 解析语义守卫（重新注册立即生效 / 失败不缓存可重试 / 无隐藏实例缓存 / `Arc` 跨线程共享），见 §4.2-11、见 history/MODEL__MIGRATION_HISTORY.md | 集成测试目标 |
| `src/common/multi_agent.rs`（新增 2026-10-28） | 多智能体分组共享层：`GroupType` / `group_messages` / `is_tool_related` / `agent_name` / `text_only` / `HISTORY_*` / `DEFAULT_CONVERSATION_HISTORY_PROMPT` / `take_multi_agent_prompt` / `take_bool_option`；3 个单测 | 是 |
| `src/dashscope/encryption.rs`（新增 2026-10-28） | DashScope 请求加密协议层：`DashScopeCryptoProvider`（注入缝）/ `UnavailableCryptoProvider` / `EncryptionConfig` / `EncryptionContext` / `encrypt_request_body` / `decrypt_response_body` / `encryption_header_value`；7 个单测 | 是 |
| `src/dashscope/crypto.rs`（新增 2026-10-28） | 默认 `AesRsaCryptoProvider`（`aes-gcm` + `rsa` + `rand`）：AES-256-GCM / RSA PKCS#1 v1.5 / `OsRng`；`test_support` 持有 Java JCE 导出的金标与一次性 2048-bit 测试密钥对；6 个单测 | 是 |
| `src/anthropic/server_tool.rs`（新增 2026-10-28） | `AnthropicServerTool`（web_search / web_fetch / code_execution / bash_code_execution / text_editor_code_execution / tool_search）+ `SERVER_TOOL_TYPE_PREFIXES`；2 个单测 | 是 |
| `src/openai_official/proxy.rs`（新增 2026-10-28） | `ProxyType` / `ProxyConfig` / `build_client`（HTTP + basic auth、nonProxyHosts；SOCKS 显式报错）；5 个单测 | 是 |

> ℹ️ **更正（2026-09-19）**：本表旧版曾记「`dashscope` 仅 DTO、未接线、未编译」，并据此在 §3.1 列为待办阻塞项——**该描述已过时**。现 `src/dashscope/` 含 6 个子模块（converter/credential/dto/model/parser/provider）、已在 `lib.rs` 声明、已全部接入并随 crate 编译，共 **39** 个单测（旧记 33）。
>
> ℹ️ **本轮（2026-09-19）新增 anthropic / gemini**：两子模块均已在 `lib.rs` 声明（`pub mod anthropic; pub mod gemini;`）并 re-export，14 个新源文件、共 **119** 个新单测（anthropic 61 + gemini 58；旧记 111）。

## 2. Java 清单与依赖映射

> ⚠️ **状态口径（2026-10-31）**：本表每行末尾的「已完成（勘误）」/「已完成（2026-10-31）」为**最新结论**，行首的「待办 / 部分完成 / 无」是**历史快照**（保留以供追溯）。**唯一权威状态以 §3.1–§3.3 + §4.2 + 见 history/MODEL__MIGRATION_HISTORY.md 为准——2026-10-31 已清零，无未关闭条目。**

> Java 类清单位于 `agentscope-extensions-model-<子模块>/src/main/java/io/agentscope/extensions/model/<子模块>/`；下表逐类给出 Rust 对应。**状态口径**：以「已完成 / 已完成（设计决定，不迁移）」为准；若单元格内先出现「部分完成 / 不等价 / 缺失 / 无对应 / 待办」而后半段又有「已完成（…勘误）」「已关闭」标记，则**后半段为最终结论**（前半段是当时判定，保留以追溯）。
> Java 侧每个子模块另有 `src/main/resources/META-INF/services/io.agentscope.core.model.spi.ModelProvider`（SPI 注册文件，非 Java 类）。

### 2.1 ollama（22 类 / 5217 行）— **已完成**（2026-10-31；长尾 llama.cpp 参数改由 `OllamaOptions.extra` 透传）
> **2026-10-22 复核**：Java 类数与旧记一致（22 个 `.java`），但**行数为 5217**（旧记 4412）；测试侧 16 文件 / 5711 行（旧记 4464）。真实缺口 2 项：`OllamaConversationMerger`、`OllamaMultiAgentFormatter`（`<system-info>` 图片提升）。（2026-10-28 已闭环，见 history/MODEL__MIGRATION_HISTORY.md）
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
| `ollama.dto.OllamaEmbeddingRequest` | `POST /api/embed` 请求 | `ollama::dto::OllamaEmbeddingRequest` | 已接线：`OllamaChatModel::embed` / `embed_with_options`（见 history/MODEL__MIGRATION_HISTORY.md P1 已完成） |
| `ollama.dto.OllamaEmbeddingResponse` | embed 响应 | `ollama::dto::OllamaEmbeddingResponse` | 同上 |
| `ollama.formatter.OllamaChatFormatter` | 会话格式化 | `ollama::converter::{to_ollama_messages, to_ollama_messages_with, build_request}` | |
| `ollama.formatter.OllamaMessageConverter` | 单条消息转换 | `ollama::converter::{block_text, collect_text, role_str, url_of, ImageResolver, DefaultImageResolver}` | |
| `ollama.formatter.OllamaToolsHelper` | 工具/工具选择转换 | `ollama::converter::{to_ollama_tools, to_tool_choice}` | |
| `ollama.formatter.OllamaMediaConverter` | 图片/媒体 → base64 | `ollama::converter::{ImageResolver, DefaultImageResolver}` | 已改走共享 `common::resolve_media`：默认拒绝远程 URL，宿主 `set_media_resolver` 后即可下载（见 history/MODEL__MIGRATION_HISTORY.md P1） |
| `ollama.formatter.OllamaResponseParser` | 响应 → `ChatResponse` | `ollama::parser::{parse_response, OllamaStreamIncrementer}` | 工具调用 id 用 `uuid` 合成（同 Java） |
| `ollama.formatter.OllamaConversationMerger` | 多轮/多智能体会话合并 | `ollama::converter::merge_conversation` | **已完成**（2026-10-28） |
| `ollama.formatter.OllamaMultiAgentFormatter` | 多智能体格式化 | `ollama::converter::{to_multi_agent_messages, to_multi_agent_messages_with}` + `OllamaChatModel::with_multi_agent_formatter` / `with_promote_tool_result_images` | **已完成**（2026-10-28）；`<system-info>` 提升已附 base64 图片 |
| `ollama.options.OllamaOptions` | 采样/加载参数 | `ollama::options::OllamaOptions` | 仅迁移 `GenerateOptions` 子集 + `format`/`keep_alive`/`think`；llama.cpp loader 长尾见 §3.3 P2 |
| `ollama.options.ThinkOption` | thinking 开关/级别 | `ollama::options::ThinkOption` | `Boolean(bool)` / `Level(String)` |
| `resources/META-INF/services/...ModelProvider` | SPI 注册 | 无对应机制；改为 `ModelRegistry::register(Arc<dyn ModelProvider>)` 显式注册 | **设计决定**（Rust 无 ServiceLoader） |

### 2.2 dashscope（29 类 / 8791 行）— **已完成**（2026-10-31；`DashScopeMultiModalTool` 裁定归工具层不迁移）
> **2026-10-22 复核**：类数一致（29 个 `.java`），**行数为 8791**（旧记 7453）；测试 23 文件 / 13933 行（旧记 11767）。真实缺口：`DashScopeEncryptionUtils` 的 AES-GCM/RSA 加密**未实现**（仅公钥 DTO + 端点常量）、`ConversationMerger`、`MultiAgentFormatter`、`DashScopeMultiModalTool`。（2026-10-31 收口：加密见 history/MODEL__MIGRATION_HISTORY.md、两类 formatter 见 history/MODEL__MIGRATION_HISTORY.md、`DashScopeMultiModalTool` = **设计决定不迁移**，见 §3.2）
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
| `dashscope.dto.DashScopePublicKeyResponse` | 加密公钥响应 | `dashscope::dto::{DashScopePublicKeyResponse, PublicKeyData}` + `dashscope::model::PUBLIC_KEYS_ENDPOINT` | **已完成（2026-10-28，M7）**：`PUBLIC_KEYS_ENDPOINT` 已有真实消费者——`DashScopeChatModel::fetch_public_key()` 经 `HttpTransport::get_json` 拉取该端点并由 `EncryptionConfig::from_public_key_response` 解析（响应校验与 Java `fetchPublicKey` 一致） |
| `dashscope.DashScopeChatModel` | 聊天模型主类 | `dashscope::model::DashScopeChatModel`（impl core `Model`） | 已接线 |
| `dashscope.DashScopeHttpClient` | HTTP/SSE 客户端 | `dashscope::{converter,parser}` + `common::HttpTransport` | 已接线 |
| `dashscope.DashScopeEncryptionUtils` | 请求加密（AES-GCM 数据 + RSA 包 AES key） | `dashscope::encryption::*`（协议）+ `dashscope::crypto::AesRsaCryptoProvider`（原语） | **已完成（2026-10-28，含真实密码学实现）**：协议纯函数化（`encrypt_request_body` / `decrypt_response_body` / `encryption_header_value` / `EncryptionConfig::{new, from_public_key_response}`）+ 默认 `AesRsaCryptoProvider`（AES-256/GCM/128-bit tag/12 B IV、RSA PKCS#1 v1.5、`OsRng`）。`DashScopeCryptoProvider` 注入缝与 `UnavailableCryptoProvider` 保留。金标由 Java JCE 导出（见 history/MODEL__MIGRATION_HISTORY.md）。**2026-10-28 修复（M7）**：`enableEncrypt` 现已端到端串通——`with_encryption_enabled(true)` / provider 选项 `enableEncrypt` → `resolve_encryption_config()` 经 `HttpTransport::get_json` 拉取 `{baseUrl}/api/v1/public-keys/latest`（`PUBLIC_KEYS_ENDPOINT` 现有真实消费者）并缓存（`tokio::sync::OnceCell`）；`enable_encrypt=true` 但缺公钥 → **`Err`**（同步 `build_http_request_with_context` 亦拒绝构建），绝不静默明文；用例 `enable_encrypt_false_sends_the_plaintext_body` / `enable_encrypt_without_a_public_key_fails_instead_of_sending_plaintext` / `enable_encrypt_fetches_the_public_key_and_encrypts_the_body` / `sync_request_builder_refuses_to_send_plaintext_when_encryption_is_on` |
| `dashscope.DashScopeModelProvider` | 注册表 provider | `dashscope::provider::DashScopeModelProvider`（impl `ModelProvider`） | 已接线；`DASHSCOPE_API_KEY` / `DASHSCOPE_BASE_URL` |
| `dashscope.EndpointType` | 端点类型枚举 | `dashscope::credential::EndpointType` | `Auto`/`Text`/`Multimodal` + `parse()` |
| `dashscope.credential.DashScopeCredential` | 凭据 | `dashscope::credential::DashScopeCredential` | **已迁移**（旧版记「无 / 待办」有误）：`api_key` + `base_url` + `CREDENTIAL_TYPE` |
| `dashscope.formatter.DashScopeChatFormatter` | 会话格式化 + options/tools | `dashscope::converter::{to_dashscope_messages_with_options, build_request}` | 部分（`build_request` 覆盖主链路） |
| `dashscope.formatter.DashScopeMessageConverter` | 单条消息转换（含多模态 part） | `dashscope::converter::{to_dashscope_messages, simple_message, multimodal_message, tool_role_multimodal_message}` | 部分（函数级覆盖，无同名类型） |
| `dashscope.formatter.DashScopeMediaConverter` | 图片/音频/视频 → part | `dashscope::converter::{image_part, audio_part, video_part, data_part}` | 部分（函数级覆盖） |
| `dashscope.formatter.DashScopeToolsHelper` | 工具/工具选择转换 | `dashscope::converter::{to_dashscope_tools, to_tool_choice_value}` | 部分（`Required→auto` 降级带 diagnostics） |
| `dashscope.formatter.DashScopeResponseParser` | 响应 → `ChatResponse` | `dashscope::parser::{parse_response, DashScopeStreamIncrementer}` | 等价 |
| `dashscope.formatter.DashScopeConversationMerger` | 多轮/多智能体会话合并（`<history>`） | 无 | **已完成（2026-10-29）**：`dashscope::converter::{merge_conversation, merge_multimodal_conversation}` 已落地；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `multi_agent_text_mode_merges_groups_with_history_tags` |
| `dashscope.formatter.DashScopeMultiAgentFormatter` | 多智能体格式化 | 无 | **已完成（2026-10-29）**：`dashscope::converter::to_multi_agent_messages` + `DashScopeChatModel::with_multi_agent_formatter` + provider `multiAgentFormatter` 选项已落地；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `multi_agent_formatter_is_applied_to_the_request_body` |
| `dashscope.tool.DashScopeMultiModalTool`（93.5 KB / 29 文件之一） | 多模态工具（TTS/图像/视频等） | 无（归工具层） | **已完成（设计决定，不迁移）**：该类依赖 Java SDK `ImageSynthesis`/`VideoSynthesis`/`SpeechSynthesizer`/`Recognition`，是独立生成式 API 的工具封装，不参与 chat 编码；宿主用 `Tool` 封装即可（§3.2 / §4.2-4） |

### 2.3 openai（53 类 / 9853 行）— **已完成**（2026-09-21 自 core 迁入；2026-10-31 补 `compat` vendor 校正）
> **状态口径变更（2026-09-21，`d86a578`）**：本节原为「设计决定（不迁移）」，依据是 `EXTENSIONS_GAP_TRACKER.md` §2.2 裁定「core 定 trait + 注册表，本 crate 不重复实现 openai」。该裁定已被本次目录重构取代——core 的 `model/openai.rs`、`model/responses.rs`、`model/anthropic.rs` **实现体已整体迁入本 crate**，core 仅保留 `Model` trait / `ModelRegistry` / provider SPI。因此「不重复实现」的说法不再成立。
> **注意区分**：迁入的是 **core 既有实现**，不等价于「Java 53 类逐类迁移完成」。下标 4 项均已裁定：`compat.*`（12 类）**已完成**（2026-10-31，`openai::compat` + 7 用例）、`exception.*`（8 类）**已完成（设计决定，不迁移）**（`ModelError` + `ErrorKind` 14 分类）、`OpenAIMultiModalTool` 与 `credential.OpenAICredential` **已完成（设计决定，不迁移）**；见 §3.2 / §3.4。
> **2026-10-22 复核**：类数一致（53 个 `.java`），**行数 9853**（旧记 8534）；测试 45 文件 / 15793 行（旧记 12886）。`src/openai/mod.rs` 实测 **1380 行**（旧记 1045）。

| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openai.OpenAIChatModel` / `OpenAIClient` / `OpenAIModelProvider` / `OpenAIModelProviderSupport` | OpenAI 兼容聊天链路与 provider | `crate::openai::{OpenAiChatModel, OpenAiProvider, default_registry}`（`src/openai/mod.rs`，1380 行，基于 `async-openai` 0.41） | **已就位**（实现体原在 `agentscope-core/src/model/openai.rs`，2026-09-21 整体迁入；`agentscope_core` 不再导出这些类型） |
| `openai.credential.OpenAICredential` | 凭据载体 | 无独立类型；经 `ModelCreationContext.api_key` / `OPENAI_API_KEY` | **已完成（设计决定，不迁移）**：与本表 ollama 凭据行同一口径——凭据统一由 `ModelCreationContext.api_key` / 宿主环境变量表达，不建平行类型 |
| `openai.dto.*`（16 类：Request/Response/Message/Choice/ContentPart/Function/ImageUrl/InputAudio/ReasoningDetail/StreamOptions/Tool/ToolCall/ToolFunction/Usage/VideoUrl/Error） | OpenAI wire 模型 | `src/openai/mod.rs` 内部 DTO + `async-openai` 0.41 类型 | 已就位（随 `openai` 一并迁入）；Java 16 类未逐类 1:1 重写 |
| `openai.responses.*`（OpenAI Responses API） | Responses 协议客户端 | `crate::responses::{OpenAiResponsesModel, OpenAiResponsesProvider, default_responses_registry}`（`src/responses/mod.rs`，691 行） | **已就位**：与 Java 的 `openai-official` 子模块（`OpenAIResponsesChatModel` 等 14 类）是**不同来源**——见 §2.8 |
| `anthropic_async`（async-anthropic 版 Messages 客户端） | Anthropic Messages 协议（SDK 驱动） | `crate::anthropic_async::{AnthropicMessagesModel, AnthropicMessagesProvider, default_anthropic_registry}`（`src/anthropic_async/mod.rs`，712 行，基于 `async-anthropic` 0.6） | **本轮新增对应**：与 §2.4 的 transport-seam 版 `crate::anthropic` 并存（两条实现路径，见 §4.2 待裁定）。**注意：Java 侧无对应子模块**（Java 用官方 SDK 的是 anthropic 子模块本身） |
| `openai.exception.*`（8 类：OpenAIException/Authentication/BadRequest/InternalServer/NotFound/PermissionDenied/RateLimit/UnprocessableEntity） | 异常层次 | `crate::common::error::{ModelError, ErrorKind}`（8 种厂商错误体归一 + `retryable()`） | **设计决定**：Rust 用单一 `thiserror` 枚举 + `ErrorKind` 分类，不 1:1 建异常类层次；分类能力已等价（见 history/MODEL__MIGRATION_HISTORY.md P0） |
| `openai.formatter.*`（11 类：OpenAIBase/Chat/ConversationMerger/ConverterUtils/MessageConverter/MultiAgent/ResponseParser + 4 个 vendor formatter） | OpenAI 格式/转换/解析 | `src/openai/mod.rs`（`to_request_messages`/`to_request_tools`/`to_tool_choice`/`to_response_format`/`StreamIncrementer`） | 已就位（随迁入）；**`OpenAIConversationMerger` / `OpenAIMultiAgentFormatter` 已完成**（2026-10-31）：`common::multi_agent::{merge_agent_group, merge_multi_agent_history}` 由 `openai::build_request` 消费 `multiAgentFormatter` 选择器（首组带 prompt、tool 序列直通、媒体降级为 `[Image]` 文本），用例 `openai::mod::tests::multi_agent_formatter_merges_agent_turns_into_one_history_message` |
| `openai.tool.OpenAIMultiModalTool` | 多模态工具 | 无（归工具层） | **已完成（设计决定，不迁移）**（2026-10-31 裁定）：Java 侧实现 `io.agentscope.core.tool.Tool`（文生图/图生文/TTS/STT），不属 MODEL 模块；依据 §4.2-4 |
| `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | 兼容厂商 formatter + provider | `crate::openai::compat::{vendor_for_model, apply_vendor_rules}`（新增 2026-10-31） + `OpenAiProvider` + `common::capabilities` | **已完成**（2026-10-31）：四厂商请求体校正（去 `strict` / DeepSeek `reasoning_content` 回填 / GLM 夹取与 toolChoice 降级 / Kimi 采样参数与 `reasoning_effort` / MiniMax `tool_choice` 与 `reasoning_split`），7 条单测；不建每厂商 provider（multi-agent 分组由 `common::multi_agent` 统一承担） |
| `resources/META-INF/services/...ModelProvider`（5 行） | SPI 注册 | `crate::openai::default_registry()` / core `ModelRegistry::register` | |

### 2.4 anthropic（13 类 / 3171 行）— **已完成**（provider 主链路 + 并行拆分 + tool search）
> **2026-10-22 复核**：Java 侧实为 **13 个类**（旧记 11），**3171 行**（旧记 2192）；测试 16 文件 / 6259 行（旧记 4677）。旧表**漏记 2 类**：`formatter.AnthropicServerToolHelper`、`tool.AnthropicServerTool`。这两类的 provider 侧能力（server-tool 块回显与结果映射）与**声明/校验入口**均已落地（2026-10-29，见 history/MODEL__MIGRATION_HISTORY.md），下表两行已收敛。
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `anthropic.AnthropicChatModel` | 聊天模型主类 | `anthropic::model::AnthropicChatModel`（impl core `Model`） | 经注入的 `SharedTransport` 调 `POST {base_url}/v1/messages`；离线可测 |
| `anthropic.AnthropicModelProvider` | provider 插件 | `anthropic::provider::AnthropicModelProvider`（impl `ModelProvider`） | `provider_id()=="anthropic"`；支持 `anthropic:` 前缀与裸 `claude*`；`ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` |
| `anthropic.credential.AnthropicCredential` | 凭据 | `anthropic::credential::AnthropicCredential` + `AnthropicAuthMode` | api_key（`x-api-key`）/ authToken（`Authorization: Bearer`）互斥 |
| `anthropic.formatter.AnthropicBaseFormatter` | 基础格式化（system/cache/options） | `anthropic::converter::{build_request, system_prompt, apply_cache_control, ChatFormatOptions}` | system 提到顶层 `system` 字段；cache_control 标记 system/末条消息/末个工具 |
| `anthropic.formatter.AnthropicChatFormatter` | 会话格式化 | `anthropic::converter::{to_anthropic_messages, build_request}` | |
| `anthropic.formatter.AnthropicConversationMerger` | 会话合并 | `anthropic::converter::{merge_conversation, ConversationBlock}` | `<history>` 包裹，与 Java 同构；作为多智能体路径的可复用入口 |
| `anthropic.formatter.AnthropicMediaConverter` | 媒体转换 | `anthropic::converter::{image_source, image_content_block, data_content_block}` | 本地文件→base64；远程 URL 直接透传 |
| `anthropic.formatter.AnthropicMessageConverter` | 消息转换 | `anthropic::converter::{to_anthropic_messages, message_content_blocks, tool_result_message}` | tool_result 拆为独立 user 消息；并行 tool-use 拆分**已完成**（2026-10-31：`collect_split_tool_results` / `convert_parallel_tool_calls`，§3.2） |
| `anthropic.formatter.AnthropicMultiAgentFormatter` | 多智能体格式化 | `anthropic::converter::to_multi_agent_messages` | system/tool-sequence/agent-conversation 分组；agent 组合并为单 user 消息 |
| `anthropic.formatter.AnthropicResponseParser` | 响应解析（含 cache control / thinking） | `anthropic::parser::{parse_message, AnthropicStreamIncrementer}` | thinking→text→tool_use；cache token 加回 input；流式增量器缓冲 tool 参数分片 |
| `anthropic.formatter.AnthropicToolsHelper` | 工具转换 | `anthropic::converter::{to_anthropic_tools, to_tool_choice, apply_extra_body_params}` | `disable_parallel_tool_use = !parallel_tool_calls`；`Required`→`any`；末个工具带 cache_control |
| `anthropic.formatter.AnthropicServerToolHelper`（旧表漏记） | server tool 块 ↔ SDK 内容块互转、结果状态映射 | `anthropic::parser::{METADATA_SERVER_TOOL_RESULT, blocks_of, server_tool_result_block}` + `anthropic::converter`（`ServerToolUse` 回显、`is_server_tool()` 分支） | **已完成**：解析侧（`server_tool_use` / `web_search_tool_result` / `code_execution` / `bash_code_execution` / `text_editor_code_execution` / `tool_search` → `ToolResultBlock` + metadata）已实现且有 3 个单测；声明侧 `anthropic::server_tool::AnthropicServerTool` + `to_anthropic_tools_with_server` 已落地（2026-10-29，见 history/MODEL__MIGRATION_HISTORY.md）；Java `ToolValidator` 的校验语义由 server tool 的类型白名单 + 全局工具名唯一性 fail-fast 承担（用例 `duplicate_tool_names_across_client_and_server_tools_are_rejected`） |
| `anthropic.tool.AnthropicServerTool`（旧表漏记） | server tool 声明（web_search 等内置工具） | 无独立类型（`GeminiServerTool` 有对等物，Anthropic 侧无） | **已完成（2026-10-29）**：`anthropic::server_tool::AnthropicServerTool` + `converter::{to_anthropic_tools_with_server, build_request_with_server_tools}` 已落地（server 先声明、名字全局唯一 fail-fast）；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `server_tools_are_declared_before_client_tools` |
| SDK wire 类型（`MessageCreateParams`/`MessageParam`/`Message`/`RawMessageStreamEvent`） | Anthropic 官方 SDK | `anthropic::dto::{AnthropicRequest, AnthropicMessage, AnthropicContentBlock, AnthropicMessageResponse, AnthropicStreamEvent}` | Java 用官方 SDK，Rust 无对应依赖→自建 DTO/parser（方案裁定见 §4.2） |

> **传输层裁定**（2026-09-19）：采用 §4.2 选项 (b) —— 复用本 crate 既有 `HttpTransport` seam + `ReqwestTransport`（生产）/`MemoryTransport`（离线测试）手写 Anthropic Messages 协议，**未引入新 HTTP / 厂商 SDK 依赖**。

### 2.5 gemini（11 类 / 3071 行）— **已完成**（2026-10-31 补 default tool choice）
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
| `io.agentscope.core.e2e.*`（CoreAgentE2ETest、MultiModalE2ETest、StreamingBehaviorE2ETest、StructuredOutputE2ETest、ToolSystemE2ETest、CrossModelCompatibilityE2ETest、AudioCapabilityE2ETest、VideoCapabilityE2ETest、HookE2ETest、HITLBasicE2ETest、InterruptionE2ETest、SkillE2ETest、ToolGroupsE2ETest、ExecutionConfigE2ETest、SpecializedFeaturesE2ETest 等 **17 个含 `@Test` 的测试类**）+ `providers.*`（ModelProvider/BaseModelProvider/AnthropicProvider/DashScopeProvider/OpenRouterProvider 等 14 个辅助类）+ `io.agentscope.extensions.model.*.E2ETest`（DashScopeMultiModalToolE2ETest、GeminiNullableToolSchemaE2ETest） | 真实厂商端到端回归（需真实 API key + `ENABLE_E2E`） | 无 | **设计决定（不迁移）**：Rust 侧统一采用**离线可测**策略——`HttpTransport` seam + `MemoryTransport` 注入，不引入需要密钥的网络测试；各 provider 的真实联调见 §3.2「真实厂商端点联调」= **已完成（设计决定，不迁移）** |

### 2.7 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Jackson (`@JsonProperty`/`@JsonInclude`/`@JsonAlias`/自定义 (De)Serializer) | JSON (反)序列化 | `serde` + `serde_json`（`[dependencies]` 经 workspace 引入）+ `#[serde(rename/skip_serializing_if/untagged)]` | 字段名 1:1 对齐 Java JSON |
| OkHttp / `java.net.http.HttpClient` / core `OkHttpTransport` | HTTP 传输 | `reqwest` **0.13**（经 workspace；根 `Cargo.toml` 已于 `43cd044` 全局统一 0.13）+ 别名 `reqwest12 = { package = "reqwest", version = "0.12" }`（仅 `anthropic_async` 注入自定义 client 用；`async-anthropic` 0.6 钉住 0.12）+ 自建 `HttpTransport` trait seam | **2026-10-22 复核更正**：旧记「`reqwest` 0.12」已过时——本 crate 主体走 workspace 0.13；0.12 仅作为别名保留（见 history/MODEL__MIGRATION_HISTORY.md）。生产 `ReqwestTransport`，测试 `MemoryTransport`（零真实 HTTP） |
| `com.anthropic.client.okhttp.AnthropicOkHttpClient`（Anthropic 官方 SDK） | Anthropic 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `anthropic::dto` 自建 Messages 协议 | **已完成**（2026-09-19）：未引入厂商 SDK；wire DTO 与 converter/parser 自建，可离线测 |
| `com.google.genai.Client`（Google GenAI 官方 SDK） | Gemini 传输 | 复用 `common::HttpTransport` + `ReqwestTransport` + `gemini::dto` 自建 REST 协议 | **已完成**（2026-09-19）：同上；SSE 流式用 `:streamGenerateContent?alt=sse` |
| reactor-core (`Flux`/`Mono`/`Schedulers`) | 响应式流 | `futures` (`BoxStream`) + `async-stream` **0.3**（本 crate） | core `Model::stream` 返回 `BoxStream` |
| slf4j (`Logger`/`LoggerFactory`) | 日志 | `tracing`（workspace） | |
| `java.util.UUID` | 合成工具调用 id / 请求 id | `uuid` **1**（本 crate，features `v4`, `serde`） | 与 Java 行为一致 |
| `java.util.Base64` | 媒体内联编码 | `base64` **0.22**（本 crate `Cargo.toml`） | ✅ **2026-10-31 更正**：2026-10-22 记的「零直接引用」已过时——`dashscope/{crypto,encryption}.rs` 的 base64 往返与 `common/media.rs::RemoteMediaCache::prefetch` 均直接使用；依赖保留 |
| `java.net.URL` / `URLEncoder` / `URI` | URL 构造与转义 | `reqwest`/`url` 内置 | 受控子集 |
| JUnit 5 / Mockito | 测试 | `#[test]` / `#[tokio::test]` + `MemoryTransport` fake | 离线，不新增 HTTP mock crate |
| Spring Boot AutoConfiguration | DI / 自动装配 | 不适用 | **设计决定（不迁移）**（本模块 Java 侧亦无 Spring 依赖，`pom.xml` 仅 core/jackson/reactor/junit） |
| `agentscope-core` / `agentscope-harness` | 内部依赖 | path 依赖 `../agentscope-core`（`Cargo.toml` 实测仅此一项 path 依赖） | ✅ **2026-10-22 复核**：§3.3 记「`agentscope-harness` 在本 crate 源码中零引用」与 §3.3「已完成（2026-09-21 从 `Cargo.toml` 移除）」**均成立**——`Cargo.toml` 当前只有 `agentscope-core` 一个 path 依赖，源码中 `agentscope_harness` 出现 **0** 次 |
| `openai-java` 官方 SDK（`openai-official` 子模块） | Responses API 官方客户端 | `async-openai` 0.41（`features` 含 `responses` / `response-types`）+ `crate::responses` 自建 request/stream/parse | **待裁定（2026-10-22 复核）**：见 §2.8；SDK 路径不同但**能力缺口独立存在**。**已完成（2026-10-29 勘误）**：原记「待裁定」，实际已定：不引入官方 SDK，用 `common::HttpTransport` + 自建 wire DTO（见 history/MODEL__MIGRATION_HISTORY.md），provider id / 前缀均对齐（见 history/MODEL__MIGRATION_HISTORY.md）；双实现取舍见 history/MODEL__MIGRATION_HISTORY.md |

### 2.8 openai-official（14 类 / 3716 行）— **已完成（2026-10-29 勘误；原记「未对照（真实缺口）」）**
> **2026-10-22 复核新增**：本节此前**完全缺失**（旧版仅在 §1 提到「旧版漏记 `openai-official`」）。Java 侧 `agentscope-extensions-model-openai-official` 是**独立 Maven 子模块**，依赖官方 `openai-java` SDK；provider id = **`openai-official`**，模型 id 前缀 = **`openai-official:`**（`OpenAIOfficialModelProvider` 的 `MODEL_ID` 正则 `openai-official:.+`），并有独立 SPI 注册文件。
> ⚠️ **关键区分**：Rust 侧 `crate::responses`（`OpenAiResponsesModel` / `OpenAiResponsesProvider`，provider id = core 常量 `openai-responses` = `"openai-responses"`）**不是**本子模块的等价物——provider id、SDK、id 前缀三者全不同，**不可互相顶替**。
> **已完成（2026-10-29 勘误）**：原记「未对照（真实缺口）」，实际 见 history/MODEL__MIGRATION_HISTORY.md 已 Phase 1 完整落地（`openai_official` 10 文件：provider id `openai-official` 可解析 + `Model` 调用链 + 错误翻译），见 history/MODEL__MIGRATION_HISTORY.md 关闭 Phase 2 三项（ProxyConfig / `x-should-retry` / 枚举白名单）；依据见 history/MODEL__MIGRATION_HISTORY.md / 见 history/MODEL__MIGRATION_HISTORY.md / 见 history/MODEL__MIGRATION_HISTORY.md / 见 history/MODEL__MIGRATION_HISTORY.md，用例 `provider::tests::registers_and_resolves_in_core_registry`、`request::tests::option_enums_are_validated_against_the_java_whitelists`。下表逐行勘误，原缺口描述保留。**单元格内的「部分完成 / 不等价 / 缺失 / 无对应」均为 2026-10-22 原判；同一单元格后半段的「已完成（2026-10-29 勘误）」为最终结论。**
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `openaiofficial.OpenAIResponsesChatModel`（415 行） | Responses 协议聊天模型主类 | `responses::OpenAiResponsesModel`（`src/responses/mod.rs`，691 行） | **部分完成**：请求/流式/解析主干有对等物；但 Java 侧 `ResponsesMultiAgentFormatter` 注入、`List<OpenAIServerTool> serverTools` 注入、`additionalBodyParams` 白名单 fail-fast **无对应**。**已完成（2026-10-29 勘误）**：原记三项「无对应」，实际 `openai_official::model::OpenAIResponsesChatModel` 已覆盖 stream/非流式 + options 合并 + 连接字段校验，formatter / serverTools / `additionalBodyParams` 白名单分别见 history/MODEL__MIGRATION_HISTORY.md 与 见 history/MODEL__MIGRATION_HISTORY.md 第 3 项 |
| `openaiofficial.OpenAIOfficialModelProvider`（118 行） | SPI 注册 provider（前缀 `openai-official:`） | `responses::OpenAiResponsesProvider`（provider id `openai-responses`，`supports()` 返回 `true` 兜底） | **不等价**：provider id 与前缀均不同，Rust 侧无 `openai-official:` 前缀解析。**已完成（2026-10-29 勘误）**：原记「不等价 / 无前缀解析」，实际 `openai_official::provider::OpenAIOfficialModelProvider` 以 provider id `openai-official`、前缀 `openai-official:` 注册于 `default_openai_official_registry()`；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `provider::tests::registers_and_resolves_in_core_registry` |
| `openaiofficial.ResponsesRequestMapper`（**834 行**，本子模块最大类） | `Msg` → Responses `ResponseCreateParams` 全量映射 | `responses::build_request`（`src/responses/mod.rs:226`） | **部分完成**：system→instructions、user→easy message、tool_use→function_call、tool_result→function_call_output 已覆盖；**多智能体分组 / 多模态图片 / `additionalBodyParams` 白名单 fail-fast / `prompt_cache_key` 等 body 参数**无对应。**已完成（2026-10-29 勘误）**：原记四项「无对应」，实际 `openai_official::request` 已覆盖（options / 白名单 + rejected fields fail-fast / history 含多模态 image / data / reasoning 回放 / `prompt_cache_key`）；依据 见 history/MODEL__MIGRATION_HISTORY.md / 见 history/MODEL__MIGRATION_HISTORY.md 第 3 项 |
| `openaiofficial.ResponsesMultiAgentFormatter`（355 行） | 多智能体分组（SYSTEM / TOOL_SEQUENCE / AGENT_CONVERSATION / BYPASS）+ `<history>` 包裹 | **无** | **缺失**：Rust `responses` 无任何 `<history>` / 多智能体分组实现（`responses/mod.rs` 全文无 `history`）。**已完成（2026-10-29 勘误）**：原记「无 / 缺失」，实际 `openai_official::formatter::ResponsesMultiAgentFormatter` 已落地，`crate::responses` 亦于 见 history/MODEL__MIGRATION_HISTORY.md 补齐 `merge_multi_agent_history`；依据 见 history/MODEL__MIGRATION_HISTORY.md / 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `multi_agent_merges_agent_turns_and_keeps_system_and_tool_sequences` |
| `openaiofficial.ResponsesStreamingAssembler`（355 行） | SSE 流式装配（reasoning / message / function_call 分桶） | `responses::ResponsesIncrementer`（`:397`） | **部分完成**：text delta / reasoning delta / function_call arguments delta → `ToolUse` 已覆盖（`src/responses/mod.rs:9-13` 文档 + `accumulate`/`into_final`）；**`encryptedContent` / reasoning item 回放 / BYPASS 分支**无对应。**已完成（2026-10-29 勘误）**：原记三项「无对应」，实际 `openai_official::parser::ResponsesStreamingAssembler` 已覆盖 output_item.added/done、reasoning summary / text / encrypted、终态 reasoning replay 元数据与 BYPASS 分组；依据 见 history/MODEL__MIGRATION_HISTORY.md |
| `openaiofficial.ResponsesResponseParser`（211 行） | 非流式响应 → `ChatResponse`（reasoning/message/function_call 分桶 + refusal 短路） | `responses::ResponsesIncrementer::into_final` + `status_string` | **部分完成**：非流式与流式共用增量器；**refusal 短路（不可重试异常）**无对应。**已完成（2026-10-29 勘误）**：原记「refusal 短路无对应」，实际 `openai_official::parser::parse_response` 在 refusal 时短路为错误；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `parser::tests::refusal_short_circuits_to_error` |
| `openaiofficial.ResponsesHelper`（194 行） | 辅助构造 | 无独立类型 | 功能级可由 `responses` 的 `easy_message` / `to_request_tools` / `to_tool_choice` 覆盖，但**无一一对应物**。**已完成（2026-10-29 勘误）**：原记「无独立类型」，实际 `openai_official::parser::{extract_response_metadata, extract_usage}` 已覆盖（id/status/incomplete_reason/citations + 各类 token + wall-clock）；依据 见 history/MODEL__MIGRATION_HISTORY.md |
| `openaiofficial.OpenAISdkClientFactory`（139 行） | SDK client 工厂 | 无 | **设计决定候选**：Rust 用 `async-openai` 的 `Client::with_config`，不需工厂层。**已完成（2026-10-29 勘误）**：原记「候选设计决定」，实际 `openai_official::model::{default_transport, build_http_request}` 承担工厂职责，且 `ProxyConfig` / `build_client` 已实现（见 history/MODEL__MIGRATION_HISTORY.md 第 1 项 / 见 history/MODEL__MIGRATION_HISTORY.md 第 1 项）；依据 见 history/MODEL__MIGRATION_HISTORY.md |
| `openaiofficial.OpenAIOfficialConstants`（70 行） | provider id / `additionalBodyParams` 白名单 / metadata 命名空间键 | 无 | **缺失**：白名单 fail-fast 行为是 Java 侧的实际请求校验，Rust 无。**已完成（2026-10-29 勘误）**：原记「缺失」，实际 `openai_official::constants`（provider id / 前缀 / 默认 base URL / env 名 / 10 项 body-params 白名单 / 元数据键 / 可重试集）已落地；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `provider::tests::create_applies_builder_options_and_strips_them`（白名单 honor） |
| `openaiofficial.OpenAIErrorTranslator`（128 行） | SDK 异常 → 统一异常 | `common::error::ModelError` + `responses::map_openai_error`（`:216`） | **部分完成**：`map_openai_error` 目前只映射 `ApiError`→`Error::HttpStatus` / 其余→`Error::Model`，**未走 `ModelError::from_parts` 的结构化归一**（与 `openai/mod.rs:338` 的实现不一致——后者已用 `ModelError::from_parts` + `ErrorKind::Timeout`/`Network`）。⚠️ **本轮新发现的不一致点**。**已完成（2026-10-29 勘误）**：原记「未走 `ModelError::from_parts`」，实际 `openai_official::error` 已用 `ModelError::from_parts` + `ErrorKind`（`error.rs:55`），408/409/429/5xx 可重试、401/403/404/422 不可重试；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `error::tests::official_retryable_status_set_includes_408_and_409`。（`responses::map_openai_error` 的同类问题已于 2026-10-22 修复，见 history/MODEL__MIGRATION_HISTORY.md） |
| `openaiofficial.OpenAIOfficialModelException`（101 行） | 模块异常 | `agentscope_core::Error` | **设计决定**：不建平行异常体系 |
| `openaiofficial.credential.OpenAIOfficialCredential`（127 行） | 凭据载体 | 无独立类型；经 `ModelCreationContext.api_key` / `OPENAI_API_KEY` | 与 `openai.credential.OpenAICredential` 同处理（§2.3 已记为待办/待裁定）。**已完成（2026-10-29 勘误）**：原记「无独立类型」，实际 `openai_official::credential::OpenAIOfficialCredential` 已落地（显式值优先 + env 回退 + blank 视为缺失）；依据 见 history/MODEL__MIGRATION_HISTORY.md（`openai.credential.OpenAICredential` 本身已裁定为 **设计决定，不迁移**，见 §2.3） |
| `openaiofficial.tool.OpenAIServerTool`（125 行） | 内置 server tool 声明（web_search / code_interpreter / image_generation / tool_search） | **无** | **缺失**：Rust `responses::to_request_tools`（`:334`）**只构造 `Tool::Function`**，无任何内置 server tool 变体。⚠️ 与 gemini 侧 `GeminiServerTool`（已实现）对比明显。**已完成（2026-10-29 勘误）**：原记「缺失」，实际 `openai_official::server_tool::OpenAIServerTool`（web_search / code_interpreter / image_generation / tool_search，其余拒绝）已落地；依据 见 history/MODEL__MIGRATION_HISTORY.md |
| `openaiofficial.tool.ResponsesServerToolHelper`（**544 行**，本子模块第二大类） | server tool 块 ↔ SDK 互转 + 结果状态映射 | **无** | **缺失**：Rust 侧无 server-tool 声明/校验/结果映射。**已完成（2026-10-29 勘误）**：原记「缺失」，实际 `openai_official::server_tool::{ResponsesServerToolHelper, HostedToolSearchPairing}`（merge tools / include 列表 / call-result 解码 / tool_search 配对 / 原始 item 回放）已落地；依据 见 history/MODEL__MIGRATION_HISTORY.md |
| `resources/META-INF/services/...ModelProvider` | SPI 注册（`OpenAIOfficialModelProvider`） | `responses::default_responses_registry` | **设计决定**（Rust 无 `ServiceLoader`），但 provider id 不一致。**已完成（2026-10-29 勘误）**：原记「provider id 不一致」，实际 `default_openai_official_registry()` + provider id `openai-official` / 前缀 `openai-official:` 已对齐；依据 见 history/MODEL__MIGRATION_HISTORY.md |

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
| dashscope DTO 接线 | `dashscope.dto.*`（15 类） | 已完成 | `src/dashscope/dto.rs` **18 个** pub 类型（15 个对应 Java + Rust 专属 `DashScopeContent` untagged enum / `PublicKeyData` / `CacheControl` 别名）；含 `DashScopePublicKeyResponse`（凭证加密公钥，**2026-10-28 起已由 `fetch_public_key()` 消费**） |
| anthropic provider 基线 | `anthropic.*`（**13** 类） | **已完成** | `src/anthropic/{dto,credential,converter,parser,model,provider,mod}.rs`，**61** 个单测；`lib.rs` 已声明 `pub mod anthropic;`；详见 §2.4 |
| gemini provider 基线 | `gemini.*`（**11** 类） | **已完成** | `src/gemini/{dto,credential,converter,parser,model,provider,mod}.rs`，**58** 个单测；`lib.rs` 已声明 `pub mod gemini;`；详见 §2.5 |
| OpenAI provider | `openai.*`（53 类） | **已完成（2026-09-21 自 core 迁入）** | `src/openai/mod.rs`（`OpenAiChatModel`/`OpenAiProvider`/`default_registry`，**1380 行**）；同批迁入 `responses`（**691 行**）与 `anthropic_async`（**712 行**）。原「设计决定（不迁移）」口径已作废；Java `compat.*`（12 类）**已完成**（2026-10-31，`openai::compat` + 7 用例）、`exception.*`（8 类）/ `OpenAIMultiModalTool` **已完成（设计决定，不迁移）**（§3.2/§3.4） |
| **`openai-official` 子模块对照** | `openaiofficial.*`（**14 类** / 3716 行） | **已完成（2026-10-29 勘误）** | **2026-10-22 复核新增**：Java 侧 `openai-official` 为**独立子模块**（`OpenAIResponsesChatModel` / `ResponsesRequestMapper` / `ResponsesMultiAgentFormatter` / `ResponsesStreamingAssembler` / `ResponsesResponseParser` / `ResponsesHelper` / `OpenAIOfficialModelProvider` / `OpenAIOfficialConstants` / `OpenAIErrorTranslator` / `OpenAIOfficialModelException` / `OpenAISdkClientFactory` / `credential.OpenAIOfficialCredential` / `tool.OpenAIServerTool` / `tool.ResponsesServerToolHelper`），provider id = **`openai-official`**（前缀 `openai-official:`），走 **官方 `openai-java` SDK**。Rust 侧 `crate::responses` 虽同为 Responses 协议，但 provider id = core 常量 **`openai-responses`**（`provider.rs:20`）、基于 `async-openai` 而非官方 Java SDK——**二者不是同一物**，不可互相顶替。**已完成（2026-10-29 勘误）**：原记「待办」，实际 `src/openai_official/` 10 文件已交付（provider id `openai-official` / 前缀 `openai-official:` / `default_openai_official_registry()`），见 history/MODEL__MIGRATION_HISTORY.md 已逐类对照；依据 见 history/MODEL__MIGRATION_HISTORY.md / 用例 `provider::tests::registers_and_resolves_in_core_registry` |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama embedding 调用链 | `ollama.dto.OllamaEmbedding*` + `OllamaHttpClient` 的 `POST /api/embed` | **已完成** | `ollama::model::OllamaChatModel::embed` / `embed_with_options` → `POST {base_url}/api/embed`（`EMBED_ENDPOINT`）。**2026-10-31 收口**：`embed_with_options` 不再忽略入参——`options.to_options_map()` 与 `keep_alive` 已写入请求体（与 `/api/chat` 同语义）；测试 `ollama::model::tests::embed_posts_to_embed_endpoint`、`embed_forwards_per_call_options_and_keep_alive` |
| Ollama 会话合并 | `ollama.formatter.OllamaConversationMerger` | **已完成**（2026-10-28） | `ollama::converter::merge_conversation`（`<history>` 包裹、`"<name>: <text>"`、tool result `"<name> (<tool>): <out>"`、base64 图片收集 / 远程图片降级），见 history/MODEL__MIGRATION_HISTORY.md |
| Ollama 多智能体格式化 | `ollama.formatter.OllamaMultiAgentFormatter` | **已完成**（2026-10-28） | `to_multi_agent_messages(_with)`（system 独立 / 单条非 tool 消息直通 / agent 组合并 / tool 序列直通 / 首组 prompt / `promote_tool_result_images` 的 `<system-info>` 提升），分组复用 `common::multi_agent`；测试 `ollama::converter::tests::multi_agent_promotes_tool_result_images_into_a_user_message` 等 4 条 |
| Ollama 远程图片下载 | `ollama.formatter.OllamaMediaConverter` | **已完成**（2026-10-31） | 新增进程级 `common::RemoteMediaCache`（`prefetch` 走 `HttpTransport::get_bytes`，新增 reqwest 实现 + `MemoryTransport.bytes_response` fake）；`DefaultMediaResolver` 对 http(s) URL 先查缓存、命中即按 base64 内联，未命中仍返回**显式错误**（不隐式联网）。测试 `common::media::tests::prefetch_downloads_through_the_transport_and_caches`、`default_resolver_serves_prefetched_remote_media`、`ollama::converter` 既有远程图片降级用例 |
| anthropic 子模块整体 | `anthropic.*`（**13** 类 / **3171** 行） | **已完成** | cache control / thinking / media / multi-agent formatter / server tool 均已落地；传输层自建（§4.2 选项 b） |
| anthropic 并行 tool-use 拆分 | `AnthropicMessageConverter#collectSplitToolResults` / `#convertParallelToolCalls` | **已完成**（2026-10-31） | `anthropic/converter.rs`：`client_tool_uses` / `collect_split_tool_results` / `should_split_parallel_tool_calls` / `convert_parallel_tool_calls`，把「一条 assistant 多 tool_use + 多条 tool_result」展开为严格 `assistant(tool_use) → user(tool_result)` 交替；前置文本只挂在第一条 assistant 上、tool 数/结果数不匹配时**不拆分**（保持原样）。server tool 不参与拆分。测试 `anthropic::converter::tests::parallel_tool_uses_split_into_alternating_pairs`、`parallel_tool_uses_stay_unsplit_when_a_result_is_missing` |
| anthropic 多智能体 system 前导文本细节 | `AnthropicMultiAgentFormatter` | **已完成**（2026-10-31 复核后关闭） | 分组（`common::multi_agent::group_messages`）、首组 prompt、`<history>` 包裹、媒体在文本前 flush 均与 Java ground-truth 一致；system 消息统一由 `to_anthropic_messages` 渲染（无 `<system-info>` 特殊路径，这是 Java 仅用于 ollama/dashscope 的分支）。测试 `conversation_merge_wraps_history_tags`、`conversation_merge_flushes_text_before_media`、`multi_agent_groups_tool_sequences_separately` |
| **anthropic 内置 server tool 声明** | `anthropic.tool.AnthropicServerTool` + `formatter.AnthropicServerToolHelper` | **已完成**（2026-10-28） | `anthropic::server_tool` + `converter::{to_anthropic_tools_with_server, build_request_with_server_tools}`；解析侧已存在。见 history/MODEL__MIGRATION_HISTORY.md |
| gemini 子模块整体 | `gemini.*`（**11** 类 / **3071** 行） | **已完成** | thought signature 持久化、thinking、多模态、multi-agent、内置 server tool 均已落地 |
| gemini Vertex AI 路径 | `GeminiModelProvider`（`project`/`location`/`vertexAI` + `GoogleCredentials`） | **已完成（设计决定，不迁移）** | 依据：Vertex AI 端点主机与认证（OAuth2 服务账号）与 API-key 流程正交，Rust 侧无 Google 凭据链；离线 adapter 在 `gemini/provider.rs:78` 读 `vertexAI`、`:83` **显式报错拒绝**而非静默降级。影响：Vertex 用户需自建适配。替代/测试：`provider::tests::create_rejects_vertex_ai` |
| gemini 远程图片下载 | `GeminiMediaConverter#readFileAsBytes`（http/https） | **已完成**（2026-10-31） | 同 Ollama 行：走 `common::resolve_media` + 共享 `RemoteMediaCache`，预取后按 base64 内联为 `inlineData`，未预取时经 `media_fallback` 回退文本并打 `unsupported_media_type` 诊断（`gemini/converter.rs`）。测试同上两条 media 用例 + `gemini::converter` 既有媒体降级用例 |
| dashscope 子模块整体 | `dashscope.*`（29 类 / **8791** 行） | **已完成**（2026-10-31 收口） | 加密、会话合并、多智能体、multimodal 端点、`DashScopeMultiModalTool` 归属均已裁定（见下行）。测试密度映射见 history/MODEL__MIGRATION_HISTORY.md |
| `DashScopeMultiModalTool`（1641 行） | `dashscope.tool.DashScopeMultiModalTool` | **已完成（设计决定，不迁移）** | 依据：该类依赖 Java SDK 的 `ImageSynthesis` / `VideoSynthesis` / `SpeechSynthesizer` / `Recognition`（见文件头 import），是**独立生成式 API 的工具封装**（文生图/图生文/TTS/ASR），不参与 chat 请求编码，且 Rust 无对应 SDK。影响：无（模型层行为零差异）。替代：宿主用 `Tool` 直连 DashScope HTTP 接口 + 轮询；模型层已支持其产出的 `ImageBlock`/`AudioBlock` |
| **dashscope 会话合并 / 多智能体格式化** | `DashScopeConversationMerger` / `DashScopeMultiAgentFormatter` | **已完成**（2026-10-28） | `merge_conversation` / `merge_multimodal_conversation` / `to_multi_agent_messages` + provider `multiAgentFormatter` 接线。见 history/MODEL__MIGRATION_HISTORY.md；测试 `multimodal_tool_result_keeps_its_media_parts`（2026-10-31 新增，覆盖 multimodal 端点下的 tool 结果媒体保留） |
| OpenAI 兼容厂商差异化行为 | `openai.compat.{deepseek,glm,kimi,minimax}`（12 类） | **已完成**（2026-10-31，方案 (b)） | 新增 `src/openai/compat.rs`：`vendor_for_model` + `apply_vendor_rules`，在 `openai/mod.rs` 序列化并在 `additional_body_params` / `reasoning_content` 注入**之后**执行（extra params 仍然最后生效，与 Java 顺序一致）。规则：四厂商均去 `strict`；DeepSeek 给缺失 `reasoning_content` 的 assistant 消息回填空串；GLM 去 penalty、`temperature∈[0,1]`/`top_p∈[0.01,1]` 夹取、`required`/指定工具降级为 `auto`、去消息 `name`、补 `user` 消息；Kimi 对 `kimi-*` 去采样参数、`reasoning_effort` 仅 `kimi-k3` 保留、K2.x 降级 `auto`；MiniMax 恒去 `tool_choice`（`none` 时连 tools 一起去）、补 `reasoning_split:true`。测试 `openai::compat::tests::*` 7 条 |
| OpenAI 多模态工具 | `openai.tool.OpenAIMultiModalTool`（594 行） | **已完成（设计决定，不迁移）** | 裁定：归属**工具层**（Java 侧 `import io.agentscope.core.tool.Tool`，实现文生图/图生文/TTS/STT），不属于 MODEL 模块；Rust 侧对应物是宿主注册的 `Tool`。影响：无模型层差异。替代：宿主用 `Tool` + `ImageBlock`/`AudioBlock` 自行封装 |
| OpenAI chat 多智能体格式化 | `openai.formatter.OpenAIMultiAgentFormatter` / `OpenAIConversationMerger` | **已完成**（2026-10-31） | 共享实现下沉到 `common::multi_agent::{merge_agent_group, merge_multi_agent_history}`（原为 `responses` 私有）；`openai::build_request` 消费 `additional_body_params["multiAgentFormatter"]`（`true` = 默认 prompt / 字符串 = 自定义）：system 独立、tool 序列直通、每段 agent 消息合并为一段 `<history>`、**仅首组**带 prompt；选择器同时在 `stream` 侧从 `extra_body` 剔除，绝不进 wire。用例 `openai::mod::tests::multi_agent_formatter_merges_agent_turns_into_one_history_message`（兼验无 selector 泄漏）、`responses::tests::multi_agent_merges_agent_turns_and_keeps_system_and_tool_sequences`（共享实现回归） |
| **`openai-official` 子模块整体** | `openaiofficial.*`（**14 类 / 3716 行**） | **已完成（见 history/MODEL__MIGRATION_HISTORY.md）/ Phase 2 残留已清**（2026-10-28） | 主体交付见 history/MODEL__MIGRATION_HISTORY.md；Phase 2 三项（proxy / `x-should-retry` / 枚举白名单）见 history/MODEL__MIGRATION_HISTORY.md |
| 原生按需工具加载（tool search） | `4b6c59af7`：`ToolSearchTool` + `ResponsesToolHelper` + Anthropic deferred tools | **已完成**（2026-10-31） | 新增 `common/tool_search.rs`（provider 无关编码层：`ToolSearchMode`、`tool_search_schema`、`is_tool_search_schema`、`loaded_tool_schemas`、`is_client_tool_search_call`、`has_client_tool_search_call`）。Anthropic：`AnthropicTool.defer_loading` + tool result 的 `tool_reference` 块 + cache marker 跳过 deferred 定义；OpenAI Responses：`tool_search` 声明（`execution: client`）、`tool_search_call` 解码为本地调用（metadata 标记）、`tool_search_output` 回放已加载定义 + `strictTools` 默认值回填、hosted 与 client 两种 tool search **互斥**报错。测试见 history/MODEL__MIGRATION_HISTORY.md |
| Java 测试用例逐条对照迁移 | Java 测试 **54660** 行（去空行）/ **63416** 行 | **已完成（映射表见 history/MODEL__MIGRATION_HISTORY.md）** | 逐条照搬**不适用**（依赖 Java 测试框架/JUnit 参数化与真实厂商 SDK）。改为「Java 测试类 → Rust 测试模块」映射表 + 每类关键用例覆盖说明；本轮 5 个 Java 提交的用例已 1:1 找到 Rust 对应用例或新增 |
| 真实厂商端点联调 | `e2e-tests`（36 文件 / 17 个含 `@Test` 的测试类 / 8556 行） | **已完成（设计决定，不迁移）** | 需真实 API key（7 个厂商 env）+ `ENABLE_E2E`；Rust 坚持离线策略，网络层以 `HttpTransport` seam + `MemoryTransport` 覆盖 |
### 3.3 增强与清理（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Ollama llama.cpp loader 长尾参数 | `ollama.options.OllamaOptions`（**45.0 KB / 1361 行**） | **已完成**（2026-10-31） | 不逐一镜像 ~1300 行字段，改为**透传**：`OllamaOptions.extra: BTreeMap<String, Value>` + `with_extra(key, value)`，`to_options_map` 末尾写入（同键时 typed 字段优先），`merge` 取并集且 per-call 侧胜出——`num_batch` / `tfs_z` / `mirostat` 等可直接下发。测试 `ollama::options::tests::extra_options_reach_the_wire_map_and_lose_to_typed_fields`、`merge_unions_extra_options_with_the_per_call_side_winning` |
| 统一远程媒体 resolver | `MediaUtils` + 各 `*MediaConverter` | **已完成**（2026-10-31） | seam 原本已有（`MediaResolver` + `resolve_media`），本轮补上**内置远程下载**：`HttpTransport::get_bytes`（reqwest 实现解析 `content-type`、非 2xx 走 `ModelError` 归一；`MemoryTransport.bytes_response` 为离线 fake）+ `RemoteMediaCache::prefetch`（base64 入缓存，重复 preftch 直返）+ 进程级 `remote_media_cache()`；`DefaultMediaResolver` 命中缓存即内联，未命中**显式报错**（保留「不隐式联网」原则）。三个 provider 统一：Gemini/Ollama 走 `resolve_media`，Anthropic 对已预取 URL 内联 base64、否则仍 URL 透传（Anthropic 原生支持）。测试 `common::media::tests::prefetch_downloads_through_the_transport_and_caches`、`prefetch_reports_transports_without_get_support`、`anthropic::converter::tests::prefetched_remote_image_is_inlined_instead_of_passed_through` |
| 重试 / 退避 / 超时可配置化 | — | **已完成（本 crate 范围内）** | `TransportConfig`（connect/read/keepalive/retry/backoff）保持不变；**per-call 执行预算已可到 wire**：`HttpRequestSpec.timeout` 由 `GenerateOptions.execution_config` → `OllamaOptions.execution_config` → `resolved_execution_config` 驱动（`ollama/model.rs::explicit_timeout`）。**2026-10-31 新增**：`ollama/options.rs::resolved_execution_config` 与 `ExecutionConfig::merge_configs` 三级合并（per-call > model default > `model_defaults()`）。测试 `ollama::model::tests::per_call_execution_timeout_reaches_the_request`、`model_default_execution_timeout_applies_when_the_call_omits_it`、`calls_without_an_execution_budget_keep_the_transport_timeout`。**剩余为设计决定**：`GenerateOptions` 新增 timeout/retry 字段属 core 变更（已记 §4.2 第 10 条）；流式不重试（避免重复已发出增量，与 Java 流式路径一致） |
| 指标 / 可观测性埋点 | slf4j 日志 | **已完成**（2026-10-31） | `common::diagnostics` 原只有 tracing；新增 `DiagnosticSink` + `set_diagnostic_sink` / `clear_diagnostic_sink`，`emit` 先分发到宿主 sink 再落 tracing。至此 9 处降级点（`tool_choice_downgraded` / `tool_choice_omitted` / `sampling_unsupported` / `unsupported_media_type`）的结构化收集**不再局限于日志**。测试 `common::diagnostics::tests::installed_sink_receives_emitted_diagnostics`、`collector_records_without_emitting_twice` |
| `agentscope-harness` 依赖清理 | — | **已完成**（2026-09-21） | `Cargo.toml` 已移除；`src/**` 零引用（提交 `d86a578`） |
| 独立 `tests/` 集成测试 | — | **已完成**（2026-10-12） | `tests/model_extensions_integration.rs`（5 个集成用例，验证公开导出面） |
| **`ModelMap` / `strip_reasoning_suffix` 未接线** | Java `ModelMap` / 推理后缀 | **已完成**（2026-10-31） | 接线到 `OpenAiProvider`：`with_model_map(ModelMap)`（链式解析，成环转 `Error::Model`）与 `with_reasoning_suffix(true)`（`-thinking-<n>` / `-<effort>` 从 wire 模型名剔除，并回填 `options.thinking_budget` / `reasoning_effort` + `enable_thinking`；已显式赋值则不覆盖）。默认关闭（向后兼容）。测试 `openai::mod::tests::provider_applies_the_model_map_before_creating_the_model`、`provider_reports_model_map_cycles_instead_of_looping`、`provider_strips_the_reasoning_suffix_and_seeds_the_thinking_knobs` |
| **`base64` / `chrono` / `thiserror` 依赖未使用** | — | **已完成**（2026-10-31） | 复核更正：`base64` 实际已被 `dashscope/crypto.rs`、`dashscope/encryption.rs` 使用，本轮又用于 `media.rs::prefetch`；`chrono` / `thiserror` 确认 0 引用 → 已从本 crate `Cargo.toml` **删除**（未动根 `[workspace.dependencies]`） |
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
| **`openai-official` 的「官方 SDK 路径」不引入** | `openai-official` 子模块 `pom.xml`（依赖 `openai-java` SDK） | **已完成（设计决定，不迁移）**（2026-10-29 裁定）：按「不引入官方 SDK」执行——`openai_official` 直接对 Responses wire protocol，见 history/MODEL__MIGRATION_HISTORY.md 类级表已覆盖 §2.8 全部能力缺口。历史叙述：Java 侧用官方 `openai-java` SDK，Rust 侧 `crate::responses` 走 `async-openai` 0.41（与 `anthropic` vs `anthropic_async` 同构，见 §4.2-7） |

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
  - （2026-10-28 实测）`cargo build -p agentscope-extensions-model` → **Finished（0 error，本 crate 0 warning）**；`cargo test -p agentscope-extensions-model` → **369 lib + 5 集成 = 374 passed / 0 failed / 0 ignored**（基线 327+5，本轮 +42）；源文件 56 个 / 22111 物理行。详见 history/MODEL__MIGRATION_HISTORY.md
  - （2026-10-22 静态复核，**未编译**）测试属性静态计数 **267**（262 内联 + 5 集成）；42 个源文件 / 15004 物理行（去空行 13833）。
  - （2026-10-12 优化轮实测，对标 new-api；详见 history/MODEL__MIGRATION_HISTORY.md）`cargo test -p agentscope-extensions-model` → **236 passed / 0 failed**（231 lib + 5 集成）；`cargo check --workspace` → Finished（仅其它 crate 存量 warning）
  - （2026-09-19 实测）`cargo build -p agentscope-extensions-model` → **Finished**（0 error；本 crate 0 error/0 warning）
  - （2026-09-19 实测）`cargo test -p agentscope-extensions-model` → **169 passed / 0 failed / 0 ignored**
  - （2026-09-21 实测，core 实现迁入后）`cargo test -p agentscope-extensions-model` → **198 passed / 0 failed / 0 ignored**；`cargo check --workspace` → **Finished**（0 error，仅其它 crate 存量 warning）
- **已知问题**
  1. ~~`src/ollama/dto.rs` 的 embedding DTO 与 `EMBED_ENDPOINT` 常量无调用方（§3.2）。~~ **已解决**（2026-10-12，`OllamaChatModel::embed` 已接线并有单测）。
  2. ~~`agentscope-harness` 为未使用依赖（§3.3）。~~ **已解决**（2026-09-21 从 `Cargo.toml` 移除，`d86a578`）。
  3. ~~anthropic 并行 tool-use 拆分、gemini Vertex AI 与**内置**远程媒体下载为**非本轮范围**细化缺口（§3.2/§3.3）。~~ **已解决（2026-10-31）**：三者均已收口（并行拆分已实现 + 2 用例；Vertex AI 裁定不迁移并显式报错；远程媒体经 `RemoteMediaCache::prefetch` 内置支持）。
  4. **（2026-10-22 复核新增）`openai-official` 子模块（14 类 / 3716 行）完全未对照**（原判；2026-10-29 已关闭，见本段末勘误）——Rust `crate::responses` 只覆盖 Responses 协议主干，缺 multi-agent formatter / server tool / body-params 白名单（§2.8）。**已完成（2026-10-29 勘误）**：原记「完全未对照」，实际 见 history/MODEL__MIGRATION_HISTORY.md 已交付 `src/openai_official/` 10 文件并逐类对照，见 history/MODEL__MIGRATION_HISTORY.md/见 history/MODEL__MIGRATION_HISTORY.md 补齐 Phase 2 与 `responses` 多智能体分组
  5. ~~**（2026-10-22 复核新增）`ModelMap` / `strip_reasoning_suffix` 未被任何 provider 消费**（§3.3）。~~ **已解决（2026-10-31）**：已接线到 `OpenAiProvider`（`with_model_map` / `with_reasoning_suffix`，3 条用例）。
  6. ~~**（2026-10-22 复核新增）`base64` / `chrono` / `thiserror` 三个依赖声明但零使用**（§3.3）。~~ **已解决（2026-10-31）**：`base64` 实际在用（dashscope 加密 + media 预取）；`chrono` / `thiserror` 已从本 crate `Cargo.toml` 删除。
  7. 无阻塞：2026-10-28 实测 build/test 全绿（374 passed）。
  8. **（2026-10-28）dashscope 加密已内置真实现**：默认 `AesRsaCryptoProvider`（`aes-gcm` 0.10 + `rsa` 0.9 + `rand` 0.8），AES-256-GCM（12 B IV、128 bit tag）+ RSA PKCS#1 v1.5 包 base64(AES key)；`UnavailableCryptoProvider` 保留为显式失败的备选，注入缝 `DashScopeCryptoProvider` 不变（见 history/MODEL__MIGRATION_HISTORY.md）。
  9. **（2026-10-28）`prompt_cache_options` 白名单收紧**：仅接受 `mode`/`ttl`，其他 key fail-fast（Java SDK 会对未知字段报错，此处对齐）。

### 4.2 下一步与阻塞

> **2026-10-31 状态：本表已清零。** 原第 1–9 条全部落为「已完成」或「已完成（设计决定，不迁移）」；第 10–11 条为**跨模块需求**（不阻塞本 crate，纯记录）。

1. **【已完成】anthropic / gemini 的传输与 SDK 选型**：按选项 **(b)** 执行——复用本 crate `HttpTransport` + `reqwest` 手写 wire 协议，未引入新依赖，测试全离线。
2. **【已完成】原「剩余 P1 细化缺口」**：① anthropic 并行 tool-use 拆分 → `anthropic/converter.rs` 三个新函数 + 2 条单测；② gemini Vertex AI → **设计决定不迁移**（显式报错，`create_rejects_vertex_ai`）；③ 统一远程媒体下载 → `RemoteMediaCache` + `HttpTransport::get_bytes`（三个 provider 均已接入）。
3. **【已完成】OpenAI 兼容厂商差异化（原跨模块裁定请求）**：裁定为 **(b) 在本 crate 实施**——新增 `src/openai/compat.rs`（DeepSeek/GLM/Kimi/MiniMax 四个 vendor 的请求体校正，7 条单测），**未修改 core**。仍不建每厂商独立 `*ModelProvider`/`*MultiAgentFormatter`：Java 那套的 multi-agent 分组已由 `common::multi_agent` 统一承担，厂商只影响请求体字段。
4. **【已完成】`openai.tool.OpenAIMultiModalTool` 归属**：裁定为**工具层**（Java 侧实现 `io.agentscope.core.tool.Tool`，能力是文生图/图生文/TTS/STT 的独立 API 调用），MODEL 模块**不迁移**；宿主用 `Tool` + `ImageBlock`/`AudioBlock` 封装即可，模型层零改动。
5. **【已完成】离线测试补强**（原第 5 条）：ollama 补 `embed_forwards_per_call_options_and_keep_alive` / `per_call_execution_timeout_reaches_the_request` 等 4 条；dashscope 补 `multimodal_tool_result_keeps_its_media_parts`；anthropic 补 `failed_tool_results_are_marked_is_error`、并行拆分 2 条、deferred tool_reference、cache marker 跳过、预取图内联；gemini 补 `build_request_honors_default_tool_choice`。
6. ~~**【非阻塞】依赖瘦身**~~ **已完成**（2026-09-21，`d86a578`；2026-10-31 又删除了 `chrono` / `thiserror`）。
7. **【已完成】`anthropic` 双实现并存**：裁定 **`crate::anthropic`（自建 wire + `HttpTransport`）为主路径**，`crate::anthropic_async` 保留但定位为 SDK 后端（`async-anthropic` 0.6 不支持 `defer_loading` / `reasoning_content` 回放，无法承担新能力）。**不改 core、不删任何实现**（删既有功能违反硬约束）。
8. **【已完成】`responses` vs `openai_official` 双实现**：维持双实现；`openai_official` 为能力主干（proxy / server tool / tool search / `x-should-retry`），`responses` 冻结为 Responses 协议主干（仅修 bug，不接新能力）。合并动作**不在本 crate 单方面执行**（已在此明确记录为终态）。
9. **【已完成】dashscope 加密与公钥拉取**：内置 `AesRsaCryptoProvider` + `HttpTransport::get_json` + `fetch_public_key()` 缓存，见 history/MODEL__MIGRATION_HISTORY.md。
10. **【跨模块需求（不阻塞，纯记录）】** 以下三项在 core 侧没有对应物，本 crate 以**局部适配层**绕过，不影响功能：
    - `Model` trait 无 `tool_search_mode()` → 以 inherent accessor 暴露（`AnthropicChatModel::tool_search_mode() = Deferred`、`OpenAIResponsesChatModel::tool_search_mode() = Client`）；
    - `ToolSchema` 无 `toolSearch` 显式标志 → 以**名称 + `query` 参数契约**识别（`tool_search::is_tool_search_schema`）；
    - `GenerateOptions` 无 timeout/retry 字段（现经 `execution_config` 走）+ `HttpTransport::get_bytes` 是本 crate 新加的 seam 方法（默认实现报错，不破坏外部实现）。
    若后续希望统一，请在 core 侧裁定后再开工；**本回合未改 core/harness**。
11. **【跨模块需求（不阻塞，纯记录）】`ModelRegistry` 的解析缓存语义（Java `9e716cb27`）**：Java 该提交修两个缺陷——`registerFactory` 不清解析缓存（旧 factory 的实例活到 JVM 结束）、并发首次解析各自建实例并丢弃 N-1 个。Rust 的 `ModelRegistry` 在 **core**（`crates/agentscope-core/src/model/provider.rs`，`:155`/`:159`），且**无任何解析缓存**：`register(&mut self)` 按 `provider_id` 用 `retain` 替换、`resolve(&self)` 每次现建（全仓 `rg resolved_cache|resolve_cache` 零命中）。⇒ 两个缺陷在本仓库**结构性不存在**，无需移植该补丁。本 crate 侧处置：新增 `tests/registry_semantics.rs`（4 用例）钉住四条不变量——重新注册在下一次解析立即生效、失败不缓存可重试、无隐藏实例缓存、`Arc<ModelRegistry>` 可跨线程共享。若 core 将来真引入缓存，**必须同时**实现「注册时按 model id 驱逐」+「单飞首次创建（并发共享一次、失败不缓存）」，并同步本表与 `tests/registry_semantics.rs`。

> **跨模块需求汇总（2026-10-31）**：本 crate **无阻塞**。上表第 10 条为「能力位缺口」（已有等价绕过）、第 11 条为 **core 侧 registry 缓存语义**（当前无缓存，故 Java `9e716cb27` 不适用，仅需 core 决策时知会）；本轮**未新增任何第三方依赖**、**未动根 `Cargo.toml`**、**未改 core/harness**。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-model` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-model` / `cargo test -p agentscope-extensions-model` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-model/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/MODEL__MIGRATION_HISTORY.md`](./history/MODEL__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
