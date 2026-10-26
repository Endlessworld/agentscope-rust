# CHANNEL 模块迁移 — 历史记录归档（冻结）

> **状态：冻结，不再更新。** 本文件由 `docs/extensions/CHANNEL__GAP_TRACKER.md` 归档而来，
> 收录原 §4（项目状态与下一步）、原 §6（2026-10-22 静态复核快照）、原 §7（P0 修复记录 G1）、
> 原 §8（P1/P2 修复记录 G2–G8）。章节编号保持原样，以便旧文档中的 `§7.x` / `§8.x` 引用继续可解析。
>
> - 归档时间：2026-10-23（CHANNEL 全部 P0/P1/P2 缺口关闭时）。
> - 归档原因：主 tracker 转为「基线 + 增量跟踪」形态，历史过程记录不再污染日常工作区。
> - 当前基线 / 逐类映射 / 增量流程：见主 tracker `CHANNEL__GAP_TRACKER.md`。

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：
  - common 主干：`BotLoopGuard`、`IdempotencyStore`、`ChannelCore`（组合 harness `PlatformCtx`）、`ChannelRegistry<C>` 及公共件 `HttpTransport`/`AccessTokenProvider`/`TokenCache`/`crypto`/XML 提取/地址解析/配置取值。
  - 七平台的 `*ChannelProperties`、`*InboundMapper`（**仅 text**）、`*OutboundClient`、`*AccessTokenProvider`、签名校验（GitHub HMAC-SHA256 / GitLab token / WeCom SHA-1 / Feishu Lark）；Feishu / WeCom 的 AES-256-CBC 解密。
  - **六个平台的 `*Channel` 适配**（Feishu / DingTalk / WeCom / GitHub / GitLab / **Weixin**），均实现 harness `PlatformChannel`。
  - **五个 axum CallbackController 薄壳**（Feishu / WeCom / GitHub / GitLab / DingTalk）。
  - **`DingTalkStreamClient`**（`dingtalk/stream.rs`）：常驻 WebSocket 网关，含注册/ACK/退避重连/幂等启停；I/O 走 trait seam，可离线单测。
  - **入站媒体契约**（`common/media.rs`）+ DingTalk/Feishu/WeCom 三平台 mapper 的媒体映射。
  - **可插拔 seam**（`InboundEventDeduplicator` / `AccessTokenStore`）与 `ChannelSeams` 工厂注入。
  - **Feishu 普通事件 `header.token` 恒定时间鉴权闸门**；`BotLoopGuard` 空闲 peer 淘汰；`IdempotencyStore` TTL 窗口重启语义。
- **验证命令与结果**（**2026-10-23 实测**）：
  - `cargo build -p agentscope-extensions-channel` → Finished，0 error（2026-09-18 历史记录）
  - `cargo check -p agentscope-extensions-channel --all-targets` → 通过（0 error / 本 crate 0 warning）
  - `cargo test -p agentscope-extensions-channel` → **385 passed / 0 failed**（源码内联 `#[test]`+`#[tokio::test]` 实测计数 = 385）
  - `cargo build --workspace` → **BUILD_EXIT=0**（历史记录）
  - 全量 `cargo test --workspace -j 2` → **1846 passed / 0 failed**（历史记录，86+ test target）

#### 4.1.1 本轮新增（会话续作）
> 行数为 **2026-10-22 静态复核实测**（`Get-Content | Measure`），旧版记录中 `dingtalk/stream.rs` 记 739、各 channel/controller 行数普遍偏低约 5–10%，已按实测修正。

| 文件 | 行数 | 内容 |
|---|---|---|
| `common/channel_core.rs` | 314 | 接线并**收敛**：由自存 `channel_id`/`config`/`router`/`gateway` 改为组合 harness `PlatformCtx`；`admit(dedupe_key, peer_key)` / `is_first_seen` / `admitted()` |
| `common/channel_registry.rs` | 238 | 接线；`Channel`/`ChannelSink` 同名方法歧义修为 `Channel::channel_id(&*c)` |
| `feishu/channel.rs` | 370 | `FeishuChannel` + `handle_callback`（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `feishu/controller.rs` | 136 | axum 薄壳 `POST /api/channels/feishu/:channel_id/callback` |
| `dingtalk/channel.rs` | 273 | `DingTalkChannel` + `handle_stream_payload` |
| `wecom/channel.rs` | 502 | `WeComChannel` + `verify_url` / `handle_callback` |
| `wecom/controller.rs` | 284 | axum 薄壳（`GET` URL 验证 + `POST` 事件） |
| `github/channel.rs` | 368 | `GitHubChannel` + `handle_webhook`（HMAC 校验 + 防自回环） |
| `github/controller.rs` | 136 | axum 薄壳 `POST /api/channels/github/:channel_id/webhook` |
| `gitlab/channel.rs` | 375 | `GitLabChannel` + `handle_webhook`（token 校验 + 防自回环） |
| `gitlab/controller.rs` | 200 | axum 薄壳 `POST /api/channels/gitlab/:channel_id/webhook` |
| `dingtalk/stream.rs` | 822 | `DingTalkStreamClient`（常驻 WebSocket 网关）+ `DingTalkStreamProtocol` + 连接器 seam + **14** 个单测 |

#### 4.1.2 跨模块改动（本轮经汇总方授权）
- **harness 新增 `agent/gateway/platform_channel.rs`**（`PlatformChannel` / `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl，6 个单测）。属「公共层写 harness」裁定的执行（见汇总 tracker §3.3）。
- **根 `Cargo.toml`**：`axum`/`http`/`bytes` 及真实后端驱动依赖已入 `[workspace.dependencies]`，与本 crate 引用对齐。

#### 4.1.3 前轮修复的真实缺陷（3 处，保留备查）
1. `common/xml.rs::xml_element_text` **实现 bug**：`quick-xml` 把 `&amp;` 从 `Event::Text` 拆出为 `Event::GeneralRef`，原实现对 `GeneralRef` 落到 `_ => {}` 而**静默丢弃实体**（`"hello & goodbye"` 被解成 `"hello  goodbye"`，同时影响 WeCom inbound）。修法：新增 `Event::GeneralRef` 分支（`#xHH`/`#NN`/预定义实体），未声明实体按 XML 解析失败返回 `None`，对齐 Java `DocumentBuilderFactory` 异常路径。
2. `feishu/outbound.rs` 测试 `send_executes_one_request_per_message` **测试缺陷**：未播种 `code:0` 响应，而 `parse_send_response` 正确要求 `code` 字段（对齐 Java `checkSendResponse`）。修法：为两次请求 seeding 合法响应。
3. `common/idempotency_store.rs` 测试 `capacity_is_bounded_by_evicting_oldest` **期望与 Java 语义冲突**：`order` 是**惰性重建**的（仅在 `sweep` 内按 `entry.seq` 重建），而 `first_seen_at("a", 4)` 本身会先跑 `sweep`（对齐 Java `sweep(now)` 在 `putIfAbsent` 之前），此时最老者已变成 `"b"` 而被淘汰。修法：断言改为 `first_seen_at("b", 4) == true` 并补 `size() == 3`。
- **已知问题**：无已知编译失败。全量 `cargo test --workspace` 曾在默认并行度下偶发 3 个 `agentscope-extensions-training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`，属**既有的测试间竞争**，与本轮改动无关；单独跑与 `-j 2` 均 3/3 全绿）。另：全量测试需保证页面文件充足，否则会因 `os error 1455/112`（mmap 失败）产生「could not compile」假阳性。

### 4.1.4 GAP-19：lease 丢失可中止在跑的一轮（2026-10-21，汇总 tracker 关闭 GAP-19）
- **裁定**：用户「(c) 为主 + (a) 兜底」——先协作式中止、宽限期内优雅收尾，超时再硬中止。
- **Java 依据**：`WeixinChannel.java:200` 的 `takeUntilOther(session.cancelled.asMono())`（取消订阅 = 中断在飞的那一轮）、`:435-438` `loseLease` 广播 `cancelled`、`:65` 的 `weixin-lease-renewal` 续租线程（续租失败 → 丢租约 → 广播）。Rust 无调度器，续租改为 spawn 任务。
- **本 crate 改动**：
  - `weixin/channel.rs`：`LeaseSession` 持有 `agentscope_core::agent::CancelSignal`（`invalidate()` 即广播 = Java 的 `cancelled` sink）；新增 `spawn_lease_renewal`（每 `leaseMs/3` 续租，失败即 fence）；`process_claim` 改为 `select!` —— 租约丢失时先让 agent 协作式收尾（宽限 2s，`TURN_CANCEL_GRACE`），超时则丢弃 turn future 硬中止；**发送前的 `require_lease` 闸门保留**（“晚到的回复绝不外发”契约不变）。
  - `common/channel_core.rs`：`run_turn` 增 `Option<CancelSignal>` 参数并下传。
- **跨模块改动（经授权）**：harness `GatewayTurnContext` 增 `cancel` 字段，`run_with_outbound` / `PlatformCtx::run_turn` 增 `Option<CancelSignal>` 参数，`CallableAgentBridge` 公开并转发信号；core 侧新增 `CancelSignal` / `Error::Cancelled` / `RunGuard`（drop 安全）。
- **测试**：`weixin::channel::tests::a_lost_lease_aborts_the_turn_in_flight`（租约在 turn 运行中丢失 → agent 观察到取消 → 不发任何请求）；已反向验证（把 `run_turn` 改回不传信号 → 该用例 FAILED）。
- **基线**：`cargo test -p agentscope-extensions-channel` → 327 passed / 0 failed；`cargo check --workspace --all-targets` 通过。

### 4.2 下一步与阻塞
1. **无剩余 P0/P1/P2 缺口**（2026-10-23）：P0（G1）见 §7；P1（G2/G3/G4/G5）与 P2（G6/G7/G8）见 §8。
2. **待真实环境联调（非代码缺口）**：DingTalk 真实租户的 HTTP 回调 / Stream 连接、Feishu 事件订阅的 `verificationToken` 配置、各平台真实 token 刷新。
3. **无阻塞项**：`cargo check --all-targets` 与 `cargo test -p agentscope-extensions-channel`（385 passed）均通过；本 crate 无下游依赖方（仅根 `Cargo.toml` 的 members 登记）。

### 4.3 与汇总 tracker 的对应
- 本文件是 CHANNEL 模块的**单一事实来源**；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行、§3.1 由汇总方登记。


---

## 6. 静态复核记录（2026-10-22）

> **阅读提示**：§6.2 / §6.5 是 2026-10-22 的**历史快照**（当时 G2–G8 尚未修复）；§6.3 的数字与结论已按 2026-10-23 实测更新；当前口径以 §1 与 §8 为准。

### 6.1 复核方式
- **纯静态**：只读源码 + `Get-Content | Measure-Object` 计数 + 符号 grep。**未执行 `cargo build` / `cargo test`**，未修改任何 Rust 代码；本文件是唯一被修改的文件。
- 统计口径：
  - **主口径 = 非空行（有效行）**：剔除空行后的代码行数，与汇总 tracker 的 CHANNEL 口径（Java 主 **8251** / 测试 **3954**、Rust **13596**）一致。
  - 副口径 = 原始行数（`Get-Content` 行数，含 license 头、空行与注释），按 `src\main` / `src\test` 分离；两种口径差值恒为注释与空行。
  - Rust 测试数 = `^\s*#\[(tokio::)?test\]` 逐文件计数（不含 `#[cfg(test)]` 模块本身）；已用 `Select-String` 反查「非行首写法」确认无遗漏。
  - Java 测试方法数 = `@Test`（含全限定 `@org.junit.jupiter.api.Test`）+ `@ParameterizedTest`。
- 复核范围：`crates/agentscope-extensions-channel/src/` 全部 68 个 `.rs`；Java 7 个子模块全部 92 个 `.java`。

### 6.2 实测统计（与旧记录对比）
| 指标 | 旧记录 | 实测（2026-10-22） | 差异 |
|---|---|---|---|
| Java 子模块数 | 6 | **7**（漏 `weixin`） | 旧记录漏登最大子模块 |
| Java 文件数 | 39 | **92**（67 main + 25 test） | 旧记录只数了部分 main |
| Java 主代码行数 | 4879 | **8251**（非空）／**9012**（原始） | +3372（旧值约为实际 41%）；旧值把 `weixin` 整块漏掉 |
| Java 测试行数 | 0 | **3954**（非空）／**4347**（原始） | 旧值错误；**测试目录真实存在**（25 个测试文件） |
| Java 测试方法数 | 未记 | **162**（`@Test` 161 + `@ParameterizedTest` 1） | — |
| Rust `.rs` 文件数 | 55 | **68** | +13（weixin 13 个文件） |
| Rust 行数 | 8872 | **13596**（非空）／**15032**（原始） | +4724 |
| Rust 内联测试数 | 241（后改 327） | **327**（225 `#[test]` + 102 `#[tokio::test]`） | 241 已过期；327 准确 |
| Rust `*Channel` 适配数 | 5 | **6**（含 `WeixinChannel`） | 旧记录漏 weixin |
| axum Controller 数 | 4 | **4**（Feishu/WeCom/GitHub/GitLab） | ✅ 属实；但 Java 有 5 个 |
| `DingTalkStreamClient` | 声称落地 | **存在**（`dingtalk/stream.rs`，822 行，14 测试） | ✅ 符号属实；但未被 Channel 启动 |

**Java 测试目录实测明细**（非空行 / 测试方法数）：common 4 文件 300 行 / 25 个；dingtalk 7 文件 801 行 / 46 个；feishu 2 文件 205 行 / 12 个；wecom 3 文件 325 行 / 16 个；weixin 9 文件 2323 行 / 70 个；**github 0、gitlab 0**。合计 **25 个测试文件 / 3954 非空行 / 162 个测试方法**。

**「5 个 `*Channel`」的解释**：旧记录的 5 指 Feishu/DingTalk/WeCom/GitHub/GitLab——它把当时的 6 个 Java 平台里的 **weixin 整块漏掉了**（既不在子模块清单，也不在类清单）。复核后实际是 **6 个 Java 平台 ↔ 6 个 Rust `*Channel`**（`impl PlatformChannel for` 命中 6 处：DingTalk/Feishu/GitHub/GitLab/WeCom/Weixin），一一对应、**无「某平台故意没有独立适配」的设计理由**。反方向的唯一不对称是 Rust 多出一个 `WeixinChannelRegistry`（Java 无此类），属对称性新增而非缺口。

**符号逐个确认（任务第 4 项）**：
| 声明 | 结论 | 证据 |
|---|---|---|
| 5 个 `*Channel` 适配 | ❌ **不实**（实为 6 个） | `impl PlatformChannel for` 命中 `dingtalk/channel.rs:142`、`feishu/channel.rs:212`、`github/channel.rs:187`、`gitlab/channel.rs:186`、`wecom/channel.rs:240`、`weixin/channel.rs:807` |
| 4 个 axum CallbackController | ✅ **属实** | `feishu/controller.rs`、`wecom/controller.rs`、`github/controller.rs`、`gitlab/controller.rs` 各含 `axum::Router::new()` + `.route(...)`；Java 有 **5** 个（缺 DingTalk） |
| `DingTalkStreamClient` 常驻 WebSocket 网关 | ✅ **属实但未接线** | `dingtalk/stream.rs:188` `pub struct DingTalkStreamClient`，配套 `DingTalkStreamProtocol`/`WebSocketConnector`/`StreamSocket`/`TungsteniteConnector`；但全 crate 无 `DingTalkChannel` 持有该 client 的字段（G3） |
| Java 6 平台 ↔ Rust 6 平台 | ✅ **一一对应** | 唯一缺失的「入口」是 **DingTalk HTTP 回调控制器**（不是某平台缺独立适配）；Java 侧 5 个 Registry ↔ Rust 6 个泛型别名（多 `WeixinChannelRegistry`） |

### 6.3 逐子模块对齐结论（行数/测试数与结论均按 2026-10-23 实测更新）
| 子模块 | Java main/test 行（非空） | Java 类数 | Rust 文件/行（非空） | Rust 测试 | 对齐结论 |
|---|---|---|---|---|---|
| common | 593 / 300 | 7 | 15 / 2215 | **62** | ✅ **等价**（含 `InboundEventDeduplicator`/`AccessTokenStore`+`CachedAccessToken`+`InMemoryAccessTokenStore`/`ChannelMediaMetadata`，见 §8.1/§8.2） |
| dingtalk | 1599 / 801 | 9 | 9 / 3454 | **78** | ✅ **等价**（Stream + HTTP 回调双模式、媒体映射，见 §7/§8.1） |
| feishu | 1212 / 205 | 8 | 8 / 1700 | **44** | ✅ **等价**（含普通事件 `header.token` 闸门与媒体映射，见 §8.1/§8.5） |
| github | 873 / 0 | 8 | 8 / 1243 | **32** | ✅ **等价**（Java 侧本就无测试，Rust 有 32 个） |
| gitlab | 766 / 0 | 7 | 8 / 1160 | **35** | ✅ **等价**（Java 侧本就无测试，Rust 有 35 个） |
| wecom | 1254 / 325 | 8 | 9 / 1790 | **48** | ✅ **等价**（含媒体映射 + `with_store` 注入，见 §8.1/§8.2） |
| weixin | 1954 / 2323 | 20 | 13 / 4732 | **86** | ✅ **等价**（含 GAP-19 取消语义，Rust 侧额外增强；Java 基线本轮无法复核，见 §8.8） |
| **合计** | **8251 / 3954** | **67** | **71 / 16334** | **385** | **7 子模块全部等价** |

> 原始行数口径（供与历史记录对账）：Java **9012 / 4347**、Rust **18000**；Rust 分模块原始行 common 2482 / dingtalk 3795 / feishu 1856 / github 1383 / gitlab 1300 / wecom 1960 / weixin 5181 + `lib.rs` 43。


### 6.4 新发现缺口

> **状态（2026-10-23）**：下表 G1–G8 **已全部关闭**——G1 见 §7，G2–G8 见 §8。本表保留为 2026-10-22 静态复核快照。

| ID | 优先级 | 缺口 | Java 依据 | Rust 现状 |
|---|---|---|---|---|
| G1 | **P0** | **DingTalk HTTP 回调模式缺失** | `DingTalkCallbackCrypto.java`(190) 的 `verify(timestamp,sign)`（HMAC-SHA256，1h 时效 + 5min 未来偏差）+ `decrypt`（`aesKey` 43 字符 base64，解 32 字节，前 16 字节作 IV）；`DingTalkCallbackController.java`(157) `POST /api/channels/dingtalk/{channelId}/callback`；`DingTalkChannelProperties` 的 `mode`/`aesKey` | `dingtalk/` 下**无 crypto.rs、无 controller.rs**；`properties.rs` 无 `mode`/`aesKey` 字段；`DingTalkChannel` 无 `crypto()` 访问器。Java 侧该能力由 commit `ca524056a` 发布且有 228 行测试，**不能按「不迁移」关闭** |
| G2 | **P1** | **入站媒体契约 `ChannelMediaMetadata` 整体缺失** | `common/ChannelMediaMetadata.java`(153)：4 种 kind + `markerText(kind,fileName)`（文件名截断 80 code point）+ 稀疏键 `channelMediaKind`/`channelMediaProviderType`/`channelMediaId`/`channelMediaUrl`/`channelMediaSecondaryId`/`channelMediaFormat`/`channelMediaFileName`/`channelMediaDurationMs`/`channelMediaRecognition` | 全 crate 无 `channelMediaKind`/`marker_text` 符号。三处 mapper 在非 text 时直接 `return None`：`dingtalk/inbound.rs:42-44`、`feishu/inbound.rs:71-76`、`wecom/inbound.rs:39-41`。影响：DingTalk picture/audio/video/file、Feishu image/file/folder/audio/media、WeCom image/voice/video **全部静默丢弃**（Java 会回中性 marker 让 agent 感知并引导用户） |
| G3 | **P1** | **`DingTalkStreamClient` 未接线** | `DingTalkChannel.start()` 在 stream 模式调 `streamClient.start()`、`stop()` 对称关闭；`DingTalkChannel` 持有 `streamClient` 字段 | `DingTalkChannel` 无 `stream_client` 字段、无 `start()`/`stop()`；`DingTalkStreamClient::for_channel` 已存在但**全 crate 无调用点**。`dingtalk/channel.rs:11-16` 模块文档仍写「Stream client is out of scope」——已过期（stream.rs 已落地），会误导后续会话 |
| G4 | **P1** | **`InboundEventDeduplicator` / `AccessTokenStore` seam 缺失** | 两个接口 + `CachedAccessToken` + `InMemoryAccessTokenStore`；5 个 `*Channel.fromProperties` 各有「默认 / 注入 deduplicator / 再注入 tokenStore」三档重载 | `IdempotencyStore`、`TokenCache` 是具体类型且被硬编码进 `*Channel::new` / `*AccessTokenProvider::new`；无 trait、无注入点。多实例部署无法共享去重与 token 缓存（Java 文档明确要求 Redis 场景） |
| G5 | **P1** | **Feishu 普通事件鉴权闸门缺失** | `FeishuCallbackController.java:136-148`：普通事件要求 `header.token` 与配置的 `verificationToken` **恒定时间相等**，否则 401 | `feishu/channel.rs` 仅在 `url_verification` 分支（:184）比对 body 的 `token`；普通事件路径无 token 校验。当 `verificationToken` 未配置时，Java 也 401，Rust 直接放行 → **行为差异，安全相关** |
| G6 | P2 | `BotLoopGuard` 空闲淘汰缺失 | `evictIdlePeers()`（idle > `window+cooldown`）+ `trackedPeers()`；随 `allow()` 搭车扫描 | 只有窗口内事件淘汰；`HashMap<String, PeerState>` 随 peer 数单调增长，无 `tracked_peers()` |
| G7 | P2 | 过期文档注释（5 处，非 3 处） | — | `dingtalk/channel.rs:8,11-16`（说 Stream client 未移植 / 「out of scope」）、`lib.rs:23-25`（说 Stream 客户端、Feishu/WeCom 控制器、AES-CBC 解密「Not yet ported」）、`feishu/mod.rs:9-10`（说 `FeishuCallbackController`「Not yet ported」，但 `feishu/controller.rs` 已落地）、`common/channel_registry.rs:1`（说控制器「not-yet-ported」）——均已被实现证伪，建议清理以免误导后续会话 |
| G8 | P2 | **`IdempotencyStore` TTL 过期后的「窗口重启」语义差异** | `common/IdempotencyStore.java:62-78`：TTL 过期后 `firstSeen` 返回 `true` **且用 CAS 把 `seen_at` 刷新为 now**（"the accepted observation restarts the retention window"），因此紧接着的同 key 重投**再次被丢弃**（Java `IdempotencyStoreTest.entriesExpireAfterTtl` 断言第 50 行 `assertFalse(store.firstSeen("ch\|1"))` 明确覆盖）；另 `sweep` 在容量未满时**直接 return**，不做 TTL 清理（`:81-86`） | `common/idempotency_store.rs:87-110` `first_seen_at`：TTL 过期时返回 `true`，但**不刷新 `seen_at_ms`**，旧时间戳保留 → 同一 TTL 窗口内的**连续重投会被反复放行**（Java 只放行一次）；且 `Inner::sweep`（:123-126）在 `entries.len() < max_entries` 时提前 return，与 Java 一致 ✓。现有 Rust 测试 `key_is_treated_as_new_again_after_ttl`（:167-174）只断言「过期后为 true」，**未覆盖「过期后紧接着的重投应被丢弃」**，故该差异未被发现。影响面：WeCom / Feishu / GitHub / GitLab 的重试去重在 TTL 边界上会多放行一次 |

### 6.5 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 Java 子模块 | 6 个（common…wecom） | **7 个**，漏 `weixin` |
| 2 | §1 Java 规模 | 主 4879 / 测试 0 行，39 文件 | 主 **8251** / 测试 **3954** 行（非空口径；原始行 9012 / 4347），**92** 文件（67+25），**162** 个测试方法（`@Test` 161 + `@ParameterizedTest` 1） |
| 3 | §1/§3.2 Java 测试 | 「Java 侧 0 行测试」 | **25 个测试文件 / 3954 非空行**：weixin 2323、dingtalk 801、wecom 325、common 300、feishu 205；仅 github、gitlab 为 0 |
| 4 | §1 Rust 规模 | 55 个 `.rs` / 8872 行 | **68** 个 / **13596** 行（非空口径；原始行 15032） |
| 5 | §1/§3.1 测试数 | 241（§4.1 又写 327） | **327**（225 `#[test]` + 102 `#[tokio::test]`），241 为过期值 |
| 6 | §2.1 common | Java 172 行 | **652** 行（原始）/ **593** 行（非空）/ 7 类（原表只列 2 类） |
| 7 | §2.1 BotLoopGuard | 85 行 / ✅ 已迁移 | **160** 行 / ⚠️ 部分（缺 `evictIdlePeers`/`trackedPeers`） |
| 8 | §2.1 IdempotencyStore | 87 行 / ✅ 已迁移 | **106** 行 / ⚠️ 部分（缺 `InboundEventDeduplicator` seam；**另有 TTL 窗口重启语义差异，见 G8**） |
| 9 | §2.2 dingtalk | Java 962 行 / 6 类 | **1730** 行（原始）/ **1599**（非空）/ **9** 类（漏 `DingTalkCallbackCrypto`、`DingTalkCallbackController`、`DingTalkChannelRegistry`） |
| 10 | §2.2 各文件行数 | Properties 80 / Inbound 117 / Outbound 163 / Token 92 / Channel 229 / Stream 281 | **136 / 232 / 175 / 116 / 346 / 305** |
| 11 | §2.3 feishu | Java 1054 行 | **1312** 行（原始）/ **1212**（非空）；各文件行数普遍偏小（Properties 76→**83**、Inbound 175→**280**、Outbound 167→**180**、Token 95→**119**、Crypto 130→**138**、Controller 164→**178**、Channel 206→**284**、Registry 41→**50**） |
| 12 | §2.3 Controller 状态 | ✅ 本轮完成 | ⚠️ 部分：缺 Java 的普通事件 `header.token` 恒定时间比对（G5） |
| 13 | §2.4 github | Java 853 行 | **964** 行（原始）/ **873**（非空）；行数：72→**78**、116→**127**、125→**134**、70→**77**、91→**100**、139→**154**、201→**246**、39→**48** |
| 14 | §2.5 gitlab | Java 746 行 | **851** 行（原始）/ **766**（非空）；行数：69→**75**、108→**119**、117→**127**、80→**89**、142→**158**、191→**235**、39→**48** |
| 15 | §2.6 wecom | Java 1092 行 | **1353** 行（原始）/ **1254**（非空）；行数：101→**108**、120→**204**、165→**202**、94→**118**、158→**166**、200→**213**、209→**288**、45→**54** |
| 16 | §2.7（缺） | 无 weixin 章节 | 已新增 §2.7：Java **2150** 行（原始）/ **1954**（非空）/ 20 类 ↔ Rust 13 文件 / **5162** 行（原始）/ **4714**（非空）/ **86** 测试，✅ 等价 |
| 17 | §3.1 provider 配置 | `*ChannelProperties.java` ×5，5 个 properties.rs | ×**7**，**7** 个 `properties.rs` |
| 18 | §4.1 「五个平台的 `*Channel` 适配」 | 5 个 | **6** 个（含 Weixin） |
| 19 | §4.1.1 文件行数 | 表中 12 行数字 | 全部按实测修正（如 `stream.rs` 739→**822**、`wecom/channel.rs` 459→**502**、`gitlab/controller.rs` 178→**200**） |
| 20 | §4.2 「无剩余 P0/P1 项」 | 无剩余 | **不成立**：P0×1（G1）+ P1×4（G2–G5） |
| 21 | §1 当前状态 | 已完成（P0+P1+P2 全部落地） | **进行中**（见 §6.4） |
| 22 | §2.3 表 | `FeishuCrypto`「✅ 本轮完成解密半部」 | ✅ **全等价**（签名 + 解密均落地，非「半部」） |
| 23 | §3.3 注册表 | 4 个 Java Registry | Java 有 **5** 个（含 `DingTalkChannelRegistry`），Rust 6 个别名（多 `WeixinChannelRegistry`） |
| 24 | §1 「五个平台 `*Channel` + 四个 Controller」 | 表述含混，暗示 5 个平台之一无适配 | 实为 **6 平台 6 适配**；缺的 Controller 是 **DingTalk**（不是某平台「无独立适配」的设计决定） |
| 25 | §6.2/§6.3 统计口径 | 上一版复核节用**原始行**（9012 / 4347 / 15032） | 本轮统一改为**非空行**主口径（**8251 / 3954 / 13596**）以对齐汇总 tracker；原始行数在 §1 与 §6.2 中并列保留 |
| 26 | §6.3 Rust 分模块测试数 | common **49**、gitlab **34**、weixin **90** | 实测 **50 / 35 / 86**（合计仍 327；dingtalk 42、feishu 37、github 32、wecom 45 均正确） |
| 27 | §2.4 / §2.5 测试行 | github「Rust 9 个单测」、gitlab「Rust 10 个单测」 | 实测 **github 32**、**gitlab 35**（旧值为写表时的局部计数，非模块合计） |
| 28 | §2.7 weixin 测试行 | 「Java 侧 2527 行 / Rust 90 个单测」 | Java **2323** 非空行（原始 2527）/ **9** 个测试文件 / 70 个测试方法；Rust **86** 个单测 |
| 29 | §6.4 G7 | 过期注释「三处」 | 实为 **5 处**（新增 `feishu/mod.rs:9-10` 声称 `FeishuCallbackController` 未移植，实际已落地） |
| 30 | §6.4（新增 G8） | 未记录 | **`IdempotencyStore` TTL 过期后不刷新 `seen_at_ms`**，与 Java「窗口重启」语义不一致（Java 过期后放行一次并立即丢弃下一次重投；Rust 会连续放行）——上一版复核未发现 |

### 6.6 建议 rollup 状态

> **已执行（2026-10-23）**：上述 P1/P2 项已全部关闭（见 §8），汇总 tracker 的 CHANNEL 行已随之改为 **`已完成`**（`EXTENSIONS_GAP_TRACKER.md` §1 第 5 行、§4.8 #14）。本节保留为 2026-10-22 的建议原文。
- 建议 `EXTENSIONS_GAP_TRACKER.md` 中 CHANNEL 行由 `已完成` 改为 **`进行中`**：文本链路已全绿（327 测试、6 平台 6 适配、4 控制器、Stream 客户端齐备），但存在 **1 个 P0（DingTalk HTTP 回调模式）** 与 **4 个 P1**（媒体契约、Stream 未接线、可插拔 seam、Feishu 事件鉴权），其中 G1/G5 属「Java 已发布能力 + 安全语义」缺口，不宜以「不迁移」关闭。
- 建议 rollup 行内的规模数字同步改为：**Java 主 8251 / 测试 3954（非空行），25 个测试文件，162 个测试方法；Rust 68 文件 / 13596 行 / 327 测试**。原文「4879 / 0」「241 test」「5 个 `*Channel` 适配」三处均需订正：
  - `4879 / 0` → Java **无测试**的前提错误，测试目录真实存在（25 个文件 / 3954 非空行）；
  - `241 test` → 已过期，当前源码实测 **327**（225 `#[test]` + 102 `#[tokio::test]`）；
  - `5 个 *Channel 适配` → 实为 **6 个**（漏 `WeixinChannel`），「4 个 axum CallbackController」与「`DingTalkStreamClient` 常驻 WebSocket 网关」两项属实。
- 剩余 P2 3 项（G6 空闲淘汰、G7 过期注释 5 处、G8 幂等 TTL 窗口重启语义）建议归入下一批「收尾批」，不阻塞波次 A。

## 7. P0 修复记录（2026-10-22）

### 7.1 G1：DingTalk HTTP 回调模式（已关闭）

| 改动文件 | 内容 |
|---|---|
| `crates/agentscope-extensions-channel/src/dingtalk/crypto.rs` | **新增** `DingTalkCallbackCrypto`：`verify`/`verify_at`（HMAC-SHA256 + 1h 时效 + 5min 未来偏移）、`decrypt`（AES-256-CBC / 32 字节 PKCS#7 / 16B 随机前缀 + 4B big-endian `msg_len`，忽略尾部 trailer）、`encrypt`/`encrypt_with_random`（32 字节块 PKCS#7）、经典 `msg_signature`（SHA1 排序拼接）以及严格解填充 |
| `crates/agentscope-extensions-channel/src/dingtalk/controller.rs` | **新增** `DingTalkCallbackController` + `router`（`POST /api/channels/dingtalk/:channel_id/callback`）：未知 channel/签名失败 401、空/超 64K/非法 JSON/解密失败 400、其余 200；`check_url` 首次验证返回加密 `success`；普通消息交给 `handle_stream_payload` |
| `crates/agentscope-extensions-channel/src/dingtalk/properties.rs` | 新增 `mode`（默认 `stream`，仅允许 `stream`/`http`）、`aes_key`（43 字符校验）、`token`（经典方案，可选）、Rust-only `requireEncryptedCallback`（严格 http 校验：缺 `aesKey`/`token` 直接报错）；`from_json`/`from_map` 全部读取 |
| `crates/agentscope-extensions-channel/src/dingtalk/channel.rs` | 新增 `crypto: Option<_>`、`with_security`、`start(self: &Arc<Self>)`/`stop()`/`is_stream_running()`；stream 模式懒建并启停 `DingTalkStreamClient`（`Weak` sink 避免 Arc 环），http 模式不启动 Stream；`from_json` 仅在 http 模式构造 crypto |
| `crates/agentscope-extensions-channel/src/dingtalk/mod.rs` | 导出 `crypto` / `controller` 模块与 `MODE_STREAM`/`MODE_HTTP` |
| `crates/agentscope-extensions-channel/src/dingtalk/stream.rs` | 仅更新 `DingTalkChannelProperties::new` 调用点的参数（无行为变化） |
| `crates/agentscope-extensions-channel/src/lib.rs` | 修正「Stream/回调控制器/AES 未移植」的过期 crate 文档 |
| `crates/agentscope-extensions-channel/Cargo.toml` | **未改**：`aes 0.8` / `cbc 0.1` / `sha1 0.10` / `hmac 0.12` / `sha2 0.10` / `base64 0.22` 已齐备，未新增依赖 |

### 7.2 与 Java 逐项对照

- **签名**：Java `DingTalkCallbackCrypto.verify` 为 `Base64(HmacSHA256(appSecret, ts + "\n" + appSecret))`，时间戳先 trim 一次，`age > 3_600_000` 或 `-age > 300_000` 拒绝；Rust `verify_at` 逐项复刻（含 `Long.parseLong` 失败即 false、常量时间比较）。
- **解密**：Java `AES/CBC/NoPadding` + 自写严格 PKCS#7（pad 上界 32）、`msgLen < 0 || 20+msgLen > len` 校验、`new String(..., UTF_8)`；Rust 使用 `NoPadding` + `pkcs7_unpad_strict`，UTF-8 采用 lossy（等价 Java 替换语义）。
- **Controller**：401（未知 channel / 缺头 / 签名错 / 过期）、400（空体、>64K 字符、非法 JSON、aesKey 与 body 不匹配）、200（重复/非文本/派发成功）全部对齐；未知 channel 与坏签名同为 401，不泄露 channelId 是否存在。
- **properties**：`mode` 默认 `stream`、非法值报错；`aesKey` 长度 43 校验与 Java 一致；`aesKey` 为可选，plain http 回调继续可用（Java 语义），因此**未**把 `aesKey`/`token` 设为 http 模式强制项。
- **lifecycle**：Java `start()` 在 http 模式注册 registry、stream 模式 `streamClient.start()`，`stop()` 对称；Rust 的 registry 是实例级显式注册（不引入进程单例），`start()` 在 http 模式只做 crypto 自检，stream 模式懒建/启动客户端，`stop()` 异步关停。

### 7.3 金标向量来源

- `HMAC_GOLD = dhJB8TJ0cHPEbwpUauugV2Wz8v4qlWoO+C42nEhJW+c=`：独立 Node `crypto` 对 `HmacSHA256("test-app-secret", "1700000000000\n" + "test-app-secret")` 求得（与 Java `DingTalkCallbackTestSupport.sign` 同式）。
- `LEGACY_GOLD = a514a5a6a03a5f8a76b09bcba6693e42668a076d`：独立 Node 对排序后的 `1700000000|abc123|nonce-1|test-token` 拼接求 SHA-1。
- `CIPHERTEXT_GOLD` / `CIPHERTEXT_GOLD_TRAILER`：独立 Node `aes-256-cbc` 按 Java 测试夹具（零 16B 前缀、`msg_len`、32B 块 PKCS#7、可选 `receive-id` trailer）生成；`AES_KEY` 与 Java 测试常量完全相同（`MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY`）。
- Java 原测试不含固定密文/签名期望值（运行时现算），故金标向量由独立实现生成，而非直接抄录。

### 7.4 验证

- `cargo check -p agentscope-extensions-channel --all-targets`：通过（无新增 error/warning）。
- `cargo test -p agentscope-extensions-channel`：**358 passed / 0 failed**（改动前 327；新增 31 条 DingTalk 回调/生命周期用例）。
- 新增覆盖：HMAC 金标、时效边界（1h / 5min 的 `=` 与 `+1ms`）、经典 msg_signature 金标、加解密往返（含 trailer/中文）、Java 金标密文、篡改首块/末块、非法 base64/长度不足/错块长的显式错误、check_url 加密应答、plain/encrypted 双向不匹配 400、重复 msgId、properties 的 mode/aesKey 校验与严格 http 缺 key 报错、stream `start()/stop()` 与 http 不启 Stream。
- 限制：未做真实 DingTalk 网络回调联调；`check_url` 的加密 `success` 应答是对 Java（返回空 200）的补充，依据 DingTalk 事件订阅协议，尚未在真实租户验证。

### 7.5 与任务描述的差异（据实说明）

- 任务描述中的「`token` + `msg_signature = SHA1(sort(...))` + 加密应答」是经典/事件订阅方案；本仓库 Java 基准（commit `ca524056a`）实际使用 **`appSecret` HMAC-SHA256 + `aesKey` 解密、响应空 200**，且 properties 无 `token`。本实现以 Java 为准，同时把 `token`/SHA1 作为可选兼容路径保留（不配置即完全等同 Java）。
- 「http 模式要求 mode/aesKey/token 三者齐备」与 Java 冲突（Java 允许无 `aesKey` 的 plain http 回调，其控制器测试即如此）。为不破坏 Java 行为，三者齐备改为 Rust-only `requireEncryptedCallback=true` 的显式严格模式；默认关闭。
- G3（Stream 未接线）同步关闭：`DingTalkChannel::start()/stop()` 已驱动 Stream 客户端。

## 8. P1/P2 修复记录（2026-10-23）

> 本轮关闭 §6.4 剩余的 **G2/G4/G5（P1）** 与 **G6/G7/G8（P2）**，共 6 项。所有改动均在本 crate 内，未触 harness/core/根 `Cargo.toml`；无新增第三方依赖。

### 8.1 G2：入站媒体契约（已关闭）

| 改动文件 | 内容 |
|---|---|
| `src/common/media.rs`（**新增**） | `ChannelMediaMetadata` 的 Rust 移植：4 个 kind 常量 + 9 个稀疏键常量（`channelMediaKind` / `channelMediaProviderType` / `channelMediaId` / `channelMediaSecondaryId` / `channelMediaUrl` / `channelMediaFileName` / `channelMediaFormat` / `channelMediaDurationMs` / `channelMediaRecognition`）+ `marker_text(kind, file_name)` + `sanitize_marker_file_name` + `put_if_present` |
| `src/dingtalk/inbound.rs` | `picture` / `audio` / `video` / `file` → marker + 元数据（顶层字段优先、`content` 对象兜底；`downloadCode` 缺失/空白则丢弃；`pictureDownloadCode`→SecondaryId；`recognition`/`duration`；`videoType`/`duration`；`fileName`→FileName 并嵌入 marker） |
| `src/feishu/inbound.rs` | `image` / `file` / `folder` / `audio` / `media` → marker + 元数据（`image`→`image_key`，其余→`file_key`；`media` 的 `image_key` 作 SecondaryId；`audio`/`media` 的 `duration`；`file_name`）；`channelMessageId`/`ReplyToId`/`ThreadId` 仍置于最前 |
| `src/wecom/inbound.rs` | `image` / `voice` / `video` → marker + 元数据（`PicUrl`→Url、`Format`→Format、`ThumbMediaId`→SecondaryId） |

- **marker 语义**：`[image]` / `[audio]` / `[video]` / `[file: report.pdf]`；文件名副本会去除 `[`/`]`、`Cc`、`Cf`、U+2028/2029（→ 空格），折叠 ASCII 空白，截断 **80 code point** 后再 trim；原始名原样落在 `channelMediaFileName`。
- **实现差异（已记录）**：Java 用正则 `\p{Cf}`；Rust std 无 general-category API，故 `is_format_char` 显式枚举常用 Cf 区间（软连字符、U+0600–0605、U+061C、U+180E、U+200B–200F、U+202A–202E、U+2060–2064、U+2066–206F、U+FEFF、U+FFF9–FFFB、U+1D173–1D17A、tag 字符等）。
- **类型语义**：Java `JsonNode.asText()` 会把数字转成字符串（如 `duration: 4200` → `"4200"`），Rust 侧 `get_str` / `as_text` 保持同语义。
- **未映射类型保持不变**：DingTalk `richText`/未知、Feishu `post`/`sticker`/`interactive`、WeCom `event`/`location`/`link` 仍返回 `None`（ack 不派发），与 Java 一致。

### 8.2 G4：`InboundEventDeduplicator` / `AccessTokenStore` 可插拔 seam（已关闭）

| 改动文件 | 内容 |
|---|---|
| `src/common/idempotency_store.rs` | **新增** `pub trait InboundEventDeduplicator: Send + Sync { fn first_seen(&self, key: &str) -> bool; }` + `impl for IdempotencyStore` |
| `src/common/token.rs` | **新增** `CachedAccessToken` / `AccessTokenStore`（`get`/`put`/`put_if_newer`（默认非原子，可覆写）/`clear`）/ `InMemoryAccessTokenStore`（单锁内原子 `put_if_newer`，等价 Java `AtomicReference` CAS）；**删除** 旧的具体类型 `TokenCache`（其职责被 `AccessTokenStore` + `InMemoryAccessTokenStore` 取代） |
| `src/common/channel_core.rs` | `ChannelCore.idempotency: IdempotencyStore` → `deduplicator: Arc<dyn InboundEventDeduplicator>`；**新增** `ChannelSeams`（`deduplicator` / `token_store` 两个可选注入位 + `deduplicator()`/`token_store()` 回落进程内默认实现） |
| `src/{dingtalk,feishu,wecom}/{token.rs}` | 字段改 `store: Arc<dyn AccessTokenStore>`；`token()` 先取 `store.get()` 再由 provider 判 `refresh_at_ms > now`；写入走 `put_if_newer`；`invalidate()` → `store.clear()`；**新增** `with_store(api_base, id, secret, transport, store)` 构造器 |
| `src/{dingtalk,feishu,github,gitlab,wecom,weixin}/channel.rs` | `*Channel::new` 的 `idempotency: IdempotencyStore` → `deduplicator: Arc<dyn InboundEventDeduplicator>`；每个平台新增 `from_json_with(..., seams: &ChannelSeams)`（对应 Java 的「默认 / 注入 deduplicator / 再注入 tokenStore」三档重载），`from_json` 委托 `&ChannelSeams::new()` |
| `src/lib.rs` / `src/common/mod.rs` | 导出新类型；删除 `TokenCache` 导出 |

- **注入链路**：`from_json_with` → `AccessTokenStore` 给 token provider；`ChannelCore` 拿 `deduplicator`，`is_first_seen` 仍在 key 前拼接 `channelId|` 命名空间。
- **未变的部分**：`BotLoopGuard` 不参与 `ChannelSeams`（Java 也没提供注入重载）。

### 8.3 G6：`BotLoopGuard` 空闲 peer 淘汰（已关闭）

- `PeerState` 新增 `last_event_ms`（创建时即打戳，永不误判为空闲）；`Inner` 新增 `last_sweep_ms`。
- 新增 `evict_idle_peers()` / `evict_idle_peers_at(now)`（返回移除数，阈值 = `window + cooldown`，严格 `>`）+ `tracked_peers()`。
- `allow_at()` 搭车 `maybe_sweep`：距上次扫描 ≥ `window + cooldown` 时先扫描再建/取 state（与 Java `maybeSweep` 同序），无后台线程。

### 8.4 G8：`IdempotencyStore` 窗口重启语义（已关闭）

- `first_seen_at` 在 TTL 过期时**先 `seen_at_ms = now` 再放行**，对应 Java `seen.replace(key, prior, now)` 的 CAS 胜者独占：过期后的第一次重投通过、随后的重投仍被丢弃。`sweep` 仍在容量未满时提前返回（与 Java 一致）。
- 新增测试 `an_expired_sighting_restarts_the_retention_window` 覆盖「过期放行 → 紧随的重投被丢弃 → 再过一个 TTL 再次放行」。

### 8.5 G5：Feishu 普通事件鉴权闸门（已关闭）

- `FeishuChannel::handle_callback` 新增第 4 步：取 `header.token`，与 `properties.verificationToken` 做 **恒定时间** 比较（`common::text::constant_time_eq`）；未配置或空白同样 401（与 Java `FeishuCallbackController.java:136-148` 逐项一致，含「不泄露原因」的行为）。
- 测试改动：`plain()` fixture 补 `verificationToken`；所有普通事件 body 补 `header.token`；新增 `normal_events_require_the_configured_header_token`、`events_are_rejected_when_no_verification_token_is_configured`；`url_verification` 用例补 body `token`（Java 同样要求）。

### 8.6 G7：过期文档注释（已关闭）

- `src/lib.rs`：common 条目补 `InboundEventDeduplicator`/`AccessTokenStore`/`media`，删去「cache abstra.」笔误，末尾改为「六平台含 stream/long-poll 生命周期 + 回调控制器 + AES-CBC」。
- `src/feishu/mod.rs`：删除「Not yet ported: FeishuCallbackController」，补全 crypto/channel/controller 条目。
- `src/common/channel_registry.rs`：去掉「(not-yet-ported)」措辞。
- 全 crate 检索 `not yet ported` / `not-yet-ported` / `out of scope` 已无命中。

### 8.7 验证与反向验证

- `cargo check -p agentscope-extensions-channel --all-targets` → **0 error**，本 crate 0 warning（`clippy` 仅剩 crate 内既有 9 条历史提示）。
- `cargo test -p agentscope-extensions-channel` → **385 passed / 0 failed**（本轮 +27：idempotency +1、bot_loop_guard +2、channel_core +3、token +1、feishu token +1、media +5、dingtalk inbound +5、feishu inbound +4、wecom inbound +3、feishu channel +2）。
- **反向验证（4 项，均先回退、确认 FAILED、再恢复）**：
  1. 回退 G8 的 `seen_at_ms` 刷新 → `an_expired_sighting_restarts_the_retention_window` FAILED。
  2. 将 `Inner::evict_idle` 改为不删除 → `idle_peers_are_evicted_and_counted` / `allow_sweeps_idle_peers_opportunistically` FAILED。
  3. 关掉 Feishu token 闸门（条件改为 `false`）→ `normal_events_require_the_configured_header_token` / `events_are_rejected_when_no_verification_token_is_configured` FAILED。
  4. 让 DingTalk `map_media` 直接返回 `None` → `maps_picture_to_the_media_contract` / `maps_audio_and_video_top_level_fields` / `maps_file_with_marker_and_raw_file_name` FAILED。
  5. 让 `ChannelSeams::deduplicator()` 忽略注入 → `seams_forward_injected_implementations` FAILED。
- **未做**：真实平台联调（DingTalk/Feishu/WeCom 租户、真实 token 刷新），仍建议上线前用一个租户各验一次。

### 8.8 与 Java 的差异（均为有意为之，非缺口）

| 项 | Java | Rust | 理由 |
|---|---|---|---|
| 注入形式 | `fromProperties(..., InboundEventDeduplicator[, AccessTokenStore])` 三档重载 | `from_json(...)` + `from_json_with(..., &ChannelSeams)` | 避免每个平台 3 个几乎相同的函数；`ChannelSeams` 两字段均可选，语义等价 |
| token 缓存类型 | `InMemoryAccessTokenStore` | `InMemoryAccessTokenStore`（取代旧 `TokenCache`） | 接口化后不再需要独立的 `TokenCache` 类型；本 crate 无下游依赖方 |
| weixin 工厂 | 本机 Java checkout（`main d7091070`）**已无** `agentscope-extensions-channel-weixin` 子模块 | `WeixinChannel::create(..., &ChannelSeams)` | §2.7 的 weixin Java 基线本轮**无法复核**，沿用既有记录；Rust 侧按对称性补 `from_json_with` |
| Cf 字符集 | 正则 `\p{Cf}` | 显式区间枚举 | Rust `std` 无 Unicode general-category API，不为此新增依赖 |


