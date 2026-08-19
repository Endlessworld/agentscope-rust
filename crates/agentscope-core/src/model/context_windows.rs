//! Context window table for well-known models, mirroring `ModelContextWindows`.
//!
//! Resolution is done by **longest prefix match**: a model id like
//! `gpt-4o-2024-05-13` matches the `gpt-4o` entry; `deepseek-chat` matches the
//! `deepseek` entry. Unknown models fall back to the default (128k).

/// Longest-prefix-match table of model id prefixes to context window token counts.
pub const CONTEXT_WINDOW_TABLE: &[(&str, usize)] = &[
    // OpenAI / GPT
    ("gpt-4o-mini", 128_000),
    ("gpt-4o", 128_000),
    ("gpt-4.1-mini", 1_000_000),
    ("gpt-4.1-nano", 1_000_000),
    ("gpt-4.1", 1_000_000),
    ("gpt-4-turbo", 128_000),
    ("gpt-4", 8_192),
    ("gpt-3.5-turbo", 16_385),
    ("o4-mini", 200_000),
    ("o3-mini", 200_000),
    ("o3", 200_000),
    ("o1", 200_000),
    // DeepSeek
    ("deepseek-reasoner", 64_000),
    ("deepseek-chat", 64_000),
    ("deepseek", 64_000),
    // GLM / Zhipu
    ("glm-4", 128_000),
    ("glm", 128_000),
    // Qwen
    ("qwen-max", 32_000),
    ("qwen-plus", 131_072),
    ("qwen-turbo", 1_000_000),
    ("qwen2.5", 131_072),
    ("qwen", 131_072),
    // Moonshot / Kimi
    ("moonshot", 128_000),
    ("kimi", 128_000),
    // xAI
    ("grok-", 131_072),
    // Meta
    ("llama", 131_072),
    // Anthropic
    ("claude", 200_000),
    // Mistral
    ("mistral", 32_768),
    // Local / Ollama defaults
    ("gemma", 8_192),
    ("phi", 128_000),
];

/// Default context window size (in tokens) used when no prefix matches.
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// Resolves the context window size for the given model id using longest-prefix
/// matching against [`CONTEXT_WINDOW_TABLE`]. Returns [`DEFAULT_CONTEXT_WINDOW`]
/// when no entry matches.
pub fn resolve_context_window(model_id: &str) -> usize {
    CONTEXT_WINDOW_TABLE
        .iter()
        .filter(|(prefix, _)| model_id.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, size)| *size)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(resolve_context_window("gpt-4o-2024-05-13"), 128_000);
        assert_eq!(resolve_context_window("gpt-4o-mini"), 128_000);
        // gpt-4.1 prefixes are longer than gpt-4; must win via length.
        assert_eq!(resolve_context_window("gpt-4.1-2025-04-14"), 1_000_000);
    }

    #[test]
    fn exact_and_family() {
        assert_eq!(resolve_context_window("deepseek-chat"), 64_000);
        assert_eq!(resolve_context_window("claude-3-7-sonnet-20250219"), 200_000);
        assert_eq!(resolve_context_window("grok-3"), 131_072);
    }

    #[test]
    fn unknown_falls_back_to_default() {
        assert_eq!(resolve_context_window("some-custom-model"), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(resolve_context_window(""), DEFAULT_CONTEXT_WINDOW);
    }
}
