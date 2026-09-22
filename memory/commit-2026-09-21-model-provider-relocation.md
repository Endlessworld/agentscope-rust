# 2026-09-21：core model provider 整体迁入 extensions-model（含文档同步）

## 事实（代码）
提交 **`d86a578`**（分支 main，20 files/171+/96-）完成目录重构：
- `crates/agentscope-core/src/model/{openai.rs,responses.rs,anthropic.rs}` → 整体迁至
  `crates/agentscope-extensions-model/src/{openai/mod.rs(1045 行), responses/mod.rs(620 行), anthropic_async/mod.rs(653 行)}`
- `core/examples/chat_demo.rs` → `extensions-model/examples/chat_demo.rs`
- core `model/mod.rs` + `lib.rs` 移除 re-export；**core 不再导出 `default_registry` / `OpenAiChatModel` / `OpenAiProvider`**
  （实测全仓 `fn default_registry` 仅存在于 `extensions-model/src/openai/mod.rs:300`）
- harness `src/acp/coding_agent.rs` 改 `use agentscope_extensions_model::OpenAiChatModel`
- 依赖下沉：`async-openai` 0.41 / `async-anthropic` 0.6 / `reqwest13`(别名) 由 core 移入
  `extensions-model/Cargo.toml`；同时移除该 crate 未使用的 `agentscope-harness` 依赖
- core `src/model/` 文件数 13 → **10**；extensions-model `src/` 31 文件 8146 行 → **34 文件 10473 行**
- 实测：`cargo test -p agentscope-extensions-model` = **198 passed / 0 failed**（原 169，迁入 +29）；
  `cargo check --workspace` = Finished（仅其它 crate 存量 warning）

> ⚠️ **MEMORY.md 已过期**：MEMORY.md「非存储侧模块 → MODEL」段仍写
> 「§2.2 已裁定：core `model/openai.rs` 已用 async-openai，core 定 trait + 注册表，本 crate 补 provider 适配，**不重复实现 openai**」。
> 该裁定的「**归属 core**」前提已作废（原意「不重复实现」仍有效）。**以本文件为准。**
> 同理 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.2 该行已标注「2026-09-21 部分被取代」。

## 文档同步批次
1. **`b5f4d92`** — `docs/extensions/MODEL__GAP_TRACKER.md`（41+/28-）：§1 状态、§1.1 基线 169→198、
   §1.2 文件清单 31/8146→34/10473、§2.3 由「设计决定（不迁移）」改「已迁移（自 core 迁入）」并加状态口径变更块、
   §3.1/§3.2/§3.3 数字与依赖清理、§3.4 理由改写，§4.1 补实测命令、§4.2 第 6 条标完成。
2. **`0e2e5e1`** — docs/** 全量同步（6 文件/15+/13-）：
   `EXTENSIONS_GAP_TRACKER.md` §2.2、`MIGRATION_GAP_TRACKER.md`（core.model 文件数、§2.2/§2.3 位置、§7.2 default_registry 归属）、
   `MIGRATION_PLAN.md` §4.2（依赖下沉）、`MULTI_ENDPOINT_PLAN.md`（顶部变更说明 + 三实现路径 + §6 影响面）、
   `ACP_AGENT_PLAN.md` §5、`TRAINING__GAP_TRACKER.md` §2。

## 刻意未改（勿误当遗漏）
- `memory/**` 与 `.agents/context/**` 下的**时间戳历史记录**（如 `memory/2026-09-18-*.md`、
  `.agents/context/history/*`、`.agents/context/state/session.md`）：属当时快照，改写即伪造历史。
- `README.md`：`d86a578` 已同步（`README.md:13` 已写明「全部具体 provider（openai / responses / anthropic /
  dashscope / gemini / ollama）实现在 `agentscope-extensions-model`」）。
- `crates/agentscope-harness/examples/README.md:41`：只提类型名未提 crate 路径，不算陈旧。
- `docs/CODEX_AGENT_COMPARISON.md` / `PROTOCOL__*` / `RAG__*` / `STUDIO__*` 的 openai 命中与位置无关（wire 模型/embedding/telemetry 映射）。

## 新发现的待裁定项（已记入 MODEL tracker §4.2 第 7 条）
**`anthropic` 双实现并存**：`extensions-model` 现同时有
`crate::anthropic`（§2.4 自建 wire DTO + `HttpTransport`，58 单测）与迁入的
`crate::anthropic_async`（基于 `async-anthropic` 0.6 SDK，653 行）。功能重叠 + 多一套依赖，
需裁定保留哪条（或主路径 + 另一条标 deprecated）。
