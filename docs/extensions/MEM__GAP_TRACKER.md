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
| Java 规模（初测） | 主代码 ~4821 行 / 测试 ~5746 行 |
| Rust crate | `crates/agentscope-extensions-mem` |
| 迁移波次 | A |
| 当前状态 | **已完成**（build + test 全绿，66 passed / 0 failed） |

### 1.1 Rust crate 文件清单（15 个源文件，~2100 行）
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
| `mem0.Mem0Message` | 消息 DTO | `mem0::dto::Mem0Message` | 字段 `role`/`content`/`name`；`name` 为 `NON_NULL` |
| `mem0.Mem0ApiType` | 部署类型枚举 | `mem0::dto::Mem0ApiType` | `Platform`/`SelfHosted`，`from_string` 大小写不敏感 |
| `mem0.Mem0AddRequest` | 添加请求 | `mem0::dto::Mem0AddRequest` | Java 无参构造默认值：`infer=true`/`output_format=v1.1`/`async_mode=true`/`immutable=false` |
| `mem0.Mem0AddResponse` | 添加响应 | `mem0::dto::Mem0AddResponse` | `results: Vec<JsonMap>`（异构对象）+ `message` |
| `mem0.Mem0SearchRequest` | 搜索请求 | `mem0::dto::Mem0SearchRequest` | `filters` 始终序列化（`ALWAYS`）；`user_id` 同时镜像进 `filters` |
| `mem0.Mem0SearchResponse` | 搜索响应 | `mem0::dto::Mem0SearchResponse` | `results: Option<Vec<Mem0SearchResult>>` |
| `mem0.Mem0SearchResult` | 单条结果（v2） | `mem0::dto::Mem0SearchResult` | `to_msg()` 为 Rust 侧扩展，映射回 core `Msg` |
| `mem0.Mem0Client` | HTTP 客户端 | `mem0::client::Mem0Client` | 端点常量 + 鉴权头（Platform `Authorization: Token` / SelfHosted `X-API-Key`） |
| `mem0.Mem0LongTermMemory` | 适配器 | `mem0::memory::Mem0LongTermMemory` | `record`/`retrieve` + `retrieve_msgs`；`top_k` 默认 5 |

### 2.2 Java 类 → Rust 对应（memory-bailian 子模块，8 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `bailian.BailianMessage` | 消息 DTO | `bailian::dto::BailianMessage` | — |
| `bailian.BailianAddRequest` | 添加请求 | `bailian::dto::BailianAddRequest` | JSON 字段 `meta_data`（非 `metadata`） |
| `bailian.BailianAddResponse` | 添加响应 | `bailian::dto::BailianAddResponse` | `request_id` + `memory_nodes` |
| `bailian.BailianSearchRequest` | 搜索请求 | `bailian::dto::BailianSearchRequest` | `top_k`/`min_score`/`enable_*` |
| `bailian.BailianSearchResponse` | 搜索响应 | `bailian::dto::BailianSearchResponse` | — |
| `bailian.BailianMemoryNode` | 记忆节点 | `bailian::dto::BailianMemoryNode` | `memory_node_id`/`old_content` 等 snake_case 覆盖 |
| `bailian.BailianMemoryClient` | HTTP 客户端 | `bailian::client::BailianMemoryClient` | `Authorization: Bearer`；key 空白即拒 |
| `bailian.BailianLongTermMemory` | 适配器 | `bailian::memory::BailianLongTermMemory` | `top_k` 默认 10、`min_score` 默认 0.3；丢弃 System/Tool 与 tool-use Assistant |

### 2.3 Java 类 → Rust 对应（reme 子模块，7 类）
| Java 类 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `reme.ReMeMessage` | 消息 DTO | `reme::dto::ReMeMessage` | — |
| `reme.ReMeTrajectory` | 轨迹（消息序列） | `reme::dto::ReMeTrajectory` | — |
| `reme.ReMeAddRequest` | 添加请求 | `reme::dto::ReMeAddRequest` | `workspace_id` + `trajectories` |
| `reme.ReMeAddResponse` | 添加响应 | `reme::dto::ReMeAddResponse` | `answer`/`success`/`metadata`（含 `ReMeAddMetadata`/`ReMeUpdateResult`） |
| `reme.ReMeSearchRequest` | 搜索请求 | `reme::dto::ReMeSearchRequest` | `top_k` 默认 5 |
| `reme.ReMeSearchResponse` | 搜索响应 | `reme::dto::ReMeSearchResponse` | `memories()` 提取 `memory_list[].content` |
| `reme.ReMeClient` | HTTP 客户端 | `reme::client::ReMeClient` | 无鉴权头，仅 `Content-Type` |
| `reme.ReMeLongTermMemory` | 适配器 | `reme::memory::ReMeLongTermMemory` | `userId` 复用为 `workspace_id` |

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
| Mem0 DTO serde 对齐 | `Mem0AddRequest` 等 6 个 DTO | 已完成 | 字段可见 §2.1 |
| Mem0 HTTP 客户端 | `Mem0Client` | 已完成 | 端点/鉴权头/两种搜索响应形状（裸数组 v1.0 + `results` 对象 v1.1） |
| Mem0 适配器 | `Mem0LongTermMemory` | 已完成 | `record`/`retrieve`/`retrieve_msgs`；`build_search_request`/`build_add_request` |
| Bailian DTO + 客户端 + 适配器 | `Bailian*` 8 类 | 已完成 | — |
| ReMe DTO + 客户端 + 适配器 | `ReMe*` 7 类 | 已完成 | — |
| 消息 → 各厂商 message 转换 | `convertTo*Message` | 已完成 | `convert_messages` / `is_recordable` / `role_to_*_role` |
| 检索结果 → 字符串/core `Msg` 映射 | `retrieve` / `getMemories` | 已完成 | `join_search_results` / `join_memory_nodes` / `render_search_response` / `Mem0SearchResult::to_msg` |
| 编译错误修复（`ContentBlockKind` 导入路径） | — | 已完成 | 见 §4.1 |
| 离线单测（66 个） | Java 测试 ~5746 行 | 已完成 | DTO 往返、转换函数、请求构造、mock HTTP 全链路 |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实网络联调（真实 Mem0/Bailian/ReMe 服务） | — | 待办 | 需真实凭据；当前仅 mock，网络层已实现但未对真实端点回归 |
| Java 测试用例逐条对照迁移 | Java tests ~5746 行 | 待办 | 当前 Rust 单测为**功能等价重写**，未逐条照搬 Java 测试 |
| 高级检索参数（`rerank`/`filter_memories`/`fields` 等） | `Mem0SearchRequest` 等 | 已完成 | 字段已迁移并序列化；仅未做端到端验证 |

### 3.3 缺口（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 重试 / 退避 / 超时可配置化 | — | 待办 | 当前超时固定（默认 60s），无重试 |
| 指标 / 可观测性 | — | 待办 | 仅 `tracing` 依赖已声明，未埋点 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成内容**：MEM 三厂商（Mem0 / Bailian / ReMe）长期记忆适配全部迁移；DTO serde 对齐；消息转换；检索结果映射；可注入 HTTP seam；离线单测 66 个。
- **本会话修复（收尾轮）**：
  1. **2 个编译错误**：`bailian/memory.rs` 与 `reme/memory.rs` 中 `use agentscope_core::{ContentBlockKind, ...}` 报 E0432——`ContentBlockKind` 并未在 `agentscope_core` crate 根 re-export（`agentscope-core/src/message/mod.rs` 仅 re-export `ContentBlock` 等，未含 `ContentBlockKind`）。
     - **修法**：改用真实公开路径 `use agentscope_core::message::msg::ContentBlockKind;`（该类型定义于 `crates/agentscope-core/src/message/msg.rs:386`，为 `pub enum`）。**未自造平行类型，未改 core**。
  2. **后续错误**：`src/http.rs` 顶层 `use std::sync::Arc;` 在移除后 `mock` 模块（经 `use super::*` 引用 `Arc`）报 E0425；已在 `mock` 模块内显式 `use std::sync::{Arc, Mutex};`。
  3. **补齐离线单测**：新增 54 个（原有 2 个），覆盖各厂商 DTO serde 往返、消息→Mem0/Bailian/ReMe 转换、检索结果→core `Msg` 映射、搜索/添加请求构造、鉴权头、端点解析、空查询短路、mock HTTP 全链路。
  4. **清理告警**：本 crate 现无任何 warning（`agentscope-core`/`harness` 的 9 个 warning 属既有、非本 crate，未越界修改）。
- **验证命令与结果**：
  - `cargo build -p agentscope-extensions-mem` → **Finished**（exit 0，本 crate 0 warning）
  - `cargo test -p agentscope-extensions-mem` → **66 passed / 0 failed / 0 ignored**（exit 0）
- **已知问题**：无阻塞。真实网络端点未联调（见 §3.2）。

### 4.2 设计裁定与阻塞

1. **【设计裁定，本会话确认】不重建 Java 已废弃的 `LongTermMemory` 体系，也不强制实现 core `Memory` trait。**
   - 事实：Java 侧 `io.agentscope.core.memory.LongTermMemory` 标注 `@Deprecated(forRemoval = true, since = "2.0.0")`；core 侧 Rust 未迁移该体系。Rust `agentscope_core::Memory` trait 是**对话缓冲**抽象（`add_message`/`get_messages`/`delete_message`/`clear`/`save_to`/`load_from`，已由 `InMemoryMemory`、`StateBackedMemory` 实现），与 `LongTermMemory`（`record(&[Msg])` / `retrieve(&Msg) -> String`）**语义不等价**——长期记忆是「按语义检索注入」，而非「缓冲增删」。
   - **裁定**：**不实现** `core::memory::Memory` trait（实现它会导致语义错配，例如把检索命中伪装成会话缓冲消息），也**不重建**废弃的 `LongTermMemory` trait（避免引入平行抽象、且 Java 已判废弃）。改为：各适配器在**本 crate 内暴露固有** `record(&[Msg])` / `retrieve(&Msg) -> String` 方法，保留 Java 行为；并在 Mem0 适配器上提供 Rust 专属便捷方法 `retrieve_msgs(&Msg) -> Vec<Msg>`（将命中映射为 assistant 侧记忆注记，metadata 携带 `memory_id`/`score`/`categories`/`memory_source`），供上层「想以 core `Msg` 形式消费检索结果」的调用方使用。
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

