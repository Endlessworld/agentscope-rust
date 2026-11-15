# 0.2.0 迁移须知

适用对象：从 0.1.x 升级到 0.2.0 的调用方、部署方与会话数据持有者。

0.2.0 是 **硬切换** 版本：不写兼容层、不留 deprecated 别名、不提供旧格式读取器（沿用
`docs/DURABLE_MIGRATION_PLAN.md` §3 的约定）。因此本次升级有两类动作：**改代码**（重命名与删除的 API）
和 **处理数据**（v1 会话文件在 0.2.0 里读不出来）。

## 1. 破坏性变更

| 决策 | 变更 | 对使用者的影响 |
| --- | --- | --- |
| **D4** | 会话日志事件与头部格式升到 **v2**（`JournalEvent::SCHEMA_VERSION = 2`、`SessionHeader::FORMAT_VERSION = 2`） | 0.1.x 写下的会话**不可读**：打开时报 `journal: Unsupported session format/authority`（头部）或 `journal: Unsupported event envelope`（事件）。仓库没有 v1→v2 迁移器，也没有旧格式读取分支。 |
| **D2** | 全量重命名：`session` → `journal` / `thread`（`SessionEvent` → `JournalEvent`、`SessionLog` → `ThreadJournal`、`SessionKey` → `ThreadKey`、`SessionRecorder` → `InvocationRecorder`、`SessionProjection` → `JournalProjection` 等） | `use` 路径与类型名要跟着改；`HarnessAgent::session_log()` 现在返回 `Arc<dyn ThreadJournal>`、`session_key()` 返回 `Option<ThreadKey>`（方法名保留 `session_*`，返回类型全是 journal/thread 类型）。完整对照见 `docs/DURABLE_MIGRATION_PLAN.md` §4。 |
| **D3** | 删除 `AgentSession`、`SessionAnswer`、`InputReceipt` 与 `HarnessAgent::session()` | 依赖这几个 API 的调用点必须改走 journal（`HarnessAgent::session_log()` / `session_transcript()`）或执行宿主路径。 |
| **D6** | 删除 `agentscope-extensions-controlplane` crate 与 `agentscope-rs` 的 `controlplane` feature | 启用该 feature 的构建直接失败（unknown feature）；控制面能力由部署方自备。 |

其余对外可见的变化：

- `ExtAnswer::to_session_answer` → **`ExtAnswer::to_answer_text`**。
- 新增事件类型 `step/resumed`（acting 阶段续跑时写出）；消费事件的一方需要能忽略未知类型，或显式处理它。
- 新增 `Error::HostPaused` 与 `GenerateReason::HostPaused`：暂停是控制流，不是工具错误、也不是失败回复。
- 新增两个 crate：`agentscope-durable`（durable 会话后端）与 `agentscope-durable-adapter-agentscope`（core/harness 适配层）。
- 版本号：workspace 内全部 crate 同步到 `0.2.0`。**内部 path 依赖的版本要求写在各 crate 清单里**，只改根 `[workspace.package] version` 会构建失败（`failed to select a version for the requirement ...`），需要逐个 crate 一起改并重新生成 `Cargo.lock`。

## 2. 升级步骤

1. **先备份数据目录。** 会话日志在 `.agentscope/sessions/<agent>/<user>/<session>/`（旧布局
   `agents/<agent>/sessions/<user>/<session>/` 仍按只读回落读取）；若用 ACP 的 durable 会话，
   scope 文档在 `<workspace>/.agentscope/durable/scopes/`。整份复制 `.agentscope/` 是最省事的做法。
2. **需要保留历史就先导出。** 在 0.1.x 上导出（`JournalExporter` / `JournalExportSink`，或
   `HarnessAgent::session_transcript()` 取 `Transcript`），把导出物存到 0.2.0 读不到的位置之外。
   0.2.0 无法再读 v1，事后补救只能是改二进制回 0.1.x。
3. **升依赖**：所有 `agentscope-*` crate 同步到 `0.2.0`。
4. **改代码**：按 §1 的对照表改 `use` 与调用点，`to_session_answer` → `to_answer_text`。
5. **处理 v1 会话目录**：升级后这些目录只会在打开时返回 `Unsupported session format/authority`。
   要么按步骤 2 导出后删除，要么留着做人工比对的样本——但不要指望 0.2.0 会读它们。
6. **验证**：`cargo check --workspace --all-targets -j 2`、`cargo test --workspace -j 2 --no-fail-fast`，
   并跑一遍自己的会话读/写/重放路径。

## 3. 回退建议

- 0.2.0 写入的会话是 v2，**0.1.x 同样读不了**（它只接受 v1）。所以「回退二进制」等于放弃升级后写入的
  会话历史——这是格式硬切的必然结果，不是缺陷。
- 想保留回退余地：升级前整份复制 `.agentscope/`，回退时**连同目录一起恢复**，而不是只回退二进制。
- 不要用 0.2.0 与 0.1.x 交替跑同一个会话目录。多宿主并存只承诺在**同一版本内**成立（durable 的租约/围栏
  保证同一 thread 只有一个执行写者）；检出到旧版本去读新会话，是最容易制造「看起来丢了历史」的做法。
- 0.2.0 内部可回退的部分：durable 侧的 `FileStore` 是「进程崩溃安全、掉电不安全」（无 `fsync`，取舍见
  `crates/agentscope-durable/src/store/file.rs` 模块文档）。掉电会留下旧文档或空/半写文档，重要部署请自行
  在文件系统层做快照。

## 4. 自查清单

```powershell
rg 'SessionEvent|SessionRecorder|SessionProjection|SessionAnswer|InputReceipt|SessionTurns' src
rg 'to_session_answer|HarnessAgent::session\(' src
rg 'controlplane' Cargo.toml
```

三条都应为空。启动日志里若仍出现 `journal: Unsupported session format/authority`，说明还有 v1 会话目录
被读到，回到 §2 步骤 5 处理。

注意：`SessionKey` **仍有同名类型**——它现在是 durable 的会话键（`agentscope_durable::SessionKey`），
而被改名的是 `core::session::SessionKey` → `agent::ThreadKey`。看到 `SessionKey` 不要当成漏改。

## 5. 相关文档

- `docs/DURABLE_MIGRATION_PLAN.md`：D1–D7 决策、里程碑与有意缺口（含本条迁移的来源）。
- `docs/DURABLE_MIGRATION_REVIEW.md`：发布后审查（缺陷修复链、性能缺口、测试资产）。
- `docs/DURABLE_API_DESIGN.md`：durable 后端与适配层的 API 设计。
