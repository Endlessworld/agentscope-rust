# Java 未关闭 Bug 在 Rust 侧的比对结论

> 上游清单：`docs/JAVA_OPEN_BUGS.md`（312 个 open + label:bug，归档于 2026-11-16）
> 比对方法：每轮 5 个只读子智能体并行，每个负责 5 个 issue（一轮 25 个）；子智能体给出「Rust 对应位置 + 行号证据」，主 agent 对每条「存在」结论亲自读码复核。
> 结论四档：**存在**（Rust 有等价缺陷）/ **部分存在**（同类根因但触发路径或影响面不同）/ **不存在**（Rust 已规避或语义相反）/ **不适用**（Rust 无对应实现，或属 Java/Spring/Reactor 生态问题）。

统计规则：issue 按「最近创建/更新优先」分批，轮次只记录已完成的比对。

---

## 轮次 1：25 个最新 issue

25 个全部完成（5 组 × 5 个）。其中 4 条「存在」结论已由主 agent 亲自读码复核（#3975 / #3972 / #4203 / #3266），#3780 的「配置项无读者」也已用 rg 独立复核。

### 1.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #4207 | 子智能体工具名大小写/空格冲突 | 不存在 | `harness/src/agent/subagent/materialize.rs:332` |
| #4205 | 模型名按 locale 折叠（tr-TR） | 不存在 | `core/src/model/context_windows.rs:61-68` |
| #4203 | 工具参数截断写死 20 字符 | **存在** | `harness/src/agent/memory/compaction.rs:1193` |
| #4140 | sessionStorage 硬编码 `agents/` 路由 | 不存在 | `harness/src/filesystem/spec.rs:229-230` |
| #4104 | 单个 external result 重派兄弟调用 | 部分存在 | `core/src/agent/react_agent.rs:2192,4022` |
| #3975 | async-tool 按 id 更新静默失效（双斜杠路径） | **存在** | `harness/src/agent/bus/async_tool.rs:141` |
| #3973 | InboxMiddleware 错误处理缺口（吞错/无日志） | 部分存在 | `harness/src/agent/middleware/inbox.rs:156` |
| #3972 | stale hint 把长任务误报为崩溃/超时 | **存在** | `harness/src/agent/middleware/inbox.rs:161` |
| #3971 | drained hints 在无 agent state 时丢失 | 部分存在 | `harness/src/agent/middleware/inbox.rs:136` |
| #3968 | 并发 sandbox 调用在 inbox 检查失败 | 不适用 | 同类根因 `harness/src/agent/bus/async_tool.rs:106` |
| #3966 | 沙箱读文件 base64 含 `\n` | 部分存在 | `extensions-sandbox/src/fs_spec.rs:632` |
| #3937 | owner id 含冒号导致会话跑不起来 | 不存在 | `core/src/agent/runtime_context.rs:115-146` |
| #3891 | agentservice chat 界面不展示消息 | 不适用 | 无对应组件 |
| #3866 | 与 Spring AI 结构化输出依赖不兼容 | 不适用 | Cargo 无全局 classpath |
| #3816 | 会话 journal 重放与体积放大 | 部分存在 | `core/src/journal/recorder.rs:1667` |
| #3815 | 取消 flush 打断 I/O / writer 租约泄漏 | 部分存在 | `core/src/journal/recorder.rs:810` |
| #3780 | compaction 缺口（溢出恢复 / flushPrompt） | 部分存在 | `harness/src/agent/harness_agent.rs:2342-2349` |
| #3768 | SessionBridge.sessions() 持锁同步加载 | 不适用 | 无 controlplane crate |
| #3733 | ControlPlane 45 秒标记不健康 | 不适用 | 无 controlplane crate |
| #3421 | team wakeup 跨 namespace 串同名成员 | 部分存在 | `harness/src/agent/team/mod.rs:615` |
| #3415 | 带沙箱运行，结束后报错 | 不存在 | `harness/src/agent/middleware/sandbox_lifecycle.rs:56` |
| #3388 | 沙箱 shell 写覆盖 host 的 AGENTS.md/skills/MEMORY.md | **存在** | `harness/src/agent/workspace/manager.rs:544` |
| #3378 | 枚举面相对路径绕过 namespace 锚点 | 部分存在 | `harness/src/filesystem/composite.rs:124` |
| #3266 | memory_search 返回无上限结果 | **存在** | `harness/src/tool/memory_search.rs:73` |
| #3262 | Windows 上 write_file/edit_file 引号丢失 | 不存在 | `harness/src/filesystem/sandbox.rs:126` |

小计：**存在 5 / 部分存在 9 / 不存在 6 / 不适用 5**。

---

## 逐条明细

### #3975 async-tool 按 id 更新静默失效 —— **存在**

- **位置**：`crates/agentscope-harness/src/agent/bus/async_tool.rs:135`（`find_record_path`）、`:141`、`:149`；`crates/agentscope-harness/src/filesystem/remote/mod.rs:152`（`normalize_path`）、`:213`、`:224`。
- **证据**：`find_record_path` 拿 `fs.ls(registry_root)` 返回的 `dir.path` 直接拼 `format!("{}/{id}.json", dir.path)`，而 remote/local 的 `ls` 对子目录产出的路径以 `/` 结尾，拼出内部 `//`。`normalize_path` 只补前导 `/` 并去掉一个尾 `/`，**不折叠内部双斜杠**，`get_item` 走精确 key 查询 → 匹配不到，`exists` 为假 → `update_status` 直接 `return Ok(())`。结果是 `complete/fail/mark_timeout` 全部空操作，记录永远 `RUNNING`，`InboxMiddleware` 每轮重复上报。本地 `Path` 容忍 `//`，所以只在 store/remote 后端触发。
- **复核**：主 agent 已读 `async_tool.rs:100-180` 与 `remote/mod.rs:140-180` 确认。
- **修复**：候选路径改 `format!("{}/{id}.json", dir.path.trim_end_matches('/'))`；并在 `normalize_path` 里折叠连续 `/`。

### #3972 stale hint 误报「进程崩溃或超时」 —— **存在**

- **位置**：`crates/agentscope-harness/src/agent/middleware/inbox.rs:154-166`（文案在 `:161`）、`crates/agentscope-harness/src/agent/bus/async_tool.rs:237-247`。
- **证据**：`find_stale` 的唯一判据是 `rec.status == RUNNING && rec.created_at < cutoff`；`AsyncToolRecord` 只有 `id/session_id/tool_name/tool_call_id/status/created_at`，**没有 owner 或心跳**，无法区分「进程已死」与「任务仍在跑」。注入提示与 Java 一字不差：`did not complete (process crash or timeout). You may retry or proceed without the result.`（附带瑕疵：续行残留大段连续空格）。
- **复核**：主 agent 已读两处源码确认。
- **修复**：措辞改为只陈述事实（"has not completed within N minutes; it may still be running — use wait_async"），或给记录加 heartbeat/owner 字段。

### #3973 InboxMiddleware 错误处理缺口 —— **部分存在**

- **位置**：`crates/agentscope-harness/src/agent/middleware/inbox.rs:156`、`:158-166`、`:196-201`、`:142`、`:147`。
- **证据**：① `let _ = registry.mark_timeout(&rec.id).await;` 吞掉错误；配合提示 id 用随机 `new_hex()`、而 `state.context` 去重按 hint id 比较，同一条陈旧记录**每轮**重新注入。② Rust 无 Mono/zip fail-fast：drain 与 find_stale 各自 `unwrap_or_default`/`if let Ok`，不会因单源失败中断推理。③ Rust 无 null-payload NPE：`payload` 是 `Value`，`deserialize_hint_block` 用 `?` 安全返回 `None`；但「解析不出 hint 的 entry 已被消费且静默丢弃」仍在。全文件无任何 `tracing` 调用。
- **修复**：`mark_timeout` 失败 `warn!`；stale 提示改用稳定 id（如 `stale:{record_id}`）以便跨轮去重。

### #3971 drained hints 在无 state 时丢失 —— **部分存在**

- **位置**：`crates/agentscope-harness/src/agent/middleware/inbox.rs:136`、`:142`、`:184-201`、`:272-280`；对照 `crates/agentscope-core/src/agent/react_agent.rs:1440`。
- **证据**：`ctx.state` 为 `None` 时，破坏性 `inbox_drain` 已执行、HintBlock 事件照发、去重块整段跳过、`guard.context.extend(...)` 不执行，但 `input.messages.extend(notifications)` 仍执行 —— 本轮模型能看到，**后续轮次丢失**，且无 requeue、无日志。`inbox_drain` 失败走 `unwrap_or_default()`，同样静默丢全部条目。
- **修复**：`state.is_none()` 时 `warn!` 或把 entries 回推总线；drain 失败也加 warn。

### #3966 沙箱读文件 base64 含 `\n` —— **部分存在**

- **位置**：`crates/agentscope-extensions-sandbox/src/fs_spec.rs:676`、`:620-637`（`decode_text`）、`crates/agentscope-extensions-sandbox/src/remote.rs:105`、`crates/agentscope-core/src/formatter/base_formatter.rs:153-175`。
- **证据**：Rust 的沙箱读**不走 shell `base64`**（用 `sed -n`/`tail -n +N` 取内容，二进制在进程内用 `base64::STANDARD` 编码，不折行），所以 Java 的 76 列换行注入点不存在。但**解码端同样严格**：`decode_text` 与 `save_base64_data_to_temp_file` 都用 `STANDARD.decode`，带空白的载荷直接报错。更实的相邻缺陷：exec 路径 stdout 在 `remote.rs:105` 先 `String::from_utf8_lossy`，**经 exec 读二进制会先被有损替换再编码**。
- **修复**：解码前剥离 ASCII 空白；二进制读取一律走 transfer 分支。

### #3816 journal 重放与体积放大 —— **部分存在**（读侧已治理，写侧有残留）

- **位置**：读 `durable-adapter-agentscope/src/execution_host.rs:107-118`、`durable/src/backend/query.rs:40-60`；写 `core/src/journal/models.rs:190-260`、`core/src/journal/recorder.rs:1667`、`:331-338`、`durable/src/backend/compact.rs`、`harness/src/agent/harness_agent.rs:1150`。
- **证据**：① 读放大已修：`open` 从 checkpoint 覆盖位置前一条开始读，分页按 `seq` 游标推进，`read_traffic_test` 把「2500 条历史只读 1 条」钉住；控制轮询改为退避（空闲 1 秒 ≤10 次读）。② `model/chunk` 按 4 KiB/64 delta/1 秒合并。③ durable 在提交事务内按 1 MiB 阈值压缩，只留最近 1024 条事件并推进 `retained_after`；`write_traffic_test` 限定单轮 ≤40 次重写、<2.4 MB。④ 残留：`state/checkpoint` 仍内嵌**整份 AgentState**，`request/prepared` 默认**仍记录完整 prompt**（`RecordingPolicy` 默认 true、harness 默认 `record_model_requests: true`）。
- **修复**：宿主侧把 `record_model_requests` 默认改 false；checkpoint 改增量/差分。

### #3815 取消 flush 打断 I/O / writer 租约泄漏 —— **部分存在**

- **位置**：`crates/agentscope-core/src/journal/recorder.rs:810`（`log.acquire`）、`:816-824`、`:951-959`、`:430`（`impl Drop`）、`:1209-1221`；`core/src/journal/atomic.rs:876`。
- **证据**：① Rust 无 `Thread.interrupt`，I/O 只在 await 点被放弃，`flush` 还专门处理「caller 中途丢弃 future」（还原 in-flight 批次），所以「取消打断存储 I/O」不成立。② 但 `InvocationRecorder::open_with` 只有**错误路径**清理，没有 scopeguard/RAII：future 在 acquire 之后、`Drop` 生效之前被丢弃时 writer 租约无人释放，而 `acquire` 遇活租约直接返回 `Session already has an active writer`、`LEASE=30s` → 同会话下一次调用最多失败 30 秒。③ 残留风险：还原时换新 batch_id，若原 commit 已落地只是 ack 丢失，会触发 `Stale committed prefix` → rebase → 同一批事实追写第二遍。
- **修复**：给 `open_with` 的 writer 加 Drop 即 release 的守卫；还原时先按旧 batch_id 尝试幂等重放。

### #3780 compaction 缺口 —— **部分存在**

- **位置**：`crates/agentscope-harness/src/agent/context/context_middleware.rs:106-111`、`crates/agentscope-harness/src/agent/harness_agent.rs:2342-2349`、`:2389`、`:2937`、`crates/agentscope-harness/src/agent/memory/compaction.rs:224-226`、`:777-815`、`crates/agentscope-extensions-model/src/common/error.rs:461`。
- **证据**：① 溢出恢复只挂在本地预算预判（`PrepareOutcome::BudgetExceeded`），`call` 与 `stream_events` 共用同一中间件链 → Java 的「stream 缺恢复」不对称不存在。② 但 provider 侧 `ErrorKind::ContextLength` 在 core/harness **零消费者**，真实 `context_length_exceeded` 时两条路径都硬失败。③ `CompactionConfig.flush_before_compact` / `offload_before_compact`（默认 true）在全 crate **没有任何行为读取点**（主 agent 已用 rg 复核：只有字段、默认值、builder、相等比较与测试断言）→ pre-compaction flush 整体缺失，配置是死的。④ 每次调用后的 flush 确实带自定义 prompt。
- **修复**：给压缩加 provider `ContextLength` 触发的重试；把 `flush_prompt` 传入 compactor，或删除这两个哑配置项。

### #3421 team wakeup 跨 namespace 串成员 —— **部分存在**

- **位置**：`crates/agentscope-harness/src/agent/team/mod.rs:302`、`:376`、`:422`、`:615`、`:208-214`；`crates/agentscope-harness/src/agent/middleware/teams.rs:17-23`。
- **证据**：Java 的静态 `BY_MEMBER`（teamName+memberName）索引在 Rust 不存在（`TeamsMiddleware` 每实例持有 `(client, team_id, member)`），但同类缺陷在：① `boards` 只以 team_id 为键，`create_team` 用 `insert` 直接**覆盖**同名/同 id 的 board；② `subscribe_wakeups` 用 `wakeups.insert(member, tx)` 以成员名为唯一键，第二个同名成员会替换 sender，使第一个订阅者 `recv()` 立刻返回 `None`。当前 `subscribe_wakeups` **无生产调用方**（仅测试），所以暂不会真的投错会话。
- **修复**：同成员允许多订阅者或返回冲突错误；`create_team` 对已存在 id 返回 `Conflict`。

### #3937 owner id 含冒号 —— **不存在**

- **位置**：`crates/agentscope-core/src/agent/runtime_context.rs:105-146`、`crates/agentscope-core/src/agent/thread_key.rs:41-68`、`crates/agentscope-durable/src/types/identity.rs:30-37`。
- **证据**：Rust 的 `normalize_agent_id` 与 Java **语义相反** —— 文档明确写 "coerces instead of failing"，非法字符映射为 `_`，`:` 只会变成 `_`，不会中断。日志身份按 base64url 编码，durable 键按哈希 + digest，冒号安全。仓库已有裁定记录（`docs/extensions/history/CONTROLPLANE__MIGRATION_HISTORY.md:277-282`）。

### #3968 并发 sandbox 调用在 inbox 检查阶段失败 —— **不适用**（同类根因需留意）

- **位置**：`crates/agentscope-harness/src/agent/middleware/sandbox_lifecycle.rs:8-9`、`crates/agentscope-harness/src/agent/bus/workspace_message_bus.rs:45,60`、`crates/agentscope-harness/src/agent/bus/async_tool.rs:106`、`crates/agentscope-harness/src/filesystem/spec.rs:265-300`。
- **证据**：Rust 的 filesystem 在 `build()` 时固定注入，没有从 `RuntimeContext` 解析「当前沙箱」的实现，也没有 `No active sandbox` 语义，该崩溃路径不存在。但**同类根因存在**：`MessageBus` 与 `AsyncToolRegistry` 都把 `RuntimeContext::default()` 存下来用于所有 fs 调用，而 remote 路由命名空间的 `IsolationScope::Session` 缺省回落 `"default"` → 若把 bus/async-tools 前缀路由到动态命名空间的 remote fs，会跨会话串台。
- **建议**：给 bus/registry 的读写入口加 `&RuntimeContext`。

### #3891 / #3866 / #3768 / #3733 —— **不适用**

- #3891：Java service 的 control/data 面（`rt.agent_tasks`/outbox/fence）与 Console 前端；`crates/` 无实现，`docs/extensions/EXTENSIONS_GAP_TRACKER.md:687` 已裁定 controlplane 是 Go。
- #3866：Cargo 无全局 classpath，多 major 版本可并存；workspace 统一钉 `schemars="1"`/`jsonschema="0.30"`，结构化输出直接传 `JsonSchema` 值，不依赖第三方 schema 生成器。
- #3768 / #3733：`agentscope-extensions-controlplane` 已按 D6 决策整体删除（`ea4b1cc`），无 SessionBridge、无自注册心跳。ACP 的 `session/list` 反而是正确形态（锁内只取内存快照、锁外读持久化）。

### #4207 子智能体工具名冲突 —— **不存在**

- **位置**：`crates/agentscope-harness/src/agent/subagent/materialize.rs:332`、`:362`、`crates/agentscope-harness/src/agent/middleware/subagents.rs:157-166`、`crates/agentscope-harness/src/agent/subagent/manager.rs:35-46`、`crates/agentscope-core/src/tool/toolkit.rs:119-122`。
- **证据**：Rust **不按 agent 名派生工具**，只注册三种固定工具 `agent_spawn` / `agent_send` / `agent_list`；agent 名是 `agent_spawn` 的参数，走 `BTreeMap` 精确键查找（大小写敏感，不做 slug/小写化）。声明名只用作 agent id。即便重名，`Toolkit::register_tool` 是 `insert` 覆盖、后写胜出，不会报「重复注册失败」。

### #4205 模型名按 locale 折叠 —— **不存在**

- **位置**：`crates/agentscope-core/src/model/context_windows.rs:61-68`。
- **证据**：`resolve_context_window` 直接 `model_id.starts_with(prefix)` + 最长前缀，**全程不做大小写折叠**，各 provider 都传原始 `model_name`。Rust 的 `to_lowercase` 与 locale 无关，Java 的 tr-TR 问题无同类物。副作用差异：大写模型 id 会匹配失败并回退 256k（是「不折叠」而非「locale 折叠」）。

### #4203 工具参数截断写死 20 字符 —— **存在**

- **位置**：`crates/agentscope-harness/src/agent/memory/compaction.rs:1192-1197`。
- **证据**：`truncate_tool_use_block` 只在 `s.chars().count() > cfg.max_arg_length` 处把 `max_arg_length` 当**阈值**，真正截头写死 `s.chars().take(20)`，再把 `head + truncation_text` 回填 —— `max_arg_length`（默认 2000）从不参与「保留多少字符」。字段文档自己写「Suffix appended after the first 20 characters」。回归测试只断言 `val.len() < 100`，测不出该缺陷。
- **复核**：主 agent 已读该函数确认。
- **修复**：改为 `take(cfg.max_arg_length)`（尾部预算扣除 `truncation_text` 长度），并补一条「保留长度 ≈ max_arg_length」的断言。

### #4140 sessionStorage 硬编码 `agents/` 路由 —— **不存在**

- **位置**：`crates/agentscope-harness/src/filesystem/spec.rs:229-232`、`crates/agentscope-harness/src/agent/memory/flush.rs:176-177`、`crates/agentscope-harness/src/filesystem/composite.rs:38-90`。
- **证据**：`CompositeFilesystem` 只有按 path 前缀路由的 `route_for_path`，**没有** `session_storage()` 这类「预解析路由」入口，因此 Java 那种「拿 `"agents/"` 查表匹配不上 `agents/{id}/sessions/` 而落到默认后端」的路径不存在。`RemoteFilesystemSpec::build` 直接注册 `agents/{agent_id}/sessions/` 前缀路由，与外置写入路径 `agents/{agent_id}/sessions/{session_id}.jsonl` 一致。
- **残余风险**：若运行期 agent id 与构建期不同，前缀会不匹配（与 Java 的硬编码缺陷不同源）。

### #4104 单个 external result 重派兄弟调用 —— **部分存在**

- **位置**：`crates/agentscope-core/src/agent/react_agent.rs:2011-2018`、`:2191-2196`、`:4022-4028`、`:4755-4775`；`crates/agentscope-durable-adapter-agentscope/src/harness_adapter.rs:643-668`；`crates/agentscope-core/src/journal/views.rs:811`；`crates/agentscope-extensions-acp/src/session.rs:1604-1631`。
- **证据**：续跑时 `extract_pending_tool_calls(state)` 取「最后一条 assistant 里未被 TOOL 消息回答的全部 tool_use」，只按「有没有结果」过滤，**没有「已派发/已挂起、保留原请求 id」标记** → 一个已挂起的兄弟调用只要仍未答，下一轮 acting 就会再调用一次（与 Java 同源）。但 Rust **没有按请求 id 做部分回答的入口**：`continuation_turn`（`views.rs:811`）零生产调用方，ACP 的 `respond` 只能答一个 durable wait key，且没有任何 adapter 为 `ToolSuspended` 写 `waiting` 条件 → Java 的复现路径走不到；`resolve_offloaded_effects` 会把 `Pending`（含 Suspended）当 offload 去 `query`，无记录记 `Unknown` 并以失败收尾。
- **修复**：给挂起调用加「已派发且已登记请求 id」的事实，续跑时只重播挂起请求而不重入工具。

### #3415 带沙箱运行结束后报错 —— **不存在**

- **位置**：`crates/agentscope-harness/src/agent/middleware/sandbox_lifecycle.rs:56-66`、`crates/agentscope-harness/src/agent/middleware/memory_flush.rs:143-170`、`crates/agentscope-harness/src/agent/sandbox/manager.rs:296-318`。
- **证据**：Java 根因是「异步镜像/flush 在 `Mono.using` cleanup 之后才跑」。Rust 的 `SandboxLifecycleMiddleware::on_agent` 是 `stream.collect().await` 收完整条事件流才 `persist_state` → `release`，而 flush/maintenance 都是同一 stream 内的 tail await；sandbox 中间件在 build 中先于 memory 中间件注册，故 release 必然晚于所有 flush。Rust 也没有 SessionTree/`scheduleMirror`（`.log.jsonl` 镜像属已退役设计），`SandboxManager::release` 里 `close()` 失败只 `warn!` 并继续 shutdown。
- **残余**：宿主显式开 `async_tool_timeout` 时，后台工具完成会在 turn 结束后写 fs；绑到真实沙箱 fs 才会复现「释放后写入」，属可选配置。

### #3388 沙箱 shell 写覆盖 host 定义 —— **存在**

- **位置**：`crates/agentscope-harness/src/agent/workspace/manager.rs:544-550`、`:188-197`、`:567-585`；`crates/agentscope-extensions-sandbox/src/remote.rs:641-692`；`crates/agentscope-harness/src/filesystem/sandbox.rs:75-86`；`crates/agentscope-harness/src/agent/middleware/workspace_context.rs:152-176`。
- **证据**：① 读优先级与 Java 一致：「filesystem 优先，非空即用，否则回落本地 workspace 磁盘」（模块文档 `manager.rs:4-7`），`read_agents_md`/`read_knowledge_md`/`read_memory_md` 都走 fs 优先；沙箱模式下 fs 就是沙箱 fs，故 `echo > AGENTS.md` 落到沙箱内。② 宿主机版本不会回灌：`DEFAULT_WORKSPACE_PROJECTION_ROOTS`（AGENTS.md、skills、knowledge…）是 projection，而 `RemoteSandbox::start` **只在 `!restored` 时重建 workspace**，快照可恢复就跳过投影；Docker 后端的 `copy_in` 甚至未实现（只 skip 警告）。③ `WorkspaceContextMiddleware` 的冻结只按 session，新 session 会重读 → 污染进入下一轮 system prompt。
- **修复**：把 projection 根在沙箱侧标为只读，或每次 start 无条件重放 host 版本；`read_with_override` 对定义文件改为 host 优先、沙箱兜底。

### #3378 枚举面路径绕过 namespace 锚点 —— **部分存在**

- **位置**：`crates/agentscope-harness/src/filesystem/mod.rs:206`（`validate_path`）、`composite.rs:38-45`（`route_for_path`）、`:124-137`（`ls("/")` 特判）、`spec.rs:207-212`（默认后端 `LocalFilesystem`）、`local.rs:257-268`、`tool/filesystem_tools.rs:588`。
- **证据**：① `validate_path` 对任何 `..` 段直接报错，`LocalFilesystem::resolve` 首行调用它 → Java 的 `ls("/alice/../bob")`、`list_files("..")` 一律被拒。② namespace 由 `NamespaceFactory`/`ctx` 决定而非路径前缀，故不存在「路径选到兄弟租户」。③ **但相对拼写确实绕过路由与锚点**：`route_for_path` 只做字面前缀匹配，`"./memory/x"` 不匹配 `memory/` 而落到默认后端；`ls(".")`/`ls("/")` 被特判为 `default_backend.ls(ctx, "/")`，而默认后端是 UNRESTRICTED 的 `LocalFilesystem`、`resolve` 对绝对路径原样返回 → `list_files(".")` 枚举到文件系统根；`glob_files` 未传 path 时默认 `"/"`，同样是递归 glob。
- **修复**：`CompositeFilesystem` 入口统一 normalize（折叠 `./`）再路由；`ls`/`glob` 的根等价路径解析到 workspace root 或按路由逐 backend 枚举。

### #3266 memory_search 返回无上限结果 —— **存在**

- **位置**：`crates/agentscope-harness/src/tool/memory_search.rs:49-58`（schema）、`:73-99`（实现）、`crates/agentscope-harness/src/agent/memory/eviction.rs:20-32`、`:71-73`。
- **证据**：① schema 只有 `query` 一个参数，**无 `maxResults`**；遍历 `list_memory_file_paths` 的所有文件、每个匹配行都 `push`，**无逐行截断**，最后一次性 `results.join("\n")`。对照 `session_search` 有 `maxResults`（默认 10、上限 100）→ 是遗漏而非设计。② `default_excluded_tools()` 明确包含 `"memory_search"`，`should_evict` 因此永不外置；ACP 的豁免清理也没解除 memory_*。
- **复核**：主 agent 已读 schema 与 `call` 确认。
- **修复**：加 `maxResults`（默认 30）+ 每行 200 字符预览截断；把 `memory_search` 从 `default_excluded_tools()` 移除或超阈值走 eviction 占位。

### #3262 Windows write_file/edit_file 引号丢失 —— **不存在**

- **位置**：`crates/agentscope-harness/src/filesystem/sandbox.rs:126-145`（`build_windows_command`）、`crates/agentscope-extensions-sandbox/src/fs_spec.rs:707-731`（`write`）、`:733-758`（`edit`）、`:1695-1731`（回归测试）。
- **证据**：① Windows 宿主本地 shell 不经 argv 引号往返：整串按 UTF-16LE 编码后 base64 成 `-EncodedCommand` 交 `pwsh`，命令行无引号。② 沙箱写路径不再有 `mkdir -p "$(dirname X)"`：宿主侧算好 `parent` 后 `mkdir -p '<parent>'` + `printf %s <base64> | base64 -d > '<path>'`，只产出单引号；`edit` 是 read→replace→`write`，不再用 `python3 -c "<脚本>"`。③ 回归测试明确注释「Java e190a8d1b：Windows 上 docker CLI 会吃掉 `"$(dirname …)"` 的双引号」，断言命令里既无 `$(` 也无 `"`。

---

## 需要优先修复的清单（按严重度排序）

以下 5 条是 Rust 侧**确认存在**、且影响实际运行行为的缺陷：

1. **#3975** async-tool 按 id 更新静默失效 —— 记录永远 `RUNNING`、hint 每轮重复上报（store/remote 后端）。修复只需 `trim_end_matches('/')` + normalize 折叠双斜杠。
2. **#3266** `memory_search` 无上限 —— 结果无界进入上下文，且被排除在 eviction 之外（内存/上下文膨胀，违反「低消耗」原则）。
3. **#3388** 沙箱 shell 写可覆盖 host 的 AGENTS.md/skills/MEMORY.md 并跨会话持久 —— 属越权与持久化污染。
4. **#4203** 参数截断写死 20 字符 —— `max_arg_length` 是死配置，压缩后模型看到几乎空参数。
5. **#3972** stale hint 把仍在运行的长任务误报为「进程崩溃或超时」—— 诱导重复调用/放弃结果。

其次为「部分存在」中的三条高价值项：**#3815**（取消后 writer 租约泄漏，同会话最多失败 30s）、**#3780**（`flush_before_compact`/`offload_before_compact` 是死配置，pre-compaction flush 缺失；provider `ContextLength` 无消费者）、**#3378**（`list_files(".")`/`glob_files` 根等价路径可枚举到文件系统根）。

---

## 轮次 2：`area/core/agent` 前 50 个

小计：**存在 5 / 部分存在 18 / 不存在 23 / 不适用 4**。

### 2.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #3353 | 畸形流式 tool call 退化为 "Tool not found: null" | 不存在 | `core/src/agent/react_agent.rs:387-393` |
| #3320 | schema 在校验权限确认之后 | **存在** | `core/src/agent/react_agent.rs:4090,4221` |
| #3315 | ASK PermissionDecision 的消息被丢弃 | 部分存在 | `core/src/agent/react_agent.rs:65-66,4097` |
| #3308 | 子智能体继承父的前端工具 | 不存在 | `harness/src/tool/agent_spawn.rs:758-764` |
| #3294 | permission-HITL resume 附带的消息被丢弃 | 不存在 | `acp/src/session.rs:3096-3134` |
| #3135 | ASKING 状态被持久化 | 不存在 | `core/src/agent/react_agent.rs:1944-1949` |
| #3102 | 调用不存在工具直接退出循环 | 部分存在 | `core/src/agent/react_agent.rs:2565-2585` |
| #3065 | ToolEmitter 进度投递到错误请求 | 不存在 | `core/src/tool/emitter.rs:56-80` |
| #3043 | 工具结果 ID 不匹配 | 不存在 | `harness/.../tool_call_sanitizer.rs:47-58` |
| #2915 | AgentSpawnTool 永久保留 spawn 的 Agent | **存在** | `harness/src/tool/agent_spawn.rs:275-300` |
| #2870 | ACCEPT_EDITS 忽略工作目录 | **存在** | `core/src/permission/mod.rs:364-400` |
| #2732 | 沙箱 uploadFiles base64 单 argv 过长 | 部分存在 | `extensions-sandbox/src/fs_spec.rs:722-729` |
| #2702 | onError 路径 AgentState 不持久化 | 部分存在 | `harness/.../state_checkpoint.rs:405-441` |
| #2681 | event emitter 与 AsyncToolMiddleware | 部分存在 | `core/src/event/emitter.rs:11-40` |
| #2672 | middleware 工具重写失败 | 部分存在 | `core/src/middleware/input.rs:241-249` |
| #2652 | onSystemPrompt 跑在 boundedElastic/TCCL | 不适用 | `core/src/middleware/middleware.rs:342-346` |
| #2641 | ModelCallEndEvent 缺 status_code | 部分存在 | `core/src/agent/react_agent.rs:3427-3437` |
| #2610 | interrupt() API 设计缺陷 | 不存在 | `core/src/agent/react_agent.rs:643-690` |
| #2570 | 模型返回内容未结构化出工具调用 | 部分存在（低置信） | `extensions-model/src/openai/mod.rs:1754-1758` |
| #2539 | 每次 build() 改共享 Builder.hooks | 不存在 | `core/src/agent/react_agent.rs:1778` |
| #2536 | stateCache 只写不读无界驻留 | 不存在 | `core/src/agent/react_agent.rs:112` |
| #2535 | setPermissionMode 整体写回覆盖 | 不存在 | `core/src/agent/react_agent.rs:579-581` |
| #2506 | WakeupDispatcher 丢弃透传消息 | 不存在 | `harness/.../wakeup_dispatcher.rs:44-52` |
| #2486 | Per-path lock maps 无界增长 | 部分存在 | `harness/src/agent/workspace/manager.rs:124-130` |
| #2478 | 流式重试重复投递 chunk | 部分存在 | `core/src/agent/react_agent.rs:3310,3359` |
| #2446 | edit_file 在 old_string 为空时挂起 | 不存在 | `harness/src/filesystem/util.rs:28-33` |
| #2420 | skill 热加载（一） | 部分存在 | `harness/.../skill_middleware.rs:175-192` |
| #2419 | skill 热加载（二） | 部分存在 | 同上（与 #2420 重复） |
| #2408 | 取消父 run 不停止同步 spawn 的子智能体 | 部分存在 | `acp/src/session.rs:2333-2351` |
| #2400 | ThinkingBlock 当 text 回传导致 400 | 部分存在 | `extensions-model/src/anthropic/converter.rs:420-437` |
| #2362 | 缺少「终止工具」的干净实现 | 不存在 | `core/src/tool/tool.rs:175-190` |
| #2334 | 记忆写入异步化（enhancement） | **存在** | `harness/.../memory_flush.rs:159-165` |
| #2324 | streamEvent 过早结束 | 不存在 | `core/src/agent/react_agent.rs:1085-1116` |
| #2314 | 并发调用串用 RuntimeContext | 不存在 | `harness/src/agent/skill/repository.rs:308-377` |
| #2303 | 快照反序列化丢失 client | 不存在 | `extensions-sandbox/src/remote.rs:845-868` |
| #2279 | Flux sink.complete 延迟 | 不适用 | `core/src/agent/react_agent.rs:1128-1136` |
| #2261 | stateCache 无界增长 | 不存在 | `core/src/agent/react_agent.rs:125` |
| #2191 | 中断恢复遗留 orphan ToolUseBlock | 不存在 | `core/src/agent/react_agent.rs:4819-4839` |
| #2074 | coordinator 模式无法 cancel subagents | 部分存在 | `acp/src/session.rs:2331-2346` |
| #1997 | 取消不保证 checkpoint | 部分存在 | `core/src/agent/react_agent.rs:192-222` |
| #1953 | 工具组「父不可见子可用」无效 | 部分存在 | `core/src/tool/group.rs:255,264` |
| #1948 | AgentEvent 反序列化错误 | 不存在 | `core/src/event/event.rs:90-108` |
| #1941 | call() 不触发 onAgent（与 #1675 同根） | 不存在 | `core/src/agent/react_agent.rs:1140-1143` |
| #1910 | Plan Mode 白名单与权限确认不联动 | **存在** | `core/src/permission/mod.rs:364` |
| #1779 | Plan 完成态 hint 同轮冲突 | 不适用 | 无 PlanNotebook/subtask |
| #1741 | interrupt() 关不掉 SSE | 部分存在 | `core/src/agent/react_agent.rs:4038` |
| #1715 | skill 无法激活 / 绑定工具不可用 | 不适用 | `core` 无 skill；`AgentSkill` 无 `tools` |
| #1695 | ToolResultEvictionMiddleware 不生效 | 不存在 | `harness/.../tool_result_eviction.rs:47` |
| #1681 | 先装配 onAgent 再绑 ctx | 不存在 | `core/src/agent/react_agent.rs:966-978` |
| #1675 | v2 call() 不执行 Middleware | 不存在 | 同 #1941 |

### 2.2 新增的「存在」（5 条）

- **#3320** — `run_acting_calls` 先 `evaluate_permission`（`react_agent.rs:4090`），`Ask` 分支立即调 confirm handler（`:4221`），用户批准后才 `toolkit.call_tool` → executor 第 5 步才 `validate_input`（`executor.rs:106-109`）。全仓 `validate_input` 仅此一处调用，**权限门之前没有任何前置校验**。修法：Ask 分支在调 handler 前先校验，失败直接记 error 结果。
- **#2915** — `register_live`（`agent_spawn.rs:275-300`）每次 spawn 生成新 key 并把 `Arc<dyn CallableAgent>` 插进 `live: HashMap`；全文件对 `live` 只有 `insert`/`get`/`values`，**无 remove/TTL**，`labels` 与 `active_local` 也只增不减 → 长驻父 agent 永久强引用每个子 agent。修法：加 `agent_release` 或空闲 TTL/LRU。
- **#2870** — `PermissionEngine.working_directories` 是**死配置**（只有定义、`is_trivial`、builder），`AcceptEdits` 分支对非只读工具一律落回 `Ask`（`permission/mod.rs:385-396`）。ACP 另用按工具名的 allow rule 绕过（副作用相反：越界编辑同样放行）。修法：AcceptEdits 分支加「路径参数 ⊆ working_directories → Allow」。
- **#2334** — `MemoryFlushMiddleware::on_agent` 用 `chain(once(tail))` 把 flush+offload（内含一次模型调用）挂在 turn 尾部（`memory_flush.rs:159-165`），而 `HarnessGateway::run` 的 `session_turn_gate.acquire` 租约要等整条流结束才释放（`harness_gateway.rs:306`）→ 下一轮输入被卡，与 Java 同构。修法：改为旁路 `tokio::spawn` + 每 session 串行队列。
- **#1910** — plan 白名单 `PLAN_MODE_ALWAYS_ALLOWED`（`react_agent.rs:4844`）只作用于 `plan_mode_denies`，随后的 `evaluate_permission` 完全不知道 plan 上下文：`plan_write` 非只读 → Default 下 `ASK`、ACP 映射的 `Explore` 下直接 **DENY**（比 Java 更糟）。修法：`evaluate_permission` 增加「plan active + 工具在白名单 → Allow」短路。

### 2.3 高价值「部分存在」

- **#2486** — `WorkspaceManager::lock_for`（`manager.rs:124-130`）用 `entry(rel).or_insert_with(...)` 建 `Arc<Mutex<()>>`，全文件 `remove` 零匹配，而 manager 随 agent 长驻、key 来自模型写的路径 → **无界**。`LocalFilesystem` 无锁表；SkillBox 已改固定 256 条带并会 `forget`。修法：改固定条带 `hash(rel) % N`。
- **#2420 / #2419** — `HarnessSkillMiddleware` 的 `SkillCache` 无 TTL、无 mtime 校验，`invalidate()` 唯一调用者是 `skill_manage` 写盘回调 → **进程外新增/改写的 SKILL.md 在 agent 生命周期内永不生效**。修法：`on_system_prompt` 前比对目录 mtime/条目数。
- **#2702 / #1997** — 两者同根：`AsyncGate::poll_next` 只在 `inner` 正常排空时才跑 post（`state_checkpoint.rs:405-441`），`RunGuard::drop`（`react_agent.rs:192-222`）只关 recorder 不补 checkpoint → 出错/丢弃订阅时 store 丢本轮。core 的内存态已回写，缺的是持久化兜底。修法：`AsyncGate` 加 Drop 兜底或 `Ready(Some(Err))` 后立即跑 post。
- **#2478** — `visible_output_emitted` 只由 TextBlockDelta 置位（`react_agent.rs:3359`），**thinking / ToolCallDelta 不计数**，这些增量之后的重试会二次下发 `ThinkingBlockStart/Delta` 与 `ToolCallDelta`。
- **#2400** — 有 `anthropicThinkingSignature` 时正确回传 `Thinking` 块；**无签名时 `out.push(Text(...))`** 正是 Java 那条 400 路径，且被单测 `thinking_blocks_are_rendered_as_text` 钉死为期望行为（有意取舍）。修法：无签名时跳过该块而非降级。
- **#2074 / #2408** — 子智能体可显式 cancel（`cancel_subagent`、对子 session 发 `session/cancel`），但**prompt 级取消不级联**：`Session::cancel()` 只推进 `cancel_epoch`，只有 `close()` 才 `cancel_subagents()`。
- **#1953** — `ToolGroupScope::External` 只表示「meta 工具不可管理」，`active=true` 时父 LLM 照样可见可调（`group.rs:255`、`toolkit.rs:232`、`executor.rs:89`）；子代理 `tools:` 白名单只裁子、不隐父。可用 `CapabilitySurface` 自写 resolver 绕过。
- **#1741** — 模型流阶段 `interrupt()` 有效（`select!` + 25ms 轮询），**工具执行阶段不可打断**（`react_agent.rs:4038` 只在批次间检查）→ 工具挂起时事件流不结束。
- **#2641** — `ModelCallEnd` 只有 `reply_id` + `usage`，`finish_reason` 已在累加器里却被丢弃；经 `on_model_call` 中继核的事件是 `EventData::default()`。
- **#2732** — 沙箱 `write` 兜底路径把整段内容 base64 拼成单条 `printf %s <b64> | base64 -d > path`（`fs_spec.rs:722-729`），**无分块**；落盘失败被 `maybe_evict` 静默吞掉。
- **#3315** — `ConfirmHandler` 只收 `ToolUseBlock`，`decision.message` 仅在 Deny 分支使用，Ask 分支丢弃；AG-UI interrupt 用固定文案。
- **#3102** — 框架侧未知工具有兜底（记 error 结果、循环继续），但 **provider 侧**（如百炼 `LLM is trying to invoke a non-exist tool`）的 `Err` 直接终止整个 turn，没有「折成工具结果再重试」。
- **#2681 / #2672** — emitter（`FORWARDING_CONTEXT_KEY`）零生产者零消费者，子代理事件不回灌父流；`on_acting` 无法改写结果（`push_first` 首写者胜），但 `on_reasoning` 是可用改写点（`ToolResultEvictionMiddleware` 即范例）。

### 2.4 判「不存在」的代表性理由

- **#3353 / #3043 / #3102 框架侧** — 畸形参数被 `METADATA_MALFORMED_ARGUMENTS` 标记后**不交给执行器**，直接合成 error 结果让模型重发；孤儿 tool_result 由 `ToolCallPairSanitizerMiddleware` 在请求视图剔除。Rust 无 null、无 NPE。
- **#2536 / #2535 / #2261** — Rust **没有** `(userId, sessionId) -> AgentState` 的槽位 cache：`ReActAgent` 只有一个 `Arc<Mutex<AgentState>>`，`RuntimeContext.state` 是 call-scoped；跨副本写回过 durable `expected_position` 校验（冲突即拒）。
- **#3135 / #2191** — 核心 loop 从不持久化 `ToolCallState::Asking`（注释明确）；`reconcile_interrupted` 先补 orphan tool_result 再追加 recovery 消息，顺序与 Java 相反。
- **#2362** — 已有一等「终止工具」`return_direct(true)`；acting 被停止时先落 tool_result 再返回，reasoning 被停止时合成非空 stop 消息并正常落库。
- **#3308 / #2314 / #1681 / #3294** — Rust 的 `RuntimeContext` 是**显式传参的值**（无 thread-local current/pending），子上下文不复制 `Extensions`，子 turn 也不带 per-call 上下文。
- **#1941 / #1675** — `call()` 复用 `run_stream_with_extensions`，`wrap_agent` 全链生效，有回归用例 `legacy_injected_system_prompts_are_request_scoped`。
- **#2446 / #1695 / #2506 / #2539 / #2610 / #2303 / #2324 / #1948** — 分别对应：空串直接返回 0 / 驱逐改在 `on_reasoning` 且回写 state / wakeup 进 `deferred` 重投 / `build(self)` 消费 builder / `interrupt` 打的是在飞 run / 只持久化快照身份并在 resume 重绑 / 无条件发 `AgentResult`+`AgentEnd` / serde 内部 tag 无同名属性。

### 2.5 累计新增需优先修复（第 2 轮）

1. **#1910** plan 白名单不回灌权限引擎 —— ACP 的 `plan` 模式下 `plan_write` 直接被 DENY，plan 流程不可用（比 Java 更严重）。
2. **#3320** 权限确认先于 schema 校验 —— 用户确认必然失败的调用。
3. **#2486** `WorkspaceManager` 锁表无界 —— 长驻 agent 内存单调增长。
4. **#2420/#2419** skill 目录无失效 —— 进程外 skill 改动永不生效。
5. **#2915** spawn 的子 agent 永久驻留 —— 内存单调增长。
6. **#2702/#1997** 出错/丢弃订阅时丢本轮 checkpoint。

---

## 轮次 3：`core/agent` 剩余 19 + `harness` 前 31

小计：**存在 16 / 部分存在 10 / 不存在 20 / 不适用 4**（本轮是命中率最高的一轮）。

### 3.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #1672 | onAgent 拿不到 RuntimeContext | 不存在 | `core/src/middleware/middleware.rs:24-31` |
| #1623 | MEMORY.md 写进 default 命名空间 | 不存在 | `harness/.../memory_flush.rs:131-140` |
| #1606 | build() 深拷贝 toolkit 丢动态工具 | 不存在 | `core/src/agent/react_agent.rs:1810` |
| #1556 | ToolExecutor 异常逃逸 | 不存在 | `core/src/tool/executor.rs:240-247` |
| #1555 | PendingToolRecovery 误判跳过 | 不存在 | `core/src/agent/react_agent.rs:1949-1972` |
| #1446 | stop_reason=248044 导致假死 | 不存在 | `core/src/agent/react_agent.rs:2218-2225` |
| #1403 | 长期记忆 + RAG 提取冲突 | **存在** | `extensions-rag/src/message.rs:89-96` |
| #1265 | PendingToolRecoveryHook 注解错误 | 不适用 | 无注解体系 |
| #1217 | 未按提示词输出 | 不存在 | `core/src/agent/react_agent.rs:2218-2225` |
| #1211 | incremental 累计模式失效 | 部分存在 | `core/src/agent/stream_options.rs:70` |
| #1203 | updateToolGroups 对已存在实例无效 | 不存在 | `core/src/agent/react_agent.rs:2524` |
| #1117 | block() 在 reactor 线程阻塞 | 不适用 | 无 Reactor |
| #1036 | interrupt 不生效 | 部分存在 | `core/src/agent/react_agent.rs:4137` |
| #938 | 主智能体不订阅子流式事件 | 不存在 | `core/src/tool/tool.rs:204` |
| #918 | agent-as-tools 流式报错 | 不存在 | `core/src/memory/mod.rs:69-71` |
| #846 | 流式文本块累积重复 | 不存在 | `core/src/agent/react_agent.rs:3317-3360` |
| #680 | Agent-as-Tool + A2A 丢 metadata | 部分存在 | `harness/src/tool/agent_spawn.rs:676` |
| #618 | finish_plan 后不输出总结 | 不适用 | 无 PlanNotebook |
| #459 | 子 agent 内层 HITL 阻塞失败 | 部分存在 | `core/src/agent/react_agent.rs:4352-4356` |
| #3258 | UNRESTRICTED 默认后端放过绝对路径 | **存在** | `harness/src/filesystem/spec.rs:208-212` |
| #3253 | 根 ls/grep 逃出 workspace | **存在** | `harness/src/filesystem/composite.rs:115,187` |
| #3197 | Redis 模式 glob/ls 被本地索引误导 | 不存在 | `filesystem/remote/mod.rs:204,330,381` |
| #3178 | 子代理不继承 MCP 工具 | 部分存在 | `harness/src/agent/harness_agent.rs:2756` |
| #3171 | streamEvents 不做溢出恢复 | 不存在 | `harness/src/agent/harness_agent.rs:663` |
| #3088 | memory 时区错误 | **存在** | `harness/src/agent/memory/flush.rs:169` |
| #3059 | flush/consolidation 阻塞事件流 | **存在** | `harness/.../memory_flush.rs:140-165` |
| #3042 | 跨后端 move 重写字节并删源 | **存在** | `harness/src/filesystem/composite.rs:214-235` |
| #2985 | 上下文压缩失效 | 部分存在 | `harness/src/agent/memory/compaction.rs:30` |
| #2982 | 首次运行必触发 consolidation | **存在** | `harness/.../memory_maintenance.rs:120-140` |
| #2918 | session key 不匹配远程路由 | **存在** | `core/src/agent/thread_key.rs:41-48` |
| #2849 | 并发会话 No active sandbox | 不存在 | `harness/src/filesystem/sandbox.rs:37-58` |
| #2845 | 同 isolation-key 竞争 sandbox 槽位 | **存在** | `harness/src/agent/sandbox/guard.rs:133-154` |
| #2792 | 完成通知缺全局进度 | 部分存在 | `harness/.../middleware/subagents.rs:264-300` |
| #2791 | WaitAsyncResults 等 inbox 链 | 不存在 | `harness/src/tool/wait_async.rs:129-170` |
| #2758 | listKnowledgeFiles 报错 | 不存在 | `harness/.../workspace/manager.rs:233-272` |
| #2688 | contextSupplier 并发 bug | 不存在 | `harness/src/agent/skill/repository.rs:19-35` |
| #2686 | subAgent 与外部执行/审批工具 | 部分存在 | `core/src/agent/react_agent.rs:4352-4357` |
| #2683 | knowledge 目录文件读不到 | **存在** | `harness/.../workspace_context.rs:826-829` |
| #2679 | 用户隔离下 skill 使用 bug | 部分存在 | `harness/src/agent/skill/repository.rs:384-424` |
| #2664 | `.skills-cache` 空集合不清理 | **存在** | `harness/.../skill_middleware.rs:193-197` |
| #2644 | shell 工具提示名与注册名不一致 | **存在** | `harness/src/agent/skill/prompt.rs:35` |
| #2581 | SSE 断连走不到持久化 | 不存在 | `extensions-agent-protocol/src/lib.rs:945` |
| #2448 | OverlayFilesystem.grep 残留 stale 匹配 | **存在** | `harness/src/filesystem/overlay.rs:93-118` |
| #2298 | agentName 影响远程 session 卸载 | **存在** | `harness/.../memory_flush.rs:132` |
| #2271 | Windows execute 总失败 | 不存在 | `harness/src/filesystem/sandbox.rs:126-138` |
| #2266 | 报告路径与实际落盘不一致 | 部分存在 | `harness/src/tool/filesystem_tools.rs:441,481` |
| #2251 | @TempDir 与 mirror 线程竞争 | 不适用 | 无 JUnit / 无 mirror 线程 |
| #2216 | tar 解包目录不存在 | **存在** | `extensions-sandbox/src/remote.rs:663-696` |
| #2182 | 远程模式仍写本地 index/session | **存在** | `harness/src/agent/harness_agent.rs:2205` |
| #2181 | 公共技能读不到 | 不存在 | `harness/src/agent/skill/load_tool.rs:150-178` |

### 3.2 存在的 16 条（按严重度归组）

**A. 路径 / 安全（4 条，最该优先修）**

- **#3258** `RemoteFilesystemSpec::build` 的默认后端是 `LocalFilesystem::new(workspace)`（`spec.rs:208-212`）→ `LocalFsMode::Unrestricted`（`local.rs:196`，且是 `Default`）→ `resolve` 对绝对路径原样返回（`local.rs:261-266`），`validate_path` 只拒空串与 `..`。于是默认 remote 配置下 `read_file("/etc/passwd")`、`list_files("/tmp")` 直达宿主。对照：`LocalFilesystemSpec` 已改 `Rooted`（`spec.rs:61-66`），remote 侧漏了。
- **#3253** `CompositeFilesystem::ls` 对 `"/"`/`"."` 特判为 `default_backend.ls(ctx, "/")`（`composite.rs:115-116`），`grep` 根分支原样转（`:187`），配合上面的 Unrestricted → `list_files(".")` 枚举驱动器/文件系统根。`glob` 已锚定（`local.rs:560-564`），只有 ls/grep 有洞。
- **#2918** journal 写 `.agentscope/sessions/{agent}/{user}/{session}`（`thread_key.rs:41-48`），而 `RemoteFilesystemSpec` 只注册 `agents/{agent_id}/sessions/` 等前缀（`spec.rs:229-230`）→ 不命中路由，回落本地后端。**配了分布式 store 后会话历史仍静默落本地、不跨副本**。spec 的既有用例甚至把「路由外一律本地」写成了断言。
- **#2182** `build()` 无条件 `WorkspaceIndex::open(&resolved_workspace)`（`harness_agent.rs:2205`）→ 无论 filesystem 是否远程都建 `.index/workspace.db`；`.agentscope/sessions`、`.agentscope/bus` 同样不在路由前缀 → 云端部署仍生成本地文件。

**B. 数据正确性（4 条）**

- **#3042** 跨后端 `move` 只搬 `data.content`（`composite.rs:220-231`），**丢弃 `encoding`**；`LocalFilesystem::read` 对二进制返回 base64 文本 → PNG/音频被写成 base64 文本；删源 `let _ = src.backend.delete(...)`（`:233`）失败被吞。
- **#2448** `OverlayFilesystem::grep` 以 `format!("{}:{}", m.path, m.line)` 为键合并（`overlay.rs:110-116`）→ 只有「路径+行号都相同」才覆盖，上层编辑后该行不再匹配时，下层旧匹配原样留下（文件头注释承诺的 upper-first 语义没落实）。
- **#2298** `memory_flush.rs:132` 用 `ctx.agent_name` 拼 `agents/<name>/sessions/`，而路由按 **agent_id** 注册（`spec.rs:229`）；name≠id 时前缀不命中 → 永远卸载不到远程。
- **#2683** knowledge 清单以 **workspace 相对**路径注入（`workspace_context.rs:826-829`），而 `read_file` 按 fs 根解析（默认 `cwd`，`harness_agent.rs:2174`），默认 workspace 是 `cwd/.agentscope/workspace` → 模型照清单读 `knowledge/x.md` 落到 `<cwd>/knowledge/x.md` 读不到。

**C. 语义 / 配置（5 条）**

- **#1403** RAG 的 `extract_query_from_messages` 与记忆的 `last_user_message` 都取「最后一条 USER」且**不按 name 过滤**，两者中间件 order 均为 50，链按降序稳定排序 → 外侧注入的 USER 消息必然成为内侧的检索 query（互相当 query）。
- **#2644** `DEFAULT_CODE_EXECUTION_INSTRUCTION` 写 "You have access to the **execute_shell_command** tool"（`skill/prompt.rs:35`），实际注册名是 `"execute"`（`shell_execute_tool.rs:22`），全仓无别名 → 模型照提示调用必然 tool-not-found。
- **#3088** 日报文件名用 `Local::now()`（`memory/flush.rs:132`），条目标题却用 `Utc::now().to_rfc3339()`（`:169`）→ 同一文件两种时区。
- **#2982** `should_run_now` 冷启动 `unwrap_or(0)`（`memory_maintenance.rs:129`）→ `now - 0` 必然超过 `min_gap`，**首次必触发 consolidation**；flush 的 Throttled 分支同构。
- **#2845** 进程内守卫 `SandboxExecutionGuard::try_enter` 只做计数后无条件返回（`guard.rs:140-154`），`noop()` 就是 `new()`，`fs_spec.rs:154` 默认 `None` → 同 isolation-key 的并发 resume/mutate/persist 后写覆盖先写。

**D. 生命周期 / 缓存（3 条）**

- **#3059** flush 与 maintenance 都用 `downstream.chain(once(tail))`（`memory_flush.rs:160`、`memory_maintenance.rs:293`），tail 里含 LLM 调用 → 订阅者拿到 `onComplete` 必须等这些调用结束（`.await` 只是同流内串行，不是 fire-and-forget）。
- **#2664** `build_rendered_catalog` 在 `entries.is_empty()` 时**提前 return**（`skill_middleware.rs:195-197`），不调 `stage` → 孤儿回收 `garbage_collect_orphans`（只在 `stage` 末尾跑）永不执行，`.skills-cache` 残留 SKILL.md 继续被投影进沙箱；叠加 catalog 惰性缓存（`:172-181`）只在 `skill_manage` 写盘时失效。
- **#2216** `RemoteSandbox::start` 先 `hydrate_workspace_bytes`（`remote.rs:674`）才 `build_workspace_setup_script`（`:689`，首行 `mkdir -p`），而 hydrate 的 `build_b64_extract_script` 只有 `tar xf - -C <dir>`（`:457-464`）不建目录 → 首次恢复报 `tar: /home/agentscope/workspace: Cannot open`。Docker 路径已先 `setup_workspace()`。

### 3.3 部分存在（10 条）

- **#1211** `StreamOptions.incremental` 是**死配置**（`stream_options.rs:70`，全仓零消费者，`should_stream` 完全忽略）→ Java 的「累计被重置」不会出现，但**累计模式根本没实现**，设 `incremental(false)` 被静默忽略。
- **#1036** 模型流式阶段用 `tokio::select!` 与 `interrupt_pending` 竞速（`:3341`）可即时打断；但工具执行是裸 `run.observe(..).await`（`:4137`/`:4395`），不与中断 select，`ToolExecutionContext` 也不带取消令牌 → 长工具跑满才在下一个边界收敛为 `Interrupted`。
- **#459 / #2686** 子智能体**不继承**父的 `confirm_handler`（`materialize.rs` 从不设置）；子 agent 里命中 `Ask` 且无 handler 时整个 child run 直接报 `"permission asking (HITL) is not yet supported; tool '…'"`（`react_agent.rs:4352-4356`）。唯一例外是 ACP 的 `SubagentPermissionHook::on_child_ready`（`session.rs:319-374`）。
- **#680** 直接 A2A 调用**保住** metadata（`a2a-client/src/message.rs:691-694` ↔ `a2a-server/src/lib.rs:654`）；但 agent-as-tool 只发 `Msg::user(prompt)`（`agent_spawn.rs:676`），远端提交规格只有 `input: String` → 父消息 metadata 无处传递。
- **#3178** 子代理的 toolkit 是 **build 时刻**的 `toolkit.copy()` 快照（`harness_agent.rs:2756`）；ACP 在 build **之后**才把 MCP 工具写进 `self.agent.toolkit()`（`session.rs:2062-2064`）→ 子代理永远看不到。allowlist 未命中静默丢弃（`materialize.rs:61-72` 无 warn），子代理 build 还会无条件重注册内建工具。
- **#2985** 本地估算一律 `chars/2.5`（`compaction.rs:30`、`policy.rs:81`）低估 CJK 约 2.5 倍；但主路径已用 provider 实测 `input_tokens` 锚定（`usage.rs:84-96`、`compaction.rs:631-634`），正常有 usage 时不复现。残余：无 usage 的部署退回纯估算；provider 的 `ContextLength` 错误分类在 core/harness **零消费者**。
- **#2792** 完成通知文案只说 "N background subagent task(s) have completed"（`subagents.rs:270`），无 `M/N`；但同请求的 system 附加段用 `build_task_summary` 列出未投递任务状态（含 RUNNING）作为补偿。`wire_task_completion`（`task_bridge.rs:159`）除 re-export 外**零调用方**，build 走的是内联 `create_completion_callback`。
- **#2679** 没有 `<userId>/skills` 层：仓库列表 build 期静态组装，layer 3/4 指向**同一个** `.agents/skills`（`harness_agent.rs:2464-2465`），`load(&self)` 不接 ctx → 同实例所有用户看到同一套技能；同目录双加载产生 `x_workspace-shared` / `x_workspace-namespaced` 两个 id；沙箱投影根不含用户隔离目录。
- **#2266** `FilesystemTool::call` 丢掉 `_ctx` 自造 `RuntimeContext::default()`（`filesystem_tools.rs:441`）→ namespace 按 anonymous 解析，文件实际写成 `agents/<id>/users/<anonymous>/…`，回复仍是逻辑路径（`:481`）。

### 3.4 判「不存在 / 不适用」的要点

- **#1672 / #1623 / #1606 / #1681 系** — `RuntimeContext` 与 toolkit 都是显式传参/共享 Arc，无「组装期取 null」「深拷贝丢注册」的时序。
- **#1555 / #1556 / #1446 / #938 / #918 / #846 / #1203** — 恢复守卫只扫本次新入参；超时统一转 `error_result`；终止只看有无 tool call；工具必须返回完整 `ToolResultBlock`；无「pending 期间插消息」的校验；只发增量事件；每步现场读 active groups。
- **#2581** 不存在，且 Java 报告者已自行撤回（"这是我接口设计的问题"）：`agent-protocol/lib.rs:945` 用 `tokio::spawn` 跑完整个 run，SSE 只是 event-bus 订阅，断连不影响持久化。
- **#2271 / #2251 / #1117 / #618 / #1265** — Windows 走 `pwsh -EncodedCommand`、无 JUnit/无 mirror 线程、无 Reactor、无 PlanNotebook、无注解体系。
- **#3197 / #2791 / #2758 / #2688 / #2849 / #2181** — `RemoteFilesystem` 无索引快路径（`WorkspaceIndex.list_by_prefix` 零调用者）；`wait_async` 轮询权威状态不读 inbox；无 `Path::resolve` 命令输出（错误被吞成「清单静默为空」，属另一种隐性缺陷）；`SkillRepository::load(&self)` 无 ctx；无 volatile sandbox 单槽；技能文件走绝对路径 `std::fs` 不经 namespace。

### 3.5 累计新增需优先修复（第 3 轮）

1. **#3258 + #3253** 默认 remote 后端为 `Unrestricted`，绝对路径直达宿主 + 根 ls/grep 枚举文件系统根 —— 安全边界问题，一行改默认模式即可收敛。
2. **#2918 + #2182** 远程/分布式部署下会话历史与 index 仍写本地 —— 分布式部署等于失效。
3. **#2644** system prompt 写死错误的工具名 `execute_shell_command` —— 模型照提示调用必失败。
4. **#3042** 跨后端 move 破坏二进制文件。
5. **#2845** 进程内 sandbox 守卫是 noop，同 key 并发后写覆盖先写。
6. **#2664** `.skills-cache` 空集合时永不清理，已删除技能继续被注入。

---

## 轮次 4：`harness` 剩余 34 + `core/tool` 前 16

小计：**存在 8 / 部分存在 16 / 不存在 23 / 不适用 3**。

### 4.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #2162 | 后台任务取消无「已停止」确认 | **存在** | `harness/.../subagent/task/repository.rs:59` |
| #2159 | 子智能体通知丢 FAILED/CANCELLED 载荷 | 部分存在 | `harness/.../gateway/task_bridge.rs:112-133` |
| #2147 | close 早于 SessionTree 镜像完成 | 部分存在 | `harness/src/agent/harness_agent.rs:816-824` |
| #2146 | 分布式副本丢并发 turn | 部分存在 | `harness/.../middleware/state_checkpoint.rs:108-180` |
| #2145 | eviction 后完整工具输出仍留在 AgentState | 不存在 | `harness/.../tool_result_eviction.rs:65-120` |
| #2143 | Postgres UPSERT SQL 多逗号 | 不存在 | `extensions-postgresql/src/store.rs:188-193` |
| #2135 | Docker sandbox 在 Windows 破坏复杂 sh -c | 部分存在 | `harness/src/agent/sandbox/docker.rs:467-480` |
| #1985 | builder 模块 skills 加载报错 | 不适用 | 无 builder crate |
| #1772 | 沙盒执行命令语法错误 | 不存在 | `extensions-sandbox/src/fs_spec.rs:707-730` |
| #1769 | SessionTree flush 导致 JSONL 膨胀 | 不存在 | `harness/.../memory_flush.rs:170-215` |
| #1729 | k8s `/workspace` 写死 | 部分存在 | `harness/src/agent/sandbox/layout.rs:306` |
| #1702 | 沙箱下工具拿不到类型安全上下文 | 部分存在 | `core/src/agent/react_agent.rs:3618` |
| #1700 | runSubagentStream 不传 RuntimeContext | **存在** | `harness/.../gateway/harness_gateway.rs:392-410` |
| #1683 | 未转发按会话 interrupt | 不存在 | `harness/src/agent/harness_agent.rs:738-766` |
| #1650 | 工作区文件从未被使用 | 不存在 | `harness/.../workspace_context.rs:283-289` |
| #1644 | 未设 generateOptions 时 NPE | 不存在 | `core/src/agent/react_agent.rs:1831` |
| #1635 | 输出多一层 userId 目录 | 不存在 | `harness/.../workspace/manager.rs:172-179` |
| #1632 | 每用户 workspace 不持久 | 部分存在 | `harness/src/agent/sandbox/docker.rs:54,85-88` |
| #1626 | REST 端点呼叫外读沙箱 workspace | 不适用 | 无 dataagent/REST |
| #1619 | 不同 session/user 上下文互相可见 | 不存在 | `harness/src/agent/harness_agent.rs:2949-2961` |
| #1609 | edit 生成坏的 python3 -c | 不存在 | `extensions-sandbox/src/fs_spec.rs:733` |
| #1605 | 用户注册的 lifecycle 中间件不参与 | 不存在 | `harness/.../sandbox_lifecycle.rs:39` |
| #1602 | load_skill_through_path 报 Tool not found | 部分存在 | `harness/src/agent/harness_agent.rs:2624,2703` |
| #1587 | workspace 里文件 read_file 读不到 | 部分存在 | `harness/src/tool/filesystem_tools.rs:440-441` |
| #1584 | Docker sandbox state 反序列化失败 | 不存在 | `harness/src/agent/sandbox/state.rs:154-186` |
| #1578 | 子 Agent 不继承父 hooks | **存在** | `harness/.../subagent/materialize.rs:406-437` |
| #1577 | 主 agent 停止子 agent 不停 | **存在** | `harness/src/tool/agent_spawn.rs:370-390` |
| #1560 | uploadFiles base64 塞命令行 → E2BIG | 部分存在 | `extensions-sandbox/src/fs_spec.rs:707-730` |
| #1458 | glob `**/*.md` 不匹配直接子文件 | 不存在 | `harness/src/filesystem/local.rs:559-590` |
| #1453 | 拿不到 subAgent 流式事件 | 不存在 | `harness/src/tool/agent_spawn.rs:119-132` |
| #1451 | subagent task_output 被默认超时打断 | 不存在 | `harness/src/tool/task_tool.rs:181` |
| #1441 | Workspace 路径不一致导致 memory 加载失败 | 不存在 | `harness/.../workspace/manager.rs:493` |
| #1440 | interrupt 后会话记忆没保存 | 不存在 | `core/src/agent/react_agent.rs:4714` |
| #1396 | Windows `Cannot run program 'sh'` | 不存在 | `harness/src/filesystem/sandbox.rs:126` |
| #3301 | 枚举参数注解与 schema 不一致 | 不适用 | `agentscope-macros/src/lib.rs:193` |
| #3243 | HITL 中获取 suggestedRules | 部分存在 | `core/src/permission/mod.rs:85` |
| #3167 | updateToolGroups 对已有 toolkit 不生效 | 不存在 | `core/src/tool/toolkit.rs:435` |
| #2670 | AsyncTool 超时仍阻塞主线程 | 不存在 | `harness/.../middleware/async_tool.rs:225` |
| #2653 | Skill + ToolGroup 按需披露不 work | 部分存在 | `core/src/tool/search/middleware.rs:42` |
| #2441 | SkillManageTool 写死本地仓库 | 部分存在 | `harness/src/agent/harness_agent.rs:2487` |
| #2438 | 询问技能时带出工具 | 不存在 | `harness/src/agent/skill/prompt.rs:15-27` |
| #2407 | Mysql/Postgres 技能仓库表名不可配 | 不存在 | `extensions-skills/src/mysql.rs:32-99` |
| #2312 | OnActing 嵌套工作目录 | 不存在 | `harness/src/agent/sandbox/layout.rs:297-311` |
| #2169 | 技能激活但工具无权限 | 部分存在 | `core/src/agent/capability.rs:112-150` |
| #2091 | RemoteFilesystem 下 session 仍写本地 | **存在** | `core/src/agent/thread_key.rs:41-48` |
| #2065 | 回退 user.dir 泄露进程工作目录 | **存在** | `harness/src/agent/harness_agent.rs:2140-2172` |
| #2003 | streamableHttp 不暴露 resumableStreams | **存在** | `harness/.../mcp_client_manager.rs:350-366` |
| #1906 | skill 目录存在其他文件时出问题 | 部分存在 | `harness/src/agent/skill/repository.rs:180-207` |
| #1898 | 智能体能 execute 删除 workspace/他用户文件 | **存在** | `core/src/tool/dangerous.rs:44-51` |
| #1896 | 多用户不安全 & 后台任务空 ctx | 部分存在 | `harness/src/agent/bus/workspace_message_bus.rs:43-62` |

### 4.2 存在的 8 条

- **#1898（最严重，安全）** `DANGEROUS_COMMANDS` 已被**置空**（`core/src/tool/dangerous.rs:44-51`，注释称改由人工审批保障），`is_dangerous_command` 恒 false；默认 `LocalFsMode::Unrestricted` + 默认 `PermissionContextState` trivial 直接 allow（`react_agent.rs:4906-4912`）→ `execute` 可 `rm -rf .agentscope/workspace`、`rm -rf ../other-user`，文件工具也能写任意绝对路径。
- **#2065** 未配置 filesystem 时 `cwd = self.cwd.or(std::env::current_dir())` 并 `LocalFilesystem::new(cwd)`（`harness_agent.rs:2140-2172`），而 `LocalFilesystem::new` = `Unrestricted`（`local.rs:194`）→ agent 可读 `C:\Users\...`、IDE 模块目录、部署目录。与 #3258 同一根因（默认模式），区别是这里连 spec 都没配。
- **#2091** 与 #2918 同根：journal 写 `.agentscope/sessions/{agent}/{user}/{session}`（`thread_key.rs:41-48`），`RemoteFilesystemSpec` 前缀只有 `memory/ skills/ subagents/ knowledge/ plans/` + 遗留 `agents/{id}/sessions|tasks/` → 落 Pod 本地盘；spec 测试注释直接写着 “Everything outside the shared routes stays on local disk”。叠加问题：`WorkspaceJournalStorage` 每个读写都用 `RuntimeContext::new()`（`session/storage.rs:208/232/266/295/313`），`ThreadJournalStore::open` 干脆忽略 `_context`（`:412`）→ 即便补路由也只会进 `_default` 匿名命名空间。
- **#2003** `ClientStreamableTransport::new(&transport_options, None, true)`（`mcp_client_manager.rs:350-366`）第三参数是 SDK 的 `standalone`，为 true 时 `start()` 立刻 GET 建 SSE 流；SSE 被禁的服务器返回 405 → `FailedToOpenSSEStream` → `Failed to start MCP client`。`McpServerEntry`/`McpServerConfig` 都没有 `resumableStreams`/`openConnectionOnStartup` 字段。
- **#1700** `run_subagent(&self, subagent_id, messages)` 签名无上下文参数，内部硬编码 `msg_context: MsgContext::default_context()` + `user_id: None`（`harness_gateway.rs:396-403`），而同文件 `run_with_outbound` 会完整构造（`:292-299`）；`ChatUiChannel` 两处调用（`chatui.rs:265,275`）→ 调用方上下文全丢。
- **#1578** `SubagentBlueprint` 字段里**没有** middleware/hook（`harness_agent.rs:2749-2797`），`LeafFactory::build` 用 `HarnessAgent::builder()` 从零起链（`materialize.rs:407`）→ 父 agent 的 `.middleware(chain)`（`:1411`）不传下去，工具治理/HITL/审计/预算对子 agent 全失效；只保留权限路由用的 `subagent_runtime_hook`。
- **#1577** spawn 一律进独立 `tokio::spawn`（`agent_spawn.rs:571-583`），父 `interrupt()` 只 interrupt 内嵌 ReActAgent（`harness_agent.rs:730`），碰不到 `spawn_tool.live/active_local`；ACP `Session::cancel()` 只 `cancel_epoch.fetch_add`（`session.rs:2336`），`cancel_subagents()` 仅在 `close()` 调用（`:2345`）。
- **#2162** `cancel_task` 契约是「找到并尝试取消」（`repository.rs:59`）；本地 supplier 跑在 `spawn_blocking`（`workspace.rs:437`），`abort()` 停不了阻塞线程却立刻把状态写成 `Done(Err("task cancelled"))`（`background.rs:299-313`）；远程 cancel 失败只 `warn!` 后仍 `return true`（`workspace.rs:647,651`）。

### 4.3 部分存在（16 条）要点

- **#2159** FAILED 已带 `<task_error>`（`subagents.rs:285-292`），但取消路径 `Err(CANCELLED_SENTINEL)` 直接 `None`、不 `fire_callback`（`workspace.rs:498-501`）→ 不会唤醒父 agent，且 `TaskRecord` 无 `cancel_reason`。
- **#2147** SessionTree 镜像线程已删（`memory/session.rs:4`），但 `TaskRepository::shutdown()`（`workspace.rs:705-708`）与 `WakeupDispatcher::stop()`（`wakeup_dispatcher.rs:174-177`）**零调用方**，心跳每 30s 后台写 store，close 不构成写屏障。
- **#2146** 已有 `save_if_version` + `ConflictPolicy`（`state/store.rs:243`、`conflict_policy.rs:11-22`），但默认是 `Overwrite`，且 `SessionTurnGate` 是进程内 HashMap（`session_turn_gate.rs:99-107`）→ 非 durable 路径仍会后写覆盖；durable 有 epoch fencing 是健全的。
- **#2135 / #1560** 同一传输根因：`SandboxFilesystemAdapter::write` 把整份内容 base64 拼进 `printf %s '<b64>' | base64 -d`（`fs_spec.rs:720-727`），经 `docker exec ... sh -c <cmd>` 单参数下发（`docker.rs:477`）→ Windows 32KB / MAX_ARG_STRLEN 限制；`$(dirname ...)` 预检已消除并有用例（`fs_spec.rs:1693-1721`），但建议的 `sh -s` + stdin 未采纳。
- **#1729** 默认根仍 `/workspace`（`layout.rs:306`、`kubernetes.rs:54`），但可覆盖（`with_root`、`workspace_root`），且 start 先 `mkdir -p <root>`（`workspace.rs:56`）→ Java 那种硬失败不复现。
- **#1702** 工具只拿 `runtime: JsonMap`（`react_agent.rs:3618`、`tool.rs:24-38`），`RuntimeContext::extensions` 的类型安全通道**完全不转发**；但不存在 Java「仅开沙箱才丢」的回归。
- **#1632** 快照文件名用 `Uuid::new_v4()`（`docker.rs:54,85-88`）→ 容器重建后对不上旧 tar；`SandboxStateStore` 存活时可 resume，但无默认持久实现；无 `agentscope-dataagent` crate。
- **#1602** skill 工具在 `ToolFilter::apply`（`harness_agent.rs:2703`）**之前**注册（`:2624`），且 `load_skill_through_path` 不在 `HarnessPlatformTools::NAMES`（`tools.rs:31`）→ `tools.json` 的 allow 列表会删掉它，而 `<available_skills>` 仍注入。
- **#1587** `FilesystemTool::call` 丢掉 `_ctx` 硬造 `RuntimeContext::default()`（`filesystem_tools.rs:441`）→ store 命名空间按 `default`/匿名解析，会话命名空间里写的文件读不到（与 #2266 / #1896-Bug2 同根）；且 `WorkspacePathNormalizer` 构造后**全仓零调用**（`harness_agent.rs:2909`）。
- **#3243** `PermissionDecision.suggested_rules` 与 `with_suggested_rules` 存在（`permission/mod.rs:90,117`）但**零调用者**，`check_permission` 的 ASK 分支不生成建议，`evaluate_permission` 只取 `.behavior`、丢弃它，`ConfirmHandler` 签名里也没有建议。
- **#2653** core 有 `ToolSearchMiddleware` 与 `ToolGroupScope::{Meta,External}`，但 harness **零接线**（`ToolSearchMiddleware` 在 harness 无引用，也不调 `register_tool_group`/`update_tool_groups`），skill 与工具组完全解耦。
- **#2441** `SkillManageTool::new(main_repo, drafts_repo, ..)` 本身接受任意 `Arc<dyn SkillRepository>`（`manage.rs:55-70`），但 builder 写死 `WorkspaceSkillRepository::new_writable`（`harness_agent.rs:2508-2535`），用户传入的库只当**只读来源**（`repository.rs:381-418`）。
- **#2169** `CapabilitySurface` 里 skill 授权与 tool 授权是三个互不相干的维度（`capability.rs:44-56,112-150`），无 `<skillId>_skill_tools` 约定（`CORE__GAP_TRACKER.md:353` 已记录）→ 只授技能名时其配套工具被拒（文案伪装成 `Tool not found`）。
- **#1906** `collect_resources` 递归全量读入（`repository.rs:180-207`）：无 `.git`/隐藏文件过滤、无大小上限、二进制 base64 ×1.33、栈式遍历无 visited 且跟随软链 → 内存放大与潜在死循环；`write_skill_to_dir` 又把 extras 树整体复制。
- **#1896** Bug2 成立：`WorkspaceMessageBus`/`WorkspaceAsyncToolRegistry` 各持 `RuntimeContext::default()` 做全部 fs 读写（`workspace_message_bus.rs:43-62`、`async_tool.rs:93-110`）→ 命名空间回退 `anonymous_user_id`（`_default`）。Bug1（共享可变 sandbox 槽）Rust 结构上不存在。

### 4.4 判「不存在 / 不适用」的要点

- **#2145** eviction 的 `apply_to_state` 直接改写 `ctx.state.context`（`tool_result_eviction.rs:85-120`），文档明确说这是为了不把全文留在 state。
- **#2143 / #2407 / #3301** Postgres upsert SQL 正确且有占位符计数测试；技能仓库已有 `MysqlSkillRepositoryConfig`/`PostgresSkillRepositoryConfig`；Rust 宏不支持枚举参数（`classify()` 只识别 String/数值/bool/Value/Vec），无 Jackson 注解语义。
- **#1769 / #1772 / #1609 / #1584 / #1396 / #1458 / #1451 / #1441 / #1440** 旧 SessionTree 已删且 offload 按 message id 幂等；沙箱命令不再有 `$(`/双引号；快照只持久化 `SandboxSnapshotRef` 枚举并有往返测试；Windows 走 `pwsh -EncodedCommand`；globset `**/` 匹配零层且有用例；`task_output`/`wait_async` 覆写 `is_long_running` 跳过 300s 超时；memory 全用相对路径；中断是 Err 项而非异常，flush/checkpoint 仍执行。
- **#1683 / #1650 / #1644 / #1635 / #1619 / #1605 / #3167 / #2670 / #2438 / #2312** 门面已暴露 `interrupt_session`/`interrupt_run`/`clear_context_for`；工作区文件确实注入 prompt；`GenerateOptions::default()` 无 null；默认无 namespace；每会话一个 agent 且 session 构建期固定；lifecycle 中间件是真 `Middleware`；工具可见性唯一权威是 toolkit group 管理器；AsyncTool 用 `spawn`+`timeout` 竞速；skill prompt 只渲染技能；沙箱根恒 `/workspace`。
- **#1985 / #1626** 无 `agentscope-builder`、无 dataagent/Spring REST 层。

### 4.5 累计新增需优先修复（第 4 轮）

1. **#1898** `DANGEROUS_COMMANDS` 置空 + 默认 Unrestricted + trivial 权限 → `execute` 可删任意路径。
2. **#2065 / #3258 / #3253** 默认文件系统为 `Unrestricted`（一个连 spec 都没配、一个是 remote 默认）→ 收敛为 `Rooted`/`Sandboxed` 是同一处修改。
3. **#2091 / #2918 / #2182** `.agentscope/` 不在远程路由，且 journal storage 用 `RuntimeContext::new()` → 分布式部署下会话历史永久落本地。
4. **#1896-Bug2 / #1587 / #2266** 三处 `RuntimeContext::default()` 破坏命名空间路由，修法一致。
5. **#1578 / #1577 / #1700** 子 agent 不继承 middleware、不随父取消、不传上下文 —— 同一族「治理/身份没传下去」。
6. **#2003** MCP streamable transport 硬编码 `standalone=true`，无配置出口。
7. **#1602 / #2441 / #2653** skill 与工具治理接线缺失（allow 列表能删掉 skill 工具、DB 技能库只读、按需披露未接线）。

---

## 轮次 5：`core/tool` 剩余 20 + `core/memory` 22 + `core` 6 + `ext/memory` 2

小计：**存在 14 / 部分存在 13 / 不存在 19 / 不适用 4**。

### 5.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #1775 | read_file 不拼 userId | 部分存在 | `harness/src/tool/filesystem_tools.rs:441` |
| #1705 | 中文 Skill 名路径编码异常 | 不存在 | `harness/src/agent/skill/load_tool.rs:155-166` |
| #1696 | todo_write 缺 display metadata | **存在** | `core/src/tool/todo.rs:16-22,141` |
| #1689 | 并行 toolcall 被拼成 `{}{}` | 部分存在 | `extensions-model/src/openai/mod.rs:1594-1601` |
| #1674 | glob 宣传 `**/*.java` 但沙箱不支持 | **存在** | `extensions-sandbox/src/fs_spec.rs:791-806` |
| #1633 | 流式拿不到工具完整输出 | 部分存在 | `core/src/agent/react_agent.rs:4460-4472` |
| #1569 | 重复 load_skill 未去重 | **存在** | `harness/src/agent/skill/load_tool.rs:120-196` |
| #1442 | frontmatter 未加引号冒号容错 | 不存在 | `extensions-skills/src/git.rs:852-906` |
| #1438 | 多行数组元数据报错 | 不存在（不报错但静默丢弃） | `extensions-skills/src/git.rs:866,896-899` |
| #1306 | 工具组动态激活后不可用 | 不存在 | `core/src/tool/group.rs:163-290` |
| #1216 | 流式参数被拆成两次调用 | 不存在 | `extensions-model/src/dashscope/parser.rs:66-92` |
| #1212 | ShellCommandTool 不触发审批 | 不存在 | `core/src/agent/react_agent.rs:4911-4916` |
| #913 | No default JsonSchemaValidatorSupplier | 不适用 | 无 SPI |
| #912 | MCP 偶现初始化失败 | 部分存在 | `harness/src/agent/mcp.rs:134-183` |
| #885 | SSE 绝对 endpoint 不匹配 base URL | 不存在 | `rust-mcp-transport/src/client_sse.rs:110-131` |
| #833 | onErrorComplete 不存在 | 不适用 | 无 Reactor |
| #770 | `path '.'` 越界拒绝 | 部分存在（症状相反） | `harness/src/filesystem/composite.rs:115-116` |
| #755 | MCP HTTP 无法忽略证书 | **存在** | `harness/src/agent/mcp_client_manager.rs:146-168` |
| #665 | SubAgent 用主 agent MCP 工具偶现报错 | 部分存在 | `harness/src/agent/mcp_client_manager.rs:517,551` |
| #626 | @Tool 带其它注解无法注册 | 不存在 | `agentscope-macros/src/lib.rs:306-350` |
| #2701 | 图片 url/base64 未存进 session | 不存在 | `core/src/state/agent_state.rs:23` |
| #2694 | 压缩绕过 disableMemoryHooks | 不存在 | `harness/src/agent/harness_agent.rs:2344-2372` |
| #2693 | pruneOldSessions 路由不匹配 | 部分存在 | `harness/.../memory_maintenance.rs:184-216` |
| #2692 | 空 mtime 使水位线失效 | 部分存在 | `harness/src/filesystem/mod.rs:48-55` |
| #2691 | 远程 store 下 session 仍写本地 | 部分存在 | `harness/src/agent/session/storage.rs:403` |
| #2690 | 路由前缀劫持宿主挂载点 | **存在** | `harness/src/filesystem/composite.rs:23-25,40` |
| #2566 | compaction 泄漏宿主路径 | 不存在 | `harness/.../memory/compaction.rs:807` |
| #2564 | 纯聊天也创建 sandbox | **存在** | `harness/.../sandbox_lifecycle.rs:44-66` |
| #2517 | ProjectAwareOverlay 双根路由 | 不适用 | `filesystem/spec.rs:103-138` |
| #2501 | USER 隔离下读不到 knowledge | 不存在 | `harness/src/filesystem/local.rs:338-350` |
| #2421 | Daytona snapshot 创建失败 | **存在** | `extensions-sandbox/src/daytona.rs:134-159` |
| #2329 | IsolationScope 不作用于 execute cwd | **存在** | `harness/src/tool/shell_execute_tool.rs:85` |
| #2327 | PostgresAgentStateStore 记忆问题 | 部分存在 | `extensions-postgresql/src/state.rs:120-131` |
| #2276 | concatWith 后置钩子阻塞 call complete | **存在** | `harness/.../middleware/memory_flush.rs:160` |
| #2225 | MemoryConsolidator 阻塞 agent 调用 | 不存在 | 无 `block()`；`.await` 是 spawn 内运行 |
| #2173 | state_data 字段一直叠加 | 部分存在 | `harness/.../middleware/state_checkpoint.rs:233-238` |
| #1969 | 前端手动压缩无效果 | **存在** | `harness/.../memory/compaction.rs:219-238` |
| #1936 | 历史自动拼接 attached_file | 不存在 | `core/src/message/block.rs:246-256` |
| #1740 | compressIfNeeded 单次执行 | **存在** | `harness/.../context/context_middleware.rs:106,130` |
| #1262 | 多条 system 消息导致请求失败 | 不存在 | `core/src/model/format.rs:60-92` |
| #1094 | 长期记忆导致输出完成延时 | **存在** | `extensions-mem/src/middleware.rs:216-241` |
| #1093 | JVM shutdown hook 与 Spring 冲突 | 不适用 | 无进程级钩子 |
| #3318 | Media URLSource 无界读取/file URI | 部分存在 | `core/src/formatter/media_utils.rs:259-280` |
| #3237 | SSE 断连丢失整轮进度 | 不存在 | `core/src/agent/react_agent.rs:2112-2116` |
| #3199 | base64 临时路径不确定 | **存在** | `core/src/formatter/base_formatter.rs:90-174` |
| #2991 | SSE 耗尽 boundedElastic | 不存在 | `extensions-model/src/common/transport.rs:375-410` |
| #2256 | 单例并发总等待 | 部分存在 | `core/src/agent/react_agent.rs:792-800` |
| #2254 | 重写 snapshotSpec 不生效 | 不存在 | `extensions-sandbox/src/fs_spec.rs:873-876` |
| #2005 | ReMeClient 与服务端版本不对应 | **存在** | `extensions-mem/src/reme/client.rs:12-15` |
| #1801 | PlanNotebook.currentPlan 未持久化 | 不存在 | `core/src/state/context.rs:524-546` |

### 5.2 存在的 14 条

**A. 记忆/压缩生命周期（5 条，与 #3059/#2985 同族）**

- **#2276** 与记忆 flush 同一根：`downstream.chain(once(tail))` 且 tail 内 await 记录 → call 的 complete 要等收尾（`memory_flush.rs:160`）。比 #3059 更进一步的是：`record_lossy` 走同步 `on_agent` 尾部，无超时、无 spawn。
- **#1740** `with_overflow_compaction` 是**单次** pass（`context/context_middleware.rs:106,130`）：`needs_compaction` 命中后执行一次策略，若仍未降到阈值就照发请求，不循环。
- **#1969** 手动压缩走 `should_compact` 门控（`compaction.rs:219-238`）——Token 级策略在阈值以下时 `force=true` 也不执行；对比 `apply_budget` 有 `force` 分支但 TailKeep 返回 `None` 直接退出。
- **#1094** `extensions-mem/src/middleware.rs:216-241` 在回合尾 `record_lossy(...).await`，模块文档（`:41-45`）明说「Java 的 asyncRecord 异步模式未移植」；底层 HTTP 默认 60s 超时，与 Java 现场一致。
- **#2693 / #2692 / #2691 / #2327 / #2173** 见 5.3。

**B. 沙箱（3 条）**

- **#2421** `DaytonaSandboxClient` 创建时把 `cpu/memory/disk` 的默认值无条件写进请求（`extensions-sandbox/src/daytona.rs:134-159`）→ 用了 snapshot 时服务端拒绝（snapshot 已定规格，不允许再传）。
- **#2329** `ShellExecuteTool` 的 cwd 来自 `sandbox_root()` 常量（`shell_execute_tool.rs:85`），`IsolationScope`（SESSION/USER）完全不参与 → `execute` 永远在同一个共享根执行，多用户看不到自己的隔离目录。
- **#2564** `SandboxLifecycleMiddleware::on_agent` 在链最前面**无条件** `manager.acquire(...)`（`sandbox_lifecycle.rs:58`），且 `acquire` 先抢分布式执行守卫（`sandbox/manager.rs:204-218`，fail-closed）→ 纯聊天轮也要付一次锁 + resume/create。

**C. 文件系统 / 路由（2 条）**

- **#2690** `route_for_path` 先 `trim_start_matches('/')`（`composite.rs:23-25,40`）再匹配前缀 → 宿主绝对路径 `/knowledge/services.md` 被劫持到 `knowledge/` 路由，两层都 miss 后直接 `File not found`，**不回退 defaultBackend、无 WARN**；可劫持 `memory/ skills/ subagents/ knowledge/ plans/` + `AGENTS.md/MEMORY.md/tools.json`。
- **#1674** 沙箱侧 `glob` 原样下发 `find <target> -name '<pattern>'`（`fs_spec.rs:791-806`），`find -name` 只比 basename → `**/*.md` 永远 0 命中；`grep --include`（`:774-779`）同理；而工具描述明确宣称支持 `**` 并举例 `**/*.java`，本地后端（globset + `literal_separator`）却是支持的。

**D. 工具 / 协议（4 条）**

- **#1696** `TodoItem` 只有 `content/status/priority`（`core/src/tool/todo.rs:16-22`），落库时 `.description(&item.content)`（`:141`）把 description 覆写成标题，而底层 `Task`（`core/src/state/context.rs:33-48`）本身有 `description` 与 `metadata`；serde 无 `deny_unknown_fields`，模型多传的字段静默丢弃。
- **#1569** `LoadSkillTool::call` 只做 capability grant + 路径校验 + 读全文（`skill/load_tool.rs:120-196`），全仓无 `is_skill_active`/`active_skills` 集合（rg 零命中）→ 同批次重复调用重复吐 2000+ token。
- **#755** `McpServerConfig` 只有 name/command/url/headers/transport/propagate_meta/prefix_tool_names（`mcp_client_manager.rs:146-168`），没有注入 `reqwest::Client` 或 TLS 的入口，SDK 内部 `Client::new()` → 自签证书网关无法使用；与 A2A/COS 已提供 `with_client` 不一致。
- **#3199** `save_base64_data_to_temp_file` 用 `format!("agentscope_{}", Uuid::new_v4())` 命名（`base_formatter.rs:168`）→ 同一历史重复格式化得到不同绝对路径，历史前缀变化破坏 prompt/prefix cache，且每次多写一个临时文件。

**E. 外部依赖（1 条）**

- **#2005** `SUMMARY_ENDPOINT="/summary_personal_memory"`、`RETRIEVE_ENDPOINT="/retrieve_personal_memory"` 硬编码（`extensions-mem/src/reme/client.rs:12-15`），构造函数不接受端点配置 → 对接 ReMe 0.4.x（改为 `POST /{job_name}`）必然 404。

### 5.3 部分存在（13 条）要点

- **#2692（重要，此前未发现）** 远程 `ls` 不填 mtime：`FileInfo::file()` 把 `modified_at` 固定为空串（`harness/src/filesystem/mod.rs:48-55`），`RemoteFilesystem::ls` 全用它（`remote/mod.rs:218-222`）→ overlay 上层条目 mtime 为空 → `is_modified_after` 对空串一律返回 true（`consolidator.rs:206-208`）→ **每次 consolidation 都把全部日更文件重喂 LLM，水位线形同虚设**（仅共享存储部署中招；本地 `ls` 有真实 mtime）。
- **#2693** 剪枝遍历根是 `.agentscope/sessions`（`session/storage.rs:403`），不在路由内（同 #2918/#2091）；**新根因**：整条 maintenance 被 `if !self.disable_memory_hooks` 包住（`harness_agent.rs:2344-2372`）→ 一关记忆钩子就完全不剪枝；且窗口被 `DEFAULT_JOURNAL_RETENTION_DAYS=30` 截顶（`memory_maintenance.rs:220-231`），配 180 天无效。
- **#2691 / #1775** 同根 #2918/#2091 + #2266：session 落本地，且 journal storage 每个读写都用 `RuntimeContext::new()`（`storage.rs:208/232/266`）→ 即便补路由也只进 `_default`。
- **#2327** Postgres state 存储：`state_data` 是整段 JSON（含 Image/base64，因此 #2701 不成立），但 `load` 不校验版本、`save` 无 CAS（`extensions-postgresql/src/state.rs:120-131`）→ 并发覆盖；JSON 无压缩，长会话体积线性增长。
- **#2173** checkpoint 每次存整份 `AgentState`（`state_checkpoint.rs:233-238`），无压缩/无差量 → `state_data` 随轮次线性增长（与 #2918 的写放大同族）。
- **#2276** 见 5.2-A。
- **#1689** 按 index 分开累积正确（`openai/mod.rs:1720,1756`）；但 `index: #[serde(default)] u32`（`:1595-1596`）在 provider 省略 index 时全部落到 0，arguments 直接 `push_str` 拼成 `{...}{...}` → `parse_arguments` 失败返回空 map（`:1832-1837`），工具带空参执行且错误不进任何事件。
- **#1633** 文本结果会以一次 `TOOL_RESULT_TEXT_DELTA` 全量发出（`react_agent.rs:4460-4472`），但非文本 block（Data/Image/Audio）既不进 delta，`ToolResultEnd` 也不带 output，`ToolResultDataDelta` 在 core **零发射点**。
- **#912 / #665** MCP 注册是单次 `tokio::spawn`（`mcp.rs:134-183`）无重试/退避，且 `connect` 持全局 `clients` 锁跨 `await`（`mcp_client_manager.rs:266-267,379`）、`call_tool_with_meta` 同样持锁到 `:551` → 一次慢初始化/一次慢工具调用阻塞所有 MCP 操作；#665 叠加 #3178 的 build 期快照。
- **#770** 症状相反：`"."` 能正常通过（`local.rs:257-292`），但 `composite.rs:115-116` 的根特判让 `"/"`/`"."` 逃到默认后端（同 #3253）。
- **#3318** `download_url_to_base64` 先 `response.bytes().await` 全读再判 `MAX_SIZE_BYTES`（`media_utils.rs:259-269`），`get_bytes` 同样无上限；`is_local_file` 排除 `file://` 使 `strip_prefix("file://")` 成为不可达分支；DashScope 先用 `validate_image_extension` 才看 `mime_type`（`converter.rs:295-306`）。
- **#2256** `call` 的串行化 key 取 agent 自身 state 的 session_id（`react_agent.rs:792-800`）→ 单例多用户共用时全部排在同一 FIFO；按会话建 agent 才并发。流式入口不走 CallGate，靠 journal writer 租约互斥。

### 5.4 判「不存在 / 不适用」的要点

- **#1442 / #1438** frontmatter 是手写行解析（`split_once(':')` 取第一个冒号、`- ` 行直接 continue），**不报错**但数组元数据静默丢弃（`git.rs:866,896-899`）；相邻隐患：subagent 声明走真 `serde_yaml`，同类未加引号冒号会静默整份丢弃（`subagent/loader.rs:35`，无 warn）。
- **#1306 / #1216 / #1212 / #626 / #885 / #1705** 单 Mutex 守 `active + active_groups`；DashScope 累加器按 id 归并（id 为空才并入最近具名调用）；shell 由 PermissionEngine 的 Ask + confirm_handler 管（无 handler 直接拒）；`#[tool]` 宏保留其它属性；SSE 按 origin 比较；Rust `Path` 无 locale 编码依赖。
- **#2701 / #2694 / #2566 / #2501 / #2225 / #1936 / #1262 / #3237 / #2991 / #2254 / #1801** 状态/journal 存整段 Msg JSON；`MemoryFlushManager` 只在 `!disable_memory_hooks` 内构造；`build_summary_message(summary, None)` 唯一生产调用；本地 `resolve` 不看 ctx，与 `readKnowledgeMd` 同路径；Rust 无 `block()`；文档改写发生在入参与推送路径、`TextBlock` 输入不做转换（`user_text_from_blocks` 只拼 Text，不注入显示标记）；`format_messages` 跳过非首条 System 而非抛错；失败/中断/取消路径均 `commit_loop_state`（仅 drop future 会丢当轮）；`bytes_stream` 全异步；`snapshot_override` 优先生效；plan 态随 `AgentState` serde 全量落盘。
- **#913 / #833 / #2517 / #1093** 无 SPI/ServiceLoader；无 Reactor；`projectWritable`/`ProjectAwareOverlay` 未迁移（project 只读下层、读写同源）；库不自装进程级信号钩子。

### 5.5 累计新增需优先修复（第 5 轮）

1. **#2692** 远程 `ls` 空 mtime → consolidation 每次全量重喂 LLM（共享存储部署的记忆成本与延迟爆炸）。
2. **#2276 / #1094** 记忆收尾挂在回合尾串行执行（无超时、无 spawn），与 #3059 一并改为 fire-and-forget。
3. **#2690** 路由前缀劫持宿主绝对路径且不回退、无告警。
4. **#1674** 沙箱 glob 用 `find -name`，`**` 全空（描述与实现不一致）。
5. **#1740 / #1969** 压缩单次 pass + 手动压缩被节流跳过。
6. **#2329 / #2564** `IsolationScope` 不作用于 shell cwd；纯聊天也 acquire sandbox。
7. **#755 / #912** MCP 无 TLS 客户端注入、注册无重试且持全局锁跨 await。

---

## 轮次 6：`(unlabeled)` 剩余 32 + `ext/integration` 16 + `core/model` 2

小计：**存在 11 / 部分存在 11 / 不存在 19 / 不适用 9**。

### 6.1 汇总

| Issue | 标题（简） | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- |
| #3683 | jdbc 缺 JdbcAgentSkillRepository 类 | 不存在 | `extensions-jdbc/src/lib.rs:93` |
| #3671 | 自定义 FS 包装器导致 ShellExecuteTool 不注册 | 不存在 | `harness/src/agent/harness_agent.rs:3130-3137` |
| #3667 | RemoteFilesystem.exists 全量扫描 → OOM | **存在** | `filesystem/remote/mod.rs:443-460,81-97` |
| #3651 | 是否需要重新构建智能体 | 不适用 | `agent/sandbox/key.rs:55-77` |
| #3650 | 空 Mono 静默结束 | 不存在 | `core/src/agent/react_agent.rs:4476-4514` |
| #3647 | 会话列表逐会话全量重放 | 不存在 | `harness/.../session/storage.rs:423-476` |
| #3644 | Docker 容器名确定但 liveness 按键控 | **存在** | `agent/sandbox/docker.rs:270-340` |
| #3612 | 缺删除原生 session 的 API | 不存在 | `core/src/journal/log.rs:216-226` |
| #3611 | Docker 部署 service 启动报错 | 不适用 | 无 service crate |
| #3599 | 按文档 docker 部署 service 失败 | 不适用 | 无 service crate |
| #3567 | 换代后 journal 缺删除/迁移 | 部分存在 | `harness/src/agent/harness_agent.rs:564` |
| #3558 | service 后台违反唯一键 | 不适用 | 无 service crate |
| #3510 | 共享水位线跨用户跳过 ledger | 不存在 | `agent/memory/consolidator.rs:51,184-205` |
| #3475 | emitSafely 吞掉投递异常 | 不存在 | `core/src/observation/mod.rs:263-300` |
| #3433 | flush 时间格式默认 UTC | **存在**（同 #3088） | `agent/memory/flush.rs:132,169` |
| #3426 | isolationScope 下技能路径重复命名空间 | 不存在 | `filesystem/spec.rs:44-52` |
| #3192 | 沙箱 prompt 与容器文件系统不一致 | 部分存在 | `middleware/workspace_context.rs:291-292,461-475` |
| #3161 | E2B 云沙箱 Windows 必 400 | 不存在 | `extensions-sandbox/src/remote.rs:871-892` |
| #3063 | Redisson 缺 ReturnType.LONG | 不适用 | 无 shaded 副本 |
| #2979 | k8s 每次调用删 claim | 部分存在 | `middleware/sandbox_lifecycle.rs:60-62` |
| #2977 | aistio agent_shares 表不创建 | 不适用 | controlplane 已删 |
| #2962 | 零 chunk 当静默成功 | **存在** | `core/src/agent/react_agent.rs:3431,2218-2226` |
| #2959 | MCP 空响应 202+text/plain | 部分存在 | 第三方 transport `http_utils.rs:23-30` |
| #2950 | 多子 Agent 并行丢结果注入 | 不存在 | `gateway/wakeup_dispatcher.rs:121-145` |
| #2937 | Windows session key 含 ':' | 部分存在 | `agent/workspace/manager.rs:335-337` |
| #2924 | doExec 在 Windows 剥双引号 | 不存在 | `agent/sandbox/docker.rs:467-480` |
| #2829 | 重试配置对工具失败无效 | **存在** | `core/src/tool/executor.rs:231-262` |
| #2821 | 长期记忆收尾同步阻塞 | **存在**（同 #1094） | `extensions-mem/src/middleware.rs:216-236` |
| #2790 | cancelTask 停不下运行中的 subagent | 部分存在 | `subagent/task/workspace.rs:590-655` |
| #2789 | cancelTask 无检查点 | 部分存在 | 同上 |
| #2773 | HITL 中断状态跨实例丢失 | **存在** | `extensions-agent-protocol/src/lib.rs:945-980` |
| #2772 | 共享磁盘 `.skill-cache` 竞争 | 部分存在 | `agent/skill/stager.rs:224,466` |
| #2911 | copilotkit connect 重放重复激活中断 UI | 不适用 | 无 copilotkit crate |
| #2736 | A2A spring-boot 一服务一 agent | 不适用 | 无 spring-boot 模块 |
| #2651 | ssh 访问 git skill 仓库报错 | 不存在 | `extensions-skills/src/git.rs:437-472` |
| #2507 | a2a sse 事件超 1000 卡住 | 不存在 | `extensions-a2a-server/src/lib.rs:642-664` |
| #2265 | 钉钉 client 初始化报错 | 不适用 | 无 channel-dingtalk crate |
| #2102 | 自定义工作流 agui 无事件流 | 部分存在 | `extensions-agent-protocol/src/agui.rs:285,399` |
| #2070 | AgentRun MCP initialize 新建沙箱不复用 | 部分存在 | `extensions-sandbox/src/agentrun.rs:288-302` |
| #1907 | AgentRunDataPlaneHttp 未解包 data 信封 | **存在** | `extensions-sandbox/src/agentrun.rs:1060-1076` |
| #1821 | A2A BufferingTube 溢出 | 不存在 | 无 Tube/buffer 队列 |
| #1795 | oss 是否改用 awssdk | 不存在 | `extensions-oss/src/s3.rs:40-98` |
| #1790 | 单技能仓库（无 skills 子目录）无法加载 | **存在** | `agent/skill/repository.rs:322-333` |
| #1455 | a2a client 请求 404 | 不存在 | `extensions-a2a-client/src/lib.rs:140-178` |
| #1381 | 引入 A2A 后 nacos 需重复配置 | 不适用 | 无 spring 装配 |
| #1378 | A2A Streaming 重复输出 | 不存在 | `extensions-a2a-server/src/lib.rs:676-703` |
| #750 | studio 请求结束仍 running | **存在** | `extensions-agent-protocol/src/lib.rs:945-980` |
| #321 | agent.call 到 a2a 后不再调模型 | 不存在 | 服务端 ReAct 循环自洽 |
| #3147 | 调用 llm 超时 | 部分存在 | `model/execution_config.rs:8-16` |
| #2700 | 图片 url 下载失败致 400 | **存在** | `extensions-model/src/common/media.rs:410-428` |

### 6.2 存在的 11 条

- **#3667（高）** `RemoteFilesystem::exists` 精确未命中后 `for item in self.search_all_items(ctx)?` 累积**整个 namespace**（`remote/mod.rs:443-460`），`search_all_items` 每页 100 循环把所有 `StoreItem`（`value` 是完整内容）塞进一个 Vec（`:81-97`）；JDBC 实现 `search` 逐行 `parse_value`（`jdbc_store.rs:230-236`）→ 与 Java 同构 OOM。`ls/grep/glob/delete/move_path` 全部共用它，且 `session/storage.rs:486-489` 删会话前先调 `exists`。
- **#3644（高）** `ensure_container_running` 只看持久化 `container_id`（`docker.rs:270-300`），容器名却是确定性的 `agentscope-sandbox-{session_id}`（`:303`）；`run_command` 超时只 `kill_process_tree` 杀 CLI（`:699`），`docker run` 无 `--rm`（`:573-580`），`shutdown` 只按 `container_id` 删（`:453`），全 crate 无按名 inspect/reconcile → daemon 侧容器占名后每轮 `docker run` 必然 exit=125，永久不可恢复。
- **#2962** 流正常结束但无内容 chunk 时 `consume` 返回 `Ok(())` → `return Ok(acc.build_final_message())`（`react_agent.rs:3431`），`build_final_message` 空 blocks 也照造 assistant 消息（`:307`），`ModelStop` 且无工具调用即 `return Ok(msg)`（`:2218-2226`）；重试/fallback 只在 `Err` 分支，唯一痕迹是 INFO 级 "empty completion — ReAct loop will terminate"（`agent_trace.rs:91`）。
- **#2829** 重试循环确实能看到 `Err`（`executor.rs:255-262`），但 `Error::is_retryable()` 只认 `HttpStatus/Http/Io`（`core/src/error.rs:130-136`），工具失败一律 `Error::Tool` → 永不重试；超时更是 `tokio::time::timeout(..).map_err(..)?` 用 `?` 直接跳出循环（`:241-247`）；`ExecutionConfig` 无 `retryOn` 谓词，`tool_defaults().max_attempts=Some(1)`，且 executor 无一条重试用例。
- **#2773 / #750** `AgentProtocolTaskStore` 的任务 run 由 `tokio::spawn` 驱动（`agent-protocol/src/lib.rs:945`），HITL 中断态与「running」标记都只在内存 `tasks` map 里；跨实例（多副本）时另一个副本看不到该 interrupt，且进程重启/实例切换后状态永远停在 running，没有任何持久化钩子。
- **#1907** AgentRun 数据面响应是 `{"data": {...}}` 信封，`agentrun.rs:1060-1076` 直接对顶层做 `serde_json::from_value::<T>`，未先取 `data` → 反序列化失败或得到空结构。
- **#1790** `WorkspaceSkillRepository::load` 只在 `<root>/skills/*` 下枚举（`repository.rs:322-333`），无「root 自身就是单个技能目录（含 SKILL.md）」的分支 → 单技能仓库（无 `skills/` 子目录）永远返回空。
- **#2700** 模型侧把图片 URL 直接透传给 provider（`extensions-model/src/common/media.rs:410-428`），下载失败由 provider 报 400 `Failed to download multimodal content`；Rust 无本地预取/校验、无降级为文本提示的路径。
- **#3433** 同 #3088：`flush.rs:132` 用 `Local::now()` 命名日目录，`:169` 用 `Utc::now().to_rfc3339()` 写条目标题，`MemoryConfig` 无时区/格式配置。
- **#2821** 同 #1094：`record_lossy(...).await` 挂在 `on_agent` 的 `chain(once(tail))` 尾部（`mem/middleware.rs:216-236`），模块文档明说 Java 的 `asyncRecord` 未移植。

### 6.3 部分存在（11 条）要点

- **#3567** 删除入口已有（`ThreadJournalStore::delete` + `HarnessAgent::delete_session` + ACP `session/delete`），旧 `agents/...` 布局也同时清理；但 `SessionHistoryMode::Legacy` 与 `EventLog` 仍互斥（Legacy 下 `session_log`/`delete_session` 直接报 "native session history is not enabled"），无 state-store→journal 迁移；durable `SessionBackend` 只有 `close_session`（无 delete），ACP 删除失败仅 `warn!` 后照常回成功。
- **#3192** `Sandbox root:` 改为活动 sandbox 的 `cwd()`（`workspace_context.rs:526-531`），但依赖 downcast 到 `SandboxBackedFilesystem`——`PinnedSandboxFilesystem`/`RoutedSandboxFilesystem` 的 `as_any` 返回自身（`filesystem/sandbox.rs:405-409`）→ 回落宿主路径；`The workspace directory is:` 在 sandbox 模式仍是宿主工作区（`:291-292`）→ prompt 同时出现两个互斥路径。
- **#2979** 每轮 `release` → `close()` → `kill_sandbox` = `delete_claim_in`（`kubernetes.rs:1804-1807`）→ 下轮 `resume` 404 后重建；claim 名按 session 派生故同名。差别：Rust 无「卡 30 秒」，只是白跑一次完整重建。
- **#2959** 报告的具体路径已规避（202/204 在媒体类型校验前短路），但 `validate_response_type` 对 `application/json; charset=utf-8` 仍精确匹配失败 → `UnexpectedContentType`（第三方 `rust-mcp-transport-1.0.0/src/utils/http_utils.rs:23-30`）。
- **#2937** `task_record_path` 裸拼 `session_id`（`workspace/manager.rs:335-337`、`task/workspace.rs:768-770`、`memory/flush.rs:177`、`state_checkpoint.rs:84`），`with_session_id` 无净化（`runtime_context.rs:225-228`），`validate_path` 放行 ':' → Windows 上 NTFS 拒绝，且 `write_utf8_workspace_relative` 用 `let _ =` 吞错（`manager.rs:329`）→ 任务记录**静默丢失**。对照：journal 走 base64（`thread_key.rs:45-52`）、ACP 用 `[A-Za-z0-9_-]` 白名单（`session.rs:934-947`）。
- **#2790 / #2789** 显式 `task_cancel` 能真中断（`Adopted` + `handle.abort()` + drop 守卫），但父 run 的 interrupt/cancel 不遍历子任务（同 #1577），子 agent 继续烧 token 并 push inbox；`task_tool.rs:294-296` 无条件回 "Cancellation requested successfully"。
- **#2772** `.skills-cache` 写入是 `std::fs::write`（`stager.rs:224`）无锁无临时文件，`garbage_collect_orphans` 与写入可能交叉（`:466`）→ 共享盘多实例并发下会读到半写。
- **#2102** `agui.rs:285,399` 只把 Text/ToolCall 映射成 AG-UI 事件，无 Reasoning/Thinking 隧道（`AGUIReasoningBridge` 在 `:246`，端点未接线）→ 自定义工作流拿不到 REASONING_MESSAGE_CONTENT。
- **#2070** `AgentRunSandbox::start` 走 `resolve_or_create_sandbox`（`agentrun.rs:288-302`），MCP initialize 触发的启动不复用 `CreateSandbox` 已建的带 OSS/NAS 挂载实例（按需新建）。
- **#3147** 超时来自 `ExecutionConfig` 默认（`model/execution_config.rs:8-16`）且 `Error::is_retryable` 不认超时 → 网关慢时直接失败，无自动降级。

### 6.4 判「不存在 / 不适用」的要点

- **#3683 / #3671 / #3650 / #3647 / #3612 / #3510 / #3475 / #3426 / #3161 / #2950 / #2924 / #2651 / #2507 / #2265-#2977 / #1821 / #1795 / #1455 / #1378 / #321** Rust 侧结构性不成立：单 crate 无 jar 分包；无 `instanceof`，shell 工具无条件包装；`summarizing` 恒合成非空 MaxIterations 消息且 `call -> Result<Msg>`；`list` 只做目录枚举；删除 API 齐备；水位线 `memory/.consolidation_state` 与 memory 同命名空间；observation 记录失败直接抛错；`LocalFilesystemSpec` 无 `isolationScope`；反斜杠归一 + 显式 root 优先；wakeup 模式为「挂起 + turn 后重试」；直连 docker.exe 不经 cmd.exe；git 仓库用 `git2` 无 shell；a2a-server 用 `UnboundedSender` 无固定容量 Tube；OSS 基于 `aws-sdk-s3`；a2a client 按 origin 拼 URL；服务端 ReAct 循环自洽。
- **不适用 9 条**：`agentscope-service` / gateway / console / aistio / copilotkit / spring-boot starter / channel-dingtalk 在 Rust 均无对应 crate（controlplane 已于 `ea4b1cc` 整体删除，归 Go）；Redisson 类冲突在 Rust 无 classloader 机制。

### 6.5 累计新增需优先修复（第 6 轮）

1. **#3667** `exists` 全量扫 namespace 物化全部分值 —— 会话删除前的存在性检查即可打爆内存。
2. **#3644** Docker sandbox 确定性名 + 按 id liveness → 容器泄漏后永久 exit=125。
3. **#2962** 零 chunk 静默成功 —— 用户侧表现为「无回复、无错误」。
4. **#2829** 工具失败永不重试、超时直接跳出重试循环 —— `ExecutionConfig` 重试形同虚设。
5. **#1907 / #2070** AgentRun 数据面 `data` 信封未解包、MCP 启动不复用已建沙箱。
6. **#1790** 单技能仓库（无 `skills/` 子目录）加载为空。
7. **#2773 / #750** 协议服务任务状态只在内存，跨实例/重启后 HITL 与 running 标记全丢。

---

## 轮次 7：`core/model` 8 + `extensions` 9 + `ext/memory` 3 + `build` 4 + `examples` 4 + `ext/rag` 4 + `docs` 3 + `ext/spring-boot` 1

小计：**存在 7 / 部分存在 8 / 不存在 10 / 不适用 11**。

### 7.1 汇总

| Issue | 标题（简） | 分节 | 结论 | 关键 Rust 位置 |
| ---: | --- | --- | --- | --- |
| #2538 | AgentRun 销毁沙箱不备份到 OSS | core/model | 部分存在 | `extensions-sandbox/src/agentrun.rs:172-193,891-909` |
| #2512 | 并行 agent_spawn 时 AG-UI TEXT_MESSAGE 交错 | core/model | 不存在 | `extensions-agui/src/adapter/strategy/registry.rs:105` |
| #1944 | DashScope + DeepSeek 前缀缓存命中低 | core/model | 不存在 | `harness/.../workspace_context.rs:127-134` |
| #1568 | dashscope `'dict object' has no attribute 'name'` | core/model | 部分存在 | `extensions-model/src/dashscope/dto.rs:216-231` |
| #1309 | 流式工具参数 JSON 不完整仍执行 | core/model | 不存在 | `core/src/agent/react_agent.rs:355-396` |
| #1287 | Http2 偶发不通建议降级 | core/model | 不存在 | `Cargo.toml:101`（未启 http2） |
| #1284 | DashScope 丢失 ThinkingBlock reasoning_content | core/model | **存在** | `extensions-model/src/dashscope/converter.rs:439,832,945` |
| #1237 | 流式 ToolUseBlock name 为 null | core/model | 部分存在 | `extensions-model/src/openai/mod.rs:1541-1548` |
| #2405 | MCP 工具未 allow 不可用 / 前缀重命名 | docs | 部分存在 | `harness/src/agent/tools.rs:103,131-135` |
| #3310 | Cron 对非法时区静默回落 GMT | extensions | 不存在 | `extensions-scheduler/src/cron.rs:190-215` |
| #3248 | Mysql 技能库 SQL 标识符未加引号 | extensions | **存在** | `extensions-skills/src/mysql.rs:334-350` |
| #3213 | createIfNotExist=false 仍执行 alter table | extensions | **存在** | `extensions-postgresql/src/state.rs:84-92` |
| #3153 | aistio 工具名带点号打挂 OpenAI 模型 | extensions | 不适用 | controlplane 已删 |
| #3132 | HttpSelfRegistration.close 跳过注销 | extensions | 不适用 | 无 service crate |
| #2396 | 创建 k8s 沙箱报错无法新建工作目录 | extensions | 不存在 | `extensions-sandbox/src/workspace.rs:56` |
| #2382 | AgentRun sandbox（标题过短） | extensions | 部分存在 | 同 #2538 |
| #2365 | 默认镜像下 write_file/edit_file 报错 | extensions | 不存在 | `extensions-sandbox/src/fs_spec.rs:1693-1721` |
| #2165 | Redis Cluster `EVAL keys must in same slot` | extensions | **存在** | `extensions-redis/src/store.rs:290-330` |
| #1358 | 当轮压缩导致 LLM 输出内部摘要 | ext/memory | 不存在 | `harness/.../memory/compaction.rs:800-830` |
| #1288 | Mem0LongTermMemory | ext/memory | 不存在 | `extensions-mem` 无 Mem0 后端 |
| #1260 | AutoContextMemory 策略 cursor/index 错位 | ext/memory | 不存在 | 同 #1740 的压缩实现 |
| #2972 | Spring AI 2.0.1 依赖冲突 | build | 不适用 | 无 Spring |
| #2738 | frontend/build 被 gitignore | build | 不适用 | 无 frontend |
| #2340 | 子 agent 自行 build 会默认注入 general-purpose | build | 部分存在 | `harness/.../subagent/materialize.rs:61-72` |
| #1271 | victools 版本冲突 | build | 不适用 | 无 Spring |
| #1456 | a2a-spring-boot-starter 依赖冲突 | spring-boot | 不适用 | 无 starter |
| #1883 | 官方示例不能直接跑、文档质量差 | docs | 部分存在 | 仓库无 examples crate |
| #1725 | builder 无法 remote/sandbox 启动 | examples | 不适用 | 无 builder crate |
| #1723 | `workspace-store.fs-spec` 从未实现 | examples | 不适用 | 同上 |
| #1600 | dataagent 启动抛 non-noop snapshotSpec | examples | 不适用 | 无 dataagent |
| #1567 | Builder 新建 agent 的 workspace 路径不正确 | examples | 不适用 | 同上 |
| #3305 | TextChunker 产出空白 chunk | ext/rag | **存在** | `extensions-rag/src/chunker.rs:96-118` |
| #1516 | RAGFlow similarityThreshold 恒为 0.5 | ext/rag | **存在** | `extensions-rag/src/ragflow.rs:118-134` |
| #1095 | OpenAITextEmbedding 设计缺陷（批量/维度） | ext/rag | **存在** | `extensions-rag/src/embedding.rs:210-260` |
| #527 | 百炼知识库标签过滤异常 | ext/rag | 部分存在 | `extensions-rag/src/bailian.rs:180-205` |
| #933 | all-in-one 文档与实现不符 | docs | 不适用 | 无 all-in-one 文档站 |

### 7.2 存在的 7 条

- **#1284** DashScope 三处转换（simple / multimodal / multi-agent）都写着 `// Thinking blocks are never sent back to the model.` 并 `ContentBlock::Thinking(_) => {}`（`dashscope/converter.rs:439,832,945`）；`DashScopeMessage.reasoning_content` 字段定义了却**全仓无写入点** → Qwen3 thinking 模式多轮历史里 reasoning 被静默丢弃。对照 OpenAI 路径 `inject_reasoning_content` 会回传（`openai/mod.rs:1221-1245`）。
- **#3248** MySQL 技能库拼 SQL 时库名/表名/列名未加反引号（`extensions-skills/src/mysql.rs:334-350`），保留字库名直接语法错误；且标识符白名单与 `MysqlAgentStateStore` 不一致。
- **#3213** Postgres agent state store 在 `create_if_not_exist=false` 时仍执行 `ALTER TABLE`（`extensions-postgresql/src/state.rs:84-92`）→ 只读账号/受限环境启动即失败。
- **#2165** Redis store 的 Lua 脚本一次 `EVAL` 跨多个 key（`extensions-redis/src/store.rs:290-330`），未用 hash tag 统一 slot → Cluster 模式报 `EVAL command keys must in same slot`。
- **#3305** `TextChunker` 切分后未过滤空白 chunk（`extensions-rag/src/chunker.rs:96-118`）→ 空串进 embedding 流水线，白烧配额且污染向量库。
- **#1516** RAGFlow 检索请求把 `similarity_threshold` 写死 0.5（`extensions-rag/src/ragflow.rs:118-134`），配置项不生效。
- **#1095** `OpenAITextEmbedding` 无批量上限保护、无维度校验、失败整批丢弃（`extensions-rag/src/embedding.rs:210-260`）。

### 7.3 部分存在（8 条）要点

- **#2538 / #2382** create 时下发 `ossMountConfig.mountPoints`（`agentrun.rs:343-357`），但 `workspace_on_nas()` 仅按 `workspace_root.starts_with(mount_dir)` 本地前缀判断（`:172-193`），命中即令 `persist_workspace_bytes` 返回空（`:891-909`），`kill_sandbox` 只 DELETE（`:822-855`）→ 无销毁时 OSS 回写、也不校验挂载是否真生效；默认根 `/home/agentscope/workspace`（`:29`）与 issue 挂载目录非前缀关系时只落 tar。
- **#1568** `DashScopeMessage.name` 是 `Option` + `skip_serializing_if="is_none"`（`dto.rs:216-231`），tool 消息缺 name 时字段整体消失 → 复现同一 Jinja 400；`tool_role_multimodal_message` 兜底分支（`converter.rs:504-540`）显式构造无 name/无 tool_call_id 的 tool 消息。默认链会填（`react_agent.rs:4939,5082,5096`），仅外部注入/兜底/旧历史触发。
- **#1237** 无名分片得到空串名（`openai/mod.rs:1762,1799-1817`）或 `__fragment__` 占位（`dashscope/parser.rs:44-92,231-240`）→ `Tool not found` 错误结果；Java 是 null 静默丢弃，Rust 不丢历史但工具同样调不通。
- **#2405** `enableTools` 明确未实现（`tools.rs:131-135` 只 `warn!`）；`ToolFilter::apply` 在 build 期只作用于当时已注册工具，MCP 是异步 bridge（ACP 在 `session.rs:2064` build 之后）→ allow/deny 完全管不到 MCP 工具（症状与 Java 相反）；`prefix_tool_names` 格式是 `{server}__{tool}` 而非文档的 `mcp__{server}__{tool}`（`mcp.rs:296`）。
- **#2340** `general-purpose` 蓝图是内建默认注入，declared subagent 与它同层（`materialize.rs:61-72`）→ 用户「自行 build 的子 agent」仍会看到平台默认代理，无法关闭。
- **#1883** 仓库根无 `examples/` crate（只有 `agents/` 与 `docs/`），官方示例的可运行性问题在 Rust 侧表现为「没有可直接跑的示例」。
- **#527** 百炼知识库 tag 过滤拼进请求体但未做类型/空值归一（`bailian.rs:180-205`），多标签与空标签行为与文档不一致。

### 7.4 判「不存在 / 不适用」的要点

- **#2512 / #1944 / #1309 / #1287** AG-UI 对带 source 的子 agent 事件默认转 `subagent.*` CUSTOM，原生模式用 `reply_id` 派生消息 id 且支持多消息并发（无单活跃消息不变量）；workspace section 按 session 冻结（`frozen_section`）以保护前缀缓存；`finalize_tool_call_arguments` + executor 守卫把不完整参数标 malformed 并拒执行；workspace 固定 `reqwest` **未启 http2** feature，全程 HTTP/1.1。
- **#3310 / #2396 / #2365 / #1358 / #1288 / #1260** Cron 时区非法时直接报错不回落；`mkdir -p` 在 setup 脚本首行；沙箱命令无 `$(`/双引号（同 #1772）；压缩摘要只作为历史中的 assistant 消息、当前轮不重写；无 Mem0 后端、无 `AutoContextMemory` 策略机。
- **不适用 11 条**：Spring AI / victools / spring-boot starter 依赖冲突在 Rust 无对应物；`agentscope-service` frontend 与 all-in-one 文档站在 Rust 不存在；`agentscope-builder` / `agentscope-dataagent` 无 crate；aistio / HttpSelfRegistration 随 controlplane 删除。

---

## 全量结论（312 条）

> 覆盖 **311 / 312** 条（`#2789`/`#2790` 与 `#3612`/`#3567` 各为同题重复录入，合并计一条），分七轮完成。

| 结论 | 条数 | 占比 |
| --- | ---: | ---: |
| **存在** | 66 | 21.2% |
| **部分存在** | 85 | 27.3% |
| **不存在** | 120 | 38.6% |
| **不适用** | 40 | 12.9% |
| 合计 | 311 | 100% |

### 分布特征

- **命中率最高的三节**：`area/harness`（轮 3/4 共 70 条中 存在 21 + 部分存在 26）、`(unlabeled)`（大量迟到的最新缺陷）、`area/core/memory`（压缩/记忆生命周期几乎全中）。
- **最集中的失效主题（按根因数合并后的 Top 8）**：
  1. **默认文件系统为 `Unrestricted`**（#3258 / #3253 / #2065 / #1898 / #3378）—— 绝对路径直达宿主、根 ls 枚举驱动器、`execute` 可删任意目录。
  2. **`.agentscope/` 不在远程路由**（#2918 / #2091 / #2182 / #2691 / #2693 / #4140）—— 分布式部署下会话历史、index、bus 全部仍落本地。
  3. **`RuntimeContext::default()` 硬造**（#2266 / #1587 / #1775 / #1896-Bug2 / #2691）—— 命名空间退化为 `_default`/anonymous，多用户路由失效。
  4. **记忆收尾挂在回合尾**（#3059 / #2276 / #2225 / #1094 / #2821）—— `chain(once(tail))` + tail 内 LLM 调用，订阅者等 onComplete。
  5. **子智能体治理不继承**（#1578 / #1577 / #1700 / #2915 / #2790 / #2789 / #3178）—— middleware、取消、上下文、MCP 工具都没传下去。
  6. **skill 与工具治理接线缺失**（#1602 / #2441 / #2653 / #2405 / #2169 / #2664）—— allow 列表能删掉 skill 工具、DB 技能库只读、按需披露未接线、空集合不清理。
  7. **沙箱生命周期**（#3644 / #2979 / #2216 / #2845 / #2564 / #2329 / #2421 / #1632）—— 确定性容器名 + 按 id liveness 导致永久 exit=125、claim 每轮被删、纯聊天也建沙箱、进程内守卫是 noop。
  8. **Docker/沙箱命令 argv**（#2135 / #1560 / #1609 / #1772 / #2924）—— base64 塞进 `sh -c` 单参数，撞 32KB/MAX_ARG_STRLEN。

### 建议的修复批次

| 批次 | 内容 | 条目 |
| --- | --- | --- |
| **P0 安全** | 默认 fs 模式改 `Rooted`/`Sandboxed`，恢复危险命令拦截 | #3258 #3253 #2065 #1898 #3378 |
| **P1 分布式** | `.agentscope/` 路由 + 撤掉 `RuntimeContext::new()` + 状态落 store | #2918 #2091 #2182 #2691 #2693 #4140 #3667 |
| **P2 生命周期** | 记忆收尾 fire-and-forget、子 agent 继承治理、取消级联 | #3059 #2276 #1094 #1578 #1577 #1700 #2162 |
| **P3 工具治理** | allow 列表与 MCP/skill 注册时序、`.skills-cache` GC、前缀命名 | #1602 #2405 #2664 #2653 #2441 |
| **P4 沙箱** | 容器按名 reconcile、claim 保留、纯聊天不建沙箱、argv 走 stdin | #3644 #2979 #2564 #2329 #2135 #1560 |
| **P5 数据正确性** | 跨后端 move 保 `encoding`、Overlay grep 覆盖、flush 时区 | #3042 #2448 #3088 #3433 |

### 说明

- 全部结论均为**只读静态比对**（未编译、未运行、未修改任何 Rust 源码）；「存在」条目均给出具体文件与行号，可直接定位。
- Java 生态专属（Spring、Reactor、Jackson、JUnit、classloader、service/console、controlplane）统一判「不适用」，因 Rust 侧无对应机制或对应 crate 已删除。
- 每条 issue 的完整证据与修复建议见上文各轮次小节；同根条目已在对应位置标注「同根 #xxx」，修复时可按根因合并处理。







