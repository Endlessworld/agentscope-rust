# agentscope-durable

可恢复执行运行时，与 agent 实现解耦。一个 turn 不是栈帧，而是一台持久状态机：每次状态变化都是
针对单一写者租约的 **fenced commit**，每次外部工具副作用都带执行身份，所以重启后能区分
`applied` / `accepted` / `unknown`，而不是猜。

本 crate 不依赖任何 harness 或模型；把具体 agent 接进来的适配层是
[`agentscope-durable-adapter-agentscope`](https://crates.io/crates/agentscope-durable-adapter-agentscope)。

## 安装

```toml
[dependencies]
agentscope-durable = "0.2.0"
```

## 内容

| 模块 | 说明 |
| --- | --- |
| `runtime` / `session` | turn 状态机、tick 与 settle、取消/暂停语义 |
| `store` | `RecordStore` 抽象与 `FileStore`（文档 + 增量日志 + 键索引） |
| `executor` / `execution_store` | 工具执行身份与结果去重（`applied` / `accepted` / `unknown`） |
| `delegation` / `harness_adapter` | 子代理与 harness 侧的接缝 |
| `event_filter` / `traffic` | 事件过滤与流量计数 |

存储后端分成独立 crate：`agentscope-durable-sqlite`（SQLite）、`agentscope-durable-jdbc`（Postgres 映射）、
`agentscope-durable-objects`（内容寻址 blob，可挂 S3 形状的传输层）。

## 用法

```bash
cargo run -p durable-quickstart           # examples/durable-quickstart
```

端到端说明（租约、围栏、恢复矩阵、存储模型）见
[`docs/DURABLE_QUICKSTART.md`](https://github.com/Endlessworld/agentscope-rust/blob/main/docs/DURABLE_QUICKSTART.md)
与 [`docs/DURABLE_API_DESIGN.md`](https://github.com/Endlessworld/agentscope-rust/blob/main/docs/DURABLE_API_DESIGN.md)。

## 相关 crate

| crate | 说明 |
| --- | --- |
| `agentscope-durable-adapter-agentscope` | core/harness 适配层（`DurableAgent` 一类入口） |
| `agentscope-durable-worker` | resident worker 与 brain/hands 驱动 |
| `agentscope-durable-sqlite` / `-jdbc` | `RecordStore` 的 SQLite / Postgres 实现 |
| `agentscope-durable-{http,mcp,rocketmq,sandbox}` | 各执行/事件传输的适配 |

## License

Apache-2.0
