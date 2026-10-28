//! In-crate port of the XXL-Job execution context and helper API
//! (`com.xxl.job.core.context.XxlJobContext` / `XxlJobHelper`).
//!
//! Java exposes both as static/thread-local helpers: `XxlJobContext` carries the
//! dispatch metadata (job id, job param, sharding index/total, log id) and
//! `XxlJobHelper.log(...)` / `XxlJobHelper.handleSuccess(...)` write back to the
//! currently executing job. Rust has no thread-locals across `.await` points, so
//! the equivalent is a [`tokio::task_local!`] scope: an execution runs inside
//! [`XxlJobContext::scope`], and helper calls resolve the context of the current
//! task (exactly the way Java resolves the thread-local one).

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

tokio::task_local! {
    static CURRENT: Arc<XxlJobContext>;
}

/// Outcome reported back to the XXL-Job admin console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XxlJobHandleCode {
    /// The job ran to completion (`XxlJobHelper.handleSuccess`).
    Success,
    /// The job failed (`XxlJobHelper.handleFail`).
    Fail,
}

impl XxlJobHandleCode {
    /// The XXL-Job wire code (`200` success, `500` fail).
    pub fn code(&self) -> i32 {
        match self {
            XxlJobHandleCode::Success => 200,
            XxlJobHandleCode::Fail => 500,
        }
    }

    /// Whether this is a success code.
    pub fn is_success(&self) -> bool {
        matches!(self, XxlJobHandleCode::Success)
    }
}

/// The result a handler reported through [`XxlJobHelper`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XxlJobHandleResult {
    code: XxlJobHandleCode,
    message: String,
}

impl XxlJobHandleResult {
    /// Creates a handle result.
    pub fn new(code: XxlJobHandleCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    /// The reported code.
    pub fn code(&self) -> XxlJobHandleCode {
        self.code
    }
    /// The reported message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Mutable state shared by a context and every helper call made inside it.
#[derive(Debug, Default)]
struct ExecutionState {
    logs: Mutex<Vec<String>>,
    handle: Mutex<Option<XxlJobHandleResult>>,
}

/// Metadata for one XXL-Job dispatch, mirroring `XxlJobContext`.
#[derive(Debug, Clone)]
pub struct XxlJobContext {
    job_id: i64,
    job_param: String,
    shard_index: i32,
    shard_total: i32,
    log_id: i64,
    state: Arc<ExecutionState>,
}

impl fmt::Display for XxlJobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XxlJobContext {{ job_id: {}, log_id: {}, job_param: {:?}, shard_index: {}/{}, logs: {}, handle: {:?} }}",
            self.job_id,
            self.log_id,
            self.job_param,
            self.shard_index,
            self.shard_total,
            self.logs().len(),
            self.handle_result()
        )
    }
}

impl XxlJobContext {
    /// Creates a context for one dispatch.
    ///
    /// `job_param` is what the XXL-Job console sends with the trigger and becomes
    /// the agent's input message (`XxlJobContext.getJobParam()`).
    pub fn new(job_id: i64, job_param: impl Into<String>) -> Self {
        Self {
            job_id,
            job_param: job_param.into(),
            shard_index: 0,
            shard_total: 1,
            log_id: 0,
            state: Arc::new(ExecutionState::default()),
        }
    }

    /// Sets the sharding index/total (`XxlJobContext.getShardIndex/Total`).
    pub fn with_sharding(mut self, index: i32, total: i32) -> Self {
        self.shard_index = index;
        self.shard_total = total;
        self
    }

    /// Sets the log id (`XxlJobContext.getLogId`).
    pub fn with_log_id(mut self, log_id: i64) -> Self {
        self.log_id = log_id;
        self
    }

    /// The dispatch job id.
    pub fn job_id(&self) -> i64 {
        self.job_id
    }

    /// The trigger parameter.
    pub fn job_param(&self) -> &str {
        &self.job_param
    }

    /// The sharding index of this dispatch.
    pub fn shard_index(&self) -> i32 {
        self.shard_index
    }

    /// The total number of shards.
    pub fn shard_total(&self) -> i32 {
        self.shard_total
    }

    /// The log id used by the admin console.
    pub fn log_id(&self) -> i64 {
        self.log_id
    }

    /// The log lines written via [`XxlJobHelper::log`].
    pub fn logs(&self) -> Vec<String> {
        self.state
            .logs
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default()
    }

    /// The handle result reported via [`XxlJobHelper::handle_success`] /
    /// [`XxlJobHelper::handle_fail`].
    pub fn handle_result(&self) -> Option<XxlJobHandleResult> {
        self.state.handle.lock().ok().and_then(|h| h.clone())
    }

    pub(crate) fn append_log(&self, message: impl Into<String>) {
        if let Ok(mut logs) = self.state.logs.lock() {
            logs.push(message.into());
        }
    }

    pub(crate) fn set_handle(&self, result: XxlJobHandleResult) {
        if let Ok(mut handle) = self.state.handle.lock() {
            *handle = Some(result);
        }
    }

    /// Runs `future` with `context` bound as the current XXL-Job context.
    pub async fn scope<F: Future>(context: Arc<Self>, future: F) -> F::Output {
        CURRENT.scope(context, future).await
    }

    /// The context of the currently executing dispatch, if any.
    pub fn current() -> Option<Arc<XxlJobContext>> {
        CURRENT.try_with(|ctx| ctx.clone()).ok()
    }
}

/// Static helper API mirroring `com.xxl.job.core.context.XxlJobHelper`.
///
/// Every call resolves [`XxlJobContext::current`]; calls made outside a dispatch
/// are no-ops (Java behaves the same: it returns early when the thread-local is
/// empty).
pub struct XxlJobHelper;

impl XxlJobHelper {
    /// Appends a line to the current job's log (`XxlJobHelper.log`).
    pub fn log(message: impl Into<String>) {
        if let Some(ctx) = XxlJobContext::current() {
            ctx.append_log(message);
        }
    }

    /// Marks the current job successful (`XxlJobHelper.handleSuccess`).
    pub fn handle_success(message: impl Into<String>) {
        if let Some(ctx) = XxlJobContext::current() {
            ctx.set_handle(XxlJobHandleResult::new(XxlJobHandleCode::Success, message));
        }
    }

    /// Marks the current job failed (`XxlJobHelper.handleFail`).
    pub fn handle_fail(message: impl Into<String>) {
        if let Some(ctx) = XxlJobContext::current() {
            ctx.set_handle(XxlJobHandleResult::new(XxlJobHandleCode::Fail, message));
        }
    }

    /// The current job parameter (`XxlJobHelper.getJobParam`).
    pub fn get_job_param() -> Option<String> {
        XxlJobContext::current().map(|ctx| ctx.job_param().to_string())
    }

    /// The current job id (`XxlJobHelper.getJobId`).
    pub fn get_job_id() -> Option<i64> {
        XxlJobContext::current().map(|ctx| ctx.job_id())
    }

    /// The current sharding index (`XxlJobHelper.getShardIndex`).
    pub fn get_shard_index() -> Option<i32> {
        XxlJobContext::current().map(|ctx| ctx.shard_index())
    }

    /// The current shard total (`XxlJobHelper.getShardTotal`).
    pub fn get_shard_total() -> Option<i32> {
        XxlJobContext::current().map(|ctx| ctx.shard_total())
    }

    /// The current log id (`XxlJobHelper.getLogId`).
    pub fn get_log_id() -> Option<i64> {
        XxlJobContext::current().map(|ctx| ctx.log_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_resolves_the_scoped_context() {
        let ctx = Arc::new(
            XxlJobContext::new(42, "payload")
                .with_sharding(1, 3)
                .with_log_id(7001),
        );
        let ctx_for_run = ctx.clone();

        XxlJobContext::scope(ctx_for_run, async {
            assert_eq!(XxlJobHelper::get_job_id(), Some(42));
            assert_eq!(XxlJobHelper::get_job_param(), Some("payload".to_string()));
            assert_eq!(XxlJobHelper::get_shard_index(), Some(1));
            assert_eq!(XxlJobHelper::get_shard_total(), Some(3));
            assert_eq!(XxlJobHelper::get_log_id(), Some(7001));

            XxlJobHelper::log("step 1");
            XxlJobHelper::log("step 2");
            XxlJobHelper::handle_success("done");
        })
        .await;

        assert_eq!(ctx.logs(), vec!["step 1".to_string(), "step 2".to_string()]);
        let handle = ctx.handle_result().expect("handle result set");
        assert_eq!(handle.code(), XxlJobHandleCode::Success);
        assert_eq!(handle.code().code(), 200);
        assert!(handle.code().is_success());
        assert_eq!(handle.message(), "done");
        assert_eq!(ctx.job_param(), "payload");
        assert_eq!(ctx.job_id(), 42);
    }

    #[tokio::test]
    async fn helpers_are_noops_outside_a_scope() {
        assert!(XxlJobContext::current().is_none());
        assert!(XxlJobHelper::get_job_param().is_none());
        // Must not panic without a context.
        XxlJobHelper::log("ignored");
        XxlJobHelper::handle_fail("ignored");
        assert!(XxlJobHelper::get_job_id().is_none());
    }

    #[tokio::test]
    async fn fail_code_is_500() {
        let ctx = Arc::new(XxlJobContext::new(1, ""));
        let run = ctx.clone();
        XxlJobContext::scope(run, async {
            XxlJobHelper::handle_fail("boom");
        })
        .await;
        let handle = ctx.handle_result().unwrap();
        assert_eq!(handle.code(), XxlJobHandleCode::Fail);
        assert_eq!(handle.code().code(), 500);
        assert!(!handle.code().is_success());
        assert_eq!(handle.message(), "boom");
    }
}
