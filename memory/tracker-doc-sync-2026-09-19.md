# Tracker 文档同步批（2026-09-19，提交 950e5ec）

## 起因
用户问：「汇总 §1 里几个标『部分完成』的是不是也可以更新成完成了？」

## 结论：大部分**不能**改
既定裁定「汇总视图不得高于独立 tracker」。逐个取证后，8 个「部分完成」的**独立 tracker §1 自述也都是「部分完成」**，且都有实测支撑的未闭合缺口：
- MODEL：anthropic / gemini 未迁移
- PROTOCOL：agui 的 converter / adapter / encoder / processor 未迁移
- RAG：bailian / haystack 未迁移
- JDBC：Java 98 个 `@Test` vs Rust **2**
- POSTGRESQL：Java 130+ `@Test` vs Rust **7**（且全挤在 `sql.rs`）
- MONGODB：GridFS 分块 / legacy 回退（P1/P2）
- REDIS：`base_store()` 自动接线 / `find_keys` SCAN 语义（P1）

→ 改「已完成」等于藏缺口，正是 `5d199ef` 刚纠正过的问题。**保留不动**。

## 反而查出三处 tracker 与源码不符（已修正，这是本批实质价值）
1. **SANDBOX §3.2 把已实现项标成「待办」**：
   - 「四类 `*FilesystemSpec`」→ 实际 `src/fs_spec.rs`（925 行、17 单测）已落地（本 crate 自建等价 `SandboxFilesystemSpec` + `SandboxFilesystemAdapter` 实现 harness `SandboxFilesystem`）
   - 「Kubernetes 文件 API」→ 实际 `src/kubernetes_files.rs`（826 行、20 单测）已落地
   - 二者**均在 `lib.rs` 声明**（我先前曾据旧 tracker 以为未实现）
   - 连带：§1 基线 71→**108**，文件清单 10 个/171KB → **12 个/6069 行**
   - **汇总 §1 SANDBOX 由「部分完成」→「已完成」**（汇总低于独立 tracker，允许修）
2. **MODEL `dashscope` 描述彻底过时**：tracker 称「仅 DTO、未接线、未编译」，并列为 §3.1 阻塞项。实测 `src/dashscope/` 已有 **6 个子模块**（converter/credential/dto/model/parser/provider，33 单测）且在 `lib.rs` 声明。→ 基线 25→**58**，文件 12/2016→**17/3954**
3. **RAG `ragflow` 标「未迁移」实则已落地**：`src/ragflow/` 全量（含 `data: false|null|object` 多态反序列化），已在 `lib.rs` 声明。→ 基线 101→**152**，文件 18/3705→**26/6375**
4. REDIS §4.1 全量基线 1485→**1866**

## 实测（不臆造）
- `cargo test --workspace -j 2 --no-fail-fast` → **1866 passed / 0 failed / 86 targets**（1 ignored 为 `platform_channel.rs` 的 ```ignore 文档示例，早于基线即存在）
- 逐文件 `#[test]`+`#[tokio::test]` 计数与 crate 粒度实测**逐项吻合**：MODEL 58、RAG 152、SANDBOX 108

## 教训（新增）
- **独立 tracker 也会过时得离谱**：SANDBOX 把已实现的两整个模块（1751 行、37 单测）标为「待办」；MODEL/RAG 把已完成的子模块描述为「未迁移/阻塞」。**「单一事实来源」≠「可信」——引用前必须以源码 + `lib.rs` 的 `mod` 声明复核**。
- **改文档要扫全文**：我已在 MODEL §1.2/§3.1 更正后，`grep` 残留才发现 §2.2 表格里还有 16 处「未接线/待办」（同段落的漏网）。**更正一处描述后，必须 grep 同义旧措辞全文清零**（本轮用了 `未接线|接线缺陷|仅 DTO|25 passed|101 passed|71 passed|10 个源文件|18 个源文件`）。
- 保留「保留备查」的历史基线（如汇总 §3.2 的 1485、第五/六/七批）属正常留痕，**不清零**，与残留错误可凭上下文区分（前者明确标注「保留备查」）。
