# agentscope-core

AgentScope Rust 的核心：原生异步 agent 运行时。包含 agent 抽象（`ReActAgent` / `Agent` / `CallableAgent`）、
消息模型、模型 provider 与 HTTP 传输**抽象**、工具系统（`Tool` / `Toolkit` / `ToolGroup`）、middleware、
memory、状态（`AgentState` + 版本 CAS）、journal 与 tracing。

它不依赖 harness、任何 `extensions-*`、任何具体 provider 或存储后端；这些都在上层 crate 里装配。
只想直接上手、不自己拼装时，用门面 crate [`agentscope-rs`](https://crates.io/crates/agentscope-rs)。

## 安装

```toml
[dependencies]
agentscope-core = "0.2.0"
```

要求 Rust 1.89+（见 `rust-version`）。

## 最小用法

```rust
use std::sync::Arc;
use agentscope_core::agent::ReActAgent;
use agentscope_core::message::{JsonMap, Msg, ToolResultBlock};
use agentscope_core::tool::{FunctionTool, ToolExecutionContext, Toolkit};

let toolkit = Toolkit::new();
toolkit.register_tool(Arc::new(FunctionTool::new(
    "add",
    "Adds two integers.",
    serde_json::json!({"type":"object","properties":{
        "a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
    Box::new(|args: JsonMap, _ctx: ToolExecutionContext| {
        let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
        let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(ToolResultBlock::text(format!("{} + {} = {}", a, b, a + b)))
    }),
)));

let agent = ReActAgent::builder()
    .name("tool-agent")
    .model(model) // `Arc<dyn Model>`；具体 provider 在 agentscope-extensions-model
    .toolkit(toolkit)
    .max_iters(3)
    .build()?;

let reply = agent.call(vec![Msg::user("What is 3 + 4?")]).await?;
println!("{}", reply.text_content());
```

可直接运行的版本（含脚本化模型，不需要 API Key）在 `examples/tool_agent_demo`：

```bash
cargo run -p agentscope-core --example tool_agent_demo
```

## 相关 crate

| crate | 说明 |
| --- | --- |
| [`agentscope-rs`](https://crates.io/crates/agentscope-rs) | 门面：按 feature 重导出整个 workspace，`core` + `harness` 默认开启 |
| [`agentscope-harness`](https://crates.io/crates/agentscope-harness) | 装配层：workspace、文件系统、沙箱、subagent、memory、gateway |
| [`agentscope-extensions-model`](https://crates.io/crates/agentscope-extensions-model) | 具体模型 provider（OpenAI / Anthropic / DashScope / Gemini / DeepSeek / Ollama） |
| [`agentscope-durable`](https://crates.io/crates/agentscope-durable) | 可恢复执行运行时（与本 crate 无依赖关系） |

## 版本注意

`0.2.0` 是硬切换版本：journal 事件与头部格式升到 v2（0.1.x 会话不可读），`session` 相关类型已
全量改名为 `journal` / `thread`。升级前先读
[`docs/MIGRATION_0.2.0.md`](https://github.com/Endlessworld/agentscope-rust/blob/main/docs/MIGRATION_0.2.0.md)。

## License

Apache-2.0
