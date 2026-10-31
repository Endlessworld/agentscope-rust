# MODEL 迁移执行历史（归档）

> 本文件是 [`../MODEL__GAP_TRACKER.md`](../MODEL__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 2026-10-12 优化轮（对标 Go 网关 new-api） · 7. 静态复核记录（2026-10-22） · 7. §4.3 P0 修复记录（2026-10-22，汇总方） · 7. P0 修复记录（2026-10-22）— OpenAI Chat Completions 非流式请求路径 · 8. §4.3 P0 #11 完成记录（2026-10-22）— `openai-official` provider · 9. 2026-10-28 执行记录（剩余 P1 关闭） · 10. 2026-10-31 loop 收口记录（对齐 Java HEAD `1524b3191`））。

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

> **2026-10-31 收口声明**：本节是 **2026-10-22 的历史快照**。文中「部分完成 / 未对照 / 待办 / 缺失」均为当时判定，截至 2026-10-31 **已全部关闭**（证据：§9.1–§9.6、§10.1–§10.6）；下方原文仅为追溯保留，**状态以 §2 + §3 + §4.2 + §10 为准**。

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
| **ollama** | 22 / 5217 行 | `src/ollama/*` 7 文件 / 1641 行 | **已完成**（2026-10-31） | `OllamaConversationMerger`（116 行）、`OllamaMultiAgentFormatter`（**508 行**）均**零对应**；`OllamaOptions`（1361 行）仅迁子集。（2026-10-29：两项 formatter 已闭环，见 §9.1；余 `OllamaOptions` 子集） |
| **dashscope** | 29 / 8791 行 | `src/dashscope/*` 7 文件 / 2469 行 | **已完成**（2026-10-31） | `DashScopeEncryptionUtils`（**AES-GCM + RSA，零实现**）、`DashScopeConversationMerger`（316 行）、`DashScopeMultiAgentFormatter`（406 行）、`DashScopeMultiModalTool`（**1770 行 / 93.5 KB**） | （2026-10-29：加密与两项 formatter 已闭环，见 §9.3 / §9.4；余 `DashScopeMultiModalTool`） |
| **anthropic** | 13 / 3171 行 | `src/anthropic/*` 7 文件 / 3002 行 | **已完成**（2026-10-31） | `tool.AnthropicServerTool`（96 行）**无等价物**（gemini 侧已有 `GeminiServerTool`）；响应侧并行 tool-use 交替拆分未实现。（2026-10-29：server tool 已闭环，见 §9.2；余并行拆分） |
| **gemini** | 11 / 3071 行 | `src/gemini/*` 7 文件 / 2682 行 | **已完成**（2026-10-31） | Vertex AI（设计决定拒绝）；**内置**远程媒体下载缺省（seam 已有） |
| **openai** | 53 / 9853 行 | `src/openai/mod.rs` 1380 行（**源自 core 迁入，非逐类重写**） | **已完成**（2026-10-31） | `compat.*` 12 类（GLM/Kimi/MiniMax formatter 差异化）、`credential.OpenAICredential`、`tool.OpenAIMultiModalTool`（594 行）、`OpenAIConversationMerger` / `OpenAIMultiAgentFormatter` |
| **responses**（Rust 独有） | — | `src/responses/mod.rs` 691 行 | **已就位（协议主干）** | 无 multi-agent 分组、无 server tool、`map_openai_error` 未走 `ModelError` 结构化归一 |
| **anthropic_async**（Rust 独有） | — | `src/anthropic_async/mod.rs` 712 行 | **已就位（SDK 路径）** | 与 `crate::anthropic` 双实现并存，待裁定（§4.2-7） |
| **openai-official** | **14 / 3716 行** | `src/openai_official/*` 10 文件 | **已完成**（2026-10-29，§8 / §9.5） | 全部 14 类；见 §2.8（multi-agent formatter 355 行 / request mapper 834 行 / server tool 669 行 / streaming assembler 355 行 / body-params 白名单）。（2026-10-29：已对照，见 §8 / §9.5） |
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

> **2026-10-31 收口**：下列 12 项已全部关闭——1–5 见 §8 / §9.1–§9.4，6–7 见 §7.1（responses 错误归一）/ §9.6，8–12 见 §3.3 / §10.3（`ModelMap` 接线、依赖瘦身、`Diagnostics` sink、`embed_with_options` 参数、`base_url` 注入）。原文保留以追溯。

**P0（阻塞核心链路）**
1. **`openai-official` 子模块（14 类 / 3716 行）完全未对照**——provider id（`openai-official` vs `openai-responses`）、id 前缀、SDK 三者全不同，`crate::responses` 不可顶替。详见 §2.8。（2026-10-29：已对照，见 §8 / §9.5）

**P1（重要功能）**
2. **`dashscope` 请求加密零实现**——Java `DashScopeEncryptionUtils`（181 行，AES-GCM 数据 + RSA 包 AES key）在 Rust 侧只有 `PUBLIC_KEYS_ENDPOINT` 常量与 `DashScopePublicKeyResponse` DTO，**无任何加密/解密代码**（`dashscope/dto.rs` / `model.rs` 全文无 `aes`/`rsa`/`encrypt`/`decrypt`）。（2026-10-29：加密已闭环，见 §9.3；余项见 §4.2）
3. **`dashscope` 会话合并 / 多智能体格式化缺失**（`ConversationMerger` 316 行 + `MultiAgentFormatter` 406 行）——anthropic/gemini 各有等价物，dashscope 无。（2026-10-29：已关闭，见 §9.4）
4. **`ollama` 会话合并 / 多智能体格式化缺失**（`ConversationMerger` 116 行 + `MultiAgentFormatter` **508 行**）。（2026-10-29：已关闭，见 §9.1）
5. **`anthropic` 内置 server tool 声明缺失**（`tool.AnthropicServerTool` 96 行）——解析/回显侧已实现，但**无法把内置 server tool 声明进请求**；gemini 侧已有 `GeminiServerTool` 可作模板。（2026-10-29：已关闭，见 §9.2）
6. **`responses` 多智能体分组 + server tool 声明缺失**（Java `ResponsesMultiAgentFormatter` 355 行 + `OpenAIServerTool` 125 行 + `ResponsesServerToolHelper` 544 行）；`to_request_tools` 仅构造 `Tool::Function`。（2026-10-29：多智能体分组已闭环（§9.6）；server tool 声明见 `openai_official::server_tool`（§8.2），`responses` 保持冻结的协议主干（§9.7））
7. **`responses::map_openai_error` 未走结构化错误归一**——与 `openai/mod.rs:338` 的 `ModelError::from_parts` + `ErrorKind::Timeout/Network` 实现**不一致**，导致 responses 路径的 429/5xx 无法进入 `retryable()` 判定与统一分类。（2026-10-22 已修，见 §7.1）

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
| 17 | §3.2 | dashscope 整体「**已完成**；含 `DashScopeEncryptionUtils` 对应物」 | 降级为「**部分完成**」——**无任何加密实现**（2026-10-29：加密已闭环，见 §9.3；余项见 §4.2） |
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
- dashscope 加密零实现、dashscope 与会话合并、ollama 多智能体、`AnthropicServerTool`、responses multi-agent 分组（§3 P1）。（2026-10-29：上列四项已闭环，见 §9.1–§9.4；responses 多智能体分组另见 §9.6；仍留 anthropic 并行 tool-use 拆分）

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
| `OpenAISdkClientFactory` | `openai_official::model`（`default_transport` + `build_http_request`） | **部分覆盖**：`maxRetries=0` 语义由 AgentScope 重试层承担；builder 级 additional headers 已注入；**ProxyConfig 已实现**（2026-10-28，见 8.4 / §9.5） |
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

### 8.4 未覆盖 / 已知差异（精确到方法；第 1–3 项已闭环，4–7 为对齐现状的设计差异，非缺口）

1. ~~**`OpenAISdkClientFactory.applyProxy`**：`ProxyConfig`（HTTP/SOCKS、认证、nonProxyHosts）未接；当前依赖 `ReqwestTransport` 的进程级默认。需在 provider/model 增加 proxy 配置并构造专用 `reqwest::Client`。~~ **已完成（2026-10-28）**：`openai_official::proxy`（`ProxyConfig` / `ProxyType` / `build_client`）+ `OpenAIResponsesChatModel::with_proxy_config` + provider `proxy` 选项；HTTP + basic auth、`nonProxyHosts` 按 base-url host 判断；**SOCKS 因 workspace `reqwest` 无 `socks` feature 显式报错**（不静默忽略）。
2. ~~**`OpenAIResponsesChatModel.isModuleRetryable` / `checkXShouldRetry`**：未读取 `x-should-retry` 响应头；~~ **已完成（2026-10-28）**：`common::transport::parse_should_retry` + `ModelError::retry_override`（`retryable()` 以头为准；`false` 时 `to_core()` 返回不可重试错误，避免 core 再重试 5xx）。`OpenAIIoException` / `OpenAIInvalidDataException` 的类型级区分仍退化为 transport `Error` + 消息启发式（未变）。
3. ~~**`ResponsesRequestMapper.buildReasoning` 的 `.validate()`**：`reasoning_effort` / `reasoning.summary|context|mode`、`service_tier`、`prompt_cache_options.mode|ttl` 直接透传字符串，未做 Java SDK 的枚举白名单校验。~~ **已完成（2026-10-28）**：`request.rs` 新增 `REASONING_*` / `SERVICE_TIERS` / `PROMPT_CACHE_*` 常量与 `validate_enum`，非法值返回带合法集合的可诊断错误；`prompt_cache_options` 仅接受 `mode`/`ttl`（其他 key fail-fast）。
4. **`OpenAIOfficialCredential.organization`**：字段已保留但不发送 `OpenAI-Organization` 头；Java `OpenAIResponsesChatModel.Builder` 同样未接线，故属对齐现状而非新增缺口。
5. **`ResponsesStreamingAssembler.handleOutputItemDone`**：与 Java 一致，只为 server-tool item 产出 block；普通 `function_call` 的最终参数不在 `output_item.done` 单独落块（由 arguments delta + 终态由上层消费方合并）。
6. **`ResponsesMultiAgentFormatter` 注入方式**：Rust 无 Java 的 `component(...)` 注入，改为 `GenerateOptions.additional_body_params["multiAgentFormatter"]`（`true` 或自定义 prompt 字符串）；`serverTools` 同理接受 JSON 数组。
7. **e2e**：未对真实 OpenAI 端点联调（**设计决定，见 §3.2「真实厂商端点联调」**）；全部为离线 wire-level 断言。

### 8.5 影响面

仅改动 `crates/agentscope-extensions-model/src/openai_official/**`、`src/lib.rs`（新增 `pub mod` / 导出）与本 tracker。未触碰 `openai` / `responses` 实现、其它 crate、根 `Cargo.toml`，未执行 git 写操作。

## 9. 2026-10-28 执行记录（剩余 P1 关闭）

> 基线：`cargo test -p agentscope-extensions-model` = **327 lib + 5 集成**。
> 本轮完工：**369 lib + 5 集成 = 374 passed / 0 failed / 0 ignored**（+42）；`cargo build -p agentscope-extensions-model` → Finished，本 crate **0 warning**。源文件 **56** 个 / **22111** 物理行。**（2026-10-28 追加：dashscope 加密真实现落地后 = 377 lib + 5 集成 = 382 passed，详见 §9.3 / §9.8）**
> 约束遵守：只改 `crates/agentscope-extensions-model/**` 与本 tracker；未动根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate；**未新增任何依赖**。

### 9.1 Ollama 会话合并 + 多智能体格式化（§3.2 两项 → 已完成）

- **Java 依据**：`OllamaConversationMerger.java:47`（`mergeToMessage`）、`OllamaMultiAgentFormatter.java:97`（`doFormat`）、`:167`（`isToolResultWithImages`）、`:181`（`createImagePromotionMessage`）、`:384`（`formatToolSeq`）。
- **Rust 落点**：`ollama/converter.rs` 新增 `merge_conversation` / `to_multi_agent_messages` / `to_multi_agent_messages_with`；`ollama/model.rs` 新增 `with_multi_agent_formatter` / `with_promote_tool_result_images`（`build_body` 分流）；`ollama/provider.rs` 消费 `multiAgentFormatter`（bool 或自定义 prompt）与 `promoteToolResultImages` 并从 options 剔除。
- **复用而非新造类型**：分组/标签/prompt 常量统一在新增的 `common/multi_agent.rs`（`group_messages` / `is_tool_related` / `agent_name` / `HISTORY_*` / `DEFAULT_CONVERSATION_HISTORY_PROMPT` / `take_multi_agent_prompt`），ollama / dashscope / responses 共用；core `formatter::base_formatter` 的 `extract_text_content` 等原样复用。
- **顺带修的真 bug**：`to_ollama_messages` 的 `<system-info>` 提升消息此前**没有附上 images**，图片从未到达模型；现按 Java `createImagePromotionMessage` 语义附上 base64（解析失败仅告警跳过）。
- **测试**：`ollama::converter::tests::{multi_agent_merges_agent_turns_and_keeps_tool_sequences, multi_agent_prompt_only_on_the_first_agent_group, multi_agent_single_plain_message_is_not_wrapped, multi_agent_merge_keeps_base64_images_and_flags_remote_ones, multi_agent_promotes_tool_result_images_into_a_user_message}`。
- **差异**：Java `formatToolSeq` 额外在 tool 正文追加 `image can be found at: <url>`；Rust 保留既有 banner 文案（语义等价、文案略异）。

### 9.2 anthropic 内置 server tool（§3.2 → 已完成）

- **Java 依据**：`tool/AnthropicServerTool.java:59`（`of()`：拒绝 null / `isTool()` / `validate()`）、`formatter/AnthropicToolsHelper.java:91`（`applyTools(..., serverTools, ...)`，server 先加）、`:147`（`validateToolNames` 全局唯一）。
- **Rust 落点**：新增 `anthropic/server_tool.rs`（`AnthropicServerTool::{new, web_search, web_fetch, code_execution, bash_code_execution, text_editor_code_execution, tool_search}` + `SERVER_TOOL_TYPE_PREFIXES`）；`dto.rs` 新增 `AnthropicToolEntry { Client, Server }`（untagged），`AnthropicRequest.tools` 改该类型；`converter.rs` 新增 `to_anthropic_tools_with_server` / `build_request_with_server_tools`（复用 `assemble_request`，无重复逻辑）；`model.rs` 新增 `with_server_tools` / `server_tools()`，`build_http_request` 改为 `Result`（重复工具名 fail-fast）。
- **测试**：`anthropic::server_tool::tests::{known_server_tools_validate_and_expose_their_name, custom_tools_and_malformed_definitions_are_rejected}`、`anthropic::converter::tests::{server_tools_are_declared_before_client_tools, duplicate_tool_names_across_client_and_server_tools_are_rejected, server_tools_alone_produce_a_tools_array}`、`anthropic::model::tests::{server_tools_are_merged_into_the_request_body, duplicate_server_and_client_tool_names_fail_fast}`。
- **差异**：Java 用 SDK 强类型 `ToolUnion.validate()`；Rust 无 SDK，按 `type` 前缀白名单 + `name` 非空校验，覆盖 Java 的 custom / 未知类型拒绝语义。

### 9.3 dashscope 请求加密（§2.2 / §3.2 → 已完成，含真实密码学实现）

- **Java 依据**：`DashScopeEncryptionUtils.java:45/48/51`（GCM tag 128 bit、IV 12 B、AES-256）、`:95`（`encryptWithAes`）、`:122`（`decryptWithAes`）、`:148`（`encryptAesKeyWithRsa`，RSA 包 base64(AES key)）；client 侧 `DashScopeHttpClient.java:672`（`encryptRequestBodyIfNeeded`：只加密 `input` 成员）、`:717`（`buildEncryptionHeader`）、`:594`（header `X-DashScope-EncryptionKey`）、`:741`（`decryptResponse`：`output` 为字符串时解密、失败保留原文）、`:521`（`fetchPublicKey`）。
- **算法参数（与 JCE 逐条对齐）**：AES-256 密钥 32 B（`KeyGenerator` 256 bit）；GCM nonce = 12 B IV 原样（无 GHASH 预处理），tag 128 bit 追加在密文尾部（`AES/GCM/NoPadding` ≡ `ciphertext || tag`）；明文按 UTF-8 编码；密文为标准 base64（带 padding）。RSA = `Cipher.getInstance("RSA")` 即 **PKCS#1 v1.5**；被加密的是 **base64 文本形式的 AES key**，外层再 base64 一次，即 `encrypt_key = base64(RSA_PKCS1v15(base64(raw_key)))`；公钥为 X.509/SPKI DER 的 base64。header `{"public_key_id","encrypt_key","iv"}`，`iv` 为 base64。
- **Rust 落点**：`dashscope/encryption.rs`（协议）+ `dashscope/crypto.rs`（原语）
  - 协议纯函数：`encrypt_request_body`（无 `input` → `Ok(None)`，同 Java）、`decrypt_response_body`（失败/非字符串保留原 body）、`encryption_header_value`、`EncryptionConfig::{new, from_public_key_response}`（错误 `code` / 缺失 `data` 均报错，消息对齐 Java）。
  - **默认 provider = `AesRsaCryptoProvider`**：`generate_aes_key` / `generate_iv` 走 `OsRng`；`aes_gcm_encrypt/decrypt` 走 `Aes256Gcm`（`Nonce` = IV，先校验长度再切片，避免 panic）；`rsa_encrypt` 走 `RsaPublicKey::from_public_key_der` + `Pkcs1v15Encrypt`。错误消息对齐 `EncryptionException`（"Failed to encrypt data with AES-GCM" 等）。
  - 注入缝保留：`DashScopeCryptoProvider` trait 不变；`UnavailableCryptoProvider` 仍可显式安装（**任一操作报错，绝不静默发明文**）。默认路径：`DashScopeChatModel::new` 即持有 `AesRsaCryptoProvider`，`with_encryption_config(config)` 显式启用加密；`with_encryption(config, provider)` 仍可替换实现。
  - 接线：`DashScopeChatModel::{with_encryption, with_encryption_config, is_encryption_enabled, build_http_request_with_context}`；`build_http_request` 返回 `Result`；流式/非流式响应在反序列化前解密。
- **金标来源（非自证）**：由 **Java JCE 生成**（`javax.crypto`，即 `DashScopeEncryptionUtils` 同款调用）：固定 AES key = ASCII `0123456789abcdef0123456789abcdef`、IV = ASCII `0123456789ab`、`input` = `{"messages":[{"role":"user","content":"hi"}]}` → `4ouKXzC5+XyQdgGH6H9eVS3qse0t6h8YQf3tNeub9sdeGgkN0MgHrQnMAjy2WdDnujA0FaOwQhAyITjGJA==`（61 B = 45 明文 + 16 tag）；另有一组含中文（你好，世界）的 UTF-8 变体。RSA 用一次性 2048-bit 测试密钥对（公钥 SPKI b64 + 私钥 PKCS#8 b64 存放在 `crypto::test_support`）；PKCS#1 v1.5 带随机填充 → 断言「私钥可解回 base64(AES key)」+ 256 B 块长，而非字节相等。
- **未接线**：`GET /api/v1/public-keys/latest` 拉取（`HttpTransport` 只有 POST 缝）→ 宿主注入 config，或自行 GET 后调 `EncryptionConfig::from_public_key_response`（记 §4.2）。（2026-10-29：已接线，见 §4.2 第 9 条；`HttpTransport::get_json` + `fetch_public_key()` 首次拉取并缓存）
- **测试（新增 8 个：crypto 6 + header 金标 1 + model 端到端 1）**：`dashscope::crypto::tests::{aes_gcm_matches_the_jce_gold_vector, aes_gcm_encodes_the_plaintext_as_utf8_like_java, aes_gcm_rejects_a_tampered_tag_and_wrong_sizes, rsa_wraps_the_base64_aes_key_with_pkcs1_v1_5, rsa_rejects_a_malformed_public_key, generated_keys_and_ivs_have_the_protocol_sizes}`；`dashscope::encryption::tests::header_gold_format_matches_the_dashscope_schema`（三字段 + 紧凑 JSON）与既有 6 个；`dashscope::model::tests::{default_crypto_provider_encrypts_the_wire_body_and_header（走真实 transport：body 为密文、header 三字段、私钥解回按键 → AES 解出 `messages[0].content == "hi"`）, encryption_without_a_provider_fails_instead_of_sending_plaintext（断言未发出任何请求）}`。
- **实测**：`cargo test -p agentscope-extensions-model` → **377 lib + 5 集成 = 382 passed / 0 failed / 0 ignored**（基线 369 + 5，新增 8）。反向验证见 §9.8。

### 9.4 dashscope 会话合并 / 多智能体格式化（§3.2 → 已完成）

- **Java 依据**：`DashScopeConversationMerger.java:75`（`mergeToMessage`，字符串 content 模式）、`:192`（`mergeToMultiModalMessage`）；`DashScopeMultiAgentFormatter.java:81/86`（`doFormat`）、`:203`（`formatMultiModal`）。
- **Rust 落点**：`dashscope/converter.rs` 新增 `merge_conversation` / `merge_multimodal_conversation` / `to_multi_agent_messages`（复用 `common::multi_agent::group_messages`）；`DashScopeChatModel::with_multi_agent_formatter`；provider 消费 `multiAgentFormatter` 并从 options 剔除。
- **细节对齐**：文本模式图片/Data 作为额外 part 且正文写 `[Image]`/`[Data]`，视频降级为 `[Video: url]` / `[Video - processing failed]`，空 tool result → `[Empty tool result]`，`ThinkingBlock` 跳过；多模态模式媒体各自成 part 并先 flush 文本，组尾补 `</history>`，空 content 补空 text；组级 cache 指令取最后一个显式标记，最后统一 `apply_automatic_cache_control`。
- **测试**：`dashscope::converter::tests::{multi_agent_text_mode_merges_groups_with_history_tags, multi_agent_text_mode_keeps_images_as_extra_parts, multi_agent_multimodal_mode_flushes_text_around_media, multi_agent_tool_group_keeps_assistant_tool_calls_and_results}`、`dashscope::model::tests::multi_agent_formatter_is_applied_to_the_request_body`。

### 9.5 openai-official Phase 2（§8.4 三项 → 已完成）

1. **`ProxyConfig`**：新增 `openai_official/proxy.rs`（`ProxyType` / `ProxyConfig{proxy_type,host,port,username,password,non_proxy_hosts}` / `should_bypass`（精确、`*.suffix`、`prefix.*`、通配）/ `from_json` / `validate` / `build_client`）+ `OpenAIResponsesChatModel::with_proxy_config`（重建 `ReqwestTransport::with_client`）+ provider 消费 `proxy` 选项。Java 依据：`OpenAISdkClientFactory.java:110`（`applyProxy`）、`:119`（认证 SOCKS 拒绝）。**支持**：HTTP 代理 + basic auth、`nonProxyHosts`（按 base URL host 判定，同 Java 语义）。**不支持但显式报错**：SOCKS4/SOCKS5（workspace `reqwest` 无 `socks` feature）、认证 SOCKS —— 绝不静默丢字段。
2. **`x-should-retry`**：`common::transport::parse_should_retry`（只认 true/false）+ `ModelError::retry_override`；`retryable()` 以头为准，`display_message()` 输出 `x-should-retry=`，`to_core()` 在显式 `false` 时返回不可重试错误（否则 core 会再次重试 5xx），状态码保留在消息里。Java 依据：`OpenAIResponsesChatModel.java:214/243`。
3. **枚举白名单 fail-fast**：`request.rs` 新增 `REASONING_EFFORTS`（none/minimal/low/medium/high/xhigh/max）、`REASONING_SUMMARIES`（auto/concise/detailed）、`REASONING_CONTEXTS`（auto/current_turn/all_turns）、`REASONING_MODES`（standard/pro）、`SERVICE_TIERS`（auto/default/flex/scale/priority/fast/ultrafast）、`PROMPT_CACHE_MODES`（implicit/explicit）、`PROMPT_CACHE_TTLS`（30m）——取值来自本地 `openai-java 4.88.0` jar 的 `*$Known` 常量（`javap` 字符串核对）；错误消息带字段名 + 合法集合。`prompt_cache_options` 仅接受 `mode`/`ttl`，其他 key fail-fast。
   - **测试**：`openai_official::proxy::tests::{bypass_patterns_match_java_semantics, http_proxy_builds_a_client_and_bypass_skips_the_proxy, unsupported_configurations_fail_loudly, parses_the_proxy_option_object, host_extraction_handles_paths_and_ports}`、`openai_official::request::tests::option_enums_are_validated_against_the_java_whitelists`、`common::error::tests::x_should_retry_override_wins_over_the_classification`、`common::transport::tests::should_retry_header_parsing`、`openai_official::model::tests::proxy_config_is_installed_on_the_model`、`openai_official::provider::tests::{proxy_option_is_consumed_and_not_rejected_by_the_whitelist, unsupported_proxy_fails_fast_at_creation}`。
   - **已知残留（不变）**：`OpenAIIoException` / `OpenAIInvalidDataException` 的类型级区分仍是 transport `Error` + 消息启发式；`x-should-retry` 的**真实 HTTP 响应头**端到端未离线测试，仅覆盖解析 + 覆盖语义。

### 9.6 `responses` 多智能体分组（§3.2 openai-official 子项 → 已对齐）

- **Java 依据**：`openaiofficial/ResponsesMultiAgentFormatter.java`（SYSTEM 独立 / TOOL_SEQUENCE 直通 / AGENT_CONVERSATION 合并 / BYPASS 独立）。`openai_official::formatter::ResponsesMultiAgentFormatter` 已完整实现（§8.2）。
- **Rust 落点**：`src/responses/mod.rs` 新增 `merge_agent_group` / `merge_multi_agent_history`，在 `build_request` 入口按 `additional_body_params["multiAgentFormatter"]`（`true` 或自定义 prompt）启用，selector 随即剥离（不进 body）。
- **与 `openai_official` 的有意差异**：`responses` 侧合并产物是 `Msg`（纯文本 `<history>`），图片/音视频降级为 `[Image]`/`[Audio]`/`[Video]` 文本引用；`openai_official` 侧保留真实 `input_image` part。原因是 `responses` 走 `async-openai` 的 `InputItem` 枚举，跨 SDK 反序列化 `Value` 级 formatter 风险高于收益；分组规则 / 首组 prompt / 工具序列直通**语义一致**。
- **测试**：`responses::tests::{multi_agent_merges_agent_turns_and_keeps_system_and_tool_sequences, multi_agent_is_off_by_default}`。

### 9.7 `responses` 与 `openai_official` 双实现评估（仅记录，未改代码）

| 维度 | `crate::responses` | `crate::openai_official` |
|---|---|---|
| provider id / 前缀 | `openai-responses` | `openai-official`（`openai-official:`） |
| SDK | `async-openai` 0.41（强类型 DTO） | 无 SDK，自建 wire DTO + `HttpTransport` seam |
| 覆盖面 | 协议主干（request/stream/parse）+ 本轮多智能体分组 | 主干 + server tools + `ResponsesServerToolHelper` + `additionalBodyParams` 白名单 + 408/409 重试 + proxy + 枚举校验 + 多模态 image parts |
| 测试 | 10 | 70 |
| 离线可测性 | 需构造 `async-openai` 类型 | 纯 `MemoryTransport` + `serde_json::Value` |

- **是否合并：本轮不建议。** `openai_official` 基本是 `responses` 的超集（除 SDK 抽象），但两者契约不同：`responses` 的 `Model` 实现依赖 `async_openai` 的 `CreateResponse` / `ResponseStreamEvent`，`openai_official` 直接发 `serde_json::Value`。合并等于二选一：(a) `responses` 改走 wire-level，等于重写，且 `async-openai` 依赖仍被 `openai` 模块使用，**不会释放依赖**；(b) `openai_official` 改由 `async-openai` 驱动，会丢掉 server tool 的 raw item 回放与 `additionalBodyParams` 直通（`async-openai` 未建模这些字段）。
- **合并成本估计**：约 1.5–2 人日（70+10 测试重新对齐 + `responses` 公开导出面调整）。**收益**：减少一套 DTO/解析器（约 1900 行）。**风险**：`default_responses_registry` 的调用方（含 TRAINING 等下游）行为漂移。
- **建议**：保留双实现，但**明确 deprecated 方向**——新能力只进 `openai_official`，`responses` 冻结为「协议主干 / 兼容入口」；待 `async-openai` 能表达 server tools 与任意 body params 时再评估合并。

### 9.8 反向验证（实现回退 → 测试 FAILED → 恢复）

| 回退点 | 回退内容 | 结果 |
|---|---|---|
| dashscope 加密 | `encrypt_request_body` 不写回加密后的 `input` | `encrypts_input_and_builds_the_header`、`encryption_encrypts_input_and_decrypts_output` **FAILED**（其余 7 passed） |
| ollama 多智能体 | 首组不再传 `history_prompt` | `multi_agent_prompt_only_on_the_first_agent_group`、`multi_agent_merges_agent_turns_and_keeps_tool_sequences` **FAILED**（16 passed） |
| anthropic server tool | 跳过客户端工具重名校验 | `duplicate_tool_names_across_client_and_server_tools_are_rejected` **FAILED** |
| dashscope 加密（真实现） | `aes_gcm_encrypt` 的 nonce 固定为 12 个 0（忽略请求 IV） | `aes_gcm_matches_the_jce_gold_vector`、`aes_gcm_encodes_the_plaintext_as_utf8_like_java`、`aes_gcm_rejects_a_tampered_tag_and_wrong_sizes` **FAILED**（其余 3 passed） |
| `x-should-retry` | `retryable()` 忽略 `retry_override` | `x_should_retry_override_wins_over_the_classification` **FAILED** |

恢复后重跑全量：**369 lib + 5 集成 = 374 passed / 0 failed**。

> **2026-10-28 追加（加密实装）**：把 `AesRsaCryptoProvider::aes_gcm_encrypt` 的 nonce 从请求 IV 换成固定 12 个 0 → `aes_gcm_matches_the_jce_gold_vector`、`aes_gcm_encodes_the_plaintext_as_utf8_like_java`、`aes_gcm_rejects_a_tampered_tag_and_wrong_sizes` **FAILED**（3 passed / 3 failed），恢复后重跑全量 **377 lib + 5 集成 = 382 passed / 0 failed**。

### 9.9 影响面

- 新增：`src/common/multi_agent.rs`、`src/dashscope/encryption.rs`、`src/dashscope/crypto.rs`（默认加密 provider，2026-10-28 追加）、`src/anthropic/server_tool.rs`、`src/openai_official/proxy.rs`。
- 修改：`src/lib.rs`（导出 `DashScopeCryptoProvider` / `EncryptionConfig` / `EncryptionContext` / `UnavailableCryptoProvider` / `AnthropicServerTool` / `ProxyConfig` / `ProxyType`）、`common/{mod,error,transport}.rs`、`ollama/{converter,model,provider,mod}.rs`、`dashscope/{converter,model,provider,mod}.rs`、`anthropic/{converter,dto,model,mod}.rs`、`responses/mod.rs`、`openai_official/{request,model,provider,mod}.rs`。
- 未动：根 `Cargo.toml`、`crates/agentscope-core/**`、`crates/agentscope-harness/**`、其它模块 crate。**`Cargo.toml` 新增依赖**（2026-10-28 加密实装，本 crate 自声明，未动根 `[workspace.dependencies]`）：`aes-gcm = "0.10"`、`rsa = "0.9"`、`rand = "0.8"`。

## 10. 2026-10-31 loop 收口记录（对齐 Java HEAD `1524b3191`）

### 10.1 六个上游提交的逐条处置
| Java 提交 | 内容 | Rust 落点 | 状态 |
|---|---|---|---|
| `34bd89818` | fix(ollama): preserve per-call execution options | `ollama/options.rs`：`OllamaOptions.execution_config` + `resolved_execution_config`（per-call > model default > `model_defaults()`）；`ollama/model.rs`：`explicit_timeout` → `HttpRequestSpec.timeout`（仅显式配置的才覆盖 transport 超时，避免把默认 300s 硬塞进每次调用）；`execution_config` **不写入 body**（对齐 Java `OllamaChatModelExecutionOptionsTest`） | **已完成** |
| `b604eca93` | fix(anthropic): report stop reason as finish reason | `anthropic/parser.rs`：非流式 `parse_message` 直传 `response.stop_reason`（`:160`）、流式 `AnthropicStreamIncrementer` 收 `message_delta.stop_reason`（`:250`/`:293`）。复核结论：**既有实现已覆盖**，本轮只补证与用例定位（`parses_text_and_tool_use_in_order`、`stream_collects_tool_use_argument_fragments`）——`length` / `max_tokens` 截断因此可被上层 `ReasoningContext` 识别 | **已完成（复核确认）** |
| `4c358772b` | fix(gemini): honor default tool choice | `gemini/converter.rs::build_request_with_server_tools`：`options.tool_choice.or(default_options.tool_choice)` 后再 `to_tool_choice`；新增用例 `gemini::converter::tests::build_request_honors_default_tool_choice`（同时验证 per-call 优先） | **已完成** |
| `7653eb7ec` | fix: preserve tool completion / hook feedback / error states | **model 层部分**：`anthropic` 的 tool_result 对 `ERROR`/`DENIED`/`INTERRUPTED` 置 `is_error: true`（`converter.rs::tool_result_message`），用例 `failed_tool_results_are_marked_is_error`。**非 model 层部分**（`void` 调用确认文案 `(completed with no output)`、`PostActingEvent.setToolResult` 回写）属 core `ToolMethodInvoker` / `ReActAgent`，**不在本 crate 范围**，已在回执中列为跨模块待办 | **已完成（model 层）** |
| `4b6c59af7` | feat(tool): on-demand tool search with native provider loading | 新增 `common/tool_search.rs`；`anthropic/{dto,converter,model}.rs`（`defer_loading`、`tool_reference`、cache marker 跳过 deferred）；`openai_official/{request,parser,model}.rs`（`tool_search` 声明 / `tool_search_call` 解码 / `tool_search_output` 回放 / `strictTools` 回填 / hosted-client 互斥） | **已完成** |
| `9e716cb27` | fix(model): evict ModelRegistry cache on registerFactory and share concurrent first creation | **跨模块（core）**：`ModelRegistry` 在 `crates/agentscope-core/src/model/provider.rs` 且**无解析缓存**（`register(&mut self)` 按 `provider_id` 替换、`resolve` 每次现建，全仓 `resolved_cache` 零命中）→ 两个缺陷结构性不存在，不移植补丁；本 crate 以 `tests/registry_semantics.rs`（4 用例）钉住等价不变量，裁定与证据见 §10.6 / §4.2-11 | **已完成（裁定不适用 + 守卫测试）** |

### 10.2 文件清单（本 loop）
**新增**：`src/openai/compat.rs`（vendor 请求体校正）、`src/common/tool_search.rs`（provider 无关 tool-search 编码层）、`tests/registry_semantics.rs`（registry 语义守卫，4 用例）；`tests/model_extensions_integration.rs` 未改（沿用 5 条）。
**修改**：`Cargo.toml`（删 `chrono`/`thiserror`）、`lib.rs`、`common/{mod,media,transport,diagnostics,multi_agent}.rs`（`multi_agent` 新增共享 `merge_agent_group` / `merge_multi_agent_history` / `MULTI_AGENT_FORMATTER_KEY`）、`ollama/{options,model}.rs`、`anthropic/{dto,converter,model}.rs`、`gemini/converter.rs`、`openai/mod.rs`、`responses/mod.rs`（合并逻辑下沉到 `common`）、`openai_official/{request,parser,model}.rs`、`dashscope/converter.rs`（仅测试）。

### 10.3 Java 测试类 → Rust 用例映射（本轮新增部分）
| Java 测试 | Rust 用例 |
|---|---|
| `OllamaChatModelExecutionOptionsTest` | `ollama::model::tests::{per_call_execution_timeout_reaches_the_request, model_default_execution_timeout_applies_when_the_call_omits_it, calls_without_an_execution_budget_keep_the_transport_timeout}`、`ollama::options::tests::{from_generate_options_preserves_execution_config, merge_prefers_per_call_execution_config}` |
| `OllamaEmbeddingTest`（长尾） | `ollama::model::tests::{embed_posts_to_embed_endpoint, embed_forwards_per_call_options_and_keep_alive}` |
| `OllamaOptionsTest`（llama.cpp 参数） | `ollama::options::tests::{extra_options_reach_the_wire_map_and_lose_to_typed_fields, merge_unions_extra_options_with_the_per_call_side_winning}` |
| `AnthropicResponseParserTest` | `anthropic::parser::tests::{parses_text_and_tool_use_in_order, stream_collects_tool_use_argument_fragments}` |
| `AnthropicMessageConverterTest` / `AnthropicChatFormatterTest` | `anthropic::converter::tests::{failed_tool_results_are_marked_is_error, parallel_tool_uses_split_into_alternating_pairs, parallel_tool_uses_stay_unsplit_when_a_result_is_missing, deferred_search_results_become_tool_references, cache_marker_skips_deferred_tool_definitions, prefetched_remote_image_is_inlined_instead_of_passed_through}` |
| `GeminiChatFormatterTest`（toolChoice） | `gemini::converter::tests::build_request_honors_default_tool_choice` |
| `ResponsesToolHelperTest` / `ResponsesRequestMapperTest`（tool search） | `common::tool_search::tests::*`（4 条）、`openai_official::{request,parser}::tests::*`（tool_search 声明/解码/回放） |
| `OpenAICompatFormatterTest`（deepseek/glm/kimi/minimax） | `openai::compat::tests::*`（7 条） |
| `ModelMapTest` | `openai::mod::tests::{provider_applies_the_model_map_before_creating_the_model, provider_reports_model_map_cycles_instead_of_looping, provider_strips_the_reasoning_suffix_and_seeds_the_thinking_knobs}` |
| `MediaUtilsTest`（远程下载） | `common::media::tests::{prefetch_downloads_through_the_transport_and_caches, prefetch_reports_transports_without_get_support, default_resolver_serves_prefetched_remote_media, default_resolver_rejects_remote_url}` |
| `DiagnosticsTest` | `common::diagnostics::tests::{installed_sink_receives_emitted_diagnostics, collector_records_without_emitting_twice, severity_serialises_lowercase}` |
| `DashScopeMultiAgentFormatterTest`（multimodal tool result） | `dashscope::converter::tests::multimodal_tool_result_keeps_its_media_parts` |
| `ModelRegistryTest`（`registerFactory_evictsCachedInstancesMatchingNewPattern` / `resolve_concurrentFirstResolution_createsSingleInstance` / `resolve_failedCreation_isNotCached` / `registerFactory_evictsEntryWhoseCreationIsStillInFlight`） | `tests/registry_semantics.rs::{re_registering_a_provider_wins_on_the_next_resolution, resolution_is_uncached_so_repeat_calls_build_independent_instances, a_failed_creation_is_not_retained_and_the_next_resolution_retries, concurrent_resolutions_are_shared_safe_and_never_reuse_one_instance}`（Java 的缓存语义在 Rust 不存在，改为钉等价不变量，见 §10.6） |
| `OpenAIMultiAgentFormatterTest` / `OpenAIConversationMergerTest` | `openai::mod::tests::multi_agent_formatter_merges_agent_turns_into_one_history_message`、`responses::tests::multi_agent_merges_agent_turns_and_keeps_system_and_tool_sequences` |

### 10.4 验证状态（受限执行体未跑 cargo）
- 本会话身份**禁止执行 cargo / rustc / clippy / fmt / git 写命令**，因此：**编译与测试运行由主智能体验证**。
- 静态计数：本 crate `#[test]`/`#[tokio::test]` 属性 **416 个（lib） + 9 个（集成，含新增 `tests/registry_semantics.rs` 4 个）= 425**（上一轮基线 377 + 5 = 382，本 loop **+39 内联 + 4 集成**）。
- 静态核查：新增/修改文件逐行自查（导入、字段、builder 方法、`ToolSchema`/`ToolResultBlock`/`ToolUseBlock` 字段名、enum variant 对齐）；无 `#[ignore]`、无删除既有断言、无新增第三方依赖。
- **编译风险点（优先看）**：`openai/mod.rs::normalize_context`（`ModelCreationContext` 字段可写性）、`ollama/options.rs::extra`（`PartialEq` + `BTreeMap`）、`common/diagnostics.rs::OnceLock<RwLock<..>>` 静态、`anthropic/converter.rs` 新测试对 `AnthropicToolResultContent::ToolReference` 的匹配。

### 10.5 已知限制（非缺陷）
1. `RemoteMediaCache` 是**进程级静态**：并发 prefetch 同一 URL 可能重复下载（幂等，仅多一次 GET）；无过期/容量上限，宿主长跑需自行 `insert` 刷新或重启进程。
2. Anthropic 未预取时的远程图仍**URL 透传**（Anthropic 原生支持 `type:url`，比强制下载更正确）。
3. 流式请求不重试（与 Java 一致），`TransportConfig.max_retries` 仅作用于非流式 `post_json`。
4. `openai/compat.rs` 的 MiniMax `reasoning_split` 只在 body 未显式给出时插入（`additional_body_params` 可覆盖），保持「extra params 最后生效」语义。

### 10.6 `9e716cb27` ModelRegistry 缓存语义裁定（跨模块：core）

- **Java 缺陷（两个）**：① `registerFactory` 从不清 `resolvedCache`——已解析过的 id 在 JVM 生命周期内永远拿旧 factory 的实例；② 并发首次解析是 `get → create → put`——N 个调用者各建一份，N-1 份被丢弃（未关闭）。
- **Rust 现状（证据）**：`ModelRegistry` 定义在 **core**（`crates/agentscope-core/src/model/provider.rs:155`，`impl` 在 `:159`），仅持 `Vec<Arc<dyn ModelProvider>>`；`register(&mut self)`（`:167`）按 `provider_id` 用 `retain` 替换旧项；`resolve(&self)` 每次调 `p.create(ctx)` 现建。全仓检索 `resolved_cache` / `resolve_cache` **零命中**，结构体无 `OnceCell`/`Mutex` 字段。
- **裁定**：本仓库**无需移植该补丁**——两个缺陷结构性不存在（无缓存可失效；无「首次创建」可共享）。该文件属 core，**本 crate 不改**（写入范围约束），仅在 §4.2-11 记跨模块需求。
- **可迁移的一半（已落地为守卫测试）**：Java 用例里可用的不变量转写为 `tests/registry_semantics.rs`：
  | Java 用例 | Rust 用例 | 说明 |
  |---|---|---|
  | `registerFactory_evictsCachedInstancesMatchingNewPattern` | `re_registering_a_provider_wins_on_the_next_resolution` | 同 id 重新注册后下一次解析必拿新 provider，且不波及其它 provider |
  | `resolve_failedCreation_isNotCached` | `a_failed_creation_is_not_retained_and_the_next_resolution_retries` | 失败不被保留，重试必再次触达 factory（计数 = 2） |
  | `resolve_concurrentFirstResolution_createsSingleInstance` | `concurrent_resolutions_are_shared_safe_and_never_reuse_one_instance` | 8 线程 + `Barrier` 并发解析全部成功；因无缓存，factory 计数 = 8（Java 为 1）——**这是当前契约，若 core 引入缓存需同步改写** |
  | `resolve_factoryResolvingItsOwnId_failsInsteadOfHanging` | 无对应用例 | Rust 无共享 future，自解析只会递归；不在本 crate 可测范围，记录为 core 决策项 |
  | （无） | `resolution_is_uncached_so_repeat_calls_build_independent_instances` | 钉住「无隐藏实例缓存」（`Arc::ptr_eq == false` 且 factory 计数 = 2） |
- **若 core 后续引入缓存必须同时满足**：注册时按 model id 驱逐 + 单飞首次创建（失败不缓存、失败者移除自己的条目），否则会重现 `#3788`；届时同步更新 `tests/registry_semantics.rs` 与本节。

### 10.7 补充交付：openai chat 多智能体格式化（消除 `OpenAIMultiAgentFormatter` / `OpenAIConversationMerger` 缺口）

- **缺口来源**：§2.3 formatter 行原记「`OpenAIConversationMerger` / `OpenAIMultiAgentFormatter` 无对应」；`openai/mod.rs` 全文无 `multi_agent` / `history` 命中（`rg` 复核）。同时该选择器若被宿主塞进 `additional_body_params`，会原样进 wire（OpenAI 不认该字段）。
- **改动**：`common::multi_agent` 新增 `merge_agent_group` / `merge_multi_agent_history`（从 `responses` 私有实现**下沉**，行为不变，含媒体降级 `[Image]`/`[Audio]`/`[Video]`/`[Data]`）与 `MULTI_AGENT_FORMATTER_KEY` 常量；`openai::build_request` 消费 `additional_body_params["multiAgentFormatter"]`（`true` = 默认 prompt / 字符串 = 自定义），`OpenAiChatModel::stream` 同时从 `extra_body` 删除该键，**保证不进 wire**；`responses` 改调共享实现（去重 ~75 行）。
- **语义**：system 消息保持独立；tool 序列（tool-use / tool-result / tool 角色）原样直通；每段连续 agent 消息合并为一条 `<history>` user 消息（`"<name>: <text>"`），**仅首组**带 `DEFAULT_CONVERSATION_HISTORY_PROMPT`。
- **接线口径**：仅 **per-call** 选项（`GenerateOptions.additional_body_params`），与 `responses` 一致；`OpenAiChatModel` 无 model-default options 字段，provider 选项不参与（不引入静默行为）。
- **测试**：`openai::mod::tests::multi_agent_formatter_merges_agent_turns_into_one_history_message`（断言 system 独立、首组带 prompt、tool 序列后仍合并、body 内无 `multiAgentFormatter`）；`responses::tests::{multi_agent_merges_agent_turns_and_keeps_system_and_tool_sequences, multi_agent_is_off_by_default}` 作为共享实现回归。
- **编译风险点（主智能体优先看）**：`openai::build_request` 新增 `let mut options = options;` 遮蔽（后续字段读取不变）、`stream` 的 `extra_body` 改 `mut`、`common/multi_agent.rs` 中 `ContentBlock` 穷尽匹配与原 `responses` 实现逐字一致。
