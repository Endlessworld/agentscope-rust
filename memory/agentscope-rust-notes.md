# agentscope-rust 环境与约定（长期）

## 路径（易变，务必先确认）
- Rust 工作区：`D:\IdeaProjects\agentscope-rust`（**项目盘为 D:，不是 E:**；E: 盘已不存在）
- Java 上游源码：`D:\IdeaProjects\agentscope-java\agentscope-extensions\...`（**不再是 `E:\local-github`**）
- 索引文件：`docs/extensions/EXTENSIONS_GAP_TRACKER.md`（汇总）+ `docs/extensions/<MODULE>__GAP_TRACKER.md`（各模块单一事实来源）

## 构建/测试注意事项（Windows + PowerShell）
- **不要用 PowerShell 管道直接接 `cargo test --workspace`**：巨量输出会触发 .NET `Stack overflow`。先 `Out-File` 到 `$env:TEMP\xx.log` 再 `Select-String` 分析。
- `os error 1455`（页面文件太小 / rlib mmap 失败）与 `os error 112`（磁盘满）都会伪装成「could not compile」假阳性。前者降并行度可解：`cargo test --workspace -j 2`。
- `CARGO_INCREMENTAL=0` 可避免 `target/debug/incremental` 膨胀。
- **既有偶发失败（非代码缺陷）**：`cargo test --workspace` 默认并行度下偶发 3 个 `agentscope-extensions-training::runner` 失败 —— 该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`，属测试间竞争；单独跑 `-p agentscope-extensions-training` 稳定 64 passed。

## harness 命名陷阱（重要）
- `agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 里的**遗留事件通道 struct**，与路由 trait **同名**。
- 路由 trait 必须用全路径导入：`agentscope_harness::agent::gateway::channel::Channel`。
- `Channel` 与 `ChannelSink` **都有 `channel_id()`**；两者同时在作用域时须限定，如 `Channel::channel_id(&x)`，否则 `E0034`。

## 公共层裁定（已落地）
- harness `agent/gateway/platform_channel.rs`：`PlatformChannel` trait + `PlatformCtx`，并为 `Channel`/`ChannelSink` 提供 **blanket impl** → 平台适配器只需实现 `ctx()` + `deliver_reply()` 两个方法。
- HTTP 框架统一 **axum 0.7**（protocol / aistio / channel 一致）。
- Channel 注册表：**不**按 Java 为每平台写单例，统一用泛型 `common::ChannelRegistry<C>`（`pub type FeishuChannelRegistry = ChannelRegistry<FeishuChannel>`），可选镜像到 harness `ChannelManager`。
- 存储模块：**不**新建公共 `agentscope-extensions-store` crate，各 crate 自建 `XxxDistributedStore`。

## 效率经验
- 多平台同构改造用并行子智能体（每个只改自己平台目录 + 禁止改 `lib.rs`/`Cargo.toml`），主会话先写**一个参考样板**（本次是 Feishu）再派发，效果好；子智能体需被告知「其它目录的编译错误可忽略」。

## EXTENSIONS 迁移状态（2026-09-18 第七批验收）
- `cargo build --workspace` → BUILD_EXIT=0；全量 **1846 passed / 0 failed**（第六批 1746）。
- CHANNEL：227 test。5 个平台 `*Channel` + 4 个 axum CallbackController 已落地；**仅剩 `DingTalkStreamClient`（常驻 WebSocket）未迁移**（`tokio-tungstenite` 依赖与 `handle_stream_payload` 入站口已就绪）。
- 仍待裁定的汇总项：harness `SandboxExecutionGuard` 为具体 struct（JDBC 锁无法驱动沙箱隔离）；Skills↔harness `skill/` 职责边界。
