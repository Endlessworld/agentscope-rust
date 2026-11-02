//! Host bridge contract for the XXL-Job **executor protocol**.
//!
//! Java delegates the whole executor protocol to `xxl-job-core`
//! (`XxlJobExecutor`): the SDK opens the HTTP endpoints the admin console calls
//! and `XxlJobAgentScheduler#registerJobHandler` only registers a handler. This
//! crate deliberately embeds **no HTTP server or client** (see tracker
//! SCHED-06), so the transport stays in the host and the crate owns just the
//! contract and the field mapping.
//!
//! A host implements the endpoints below with its own HTTP stack and forwards
//! to [`ExecutorBridge`]:
//!
//! | admin endpoint | request | bridge |
//! |---|---|---|
//! | `POST /api/registry` | `{registryGroup, registryKey, registryValue}` | host only — registers the executor address in the console, no scheduler state |
//! | `POST /api/beat` | `{registryGroup, registryKey, registryValue}` | [`ExecutorBridge::idle_beat`] (reply `{code, msg}`) |
//! | `POST /api/idleBeat` | `{jobId}` | [`ExecutorBridge::idle_beat`] |
//! | `POST /api/run` | [`DispatchRequest`] (= Java `TriggerParam`) | [`ExecutorBridge::run`] |
//! | `POST /api/kill` | `{jobId}` | [`ExecutorBridge::kill`] |
//! | `POST /api/log` | `{logId, logDateTime, fromLineNum}` | host only — keep the `Arc<XxlJobContext>` passed to `trigger_with_context` and serve its `logs()` |
//!
//! Field mapping for `/api/run` (all camelCase on the wire, as XXL-Job sends
//! them):
//!
//! | `TriggerParam` / [`DispatchRequest`] | [`XxlJobContext`] | notes |
//! |---|---|---|
//! | `jobId` | `job_id()` | echoed back in the dispatch log |
//! | `executorHandler` | — | selects the registered handler (= agent name) |
//! | `executorParams` | `job_param()` | becomes the agent's input message |
//! | `broadcastIndex` | `shard_index()` | XXL-Job carries sharding in the broadcast fields |
//! | `broadcastTotal` | `shard_total()` | `<= 0` normalises to `1` |
//! | `logId` | `log_id()` | console log correlation |
//! | `executorBlockStrategy` / `executorTimeout` / `glueType` / `glueSource` / `glueUpdatetime` / `logDateTime` | — | no Rust counterpart (no job threads, no GLUE, in-flight cancellation unsupported) |
//!
//! The reply body of `/run`, `/idleBeat` and `/kill` is [`ExecuteResponse`]
//! (Java `ReturnT<String>`): `200` = success, `500` = failure.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::context::{XxlJobContext, XxlJobHandleCode};
use super::XxlJobAgentScheduler;

/// The `/api/run` request body (Java `com.xxl.job.core.biz.model.TriggerParam`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DispatchRequest {
    /// The console job id.
    pub job_id: i32,
    /// The registered handler name (= agent name).
    pub executor_handler: String,
    /// The job parameter; becomes the agent's input message.
    pub executor_params: String,
    /// `SERIAL_EXECUTION` / `DISCARD_LATER` / `COVER_EARLY`.
    pub executor_block_strategy: String,
    /// Per-dispatch timeout in seconds (`0` = console default).
    pub executor_timeout: i32,
    /// The console log id.
    pub log_id: i64,
    /// The console log timestamp (ms).
    pub log_date_time: i64,
    /// `BEAN` / `GLUE(Java)` …
    pub glue_type: String,
    /// Inline GLUE source (unused: Rust handlers are always code).
    pub glue_source: String,
    /// GLUE update timestamp.
    pub glue_updatetime: i64,
    /// Sharding index, `0` for non-sharded dispatches.
    pub broadcast_index: i32,
    /// Total shards, `0`/`1` for non-sharded dispatches.
    pub broadcast_total: i32,
}

impl DispatchRequest {
    /// Maps the dispatch body onto the in-crate execution context.
    ///
    /// This is the one place where XXL-Job wire names are translated into the
    /// Rust dispatch metadata; see the module-level table for the full mapping.
    pub fn to_context(&self) -> XxlJobContext {
        let shard_total = if self.broadcast_total > 0 {
            self.broadcast_total
        } else {
            1
        };
        let shard_index = if shard_total > 1 {
            self.broadcast_index.max(0)
        } else {
            0
        };
        XxlJobContext::new(self.job_id as i64, self.executor_params.clone())
            .with_sharding(shard_index, shard_total)
            .with_log_id(self.log_id)
    }
}

/// The reply body of `/api/run`, `/api/idleBeat` and `/api/kill`
/// (Java `ReturnT<String>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// `200` success, `500` failure (`XxlJobHandleCode`).
    pub code: i32,
    /// The human-readable message (`""` on success, like Java's `null` msg).
    pub msg: String,
}

impl ExecuteResponse {
    /// A success reply (`ReturnT.SUCCESS`).
    pub fn success() -> Self {
        Self::from_handle(XxlJobHandleCode::Success, "")
    }

    /// A failure reply (`ReturnT.FAIL`) with a message.
    pub fn fail(msg: impl Into<String>) -> Self {
        Self::from_handle(XxlJobHandleCode::Fail, msg)
    }

    /// Builds a reply from an [`XxlJobHandleCode`] and a message.
    pub fn from_handle(code: XxlJobHandleCode, msg: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            msg: msg.into(),
        }
    }

    /// Whether this reply reports success.
    pub fn is_success(&self) -> bool {
        self.code == XxlJobHandleCode::Success.code()
    }
}

/// The executor endpoints a host must serve, expressed without any transport.
///
/// Implement it with the host's HTTP framework (axum/hyper/…) and forward each
/// request body; [`XxlJobAgentScheduler`] already implements it, so the usual
/// host code is `ExecutorBridge::run(&scheduler, body)`.
#[async_trait]
pub trait ExecutorBridge: Send + Sync {
    /// `POST /api/run`: dispatch one job to the registered handler.
    ///
    /// The handler is selected by [`DispatchRequest::executor_handler`] and runs
    /// inside the context mapped by [`DispatchRequest::to_context`], so
    /// `XxlJobHelper` resolves the console metadata while the agent runs.
    async fn run(&self, request: DispatchRequest) -> ExecuteResponse;

    /// `POST /api/idleBeat` (and `POST /api/beat`): whether the executor is idle
    /// for `job_id`.
    fn idle_beat(&self, job_id: i64) -> ExecuteResponse;

    /// `POST /api/kill`: asks the executor to stop a running job.
    fn kill(&self, job_id: i64) -> ExecuteResponse;
}

#[async_trait]
impl ExecutorBridge for XxlJobAgentScheduler {
    async fn run(&self, request: DispatchRequest) -> ExecuteResponse {
        let handler = request.executor_handler.clone();
        let context = Arc::new(request.to_context());
        match self.trigger_with_context(&handler, context).await {
            Ok(_) => ExecuteResponse::success(),
            Err(err) => ExecuteResponse::fail(err.to_string()),
        }
    }

    fn idle_beat(&self, _job_id: i64) -> ExecuteResponse {
        // Java `XxlJobExecutor#idleBeat`: fail only while the job thread for that
        // id still runs. This crate runs one future per dispatch and keeps no
        // per-job thread table, so the executor is always idle as far as the
        // console can observe (equivalent to Java's "no job thread" branch).
        ExecuteResponse::success()
    }

    fn kill(&self, _job_id: i64) -> ExecuteResponse {
        // Java kills a `JobThread` in the executor's repository; here an
        // execution is a future inside `trigger_with_context` and there is no
        // handle to interrupt by job id (the same reason `cancel` is
        // unsupported, see `XxlJobAgentScheduler::try_cancel`).
        ExecuteResponse::fail(
            "Kill operation is not supported by XxlJobAgentScheduler. \
             Please use XXL-Job admin console to pause or delete the job.",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use agentscope_core::error::Result as CoreResult;
    use agentscope_core::message::Msg;

    use super::*;
    use crate::common::config::{RuntimeAgentConfig, ScheduleConfig};
    use crate::common::scheduler::AgentFactory;
    use crate::xxljob::context::XxlJobHelper;

    /// Factory that records the dispatch metadata visible through `XxlJobHelper`.
    #[derive(Default)]
    struct RecordingFactory {
        calls: AtomicUsize,
        seen: Mutex<Vec<(i64, i32, i32, i64, String)>>,
    }

    #[async_trait]
    impl AgentFactory for RecordingFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, messages: Vec<Msg>) -> CoreResult<Msg> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push((
                XxlJobHelper::get_job_id().unwrap_or(-1),
                XxlJobHelper::get_shard_index().unwrap_or(-1),
                XxlJobHelper::get_shard_total().unwrap_or(-1),
                XxlJobHelper::get_log_id().unwrap_or(-1),
                messages
                    .first()
                    .map(|m| m.text_content())
                    .unwrap_or_default(),
            ));
            Ok(Msg::assistant("ok"))
        }
    }

    fn runtime(name: &str) -> Arc<RuntimeAgentConfig> {
        Arc::new(
            RuntimeAgentConfig::builder()
                .name(name)
                .model_name("m")
                .sys_prompt("sys")
                .model(crate::test_support::stub_model("reasoned"))
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn dispatch_request_maps_onto_the_execution_context() {
        // Wire format is the camelCase `TriggerParam` the console POSTs.
        let json = r#"{"jobId":7,"executorHandler":"A","executorParams":"hello",
            "logId":99,"logDateTime":1700000000000,"broadcastIndex":1,
            "broadcastTotal":3,"glueType":"BEAN","executorTimeout":0}"#;
        let request: DispatchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.job_id, 7);
        assert_eq!(request.executor_handler, "A");
        assert_eq!(request.executor_params, "hello");

        let ctx = request.to_context();
        assert_eq!(ctx.job_id(), 7);
        assert_eq!(ctx.job_param(), "hello");
        assert_eq!(ctx.shard_index(), 1);
        assert_eq!(ctx.shard_total(), 3);
        assert_eq!(ctx.log_id(), 99);

        // A non-sharded dispatch (no `broadcastTotal`) normalises to one shard.
        let plain: DispatchRequest =
            serde_json::from_str(r#"{"jobId":1,"executorHandler":"B"}"#).unwrap();
        let ctx = plain.to_context();
        assert_eq!(ctx.job_id(), 1);
        assert_eq!(ctx.shard_index(), 0);
        assert_eq!(ctx.shard_total(), 1);
        assert_eq!(ctx.log_id(), 0);
    }

    #[tokio::test]
    async fn bridge_run_dispatches_with_the_mapped_context() {
        let factory = Arc::new(RecordingFactory::default());
        let scheduler = Arc::new(XxlJobAgentScheduler::new(factory.clone()));
        scheduler
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();

        let bridge: &dyn ExecutorBridge = scheduler.as_ref();
        let response = bridge
            .run(DispatchRequest {
                job_id: 7,
                executor_handler: "A".to_string(),
                executor_params: "hello".to_string(),
                log_id: 99,
                broadcast_index: 1,
                broadcast_total: 3,
                ..Default::default()
            })
            .await;

        assert!(response.is_success(), "response: {response:?}");
        assert_eq!(response.code, 200);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            factory.seen.lock().unwrap().clone(),
            vec![(7, 1, 3, 99, "hello".to_string())],
            "the /run body must reach the handler as dispatch metadata"
        );
    }

    #[tokio::test]
    async fn bridge_run_reports_500_for_an_unknown_handler() {
        let scheduler = XxlJobAgentScheduler::new(Arc::new(RecordingFactory::default()));
        let bridge: &dyn ExecutorBridge = &scheduler;
        let response = bridge
            .run(DispatchRequest {
                job_id: 3,
                executor_handler: "missing".to_string(),
                ..Default::default()
            })
            .await;
        assert!(!response.is_success());
        assert_eq!(response.code, 500);
        assert!(response.msg.contains("missing"), "msg: {}", response.msg);
    }

    #[tokio::test]
    async fn idle_beat_and_kill_report_the_expected_codes() {
        let scheduler = XxlJobAgentScheduler::new(Arc::new(RecordingFactory::default()));
        let bridge: &dyn ExecutorBridge = &scheduler;

        assert!(bridge.idle_beat(1).is_success());

        // No per-job thread to kill: the endpoint reports the same unsupported
        // semantics as `cancel` (Java's `XxlJobAgentScheduler` has no kill).
        let kill = bridge.kill(1);
        assert_eq!(kill.code, 500);
        assert!(
            kill.msg.contains("Kill operation is not supported"),
            "msg: {}",
            kill.msg
        );
    }
}
