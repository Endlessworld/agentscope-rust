# __NAME__ 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **__NAME__** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\__JAVA__\`
> **Java 子模块**：__SUB__
> **Rust 目标**：`crates/__CRATE__/`（波次 __WAVE__）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。
- **本文件只写现行状态（§0–§5）**：执行过程记录（回执 / 静态复核 / 勘误 / 反向验证明细）写入 `docs/extensions/history/__NAME__MIGRATION_HISTORY.md`，本文件只在 §6 留一行相对路径链接。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `__JAVA__` |
| Java 子模块 | __SUB__ |
| Java 规模（初测） | 主代码 ~__MAIN__ 行 / 测试 ~__TEST__ 行 |
| Rust crate | `crates/__CRATE__` |
| 迁移波次 | __WAVE__ |
| 当前状态 | 待办 |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| _（调研后填写）_ | | | |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| _（调研后填写）_ | | | |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| _（调研后填写）_ | | 待办 | |

## 4. 项目状态与下一步

### 4.1 当前状态
- _（填写：已完成内容 / 验证命令与结果 / 已知问题）_
### 4.2 下一步与阻塞
1. _（填写）_


### 4.3 与 Java 的已知差异（有意为之，非缺口）

#### 4.3.1 设计决定（不迁移）
- _ (填写语言/架构等差异无法迁移的内容)
#### 4.3.2 设计决定与已知差异
- _ ( 与 Java 的已知差异（有意为之，非缺口）)
## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p __CRATE__` 基线。
- 改动中：保持 `cargo build -p __CRATE__` / `cargo test -p __CRATE__` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- 完工后（归档）：执行记录写入 `docs/extensions/history/__NAME__MIGRATION_HISTORY.md`，**不要**在本文件追加历史章节；跨文件引用一律用相对路径链接，不复制正文。
- **仅在本 crate 内改动**；新增依赖写入 `crates/__CRATE__/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。
