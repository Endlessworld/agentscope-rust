# COS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **COS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-cos\`
> **Java 子模块**：(鍗曟ā鍧?
> **Rust 目标**：`crates/agentscope-extensions-cos/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-cos` |
| Java 子模块 | 单模块 |
| Java 规模（初测） | 主代码 ~882 行 / 测试 ~760 行 |
| Rust crate | `crates/agentscope-extensions-cos` |
| 迁移波次 | B |
| 当前状态 | 已完成（后端为内存实现 + async 抽象；真实 COS SDK 适配见 §4.2） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `CosAgentStateStore` | COS 对象存储上的 `AgentStateStore` | `CosAgentStateStore`（implements `agentscope_core::state::AgentStateStore`） | key 布局对齐：`{prefix}{user}/{session}/{key}.json` / `.list.json` / `.list.hash`；anonymous 用户段 `__anon__`；前缀归一化对齐 |
| `CosBaseStore` | COS 上的 `BaseStore`（远程 KV） | `CosStore`（implements `agentscope_harness::filesystem::remote::store::BaseStore`） | key 布局：`{prefix}{ns0}/{ns1}/.../{key}.json` / `.version`；version 成功写入 +1；CAS 语义以 harness 为准 |
| `CosDistributedStore` | 组合 agent state / base store / snapshot spec | `CosDistributedStore` | Java 含 `agent_state_store`/`base_store`/`sandbox_snapshot_spec`；Rust 版额外含 `sandbox_execution_guard`，并可 `to_harness_distributed_store()` 投影（harness 结构体字段不足，见汇总 §2.2） |
| `CosRemoteSnapshotClient` | COS 上的 `RemoteSnapshotClient` | `CosRemoteSnapshotClient`（implements `...snapshot::RemoteSnapshotClient`） | 对象 key：`{prefix}{snapshotId}.tar`；snapshot id 安全校验复用 harness `validate_snapshot_id` 语义 |
| `CosSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `CosSnapshotSpec`（implements `...snapshot::SandboxSnapshotSpec`） | 复用 harness `RemoteSnapshotSpec` 包装，与 Java 继承 `RemoteSnapshotSpec` 等价 |
| （Java 无独立类） | 沙箱执行互斥 | `CosSandboxExecutionGuard` | 包装 harness `SandboxExecutionGuard`，保留共享 object store 作为（未来）分布式协调接缝 |
| （Java 依赖 `com.qcloud.cos.COSClient`） | 对象存储后端 | `CosObjectStore`（async trait）+ `InMemoryCosObjectStore` | 见 §2.1、§4.2 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.qcloud.cos:COSClient`（腾讯云 COS SDK） | `getObjectMetadata`/`getObject`/`putObject`/`deleteObject`/`listObjects` | 自有 async trait `CosObjectStore`（`get/put/delete/exists/len/list_prefix` + UTF-8 便捷方法）；测试用 `InMemoryCosObjectStore` | 未引入第三方 Rust COS SDK；真实适配器待办（§4.2） |
| `io.agentscope.core.state.AgentStateStore` | 状态存储 trait | `agentscope_core::state::AgentStateStore` | §2.1 裁定：实现它，不重建 |
| `io.agentscope.core.state.ListHashUtil` | list 增量 hash | `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`） | **已收敛（2026-09-19）**：删除本 crate 平行实现，`save_list` 直接委托 core；`sha2` 依赖已移除。详见 §2.2 / §3.2 / §4.1 |
| `io.agentscope.core.util.JsonUtils` | JSON 编解码 | `serde_json` | |
| `harness...remote.store.BaseStore` / `StoreItem` | 远程 KV trait | `agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue}` | 实现既有 trait |
| `harness...sandbox.snapshot.{RemoteSnapshotClient,RemoteSnapshotSpec,SandboxSnapshotSpec}` | 快照抽象 | 同名 harness trait | 实现/复用，不重建 |
| `harness...sandbox.SandboxExecutionGuard` | 执行互斥 | 同名 harness 类型 + 本 crate `CosSandboxExecutionGuard` 包装 | |
| `agentscope.harness.agent.DistributedStore` | 组合包 | `agentscope_harness::agent::DistributedStore` | 字段不足（仅 `agent_state_store`+`isolation_scope`）；本 crate 用自有组合 + 投影函数规避 |

### 2.2 list 哈希与增量追加（core 收敛后的事实）

| 项 | 内容 |
|---|---|
| 唯一实现 | `agentscope_core::state::list_hash` — `compute_hash` / `has_changed` / `needs_full_rewrite` / `EMPTY_HASH` / `SAMPLING_THRESHOLD`，`agentscope_core::state` 已 `pub use` |
| 本 crate 用法 | `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite};`（`agent_state_store.rs`） |
| 已删除 | 本地 `fn compute_list_hash`（全量 SHA-256，length-prefixed JSON）与本地 `fn needs_full_rewrite` |
| 已删除依赖 | `sha2`（经 `rg` 确认仅旧 `compute_list_hash` 使用，无其他签名/加密用途） |
| 存储布局（不变） | `{prefix}{user}/{session}/{key}.list.json` 存列表实体；`.list.hash` 存**全量**列表的 core 采样哈希（用于下次的「前缀比较」） |
| `existing_count` 语义 | = 已存列表**实体**的元素数，**与是否存在 hash 对象解耦**（Java `existingCount` 同义）；旧代码仅在 `stored_hash.is_some()` 时才读列表长度，会令 core 的 Case 2 永久不可达 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + 依赖 | pom.xml | 已完成 | 仅改本 crate `Cargo.toml`；新增 `sha2`（workspace 已有）——**已于 2026-09-19 随 list_hash 收敛移除**；`tokio` 增 `rt-multi-thread` feature；未改根 `[workspace.dependencies]` |
| list 增量哈希收敛到 core | `ListHashUtil.needsFullRewrite` | 已完成 | **2026-09-19**：删除本地 `compute_list_hash`（全量 SHA-256）+ 本地 `needs_full_rewrite`，改 `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite}`；`save_list` 调用点同步收敛；`existing_count` 改为「已存列表实体长度」，与 hash 对象解耦；`sha2` 依赖移除（grep 确认无其他用途）。验证：31 passed / 0 failed + 2 doctests |
| 修复：纯追加不再恒判全量重写 | `ListHashUtil.needsFullRewrite` Case 3 | 已完成 | 旧条件 `!compute_list_hash(values).starts_with(stored_hash)` 依赖「全量 SHA-256 具有前缀性质」——**实测该假设错误**（`sha256([a])` 与 `sha256([a,b])` 互不为前缀），故条件恒 `true`，纯追加永远判需全量重写（数据无损，追加优化失效）。已实测旧实现 `old_needs_full_rewrite([a,b], old_hash([a]), 1) == true`；新实现 `needs_full_rewrite == false`（core 取 `existing_count` 前缀哈希比较） |
| 修复：`stored_hash == None` 误判 | `ListHashUtil.needsFullRewrite` Case 2 | 已完成 | 旧实现 `None => true`；core 语义为 `stored_hash.is_none() && existing_count > 0` 才 `true`，即无既有数据（`existing_count == 0`）时**不**重写 |
| 对象存储后端抽象 | `COSClient` | 已完成 | `CosObjectStore` async trait + `InMemoryCosObjectStore`；真实 COS SDK 适配待办（§4.2） |
| key 布局 / 前缀归一化 | `CosAgentStateStore`/`CosBaseStore` 私有方法 | 已完成 | `layout.rs`：`normalize_prefix` / `session_prefix` / `namespace_prefix` / `normalize_user` / `validate_snapshot_id` |
| `AgentStateStore` 实现 | `CosAgentStateStore` | 已完成 | `CosAgentStateStore`（builder + `Arc<dyn>` 便捷函数）；`exists`/`delete`/`list_session_ids` 基于 prefix list |
| `BaseStore` 实现（CAS/版本） | `CosBaseStore` | 已完成 | `CosStore`；version 从 0 起、成功写入 +1；`put_if_version(ns,key,v,0)`=仅当不存在时写入；写路径 async mutex 串行化 |
| 快照 client + spec | `CosRemoteSnapshotClient`/`CosSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`；spec 包裹 harness `RemoteSnapshotSpec` |
| 沙箱执行 guard | （Java 无） | 已完成 | `CosSandboxExecutionGuard` 包装 harness guard |
| 组合者 | `CosDistributedStore` | 已完成 | `CosDistributedStore::create` + 子前缀 `state/`/`store/`/`snapshot/` + `to_harness_distributed_store()` |
| · 单元测试 | 760 行测试 | 已完成 | **31** lib unit tests + 2 doctests = 33 passed / 0 failed（含 CAS、分页、namespace 隔离、快照往返、guard 重入，及 2026-09-19 新增的 7 个 list_hash 收敛回归测试） |

### 3.2 后续（P1/P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实 COS SDK 适配器 | `COSClient` 构造 | 待办 | 需 bucket/region/credential + HTTP 签名（自实现或接第三方）；实现 `CosObjectStore` 即可接入 |
| ~~`ListHashUtil` 精确对齐~~ | `ListHashUtil` | **已完成（2026-09-19）** | 已收敛到 `agentscope_core::state::list_hash`，本地平行实现删除，详细见 §2.2 / §3.1 / §4.1。**残留 PS2**：`.list.hash` 不再与 Java 旧值二进制兼容（core 采样策略 vs 旧 SHA-256）；跨语言共读同一 bucket 时首次写入会自愈（见 §4.2 第 5 条） |
| 分布式跨进程执行互斥 | （Java 亦为进程内） | 设计决定（不迁移） | harness guard 为进程内；跨进程协调未在 Java 实现，Rust 亦不做 |

## 4. 项目状态与下一步
### 4.1 当前状态
**已完成**：COS 模块全部产出契约落地并复用既有 trait；list 增量哈希已收敛到 core（无平行实现）。
- 文件：`src/{lib,error,layout,object_store,agent_state_store,base_store,snapshot,guard,distributed_store}.rs`
- 验证命令与结果（2026-09-19 重跑）：
  - `cargo build -p agentscope-extensions-cos` → **通过**（exit 0；无本 crate 错误）
  - `cargo test -p agentscope-extensions-cos` → **通过**：**31 lib unit tests + 2 doctests = 33 passed / 0 failed / 0 ignored**
  - `rg "fn compute_list_hash|fn needs_full_rewrite|starts_with\(h\)" crates/agentscope-extensions-cos` → **无结果**（exit 1，平行实现已清空）
- 本次新增的回归测试（`agent_state_store::tests`）：
  | 测试 | 断言 |
  |---|---|
  | `pure_append_does_not_trigger_full_rewrite` | `[a]` → `[a,b]`：`needs_full_rewrite == false`；同时断言旧的 `!whole.starts_with(stored_hash)` **恒为 true**（直接证明旧假设错误）；并验证追加后数据与 `.list.hash` 均正确 |
  | `identical_list_is_not_rewritten` | 同长且未变：`CountingStore` 计数证明仅写 `.list.hash`（2 次 → 3 次），列表实体不重写 |
  | `appended_prefix_hash_stays_valid` | `[a,b]` → `[a,b,c]` 纯追加不重写 |
  | `modified_element_triggers_full_rewrite` | 前缀内元素被改 → 重写 |
  | `shrink_triggers_full_rewrite` | 列表缩短 → 重写 |
  | `empty_existing_state_does_not_force_rewrite` | 无既有数据 + 无 hash → 不重写；有既有数据 + 无 hash → 重写（Case 2） |
  | `legacy_state_without_hash_self_heals` | 旧 SHA-256 hash 遗留 + 新写入 → 列表重写、hash 被 core 值覆盖，数据无损 |
- 旧行为实测（一次性探针，已删除）：旧 `needs_full_rewrite([a,b], old_hash([a]), 1) == true`（纯追加被误判为全量重写）；新实现同输入为 `false`。
- 已知问题：无编译错误；真实 COS SDK 未接入（后端抽象 + 内存实现）。

### 4.2 下一步与阻塞
1. **真实 COS SDK 适配器**：实现 `CosObjectStore`（bucket/region/secretId/secretKey + 请求签名 + 分页 list）。当前所有组件已按该 trait 编写，接入后无需改动上层。
2. ~~`ListHashUtil` 精确对齐~~ → **已完成（2026-09-19）**，见 §2.2 / §3.1。
3. **跨模块依赖（需汇总方处理）**：
   - harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`），COS 的 `base_store`/`snapshot_spec`/`execution_guard` 无法装入 → 本 crate 用自有组合 + 投影规避；建议扩展 harness（与汇总 tracker §2.2 一致）。
   - 根 `Cargo.toml` 无需新增 workspace 依赖；本次反而**移除了**本 crate 的 `sha2` 声明（根 `[workspace.dependencies]` 的 `sha2` 未动，也无其他 crate 受影响）。
   - **无需汇总方回填的跨模块拆分**：COS 本次仅删除本 crate 内部平行实现，与 MONGODB/REDIS/JDBC/OSS 各自的 list_hash 收敛互不影响（各自 crate 自洽）。
4. `CosSandboxExecutionGuard` 目前仅进程内语义；如需跨进程分布式互斥，需汇总方评估存储层协调方案。
5. **存量数据自愈性说明（无数据迁移脚本）**：存量 COS bucket 中 `.list.hash` 为旧 SHA-256 值。收敛后首次 `save_list` 时，core `needs_full_rewrite` 的 Case 3 会拿「新列表 `existing_count` 前缀的 core 哈希」与「旧 SHA-256 值」比较 → 必然不等 → 判定需全量重写 → 列表实体被当前 `values` 覆盖（数据以调用方传入为准，**无损**），`.list.hash` 随即被 core 值覆盖，后续写入恢复正常增量语义。若 `.list.json` 存在但 `.list.hash` 丢失：新代码读到 `existing_count > 0` 且 `stored_hash == None` → Case 2 命中（需重写），同样一步自愈。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-cos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-cos` / `cargo test -p agentscope-extensions-cos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-cos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

