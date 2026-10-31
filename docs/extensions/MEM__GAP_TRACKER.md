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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 121 passed / 0 failed，另 3 个环境门控集成用例 `tests/live_endpoints.rs`，无 `#[ignore]`）—— 25 个 Java 类 1:1 落地，§3.2 / §3.3 全部条目关闭（14 项 `已完成` + 3 项 `已完成（设计决定，不迁移）`）。历史见 history/MEM__MIGRATION_HISTORY.md |

### 1.1 Rust crate 文件清单（17 个源文件 + 1 个集成测试；2026-10-31 增 `retry.rs` / `telemetry.rs` / `tests/live_endpoints.rs`）
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：模块声明、re-export、设计裁定说明（§4.2）、**吞错口径表**、resilience 说明 |
| `src/error.rs` | `MemError` / `Result`（Config / Transport / HttpStatus / Json） |
| `src/http.rs` | `HttpTransport` seam + `ReqwestTransport` + `mock::MockTransport`（离线测试） |
| `src/retry.rs` | `RetryPolicy`（退避/上限/multiplier，默认单次=Java 等价）+ `RetryTransport` 装饰器（仅重试 Transport/408/429/5xx） |
| `src/telemetry.rs` | `MemMetrics`（requests / failures / latency_ms_total 原子计数）+ `InstrumentedTransport`（每请求 `mem.http.post` span） |
| `src/mem0/{mod,dto,client,memory}.rs` | Mem0 DTO、HTTP 客户端、`Mem0LongTermMemory` 适配器 |
| `src/bailian/{mod,dto,client,memory}.rs` | Bailian DTO、HTTP 客户端、`BailianLongTermMemory` 适配器 |
| `src/reme/{mod,dto,client,memory}.rs` | ReMe DTO、HTTP 客户端、`ReMeLongTermMemory` 适配器 |
| `tests/live_endpoints.rs` | 3 个环境变量门控的真实端点往返测试（变量缺失即直接返回，非 `#[ignore]`） |

## 2. Java 清单与依赖映射

### 2.1 Java 类 → Rust 对应（mem0 子模块，9 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `mem0.Mem0Message` | 消息 DTO | `mem0::dto::Mem0Message` | 字段 `role`/`content`/`name`；`name` 为 `NON_NULL`。**注意**：Java `convertToMem0Message` 只设 `role`/`content`、**从不填 `name`**；Rust `convert_to_mem0_message` 会带出 `Msg.name`（外发报文差异，见 history/MEM__MIGRATION_HISTORY.md） |
| `mem0.Mem0ApiType` | 部署类型枚举 | `mem0::dto::Mem0ApiType` | `Platform`/`SelfHosted`，`from_string` 大小写不敏感 |
| `mem0.Mem0AddRequest` | 添加请求 | `mem0::dto::Mem0AddRequest` | Java 无参构造默认值：`infer=true`/`output_format=v1.1`/`async_mode=true`/`immutable=false` |
| `mem0.Mem0AddResponse` | 添加响应 | `mem0::dto::Mem0AddResponse` | `results: Vec<JsonMap>`（异构对象）+ `message` |
| `mem0.Mem0SearchRequest` | 搜索请求 | `mem0::dto::Mem0SearchRequest` | `filters` 始终序列化（`ALWAYS`）；`user_id` 同时镜像进 `filters` |
| `mem0.Mem0SearchResponse` | 搜索响应 | `mem0::dto::Mem0SearchResponse` | `results: Option<Vec<Mem0SearchResult>>` |
| `mem0.Mem0SearchResult` | 单条结果（v2） | `mem0::dto::Mem0SearchResult` | `to_msg()` 为 Rust 侧扩展，映射回 core `Msg` |
| `mem0.Mem0Client` | HTTP 客户端 | `mem0::client::Mem0Client` | 端点常量 + 鉴权头（Platform `Authorization: Token` / SelfHosted `X-API-Key`） |
| `mem0.Mem0LongTermMemory` | 适配器 | `mem0::memory::Mem0LongTermMemory` | `record`/`retrieve` + `retrieve_msgs`；`top_k` 默认 5。**注意**：Java `retrieve` 以 `onErrorReturn("")` 吞错，Rust 主方法 `retrieve` 返回 `Result`，Java 等价语义在 `retrieve_lossy`（见 history/MEM__MIGRATION_HISTORY.md 勘误 E5） |

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
| 离线单测（**121** 个单元测试 + 3 个环境门控集成测试；2026-10-31 收口，原记 96） | Java 测试 18 文件 / 6850 行 / 326 `@Test` | 已完成 | 2026-10-31 静态计数 `#[test]` / `#[tokio::test]` = **121**（含新增 `retry` 7 + `telemetry` 3 + Builder 逐字段 3 + 适配器语义 4 + client 5 + DTO/其他 3），另加 `tests/live_endpoints.rs` 3 个（环境未设即直接返回）。逐文件分布与 Java 对照表见 history/MEM__MIGRATION_HISTORY.md |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实网络联调（真实 Mem0/Bailian/ReMe 服务） | — | 已完成（设计决定，不迁移）（2026-10-31） | **环境受限**：仓库内无任何厂商凭据，CI 与本地均不得联网，故真实端点回归**不在自动化范围内**。影响：仅能证明 wire 契约（端点/鉴权头/报文形状/双响应形状）正确，不能证明厂商侧行为。替代方案：`tests/live_endpoints.rs` 三个**环境变量门控**用例（`MEM0_LIVE_BASE_URL` / `BAILIAN_LIVE_API_KEY` / `REME_LIVE_BASE_URL`，变量缺失即 `return`，**不用 `#[ignore]` 伪装**），有凭据时 `cargo test -p agentscope-extensions-mem --test live_endpoints` 即可跑真链路；DTO/请求体/鉴权已由 `MockTransport` 离线全链路覆盖 |
| Java 测试用例逐条对照迁移 | Java tests 18 文件 / 6850 行 / 326 `@Test` | 已完成（2026-10-31） | **映射表见 history/MEM__MIGRATION_HISTORY.md（18/18 测试类逐条对齐）**。口径：326 个 `@Test` 中，**语义**部分全部有对应用例（Builder 逐字段 ×2、HTTP 状态/非法 JSON 透传、`user_id`↔`filters` 镜像、检索过滤、吞错/严格双路径、嵌套 POJO serde）；其余为 Java bean 样板（getter/setter/`toString`/`null` 构造器/`MockWebServer` 的 `shutdown()` 与 `writeTimeout`）已一句关闭，理由逐类写在 见 history/MEM__MIGRATION_HISTORY.md 末列。新增用例 25 个（96 → 121） |
| Java 吞错语义在 Rust 主 API 上不一致 | `Mem0LongTermMemory.retrieve` / `BailianLongTermMemory.record`+`retrieve` / `ReMeLongTermMemory.retrieve` | 已完成（2026-10-31 裁定） | **正式裁定：默认严格，Java 等价语义显式命名为 `*_lossy`**（`lib.rs`「Error semantics」表 + §4.2 第 4 条固化）。映射：`retrieve().onErrorReturn("")` ↔ `retrieve_lossy(&Msg) -> String`（永不失败 + `tracing::warn!` ≈ Java `log.warn`）；`record().onErrorComplete()` ↔ `record_lossy(&[Msg])`（返回 `()` + warn）；`Mem0`/`ReMe` 的 `record` 在 Java 侧本就透传 → Rust 无 `record_lossy`（对称）。装配口径：写入 agent 一轮对话的调用点用 `*_lossy`（记忆故障不得中断回合），需要感知故障/计数的调用点用严格版。测试：`mem0::memory::tests::{retrieve_lossy_returns_empty_on_http_status_error, retrieve_lossy_returns_empty_on_transport_error, retrieve_lossy_passes_through_successful_results, record_surfaces_http_errors_like_java}`、`bailian::memory::tests::record_lossy_swallows_http_errors`、`reme::memory::tests::{retrieve_lossy_returns_empty_on_http_error, retrieve_lossy_passes_through_memory_list}` |
| 高级检索参数（`rerank`/`filter_memories`/`fields` 等） | `Mem0SearchRequest` 等 | 已完成 | 字段已迁移并序列化；`with_rerank` / `with_filter_memories` / `with_fields` 等 8 个便捷构造器已于 2026-10-31 补齐（§3.3），报文逐字段断言见 `mem0::dto::tests::search_request_builder_covers_every_java_setter` |
| `api_base_url` 必填校验缺失（Mem0 / ReMe） | `Mem0LongTermMemory.Builder.build()`（`"apiBaseUrl is required"`）/ `ReMeLongTermMemory` 构造器（`"apiBaseUrl is required"`） | 已完成 | **2026-10-28 修复**：`Mem0Client::assemble` / `ReMeClient::with_transport` 对空与全空白 base URL 返回 `MemError::Config("apiBaseUrl is required")`（与 Java 文案逐字对齐）；`new` / `with_api_type` / `with_timeout` 均经这两处而同样失败。`with_transport` 签名由 `-> Self` 改为 `-> Result<Self>`——它是唯一可绕过校验的构造路径，且与 Bailian `BailianMemoryClient::with_transport` 既有风格一致；调用点与测试已同步。base URL 先 `trim()` 再剥末尾 `/`。测试：`mem0::client::tests::{blank_base_url_is_rejected_at_construction, new_rejects_blank_base_url, base_url_is_trimmed_before_building_the_request_url}`、`reme::client::tests::{blank_base_url_is_rejected_at_construction, new_rejects_blank_base_url, base_url_is_trimmed_and_trailing_slash_stripped}`。详见 history/MEM__MIGRATION_HISTORY.md |
| 传输层语义差异（连接/写超时、`close`） | OkHttp `connectTimeout(30s)`/`writeTimeout(30s)`/`shutdown()` | 已完成 | **2026-10-28**：`ReqwestTransport::new` 已加 `.connect_timeout(DEFAULT_CONNECT_TIMEOUT)`，常量 = **30s**（对齐 Java `connectTimeout(30s)`）；per-request `timeout` 仍对应 Java `readTimeout`。单测 `http::tests::connect_timeout_matches_java_thirty_seconds`。剩余两项记为 **设计决定（不迁移）**：`writeTimeout(30s)` 与 `shutdown()`/`close()` 在 Rust/reqwest 无对应能力——reqwest 不暴露独立写超时（`timeout` 覆盖含请求体写入在内的整次请求），`reqwest::Client` 无 `close`（由 Drop 释放）。不硬造。详见 history/MEM__MIGRATION_HISTORY.md |
| `bailian/dto.rs` / `reme/dto.rs` 零单测 | Java：DTO 测试共 12 类 / 161 个 `@Test`（mem0 6 类 89 个 + bailian 6 类 72 个；reme 无 DTO 测试类） | 已完成 | **2026-10-28**：补 **21** 个离线 serde 测试（bailian **11** + reme **10**），对标 `mem0/dto.rs` 的 11 个。覆盖字段别名（`meta_data` / `project_ids` / `memory_node_id` / `old_content` / `created_at` / `workspace_id` / `when_to_use` / `time_created` / `reflection_subject` / `update_result` …）、缺省值（bailian DTO 无默认 vs ReMe `top_k=5`）、`skip_serializing_if` 空缺省、未知字段容忍（Java `FAIL_ON_UNKNOWN_PROPERTIES=false`）、添加/删除计数与 `memories()` 提取顺序。无 `#[ignore]`、无网络。详见 history/MEM__MIGRATION_HISTORY.md |

### 3.3 缺口（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 重试 / 退避 / 超时可配置化 | — | 已完成（2026-10-31） | 新增 `src/retry.rs`：`RetryPolicy`（`max_attempts` / `initial_backoff` / `max_backoff` / `multiplier`，`RetryPolicy::default() == none()` 即单次尝试 = Java 无重试）+ `RetryTransport` 装饰器（仅重试 `Transport`、`408`、`429`、`5xx`）；接线 `Mem0Client::with_api_type_and_policy` / `BailianMemoryClient::with_timeout_and_policy` / `ReMeClient::with_timeout_and_policy`。超时本就可配（`with_api_type(timeout)` / `with_timeout`，per-request = Java `readTimeout`，connect 固定 30s）。测试：`retry::tests::{default_policy_is_a_single_attempt_like_java, backoff_grows_exponentially_and_is_capped, retries_transport_errors_until_success, retries_server_errors_but_not_client_errors, stops_after_max_attempts}`、`bailian::client::tests::policy_constructor_is_equivalent_to_the_default_one` |
| 指标 / 可观测性 | `Slf4j` 日志 | 已完成（2026-10-31） | 新增 `src/telemetry.rs`：`MemMetrics`（`requests` / `failures` / `latency_ms_total` 原子计数，`snapshot()` 可被宿主抓取，不引指标 crate）+ `InstrumentedTransport`（每请求 `mem.http.post` span：`url` / `status` / `latency_ms`）。三个 client 的 `reqwest` 构造器默认链 `Retry(Instrumented(Reqwest))` 并暴露 `metrics()`；`*_lossy` 保留 4 处 `tracing::warn!`（Java `log.warn` 等价）。测试：`telemetry::tests::{records_successful_requests, records_failed_attempts_and_returns_the_error_unchanged, metrics_handle_is_shared_with_the_decorator}`、`mem0::client::tests::default_construction_exposes_metrics`、`bailian::client::tests::default_construction_defaults_the_base_url_and_installs_metrics`、`reme::client::tests::default_construction_installs_metrics_and_policy_is_configurable` |
| 未使用依赖 | — | 已完成（2026-10-31） | `Cargo.toml` 删除 `futures`、`agentscope-harness`（`rg` 全 crate 零命中）。`tokio` **保留在 `[dependencies]`**（理由变更：`retry.rs` 的退避走 `tokio::time::sleep`，已是生产路径依赖；同时驱动 `#[tokio::test]`），Cargo.toml 内注明，不再属于「应入 dev-dependencies」 |
| `Mem0AddRequest` 便捷构造器覆盖度 | `Mem0AddRequest.Builder`（20 个 setter） | 已完成（2026-10-31） | 补齐 14 个 `with_*`：`with_app_id` / `with_includes` / `with_excludes` / `with_output_format` / `with_custom_categories` / `with_custom_instructions` / `with_immutable` / `with_async_mode` / `with_timestamp` / `with_expiration_date` / `with_org_id` / `with_project_id` / `with_version` / `with_memory_type`（合计 20，与 Java setter 1:1）。测试：`mem0::dto::tests::add_request_builder_covers_every_java_setter`（逐字段断言序列化键名） |
| `Mem0SearchRequest` 便捷构造器覆盖度 | `Mem0SearchRequest.Builder` | 已完成（2026-10-31） | 补齐 `with_version` / `with_fields` / `with_rerank` / `with_keyword_search` / `with_filter_memories` / `with_threshold` / `with_org_id` / `with_project_id`。测试：`mem0::dto::tests::search_request_builder_covers_every_java_setter` |
| `BailianLongTermMemory` 资源释放 | `AutoCloseable.close()` | 已完成（设计决定，不迁移） | Rust 无 `close()`：`reqwest::Client` 由 Drop 释放，不硬造主动关闭 API；Java 侧 `close()` 转发到 `BailianMemoryClient.close()` → `HttpTransport.close()`。依据同 §3.2「传输层语义差异」行与 见 history/MEM__MIGRATION_HISTORY.md |
| Mem0 消息体多带 `name` 字段 | `Mem0LongTermMemory.convertToMem0Message` | 已完成（2026-10-31） | 改为与 Java 逐字对齐：`convert_to_mem0_message` **不再**设置 `name`（`Mem0Message::with_name` 仍可供显式需要者使用），外发报文不再多 `name` 键。测试：`mem0::memory::tests::convert_message_matches_java_and_omits_name`、`record_posts_all_configured_identifiers_without_a_name_key`（断言 `messages[0].name` 不存在） |
| `BailianLongTermMemoryConfig.api_key` 为「死字段」 | `BailianLongTermMemory` 构造器校验 `apiKey` | 已完成（2026-10-31） | `BailianLongTermMemory::new` 现在校验 `config.api_key`（`"apiKey cannot be null or blank"`，与 Java 文案逐字对齐）后再校验 `user_id`，字段不再是死字段；空 key + 合法 client 不再能通过。测试：`bailian::memory::tests::new_rejects_blank_api_key` |
| `Mem0SearchRequest.user_id` 直接赋值绕过 filters 镜像 | `Mem0SearchRequest.setUserId` | 已完成（2026-10-31） | 字段改**私有**，只经 `with_user_id` / `set_user_id`（两者都同步 `filters`）写入，另提供 `user_id()` 读取与 `sync_filters()`（反序列化后补镜像）；`Mem0Client::search` 在序列化前无条件 `sync_filters()`，wire 层不可能丢租户过滤。测试：`mem0::dto::tests::search_request_keeps_user_id_mirrored_into_filters`、`mem0::client::tests::search_wire_payload_always_carries_the_user_id_filter` |

> **2026-10-31 收口**：§3.3 九行全部关闭（8 `已完成` + 1 `已完成（设计决定，不迁移）`）。上文 2026-10-29 的「真实剩余 ①–⑩」已全部消项，明细与测试清单见 history/MEM__MIGRATION_HISTORY.md。

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：MEM 三厂商（Mem0 / Bailian / ReMe）长期记忆适配全部迁移；DTO serde 对齐；消息转换；检索结果映射；可注入 HTTP seam；重试/退避与指标/span；**121 个离线单元测试 + 3 个环境门控集成测试**（2026-10-31，明细见 history/MEM__MIGRATION_HISTORY.md）。
- **2026-10-31 收口轮（本轮）**：
  1. **吞错口径正式裁定**：默认 API 保持严格（`record`/`retrieve -> Result`），Java 等价吞错语义显式命名 `*_lossy`，映射表固化在 `lib.rs`「Error semantics」与 §4.2 第 4 条。
  2. **测试密度**：新增 25 个用例（96 → 121），补 Builder 逐字段断言、HTTP 状态/非法 JSON 透传、`user_id`↔`filters` 镜像、可选标识符上线、严格 vs 吞错双路径；「Java 测试类 → Rust 用例」映射表见 history/MEM__MIGRATION_HISTORY.md。
  3. **P2 全清**：新增 `src/retry.rs`（`RetryPolicy` + `RetryTransport`）与 `src/telemetry.rs`（`MemMetrics` + `InstrumentedTransport`），三 client 默认接线并暴露 `metrics()`；删除 `futures` / `agentscope-harness` 依赖；补齐 `Mem0AddRequest` 14 个与 `Mem0SearchRequest` 8 个 `with_*`；Mem0 报文去掉 `name`；`BailianLongTermMemoryConfig.api_key` 改为在 `new` 中校验；`Mem0SearchRequest.user_id` 改私有 + `set_user_id` + `sync_filters()`。
  4. **真实端点**：以环境变量门控的 `tests/live_endpoints.rs` 替代凭据依赖（不用 `#[ignore]`），见 §3.2 与 §4.2 第 6 条。
  - **验证状态**：本轮子智能体按 loop 约定**未运行 cargo**（静态计数 `#[test]`/`#[tokio::test]` = 121 + 3 集成），由主智能体统一 `cargo test -p agentscope-extensions-mem` 复核；上一轮（2026-10-28）实测为 **96 passed / 0 failed**。
- **历史轮次（2026-10-22 / 2026-10-28）**：
  1. **2 个编译错误**：`bailian/memory.rs` 与 `reme/memory.rs` 中 `use agentscope_core::{ContentBlockKind, ...}` 报 E0432——`ContentBlockKind` 并未在 `agentscope_core` crate 根 re-export。
     - **修法**：改用真实公开路径 `use agentscope_core::message::msg::ContentBlockKind;`（定义于 `crates/agentscope-core/src/message/msg.rs:401`）。**未自造平行类型，未改 core**。
  2. **后续错误**：`src/http.rs` 顶层 `use std::sync::Arc;` 在移除后 `mock` 模块报 E0425；已在 `mock` 模块内显式 `use std::sync::{Arc, Mutex};`。
  3. **补齐离线单测**：66（`#[test]` 40 + `#[tokio::test]` 26，2026-10-22 逐文件统计见 history/MEM__MIGRATION_HISTORY.md）→ 2026-10-28 **96**（`#[test]` 67 + `#[tokio::test]` 29）。
  4. **`api_base_url` 空校验**：Mem0 / ReMe client 构造期拒绝空与全空白 base URL（`"apiBaseUrl is required"`）；`with_transport` 改返回 `Result<Self>`。
  5. **HTTP 超时对齐**：`ReqwestTransport` 新增 `connect_timeout(30s)`；`writeTimeout` / `close()` 记设计决定（不迁移）。
  6. **DTO 单测**：bailian 11 + reme 10 个离线 serde 测试。
  - **2026-10-28 实测**：`cargo build -p agentscope-extensions-mem` → exit 0（本 crate 0 warning）；`cargo test -p agentscope-extensions-mem` → **96 passed / 0 failed / 0 ignored**。详见 history/MEM__MIGRATION_HISTORY.md。
- **已知问题**：无阻塞、无未关闭条目。`writeTimeout(30s)` / `shutdown()`、真实端点自动化、`BailianLongTermMemory` 的 `close()` 为**设计决定（不迁移）**（§3.2 / §3.3 / §4.2 第 6 条）。

### 4.2 设计裁定与阻塞

1. **【设计裁定，本会话确认】不重建 Java 已废弃的 `LongTermMemory` 体系，也不强制实现 core `Memory` trait。**
   - 事实：Java 侧 `io.agentscope.core.memory.LongTermMemory` 标注 `@Deprecated(forRemoval = true, since = "2.0.0")`；core 侧 Rust 未迁移该体系。Rust `agentscope_core::Memory` trait 是**对话缓冲**抽象（`add_message`/`get_messages`/`delete_message`/`clear`/`save_to`/`load_from`，已由 `InMemoryMemory`、`StateBackedMemory` 实现），与 `LongTermMemory`（`record(&[Msg])` / `retrieve(&Msg) -> String`）**语义不等价**——长期记忆是「按语义检索注入」，而非「缓冲增删」。
   - **裁定**：**不实现** `core::memory::Memory` trait（实现它会导致语义错配，例如把检索命中伪装成会话缓冲消息），也**不重建**废弃的 `LongTermMemory` trait（避免引入平行抽象、且 Java 已判废弃）。改为：各适配器在**本 crate 内暴露固有** `record(&[Msg]) -> Result<()>` / `retrieve(&Msg) -> Result<String>` 方法，并各自提供 `*_lossy` 变体复刻 Java 的 `onErrorReturn("")` / `onErrorComplete()` 吞错语义（§3.2）；并在 Mem0 适配器上提供 Rust 专属便捷方法 `retrieve_msgs(&Msg) -> Result<Vec<Msg>>`（将命中映射为 assistant 侧记忆注记，metadata 携带 `memory_id`/`score`/`categories`/`memory_source`），供上层「想以 core `Msg` 形式消费检索结果」的调用方使用。
   - 理由：(a) 不制造与 core 活跃 trait 的语义错配；(b) 不复活废弃体系；(c) 保留 Java 可观测行为与最小 API 面；(d) 桥接方法 `retrieve_msgs` 以可选便利形式满足 Rust 侧 `Msg` 消费需求。
   - **跨模块待裁定**：若汇总方要求「必须接入 core `Memory` trait」（例如为 harness 统一装配），应新增一个独立 wrapper 类型做**适配**（缓冲语义 ↔ 长期检索语义由装配层显式决定），而非让厂商适配器直接实现该 trait。
2. **【跨模块需求】无硬阻塞。** 本 crate 仅依赖 `agentscope-core`（`Msg`/`MsgRole`/`JsonMap`/`ContentBlock`/`TextBlock`/`ToolUseBlock` 及 `message::msg::ContentBlockKind`）与 `reqwest`/`serde`/`tokio`/`tracing`/`chrono`/`async-trait`/`thiserror`（2026-10-31 已删除未引用的 `agentscope-harness` 与 `futures`）。
   - **建议（非阻塞）**：汇总方若认为 `ContentBlockKind` 属通用查询类型，可考虑在 `agentscope_core::message` 层 re-export（当前需从 `message::msg` 取）——**这属 core 变更，本 crate 不得执行**，仅记录。
3. **【建议（非阻塞）】** 新增依赖均写在本 crate `Cargo.toml` `[dependencies]` 并以 workspace 形式引用；未改根 `Cargo.toml`。`tokio` 因退避 `sleep` 保留为运行时依赖（理由见 §3.3）。
4. **【正式裁定 2026-10-31】吞错口径：默认严格 + `*_lossy` 复刻 Java。**
   - **裁定内容**：无后缀的 `record` / `retrieve` 是**默认可观测 API**，始终返回 `Result`（失败可见）；Java 的吞错语义以 `*_lossy` 命名并逐条对应：`onErrorReturn("")` → `retrieve_lossy(&Msg) -> String`（永不失败，`tracing::warn!` ≈ Java `log.warn`）；`onErrorComplete()` → `record_lossy(&[Msg])`（返回 `()` + warn）。`Mem0LongTermMemory.record` 与 `ReMeLongTermMemory.record` 在 Java 侧**本就透传**（`client.add(...).then()`），故这两个适配器**不提供** `record_lossy`（对称、不制造 Java 不存在的语义）。
   - **装配口径**：把记忆写入/注入到 agent 一轮对话的调用点（Java `LongTermMemoryHook` 的等价位置）使用 `*_lossy`——记忆后端故障不得中断回合；需要上报故障、计数或做健康检查的调用点使用严格版。
   - **依据**：Java 源码逐行核对（`Mem0LongTermMemory.retrieve` `.onErrorReturn("")`；`BailianLongTermMemory.record` `.doOnError(log.warn).onErrorComplete()`、`retrieve` `.onErrorReturn("")`；`ReMeLongTermMemory.retrieve` `.onErrorReturn("")`）。固化位置：`crates/agentscope-extensions-mem/src/lib.rs`「Error semantics」表 + 各方法 rustdoc。
   - **影响**：Rust 默认行为比 Java 更严格（会报错而不是静默）；两者都可显式选择，不存在「Java 有能力、Rust 没有」的缺口。
5. **【裁定 2026-10-31】重试/退避/可观测性放在 transport 装饰器，而非改 client 语义。**
   - `RetryTransport`（`src/retry.rs`）只重试 `Transport`、`408`、`429`、`5xx`；`RetryPolicy::default()` = 单次尝试，因此不动 Java 行为。`InstrumentedTransport`（`src/telemetry.rs`）不引 `metrics` crate，只暴露原子计数 + `mem.http.post` span。
   - 三个 client 的 `reqwest` 构造器固定链 `Retry(Instrumented(Reqwest))` 并经 `metrics()` 暴露计数；注入自定义 transport（`with_transport`）时不自动加装饰器（该 seam 归调用方）。
6. **【设计决定 2026-10-31】真实端点联调不纳入自动化。** 依据：仓库无厂商凭据 / CI 禁止外网，无法在离线环境证明厂商侧行为。替代：`tests/live_endpoints.rs` 环境变量门控（`MEM0_LIVE_BASE_URL` / `BAILIAN_LIVE_API_KEY` / `REME_LIVE_BASE_URL`），未设变量即直接返回；wire 契约由 mock 全链路覆盖。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mem` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mem` / `cargo test -p agentscope-extensions-mem` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mem/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/MEM__MIGRATION_HISTORY.md`](./history/MEM__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
