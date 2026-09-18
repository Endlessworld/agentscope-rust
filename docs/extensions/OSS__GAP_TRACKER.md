# OSS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **OSS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-oss\`
> **Java 子模块**：(单模块)
> **Rust 目标**：`crates/agentscope-extensions-oss/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-oss` |
| Java 子模块 | (单模块) |
| Java 规模（初测） | 主代码 ~795 行 / 测试 ~290 行 |
| Rust crate | `crates/agentscope-extensions-oss` |
| 迁移波次 | B |
| 当前状态 | 已完成 |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `OssAgentStateStore` | OSS 后端的 `core.state.AgentStateStore`；状态以 JSON 存于 `{prefix}{user}/{session}/{key}.json`，列表另有 `.list.json` + `.list.hash` | `OssAgentStateStore`（`state_store.rs`） | 实现 `agentscope_core::state::store::AgentStateStore`（async_trait）；复用 `core::state::list_hash` 的 `compute_hash`/`needs_full_rewrite` 做增量 append 检测 |
| `OssBaseStore` | OSS 后端的 harness `BaseStore`；`{prefix}{ns...}/{key}.json` + `.version` | `OssStore`（`base_store.rs`） | 实现 `agentscope_harness::filesystem::remote::store::BaseStore`（同步）；CAS/版本语义以 harness 为准（version 从 1 起、`put_if_version(0)`=仅当不存在时写） |
| `OssRemoteSnapshotClient` | OSS 后端的 `RemoteSnapshotClient`；`{prefix}{snapshotId}.tar` | `OssRemoteSnapshotClient`（`snapshot.rs`） | 实现 `agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（async_trait） |
| `OssSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `OssSnapshotSpec`（`snapshot.rs`） | 实现 `SandboxSnapshotSpec`，内部包装 harness `RemoteSnapshotSpec`（id 校验/`RemoteSandboxSnapshot` 接线复用） |
| `OssDistributedStore` | 组合者：state store + base store + snapshot spec（+ guard） | `OssDistributedStore`（`distributed_store.rs`） | 共享同一 `OssClient`，按 `state/`/`store/`/`snapshot/` 子前缀组合；另暴露 `distributed_store()` 产出 harness `DistributedStore`（字段不足见 §4.2） |
| (harness `SandboxExecutionGuard`) | 隔离键执行租约 | `OssSandboxExecutionGuard`（`guard.rs`） | **不新建平行类型**（遵守 EXTENSIONS_GAP_TRACKER §2.1）：透明包装 harness `SandboxExecutionGuard`，语义完全委托 |
| `com.aliyun.oss.OSS` | 阿里云 OSS SDK 客户端 | `OssClient` trait（`client.rs`）+ `InMemoryOssClient` | 后端抽象（put/get/exists/delete/list）；实机后端经 feature `aliyun`（`aliyun-oss-rust-sdk`）提供，见 §4.2 |
| `JsonUtils` / Jackson | JSON 编解码 | `serde_json` | 直接映射 |
| `ListHashUtil` | 列表变更检测 | `agentscope_core::state::list_hash` | 直接复用，无平行实现 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.aliyun.oss:aliyun-sdk-oss:3.18.5` | 阿里云 OSS 对象读写 | `aliyun-oss-rust-sdk`（optional，feature `aliyun`） | 通过 `OssClient` trait 隔离；默认不编译（避免强拉未 CU 验证的 SDK）。实机后端待接入 |
| Jackson (`JsonUtils`) | JSON | `serde` / `serde_json`（workspace） | 已用 |
| `agentscope-core` (provided) | AgentStateStore/ListHash | `agentscope-core`（path 依赖） | 实现其 trait，不重建 |
| `agentscope-harness` (provided) | BaseStore/RemoteSnapshotClient/SandboxSnapshotSpec/Guard | `agentscope-harness`（path 依赖） | 实现其 trait，不重建 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + Cargo 依赖 | pom.xml | 已完成 | 依赖仅写入本 crate `Cargo.toml`；未改根 `[workspace.dependencies]`，未改 core/harness |
| `OssClient` 后端抽象 + `InMemoryOssClient` | `OSS` (SDK) | 已完成 | `client.rs`；put/get/exists/delete/list_page/list_all/shutdown；key 前缀归一 + snapshotId 校验辅助 |
| `OssAgentStateStore` | `OssAgentStateStore` | 已完成 | 实现 `AgentStateStore`（save/save_list/get/get_list/exists/delete/list_session_ids/close）；`__anon__` 默认用户；三后缀 `.json`/`.list.json`/`.list.hash` |
| `OssStore`（`BaseStore`） | `OssBaseStore` | 已完成 | 实现 `BaseStore`（get/put/put_if_version/search/delete）；`search` 按 key 排序分页、排除 `.version`；key 去前导 `/` |
| `OssRemoteSnapshotClient` + `OssSnapshotSpec` | `OssRemoteSnapshotClient`/`OssSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`/`SandboxSnapshotSpec`；`.tar` 布局；内部复用 harness `RemoteSnapshotSpec` |
| `OssDistributedStore` | `OssDistributedStore` | 已完成 | 组合 state/base store + snapshot spec + guard；子前缀 `state/`/`store/`/`snapshot/`；`distributed_store()` 产出 harness 组合 |
| `OssSandboxExecutionGuard` | (harness guard) | 已完成 | 透明包装 harness `SandboxExecutionGuard`（不新建平行类型，遵守 §2.1） |

### 3.2 测试（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AgentStateStore 测试 | `OssAgentStateStoreTest` | 已完成 | `tests/agent_state_store.rs`（15 用例）：builder 校验、key 布局、`__anon__`、单值/列表往返、append 重写、exists、list_session_ids、delete、close |
| BaseStore 测试 | `OssBaseStoreTest` | 已完成 | `tests/base_store.rs`（13 用例）：builder 校验、put/get+version、CAS 命中/未命中/expected=0、search 分页排序、排除 `.version`、delete、去前导 `/`、命名空间隔离 |
| Snapshot 测试 | — | 已完成 | `tests/snapshot.rs`（7 用例）：上传/下载/存在往返、缺失失败、空桶校验、非法 id 拒绝、spec build、persist/restore |
| Guard + DistributedStore 测试 | — | 已完成 | `tests/guard_and_distributed.rs`（6 用例）：guard 委托语义/可重入、空桶校验、子前缀组合、harness `DistributedStore` 组装、`dyn BaseStore` 可用性 |
| 文档测试 | — | 已完成 | `lib.rs` doc example（no_run） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：`agentscope-extensions-oss` 全量实现，产出契约 4 类 trait 实现 + 组合者 + 后端抽象。
- **验证命令与结果**：
  - `cargo build -p agentscope-extensions-oss` → Finished（本 crate 无 error/warning）
  - `cargo test -p agentscope-extensions-oss` → **42 passed / 0 failed**
    - `agent_state_store` 15 ✓ / `base_store` 13 ✓ / `guard_and_distributed` 6 ✓ / `snapshot` 7 ✓ / doc-test 1 ✓
- **文件清单**：
  - `crates/agentscope-extensions-oss/Cargo.toml`（+ feature `aliyun`、optional `aliyun-oss-rust-sdk`）
  - `src/lib.rs`、`src/error.rs`、`src/client.rs`、`src/state_store.rs`、`src/base_store.rs`、`src/snapshot.rs`、`src/guard.rs`、`src/distributed_store.rs`
  - `tests/common/mod.rs`、`tests/agent_state_store.rs`、`tests/base_store.rs`、`tests/snapshot.rs`、`tests/guard_and_distributed.rs`
- **已知问题**：无。默认不编译实机 OSS SDK；`OssClient` trait 面已完整且被测。

### 4.2 下一步与阻塞（需汇总方裁定）
1. **harness `DistributedStore` 字段不足**（与 EXTENSIONS_GAP_TRACKER §2.2 同项）：harness `DistributedStore` 仅 `agent_state_store` + `isolation_scope`，无 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard`。当前 `OssDistributedStore` 自行持有并暴露这些组件，`distributed_store()` 仅能产出含 `agent_state_store` 的 harness 结构。建议扩展到 harness 或明确由各扩展自行组合。
2. **实机阿里云后端**：`aliyun-oss-rust-sdk` 为 optional（feature `aliyun`），默认关闭。是否改为 crates.io 常规依赖 / 改用 `opendal`(oss service) 需汇总方裁定；`OssClient` trait 已隔离后端，切换成本低。
3. **未改根 `[workspace.dependencies]`**：本 crate 的 `aliyun-oss-rust-sdk` 为本地 `[dependencies]` optional，未登记到根；如需统一版本请汇总方处理。
4. **实机 OSS live 验证待凭据**：端到端需真实 OSS endpoint/AK/SK（同类于 MIGRATION_GAP_TRACKER §7.3）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-oss` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-oss` / `cargo test -p agentscope-extensions-oss` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-oss/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

