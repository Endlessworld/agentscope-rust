# CHANNEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CHANNEL** 模块迁移的**单一事实来源**与**增量跟踪入口**。开工前读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md`；历史过程记录见 `docs/extensions/history/CHANNEL__MIGRATION_HISTORY.md`（**冻结**）。切勿臆断代码现状——以本文件 + `git status` + `cargo test` 实际验证为准。
> **当前状态**：历史基线**已完成（2026-10-23）**；**2026-10-31 全量对齐复核**新增 `CHANNEL-N01`–`N22`（**3 个 P0 未关闭**，见 §3.5），现行状态以 §3.5 为准。Java 后续新特性按 §6 增量跟踪流程登记。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-channel\`（本机 `main` 为 **7** 个子模块，**含 weixin**；早期「已无 weixin」的记载已于 2026-10-26 更正，依据见 §4.2 `CHANNEL-X03`）
> **Java 子模块**：common, dingtalk, feishu, github, gitlab, wecom, weixin（**7** 个；weixin 基线来自早期快照）
> **Rust 目标**：`crates/agentscope-extensions-channel/`（波次 A）

## 0. 如何阅读本文件
- 章节导航：§1 基线 → §2 逐类映射 → §3 历史缺口清单（G1–G10，已全关闭）→ **§3.5 2026-10-31 全量对齐复核清单（唯一编号 N01–N22，现行待办以此为准）** → §4 设计决定/差异（D01–D08 / X01–X13）→ §5 已知限制 → §6 增量流程 → §7 验证基线 → §8 归档。
- **状态取值**：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把 §3 状态改 `已完成`，记录关闭日期，并在 §7 写最新验证结果（命令 + 结果）。
- 本文件只保留**当前事实与待办**；过程记录（复核日志、逐项修复细节、反向验证）一律写入 `CHANNEL__MIGRATION_HISTORY.md`。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-channel`（**7** 个 Maven 子模块：common, dingtalk, feishu, github, gitlab, wecom, **weixin**） |
| Java 规模（实测 2026-10-22） | 主代码 **8251** 行 / 测试 **3954** 行（**非空行**口径；`Get-ChildItem -Recurse -Filter *.java` 逐文件非空行求和，共 **92** 个文件 = 67 main + 25 test；`@Test` 计 **161** 个 + `@ParameterizedTest` 1 个）。分模块 main/test：common 593/300、dingtalk 1599/801、feishu 1212/205、github 873/**0**、gitlab 766/**0**、wecom 1254/325、weixin 1954/2323（原始行数口径：主 **9012** / 测试 **4347**） |
| Rust crate | `crates/agentscope-extensions-channel`（`src/` **71** 个 `.rs`，**16334** 行（非空行口径；原始行数 **18000**）；内联测试 **405** passed（2026-10-31 实测）；已知基线：2026-10-26 G9 关闭后实测 **399**（**2026-10-29 勘误**：原记 385 = 266 `#[test]` + 119 `#[tokio::test]`，见 §3.1；**2026-10-31** G10 新增 6 个） |
| 迁移波次 | A |
| 当前状态 | **历史基线已完成，2026-10-31 全量复核新增 3 个 P0（未关闭）**（见 §3.5：N01 GitHub/GitLab 机器人身份未解析、N02 Feishu dispatch 失败丢消息、N03 WeCom 出站忽略 errcode）。历史基线（P1/P2 清零；2026-10-31 实测 405 passed / 0 failed）—— 基线 2026-10-23 起全部 P0/P1/P2 缺口关闭（§3，细节见 history/CHANNEL__MIGRATION_HISTORY.md）：common 主干（`ChannelMediaMetadata`、`InboundEventDeduplicator`/`AccessTokenStore` seam）、七平台 `*ChannelProperties`/`*InboundMapper`（text + 媒体）/`*OutboundClient`/`*AccessTokenProvider`/签名与解密、六个平台 `*Channel` 适配（含 weixin）+ 五个 axum CallbackController + `DingTalkStreamClient`（常驻 WebSocket 网关）均已落地；G10（weixin 消费循环重试链 + 退避升级）已关闭。验证基线见 §7。 |

### 1.1 与 harness 的边界（已裁定，必须遵守）
harness 提供**路由/会话绑定骨架**（不可改）：`crates/agentscope-harness/src/agent/` 下 `channel.rs`、`chatui.rs`、`inbound.rs`、`router.rs`、`binding.rs`、`peer.rs`、`dm_scope.rs`、`config.rs`。
本 crate 只提供**平台适配**：`access token provider`、`inbound mapper`、`outbound client`、`channel properties`、`signature verifier`。
不重复定义 `Channel` / `InboundMessage` / `Peer` / `PeerKind` / `OutboundAddress` —— 一律从 `agentscope_harness::agent` 导入复用。

### 1.2 公共层归属（已裁定）

harness 新增 **`agent/gateway/platform_channel.rs`**（`PlatformChannel` trait + `PlatformCtx`），承载「每平台 inbound 分发 + outbound 投递」中**完全相同**的部分：持有 `init` 注入的 gateway、`ChannelRouter` 路由、`apply_routing_config` 热切换，并提供 `Channel` / `ChannelSink` 的 **blanket impl** —— 因此每个平台适配器只需实现 `ctx()` + `deliver_reply()` 两个方法，就能直接当作 harness `Channel` 使用，无需额外包装类型。

本 crate 的 `common/channel_core.rs`（`ChannelCore`）**组合** `PlatformCtx`（不再自存 `channel_id`/`config`/`router`/`gateway`），只追加 Java 各 `*Channel` 私有持有的两个守卫（`InboundEventDeduplicator` + `BotLoopGuard`，经 `ChannelSeams` 可注入），并提供 `admit(dedupe_key, peer_key)` 按 Java 顺序一次性执行「去重 → 防回环」。

> 历史坑（勿重蹈）：`common/channel_core.rs` 与 `common/channel_registry.rs` 曾「已落盘但未在 `mod.rs` 声明」，从不参与编译。新增文件后必须确认 `mod.rs` 已声明并跑测试。

> 注意事项：`agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 里的**遗留事件通道 struct**（与路由 trait 同名）；路由 trait 需用全路径 `agentscope_harness::agent::gateway::channel::Channel` 导入。同时 `Channel` 与 `ChannelSink` 都有 `channel_id()`，在两者均在作用域时需限定调用（如 `Channel::channel_id(&c)`）。

## 2. Java 清单与依赖映射

> **行数口径**：本节各表内标注的 Java 逐类行数与 Rust 逐文件行数均为**原始行数**（含空行与注释）；括号内补充**非空行**（有效行）数，与 §1 汇总口径（Java 主 8251 / 测试 3954、Rust 16334）一致。两种口径差值恒为注释与空行。

### 2.1 common（Java **652** 行（非空 593）/ 7 类 → Rust `common/` 15 个文件 2482 行（非空 2215）含 crate 级公共件）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `common/BotLoopGuard.java` (**160**) | 按 peer 的滑动窗口节流，防机器人自回环（含 `windowMillis+cooldownMillis` 后的空闲 peer 淘汰） | `common/bot_loop_guard.rs` (`BotLoopGuard`, `BotLoopGuardConfig`) | ✅ 等价：节流/冷却/窗口淘汰 + `evict_idle_peers()`/`tracked_peers()` + 搭车 `maybe_sweep` |
| `common/IdempotencyStore.java` (**106**) | 有界 + TTL 的入站事件去重（`implements InboundEventDeduplicator`） | `common/idempotency_store.rs` (`IdempotencyStore`) | ✅ 等价：TTL/容量/惰性 `sweep`，且**过期放行后刷新 `seen_at_ms`（窗口重启语义）** |
| `common/InboundEventDeduplicator.java` (54) | 去重策略接口（共享存储可插拔；`null` key 视为首次） | `common/idempotency_store.rs::InboundEventDeduplicator`（+ `impl for IdempotencyStore`） | ✅ 等价；经 `ChannelSeams::deduplicator()` 注入进 `ChannelCore` |
| `common/AccessTokenStore.java` (85) + `CachedAccessToken.java` (34) + `InMemoryAccessTokenStore.java` (60) | 可插拔 token 缓存槽（`get`/`put`/`putIfNewer`，单凭据一实例） | `common/token.rs`：`AccessTokenStore` / `CachedAccessToken` / `InMemoryAccessTokenStore` | ✅ 等价（`put_if_newer` 在内存实现中原子合并；三个 provider 均有 `with_store(...)` 注入点） |
| `common/ChannelMediaMetadata.java` (153) | 入站媒体（image/audio/video/file）中性 marker 文本 + 稀疏元数据键契约（`channelMediaKind`/`channelMediaId`/`channelMediaUrl`/…） | `common/media.rs`（常量 + `marker_text` / `sanitize_marker_file_name` / `put_if_present`） | ✅ 等价（DingTalk/Feishu/WeCom 三平台 mapper 均已接入） |
| —（Rust 侧新增） | 每平台必用的公共件 | `common/error.rs`(`ChannelError`)、`common/http.rs`(`HttpTransport`/`ReqwestTransport`/`PreparedRequest{Response}`)、`common/outbound_address.rs`(`parse_peer_target`/`PeerTarget`)、`common/props.rs`、`common/text.rs`、`common/token.rs`(`AccessTokenProvider`/`AccessTokenStore`)、`common/crypto.rs`(`aes_256_cbc_decrypt`/`pkcs7_unpad`)、`common/xml.rs`、`common/media.rs`、`common/testing.rs`(`#[cfg(test)]` 测试替身) | ✅ |
| —（Rust 侧新增） | `*Channel` 共用的分发管线 + 类型化注册表 + 注入缝 | `common/channel_core.rs`(`ChannelCore`：组合 harness `PlatformCtx` + 两个守卫 + `admit(...)`/`is_first_seen(...)`/`run_turn(...)`；`ChannelSeams`：`deduplicator`/`token_store` 注入包)、`common/channel_registry.rs`(`ChannelRegistry<C>`：类型化 `channel_id -> Arc<C>` 查找，可选镜像注册到 harness `ChannelManager`) | ✅ 已接线 |

### 2.2 dingtalk（Java **1730** 行（非空 1599）/ 9 类 → Rust `dingtalk/` 9 个文件 3795 行（非空 3454））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DingTalkChannelProperties.java` (**136**) | 配置解析（含 `mode`=stream/http、`aesKey`） | `dingtalk/properties.rs` | ✅ 等价（含 `mode`=stream/http、`aesKey` 43 字符校验、可选 `token`、Rust-only `requireEncryptedCallback`；见 history §7） |
| `DingTalkInboundMapper.java` (**232**) | Stream/HTTP 机器人消息 JSON → `InboundMessage`（text + picture/audio/video/file 媒体） | `dingtalk/inbound.rs` | ✅ 等价（text、DIRECT/GROUP peer、`channelMessageId`、媒体 marker + `channelMediaKind`/`Id`/`SecondaryId`/`Format`/`DurationMs`/`Recognition`/`FileName`） |
| `DingTalkOutboundClient.java` (**175**) | 单聊/群聊发送请求体构造 | `dingtalk/outbound.rs` | ✅ 等价（`/v1.0/robot/oToMessages/batchSend` + `/v1.0/robot/groupMessages/send` 路径、PeerKind 分支一致） |
| `DingTalkAccessTokenProvider.java` (**116**) | OpenAPI `accessToken` 缓存获取 | `dingtalk/token.rs` | ✅ 等价（`/v1.0/oauth2/accessToken` + 80% TTL + `with_store(...)` 注入 `AccessTokenStore`） |
| `DingTalkChannel.java` (**346**) | `Channel` 生命周期/路由适配（stream/http 双模式，`start()` 二选一） | `dingtalk/channel.rs`（`DingTalkChannel` + `handle_stream_payload`，实现 harness `PlatformChannel`） | ✅ 等价（入站管线 + `deliver_reply`；`start(self: &Arc<Self>)` / `stop()` / `is_stream_running()` 按模式拉起 Stream 客户端或自检 crypto，见 history §7） |
| `DingTalkStreamClient.java` (**305**) | 常驻 WebSocket 网关（Stream 模式） | `dingtalk/stream.rs`（`DingTalkStreamClient` + `DingTalkStreamProtocol` + `WebSocketConnector`/`StreamSocket` seam + `TungsteniteConnector`） | ✅ 协议/注册/ACK/退避重连/幂等启停等价（14 个单测）；已由 `DingTalkChannel::start()/stop()` 拉起与关停（history §7） |
| `DingTalkCallbackCrypto.java` (**190**) | HTTP 回调 `sign`（HMAC-SHA256，1h 时效 + 5min 未来偏差）校验 + `aesKey` AES-CBC 载荷解密 | `dingtalk/crypto.rs`（`DingTalkCallbackCrypto`） | ✅ 等价（见 history §7.2） |
| `DingTalkCallbackController.java` (**157**) | HTTP 回调入口（`POST /api/channels/dingtalk/{channelId}/callback`，未知 channel/验签失败→401，body 非法→400，其余 200） | `dingtalk/controller.rs`（`DingTalkCallbackController` + `router`） | ✅ 等价（401/400/200 + `check_url` 加密应答） |
| `DingTalkChannelRegistry.java` (**73**) | 注册表 | `dingtalk::DingTalkChannelRegistry` = `common::ChannelRegistry<DingTalkChannel>`（泛型收敛） | ✅ 等价（实例作用域，非进程单例） |

### 2.3 feishu（Java **1312** 行（非空 1212）/ 8 类 → Rust `feishu/` 8 个文件 1856 行（非空 1700））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `FeishuChannelProperties.java` (**83**) | 配置解析 | `feishu/properties.rs` | ✅ 等价（appId/appSecret/encryptKey/verificationToken/callbackPath/apiBase） |
| `FeishuInboundMapper.java` (**280**) | 事件订阅 v2 信封 → `InboundMessage`（text + image/file/folder/audio/media 媒体） | `feishu/inbound.rs` | ✅ 等价（text、mention 剥离、DIRECT/GROUP、`channelMessageId`/`channelReplyToId`/`channelThreadId`、媒体 marker + `channelMediaKind`/`Id`/`SecondaryId`/`DurationMs`/`FileName`） |
| `FeishuOutboundClient.java` (**180**) | `POST /open-apis/im/v1/messages` 请求构造（含 threaded reply / `uuid` 去重） | `feishu/outbound.rs` | ✅ 等价（`uuid`、`reply_in_thread`、`/messages/{thread_id}/reply`） |
| `FeishuAccessTokenProvider.java` (**119**) | `tenant_access_token` 缓存获取 + 失效重取 | `feishu/token.rs` | ✅ 等价（缓存/失效重取 + `with_store(...)` 注入 `AccessTokenStore`） |
| `FeishuCrypto.java` (**138**) | AES-256-CBC 载荷解密 + `X-Lark-Signature` 校验 | `feishu/crypto.rs` | ✅ 等价（`SHA-256(encryptKey)` 派生、`|16B IV|CBC+PKCS#7|`、`SHA256(ts+nonce+key+body)`） |
| `FeishuCallbackController.java` (**178**) | HTTP 回调入口 | `feishu/controller.rs`（axum 薄壳，承载全部逻辑在 `FeishuChannel::handle_callback`） | ✅ 等价（路由/401/400/challenge 回显/`{}` 200；含普通事件 `header.token` 恒定时间闸门，history §8.5） |
| `FeishuChannel.java` (**284**) | Channel 适配 | `feishu/channel.rs`（实现 harness `PlatformChannel`） | ✅ 等价（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `FeishuChannelRegistry.java` (**50**) | 注册表 | `feishu::FeishuChannelRegistry` = `common::ChannelRegistry<FeishuChannel>`（泛型收敛） | ✅ 等价 |

### 2.4 github（Java **964** 行（非空 873）/ 8 类 → Rust `github/` 8 个文件 1383 行（非空 1243））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitHubChannelProperties.java` (**78**) | 配置解析 | `github/properties.rs` | ✅ 等价 |
| `GitHubInboundMapper.java` (**127**) | webhook delivery → `InboundMessage` | `github/inbound.rs` | ✅ 等价（`issue_comment` / `pull_request_review_comment`、THREAD peer `owner/repo#n`） |
| `GitHubOutboundClient.java` (**134**) | issue/PR 评论 POST 请求构造 | `github/outbound.rs` | ✅ 等价 |
| `GitHubSignatureVerifier.java` (**77**) | `X-Hub-Signature-256` HMAC-SHA256 校验 | `github/signature.rs` | ✅ 等价 |
| `GitHubBotIdentityResolver.java` (**100**) | 机器人身份解析（防自回环判定，`refresh()` 失败保留旧值） | `github/bot_identity.rs` | ✅ 等价（含 `refresh`/`try_refresh`/`is_self`） |
| `GitHubWebhookController.java` (**154**) | HTTP webhook 入口 | `github/controller.rs`（axum 薄壳） | ✅ 等价（404/401/400/200 映射） |
| `GitHubChannel.java` (**246**) | Channel 适配 | `github/channel.rs`（`handle_webhook`：HMAC→解析→映射→自评论丢弃→去重→防回环→分发） | ✅ 等价 |
| `GitHubChannelRegistry.java` (**48**) | 注册表 | `github::GitHubChannelRegistry`（泛型收敛） | ✅ 等价 |
| —（测试） | 该子模块 Java 侧 **0 行测试**（无 `src/test` 目录） | Rust **32** 个单测 | ✅ |

### 2.5 gitlab（Java **851** 行（非空 766）/ 7 类 → Rust `gitlab/` 8 个文件 1300 行（非空 1160））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitLabChannelProperties.java` (**75**) | 配置解析 | `gitlab/properties.rs` | ✅ 等价 |
| `GitLabInboundMapper.java` (**119**) | Note Hook 载荷 → `InboundMessage` | `gitlab/inbound.rs` | ✅ 等价 |
| `GitLabOutboundClient.java` (**127**) | issue/MR note POST 请求构造 | `gitlab/outbound.rs` | ✅ 等价 |
| —（Java 内联在 Controller） | `X-Gitlab-Token` 常量时间校验 | `gitlab/signature.rs` (`verify_webhook_token`) | ✅ 等价 |
| `GitLabBotIdentityResolver.java` (**89**) | 机器人身份解析 | `gitlab/bot_identity.rs` | ✅ 等价 |
| `GitLabWebhookController.java` (**158**) | HTTP webhook 入口 | `gitlab/controller.rs`（axum 薄壳） | ✅ 等价（404/401/400/200） |
| `GitLabChannel.java` (**235**) | Channel 适配 | `gitlab/channel.rs`（`handle_webhook`：token→解析→映射→自评论丢弃→去重→防回环→分发） | ✅ 等价 |
| `GitLabChannelRegistry.java` (**48**) | 注册表 | `gitlab::GitLabChannelRegistry`（泛型收敛） | ✅ 等价 |
| —（测试） | 该子模块 Java 侧 **0 行测试**（无 `src/test` 目录） | Rust **35** 个单测 | ✅ |

### 2.6 wecom（Java **1353** 行（非空 1254）/ 8 类 → Rust `wecom/` 9 个文件 1960 行（非空 1790））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeComChannelProperties.java` (**108**) | 配置解析 | `wecom/properties.rs` | ✅ 等价 |
| `WeComInboundMapper.java` (**204**) | 解密后回调 XML → `InboundMessage`（text + image/voice/video 媒体） | `wecom/inbound.rs`（依赖 `common/xml.rs`） | ✅ 等价（text、`MsgId`、DIRECT peer、媒体 marker + `channelMediaKind`/`Id`/`Url`/`Format`/`SecondaryId`） |
| `WeComOutboundClient.java` (**202**) | `/cgi-bin/message/send` + `/cgi-bin/appchat/send` 请求构造 | `wecom/outbound.rs` | ✅ 等价 |
| `WeComAccessTokenProvider.java` (**118**) | `access_token` 缓存获取 | `wecom/token.rs` | ✅ 等价（缓存/失效重取 + `with_store(...)` 注入 `AccessTokenStore`） |
| `WeComCrypto.java` (**166**) | 回调 SHA-1 签名校验 + AES-256-CBC 解密（+ 加密） | `wecom/crypto.rs` + `wecom/signature.rs` | ✅ 等价（`sign`/`verify_signature`/`decrypt`/`encrypt`） |
| `WeComCallbackController.java` (**213**) | HTTP 回调入口（含 URL 验证握手） | `wecom/controller.rs`（axum 薄壳；`GET` 走 URL 验证、`POST` 走事件） | ✅ 等价 |
| `WeComChannel.java` (**288**) | Channel 适配 | `wecom/channel.rs`（`verify_url` / `handle_callback`：验签→解密→去重→映射→防回环→分发） | ✅ 等价 |
| `WeComChannelRegistry.java` (**54**) | 注册表 | `wecom::WeComChannelRegistry`（泛型收敛） | ✅ 等价 |

### 2.7 weixin（Java **2150** 行（非空 1954）/ 20 类 → Rust `weixin/` 13 个文件 **5181** 行（非空 4732））
> 注：weixin 子模块不在 tracker 旧版清单内（旧版只登记 6 个子模块）。Rust 侧 `weixin/` 是全 crate 最大的模块（4732 非空行，占 16334 的 29%），13 个文件 / 86 个单测。**本机 Java checkout 已无 weixin 子模块（见 §4.2），下表的 Java 行数/类数来自早期快照，下次有 weixin 分支时需重新核对。**

| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeixinChannelProperties.java` (**83**) | 配置解析（accountId/baseUrl/ilinkUserId/channelVersion/botAgent/5 个超时与租约） | `weixin/properties.rs` | ✅ 等价（10 个字段逐一对应） |
| `WeixinChannel.java` (**644**) | Channel 生命周期/长轮询/租约/收件箱/回复 | `weixin/channel.rs` | ✅ 等价（`begin_consuming`/`end_consuming`/`run_consumer`/`poll_once`/`drain_inbox`/`send_reply`/`spawn_lease_renewal`/`LeaseSession`；含 GAP-19 取消信号；消费退避按 Java 语义升级至 `maxBackoffMs`，见 §3.2 G10） |
| `WeixinInboundMapper.java` (**81**) | iLink 文本消息 → `InboundMessage`（仅 DIRECT，`selfId` 自过滤） | `weixin/inbound.rs` | ✅ 等价（`channelMessageId` + `weixinContextToken`） |
| `WeixinOutboundClient.java` (**297**) | `sendmessage`/`getupdates`/`notifystart`/`notifystop` | `weixin/outbound.rs` (**712**) | ✅ 等价 |
| `WeixinLoginClient.java` (**174**) | 扫码登录 `start`/`poll`/`verify` + `scaned_but_redirect` 换域名 | `weixin/login.rs` (**470**) | ✅ 等价 |
| `WeixinEndpointPolicy.java` (**85**) | 发送凭据前的域名白名单 | `weixin/endpoint.rs` | ✅ 等价 |
| `WeixinProtocolHeaders.java` (56) | iLink 公共头（`iLink-App-Id`=bot、`iLink-App-ClientVersion`=132105） | `weixin/protocol.rs` | ✅ 等价（`APP_ID`/`APP_CLIENT_VERSION`/`application_headers`/`json_post`/`authenticated_json_post`） |
| `WeixinStateStore.java` (77) + `InMemoryWeixinStateStore.java` (**270**) | 账户级状态契约（cursor / peer context token / inbox claim / 租约围栏）+ 内存实现 | `weixin/state_store.rs` (**836**) | ✅ 等价（`WeixinStateStore` trait + `InMemoryWeixinStateStore`） |
| `WeixinCredentials.java` (34) / `WeixinCredentialProvider.java` (35) | 凭据值与提供者 seam | `weixin/credentials.rs` | ✅ 等价（`WeixinCredentials`/`WeixinCredentialProvider`/`FixedCredentialProvider`） |
| `WeixinRuntimeListener.java` (48) | 运行时事件监听 seam | `weixin/listener.rs` | ✅ 等价（`WeixinRuntimeListener`/`NoopWeixinRuntimeListener`/`RecordingWeixinRuntimeListener`） |
| `WeixinOperationException.java` (27) / `WeixinCredentialRejectedException.java` (24) | 异常类型（凭据拒绝需上报） | `weixin/error.rs`（`WeixinError::{CredentialRejected,Operation,Transport}`） | ✅ 等价 |
| `WeixinInboxClaim.java` (32) / `WeixinInboxMessage.java` (32) / `WeixinLease.java` (32) / `WeixinLoginChallenge.java` (35) / `WeixinLoginSession.java` (39) / `WeixinLoginStep.java` (45) | 值对象 | `weixin/state_store.rs` + `weixin/dto.rs` | ✅ 等价 |
| —（Rust 侧新增） | — | `weixin/channel.rs::WeixinChannelRegistry` = `common::ChannelRegistry<WeixinChannel>` | ℹ️ **Java 侧无 `WeixinChannelRegistry`**（Rust 为对称性新增，非缺口） |
| —（测试） | Java 侧 **2527** 行（非空 2323）/ 9 个测试文件（`@Test` 最多：62 个 + `@ParameterizedTest` 1 个） | Rust **86** 个单测 | ✅ |

### 2.8 依赖映射（Java → Rust）
以下版本均取自本 crate `crates/agentscope-extensions-channel/Cargo.toml` 的 `[dependencies]`（未改根 `Cargo.toml` 的 `[workspace.dependencies]`）。
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Spring `WebClient` / OkHttp | 出站 HTTP | `reqwest`（workspace 版本）+ 自建 `HttpTransport` 薄封装 | `HttpTransport` 是可注入 seam，测试用 `common::testing::RecordingTransport` 离线录制，**不发起真实网络** |
| Jackson `ObjectMapper` | JSON 解析/序列化 | `serde` + `serde_json`（workspace 版本） | DTO 直接以 `serde_json::Value` 解析，保持依赖轻量、可离线测 |
| `DocumentBuilderFactory`（DOM XML） | WeCom 回调 XML 取文本 | `quick-xml = "0.39"` | `common/xml.rs::xml_element_text` 复刻 `getElementsByTagName` + `getTextContent` 语义（含 CDATA、实体解析、首个匹配元素） |
| `javax.crypto.Mac` HmacSHA256 | GitHub 签名校验 | `hmac = "0.12"` + `sha2 = "0.10"` | — |
| `MessageDigest` SHA-1 | WeCom 回调签名 | `sha1 = "0.10"` | — |
| `java.util.Base64` | 签名/密钥编解码 | `base64 = "0.22"` | — |
| `URLEncoder` | 表单编码 | `percent-encoding = "2.3"` | — |
| OkHttp WebSocket / `reactor-netty` | 常驻连接（DingTalk Stream） | `tokio-tungstenite` 0.29（`connect` + `handshake` + `rustls-tls-webpki-roots`） | 生产连接器 `dingtalk/stream.rs::TungsteniteConnector`；协议逻辑与 I/O 边界分离，可离线单测 |
| Spring `@RestController` | 回调 HTTP 入口 | `axum` 0.7（workspace 依赖）+ `http`/`bytes` | **已裁定**：与 protocol/controlplane 统一用 axum；控制器为薄壳，只做参数提取与 `*CallbackOutcome` → HTTP 映射 |
## 3. 缺口清单（G1–G10 已全部关闭；**2026-10-31 全量复核后的现行待办见 §3.5**）

> 历史 P0/P1/P2 的发现、修复与反向验证细节见归档 `CHANNEL__MIGRATION_HISTORY.md`（章节编号沿用原 §6/§7/§8）。

| 编号 | 优先级 | 缺口 | 状态 | 关闭日期 | 关闭记录 |
|---|---|---|---|---|---|
| G1 | P0 | DingTalk HTTP 回调模式（`crypto.rs` + `controller.rs` + `properties.{mode,aesKey,token}`） | 已完成 | 2026-10-22 | history §7 |
| G3 | P1 | `DingTalkStreamClient` 未被 Channel 启动（无 `start()`/`stop()`） | 已完成 | 2026-10-22 | history §7.5 |
| G2 | P1 | 入站媒体契约 `ChannelMediaMetadata`（三平台 mapper 丢媒体） | 已完成 | 2026-10-23 | history §8.1 |
| G4 | P1 | `InboundEventDeduplicator` / `AccessTokenStore` 可插拔 seam | 已完成 | 2026-10-23 | history §8.2 |
| G5 | P1 | Feishu 普通事件 `header.token` 恒定时间闸门 | 已完成 | 2026-10-23 | history §8.5 |
| G6 | P2 | `BotLoopGuard` 空闲 peer 淘汰 | 已完成 | 2026-10-23 | history §8.3 |
| G7 | P2 | 过期文档注释（5 处） | 已完成 | 2026-10-23 | history §8.6 |
| G8 | P2 | `IdempotencyStore` TTL 窗口重启语义 | 已完成 | 2026-10-23 | history §8.4 |
| G9 | P2 | 飞书 `responseTimeout` 不可配（`FeishuAccessTokenProvider` / `FeishuOutboundClient` 构造参数，默认 10 s，构造期校验正数） | 已完成 | 2026-10-26 | 下表 + `72bfef4` |
| G10 | P1 | 个人微信 iLink 消费循环重试链失效 + 退避不升级（`maxBackoffMs` 死配置）：`end_consuming()` 清 `running` → `run_consumer()` 首个任期即退出（永不重试）；`begin_consuming()` 每次把 `backoff_ms` 重置为 1000 → 永远停在 2000 | 已完成 | 2026-10-31 | `weixin/channel.rs`（`end_consuming`/`is_running` doc、`INITIAL_BACKOFF_MS`/`next_backoff`/`escalate_backoff`/`reset_backoff`）+ §3.2 |
| — | — | 骨架/接线/依赖/离线测试覆盖（原 §3.1 四项） | 已完成 | 2026-10-22 | history §4.1 |

### 3.1 G9 关闭记录（2026-10-26）

> Java 基线 `16cb024ae`；该 `responseTimeout` 参数正是此提交引入（**新漂移**）。
> 来源：`docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md` §4.1。

**缺口**：Rust 把 10 s 写死在 `common/http.rs::DEFAULT_TIMEOUT` + `ReqwestTransport::new()`，
两个飞书客户端都没有超时形参，云端慢响应只能改代码。

**修法**：
- `common/http.rs`：`ensure_positive_timeout()`（零值 → `ChannelError::Config("responseTimeout must be positive")`，对齐 Java `IllegalArgumentException`）；`ReqwestTransport` 新增 `timeout: Option<Duration>` + `with_timeout()`，`execute()` 时按请求 `.timeout(t)` 覆盖；`with_client()` 保留外部 client 自身配置（`timeout = None`）
- `HttpTransport` 新增**带默认实现**的 `with_response_timeout()`：默认 `None`（注入的 fake / 外部持有的 client 不可重配），`ReqwestTransport` 覆写为复用底层 client（保留连接池）只换超时
- `feishu/token.rs` / `feishu/outbound.rs`：各加 `DEFAULT_RESPONSE_TIMEOUT = 10s`、`response_timeout` 字段、链式 `with_response_timeout()` 与读取器；**`new()` / `with_store()` 签名不变**

**为何不用 builder / 平行类型**：超时在 Rust 挂在 transport 而非 client 上；用 trait 默认方法可以让「可重配」（`ReqwestTransport`）与「不可重配」（测试双、外部 client）两种情况共用一条调用路径，且不改动既有 trait 注入设计。

**测试**（全离线，不真发请求）：`reqwest_transport_defaults_to_ten_seconds`、`reqwest_transport_accepts_explicit_timeout`、`reqwest_transport_rejects_zero_timeout`、`with_client_defers_to_the_wrapped_client_timeout`、`with_response_timeout`（trait 层）；`outbound` / `token` 各自：默认 10 s、显式配置对可重配 transport 生效、不可重配时仅记录、零值被拒。新增测试双 `common/testing.rs::TimeoutAwareTransport`。

**验证**：`cargo test -p agentscope-extensions-channel` → **399 passed / 0 failed**。

### 3.2 G10 关闭记录（2026-10-31）

**缺口**（两个叠加缺陷，都在同一条「重试链」上）：
1. **重试链形同虚设**：`end_consuming()` 在释放租约时把 `Runtime.running` 一并清零，而 `run_consumer()` 每轮都调它，因此**第一个租约任期结束即退出**（瞬时失败、租约丢失都不会重试），退避代码永不可达；
2. **退避不升级**：`begin_consuming()` 每次把 `runtime.backoff_ms` 重置为 `1_000`，失败退避只在 `run_consumer()` 里加倍一次，因每次失败都会重新获取租约，退避永远停在 `2000` 一档，配置项 `maxBackoffMs` 实际无效。

**Java 基线**（`WeixinChannel.java`）：`running` 是**通道级**开关（只由停止/凭据拒绝清零）；`private volatile long backoff = 1000`；`catch` 分支 `sleep(backoff); backoff = Math.min(p.maxBackoffMs(), backoff * 2)`；只有 `polled && drainInbox` 成功后才 `backoff = 1000`。

**修法**：
- `end_consuming()` 只清 `active`，不再改 `running`（并在 doc 里写清「running ≠ 当前有租约」）；`is_running()` 补同口径 doc；
- 新增 `pub const INITIAL_BACKOFF_MS: i64 = 1_000` 与 `pub fn next_backoff(current, max)`（`saturating_mul(2).min(max)`，防溢出后 `sleep(负数)`）；
- `escalate_backoff()` / `reset_backoff()` 两个私有方法，`run_consumer()` 失败后调前者，`consume()` 在 poll+drain 都成功后调后者；
- `begin_consuming()` 不再改 `backoff_ms`；模块头注释补充该差异点。

**测试**（全离线，无真实网络）：`weixin::channel::tests::the_consumer_reacquires_the_lease_after_a_transient_failure`（**回退 `end_consuming` 的 `running = false` 即 FAILED**：`run_consumer` 首次任期即退出，只看到 3 个请求）、`consumer_backoff_escalates_and_survives_a_reacquired_lease`（**回退 `begin_consuming` 的重置行即 FAILED**）、`a_healthy_poll_and_drain_resets_the_consumer_backoff`（notifystart + 一次健康 getupdates + 一次 `ret:-1`，断言回到 `INITIAL_BACKOFF_MS`）、`next_backoff_doubles_and_stops_at_the_cap`。

> 本轮的验证由主智能体统一执行（子智能体不跑 cargo），因此 §7 仍以主智能体实测为准；本轮新增测试共 **6** 个（weixin 4 + feishu token 1 + wecom token 1）。

新条目一律追加在表尾（编号接 G11…），状态先置 `待办`，关闭后补日期与历史引用。

已定性、不属遗留工作的事项：
- **A2A / 长轮询等拉取式入站**：Java 侧无对应类 → 设计决定（不迁移）；weixin 的 iLink 长轮询已迁移在 `weixin/channel.rs`。
- **真实平台联调**：见 §5（需租户环境，非代码缺口）。

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`（`agentscope-extensions-channel` 7 子模块：common / dingtalk / feishu / github / gitlab / wecom / weixin，主代码 92 文件）；Rust 工作树 `crates/agentscope-extensions-channel`（71 个 `.rs`）。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单（G1–G10）保持原样未重新编号**。
> 编号规则：`CHANNEL-NN`（缺口/复核结论，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；差异另有 `CHANNEL-DNN`（§4.1 设计决定不迁移）/ `CHANNEL-XNN`（§4.2 有意差异）系列。编号本模块内唯一、单调递增，不复用历史 `G<n>`。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 核对面：消息模型（text / image / audio / video / file / folder / media）· 签名与解密算法（HMAC-SHA256 / SHA-256 / SHA-1 + AES-256-CBC）· 回调路由与幂等 · token 获取与 80% TTL 刷新 · 长连接（DingTalk Stream ACK 时序）· 发送 API 字段 · 错误码分类 · 限流/退避重试。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| CHANNEL-N01 | P0 | **GitHub/GitLab 机器人身份从未解析 → 自评论回环防护在生产路径上恒为关闭**：`is_self()` 要求 `bot_user_id > 0`，而未调用 `refresh()` 时恒为 `-1`；只有 `botUserLogin` 覆盖也只是 `(-1, login)`，同样不生效。后果：机器人回复自己的评论会再次触发 webhook，仅靠 `BotLoopGuard`（20 条/60 s + 60 s 冷却）节流，形成长期刷屏循环 | `github/GitHubChannel.java#start`（`botIdentityResolver.refresh()`，L151）、`gitlab/GitLabChannel.java#start`（L147-149）；`GitHubBotIdentityResolver.java#refresh`（GET `/user`） | `src/github/channel.rs:113` 仅构造 `GitHubBotIdentityResolver`；`src/github/bot_identity.rs:149 refresh` / `:182 is_self`；`rg -n '\.refresh\(' src` **生产代码零命中**，仅 `bot_identity.rs:171,185` 与 `gitlab/bot_identity.rs:171,185` 的测试调用（另核 `try_refresh` 亦仅 `github/bot_identity.rs:268,282` 测试） | 已完成 | ①在两平台通道增加启动钩子（`start()` 或在 `from_json_with` 后由宿主调用）；②`refresh()` 失败保持“未知”并 warn（已是 best-effort，勿改成启动失败）；③验收：新增测试断言「构造后未解析 → `is_self` false；`start()`/`refresh()` 后自评论被丢弃」，并做反向验证（去掉调用 → FAILED） 【2026-10-31 落地：新增 `GitHubChannel::start` / `GitLabChannel::start`，内部调 `bot_identity.refresh()`，失败只 `warn` 并保持未知（`github/channel.rs::start`、`gitlab/channel.rs::start`）；测试 `github::channel::tests::start_resolves_the_bot_identity_so_self_comments_are_dropped`、`gitlab::channel::tests::start_resolves_the_bot_identity_so_self_notes_are_dropped`、`a_failed_start_keeps_the_bot_identity_unknown`（两平台各一）；修复前无启动钩子、自评论恒被受理 → FAILED】 |
| CHANNEL-N02 | P0 | **Feishu 消息在 dispatch 失败时被永久丢弃**：Rust 先按 `header.event_id` 占用幂等键（`ChannelCore::admit`）再 dispatch，且把 dispatch 错误整体 `let _ =` 吞掉并回 200；Java 明确不在入口去重（“Durable intake owns deduplication”，避免“标记已见但未持久化”），并对 dispatch 失败回 **503** 让飞书重投。叠加后 Rust 在网关/agent 瞬时失败时既无重投也无错误信号 | `feishu/FeishuCallbackController.java#callback`（去重注释 + `onErrorResume` → `HttpStatus.SERVICE_UNAVAILABLE`） | `src/feishu/channel.rs:236-243`（`self.core.admit(event_id…)` → `let _ = PlatformChannel::dispatch(...)` → `Accepted`）；`FeishuCallbackOutcome` 无 `ServiceUnavailable` 变体（`src/feishu/channel.rs:43-52`），`src/feishu/controller.rs:66-75` 无 503 映射 | 已完成 | ①新增 `FeishuCallbackOutcome::Unavailable` → `503`；②dispatch 失败时不吞错，且**先 dispatch 成功再落幂等键**（或保留“入口不去重”，把去重交给持久化入口）；③验收：`dispatch` 返回 Err 时控制器回 503 且同一 `event_id` 二次投递仍被受理 【2026-10-31 落地：`FeishuCallbackOutcome::Unavailable` → 503（`feishu/controller.rs`）；`handle_callback` 改为「先 peek 去重（`ChannelCore::is_duplicate`，新增 `InboundEventDeduplicator::contains` 默认方法）→ dispatch → 成功后才 `ChannelCore::record` 落幂等键」，失败不再吞错而回 503；测试 `feishu::channel::tests::a_failed_dispatch_is_unavailable_and_stays_redeliverable`、`feishu::controller::tests::a_dispatch_failure_answers_503_so_feishu_redelivers`；修复前 dispatch Err 仍 200 且首次投递即占用 event_id → FAILED】 |
| CHANNEL-N03 | P0 | **WeCom 出站把被拒绝的发送当成功**：`handle_send_response` 只对 `errcode` 42001/40014 做 token 失效，**不对非 0 errcode 返回错误**，也不校验空 body / 缺 `errcode`；Java `checkSendResponse` 对 `errcode != 0` 抛 `IllegalStateException`、对空 body / 缺 `errcode` 抛错。后果：消息被企业微信拒绝（如 81013 用户不存在、45009 接口超限）时返回 `Ok(())`，上层认为已投递，静默丢消息 | `wecom/WeComOutboundClient.java#checkSendResponse`（`errcode != 0` → throw；`defaultIfEmpty("")`；`MAX_SNIPPET_LENGTH` 截断） | `src/wecom/outbound.rs:80-90`（`send()` 只 `handle_send_response(&response.body)` 后 `Ok(())`）；`src/wecom/outbound.rs:94-105`（`handle_send_response` 用 `unwrap_or(0)`，解析失败直接 return） | 已完成 | ①`handle_send_response` 改返回 `Result<()>`：body 非 JSON / 缺 `errcode` / `errcode != 0` → `ChannelError::Http`（携带截断后的 body，≤200 字符）；②保留 42001/40014 失效后仍报错；③验收：新增 `errcode=45009` → Err、空 body → Err 的测试，并反向验证（恢复旧实现 → FAILED） 【2026-10-31 落地：`WeComOutboundClient::handle_send_response` 改 `Result<()>`，非 JSON / 缺 `errcode` / `errcode != 0` → `ChannelError::Http`（body 经 `error_snippet` 截断至 `MAX_ERROR_SNIPPET_CHARS = 200` 字符），42001/40014 先失效 token 再报错，`send()` 用 `?` 透传（`wecom/outbound.rs`）；测试 `a_rejected_send_is_an_error`（45009）、`an_empty_unparsable_or_errcode_less_body_is_an_error`、`an_error_body_is_truncated_to_a_bounded_snippet`、`send_propagates_a_rejection`、`auth_errors_invalidate_the_token_and_still_report_a_failure`】 |
| CHANNEL-N04 | P1 | **GitHub / GitLab / DingTalk 出站不校验 HTTP 状态码**：`HttpTransport::execute` 一律返回 `PreparedResponse`（含 4xx/5xx），三个客户端 `send()` 直接 `Ok(())`，401（坏 token）/403/404（仓库或 issue 不存在）/422（内容被拒）/429 都被当作投递成功。Java 走 `WebClient.retrieve()`，非 2xx 直接进 `onError` | `github/GitHubOutboundClient.java#sendOne`（`retrieve()`，15 s 超时）、`gitlab/GitLabOutboundClient.java#sendOne`、`dingtalk/DingTalkOutboundClient.java#post`（`retrieve()` + 10 s 超时 + token 失效） | `src/github/outbound.rs:111-119`、`src/gitlab/outbound.rs:112-120`（`self.transport.execute(request).await?` 后不查 `is_success()`）；`src/dingtalk/outbound.rs:110-122`（仅 `!is_success()` 时 `invalidate()`，仍返回 `Ok`）；`PreparedResponse::is_success` 已存在于 `src/common/http.rs` 但未被上述三处使用 | 待办 | ①三处 `send()` 在 `execute` 后加 `if !response.is_success() { return Err(ChannelError::Http(...)) }`（GitHub/GitLab 同时保留 `x-ratelimit-remaining` 诊断日志）；②DingTalk 保留失效逻辑后再报错；③验收：`RecordingTransport` 返回 401/422 时 `send()` 为 Err，反向验证 |
| CHANNEL-N05 | P1 | **回执式投递（`deliverWithReceipt`）在 Rust 抽象层缺失**：Java `Channel` 接口有 `Mono<String> deliverWithReceipt(address, msg, deliveryId)`，飞书（`uuid` 去重 + 返回 `message_id`）与微信（返回 provider 回执）各自实现，供宿主持久化投递 worker 使用；Rust `PlatformChannel`/harness `Channel`/`ChannelSink` 都没有该方法，`FeishuOutboundClient::send_with_receipt` 无从通过通道调用（`WeixinChannel::deliver_with_receipt` 仅为具体类型固有方法） | `feishu/FeishuChannel.java#deliverWithReceipt`、`feishu/FeishuOutboundClient.java#sendWithReceipt`、`weixin/WeixinChannel.java#deliverWithReceipt` | `crates/agentscope-harness/src/agent/gateway/channel/channel.rs:22-44`（`Channel` 只有 channel_id/config/init/dispatch/apply_routing_config）；`crates/.../platform_channel.rs:165-217`（仅 `ctx()`/`deliver_reply()`/`dispatch`）；`rg 'deliver_with_receipt' crates` 仅命中 `src/weixin/channel.rs:754`（另核 `deliverWithReceipt` 全仓零命中）；`FeishuChannel` 无该方法（`src/feishu/channel.rs:63-260`） | 待办 | 需先裁定抽象归属：**A**=给 harness `Channel`/`ChannelSink` 增默认 `deliver_with_receipt`（默认 `Err(Unsupported)`，飞书/微信覆写，需 harness 授权）；**B**=在 channel crate 用具体类型方法 + 宿主 `match`。裁定后补 `FeishuChannel::deliver_with_receipt`（`uuid`+回执）与 trait 接线测试 【裁定 2026-10-31：A：harness Channel/ChannelSink 加默认 deliver_with_receipt（默认 Err(Unsupported)），飞书/微信覆写】 【阻塞（本轮跳过）：等待 core/harness 批次 1/2 的 `Channel`/`ChannelSink::deliver_with_receipt`（默认 `Err(Unsupported)`）；本 crate 侧 `FeishuChannel::deliver_with_receipt` 待该 trait 方法落地后再接线，未自造替代品】 |
| CHANNEL-N06 | P2 | **webhook 验签基于有损 UTF-8 字符串而非原始字节**：控制器用 `String::from_utf8_lossy(&body)` 后按 `&str` 的字节算 HMAC；非 UTF-8 字节被替换为 U+FFFD，签名与被签名原文不一致 → 401，且 JSON 解析内容与 Java 不同。Java 以 `byte[]`/`String`（Spring 严格解码）验签原始请求体 | `github/GitHubWebhookController.java#webhook`（`@RequestBody byte[] rawBody`，HMAC over 原始字节）、`dingtalk/DingTalkCallbackController.java#dispatch`（`String body` 原样参与 HMAC） | `src/github/controller.rs:44`、`src/gitlab/controller.rs:43`、`src/dingtalk/controller.rs:143`、`src/feishu/controller.rs:45`、`src/wecom/controller.rs:77` 均 `String::from_utf8_lossy`；`src/github/channel.rs:161` 对 `raw_body.as_bytes()` 验签 | 已完成 | ①控制器改为传 `&[u8]`/`Bytes`，验签用原始字节（GitHub/GitLab），XML/JSON 解析再各自解码；②或至少用 `std::str::from_utf8` 严格解码并在非 UTF-8 时回 400（而不是验签失败）；③验收：含非 UTF-8 字节的 body 验签结果与 Java 一致 【2026-10-31 落地：采用方案②——五个控制器（github/gitlab/feishu/wecom/dingtalk）改为 `std::str::from_utf8` 严格解码，非 UTF-8 直接 `BadRequest`（400），不再用 `from_utf8_lossy` 让 U+FFFD 参与验签；测试 `github::controller::tests::a_non_utf8_body_is_a_bad_request_not_a_signature_failure`、`gitlab::controller::tests::a_non_utf8_body_is_a_bad_request`、`feishu::controller::tests::a_non_utf8_body_is_a_bad_request`、`wecom::controller::tests::dispatch_rejects_a_non_utf8_body`、`dingtalk::controller::tests::a_non_utf8_body_is_a_bad_request`；修复前分别回 401/到达 channel → FAILED】 |
| CHANNEL-N07 | P2 | **未处理的 webhook 事件返回码不一致**：Java 对 GitHub 非 `issue_comment`/`pull_request_review_comment`、GitLab 非 `Note Hook` 返回 **204 No Content**；Rust 统一返回 200 `{}`（`Accepted`），语义上把“未处理”和“已处理”混同，宿主/监控无法区分 | `github/GitHubWebhookController.java#webhook`（`ResponseEntity.noContent()`）、`gitlab/GitLabWebhookController.java#webhook`（`noContent()`） | `src/github/controller.rs:70-75`（全部 `Accepted` → `Json({})`）；`src/gitlab/controller.rs` 同理；`GitHubCallbackOutcome`/`GitLabCallbackOutcome` 无 `Ignored` 变体 | 已完成 | 增 `Ignored` 变体 → `204`，仅在“事件类型不支持”时使用；验收：`push` 事件返回 204，`issue_comment` 仍 200 【2026-10-31 落地：`GitHubCallbackOutcome::Ignored` / `GitLabCallbackOutcome::Ignored` → `204 No Content`，仅在事件类型不支持时使用（GitHub 新增 `SUPPORTED_EVENT_TYPES` 门、GitLab 非 `Note Hook` 门），`push` 等事件不再回 200（`github/channel.rs`、`gitlab/channel.rs`、两控制器 `into_response`）；测试 `github::channel::tests::ignores_unsupported_events`、`gitlab::channel::tests::ignores_non_note_hook_events`、`github::controller::tests::an_unhandled_event_type_is_204`、`gitlab::controller::tests::other_hook_types_are_acknowledged`】 |
| CHANNEL-N08 | P2 | **平台 `TYPE` 常量缺失**：Java 六个通道类均有 `public static final String TYPE`（`"dingtalk"`/`"feishu"`/`"github"`/`"gitlab"`/`"wecom"`/`"weixin"`），是配置分发（`agentscope.json#channels[*].type`）与诊断的唯一标识来源；Rust 只有 `weixin` 导出 `CHANNEL_TYPE` | `dingtalk/DingTalkChannel.java:58`、`feishu/FeishuChannel.java`、`github/GitHubChannel.java`、`gitlab/GitLabChannel.java`、`wecom/WeComChannel.java`、`weixin/WeixinChannel.java` | `rg 'CHANNEL_TYPE' 与 'pub const TYPE' src` 均仅命中 `src/weixin/channel.rs:66`；其余五个平台零命中（另核 `pub const CHANNEL_ID`/`const TYPE` 亦零命中） | 已完成 | 五个平台模块各加 `pub const CHANNEL_TYPE: &str`，并在 `mod.rs` re-export；验收：编译期常量断言测试 【2026-10-31 落地：`dingtalk`/`feishu`/`github`/`gitlab`/`wecom` 各新增 `pub const CHANNEL_TYPE: &str` 并在各自 `mod.rs` re-export（`weixin` 原有）；`lib.rs` 增 `const _: () = {...}` 编译期断言（非空 + 两两互不相等）与测试 `channel_type_tests::channel_type_constants_match_the_java_type_fields`】 |
| CHANNEL-N09 | P1 | **HTTP 模式通道无启动钩子、靠宿主显式 `register()`（否则回调一律 401/404）**：Java `start()` 把实例注册进 `*ChannelRegistry`，`stop()` 反注册；Rust 的 `ChannelRegistry::register` 在本 crate 生产代码中零调用（仅测试），`DingTalkChannel::start()`（http 模式）只校验 crypto 不注册，Feishu/GitHub/GitLab/WeCom 连 `start()/stop()` 都没有 → 宿主漏调 register 时 webhook 全部失败，且无编译期提示 | `dingtalk/DingTalkChannel.java#start/#stop`、`feishu/FeishuChannel.java#start`、`wecom/WeComChannel.java#start`（均 `registry.register(this)`） | `src/common/channel_registry.rs:85-100`（`register` 显式）；`rg -n '\.register\(' src` 仅 `github/controller.rs:106`、`feishu/controller.rs:107`、`gitlab/controller.rs:89`、`wecom/controller.rs:158`、`dingtalk/controller.rs:274,561`（**全部在 `#[cfg(test)]` 内**）；`src/dingtalk/channel.rs:181-193`（http 模式仅校验 crypto） | 已完成（本 crate 侧） | 裁定归属：**A**=在 channel crate 加 `GatewayBootstrap`/`build_registry(…)->(Router, Registry)` 装配入口（宿主一行接入）；**B**=保持“宿主自装配”但在各平台补 `start()/stop()` 文档契约。裁定后补一条「走真实装配入口」的测试（防 §1.2 记载的“测试绿但装配漏接线”复发） 【裁定 2026-10-31：A：channel crate 提供 build_gateway_registry 单入口 + 真实装配测试】 【2026-10-31 落地（仅本 crate 侧，未改 harness）：新增 `src/gateway.rs::build_gateway_registry(channels, Option<Arc<ChannelManager>>, build_router) -> (Router, Arc<ChannelRegistry<C>>)`，注册后把**同一个** `Arc<ChannelRegistry<C>>` 交给 router builder；测试 `gateway::tests::build_gateway_registry_registers_the_channels_the_router_reads`（全程不显式 `register`，断言 `Arc::ptr_eq` 且注册句柄可用）、`build_gateway_registry_can_mirror_into_the_channel_manager`】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| CHANNEL-N10 | `BotLoopGuard`：20 事件/60 s 窗口、超限后 60 s 冷却、空闲 peer 按 `window+cooldown` 淘汰、无后台线程 | `common/BotLoopGuard.java#allow/#maybeSweep` | `src/common/bot_loop_guard.rs:97 allow_at`、`:27 BotLoopGuardConfig::default`、`evict_idle_peers`/`tracked_peers`；测试 `bot_loop_guard.rs::tests::*`（含种子时钟） | 已完成 |
| CHANNEL-N11 | `IdempotencyStore`：默认 5 min/10k，满容量先清过期再按插入序淘汰，过期后“仅 CAS 胜者重开窗口” | `common/IdempotencyStore.java#firstSeen/#sweep` | `src/common/idempotency_store.rs:36 DEFAULT_TTL_MS`、`:112 first_seen_at`（`Entry{seen_at_ms,seq}` + `order`）+ `sweep`；测试 `idempotency_store.rs::tests::*` | 已完成 |
| CHANNEL-N12 | token 缓存：`putIfNewer` 严格比较 `refreshAtMs`、80% TTL（`expiresIn*800`）、三平台 provider 可注入 `AccessTokenStore` | `common/AccessTokenStore.java#putIfNewer`、`common/InMemoryAccessTokenStore.java`、各 `*AccessTokenProvider#refresh` | `src/common/token.rs:25 refresh_at_ms`、`:73 put_if_newer`（默认实现）、`:94 Mutex` 原子合并；`DingTalk/Feishu/WeCom` 均有 `with_store(...)`（`dingtalk/token.rs:40`、`feishu/token.rs:60`、`wecom/token.rs`） | 已完成 |
| CHANNEL-N13 | DingTalk 回调验签/解密：`HMAC-SHA256(appSecret, ts+"\n"+appSecret)`、时间戳 ≤1 h 且未来偏差 ≤5 min、`aesKey` 43 字符 + `"="` 解码 32 字节、`16B ‖ 4B len ‖ msg ‖ trailer`（明文布局，竖线仅作分隔记号）、PKCS#7 按 **32 字节**块严格校验 | `dingtalk/DingTalkCallbackCrypto.java#verify/#decrypt` | `src/dingtalk/crypto.rs:52-53 MAX_TIMESTAMP_AGE_MS/MAX_FUTURE_TIMESTAMP_MS`、`JAVA_BASE64`（容忍尾部非零位）；金标向量测试 `hmac_signature_matches_the_java_gold_vector`、`decrypts_the_java_gold_ciphertext` | 已完成 |
| CHANNEL-N14 | DingTalk Stream：注册体 `clientId/clientSecret/ua/subscriptions[CALLBACK,/v1.0/im/bot/messages/get]`、**先 ACK 再 dispatch**、1 s→60 s 指数退避并在连上后复位 | `dingtalk/DingTalkStreamClient.java#openConnection/#handleFrame/#ackFrame/#scheduleReconnect` | `src/dingtalk/stream.rs:61`（注册体 `ua`，整体 57-66）、`:346-352`（先 ACK 后派发）、`:137 next_backoff`、`:319`（连上复位）；测试 `callback_is_acked_before_the_payload_is_dispatched`、`backoff_doubles_up_to_the_cap` | 已完成 |
| CHANNEL-N15 | DingTalk 配置：`appKey/appSecret/robotCode` 必填、`mode` ∈ {stream,http} 缺省 stream、`aesKey` 43 字符、`apiBase/oapiBase/streamRegisterUrl` 默认值、`token` 仅 Rust 扩展 | `dingtalk/DingTalkChannelProperties.java`（紧凑构造器） | `src/dingtalk/properties.rs:94-124`（默认值与校验逐条对应，错误文案对齐）；测试 `properties.rs::tests::*` | 已完成 |
| CHANNEL-N16 | Feishu 加解密：AES key = `SHA-256(encryptKey)`、`16B IV ‖ CBC+PKCS#7`、签名 `SHA-256(ts+nonce+key+rawBody)` 常量时间比较、仅“已加密且带签名头”时强制校验 | `feishu/FeishuCrypto.java#verifySignature/#decrypt`、`feishu/FeishuCallbackController.java`（L1 步） | `src/feishu/crypto.rs`（`derive`/`verify_signature`/`decrypt`/`sign`）；`src/feishu/channel.rs:165-175`（校验条件）；测试 `encrypted_channel_rejects_a_signature_mismatch`、`encrypted_channel_accepts_a_valid_signature` | 已完成 |
| CHANNEL-N17 | Feishu 出站：`receive_id_type=chat_id`、threaded reply 走 `/messages/{root_id}/reply` + `reply_in_thread`、`uuid=deliveryId`、`code 99991663/99991664` 失效 token、`code != 0` 报错 | `feishu/FeishuOutboundClient.java#sendOneReceipt/#checkSendResponse` | `src/feishu/outbound.rs:65-100`（uuid / reply_in_thread / receive_id）、`:136-170`（`parse_send_response` 失效并报错）；测试 `send_with_receipt_returns_message_id`、`parse_send_response_extracts_message_id_and_invalidates_on_auth_error` | 已完成 |
| CHANNEL-N18 | GitHub：`X-Hub-Signature-256`（`sha256=` 前缀 + 常量时间）、`created` 限定、`issue_comment`/`pull_request_review_comment` 分支、`owner/repo#n` THREAD peer、自评论丢弃、按 `comment.id` 去重 | `github/GitHubSignatureVerifier.java#verify`、`github/GitHubInboundMapper.java#map`、`github/GitHubWebhookController.java` | `src/github/signature.rs`、`src/github/inbound.rs`、`src/github/channel.rs:156-196`；测试 `rejects_a_bad_signature`、`drops_self_authored_comments`、`admits_a_signed_issue_comment_once` | 已完成 |
| CHANNEL-N19 | GitLab：`X-Gitlab-Token` 常量时间、仅 `Note Hook`、project path 按 Java `URLEncoder` 规则 percent-encode（`grp%2Fproj`）、`issues`/`merge_requests` 分支、`note.id` 去重 | `gitlab/GitLabWebhookController.java#webhook`、`gitlab/GitLabOutboundClient.java#sendOne` | `src/gitlab/signature.rs:verify_webhook_token`、`src/gitlab/outbound.rs:88-110`；测试 `builds_issue_note_request_with_encoded_project_id`、`a_valid_note_hook_is_accepted` | 已完成 |
| CHANNEL-N20 | WeCom：`SHA-1(sort(token,ts,nonce,encrypt))` 常量时间、AES-256-CBC + 尾部 **receive_id 必须等于 corpId**、GET 握手解密 `echostr` 明文回显、POST 缺 `<Encrypt>`→400 | `wecom/WeComCrypto.java#verifySignature/#decrypt`、`wecom/WeComCallbackController.java#verify/#dispatch` | `src/wecom/signature.rs`、`src/wecom/crypto.rs`（receive_id 校验）、`src/wecom/channel.rs:162-186 verify_url`、`src/wecom/controller.rs:36-60`；测试 `verify_echoes_the_decrypted_echostr_as_plain_text` 等 | 已完成 |
| CHANNEL-N21 | weixin 租约与投递：`is_lease_current` 围栏（取/续/写 context token/落库前各检一次）、`MAX_DISPATCH_ATTEMPTS=3` 后 `abandon_message`、`-14` 视为凭据拒绝并停消费 | `weixin/WeixinChannel.java#consume/#drainInbox/#requireLease`、`InMemoryWeixinStateStore.java` | `src/weixin/channel.rs:611-620`（重试预算 + abandon）、`:377-390`（notify_stop 前检查 lease）、`src/weixin/state_store.rs:110-175`；测试 `drain_inbox_abandons_a_message_after_the_retry_budget`、`stale_lease_cannot_write` 类 | 已完成 |
| CHANNEL-N22 | weixin 配置 10 字段默认值逐一一致（`longPollTimeoutMs=35000`/`requestTimeoutMs=45000`/`maxBackoffMs=60000`/`leaseMs=90000`/`dispatchTimeoutMs=120000`/baseUrl 归一化/`accountId` 回落 channelId） | `weixin/WeixinChannelProperties.java`（紧凑构造器 + `from`） | `src/weixin/properties.rs:49-100`（`new` 内 `positive_or` 默认值表，`from_json` 见 `:93`）；测试 `properties.rs::tests::*` | 已完成 |

## 4. 设计决定与已知差异

### 4.1 设计决定（不迁移）

> 编号 `CHANNEL-DNN`：Java 有、Rust **有意不迁**（JVM 专属 / 与 Rust 生态重复 / 已被更优机制替代）。所有条目均给出源码级依据。

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| CHANNEL-D01 | Spring AutoConfiguration / `@ConfigurationProperties` 绑定 | 各子模块 Spring 配置类（构造器即校验，由容器绑定 `agentscope.json`） | Rust 无 Spring 容器与 DI；改为显式解析函数：`src/dingtalk/properties.rs::DingTalkChannelProperties::from_json`、`feishu` / `wecom` / `weixin` 的 `properties::from_json`、`github` / `gitlab` 的 `properties::from_json`（`rg '@ConfigurationProperties' crates` 零命中，另核 `AutoConfiguration` 亦零命中） | 宿主须显式构造通道并 `register`（见 CHANNEL-N09），没有容器自动装配 |
| CHANNEL-D02 | 进程级 `*ChannelRegistry` 单例（`instance()`） | `DingTalk/Feishu/GitHub/GitLab/WeComChannelRegistry.java`（各含 `static instance()`） | 收敛为泛型 `common::ChannelRegistry<C>`（`src/common/channel_registry.rs`），实例作用域、可注入控制器，便于测试隔离；Rust 另加 Java 没有的 `WeixinChannelRegistry`（`src/weixin/channel.rs` 内 `type` 别名） | 装配点从“静态单例”改为“构造时注入”（与 CHANNEL-N09 同源） |
| CHANNEL-D03 | A2A / 长轮询等拉取式入站抽象 | —（Java 侧无对应类） | 无基线可迁；weixin 的 iLink 长轮询按 `WeixinChannel` 语义直接落在 `src/weixin/channel.rs::run_consumer/poll_once` | 无 |
| CHANNEL-D04 | 配置键名 camelCase 转 snake_case 的映射层 | 各 `*ChannelProperties.java` 直接读 camelCase 键 | 保留 Java 键名（`appKey`/`encodingAesKey`/`longPollTimeoutMs`…），降低下游 `agentscope.json` 迁移成本；Rust 侧仅结构体字段用 snake_case，读取键名不变（`src/common/props.rs::get_str/get_int`） | 配置文件可直接复用 |
| CHANNEL-D05 | JVM 加密原语 `javax.crypto.Mac/Cipher`、`MessageDigest`、`java.util.Base64`、`MessageDigest.isEqual` | `DingTalkCallbackCrypto`/`FeishuCrypto`/`WeComCrypto`/`GitHubSignatureVerifier` | 替换为 RustCrypto + `base64`：`hmac`0.12 + `sha2`0.10（`src/dingtalk/crypto.rs`、`src/github/signature.rs`）、`sha1`0.10（`src/wecom/signature.rs`）、`aes`+`cbc`（`src/common/crypto.rs::aes_256_cbc_decrypt`）、`base64`0.22；`rg 'javax.crypto' crates` 与 `rg 'MessageDigest' crates` 均零命中 | 算法字节级等价（金标向量已覆盖）；仅 Java `Base64.getDecoder()` 的“忽略末位非零 bit”需显式开启（`with_decode_allow_trailing_bits(true)`，见 `dingtalk/crypto.rs::JAVA_BASE64`） |
| CHANNEL-D06 | Jackson `ObjectMapper` + `DocumentBuilderFactory`(DOM, XXE 加固) | 各 inbound mapper / controller（`WeComInboundMapper`、`WeComCallbackController`） | `serde_json` 直接解析为 `Value`（保依赖轻量、可离线测）；XML 用 `quick-xml` 0.39（`src/common/xml.rs::xml_element_text` 复刻 `getElementsByTagName` + `getTextContent`，含 CDATA/实体/首个匹配）；`rg 'DocumentBuilderFactory' crates` 零命中 | XXE 加固由 quick-xml 默认不解析外部实体承担（无需 Java 那组 feature 开关） |
| CHANNEL-D07 | Spring `@RestController` / `@RequestMapping` / `@RequestBody` | 五个 `*CallbackController.java` | 统一用 axum 0.7 薄壳（`src/*/controller.rs::router`），与 protocol/controlplane 一致；控制器只做参数提取 + `*CallbackOutcome → StatusCode` 映射 | 路由需宿主 `Router::merge` 挂载（Java 由组件扫描自动注册） |
| CHANNEL-D08 | Reactor `Mono/Flux`（`concatMap` 顺序、`timeout`、`onErrorResume`） | 各 `*OutboundClient` / 控制器返回 `Mono` | 改 `async/await`：`for` 循环顺序发送（等价 `concatMap`）、`tokio::time::timeout`、`match` 展平错误；`rg 'reactor' crates/agentscope-extensions-channel` 与 `rg 'Mono<' …` 均零命中 | 无背压概念；单条消息顺序投递语义保持 |

### 4.2 设计决定与已知差异

> 编号 `CHANNEL-XNN`：Rust **已实现但与 Java 行为不同**（架构差异 / 有意优化 / 修复）。下表不含缺口——缺口见 §3.5.1。

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| CHANNEL-X01 | seam 注入形式 | `from_json(...)` + `from_json_with(..., &ChannelSeams)`（`ChannelSeams` 同时携带 `deduplicator` + `token_store`） | Java 是 `fromProperties(..., InboundEventDeduplicator[, AccessTokenStore])` 三档重载 | 避免每平台 3 个几乎相同的函数；两字段均可选，语义等价 |
| CHANNEL-X02 | token 缓存类型 | `common::InMemoryAccessTokenStore`（实现 `AccessTokenStore` trait） | Java 同名类；Rust 早前另有 `TokenCache` 已被取代 | 接口化后不需要独立缓存类型；本 crate 无下游依赖方 |
| CHANNEL-X03 | weixin 工厂 | `WeixinChannel::create(..., &ChannelSeams)` / `from_json_with` | Java 有 weixin 子模块（HEAD 主 20 类 / 1955 非空行，测 9 文件 / 2323 非空行） | **2026-10-26 复评更正**：曾记「本机 checkout 已无 weixin 子模块」**不成立**（`git cat-file -e HEAD:.../channel-weixin` 成功）；子模块由 `63a6f6168`（2026-09-23）引入并一直在基线内；§2.7 快照数字与实测一致 |
| CHANNEL-X04 | `ChannelMediaMetadata` 文件名净化 | 显式 Cf 区间枚举（`src/common/media.rs::sanitize_marker_file_name`） | Java 用正则 `\p{Cf}` | Rust `std` 无 Unicode general-category API，不为一个字符类新增依赖；覆盖范围对“零宽/格式字符”等价 |
| CHANNEL-X05 | DingTalk 回调签名 | 2026 方案 `HMAC-SHA256(appSecret, ts+"\n"+appSecret)` + **可选**经典 `SHA1(sort(token,ts,nonce,encrypt))`（配置 `token` 时才启用） | Java 只有 appSecret HMAC 方案（无 `token` 属性） | 默认不配置 `token` 即完全等同 Java；保留老租户兼容路径。差异仅在显式配置后出现 |
| CHANNEL-X06 | DingTalk `check_url` 首次验证 | 返回加密的 `{"msgtype":"text","text":{"content":"success"}}`（`src/dingtalk/controller.rs::CHECK_URL_SUCCESS`） | Java 返回空 200 | 依据 DingTalk 事件订阅协议（部分租户要求 success 应答）；**尚未在真实租户验证**，若与租户版本冲突需回退 |
| CHANNEL-X07 | weixin 失败后租约处理 | `consume()` 结束即 `end_consuming()` 释放租约，外层 `begin_consuming()` 重取；退避值跨租约保留（`INITIAL_BACKOFF_MS`/`next_backoff`） | Java 在同一租约内原地 `sleep(backoff)`，只保留租约 | 两者语义等价（重试 + 指数退避）；Rust 释放租约让其它实例可接管，重取时重新审计 `notifystart`；退避只由健康 poll+drain 复位（§3.2 G10） |
| CHANNEL-X08 | 分发/投递骨架归属 | 每平台只实现 `PlatformChannel::ctx()` + `deliver_reply()`，路由/会话绑定/reply 投递由 harness `PlatformCtx` + blanket impl 提供（`crates/agentscope-harness/src/agent/gateway/platform_channel.rs:165-217`） | Java 每个 `*Channel` 重复实现 `channelId/config/init/dispatch/deliver/sendReply`（同一段 routing 代码 6 份） | 消除重复、统一热切换（`apply_routing_config`）；新增平台只写两方法。**代价**：`deliverWithReceipt` 未随之进入抽象（缺口 CHANNEL-N05） |
| CHANNEL-X09 | 生命周期与注册 | 无 `start()/stop()`（HTTP 模式），`register()` 由宿主显式调用；`ChannelRegistry` 可选镜像到 harness `ChannelManager` | Java `start()` 自注册 / `stop()` 反注册 | 实例作用域更易测（不污染进程单例）；镜像注册让 proactive delivery 复用同一 `deliver_reply`。**风险**：漏注册即回调失败（CHANNEL-N09） |
| CHANNEL-X10 | weixin dispatch 重试计数位置 | `WeixinChannel.dispatch_failures: Mutex<HashMap<message_id, i32>>`（通道级，跨租约累计） | Java 放在 `LeaseSession.dispatchFailures`（每租约清零） | 防止“租约抖动 → 无限重试同一毒消息”；代价是租约频繁更替时可能提前 `abandon`。`MAX_DISPATCH_ATTEMPTS=3` 与 abandon 语义一致 |
| CHANNEL-X11 | 并发原语 | `Mutex<HashMap>` / `Mutex<Inner>` 单锁（`BotLoopGuard`、`IdempotencyStore`、`InMemoryAccessTokenStore`） | Java 用 `ConcurrentHashMap`（分段）+ `AtomicReference` CAS 循环 | 临界区极短（几十 ns 级），单锁实现更简单且无可观测阻塞；`put_if_newer` 语义仍为“严格更新者胜” |
| CHANNEL-X12 | weixin 并发模型 | tokio task + `tokio::select!` + `CancelSignal`（租约丢失即取消 turn，含 `TURN_CANCEL_GRACE` 宽限） | Java 守护线程 + `volatile running`/`AtomicBoolean` + `block(Duration)` 超时 | 取消能真正中断执行（Java 仅 `block` 超时后放弃等待）；`require_lease` 仍是同一套 fencing 语义 |
| CHANNEL-X13 | 注册表与 harness 的桥接 | `ChannelRegistry::with_manager` 在注册时同步 `ChannelManager`（`src/common/channel_registry.rs:85-100`） | Java 无此桥（registry 只服务 controller） | 主动推送（gateway 发起）与回调回复共用同一 `deliver_reply` 路径；`ChannelManager` 无 `unregister`，故注释说明 stop 只摘本地表 |

### 4.3 复核收敛记录（原「遗留」表，信息保留；非差异条目）

> 本节原为 §4.2 之后的「遗留」段，随 §4 重构移入独立小节，内容未改，仅作关闭证据留档。

**遗留（2026-10-31 复核，已收敛）**：§3 代码缺口清单已空（G1–G10 全部关闭）。2026-10-29 曾把三项登记为「非代码缺口（环境验证）」，本轮逐项复核后重新裁定：**三项的离线可测部分均已落地为代码+单测**，只剩真实租户联调（不在本 crate 代码范围）：

| 原登记项 | 2026-10-31 裁定 | 源码 + 离线测试证据 |
|---|---|---|
| ① DingTalk `check_url` 加密应答 / 验签解密 | 已完成（离线可测部分已全覆盖） | `dingtalk/controller.rs::answers_the_first_check_url_verification_with_an_encrypted_success`、`answers_check_url_in_plain_text_when_encryption_is_disabled`、`dispatches_an_encrypted_message`、`accepts_the_classic_msg_signature_when_a_token_is_configured`；`dingtalk/crypto.rs` 金标向量 `hmac_signature_matches_the_java_gold_vector` / `decrypts_the_java_gold_ciphertext` |
| ② DingTalk Stream 退避重连 | 已完成（离线可测部分已全覆盖） | `dingtalk/stream.rs::backoff_doubles_up_to_the_cap`、`start_serves_frames_and_reconnects_after_close`、`stop_while_sleeping_in_backoff_ends_the_loop`、`callback_is_acked_before_the_payload_is_dispatched`（ACK 时序） |
| ③ Feishu / WeCom token 刷新与失效重取 | 已完成（2026-10-31 补「过期缓存 → 重取」缺失覆盖） | 失效重取：`feishu/outbound.rs::parse_send_response_extracts_message_id_and_invalidates_on_auth_error`、`wecom/outbound.rs` 同构测试、各 provider `token_is_cached_until_invalidated`；**新增** 过期重取：`feishu/token.rs::an_expired_cache_entry_is_refetched`、`wecom/token.rs::an_expired_cache_entry_is_refetched`；80% TTL：`common/token.rs` 中 `refresh_at_ms` 测试 |
| ④ 个人微信 iLink 消费循环重试/退避 | **原为真缺口（G10），本轮已修** | 见 §3 + §3.2：`end_consuming()` 曾清 `running` 导致首任期即退出（永不重试）；`begin_consuming()` 曾重置 `backoff_ms` 导致 `maxBackoffMs` 死配置。现按 Java 语义重试 + 跨租约升级、仅由健康 poll+drain 重置 |

## 5. 已知限制（只剩「真实租户联调」，离线可测部分已全覆盖）

> 2026-10-31 复核：下列四项都曾有「环境验证」注册，但经核查，它们的**可离线部分均有源码 + 单测**（证据见 §4.3），因此只剩真实租户/真实网关联调。

- **DingTalk**：真实租户的 HTTP 回调验签/解密、`check_url` 加密应答、Stream 连接与退避重连（离线已由 `dingtalk/controller.rs` 与 `dingtalk/stream.rs` 测试覆盖）。
- **Feishu**：真实事件订阅需配置 `verificationToken`（未配置时普通事件一律 401，与 Java 一致）。
- **WeCom / DingTalk / Feishu**：真实 token 刷新与失效重取（离线已由 `*_token.rs` / `*_outbound.rs` 测试覆盖，含新增的「过期缓存 → 重取」）。
- **weixin**：真实 iLink 网关的长轮询/租约/消息投递；退避策略已离线覆盖（G10）。
- 全部单测均为离线（`HttpTransport` / `WebSocketConnector` seam + 金标向量），不发真实网络请求。

## 6. 增量跟踪流程（下次 Java 出现新特性时）

1. **读本文件**（§1 基线 / §2 逐类映射 / §4 差异 / §5 限制）+ `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 的 CHANNEL 行；需要历史背景时查 `CHANNEL__MIGRATION_HISTORY.md`。
2. **确认基线**：`git status` + `cargo test -p agentscope-extensions-channel`（期望结果见 §7）。数字与期望不符时先查明原因再动工。
3. **分析 Java 提交**：`git -C <java-repo> log --oneline <range>`；先 `--stat`，再按文件 `show <hash> -- <path>`。**禁用整提交全量 `show`**（上下文爆炸会导致分析中断）。
4. **登记条目**：在 §3 增加一行（沿用 `G<n>` 编号，附 Java 提交 hash / 类名），状态置 `待办`。
5. **实现**：只改 `crates/agentscope-extensions-channel/**` 与本文件；复用 harness 既有抽象（`PlatformChannel`/`PlatformCtx`/`InboundMessage`/`Peer`），**不新建平行类型**。
6. **测试**：新增/调整单测必须离线；行为缺陷先写复现用例（改前 FAILED），关键项做**反向验证**（回退实现 → 确认 FAILED → 恢复）。
7. **收尾**：更新 §1 基线数字、§2 对应行状态、§3 条目状态与关闭日期、§7 验证结果；在汇总 tracker 登记；过程记录写入 `CHANNEL__MIGRATION_HISTORY.md`（不写回本文件）。

### 6.1 硬约束

- 不得改 `agentscope-core` / `agentscope-harness`（`agent/gateway/platform_channel.rs` 属公共层落地点，仅在汇总方授权下可改）与其它模块 crate。
- 根 `Cargo.toml` 仅在「新建 crate 登记 member」时经授权改动，不得动 `[workspace.dependencies]`。
- 不得删既有功能 / 删断言 / `#[ignore]` 凑绿。
- 依赖优先用根 `[workspace.dependencies]` 已有项；新增依赖须在本 crate `Cargo.toml` 标具体版本并说明理由。
- 风格：最小改动、高内聚低耦合、不臆造 API。

### 6.2 状态取值

`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。

## 7. 验证基线（2026-10-23）

| 命令 | 结果 |
|---|---|
| `cargo check -p agentscope-extensions-channel --all-targets` | 0 error；本 crate 0 warning |
| `cargo test -p agentscope-extensions-channel` | **385 passed / 0 failed / 0 ignored**（doc-test 0） |
| `cargo clippy -p agentscope-extensions-channel --all-targets` | 仅 crate 内既有历史提示，无新增 |

**2026-10-31 增量（实测）**：新增 6 个离线单测（`weixin::channel::tests` 4 + `feishu::token::tests` 1 + `wecom::token::tests` 1），实测基线 `399 → 405 passed / 0 failed`；反向验证项：`the_consumer_reacquires_the_lease_after_a_transient_failure`（回退 `end_consuming` 的 `running = false` → FAILED）、`consumer_backoff_escalates_and_survives_a_reacquired_lease`（回退 `begin_consuming` 的重置行 → FAILED）。

**2026-10-31 Round 1 增量（子智能体只做代码编辑，实测由主智能体统一执行）**：本轮关闭 N01/N02/N03/N06/N07/N08/N09（N05 按批次约定跳过），新增离线单测 **22** 个：`common::idempotency_store::tests::contains_peeks_without_recording`、`common::channel_core::tests::{record_only_after_success_keeps_the_event_redeliverable, a_deduplicator_that_cannot_peek_falls_back_to_java_behaviour}`、`feishu::channel::tests::a_failed_dispatch_is_unavailable_and_stays_redeliverable`、`feishu::controller::tests::{a_dispatch_failure_answers_503_so_feishu_redelivers, a_non_utf8_body_is_a_bad_request}`、`wecom::outbound::tests::{a_rejected_send_is_an_error, an_empty_unparsable_or_errcode_less_body_is_an_error, an_error_body_is_truncated_to_a_bounded_snippet, send_propagates_a_rejection}`、`github::channel::tests::{start_resolves_the_bot_identity_so_self_comments_are_dropped, a_failed_start_keeps_the_bot_identity_unknown}`、`gitlab::channel::tests::{start_resolves_the_bot_identity_so_self_notes_are_dropped, a_failed_start_keeps_the_bot_identity_unknown}`、`github::controller::tests::{an_unhandled_event_type_is_204, a_non_utf8_body_is_a_bad_request_not_a_signature_failure}`、`gitlab::controller::tests::a_non_utf8_body_is_a_bad_request`、`wecom::controller::tests::dispatch_rejects_a_non_utf8_body`、`dingtalk::controller::tests::a_non_utf8_body_is_a_bad_request`、`gateway::tests::{build_gateway_registry_registers_the_channels_the_router_reads, build_gateway_registry_can_mirror_into_the_channel_manager}`、`channel_type_tests::channel_type_constants_match_the_java_type_fields`。另有 5 条既有测试**因行为修正**更新期望值（未删除、未放宽）：`github::channel::tests::ignores_unsupported_events`（Accepted → Ignored）、`gitlab::channel::tests::ignores_non_note_hook_events`（Accepted → Ignored）、`gitlab::controller::tests::other_hook_types_are_acknowledged`（200 → 204）、`wecom::outbound::tests::send_executes_one_request_per_message`（响应 seeded `errcode:0`）、`feishu::channel::tests::{normal_events_require_the_configured_header_token, duplicate_event_ids_are_dropped_by_the_idempotency_guard}`（改接真实 gateway，dispatch 成功后才算 admitted）。新增测试全部离线（`HttpTransport` seam + `HarnessGateway::create_standalone()` + 内存 agent），无网络、无 sleep。

- 测试构成：266 `#[test]` + 119 `#[tokio::test]`。
- 分模块：common 62 / dingtalk 78 / feishu 44 / github 32 / gitlab 35 / wecom 48 / weixin 86。
- 本 crate 无下游依赖方（仅根 `Cargo.toml` 的 members 登记），影响面即本 crate。

## 8. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/CHANNEL__MIGRATION_HISTORY.md`](./history/CHANNEL__MIGRATION_HISTORY.md)（**冻结**：原 §4 项目状态、§6 静态复核快照、§7 P0 修复（G1）、§8 P1/P2 修复（G2–G8））。
- 本文件只保留现行状态（§0–§7）；历史条目一律写进上述归档文件，不再回填本文。
- 汇总登记：`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行（CHANNEL = 已完成）。
