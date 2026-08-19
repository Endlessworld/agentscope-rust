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

    pub fn time(mut self, time: f64) -> Self {
        self.inner.time = time;
        self
    }

    pub fn build(self) -> ChatUsage {
        self.inner
    }
}
