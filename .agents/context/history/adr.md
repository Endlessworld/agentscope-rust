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
