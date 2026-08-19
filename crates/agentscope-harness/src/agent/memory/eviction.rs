//! Per-tool-result eviction of oversized outputs, mirroring `ToolResultEvictionConfig`.
//!
//! When a tool produces a result whose text content exceeds `max_result_chars`, the
//! framework may write the full output to the workspace at `eviction_path` and replace
//! the in-context tool result with a compact head+tail preview placeholder. This is
//! orthogonal to conversation summarization: eviction addresses context *width*
//! (single oversized messages) while compaction addresses context *depth*.

use std::collections::HashSet;

/// ~20 K tokens × 4 chars/token — default eviction threshold.
pub const DEFAULT_MAX_RESULT_CHARS: usize = 80_000;

/// Characters shown at the head and tail of an eviction placeholder preview.
pub const DEFAULT_PREVIEW_CHARS: usize = 2_000;

/// Workspace-relative path prefix under which evicted results are stored.
pub const DEFAULT_EVICTION_PATH: &str = "large_tool_results";

/// Tools excluded from eviction by default (small or self-paginating).
pub fn default_excluded_tools() -> HashSet<String> {
    [
        "read_file",
        "write_file",
        "edit_file",
        "grep_files",
        "glob_files",
        "list_files",
        "memory_search",
        "memory_get",
        "session_search",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}


/// Configuration for per-tool-result eviction of oversized outputs.
#[derive(Debug, Clone)]
pub struct ToolResultEvictionConfig {
    /// Maximum text length (chars) before eviction fires.
    pub max_result_chars: usize,
    /// Characters shown in the head and tail preview of the placeholder.
    pub preview_chars: usize,
    /// Root path prefix under which evicted files are written.
    pub eviction_path: String,
    /// Tool names that are never evicted regardless of result size.
    pub excluded_tool_names: HashSet<String>,
}

impl ToolResultEvictionConfig {
    /// Creates a config with all defaults applied.
    pub fn defaults() -> Self {
        Self::builder().build()
    }

    /// Starts a builder for [`ToolResultEvictionConfig`].
    pub fn builder() -> ToolResultEvictionConfigBuilder {
        ToolResultEvictionConfigBuilder::default()
    }

    /// Whether a result from `tool_name` with `output_chars` characters should be evicted.
    pub fn should_evict(&self, tool_name: &str, output_chars: usize) -> bool {
        output_chars > self.max_result_chars && !self.excluded_tool_names.contains(tool_name)
    }

    /// Renders the compact placeholder that replaces an evicted tool result.
    pub fn build_placeholder(&self, head: &str, tail: &str) -> String {
        format!(
            "[tool_result evicted: output exceeded {} chars. Preview:\n{}\n...\n{}\nUse `read_file` to view the full content stored under `{}`.]",
            self.max_result_chars, head, tail, self.eviction_path
        )
    }
}

/// Builder for [`ToolResultEvictionConfig`].
#[derive(Debug, Clone)]
pub struct ToolResultEvictionConfigBuilder {
    pub max_result_chars: usize,
    pub preview_chars: usize,
    pub eviction_path: String,
    pub excluded_tool_names: HashSet<String>,
}

impl Default for ToolResultEvictionConfigBuilder {
    fn default() -> Self {
        Self {
            max_result_chars: DEFAULT_MAX_RESULT_CHARS,
            preview_chars: DEFAULT_PREVIEW_CHARS,
            eviction_path: DEFAULT_EVICTION_PATH.to_string(),
            excluded_tool_names: default_excluded_tools(),
        }
    }
}

impl ToolResultEvictionConfigBuilder {
    pub fn max_result_chars(mut self, v: usize) -> Self {
        self.max_result_chars = v;
        self
    }
    pub fn preview_chars(mut self, v: usize) -> Self {
        self.preview_chars = v;
        self
    }
    pub fn eviction_path(mut self, v: impl Into<String>) -> Self {
        self.eviction_path = v.into();
        self
    }
    pub fn excluded_tool_names(mut self, v: HashSet<String>) -> Self {
        self.excluded_tool_names = v;
        self
    }
    pub fn build(self) -> ToolResultEvictionConfig {
        ToolResultEvictionConfig {
            max_result_chars: self.max_result_chars,
            preview_chars: self.preview_chars,
            eviction_path: self.eviction_path,
            excluded_tool_names: self.excluded_tool_names,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_evict_large_non_excluded_results() {
        let cfg = ToolResultEvictionConfig::defaults();
        assert_eq!(cfg.max_result_chars, 80_000);
        assert_eq!(cfg.eviction_path, "large_tool_results");
        assert!(cfg.excluded_tool_names.contains("read_file"));
        // execute (shell) is NOT excluded and can be evicted.
        assert!(!cfg.excluded_tool_names.contains("execute"));
        assert!(cfg.should_evict("execute", 100_000));
        // read_file is excluded regardless of size.
        assert!(!cfg.should_evict("read_file", 1_000_000));
        // Under the threshold, nothing is evicted.
        assert!(!cfg.should_evict("execute", 1_000));
    }

    #[test]
    fn builder_overrides_threshold_and_path() {
        let cfg = ToolResultEvictionConfig::builder()
            .max_result_chars(1_000)
            .preview_chars(100)
            .eviction_path("blobs")
            .build();
        assert_eq!(cfg.max_result_chars, 1_000);
        assert_eq!(cfg.preview_chars, 100);
        assert_eq!(cfg.eviction_path, "blobs");
        assert!(cfg.should_evict("execute", 2_000));
    }

    #[test]
    fn placeholder_includes_head_tail_and_path() {
        let cfg = ToolResultEvictionConfig::defaults();
        let p = cfg.build_placeholder("HEAD", "TAIL");
        assert!(p.contains("HEAD"));
        assert!(p.contains("TAIL"));
        assert!(p.contains("read_file"));
        assert!(p.contains("large_tool_results"));
    }
}
