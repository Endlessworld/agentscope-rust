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
| 4 | A | SANDBOX | `agentscope-extensions-sandbox` | 8024 / 1577 | **已完成**（build + test 通过；**108 test**；P0+P1 已落地：四后端 + `fs_spec`（四类 `*FilesystemSpec` 适配层）+ `kubernetes_files`（文件 API + `SandboxFileTransfer`）；harness 抽象缺口见独立 tracker §4.2） | `SANDBOX__GAP_TRACKER.md` |
| 5 | A | CHANNEL | `agentscope-extensions-channel` | 4879 / 0 | **已完成**（build + test 通过；**241 test**；P0+P1+P2 全部落地：common 全量 + 六平台 provider 件 + **5 个 `*Channel` 适配 + 4 个 axum CallbackController + `DingTalkStreamClient` 常驻 WebSocket 网关**） | `CHANNEL__GAP_TRACKER.md` |
| 6 | A | MEM | `agentscope-extensions-mem` | 4821 / 5746 | 已完成（build + test 通过；66 test） | `MEM__GAP_TRACKER.md` |
| 7 | B | JDBC | `agentscope-extensions-jdbc` | 3106 / 1558 | 部分完成（build + test 通过；5 个编译错误已修；测试仅 lib 2，对照 Java 98 个 `@Test` 为 P0 缺口，见 §3.1） | `JDBC__GAP_TRACKER.md` |
| 8 | B | REDIS | `agentscope-extensions-redis` | 2884 / 217 | 部分完成（21+2 test 通过；9 个编译错误已修；`base_store()` 自动接线与 find_keys SCAN 语义为 P1，见 §3.2）；list_hash 已收敛到 core（2026-09-19） | `REDIS__GAP_TRACKER.md` |
| 9 | B | MONGODB | `agentscope-extensions-mongodb` | 2231 / 2427 | 部分完成（61+1 test 通过；GridFS 分块/legacy 回退为 P1/P2，见 §3）；list_hash 已收敛到 core（2026-09-19，原本地 SHA-256 实现删除） | `MONGODB__GAP_TRACKER.md` |
| 10 | B | MYSQL | `agentscope-extensions-mysql` | 2070 / 204 | 已完成（build + test 通过） | `MYSQL__GAP_TRACKER.md` |
| 11 | B | POSTGRESQL | `agentscope-extensions-postgresql` | 1571 / 1583 | 部分完成（build + test 通过；7 test，测试覆盖仍偏少，见 §3.1） | `POSTGRESQL__GAP_TRACKER.md` |
| 12 | B | COS | `agentscope-extensions-cos` | 882 / 760 | 已完成（build + test 通过，31+2 test；真实 SDK 适配见各模块 §4.2）；list_hash 已收敛到 core 并修复「追加恒全量重写」行为偏离（2026-09-19） | `COS__GAP_TRACKER.md` |
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
| 根 `Cargo.toml` workspace 依赖 | **已执行**（见 §3.3）。统一加入 `sqlx`/`mysql_async`/`tokio-postgres`/`redis`/`mongodb`/`object_store`/`axum`/`http`/`bytes`；`protocol`/`aistio`/`channel` 已改引 workspace。 | 已完成 |
| Sandbox 后端抽象 | 已裁定：SANDBOX 模块实现 harness `Sandbox`/`SandboxClient` trait | 已裁定 |
| Channel 抽象 | 已裁定：harness `gateway/channel` 为路由骨架；**新增公共层 `gateway/platform_channel`**（`PlatformChannel`/`PlatformCtx`）承载各平台相同的分发/投递流程，各模块只写不通用部分 | **已裁定并已落地** |
| Skills repository | SKILLS 模块与 harness `skill/` 存量代码的关系 | **已裁定**：分层与 Java 一致（Java 接口在 core、实现在 extensions-skills；Rust 接口在 harness、实现在本 crate），属**归属位移非层次错位**。见 §3.3 |
| MODEL 模块与 core `model/openai.rs`（已用 async-openai） | 已下发约束：core 定义 trait + 注册表，本 crate 补 provider 适配，不重复实现 openai | 已裁定 |

## 3. 汇总登记区（执行后由汇总方填写）

### 3.1 子智能体回执

### 3.1.2 子智能体回执（第七批：CHANNEL P1 主体攻坚）

**本批为汇总方亲自执行 + 4 个后台子智能体并行**（每子智能体只改自己平台目录）：

| 模块 | 任务 | 处置/产出 | 状态 |
|---|---|---|---|
| HARNESS | 公共层落地（汇总方） | 新增 `agent/gateway/platform_channel.rs`：`PlatformChannel` trait + `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl；后续顺手移除冗余包装类型 `PlatformChannelSink` | ✅ 6 test 通过 |
| CHANNEL(common) | 接线从未编译的 `channel_core.rs`/`channel_registry.rs`（汇总方） | `ChannelCore` 由自存 routing/gateway **收敛为组合 harness `PlatformCtx`**，新增 `admit()`/`admitted()`；`ChannelRegistry<C>` 接线并修 `Channel`/`ChannelSink` 同名方法歧义 | ✅ |
| CHANNEL(feishu) | 参考样板（汇总方） | `channel.rs`（`handle_callback` 全流程）+ `controller.rs`（axum 薄壳）；修复测试载荷缺 mapper 必需字段 | ✅ 173 test（crate 级） |
| CHANNEL(dingtalk) | 平台适配（子智能体） | `channel.rs`：`DingTalkChannel` + `handle_stream_payload` | ✅ dingtalk:: 28 test |
| CHANNEL(wecom) | 平台适配 + controller（子智能体） | `channel.rs`（`verify_url`/`handle_callback`）+ `controller.rs`（GET URL 验证 + POST 事件） | ✅ wecom:: 45 test |
| CHANNEL(github) | 平台适配 + controller（子智能体） | `channel.rs`（HMAC 校验 + 防自回环 `is_self`）+ `controller.rs` | ✅ github:: 32 test |
| CHANNEL(gitlab) | 平台适配 + controller（子智能体） | `channel.rs`（`X-Gitlab-Token` 校验 + 防自回环）+ `controller.rs` | ✅ gitlab:: 35 test |
| CHANNEL(dingtalk-stream) | `DingTalkStreamClient` 常驻 WebSocket 网关（汇总方接线） | `stream.rs`（注册 → 连接 → ACK → 分发 → 指数退避重连；I/O 走 `HttpTransport` + `WebSocketConnector`/`StreamSocket` seam；生产连接器 `TungsteniteConnector`）；`mod.rs` 声明接线 + 清理未用导入 | ✅ crate 227 → **241 test** |

**本批修复的真实缺陷（2 处，均在汇总方收尾阶段）**：
1. **`agentscope_harness::agent::Channel` 名称陷阱**：`gateway/mod.rs` 里保留的**遗留事件通道 struct** `Channel` 与路由 trait `Channel` 同名，`agent::Channel` 路径解析到 struct 而非 trait（`E0404 expected trait, found struct`）。修法：路由 trait 一律用全路径 `agentscope_harness::agent::gateway::channel::Channel` 导入（已写入 CHANNEL tracker §1.2 注意事项）。
2. **`Channel`/`ChannelSink` 的 `channel_id()` 歧义（E0034）**：新增 `ChannelSink` blanket impl 后，两 trait 同名方法同时在作用域内会使 `c.channel_id()` 无法推导。修法：在 `channel_registry.rs` 与 harness 测试中限定为 `Channel::channel_id(&…​)`。

> 另外两次失败经查为**环境/既有问题、非代码缺陷**：①全量测试在默认并行度下偶发 3 个 `training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`，属既有测试间竞争；单独跑 3/3 全绿）；②`os error 1455/112`（页面文件不足导致 rlib mmap 失败）会产生「could not compile」假阳性，降并行度（`-j 2`）可规避。

### 3.1.3 子智能体回执（第五批：全量复测与收尾）

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| AISTIO | ①`Cargo.toml` 重复 `tokio` key；②误删 6 个依赖（`futures`/`tracing`/`chrono`/`reqwest`/`sha2`/`uuid`）；③axum 0.7 `fallback` 拒绝同步零参 `not_found`；④测试 `E0596` | 恢复 6 个依赖；`not_found` 改 `async fn`；测试 `let server` → `let mut server` | ✅ 65 test 通过 |
| PROTOCOL | `agui` 整包未接线（`lib.rs` 缺 `pub mod agui;`、缺 `model.rs`）；`event.rs` 泛型约束不足 | 新增 `agui/model.rs`+`agui/mod.rs`（忠实映射 Java `agui/model`）、`lib.rs` 声明；`parse_json` bound 改 `DeserializeOwned` | ✅ 130 test 通过 |
| RAG | float 精度导致测试失败 | `serde_json` 启用 `float_roundtrip`；`ragflow` 落地 | ✅ 152 test 通过 |
| SANDBOX | `fs_spec` 未落地 | 补齐 | ✅ 108 test 通过 |
| CHANNEL | WeCom 签名 `Clone` 约束；base64 `decode_allow_trailing_bits` 语义 | 修 `wecom/signature.rs`、`wecom/crypto.rs`（对齐 Java `Base64.getDecoder()`） | ✅ 149 test 通过 |
| MODEL / STUDIO / HIGRESS | — | 复测绿 | ✅ 58 / 30 / 30 |

- **第九批汇总验收（文档同步批，2026-09-19）**：`cargo test --workspace -j 2 --no-fail-fast` → **1866 passed / 0 failed**（86 test target；唯一 `1 ignored` 为 `platform_channel.rs` 的 ` ```ignore ` 文档示例，早于基线即存在）。本批**仅改文档**：逐模块核对并修正独立 tracker 中过时/错误结论（MODEL `dashscope` 已接线非阻塞、RAG `ragflow` 已落地、SANDBOX `fs_spec`/`kubernetes_files` 已落地），汇总 §1 SANDBOX 行由「部分完成」校正为「已完成」。
- **第七批汇总验收**（保留备查）：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace -j 2` → **1846 passed / 0 failed**（较第六批 1746 增加 100：harness 299 +6，channel 227 +78，其余持平）。
- **第六批汇总验收**（保留备查）：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace` → **1746 passed / 0 failed**（86 test target，TEST_EXIT=0，见 §3.2）。
- **第五批汇总验收**（保留备查）：`cargo test --workspace` → **1728 passed / 0 failed**（86 target）。

> 波次：A=MODEL/PROTOCOL/RAG/SANDBOX/CHANNEL/MEM；B=7 个存储模块；C=AISTIO/STUDIO/SKILLS/SCHEDULER/TRAINING/NACOS/HIGRESS。

**第一批（20 个模块迁移）**：全部 20 个模块均产出了实质性代码（`crates/agentscope-extensions-*`）。首轮逐 crate 构建结果：**16 PASS / 4 FAIL**（FAIL：`jdbc`、`redis`、`mongodb`、`higress`）。

**第二批（修复轮）**

| 模块 | 问题 | 处置 | 状态 |
|---|---|---|---|
| REDIS | 9 个编译错误：`state::adapter`/`state::connection` 未 `mod` 声明；harness `sandbox::key` 私有 | 补 `mod` 声明接线既有源码（未造空模块）；`key` 改用 harness 公开 re-export；修正 `lrange` 泛型签名 | ✅ 21+2 test 通过 |
| MONGODB | 测试编译失败（私有模块）+ 2 个测试逻辑失败（upsert 重复 `_id`、list_hash 前缀语义）+ `E0499` 双可变借用 | 改用公开 re-export；按 Java 语义修 `find_one_and_update`/`needs_full_rewrite`；`client.rs` GridFS 路径限定 `files` 借用作用域 | ✅ 62+1 test 通过 |
| MONGODB / COS / REDIS（list_hash 收敛，2026-09-19） | 三 crate 各持一份平行 list_hash 实现（MONGODB/COS 为全量 SHA-256，REDIS 为采样重复实现）；其中 MONGODB 与 COS 的 `needs_full_rewrite` 在 `stored_hash == None && existing_count == 0` 时返回 `true`，**偏离 Java**（Java 返回 `false`）；COS 另用 `!whole_hash.starts_with(stored)` 判追加，因全量哈希无前缀性质而**恒判需全量重写** | 统一删除本地实现、委托 `agentscope_core::state::list_hash`；修正与 Java 相反的测试断言；COS 解耦 `existing_count`（原仅在 hash 存在时读取，致 core Case 2 不可达）；COS 移除已无用途的 `sha2` 依赖 | ✅ 三 crate 全绿（MONGODB 61+1、COS 31+2、REDIS 21+2），workspace **1866 passed / 0 failed** |
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
- **验收（第八批，当前）**：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace -j 2 --no-fail-fast` → **1860 passed / 0 failed**（86 个 test target，EXIT=0）。较第七批 1846 增加 14：仅 `channel` 149→227→**241**（+14 为 `dingtalk/stream.rs` 的常驻 WebSocket 客户端单测），其余 crate 持平。CHANNEL 模块至此 **P0+P1+P2 全部落地**。
- **验收（第七批）**：`cargo build --workspace` → **BUILD_EXIT=0**（无 error）；`cargo test --workspace -j 2` → **1846 passed / 0 failed**（TEST_EXIT=0）。较第六批 1746 增加 100：`harness` 291→**299**（+6 `platform_channel` 单测）、`channel` 149→**227**（+78：5 个 `*Channel` + 4 个 controller），其余 crate 持平。
- **验收（第六批，保留备查）**：`cargo build --workspace` → **BUILD_EXIT=0**；`cargo test --workspace` → **1746 passed / 0 failed**（86 个 test target）。较上轮基线 1485 增加 261。逐 crate 明细（第六批实测日志）：`core` 73、`harness` 291、`aistio` 65、`channel` 149、`cos` 24、`higress` 30、`jdbc` 2、`mem` 66、`model` 58、`mongodb` 62、`mysql` 43、`nacos` 43、`postgresql` 7、`protocol` 130、`rag` 152、`redis` 21、`sandbox` 108、`scheduler` 37、`skills` 33、`studio` 30、`training` 64（另含 1 个 mongodb doc-test）。
- **上轮基线**（保留备查）：`cargo test --workspace` → 1485 passed / 0 failed；第五批 1728 passed / 0 failed。
- **约束遵守**：除汇总方已裁定的两处例外（根 `Cargo.toml` `[workspace.dependencies]`、harness 公共层 `gateway/platform_channel.rs`），各模块仅在本 crate 内改动 + 各自 tracker。
- **构建注意事项（非代码缺陷）**：
  1. **磁盘/页面文件**：曾因 E: 盘耗尽（`os error 112`）报假阳性；当前项目盘已为 `D:`（实测剩余 407GB）。此外 `os error 1455`（页面文件太小，rlib mmap 失败）会在高并行度下报假 `could not compile`，降并行度（`cargo test --workspace -j 2`）可规避。
  2. **既有测试间竞争**：`cargo test --workspace` 在默认并行度下偶发 3 个 `agentscope-extensions-training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`）；**与 CHANNEL/harness 本轮改动无关**，单跑 `-p agentscope-extensions-training` 为 3/3 全绿（64 passed）。第八批以 `-j 2` 复跑未复现该失败。
  3. **PowerShell 管道**：直接管道接 `cargo test --workspace` 全量输出会触发 PowerShell 栈溢出，建议先 `Out-File` 再分析。
- **本批 harness 注意点**：`agentscope_harness::agent::Channel` 是遗留事件 struct（与路由 trait 同名），路由 trait 需全路径导入 `agent::gateway::channel::Channel`。

### 3.3 汇总方待办（跨模块，未决）

| 项 | 说明 | 状态 |
|---|---|---|
| 根 `Cargo.toml` workspace 依赖 | **已执行**。统一加入 `[workspace.dependencies]`：`sqlx`/`mysql_async`/`tokio-postgres`（MYSQL/POSTGRESQL/SKILLS）、`redis`（REDIS）、`mongodb`（MONGODB）、`object_store`（COS/OSS）、`axum`/`http`/`bytes`（HTTP 统一）。已验证 `cargo metadata` 与 `cargo build --workspace` 通过；`protocol`/`aistio`/`channel` 均改引 workspace。 | 已完成 |
| harness `DistributedStore` 组合 | 已裁定不改 harness；各存储 crate 自建 `XxxDistributedStore`（见 §2.2）。若后续要在 harness 内置该组合，另立任务。 | 已裁定 |
| harness `SandboxExecutionGuard` 为具体 struct | **已裁定（依 Java 源码）：确认 harness 存在缺口，维持「不改 harness」直到另立任务**。Java 侧 `io.agentscope.harness.agent.sandbox.SandboxExecutionGuard` 是 `@FunctionalInterface` **接口**（唯一方法 `SandboxLease tryEnter(SandboxIsolationKey) throws InterruptedException`，语义为**阻塞直到获得槽位**，并提供 `noop()`）；Rust `harness/src/agent/sandbox/guard.rs` 却是**具体 struct**（`leases: HashMap<key,usize>`），其 `try_enter` **只做可重入计数、不阻塞也不跨进程**。故 JDBC/Redis/Mongo 等的分布式锁**无法驱动沙箱隔离**（各 crate 只能自带 guard 类型，形成平行类型）。修复需 harness 把该 struct 改为 trait + 默认 `NoopGuard`（同 Java），属 harness 公共层变更。**影响面（实测）**：aistio/cos/jdbc/mongodb/mysql/oss/postgresql/redis 共 8 个 crate 均已有各自的 guard 类型。 | **已裁定**：缺口确认；改 harness 列为独立任务（本批不动） |
| Skills↔harness `skill/` 关系 | **已裁定：维持原结论，分层与 Java 一致**。Java 源码核实（2026-09-19）：`agentscope-extensions-skills` **只含 repository 实现**（`GitSkillRepository` / `MysqlSkillRepository` / `PostgresSkillRepository`），模块根 `src/` 下**无任何 `.java`**；接口 `AgentSkillRepository` 位于 **core**（`io.agentscope.core.skill.repository`，由 core 的 `ReActAgent` / `DynamicSkillMiddleware` 消费）⇒ Java 分层 =【core：接口+模型】【extensions-skills：后端实现】。Rust 侧分工一致，唯一差异是接口**落在 harness 而非 core**（因 `agentscope-core` 无 `skill` 模块），属**归属位移而非层次错位**。残留观察：若将来 core 的 agent 也需挂载 skill 仓库，需重议是否上提 trait 到 core。 | **已裁定**（见 `SKILLS__GAP_TRACKER.md` §2.0） |
| `list_hash` 算法并存 | **已裁定并已落地（依 Java 源码 + Rust 实测）：以 core 为正解；MONGODB / COS / REDIS 三处平行实现已于 2026-09-19 全部收敛到 `agentscope_core::state::list_hash`**。Java `io.agentscope.core.state.ListHashUtil` 采用**采样**策略：`"size:N;"` + 采样位 `"i:itemHashCode,"`，末了 `Integer.toHexString(sb.hashCode())`；常量 `EMPTY_HASH="empty:0"`、`SAMPLING_THRESHOLD=5`、采样位 `[0,n/4,n/2,3n/4,n-1]`；`needsFullRewrite` 也是拿 `existingCount` 前缀哈希与存储哈希比。`agentscope-core/src/state/list_hash.rs` **忠实移植**了该策略，且**已经在 `state/mod.rs` `pub use`**。**Rust 实测分布（2026-09-19）**：① **JDBC** `state/jdbc_agent_state_store.rs` 与 ② **PostgreSQL** `state.rs` **已调用 core**（`agentscope_core::state::list_hash`，共 2 个 crate 落地）；③ **REDIS** `state/list_hash.rs` 策略忠实（采样，仅 per-element 输入改为 JSON 文本），属**重复实现但语义一致**；④ **MONGODB** 与 ⑤ **COS** 均为全量 SHA-256；⑥ MySQL 未涉及该逻辑。**两处真实行为偏离（已修）**：(a) ④MONGODB 与 ⑤COS 的 `needs_full_rewrite` 在 `stored_hash == None && existing_count == 0` 时返 `true`，**与 Java 相反**——Java 经 Case 3 得空前缀 → `computeHash(空)="empty:0"` → `hasChanged(·, null) == false` ⇒ 返 **false**（MONGODB 的测试断言本身即错，已按 Java 语义更正）；(b) ⑤COS 另用 `!whole_hash.starts_with(stored)` 判追加，**实测全量 SHA-256 无前缀性质**（`hash([a,b,c])` 不以 `hash([a,b])` 开头），致**纯追加恒判需全量重写**（增量优化失效；数据无损）。**收敛结果**：三 crate 弃本地实现 → 委托 core（MONGODB 保留 `list_hash.rs` 作薄 re-export 以不动公开路径；REDIS 删除 `state/list_hash.rs`；COS 删除两个 private 函数并移除已无用的 `sha2` 依赖，另解耦 `existing_count` 使 core Case 2 可达）。存量数据自愈：hash 值不可比 → 首次写入走全量重写，数据无损、无需迁移脚本。 | **已裁定并已落地**（见 `MONGODB__GAP_TRACKER.md` §3.3、`COS__GAP_TRACKER.md` §3.1、`REDIS__GAP_TRACKER.md` §4.1.1） |
| 各模块 tracker §2/§3 回填 | **全部完成**。MODEL / RAG / SANDBOX / POSTGRESQL / CHANNEL 已由主会话与后台子智能体回填（`docs/extensions/` 下已无空模板，各 tracker 8.9K–31K）；CHANNEL 本轮再次回填（§1.2 公共层归属、§2.6 逐类映射、§3.2 P1 状态、§4.1-§4.3 验收与遗留）。MEM / STUDIO / NACOS / TRAINING / AISTIO / PROTOCOL / SKILLS / SCHEDULER / HIGRESS / 7 个存储模块均已回填。 | 已完成 |
| HTTP 框架选型（CHANNEL 依赖） | **已裁定：统一用 `axum`**（与 `agentscope-extensions-protocol`、`agentscope-extensions-aistio` 一致）。4 个 `*CallbackController`（feishu/github/gitlab/wecom）已以 `axum::Router` 承载，路由/解析为薄壳，签名校验、解密、inbound 映射复用已落地纯逻辑。 | 已裁定并**已落地** |
| harness `agent::Channel` 契约是否足够 | **已裁定：契约足够，公共层写 harness，不通用部分写各模块**。已在 harness 新增 `agent::gateway::platform_channel`（`PlatformChannel` trait + `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl，6 个单测）；五个平台 `*Channel` 只实现 `ctx()` + `deliver_reply()`。 | 已裁定并**已落地** |
