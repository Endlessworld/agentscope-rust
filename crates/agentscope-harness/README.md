# agentscope-harness

AgentScope Rust 的装配层：`HarnessAgent` 把一个 `agentscope-core` 的 agent 接上 workspace、
文件系统抽象（Local / Overlay / Composite / Sandbox / Remote）、沙箱后端（Local / Docker）、
工具与 middleware（压缩、权限、skills）、subagent 编排、memory、任务仓库与 gateway 通道。

依赖只有 [`agentscope-core`](https://crates.io/crates/agentscope-core)、
[`agentscope-extensions-model`](https://crates.io/crates/agentscope-extensions-model) 和通用库；
平台/协议扩展（ACP、A2A、RAG、各数据库）在对应的 `agentscope-extensions-*` crate 里。

## 安装

```toml
[dependencies]
agentscope-harness = "0.2.0"
```

需要最小体积时关掉默认 feature：`default-features = false` 会去掉内置 SQLite 索引（`lite`），
这也是 ACP 预编译产物的构建方式。

## 用法

```rust
use agentscope_core::agent::StreamOptions;
use agentscope_core::message::Msg;
use agentscope_harness::agent::HarnessAgent;
use futures::StreamExt;

let agent = HarnessAgent::builder()
    .model(model)              // `Arc<dyn Model>`
    .workspace("./workspace")
    .build()?;

// 事件流：文本增量、工具状态、最终结果（`AgentEvent::AgentResult`）
let mut events = agent.stream_events(
    vec![Msg::user("列一下工作区里的文件")],
    StreamOptions::defaults(),
);
while let Some(event) = events.next().await {
    println!("{:?}", event?);
}
```

不需要流式时用 `HarnessAgent::agent()` 取到底层 `ReActAgent`，直接 `call()` 拿一条 `Msg`。

`HarnessAgent` 的完整能力清单（会话 journal、权限、subagent、压缩、MCP 桥接、gateway 路由）
见仓库 README 的「装配层」一节；可跑的例子：

```bash
cargo run -p agentscope-harness --example session_journal_memory_probe
```

## 相关 crate

| crate | 说明 |
| --- | --- |
| [`agentscope-core`](https://crates.io/crates/agentscope-core) | agent 抽象、消息、工具系统、状态与 journal（本 crate 的基础） |
| [`agentscope-rs`](https://crates.io/crates/agentscope-rs) | 门面：`cargo add agentscope-rs` 默认就带 `core` + `harness` |
| [`agentscope-durable-adapter-agentscope`](https://crates.io/crates/agentscope-durable-adapter-agentscope) | 把本 crate 的 agent 接到 durable 运行时 |
| [`agentscope-extensions-acp`](https://crates.io/crates/agentscope-extensions-acp) | 基于本 crate 的 ACP（Agent Client Protocol）编码智能体 |

## 版本注意

`0.2.0` 是硬切换版本：`HarnessAgent::session()` / `AgentSession` 已删除，会话历史走 journal
（`session_log()` / `session_transcript()`），会话文件格式为 v2（0.1.x 会话不可读）。升级前先读
[`docs/MIGRATION_0.2.0.md`](https://github.com/Endlessworld/agentscope-rust/blob/main/docs/MIGRATION_0.2.0.md)。

## License

Apache-2.0
