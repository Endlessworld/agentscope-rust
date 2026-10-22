# JUDGE 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **JUDGE** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-judge\`
> **Java 子模块**：`agentscope-extensions-jev`（68 类）、`agentscope-extensions-openai-decisions`（8 类）
> **Rust 目标**：`crates/agentscope-extensions-judge/`
> **建立缘由**：本模块此前**未在 rollup 与 `EXTENSIONS_MIGRATION_PLAN.md` 登记**；2026-10-22 经 §4.5 裁定 4 **纳入迁移范围**。本文件为首次建立（静态复核基线，未编译验证）。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览

| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-judge` |
| Java 子模块 | `jev`（System One 判定框架，68 类）、`openai-decisions`（OpenAI 决策 API 客户端，8 类） |
| Java 规模（实测 2026-10-22，非空行） | 主代码 **12469**（jev 11674 + openai-decisions 795）/ 测试 **4035**（15 文件 / **140 个 `@Test`**） |
| Rust crate | `crates/agentscope-extensions-judge` |
| Rust 规模（实测 2026-10-22） | **9** 个 `src/*.rs` / **3181** 行 / **57** 个测试（`#[test]`，无 `#[tokio::test]`、无 `tests/`、无 doc-test） |
| 是否注册 workspace | **是**（`Cargo.toml` 的 `crates/agentscope-extensions-judge`） |
| 当前状态 | **进行中**（仅 DTO + 纯函数；HTTP 客户端与 8 个子包未迁移） |

### 1.1 Rust crate 文件清单

| 文件 | 行数 | 测试 | 职责 |
|---|---|---|---|
| `src/lib.rs` | 31 | 0 | crate 根：`pub mod error` / `pub mod jev` |
| `src/error.rs` | 132 | 2 | `JevError` |
| `src/jev/mod.rs` | 246 | 5 | 模块聚合 + 共享类型 |
| `src/jev/dto.rs` | 437 | 5 | Answer / Question / Usage / SystemOne 请求响应 DTO |
| `src/jev/execution.rs` | 291 | 4 | `JevExecution` |
| `src/jev/guardrail.rs` | 550 | 7 | `JevGuardrail` |
| `src/jev/probability.rs` | 152 | 8 | 概率/评分工具 |
| `src/jev/review.rs` | 581 | 9 | `JevReview` 相关 |
| `src/jev/validation.rs` | 761 | 17 | 校验/评估编排 |

> `Cargo.toml` 仅依赖 `serde` / `serde_json` / `thiserror`；注释自述 `agentscope-core` / `agentscope-harness` / `reqwest` **刻意未声明**，因为 HTTP 客户端尚未落地。

## 2. Java 清单与依赖映射

### 2.1 jev 子包规模（实测）

| 子包 | 文件数 | 行数 | Rust 对应 | 状态 |
|---|---|---|---|---|
| `<root>`（Answer/Question/Choice/Score/SystemOne/JevClient 等） | 18 | 1451 | `jev/dto.rs`（部分）、`jev/mod.rs` | **部分完成** |
| `application`（Planner/Selector/Context/CustomerSupport/DraftPipeline/MemoryGate/Rag/StageRouter/Supervisor/TeamPlanner） | 10 | 950 | 无 | **待办 P1** |
| `browser`（Navigator/ReadTool/Session/PlaywrightSession） | 4 | 1056 | 无 | **待办 P2**（依赖 Playwright，可考虑「设计决定（不迁移）」） |
| `context`（CompactionTranscript/FileJevContextArchive/JevContextArchive/JevContextCompactor/StoreJevContextArchive） | 5 | 903 | 无 | **待办 P1** |
| `evaluation`（Evaluator/JevEvaluationRunner/JevEvaluator 等） | 12 | 1828 | 无（`validation.rs` 仅覆盖部分校验） | **待办 P1** |
| `evidence` | 3 | 744 | 无 | **待办 P1** |
| `example` | 4 | 1130 | 无 | **待办 P2**（示例） |
| `integration` | 3 | 607 | 无 | **待办 P1** |
| `review` | 4 | 1332 | `jev/review.rs`（581 行） | **部分完成** |
| `routing` | 2 | 818 | 无 | **待办 P1** |
| `supervision` | 3 | 855 | 无 | **待办 P1** |
| `openai-decisions`（DecisionQuestion/Input/Answer/Request/Result/Usage/Client/Exception） | 8 | 795 | 无 | **待办 P1（整块缺失）** |

### 2.2 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp / `java.net.http` | `JevClient` / `OpenAIDecisionsClient` HTTP | **未引入** | 建议复用 `reqwest`（workspace） |
| Jackson | JSON（反）序列化 | `serde` + `serde_json` | 已落地 |
| Playwright（`browser` 子包） | 浏览器自动化 | 无 | 与 SANDBOX/CHANNEL 同级判定：**建议「设计决定（不迁移）」**，保留 trait 扩展点 |
| Reactor `Mono` | 异步流 | 未使用（当前全同步纯函数） | HTTP 客户端落地时改用 `async_trait` + `tokio` |
| Slf4j | 日志 | `tracing` | 未声明 |

## 3. 详细缺口清单

### 3.1 P0（核心链路）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| HTTP 客户端 | `JevClient`（root）、`OpenAIDecisionsClient` | **待办 P0** | 当前 crate 无任何网络能力，`JevError` 也无 HTTP/Api 变体 |
| System One 请求/响应契约 | `SystemOneRequest` / `SystemOneResult` | **进行中** | `jev/dto.rs` 有 DTO，但需逐字段对照确认（含 `Usage` 语义） |

### 3.2 P1（重要）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `openai-decisions` 整块 | 8 类 / 795 行 | **待办** | 无对应模块 |
| `evaluation` 子包 | 12 类 / 1828 行 | **待办** | Rust `validation.rs` 不等价 |
| `application` 子包 | 10 类 / 950 行 | **待办** | Planner/Supervisor/StageRouter 等编排层 |
| `context` 子包 | 5 类 / 903 行 | **待办** | 上下文压缩与归档 |
| `evidence` 子包 | 3 类 / 744 行 | **待办** | — |
| `routing` 子包 | 2 类 / 818 行 | **待办** | — |
| `supervision` 子包 | 3 类 / 855 行 | **待办** | — |
| `integration` 子包 | 3 类 / 607 行 | **待办** | — |
| `review` 完整语义 | 4 类 / 1332 行 | **进行中** | Rust 581 行，覆盖约一半 |
| 测试覆盖 | 140 个 `@Test` | **待办** | Rust 57 个，且有 8 个子包零覆盖 |

### 3.3 P2（增强）

| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `browser` 子包（Playwright） | 4 类 / 1056 行 | **建议「设计决定（不迁移）」** | 与 RAG 的 Tika/PDF reader 同理：引入浏览器自动化运行时与本 crate 定位不符；保留 trait 扩展点 |
| `example` 子包 | 4 类 / 1130 行 | **设计决定（不迁移）** | 示例代码，非运行链路 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **已完成**：crate 骨架、错误类型、System One 的 DTO/执行/guardrail/概率/校验/评审纯函数层（9 文件 3181 行 57 test）。
- **验证命令与结果**：**本轮未执行** `cargo build` / `cargo test`（静态复核口径）。历史记录仅有 crate 内注释，无实测基线 —— **下次开工必须先跑一次** `cargo test -p agentscope-extensions-judge` 并回填。
- **已知问题**：无网络能力；8 个子包（含 `openai-decisions`）未迁移；无 `tests/` 集成测试。

### 4.2 下一步与阻塞

1. **P0**：落地 HTTP 客户端（建议 `reqwest` + 可注入 transport seam，复用 RAG/CONTROLPLANE 的既有模式）。
2. **P0**：逐字段核对 `SystemOneRequest`/`SystemOneResult`/`Usage` 与 Rust DTO。
3. **P1**：按依赖顺序迁 `evaluation` → `application` → `context` → `evidence`/`routing`/`supervision`/`integration` → `openai-decisions`。
4. **需汇总方裁定**：`browser`（Playwright）是否按「设计决定（不迁移）」关闭；`example` 建议直接关闭。
5. **跨模块**：`application`/`supervision` 可能依赖 harness 的 subagent / team 抽象，迁移前需先核对 harness 侧是否有对应 trait（遵循「不重复发明」原则）。

## 5. 执行约定（所有会话必须遵守）

- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-judge` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-judge` / `cargo test -p agentscope-extensions-judge` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-judge/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

- **复核方式**：纯静态（Java 逐包统计 + Rust 逐文件统计），**未编译、未跑测试**。
- **口径**：行数为非空行；Rust 测试数为 `#[test]` 属性计数。
- **结论**：Rust 侧当前覆盖 ≈ **3181 / 12469 ≈ 26%**（按行），且集中在 `guardrail` / `validation` / `review` / `dto` 四个纯函数区域；**`openai-decisions` 与 8 个 jev 子包零实现**。
- **状态判定**：`进行中`（非「待办」——已有 57 个测试的纯函数层；非「已完成」——核心 HTTP 链路与过半子包缺失）。
