//! End-to-end execution tests for the Quartz backend.
//!
//! These exercise the public scheduling API (scheduler -> task -> factory) and
//! therefore fail against the pre-fix code where tasks were built with a
//! `NoopFactory` and the initial input was dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use agentscope_extensions_scheduler::{
    AgentFactory, AgentScheduler, QuartzAgentScheduler, QuartzScheduleAgentTask,
    RuntimeAgentConfig, ScheduleAgentTask, ScheduleConfig,
};

/// Factory that records how often it ran and the messages it received.
#[derive(Default)]
struct RecordingFactory {
    calls: AtomicUsize,
    received: Mutex<Vec<Vec<Msg>>>,
}

impl RecordingFactory {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn received(&self) -> Vec<Vec<Msg>> {
        self.received.lock().unwrap().clone()
    }
}

#[async_trait]
impl AgentFactory for RecordingFactory {
    async fn run(&self, _config: &RuntimeAgentConfig, messages: Vec<Msg>) -> Result<Msg> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received.lock().unwrap().push(messages.clone());
        Ok(Msg::assistant(format!("ok:{}", messages.len())))
    }
}

/// Model stub: execution validation requires a non-null model (Java
/// `Model cannot be null.`), but these tests drive a recording factory and never
/// reach a provider.
struct StubModel;

impl agentscope_core::model::Model for StubModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<agentscope_core::model::ToolSchema>,
        _options: agentscope_core::model::GenerateOptions,
    ) -> futures::stream::BoxStream<'_, Result<agentscope_core::model::ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Ok(agentscope_core::model::ChatResponse::builder()
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("stub"),
                )])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "stub-model"
    }
}

fn runtime(name: &str) -> Arc<RuntimeAgentConfig> {
    Arc::new(
        RuntimeAgentConfig::builder()
            .name(name)
            .model_name("m")
            .sys_prompt("sys")
            .model(Arc::new(StubModel))
            .build()
            .unwrap(),
    )
}

fn quartz_task(task: &Arc<dyn ScheduleAgentTask>) -> &QuartzScheduleAgentTask {
    task.as_any()
        .downcast_ref::<QuartzScheduleAgentTask>()
        .expect("task should be a QuartzScheduleAgentTask")
}

/// Regression for P0-1 / P0-3: scheduler-created tasks must execute through the
/// scheduler's factory and expose the execution count.
///
/// Pre-fix: `trigger()` returned `Err("No agent factory attached ...")`, the
/// factory was never called, and `execution_count()` did not exist at all.
#[tokio::test]
async fn trigger_and_trait_run_use_injected_factory_and_count() {
    let factory = Arc::new(RecordingFactory::default());
    let scheduler = QuartzAgentScheduler::new(factory.clone());

    let task = scheduler
        .schedule_runtime(runtime("Manual"), ScheduleConfig::none(), None)
        .await
        .unwrap();
    let qt = quartz_task(&task);

    let reply = scheduler.trigger("Manual").await.unwrap();
    assert_eq!(reply.text_content(), "ok:0");
    assert_eq!(factory.calls(), 1);
    assert_eq!(qt.execution_count(), 1);

    // `ScheduleAgentTask::run` is the third execution path (trait level).
    let reply = task.run(vec![Msg::user("explicit")]).await.unwrap();
    assert_eq!(reply.text_content(), "ok:1");
    assert_eq!(factory.calls(), 2);
    assert_eq!(qt.execution_count(), 2);
    assert_eq!(factory.received()[1][0].text_content(), "explicit");

    scheduler.shutdown().await;
}

/// Regression for P0-2: `schedule_with_input` must deliver the initial message
/// to the factory.
///
/// Pre-fix: the message was stored in a write-only `runtimes` map and `fire()`
/// called `run_with(Vec::new())`, so the factory (once reachable) saw zero
/// messages. This test asserts the message text, so fixing only P0-1 is not
/// enough to make it pass.
#[tokio::test]
async fn schedule_with_input_delivers_initial_message_to_factory() {
    let factory = Arc::new(RecordingFactory::default());
    let scheduler = QuartzAgentScheduler::new(factory.clone());

    let task = scheduler
        .schedule_runtime(
            runtime("WithInput"),
            ScheduleConfig::none(),
            Some(Msg::user("bootstrap")),
        )
        .await
        .unwrap();
    let qt = quartz_task(&task);

    scheduler.trigger("WithInput").await.unwrap();

    let received = factory.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].len(), 1);
    assert_eq!(received[0][0].text_content(), "bootstrap");
    assert_eq!(qt.execution_count(), 1);

    scheduler.shutdown().await;
}

/// Regression for the automatic job-loop path: a FIXED_DELAY firing must reach
/// the factory and increment the count.
///
/// Pre-fix: the loop fired, `NoopFactory` returned an error, and the count
/// stayed at 0 (the factory was never called).
#[tokio::test]
async fn fixed_delay_firing_reaches_factory() {
    let factory = Arc::new(RecordingFactory::default());
    let scheduler = QuartzAgentScheduler::new(factory.clone());

    let config = ScheduleConfig::builder()
        .fixed_delay(20)
        .initial_delay(10)
        .build()
        .unwrap();
    let task = scheduler
        .schedule_runtime(runtime("Auto"), config, None)
        .await
        .unwrap();
    let qt = quartz_task(&task);

    for _ in 0..100 {
        if qt.execution_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(qt.execution_count() >= 1, "task never fired");
    assert!(factory.calls() >= 1, "factory never called");
    assert_eq!(factory.received()[0].len(), 0);

    scheduler.cancel("Auto").await;
}
