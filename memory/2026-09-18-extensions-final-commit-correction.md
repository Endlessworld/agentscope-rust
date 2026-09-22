# 2026-09-18 补充更正：core/harness 两处改动已提交

对 `2026-09-18-extensions-final-commit.md` 中「未提交的工作区改动（越界）」一节的更正：

- 该 2 处改动经用户说明系**其手动修改**，已一并提交为 **`1126549`**：`调整危险命令清单与 ACP 示例迭代上限`（2 files changed, 1 insertion(+), 3 deletions(-)）。
  - `crates/agentscope-core/src/tool/dangerous.rs`：`DANGEROUS_COMMANDS` 删除 `"format"`、`"kill -9"`。
  - `../crates/agentscope-harness/src/acp/acp_harness_agent.rs`：`.max_iters(50)` → `.max_iters(9999)`。
- 提交后 `git status --porcelain` 为空，工作区干净。extensions 收尾波次至此完全收口。
- 交付提交链：`b4c91ad`（20 模块迁移）→ `178867a`（收尾波次 86 files）→ `1126549`（手动调整 2 files）。

## 教训
- 工作区中非本会话产生的改动**未必是违规产物**，可能正是用户手动编辑。**报告并询问**，不要自行判定为「越界」而暂缓/还原。
- `memory_save` 无法覆盖已存在的记忆文件（报 `already exists`）→ 需另建新路径或用其它方式追加。
