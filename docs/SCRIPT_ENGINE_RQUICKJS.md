# 脚本引擎（rquickjs）方案

> 目标：给 Agent 运行时内嵌一个 JS 脚本引擎，把 `Toolkit` 里的工具暴露成脚本内的函数，
> 让 LLM / 用户用 JavaScript 编排多步工具调用（循环、条件、变量、复用），
> 并在此之上提供**文件系统 / 网络**操作能力。
>
> 状态：设计稿（未落地代码）。落地建议见文末「落地路线」。

## 1. 结论先行

- 选 **rquickjs**（QuickJS 的 Rust 绑定）：纯 C 实现、体积小（~几百 KB 级）、语法即 JS（LLM 生成质量最好）、支持 Promise / async。
- **关键事实（务必先纠正）**：rquickjs / QuickJS **本身不提供文件系统、网络、进程等任何宿主能力**。QuickJS 标准库只有 ECMAScript 核心（`Object/Array/JSON/Math/Date` 等），**没有 Node 的 `fs` / `net` / `http`**。
  - 因此「文件系统和网络操作码」不是引擎自带开关，而是**由宿主（Rust）注册的原生函数**提供。
  - 我们把这部分统一收敛到「**能力即工具**」模型（见 §5）：脚本通过 `tools.<name>()` 调工具，工具内部再去读写文件 / 发 HTTP，从而天然复用 `Toolkit` 的权限、超时、拦截与治理。

## 2. 选型对比（浓缩）

| 引擎 | 语言 | 纯 Rust | 原生 async | 体积 | LLM 友好 | 备注 |
|---|---|---|---|---|---|---|
| **rquickjs** | JS | ❌(QuickJS C) | ✅ Promise | 小 | **最好** | 本文推荐 |
| deno_core(V8) | JS | ❌ | ✅ | 大(几十 MB) | 最好 | 过重 |
| Rhai | Rhai | ✅ | ❌ 同步 | 小 | 一般 | 需 `spawn_blocking+block_on` |
| mlua | Lua | ❌(vendored) | ✅ | 中 | 尚可 | 语法非 JS |
| wasmtime | WASM | ❌ | ✅ host fn | 中/大 | 差(需先编译) | 适合强沙箱，不适合现场生成脚本 |

结论：要「LLM 现场写编排逻辑」，JS + rquickjs 是性价比最高的组合。

## 3. 架构

```
LLM / 用户
   │ 生成 JS 源码
   ▼
ScriptEngine::run(source)
   │  spawn_blocking（rquickjs 的 Runtime/Context 是 !Send）
   ▼
 rquickjs Runtime + Context
   │  注入原生桥:  __call_tool(name, argsJson) -> {ok,data|error}
   │  prelude:      globalThis.tools.<name> = (args) => ...
   │  运行脚本:     result(v) 交回结果
   ▼
 __call_tool ──► Toolkit::call_tool(&ToolUseBlock, ToolExecutionContext)
                   │  走完整治理管线（interceptor / 超时 / 重试 / capability surface / MCP）
                   ▼
                 Tool::call(...)  （文件 / 网络 / 业务工具）
```

要点：

1. **只经 `Toolkit::call_tool` 分发**，绝不直接 `tool.call()`——否则拦截器、超时、重试、并行开关、能力面、MCP 外部工具都会失效。
2. **`!Send` 处理**：`rquickjs::Runtime/Context` 不能跨线程，整套 JS 执行放进 `tokio::task::spawn_blocking`，在该线程内 create + drop。
3. **async 桥接**：在 blocking 线程里用 `tokio::runtime::Handle::block_on` 等 `Tool::call`（blocking 线程不是 async worker，`block_on` 合法）。

## 4. 脚本 API 契约

```js
// 每个已注册工具挂成 tools.<name>(args)，args 为对象
const r = tools.read_file({ path: "a.rs" });   // r 是 ToolResultBlock 的 JSON

// 顶层 await / .then 都受支持（返回值是 thenable）
const same = await tools.read_file({ path: "a.rs" });
tools.read_file({ path: "a.rs" }).then((x) => result(x.output[0].text));

// 交回结果（字符串原样返回，对象则 JSON.stringify）
result({ ok: true, n: 3 });
// 未调用 result 时，顶层 return 等价：
// return { ok: true };
```

- `tools.<name>(args)`：参数是一个普通对象；返回值是 `ToolResultBlock` 反序列化后的 JSON，同时是 **thenable**：`await tools.x(...)` 与 `tools.x(...).then(cb)` 都可用，而同步字段访问（`r.output[0].text`）保持可用。
- `result(v)`：交回结果（字符串原样，对象 `JSON.stringify`）。`v` 是 thenable/Promise 时会等待其 resolve 再序列化（`result(Promise.resolve("x"))` → `x`）。未调用时，脚本顶层 `return v` 等价。
- 执行模型：脚本被包进 `async () => { ... }` 后 eval，随后 drain 微任务队列（`Ctx::execute_pending_job`），因此**顶层 `await` 合法**、`.then()` 回调与 async IIFE 里的 `result(...)` 都会真正执行；drain 循环带 deadline 保护。
- 错误：工具失败会 `throw`，脚本可 `try/catch`；`await` 的 rejection 同样可 `try/catch`；桥函数内部**不 panic、不返回 Rust Err**，一律编码为 `{ok:false,error}`。
- `console.log/info/warn/debug/error`：追加到内部日志；脚本没调用 `result` 时，日志按行作为输出返回（调试用）。
- 诊断：`throw new Error("boom")` 保留 `name: message` + 栈（不再只剩栈）；语法错误带 JS 异常消息；超时统一报 `script exceeded time limit`，内存耗尽/内部错误报 `script aborted (out of memory or internal error)`。
- 隔离：PRELUDE 在 IIFE 内构建 `tools`，并 `delete` 掉 `__call_tool`/`__tool_names`；内部状态（`__result`/`__settled`/`__error`/`__logs`）用不可枚举属性隐藏。
- 工具隔离：`ScriptEngine::with_denied_tools([...])` 让指定工具在脚本里**完全不可见/不可调**（不进入 `tools`，桥也不接收）；ACP 默认 deny `run_script`/`agent_spawn`/`agent_send`/`task_cancel`/`compact_context`/`propose_skill`/`skill_manage`，保留 fs/execute 等编排工具。

## 5. 文件系统与网络操作

### 5.1 事实更正

> **rquickjs 不自带 fs/net。** 需要「文件系统 / 网络操作码」时，只能二选一：
> - **路线 A（推荐）**：把每个文件/网络操作实现成 `Tool`，脚本用 `tools.<op>()` 调；
> - **路线 B**：在宿主侧注册专门的 `fs` / `net` 原生对象，直接暴露给脚本。

### 5.2 路线 A：能力即工具（推荐）

优势：所有操作走 `Toolkit::call_tool`，自动获得 **权限过滤（capability surface）、拦截器、超时、重试、审计、MCP 外部工具**；脚本侧只认 `tools.*`，无需额外抽象。

操作码 → 工具名映射（示例，最终以实际注册的工具为准）：

| 命名空间 | 操作码 | 建议工具名 | 底层实现 |
|---|---|---|---|
| 文件 | `fs.read` | `read_file` | `FilesystemTool { operation: "read" }` |
| 文件 | `fs.write` | `write_file` | `FilesystemTool { operation: "write" }` |
| 文件 | `fs.list` | `list_dir` | `FilesystemTool { operation: "list" }` |
| 文件 | `fs.grep` | `grep` | `LocalFilesystem::grep`（harness），需注册为工具 |
| 文件 | `fs.glob` | `glob` | `LocalFilesystem::glob`（harness），需注册为工具 |
| 网络 | `net.fetch` | `http_request` | 需新增受控 HTTP 工具（reqwest/ureq + URL 白名单） |

> 现状说明：`agentscope-core` 目前只提供 `FilesystemTool`（`read|write|list` 三种 operation）；
> `grep` / `glob` 位于 harness 的 workspace 文件系统；**网络没有任何内置工具**。
> 因此要启用 `net.*` 操作码，必须先新增一个受控 HTTP 工具（见 §5.4）。

**为什么用「工具」而不是直接给脚本 `fs`/`net`**：工具调用会经过 `Toolkit` 的完整治理链——
能力面（capability surface，子 Agent 白名单外调不到）、拦截器（审计/审批）、超时/重试、MCP 外部工具。
脚本仅仅是「编排者」，无权绕过这些约束。

### 5.3 路线 B：原生 `fs` / `net` 模块（可选，默认关闭）

如果确实需要「脚本里像 Node 一样直接 `fs.readFileSync` / `fetch`」，可在宿主注册原生对象：

```rust
// 先注册原生函数，再由 prelude 组装成 fs/net 对象
ctx.globals().set("__fs_read", Function::new(ctx.clone(), move |path: String| -> String { /* 宿主读文件 */ }))?;
ctx.globals().set("__net_fetch", Function::new(ctx.clone(), move |url: String| -> String { /* 宿主发请求 */ }))?;

ctx.eval::<(), _>(r#"
  globalThis.fs = {
    read: (p) => JSON.parse(__fs_read(p)),
    // write/list...
  };
  globalThis.net = {
    fetch: (u, opt) => JSON.parse(__net_fetch(u)),
  };
"#)?;
```

代价与约束：
- **绕过 `Toolkit` 治理**：权限、审计、超时、MCP 全部要自己重做一遍；
- 因此仅供「可信脚本 / 受控环境」使用，作为 cargo feature 默认关闭；
- 若开启，必须同时施加 §5.4 的沙箱策略。

### 5.4 fs / net 的权限与沙箱（必须）

| 风险 | 控制手段 |
|---|---|
| 路径穿越（`../../etc`） | 根目录白名单：`canonicalize` 后 `starts_with(root)`；相对路径统一 join 到 root |
| 覆盖/删除关键文件 | 只读模式；写入前 `is_dangerous_path` 检查（复用 `tool/dangerous.rs`） |
| 大文件撑爆内存 | 单次读写大小上限（如 1–8 MiB），超限报错 |
| SSRF（访问内网/元数据地址） | 协议白名单（仅 `http/https`）；禁止解析到私有 IP 段；重定向后重新校验 |
| 请求挂死 | 单次请求超时；整体脚本超时（见 §6） |
| 日志缺失 | 每次 fs/net 操作写审计事件（可走 `ToolEmitter`） |

示例：路径解析必须在宿主完成，**不要让脚本自己拼路径**：

```rust
fn resolve_in_root(root: &Path, user_path: &str) -> Result<PathBuf> {
    let p = root.join(user_path);                     // 相对则拼到 root
    let canon = p.canonicalize()?;
    if !canon.starts_with(root) {
        anyhow::bail!("path escapes workspace");
    }
    Ok(canon)
}
```

## 6. 资源限制与执行控制

| 维度 | 手段 | 说明 |
|---|---|---|
| 内存 | `rt.set_memory_limit(bytes)` | 如 16 MiB；超限抛 `InternalError` |
| 栈 | `rt.set_max_stack_size(bytes)` | 防深递归 |
| 超时/死循环 | `set_interrupt_handler`（按 QuickJS 实现返回 true 即中断） | **唯一能真正停住死循环的手段** |
| 外层兜底 | `tokio::time::timeout(spawn_blocking(...))` | 保证调用方及时返回（但停不掉 blocking 线程里的死循环，故仍需中断） |
| 危险 API | 不注册 loader；按需禁用 `eval` | QuickJS 默认无 `require`/`fs`/`process` |

## 7. 关键陷阱（实现时必看）

1. **`!Send`**：`Runtime`/`Context` 只能在 `spawn_blocking` 的单线程内创建/销毁，绝不跨 `await` 或线程。
2. **`block_on` 位置**：只能在 blocking 线程里 `Handle::block_on`；在 `async fn` 主体里直接 `block_on` 会 panic。
3. **错误编码**：原生桥返回 `{ok,data|error}` 字符串，不返回 `rquickjs::Error`，避免绑定版本 API 变化导致编译失败。
4. **上下文透传**：若调用方有带 `emitter` / `process_registry` 的 `ToolExecutionContext`，桥里应使用它，才能让流式输出、取消、子进程 kill 生效。
5. **中断 API 位置随版本**：`set_interrupt_handler` 在 rquickjs 0.8 / 0.9 的挂载对象不同，按锁定版本调整。

## 8. 代码骨架

```rust
pub struct ScriptEngine { toolkit: Toolkit, limits: ScriptLimits }

impl ScriptEngine {
    pub async fn run(&self, source: String) -> anyhow::Result<String> {
        let toolkit = self.toolkit.clone();
        let limits = self.limits.clone();
        let handle = tokio::runtime::Handle::current();   // 在 async 侧取，克隆进 blocking

        let join = tokio::task::spawn_blocking(move || {
            let rt = Runtime::new()?;
            rt.set_memory_limit(limits.memory_bytes);
            rt.set_max_stack_size(limits.stack_bytes);
            let deadline = Instant::now() + limits.timeout;
            rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));

            let ctx = Context::full(&rt)?;
            ctx.with(|ctx| {
                let tk = Rc::new(toolkit);
                let h = handle.clone();
                let tk_for_fn = Rc::clone(&tk);
                let bridge = Function::new(ctx.clone(), move |name: String, args: String| -> String {
                    let tk = Rc::clone(&tk_for_fn);
                    let h = h.clone();
                    match h.block_on(call_tool_async(tk.as_ref(), &name, &args)) {
                        Ok(v) => json!({ "ok": true, "data": v }).to_string(),
                        Err(e) => json!({ "ok": false, "error": e.to_string() }).to_string(),
                    }
                })?;
                ctx.globals().set("__call_tool", bridge)?;
                ctx.globals().set("__tool_names", serde_json::to_string(&tk.tool_names())?)?;
                ctx.eval::<(), _>(PRELUDE)?;                 // 建 tools.*
                ctx.eval::<(), _>(source.as_str())?;         // 用户脚本
                Ok::<_, anyhow::Error>(ctx.globals().get::<_, Option<String>>("__result")?.flatten().unwrap_or_default())
            })
        });

        match tokio::time::timeout(self.limits.timeout + Duration::from_secs(1), join).await {
            Ok(r) => r?,
            Err(_) => anyhow::bail!("script exceeded time limit"),
        }
    }
}

async fn call_tool_async(tk: &Toolkit, name: &str, args_json: &str) -> anyhow::Result<serde_json::Value> {
    let input: JsonMap = if args_json.trim().is_empty() { JsonMap::new() } else { serde_json::from_str(args_json)? };
    let call = ToolUseBlock::new(format!("js-{}", next_id()), name, input);
    let ctx = ToolExecutionContext { tool_call: Some(call.clone()), ..Default::default() };
    Ok(serde_json::to_value(&tk.call_tool(&call, ctx).await?)?)
}
```

`PRELUDE`（JS）：把工具名挂到 `tools` 命名空间，并定义 `result()`。

## 9. 落地路线（P1–P5 已完成）

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 | 新建 `crates/agentscope-extensions-script`；`ScriptEngine` + `tools.*` + `result` + 资源限制 | ✅ 已落地 |
| P2 | fs 操作码：`read_file` / `write_file` / `list_dir` / `grep` / `glob`（沙箱根目录） | ✅ 已落地 |
| P3 | net 操作码：受控 `http_request`（白名单 + SSRF 防护 + 超时 + 大小上限） | ✅ 已落地 |
| P4 | 接入 Agent：`run_script` 工具，模型可触发脚本编排 | ✅ 已落地 |
| P5 | 测试：沙箱逃逸、路径穿越、SSRF、超时死循环、内存炸弹、错误传播 | ✅ 9 passed / 0 failed |

### 9.1 实际实现

crate 已加入根 `Cargo.toml` 的 `members`，依赖 `rquickjs = "0.14"`（已进 `Cargo.lock`）。

| 文件 | 内容 |
|---|---|
| `src/engine.rs` | `ScriptEngine`：`spawn_blocking` + `Handle::block_on` 桥接；PRELUDE 注入 `tools.*` / `text()` / `result()` |
| `src/limits.rs` | `ScriptLimits { timeout, memory_bytes, stack_bytes }` |
| `src/fs.rs` | `FsSandbox`：注册 5 个细粒度工具；逻辑 `..` 归一化拒绝路径穿越 |
| `src/http.rs` | `HttpConfig` + `HttpRequestTool`：`http/https`、私有 IP 拒绝、`Policy::none`、超时、响应大小上限 |
| `src/run_script.rs` | `RunScriptTool`：把引擎暴露为 `run_script` 工具（`is_concurrency_safe = false`） |
| `tests/script_engine.rs` | 11 个离线测试 |

> 与 §8 骨架的差异：实际实现用 `agentscope_core::error::Error`（而非 `anyhow`），
> 且 HTTP client 构建失败时用 `expect` 而非回退到会跟随重定向的 `Client::new()`。

**验证命令**：`cargo test -p agentscope-extensions-script -j 2` → `11 passed; 0 failed`。

**运行时要求（重要）**：`ScriptEngine::run` 内部 `spawn_blocking` + `Handle::block_on`，
**必须在 multi-thread runtime（`#[tokio::main]` 默认）上调用**；`current_thread` 上可能死锁。

**已知限制**：
- HTTP 的 DNS 预解析与 `reqwest` 实际解析存在 TOCTOU；严格场景需 pin 解析地址。

> 已补：fs 对存在路径 `canonicalize` 后检查是否仍在 root 内（拒绝符号链接逃逸），对写入目标
> canonicalize 最近存在的祖先再拼回尾部；HTTP 响应改为 `bytes_stream()` 流式累加，超过上限
> 立即中止（保留 `Content-Length` 预检）。

### 9.2 体积门控与集成

**crate 级门控**：`rquickjs` 是 optional，由 `js` feature 开启（`default = ["js"]`）。关闭后
`fs` / `http` 工具仍可用，不编 QuickJS：

```toml
agentscope-extensions-script = { version = "0.2.0", default-features = false } # no QuickJS
```

**facade（`agentscope-rs`）**：

| feature | 内容 |
|---|---|
| `script` | fs/http 工具（不含 QuickJS；facade 对 script crate 用 `default-features = false`） |
| `script-js` | 额外开启 `agentscope-extensions-script/js`（含 QuickJS） |

facade 默认 feature 集不含 `script`，因此默认构建不引入 rquickjs。

**ACP（`agentscope-extensions-acp`）**：

- `script` feature（**默认关闭**）在 `acp_harness_agent` 的 base toolkit 上注册
  `FsSandbox(cwd)` + `RunScriptTool`。
- `HarnessAgentBuilder::toolkit` 在 build 时 clone 同一个 `Toolkit`（内部 `Arc<Mutex>` 共享），
  harness 后续注册的工具进入同一张表，因此脚本可调用 ACP 的**全部工具**。
- 启用：`cargo xtask acp build sqlite --profile embedded`（xtask 构建**默认**追加 `script`；
  不需要脚本时加 `--no-script`，其它 feature 用 `--features a,b` 追加）。
  等价的直接 cargo 命令：`--no-default-features --features "lite,durable-sqlite,acp-v2,script"`。

> 体积提醒：rquickjs 会随二进制打包 QuickJS；对体积敏感目标（`embedded` profile）需评估
> `docs/EMBEDDED_SIZE.md` 的增量，需要时关闭 `js` / `script` / `script-js` feature。

## 10. 与现有框架的衔接要点

- **复用而非重建**：`Toolkit`、`FilesystemTool`、`tool/dangerous.rs`、`ToolEmitter`、`ToolExecutionContext` 均已存在，脚本引擎只做「桥」。
- **脚本本身也是一个工具**：建议把 `run_script(script)` 实现为 `Tool`（`is_concurrency_safe=false`、`is_long_running` 视情况），由模型像调普通工具一样触发，这样脚本执行也进入同一条治理/审计链。
- **不要把脚本执行塞进 `Tool::call` 的同步栈**：`run_script` 是 async，内部再 `spawn_blocking`，避免阻塞 async worker。
