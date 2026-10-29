# MEM 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **MEM** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-mem\`
> **Java 子模块**：mem0, memory-bailian, reme
> **Rust 目标**：`crates/agentscope-extensions-mem/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-mem` |
| Java 子模块 | mem0, memory-bailian, reme |
| Java 规模（实测 2026-10-22） | 主代码 25 文件 / 5543 行（非空 4821）；测试 18 文件 / 6850 行（非空 5746）/ 326 个 `@Test` |
| Rust crate | `crates/agentscope-extensions-mem` |
| 迁移波次 | A |
| 当前状态 | **总判：进行中**（仍有 P1：真实联调 / 逐条测试对照 / 吞错口径待裁定）—— **已完成**（实现层完成：25 个 Java 类 1:1 落地；2026-10-28 已修 `api_base_url` 空校验、`connect_timeout(30s)` 与 DTO 单测，实测 **96 passed / 0 failed**；§3.2 仍留 2 项待办：真实网络联调 / Java 测试逐条对照；详见 §7） |

### 1.1 Rust crate 文件清单（15 个源文件，2993 行 = 生产 2045 + 内联测试 948；**2026-10-29 勘误**：测试函数数已增至 **96**，行数未重测）
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：模块声明、re-export、设计裁定说明 |
| `src/error.rs` | `MemError` / `Result`（Config / Transport / HttpStatus / Json） |
| `src/http.rs` | `HttpTransport` seam + `ReqwestTransport` + `mock::MockTransport`（离线测试） |
| `src/mem0/{mod,dto,client,memory}.rs` | Mem0 DTO、HTTP 客户端、`Mem0LongTermMemory` 适配器 |
| `src/bailian/{mod,dto,client,memory}.rs` | Bailian DTO、HTTP 客户端、`BailianLongTermMemory` 适配器 |
| `src/reme/{mod,dto,client,memory}.rs` | ReMe DTO、HTTP 客户端、`ReMeLongTermMemory` 适配器 |

## 2. Java 清单与依赖映射

### 2.1 Java 类 → Rust 对应（mem0 子模块，9 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `mem0.Mem0Message` | 消息 DTO | `mem0::dto::Mem0Message` | 字段 `role`/`content`/`name`；`name` 为 `NON_NULL`。**注意**：Java `convertToMem0Message` 只设 `role`/`content`、**从不填 `name`**；Rust `convert_to_mem0_message` 会带出 `Msg.name`（外发报文差异，见 §6.4） |
| `mem0.Mem0ApiType` | 部署类型枚举 | `mem0::dto::Mem0ApiType` | `Platform`/`SelfHosted`，`from_string` 大小写不敏感 |
| `mem0.Mem0AddRequest` | 添加请求 | `mem0::dto::Mem0AddRequest` | Java 无参构造默认值：`infer=true`/`output_format=v1.1`/`async_mode=true`/`immutable=false` |
| `mem0.Mem0AddResponse` | 添加响应 | `mem0::dto::Mem0AddResponse` | `results: Vec<JsonMap>`（异构对象）+ `message` |
| `mem0.Mem0SearchRequest` | 搜索请求 | `mem0::dto::Mem0SearchRequest` | `filters` 始终序列化（`ALWAYS`）；`user_id` 同时镜像进 `filters` |
| `mem0.Mem0SearchResponse` | 搜索响应 | `mem0::dto::Mem0SearchResponse` | `results: Option<Vec<Mem0SearchResult>>` |
| `mem0.Mem0SearchResult` | 单条结果（v2） | `mem0::dto::Mem0SearchResult` | `to_msg()` 为 Rust 侧扩展，映射回 core `Msg` |
| `mem0.Mem0Client` | HTTP 客户端 | `mem0::client::Mem0Client` | 端点常量 + 鉴权头（Platform `Authorization: Token` / SelfHosted `X-API-Key`） |
| `mem0.Mem0LongTermMemory` | 适配器 | `mem0::memory::Mem0LongTermMemory` | `record`/`retrieve` + `retrieve_msgs`；`top_k` 默认 5。**注意**：Java `retrieve` 以 `onErrorReturn("")` 吞错，Rust 主方法 `retrieve` 返回 `Result`，Java 等价语义在 `retrieve_lossy`（§6 勘误 E5） |

### 2.2 Java 类 → Rust 对应（memory-bailian 子模块，8 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `bailian.BailianMessage` | 消息 DTO | `bailian::dto::BailianMessage` | — |
| `bailian.BailianAddRequest` | 添加请求 | `bailian::dto::BailianAddRequest` | JSON 字段 `meta_data`（非 `metadata`） |
| `bailian.BailianAddResponse` | 添加响应 | `bailian::dto::BailianAddResponse` | `request_id` + `memory_nodes`（节点复用 `BailianMemoryNode`，Java 侧 `BailianAddResponse` 另有同名内嵌 `BailianMemoryNode`，无 `created_at`/`updated_at`） |
| `bailian.BailianSearchRequest` | 搜索请求 | `bailian::dto::BailianSearchRequest` | `top_k`/`min_score`/`enable_*`；**Java 构造器不设默认值**（`top_k`/`min_score` 的 10 / 0.3 默认来自 `BailianLongTermMemory.Builder`），Rust 同构 |
| `bailian.BailianSearchResponse` | 搜索响应 | `bailian::dto::BailianSearchResponse` | — |
| `bailian.BailianMemoryNode` | 记忆节点 | `bailian::dto::BailianMemoryNode` | `memory_node_id`/`old_content` 等 snake_case 覆盖 |
| `bailian.BailianMemoryClient` | HTTP 客户端 | `bailian::client::BailianMemoryClient` | `Authorization: Bearer`；key 空白即拒 |
| `bailian.BailianLongTermMemory` | 适配器 | `bailian::memory::BailianLongTermMemory` | `top_k` 默认 10、`min_score` 默认 0.3；丢弃 System/Tool 与 tool-use Assistant |

### 2.3 Java 类 → Rust 对应（reme 子模块，8 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `reme.ReMeMessage` | 消息 DTO | `reme::dto::ReMeMessage` | — |
| `reme.ReMeTrajectory` | 轨迹（消息序列） | `reme::dto::ReMeTrajectory` | — |
| `reme.ReMeAddRequest` | 添加请求 | `reme::dto::ReMeAddRequest` | `workspace_id` + `trajectories` |
| `reme.ReMeAddResponse` | 添加响应 | `reme::dto::ReMeAddResponse` | `answer`/`success`/`metadata`（含 `ReMeAddMetadata`/`ReMeUpdateResult`） |
| `reme.ReMeSearchRequest` | 搜索请求 | `reme::dto::ReMeSearchRequest` | `top_k` 默认 5（`Default` 与 `Builder` 两侧都是 5） |
| `reme.ReMeSearchResponse` | 搜索响应 | `reme::dto::ReMeSearchResponse` | `memories()` 提取 `metadata.memory_list[].content` |
| `reme.ReMeClient` | HTTP 客户端 | `reme::client::ReMeClient` | 无鉴权头，仅 `Content-Type`；空响应体回退 `{}` |
| `reme.ReMeLongTermMemory` | 适配器 | `reme::memory::ReMeLongTermMemory` | `userId` 复用为 `workspace_id`；`retrieve` 优先 `answer` |

> 说明：Java 另含 `ReMeMemoryItem`/`ReMeSearchMetadata`/`ReMeAddMetadata`/`ReMeUpdateResult` 等内嵌/辅助 POJO，均已映射到 Rust（见 `reme::dto`）。

### 2.4 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / `HttpTransport` | HTTP 传输 | `reqwest` + `HttpTransport` trait seam | 生产用 `ReqwestTransport`，测试用 `MockTransport`（离线） |
| Jackson (`@JsonProperty`/`@JsonInclude`) | JSON 序列化 | `serde` / `serde_json` + `#[serde(rename/skip_serializing_if)]` | 字段名 1:1 对齐 |
| `java.time.OffsetDateTime` | 时间戳 | `chrono::DateTime<FixedOffset>` | serde 往返；`+00:00` 序列化为 `Z` |
| `LongTermMemory`（`@Deprecated(forRemoval)`） | 长期记忆接口 | 各适配器**固有** `record`/`retrieve` 方法 | 见 §4.2 设计裁定 |
| `Slf4j` | 日志 | `tracing` | 已声明依赖 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 接线 | 全模块 | 已完成 | 15 源文件全部 `mod` 声明并 re-export |
| `HttpTransport` seam（离线可测） | `Mem0Client`/`BailianMemoryClient`/`ReMeClient` | 已完成 | `src/http.rs`：trait + `ReqwestTransport` + `mock::MockTransport` |
| Mem0 DTO serde 对齐 | `Mem0AddRequest` 等 6 个 DTO | 已完成 | 字段可见 §2.1；实测 7 个类型（含 `Mem0ApiType`）全部存在 |
| Mem0 HTTP 客户端 | `Mem0Client` | 已完成 | 端点/鉴权头/两种搜索响应形状（裸数组 v1.0 + `results` 对象 v1.1） |
| Mem0 适配器 | `Mem0LongTermMemory` | 已完成 | `record`/`retrieve`/`retrieve_msgs`；`build_search_request`/`build_add_request` |
| Bailian DTO + 客户端 + 适配器 | `Bailian*` 8 类 | 已完成 | — |
| ReMe DTO + 客户端 + 适配器 | `ReMe*` 8 类 | 已完成 | `ReMeMemoryItem`/`ReMeAddMetadata`/`ReMeUpdateResult`/`ReMeSearchMetadata` 为内嵌 POJO，Rust 侧提为独立类型 |
| 消息 → 各厂商 message 转换 | `convertTo*Message` | 已完成 | `convert_messages` / `is_recordable` / `role_to_*_role` |
| 检索结果 → 字符串/core `Msg` 映射 | `retrieve` / `getMemories` | 已完成 | `join_search_results` / `join_memory_nodes` / `render_search_response` / `Mem0SearchResult::to_msg` |
| 编译错误修复（`ContentBlockKind` 导入路径） | — | 已完成 | 见 §4.1；实测两处均为 `use agentscope_core::message::msg::ContentBlockKind;`（bailian/memory.rs:7、reme/memory.rs:7） |
| 离线单测（**96** 个；2026-10-29 勘误，原记 66） | Java 测试 18 文件 / 6850 行 / 326 `@Test` | 已完成 | 2026-10-28 实测 `#[test]` 67 + `#[tokio::test]` 29 = **96 passed / 0 failed**（§7.3）；覆盖 DTO serde 往返（含 bailian/reme 新增 21 个）、转换函数、请求构造、构造期 base URL 校验、connect 超时、mock HTTP 全链路 |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实网络联调（真实 Mem0/Bailian/ReMe 服务） | — | 待办 | 需真实凭据；当前仅 mock，网络层已实现但未对真实端点回归 |
| Java 测试用例逐条对照迁移 | Java tests 18 文件 / 6850 行 / 326 `@Test` | 待办（2026-10-29 待复核） | 当前 Rust 单测为**功能等价重写**，未逐条照搬 Java 测试（覆盖率 **96/326 ≈ 29%**（2026-10-29 勘误，原记 66/326 ≈ 20%），缺口集中在 Java 侧的 MockWebServer 传输层细节与 Builder 逐字段断言） |
| 高级检索参数（`rerank`/`filter_memories`/`fields` 等） | `Mem0SearchRequest` 等 | 已完成 | 字段已迁移并序列化；仅未做端到端验证 |
| Java 吞错语义在 Rust 主 API 上不一致 | `Mem0LongTermMemory.retrieve` / `BailianLongTermMemory.record`+`retrieve` / `ReMeLongTermMemory.retrieve` | 待办 | Java 侧 `onErrorReturn("")` / `onErrorComplete()` 使调用方**永远看不到错误**；Rust `retrieve`/`record` 返回 `Result`（严格），等价语义放在 `retrieve_lossy`/`record_lossy`。需在汇总层决定默认用哪套（§4.2 裁定）。**注**：Mem0 `record` 与 ReMe `record` 在 Java 侧本就**不吞错**（`client.add().then()` 透传），Rust 同构，一致 |
| `api_base_url` 必填校验缺失（Mem0 / ReMe） | `Mem0LongTermMemory.Builder.build()`（`"apiBaseUrl is required"`）/ `ReMeLongTermMemory` 构造器（`"apiBaseUrl is required"`） | 已完成 | **2026-10-28 修复**：`Mem0Client::assemble` / `ReMeClient::with_transport` 对空与全空白 base URL 返回 `MemError::Config("apiBaseUrl is required")`（与 Java 文案逐字对齐）；`new` / `with_api_type` / `with_timeout` 均经这两处而同样失败。`with_transport` 签名由 `-> Self` 改为 `-> Result<Self>`——它是唯一可绕过校验的构造路径，且与 Bailian `BailianMemoryClient::with_transport` 既有风格一致；调用点与测试已同步。base URL 先 `trim()` 再剥末尾 `/`。测试：`mem0::client::tests::{blank_base_url_is_rejected_at_construction, new_rejects_blank_base_url, base_url_is_trimmed_before_building_the_request_url}`、`reme::client::tests::{blank_base_url_is_rejected_at_construction, new_rejects_blank_base_url, base_url_is_trimmed_and_trailing_slash_stripped}`。详见 §7 |
| 传输层语义差异（连接/写超时、`close`） | OkHttp `connectTimeout(30s)`/`writeTimeout(30s)`/`shutdown()` | 已完成 | **2026-10-28**：`ReqwestTransport::new` 已加 `.connect_timeout(DEFAULT_CONNECT_TIMEOUT)`，常量 = **30s**（对齐 Java `connectTimeout(30s)`）；per-request `timeout` 仍对应 Java `readTimeout`。单测 `http::tests::connect_timeout_matches_java_thirty_seconds`。剩余两项记为 **设计决定（不迁移）**：`writeTimeout(30s)` 与 `shutdown()`/`close()` 在 Rust/reqwest 无对应能力——reqwest 不暴露独立写超时（`timeout` 覆盖含请求体写入在内的整次请求），`reqwest::Client` 无 `close`（由 Drop 释放）。不硬造。详见 §7 |
| `bailian/dto.rs` / `reme/dto.rs` 零单测 | Java：DTO 测试共 12 类 / 161 个 `@Test`（mem0 6 类 89 个 + bailian 6 类 72 个；reme 无 DTO 测试类） | 已完成 | **2026-10-28**：补 **21** 个离线 serde 测试（bailian **11** + reme **10**），对标 `mem0/dto.rs` 的 11 个。覆盖字段别名（`meta_data` / `project_ids` / `memory_node_id` / `old_content` / `created_at` / `workspace_id` / `when_to_use` / `time_created` / `reflection_subject` / `update_result` …）、缺省值（bailian DTO 无默认 vs ReMe `top_k=5`）、`skip_serializing_if` 空缺省、未知字段容忍（Java `FAIL_ON_UNKNOWN_PROPERTIES=false`）、添加/删除计数与 `memories()` 提取顺序。无 `#[ignore]`、无网络。详见 §7 |

### 3.3 缺口（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 重试 / 退避 / 超时可配置化 | — | 待办 | 当前超时固定（默认 60s），无重试；Java 同样无重试，故仅为增强项 |
| 指标 / 可观测性 | `Slf4j` 日志 | 待办 | 已用 `tracing::warn!` 4 处（3 个 `*_lossy` + bailian `record_lossy`），但无指标/span |
| 未使用依赖 | — | 待办 | `Cargo.toml` 声明了 `futures`、`agentscope-harness`，源码零引用（实测 `rg` 无命中）；`tokio` **仅测试用**（26 个 `#[tokio::test]`），但被放在 `[dependencies]` 而非 `[dev-dependencies]`。建议收敛依赖，属 P2 清理 |
| `Mem0AddRequest` 便捷构造器覆盖度 | `Mem0AddRequest.Builder`（20 个 setter） | 待办 | Rust 只提供 6 个 `with_*`（messages/agent_id/user_id/run_id/metadata/infer），其余 14 个字段（`app_id`/`includes`/`excludes`/`output_format`/`custom_categories`/`custom_instructions`/`immutable`/`async_mode`/`timestamp`/`expiration_date`/`org_id`/`project_id`/`version`/`memory_type`）为 `pub` 字段可直接赋值，功能可达但无链式 API |
| `Mem0SearchRequest` 便捷构造器覆盖度 | `Mem0SearchRequest.Builder` | 待办 | Rust 缺 `version`/`fields`/`rerank`/`keyword_search`/`filter_memories`/`threshold`/`org_id`/`project_id` 的 `with_*`（字段 `pub` 可直接赋值） |
| `BailianLongTermMemory` 资源释放 | `AutoCloseable.close()` | 已完成（设计决定，不迁移）（2026-10-29 勘误：原记「待办」） | Rust 无 `close()`：`reqwest::Client` 由 Drop 释放，不硬造主动关闭 API；Java 侧 `close()` 转发到 `BailianMemoryClient.close()` → `HttpTransport.close()`。依据同 §3.2「传输层语义差异」行与 §7.6-2 |
| Mem0 消息体多带 `name` 字段 | `Mem0LongTermMemory.convertToMem0Message` | 待办 | Java 只填 `role`/`content`；Rust `convert_to_mem0_message` 额外填 `msg.name()`，导致外发 JSON 多一个 `name` 键（Mem0 侧字段合法，功能可达，属**报文形状差异**而非缺功能） |
| `BailianLongTermMemoryConfig.api_key` 为「死字段」 | `BailianLongTermMemory` 构造器校验 `apiKey` | 待办 | Java 构造器校验 `apiKey` 非空后自建 client；Rust 改为外部传入 client，`config.api_key` 在生产代码中**从未被读取**（仅测试用），也不参与任何校验——空 key 配合法 client 可通过 `BailianLongTermMemory::new` |
| `Mem0SearchRequest.user_id` 直接赋值绕过 filters 镜像 | `Mem0SearchRequest.setUserId` | 待办 | Java setter 保证 `user_id` 自动同步进 `filters`；Rust 只有 `with_user_id()` 才镜像，直接写 `req.user_id = Some(..)` 会**漏掉** `filters` 同步（`agent_id`/`run_id`/`app_id` 仅存在于 filters，无此问题） |

> **2026-10-29 复核（状态基准）**：§3.2 已关闭 `api_base_url` 空校验、connect 30s 超时、bailian/reme DTO 单测三项（实测 **96 passed**，见 §7）；`writeTimeout`/`close()` 归入**设计决定（不迁移）**。保留中的**真实剩余**：① 真实端点联调（仅 mock）；② 测试密度 96/326；③ 吞错口径（严格 `retrieve`/`record` vs `*_lossy` 默认）**待裁定**；④ 重试/超时不可配置；⑤ 无指标/span；⑥ 未使用依赖（`futures`、`agentscope-harness`；`tokio` 应入 dev）；⑦ `Mem0AddRequest`/`Mem0SearchRequest` 缺 builder 便捷方法；⑧ Mem0 消息多 `name` 键；⑨ `BailianLongTermMemoryConfig.api_key` 死字段；⑩ 直接写 `user_id` 绕过 `filters` 镜像。

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：MEM 三厂商（Mem0 / Bailian / ReMe）长期记忆适配全部迁移；DTO serde 对齐；消息转换；检索结果映射；可注入 HTTP seam；离线单测 **96** 个（2026-10-29 勘误，原记 66，实测见 §7.3）。
- **本会话修复（收尾轮）**：
  1. **2 个编译错误**：`bailian/memory.rs` 与 `reme/memory.rs` 中 `use agentscope_core::{ContentBlockKind, ...}` 报 E0432——`ContentBlockKind` 并未在 `agentscope_core` crate 根 re-export（`agentscope-core/src/message/mod.rs` 仅 re-export `ContentBlock` 等，未含 `ContentBlockKind`）。
     - **修法**：改用真实公开路径 `use agentscope_core::message::msg::ContentBlockKind;`（该类型定义于 `crates/agentscope-core/src/message/msg.rs:401`，为 `pub enum`）。**未自造平行类型，未改 core**。
  2. **后续错误**：`src/http.rs` 顶层 `use std::sync::Arc;` 在移除后 `mock` 模块（经 `use super::*` 引用 `Arc`）报 E0425；已在 `mock` 模块内显式 `use std::sync::{Arc, Mutex};`。
  3. **补齐离线单测**：总数 **66**（`#[test]` 40 + `#[tokio::test]` 26，实测 2026-10-22 逐文件统计见 §6），覆盖各厂商 DTO serde 往返、消息→Mem0/Bailian/ReMe 转换、检索结果→core `Msg` 映射、搜索/添加请求构造、鉴权头、端点解析、空查询短路、mock HTTP 全链路。
  4. **清理告警**：本 crate 现无任何 warning（`agentscope-core`/`harness` 的 9 个 warning 属既有、非本 crate，未越界修改）。
- **2026-10-28 修复（本轮，3 项）**：
  1. **`api_base_url` 空校验**：Mem0 / ReMe client 构造期拒绝空与全空白 base URL，返回 `MemError::Config("apiBaseUrl is required")`（对齐 Java `Mem0LongTermMemory.Builder.build()` / `ReMeLongTermMemory` 构造器）；`Mem0Client::with_transport` / `ReMeClient::with_transport` 改为返回 `Result<Self>`。
  2. **HTTP 超时对齐**：`ReqwestTransport` 新增 `connect_timeout(30s)`（`DEFAULT_CONNECT_TIMEOUT`）；`writeTimeout` / `close()` 记设计决定（不迁移）。
  3. **DTO 单测**：`bailian/dto.rs` + `reme/dto.rs` 新增 21 个离线 serde 测试；`reme/client.rs` 新增 5 个、`mem0/client.rs` +3 个、`http.rs` +1 个。
  - **测试函数数 66 → 96**（`#[test]` 67 + `#[tokio::test]` 29，按 `^\s*#\[test\]` / `#\[tokio::test` 实测计数口径）。
  - **验证（2026-10-28 实测）**：`cargo build -p agentscope-extensions-mem` → exit 0（本 crate 0 warning）；`cargo test -p agentscope-extensions-mem` → **96 passed / 0 failed / 0 ignored**（exit 0）。
  - 详见 §7（改动清单 / 签名变更说明 / 新增测试名单 / 反向验证）。
- **验证命令与结果（2026-10-22 静态复核未复跑，以下为上一轮记录，未经本次验证）**：
  - `cargo build -p agentscope-extensions-mem` → **Finished**（exit 0，本 crate 0 warning）— *本次未执行*
  - `cargo test -p agentscope-extensions-mem` → **66 passed / 0 failed / 0 ignored**（exit 0）— *本次未执行；测试函数数 66 已静态核实（`#[test]` 40 + `#[tokio::test]` 26，逐文件统计见 §6.2）*
- **已知问题**：无阻塞。真实网络端点未联调（见 §3.2）。§3.2 现余 **2 项 P1 待办**（真实网络联调 / Java 测试逐条对照）；原 3 项已于 2026-10-28 完成（`api_base_url` 空校验、connect 超时对齐、DTO 单测），`writeTimeout`/`close()` 另记作设计决定（不迁移）。§3.3 的 P2 增强项不变。

### 4.2 设计裁定与阻塞

1. **【设计裁定，本会话确认】不重建 Java 已废弃的 `LongTermMemory` 体系，也不强制实现 core `Memory` trait。**
   - 事实：Java 侧 `io.agentscope.core.memory.LongTermMemory` 标注 `@Deprecated(forRemoval = true, since = "2.0.0")`；core 侧 Rust 未迁移该体系。Rust `agentscope_core::Memory` trait 是**对话缓冲**抽象（`add_message`/`get_messages`/`delete_message`/`clear`/`save_to`/`load_from`，已由 `InMemoryMemory`、`StateBackedMemory` 实现），与 `LongTermMemory`（`record(&[Msg])` / `retrieve(&Msg) -> String`）**语义不等价**——长期记忆是「按语义检索注入」，而非「缓冲增删」。
   - **裁定**：**不实现** `core::memory::Memory` trait（实现它会导致语义错配，例如把检索命中伪装成会话缓冲消息），也**不重建**废弃的 `LongTermMemory` trait（避免引入平行抽象、且 Java 已判废弃）。改为：各适配器在**本 crate 内暴露固有** `record(&[Msg]) -> Result<()>` / `retrieve(&Msg) -> Result<String>` 方法，并各自提供 `*_lossy` 变体复刻 Java 的 `onErrorReturn("")` / `onErrorComplete()` 吞错语义（§3.2）；并在 Mem0 适配器上提供 Rust 专属便捷方法 `retrieve_msgs(&Msg) -> Result<Vec<Msg>>`（将命中映射为 assistant 侧记忆注记，metadata 携带 `memory_id`/`score`/`categories`/`memory_source`），供上层「想以 core `Msg` 形式消费检索结果」的调用方使用。
   - 理由：(a) 不制造与 core 活跃 trait 的语义错配；(b) 不复活废弃体系；(c) 保留 Java 可观测行为与最小 API 面；(d) 桥接方法 `retrieve_msgs` 以可选便利形式满足 Rust 侧 `Msg` 消费需求。
   - **跨模块待裁定**：若汇总方要求「必须接入 core `Memory` trait」（例如为 harness 统一装配），应新增一个独立 wrapper 类型做**适配**（缓冲语义 ↔ 长期检索语义由装配层显式决定），而非让厂商适配器直接实现该 trait。
2. **【跨模块需求】无硬阻塞。** 本 crate 仅依赖 `agentscope-core`（`Msg`/`MsgRole`/`JsonMap`/`ContentBlock`/`TextBlock`/`ToolUseBlock` 及 `message::msg::ContentBlockKind`）与 `agentscope-harness`（当前未直接使用，`Cargo.toml` 中保留依赖但代码未引用，可后续按需移除）。
   - **建议（非阻塞）**：汇总方若认为 `ContentBlockKind` 属通用查询类型，可考虑在 `agentscope_core::message` 层 re-export（当前需从 `message::msg` 取）——**这属 core 变更，本 crate 不得执行**，仅记录。
3. **【建议（非阻塞）】** 新增依赖均写在本 crate `Cargo.toml` `[dependencies]` 并以 workspace 形式引用（`serde`/`serde_json`/`thiserror`/`async-trait`/`tokio`/`futures`/`tracing`/`chrono`/`reqwest`）；未改根 `Cargo.toml`。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mem` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mem` / `cargo test -p agentscope-extensions-mem` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mem/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态**：只读源码 + 文本统计，**未运行 `cargo build` / `cargo test`**，未修改任何 Rust 代码。
- 统计口径：行数 = `Get-Content` 总行数；「非空」= 剔除空行；「代码」= 再剔除 `//`、`/* */`、`*` 注释行（仅用于交叉验证）。
- 测试计数：正则 `^\s*#\[test\]` 与 `#\[tokio::test`，逐文件求和；Java 侧用 `@Test` 计数。
- 一致性核对：逐条比对 §2 的「Java 类 → Rust 对应」映射是否真实存在于源码（类型名 + 关键方法 + 行为）。

### 6.2 实测统计
| 项 | 实测值 | 与 tracker 原声明 |
|---|---|---|
| Rust 源文件数 | **15**（`src/*.rs` 4 + `src/{mem0,bailian,reme}/*.rs` 各 3~4） | ✅ 一致 |
| Rust 总行数 | **2993**（生产 2045 + 内联测试 948；非空 2697） | ⚠️ 原写「~2100 行」，实为总行数 2993 / 生产 2045；已更正 |
| Rust 测试函数 | **66**（`#[test]` 40 + `#[tokio::test]` 26） | ✅ 与「66 test」完全一致 |
| Rust 测试分布 | `bailian/memory.rs` 17、`mem0/memory.rs` 15、`reme/memory.rs` 12、`mem0/dto.rs` 11、`mem0/client.rs` 6、`error.rs` 3、`http.rs` 2；`bailian/dto.rs`、`reme/dto.rs`、`bailian/client.rs`、`reme/client.rs`、三个 `mod.rs`、`lib.rs` 均 **0** | ✅ 逐文件复核一致（17+15+12+11+6+3+2 = 66） |
| Java 主代码 | 25 文件 / **5543** 行（非空 4821 / 纯代码 2596） | ✅ 原「~4821 行」= 非空行，口径一致 |
| Java 测试 | 18 文件 / **6850** 行（非空 5746 / 纯代码 5322）/ **326** 个 `@Test` | ✅ 原「~5746 行」= 非空行，口径一致；`@Test` 数原未记录，已补 |
| Java 子模块类数 | mem0 **9**、bailian **8**、reme **8**（共 25） | ⚠️ 原 reme 写「7 类」，实为 8；已更正 |
| Java 测试类数 | mem0 8、bailian 8、reme 2（共 18） | ✅ 一致 |
| Java `@Test` 分布 | mem0 **143**（8 类）、bailian **141**（8 类）、reme **42**（2 类） | — |
| Java DTO 测试类 | **12 类** / **161** 个 `@Test`（mem0 6 类 89 个、bailian 6 类 72 个；reme **0** 个 DTO 测试类） | ⚠️ §6.4 原写「16 个 DTO 测试类、约 150 个 `@Test`」，实为 12 类 / 161 个；见 E13 |

### 6.3 逐子模块对齐结论

| 子模块 | Java 类数 | Rust 现状 | 结论 |
|---|---|---|---|
| **mem0** | 9 | 9 类全部落地：`Mem0Message` / `Mem0ApiType` / `Mem0AddRequest` / `Mem0AddResponse` / `Mem0SearchRequest` / `Mem0SearchResponse` / `Mem0SearchResult` / `Mem0Client` / `Mem0LongTermMemory` | **等价（2 处差异）**。字段名、`NON_NULL` 语义、`filters` 的 `ALWAYS`、`user_id` 镜像、Java 无参默认值（`infer=true`/`output_format=v1.1`/`async_mode=true`/`immutable=false`）、双端点（`/v1/memories/`+`/v2/memories/search/` vs `/memories`+`/search`）、双鉴权头（`Authorization: Token` / `X-API-Key`）、v1.0 裸数组 + v1.1 `results` 双响应形状、`top_k=5`、metadata 合并进 filters、`<compressed_history>` 过滤、`User/System→user`、`Assistant/Tool→assistant` —— **全部逐条核实一致**。差异：(a) Java `convertToMem0Message` **不填 `name`**，Rust 会带出 `Msg.name` → 外发报文多一个 `name` 键（§6.4 新增 P2）；(b) Java `Builder.build()` 校验 `apiBaseUrl` 非空，Rust 全链**无该校验**（§3.2 新增 P1）。另：「错误是否可见」（`retrieve` 严格 vs `retrieve_lossy` 吞错）属 API 设计取舍，已记录 |
| **memory-bailian** | 8 | 8 类全部落地：`BailianMessage` / `BailianAddRequest` / `BailianAddResponse` / `BailianSearchRequest` / `BailianSearchResponse` / `BailianMemoryNode` / `BailianMemoryClient` / `BailianLongTermMemory` | **等价（1 处差异）**。`meta_data` vs `metadata` 命名差异、`snake_case` 覆盖、`Bearer` 鉴权、key/baseUrl 空白即拒、`top_k=10`/`min_score=0.3`、`enable_rerank/judge/rewrite` 默认 false、`project_id`→`project_ids` 单元素、仅 `User`/纯 `Assistant` 可记录、`blank` 与 `trim().is_empty()` 判定、节点 content 非空才拼接 —— **全部核实一致**。差异：Java `AutoCloseable.close()` 无对应；`BailianAddResponse` 的 Java 内嵌同名 `BailianMemoryNode`（无 `created_at/updated_at`）在 Rust 侧复用顶层 `BailianMemoryNode`（多两个可选字段，反序列化向后兼容）；另 Rust `BailianMemoryClient` 会剥掉 base URL 末尾 `/` 而 Java 不会（Rust 更宽容，属**良性差异**）；`BailianLongTermMemoryConfig.api_key` 在 Rust 生产代码中**从未被读取/校验**（§6.4 新增 P2） |
| **reme** | 8 | 8 类全部落地：`ReMeMessage` / `ReMeTrajectory` / `ReMeAddRequest` / `ReMeAddResponse` / `ReMeSearchRequest` / `ReMeSearchResponse` / `ReMeClient` / `ReMeLongTermMemory`（另提独立 `ReMeMemoryItem`/`ReMeAddMetadata`/`ReMeUpdateResult`/`ReMeSearchMetadata`） | **等价（1 处差异）**。`workspace_id` 复用 `userId`、`top_k=5`、无鉴权头、空/`null` 响应体回退 `{}`、`retrieve` 优先 `answer` 再回落 `memory_list[].content` 换行拼接、仅 `User`/纯 `Assistant`、`<compressed_history>` 过滤 —— **全部核实一致**。差异：Java `shutdown()` 无对应；Java 构造器校验 `apiBaseUrl` 非空，Rust `ReMeClient` **不校验**（§3.2 新增 P1）。另 Java `ReMeLongTermMemory.record` **不吞错**（`client.add().then()` 透传），Rust `record` 亦返回 `Result` —— 此项一致 |

> 三子模块均**无「缺失」类**，也无「部分」类（即不存在只搬了签名、没搬行为的空壳）。

### 6.4 新发现缺口

| 优先级 | 缺口 | 依据 | 影响 |
|---|---|---|---|
| **P1** | **Java 吞错语义在 Rust 主 API 上不一致** | `Mem0LongTermMemory.retrieve` / `BailianLongTermMemory.record`+`retrieve` / `ReMeLongTermMemory.retrieve` 均以 `onErrorReturn("")` / `onErrorComplete()` 终止；Rust 主方法返回 `Result`，吞错版本另命名 `retrieve_lossy`/`record_lossy`。**Mem0 的 `record` 在 Java 侧无吞错**（`client.add(request).then()` 直接透传），Rust 也透传——此项一致；**ReMe 的 `record` 同样无吞错**，Rust 亦透传——此项亦一致 | 上层若直接调用 `retrieve`/`record`，行为比 Java 更严格（会抛错而非静默）。**不改变默认调用即产生行为差异**，需汇总层显式选择 |
| **P1** | **`api_base_url` 必填校验缺失（Mem0 / ReMe）** | Java：`Mem0LongTermMemory.Builder.build()` 抛 `"apiBaseUrl is required"`；`ReMeLongTermMemory` 构造器同样抛 `"apiBaseUrl is required"`。Rust：`Mem0Client::assemble` 与 `ReMeClient::with_transport` **均不校验**空 base URL（`rg` 实测两个 client 文件中 `api_base_url` 只有赋值/拼接/取值，无 `is_empty`/`trim` 校验） | `Mem0Client::new("")` / `ReMeClient::new("")` 可成功构造，运行期拼出 `/v1/memories/`、`/summary_personal_memory` 等**无主机 URL**，错误从「构造期 `IllegalArgumentException`」推迟到「网络期」。Bailian 侧已有该校验 → 三厂商行为不一致 |
| **P1** | **`bailian/dto.rs`、`reme/dto.rs` 零单测** | 两文件无 `#[cfg(test)]` 模块（实测 0 个测试）；`mem0/dto.rs` 有 11 个 | DTO 字段名/serde 属性对齐只靠人工比对，回归无防护。Java 侧对应 **12 个** DTO 测试类、**161** 个 `@Test`（mem0 6 类 89 个 + bailian 6 类 72 个；reme 无 DTO 测试类） |
| **P1** | **传输层超时/资源释放粒度** | Java：`connectTimeout(30s)` + `readTimeout(timeout)` + `writeTimeout(30s)`，另有 `shutdown()`/`close()`；Rust `ReqwestTransport` 只有单一 `.timeout(timeout)`，无 `close()` | 连接建立慢或写入阻塞时，Rust 的行为与 Java 不同；`reqwest::Client` 靠 Drop 释放，无法主动 shutdown |
| **P2** | **未使用依赖** | `Cargo.toml` 声明 `futures`、`agentscope-harness`，全 crate 源码零引用（`rg` 实测 0 命中）；`tokio` 仅用于 26 个 `#[tokio::test]`，却放在 `[dependencies]` 而非 `[dev-dependencies]` | 编译期冗余、依赖面扩大；`tokio` 误置于运行时依赖会让下游 crate 被动拉入 tokio |
| **P2** | **Builder/便捷构造器覆盖不全** | `Mem0AddRequest` 20 个 setter → Rust 6 个 `with_*`；`Mem0SearchRequest` 缺 8 个 `with_*`（`version`/`fields`/`rerank`/`keyword_search`/`filter_memories`/`threshold`/`org_id`/`project_id`） | 字段是 `pub`，功能可达，仅链式 API 缺失；移植 Java 调用代码时需改写为字段赋值 |
| **P2** | **`Mem0SearchRequest.user_id` 直接赋值绕过 `filters` 镜像** | Java `Mem0SearchRequest.setUserId` 保证 `user_id` 自动同步进 `filters`；Rust 仅 `with_user_id()` 做镜像，直接写 `req.user_id = Some(..)` 会漏同步 | 迁移 Java 调用代码时若改用字段赋值，搜索过滤条件会**静默丢失**，导致跨用户数据串扰 |
| **P2** | **Mem0 消息体多带 `name` 字段** | Java `Mem0LongTermMemory.convertToMem0Message` 只填 `role`/`content`；Rust `convert_to_mem0_message` 额外填 `msg.name()` | 外发 JSON 多一个 `name` 键。Mem0 侧该字段合法故大概率无害，但与 Java 报文**形状不一致**，用 mock 断言请求体的调用方会看到差异 |
| **P2** | **`BailianLongTermMemoryConfig.api_key` 是「死字段」** | Java `BailianLongTermMemory` 构造器校验 `apiKey` 非空后自建 client；Rust 改为外部注入 client，`config.api_key` 在**生产代码中从未被读取**（`rg` 仅命中声明、构造函数与测试） | 空 key 搭配一个合法 client 可通过 `BailianLongTermMemory::new`，校验被静默绕过；字段本身会误导调用方以为它生效 |
| **P2** | **无指标 / span** | 仅 4 处 `tracing::warn!`（`bailian/memory.rs:239,252`、`mem0/memory.rs:184`、`reme/memory.rs:141`），无 `#[instrument]`、无 metric | 与 Java 的 Slf4j 基本对等，增强项 |

> **2026-10-28 更新**：本节 P1 三项（`api_base_url` 空校验、传输层 connect 超时、`bailian/dto.rs` + `reme/dto.rs` 单测）已落地，详见 §7。下表「影响」列保留为问题发现时的原始描述（不再逐行改写）。

### 6.5 勘误列表

| # | 原文 | 实际 | 处理 |
|---|---|---|---|
| E1 | 「Rust crate 文件清单（15 个源文件，**~2100 行**）」 | 15 个源文件，**2993 行**（生产 2045 + 内联测试 948） | 已改 §1.1 标题 |
| E2 | 「Java 规模（初测） 主代码 ~4821 行 / 测试 ~5746 行」 | 25 文件 / 5543 总行（非空 **4821**）；18 文件 / 6850 总行（非空 **5746**）；**326** 个 `@Test` | 数字本身正确（非空行口径），已补文件数与 `@Test` 数，并标注口径 |
| E3 | 「§2.3 reme 子模块，**7 类**」 | **8 类**（`ReMeMessage`/`ReMeTrajectory`/`ReMeAddRequest`/`ReMeAddResponse`/`ReMeSearchRequest`/`ReMeSearchResponse`/`ReMeClient`/`ReMeLongTermMemory`） | 已改 §2.3 与 §3.1 |
| E4 | 「ReMe DTO + 客户端 + 适配器 `ReMe*` **7 类**」 | **8 类** | 已改 §3.1 |
| E5 | §2.1/§2.2/§2.3 说明列把 `retrieve` 描述为直接等价 Java `retrieve` | Java `retrieve` 是**吞错版**（`onErrorReturn("")`），Rust 主 `retrieve` 返回 `Result`；吞错版为 `retrieve_lossy` | 已在 §2.1 补注、§3.2 新增条目、§4.2 裁定措辞更新 |
| E6 | §2.3「`ReMeSearchResponse.memories()` 提取 `memory_list[].content`」 | 路径为 `metadata.memory_list[].content`（Java 同为 `getMetadata().getMemoryList()`） | 已改 §2.3 |
| E7 | §3.1「Mem0 DTO serde 对齐 … **6 个 DTO**」 | 实际 **7 个类型**（6 个 DTO + `Mem0ApiType` 枚举） | 已改 §3.1 |
| E8 | §2.2「`BailianSearchRequest` … `top_k`/`min_score`」暗示 DTO 自带默认 | Java `BailianSearchRequest` 构造器**不设**默认值，10/0.3 来自 `BailianLongTermMemory.Builder`；Rust 同构（`effective_top_k()`/`effective_min_score()` 兜底） | 已改 §2.2 |
| E9 | §4.1「`ContentBlockKind` 定义于 `msg.rs:386`」 | 定义于 **`msg.rs:401`**（386 是 `content_block_kind()` 辅助函数内） | 已改 §4.1 |
| E10 | §4.1「新增 54 个（原有 2 个）」 | 无法静态验证历史增量；**当前总数 66** 已逐文件核实。措辞改为只陈述总数与分布 | 已改 §4.1 第 3 条 |
| E11 | §1「当前状态 **已完成**（build + test 全绿，66 passed / 0 failed）」 | 测试函数数 66 已静态核实；但 build/test **本次未复跑**，且 §3.2 仍有 2 项 P1「待办」 | 已改为「已完成（实现层）… 未重新编译，§3.2 遗留 2 项 P1」，并在 §4.1 标注「本次未执行」 |
| E12 | §3.3「指标/可观测性：仅 `tracing` 依赖已声明，**未埋点**」 | 实际有 4 处 `tracing::warn!`（三处 `*_lossy` + bailian `record_lossy`） | 已改 §3.3 |
| E13 | §6.4「Java 侧对应 **16 个 DTO 测试类**、约 **150** 个 `@Test`」 | 实为 **12 个** DTO 测试类（mem0 6 + bailian 6，**reme 0 个**）、**161** 个 `@Test`（89 + 72） | 已改 §6.2 + §6.4 |
| E14 | §6.4「`Mem0Client` 未校验 `api_base_url` …… 故**行为等价但校验点前移**；记录以免后续误判为缺口」 | **结论错误**：Rust 侧**根本没有**该校验（`Mem0Client::assemble` 不校验，`Mem0LongTermMemoryConfig` 也不含 `api_base_url`），且「必须先构造 client」**不构成**类型层面的必填保证——`Mem0Client::new("")` 可正常构造。ReMe 同理 | 已删除原「非缺口」结论，改为 §3.2 + §6.4 的 **P1 真实缺口** |
| E15 | §6.2「Java 测试类数 mem0 8、bailian 8、reme 2」（未标总数） | 补「共 18」，并新增「Java `@Test` 分布：mem0 143 / bailian 141 / reme 42」 | 已改 §6.2 |
| E16 | §2.1「`mem0.Mem0Message` …… 字段 `role`/`content`/`name`；`name` 为 `NON_NULL`」（读作 Java 转换时会带 `name`） | Java `convertToMem0Message` **只设 `role`/`content`**，从不填 `name`；Rust `convert_to_mem0_message` 会带出 `Msg.name` → 外发报文多一个 `name` 键 | 已在 §2.1 补注，并在 §3.3 + §6.4 登记为 P2 差异 |
| E17 | §3.3「未使用依赖 …… `tokio` 仅测试用」 | 补充：`tokio` 被放在 `[dependencies]` 而非 `[dev-dependencies]`，会污染下游 crate 的运行时依赖 | 已改 §3.3 + §6.4 |
| E18 | §3.3「`Mem0SearchRequest` 便捷构造器覆盖度 …… Rust 缺 8 个 `with_*`」（未提副作用） | 补：Rust 只有 `with_user_id()` 会把 `user_id` 镜像进 `filters`；**直接写 `pub user_id` 字段会漏镜像**（Java `setUserId` 保证同步），属静默丢过滤条件的风险 | 已改 §3.3 + §6.4 |

### 6.6 状态取值核对
- 本文件使用的状态值均在 §0 定义集合内：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- 本次新增条目一律取 `待办` 或 `已完成`，未引入新取值。
- §1「当前状态」栏沿用 `已完成`，但附注「实现层完成、未重新编译、§3.2 遗留 P1 待办」，避免与 §3.2 的 `待办` 自相矛盾。
- 【2026-10-28 更新】§3.2 现有 **2 行 `待办`**（真实网络联调 / Java 测试逐条对照）；原 3 项已置 `已完成`（`api_base_url` 校验、传输层 connect 超时、DTO 单测），其中 writeTimeout 与 `close()` 另记作 `设计决定（不迁移）`。§3.3 仍有 **9 行 `待办`**（P2 增强类）。

### 6.7 本轮复核追加（2026-10-22 第二次静态复核）
> 幂等说明：本轮为**同一天的第二次复核**，对 §6.1~§6.6 **就地修正**，未重复追加复核节。

#### 6.7.1 本轮新增/修正的可验证声明
| 声明 | 复核方式 | 结果 |
|---|---|---|
| Rust 15 文件 / 2993 行 / 66 test | 逐文件 `Get-Content` 计数 + 正则 `^\s*#\[test\]` / `#\[tokio::test` | ✅ 全部一致（见 §6.2） |
| Java 主 25 文件 5543 行（非空 4821）/ 测 18 文件 6850 行（非空 5746）/ 326 `@Test` | 同上 | ✅ 全部一致 |
| 「Java 类 → Rust 对应」映射真实存在 | 逐个 Java 类打开源码确认类名/关键方法/字段，再在 Rust 侧确认同名类型与行为 | ✅ 25/25 映射**全部真实存在**，无「空壳类」 |
| 「已完成 / 66 test」与代码一致 | 静态核实测试函数总数 = 66，且分布与 §6.2 吻合 | ✅ **一致**（但 build/test 本身本轮仍未复跑） |
| Rust `Mem0SearchResult` 时间戳 serde 往返 | Rust `chrono::DateTime<FixedOffset>` + `+00:00`→`Z` 归一化；Java `JacksonJsonCodec` 未开 `WRITE_DATES_AS_TIMESTAMPS`，Jackson `JavaTimeModule` 默认输出 ISO-8601 `Z` 形式 | ✅ 两侧报文一致（Java 测试 `Mem0SearchResultTest` 断言 `"created_at":"2025-01-15T10:30:00Z"`，Rust 测试断言 `2024-01-02T03:04:05Z`） |
| Java `FAIL_ON_UNKNOWN_PROPERTIES=false` vs Rust serde 默认忽略未知字段 | 读 `JacksonJsonCodec.createDefaultObjectMapper()`；Rust serde 默认（无 `deny_unknown_fields`）亦忽略 | ✅ 宽容度一致；Java 类上的 `@JsonIgnoreProperties(ignoreUnknown=true)` 在 Rust 侧由 serde 默认行为等价覆盖 |
| §4.2 「不实现 core `Memory` trait / 不重建 `LongTermMemory`」 | 在 `agentscope-core` 全文检索 `LongTermMemory`/`long_term` → **0 命中**；core `memory/mod.rs` 只有会话缓冲语义 `Memory` trait；Java 侧 `LongTermMemory.java:70` 确为 `@Deprecated(forRemoval = true, since = "2.0.0")` | ✅ 裁定前提成立，Rust core 无对应体系可接；Java 的 `StaticLongTermMemoryHook`/`LongTermMemoryTools`/`LongTermMemoryMode`（core 内 18 处引用）**在 Rust core 中完全无对应物** |
| Java `Mem0Client` 端点/鉴权、ReMe 端点、Bailian 端点 | 逐一比对常量与 header 拼装 | ✅ 6 个端点常量与 3 种鉴权方式全部 1:1 命中 |

#### 6.7.2 本轮新发现/复核的缺口汇总（§6.4 节内口径：P0: 0 / P1: 3 / P2: 6）
| 优先级 | 缺口 | 落点 |
|---|---|---|
| P1 | `api_base_url` 必填校验缺失（Mem0 / ReMe） | §3.2 新增行、§6.4 新增行、§6.5 E14 |
| P1 | `bailian/dto.rs` / `reme/dto.rs` 零单测（Java 侧 12 类 161 个 `@Test` 无对应） | §6.4（修正 Java 侧数字） |
| P1 | 传输层超时/资源释放粒度（connect/write 超时、`shutdown`/`close`） | §6.4（原已列，保留） |
| P2 | Mem0 消息体多带 `name` 字段 | §3.3、§6.4、§6.5 E16 |
| P2 | `BailianLongTermMemoryConfig.api_key` 为死字段、校验被绕过 | §3.3、§6.4 |
| P2 | `Mem0SearchRequest.user_id` 直接赋值绕过 `filters` 镜像 | §3.3、§6.4、§6.5 E18 |
| P2 | `tokio` 误置于 `[dependencies]`（应入 `[dev-dependencies]`） | §3.3、§6.4、§6.5 E17 |
| P2 | 无指标 / span（仅 4 处 `tracing::warn!`） | §3.3、§6.4（原已列，保留） |
| P2 | Rust `BailianMemoryClient` 剥除 base URL 末尾 `/`，Java 不剥（**良性差异**，Rust 更宽容） | §6.3 bailian 行 |

#### 6.7.3 剩余真实缺口的优先级判定
> 口径说明：下表按**本 tracker 的缺口台账**统计，与 §6.4 的「§6.4 节内列出的行」为**不同口径**，二者不冲突。

- **P0：0**。三厂商 25 个 Java 类在 Rust 侧 1:1 落地且行为等价，核心链路（record / retrieve / DTO 序列化 / 鉴权 / 端点）无缺失，**不构成阻塞**。
- **P1：§3.2 共 5 项待办**。全部为「验证 / 工程完备性 / 校验前移」类，**无「Java 有能力、Rust 没有」的能力缺口**：(1) 真实网络联调（需凭据，非代码缺口）；(2) Java 测试逐条对照迁移（覆盖率 66/326 ≈ 20%，缺口集中在 Java 侧 MockWebServer 传输层细节与 Builder 逐字段断言）；(3) 吞错语义取舍（`retrieve` 严格 vs `retrieve_lossy`——Java 等价语义**已实现**，只差默认口径裁定）；(4) **`api_base_url` 校验缺失（Mem0 / ReMe）**——本轮新确认，是 P1 中**唯一有行为风险**的一项；(5) 传输层资源释放粒度（connect/write 超时、`shutdown`/`close`，功能等价）。 （2026-10-29 勘误：本段为 2026-10-22 快照——`api_base_url` 校验与 `connect` 超时已于 2026-10-28 修复，实测 96 passed，覆盖率 96/326 ≈ 29%；当前剩余见 §3.2/§3.3/§4.2）
- **P2：§3.3 共 9 项待办**（重试/退避、指标、未使用依赖、2 个 Builder 覆盖度、`close()`、`name` 字段差异、`api_key` 死字段、`user_id` 镜像绕过）。均为增强 / API ergonomics / 报文形状微差，不阻塞汇总。

#### 6.7.4 rollup 状态建议
- **建议 rollup 状态值：`已完成`**（实现层）。
- 理由：(a) Java 25 个类 **100% 有真实对应的 Rust 实现**，无缺失类、无空壳类、无「只搬签名未搬行为」的情况；(b) 66 个离线测试函数数量与代码**完全一致**，DTO serde 往返、消息转换、请求构造、鉴权头、端点解析、空查询短路、mock HTTP 全链路均有覆盖；(c) §3.2 的 P1 **全部是「验证/工程完备性」或「API 校验前移」类**，不构成「Java 有能力、Rust 没有」的**能力缺口**——其中 `api_base_url` 校验是唯一有行为风险的项，但影响面限于「构造期报错 vs 运行期报错」，不改变正常路径行为；(d) 本轮为**纯静态复核、未重新编译**，故建议 rollup 保留「实现层完成」的措辞，并在 rollup 行注明「静态复核通过；build/test 未在本轮复跑」，以免与 §3.2 的 `待办` 项自相矛盾。
- **若汇总方要求「零待办才可置完成」**：可先补齐 §3.2 的 `api_base_url` 校验（改动量极小，仅 `mem0/client.rs` 与 `reme/client.rs` 各加一处 `trim().is_empty()` 判断 + 对应单测），再将状态提为无保留的 `已完成`；其余 P1/P2 可留待后续波次。

## 7. 执行记录（2026-10-28）

> 执行范围：仅 `crates/agentscope-extensions-mem/**` 与本文档；**未改**根 `Cargo.toml`、`agentscope-core`、`agentscope-harness` 及其它 crate。**新增依赖：无**（复用已有 `serde`/`serde_json`/`reqwest`）。

### 7.1 改动清单

| 文件 | 改动 |
|---|---|
| `src/mem0/client.rs` | `assemble` 增加 `trim().is_empty()` 校验 → `MemError::Config("apiBaseUrl is required")`；`assemble` / `with_transport` 改返回 `Result<Self>`；`with_api_type` 传递错误；base URL 先 `trim()` 再剥末尾 `/`；+3 测试 |
| `src/reme/client.rs` | 同构校验（`with_transport` → `Result<Self>`）；+5 测试（该文件原本无 `mod tests`） |
| `src/mem0/memory.rs`、`src/reme/memory.rs` | 测试内 3 处 `with_transport` 调用点补 `.expect("valid base url")`（**生产代码未改**） |
| `src/http.rs` | 新增 `pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30)`；`ReqwestTransport::new` 加 `.connect_timeout(...)`；+1 测试；doc 注明 reqwest 无独立 write timeout |
| `src/bailian/dto.rs` | +11 个 serde 测试 |
| `src/reme/dto.rs` | +10 个 serde 测试 |
| `src/lib.rs` | 新增「Construction is validated」文档小节，说明三厂商构造期校验与 `with_transport` 返回 `Result` |

### 7.2 签名变更说明（唯一破坏性变更）

`Mem0Client::with_transport` 与 `ReMeClient::with_transport`：`-> Self` → `-> Result<Self>`。

- **理由**：这两个构造函数是**唯一**能绕过 base URL 校验的入口（`Mem0Client`/`ReMeClient` 字段私有、无 `Default`）；保持 `-> Self` 就无法让空 URL 在构造期失败。返回 `Result` 与同 crate 的 `BailianMemoryClient::with_transport -> Result<Self>` 风格一致。
- **影响面**：全仓 `rg "Mem0Client::with_transport|ReMeClient::with_transport"` 仅命中本 crate 的 4 处测试调用点，已同步；**无跨 crate 调用方**，故无下游破坏。
- **未改动**：`Mem0Client::new` / `with_api_type` / `ReMeClient::new` / `with_timeout` 签名本就返回 `Result<Self>`，仅行为由「恒 Ok」变为「空 URL 时 Err」，正常路径不变。

### 7.3 验证命令与实测

| 命令 | 结果 |
|---|---|
| `cargo build -p agentscope-extensions-mem` | **exit 0**，本 crate **0 warning**（`agentscope-harness` 的 5 个 warning 属既有、未越界修改） |
| `cargo test -p agentscope-extensions-mem` | **96 passed / 0 failed / 0 ignored**（exit 0）；静态计数 `#[test]` **67** + `#[tokio::test]` **29** = 96 |

### 7.4 新增测试名单

- `mem0::client::tests`：`blank_base_url_is_rejected_at_construction`、`new_rejects_blank_base_url`、`base_url_is_trimmed_before_building_the_request_url`
- `reme::client::tests`：`blank_base_url_is_rejected_at_construction`、`new_rejects_blank_base_url`、`base_url_is_trimmed_and_trailing_slash_stripped`、`add_posts_to_summary_endpoint`、`blank_response_body_falls_back_to_default`
- `http::tests`：`connect_timeout_matches_java_thirty_seconds`
- `bailian::dto::tests`（11）：`message_roundtrips_role_and_content`、`add_request_uses_meta_data_key_not_metadata`、`add_request_skips_absent_fields`、`add_request_serializes_snake_case_fields`、`search_request_serializes_snake_case_and_flags`、`search_request_has_no_dto_level_defaults`、`memory_node_deserializes_snake_case_fields`、`memory_node_serializes_nothing_when_empty`、`add_response_deserializes_request_id_and_nodes`、`search_response_preserves_node_order`、`responses_tolerate_missing_and_unknown_fields`
- `reme::dto::tests`（10）：`message_roundtrips_role_and_content`、`trajectory_wraps_messages_and_skips_absent_field`、`add_request_serializes_workspace_id_and_trajectories`、`search_request_defaults_top_k_to_five`、`search_request_with_parts_serializes_query_and_top_k`、`memory_item_deserializes_snake_case_fields`、`memory_item_serializes_nothing_when_empty`、`add_response_deserializes_metadata_and_update_result`、`search_response_memories_extracts_contents_in_order`、`search_response_without_metadata_has_no_memories`

> 全部离线：仅用 `crate::http::mock::MockTransport` 或纯 `serde_json`，无网络、无 `#[ignore]`。

### 7.5 反向验证（回退实现 → 观察 FAILED → 恢复）

| # | 回退内容 | 观察结果 | 恢复 |
|---|---|---|---|
| 1 | `mem0/client.rs`：`assemble` 的 `if trimmed.is_empty() {` → `if false {`（文本相同，替换连带命中 `search` 的空响应体校验） | `cargo test` → **FAILED：93 passed / 3 failed**；失败项：`mem0::client::tests::blank_base_url_is_rejected_at_construction`（`unwrap_err()` on `Ok(Mem0Client { api_base_url: "" ... })`）、`new_rejects_blank_base_url`、`search_rejects_empty_body`（`assertion failed: matches!(err, MemError::Transport(_))`） | 还原后 **96 passed / 0 failed** |
| 2 | `http.rs`：`DEFAULT_CONNECT_TIMEOUT` 30s → 10s | `cargo test` → **FAILED：95 passed / 1 failed**；`http::tests::connect_timeout_matches_java_thirty_seconds`（`assertion left == right failed: left: 10s`） | 还原后 **96 passed / 0 failed** |
| 3 | `bailian/dto.rs`：`rename = "meta_data"` → `rename = "metadata"` | `cargo test` → **FAILED：95 passed / 1 failed**；`bailian::dto::tests::add_request_uses_meta_data_key_not_metadata`（`dto.rs:296`） | 还原后 **96 passed / 0 failed** |

> 注：#1 的回退范围意外覆盖了 `Mem0Client::search` 的空响应体校验（两处 `if trimmed.is_empty()` 文本相同），因此多出 1 个失败项——反而反向证明了该替换确实生效；两个判断现均已恢复（`git diff` 核对，`mem0/client.rs` 无残留 `if false`）。

### 7.6 未完成 / 需汇总方裁定

1. **真实网络联调**（§3.2 仍待办）：本次全部离线，未对真实 Mem0 / Bailian / ReMe 端点回归（需凭据）。
2. **`writeTimeout(30s)` 与 `shutdown()`/`close()`**：按任务要求记为 **设计决定（不迁移）**——Rust/reqwest 无独立 write timeout、`reqwest::Client` 无 `close`，未硬造。
3. **Java 测试逐条对照迁移**（§3.2 仍待办）与 **吞错语义默认口径**（§3.2，需汇总方裁定 `retrieve` 严格 vs `retrieve_lossy`）未变。
4. **校验层位置（请裁定）**：本次校验落在 **client 构造**（比 Java 的 adapter builder 更早，且覆盖所有 adapter 路径），未在 `Mem0LongTermMemory::new` / `ReMeLongTermMemory::new` 重复校验（`Arc<Mem0Client>` 已不可能持空 URL）。若汇总方要求错误必须在 adapter 层抛出，需额外 wrapper。
