# 第八批验收与 CHANNEL 收官（2026-09-19）

承接 `agentscope-rust-notes.md`（路径/环境/陷阱部分仍有效，本文件只记增量）。

## 本轮结论：CHANNEL 模块「已完成」
- `cargo build --workspace` → BUILD_EXIT=0
- `cargo test --workspace -j 2 --no-fail-fast` → **1860 passed / 0 failed**（86 targets）
- 批次数字：第五批 1728 → 第六批 1746 → 第七批 1846 → **第八批 1860**（+14 全来自 `dingtalk/stream.rs`）
- `channel` crate：149 → 227 → **241 test**；`harness` 299；`core` 87

### 落地的最后一块拼图
`crates/agentscope-extensions-channel/src/dingtalk/stream.rs`（739 行，14 个单测）
- `DingTalkStreamClient`：常驻 WebSocket 网关 = 注册 → 连接 → ACK → 分发 → 指数退避重连（1s→60s，连上复位）；`start`/`stop` 幂等（对应 Java `AtomicBoolean`）
- 两个 I/O 边界为 trait：`HttpTransport`（注册调用）+ `WebSocketConnector`/`StreamSocket`（socket），测试用脚本化 mock 离线驱动；生产连接器 `TungsteniteConnector`（`tokio-tungstenite` 0.29，features `connect`/`handshake`/`rustls-tls-webpki-roots`）
- ACK **在分发前**发出（对齐 Java，避免重试缓冲重复投递）；可见回复仍走 `DingTalkOutboundClient`

## 提交
`6024739`（main）`feat(channel): 五平台 Channel 适配 + axum Webhook Controller + DingTalk Stream 客户端`
30 files, +4107/-152。父提交 `57ca1ae`。

## 教训（重复出现，务必警惕）
**「文件已落盘但从未接线」已发生两次**：
1. `common/channel_core.rs` / `channel_registry.rs` —— 未在 `mod.rs` 声明，从未编译
2. `dingtalk/stream.rs` —— 未在 `dingtalk/mod.rs` 声明，从未编译（子智能体写完就返回）

→ 复核子智能体/上一轮产物时，**先 `grep mod.rs` 确认模块已声明**，再看 `git status` 是否 `??`，最后才跑测试（未声明的文件不会产生任何编译/测试痕迹，容易假判为「已交付」）。

## 未决 / 跨模块
- harness `SandboxExecutionGuard` 为具体 struct（JDBC 锁无法驱动沙箱隔离）
- Skills ↔ harness `skill/` 职责边界未裁定
- MONGODB `list_hash` 算法并存（crate 本地 SHA-256 vs `agentscope_core::state::list_hash`）
- 既有 warning（非本轮引入、文件未改动）：`channel/src/{feishu,wecom}/token.rs` 的 `StaticTokenProvider` 未使用导入
