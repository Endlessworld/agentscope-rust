# agentscope-extensions 迁移 — 汇总进度跟踪文件（Rollup Gap Tracker）

> **用途**：本文件是 extensions 迁移的**汇总视图**，由**汇总方（主会话）**维护。每个模块的**单一事实来源**是它自己的独立 tracker：`docs/extensions/<MODULE>__GAP_TRACKER.md`。本文件不替代它们，只汇总状态、跨模块依赖与待办。
> **方案**：`docs/extensions/EXTENSIONS_MIGRATION_PLAN.md`
> **上游**：`docs/MIGRATION_PLAN.md` + `docs/MIGRATION_GAP_TRACKER.md`（core/harness）
> **范围**：`agentscope-extensions` 下 20 个模块（`agentscope-spring-boot-starters` 不迁移）。
> **基线**：2026-09-18 创建；`cargo build --workspace` 通过（20 个空壳 crate 已注册 workspace member）。

## 0. 如何阅读本文件
- 子智能体**不直接改本文件**；完工后在回执中报告，由汇总方登记（避免并发写冲突）。
- 状态取值：`待办` / `进行中` / `部分完成` / `已完成` / `阻塞` / `设计决定（不迁移）`。

## 1. 总览

| # | 波次 | 模块 | Rust crate | Java 主/测行 | 状态 | 独立 tracker |
|---|---|---|---|---|---|---|
| 1 | A | MODEL | `agentscope-extensions-model` | 24885 / 45043 | 部分完成（build + test 通过；58 test；`dashscope` 已落地；逐类映射/缺口见独立 tracker） | `MODEL__GAP_TRACKER.md` |
| 2 | A | PROTOCOL | `agentscope-extensions-protocol` | 16844 / 18860 | 部分完成（build + test 通过；130 test；`chat-completions-web` 已落地，`agui` 部分迁移（event+model 已落地，converter/adapter/encoder/processor/registry/runtime 未迁移）） | `PROTOCOL__GAP_TRACKER.md` |
| 3 | A | RAG | `agentscope-extensions-rag` | 15438 / 16057 | 部分完成（build + test 通过；152 test；`ragflow` 已落地；逐类映射/缺口见独立 tracker） | `RAG__GAP_TRACKER.md` |
| 4 | A | SANDBOX | `agentscope-extensions-sandbox` | 8024 / 1577 | 部分完成（build + test 通过；108 test；`fs_spec` 已落地；harness trait 复用契约见独立 tracker） | `SANDBOX__GAP_TRACKER.md` |
| 5 | A | CHANNEL | `agentscope-extensions-channel` | 4879 / 0 | 部分完成（build + test 通过；149 test；P0 离线适配层全落地，Stream/Controller/Crypto/`*Channel` 未迁移见 §3） | `CHANNEL__GAP_TRACKER.md` |
| 6 | A | MEM | `agentscope-extensions-mem` | 4821 / 5746 | 已完成（build + test 通过；66 test） | `MEM__GAP_TRACKER.md` |
| 7 | B | JDBC | `agentscope-extensions-jdbc` | 3106 / 1558 | 部分完成（build + test 通过；5 个编译错误已修；测试仅 lib 2，对照 Java 98 个 `@Test` 为 P0 缺口，见 §3.1） | `JDBC__GAP_TRACKER.md` |
| 8 | B | REDIS | `agentscope-extensions-redis` | 2884 / 217 | 部分完成（21+2 test 通过；9 个编译错误已修；`base_store()` 自动接线与 find_keys SCAN 语义为 P1，见 §3.2） | `REDIS__GAP_TRACKER.md` |
| 9 | B | MONGODB | `agentscope-extensions-mongodb` | 2231 / 2427 | 部分完成（62+1 test 通过；GridFS 分块/legacy 回退与 list_hash 算法统一为 P1/P2，见 §3） | `MONGODB__GAP_TRACKER.md` |
| 10 | B | MYSQL | `agentscope-extensions-mysql` | 2070 / 204 | 已完成（build + test 通过） | `MYSQL__GAP_TRACKER.md` |
| 11 | B | POSTGRESQL | `agentscope-extensions-postgresql` | 1571 / 1583 | 部分完成（build + test 通过；7 test，测试覆盖仍偏少，见 §3.1） | `POSTGRESQL__GAP_TRACKER.md` |
| 12 | B | COS | `agentscope-extensions-cos` | 882 / 760 | 已完成（build + test 通过；真实 SDK 适配见各模块 §4.2） | `COS__GAP_TRACKER.md` |
| 13 | B | OSS | `agentscope-extensions-oss` | 795 / 290 | 已完成（build + test 通过） | `OSS__GAP_TRACKER.md` |
| 14 | C | AISTIO | `agentscope-extensions-aistio` | 9352 / 2841 | 已完成（build + test 通过；65 test；本会话修复 2 个失败 + 恢复误删的 6 个依赖） | `AISTIO__GAP_TRACKER.md` |
| 15 | C | STUDIO | `agentscope-extensions-studio` | 3700 / 1452 | 已完成（build + test 通过；30 test） | `STUDIO__GAP_TRACKER.md` |
| 16 | C | SKILLS | `agentscope-extensions-skills` | 3177 / 2813 | 已完成（33 test 通过；6 个测试失败已修） | `SKILLS__GAP_TRACKER.md` |
| 17 | C | SCHEDULER | `agentscope-extensions-scheduler` | 3009 / 2589 | 已完成（37 test 通过；测试编译错误 + cron 解析 bug 已修） | `SCHEDULER__GAP_TRACKER.md` |
| 18 | C | TRAINING | `agentscope-extensions-training` | 2890 / 2918 | 已完成（build + test 通过；64 test） | `TRAINING__GAP_TRACKER.md` |
| 19 | C | NACOS | `agentscope-extensions-nacos` | 1873 / 2378 | 已完成（build + test 通过；43 test） | `NACOS__GAP_TRACKER.md` |
| 20 | C | HIGRESS | `agentscope-extensions-higress` | 1071 / 971 | 已完成（build + test 通过；30 test；search_tools 签名 bug 已修） | `HIGRESS__GAP_TRACKER.md` |

## 2. 跨模块依赖与汇总方待办

### 2.1 已裁定的公共抽象归属（**B 波次 7 个存储模块必须遵守**）

**裁定：不新建公共 `agentscope-extensions-store` crate。** 复用 core/harness 已有 trait（对齐 Java `harness.agent.DistributedStore` 的组合方式）：

| Java 组件 | Rust 既有 trait（**实现它，不重建**） | 位置 |
|---|---|---|
| `core.state.AgentStateStore` | `AgentStateStore`（async_trait；`save/save_list/get/get_list/exists/delete/list_session_ids`） | `agentscope_core::state::store` |
| `BaseStore`（远程 KV） | `BaseStore`（`StoreItem{key,value,version}`；`get/put/put_if_version/search/delete`） | `agentscope_harness::filesystem::remote::store` |
| `SandboxSnapshotSpec` | `SandboxSnapshotSpec`（`build(&self, snapshot_id) -> Result<Arc<dyn SandboxSnapshot>>`） | `agentscope_harness::agent::sandbox::snapshot` |
| `SandboxExecutionGuard` | `SandboxExecutionGuard`（`try_enter/release/is_held`） | `agentscope_harness::agent::sandbox::guard` |
| `MessageBus` | `MessageBus`（async_trait；Mode A/C/D 原语 + 领域 helper） | `agentscope_harness::agent::bus::message_bus` |
| `AsyncToolRegistry` | `AsyncToolRegistry` | `agentscope_harness::agent::bus::async_tool` |
| `TaskRepository` | `TaskRepository` | `agentscope_harness::agent::subagent::task::repository` |
| `DistributedStore`（组合包） | `agentscope_harness::agent::distributed_store::DistributedStore`（**当前仅 `agent_state_store` + `isolation_scope`，字段不足 → 见 §2.2**） | harness |

各存储模块的**统一产出契约**：本 crate 内提供 `XxxDistributedStore`（组合者）、`XxxAgentStateStore`、`XxxStore`（`BaseStore` 实现）、`XxxSnapshotSpec`、`XxxSandboxExecutionGuard`。

**避免重复发明**：`put_if_version` 的 CAS 语义、`StoreItem.version` 含义、快照 id 校验等一律以 harness 定义为准，不得自定义平行类型。

### 2.2 汇总方待办

| 项 | 说明 | 状态 |
|---|---|---|
| `DistributedStore` 字段不足 | **已裁定：不改 harness**。harness `DistributedStore` 保持 `agent_state_store` + `isolation_scope` 两字段不动，避免 7 个存储模块阻塞于一次共享编辑。

各存储 crate 提供自己的组合者 `XxxDistributedStore`，在原两字段基础上追加 `base_store: Option<Arc<dyn BaseStore>>` / `sandbox_snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>` / `sandbox_execution_guard: Option<Arc<SandboxExecutionGuard>>`（用 `Arc` 包裹，`BaseStore`/`SandboxSnapshotSpec` 本为 `Send+Sync` trait 对象）。各 crate 的 `XxxDistributedStore` 自建 builder，不依赖 harness builder。

**若后续发现需要 harness 内置该组合**：作为独立后续任务重估，不由本轮 14 个子智能体处理。 | 已裁定 |
| 根 `Cargo.toml` workspace 依赖 | 子智能体不得直接修改；需求汇总在此 | 待汇总（子智能体回执后填写） |
| Sandbox 后端抽象 | 已裁定：SANDBOX 模块实现 harness `Sandbox`/`SandboxClient` trait | 已裁定 |
| Channel 抽象 | 已裁定：harness `agent/channel.rs` 为路由骨架，CHANNEL 模块提供平台适配 | 已裁定 |
| Skills repository | SKILLS 模块与 harness `skill/` 存量代码的关系 | 待裁定 |
| MODEL 模块与 core `model/openai.rs`（已用 async-openai） | 已下发约束：core 定义 trait + 注册表，本 crate 补 provider 适配，不重复实现 openai | 已裁定 |

## 3. 汇总登记区（执行后由汇总方填写）

### 3.1 子智能体回执

### 3.1.2 子智能体回执（第五批：全量复测与收尾）

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| AISTIO | ①`Cargo.toml` 重复 `tokio` key；②误删 6 个依赖（`futures`/`tracing`/`chrono`/`reqwest`/`sha2`/`uuid`）；③axum 0.7 `fallback` 拒绝同步零参 `not_found`；④测试 `E0596` | 恢复 6 个依赖；`not_found` 改 `async fn`；测试 `let server` → `let mut server` | ✅ 65 test 通过 |
| PROTOCOL | `agui` 整包未接线（`lib.rs` 缺 `pub mod agui;`、缺 `model.rs`）；`event.rs` 泛型约束不足 | 新增 `agui/model.rs`+`agui/mod.rs`（忠实映射 Java `agui/model`）、`lib.rs` 声明；`parse_json` bound 改 `DeserializeOwned` | ✅ 130 test 通过 |
| RAG | float 精度导致测试失败 | `serde_json` 启用 `float_roundtrip`；`ragflow` 落地 | ✅ 152 test 通过 |
| SANDBOX | `fs_spec` 未落地 | 补齐 | ✅ 108 test 通过 |
| CHANNEL | WeCom 签名 `Clone` 约束；base64 `decode_allow_trailing_bits` 语义 | 修 `wecom/signature.rs`、`wecom/crypto.rs`（对齐 Java `Base64.getDecoder()`） | ✅ 149 test 通过 |
| MODEL / STUDIO / HIGRESS | — | 复测绿 | ✅ 58 / 30 / 30 |

- **第六批汇总验收（最终）**：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace` → **1746 passed / 0 failed**（86 test target，TEST_EXIT=0，见 §3.2）。
- **第五批汇总验收**（保留备查）：`cargo test --workspace` → **1728 passed / 0 failed**（86 target）。

> 波次：A=MODEL/PROTOCOL/RAG/SANDBOX/CHANNEL/MEM；B=7 个存储模块；C=AISTIO/STUDIO/SKILLS/SCHEDULER/TRAINING/NACOS/HIGRESS。

**第一批（20 个模块迁移）**：全部 20 个模块均产出了实质性代码（`crates/agentscope-extensions-*`）。首轮逐 crate 构建结果：**16 PASS / 4 FAIL**（FAIL：`jdbc`、`redis`、`mongodb`、`higress`）。

**第二批（修复轮）**

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| REDIS | 9 个编译错误：`state::adapter`/`state::connection` 未 `mod` 声明；harness `sandbox::key` 私有 | 补 `mod` 声明接线既有源码（未造空模块）；`key` 改用 harness 公开 re-export；修正 `lrange` 泛型签名 | ✅ 21+2 test 通过 |
| MONGODB | 测试编译失败（私有模块）+ 2 个测试逻辑失败（upsert 重复 `_id`、list_hash 前缀语义）+ `E0499` 双可变借用 | 改用公开 re-export；按 Java 语义修 `find_one_and_update`/`needs_full_rewrite`；`client.rs` GridFS 路径限定 `files` 借用作用域 | ✅ 62+1 test 通过 |
| JDBC | 5 个编译错误：`super::dialect` 路径、`OptionalExtension` 误用、`SandboxExecutionGuard` 误当 trait、`hash_list` 缺失 | 修正导入路径；`JdbcSandboxExecutionGuard` 改固有方法；手写 `Debug`/`Clone`；复用 core `list_hash` | ✅ 2 test 通过 |
| HIGRESS | `search_tools(&query, top_k)` 调用与单参定义不符 | 依据 Java `HigressMcpClientWrapper` 两参重载，改调 `search_tools_with_top_k`（保留 `tool_search_top_k` 生效） | ✅ build + test 通过 |
| SCHEDULER | lib test 编译失败：`ScheduleMode` 未导入、`Msg::get_text_content` 不存在（3 处） | 补 `use crate::common::config::ScheduleMode`；改 `text_content()`；另修 cron `a/step` 解析真实 bug | ✅ 37 test 通过 |
| SKILLS | 6 个测试失败：建表/表名解析、`metadata_json_column_supported`、resource 落库、绝对路径校验 | 均判为**实现 bug**：扩展 `parse_table`/`execute` INSERT、探测前置取第 2 参数、POSIX 绝对路径校验 | ✅ 33 test 通过 |

**第三批（续作与接线轮）**

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| STUDIO | 13 个 src 文件从未编译（`lib.rs` 仅 1 行注释、无 `mod` 声明）；E0252/E0277 等接线错误 | 接线 `pub mod studio/telemetry` + re-export + crate 级文档；修重名导入与 trait 约束 | ✅ 30 test 通过 |
| TRAINING | 子智能体达迭代上限；tracker 未回填 | 收尾并补 `TrainingContext::mark_with_metadata(...)` | ✅ 64 test 通过 |
| MEM | 子智能体达迭代上限，但磁盘实际已修复 | 复核后确认可编译 | ✅ 66 test 通过 |
| AISTIO | `store::task_repository` 2 个测试失败（本地任务完成状态持久化顺序） | 新增 `persist_status_detached(after, …)` 等；按持久化顺序修实现 | ✅ 55 test 通过 |
| PROTOCOL | 1 个测试失败：`media_part_mime_falls_back_to_primary_type_for_unknown_extension` | 判为**测试断言写错**（与本测试名 falls back to primary type 及 Java `FilePartParser#getPrimaryType` 语义矛盾）：改为断言可还原为 `ContentBlock::Audio` 且 URL 源不变 | ✅ 79 test 通过 |
| CHANNEL | 子智能体产出仅 11 文件 / 1054 行且严重损坏（`lib.rs` 声明不存在的子模块、缺 `quick_xml`）；修复子智能体再次达迭代上限，最终 114 passed / 4 failed | 主会话直接处置：补全六平台适配层（38 文件），并修 3 处真实缺陷 —— ①`common/xml.rs` 未处理 `quick-xml` 的 `Event::GeneralRef`，实体被静默丢弃（影响 WeCom inbound）；②feishu `send` 测试未播种 `code:0` 响应；③幂等库测试断言与 Java [sweep 先于 putIfAbsent] 语义冲突 | ✅ 118 test 通过 |

### 3.1.1 子智能体回执（第四批：tracker 回填）

| 模块 | 任务 | 状态 |
|---|---|---|
| CHANNEL | 回填独立 tracker §1–§4（Java 39 类逐文件映射、P0/P1/P2 缺口、3 处缺陷根因、跨模块需求） | ✅ 主会话完成 |
| MODEL / RAG / SANDBOX / POSTGRESQL | 并行回填独立 tracker（4 个后台子智能体） | 进行中 |

### 3.2 汇总验收

- **基线**：`cargo build --workspace` ✅（仅 warning，无 error）。
- **验收（最终全量）**：`cargo build --workspace` → **BUILD_EXIT=0**（无 error）；`cargo test --workspace` → **1746 passed / 0 failed**（86 个 test target，TEST_EXIT=0）。较上轮基线 1485 增加 261。逐 crate 明细（lib test，最终实测日志）：`core` 73、`harness` 291、`aistio` 65、`channel` 149、`cos` 24、`higress` 30、`jdbc` 2、`mem` 66、`model` 58、`mongodb` 62、`mysql` 43、`nacos` 43、`postgresql` 7、`protocol` 130、`rag` 152、`redis` 21、`sandbox` 108、`scheduler` 37、`skills` 33、`studio` 30、`training` 64（另含 1 个 mongodb doc-test）。
- **上轮基线**（保留备查）：`cargo test --workspace` → 1485 passed / 0 failed；第五批 1728 passed / 0 failed。
- **约束遵守**：全轮未改根 `Cargo.toml` 的 `[workspace.dependencies]`、未改 `agentscope-core`/`agentscope-harness`、各模块仅在本 crate 内改动 + 各自 tracker。
- **构建注意事项（非代码缺陷）**：全量测试曾因 E: 盘耗尽（`os error 112`、rustc-LLVM IO failure）报「could not compile」假阳性。根因是 `target/debug/incremental` 累积 19.55GB（target 共 55.37GB）。处置：删除 `incremental` 回收 19.5GB，并以 `CARGO_INCREMENTAL=0` 重跑全量测试。**后续跑 `--workspace` 前请先确认 E: 盘剩余空间 ≥ 15GB。**

### 3.3 汇总方待办（跨模块，未决）

| 项 | 说明 | 状态 |
|---|---|---|
| 根 `Cargo.toml` workspace 依赖 | 多个模块需真实后端驱动，建议后续统一加入 `[workspace.dependencies]`：`sqlx`/`mysql_async`/`tokio-postgres`（MYSQL/POSTGRESQL/SKILLS）、`redis`（REDIS 已本地依赖）、`mongodb`（MONGODB）、`object_store` 或各云厂商 SDK（COS/OSS）。当前均用内存实现以保可测。 | 待汇总 |
| harness `DistributedStore` 组合 | 已裁定不改 harness；各存储 crate 自建 `XxxDistributedStore`（见 §2.2）。若后续要在 harness 内置该组合，另立任务。 | 已裁定 |
| harness `SandboxExecutionGuard` 为具体 struct | JDBC 回执指出：harness `SandboxManager` 只接受具体 `SandboxExecutionGuard`；若要 JDBC 锁真正驱动沙箱隔离，需 harness 提供 trait 抽象/适配层。 | 待裁定 |
| Skills↔harness `skill/` 关系 | SKILLS 模块已交付并通过测试；与 harness 存量 `skill/` 的职责边界仍需最终裁定（原 §2.2 遗留）。 | 待裁定 |
| 各模块 tracker §2/§3 回填 | MODEL / RAG / SANDBOX / POSTGRESQL / CHANNEL 曾为占位模板；CHANNEL 已由主会话回填，其余 4 个已派发后台子智能体。MEM / STUDIO / NACOS / TRAINING / AISTIO / PROTOCOL / SKILLS / SCHEDULER / HIGRESS / 7 个存储模块已回填。 | 进行中 |
| HTTP 框架选型（CHANNEL 依赖） | 4 个 `*CallbackController`（feishu/github/gitlab/wecom，Java 共 645 行）需 HTTP 框架承载；`agentscope-extensions-protocol` 已用 `axum`，建议统一。**CHANNEL 未擅自引入**，待汇总方裁定。 | 待裁定 |
| harness `agent::Channel` 契约是否足够 | CHANNEL 的 5 个 `*Channel` 适配（Java 共 1036 行）需先确认 harness `agent/channel.rs` trait 能否承载「每平台 inbound 分发 + outbound 投递」；若不足，需裁定「扩 harness」还是「本 crate 内适配层」。 | 待裁定 |
