//! Model-id resolution against a normalized snapshot.
//!
//! Priority is deliberately conservative: exact id first (provider, then bare, then
//! canonical), and only then normalization/prefix consensus. A `-free` variant may never
//! inherit a large paid window just because the `-free` suffix was stripped.

use std::sync::Arc;

use super::types::{
    CatalogSnapshot, MatchKind, ModelDescriptor, Resolution, DEFAULT_CONTEXT_WINDOW,
};

/// Inputs for one lookup.
#[derive(Debug, Clone, Copy)]
pub struct ResolveRequest<'a> {
    /// Local provider key (`Go`, `Zen`, ...), not necessarily a catalog provider id.
    pub provider_key: Option<&'a str>,
    /// Model id as configured locally; may include a `Provider/` prefix.
    pub model_id: &'a str,
    /// Local provider base URL, used to identify the catalog provider by host.
    pub base_url: Option<&'a str>,
    /// Local model ids for the provider, used to disambiguate shared API hosts.
    pub local_models: &'a [String],
}

impl<'a> ResolveRequest<'a> {
    /// Convenience constructor for callers without a local model list.
    pub fn new(model_id: &'a str) -> Self {
        Self {
            provider_key: None,
            model_id,
            base_url: None,
            local_models: &[],
        }
    }
}

/// Resolves one model id. Never fails: an unresolved id yields a default `Resolution`.
pub fn resolve(snapshot: &CatalogSnapshot, request: &ResolveRequest<'_>) -> Resolution {
    let raw = request.model_id.trim();
    if raw.is_empty() {
        return Resolution::default_resolution();
    }

    let provider_candidates = provider_candidates(snapshot, request);
    let bare = strip_provider_prefix(raw, request.provider_key);
    let bare_lower = bare.to_ascii_lowercase();
    let raw_lower = raw.to_ascii_lowercase();

    // 1) provider-exact key.
    for provider in &provider_candidates {
        if let Some(descriptor) = snapshot.get(&format!("{provider}/{bare}")) {
            return found(descriptor, MatchKind::ExactProvider, 1.0, Some(provider));
        }
        if bare != raw {
            if let Some(descriptor) = snapshot.get(&format!("{provider}/{raw}")) {
                return found(descriptor, MatchKind::ExactProvider, 1.0, Some(provider));
            }
        }
    }

    // 2) bare exact (only when it is unambiguous).
    if let Some(candidates) = snapshot.by_bare.get(&bare_lower) {
        if candidates.len() == 1 {
            return found(&candidates[0], MatchKind::ExactBare, 0.95, None);
        }
        if let Some(descriptor) = pick_provider(candidates, &provider_candidates) {
            return found(descriptor, MatchKind::ExactProvider, 0.95, None);
        }
    }

    // 3) canonical id (only after exact matches).
    if let Some(candidates) = snapshot.by_canonical.get(&raw_lower) {
        if candidates.len() == 1 {
            return found(&candidates[0], MatchKind::Canonical, 0.9, None);
        }
        if let Some(descriptor) = pick_provider(candidates, &provider_candidates) {
            return found(descriptor, MatchKind::Canonical, 0.9, None);
        }
    }

    // 4) normalized id. `-free` is only stripped here, never before exact matching.
    let (normalized, stripped_free) = normalize_for_match(&bare_lower, request.provider_key);
    if !normalized.is_empty() && normalized != bare_lower {
        if let Some(candidates) = snapshot.by_bare.get(&normalized) {
            if let Some(descriptor) = unique_or_provider(candidates, &provider_candidates) {
                if !(stripped_free && descriptor.context_window > DEFAULT_CONTEXT_WINDOW) {
                    return found(descriptor, MatchKind::Normalized, 0.7, None);
                }
            }
        }
        if let Some(candidates) = snapshot.by_canonical.get(&normalized) {
            if let Some(descriptor) = unique_or_provider(candidates, &provider_candidates) {
                if !(stripped_free && descriptor.context_window > DEFAULT_CONTEXT_WINDOW) {
                    return found(descriptor, MatchKind::Normalized, 0.7, None);
                }
            }
        }
    }

    // 5) prefix consensus. Require at least two candidates so a single lookalike
    //    (`m2.5` vs `m2.5-highspeed`) cannot silently redefine a model.
    let candidates = prefix_candidates(snapshot, &normalized, &provider_candidates);
    if candidates.len() >= 2 {
        let first = &candidates[0];
        if candidates.iter().all(|candidate| {
            candidate.context_window == first.context_window
                && candidate.max_output == first.max_output
        }) {
            return found(first, MatchKind::PrefixConsensus, 0.55, None);
        }
    }

    Resolution::default_resolution()
}

fn found(
    descriptor: &Arc<ModelDescriptor>,
    kind: MatchKind,
    confidence: f32,
    provider: Option<&str>,
) -> Resolution {
    Resolution {
        descriptor: Some(Arc::clone(descriptor)),
        kind,
        confidence,
        provider: provider.map(ToString::to_string),
    }
}

fn unique_or_provider<'a>(
    candidates: &'a [Arc<ModelDescriptor>],
    providers: &[String],
) -> Option<&'a Arc<ModelDescriptor>> {
    if candidates.len() == 1 {
        return candidates.first();
    }
    pick_provider(candidates, providers)
}

fn pick_provider<'a>(
    candidates: &'a [Arc<ModelDescriptor>],
    providers: &[String],
) -> Option<&'a Arc<ModelDescriptor>> {
    if providers.is_empty() {
        return None;
    }
    let mut found = None;
    for candidate in candidates {
        if providers.iter().any(|p| p == &candidate.provider) {
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}

fn prefix_candidates(
    snapshot: &CatalogSnapshot,
    normalized: &str,
    providers: &[String],
) -> Vec<Arc<ModelDescriptor>> {
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<Arc<ModelDescriptor>> = snapshot
        .entries
        .values()
        .filter(|descriptor| {
            if !providers.is_empty() && !providers.iter().any(|p| p == &descriptor.provider) {
                return false;
            }
            let id = descriptor.id.to_ascii_lowercase();
            id != normalized && (id.starts_with(normalized) || normalized.starts_with(&id))
        })
        .cloned()
        .collect();
    candidates.sort_by(|a, b| a.key.cmp(&b.key));
    candidates
}

fn provider_candidates(
    snapshot: &CatalogSnapshot,
    request: &ResolveRequest<'_>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(hint) = request.provider_key {
        let hint = hint.trim();
        if !hint.is_empty() && snapshot.by_provider.contains_key(hint) {
            candidates.push(hint.to_string());
        }
    }

    // A `Provider/model` prefix can name a catalog provider directly.
    if let Some((prefix, _)) = request.model_id.split_once('/') {
        if snapshot.by_provider.contains_key(prefix) && !candidates.iter().any(|p| p == prefix) {
            candidates.push(prefix.to_string());
        }
    }

    if let Some(base_url) = request.base_url {
        for provider in snapshot.providers_for_host(base_url) {
            if !candidates.iter().any(|candidate| candidate == provider) {
                candidates.push(provider.to_string());
            }
        }
    }
    if candidates.len() > 1 && !request.local_models.is_empty() {
        let mut scored: Vec<((usize, usize), String)> = candidates
            .iter()
            .map(|provider| {
                let overlap = overlap_score(snapshot, provider, request.local_models);
                let path = request
                    .base_url
                    .map(|base_url| path_affinity(snapshot, provider, base_url))
                    .unwrap_or(0);
                ((overlap, path), provider.clone())
            })
            .collect();
        let max = scored.iter().map(|(score, _)| *score).max().unwrap_or((0, 0));
        if max > (0, 0) {
            scored.retain(|(score, _)| *score == max);
            scored.sort_by(|a, b| a.1.cmp(&b.1));
            candidates = scored.into_iter().map(|(_, provider)| provider).collect();
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn overlap_score(snapshot: &CatalogSnapshot, provider: &str, local_models: &[String]) -> usize {
    let Some(entries) = snapshot.by_provider.get(provider) else {
        return 0;
    };
    let local: Vec<String> = local_models
        .iter()
        .map(|model| model.trim().to_ascii_lowercase())
        .collect();
    entries
        .iter()
        .filter(|descriptor| {
            let id = descriptor.id.to_ascii_lowercase();
            local.iter().any(|model| model == &id)
        })
        .count()
}

/// Breadcrumb from the URL path. Shared gateways often expose different providers on the
/// same host (`opencode.ai/inference/openai` vs `opencode.ai/inference/go/openai`), so a
/// distinctive path token (`go`) is a much stronger signal than the host alone.
fn path_affinity(snapshot: &CatalogSnapshot, provider: &str, base_url: &str) -> usize {
    let Some(api) = snapshot.provider_api.get(provider) else {
        return 0;
    };
    let local: std::collections::HashSet<String> = path_tokens(base_url).into_iter().collect();
    path_tokens(api)
        .into_iter()
        .filter(|token| local.contains(token))
        .filter(|token| !matches!(token.as_str(), "v1" | "v2" | "api" | "openai"))
        .count()
}

fn path_tokens(url: &str) -> Vec<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = path.split_once('/').map(|(_, path)| path).unwrap_or("");
    path.split(['/', '-', '_', '.'])
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| token.len() > 1)
        .collect()
}

fn strip_provider_prefix<'a>(model_id: &'a str, provider_key: Option<&str>) -> &'a str {
    let Some(provider) = provider_key else {
        return model_id;
    };
    let prefix = format!("{provider}/");
    if model_id.len() > prefix.len()
        && model_id
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
    {
        &model_id[prefix.len()..]
    } else {
        model_id
    }
}

/// Returns `(normalized_id, stripped_free)`.
fn normalize_for_match(model_id: &str, provider_key: Option<&str>) -> (String, bool) {
    let mut id = strip_provider_prefix(model_id, provider_key).to_ascii_lowercase();
    let mut stripped_free = false;
    for suffix in ["-free", "-preview", "-exp"] {
        if let Some(stripped) = id.strip_suffix(suffix) {
            if !stripped.is_empty() {
                id = stripped.to_string();
                stripped_free |= suffix == "-free";
            }
        }
    }
    // Strip a trailing YYYY-MM-DD or YYYYMMDD release suffix.
    if let Some((head, tail)) = id.rsplit_once('-') {
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() {
            id = head.to_string();
        } else if tail.len() == 10
            && tail.as_bytes().get(4) == Some(&b'-')
            && tail
                .chars()
                .enumerate()
                .all(|(index, c)| index == 4 || c.is_ascii_digit())
            && !head.is_empty()
        {
            id = head.to_string();
        }
    }
    (id, stripped_free)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::catalog::source::{merge_descriptors, parse_api_json, parse_models_json};

    fn snapshot_from(flat: &str, api: Option<&str>) -> CatalogSnapshot {
        let flat = parse_models_json(flat.as_bytes()).unwrap();
        let mut entries: std::collections::HashMap<String, ModelDescriptor> = flat
            .entries
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        let mut provider_api = std::collections::HashMap::new();
        if let Some(api) = api {
            let parsed = parse_api_json(api.as_bytes()).unwrap();
            provider_api = parsed.provider_api;
            for enhanced in parsed.entries {
                if let Some(base) = entries.remove(&enhanced.key) {
                    entries.insert(enhanced.key.clone(), merge_descriptors(base, enhanced));
                } else {
                    entries.insert(enhanced.key.clone(), enhanced);
                }
            }
        }
        let mut snapshot = CatalogSnapshot::empty();
        snapshot.provider_api = provider_api;
        snippet_insert(&mut snapshot, entries);
        snapshot
    }

    fn snippet_insert(
        snapshot: &mut CatalogSnapshot,
        entries: std::collections::HashMap<String, ModelDescriptor>,
    ) {
        snapshot.entries = entries
            .into_iter()
            .map(|(key, value)| (key, Arc::new(value)))
            .collect();
        snapshot.rebuild_indices();
    }

    #[test]
    fn provider_exact_wins_over_bare_alias() {
        let snapshot = snapshot_from(
            r#"{
                "p/m": {"limit": {"context": 1024}},
                "q/m": {"limit": {"context": 2000}}
            }"#,
            None,
        );
        let request = ResolveRequest {
            provider_key: Some("p"),
            model_id: "m",
            base_url: None,
            local_models: &[],
        };
        let result = resolve(&snapshot, &request);
        assert_eq!(result.kind, MatchKind::ExactProvider);
        assert_eq!(result.descriptor.unwrap().context_window, 1024);
    }

    #[test]
    fn bare_unique_is_used_when_provider_is_unknown() {
        let snapshot = snapshot_from(r#"{"p/only": {"limit": {"context": 123000}}}"#, None);
        let result = resolve(&snapshot, &ResolveRequest::new("only"));
        assert_eq!(result.kind, MatchKind::ExactBare);
        assert_eq!(result.descriptor.unwrap().context_window, 123_000);
    }

    #[test]
    fn free_exact_is_not_replaced_by_paid_window() {
        let snapshot = snapshot_from(
            r#"{
                "paid/free-model": {"limit": {"context": 1000000}},
                "paid/free-model-free": {"limit": {"context": 200000}}
            }"#,
            None,
        );
        let result = resolve(&snapshot, &ResolveRequest::new("free-model-free"));
        assert_eq!(result.kind, MatchKind::ExactBare);
        assert_eq!(result.descriptor.unwrap().context_window, 200_000);
    }

    #[test]
    fn stripped_free_never_borrows_a_large_window() {
        let snapshot = snapshot_from(
            r#"{"paid/model": {"limit": {"context": 1000000}}}"#,
            None,
        );
        let result = resolve(&snapshot, &ResolveRequest::new("model-free"));
        assert_eq!(result.kind, MatchKind::Default);
        assert!(result.descriptor.is_none());
    }

    #[test]
    fn canonical_matches_after_exact_misses() {
        let snapshot = snapshot_from(
            r#"{"p/m": {"canonical_model_id": "upstream/m"}}"#,
            None,
        );
        let result = resolve(&snapshot, &ResolveRequest::new("upstream/m"));
        assert_eq!(result.kind, MatchKind::Canonical);
        assert_eq!(result.descriptor.unwrap().key, "p/m");
    }

    #[test]
    fn host_and_local_models_disambiguate_shared_gateway() {
        let snapshot = snapshot_from(
            r#"{
                "opencode-go/deepseek-v4.1-flash": {"limit": {"context": 1000000}},
                "opencode/other": {"limit": {"context": 200000}}
            }"#,
            Some(
                r#"{
                    "opencode-go": {"api": "https://gateway.example/v1", "models": {"deepseek-v4.1-flash": {}}},
                    "opencode": {"api": "https://gateway.example/v1", "models": {"other": {}}}
                }"#,
            ),
        );
        let local = vec!["deepseek-v4.1-flash".to_string()];
        let request = ResolveRequest {
            provider_key: Some("Go"),
            model_id: "deepseek-v4.1-flash",
            base_url: Some("https://gateway.example/v1"),
            local_models: &local,
        };
        let result = resolve(&snapshot, &request);
        assert_eq!(result.kind, MatchKind::ExactProvider);
        assert_eq!(result.descriptor.unwrap().provider, "opencode-go");
    }

    #[test]
    fn ambiguous_prefix_does_not_guess() {
        let snapshot = snapshot_from(
            r#"{
                "p/minimax-m2.5-highspeed": {"limit": {"context": 128000}},
                "p/minimax-m2.5-air": {"limit": {"context": 64000}}
            }"#,
            None,
        );
        let result = resolve(&snapshot, &ResolveRequest::new("minimax-m2.5"));
        assert_eq!(result.kind, MatchKind::Default);
    }

    #[test]
    fn prefix_consensus_accepts_identical_windows() {
        let snapshot = snapshot_from(
            r#"{
                "p/nemotron-3-ultra-a": {"limit": {"context": 256000, "output": 32000}},
                "p/nemotron-3-ultra-b": {"limit": {"context": 256000, "output": 32000}}
            }"#,
            None,
        );
        let result = resolve(&snapshot, &ResolveRequest::new("nemotron-3-ultra-free"));
        assert_eq!(result.kind, MatchKind::PrefixConsensus);
    }

    #[test]
    #[ignore = "reads downloaded models.dev payloads from the OS temp directory"]
    fn resolves_real_opencode_alias_through_host_and_local_models() {
        let temp = std::env::temp_dir();
        let flat = std::fs::read(temp.join("models-all.json")).expect("models-all.json");
        let api = std::fs::read(temp.join("models-api.json")).expect("models-api.json");
        let flat = parse_models_json(&flat).unwrap();
        let detailed = parse_api_json(&api).unwrap();
        let provider_api = detailed.provider_api.clone();
        let mut entries: std::collections::HashMap<String, ModelDescriptor> = flat
            .entries
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        for enhanced in detailed.entries {
            if let Some(base) = entries.remove(&enhanced.key) {
                entries.insert(enhanced.key.clone(), merge_descriptors(base, enhanced));
            } else {
                entries.insert(enhanced.key.clone(), enhanced);
            }
        }
        let mut snapshot = CatalogSnapshot::empty();
        snapshot.provider_api = provider_api;
        snippet_insert(&mut snapshot, entries);

        let local = vec!["deepseek-v4.1-flash".to_string()];
        let request = ResolveRequest {
            provider_key: Some("Go"),
            model_id: "deepseek-v4.1-flash",
            base_url: Some("https://opencode.ai/zen/go/v1"),
            local_models: &local,
        };
        let result = resolve(&snapshot, &request);
        let descriptor = result.descriptor.expect("resolved");
        assert_eq!(descriptor.provider, "opencode-go");
        assert_eq!(descriptor.context_window, 1_000_000);
        assert_eq!(descriptor.max_output, 384_000);
    }
}
