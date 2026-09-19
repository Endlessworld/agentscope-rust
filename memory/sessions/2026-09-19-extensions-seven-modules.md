# 第十批：七模块缺口攻坚（2026-09-19）

## 任务
用户点名七个模块的未完成部分，并行派发 7 个子智能体；**裁定不迁移的条目一律标记为「已完成（设计决定，不迁移）」**。

## 结果（全部经汇总方独立复跑，不采信回执）

| 模块 | 基线 → 结果 | 本轮实质交付 |
|---|---|---|
| MODEL | 58 → **169** passed / 0 failed | 新增 `src/anthropic/`（7 文件）+ `src/gemini/`（7 文件）；实现 core `Model`/`ModelProvider`；复用 `HttpTransport` seam；**零新增依赖** |
| PROTOCOL | 130 → **290** passed / 0 failed | `agui` 整包（~8800 行）：模型补齐 + converter + adapter(+strategy) + encoder(SSE) + processor + registry + runtime + constants + error |
| RAG | 152 → **265** passed / 0 failed | 新增 `src/bailian/`（9 文件，**自建 ACS4-HMAC-SHA256 签名**，SDK jar 逆向 + 金标向量钉测试）+ `src/haystack/`（7 文件）；本 crate 加 `hmac 0.12`/`sha2 0.10`/`uuid` |
| JDBC | 2 → **115** passed / 0 failed | 新增 6 个 `tests/*.rs`；对照 Java 98 `@Test` 的 P0 测试缺口关闭 |
| POSTGRESQL | 7 → **104** passed / 0 failed | `state`/`store`/`snapshot`/`guard`/`distributed` 零测试区补齐 |
| MONGODB | 61 → **76 + 1 doc-test** / 0 failed | GridFS 255KB 分块 + chunk 完整性校验 + legacy 回退；`database_name` 生效；`delete_key` 接线 |
| REDIS | 21 → **92 + 2 doc-test** / 0 failed | `find_keys_by_pattern` 改 **SCAN 游标**；`base_store()` 自动接线（保留 URL + `connect_lazy`）；`released` 改 `AtomicBool` 接线 |

**全量验收（2026-09-19）**：`cargo build --workspace` BUILD_EXIT=0；`cargo test --workspace -j 2 --no-fail-fast` → **2546 passed / 0 failed / 1 ignored**（92 test target）。`1 ignored` 是 `harness/.../platform_channel.rs:22` 的 ```ignore 文档示例，早于基线即存在。

## 补测暴露的真 bug（高价值）
- **JDBC 4 个**：①`resolved_session_state_table_name()` `None` 分支**递归调自身**（任何调用即栈溢出）；②`namespace_path` 缺尾分隔符 → `search(["parent"])` 漏同命名空间条目；③`save_list` 增量分支误用 `execute_prepared_batch` 混跑 INSERT/UPSERT 模板 → **数据丢失**；④锁表 DDL 建 `lock_name PK` 但 INSERT 写 `(lock_name, acquired_at)` → **锁永远插不进**。
- **POSTGRESQL 1 个**：`MemoryClient` SQL 分派只匹配**未加引号**表名字面量，而 `state.rs` 实际发 schema 限定名 `"agentscope"."agentscope_sessions"` → 模拟器对大半个模块**实际不可用**（既有 7 用例全在 `sql.rs` 故从未触发；首轮 18 failed 即暴露）。
- JDBC 旧结论已失效：会话表列名（`session_id`）与锁表名（`{prefix}distributed_locks`）两语言**现已一致**，早前「跨语言同库不兼容」作废。

## 关键教训（本轮新增）
1. **子智能体网络中断会留下「半成品 + 零文档」**：PROTOCOL 首个子智能体落盘 8800 行但**从未跑过编译**（5 个编译错误 + 11 个失败用例），且 **tracker 完全未改**。收尾必须：先 `cargo build` 看真实错误 → 再逐用例判定「实现 bug vs 断言错」→ 最后回填 tracker。
2. **收尾 agent 连续失败**：PROTOCOL 收尾首次派发亦网络失败（0 改动落盘），第二次回执为空但改了代码（失败数 8→7 且新引入 1 个）。**结论：长任务必须尽早落盘并分段验证；汇总方要准备好亲自接手**。
3. **PROTOCOL 我亲自修的真实技术点**（`agui` 编译/语义）：
   - core `AgentEventType` **不实现 `Hash`** → 转换表不能用 `HashMap`，我改为 `Vec<(AgentEventType, Arc<dyn AgentEventConverter>)>` + 线性查找（保留「后注册覆盖」语义）；不要改回 HashMap。
   - 事件流：async `flat_map` 不成立（返回 Future 非 Stream）→ 用 `.then(async{...}).flat_map(identity)` 或 `.then(...).flatten()`。
   - `AguiStreamableAgent` trait object 无 `Debug` → 测试用 `match` 而非 `unwrap_err()`。
   - 中流失败**不得再补发 `RUN_STARTED`**（会与 `AgentStart` 产出的重复）。
   - `RunFinished.outcome` 与 Java 同为 `@JsonInclude(NON_NULL)` 语义（省略而非 null）。
   - `block_message_id` 无显式 block_id 时**按事件类型推导块类型**（text/thinking），使 delta 与 end 事件落在同一 message id。
   - `headers().len()` 为 3（存储**大小写敏感**、查找不敏感，`Accept`/`accept` 是两条 + `X-Agent-Id`）。
4. **裁定「不迁移」= 已完成（设计决定）**：各 tracker 已统一改写（MODEL 4 处 / PROTOCOL 2 / RAG 3 / JDBC 9 / POSTGRESQL 3 / MONGODB 3 / REDIS 8 处标记）。典型依据：Spring `*AutoConfiguration`（Rust 无 DI）、已弃用的 `JedisAgentStateStore`/`RedissonAgentStateStore`、需真实 API key 的 e2e-tests、依赖 JVM 库（Tika/PDFBox/POI）的 reader、`NetworkUtils`、`close()`/连接所有权/`objectMapper` 注入等。

## 未闭合项（留待后续，勿误当已完成）
- **REDIS**：真实 Redis 集成测试缺失（环境受限）；现有 92 用例基于内存替身，**未验证**真实 Lua 执行、多轮 SCAN 游标、`ConnectionManager` 重连/鉴权、Sentinel/Cluster。
- **MONGODB/REDIS**：真实驱动未接（`MongoClient` trait 已就绪可直接接；`mongodb` crate 未进根 workspace 依赖）。
- **POSTGRESQL**：锁 key 哈希 Java MurmurHash3 vs Rust `DefaultHasher`，跨语言不互通，仍「待确认」。
- **JDBC**：state 版本化 API（`saveIfVersion`/`getVersioned`/`supportsVersioning`）需 core 裁定（core `AgentStateStore` 无版本方法）。
- **RAG**：embedding HTTP 提供商归属、外部向量库（ES/Milvus/PgVector/Qdrant）、reader 增强为非本轮范围。
- **MODEL**：远程图片下载 resolver、Ollama embedding 调用链/会话合并、OpenAI 兼容厂商差异待裁定。
- **PROTOCOL**：A2A runner/registry/transport 的 1:1 补齐、agent-protocol workspace 落盘仓库为存量 P1 长尾。

## 工作区纪律复核
根 `Cargo.toml`、`agentscope-core/**` **零改动**；`agentscope-harness/**` 仅 `memory/` 记忆文件（自动产物，非代码）。`Cargo.lock` 仅 +3 行（RAG 的 `hmac`/`sha2`/`uuid`），合规。
