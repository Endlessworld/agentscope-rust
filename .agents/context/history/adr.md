# 架构决策记录（ADR）

## ADR-001 Docker 沙箱后端：走 `docker` CLI，不引 `bollard`（2026-09-17）

**背景**：迁移 Tracker B1 原计划「`sandbox/docker.rs` 实现 `Sandbox`+`SandboxClient`；`Cargo.toml` 加 `bollard`」。

**决策**：改用 `docker` CLI（`tokio::process::Command`）实现 Docker 后端，**不引入 `bollard`**。

**理由**：
1. Java 参考实现（`DockerSandbox`）本身就是 CLI 驱动（`ProcessBuilder` + `docker run/exec/inspect/stop/rm/cp`），逐行对照迁移成本最低、语义可验证。
2. 不增加 daemon API 依赖与版本升级负担（bollard 需 daemon 版本假设、类型层映射）。
3. 与既有 harness 子系统一致：`filesystem/sandbox.rs` 已用进程树 + 双流 drain + `taskkill /T` 模式，可直接复用（`kill_process_tree` 提升为 `pub(crate)`）。

**代价 / 已知限制**：
- 依赖宿主 `PATH` 中有 `docker` CLI（Java 同样）。
- 快照（B2）与 layout entries（B3）仍待接；`start` 只实现 Java 4 分支中的 A/B/D，分支 C（从 snapshot 恢复）待 snapshot 落地。
- `WorkspaceSpec` 的 `copy_in` 尚未实现宿主→容器传输（记 warning 跳过）。

**替代方案**：`bollard`（daemon API）。放弃原因：与 Java 参考偏差大、迁移成本高。

## ADR-002 AgentSpawnTool 由 HarnessAgent.build() 内部接线，而非 example 注册（2026-09-07）

**背景**：`agent_spawn`/`agent_send`/`agent_list` 在 Rust 侧早已实现，但 `HarnessAgent::build()` 从未
创建 `DefaultAgentManager` 与 `AgentSpawnTool`，也未把 `factory_builder` / `agent_manager` 传给
`DynamicSubagentsMiddleware`，导致 example（含 `acp_harness_agent`）拿不到子 agent 能力。

**决策**：接线点放在 `HarnessAgentBuilder::build()` 内部（对齐 Java `HarnessAgent.build()` +
`SubagentsMiddleware.getTools()`），example 不手动注册工具。

**理由**：
1. Java 参考实现即如此——`SubagentsMiddleware` 构造时自建 `DefaultAgentManager` + `AgentSpawnTool`，
   `getTools()` 统一吐出，`HarnessAgent` 只负责把它灌进 toolkit。
2. example 手动注册会在每个使用方重复一遍 manager/entries/factory 接线，且无法与 middleware 共享同一实例
   （Java 注释明确：toolkit 在 orchestration 时绑定的是 middleware 持有的那个实例）。
3. `general-purpose` 无条件注册是 Java 语义，所有使用方零配置即可获得可原子任务委派能力。

**代价 / 已知限制**：subagent 段必须从「工具注册之前」搬到「全部工具注册之后」，否则子 agent 继承的是空 toolkit；
本轮未转发 `decl.model`、skills 白名单、父 middleware 与 plan-mode。

**替代方案**：在 `acp_harness_agent.rs` 中显式 `toolkit.register_tool(...)` + `subagent_entries(...)`。
放弃原因：偏离 Java、example 污染、每个使用方重复接线。
