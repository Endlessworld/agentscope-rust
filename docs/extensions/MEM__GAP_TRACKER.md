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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 121 passed / 0 failed，另 3 个环境门控集成用例 `tests/live_endpoints.rs`，无 `#[ignore]`）—— 25 个 Java 类 1:1 落地，§3.2 / §3.3 全部条目关闭（14 项 `已完成` + 3 项 `已完成（设计决定，不迁移）`）。历史见 history/MEM__MIGRATION_HISTORY.md 。**2026-10-31 复核批次**：`MEM-01`（P0 框架装配点）/ `MEM-02`（P1 错误上下文）/ `MEM-03`（P2 ReMe 文案）已落地，静态计数 121 → **131** 单测 + 新增 1 个真实装配集成用例（本轮未跑 cargo，实测归汇总方）。**2026-11-01 Round 2**（本轮实测）：`MEM-04`（`is_recordable` 补齐完整 Java 谓词）/ `MEM-06`（Bailian+ReMe client 错误路径用例）已落地，静态计数 131 → **138**；`cargo test -p agentscope-extensions-mem -j 2 --no-fail-fast` → **138 + 3 + 1 passed / 0 failed / EXIT=0**（lib 138 + `tests/live_endpoints.rs` 3 + `tests/long_term_memory_wiring_test.rs` 1）。**2026-11-01 Round 4**（本轮实测）：`MEM-05`（非幂等 `add` 的重试风险明示 + 构造期警示 + 行为用例）已落地，静态计数 138 → **139**；`cargo build -p agentscope-extensions-mem -j 2` EXIT=0、`cargo test -p agentscope-extensions-mem -j 2 --no-fail-fast` → **139 + 3 + 1 passed / 0 failed / EXIT=0**；§3.5.1 六个编号（MEM-01…MEM-06）**全部关闭**。 |

> **2026-10-31 全量对齐复核**：基线 Java `main` @ `1524b3191`，逐文件核对结论见 §3.5（缺口/待裁定 `MEM-01…MEM-06`，已对齐抽样 `MEM-07…MEM-20`）与 §4.3（不迁移 `MEM-D01…D05`、已知差异 `MEM-X01…X09`）。结论：wire 契约与适配语义已对齐；唯一 P0 为 **MEM-01（无框架装配点）**，已于 2026-10-31 落地（`src/middleware.rs`）。

### 1.1 Rust crate 文件清单（18 个源文件 + 2 个集成测试；2026-10-31 增 `retry.rs` / `telemetry.rs` / `middleware.rs` / `tests/live_endpoints.rs` / `tests/long_term_memory_wiring_test.rs`）
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：模块声明、re-export、设计裁定说明（§4.2）、**吞错口径表**、resilience 说明 |
| `src/error.rs` | `MemError` / `Result`（Config / Transport / HttpStatus / Json） |
| `src/http.rs` | `HttpTransport` seam + `ReqwestTransport` + `mock::MockTransport`（离线测试） |
| `src/retry.rs` | `RetryPolicy`（退避/上限/multiplier，默认单次=Java 等价）+ `RetryTransport` 装饰器（仅重试 Transport/408/429/5xx） |
| `src/telemetry.rs` | `MemMetrics`（requests / failures / latency_ms_total 原子计数）+ `InstrumentedTransport`（每请求 `mem.http.post` span） |
| `src/middleware.rs` | `MemLongTermMemoryMiddleware`（`on_reasoning` 检索注入 / `on_agent` 尾部 `record_lossy`，对齐 Java `StaticLongTermMemoryHook`）+ `LongTermMemoryBackend` 窄 seam（三适配器实现）+ `wrap_long_term_memory` / `long_term_memory_message` |
| `tests/long_term_memory_wiring_test.rs` | MEM-01 验收：StubModel + 内存 transport 经真实 `HarnessAgent::builder().middleware(chain)` 驱动一轮对话 |
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
| 离线单测（**131** 个单元测试 + 4 个集成测试（3 环境门控 + 1 真实装配）；2026-10-31 复核批次后静态计数） | Java 测试 18 文件 / 6850 行 / 326 `@Test` | 已完成（131 = 121 + 本轮 10：`middleware` 5 + `error::with_context` 2 + 三厂商 `http_failures_name_the_vendor_and_the_operation` 3） | 2026-10-31 静态计数 `#[test]` / `#[tokio::test]` = **121**（含新增 `retry` 7 + `telemetry` 3 + Builder 逐字段 3 + 适配器语义 4 + client 5 + DTO/其他 3），另加 `tests/live_endpoints.rs` 3 个（环境未设即直接返回）。逐文件分布与 Java 对照表见 history/MEM__MIGRATION_HISTORY.md |

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

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.3 历史清单保持原样未重新编号**。
> 编号规则：`MEM-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `MEM-DNN` / `MEM-XNN` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> **2026-10-31 复核后批次（本轮）**：`MEM-01`（P0）、`MEM-02`（P1）、`MEM-03`（P2）已落地并转 `已完成`（明细见各行行内【2026-10-31 落地】注记）；`MEM-04` 于 2026-11-01 Round 2 关闭；`MEM-05` / `MEM-06` 于 2026-11-01 Round 4 / Round 2 关闭（§3.5.1 缺口表清零）。
> 覆盖范围：Java 3 子模块 25 个主代码类（5543 行，测试 18 类 / 326 个 `@Test`）↔ Rust 17 个 `.rs`（含各子模块 `mod.rs`）+ 1 个集成测试；测试数只引用既有实测（`#[test]`/`#[tokio::test]` 静态计数 **121**，另 `tests/live_endpoints.rs` 3 个环境门控用例），本轮复核未运行 cargo。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| MEM-01 | P0 | 三厂商适配器**无任何框架装配点**，crate 外零引用：Java 由 `ReActAgent.Builder.longTermMemory/longTermMemoryMode/longTermMemoryAsyncRecord`（三者均 `@Deprecated(forRemoval = true, since = "2.0.0")`）+ `StaticLongTermMemoryHook`（PreReasoning 检索注入 / PostCall 记录）+ `LongTermMemoryTools`（agent 自主 memory 工具）驱动；Rust 侧既无等价 hook/middleware，也无 `dyn` 抽象，宿主必须自己手写调用点 | `agentscope-core/src/main/java/io/agentscope/core/ReActAgent.java#Builder.longTermMemory`、`memory/StaticLongTermMemoryHook.java#StaticLongTermMemoryHook`、`memory/LongTermMemoryTools.java#LongTermMemoryTools`、`memory/LongTermMemoryMode.java#LongTermMemoryMode` | `src/lib.rs:11-25` 只声明「不重建废弃 trait」；`rg -ni 'long.?term.?memory' crates/agentscope-harness/src crates/agentscope-core/src` 仅命中文件型工具 `tool/memory_{save,get,search}.rs` 与 `agent/memory/`（flush/consolidate），无 `LongTermMemory`/hook/中间件 seam；`rg -n 'agentscope_extensions_mem' crates` 仅命中本 crate 的 `src/lib.rs`、`tests/live_endpoints.rs`、`Cargo.toml` | 已完成 | 裁定二选一：(a) 参照 Java 废弃注释「reimplement as a MiddlewareBase if required」提供 Middleware 形态 seam（run 前注入检索、run 后 `record_lossy`），落 harness 或本 crate 的适配层；(b) 明确不迁移（转 §4.3.1 MEM-D01）。验收：(a) 需一条**走真实 `HarnessAgent` 装配入口**的用例（不得直接 poke 适配器） 【裁定 2026-10-31：A：mem crate 提供 MemLongTermMemoryMiddleware（on_reasoning 注入 / on_agent 记录），宿主经 builder.middleware 挂载】 【2026-10-31 落地：已完成——新增 `src/middleware.rs`：`MemLongTermMemoryMiddleware`（`order=50`；`on_reasoning` 取最后一条 USER 消息检索并按 Java `LongTermMemoryTools.wrap` 追加 `long_term_memory` USER 消息、按 `run_id` 缓存保证每轮只检索一次且不重复注入；`on_agent` 结束后把 `AgentState.context` 交给 `record_lossy`，失败降级 warn）+ `LongTermMemoryBackend` 窄 seam（Mem0/Bailian/ReMe 三适配器实现，非复活废弃 `LongTermMemory`）。真实装配入口用例：`tests/long_term_memory_wiring_test.rs::the_middleware_retrieves_before_reasoning_and_records_after_the_turn`（StubModel + 内存 transport 走 `builder().middleware(chain)`，断言检索结果进入模型请求、search 仅 1 次、add 含 user+assistant；新增 dev-dep `agentscope-harness`、依赖 `futures`）】 |
| MEM-02 | P1 | 错误**上下文丢失**：Java 每条失败都是 `"<Vendor> API <operation> failed with status <code>: <body>"`，Rust `MemError::HttpStatus` 只有 `status` + `body`，Display 为 `"memory API request failed with status …"`，丢失厂商与操作名（add / search request / summary_personal_memory） | `mem0/Mem0Client.java#executePostRaw`、`bailian/BailianMemoryClient.java#executePost`、`reme/ReMeClient.java#executePost` | `src/error.rs:27-32`（无 vendor/operation 字段）；三个 client 的 `post()` 直接透传：`src/mem0/client.rs:192-196`、`src/bailian/client.rs:130-135`、`src/reme/client.rs:109-114` | 已完成 | 三处 `post()` 用 `map_err` 包装为「带 operation 的 `HttpStatus`」（或给 `HttpStatus` 增 `operation: &'static str`）；验收：新增断言 `401` 文案含 `Mem0 API add request failed with status 401` 语义（三厂商各 1 条） 【2026-10-31 落地：已完成——`MemError::HttpStatus` 增 `vendor`/`operation`（`&'static str`，Display `{vendor} API {operation} failed with status {status}: {body}`），新增 `MemError::http_status()`（transport seam 用 `memory`/`request` 占位，文案不变）与 `MemError::with_context()`；三厂商 `post()` 统一 `map_err` 包装：Mem0 `add request`/`search request`、Bailian `add memory`/`search memory`、ReMe `summary_personal_memory`/`retrieve_personal_memory`（`src/error.rs:44-100`、`src/mem0/client.rs:192-201`、`src/bailian/client.rs:139-151`、`src/reme/client.rs:104-115`）。测试：`mem0::client::tests::http_failures_name_the_vendor_and_the_operation`、`bailian::client::tests::http_failures_name_the_vendor_and_the_operation`、`reme::client::tests::http_failures_name_the_vendor_and_the_operation` + `error::tests::with_context_adds_the_vendor_and_operation_like_java`】 |
| MEM-03 | P2 | ReMe 适配器 `userId` 校验**文案**与 Java 不一致 | `reme/ReMeLongTermMemory.java#ReMeLongTermMemory`（`"userId is required"`） | `src/reme/memory.rs:83-90` 返回 `"userId cannot be null or blank"`（与 Bailian 文案串了）；测试 `new_rejects_blank_user_id` 只断言 `MemError::Config` | 已完成 | 改为 `"userId is required"`（与 Java 逐字对齐），断言文案；保留「全空白也拒绝」的更严行为并记入 §4.3.2 MEM-X03 【2026-10-31 落地：已完成——文案改 `userId is required`（`src/reme/memory.rs:80-92`），`reme::memory::tests::new_rejects_blank_user_id` 断言完整 Display（空串/空格/制表符三种）】 |
| MEM-04 | P2 | `is_recordable`（Bailian / ReMe）语义**窄于** Java `record` 的过滤谓词：它只判角色与 `ToolUseBlock`，不判空白文本与 `<compressed_history>`；`convert_messages` 在外层另判，但公开 helper 单独使用时会把空白/压缩消息判为「可记录」 | `bailian/BailianLongTermMemory.java#record`（单一谓词：角色 + ToolUse + `isBlank` + 压缩标记）、`reme/ReMeLongTermMemory.java#record`（同上，`isEmpty`） | `src/bailian/memory.rs:36-43`、`src/reme/memory.rs:32-39`；测试 `is_recordable_rejects_tool_use_assistant_and_system` / `is_recordable_rejects_tool_use_assistant` 均未覆盖空白文本 | 已完成 | 把文本/压缩标记检查并入 `is_recordable`（与 Java 谓词 1:1，`convert_messages` 去掉重复判断），或改名为 `is_role_recordable` 并在 rustdoc 写明「不含文本过滤」；验收：`is_recordable(&Msg::user(""))` 的行为在测试中被固定 【2026-11-01 Round 2 落地：已完成——`is_recordable` 现携带 Java 的**完整**单一谓词（角色 + ToolUse + 文本 + 压缩标记），`convert_messages` 只保留 `filter(is_recordable)` 不再重复判断；Bailian 用 `trim()`（Java `isBlank`）、ReMe 用 `is_empty()`（Java `isEmpty`）。测试：`bailian::memory::tests::is_recordable_rejects_blank_text_and_compressed_history`、`reme::memory::tests::is_recordable_rejects_empty_text_and_compressed_history`（固定 `is_recordable(&Msg::user("")) == false`，并区分空白串在两厂商下的不同结果）】 |
| MEM-05 | P2 | 重试装饰器对**所有 POST 生效**，含非幂等的 `add`（Mem0 add 会重新抽取、ReMe `/summary_personal_memory` 会重复写入；Java 完全不重试）。默认 `RetryPolicy::none()` 安全，但宿主一旦开启即无 per-call 关闭手段，也无警示 | `mem0/Mem0Client.java#add`、`reme/ReMeClient.java#add`（无重试，失败即透传） | `src/retry.rs:103-160`（`RetryTransport::post_json` 不区分操作/方法）；`src/mem0/client.rs:198-205`、`src/reme/client.rs:123-128` 与 `search` 共用同一 transport | 已完成 | 在 `retry.rs` / `lib.rs` rustdoc 明示「重试可能造成重复写入，add 请评估幂等性」，并补一条 `add` 在 `max_attempts>1` 下的行为用例（写明会重发） 【2026-11-01 Round 4 落地：已完成——①`src/retry.rs` 新增模块文档「## Non-idempotent `add`」（POST 无幂等提示 → 重发所有 POST；Mem0 重新抽取 / ReMe 重新摘要 / Bailian 追加；`RetryPolicy::default` 单次因此安全；无 per-call 开关 → 同 base URL 两 client：带策略给 `search`、`none()` 给 `add`），`RetryPolicy` 与 `RetryTransport::new` rustdoc 交叉引用；②`RetryTransport::new` 在 `max_attempts > 1` 时 `tracing::warn!`（构造期一次，默认策略零输出）；③`src/lib.rs`「Resilience and observability」补同义段落。用例：`mem0::client::tests::add_is_re_sent_verbatim_when_retries_are_enabled`（`max_attempts=2` + 先 503 后成功 → 2 次请求、同 URL、同 body，固定「会重发」） |
| MEM-06 | P2 | 测试密度不均：`BailianMemoryClient`（3 例）与 `ReMeClient`（6 例）缺「非 2xx 状态」与「非法 JSON」两类用例，而 `Mem0Client` 已有 4 条同类（`add_propagates_http_status_and_body` / `invalid_json_surfaces_as_a_json_error` / `search_rejects_empty_body` 等） | `bailian/BailianMemoryClientTest.java`、`reme/ReMeClientTest.java`（`MockWebServer` 覆盖状态码/坏 JSON） | `src/bailian/client.rs:163-190`、`src/reme/client.rs:153-215`（无状态码/坏 JSON 用例）；错误路径仅由适配器层 `record_lossy`/`retrieve_lossy` 间接覆盖（`src/bailian/memory.rs:524-560`、`src/reme/memory.rs:319-345`） | 已完成 | 两个 client 各补 `add_propagates_http_status` 与 `invalid_json_surfaces_as_a_json_error`；验收：`cargo test -p agentscope-extensions-mem` 新增 ≥4 例且离线 【2026-11-01 Round 2 落地：已完成——Bailian 增 `add_propagates_http_status_and_body` / `invalid_json_surfaces_as_a_json_error` / `empty_response_body_is_a_transport_error`，ReMe 增 `add_propagates_http_status_and_body` / `invalid_json_surfaces_as_a_json_error`（共 5 例，全部 `MockTransport` 离线）；`add` 的状态用例断言 `MemError::HttpStatus{vendor,operation,status,body}` 结构体字段而非仅 Display；Bailian 空响应体为 `Transport` 错误、ReMe 空体回落 `{}` 的既有差异各有用例固定】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| MEM-07 | Mem0 端点与两套鉴权：Platform `/v1/memories/` + `/v2/memories/search/` 用 `Authorization: Token`，SelfHosted `/memories` + `/search` 用 `X-API-Key`，空 key 不发送鉴权头 | `mem0/Mem0Client.java#Mem0Client`（端点常量）、`#executePostRaw`（header 分支） | `src/mem0/client.rs:16-22`（常量）、`177-190`（`headers()`）；测试 `self_hosted_uses_self_hosted_endpoints` / `platform_auth_uses_token_scheme` / `self_hosted_auth_uses_x_api_key` / `blank_api_key_omits_authorization_header` | 已完成 |
| MEM-08 | Mem0 搜索双响应形状（v1.0 裸数组、v1.1 `{"results":[…]}`）与空体处理 | `mem0/Mem0Client.java#search` | `src/mem0/client.rs:212-230`；测试 `retrieve_handles_array_and_object_search_shapes` / `search_rejects_empty_body` | 已完成 |
| MEM-09 | Mem0 `Mem0AddRequest` 20 字段 + Java 无参默认（`infer=true` / `output_format="v1.1"` / `async_mode=true` / `immutable=false`）+ `NON_NULL` 省略 | `mem0/Mem0AddRequest.java#Builder`（字段默认值区） | `src/mem0/dto.rs:96-200`、`147-157`（`new()` 默认）；测试 `add_request_carries_java_defaults` / `add_request_builder_covers_every_java_setter` / `add_request_serializes_snake_case_identifiers` | 已完成 |
| MEM-10 | Mem0 `Mem0SearchRequest` 13 字段 + 默认 `version="v2"` / `top_k=10` + `filters` 恒序列化（`@JsonInclude(ALWAYS)`）+ `user_id` 镜像进 filters | `mem0/Mem0SearchRequest.java#Builder`、`#setUserId` | `src/mem0/dto.rs:305-420`；测试 `search_request_has_java_defaults_and_always_emits_filters` / `search_request_mirrors_identifiers_into_filters` / `search_request_builder_covers_every_java_setter` | 已完成 |
| MEM-11 | Mem0 消息转换（User/System→`user`、Assistant/Tool→`assistant`）、滤空文本与 `<compressed_history>`、不外发 `name`、`top_k=5`、metadata 合并进 filters（后写覆盖，与 Java `putAll` 同序） | `mem0/Mem0LongTermMemory.java#record` / `#retrieve` / `#buildSearchRequest` / `#convertToMem0Message` | `src/mem0/memory.rs:16-70`、`109-160`；测试 `role_mapping_matches_java` / `convert_message_matches_java_and_omits_name` / `record_posts_all_configured_identifiers_without_a_name_key` / `build_search_request_uses_top_k_5_and_merges_config` | 已完成 |
| MEM-12 | Bailian 端点与默认 base URL、`Authorization: Bearer`、空响应体报错 | `bailian/BailianMemoryClient.java#BailianMemoryClient`（`DEFAULT_BASE_URL`/`ADD_MEMORY_ENDPOINT`）、`#executePost` | `src/bailian/client.rs:15-19`、`120-142`；测试 `default_construction_defaults_the_base_url_and_installs_metrics` / `record_posts_to_add_endpoint_with_bearer_auth` | 已完成 |
| MEM-13 | Bailian 检索参数：`top_k=10` / `min_score=0.3` / `enable_rerank`、`enable_judge`、`enable_rewrite` 默认 `false` / 单条 `user` 查询消息 / `project_id` 转 `project_ids=[..]` | `bailian/BailianLongTermMemory.java#retrieve`、`#Builder`（`DEFAULT_TOP_K`/`DEFAULT_MIN_SCORE`） | `src/bailian/memory.rs:18-20`、`201-219`；测试 `build_search_request_uses_config_values` / `config_effective_defaults` | 已完成 |
| MEM-14 | Bailian 记录过滤（仅 User 与无 `ToolUseBlock` 的 Assistant；剔空白与 `<compressed_history>`）、`meta_data` 键名、空可选字段不序列化 | `bailian/BailianLongTermMemory.java#record`、`bailian/BailianAddRequest.java#Builder`（`@JsonProperty("meta_data")`） | `src/bailian/memory.rs:36-70`、`179-199`；测试 `convert_messages_applies_bailian_filters` / `add_request_uses_meta_data_key_not_metadata` / `build_add_request_skips_blank_optional_fields` | 已完成 |
| MEM-15 | ReMe 端点 `/summary_personal_memory` + `/retrieve_personal_memory`、无鉴权头、`userId`→`workspace_id`、`top_k=5`、`answer` 优先否则拼 `memory_list[].content` | `reme/ReMeClient.java#add` / `#search`、`reme/ReMeLongTermMemory.java#record` / `#retrieve` | `src/reme/client.rs:13-15`、`105-107`；`src/reme/memory.rs:65-74`、`102-140`；测试 `build_requests_use_user_id_as_workspace` / `render_prefers_answer_then_joins_memories` / `search_request_defaults_top_k_to_five` | 已完成 |
| MEM-16 | ReMe 空响应体回落 `{}`（Java 对 null body 与 blank body 均 `fromJson("{}")`） | `reme/ReMeClient.java#executePost` | `src/reme/client.rs:109-120`、`139-149`（`parse_or_default`）；测试 `blank_response_body_falls_back_to_default` | 已完成 |
| MEM-17 | 构造期校验与文案：Mem0 至少一个标识符（`"at least one of agentName, userName, or runName must be provided"`）、Mem0/ReMe `"apiBaseUrl is required"`、Bailian `"apiKey cannot be null or blank"` / `"apiBaseUrl cannot be null or blank"`、Bailian 适配器复检 `api_key`/`user_id` | `mem0/Mem0LongTermMemory.java#Mem0LongTermMemory`、`reme/ReMeLongTermMemory.java#ReMeLongTermMemory`、`bailian/BailianMemoryClient.java#BailianMemoryClient`、`bailian/BailianLongTermMemory.java#BailianLongTermMemory` | `src/mem0/client.rs:117-140`、`src/mem0/memory.rs:89-98`、`src/bailian/client.rs:81-105`、`src/bailian/memory.rs:155-172`、`src/reme/client.rs:73-95`；测试 `blank_base_url_is_rejected_at_construction` / `new_requires_at_least_one_identifier` / `new_rejects_blank_api_key` / `new_rejects_blank_user_id` | 已完成 |
| MEM-18 | 超时分层：connect 30s（Java `connectTimeout(30s)`）、per-request = Java `readTimeout` 默认 60s | `mem0/Mem0Client.java#Mem0Client`、`bailian/BailianMemoryClient.java`、`reme/ReMeClient.java#ReMeClient` | `src/http.rs:36`（`DEFAULT_CONNECT_TIMEOUT`）、`52-60`（`ReqwestTransport::new`）；测试 `connect_timeout_matches_java_thirty_seconds` / `reqwest_transport_builds` | 已完成 |
| MEM-19 | 吞错双路径：默认严格 + `*_lossy` 复刻 `onErrorReturn("")` / `onErrorComplete()` | `mem0/Mem0LongTermMemory.java#retrieve`、`bailian/BailianLongTermMemory.java#record` / `#retrieve`、`reme/ReMeLongTermMemory.java#retrieve` | `src/lib.rs:36-58`（语义表）；`src/mem0/memory.rs:196-205`、`src/bailian/memory.rs:255-277`、`src/reme/memory.rs:147-157`；测试 `retrieve_lossy_returns_empty_on_http_status_error` / `record_lossy_swallows_http_errors` / `record_surfaces_http_errors_like_java` / `retrieve_lossy_passes_through_memory_list` | 已完成 |
| MEM-20 | Java 侧本就**没有** delete / clear / update 记忆端点、无分页游标、无异步任务状态轮询（`async_mode` 与 ReMe 的 `update_result` 是服务端行为，客户端只解析响应）→ Rust 无需对应实现 | `mem0/Mem0Client.java`、`bailian/BailianMemoryClient.java`、`reme/ReMeClient.java`（记忆操作只有 `add` + `search`，另有 `shutdown`/`close`） | Java 主代码逐词检索 `delete` / `clear(` / `update` / `pagination` / `page_size` / `cursor`：仅命中 `reme/ReMeAddResponse.java` 的 `deleted_memory_ids` / `update_result`（响应字段）；Rust 侧同组词（含 `update_memory`）在 `src` 零命中（口径一致） | 已完成 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：MEM 三厂商（Mem0 / Bailian / ReMe）长期记忆适配全部迁移；DTO serde 对齐；消息转换；检索结果映射；可注入 HTTP seam；重试/退避与指标/span；**121 个离线单元测试 + 3 个环境门控集成测试**（2026-10-31，明细见 history/MEM__MIGRATION_HISTORY.md）。
- **2026-10-31 收口轮（本轮）**：
  1. **吞错口径正式裁定**：默认 API 保持严格（`record`/`retrieve -> Result`），Java 等价吞错语义显式命名 `*_lossy`，映射表固化在 `lib.rs`「Error semantics」与 §4.2 第 4 条。
  2. **测试密度**：新增 25 个用例（96 → 121），补 Builder 逐字段断言、HTTP 状态/非法 JSON 透传、`user_id`↔`filters` 镜像、可选标识符上线、严格 vs 吞错双路径；「Java 测试类 → Rust 用例」映射表见 history/MEM__MIGRATION_HISTORY.md。
  3. **P2 全清**：新增 `src/retry.rs`（`RetryPolicy` + `RetryTransport`）与 `src/telemetry.rs`（`MemMetrics` + `InstrumentedTransport`），三 client 默认接线并暴露 `metrics()`；删除当时未引用的 `futures` / `agentscope-harness` 依赖（**同日晚些的 MEM-01 使 `futures` 回归为运行时依赖、`agentscope-harness` 回归为 dev-dependency，见 §4.2 第 2 条**）；补齐 `Mem0AddRequest` 14 个与 `Mem0SearchRequest` 8 个 `with_*`；Mem0 报文去掉 `name`；`BailianLongTermMemoryConfig.api_key` 改为在 `new` 中校验；`Mem0SearchRequest.user_id` 改私有 + `set_user_id` + `sync_filters()`。
  4. **真实端点**：以环境变量门控的 `tests/live_endpoints.rs` 替代凭据依赖（不用 `#[ignore]`），见 §3.2 与 §4.2 第 6 条。
  - **验证状态**：本轮子智能体按 loop 约定**未运行 cargo**（静态计数 `#[test]`/`#[tokio::test]` = 121 + 3 集成），由主智能体统一 `cargo test -p agentscope-extensions-mem` 复核；上一轮（2026-10-28）实测为 **96 passed / 0 failed**。
- **2026-10-31 复核批次（MEM-01/02/03）**：单测静态计数 **121 → 131**；新增集成目标 `tests/long_term_memory_wiring_test.rs`（1 例，需 dev-dep `agentscope-harness`）。验证命令（由主智能体执行）：
  ```powershell
  cargo test -p agentscope-extensions-mem -j 2 --no-fail-fast
  cargo test -p agentscope-extensions-mem --test long_term_memory_wiring_test   # 单跑 MEM-01 验收用例
  ```
  反向验证建议（未运行）：把 `MemLongTermMemoryMiddleware::on_agent` 的 `record_lossy` 调用或 `on_reasoning` 的 `inject` 去掉，对应用例应 FAILED（search/add 计数或 `long_term_memory` 消息断言先失败）。
- **2026-11-01 Round 2（MEM-04 / MEM-06）**：
  1. **MEM-04**：`is_recordable`（Bailian / ReMe）现携带 Java `record` 的**完整单一谓词**（角色 + 无 `ToolUseBlock` + 文本非空 + 无 `<compressed_history>`），`convert_messages` 只留 `filter(is_recordable)`，消除了「公开 helper 单独使用时把空白/压缩消息判为可记录」的语义窄化；Bailian 用 `trim()`（Java `isBlank`）、ReMe 用 `is_empty()`（Java `isEmpty`）的差异被测试固定。
  2. **MEM-06**：Bailian 增 3 例（`add_propagates_http_status_and_body` / `invalid_json_surfaces_as_a_json_error` / `empty_response_body_is_a_transport_error`）、ReMe 增 2 例（`add_propagates_http_status_and_body` / `invalid_json_surfaces_as_a_json_error`），全部 `MockTransport` 离线；状态码用例断言 `MemError::HttpStatus` 的结构体字段（vendor/operation/status/body）。
  - **实测**：`cargo build -p agentscope-extensions-mem -j 2` → EXIT=0；`cargo test -p agentscope-extensions-mem -j 2 --no-fail-fast` → **138 + 3 + 1 passed / 0 failed / EXIT=0**（lib 138 / `live_endpoints` 3 / `long_term_memory_wiring_test` 1）。反向验证：把 ReMe `is_recordable` 的文本/压缩标记判断回退为 `true` → `is_recordable_rejects_empty_text_and_compressed_history` 与 `convert_messages_applies_reme_filters` **FAILED**（EXIT=101，11 passed / 2 failed）→ 还原后全绿。
  - **剩余**（Round 2 时点）：`MEM-05`（非幂等 `add` 的重试警示，P2）——已于下面 Round 4 关闭。
- **2026-11-01 Round 4（MEM-05）**：
  1. **风险明示**：`src/retry.rs` 新增「## Non-idempotent `add`」模块文档段（`post_json` 无幂等提示 → 装饰器重发**所有** POST；Mem0 `add` 重新抽取、ReMe `/summary_personal_memory` 重新摘要、Bailian `add` 追加记录；`RetryPolicy::default` 单次尝试因此安全），`RetryPolicy` / `RetryTransport::new` rustdoc 交叉引用；`src/lib.rs`「Resilience and observability」补同义段落，并写明「本 seam 无 per-call 开关 → 想只重试读，就在同一 base URL 上建两个 client：带策略的给 `search`、`RetryPolicy::none()` 的给 `add`」。
  2. **构造期警示**：`RetryTransport::new` 在 `policy.max_attempts > 1` 时 `tracing::warn!`（带 `max_attempts` 字段）——宿主一开启重试即可在日志看到重复写入风险；默认策略零额外输出（不改变既有用例语义）。
  3. **行为用例**：`mem0::client::tests::add_is_re_sent_verbatim_when_retries_are_enabled`（`RetryPolicy::new(2, …)` + `MockTransport` 先 503 后成功 → 断言 `add` 被**原样重发**：2 次请求、同 URL、同 body），把「重试会重复写入」固定为被测性质而非隐患。
  - **实测**：`cargo build -p agentscope-extensions-mem -j 2` → EXIT=0（本 crate 0 warning）；`cargo test -p agentscope-extensions-mem -j 2 --no-fail-fast` → **139 + 3 + 1 passed / 0 failed / EXIT=0**（lib 139 / `live_endpoints` 3 / `long_term_memory_wiring_test` 1）。反向验证：把 `RetryTransport::post_json` 的 `max_attempts` 临时硬编码为 `1`（等于关掉重试）→ `add_is_re_sent_verbatim_when_retries_are_enabled` **FAILED**（EXIT=101，0 passed / 1 failed）→ 还原后全绿。
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
2. **【跨模块需求】无硬阻塞。** 本 crate 运行时依赖 `agentscope-core`（`Msg`/`MsgRole`/`JsonMap`/`ContentBlock`/`TextBlock`/`ToolUseBlock`、`message::msg::ContentBlockKind` 及 `middleware::{Middleware, AgentInput, ReasoningInput, EventStream, Next, ExtensionPoint}`）与 `reqwest`/`serde`/`tokio`/`tracing`/`chrono`/`async-trait`/`thiserror`/`futures`（`futures` 于 2026-10-31 因 MEM-01 重新引入：中间件要构造 `EventStream`）；`agentscope-harness` 仅为 **dev-dependency**（MEM-01 验收用例必须走真实 `builder().middleware()` 装配入口），生产 crate 仍与 harness 无关，依赖无循环（harness 不依赖本 crate），亦不改 harness 源码。
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
   - **非幂等 `add`（MEM-05，2026-11-01 关闭）**：`post_json` 不携带幂等提示，故重试对所有 POST 生效。**不**为 `add` 单独关掉重试（那会让宿主显式选择的策略对写请求**静默失效**），改为三件套：文档明示风险、`RetryTransport::new` 在 `max_attempts > 1` 时 `warn!`、用例固定「会原样重发」。只想重试读的宿主：用 `with_transport` 在同一 base URL 上建两个 client（带策略的给 `search`、`RetryPolicy::none()` 的给 `add`）。
6. **【设计决定 2026-10-31】真实端点联调不纳入自动化。** 依据：仓库无厂商凭据 / CI 禁止外网，无法在离线环境证明厂商侧行为。替代：`tests/live_endpoints.rs` 环境变量门控（`MEM0_LIVE_BASE_URL` / `BAILIAN_LIVE_API_KEY` / `REME_LIVE_BASE_URL`），未设变量即直接返回；wire 契约由 mock 全链路覆盖。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本节由 2026-10-31 全量对齐复核归并。分两类：4.3.1 = Java 有、Rust 有意不迁；4.3.2 = Rust 已实现但与 Java 行为/形态不同（修复、加固或架构差异）。与 §3.5 缺口（MEM-01…MEM-06）区分：本节不产生待办。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| MEM-D01 | `LongTermMemory` 体系：接口 + `StaticLongTermMemoryHook` + `LongTermMemoryMode` + `LongTermMemoryTools` | `agentscope-core/src/main/java/io/agentscope/core/memory/LongTermMemory.java#LongTermMemory`、`memory/StaticLongTermMemoryHook.java#StaticLongTermMemoryHook`、`memory/LongTermMemoryMode.java#LongTermMemoryMode`、`memory/LongTermMemoryTools.java#LongTermMemoryTools` | 四个类型全部 `@Deprecated(forRemoval = true, since = "2.0.0")`；`StaticLongTermMemoryHook` 类注释明写「Long-term memory and the Hook system are removed; reimplement as a MiddlewareBase if required」；`ReActAgent.Builder.longTermMemory/longTermMemoryMode/longTermMemoryAsyncRecord` 同样是 `@Deprecated(forRemoval = true, since = "2.0.0")`。Rust 不复活废弃体系（`crates/agentscope-extensions-mem/src/lib.rs:11-25`） | 适配器缺框架装配点 → 该「不迁移」是否成立取决于 MEM-01 裁定（若采纳 Middleware 形态，仍不迁 `LongTermMemory` 接口本体） |
| MEM-D02 | core `Memory` trait 适配（用 `add_message`/`get_messages` 等承载长期记忆） | 无直接对应：Java `Memory` 为会话缓冲，长期记忆是独立接口 `LongTermMemory` | Rust `crates/agentscope-core/src/memory/mod.rs:14-40` 的 6 个方法（`add_message`/`get_messages`/`delete_message`/`clear`/`save_to`/`load_from`）全是**缓冲增删 + 会话持久化**；`record(&[Msg])` / `retrieve(&Msg) -> String` 是「按语义检索注入」，实现该 trait 会把检索命中伪装成会话缓冲消息（语义错配） | 长期记忆改以固有方法暴露（见 MEM-X01）；若汇总方要求统一装配，须新增独立 wrapper 而非让厂商适配器实现该 trait（§4.2 第 1 条） |
| MEM-D03 | OkHttp `writeTimeout(30s)`、`shutdown()` / `close()` / `AutoCloseable` | `mem0/Mem0Client.java#shutdown`、`reme/ReMeClient.java#shutdown`、`bailian/BailianMemoryClient.java#close`、`bailian/BailianLongTermMemory.java#close` | reqwest 不暴露独立写超时（per-request `timeout` 覆盖含请求体写入的整次请求）；`reqwest::Client` 无 `close`，由 Drop 释放连接池。`rg` 逐词检索 `fn close` 与 `fn shutdown` 在 `crates/agentscope-extensions-mem/src` 均零命中 | 宿主无需显式释放；长连接由 reqwest 连接池管理 |
| MEM-D04 | `Mem0SearchRequest.Builder.limit(Integer)` | `mem0/Mem0SearchRequest.java#Builder.limit` | Java 自身标 `@Deprecated`（语义等于 `topK`），Rust 仅保留 `with_top_k`（`src/mem0/dto.rs:375-380`） | 无（调用方改 `with_top_k`） |
| MEM-D05 | Jackson `JsonCodec`/`JsonUtils`、Reactor `Schedulers.boundedElastic()`、测试用 `MockWebServer` | `mem0/Mem0Client.java#executePostRaw`、各 `*ClientTest.java` | 机制替换而非能力缺失：serde/serde_json（`#[serde(rename/skip_serializing_if)]`，`src/mem0/dto.rs` 全量）、tokio 异步、`http::mock::MockTransport`（`src/http.rs:96-160`）；Java bean 样板断言（getter/setter/`toString`）按 §3.2 口径一句关闭 | 无（语义用例已在 121 个离线测试中覆盖） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| MEM-X01 | 长期记忆 API 形态 | 各适配器**固有**方法 `record(&[Msg]) -> Result<()>` / `retrieve(&Msg) -> Result<String>`，另有 `record_lossy` / `retrieve_lossy`；Mem0 额外 `retrieve_msgs(&Msg) -> Result<Vec<Msg>>` | Java 实现 `LongTermMemory` 接口并返回 Reactor `Mono<Void>` / `Mono<String>`；Rust 不实现 core `Memory`、不重建 `LongTermMemory` | 避免与 core 活跃 trait 语义错配（`src/lib.rs:11-25`、`crates/agentscope-core/src/memory/mod.rs:14-40`）；`retrieve_msgs` 让「想以 `Msg` 消费检索结果」的调用方无需新 trait（`src/mem0/memory.rs:207-225`、命中→assistant 记忆注记 + metadata `memory_id`/`score`/`categories`/`memory_source`，`src/mem0/dto.rs:560-591`） |
| MEM-X02 | 错误语义：严格默认 + 显式 `*_lossy` | 无后缀 `record`/`retrieve` 恒返回 `Result`；`*_lossy` 复刻 Java 吞错（`tracing::warn!` + 空串/`()`） | Java `retrieve` 恒 `.onErrorReturn("")`、Bailian `record` `.onErrorComplete()`，错误默认不可见 | 失败默认可观测；移植 Java 调用点只需把 `retrieve`→`retrieve_lossy`、`record`→`record_lossy`（`src/lib.rs:36-58` 语义表；测试 `retrieve_lossy_returns_empty_on_http_status_error` / `record_lossy_swallows_http_errors`） |
| MEM-X03 | 输入校验更严 | base URL / `userId` 除空串外，**全空白**也拒绝（`trim().is_empty()`）；base URL 先 `trim()` | Java Mem0/ReMe 只判 `isEmpty`（空白串可通过），Bailian 用 `isBlank` | 避免拼出 host-less 请求 URL（`src/mem0/client.rs:117-140`、`src/reme/client.rs:73-95`、`src/reme/memory.rs:83-90`）；校验文案保持 Java 原文（ReMe `userId` 已与 Java 逐字对齐，MEM-03 已关闭） |
| MEM-X04 | 错误类型化 | `MemError::{Config, Transport, HttpStatus{vendor, operation, status, body}, Json}`（`thiserror`） | Java 统一 `IOException` + 文案内嵌 vendor/operation | 调用方可按类分支，`RetryPolicy::is_retryable` 依赖该分类（`src/error.rs:30-100`、`src/retry.rs:90-101`）；vendor/operation 已是 `HttpStatus` 的字段，Display 与 Java 同形（MEM-02 已关闭） |
| MEM-X05 | 传输装饰器：重试 + 指标 | `RetryPolicy`/`RetryTransport`（仅重试 `Transport`/`408`/`429`/`5xx`）+ `MemMetrics`/`InstrumentedTransport`（`mem.http.post` span、requests/failures/latency 计数）；三 client 的 reqwest 构造器固定链 `Retry(Instrumented(Reqwest))` 并暴露 `metrics()` | Java 无重试、无指标（仅 slf4j `log.warn/debug`）；默认行为由 `RetryPolicy::default() == none()` 保持单次尝试 | 生产可观测、可选退避；重试对非幂等 `add` 的风险已于 MEM-05 关闭（2026-11-01：`src/retry.rs` 模块文档「Non-idempotent `add`」+ `RetryTransport::new` 构造期 `warn!` + 用例 `mem0::client::tests::add_is_re_sent_verbatim_when_retries_are_enabled`；`src/retry.rs:1-125`、`src/telemetry.rs:35-90`、`src/mem0/client.rs:85-105`） |
| MEM-X06 | Bailian base URL 归一化与超时可配 | `with_transport` 对 base URL `trim()` + 剥单个尾 `/`；`with_timeout`/`with_timeout_and_policy` 提供 per-request 超时（默认 60s） | Java `BailianMemoryClient` 既不 trim 也不剥尾 `/`（`"https://x/"` 会拼出 `//api/v2/...`），且无超时参数（用 core `HttpTransport` 默认） | 避免双斜杠 404；超时可配（`src/bailian/client.rs:47-118`） |
| MEM-X07 | Mem0 租户过滤的**类型层**不变量 | `Mem0SearchRequest.user_id` 为私有字段，只经 `with_user_id`/`set_user_id`（同时写 `filters`）与 `sync_filters()` 写入；`Mem0Client::search` 序列化前无条件 `sync_filters()` | Java 靠 `setUserId` 约定同步 `filters`，字段仍可被直接赋值绕过 | 反序列化或字段赋值都不可能丢 `user_id` 过滤，避免跨租户串数据（`src/mem0/dto.rs:330-420`、`src/mem0/client.rs:212-216`；测试 `search_request_keeps_user_id_mirrored_into_filters` / `search_wire_payload_always_carries_the_user_id_filter`） |
| MEM-X08 | 空响应体的错误分类 | Mem0 search 空体 → `MemError::Transport("Mem0 API returned empty response")`；Bailian 空体 → `Transport("Bailian API returned empty response")`；ReMe 空体 → `{}`（复用默认响应） | Java：Mem0 空体走 JSON 解析异常；Bailian 抛 `IOException("... returned empty response")`；ReMe 回落 `{}` | 把「厂商返回空」与「报文格式错误」分开，便于重试策略与告警（`src/mem0/client.rs:217-228`、`src/bailian/client.rs:130-141`、`src/reme/client.rs:109-120`） |
| MEM-X09 | 长期记忆装配形态（MEM-01 落地后的差异） | `MemLongTermMemoryMiddleware` + `LongTermMemoryBackend` 窄 seam（`src/middleware.rs`）：检索在 `on_reasoning`、记录在 `on_agent` 尾部（读 `RuntimeContext::state` 的 `AgentState.context`） | ①Java 在 `PreCallEvent` 检索并改写 `inputMessages`（该消息会被写进会话历史）；Rust 在 `on_reasoning` 注入且**不持久化**（请求级）。②Java 每轮都带该消息；Rust 仅在每轮首个 iteration 注入（避免在 tool 结果后再追加一条 USER，严格 provider 会拒）。③Java 可选 `asyncRecord`（bounded scheduler fire-and-forget）；Rust 只做同步记录（此时回复已产出）。④Java 吞错在 hook 的 `onErrorResume`；Rust 在 adapter 的 `*_lossy` + 中间件层 warn | ①避免把检索结果污染成对话历史（与 core 的“system prompt 不落盘”口径一致）；②保持每轮只 1 次检索请求（`run_id` 缓存）且报文始终 provider-valid；③不引入后台任务/drop 不可观测；④失败不中断回合（`tests/long_term_memory_wiring_test.rs` 走真实 `builder().middleware(chain)`） |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-mem` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-mem` / `cargo test -p agentscope-extensions-mem` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-mem/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。
- **例外（自 2026-10-31）**：本 crate `Cargo.toml` 可声明 `agentscope-harness` 为 **dev-dependency**（MEM-01 验收要求走真实装配入口），仍不得改 harness 源码，也不得把 harness 升为运行时依赖。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/MEM__MIGRATION_HISTORY.md`](./history/MEM__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
