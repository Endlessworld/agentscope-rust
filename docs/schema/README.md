# Durable 协议 JSON-Schema

`crates/agentscope-durable` 的跨语言数据契约。每个对外数据形状一份 `*.schema.json`（draft 2020-12），配一份合法样例与一份非法样例。**这些文件是文档与契约，不是运行时校验器**：Rust 侧校验仍由 `Commit::validate` / `json::text` / `clamp_limit` 等完成，schema 只负责让 Java、其它宿主与 CI 能在不链接 Rust 的前提下检查同一份字节。

## 目录

```
docs/schema/
  index.json                  # 全部 schema + 版本号 + 四条硬约束索引
  README.md
  verify.ps1                  # 本机校验：JSON 可解析 + schema 自检 + 样例对错
  *.schema.json               # 14 份形状契约
  examples/<name>.valid.json
  examples/<name>.invalid.json
```

## 版本策略

| 版本 | 值 | 何时提升 |
| --- | --- | --- |
| `protocolVersion` | `durable.v1` | `types/commit.rs::PROTOCOL_VERSION`，线上一致；语义不兼容变更才提升 |
| `schemaSetVersion` | `durable.schema.v1` | 任一份 schema 发生**破坏性**变更（字段改名、枚举删值、收紧约束）时提升；只新增可选字段不提升 |
| `checkpointSchemaVersion` | `1` | `CHECKPOINT_SCHEMA_VERSION`，checkpoint 布局变化时提升 |
| 事件信封 | `schemaVersion: "1"` | durable 公开事件信封；与 core journal 信封（`JournalEvent::SCHEMA_VERSION = 2`）是两个独立计数器，别混用 |

规则：

- **只增不删**。新增可选字段 = 兼容变更，`schemaSetVersion` 不动；删除字段、改字段名、从枚举里删值、收紧 `minLength`/`maximum` = 破坏性变更，必须提升 `schemaSetVersion` 并在此表留一行历史。
- schema 的 `additionalProperties: false` 是**契约立场**，不是 Rust 行为：serde 默认忽略未知字段，但跨语言互通时未知字段通常意味着版本不一致，应当报错而不是静默丢弃。
- draft 2020-12 无法表达跨字段算术（如 `failures <= policy.maxRetries`、`maxDelayMillis >= initialDelayMillis`、`revision == expectedRevision + 1`）。这些规则以 `$comment` / `description` 标注，由运行时保留执行；**不要**为了在 schema 里表达它们而编造字段。

## 与 Rust / Java 的对应

| Schema | Rust | Java |
| --- | --- | --- |
| `session-key.schema.json` | `types/identity.rs::SessionKey` | `DurableTypes.SessionKey` |
| `thread.schema.json` | `types/identity.rs::{ThreadView, ThreadStateView}` | `DurableTypes.ThreadView` / `ThreadState` |
| `turn-view.schema.json` | `types/turn.rs::TurnView` | `DurableTypes.TurnView` |
| `turn-data.schema.json` | `backend/model.rs::TurnData`（持久化记录，非公开视图） | 无同名视图 |
| `run-view.schema.json` | `types/run.rs::RunView` | `DurableTypes.RunView` |
| `execution-view.schema.json` | `types/execution.rs::ToolExecution` | `DurableTypes.ToolExecution` |
| `commit.schema.json` | `types/commit.rs::Commit` | `DurableTypes.Commit` |
| `receipt.schema.json` | `types/commit.rs::CommitReceipt`、`types/turn.rs::{TurnReceipt, CommandReceipt}` | 同名三类回执 |
| `event.schema.json` | `types/commit.rs::PublicEvent` | `DurableTypes.PublicEvent` |
| `resource-value.schema.json` | `types/resource.rs::ResourceValue` | `DurableTypes.ResourceValue` |
| `work-item.schema.json` | `store.rs::WorkItem`（落盘为 `store/file.rs::WorkRecord`） | 就绪索引条目 |
| `blob.schema.json` | `store/file.rs::blob_reference`（布局同 `agentscope-durable-objects`） | Java file store 的 blob 引用 |
| `page.schema.json` | `session_backend.rs` 的 `limit` 形参 + `backend/mod.rs::clamp_limit` | 分页请求 |
| `frame.schema.json` | `types/commit.rs::Frame` | `DurableTypes.Frame` |

## 序列化约定（schema 已固化，照抄即互通）

- **字段名 camelCase**：全部结构体 `#[serde(rename_all = "camelCase")]`。
- **枚举值 SCREAMING_SNAKE_CASE**：`TurnStatus::Waiting` → `"WAITING"`。
- **可选即省略**：Rust 用 `skip_serializing_if = "Option::is_none"`，所以「没有值」表现为**字段缺失**，不是 `null`；schema 里这类字段都不在 `required` 中，也没有 `null` 类型。唯一例外是 `output`/`input`/`checkpoint` 这类 `Value` 字段，它们的默认值就是 `null`。
- **大整数以字符串传输**：`sequence`、`cursor`、`CheckpointMetadata.position` 都是十进制字符串（避免 JS 侧 2^53 精度丢失）。
- **事件 kind 的线上字段名是 `type`**（`#[serde(rename = "type")]`）。文档里说的 `kind` 就是它；不要新增一个 `kind` 字段。
- **`executionId` 是派生值**：`"<turnId>:<callId>"`，由 `ToolCall::execution_id` 生成，宿主不要自造。

## 四条硬约束

1. **`execution_id` 上限 513** — `EXECUTION_ID_MAX_CHARS = 256 + 1 + 256`（`types/execution.rs`）。落在 `execution-view.schema.json`、`commit.schema.json`（`dispatches`/`results`/`appliedResults`）、`turn-view.schema.json`/`turn-data.schema.json`/`frame.schema.json` 的 `parent.executionId`：`maxLength: 513`。反例 `examples/execution-view.invalid-long-id.json`（514 字符）。
2. **事件 kind 枚举含 required 扩展事件** — `event.schema.json::$defs.KnownKind` 列了后端自己写的 8 种 `turn.*` 与 `durable/finalization_started` / `durable/finalization_completed`；这两个是 core `JournalEvent.required` 语义的扩展事件，未知 codec 必须停止恢复。宿主自定义 kind 由同一 `anyOf` 的第二支放行（`Commit.events` 仅拒绝 `session./thread./run./step.` 前缀）。
3. **分页 `limit` 1..=1000** — `page.schema.json::limit` 为 `minimum: 1, maximum: 1000`；`clamp_limit` 对 0 和 1001 都是 `INVALID`（不 clamp）。`ResourceRequest.limit` 同样 ≤1000。
4. **`$blob` 引用形状** — `blob.schema.json`：`$blob` 必须是唯一键，内部恰好 `digest`（64 hex）、`bytes`（整数）、`kind`（const `"json"`）。`$blob` 字段多一个键，存储层就按普通数据处理（`blob_reference_of` 的 `map.len() != 1` / `reference.len() != 3` 分支）。

## 如何校验

最轻量（只需要 PowerShell，无第三方依赖）：

```powershell
Get-ChildItem docs/schema -Recurse -Filter *.json | ForEach-Object {
  Get-Content $_.FullName -Raw | ConvertFrom-Json | Out-Null
}
```

或用仓库自带的校验脚本（解析 + schema 自检 + 样例对错）：

```powershell
pwsh -File docs/schema/verify.ps1
```

用 `ajv`（Node）：

```bash
npx ajv-cli validate --spec=draft2020 \
  -s docs/schema/execution-view.schema.json \
  -d docs/schema/examples/execution-view.valid.json
# 期望：valid；把 -d 换成 execution-view.invalid.json 应报 maxLength/const 失败
```

用 Python `jsonschema`：

```python
import json, pathlib
from jsonschema import Draft202012Validator
schema = json.loads(pathlib.Path("docs/schema/commit.schema.json").read_text(encoding="utf-8"))
Draft202012Validator.check_schema(schema)
Draft202012Validator(schema).validate(json.loads(pathlib.Path("docs/schema/examples/commit.valid.json").read_text(encoding="utf-8")))
```

注意 `receipt` / `thread` 用 `oneOf`，一个文档必须**恰好**匹配一支；`event` 的 kind 用 `anyOf`（已知枚举 + 宿主自定义），所以已知 kind 不会因为两支都匹配而失败。

## 样例与违规点

| 非法样例 | 违反的约束 |
| --- | --- |
| `session-key.invalid.json` | `sessionId` 为空串 → `minLength: 1`；Rust 侧对应 `json::text` 的「必须含 1..256 个字符」 |
| `thread.invalid.json` | `checkpointMetadata.position` 是 `"042"` → 违反 `^[1-9][0-9]*$`；`CheckpointMetadata::verify` 明确拒绝前导零 |
| `turn-view.invalid.json` | `recovery.retryable = false` 却带 `retryAt` → `RecoveryView::validate` 要求「可重试 ⟺ retryAt 存在」 |
| `turn-data.invalid.json` | 缺 `cancel` → 违反持久化记录的 `required`；缺字段的 scope 不是一条合法 turn 记录 |
| `run-view.invalid.json` | `epoch = -1` → 违反 `minimum: 0`（fencing 计数不可能为负） |
| `execution-view.invalid.json` | `result.state = "NOT_FOUND"` → 存储态枚举不含它；`ToolResultRecord::validate` 报「NOT_FOUND is not a recordable execution result」 |
| `execution-view.invalid-long-id.json` | `executionId` 514 字符 → 违反 `maxLength: 513` |
| `commit.invalid.json` | `status = "PAUSING"` → `Commit::validate` 拒绝 QUEUED/PAUSING/CANCELLING 作为提交目标 |
| `receipt.invalid.json` | `revision = 0` → 违反 `minimum: 1`；回执是提交**之后**的 revision，首次提交即 1 |
| `event.invalid.json` | `sequence = "0"` → 违反 `^[1-9][0-9]*$`；会话内序号从 1 开始 |
| `resource-value.invalid.json` | `version = 0` → 违反 `minimum: 1`；资源版本从 1 开始，0 只出现在请求的 `expectedVersion`（create-only） |
| `work-item.invalid.json` | `readyAt` 是字符串 `"1760000000000"` → 违反 `integer`；就绪时间必须是 store 时钟的整数毫秒 |
| `blob.invalid.json` | `$blob` 内多出 `encoding` → 违反 `additionalProperties: false`；存储层会把它当普通数据处理，而不是引用 |
| `page.invalid.json` | `limit = 1001` → 违反 `maximum: 1000`；Rust 侧 `clamp_limit` 报 `Page limit must be 1..=1000` |
| `frame.invalid.json` | `now` 是字符串 → 违反 `integer`；worker 不得用自己的时钟做租约判断 |

合法样例是每种形状的一个真实形态（例如 `commit.valid.json` 覆盖 `tools`/`dispatches`/`results`/`appliedResults`/`modelCalls`/`journal` 同时在场的提交）。

## 未覆盖的形状

- `SessionSpec` / `SessionView` / `SessionPage`、`ThreadPage`：同一批 identity 视图，字段已被 `session-key` / `thread` 覆盖，暂未单独出文件。
- `StepView` / `ModelCallView` / `ModelRequestView`：只作为 `turn-data` 的 `$defs` 出现（它们是 turn 内部事实的投影），未单独出文件。
- `ControlCommand` / `Signal` / `Lease` / `Restoration` / `DelegationTarget` / `Snapshot` / `EventPage` / `HistoryPage` / `OutputPage` / `CheckpointView`：调用方向或恢复向的形状，本批未出文件。
- `EventInput` / `LiveEvent` 的区别（前者进提交、后者是最佳努力实时信号）：只在 `commit` 的 `$defs.EventInput` 里给出。

补这些形状时沿用同一约定：一份形状一个文件、`$id` 用 `https://agentscope.dev/schema/durable.v1/<file>`、配一对样例并登记到 `index.json`。
