# 嵌入式（体积优先）构建

ACP 二进制（`acp_harness_agent` / `acp_agent_cli`）的体积优化手册：现有开关、实测收益、
以及尚未采用的进一步选项与其代价。

## 构建方式

```bat
build-acp-embedded.bat
rem 等价于：
cargo +nightly build --profile embedded --config .cargo/config.embedded.toml ^
  -p agentscope-extensions-acp --no-default-features --features lite --bins
```

需要 nightly（`build-std`）且已装 `rust-src`；产物在 `target/embedded/`（`.pdb` 不要打包）。

交叉 / 指定平台（发布矩阵走的就是这条，产物在 `target/<triple>/embedded/`）：

```bash
cargo +nightly build --profile embedded --config .cargo/config.embedded.toml \
  --target x86_64-unknown-linux-gnu \
  -p agentscope-extensions-acp --no-default-features --features lite --bins
```

支持 6 个 triple：`x86_64|aarch64-pc-windows-msvc`、`x86_64|aarch64-unknown-linux-gnu`、
`x86_64|aarch64-apple-darwin`。交叉到 `aarch64-unknown-linux-gnu` 时还需
`gcc-aarch64-linux-gnu` 并把 `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`
设上（`cc` crate 会自动找同名交叉 gcc 编 C 依赖，如 mimalloc）。

## 已生效的开关与实测收益

Windows MSVC / nightly，`acp_harness_agent.exe` 实测（同一台机器）：

| 阶段 | 体积 | 增量 | 说明 |
|---|---|---|---|
| 基线 | 10,579,456 | — | `profile embedded` + `build-std` + `/OPT:REF,ICF` |
| 单一 rustls 后端 | 9,765,376 | **-814,080** | 见下节 |
| 去 brotli / zstd | 9,500,672 | **-264,704** | `reqwest` 只保留 gzip/deflate |
| `-Zlocation-detail=none` | 9,240,576 | **-260,096** | panic 消息不再带文件/行号 |
| 目录快照按需释放（ACP 内存） | 9,246,720 | +6,144 | 见 `docs/ACP_MEMORY.md`，为内存换来的小代价 |
| **当前合计** | **9,246,720** | **-1,332,736（-12.6%）** | |

`acp_agent_cli.exe`：4,128,256 → 3,424,768（-17.0%）。

> 已回退的尝试：去掉 `async-anthropic`（`a625bf2`）实测 **harness +12,288 / cli −3,584**，没有体积收益
> （`Messages` 换成能力更全的 wire 适配器，两者体积相当），已 `git revert`；改动只剩「去重一份实现与
> 一个 reqwest 0.12 钉版」这一个价值，不值得用体积换。

各开关位置：

- `Cargo.toml` `[profile.embedded]`：`opt-level="z"`、`lto=fat`、`codegen-units=1`、
  `panic="abort"`、`strip="symbols"`、`debug=false`，依赖同样 `opt-level="z"`
  （`[profile.embedded.package."*"]`）。
- `.cargo/config.embedded.toml`：`build-std`（std + panic_abort）、`-Zlocation-detail=none`；
  链接期死代码消除按平台写在各自的 `[target.<triple>]` 里 —— MSVC 用
  `/OPT:REF`、`/OPT:ICF`、`/DEBUG:NONE`，GNU ld 用 `-Wl,--gc-sections,--icf=all`，
  ld64（macOS）用 `-Wl,-dead_strip`。`[build]` 只放与链接器无关的
  `-Zlocation-detail=none`，供不带 `--target` 的本机构建使用。
- `crates/agentscope-extensions-acp`：`--features lite` 去掉 bundled SQLite。

## rustls 只保留一个加密后端

`reqwest` 0.13 用 `rustls-no-provider` 特性（workspace `Cargo.toml`），进程内由
`agentscope_core::tls::ensure_crypto_provider()` 安装 `ring`：

- `reqwest` 0.12（经 `rust-mcp-transport` / `async-anthropic` 引入）本来就默认选 `ring`；
- 若 `reqwest` 0.13 走默认的 `rustls` 特性，会再编译并链接 `aws-lc-rs`（BoringSSL 分支），
  实测 **+814 KB** 且无功能收益。

`rustls-no-provider` 下 `reqwest` 在**构造 `Client` 时会 panic**（“No rustls crypto provider
is configured”），因此有两层保护：

1. 每个 `Client` 工厂先调 `ensure_crypto_provider()`（`grep -r ensure_crypto_provider crates`）；
2. `agentscope_core::tls` 里有一个 pre-`main` 初始化器（Windows `.CRT$XCU` / ELF
   `.init_array` / Mach-O `__mod_init_func`），覆盖不经过上述工厂的路径（如
   `#[derive(Default)]` 包装类型）。已实测该初始化器在 `lto=fat` + `/OPT:REF` 的产物中仍然存在。

新增 HTTP 客户端工厂时请照抄第 1 层；漏了会在测试里以 panic 暴露（每个 crate 的客户端
构造都有测试覆盖）。

## 未采用（可选，按需开启）

| 选项 | 实测/预估收益 | 代价 |
|---|---|---|
| `-Zfmt-debug=none`（加到 `.cargo/config.embedded.toml` 的 rustflags） | -168 KB（实测） | 所有 `{:?}` 输出变空，日志/错误信息基本不可读 |
| `-Zfmt-debug=shallow` | -131 KB（实测） | `{:?}` 只留类型名 |
| 用自写 JSON Schema 校验器替换 `jsonschema` | -474 KB（实测：临时移除该依赖） | 工具入参/结构化输出校验的正确性风险；需覆盖 type/properties/required/enum/items/oneOf 等关键字 |
| 去掉 `reqwest` 0.12 整条栈 | 预估 -0.8~1.2 MB | 需 feature gate：MCP 的 SSE/Streamable-HTTP 传输（`rust-mcp-transport` 强制 `cookies`/`multipart`/`rustls-tls`）与 `anthropic_async`（`async-anthropic` 0.6 钉住 `reqwest` 0.12）；会移除相应功能 |
| UPX 等压缩壳 | -66%（实测 9,240,576 → 3,104,256） | 启动 +130 ms、内存 +7.5 MB、杀软启发式命中；见下节 |

另：`-C force-unwind-tables=no` 在 `x86_64-pc-windows-msvc` 上被 rustc 直接拒绝（target 要求
unwind table），不要尝试；`panic_immediate_abort` 已从新 nightly 的 std 中移除。

## UPX 加壳（可选，手动执行，默认不启用）

`pack-acp-upx.bat` 把 `target/embedded/*.exe` 加壳后**另存**到 `target/embedded/upx/`，
原始产物不动：

```bat
set UPX_EXE=C:\tools\upx\upx.exe   rem 或者把 upx 加进 PATH
pack-acp-upx.bat
```

脚本只做 `upx --best --lzma` + `upx -t` 完整性校验 + 打印前后字节数；不联网、不覆盖原始文件、
不并入 `build-acp-embedded.bat`（漏装 UPX 时直接报错退出，不影响正常构建）。

实测（UPX 5.3.0 win64，与上表同一份产物）：

| 目标 | 原始 | `--best --lzma` | 比例 |
|---|---|---|---|
| `acp_harness_agent.exe` | 9,240,576 | **3,104,256** | 33.6% |
| `acp_agent_cli.exe` | 3,424,768 | **1,234,944** | 36.1% |

`--ultra-brute` 多耗 32 s 只再省 23 KB（3,081,216），没有意义。

### 代价（同一台机器实测）

| 指标 | 原始 | UPX | 差异 |
|---|---|---|---|
| 启动到退出 | 72~77 ms | 205~214 ms | **+130 ms/次** |
| 常驻工作集 | 25.9 MB | 33.4 MB | +7.5 MB |
| 峰值工作集 | 32.3 MB | 40.3 MB | +8.0 MB |

解压发生在启动阶段，所以每次冷启动都要付这 130 ms；常驻/峰值上升是因为解压后的镜像
无法再与磁盘上的压缩镜像共享页。

### 风险清单

- 段名变成 `UPX0/UPX1/UPX2`：这是多引擎 AV/EDR 的经典启发式命中项。本机 Defender
  引擎扫描对原始与加壳产物都报 no threats，但**不能外推**到 CrowdStrike / SentinelOne /
  火绒等；交付前必须在目标环境的 AV 上实测。
- 必须**先加壳后签名**（加壳会改写 PE，先签名再压缩等于毁掉签名）；部分客户审计策略
  直接禁止 packed binary。
- 崩溃转储、堆栈回溯、PDB 符号映射基本不可用，线上排障能力显著下降。
- 已实测保留：ASLR（`DYNAMIC_BASE`）、DEP（`NX_COMPAT`）、`HIGH_ENTROPY_VA`；
  `upx -t` 校验通过；加壳后的两个 exe 都能正常启动运行（退出行为与原始产物一致）。

### 如果目的只是「下载包更小」，别用 UPX

同一份原文件换压缩容器（运行时零代价）：

| 形式 | 体积 | 比例 |
|---|---|---|
| `tar.xz`（LZMA2） | 3,097,420 | 33.5% |
| `upx --best --lzma` | 3,104,256 | 33.6% |
| `gzip -9` | 4,159,906 | 45.0% |

`tar.xz` 比 UPX 还小 7 KB，且不触碰二进制本身。**UPX 唯一的价值是让「磁盘上的 exe」变小**
（镜像层、U 盘分发、单文件交付限制），而不是让下载更快。

## 体积审计小抄

- 分区：`acp_harness_agent.exe` 约 `.text` 7.5 MB + `.rdata` 1.5 MB + `.pdata` 0.19 MB；
  已无 `.debug*` 段（`strip` + `/DEBUG:NONE` 生效）。
- 改 rustflags 会触发全量重建（本机约 2~2.5 分钟）；改特性只重建受影响子图。
- 定位大头：`cargo tree -e features -i <crate>` 看特性来源；临时移除依赖测差值比估。
- 运行时内存（不是文件体积）：见 `docs/ACP_MEMORY.md` —— 26 MB 基线里 15.6 MB 是模型目录，空闲裁剪
  `AGENTSCOPE_ACP_MEM_TRIM_IDLE`（默认 3 秒）可把常驻工作集降到 ~0。
