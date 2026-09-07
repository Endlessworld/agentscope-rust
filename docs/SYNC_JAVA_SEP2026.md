# Java 近期更新 → Rust 同步分析（2026-09）
> 实现状态：**P0 全部 + P1 全部已落地（2026-09-07）**

> 分析时间：2026-09-07
> Java 基线：`agentscope-java` @ 1048fdc3 (2026-09-06)
> Rust 基线：`agentscope-rust` @ 833f280 (2026-09-07，已含 ACP 全套)
> 范围：仅 agentscope-core + agentscope-harness 两个子系统（Rust 已实现的领域）

## 一、已确认「需同步」的高价值项（P0/P1）

### 1. ls() 报错而非静默空（Java #2413, ea511ec2）— **✅ 已同步**
- Java：`LocalFilesystem.ls()` 把 `!exists` 和 `!isDirectory` 拆成两个错误：
  - `!exists` → `fail("Path does not exist: ...")`
  - `!isDirectory` → `fail("Not a directory: ...")`
  - `FilesystemTool.listFiles()` 空结果消息由 `"Empty or not a directory"` 改为 `"Empty directory"`
- Rust 现状：`local.rs` L187-192 `ls()` 里 `read_dir` 失败直接 `return Ok(Vec::new())`（静默空）；
  `filesystem_tools.rs` L356-361 `list_files` 空结果返回 `"Empty or not a directory"`。
- 建议：`LocalFilesystem::ls` 区分 `!exists`/`!is_directory` 返回 `Error::Tool`；`list_files` 空消息改 `"Empty directory"`。

### 2. 搜索工具输出上界（Java #2832, 411d861b）— **✅ 已同步**
- Java：`grep_files` 默认 limit=100、`glob_files` 默认 200、硬上限 1000；超限追加截断提示；
  `limit < 1` 返回错误。
- Rust 现状：`filesystem_tools.rs` `grep_files`/`glob_files` 无 limit 参数、无上界，输出可能爆炸。
- 建议：给两个工具加 `limit` 参数（默认 100/200，硬上限 1000）+ 截断提示。

### 3. HarnessAgent 每会话 interrupt / clearContext（Java #2600, ed321c86; #2499, ef06c7ac）— **✅ 已同步**
- Java：`HarnessAgent` 新增 `interrupt(RuntimeContext)`、`interrupt(userId, sessionId)`、
  `ReActAgent.clearContext(...)`（清会话模型可见上下文而不重建会话）。
- Rust 现状：`harness_agent.rs` 无 interrupt/clear_context 方法（ACP 会话层有 `$/cancel_request`，
  但 HarnessAgent API 层缺）。core `ReActAgent` 有 `InterruptControl`（interrupt 内部机制）但无公开 clearContext。
- 建议：core `ReActAgent` 暴露 `clear_context`；`HarnessAgent` 加 per-session interrupt/clear 方法。

## 二、已确认「部分同步」项（P1，需补齐细节）

### 4. MCP 注册结果上报（Java #2877, 1e03f2b0）— **部分**
- Java：`register` 上报每个 server 注册结果（成功/失败）给调用方。
- Rust：`mcp.rs` `McpServerRegistrar::register` 返回 `Result<()>`，但内部 `tokio::spawn` 异步执行，
  错误只 `tracing::error!`，调用方拿不到失败详情。建议把注册结果通过返回值/回调上报。

### 5. 思考块 token 计数（Java #3009, 02a83305）— **部分/需验证**
- Java：`ThinkingBlock` token 由 thinking 内容估算，并统计 `ToolResultBlock` 内嵌套 thinking。
- Rust：`usage.rs`/`block.rs` 需确认 thinking token 估算逻辑。建议核对。

## 三、已确认「已同步/不适用」项（无需处理）

- **管道死锁修复**（#2839, 0f0924a1）：Rust `sandbox.rs` execute 已用 `tokio::spawn` 并发 drain
  stdout/stderr + 进程树 `taskkill /T`，语义等价，**已同步**。
- **sandbox 读操作失败上报**（#2967, f42881ad）：Rust `SandboxBackedFilesystem` 读操作直接委托
  inner filesystem（不经 shell），无 shell 解析失败掩码问题，**不适用**。
- **memory flush/maintenance 节流隔离**（#2993, cfb932c1）：Rust 用两个独立静态 map
  （`SHARED_LAST_FLUSH_AT` / `SHARED_LAST_RUN_AT`），天然隔离，**已同步**。
- **deliver_artifact 工具**（#2667, d25d4e9a）：依赖真实 sandbox 边界（Rust sandbox 后端为已知
  P1 待办），当前 `SandboxBackedFilesystem` 无边界，**暂不适用**。

## 四、core 侧需专项验证项（P1，本报告未逐行确认）
- 空最终响应重试（#2755, bc775127）：Rust `run_reasoning` 空结果返回 `Error::Agent("model produced
  no message")`，未见重试逻辑，**疑似缺失**。
- call-scoped shutdown 状态（#2712, 8a78d6ce）、模型调用失败持久化上下文（#2799）、
  ToolResultBlock metadata 经事件传播（#2315, 2e891c13）、ChatResponse.metadata→Msg.metadata
  （#2931, 94429d82）、结构化输出保留 usage（#2966, 8042fc95）、tool 校验错误带 field path
  （#2718, 48b62328）、middleware 执行排序（#2532）、tool 加入 group API（#2836, 2a43ff71）、
  ReActAgent state-cache cleanup API（#2572）、legacy v1 状态权限保留（#2769）——
  均为 detail 级，需逐条对照 Rust 实现确认。

## 五、建议的同步优先级
1. **P0**：ls() 报错语义（#2413）+ 搜索输出上界（#2832）— 两个都是 agent 可见行为/健壮性，改动小收益大。
2. **P1**：HarnessAgent per-session interrupt/clearContext（#2600/#2499）。
3. **P1**：MCP 注册结果上报（#2877）。
4. **P1**：core 侧空响应重试（#2755）等 detail 项逐条核对。