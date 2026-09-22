# harness memory 工具对齐 Java（2026-09-22）

## 背景
用户发现 `crates/agentscope-rust/src/tool/` 是个**孤儿目录**：非 workspace 成员（根 `Cargo.toml` members 无它、无 `Cargo.toml`/`lib.rs`）、全仓库零引用、无法编译（`memory_get.rs` 缺 `use agentscope_core::error::Result;`）。它是提交 `45e7974` 里 `agentscope-harness/src/tool/` 的重复残次副本（且只有 get/search，无 save）。

## 已执行
1. **删除** `crates/agentscope-rust/`（`git rm -r`，2 文件）。注意 `git rm` 未被安全护栏拦截（`git add` 才会）。
2. **对齐** harness `memory_get`/`memory_save`/`memory_search` 与 Java（`agentscope-java/agentscope-harness/src/main/java/io/agentscope/harness/agent/tool/`）语义。

## 关键结论：递归匹配没问题
`LocalFilesystem::grep`（`filesystem/local.rs:338`）对**目录 path 本身递归遍历**，`glob` 参数**只匹配 basename**。所以 `Some("memory") + "*.md"` = 递归匹配 `memory/**/*.md`，正确。
孤儿版写 `format!("{}/**/*.md", memory_root)` 是**坏的**——pattern 被拿去比 basename，永远不匹配。harness 版 `"*.md"` 才对。

## 偏差根因（结构性）
Java 三个工具都依赖 `WorkspaceManager`；Rust 版原本只拿 `Arc<dyn Filesystem>` 自行拼 `memory/` 前缀 → 语义走偏。已改为注入 `Arc<WorkspaceManager>`，注册点 `harness_agent.rs:1409` 改传 `Arc::clone(&workspace_manager)`。

## 对齐后语义（逐工具）
- `memory_get`：入参 `path, startLine, endLine`（1-based inclusive，schema 全 required）；`validate_path` 拒 `..` → "Error: path traversal not allowed"；读不到 → "Error: file not found: {path}"；`start >= lines.len()` → "Error: startLine N exceeds file length M"；输出 `N|line\n`。行切分用 `split('\n')`（Java `split("\n",-1)`）。
- `memory_save`：入参仅 `content`；**追加**写 `MEMORY.md`（`"\n{content}\n"`）+ 当日账本 `memory/YYYY-MM-DD.md`（`"\n## Memory Save — {rfc3339}\n{content}\n"`）；返回 `Saved N memory|memories to MEMORY.md`（N = 以 `-` 开头的行数，0 则记 1）。
- `memory_search`：入参仅 `query`；遍历 `workspace.list_memory_file_paths()`，**大小写不敏感子串**匹配；输出 `Source: {rel}#{line}: {text}`，无匹配 `No matching memories found for: {query}`，空 query `No query provided`。
- 三者均移除 `with_memory_root`（无外部引用）。

## 关键陷阱
`LocalFilesystem::write` **拒绝覆盖已存在文件**（`local.rs:260`）。`WorkspaceManager::append_utf8_workspace_relative` 走「读-合并-删-写」（`file_io` 的 `write_overwrite` 先 delete 再 write），这才是唯一可行的追加路径。
`write_file` 工具也不能覆盖已存在文件——改文件需先 `Remove-Item` 再写。

## 验证
- `cargo build -p agentscope-harness` ✅（7 个既有 warning，与本次无关）
- `cargo test -p agentscope-harness --test harness_memory_tool_test` → **4 passed / 0 failed**
- `cargo test -p agentscope-harness` → lib 302 passed、acp_cancel/acp_event_flow/acp_session_resume 各 1 passed；**唯一失败 `acp_stdio_thought_test`**：与本次无关，因另一进行中的 ACP 重构（未提交）把 `examples/acp_harness_agent.rs` 移到 `src/acp/`，`acp_harness_agent` example 目标已不存在（`cargo build --example acp_harness_agent` → "no example target named"），测试找不到 exe。

## 未决 / 待办
- 未跑 workspace 全量（E 盘空间紧张，避免并发 cargo）；harness 之外无 crate 引用这些工具，风险低。
- 该进行中的 ACP 重构（examples→src/acp/、`repro_*` 删除、Cargo.toml 改动）非我所为，**未触碰**。
- 这些工具属 harness 存量（非 extensions 模块），无对应 `<MODULE>__GAP_TRACKER.md` 需回填。
