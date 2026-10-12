//! Model-name mapping and reasoning-suffix parsing.
//!
//! A gateway may expose friendly model ids that map onto upstream ones, and
//! several vendors encode reasoning controls in the model name (e.g.
//! `claude-3-7-sonnet-thinking-4096`, `gpt-5-high`). This module provides the
//! chain resolution (with cycle detection) and the suffix parser used when
//! routing a request to an upstream model (mirrors new-api's `model_mapped.go`
//! and `reasoning_suffix.go`).

use std::collections::{HashMap, HashSet};

/// Error resolving a model through a mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMapError {
    /// The mapping table contains a cycle (`a → b → a`).
    Cycle {
        /// The model id at which the cycle was detected.
        model: String,
    },
}

impl std::fmt::Display for ModelMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelMapError::Cycle { model } => {
                write!(f, "model mapping contains a cycle at '{model}'")
            }
        }
    }
}

impl std::error::Error for ModelMapError {}

/// A `requested-model → upstream-model` mapping with chain resolution.
#[derive(Debug, Clone, Default)]
pub struct ModelMap {
    table: HashMap<String, String>,
}

impl ModelMap {
    /// Builds a map from a JSON object of `string → string` entries; non-string
    /// values are ignored.
    pub fn from_json(value: &serde_json::Value) -> Self {
        let mut table = HashMap::new();
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() {
                        table.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        Self { table }
    }

    /// Records one mapping.
    pub fn insert(&mut self, from: impl Into<String>, to: impl Into<String>) {
        self.table.insert(from.into(), to.into());
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Resolves `model` through the mapping chain until a terminal id is reached.
    ///
    /// When the exact id is absent, the trailing reasoning suffix is stripped and
    /// the base name is looked up once (so `gpt-5-high` still matches a mapping
    /// registered for `gpt-5`).
    pub fn resolve(&self, model: &str) -> Result<String, ModelMapError> {
        if self.table.is_empty() {
            return Ok(model.to_string());
        }
        let mut current = model.to_string();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(current.clone());
        loop {
            let next = match self.table.get(&current) {
                Some(next) => next.clone(),
                None => {
                    // Try the base name (reasoning suffix stripped) exactly once.
                    let base = strip_reasoning_suffix(&current).0;
                    match self.table.get(&base) {
                        Some(next) if base != current => next.clone(),
                        _ => break,
                    }
                }
            };
            if next == current {
                break;
            }
            if !visited.insert(next.clone()) {
                return Err(ModelMapError::Cycle { model: next });
            }
            current = next;
        }
        Ok(current)
    }
}

/// Reasoning controls parsed from a model-name suffix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReasoningHint {
    /// Extended-thinking token budget, e.g. `-thinking-4096`.
    pub budget_tokens: Option<u32>,
    /// Reasoning effort, e.g. `-high`.
    pub effort: Option<String>,
}

const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Splits a reasoning suffix off a model id.
///
/// Recognises `-thinking-<n>` (token budget) and a trailing `-<effort>`.
/// Returns the base model id and the parsed hint (`None` when there is no
/// recognised suffix).
pub fn strip_reasoning_suffix(model: &str) -> (String, Option<ReasoningHint>) {
    if let Some(idx) = model.rfind("-thinking") {
        let rest = &model[idx + "-thinking".len()..];
        if let Some(budget) = rest.strip_prefix('-').and_then(|n| n.parse::<u32>().ok()) {
            let base = model[..idx].to_string();
            if !base.is_empty() {
                return (
                    base,
                    Some(ReasoningHint { budget_tokens: Some(budget), effort: None }),
                );
            }
        } else if rest.is_empty() {
            return (
                model[..idx].to_string(),
                Some(ReasoningHint { budget_tokens: None, effort: None }),
            );
        }
    }
    if let Some(idx) = model.rfind('-') {
        let suffix = &model[idx + 1..];
        if EFFORTS.contains(&suffix.to_ascii_lowercase().as_str()) && idx > 0 {
            return (
                model[..idx].to_string(),
                Some(ReasoningHint { budget_tokens: None, effort: Some(suffix.to_ascii_lowercase()) }),
            );
        }
    }
    (model.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_chain() {
        let mut map = ModelMap::default();
        map.insert("fast", "gpt-4o-mini");
        map.insert("gpt-4o-mini", "gpt-4o-mini-2024-07-18");
        assert_eq!(map.resolve("fast").unwrap(), "gpt-4o-mini-2024-07-18");
        assert_eq!(map.resolve("unknown").unwrap(), "unknown");
    }

    #[test]
    fn detects_cycle() {
        let mut map = ModelMap::default();
        map.insert("a", "b");
        map.insert("b", "a");
        assert_eq!(map.resolve("a"), Err(ModelMapError::Cycle { model: "a".into() }));
    }

    #[test]
    fn falls_back_to_base_name() {
        let map = ModelMap::from_json(&json!({"gpt-5": "gpt-5-2025-01-01"}));
        assert_eq!(map.resolve("gpt-5-high").unwrap(), "gpt-5-2025-01-01");
    }

    #[test]
    fn parses_suffixes() {
        let (base, hint) = strip_reasoning_suffix("claude-3-7-sonnet-thinking-4096");
        assert_eq!(base, "claude-3-7-sonnet");
        assert_eq!(hint.unwrap().budget_tokens, Some(4096));

        let (base, hint) = strip_reasoning_suffix("gpt-5-high");
        assert_eq!(base, "gpt-5");
        assert_eq!(hint.unwrap().effort.as_deref(), Some("high"));

        let (base, hint) = strip_reasoning_suffix("gpt-4o");
        assert_eq!(base, "gpt-4o");
        assert!(hint.is_none());
    }
}
