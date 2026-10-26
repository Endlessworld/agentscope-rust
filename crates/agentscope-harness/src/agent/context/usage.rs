//! Provider-anchored context accounting.
//!
//! The provider reports exact `usage.input_tokens` for the request it just ran. The next
//! request always differs (the assistant reply, tool results, a new user turn), so this
//! module uses the reported number as the authority for everything up to the message that
//! carried it and only estimates the suffix added afterwards.
//!
//! This is the closest possible count before a request is sent: an exact count for the
//! *next* request is only available from the provider after the call. The local estimator
//! is used only when no provider usage exists yet (first call, or after a compaction
//! invalidated the previous anchor).

use agentscope_core::message::{ChatUsage, Msg, MsgRole};
use agentscope_core::model::ToolSchema;

use super::policy::{ApproximateEstimator, EstimateInput, TokenEstimator};

/// The manifest method string when the count is anchored on provider usage.
pub const PROVIDER_METHOD: &str = "provider-usage-anchored-v1";
/// The manifest method string when only the local estimator is available.
pub const LOCAL_METHOD: &str = "mixed-text-2.5chars-media-sized-v2";

/// Which authority produced a [`ContextUsage`] count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// Real provider `input_tokens` plus a locally estimated suffix.
    ProviderAnchored,
    /// Local character-based estimate (no provider usage available).
    LocalEstimate,
}

/// The current context size, anchored on the latest provider-reported usage when possible.
#[derive(Debug, Clone)]
pub struct ContextUsage {
    /// Estimated input tokens for the request that is about to be sent.
    pub tokens: i64,
    /// Whether `tokens` is anchored on real provider usage.
    pub source: UsageSource,
    /// The provider's exact `input_tokens` for the previous request, when available.
    pub provider_input_tokens: Option<u64>,
    /// The local estimator's count for the whole current request.
    pub local_tokens: i64,
    /// Locally estimated tokens added after the message that carried the usage.
    pub suffix_tokens: i64,
}

impl ContextUsage {
    /// The manifest `countingMethod` string.
    pub fn method(&self) -> &'static str {
        match self.source {
            UsageSource::ProviderAnchored => PROVIDER_METHOD,
            UsageSource::LocalEstimate => LOCAL_METHOD,
        }
    }

    /// Whether real provider usage was available.
    pub fn is_provider_anchored(&self) -> bool {
        self.source == UsageSource::ProviderAnchored
    }
}

/// Counts a request, using the latest provider-reported usage as the anchor.
///
/// `messages` must be the messages that will be sent (including the system message when
/// the caller has one). `tools` are the tool schemas that will be sent. `model` is the
/// live model name; a usage record produced by a different model is ignored because its
/// tokenizer does not apply.
pub fn context_usage(
    messages: &[Msg],
    tools: &[ToolSchema],
    model: Option<&str>,
) -> ContextUsage {
    let estimator = ApproximateEstimator;
    let local_tokens = estimator
        .estimate(&EstimateInput::new(messages, tools))
        .tokens;

    let Some((index, usage)) = last_provider_usage(messages, model) else {
        return ContextUsage {
            tokens: local_tokens,
            source: UsageSource::LocalEstimate,
            provider_input_tokens: None,
            local_tokens,
            suffix_tokens: 0,
        };
    };

    // `input_tokens` covers every message *before* the assistant reply that carried it.
    // The current request additionally contains that reply and everything appended after
    // it; only that suffix needs the local estimator.
    let suffix_tokens = estimator
        .estimate(&EstimateInput::new(&messages[index..], &[]))
        .tokens;
    ContextUsage {
        tokens: usage.input_tokens as i64 + suffix_tokens,
        source: UsageSource::ProviderAnchored,
        provider_input_tokens: Some(usage.input_tokens),
        local_tokens,
        suffix_tokens,
    }
}

fn last_provider_usage(messages: &[Msg], model: Option<&str>) -> Option<(usize, ChatUsage)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if message.role != MsgRole::Assistant {
                return None;
            }
            let usage = message.usage?;
            if usage.input_tokens == 0 {
                return None;
            }
            if let Some(model) = model {
                // Messages recorded before the model tag existed carry no tag and are
                // accepted; a tag that names another model invalidates the anchor.
                if let Some(recorded) = message
                    .metadata
                    .get(Msg::METADATA_USAGE_MODEL)
                    .and_then(|value| value.as_str())
                {
                    if recorded != model {
                        return None;
                    }
                }
            }
            Some((index, usage))
        })
}

/// Removes usage records that no longer describe the current request.
///
/// Compaction replaces the prefix with a summary while retaining a tail. Usage attached
/// to retained messages was produced against the old prefix, so anchoring on it would
/// count a request the provider never saw. The next post-compaction call re-establishes
/// a valid anchor.
pub fn strip_provider_usage(messages: Vec<Msg>) -> Vec<Msg> {
    messages
        .into_iter()
        .map(|mut message| {
            message.usage = None;
            message
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ChatUsage;

    fn usage(input: u64, output: u64) -> ChatUsage {
        ChatUsage {
            input_tokens: input,
            output_tokens: output,
            ..ChatUsage::default()
        }
    }

    fn assistant_with_usage(model: &str) -> Msg {
        let mut message = Msg::assistant("world");
        message.usage = Some(usage(1_000, 10));
        message.metadata.insert(
            Msg::METADATA_USAGE_MODEL.to_string(),
            serde_json::Value::String(model.to_string()),
        );
        message
    }

    #[test]
    fn provider_usage_anchors_the_previous_prefix() {
        let messages = vec![
            Msg::system("sys"),
            Msg::user("hello"),
            assistant_with_usage("model-a"),
        ];
        let usage = context_usage(&messages, &[], Some("model-a"));
        assert!(usage.is_provider_anchored());
        assert_eq!(usage.provider_input_tokens, Some(1_000));
        // The suffix contains the assistant reply, so the count is above the anchor.
        assert!(usage.tokens > 1_000, "{usage:?}");
        assert_eq!(usage.method(), PROVIDER_METHOD);
    }

    #[test]
    fn a_usage_from_another_model_is_ignored() {
        let messages = vec![
            Msg::user("hello"),
            assistant_with_usage("model-a"),
        ];
        let usage = context_usage(&messages, &[], Some("model-b"));
        assert!(!usage.is_provider_anchored());
        assert_eq!(usage.provider_input_tokens, None);
        assert_eq!(usage.method(), LOCAL_METHOD);
        assert_eq!(usage.tokens, usage.local_tokens);
    }

    #[test]
    fn no_provider_usage_falls_back_to_the_local_estimator() {
        let messages = vec![Msg::user("hello")];
        let usage = context_usage(&messages, &[], None);
        assert!(!usage.is_provider_anchored());
        assert_eq!(usage.tokens, usage.local_tokens);
    }

    #[test]
    fn compaction_invalidates_retained_usage() {
        let stripped = strip_provider_usage(vec![assistant_with_usage("model-a")]);
        assert!(stripped[0].usage.is_none());
    }
}
