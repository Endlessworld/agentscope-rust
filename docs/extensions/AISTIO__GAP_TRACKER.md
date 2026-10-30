# AISTIO 模块 tracker 已迁移（重定向）

> **本文件仅为保持历史链接可用而保留。**

按 `EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 3，crate 已从 `agentscope-extensions-aistio` 整体改名为 **`agentscope-extensions-controlplane`**（对齐 Java commit `a2cbea5c3`）。

- 新 tracker：**[`CONTROLPLANE__GAP_TRACKER.md`](./CONTROLPLANE__GAP_TRACKER.md)**
- 新 crate：`crates/agentscope-extensions-controlplane/`
- 类型前缀：`Aistio*` → `ControlPlane*`
- 环境变量：`AISTIO_*` → `CONTROL_PLANE_*`

历史内容已全部迁入新文件，请勿在此继续记录。

> **2026-10-31 二轮收口（本模块）**：AISTIO 遗留的两类残留已在 CONTROLPLANE crate 内闭环——① `GrpcTransport`：wire 层维持「设计决定（不迁移）」（Java proto 契约存在：`agentscope-service/service-controlplane/internal/asdp/asdp.proto`，但需 protoc + tonic/prost + 构建期拷贝，且 bidi 流无法走 unary JSON 通道），**语义层已落地** `src/transport/asdp.rs`（handshake/退避/心跳/ack 门控 outbox/下行分发 + `AsdpLink` seam）；② `SessionCompactor` / `SessionHistorySource` / `AgentRuntimeSource` 已提供宿主默认实现（`src/adapter/host_impls.rs`），`AgentTaskStarter` 早已由 `HarnessAgentTaskStarter` 实现。详情见 `CONTROLPLANE__GAP_TRACKER.md` 第 10 节。
