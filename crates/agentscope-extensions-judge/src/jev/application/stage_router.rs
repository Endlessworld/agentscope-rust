//! Explicit application phase boundaries (`JevStageRouter`).
//!
//! Java's router is a thin wrapper that stamps `jev.stage` onto a child `RuntimeContext` and then
//! delegates to `JevModelRouterMiddleware` from the `example` sub-package. The `example`
//! sub-package is a *demonstration* of the model-routing middleware and has not been migrated
//! (see `docs/extensions/JUDGE__GAP_TRACKER.md` §3.3), and this crate does not depend on the core
//! `RuntimeContext`/middleware traits. The type therefore stays a documented skeleton that fails
//! closed instead of silently routing nothing.

use crate::error::JevError;

/// Why [`JevStageRouter::begin`] cannot produce a usable phase.
pub const STAGE_ROUTER_UNAVAILABLE: &str =
    "JevStageRouter requires JevModelRouterMiddleware from the `example` sub-package (not migrated) and the core RuntimeContext middleware seam";

/// The stage metadata key Java stamps on the child context.
pub const STAGE_METADATA_KEY: &str = "jev.stage";

/// An explicit application phase (`JevStageRouter.Phase`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    /// The stage name.
    pub name: String,
}

/// Explicit application phase boundaries with a sticky routing context per phase.
#[derive(Debug, Default, Clone, Copy)]
pub struct JevStageRouter;

impl JevStageRouter {
    /// Creates the router.
    pub fn new() -> Self {
        Self
    }

    /// Validates the inputs and reports the missing dependency.
    ///
    /// Java validates `stage`/`goal` and then begins a routing scope; the port keeps the validation
    /// (so caller bugs surface identically) and fails closed on the unimplemented delegation.
    pub fn begin(&self, stage: &str, goal: &str) -> Result<Phase, JevError> {
        if stage.trim().is_empty() || goal.trim().is_empty() {
            return Err(JevError::invalid("stage and goal required"));
        }
        Err(JevError::protocol(STAGE_ROUTER_UNAVAILABLE))
    }

    /// The metadata key Java uses for the stage.
    pub fn metadata_key(&self) -> &'static str {
        STAGE_METADATA_KEY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_stages_are_rejected_and_valid_ones_fail_closed() {
        let router = JevStageRouter::new();
        assert_eq!(
            router.begin("", "goal").unwrap_err().message(),
            "stage and goal required"
        );
        assert_eq!(
            router.begin("plan", "  ").unwrap_err().message(),
            "stage and goal required"
        );
        let error = router.begin("plan", "ship it").unwrap_err();
        assert!(error.message().contains("JevModelRouterMiddleware"));
        assert_eq!(error.message(), STAGE_ROUTER_UNAVAILABLE);
        assert_eq!(router.metadata_key(), "jev.stage");
        assert_eq!(std::mem::size_of::<Phase>(), std::mem::size_of::<String>());
    }
}
