//! Reward calculation for shadow agents, mirroring
//! `io.agentscope.core.training.reward.RewardCalculator`.

use agentscope_core::ReActAgent;

/// Calculates a reward value from a shadow agent's execution result.
///
/// Mirrors the Java `RewardCalculator` interface. Where Java takes an `Agent`,
/// Rust takes the concrete [`ReActAgent`] produced by
/// [`crate::runner::AgentCloner`].
pub trait RewardCalculator: Send + Sync {
    /// Calculates the reward for the given shadow agent (typically in `[-1, 1]`).
    fn calculate(&self, agent: &ReActAgent) -> f64;
}

/// Adapter turning a plain closure into a [`RewardCalculator`].
///
/// Mirrors the Java lambda usage `agent -> 0.0`.
pub struct FnRewardCalculator<F> {
    f: F,
}

impl<F> FnRewardCalculator<F>
where
    F: Fn(&ReActAgent) -> f64 + Send + Sync,
{
    /// Wraps a reward function.
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F> RewardCalculator for FnRewardCalculator<F>
where
    F: Fn(&ReActAgent) -> f64 + Send + Sync,
{
    fn calculate(&self, agent: &ReActAgent) -> f64 {
        (self.f)(agent)
    }
}

/// Constantly returns `0.0`, useful as a placeholder reward calculator.
pub struct ZeroRewardCalculator;

impl RewardCalculator for ZeroRewardCalculator {
    fn calculate(&self, _agent: &ReActAgent) -> f64 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
    use agentscope_core::message::{ChatUsage, Msg};
    use agentscope_core::error::Result;
    use futures::stream;
    use std::sync::Arc;

    struct FakeModel;
    impl Model for FakeModel {
        fn stream(
            &self,
            _m: Vec<Msg>,
            _t: Vec<ToolSchema>,
            _o: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
            Box::pin(stream::once(async {
                Ok(ChatResponse::new("id", vec![], ChatUsage::default(), None, None))
            }))
        }
        fn model_name(&self) -> &str {
            "fake"
        }
    }

    fn agent() -> ReActAgent {
        ReActAgent::builder()
            .name("a")
            .model(Arc::new(FakeModel))
            .build()
            .unwrap()
    }

    #[test]
    fn fn_calculator_delegates() {
        let calc = FnRewardCalculator::new(|_a: &ReActAgent| 0.42);
        assert_eq!(calc.calculate(&agent()), 0.42);
    }

    #[test]
    fn zero_calculator() {
        assert_eq!(ZeroRewardCalculator.calculate(&agent()), 0.0);
    }

    #[test]
    fn fn_calculator_can_inspect_agent() {
        let calc = FnRewardCalculator::new(|a: &ReActAgent| {
            if a.name() == "a" {
                1.0
            } else {
                -1.0
            }
        });
        assert_eq!(calc.calculate(&agent()), 1.0);
    }
}
