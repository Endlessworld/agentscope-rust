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
| `io.agentscope.core.state.ListHashUtil` | list 增量 hash | 本 crate 内 `compute_list_hash` / `needs_full_rewrite` | core 无对应公开 API；用等价稳定 hash（见 §4.2） |
| `io.agentscope.core.util.JsonUtils` | JSON 编解码 | `serde_json` | |
| `harness...remote.store.BaseStore` / `StoreItem` | 远程 KV trait | `agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue}` | 实现既有 trait |
| `harness...sandbox.snapshot.{RemoteSnapshotClient,RemoteSnapshotSpec,SandboxSnapshotSpec}` | 快照抽象 | 同名 harness trait | 实现/复用，不重建 |
| `harness...sandbox.SandboxExecutionGuard` | 执行互斥 | 同名 harness 类型 + 本 crate `CosSandboxExecutionGuard` 包装 | |
| `agentscope.harness.agent.DistributedStore` | 组合包 | `agentscope_harness::agent::DistributedStore` | 字段不足（仅 `agent_state_store`+`isolation_scope`）；本 crate 用自有组合 + 投影函数规避 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + 依赖 | pom.xml | 已完成 | 仅改本 crate `Cargo.toml`；新增 `sha2`（workspace 已有），`tokio` 增 `rt-multi-thread` feature；未改根 `[workspace.dependencies]` |
| 对象存储后端抽象 | `COSClient` | 已完成 | `CosObjectStore` async trait + `InMemoryCosObjectStore`；真实 COS SDK 适配待办（§4.2） |
| key 布局 / 前缀归一化 | `CosAgentStateStore`/`CosBaseStore` 私有方法 | 已完成 | `layout.rs`：`normalize_prefix` / `session_prefix` / `namespace_prefix` / `normalize_user` / `validate_snapshot_id` |
| `AgentStateStore` 实现 | `CosAgentStateStore` | 已完成 | `CosAgentStateStore`（builder + `Arc<dyn>` 便捷函数）；`exists`/`delete`/`list_session_ids` 基于 prefix list |
| `BaseStore` 实现（CAS/版本） | `CosBaseStore` | 已完成 | `CosStore`；version 从 0 起、成功写入 +1；`put_if_version(ns,key,v,0)`=仅当不存在时写入；写路径 async mutex 串行化 |
| 快照 client + spec | `CosRemoteSnapshotClient`/`CosSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`；spec 包裹 harness `RemoteSnapshotSpec` |
| 沙箱执行 guard | （Java 无） | 已完成 | `CosSandboxExecutionGuard` 包装 harness guard |
| 组合者 | `CosDistributedStore` | 已完成 | `CosDistributedStore::create` + 子前缀 `state/`/`store/`/`snapshot/` + `to_harness_distributed_store()` |
| 单元测试 | 760 行测试 | 已完成 | 26 个测试全部通过（含 CAS、分页、namespace 隔离、快照往返、guard 重入） |

### 3.2 后续（P1/P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 真实 COS SDK 适配器 | `COSClient` 构造 | 待办 | 需 bucket/region/credential + HTTP 签名（自实现或接第三方）；实现 `CosObjectStore` 即可接入 |
| `ListHashUtil` 精确对齐 | `ListHashUtil` | 待办 | 当前用等价稳定 hash；跨语言可加载性未验证 |
| 分布式跨进程执行互斥 | （Java 亦为进程内） | 设计决定（不迁移） | harness guard 为进程内；跨进程协调未在 Java 实现，Rust 亦不做 |

## 4. 项目状态与下一步
### 4.1 当前状态
**已完成**：COS 模块全部产出契约落地并复用既有 trait。
- 文件：`src/{lib,error,layout,object_store,agent_state_store,base_store,snapshot,guard,distributed_store}.rs`
- 验证命令与结果：
  - `cargo build -p agentscope-extensions-cos` → **通过**（exit 0；无本 crate 错误）
  - `cargo test -p agentscope-extensions-cos` → **通过**：24 lib unit tests + 2 doctests = 26 passed / 0 failed
- 已知问题：无编译错误；真实 COS SDK 未接入（后端抽象 + 内存实现）。

### 4.2 下一步与阻塞
1. **真实 COS SDK 适配器**：实现 `CosObjectStore`（bucket/region/secretId/secretKey + 请求签名 + 分页 list）。当前所有组件已按该 trait 编写，接入后无需改动上层。
2. **`ListHashUtil` 精确对齐**（如需跨语言 `.list.hash` 兼容）。
3. **跨模块依赖（需汇总方处理）**：
   - harness `DistributedStore` 字段不足（仅 `agent_state_store`+`isolation_scope`），COS 的 `base_store`/`snapshot_spec`/`execution_guard` 无法装入 → 本 crate 用自有组合 + 投影规避；建议扩展 harness（与汇总 tracker §2.2 一致）。
   - 根 `Cargo.toml` 无需新增 workspace 依赖（`sha2` 已存在）。
4. `CosSandboxExecutionGuard` 目前仅进程内语义；如需跨进程分布式互斥，需汇总方评估存储层协调方案。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-cos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-cos` / `cargo test -p agentscope-extensions-cos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-cos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

