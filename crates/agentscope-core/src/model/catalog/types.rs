//! Catalog data model.
//!
//! The catalog is deliberately a set of owned, normalized values: it is built once per
//! refresh and only read afterwards, so request handling never has to parse or merge JSON.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema version of the on-disk normalized snapshot.
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

/// Default context window used when a model cannot be resolved.
pub use crate::model::context_windows::DEFAULT_CONTEXT_WINDOW;

/// Default output reservation used by the context compiler.
pub const DEFAULT_RESERVED_OUTPUT: usize = 8_192;

/// Maximum safety margin used by the context compiler.
pub const MAX_SAFETY_MARGIN: usize = 16_000;

/// Normalized modality set, for example `["text", "image"]`.
pub type ModalitySet = BTreeSet<String>;

/// Derived use-case labels, for example `["coding", "agentic"]`.
pub type UseCaseSet = BTreeSet<String>;

/// How the descriptor was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogConfidence {
    /// Provider-exact, bare-id-exact, or canonical match.
    Catalog,
    /// Normalized id or prefix consensus.
    Normalized,
    /// No match; defaults were used.
    Default,
}

impl Default for CatalogConfidence {
    fn default() -> Self {
        Self::Default
    }
}

/// Lifecycle state published by models.dev.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    /// Stable model.
    #[default]
    Active,
    Beta,
    Alpha,
    Deprecated,
}

/// A reasoning knob advertised by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    /// Reasoning can only be switched on/off.
    Toggle,
    /// Reasoning effort values, in catalog order after normalization.
    Effort { values: Vec<String> },
    /// Provider accepts a thinking-token budget.
    BudgetTokens,
}

/// Token pricing in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Cost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
}

impl Eq for Cost {}

impl Hash for Cost {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.input.to_bits().hash(state);
        self.output.to_bits().hash(state);
        self.cache_read.to_bits().hash(state);
    }
}

/// Broad model category. Only `Chat` models are offered to chat sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    #[default]
    Chat,
    Embedding,
    Image,
    Speech,
    Transcription,
    Reranking,
    Video,
    Realtime,
    Decision,
    /// Unknown `type` value.
    #[serde(other)]
    Other,
}

impl ModelKind {
    /// Parses the optional `type` field from models.dev.
    pub fn from_type(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None | Some("chat") => Self::Chat,
            Some("embedding") => Self::Embedding,
            Some("image") => Self::Image,
            Some("speech") => Self::Speech,
            Some("transcription") => Self::Transcription,
            Some("reranking") => Self::Reranking,
            Some("video") => Self::Video,
            Some("realtime") => Self::Realtime,
            Some("decision") => Self::Decision,
            Some(_) => Self::Other,
        }
    }
}

/// One normalized model entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Catalog key: `provider/model_id` (model ids may themselves contain `/`).
    pub key: String,
    /// Catalog provider id, for example `opencode-go`.
    pub provider: String,
    /// Provider-local model id, for example `deepseek-v4.1-flash`.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub family: Option<String>,
    pub kind: ModelKind,
    /// Context window in tokens. `0` means not applicable/unknown.
    pub context_window: usize,
    /// Maximum output tokens. `0` means unspecified.
    pub max_output: usize,
    pub input_modalities: ModalitySet,
    pub output_modalities: ModalitySet,
    pub attachment: bool,
    pub reasoning: bool,
    pub reasoning_options: Vec<ReasoningOption>,
    /// Field used to pass reasoning content back, for example `reasoning_content`.
    pub interleaved_field: Option<String>,
    pub tool_call: bool,
    pub structured_output: Option<bool>,
    pub temperature: bool,
    pub status: ModelStatus,
    pub open_weights: bool,
    pub cost: Option<Cost>,
    /// Optional upstream canonical id, for example `deepseek/deepseek-v4.1-flash`.
    pub canonical: Option<String>,
    /// Derived labels; recomputed on refresh.
    pub use_cases: UseCaseSet,
    /// Content hash used to compute `CatalogEvent::changed_keys`.
    pub fingerprint: u64,
}

impl ModelDescriptor {
    /// Returns the context window when it is known.
    pub fn effective_context(&self) -> Option<usize> {
        (self.context_window > 0).then_some(self.context_window)
    }

    /// Whether the model can be used in a chat session.
    pub fn is_chat(&self) -> bool {
        self.kind == ModelKind::Chat
    }

    /// Default input allowance for this model.
    ///
    /// This mirrors the context compiler: reserve output tokens first, then apply a
    /// safety margin capped at [`MAX_SAFETY_MARGIN`]. `0` is returned for models
    /// whose window is not applicable.
    pub fn input_limit(&self, reserved_output: usize) -> usize {
        let window = self.context_window;
        if window == 0 {
            return 0;
        }
        let reserved = if self.max_output > 0 {
            reserved_output.min(self.max_output)
        } else {
            reserved_output
        };
        let safety = (window / 50).clamp(1_024, MAX_SAFETY_MARGIN);
        window.saturating_sub(reserved).saturating_sub(safety)
    }

    /// Recomputes the fingerprint from every field that can affect callers.
    pub fn recompute_fingerprint(&mut self) {
        let mut hasher = DefaultHasher::new();
        self.key.hash(&mut hasher);
        self.provider.hash(&mut hasher);
        self.id.hash(&mut hasher);
        self.name.hash(&mut hasher);
        self.description.hash(&mut hasher);
        self.family.hash(&mut hasher);
        self.kind.hash(&mut hasher);
        self.context_window.hash(&mut hasher);
        self.max_output.hash(&mut hasher);
        self.input_modalities.hash(&mut hasher);
        self.output_modalities.hash(&mut hasher);
        self.attachment.hash(&mut hasher);
        self.reasoning.hash(&mut hasher);
        self.reasoning_options.hash(&mut hasher);
        self.interleaved_field.hash(&mut hasher);
        self.tool_call.hash(&mut hasher);
        self.structured_output.hash(&mut hasher);
        self.temperature.hash(&mut hasher);
        self.status.hash(&mut hasher);
        self.open_weights.hash(&mut hasher);
        self.cost.hash(&mut hasher);
        self.canonical.hash(&mut hasher);
        self.use_cases.hash(&mut hasher);
        self.fingerprint = hasher.finish();
    }
}

/// Metadata for one downloaded source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Logical name: `models` or `api`.
    pub name: String,
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub checked_at: DateTime<Utc>,
    pub bytes: u64,
    pub sha256: Option<String>,
}

/// Immutable, normalized catalog snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub schema_version: u32,
    /// Incremented only when content changes; a `304` leaves it untouched.
    pub generation: u64,
    #[serde(default)]
    pub sources: Vec<SourceInfo>,
    #[serde(default)]
    pub entries: HashMap<String, Arc<ModelDescriptor>>,
    /// Built in memory after load; not persisted because it duplicates entries.
    #[serde(skip)]
    pub by_bare: HashMap<String, Vec<Arc<ModelDescriptor>>>,
    #[serde(skip)]
    pub by_provider: HashMap<String, Vec<Arc<ModelDescriptor>>>,
    #[serde(skip)]
    pub by_canonical: HashMap<String, Vec<Arc<ModelDescriptor>>>,
    #[serde(default)]
    pub provider_api: HashMap<String, String>,
    /// Environment variables that carry a provider's credentials, from models.dev
    /// (`providers.<id>.env`). Used to discover already-configured providers at
    /// startup and to name the variable a `--setup` wizard can fall back to.
    #[serde(default)]
    pub provider_env: HashMap<String, Vec<String>>,
    pub fetched_at: DateTime<Utc>,
}

impl CatalogSnapshot {
    /// Empty snapshot used before the first successful refresh.
    pub fn empty() -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            generation: 0,
            sources: Vec::new(),
            entries: HashMap::new(),
            by_bare: HashMap::new(),
            by_provider: HashMap::new(),
            by_canonical: HashMap::new(),
            provider_api: HashMap::new(),
            provider_env: HashMap::new(),
            fetched_at: Utc::now(),
        }
    }

    /// Rebuilds the lookup indices. Must be called after construction or deserialization.
    pub fn rebuild_indices(&mut self) {
        self.by_bare.clear();
        self.by_provider.clear();
        self.by_canonical.clear();
        for (key, descriptor) in &self.entries {
            self.by_bare
                .entry(descriptor.id.to_ascii_lowercase())
                .or_default()
                .push(Arc::clone(descriptor));
            self.by_provider
                .entry(descriptor.provider.clone())
                .or_default()
                .push(Arc::clone(descriptor));
            if let Some(canonical) = &descriptor.canonical {
                self.by_canonical
                    .entry(canonical.to_ascii_lowercase())
                    .or_default()
                    .push(Arc::clone(descriptor));
            }
            debug_assert_eq!(key, &descriptor.key);
        }
        for values in self.by_bare.values_mut() {
            values.sort_by(|a, b| a.key.cmp(&b.key));
        }
        for values in self.by_provider.values_mut() {
            values.sort_by(|a, b| a.id.cmp(&b.id));
        }
        for values in self.by_canonical.values_mut() {
            values.sort_by(|a, b| a.key.cmp(&b.key));
        }
    }

    /// Returns a descriptor by exact catalog key.
    pub fn get(&self, key: &str) -> Option<&Arc<ModelDescriptor>> {
        self.entries.get(key)
    }

    /// Returns all providers whose API host matches `base_url`.
    pub fn providers_for_host(&self, base_url: &str) -> Vec<&str> {
        let Some(host) = host_of(base_url) else {
            return Vec::new();
        };
        let mut providers: Vec<&str> = self
            .provider_api
            .iter()
            .filter(|(_, api)| host_of(api).as_deref() == Some(host.as_str()))
            .map(|(provider, _)| provider.as_str())
            .collect();
        providers.sort_unstable();
        providers
    }
}

/// Content-change event published by the catalog.
#[derive(Debug, Clone)]
pub struct CatalogEvent {
    pub generation: u64,
    /// Keys whose descriptor was added, removed, or changed.
    pub changed_keys: Arc<HashSet<String>>,
}

impl CatalogEvent {
    /// Empty event for generation zero.
    pub fn empty() -> Self {
        Self {
            generation: 0,
            changed_keys: Arc::new(HashSet::new()),
        }
    }
}

/// How a model id was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    ExactProvider,
    ExactBare,
    Canonical,
    Normalized,
    PrefixConsensus,
    Default,
}

/// Result of a catalog lookup.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub descriptor: Option<Arc<ModelDescriptor>>,
    pub kind: MatchKind,
    /// Internal ranking score; card-facing code maps it to [`CatalogConfidence`].
    pub confidence: f32,
    pub provider: Option<String>,
}

impl Resolution {
    /// Default resolution for an unknown model.
    pub fn default_resolution() -> Self {
        Self {
            descriptor: None,
            kind: MatchKind::Default,
            confidence: 0.0,
            provider: None,
        }
    }

    /// Card-facing confidence.
    pub fn catalog_confidence(&self) -> CatalogConfidence {
        if self.confidence >= 0.9 {
            CatalogConfidence::Catalog
        } else if self.confidence >= 0.5 {
            CatalogConfidence::Normalized
        } else {
            CatalogConfidence::Default
        }
    }
}

/// Extracts a host from a URL without pulling a URL parser into hot paths.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}
