use serde::{Deserialize, Serialize};

/// Token usage information for chat completion responses, mirroring `ChatUsage`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    /// Input tokens used to create a prompt cache entry (a subset of
    /// [`input_tokens`](Self::input_tokens), not an additional amount). `0` when the
    /// provider does not report this metric. Mirrors Java
    /// `ChatUsage.getCacheCreationTokens`.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Output tokens used for model reasoning (a subset of
    /// [`output_tokens`](Self::output_tokens), not an additional amount). `0` when the
    /// provider does not report this metric. Mirrors Java
    /// `ChatUsage.getReasoningTokens`.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Input tokens representing tool results returned to the model (a subset of
    /// [`input_tokens`](Self::input_tokens), not an additional amount). `0` when the
    /// provider does not report this metric. Mirrors Java
    /// `ChatUsage.getToolUsePromptTokens`.
    #[serde(default)]
    pub tool_use_prompt_tokens: u64,
    #[serde(default)]
    pub time: f64,
}

impl ChatUsage {
    pub fn builder() -> ChatUsageBuilder {
        ChatUsageBuilder::default()
    }

    /// Total number of tokens used (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Default)]
pub struct ChatUsageBuilder {
    inner: ChatUsage,
}

impl ChatUsageBuilder {
    pub fn input_tokens(mut self, input_tokens: u64) -> Self {
        self.inner.input_tokens = input_tokens;
        self
    }

    pub fn output_tokens(mut self, output_tokens: u64) -> Self {
        self.inner.output_tokens = output_tokens;
        self
    }

    pub fn cached_tokens(mut self, cached_tokens: u64) -> Self {
        self.inner.cached_tokens = cached_tokens;
        self
    }

    /// Sets the number of input tokens used to create a prompt cache entry (a
    /// subset of `input_tokens`).
    pub fn cache_creation_tokens(mut self, cache_creation_tokens: u64) -> Self {
        self.inner.cache_creation_tokens = cache_creation_tokens;
        self
    }

    /// Sets the number of output tokens used for model reasoning (a subset of
    /// `output_tokens`).
    pub fn reasoning_tokens(mut self, reasoning_tokens: u64) -> Self {
        self.inner.reasoning_tokens = reasoning_tokens;
        self
    }

    /// Sets the number of input tokens representing tool results returned to the
    /// model (a subset of `input_tokens`).
    pub fn tool_use_prompt_tokens(mut self, tool_use_prompt_tokens: u64) -> Self {
        self.inner.tool_use_prompt_tokens = tool_use_prompt_tokens;
        self
    }

    pub fn time(mut self, time: f64) -> Self {
        self.inner.time = time;
        self
    }

    pub fn build(self) -> ChatUsage {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detailed_token_metrics_round_trip() {
        let usage = ChatUsage::builder()
            .input_tokens(10)
            .output_tokens(20)
            .cached_tokens(3)
            .cache_creation_tokens(4)
            .reasoning_tokens(5)
            .tool_use_prompt_tokens(6)
            .time(1.25)
            .build();
        let value = serde_json::to_value(usage).unwrap();
        // Java `@JsonProperty` names are camelCase, matching this struct's rename_all.
        assert_eq!(value["cacheCreationTokens"], json!(4));
        assert_eq!(value["reasoningTokens"], json!(5));
        assert_eq!(value["toolUsePromptTokens"], json!(6));
        let back: ChatUsage = serde_json::from_value(value).unwrap();
        assert_eq!(back, usage);
        assert_eq!(back.cache_creation_tokens, 4);
        assert_eq!(back.reasoning_tokens, 5);
        assert_eq!(back.tool_use_prompt_tokens, 6);
    }

    #[test]
    fn legacy_json_without_detailed_metrics_defaults_to_zero() {
        // Serialized by an older version (or a provider that omits the metrics).
        let legacy = json!({
            "inputTokens": 7,
            "outputTokens": 8,
            "cachedTokens": 1,
            "time": 0.5
        });
        let usage: ChatUsage = serde_json::from_value(legacy).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
        assert_eq!(usage.tool_use_prompt_tokens, 0);
    }

    #[test]
    fn default_usage_has_zero_breakdowns() {
        let usage = ChatUsage::default();
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
        assert_eq!(usage.tool_use_prompt_tokens, 0);
    }
}
