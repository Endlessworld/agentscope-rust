# load_skill_through_path 修复记录（2026-09-20）

## ✅ 根因已修复（harness 源码 + 3 个回归测试，302 passed / 0 failed）
**根因**：`SkillRepository::skill_dir()` 是 `load_skill_through_path` 的索引源
（`HarnessSkillMiddleware::load_index()` 依赖 `repo.skill_dir(&skill.name())`），但
`FileSystemSkillRepository` 与 `WorkspaceSkillRepository` 的 `impl SkillRepository`
遗漏了 `skill_dir` 转发（只转发了 repo_id/source/is_writeable/repo_kind/load/
write_skill/delete_skill）→ `load_index()` 全部为 None → 索引空图 →
工具报 `Unknown skillId 'X'. Available skills: `（空列表）。

**为何 `<available_skills>` 渲染正常**：提示块走 `stager::stage` + `ShellPathPolicy::resolve`
（filesRoot）路径，不依赖 `skill_dir`；两条路径在 `.skills-cache` 各司其职。

**改动文件**：
- `crates\agentscope-harness\src\agent\skill\repository.rs`：两个 impl 各补
  `fn skill_dir(&self, name) { self.inner.skill_dir(name) }`。
- 新增测试：`repository.rs`（file_repo_skill_dir_resolves_subdir /
  workspace_repo_skill_dir_resolves_subdir）、`middleware\skill_middleware.rs`
  `load_index_resolves_dirs_from_repos`（FileSystemSkillRepository → load_index()
  断言 `{name}_{source}` 命中且 SKILL.md 在盘）。
- 验证：`cargo test -p agentscope-harness --lib` = **302 passed / 0 failed**。

## ⚠️ 生效条件：需重启宿主进程
工具是 `LazyLoadSkillTool`（首次调用才 `load_index()`，内部缓存）。当前会话宿主二进制
为修复前编译，会话内再次调用仍报 Unknown skillId；重新编译并重启 ACP 宿主后即正常。

## 既有知识（不覆盖）
- `memory/memory/skill-registry-notes.md`：三处存储分离（提示块/工具索引/skill_manage 注册表）、
  磁盘真身、控制台 mojibake、memory_save 不能覆盖已存在文件。