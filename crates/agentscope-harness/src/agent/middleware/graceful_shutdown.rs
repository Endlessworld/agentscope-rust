//! `GracefulShutdownMiddleware` — 把优雅关闭接入 agent 调用链。
//!
//! 对齐 Java `io.agentscope.core.shutdown.GracefulShutdownMiddleware` 的语义：
//!
//! - **一次请求**开始时登记（per-request 粒度，用请求 id 而不是 agent id），
//!   结束时注销；
//! - 注销由 RAII 守卫兜底：正常结束、错误、取消（事件流被丢弃）三条路径都会注销，
//!   不需要在多个分支里手写；
//! - manager 处于 `ShuttingDown` / `Terminated` 时**拒绝新请求**并给出明确错误
//!   （与 core 的 `ensure_accepting_requests()` 语义一致）。
//!
//! Java 侧把"在安全点中断推理/执行"也放在这个中间件里（`onReasoning` /
//! `onActing` 的 `doOnComplete`）。Rust 侧的中断由持有 `RunControl` 的调用方
//! 负责（core 的 `ExecutionGuard` + `ReActAgent::interrupt`），因此这里只负责
//! 请求登记与拒绝新请求。
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{AgentInput, EventStream, ExtensionPoint, Middleware, Next};
use agentscope_core::shutdown::GracefulShutdownManager;
use futures::Stream;

/// 拒绝新请求时使用的错误前缀（便于调用方识别）。
pub const SHUTDOWN_REJECTION_PREFIX: &str = "agent is shutting down";

/// 把优雅关闭接入 agent 调用链的中间件。
pub struct GracefulShutdownMiddleware {
    manager: Arc<GracefulShutdownManager>,
}

impl GracefulShutdownMiddleware {
    /// 使用指定的关闭管理器。
    pub fn new(manager: Arc<GracefulShutdownManager>) -> Self {
        Self { manager }
    }

    /// 使用进程级单例（`GracefulShutdownManager::instance`）。
    pub fn global() -> Self {
        Self {
            manager: GracefulShutdownManager::instance_arc(),
        }
    }

    /// 绑定的关闭管理器。
    pub fn manager(&self) -> &Arc<GracefulShutdownManager> {
        &self.manager
    }
}

impl Middleware for GracefulShutdownMiddleware {
    /// 优先级最高：请求登记必须包住整条调用链（先登记、最后注销）。
    fn order(&self) -> i32 {
        100
    }

    /// 只参与 `OnAgent`：请求登记以"一次 agent 调用"为单位。
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnAgent]
    }

    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let manager = Arc::clone(&self.manager);

        // shutdown 期间拒绝新请求。
        if let Err(_err) = manager.ensure_accepting_requests() {
            let err = Error::ShuttingDown(format!(
                "{SHUTDOWN_REJECTION_PREFIX} (state={:?}); new request rejected by \
                 GracefulShutdownMiddleware",
                manager.get_state()
            ));
            return Box::pin(futures::stream::once(async move { Err(err) }));
        }

        // 登记这一次请求（per-request 粒度）。
        let request_id = format!("req-{}", uuid::Uuid::new_v4());
        manager.register_request_with_id(request_id.clone());

        let inner = next(input);
        Box::pin(TrackedStream {
            inner,
            guard: Some(RequestGuard {
                manager: Arc::clone(&manager),
                request_id,
            }),
        })
    }
}

/// 请求登记的 RAII 守卫：无论正常结束、报错还是被取消，销毁时都会注销请求。
struct RequestGuard {
    manager: Arc<GracefulShutdownManager>,
    request_id: String,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.manager.unregister_request_with_id(&self.request_id);
    }
}

/// 包装内部事件流：流结束（或被丢弃）时释放登记守卫。
struct TrackedStream {
    inner: EventStream,
    guard: Option<RequestGuard>,
}

impl Stream for TrackedStream {
    type Item = Result<AgentEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(None) => {
                // 请求正常结束：立刻注销（而不是等到流对象被回收）。
                drop(this.guard.take());
                Poll::Ready(None)
            }
            other => other,
        }
    }
}
