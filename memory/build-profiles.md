# agentscope-rust 构建 profile / 体积优化（2026-09-22 建立）

## 背景
用户在本仓库做「嵌入式极限体积」优化，要求把 release/embedded profile 与 nightly RUSTFLAGS 落到仓库。

## 当前落地状态（均已 `cargo metadata` 校验 exit=0）

根 `Cargo.toml`：
- `[profile.release]`：`lto="fat"` / `codegen-units=1` / `panic="abort"` / `strip=true` / `opt-level="z"`，并新增 `[profile.release.package."*"] opt-level = 3`（依赖求快，会让 release 变大）。
- `[profile.embedded]`：`inherits="release"` + `opt-level="z"` / `lto=true`(==fat) / `codegen-units=1` / `panic="abort"` / `strip="symbols"`(==true) / `debug=false`(=默认)；`[profile.embedded.package."*"] opt-level = "z"`（尺寸优先，**有意与 release 相反**）；`[profile.embedded.build-override] opt-level = 3`。
- 结论：除 `package."*"` 外，release 与 embedded **artifact 级等效**。

`.cargo/config.toml`（自动加载）：仅 `[build] incremental = false` + 指路注释。**切勿**在此放 nightly `-Z` 标志——无法按 profile/toolchain 区分，会打挂 stable 下所有 `cargo build/test`。

`.cargo/config.embedded.toml`（**不自动加载**，需显式 `--config`）：
```toml
[unstable]
build-std = ["std", "panic_abort"]
build-std-features = ["panic_immediate_abort"]
[build]
rustflags = ["-Z","location-detail=none","-C","link-arg=/OPT:REF","-C","link-arg=/OPT:ICF","-C","link-arg=/DEBUG:NONE"]
```
正确命令：`cargo +nightly build -p <crate> --profile embedded --config .cargo/config.embedded.toml`

## 踩过的坑（重要，勿重复）
1. **`-Z build-std` / `build-std-features` 是 cargo 的选项，不是 rustc 的**。放进 `[build] rustflags` 会被转发给 rustc → `unknown unstable option: build-std`。必须放 `[unstable]` 表。
2. **必须 `cargo +nightly`**。默认工具链是 stable 1.96.1，任何 `-Z` 都会在「探测 target 信息」阶段直接失败（`the option Z is only accepted on the nightly compiler`，exit 101）。RustRover 的 Run Configuration（`--color=always ... -Wwarnings`）默认用 stable，需改为 nightly。
3. **自定义 profile 目录下 `incremental=false` 无效**：`target/embedded/incremental/` 仍会生成并占盘（E 盘曾因此紧张）；`debug/incremental` 才是被该设置管住的那个。
4. 用户曾报「embedded 比 release 大」——机制是 `package."*" opt-level=3`（越大越快的依赖优化）。若 release 参考值取自加该行之前，就会误判。

## Windows MSVC 实测（rustc 1.96.1 + link.exe，hello world）
- `-C strip=symbols` 对 `.exe` **无可测效果**（`-g` 129024 vs `-g`+strip 129024）。
- `.pdb` **无条件生成**：`debuginfo=0` 也产 1.27 MB pdb。`debug=false` 拦不住它。真实项目 `acp_harness_agent.pdb` = 3.88 MB。
- `-C link-arg=/DEBUG:NONE` 被接受，无尺寸影响（125440 = 默认）。
- ⇒ 打包时若不排除 `.pdb`，体积会被它主导；两个 profile 都会产 pdb。

## 环境事实
- 工具链：stable 1.96.1（默认）、nightly 1.99.0-nightly (2026-07-06)；`rust-src` 已装（build-std 必需）。
- E 盘可用空间实测 **51.8 GB**（MEMORY.md 里「约 9GB 紧张」的记录已过期）。
- `build-std` 会从源码重编 core/std/compiler_builtins，换 flag 或清盘即整棵重来，耗时长。

## 未做 / 待定
- 从未做过 release vs embedded 的真机 A/B 尺寸对比（用户始终未提供自己的测量数字与构建命令）。
- `opt-level` 用 `"z"` 还是 `"s"` 未实测（已定 `z`）。
