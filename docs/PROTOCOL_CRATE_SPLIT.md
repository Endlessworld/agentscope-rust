# PROTOCOL / ACP crate 拆分（2026-10-22）

## 目标

消除「一个 crate 装多个协议、依赖无法隔离」的问题：

1. `agentscope-extensions-protocol` 在 Java 侧是 Maven 聚合模块，实际协议各有独立子模块；
   Rust 侧原先是一个 crate，`reqwest` / `axum` / `harness` / `agent-client-protocol`
   互相污染。
2. `acp` 只是 harness 的一个前端，却让 `agentscope-harness`（以及依赖它的 20 个
   extension crate）无条件编译整个 `agent-client-protocol` SDK。

本次只做**搬迁 + 依赖隔离**，不改变任何协议行为；ACP 也不做 `AgentRuntime` 抽象
（已确认没有「非 HarnessAgent 跑 ACP」的需求）。

## 最终 crate 图

```
agentscope-core
agentscope-harness ──────────────┐   (不再依赖 agent-client-protocol)
  ├── agentscope-extensions-agent-protocol
  └── agentscope-extensions-acp
agentscope-extensions-a2a-client
  └── agentscope-extensions-a2a-server
agentscope-extensions-agui          (core-only; `harness` feature 可选)
agentscope-extensions-chat-completions-web
agentscope-extensions-protocol      (facade，只 re-export)
```

| Java Maven 模块 | Rust crate | 依赖 | 说明 |
| --- | --- | --- | --- |
| `agentscope-extensions-a2a-client` | `agentscope-extensions-a2a-client` | core, serde, serde_json, thiserror, reqwest, tokio, uuid | `model` / `card` / `message` / `client` / `error` |
| `agentscope-extensions-a2a-server` | `agentscope-extensions-a2a-server` | a2a-client, core, axum, async-trait, serde, serde_json, tokio, uuid | `server` + 服务端专属 `card` |
| `agentscope-extensions-agent-protocol` | `agentscope-extensions-agent-protocol` | harness, core, axum, http, async-trait, futures, chrono, serde, serde_json, thiserror, tokio, tracing | 任务生命周期 HTTP+SSE |
| `agentscope-extensions-agui` | `agentscope-extensions-agui` | core, axum, chrono, futures, serde, serde_json, thiserror, tokio, tracing；`harness` 可选 | `HarnessAgent` 的 impl 放在 `harness` feature |
| `agentscope-extensions-chat-completions-web` | `agentscope-extensions-chat-completions-web` | core, chrono, futures, serde, serde_json | 纯转换层 |
| （父 pom，无代码） | `agentscope-extensions-protocol` | 上述 5 个，全部 optional | feature：`a2a` / `agent-protocol` / `agui` / `chat-completions-web` |
| （无 Java 对应） | `agentscope-extensions-acp` | core, harness, extensions-model, agent-client-protocol, async-trait, chrono, serde, serde_json, thiserror, tokio(signal), tracing, tracing-subscriber, uuid | 两个 bin + 46 inline tests + 7 集成 tests |

## 关键改动

### 1. 错误类型按 crate 拆分

原共享 `ProtocolError` 同时服务 A2A 与 agent-protocol。拆分后：

- `agentscope-extensions-a2a-client::error::{A2aError, A2aResult}`
  （Json / Core / Transport / Http / JsonRpc / UnsupportedMethod / InvalidRequest）
- `agentscope-extensions-agent-protocol::error::{AgentProtocolError, AgentProtocolResult}`
  （Json / Core / TaskNotFound / UnsupportedMethod / InvalidRequest / Timeout）
- `agentscope-extensions-a2a-server` 复用 `A2aError`（与 Java `a2a-server` 依赖
  `a2a-client` 一致）。

### 2. A2A 服务端 card 类型下移

`ConfigurableAgentCard` / `TransportProperties` / `AgentDescriptor` / `create_agent_card`
原在 `a2a/card.rs`。拆 crate 后 `impl<T: AgentRunner + ?Sized> AgentDescriptor for T`
触发 orphan rule（trait 在 client、类型参数在 server）。按 Java 分层（`a2a.server.card`）
把服务端 card 构造移到 `agentscope-extensions-a2a-server/src/card.rs`，client 只保留
`AgentCardResolver` / `FixedAgentCardResolver` / `WellKnownAgentCardResolver` / `CardFetcher`。

### 3. ACP 搬迁

- `crates/agentscope-harness/src/acp/**` → `crates/agentscope-extensions-acp/src/**`
- `crates/agentscope-harness/tests/acp_*` → `crates/agentscope-extensions-acp/tests/`
- harness `Cargo.toml` 删除 `agent-client-protocol`、`tracing-subscriber`、`tokio/signal`
  与两个 `[[bin]]`；`pub mod acp` 与根级 re-export 删除。
- `ACP_CONTEXT_WINDOW_TOKENS` 被 `harness_memory_test.rs` 使用；为避免
  `acp → harness` + `harness(dev) → acp` 的 dev 环，测试内本地化为常量。
- feature 透传：`default = ["rusqlite"]` → `agentscope-harness/rusqlite`，`lite` → harness `lite`；
  依赖声明为 `default-features = false`，否则嵌入式构建会偷偷带回 rusqlite。

### 4. feature 统一导致的隐性测试依赖

`agentscope-extensions-agui` 的
`token_usage_events_are_opt_in_and_accumulate` 原断言
`"cumulative":{"inputTokens":20`。该顺序依赖 `serde_json/preserve_order`，而
`preserve_order` 之前由 harness → `agent-client-protocol-schema` 传递开启。
ACP 抽离后不再成立，测试改为解析 JSON 后按字段断言。

## 验证

```text
cargo check --workspace --all-targets                 -> Finished (0 errors)
cargo test  -p agentscope-extensions-a2a-client       -> 39 passed
cargo test  -p agentscope-extensions-a2a-server       -> 18 passed
cargo test  -p agentscope-extensions-agent-protocol   -> 24 passed
cargo test  -p agentscope-extensions-agui             -> 188 passed
cargo test  -p agentscope-extensions-chat-completions-web -> 35 passed
cargo test  -p agentscope-extensions-acp              -> 46 inline + 7 integration passed
cargo check -p agentscope-extensions-acp --no-default-features --features lite --bins -> Finished
cargo tree  -p agentscope-harness                     -> 无 agent-client-protocol（781 -> 668 行）
```

## 兼容性说明

- `agentscope_harness::acp` 路径不再存在；迁移到 `agentscope_extensions_acp`。
- `agentscope_extensions_protocol::ProtocolError` 不再存在；按协议使用
  `A2aError` / `AgentProtocolError`。
- `agentscope_extensions_protocol::a2a::card::{ConfigurableAgentCard, ...}` 变为
  `agentscope_extensions_protocol::a2a::server::card::{...}`。
- `agui` 的 `HarnessAgent` impl 需要 `features = ["harness"]`。
- 全仓已无 `agentscope-extensions-protocol` / `agentscope-harness::acp` 的消费者，
  因此以上路径变更不影响 workspace 内其它 crate。
