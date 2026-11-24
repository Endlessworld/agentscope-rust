# ACP Registry 元数据

`agent.json` 是提交给 [ACP Registry](https://github.com/zed-industries/agent-client-protocol) 的注册元数据，
由脚本生成，不要手改数字：

```bash
# 把一次 Release 的产物（含 SHA256SUMS.txt）拿来，重生成 agent.json
cargo xtask acp registry <artifacts-dir>

# 只写到别处（不覆盖本目录）
cargo xtask acp registry <artifacts-dir> --out target/agent.json
```

- `version` 取自根 `Cargo.toml` 的 `[workspace.package] version`，与 tag 由
  `.github/workflows/release-acp.yml` 的 `version-check` job 对齐；
- `archive` URL 固定为 `.../releases/download/v<version>/agentscope-acp-<platform>.zip|tar.gz`
  （资产名不带版本号，Registry 的自动更新只替换 URL 里的 `/vX.Y.Z/`）；
- `sha256` 直接取 `SHA256SUMS.txt`。

推 tag 后，Release 流水线会把生成好的 `acp-registry-agent.json` 一起传上去，发布完把它拷回本目录
（改名为 `agent.json`）再提 Registry PR，不用手抄 sha256。

> 当前本目录的 `agent.json` 仍是 **v0.1.1** 的产物：v0.2.0 的六平台压缩包要由 CI 构建，
> 本机拿不到它们的 sha256，所以不能提前回填。发完 v0.2.0 后按上面的做法覆盖一次即可。
