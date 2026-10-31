//! agentscope-extensions-controlplane — bypass-observation data plane SDK that reports a
//! self-deployed AgentScope agent (session snapshots, event stream, effective context) to
//! an controlplane control plane, and serves the `/agentscope/*` HTTP contract in-process.
//!
//! Ported from `io.agentscope.extensions.controlplane`. Progress and any deferred work are
//! tracked in `docs/extensions/CONTROLPLANE__GAP_TRACKER.md`.

pub mod adapter;
pub mod bridge;
pub mod committed_export;
pub mod config;
pub mod error;
pub mod event_journal;
pub mod model;
pub mod store;
pub mod transport;

pub use bridge::ControlPlaneBridge;
pub use committed_export::{ControlPlaneCommittedEventSink, SINK_NAME};
pub use config::{ControlPlaneConfig, ControlPlaneConfigBuilder};
pub use error::{ControlPlaneError, ControlPlaneResult};

use crate::adapter::FrameworkAdapter;

/// Framework identifier reported by the built-in AgentScope adapter.
pub const FRAMEWORK_NAME: &str = "agentscope-rust";

/// Instruments an already-built agent, mirroring `ControlPlane.instrument`.
///
/// No Level-2 events are emitted unless the adapter was wired as a middleware at
/// construction time; see [`ControlPlaneBridge`]. Async because starting the bridge opens
/// its channels (contract server, self-registration).
pub async fn instrument(
    target: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    config: ControlPlaneConfig,
    adapter: Box<dyn FrameworkAdapter>,
) -> ControlPlaneResult<ControlPlaneBridge> {
    let mut bridge = ControlPlaneBridge::new(config);
    bridge.attach(target, adapter)?;
    bridge.start().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn instrument_requires_handleable_target() {
        struct RejectAdapter;
        use async_trait::async_trait;
        #[async_trait]
        impl FrameworkAdapter for RejectAdapter {
            fn framework_name(&self) -> &str {
                FRAMEWORK_NAME
            }
            fn capabilities(&self) -> Vec<String> {
                vec![]
            }
            fn can_handle(&self, _t: &(dyn std::any::Any + Send + Sync)) -> bool {
                false
            }
            fn attach(
                &mut self,
                _t: std::sync::Arc<dyn std::any::Any + Send + Sync>,
                _e: std::sync::Arc<dyn Fn(model::SessionEvent) + Send + Sync>,
            ) -> ControlPlaneResult<()> {
                Ok(())
            }
            fn detach(&self) -> ControlPlaneResult<()> {
                Ok(())
            }
            async fn extract_context(&self, s: &str) -> ControlPlaneResult<model::ContextSnapshot> {
                Ok(model::ContextSnapshot::builder(s).build())
            }
        }
        let cfg = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .build()
            .unwrap();
        let target: std::sync::Arc<dyn std::any::Any + Send + Sync> = std::sync::Arc::new(42u32);
        assert!(instrument(target, cfg, Box::new(RejectAdapter))
            .await
            .is_err());
    }
}
