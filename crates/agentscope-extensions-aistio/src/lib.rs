//! agentscope-extensions-aistio — bypass-observation data plane SDK that reports a
//! self-deployed AgentScope agent (session snapshots, event stream, effective context) to
//! an aistio control plane, and serves the `/agentscope/*` HTTP contract in-process.
//!
//! Ported from `io.agentscope.extensions.aistio`. Progress and any deferred work are
//! tracked in `docs/extensions/AISTIO__GAP_TRACKER.md`.

pub mod adapter;
pub mod bridge;
pub mod config;
pub mod error;
pub mod event_journal;
pub mod model;
pub mod store;
pub mod transport;

pub use bridge::AistioBridge;
pub use config::{AistioConfig, AistioConfigBuilder};
pub use error::{AistioError, AistioResult};

use crate::adapter::FrameworkAdapter;

/// Framework identifier reported by the built-in AgentScope adapter.
pub const FRAMEWORK_NAME: &str = "agentscope-rust";

/// Instruments an already-built agent, mirroring `Aistio.instrument`.
///
/// No Level-2 events are emitted unless the adapter was wired as a middleware at
/// construction time; see [`AistioBridge`].
pub fn instrument(
    target: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    config: AistioConfig,
    adapter: Box<dyn FrameworkAdapter>,
) -> AistioResult<AistioBridge> {
    let mut bridge = AistioBridge::new(config);
    bridge.attach(target, adapter)?;
    bridge.start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrument_requires_handleable_target() {
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
            ) -> AistioResult<()> {
                Ok(())
            }
            fn detach(&mut self) -> AistioResult<()> {
                Ok(())
            }
            async fn extract_context(&self, s: &str) -> AistioResult<model::ContextSnapshot> {
                Ok(model::ContextSnapshot::builder(s).build())
            }
        }
        let cfg = AistioConfig::builder("friday").instance_key("rep-1").build().unwrap();
        let target: std::sync::Arc<dyn std::any::Any + Send + Sync> = std::sync::Arc::new(42u32);
        assert!(instrument(target, cfg, Box::new(RejectAdapter)).is_err());
    }
}
