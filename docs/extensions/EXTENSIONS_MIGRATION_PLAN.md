# agentscope-extensions 迁移编排方案（Rust）

> **定位**：本文件仅承载**方案**（模块清单、crate 映射、波次编排、并行约束）。
> **进度与缺口清单**：见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md`（汇总 / 我的职责）+ 每个模块的 `<MODULE>__GAP_TRACKER.md`（子任务独立 tracker）。
> **上游**：`docs/MIGRATION_PLAN.md`（core + harness 方案）、`docs/MIGRATION_GAP_TRACKER.md`。
> **Java 源**：`E:\local-github\agentscope-java\agentscope-extensions\`（排除 `agentscope-spring-boot-starters`）
> **Rust 目标**：`crates/agentscope-extensions-<module>/`（已在根 `Cargo.toml` 注册为 workspace member）

## 1. 范围与原则
- 迁移 `agentscope-extensions` 下 **20 个模块**（`agentscope-spring-boot-starters` 依赖 JVM 生态，明确不迁移）。
- **一个 Java 聚合模块 = 一个 Rust crate**（子模块以 crate 内 `src/<sub>.rs` 体现），保证 1:1 可追溯。
- 每个模块有**独立 tracker**：`docs/extensions/<MODULE>__GAP_TRACKER.md`，该文件是子任务的单一事实来源。
- 汇总职责（本人）：维护 §EXTENSIONS_GAP_TRACKER、合并各模块结果、处理跨模块依赖/根 `Cargo.toml` 变更。

## 2. 模块清单与波次

| 波 | 模块 | Java 子模块 | Java 主/测行数（初测） | Rust crate |
|---|---|---|---|---|
| A | MODEL | anthropic, dashscope, gemini, ollama, openai, e2e-tests | 24885 / 45043 | `agentscope-extensions-model` |
| A | PROTOCOL | a2a-client, a2a-server, agent-protocol, agui, chat-completions-web | 16844 / 18860 | `agentscope-extensions-protocol` |
| A | RAG | bailian, dify, haystack, ragflow, simple | 15438 / 16057 | `agentscope-extensions-rag` |
| A | SANDBOX | agentrun, daytona, e2b, kubernetes | 8024 / 1577 | `agentscope-extensions-sandbox` |
| A | CHANNEL | common, dingtalk, feishu, github, gitlab, wecom | 4879 / 0 | `agentscope-extensions-channel` |
| A | MEM | mem0, memory-bailian, reme | 4821 / 5746 | `agentscope-extensions-mem` |
| B | JDBC | 单模块（dialect/store/state/snapshot/sandbox） | 3106 / 1558 | `agentscope-extensions-jdbc` |
| B | REDIS | 单模块 | 2884 / 217 | `agentscope-extensions-redis` |
| B | MONGODB | 单模块 | 2231 / 2427 | `agentscope-extensions-mongodb` |
| B | MYSQL | 单模块 | 2070 / 204 | `agentscope-extensions-mysql` |
| B | POSTGRESQL | 单模块 | 1571 / 1583 | `agentscope-extensions-postgresql` |
| B | COS | 单模块 | 882 / 760 | `agentscope-extensions-cos` |
| B | OSS | 单模块 | 795 / 290 | `agentscope-extensions-oss` |
| C | AISTIO | 单模块（adapter/model/store/transport） | 9352 / 2841 | `agentscope-extensions-aistio` |
| C | STUDIO | 含 core.tracing.telemetry | 3700 / 1452 | `agentscope-extensions-studio` |
| C | SKILLS | git / mysql / postgresql repository | 3177 / 2813 | `agentscope-extensions-skills` |
| C | SCHEDULER | common, quartz, xxl-job | 3009 / 2589 | `agentscope-extensions-scheduler` |
| C | TRAINING | backend/reward/runner/strategy | 2890 / 2918 | `agentscope-extensions-training` |
| C | NACOS | a2a, prompt, skill | 1873 / 2378 | `agentscope-extensions-nacos` |
| C | HIGRESS | 单模块 | 1071 / 971 | `agentscope-extensions-higress` |

合计：主代码 ~105,000 行 / 测试 ~102,000 行（20 模块）。

## 3. 依赖映射表（补充 core/harness 方案）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp / WebClient | HTTP 客户端 | `reqwest`（workspace 已声明 0.12, rustls） | |
| jackson-databind | JSON | `serde` / `serde_json` | |
| Reactor Flux/Mono | 流式/异步 | `tokio` + `futures` + `async-stream` | |
| jedis / lettuce / redisson | Redis 客户端 | `redis` crate | 各客户端适配统一为 client-agnostic trait |
| mongodb-driver-sync | MongoDB | `mongodb` crate | |
| mysql-connector-j / pgjdbc / h2 / sqlite-jdbc | JDBC | `sqlx`（或各 native crate）+ 方言枚举 | 方言能力保留为 `Dialect` trait |
| aliyun-oss-sdk / qcloud cos sdk | 对象存储 | `object_store` 或厂商 REST | 先构 trait 抽象层 |
| quartz / xxl-job | 调度 | `tokio-cron-scheduler` + 自建 Job trait | `Scheduler` 抽象优先 |
| tree-sitter / 其他 | — | 同 core 方案 | |
| OkHttp WebSocket | 实时通道 | `tokio-tungstenite` | channel / studio |
| opentelemetry | 遥测 | `opentelemetry` + `tracing-opentelemetry` | studio |
| nacos-client / higress | 注册配置 | `reqwest` 直调 OpenAPI 或 SDK crate | 先构 trait |
| grpc-java | AISTIO 控制面 | `tonic`（+ `prost`） | 可后置 |

## 4. 并行编排约束（子智能体必须遵守）

1. **文件所有权**：子智能体**只允许写** `crates/<自己的 crate>/**` 与 `docs/extensions/<自己的 MODULE>__GAP_TRACKER.md`。
2. **根 `Cargo.toml` 不可改**（workspace deps / members 由汇总方维护）；新增依赖写在本 crate 的 `[dependencies]`，若需要 workspace 级依赖则记入自己 tracker 的 §4.2 由汇总方处理。
3. **构建**：用 `cargo build -p <crate>` / `cargo test -p <crate>`。并发构建会因 target 目录锁互相等待（正常现象，不要改 `CARGO_TARGET_DIR`）。
4. **交付物**：（a）填好的 `<MODULE>__GAP_TRACKER.md`；（b）可编译可测的 crate；（c）结构化回执（见 §5）。
5. **不做超范围重构**：不修改 core/harness，不修改其他模块。确有需要 → 写进自己 tracker 的 §4.2 阻塞项。

## 5. 子智能体回执格式（用于汇总）

```
module: <MODULE>
状态: 已完成|部分完成|阻塞
交付:
  - <文件/符号 列表>
验证:
  - cargo build -p <crate>  → <结果>
  - cargo test  -p <crate>  → <结果：N passed / M failed>
剩余缺口: <按优先级列出，需回写进 tracker>
跨模块需求: <如需汇总方改根 Cargo.toml / 新增 workspace 依赖，列在此>
```

## 6. 验收标准（每个 crate）
1. `cargo build -p <crate>` 通过，无新增 warning 级别错误。
2. `cargo test -p <crate>` 通过（至少覆盖：序列化往返 / 纯函数 / 协议映射）。
3. 该模块 tracker 的 §1 状态、§2 清单、§3 缺口表、§4 状态均已填写。
4. 无对 core/harness 与其他模块的越界修改。
