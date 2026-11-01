# ACP 进程内存：基线、回收与诊断

`acp_harness_agent` 空闲时常驻 ~26 MB。本文记录实测构成、三个可调开关，以及
“为什么流式响应结束后不需要额外回收”。

## 实测基线（Windows，本机 32 核）

| 场景 | WS | commit | 线程 |
|---|---|---|---|
| 空 Rust 程序（下限） | 4.1 MB | 0.7 MB | 1 |
| `acp_harness_agent` 启动 72 ms 后 | 25.9 MB | 29.0 MB | 13 |
| 同上，空闲 6 s | 25.9 MB | 29.0 MB | 13 |
| 同上，`AGENTSCOPE_MODEL_CATALOG=0` | 6.9 MB | 9.7 MB | 13 |

启动时间线：16 ms 时 16.5 MB → 72 ms 时 25.9 MB，之后完全不变。**这 26 MB 与是否跑过对话无关**，
所以不存在“响应结束后没回收”的残留。

线程不是变量：`AGENTSCOPE_ACP_WORKER_THREADS` 从 1 调到 16，WS 只从 25.8 变到 26.2 MB
（栈是保留地址空间，提交量很小）。

## 构成（`--features alloc-stats` 实测 live heap）

`[mem] serving | rss 25.7MB commit 20.9MB peak_rss 32.4MB | live 15.6MB peak_live 22.7MB blocks 215788`

| 组成 | live 堆 | 说明 |
|---|---|---|
| 模型目录 `~/.agentscope/model-catalog/catalog.v1.json`（7.4 MB JSON） | **15.6 MB / 215,788 块** | `ModelCatalog::global()` 启动即 `read_snapshot()` 解析成结构 |
| 其余全部（runtime、工具表、配置、日志） | 0.2 MB / 506 块 | `live 0.2MB blocks 506` |
| 进程固定开销（镜像 + CRT + 系统 DLL） | — | 约 6.6 MB WS |

关掉目录（`AGENTSCOPE_MODEL_CATALOG=0`）后 live 堆从 15.6 MB 掉到 0.2 MB、WS 从 25.9 掉到 6.9 MB
—— 这就是 26 MB 的主因。（嵌入版不能关：目录提供模型能力位，关掉等于减功能。）

## 开关

| 变量 | 默认 | 作用 |
|---|---|---|
| `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | 运行时 worker 数 |
| `AGENTSCOPE_ACP_MEM_COLLECT` | 开 | 每轮结束 `HeapCompact`（Windows）/ `mi_collect(true)`（unix）；`=0` 关闭 |
| `AGENTSCOPE_ACP_MEM_TRIM_IDLE` | `3` | 空闲 N 秒后 `EmptyWorkingSet` 丢弃工作集；`0` 关闭 |
| `AGENTSCOPE_ACP_MEM_REPORT` | 关 | `[mem] turn` 行附加 `AgentState` 序列化大小 |

诊断构建：`--features alloc-stats` 把全局分配器换成计数版，`[mem]` 行附带 `live/peak_live/blocks`
（只给两个 bin 用，库不装 `#[global_allocator]`；体积 +6 KB，热路径带原子计数，不要用于交付）。

## 实测：两种回收手段

| 手段 | 实测结果 |
|---|---|
| 大块 buffer 释放（40 MB） | Windows 立即归还：commit 40.7 → 0.7 MB，**应用层不需要做任何事** |
| 小块碎片（20000 次 96 B~8 KB，峰值 18 MB） | 释放后 commit 3.3 MB；再 `HeapCompact` **无变化** |
| A：每轮 `HeapCompact` | 没有释放时收益 ≈ 0（NT 堆自己会 decommit 空闲页）；目录释放之后再裁剪额外省约 1.3 MB commit |
| B：空闲 3 s `EmptyWorkingSet` | WS 25.9 → **0.0 MB**（commit 不变）；重新触碰 16 MB 只需 **1.0 ms** |

结论：“流式响应结束后回收”在 Windows 上本来就是系统行为；真正的大头是启动时解析的模型目录，
它不会因为一轮对话结束而消失。B 把它换出到 pagefile，代价是下一轮首次访问的软缺页
（实测 ~1 ms / 16 MB，页缓存命中时几乎无感）。

## 目录快照：选定模型后立即释放（已落地）

目录（`~/.agentscope/model-catalog/catalog.v1.json`，9,312 条；解析后整表约 **15.5 MB live / 21.5 万个块**）
只在“有哪些模型”这类问题上需要：`initialize`、`session/new`、`session/load`、`session/resume`、切模型。
会话一旦绑定好模型，每轮 prompt 只需要**当前模型那一个** `Arc<ModelDescriptor>`。

实现（按需求触发，不是按时间空闲释放）：

1. `AcpAgentSession` 在绑定模型时（构造 + `set_model`）就解析好 `ModelDescriptor` 并缓存，
   `selected_descriptor()` 之后只读缓存，`selected_catalog_key()` / 用量仪表不再碰目录。
   `resolved` 标志区分“目录里没有这个模型”和“还没解析”—— 否则未知模型会让每轮都重新解析整表。
2. `ModelCatalog::release_snapshot()` 丢掉解析后的表，只保留 `schema_version` / `generation` /
   `sources` / `fetched_at`；`ensure_loaded()` 在下次列举时重新解析（约 45 ms）。
   `is_stale()` 对“已释放”返回 false —— 否则会被当成过期而触发真实的 models.dev 网络刷新。
3. 释放点：`session/new`、`session/load`、`session/resume`、`set_session_config_option` 的响应之前，
   以及目录刷新事件之后（启动刷新可能晚于 `session/new` 完成，不挂这里会白占一个 TTL）。
   守卫是“所有已注册会话都已绑定模型”，任何一个没绑定就跳过。
4. 释放成功后顺手 arm 一次空闲裁剪，把刚变成空闲的堆页真正还给系统。

实测（embedded `acp_harness_agent.exe`，同一进程内 A/B）：

| 阶段 | WS | commit |
|---|---|---|
| `initialize` 之后（旧实现：表已加载、未释放） | 26.7 MB | 29.4 MB |
| `initialize` 之后（当前实现：不再加载目录，见下节） | **2.6 MB** | **11.7 MB** |
| `session/new` 之后（已释放 + 空闲裁剪） | **0.6~1.5 MB** | **13.5 MB** |
| `session/new` 之后，`AGENTSCOPE_ACP_MEM_TRIM_IDLE=0`（不裁剪） | 11.9 MB | 14.8 MB |
| 参照：关掉目录（`AGENTSCOPE_MODEL_CATALOG=0`） | 8.6 MB | 10.4 MB |
| 一轮真实对话结束后 | 2.2 MB | 15.9 MB |

即 **commit −15.5 MB（29.0 → 13.5，−53%）、WS −24 MB**：释放本身就把整张表（15.5 MB 活数据）连同解析
峰值留下的空闲页一起还给了系统（`EmptyWorkingSet` 只是把剩下的换出去）。代价：`session/new` /
切模型 / 24h 刷新各多一次约 45 ms 的重新解析，每轮 prompt 不受影响。

## `initialize` 窗口：`capabilities()` 曾为一个布尔值解析整张表（已修）

`initialize` 响应里的 `promptCapabilities.image` 只问「有没有任何一个可选模型能收图片」，旧实现每次
`initialize` 都跑 `self.models.iter().filter_map(resolve_choice).any(...)`，而 `resolve_choice` 会
`ensure_loaded()` → 整张表 15.5 MB。打点实测（alloc-stats，同一进程）：

| 打点 | live | blocks |
|---|---|---|
| `cap-enter`（`capabilities()` 入口） | 8.2 MB | 1,043 |
| `cap-exit`（目录在用） | 23.7 MB | 215,984 |
| `cap-exit`（`AGENTSCOPE_MODEL_CATALOG=0`） | 8.2 MB | 810 |

即这一个布尔值要多花 **15.5 MB live / 21.5 万个块**，也正是 `initialize` 后 WS 22.0 MB / commit 29.4 MB
的来源（关目录时同一时刻只有 7.4 / 9.8 MB）。

修法（`ImageSupport`）：答案缓存在 `AcpCodingAgent` 上，**只在目录已经常驻时**重算 —— 启动后
（`ModelCatalog::new` 已读过盘上快照）与每次目录刷新事件之后，都赶在 `release_catalog_if_unused`
之前算好；`capabilities()` 只读缓存。宿主若从未刷新过缓存（库的直接用户），`capabilities()` 仍回落
旧行为，取值与功能完全一致。

实测（embedded，同一份探针）：

| 阶段 | WS | commit |
|---|---|---|
| `initialize` 之后（旧） | 26.5 MB | 29.6 MB |
| `initialize` 之后（新） | **2.6 MB** | **11.7 MB** |

能力值 A/B：新旧二进制对同一份目录都回
`{"image":true,"audio":false,"embeddedContext":true}`。

## 释放到底释放了什么（`--features alloc-stats` 实测 live heap）

| 阶段 | live | blocks | commit |
|---|---|---|---|
| 启动、目录已解析（`serving`） | 15.6 MB | 215,515 | 21.0 MB |
| 释放后（`post-release`） | **8.2 MB** | **1,008** | 12.0 MB |
| 同一时刻、`AGENTSCOPE_MODEL_CATALOG=0` | 8.2 MB | 611 | 9.8 MB |
| `cap-exit`（目录在用） | 23.7 MB | 215,984 | 29.5 MB |
| `session/new` 后再释放（含一个会话） | 8.7 MB | 5,369 | 13.4 MB |

- 目录表 = **15.5 MB live / 约 214.5k 个块**（`cap-exit − cap-enter`，且关目录时同一调用只 +0.2 MB）；
- 那 8.2 MB 是 ACP/agent 侧骨架（约 1 千个大块），关掉目录也一样存在；
- 一个会话（agent + 工具 + 配置项）再加 0.5 MB / 4,361 块。

早期把 `post-release` 的 8.2 MB 当成骨架、把表记成 **7.4 MB** 是错的：`serving` 那一刻骨架还没分配
（关目录时 `serving` 只有 0.2 MB），7.4 MB 只是「后来分配的骨架 8.0 MB」与「表 15.4 MB」之差。

两个容易误判的点：

- ACP 的模型下拉框**不是** 9,312 条的目录，而是 `~/.agentscope/models.json` 里的 provider 列表
  （实测 `all_models()` 只新增 **265 个块 / <0.1 MB**）。所以「让 `config_options()` 按需重载」
  几乎省不到东西（<0.1 MB），**不值得做**。
- commit 比 live 多出来的约 4.7 MB 是解析峰值（`peak_live` 31.6 MB）留下的空闲堆页，不是可释放的
  数据结构；空闲裁剪里的 `HeapCompact` 已能拿回其中约 1.3 MB，其余属于分配器保留策略。

## 怎么自己核

- 启动后看日志 `<cwd>/.agentscope/log/acp-agent.log` 里的 `[mem] serving` / `[mem] init` 行；
- 每轮结束看 `[mem] turn stop=... messages=N | rss X->YMB commit ...`（`X` 持续上涨才是泄漏）；
- 空闲裁剪发生后会多一行 `[mem] idle trim | rss X->YMB commit ...`。
