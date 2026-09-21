# codegraph CLI 实测笔记（2026-09-20，项目 E:\local-github\agentscope-rust）

索引规模：634 files / 17,658 nodes / 62,968 edges（rust 630, xml 3, js 1）。`codegraph status -p <path>` 查状态与统计。

## 命令分层（组合用法）
- 定位：`query <name> [-k kind] [-l n]`；`files --filter <dir>` / `--pattern <glob> [-j]`
- 上下文：`node <symbol>`（源+调用轨迹）；`node -f <file> [--symbols-only | --offset N --limit N]`（文件模式=读取+符号表+依赖方，一次调用替代 Read）
- 面探索：`explore <关键词...> --max-files N`（符号源+调用路径+blast radius，含「no covering tests found」提示）
- 关系：`callers <sym>` / `callees <sym>` / `impact <sym> -d N` / `affected <file> -q`（受影响测试文件）
- 维护：`status` / `sync` / `index` / `unlock`

## 已实测的坑
1. `callers`/`callees` 只接受**纯符号名**；传 `path:line` → `Symbol "..." not found`。同名符号按名聚合产生噪声，用 `模块::符号`（如 `coordination::try_claim`）可收敛。
2. `callees` 结果可能混入无关同名符号（如 `get`）甚至符号自身。
3. `files` 无 `-l` 选项（报 unknown option）；有 `--filter/--pattern/--format/--max-depth/--no-metadata/-j`。
4. ⚠️ **环境坑：本机 shell 安全护栏会拦截含 `--format <value>` 的命令**（`tree`/`flat`/`grouped` 均被拒，报 "dangerous fragment"）；省略 `--format` 或改用 `-j` 正常。此拦截与 `git add` 拦截同类。
5. `explore` 输出即「已读源文件」，含 dynamic-dispatch 合成边（interface→impl）。
6. 未定义符号返回 `[i] Symbol "..." not found`，退出码 0（非错误）。

## SKILL.md 强化（2026-09-20）
- 路径：`C:\Users\Administrator\.agents\skills\codegraph\SKILL.md`（4524 → 10629 字节）。记忆根目录实为 `E:\local-github\agentscope-rust\memory\`（`memory_save` 只能新建、不能覆盖同名文件）。
- 动机：模型默认不走该技能。**frontmatter 的 `description` 是技能选择阶段唯一可见内容**，故首句加「【默认必用·优先级高于 grep/glob/Read】」+ 触发场景枚举（符号在哪/定义/谁调用/影响面/测试范围）。
- 正文新增：⛔ 强制触发规则（MUST 对照表：禁止 grep → 必须 codegraph）、自检三问、分层组合法 L0–L4、标准工作流四步、输出解读三信号（blast radius / dynamic 边 / Trail）、实测避坑 6 条。
- 唯一允许回退 rg/read_file 的情形：非代码文本（JSON/Markdown/配置/字符串）或 codegraph 返回 not found。原则：**codegraph 定代码结构，rg 定文本字面量。**
