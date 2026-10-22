# COS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **COS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-cos\`
> **Java 子模块**：单模块
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
| Java 规模（实测 2026-10-22） | 主代码 **882 非空行**（5 个类 / 971 总行 / 709 去空去注释行）；测试 **760 非空行**（4 个测试类 / 898 总行 / 62 个 `@Test`） |
| Rust 规模（实测 2026-10-22） | 9 个 `src/*.rs` / **1517 非空行**（1719 总行 / 1266 去空去注释行）；无 `tests/` 目录；**31** 内联 test（19 个 `#[test]` + 12 个 `#[tokio::test]`）+ 2 个 `no_run` doctest（**只编译、不执行**） |
| Rust crate | `crates/agentscope-extensions-cos` |
| 迁移波次 | B |
| 当前状态 | 已完成（后端为内存实现 + async 抽象；真实 COS SDK 适配见 §4.2；**2026-10-22 静态复核确认**） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `CosAgentStateStore` | COS 对象存储上的 `AgentStateStore` | `CosAgentStateStore`（implements `agentscope_core::state::AgentStateStore`） | key 布局对齐：`{prefix}{user}/{session}/{key}.json` / `.list.json` / `.list.hash`；anonymous 用户段 `__anon__`；前缀归一化对齐。**缺口**：Java 额外覆写了 `delete(userId, sessionId, key)`（删 `.json`+`.list.json`+`.list.hash`），Rust core `AgentStateStore` trait **无该方法** → 无法实现（core 层缺口，见 §6 P2-①）；Java `close()` 关闭 `COSClient`，Rust 无 `close`（trait 无） |
| `CosBaseStore` | COS 上的 `BaseStore`（远程 KV） | `CosStore`（implements `agentscope_harness::filesystem::remote::store::BaseStore`） | key 布局：`{prefix}{ns0}/{ns1}/.../{key}.json` / `.version`；version 成功写入 +1；CAS 语义以 harness 为准。**对齐**：Java `CosBaseStore` **未**覆写 `supportsAtomicSessionStorage()`（默认 `false`），Rust `CosStore` 亦未覆写 `supports_atomic_session_storage()`（默认 `false`）→ 一致。**强化**：Rust `put`/`put_if_version`/`delete` 走进程内 `tokio::Mutex` 串行化（Java 无锁，read-check-write 有竞态） |
| `CosDistributedStore` | 组合 agent state / base store / snapshot spec | `CosDistributedStore` | Java 含 `agent_state_store`/`base_store`/`sandbox_snapshot_spec`；Rust 版额外含 `sandbox_execution_guard`，并可 `to_harness_distributed_store()` 投影（harness 结构体字段不足，见汇总 §2.2）。**关键差异**：Java `CosDistributedStore` **未**覆写 `sandboxExecutionGuard()`（继承默认 `noop()`），Rust 版提供**真实** `CosSandboxExecutionGuard`（harness 进程内计数 guard）→ Rust 语义**强于** Java，非缺失。**缺口**：Java `agentStateStore()`/`baseStore()`/`sandboxSnapshotSpec()` 每次调用**新建**实例；Rust 版在 `create()` 时**一次性构造并缓存**（`Arc`）→ 语义等价但少了「每次新实例」的隔离性（无害，见 §6 P2-③） |
| `CosRemoteSnapshotClient` | COS 上的 `RemoteSnapshotClient` | `CosRemoteSnapshotClient`（implements `...snapshot::RemoteSnapshotClient`） | 对象 key：`{prefix}{snapshotId}.tar`；snapshot id 安全校验复用 harness `validate_snapshot_id` 语义。**差异（Rust 更严）**：Java 仅拒绝 blank/null `snapshotId`，Rust 额外拒绝 `/`、`\`、`..`、`\0`；Java `RemoteSnapshotSpec.build` 不校验，Rust 经 harness `RemoteSnapshotSpec::build` 校验 → Rust 是**安全加固**，非缺失（见 §6 P2-④） |
| `CosSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `CosSnapshotSpec`（implements `...snapshot::SandboxSnapshotSpec`） | 复用 harness `RemoteSnapshotSpec` 包装，与 Java 继承 `RemoteSnapshotSpec` 等价 |
| （Java 无独立类） | 沙箱执行互斥 | `CosSandboxExecutionGuard` | 包装 harness `SandboxExecutionGuard`，保留共享 object store 作为（未来）分布式协调接缝 |
| （Java 依赖 `com.qcloud.cos.COSClient`） | 对象存储后端 | `CosObjectStore`（async trait）+ `InMemoryCosObjectStore` | 见 §2.1、§4.2 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.qcloud.cos:COSClient`（腾讯云 COS SDK） | `getObjectMetadata`/`getObject`/`putObject`/`deleteObject`/`listObjects` | 自有 async trait `CosObjectStore`（`get/put/delete/exists/len/list_prefix` + UTF-8 便捷方法）；测试用 `InMemoryCosObjectStore` | 未引入第三方 Rust COS SDK；真实适配器待办（§4.2）。**2026-10-22 静态复核确认**：全 crate `rg 'q-sign-algorithm\|Hmac\|Sha1\|sha1\|secretId'` → **零命中**，即**既无自建签名、也无 SDK 签名**，`q-sign-algorithm` **完全未实现**（§4.2） |
| `io.agentscope.core.state.AgentStateStore` | 状态存储 trait | `agentscope_core::state::AgentStateStore` | §2.1 裁定：实现它，不重建 |
| `io.agentscope.core.state.ListHashUtil` | list 增量 hash | `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`） | **已收敛（2026-09-19，2026-10-22 静态复核确认）**：删除本 crate 平行实现，`save_list` 直接委托 core；`sha2` 依赖已从 `Cargo.toml` 移除。详见 §2.2 / §3.2 / §4.1。**注意**：core 的 `compute_hash` 用 `std::collections::hash_map::DefaultHasher`（SipHash-1-3）而非 Java `String.hashCode()`，故与 Java 侧 hash 值**永不相等**（见 §2.2 新增行） |
| `io.agentscope.core.util.JsonUtils` | JSON 编解码 | `serde_json` | |
| `harness...remote.store.BaseStore` / `StoreItem` | 远程 KV trait | `agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue}` | 实现既有 trait |
| `harness...sandbox.snapshot.{RemoteSnapshotClient,RemoteSnapshotSpec,SandboxSnapshotSpec}` | 快照抽象 | 同名 harness trait | 实现/复用，不重建 |
| `harness...sandbox.SandboxExecutionGuard` | 执行互斥 | 同名 harness 类型 + 本 crate `CosSandboxExecutionGuard` 包装 | |
| `agentscope.harness.agent.DistributedStore` | 组合包 | `agentscope_harness::agent::DistributedStore` | 字段不足（仅 `agent_state_store`+`isolation_scope`）；本 crate 用自有组合 + 投影函数规避。**2026-10-22 复核**：Java `DistributedStore` 是 **interface**（含 `agentStateStore`/`baseStore`/`sandboxSnapshotSpec`/`sandboxExecutionGuard`/`messageBus`/… 9 个方法），Rust harness 侧是 **struct**（仅 2 字段）→ 缺口属实，需汇总方扩展 harness |

### 2.2 list 哈希与增量追加（core 收敛后的事实）

| 项 | 内容 |
|---|---|
| 唯一实现 | `agentscope_core::state::list_hash` — `compute_hash` / `has_changed` / `needs_full_rewrite` / `EMPTY_HASH` / `SAMPLING_THRESHOLD`，`agentscope_core::state` 已 `pub use`（`state/mod.rs:19`） |
| 本 crate 用法 | `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite};`（`agent_state_store.rs:15`） |
| 已删除 | 本地 `fn compute_list_hash`（全量 SHA-256，length-prefixed JSON）与本地 `fn needs_full_rewrite`（**已由 `git show e01809d:.../agent_state_store.rs` 复核确认历史实现确为 `!compute_list_hash(values).starts_with(h)`**） |
| 已删除依赖 | `sha2`（经 `rg` 确认仅旧 `compute_list_hash` 使用，无其他签名/加密用途；`crates/agentscope-extensions-cos/Cargo.toml` 现无 `sha2`；全 workspace 仅 core/controlplane/channel/jdbc/mongodb/rag/harness 仍用 `sha2`，与 COS 无关） |
| 存储布局（不变） | `{prefix}{user}/{session}/{key}.list.json` 存列表实体；`.list.hash` 存**全量**列表的 core 采样哈希（用于下次的「前缀比较」） |
| `existing_count` 语义 | = 已存列表**实体**的元素数，**与是否存在 hash 对象解耦**（Java `existingCount` 同义）；旧代码仅在 `stored_hash.is_some()` 时才读列表长度，会令 core 的 Case 2 永久不可达 |
| 跨语言 hash 值 | **不兼容（确认）**：Java `computeHash` = `"size:N;"` + `"i:item.hashCode(),"` → `Integer.toHexString(sb.hashCode())`（**Java `String.hashCode`**，32 位）；Rust core `compute_hash` = 同构采样串 → `DefaultHasher`（**SipHash-1-3，64 位**）。两者算法骨架同源、**哈希函数不同** → 同一列表两侧 hash 值**永不相等**。存量自愈路径见 §4.2 第 5 条 |
| ⚠️ 语义偏离（残留 P1） | core `needs_full_rewrite` Case 3 比较的是「**当前列表 `existing_count` 前缀**的哈希」与「**存储的整表哈希**」。首次写入时 `stored_hash` 存的是**整表**哈希，故第 2 次写入（`existing_count` = 整表长）时前缀 = 整表 → 相等 → 追加判定正确；但**纯追加后**若再追加一次，`stored_hash` 是上一次的整表哈希，而前缀只取到旧长度 → **必然不等 → 判全量重写**。即 **core 与 Java 同款「第二次追加仍全量重写」的行为问题**（Java `ListHashUtil.needsFullRewrite` 同样如此）。本 crate 已忠实委托 core，故与 Java 行为**一致**，非本 crate 缺陷（core 层议题，见 §6 P1-①） |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + 依赖 | pom.xml | 已完成 | 仅改本 crate `Cargo.toml`；新增 `sha2`（workspace 已有）——**已于 2026-09-19 随 list_hash 收敛移除**；`tokio` 增 `rt-multi-thread` feature；未改根 `[workspace.dependencies]` |
| list 增量哈希收敛到 core | `ListHashUtil.needsFullRewrite` | 已完成 | **2026-09-19**：删除本地 `compute_list_hash`（全量 SHA-256）+ 本地 `needs_full_rewrite`，改 `use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite}`；`save_list` 调用点同步收敛；`existing_count` 改为「已存列表实体长度」，与 hash 对象解耦；`sha2` 依赖移除（grep 确认无其他用途）。验证：**31 unit test passed / 0 failed**（19 × `#[test]` + 12 × `#[tokio::test]`）+ 2 个 `no_run` doctest（只编译不执行）。**2026-10-22 静态复核确认**：`rg 'fn compute_list_hash\|fn needs_full_rewrite'` 全 crate → 零命中；`Cargo.toml` 无 `sha2`（`git log -S "sha2"` → 提交 `daa700d`）→ 收敛属实 |
| 修复：纯追加不再恒判全量重写 | `ListHashUtil.needsFullRewrite` Case 3 | 已完成 | 旧条件 `!compute_list_hash(values).starts_with(stored_hash)` 依赖「全量 SHA-256 具有前缀性质」——**实测该假设错误**（`sha256([a])` 与 `sha256([a,b])` 互不为前缀），故条件恒 `true`，纯追加永远判需全量重写（数据无损，追加优化失效）。已实测旧实现 `old_needs_full_rewrite([a,b], old_hash([a]), 1) == true`；新实现 `needs_full_rewrite == false`（core 取 `existing_count` 前缀哈希比较）。**2026-10-22 静态复核确认**：该回归断言现以 `agent_state_store.rs::pure_append_does_not_trigger_full_rewrite` 内联形式保留（同时断言旧 `!whole.starts_with(stored_hash)` 恒为 `true`）。**注意**：core 只修到「单次追加不判重写」，**连续第二次追加**仍会因「存整表哈希 vs 比前缀哈希」而判全量重写（与 Java 同款，见 §2.2 P1-①） |
| 修复：`stored_hash == None` 误判 | `ListHashUtil.needsFullRewrite` Case 2 | 已完成 | 旧实现 `None => true`；core 语义为 `stored_hash.is_none() && existing_count > 0` 才 `true`，即无既有数据（`existing_count == 0`）时**不**重写 |
| 对象存储后端抽象 | `COSClient` | 已完成 | `CosObjectStore` async trait + `InMemoryCosObjectStore`；真实 COS SDK 适配待办（§4.2） |
| key 布局 / 前缀归一化 | `CosAgentStateStore`/`CosBaseStore` 私有方法 | 已完成 | `layout.rs`：`normalize_prefix` / `session_prefix` / `namespace_prefix` / `normalize_user` / `validate_snapshot_id` |
| `AgentStateStore` 实现 | `CosAgentStateStore` | 已完成 | `CosAgentStateStore`（builder + `Arc<dyn>` 便捷函数）；`exists`/`delete`/`list_session_ids` 基于 prefix list |
| `BaseStore` 实现（CAS/版本） | `CosBaseStore` | 已完成 | `CosStore`；version 从 0 起、成功写入 +1；`put_if_version(ns,key,v,0)`=仅当不存在时写入；写路径 async mutex 串行化 |
| 快照 client + spec | `CosRemoteSnapshotClient`/`CosSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`；spec 包裹 harness `RemoteSnapshotSpec` |
| 沙箱执行 guard | （Java 无） | 已完成 | `CosSandboxExecutionGuard` 包装 harness guard |
| 组合者 | `CosDistributedStore` | 已完成 | `CosDistributedStore::create` + 子前缀 `state/`/`store/`/`snapshot/` + `to_harness_distributed_store()` |
| · 单元测试 | 760 行测试 | 已完成 | **31** lib unit tests（19 × `#[test]` + 12 × `#[tokio::test]`）+ 2 个 `no_run` doctest（只编译不执行）。分布：`agent_state_store` 12（7 tokio + 5 test）、`base_store` 5、`distributed_store` 5（1 tokio + 4 test）、`guard` 3、`snapshot` 5（3 tokio + 2 test）、`object_store` 1（tokio）。**对照 Java 62 个 `@Test` → 覆盖度约 50%**（Java 用 Mockito 精确断言 COS 请求；Rust 走 `InMemoryCosObjectStore` 抽象层，无法断言 HTTP 细节），差异清单见 §6 P1-② |

### 3.2 后续（P1/P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实 COS SDK 适配器 | `COSClient` 构造 | 待办 | 需 bucket/region/credential + HTTP 签名（自实现或接第三方）；实现 `CosObjectStore` 即可接入。**2026-10-22 静态复核**：全 crate 检索 `q-sign-algorithm` / `Hmac` / `Sha1` / `sha1` / `secretId` → **零命中**（仅 3 处注释提到 `com.qcloud.cos.COSClient` 与「HTTP signing」为待办）。→ **既无 SDK 也无自建签名**，签名工作量**完全未开始**。腾讯云 COS v5 签名需 `q-sign-algorithm=sha1` + HMAC-SHA1 派生链（依赖 `hmac`/`sha1` crate，workspace 根 `Cargo.toml` **当前无** `sha1`/`hmac`） |
| ~~`ListHashUtil` 精确对齐~~ | `ListHashUtil` | **已完成（2026-09-19）** | 已收敛到 `agentscope_core::state::list_hash`，本地平行实现删除，详细见 §2.2 / §3.1 / §4.1。**残留 PS2**：`.list.hash` 不再与 Java 旧值二进制兼容（core 采样策略 vs 旧 SHA-256；且 core 用 `DefaultHasher`（SipHash-1-3）而 Java 用 `String.hashCode()` → 两侧值**永不相等**，见 §2.2「跨语言 hash 值」行）；跨语言共读同一 bucket 时首次写入会自愈（见 §4.2 第 5 条） |
| 分布式跨进程执行互斥 | （Java 亦为进程内） | 设计决定（不迁移） | harness guard 为进程内；跨进程协调未在 Java 实现，Rust 亦不做 |

## 4. 项目状态与下一步
### 4.1 当前状态
**已完成**：COS 模块全部产出契约落地并复用既有 trait；list 增量哈希已收敛到 core（无平行实现）。
- 文件（**9 个**，与 §1 规模行一致）：`src/{lib,error,layout,object_store,agent_state_store,base_store,snapshot,guard,distributed_store}.rs`
  - 非空行分布：`agent_state_store` 474、`base_store` 308、`distributed_store` 197、`snapshot` 147、`guard` 120、`object_store` 114、`layout` 85、`lib` 52、`error` 20 → **合计 1517**
- 验证命令与结果（2026-09-19 重跑）：
  - `cargo build -p agentscope-extensions-cos` → **通过**（exit 0；无本 crate 错误）
  - `cargo test -p agentscope-extensions-cos` → **通过**：**31 lib unit tests + 2 doctests = 33 passed / 0 failed / 0 ignored**
    - **2026-10-22 静态复核修正**：31 个 unit test 中 **19 个 `#[test]` + 12 个 `#[tokio::test]`**（按 `#[test]` / `#[tokio::test]` 逐文件统计：`agent_state_store` 5+7、`base_store` 5+0、`distributed_store` 4+1、`guard` 3+0、`snapshot` 2+3、`object_store` 0+1 → 19+12=31）；2 个 doctest 均为 **`no_run`**（`lib.rs:27`、`agent_state_store.rs:32`）→ **只编译不执行**，故「33 passed」的表述应为「31 实际执行 + 2 仅编译」
  - `rg "fn compute_list_hash|fn needs_full_rewrite|starts_with\(h\)" crates/agentscope-extensions-cos` → **无结果**（exit 1，平行实现已清空）——**2026-10-22 复核重跑仍为 exit 1，确认属实**
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
- **2026-10-22 静态复核新增已知问题**：
  - `Cargo.toml` 声明 `serde` / `futures` / `chrono` 三个依赖，但 crate 内 `rg '\bserde\b'` → **零命中**（仅用 `serde_json`，未用 `serde` derive）、`rg 'futures'` → **零命中**、`rg 'chrono'` → 2 处命中但均为 `base_store.rs:80/85` 英文注释中的 "**synchronous**" 子串（非 `chrono` crate 调用）→ 三个依赖为**冗余声明**，可在真实适配器落地时清理（P2）
  - `CosError::Backend` / `CosError::NotFound` 两个变体**从未被构造**（全 crate 仅 `Config` / `Serialization` 有构造点）→ 预留变体（P2）
  - `object_store`（根 `[workspace.dependencies]`，`0.14`）已声明但**全 workspace 无 crate 引用**，且不在 `Cargo.lock` 中 → 根依赖声明与实际不符（P2，需汇总方处理）

### 4.2 下一步与阻塞
1. **真实 COS SDK 适配器**：实现 `CosObjectStore`（bucket/region/secretId/secretKey + 请求签名 + 分页 list）。当前所有组件已按该 trait 编写，接入后无需改动上层。
   - **2026-10-22 静态复核结论**：**签名未实现**（全 crate `rg 'q-sign-algorithm|Hmac|Sha1|sha1|secretId'` → 零命中，仅注释提及「HTTP signing」为待办）。COS v5 签名算法为 `q-sign-algorithm=sha1`，需 HMAC-SHA1 多级派生 + `q-sign-time` 有效期 + Canonical 请求串拼接；**当前既无第三方 SDK（workspace 无 `sha1`/`hmac` 依赖）也无自建实现** → 该项实为**从零开始**
   - 另需覆盖：分页 `list`（Java `listAllKeys` 用 marker 循环，Rust `list_prefix` 一次返回全量 → 真实适配器需在 trait 内实现 marker 循环）、批量删除（Java `deleteObjects` 1000/批，Rust `delete` 为逐键删除）
2. ~~`ListHashUtil` 精确对齐~~ → **已完成（2026-09-19）**，见 §2.2 / §3.1。
3. **跨模块依赖（需汇总方处理）**：
   - harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`），COS 的 `base_store`/`snapshot_spec`/`execution_guard` 无法装入 → 本 crate 用自有组合 + 投影规避；建议扩展 harness（与汇总 tracker §2.2 一致）。**2026-10-22 复核确认**：Java `DistributedStore` 是 **interface**，`HarnessAgent` 会用 `distributedStore.baseStore()` 自动注入 `RemoteFilesystemSpec`/`StoreBackedPeriodicGate`/`SkillUsageStore`；Rust harness 侧 `DistributedStore` 仅 2 字段 → **COS 的 `base_store` 目前无法经 `HarnessAgent::builder().distributed_store(...)` 自动接线**，只能由调用方手动 `RemoteFilesystem::new(store, ...)`（**P1，汇总方**）
   - 根 `Cargo.toml` 无需新增 workspace 依赖；本次反而**移除了**本 crate 的 `sha2` 声明（根 `[workspace.dependencies]` 的 `sha2` 未动，也无其他 crate 受影响）。**2026-10-22 复核**：根 `[workspace.dependencies]` 中的 `object_store = "0.14"` **全 workspace 无任何 crate 引用**且不在 `Cargo.lock` → 建议汇总方清理
   - **无需汇总方回填的跨模块拆分**：COS 本次仅删除本 crate 内部平行实现，与 MONGODB/REDIS/JDBC/OSS 各自的 list_hash 收敛互不影响（各自 crate 自洽）。
4. `CosSandboxExecutionGuard` 目前仅进程内语义；如需跨进程分布式互斥，需汇总方评估存储层协调方案。**2026-10-22 复核补充**：Java `CosDistributedStore` **未**覆写 `sandboxExecutionGuard()`（用 harness 默认 `noop()`），故 Rust 提供真实 guard 属**增强**；但 Java `SandboxExecutionGuard` 是 `@FunctionalInterface`（`tryEnter` **阻塞**至获得槽位），Rust harness 是**具体 struct**（仅可重入计数、不阻塞、不跨进程）→ 即便接上 Rust guard 也**达不到 Java 的阻塞语义**（汇总 tracker 已裁定「harness guard 缺口确认」，此处仅作 COS 侧记录）
5. **存量数据自愈性说明（无数据迁移脚本）**：存量 COS bucket 中 `.list.hash` 为旧 SHA-256 值。收敛后首次 `save_list` 时，core `needs_full_rewrite` 的 Case 3 会拿「新列表 `existing_count` 前缀的 core 哈希」与「旧 SHA-256 值」比较 → 必然不等 → 判定需全量重写 → 列表实体被当前 `values` 覆盖（数据以调用方传入为准，**无损**），`.list.hash` 随即被 core 值覆盖，后续写入恢复正常增量语义。若 `.list.json` 存在但 `.list.hash` 丢失：新代码读到 `existing_count > 0` 且 `stored_hash == None` → Case 2 命中（需重写），同样一步自愈。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-cos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-cos` / `cargo test -p agentscope-extensions-cos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-cos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **静态核对，零编译、零测试执行**（`cargo build` / `cargo test` 全程未运行）。
- 手段：逐文件通读 Java 5 个主类 + 4 个测试类 / Rust 9 个 `src/*.rs`；`rg` 检索符号与依赖；`git show e01809d:...` 取历史实现复核旧行为；`git log -S "sha2"` 复核依赖收敛。
- 复核者：静态复核子智能体（只读 + 仅改本 tracker）。

### 6.2 实测统计（静态口径）

| 项 | Java | Rust |
|---|---|---|
| 主代码文件 | 5 个 `.java` | 9 个 `src/*.rs`（无 `tests/` 目录） |
| 主代码总行 | 971 | 1719 |
| 主代码非空行 | **882** | **1517** |
| 主代码去空去注释行 | 709 | 1266 |
| 测试文件 | 4 个测试类 | 0（全部内联于 `src/`） |
| 测试总行 | 898 | — |
| 测试非空行 | **760** | — |
| 测试用例数 | **62 个 `@Test`**（无 `@ParameterizedTest`/`@TestFactory`） | **31**（19 × `#[test]` + 12 × `#[tokio::test]`） |
| doctest | — | **2 个，均为 `no_run`**（`lib.rs:27`、`agent_state_store.rs:32`）→ 只编译不执行 |

- Rust 31 test 分布：`agent_state_store` 12、`base_store` 5、`distributed_store` 5、`snapshot` 5、`guard` 3、`object_store` 1、`layout` 0、`error` 0、`lib` 0。
- Java 62 `@Test` 分布：`CosAgentStateStoreTest` 20、`CosRemoteSnapshotClientTest` 18、`CosBaseStoreTest` 16、`CosDistributedStoreTest` 8。
- **行数/类数核对结论**：Java 主 882 / 测 760 与 rollup 一致（**属实**）；Rust 9 文件 / 1517 非空行 / 31 test 属实（rollup 记「31+2」中的 2 为 doctest，口径需注明为 `no_run`）。

### 6.3 对齐结论表

| Java 类 → Rust 对应 | 存在？ | 行为覆盖判定 |
|---|---|---|
| `CosAgentStateStore` → `CosAgentStateStore` | ✅ | key 布局（`.json`/`.list.json`/`.list.hash`）、`__anon__`、前缀归一化（`\`→`/`、去首 `/`、补尾 `/`）、blank 默认值 **全部对齐**；`save_list` 的 hash/追加判定已委托 core。**缺 `delete(user,session,key)` 与 `close()`**（core trait 无此二方法） |
| `CosBaseStore` → `CosStore`（`BaseStore`） | ✅ | `{ns}/{key}.json` + `.version`、version 0→+1、`put_if_version(0)`=create-if-absent、`search` 排序+offset/limit、key 去前导 `/` **全部对齐**；`supports_atomic_session_storage` 两边均 `false` **一致**；Rust 额外加进程内 mutex（强化） |
| `CosDistributedStore` → `CosDistributedStore` | ✅ | 子前缀 `state/`/`store/`/`snapshot/` 对齐；默认前缀 `agentscope/` 对齐。差异：Java 每次访问新建实例，Rust 构造时缓存（无害）；Rust 多提供真实 guard（Java 为 `noop()` 默认，**增强**） |
| `CosRemoteSnapshotClient` → `CosRemoteSnapshotClient` | ✅ | `{prefix}{id}.tar` 对齐；prefix 归一化对齐；Rust 额外做 `/`、`\`、`..`、`\0` 校验（**加固**） |
| `CosSnapshotSpec` → `CosSnapshotSpec` | ✅ | 均为「包裹 harness `RemoteSnapshotSpec`」结构，语义等价 |
| （Java 无）→ `CosSandboxExecutionGuard` | ✅ | 包装 harness guard，保留 `try_enter`/`release`/`is_held`/`active_count`；**未**实现跨进程协调（已记为设计决定不迁移） |
| `com.qcloud.cos.COSClient` → `CosObjectStore` + `InMemoryCosObjectStore` | ⚠️ | 抽象层齐备，**真实后端完全缺失**（含签名） |

### 6.4 新发现缺口

| 编号 | 优先级 | 缺口 | 归属 |
|---|---|---|---|
| P1-① | P1 | **core `needs_full_rewrite` 连续第二次追加仍判全量重写**：存的是整表哈希、比的是 `existing_count` 前缀哈希 → 追加一次后 `stored_hash`=旧整表哈希，下次比前缀（较短）必不等。**Java `ListHashUtil` 同款问题**，故本 crate 与 Java 一致、非本 crate 缺陷 | core（汇总方） |
| P1-② | P1 | **测试覆盖缺口**：31 test vs Java 62 `@Test`（≈50%）。Java 用 Mockito 精确断言 COS 请求（`putObject` 次数、key 字面量、`deleteObjects` 批量）；Rust 走 `InMemoryCosObjectStore` 抽象层，**无法断言 HTTP/签名层细节**。Java 独有的 `deleteSingleKey_removesStateListAndHash`、`builderRejectsNullClient/BlankBucket`、`close_shutsDownClient`、`putStripsLeadingSlashFromKey`、`emptyNamespace_usesPrefixOnly`、`nullPrefix_treatedAsEmpty`、`prefixWithLeadingSlash_isStripped` 等**在 Rust 侧无对应断言** | 本 crate（P1：可在内存后端上补 key 字面量级断言） |
| P1-③ | P1 | **`base_store` 无法经 `HarnessAgent::builder().distributed_store(...)` 自动接线**：harness `DistributedStore` 仅 2 字段，而 Java `HarnessAgent` 会用 `distributedStore.baseStore()` 自动注入 `RemoteFilesystemSpec`/`StoreBackedPeriodicGate`/`SkillUsageStore` | harness（汇总方，汇总 tracker §2.2 已裁定） |
| P0 | — | **无新增 P0**。核心链路（state / base store / snapshot / guard / 组合者）产出契约齐全且行为对齐 | — |
| P2-① | P2 | Java `AgentStateStore.delete(user,session,key)`（删 3 类对象）与 `close()` 在 Rust core trait 中**不存在** → 本 crate 无法实现 | core（汇总方） |
| P2-② | P2 | `Cargo.toml` 冗余依赖：`serde` / `futures` / `chrono` 声明但**零使用**（`rg` 确认） | 本 crate |
| P2-③ | P2 | `CosError::Backend` / `CosError::NotFound` 变体从未被构造（仅 `Config`/`Serialization` 有构造点） | 本 crate |
| P2-④ | P2 | snapshot id 校验 Rust 比 Java **更严**（Java 仅拒 blank）→ 加固，非缺失，仅需在文档中明示 | 已在本文件 §2 明示 |
| P2-⑤ | P2 | 根 `[workspace.dependencies]` 的 `object_store = "0.14"` **全 workspace 无 crate 引用**且不在 `Cargo.lock` | 汇总方 |
| P2-⑥ | P2 | 错误传播语义差异：Java `save`/`get`/`getList` 失败抛 `RuntimeException`；Rust `AgentStateStore` trait 返回 `()` → Rust 侧 `tracing::error!` 吞掉错误（`agent_state_store.rs:134/172/178`） | core trait 签名决定 |
| P2-⑦ | P2 | 真实适配器还需覆盖 Java 的分页 `listObjects`（marker 循环）与批量 `deleteObjects`（1000/批），Rust `CosObjectStore` 契约（单次 `list_prefix` 全量、逐键 `delete`）与之不等价 | 本 crate trait 设计（§4.2 已登记） |

### 6.5 勘误列表（原文 → 实际）

| # | 原文 | 实际 | 处置 |
|---|---|---|---|
| 1 | §4.1「**31** lib unit tests + 2 doctests = **33 passed**」 | 31 = **19 × `#[test]` + 12 × `#[tokio::test]`**；2 个 doctest 均为 **`no_run`**（只编译不执行）→ 实际执行 **31**，非 33 | §4.1 已改写并注明口径 |
| 2 | §1「**31** 内联 test + 2 doctests」 | 数字正确，但未区分 `#[test]`/`#[tokio::test]`，且未说明 doctest 为 `no_run` | §1 已补明细 |
| 3 | §4.1「31 passed / 0 failed + 2 doctests」（§3.1 单元测试行） | 同上 | §3.1 已改写 |
| 4 | §2.1「`ListHashUtil` → `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`）」 | 正确，但**遗漏** core `compute_hash` 用 `DefaultHasher`（SipHash-1-3）而 Java 用 `String.hashCode()` → 两侧 hash 值**永不相等**（跨语言不兼容） | §2.1 + §2.2 已补「跨语言 hash 值」行 |
| 5 | §3.1「修复：纯追加不再恒判全量重写」表述为已彻底修复 | 属实（单次追加已修），但**连续第二次追加**仍判全量重写（core Case 3 结构所致，Java 同款） | §2.2 + §3.1 已补限定 + P1-① |
| 6 | §3.2「真实 COS SDK 适配器 → 需 … HTTP 签名（自实现或接第三方）」 | 表述偏轻：实际**既无 SDK 也无自建签名**，且 workspace 无 `sha1`/`hmac` 依赖 → **从零开始** | §3.2 + §4.2 第 1 条已改写 |
| 7 | §2 映射表未记录 Java `CosDistributedStore` 的 `sandboxExecutionGuard()` 用 harness 默认 `noop()` | 已核实（Java 该类**无** `sandboxExecutionGuard` 覆写）→ Rust 提供真实 guard 属**增强**，非缺失 | §2 映射表已补 |
| 8 | §2 映射表未记录 Java `CosBaseStore` 未覆写 `supportsAtomicSessionStorage()` | 已核实（Java `BaseStore` 默认 `false`，`CosBaseStore` 未覆写）→ Rust 亦 `false`，**一致** | §2 映射表已补 |
| 9 | §4.2 第 3 条「harness `DistributedStore` 字段不足」表述笼统 | 已核实 Java 侧 `HarnessAgent` 会用 `distributedStore.baseStore()` **自动接线**多处 → Rust 侧丧失该自动接线能力 | §4.2 第 3 条已补 + P1-③ |
| 10 | §4.1「已知问题：无编译错误；真实 COS SDK 未接入」 | 遗漏 3 项：① `serde`/`futures`/`chrono` 冗余依赖 ② `CosError::Backend`/`NotFound` 死变体 ③ 根 `object_store` 依赖全局无引用 | §4.1「已知问题」已补 |
| 11 | §3.2「`ListHashUtil` 精确对齐」残留 PS2 表述 | 「跨语言共读同一 bucket 时首次写入会自愈」**属实**（core Case 3 必不等 → 全量重写 → hash 被覆盖）；但需补充「**两侧 hash 值永不相等**」的根因说明 | §2.2 已补 |

### 6.6 复核结论

- **§4.2「真实 SDK 适配」现状判定**：Rust 侧为**自建抽象 trait**（`CosObjectStore`），**非**自建签名、**非**第三方 SDK；`q-sign-algorithm` 签名**完全未实现**（静态可判定：`rg 'q-sign-algorithm|Hmac|Sha1|sha1|secretId'` 全 crate 零命中）。
- **`list_hash` 收敛到 core**：**属实**。本 crate 无平行实现（`rg 'fn compute_list_hash|fn needs_full_rewrite'` → 零命中），`sha2` 已从 `Cargo.toml` 移除（`git log -S "sha2"` 确认提交 `daa700d`）。
- **「追加恒全量重写」偏离修复**：**属实**。历史实现经 `git show e01809d:.../agent_state_store.rs` 确认为 `!compute_list_hash(values).starts_with(h)`，全量 SHA-256 无前缀性质 → 恒 `true`；现委托 core 后单次纯追加返回 `false`（回归测试内联保留）。
- **B 波次统一产出契约**：**5/5 齐全**（`CosDistributedStore` / `CosAgentStateStore` / `CosStore` / `CosSnapshotSpec` / `CosSandboxExecutionGuard`），且均可从 `CosDistributedStore` 访问（`agent_state_store()`/`base_store()`/`sandbox_snapshot_spec()`/`sandbox_execution_guard()` + `shared_*` trait-object 便捷方法）。
- **建议 rollup 状态值**：**保持 `已完成`**（rollup 行 12 现值「已完成（build + test 通过，31+2 test；…）」）。理由：P0 核心链路无缺口、产出契约 5/5 齐备、list_hash 收敛与追加偏离修复均经静态复核属实；剩余项均为 P1/P2 且其中 P1-①/P1-③ 属 core/harness 层（汇总方）、P1-② 属测试覆盖增强而非功能缺失。**唯一需在 rollup 同步的措辞修正**：「31+2 test」应表述为「**31 unit test（19 `#[test]` + 12 `#[tokio::test]`）+ 2 `no_run` doctest（只编译）**」，并追加「**真实 COS SDK 适配（含 `q-sign-algorithm` 签名）仍未开始**」。


## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。