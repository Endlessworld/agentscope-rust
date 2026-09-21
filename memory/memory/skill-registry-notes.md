# skill 系统事实（2026-09-20 实测，agentscope-rust 工作区）

## ⚠️ load_skill_through_path 在本会话不可用（注册表为空）
三处存储彼此**不连通**：
1. **会话提示中的 `<available_skills>`**：列 17 个 —— apple / bash / codegraph / executing-plans / find-skills / frontend-design / fsx-cli / github / groovy-plugin-development / ppt-editing-skill / prd / tauri-debugger / tauri2-react-rust / ui-design-pro / using-git-worktrees / writing-plans / writing-skills，每项带 `<files-root>`。
2. **`load_skill_through_path` 的解析器：注册表为空。** 报错形如 `Unknown skillId 'X'. Available skills:`（列表为空串）。
   - 实测 `codegraph_user`/`apple_user`/`prd_user`/`find-skills_user`/`using-git-worktrees_user`/`writing-plans_user`/`executing-plans_user`/`writing-skills_user`，以及**去 `_user` 后缀**的 `codegraph`/`apple`/`prd` —— 全部 `Unknown skillId`。
   - 结论：**该工具只能「加载」已注册 skill，本身不是注册工具**；注册表为空 → 无法加载任何 skill，更无法用它「注册所有 skill」。
3. **`skill_manage` 注册表**：`.agentscope\workspace\skills\`。
   - `action=list` → `drafts: [codegraph]`、`main:`（空）。
   - `.audit_log.jsonl`：`codegraph` 晋升被**拒 2 次**，理由 `name 'codegraph' not in allow-list`。
   - `.usage.json`：codegraph `state: draft`、`created_by: agent`、`environments: []`。

## 磁盘真身（17 个均在，frontmatter 完好）
- 路径：`.agentscope\workspace\.skills-cache\user_N\<name>\SKILL.md`（user_1..user_17）。
- 附资源示例：`user_14\ui-design-pro\README.md`、`user_16\writing-plans\plan-document-reviewer-prompt.md`、`user_17\writing-skills\{anthropic-best-practices.md, persuasion-principles.md, testing-skills-with-subagents.md, render-graphs.js, graphviz-conventions.dot, examples\}`。
- 注：PowerShell 控制台输出中文会花屏（mojibake），但 `read_file` 读同一文件中文正常 —— 控制台编码问题，非文件损坏。

## 结论与绕过
- 要让 `load_skill_through_path` 工作，需修平台侧注册表 / allow-list（`skill_manage` 的 promote 通道），**非本会话工具所能完成**。
- 绕过：直接 `read_file` 各 `SKILL.md`（内容等效进入上下文）。
- 附带发现（不重复删除）：`.agentscope\workspace\skills\_drafts\codegraph\SKILL.md` 已存在；工作区另有 `gen_ext.ps1`/`gen_ext2.ps1`/`gen_ext3.ps1`/`ws_test.log`。
- ⚠️ memory 写入注意：`memory_save` **不能覆盖已存在文件**（返回 "already exists"），且对同一逻辑文件用 `memory/tooling-notes.md` 与 `tooling-notes.md` 会落到两个不同实际路径（`memory/memory/tooling-notes.md` 与 `memory/tooling-notes.md`）。更新记忆请写新路径。
