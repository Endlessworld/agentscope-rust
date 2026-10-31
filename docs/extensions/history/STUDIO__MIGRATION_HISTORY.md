# STUDIO 迁移执行历史（归档）

> 本文件是 [`../STUDIO__GAP_TRACKER.md`](../STUDIO__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. §4.3 P0 修复记录（2026-10-22，汇总方） · 8. 2026-10-28 执行记录（子智能体：关闭 P1×2） · 9. 2026-10-30 执行记录 — StudioClient 传输注入 / UserAgent 集成评估 · 10. 2026-10-31 执行记录 — P2 收口 + Java 增量提交核对）。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **方式**：纯静态核对（逐文件读源码 + `rg` 交叉检索 + PowerShell 行数/注解计数）。**未执行** `cargo build` / `cargo test`，**未修改任何 Rust 代码**；本节及上文所有"已更正"均为本 tracker 文档内的定点修订。
- **核对范围**：Java `agentscope-extensions/agentscope-extensions-studio`（主/测全部 `.java`）↔ Rust `crates/agentscope-extensions-studio`（全部 18 个 `.rs`）+ 跨仓检索 `agentscope-core` / `agentscope-extensions-protocol` / `agentscope-extensions-controlplane` / `agentscope-harness`（用于确认事件字段契约与依赖）。
- **计数口径**：行数 = `[System.IO.File]::ReadAllLines().Length`（物理行，含空行与注释）；**非空行**另计（剔除空字符串行）；测试数 = 源码中 `#[test]` 与 `#[tokio::test]` 注解逐条计数（非 `cargo test` 实际执行数）。

### 6.2 实测统计
| 项 | Java | Rust |
|---|---|---|
| 主代码文件 | 28 | 18（`src/` 全量 `.rs`）|
| 主代码行（物理）| **4305** | **5048** |
| 主代码行（非空）| **3738** | **4578** |
| 测试文件 | 10 | 8（含测试的文件；`error.rs`/`lib.rs`/`mod.rs`/`user_agent.rs`/`message_hook.rs`/`model.rs` 等无内联测试）|
| 测试行（物理）| **1837** | — |
| 测试行（非空）| **1554** | — |
| 测试用例数 | **91** 个 `@Test` 方法 | **92**（62 `#[test]` + 30 `#[tokio::test]`；2026-10-31 静态计数，待主智能体 `cargo test` 验证。历史：2026-10-28 实测 70 passed）|
| 顶层类/类型 | 28 文件 / **44 个类型声明**（含嵌套类与接口）| 18 文件 / 与 Java 一一映射（见 §6.3）|
| 属性键常量 | `GenAiIncubatingAttributes` 38 个 `AttributeKey` + `AgentScopeIncubatingAttributes` 6 个 | **38 + 6**（2026-10-31 已补齐，§10）|
| OTLP 部件模型 | `telemetry/model` **13** 个类 | `telemetry::model` **13** 个 `pub` 类型（一一对应）|

Rust 逐文件测试分布（`#[test]` / `#[tokio::test]` / 合计；**2026-10-28 实测**）：
`config.rs` 3/0/3、`message_hook.rs` 3/0/3、`pojo.rs` 5/0/5、`socket_io.rs` 14/0/14、`websocket.rs` 6/5/11、`aggregator.rs` 3/0/3、`exporter.rs` 8/2/10、`extractors.rs` 8/0/8、`tracing.rs` 2/11/13 → **合计 54/16/70**（原记 `socket_io` 12、`tracing` 7 且漏 `message_hook` 3，合计 59 为 2026-10-22 静态计数）。
> **2026-10-31 静态重算（§10，待主智能体验证）**：`exporter.rs` 11/2/13、`extractors.rs` 9/0/9、`gen_ai_attributes.rs` 2/0/2、`agent_scope_attributes.rs` 2/0/2、`user_agent.rs` 1/3/4（其余文件不变）→ **合计 62/30/92**。

### 6.3 对齐结论表
| Java 类 / 包 | Rust 对应 | 行为覆盖 | 结论 |
|---|---|---|---|
| `studio.StudioConfig` | `studio::StudioConfig` + `StudioConfigBuilder` | 默认值/尾部 `/` trim/`tracingUrl` 推导全对齐；Java 无 trim，Rust 多做一次 trim（无害）| ✅ 对齐 |
| `studio.StudioClient` | `studio::StudioClient` | 三方法 + 重试 + shutdown 均在；**重试谓词不同**（Java 含非 2xx，Rust 不含）| ⚠️ 部分（§6.4 P2-1）|
| `studio.StudioWebSocketClient` | `studio::StudioWebSocketClient` + `studio::socket_io` | 握手/入房/ping-pong/重连/`forwardUserInput` 解码全在；鉴权键已对齐 Java `run_id`（旧 `runId` 仅 opt-in，§4.1.0v）| ✅ 对齐 |
| `studio.StudioWebSocketClient.UserInputData` | `studio::UserInputData` | blocksInput + structuredInput 结构一致 | ✅ 对齐 |
| `studio.StudioManager` | `studio::StudioManager` | 全局单例/init builder/getters/shutdown 在；**Java 的 `TracerRegistry.register` 自动装配不迁移**（core 无 `TracerRegistry`，§4.2 第 4 条）| ✅ 设计决定（不迁移，2026-10-31）|
| `studio.StudioMessageHook` | `studio::StudioMessageHook` | Middleware 形态在，**终态消息捕获键已修正**（`reply` 优先，`message` 回退，§7.1）| ✅ 对齐 |
| `studio.StudioUserAgent` | `studio::StudioUserAgent` | 双模式 + 失败回退 + 30min 超时在；非 `AgentBase` 子类（core 无此类，§3.2 已登记）| ✅ 设计等价 |
| `studio.pojo.*`（4 个）| `studio::pojo.*`（4 个）| 字段名/`run_dir`/`camelCase`/`NON_NULL` 跳过逐条对齐 | ✅ 对齐 |
| `telemetry.GenAiIncubatingAttributes` | `telemetry::gen_ai_attributes` | 38/38（含 `gen_ai.tool.type` + 3 个 usage 键）| ✅ 对齐（2026-10-31）|
| `telemetry.AgentScopeIncubatingAttributes` | `telemetry::agent_scope_attributes` | 6/6（含 `agentscope.usage.tool_use_prompt_tokens`）| ✅ 对齐（2026-10-31）|
| `telemetry.AttributesExtractors` | `telemetry::extractors`（模块，非同名类型）| 10 个 `get_*` 函数一一对应；`GenerateOptions` 用本地投影绕过 core `Serialize` 缺口 | ✅ 对齐 |
| `…FormatterConverter` | `telemetry::FormatterConverter` | 映射表一致 | ✅ 对齐 |
| `…ProviderNameConverter` | `telemetry::ProviderNameConverter` | 函数在，但 `TelemetryTracing` 未调用（provider 恒为 `unknown`）| ⚠️ 部分（§6.4 P1-1）|
| `telemetry.StreamChatResponseAggregator` | `telemetry::StreamChatResponseAggregator` | 聚合逻辑在；**已接入 `on_model_call`**（`AggregatingModel` tee 模型流）| ✅ 对齐 |
| `telemetry.TelemetryTracer` | **`telemetry::TelemetryTracing`** | span 生命周期/父子/RAII 达标；`agentscope.function.name` 四类 span、三个响应侧抽取器、`call_format` format span 均已落地（2026-10-28）；provider 名仍取配置（Rust 无反射）| ✅ 对齐（§4.1.0v）|
| `telemetry.model.*`（13 类）| `telemetry::model`（13 类型）| 一一对应，`type` 判别 + `mime_type`/`file_id`/`finish_reason` 字段名对齐 | ✅ 对齐 |
| OTLP 导出（`exporter-otlp-http`）| `telemetry::exporter`（`OtlpEncoder` + `OtlpHttpExporter`）| OTLP/HTTP JSON 编码 + POST 实现；**未引入 `opentelemetry`（已核实）**；自定义 header 已支持（`with_header`/`with_headers`）| ✅ 设计等价（2026-10-31）|

**核实结论（逐条 grep）**：
- 「不引入 `opentelemetry`」**属实** —— `crates/agentscope-extensions-studio/Cargo.toml` 无 `opentelemetry*` 依赖；根 `Cargo.toml` 的 `[workspace.dependencies]` 亦无；crate 内 `opentelemetry` 仅出现在 6 处**文档注释**中。
- 「Socket.IO 真实握手/鉴权/重连」**属实但有一处键名分歧** —— `studio/socket_io.rs` 确有 `SocketIoTransport`（tokio-tungstenite 0.29 真连）、`SocketIoProtocol` 状态机、`parse_engine_io_open`/`namespace_connect_frame`/`is_ping`/`reconnect_delay`；`Cargo.toml` 的 `tokio-tungstenite = 0.29`（default-features=false + connect/handshake/rustls-tls-webpki-roots）与 CHANNEL crate 同版本同 features。
- 「§3.3 / §4.1 的 `TelemetryTracer` 接线缺口」**仍存在** —— `TelemetryTracing` 全仓引用仅限 `telemetry/tracing.rs`、`telemetry/mod.rs` 与其单测；`StudioManager::initialize*` 未装配。

### 6.4 新发现缺口
| 编号 | 优先级 | 缺口 | 证据 | 影响 |
|---|---|---|---|---|
| P0-1 | **P0** | `StudioMessageHook` 捕获终态消息的键名错误，转发**恒不触发** | 已于 2026-10-22 修复（§7.1）：优先读 `fields["reply"]`，`fields["message"]` 仅作第三方回退 | — |
| P1-1 | ~~P1~~ **已关闭（2026-10-28）** | `TelemetryTracer` 三处 Java 语义未覆盖：①span 缺 `agentscope.function.name`；②响应侧属性 + `StreamChatResponseAggregator` 未接入；③缺 `callFormat` 的 `format {target}` span | 原证据：`tracing.rs:32-36` import 只含 3 个 request 侧抽取器；`AGENTSCOPE_FUNCTION_NAME` 仅定义处出现；`get_*_response_attributes` 只被 `mod.rs` re-export | 已修复，依据与用例见 §4.1.0v；残留设计取舍：`gen_ai.provider.name` 仍取配置（Rust 无反射）、`format` span 需宿主调用 `call_format` |
| P1-2 | ~~P1~~ **已关闭（2026-10-28）** | Socket.IO 鉴权载荷键名分歧 | 原证据：Java `StudioWebSocketClient.java:98` `auth.put("run_id", ...)`；Rust 曾发 `json!({ "runId": ... })` | 已修复：`auth_payload()` 默认 `run_id`，旧 `runId` 经 `with_legacy_auth_key()` 可选叠加；用例 `handshake_auth_uses_the_java_snake_case_run_id_key` / `the_legacy_camel_case_auth_key_is_opt_in`（§4.1.0v）|
| P2-1 | **P2** | HTTP 错误重试语义分歧 | Java `StudioClient.java:116/161/209` 把非 2xx 包成 `IOException` → 被 `Retry.filter(t -> t instanceof IOException)` 命中**会重试**；Rust `client.rs` 仅 `StudioError::Transport` 可重试，`StudioError::Http` 不重试 | **裁定：已完成（设计决定，不迁移），2026-10-31（§10）**。Rust 保留「传输失败 / HTTP 状态」两类可区分错误；盲重试 4xx 会重复 `pushMessage`/`requestUserInput`。已有断言 `http_status_errors_are_not_retried` 固化该边界；`error.rs::is_retryable` 文档已改正（原文误称「镜像 Java」）|
| P2-2 | **P2** | OTLP exporter 无自定义 header | Java `TelemetryTracer.Builder.addHeader/headers`（注释明写 "for authentication (e.g., Langfuse)"）| **已关闭（2026-10-31）**：`OtlpHttpExporter::with_header` / `with_headers` / `headers()`（同名覆盖 = Java `HashMap.put`），export 时逐个下发；用例 `otlp_headers_replace_same_named_entries_like_java_add_header` / `with_headers_replaces_the_whole_header_set_like_java_headers` / `an_exporter_from_config_sends_no_extra_headers` |
| P2-3 | **P2** | 属性键常量缺 5 个（Java 44 → Rust 39）| 缺 `gen_ai.tool.type`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.usage.cache_read.input_tokens`、`gen_ai.usage.reasoning.output_tokens`、`agentscope.usage.tool_use_prompt_tokens` | **已关闭（2026-10-31）**：5 个常量已补（38 + 6 = Java 44）；`get_llm_response_attributes` 同时接入 4 个 usage 属性（Java `AttributesExtractors.java:229-243` 无零值过滤）。用例 `the_key_set_matches_the_java_class_exactly`（gen_ai/agent_scope 各一）、`the_previously_missing_keys_match_the_java_literals`、`llm_response_attributes_record_every_usage_metric` |
| P2-4 | **P2** | `StudioUserAgent` 缺 Java 的 name 非空校验 | Java `StudioUserAgent.Builder.build()`（`StudioUserAgent.java:327-331`）对空名抛 `IllegalArgumentException("Agent name cannot be null or empty")` | **已关闭（2026-10-31）**：`build()` 改返回 `StudioResult<StudioUserAgent>`，blank name 返 `StudioError::InvalidArgument`（新增变体，带同一文案）。用例 `blank_names_are_rejected_like_java`（空/空白/制表符三种）|

### 6.5 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | Java 规模「主代码 28 文件 / 4305 行；测试 10 文件 / 1837 行」 | 数值**正确**（物理行口径）；补注非空行 3738/1554 与 `@Test` 91 个，并说明与汇总 tracker 3700/1452 的口径差异 |
| 2 | §1 模块概览 | Rust 规模「18 个 `.rs` / 5048 行 / 59 个内联测试（46+13）」 | **正确**；补注非空行 4578 |
| 3 | §1 当前状态 | 「见 §4.2.4」 | 交叉引用错误，实际为 **§4.2 第 4 条** |
| 4 | §2.0 `StudioWebSocketClient` 行 | 未提鉴权键 | 补注 Rust=`runId` vs Java=`run_id`（P1-2）|
| 5 | §2.0 `StudioClient` 行 | 「仅 IO/网络错误重试（**对齐 Java 仅重试 IOException**）」| 断言**不成立**：Java 的非 2xx 也是 IOException → 也会重试。改为标注语义分歧（P2-1）|
| 6 | §2.0 `StudioMessageHook` 行 | 「在事件流上捕获终态消息并推送」| 补注捕获键不符导致恒不触发（P0-1）|
| 7 | §2.0 `AttributesExtractors` 行 | Rust 对应「`telemetry::AttributesExtractors`」| Rust **无该类型**，实际是模块 `telemetry::extractors`（一组 `get_*` 自由函数）|
| 8 | §2.0 `ProviderNameConverter` 行 | 「改为按模型名/`Model` 显式 provider 探测（见 §4.1）」| 实际按**模型类型名后缀**探测；且 `TelemetryTracing` 未调用它，provider 恒为 `unknown`（P1-1）|
| 9 | §2.0 `TelemetryTracer` 行 | Rust 对应「`telemetry::TelemetryTracer`」+「on_agent/on_reasoning/on_acting/on_model_call」| Rust **无 `TelemetryTracer` 类型**（实际 `telemetry::TelemetryTracing`）；拦截点**无 `on_reasoning`**，只有 on_agent/on_model_call/on_acting；缺 Java 的 format span |
| 10 | §2.0 `telemetry.model.*` 行 | 「（15 个类）」| Java `telemetry/model` 实为 **13 个类**（Rust 亦 13 个类型，一一对应）|
| 11 | §2.1 依赖映射 | 「`reqwest`（workspace **0.12**, json/rustls）」| 根 `[workspace.dependencies]` 为 **0.13** |
| 12 | §3.1 crate 骨架行 | 「共 15 个文件」| 现为 **18 个文件**（新增 socket_io.rs / exporter.rs / tracing.rs）|
| 13 | §3.1 `StudioClient` 行 | 「（对齐 Java 仅重试 IOException）」| 同 #5，标注分歧（P2-1）|
| 14 | §3.1 `StudioWebSocketClient` 行 | 「单测 `studio::websocket::tests`（**9**）」| 实际 **11**（6 `#[test]` + 5 `#[tokio::test]`）|
| 15 | §3.1 `StudioMessageHook` 行 | 「捕获终态消息 fire-and-forget 推送」| 补注捕获键不符 → 恒不触发（P0-1）；另 `tokio::spawn` 需 runtime |
| 16 | §3.1 telemetry model 行 | 「telemetry model 部件 ×**15**」| 实际 **13** |
| 17 | §3.1 `AttributesExtractors` 行 | 「单测 `telemetry::extractors::tests`（**7**）」| 实际 **8** |
| 18 | §3.2 Socket.IO 行 | 状态「**已完成**（2026-10-08）」| 实现完整，但存在鉴权键名分歧 → 降级为「**进行中**」，并新增待办行（§3.2）|
| 19 | §3.3 `TelemetryTracer` 行 | 状态「**已完成**（2026-10-21，`TelemetryTracing`）」| span 生命周期达标，但语义覆盖缺 3 项 → 降级为「**进行中**」，并列出待补 ①②③（P1-1）|
| 20 | §3.3 OTLP 导出行 | 「已完成（降级 exporter）」| 编码与「不引入 opentelemetry」**属实**；补记单测 **10** 个 + 缺自定义 header（P2-2）|
| 21 | §3.2 | 缺 `StudioMessageHook` 捕获键条目 | **新增**「`StudioMessageHook` 终态消息捕获键修正」行（待办）；2026-10-29 勘误：已修（`reply` 优先 + `message` 回退，3 用例） |
| 22 | §4.1 当前状态 | 「交付：`src/` **15 个文件**已接线并编译」| **18 个文件** |
| 23 | §4.1 当前状态 | 「状态：已完成（cargo build / cargo test 全绿）」| 补注：本轮**静态复核未编译**，build/test 结论沿用历史记录未经复验 |
| 24 | §4.1.0s | 「Socket.IO 帧层 11 个 …，OTLP 导出 **11** 个」| 导出实为 **10** 个（socket_io 12 + exporter 10 = 22，合计正确）|
| 25 | §4.1.0u 设计要点 | 「首版不含响应侧属性（`gen_ai.usage.*`）」| 表述偏窄：实际是**三个响应侧抽取器整体未接入** + 聚合器未接线 + 缺 `agentscope.function.name` + 缺 format span（P1-1）|
| 26 | §4.2 第 4 条 | 「【接线待办】」| 复核确认**仍存在**，补记证据（`TelemetryTracing` 引用范围）|
| 27 | §2.0 `GenAiIncubatingAttributes` 行 | 「OTel GenAI 语义约定 **1.37.0** 属性键」| Java 类注释明写「semantic conventions **1.41.0**」；Rust `gen_ai_attributes.rs` 模块文档也写 1.37.0 → 以 Java 为准应为 1.41.0（文档口径已更正，Rust 侧源码 2026-10-31 已改为 1.41.0，§10）|
| 28 | §2.1 依赖映射 | 「手写 `retry_with_backoff` 辅助」| Rust **无该函数**，退避循环内联在 `StudioClient::post_json` 中 |
| 29 | §1 当前状态 | 「见 §4.2.4」| 交叉引用错误，实际为 **§4.2 第 4 条** |
| 30 | §6.2 实测统计 | 「顶层类/类型 39 个声明」| 实为 **44** 个类型声明（含嵌套类/接口），已更正 |

> **未发现问题的声明（逐条核实为真）**：①「不引入 `opentelemetry`」；②「Socket.IO 真实握手/重连/ping-pong 已落地」；③「OTLP 降级导出为真实 reqwest POST」；④「59 个离线测试」；⑤「`GEN_AI_TOOL_TYPE` + 3 个 usage 键缺失」与「`AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS` 缺失」的计数；⑥「core 无 `Hook`/`TracerRegistry`/`AgentBase`」；⑦「`GenerateOptions` 未实现 `Serialize`」。


## 7. §4.3 P0 修复记录（2026-10-22，汇总方）

### 7.1 已修：Studio 消息转发恒不触发（§4.3 #5）

- **缺陷**：`StudioMessageHook::final_message` 读 `fields["message"]`，而 core 在 `react_agent.rs` 写的是 `fields["reply"]` → 提取恒为 `None`，转发逻辑从未执行。
- **修复**：优先读 `reply`，保留 `message` 作为第三方生产者的回退键。
- **用例**（3 条）：`reads_reply_key_written_by_core` / `falls_back_to_message_key` / `non_result_events_yield_none`。
- **验证**：`cargo test -p agentscope-extensions-studio` → **62 passed / 0 failed**。

### 7.2 仍待办

- ~~`TelemetryTracer` 缺 `agentscope.function.name` span、三个响应侧抽取器 + `StreamChatResponseAggregator` 未接入~~ → **2026-10-28 已修，见 §4.1.0v**。
- ~~Socket.IO 鉴权键 Rust `runId` vs Java `run_id`~~ → **2026-10-28 已修，见 §4.1.0v**。
- **2026-10-31 已了结**：P2-2/P2-3/P2-4 已修复（§10）；P2-1 裁定为设计决定（不迁移）。
- **仍开放（非本 crate 可解决）**：`format` span 的宿主调用点（§4.2 第 6 条，core 无 formatter 钩子 → 跨模块需求）；多模态抽取跟随 Java TODO（§3.3）。

## 8. 2026-10-28 执行记录（子智能体：关闭 P1×2）

**改动文件**（仅本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `crates/agentscope-extensions-studio/src/studio/socket_io.rs` | `AUTH_RUN_ID_KEY`/`LEGACY_AUTH_RUN_ID_KEY`/`auth_payload()`；`SocketIoTransport::new` 默认发 `run_id`；`with_legacy_auth_key()` 兼容回退；+2 测试 |
| `crates/agentscope-extensions-studio/src/telemetry/tracing.rs` | `agentscope.function.name`（agent/model/tool/format）；`SpanGuard::finalizer` + 三个响应侧抽取器接入；`AggregatingModel`（tee 模型流）+ `StreamChatResponseAggregator`；`call_format()` format span；agent span 名 `invoke_agent <name>`；+6 测试，1 个端到端用例加强 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§2.0/§3.2/§3.3/§4.1/§4.2/§6.2/§6.3/§6.4/§7.2 状态与计数；新增 §4.1.0v 与本节 |

**Java 依据**：`StudioWebSocketClient.java:96-99`（auth `run_id`）；`TelemetryTracer.java:96/104/110`（agent span 名、function.name、响应侧）、`:123-148`（model function.name + aggregator + doFinally）、`:169/190`（tool）、`:203-232`（callFormat）；`AttributesExtractors.java:350`（`getFunctionName`）；`StudioWebSocketClient.java` 全模块 grep 确认握手鉴权只有 `run_id` 一个键。

**命令与结果（实测）**
- `cargo build -p agentscope-extensions-studio` → EXIT=0，本 crate 0 warning。
- `cargo test -p agentscope-extensions-studio` → **70 passed / 0 failed**（基线 62，+8）。
- `cargo fmt -p agentscope-extensions-studio -- --check` → 干净。

**反向验证**：5 项回退逐一确认对应用例 FAILED 后恢复（明细见 §4.1.0v 表）。

**未完成**：`StudioManager` 自动装配（§4.2 #4）；`format` span 需宿主调用（§4.2 #6）；P2-1/P2-2/P2-3/P2-4。

## 9. 2026-10-30 执行记录 — StudioClient 传输注入 / UserAgent 集成评估

**任务与 Java 依据**
- 任务 1（P1）：`StudioClient` 加 HTTP 传输注入点，使网络路径可离线测。Java 依据 `StudioClient.java`：`registerRun` → `POST {base}/trpc/registerRun`、`pushMessage` → `POST {base}/trpc/pushMessage`、`requestUserInput` → `POST {base}/trpc/requestUserInput`；三者均 `RequestBody.create(json, MediaType.get("application/json; charset=utf-8"))`，**无鉴权头**；`executeWithRetry` = `Retry.backoff(config.getMaxRetries(), 100ms).maxBackoff(5s).filter(IOException)`。
- 任务 2（P1）：`StudioUserAgent` 与 core agent 循环集成。Java `StudioUserAgent extends AgentBase`（`getInputFromStudio` = `requestUserInput` + `waitForInput(timeout)`，`onErrorResume` 回退终端）。

**改动文件**（仅本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `src/studio/http.rs`（**新增**） | `StudioHttpRequest`/`StudioHttpResponse`/`trait StudioHttpTransport`（`async fn execute(&self, StudioHttpRequest) -> StudioResult<StudioHttpResponse>`）+ `ReqwestStudioTransport`（生产实现，10s connect / 30s request，行为与旧内联实现一致）+ `JSON_CONTENT_TYPE`/`HTTP_POST` + `#[cfg(test)] testing::{ScriptedTransport, Outcome}` |
| `src/studio/client.rs` | `http: reqwest::Client` → `transport: Arc<dyn StudioHttpTransport>`；新增 `with_transport()`；`post_json` 改为「序列化 → 构建 `StudioHttpRequest` → 重试循环（`send_once`）」；`send_once` 负责非 2xx → `StudioError::Http`；+10 测试 |
| `src/studio/mod.rs` | `pub mod http` + re-export；新增 `StudioManager::initialize_with_transports(config, http, ws)`（`initialize_with_transport` 改为委托，语义不变） |
| `src/studio/user_agent.rs` | 新增 `drive_round<A: CallableAgent>` / `drive_round_with<A: CallableAgent>(agent, Msg)`；+3 测试 |
| `src/error.rs` | 新增 `StudioError::Agent(String)`（不可重试，`is_retryable` 仍仅 `Transport`） |
| `src/lib.rs` | re-export `StudioHttpRequest/Response/Transport/ReqwestStudioTransport`；模块文档补一条 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§3.2/§4.2 #7 + 本节 |

**新增测试（+13，均离线；transport 为脚本化）**
- `studio::client::tests`（10，全部通过 `StudioClient` 公开方法）：`register_run_posts_java_shaped_request`（方法/路径/`Content-Type`/无鉴权头/body 六字段/`yyyy-MM-dd HH:mm:ss.SSS` 时间戳）、`push_message_posts_java_shaped_request`、`push_message_falls_back_to_reply_id_for_unnamed_messages`、`request_user_input_posts_java_shaped_request`（返回值 == body `requestId`、无 `structuredInput`）、`request_user_input_forwards_structured_schema`、`success_statuses_are_decoded_as_ok`（200/204）、`http_status_errors_are_not_retried`（§4.2 P2-1 现状锁定：500 仅 1 次尝试）、`transport_errors_are_retried_then_succeed`（重试请求体与 URL 不变）、`transport_errors_stop_after_max_retries`（1 + max_retries 次）、`role_labels_match_java`
- `studio::user_agent::tests`（3）：`drive_round_with_feeds_a_core_agent_loop`（真实 `ReActAgent` + StubModel，断言回复与 `/trpc/pushMessage` 的用户消息）、`drive_round_with_tolerates_a_failed_push`（500 不中断本轮）、`drive_round_with_surfaces_agent_errors`（→ `StudioError::Agent`）

**命令与结果（实测）**
- `cargo test -p agentscope-extensions-studio -j 2` → **EXIT=0，83 passed / 0 failed / 0 ignored**（doc-tests 0 passed；基线 70）
- `cargo clippy -p agentscope-extensions-studio --all-targets` → EXIT=0；本 crate 仅 2 条**既有** `clippy::type_complexity`（`telemetry/tracing.rs:510/520`），新代码 0 告警
- `cargo fmt -p agentscope-extensions-studio -- --check` → 干净

**反向验证（2 组，均回退 → FAILED → 恢复 → 全绿）**
| # | 回退内容 | 结果 |
|---|---|---|
| 1 | `client.rs::send_once` 的 `if !response.is_success()` → `if false && !response.is_success()`（非 2xx 不再报错） | `http_status_errors_are_not_retried` FAILED（`test result: FAILED. 0 passed; 1 failed`） |
| 2 | `user_agent.rs::drive_round_with` 推送失败改为 `client.push_message(...).await?`（不再容忍） | `drive_round_with_tolerates_a_failed_push` FAILED |

**设计要点与诚实边界**
- 传输契约：非 2xx 以 `Ok(StudioHttpResponse)` 返回、由 `StudioClient` 映射为 `StudioError::Http`；网络失败为 `StudioError::Transport`（唯一可重试变体）；传输层自身不重试（与 `judge` crate 的 `JudgeHttpTransport` 同构）。
- **只锁定不改语义**：Java 把非 2xx 包成 `IOException` 会重试，Rust 不重试 —— 该分歧仍归 §4.2 P2-1，本轮用测试把现状固化（`http_status_errors_are_not_retried`），**未顺手修改**。
- `StudioManager` 新增 `initialize_with_transports(config, http, ws)`；`initialize_with_transport` 语义不变（HTTP 侧 reqwest）。
- UserAgent 集成**未改 core/harness**：`drive_round*` 以 core `CallableAgent`（`agentscope_core::agent::agent_traits`）为泛型边界，`ReActAgent` 与 `HarnessAgent` 直接可用；用户消息先推 Studio（失败仅告警），agent 错误映射为 `StudioError::Agent`（Java 的 `getInputFromStudio` 失败才回退终端，agent 错误不吞）。

**未完成项与原因**
1. `StudioUserAgent` 自动多轮循环（`loop { drive_round }` + 退出条件 + 中断传播）→ 需宿主决定终止/中断策略（Java `AgentBase` 子类化后由框架驱动，本 crate 无同构宿主），不臆造。
2. `StudioManager` 自动装配 `TelemetryTracing`（§4.2 #4）、`format` span 宿主调用（§4.2 #6）—— 本轮未涉及。
3. P2-1（HTTP 错误重试语义分歧，本轮按现状锁定）、P2-2（OTLP 自定义 header）、P2-3（属性键缺 5）、P2-4（`StudioUserAgent` name 校验）仍开放。**→ 后续：2026-10-31 已全部收口（§10）。**

## 10. 2026-10-31 执行记录 — P2 收口 + Java 增量提交核对

**任务**：把 §6.4 未关闭条目与 §4.2 遗留项收敛为「落代码 + 离线测试」或「已完成（设计决定，不迁移）+ 源码级证据」。只改本 crate 与本 tracker。

**处置总表**
| 条目 | 处置 | 依据 / 位置 |
|---|---|---|
| P2-2 OTLP 自定义 header | **已关闭（代码+测试）** | `exporter.rs`：`OtlpHttpExporter::with_header`（同名覆盖 = Java `HashMap.put`）/ `with_headers`（整体替换 = Java `Builder.headers`）/ `headers()`；`export` 逐个下发；Debug 只显个数 |
| P2-3 属性键缺 5 个 | **已关闭（代码+测试）** | `gen_ai_attributes.rs` +4 键（38）、`agent_scope_attributes.rs` +1 键（6）；`extractors::get_llm_response_attributes` 接入 4 个 usage 属性（Java `AttributesExtractors.java:229-243`，无零值过滤）|
| P2-4 `StudioUserAgent` name 校验 | **已关闭（代码+测试）** | `user_agent.rs::StudioUserAgentBuilder::build() -> StudioResult<StudioUserAgent>`，blank → `StudioError::InvalidArgument`；对应 Java `StudioUserAgent.java:327-331` |
| P2-1 HTTP 错误重试语义 | **已完成（设计决定，不迁移）** | 保留 `StudioError::Http` 不可重试（`is_retryable` 只认 `Transport`）；`error.rs` 文档已改正（原文误称「镜像 Java」）；既有断言 `http_status_errors_are_not_retried` 不动 |
| §4.2 #4 `StudioManager` 自动装配 | **已完成（设计决定，不迁移）** | core 无 `TracerRegistry`（`rg` 零命中），`MiddlewareChain::add` 是唯一挂载入口；自动装配还需产品级裁定（共享实例 vs 每 agent 一条）|
| §4.2 #6 `format` span 自动接线 | **跨模块需求（保留）** | `ExtensionPoint` 8 变体无 formatter 钩子（`middleware/middleware.rs:136-147`）；`core::formatter` 只有自由函数无 formatter trait |
| 多模态 `block_to_part` | **已完成（设计决定，不迁移）** | Java 自身 4 处 `// TODO: support multi modal content`（`AttributesExtractors.java:400/461/503/548`）|
| 裁决 B5 `TelemetryTracer` seam | **已完成（既有实现即最小适配）** | `TelemetryTracing::new(Arc<dyn TraceExporter>)`；映射见 §2.0 `TelemetryTracer` 行；离线由 `RecordingTraceExporter` 覆盖（`tracing.rs` 12 个用例）|

**改动文件**（均在本 crate + 本 tracker）
| 文件 | 改动 |
|---|---|
| `src/telemetry/gen_ai_attributes.rs` | 模块文档 1.37.0 → 1.41.0（对齐 Java 类注释且与 `opentelemetry-bom` 版本解耦）；+4 常量；+2 测试（含「Rust 常量 vs Java 字面量」双向交叉校验）|
| `src/telemetry/agent_scope_attributes.rs` | +`AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS`；+2 测试 |
| `src/telemetry/extractors.rs` | `get_llm_response_attributes` 接入 4 个 usage 属性；+1 测试 |
| `src/telemetry/exporter.rs` | `headers` 字段 + `with_header`/`with_headers`/`headers()`，export 时下发；Debug 不泄密；+3 测试 |
| `src/error.rs` | 新增 `StudioError::InvalidArgument`；`is_retryable` 文档改正为「与 Java 的偏差」 |
| `src/studio/user_agent.rs` | `build()` 返回 `StudioResult` + 空名校验；3 处调用点补 `expect`；+1 测试 |
| `docs/extensions/STUDIO__GAP_TRACKER.md` | §1/§2.0/§2.1/§3.3/§4.2/§6.4/§7.2 与本节 |

**新增测试（9 个，均为离线）**
- `telemetry::gen_ai_attributes::tests::the_previously_missing_keys_match_the_java_literals`
- `telemetry::gen_ai_attributes::tests::the_key_set_matches_the_java_class_exactly`
- `telemetry::agent_scope_attributes::tests::the_key_set_matches_the_java_class_exactly`
- `telemetry::agent_scope_attributes::tests::the_previously_missing_usage_key_matches_the_java_literal`
- `telemetry::extractors::tests::llm_response_attributes_record_every_usage_metric`
- `telemetry::exporter::tests::otlp_headers_replace_same_named_entries_like_java_add_header`
- `telemetry::exporter::tests::with_headers_replaces_the_whole_header_set_like_java_headers`
- `telemetry::exporter::tests::an_exporter_from_config_sends_no_extra_headers`
- `studio::user_agent::tests::blank_names_are_rejected_like_java`

**验证状态**：本轮按子智能体约束**未执行** `cargo build` / `cargo test` / `cargo fmt` / 反向验证（由主智能体统一验证）。静态自检口径：19 个 `.rs` / 6998 行（非空 6363）/ 92 个内联测试（62 `#[test]` + 30 `#[tokio::test]`）。
**建议的反向验证点**（主智能体执行时）：①去掉 4 个 usage 属性 → `llm_response_attributes_record_every_usage_metric` 应 FAILED；②`with_header` 不做同名覆盖 → `otlp_headers_replace_same_named_entries_like_java_add_header` 应 FAILED；③`with_headers` 改为合并而非替换 → `with_headers_replaces_the_whole_header_set_like_java_headers` 应 FAILED；④`build()` 去掉空名校验 → `blank_names_are_rejected_like_java` 应 FAILED。

**Java 增量提交 `8690ab3f5` 核对（`chore(deps): bump opentelemetry-bom 1.66.0→1.67.0`）**
- 影响面：仅 `agentscope-dependencies-bom/pom.xml`（`git show --stat` = 1 file, 2 insertions / 2 deletions），无源码、无 attribute key 变更。
- Rust 侧不依赖 `opentelemetry`/`opentelemetry-otlp`（§4.2 #1），键为字面量字符串；Java 类注释固定的 **semantic conventions 1.41.0** 与 SDK BOM 版本无关 ⇒ **无需动作**。
- 顺带核对出的既有语义差异（已裁定，非新增缺口）：Java `OtlpHttpSpanExporter` 默认 protobuf，本 crate 发 OTLP/HTTP **JSON**；协议允许两种编码，属 §4.2 #1 降级设计的一部分。
