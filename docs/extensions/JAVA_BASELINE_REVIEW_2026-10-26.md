# Java 基线漂移复评（2026-10-26 今日对齐提交 vs Java 最新 HEAD）

> **用途**：复评 2026-10-26 当天 14 个 Rust 提交中「对齐 Java」的部分，在 Java 仓库刚 `git pull` 到新 HEAD 后是否仍然成立、是否需要新的对齐。
> **本文件是一次性复评报告**；长期状态仍以各模块 `<MODULE>__GAP_TRACKER.md` 为单一事实来源，本次结论已同步写入相关 tracker（见 §6）。

## 1. 基线界定（关键前提）

| 项 | 值 | 判定依据 |
|---|---|---|
| Rust HEAD | `f1674b1`（2026-10-26 23:01） | `git log -1` |
| Java 旧基线（各 tracker 的对照基准） | `ed8073259`（2026-10-21 17:36） | `CONTROLPLANE__GAP_TRACKER.md:345`、`MYSQL__GAP_TRACKER.md:119` 显式记录 |
| Java 新 HEAD（用户刚 pull） | `16cb024ae`（2026-10-26 17:46） | `git reflog`：`16cb024ae HEAD@{2026-10-26 23:25:27}: pull: Fast-forward` |
| 新增区间 | **`ed8073259..16cb024ae`，共 27 个提交** | `git log --oneline ed8073259..HEAD` |

**方法学纠正（重要）**：判断「某 Java 提交是否属于新漂移」必须用 `git merge-base --is-ancestor <hash> ed8073259`，**不能凭 `git log` 日期排序**。本次用该命令确认以下提交**全部已在旧基线内**，不属本次漂移：`2c6d0eb0b`（skill frontmatter）、`5bfb36b44`（rag embedding usage）、`34b209e86`（scheduler channel scan）、`39f2ac6ab`（orphan task）、`997c60ac7`、`49622b392`、`d1eaa9fa9`、`8f87895f5`、`d12c638df`、`8cc6795b0`、`702a9fca1`。

**结论先行**：今天 14 个提交里 **10 个与 Java 无对齐关系**（纯 Rust 内部缺陷修复、文档、记忆同步），**4 个与 Java 对齐相关**：其中 **3 个仍成立、1 个（RAG）的核心结论已被推翻**。另发现 **11 项对齐缺口**：其中仅 1 项由新提交引入（CHANNEL 飞书超时），其余均为「旧基线内但 Rust 未对齐」的既有缺口，本次因复评才被发现。

## 2. 今日 14 个提交分类

| # | 提交 | 主题 | 是否对齐 Java | 复评结论 |
|---|---|---|---|---|
| 1 | `1104e6b` | session journal CAS / ACP 复用会话 | ❌ 否（Rust 并发缺陷） | 无对齐关系。Java `core/session/**` 与 `harness/agent/session/**` 最后一次改动仍是 `a2cbea5c3`（2026-10-08），新 27 个提交**零涉及**这两个目录（`git log ed8073259..HEAD -- .../core/session .../agent/session` 输出为空） |
| 2 | `3ed1737` | CHANNEL 入站媒体契约 + 可插拔 seam | ✅ 是 | 对齐对象 `ChannelMediaMetadata`、`InboundEventDeduplicator`、`AccessTokenStore`、`BotLoopGuard` **全部在旧基线内**，无漂移。**新发现 1 项**：Java `16cb024ae` 给 `FeishuAccessTokenProvider`/`FeishuOutboundClient` 加了可配置 `responseTimeout`，Rust 是全局常量 → §4.1 |
| 3 | `5108850` | CHANNEL tracker 归档 | — 文档 | 无 |
| 4 | `a8e2ed5` | 删除已完成的迁移分析文档 | — 文档 | 无（`java-recent-commits-migration-analysis.md` 已删，本次复评结论改落本文件 + 各 tracker） |
| 5 | `e035cce` | 记忆同步 | — | 无 |
| 6 | `735f185` | subagent 任务记录原子写 + cancel CAS | ❌ 否（Rust 竞态） | 无对齐关系。复核中确认 **Rust 的 orphan sweep 已有 CAS + terminal/cancel 双重守卫**（`workspace.rs:546-599`）→「无 CAS」的担心**不成立**；真实剩余缺口只在「按隔离域写回」，见 §4.2 |
| 7 | `c1f4647` | MONGODB ttl_days / database_name | ❌ 否（Rust 字段丢弃） | 无对齐关系；新 27 个提交**零涉及** mongodb |
| 8 | `3ac098c` | SCHEDULER cron 对齐 Quartz | ✅ 是 | 对齐对象是 `org.quartz.CronExpression`（第三方库），Java 侧**无自有 cron 实现**。新提交中唯一相关的是 `34b209e86`（**旧基线内**，`SchedulerApp` 扫 channel 包以注册回调控制器）→ 属 Java service 层，Rust 无 service crate → 记「设计决定（不迁移）」。无漂移 |
| 9 | `2caf1df` | MYSQL 列表哈希 / CAS / 标识符校验 | ❌ 否（Rust 数据正确性） | 无对齐关系；新 27 个提交**零涉及** mysql |
| 10 | `0d100ce` | OSS per-key 删除 / 分页 / 错误传播 | ❌ 否（Rust 缺陷） | 无对齐关系；新 27 个提交**零涉及** oss |
| 11 | `3d70a75` | RAG HTTP embedding 三 provider + reader | ✅ 是（**结论被推翻**） | 对齐对象 `EmbeddingUsage`/`EmbeddingResponse`/`SimpleKnowledge.usageListener` 由 `5bfb36b44` 引入，该提交**在旧基线内**。**Rust tracker §7.3 勘误 1 断言「Java 全仓零命中」——该断言错误**，见 §3.1 |
| 12 | `5176eda` | SKILLS git 身份/分支 + SQL 后端语义 | ✅ 是 | 对齐对象 frontmatter 规则来自 `2c6d0eb0b`（**旧基线内**）。**新发现 1 项**：Java 要求闭合分隔符独占一行，Rust 用 `trim()` 后比较 → §4.3 |
| 13 | `16e996d` | 第十二批回执登记 | — 文档 | 无 |
| 14 | `f1674b1` | context 计数锚定 provider usage + 预算溢出压缩 | ⚠️ 部分 | 对齐的「溢出压缩重试」在 Java 由 `HarnessAgent#forceCompactAndRetry` 承担（`168647c90`，**旧基线内**）。Rust 走 `ContextMiddleware::with_overflow_compaction`（不同架构）。复核结论：**持久化路径不同但语义可接受**（Rust 在 `on_model_call` 内直接改共享 `AgentState`，`StateCheckpointMiddleware` 在本轮结束时整体落盘），但**「输入已在上下文则空输入重试」缺失**，见 §4.4 |

## 3. 需要修正的历史结论

### 3.1 🔴 RAG tracker §7.3 勘误 1 是错的（必须改正）

RAG tracker §7.3 勘误 1 原文：

> 对 `D:\IdeaProjects\agentscope-java` 全仓 `rg "usageListener|EmbeddingUsage|EmbeddingResponse"` **零命中**；Java 既无这两个类，也无 `usageListener`。

**实测反证**（Java HEAD `16cb024ae`）：

```
$ rg -n "usageListener" --glob '*.java' D:\IdeaProjects\agentscope-java
...\core\rag\knowledge\SimpleKnowledge.java:79:  private final Consumer<EmbeddingUsage> usageListener;
...\core\rag\knowledge\SimpleKnowledge.java:230:     if (usageListener != null && usage != null) {
...\core\rag\knowledge\SimpleKnowledgeTest.java:119: .usageListener(observedUsages::add)

$ git cat-file -e ed8073259:.../core/embedding/EmbeddingUsage.java   -> EXISTS
```

- `core/embedding/EmbeddingUsage.java`（145 行）与 `EmbeddingResponse.java`（106 行）**均存在**，由 `5bfb36b44`（2026-10-20，#3589）新增，且 `5bfb36b44` **是旧基线 `ed8073259` 的祖先**（`merge-base --is-ancestor` 返回 true）。
- 因此 §2.3/§3.3 里「Rust 侧增值、不得写作 Java 已有」的表述**方向反了**：这些类型是 **Java 已有、Rust 迁移过来的**。
- §7.3 勘误 3「embedding 实为 7 类」随之失效：现在是 **9 类**（原 7 + `EmbeddingUsage` + `EmbeddingResponse`）。

**成因**：做该复核时 Java 工作区可能停在更早提交（`rg` 搜的是工作区而非 `git show HEAD:`），或路径仍指向不存在的 `E:\local-github\agentscope-java`。**教训**：断言「Java 没有 X」必须同时给出 `git rev-parse HEAD` 与原始命令输出。

### 3.2 仍然成立的结论（真实缺口，非勘误）

| 项 | Java | Rust |
|---|---|---|
| `EmbeddingUsage` 字段集 | `inputTokens` / `totalTokens` / `time`（`time` = 客户端端到端**秒数**，在 usage 内） | `input_tokens` / `output_tokens` / `total_tokens`；耗时放在 `EmbeddingResponse.elapsed` |
| `EmbeddingModel::embed` 返回 | `Mono<EmbeddingResponse>`（breaking change） | `embed() -> Vec<f64>` + 新增默认方法 `embed_with_usage()`（刻意兼容取舍） |
| `SimpleKnowledge` usage 回调 | builder 有 `usageListener`，每次 embed 后回调（`addDocuments` 与 `retrieve` 都调） | `SimpleKnowledge::new(model, store)` **无 listener 形参**，内部只调 `embed()`，usage 不外传 |

## 4. 对齐缺口清单（本次复评新增/确认）

> 标「旧基线内」= 该 Java 提交已在 `ed8073259` 之前，属**既有缺口**而非本次新漂移；本次因复评才被发现。

| # | 模块 | 缺口 | Java 依据 | Rust 现状（已核实） | 优先级 |
|---|---|---|---|---|---|
| 4.1 | CHANNEL | 飞书 token / outbound 超时不可配 | `16cb024ae`（**新漂移**）：`FeishuAccessTokenProvider`/`FeishuOutboundClient` 新增 `responseTimeout` 构造参数，默认 10s，构造期校验正数 | `common/http.rs:16` 全局常量 `DEFAULT_TIMEOUT = 10s`，`ReqwestTransport::new()` 写死；`feishu/token.rs` 无超时形参 | P2 |
| 4.2 | harness.subagent | orphan sweep 不按**原始隔离域**写回 | `39f2ac6ab`（旧基线内）：把 user/session 路由元数据注册到**独立 BaseStore namespace**，sweep 后按 `scoped.context()` 写 FAILED | `sweep_once` 读用 `RuntimeContext::default()`；写回 ctx 取 `local_contexts` 命中值，**未命中则回落 `RuntimeContext::default()`**（`workspace.rs:870-873`）→ user/session 命名空间下的记录在重启后无法按原域写回。**已具备**：全量 glob（`*.json`，无 recent-window 限制）、CAS 重试、terminal/cancel 双重守卫 | P1 |
| 4.3 | SKILLS | frontmatter 闭合分隔符未要求「独占一行」 | `2c6d0eb0b`（旧基线内）：正则要求 `---` 前后为行边界/行尾，缩进 `  ---` **不算**闭合 | `git.rs::split_frontmatter` 用 `line.trim() == "---"` → 缩进行**会被当作闭合**，把后续正文误判为 frontmatter 之后 | P1 |
| 4.4 | harness.context | 溢出压缩后重试未判「输入已在上下文」 | `168647c90`（旧基线内）：按 message id 判断本轮输入已在 context 内 → 重试传**空输入**（避免重复注入，且不再重跑已完成的工具） | `context_middleware.rs` 的 `compact_overflow` 会写回共享 `AgentState`（`*guard.context_mut() = new_context`），但重试仍用**同一 `messages`** 重新编译，无「已在上下文则空输入」判定 | P1 |
| 4.5 | harness.middleware | Inbox 提示注入形态与 Java 相反 | `d04529e99`（旧基线内）：每条 hint → **独立 user-role 通知消息**（元数据 `synthetic`/`reminderKind`/`source`/`hintId` + 溯源句「Automated background event from ...」），**同时写入 `AgentState.context`** | `inbox.rs`：把全部 hint 收进**一条 Assistant 消息**追加到 `input.messages`；**无元数据、无溯源句、不写回 `AgentState.context`**。注：Rust 确实把提示送进了本轮 model 输入，故「不达下一请求」的部分**不成立** | P1 |
| 4.6 | harness.context | `@path` 展开形态与截断提示 | `d04529e99`（旧基线内）：**追加**独立预览块（保留原 blocks）、中和 `</attached_file>`、超 1000 行加 `[Attachment preview limited to ...]` | `at_path.rs`：只就地改写 `ContentBlock::Text` 文本（非文本块**未丢失**）、**不追加独立预览块**、标签为 `<file path=...>`（非 `<attached_file>`）、**无闭合标签中和**、截断按**字符**数并加 `…(truncated to N chars)` | P2 |
| 4.7 | harness.skill | `<available_skills>` 默认仍是 full-metadata 视图 | `041ce1707`（旧基线内）：`SkillPromptBuilder()` 默认 `exposeAllMetadata` 由 `true` 改 **`false`**（slim：name/description/skill-id/files-root），usage 文案同步精简 | `skill/prompt.rs:67` 默认 `expose_all_metadata: true`；header 文案仍是旧的长版本 | P2 |
| 4.8 | harness.middleware | **plan mode 无 PLAN MODE 横幅，且无 model-call 刷新** | `041ce1707`（旧基线内）：`activePoints()` 增 `ON_MODEL_CALL`；`onSystemPrompt` 注册 `ContextItem.instruction("mode_rules", ...)`，内容为 `PLAN MODE is active (read-only). Plan file: %s` + 完整行为约束 + BUILD 切换提示 | ① `middleware/mod.rs::PlanModeMiddleware` 只有 `on_system_prompt`，仅拼接静态 `config.plan`；② 装配处 `harness_agent.rs:1889-1892` 传的是 `plan: String::new()` → **默认构建路径下该中间件近似 no-op**；③ 全仓搜不到 `PLAN MODE is active` 横幅文案（只有 core 的 `PLAN_MODE_DENY_MESSAGE`）；④ 无 `active_points()` 覆写（默认全开，但因无 `on_model_call` 实现，效果等同未注册） | **P0** |
| 4.9 | harness.subagent | 子智能体无法独立配置 compaction | `d7e2d589b`（旧基线内）：`SubagentDeclaration.Builder.compaction(CompactionConfig)` / `.disableCompaction()`，声明级优先于父级继承 | `subagent/mod.rs::SubagentDeclaration` **无** compaction 相关字段 | P2 |
| 4.10 | harness.context | 渲染时转义正文（Java 已改为只中和自身闭合标签） | `455be59b7`（旧基线内）：`ContextRenderer` **保留正文原样**，只把自身闭合标签（及外框的 `</HARNESS_CONTEXT>`）中和为 `<\/...>`；属性值仍转义 | `context/render.rs:100` 对 `item.content` 调用 `escape()` → 正文里的 `&&`/`<String>` 变成 `&amp;&amp;`/`&lt;String&gt;`（正是 Java 修掉的那个 bug）。另：AGENTS.md **无 100 KiB 上限**（`workspace_context.rs` 无任何 cap，全仓搜 `102_400` / `File truncated` 零命中） | P1 |
| 4.11 | harness.memory | compaction 摘要提示远简于 Java | `041ce1707`：`DEFAULT_SUMMARY_PROMPT` 7 段（含 `USER PREFERENCES AND CORRECTIONS`、`COMMANDS AND ERRORS`、`extraction_rules`、`secrecy`），并新增「保留仍生效约束原文；被撤销决定标 superseded」 | `memory/compaction.rs::DEFAULT_SUMMARY_PROMPT` 只有 4 段（`SESSION INTENT`/`SUMMARY`/`ARTIFACTS`/`NEXT STEPS`），无 extraction_rules、无 secrecy、无 superseded 规则 | P1 |
| 4.12 | harness.memory | 后台 flush/consolidation 上下文回放 | `bb4dbc753`（旧基线内）：Reactor 需 `contextWrite(contextView)` 回放 | **非缺口**：Rust 用 `RuntimeContext` 显式传参（`memory_flush.rs` 已 `ctx.clone()`），无隐式上下文问题 | — |

> **落地状态（2026-10-27）**：上表 4.1–4.3、4.5–4.11 已实现并验证，逐项修法与反向验证见
> `docs/MIGRATION_GAP_TRACKER.md` §3.15 与 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.1.7。
> **4.4 未修**，且复评后深挖发现成因与本表描述不同（见下）。
> 三处复核更正：
> - **4.4 成因不同、影响更重（P0）**：报告的两条都不成立 —— 「输入重复注入」不存在（`on_model_call`
>   内联改写消息列表），「缺 isInContext 判定」也不适用（Rust 重试不单独传 input）。真实缺陷是
>   **中间件看到的 `ctx.state` 落后于本轮本地 state**：`react_agent.rs:1402` 克隆一份本地 state 跑循环，
>   `:1509` 才回写共享；而 `compact_overflow` 从 `state.context()` 取历史（`context_middleware.rs:277-296`），
>   重试只拼「system + 压缩后的旧历史」（`:355-360`）→ **本轮输入（用户问题 + 本轮工具结果）被丢弃**；
>   `:353` 的写回随后被 `:1509` 覆盖 → **压缩结果不落盘**。首轮溢出时 `history.is_empty()` 直接放弃。
>   现有测试把 shared state 与输入设成同一份，掩盖了该问题。
> - **4.8 行号更正**：`harness_agent.rs:1889-1892` → 实际为 **`:1900-1913`**（`plan: String::new()`
>   在 `:1909`）。
> - **4.8 措辞更正**：严格说是「模型侧提示为零」，而非「plan mode 完全缺失」——工具侧拦截
>   （core `react_agent.rs`）一直是完整的，缺的是让模型知道自己在 PLAN 模式。

## 5. 明确「不迁移 / 无漂移」清单

| 项 | 说明 |
|---|---|
| `34b209e86` scheduler 扫描 channel 包 | Java Spring `@ComponentScan` 的 service 装配问题；Rust 无 service crate，控制器注册由调用方显式完成 |
| `d90517ab3` AG-UI SSE 断连 | 只改 `spring-boot-starter`（不迁移） |
| `1aebcf0b2` AG-UI schema | 在 `extensions-agui`，但**旧基线内** → 属既有 P1 缺口，非新漂移 |
| a2a 系列（`6e1ee68d7`/`dcb131a3f`/`e6f015e14`） | **新提交**。Rust a2a 侧本就未实现 `A2aAgent` 门面（只有 wire 模型 + JSON-RPC client + server），缺口已由 PROTOCOL tracker 既有 P1 覆盖。其中 `6e1ee68d7`（COMPLETED 任务优先取 artifacts）已登记为新细项 |
| `1ac8c8304` / `131c3bd9a` tool-call 参数完整性 | **新提交**，`core`。Rust 无 `ToolCallsAccumulator`（流式累积在 provider parser 内）；`anthropic/parser.rs` 有显式测试 `malformed_tool_arguments_degrade_to_empty_input` **把畸形参数降级为 `{}`**，与 Java 新语义（标记 malformed 并拒绝执行）**正好相反** → 真实缺口，登记到 core 侧（不在今天 14 个提交的对齐面内） |
| `db0b67eb8` progressive tool events | **新提交**，`core`。Rust 已有 `ToolCallStart/Delta/End` 与 `ToolResultStart/TextDelta/End`，但**按批聚合后才发**，无「单个工具完成即发」回调 → 与 Java 新行为有差异，登记为 core 侧后续 |
| `7976a20d7` / `1aeb863c4` tool schema | **新提交**，`core`。Rust 无 `@Tool` proc-macro（既有 P0），`additionalProperties` 无落点 |
| `9730e8171` / `aa66abfd0` e2b | **新提交**。Rust e2b `build_create_sandbox_body` 只带 `templateID`+`timeout`，**无 volume mount**；exec 超时走 `kill_sandbox`，缺「服务端 kill + 可靠超时」。既有 SANDBOX P1 已覆盖同族问题，新增 2 细项 |
| `31c7db8c9` / `4cdcc1785` judge openai-decisions | **新提交**。JUDGE tracker 的 P1「`openai-decisions` 整块缺失」规模已变化（新增 middleware/example/starter），结论不变 |
| `3be820137` / `1a1d73c04` 依赖 | Java 依赖版本，Rust 用 async-openai 0.41 / async-anthropic 0.6，无直接对应 |
| `17c4ed71b` / `96151142a` / `16cb024ae` 等 deflake | 测试稳定性，不涉语义 |

## 6. 本次落盘的文档变更

| 文件 | 变更 |
|---|---|
| `docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md` | 本文件（新建） |
| `docs/extensions/RAG__GAP_TRACKER.md` | **修正 §7.3 勘误 1/3**；新增 §7.6 复评结论；§2.3/§3.3 改口径 |
| `docs/extensions/SKILLS__GAP_TRACKER.md` | 新增 P1-11：frontmatter 闭合分隔符未要求独占一行 |
| `docs/extensions/CHANNEL__GAP_TRACKER.md` | §3 新增 G9：飞书 `responseTimeout` 不可配 |
| `docs/extensions/SCHEDULER__GAP_TRACKER.md` | 登记「channel 回调控制器注册」为设计决定（不迁移） |
| `docs/extensions/PROTOCOL__GAP_TRACKER.md` | 新增细项：COMPLETED 任务优先取 artifacts |
| `docs/extensions/SANDBOX__GAP_TRACKER.md` | 新增细项：e2b volume mount / exec 服务端 kill 超时 |
| `docs/MIGRATION_GAP_TRACKER.md` | 新增 §3.15：harness/core 侧新漂移（4.2–4.11） |
| `docs/extensions/EXTENSIONS_GAP_TRACKER.md` | §1 相关行追加复评注记 + §3.1.7 登记本批复评 |

## 7. 复评方法与可复现命令

```powershell
# 1) 基线界定
cd D:\IdeaProjects\agentscope-java
git reflog -5 --date=iso
git log --oneline ed8073259..HEAD | Measure-Object -Line

# 2) 判断某提交是否属于新漂移（不要用日期！）
git merge-base --is-ancestor 5bfb36b44 ed8073259   # true = 已在旧基线内

# 3) 反证 RAG 勘误
rg -n "usageListener|EmbeddingUsage" --glob '*.java' D:\IdeaProjects\agentscope-java
git cat-file -e ed8073259:agentscope-extensions/agentscope-extensions-rag/agentscope-extensions-rag-simple/src/main/java/io/agentscope/core/embedding/EmbeddingUsage.java

# 4) 逐模块看新提交
foreach ($m in @('channel','rag','skills','mysql','mongodb','oss','scheduler','sandbox','judge','protocol')) {
  git log --format='%h %ad %s' --date=short ed8073259..HEAD -- "agentscope-extensions/agentscope-extensions-$m"
}
```

## 8. 教训（应写入记忆）

1. **基线漂移必须显式界定区间**：`Java 最新` 不是稳定基准。tracker 对照行应写明 `Java HEAD = <hash>（<date>）`，每次 `git pull` 后重算。
2. **`git log` 排序 ≠ 祖先关系**：日期早的提交也可能在旧基线之后（合并顺序 / rebase）。判定必须用 `merge-base --is-ancestor`。
3. **「Java 没有 X」的断言必须带 `git rev-parse HEAD`**：本次 RAG §7.3 勘误 1 因缺这一步，把「Java 已有」写成「Java 没有」，并据此把迁移工作标成「增值」，误导了一整轮。
4. **`rg` 只搜工作区**：Java 工作区可能停在旧提交或含未提交改动；要断言「HEAD 上没有」，用 `git cat-file -e <rev>:<path>` 或 `git show <rev>:<path>`。
5. **复评报告里每条断言都要回到源码核实**：本次初稿曾写下「Rust orphan sweep 无 CAS」「Rust `@path` 会丢非文本块」两条，经读源码后**均被推翻**（`workspace.rs:546-599` 有 CAS+双守卫；`at_path.rs` 只改 Text 块）。未核实的断言会直接把错误写进 tracker。
