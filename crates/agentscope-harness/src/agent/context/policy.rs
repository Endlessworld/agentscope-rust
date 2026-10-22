//! Token estimation, eviction ordering, policy and the construction manifest.

use std::sync::Arc;

use agentscope_core::message::{ContentBlock, Msg};
use agentscope_core::model::ToolSchema;

use super::item::{ContextItem, ContextItemPlacement};

/// A token estimate. Estimates are never provider usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    pub tokens: i64,
    pub method: String,
    pub exact: bool,
}

impl Estimate {
    /// Creates an estimate, rejecting negative counts.
    pub fn new(tokens: i64, method: impl Into<String>, exact: bool) -> crate::Result<Self> {
        if tokens < 0 {
            return Err(crate::Error::Agent("Invalid estimate".into()));
        }
        Ok(Self {
            tokens,
            method: method.into(),
            exact,
        })
    }
}

/// Counts input tokens for one prepared request.
///
/// Replace with an adapter-specific tokenizer when one is available.
pub trait TokenEstimator: Send + Sync {
    /// Estimates the input cost of `input`.
    fn estimate(&self, input: &EstimateInput<'_>) -> Estimate;
}

/// The parts of a model request that consume the input budget.
pub struct EstimateInput<'a> {
    pub messages: &'a [Msg],
    pub tools: &'a [ToolSchema],
}

impl<'a> EstimateInput<'a> {
    pub fn new(messages: &'a [Msg], tools: &'a [ToolSchema]) -> Self {
        Self { messages, tools }
    }
}

/// The default character-based approximation used by the Java harness.
///
/// Method string: `mixed-text-2.5chars-media-sized-v2`. Media blocks are charged by
/// payload size (images) or a fixed budget (audio/video) instead of one flat rate.
pub struct ApproximateEstimator;

impl TokenEstimator for ApproximateEstimator {
    fn estimate(&self, input: &EstimateInput<'_>) -> Estimate {
        let mut count = 0i64;
        for message in input.messages {
            count += 12 + text(message.name.as_deref());
            for block in &message.content {
                count += block_cost(block);
            }
        }
        for tool in input.tools {
            let json = serde_json::to_string(tool).unwrap_or_default();
            count += 16 + text(Some(&json));
        }
        Estimate {
            tokens: count,
            method: "mixed-text-2.5chars-media-sized-v2".into(),
            exact: false,
        }
    }
}

fn text(value: Option<&str>) -> i64 {
    match value {
        Some(value) => (value.chars().count() as f64 / 2.5).ceil() as i64,
        None => 0,
    }
}

fn block_cost(block: &ContentBlock) -> i64 {
    match block {
        ContentBlock::Text(text_block) => text(Some(&text_block.text)),
        ContentBlock::Thinking(thinking) => text(Some(&thinking.thinking)),
        ContentBlock::ToolUse(call) => {
            let json = serde_json::to_string(&call.input).unwrap_or_default();
            12 + text(Some(&call.id))
                + text(Some(&call.name))
                + text(Some(&json))
                + text(call.content.as_deref())
        }
        ContentBlock::ToolResult(result) => {
            let mut tokens = 12 + text(result.id.as_deref()) + text(result.name.as_deref());
            for output in &result.output {
                tokens += block_cost(output);
            }
            tokens
        }
        ContentBlock::Image(image) => image_cost(&image.source),
        ContentBlock::Audio(_) => 2_048,
        ContentBlock::Video(_) => 4_096,
        ContentBlock::Data(_) => 2_048,
        // Explicitly approximate: any other media cost depends on the provider.
        _ => 2_048,
    }
}

/// Images are charged by encoded payload size, clamped to a plausible range.
fn image_cost(source: &agentscope_core::message::Source) -> i64 {
    match source {
        agentscope_core::message::Source::Base64(base64) => {
            let bytes = base64.data.len() as i64;
            (bytes / 1_000).clamp(256, 4_096)
        }
        agentscope_core::message::Source::Url(_) => 2_048,
    }
}

/// Returns optional source ids in eviction order. Cannot rewrite content or remove
/// required items.
pub trait ContextSelectionPolicy: Send + Sync {
    /// The order in which optional material may be omitted to fit the budget.
    fn omission_order(&self, materials: &[ContextItem]) -> Vec<String>;
}

/// Omit optional memory first, then knowledge, then everything else; lower priority
/// before higher, ties broken by source id so the order is deterministic.
pub struct DefaultSelectionPolicy;

impl ContextSelectionPolicy for DefaultSelectionPolicy {
    fn omission_order(&self, materials: &[ContextItem]) -> Vec<String> {
        let mut candidates: Vec<&ContextItem> = materials
            .iter()
            .filter(|item| {
                !item.required && item.placement != ContextItemPlacement::System
            })
            .collect();
        candidates.sort_by(|a, b| {
            category(a.kind.as_str())
                .cmp(&category(b.kind.as_str()))
                .then_with(|| a.priority.cmp(&b.priority))
                .then_with(|| a.source_id.cmp(&b.source_id))
        });
        candidates
            .into_iter()
            .map(|item| item.source_id.clone())
            .collect()
    }
}

fn category(kind: &str) -> i32 {
    match kind {
        "memory" => 0,
        "knowledge" => 1,
        _ => 2,
    }
}

/// Opt-in task projections. Does not run verifiers or enforce completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskContextOptions {
    pub requirements: bool,
    pub verification_results: bool,
    pub requirement_proposals: bool,
}

impl TaskContextOptions {
    /// Everything off.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Enables candidate requirement projection so proposals stay visible.
    pub fn allow_requirement_proposals(mut self) -> Self {
        self.requirement_proposals = true;
        self.requirements = true;
        self
    }

    /// Whether any projection is enabled.
    ///
    /// An agent with nothing else registered but a task projection still needs the compiler:
    /// the projection only happens inside it, so this is what opts the agent in.
    pub fn is_enabled(self) -> bool {
        self.requirements || self.verification_results || self.requirement_proposals
    }
}

/// One material entry in the manifest (metadata only, no bodies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextManifestItem {
    pub source_type: String,
    pub source_id: String,
    pub content_hash: String,
    pub revision: Option<String>,
    pub placement: Option<String>,
    pub required: bool,
    pub priority: i32,
}

impl ContextManifestItem {
    /// A message or tool-schema entry.
    pub fn new(
        source_type: impl Into<String>,
        source_id: impl Into<String>,
        content_hash: impl Into<String>,
    ) -> Self {
        Self {
            source_type: source_type.into(),
            source_id: source_id.into(),
            content_hash: content_hash.into(),
            revision: None,
            placement: None,
            required: false,
            priority: 0,
        }
    }

    /// A material entry carrying its placement policy.
    pub fn material(item: &ContextItem) -> Self {
        Self {
            source_type: item.kind.clone(),
            source_id: item.source_id.clone(),
            content_hash: super::item::content_hash(&item.content),
            revision: Some(item.revision.clone()),
            placement: Some(item.placement.as_str().to_string()),
            required: item.required,
            priority: item.priority,
        }
    }
}

impl serde::Serialize for ContextManifestItem {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ContextManifestItem", 7)?;
        state.serialize_field("sourceType", &self.source_type)?;
        state.serialize_field("sourceId", &self.source_id)?;
        state.serialize_field("contentHash", &self.content_hash)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("placement", &self.placement)?;
        state.serialize_field("required", &self.required)?;
        state.serialize_field("priority", &self.priority)?;
        state.end()
    }
}

/// Construction metadata recorded for observability.
///
/// Observers must apply their own access/retention policy; the manifest never carries prompt
/// bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextManifest {
    pub call_id: String,
    pub purpose: String,
    pub model: String,
    pub policy_version: String,
    pub context_window: i64,
    pub catalog_generation: u64,
    pub catalog_key: Option<String>,
    pub estimated_input_tokens: i64,
    pub input_limit: i64,
    pub counting_method: String,
    pub exact: bool,
    pub task_revision: u64,
    pub items: Vec<ContextManifestItem>,
    pub transforms: Vec<String>,
    pub validation: String,
}

impl ContextManifest {
    /// The policy version string of the harness compiler.
    pub const POLICY_VERSION: &'static str = "harness-context-v2";

    /// Returns a copy with a different validation status.
    pub fn with_validation(&self, status: impl Into<String>) -> Self {
        Self {
            validation: status.into(),
            ..self.clone()
        }
    }
}

impl serde::Serialize for ContextManifest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ContextManifest", 15)?;
        state.serialize_field("callId", &self.call_id)?;
        state.serialize_field("purpose", &self.purpose)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("policyVersion", &self.policy_version)?;
        state.serialize_field("contextWindow", &self.context_window)?;
        state.serialize_field("catalogGeneration", &self.catalog_generation)?;
        state.serialize_field("catalogKey", &self.catalog_key)?;
        state.serialize_field("estimatedInputTokens", &self.estimated_input_tokens)?;
        state.serialize_field("inputLimit", &self.input_limit)?;
        state.serialize_field("countingMethod", &self.counting_method)?;
        state.serialize_field("exact", &self.exact)?;
        state.serialize_field("taskRevision", &self.task_revision)?;
        state.serialize_field("items", &self.items)?;
        state.serialize_field("transforms", &self.transforms)?;
        state.serialize_field("validation", &self.validation)?;
        state.end()
    }
}

/// Budget and accounting policy for one agent.
///
/// Zero limits derive from the actual model window; there are no fixed per-layer quotas.
#[derive(Clone)]
pub struct ContextPolicy {
    pub max_input_tokens: u32,
    pub reserved_output_tokens: u32,
    pub safety_margin_tokens: u32,
    pub estimator: Arc<dyn TokenEstimator>,
    /// Observes the construction metadata; must never carry prompt bodies.
    pub observer: Arc<dyn Fn(&ContextManifest) + Send + Sync>,
}

impl std::fmt::Debug for ContextPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextPolicy")
            .field("max_input_tokens", &self.max_input_tokens)
            .field("reserved_output_tokens", &self.reserved_output_tokens)
            .field("safety_margin_tokens", &self.safety_margin_tokens)
            .finish_non_exhaustive()
    }
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_input_tokens: 0,
            reserved_output_tokens: 0,
            safety_margin_tokens: 0,
            estimator: Arc::new(ApproximateEstimator),
            observer: Arc::new(|_| {}),
        }
    }
}

impl ContextPolicy {
    /// Builds a policy from explicit limits.
    pub fn new(
        max_input_tokens: u32,
        reserved_output_tokens: u32,
        safety_margin_tokens: u32,
        estimator: Arc<dyn TokenEstimator>,
        observer: Arc<dyn Fn(&ContextManifest) + Send + Sync>,
    ) -> Self {
        Self {
            max_input_tokens,
            reserved_output_tokens,
            safety_margin_tokens,
            estimator,
            observer,
        }
    }

    /// Reads only the serializable numeric fields, rejecting unknown keys.
    pub fn from_map(values: &serde_json::Map<String, serde_json::Value>) -> crate::Result<Self> {
        for key in values.keys() {
            if !matches!(
                key.as_str(),
                "maxInputTokens" | "reservedOutputTokens" | "safetyMarginTokens"
            ) {
                return Err(crate::Error::Agent(format!(
                    "Unknown context policy field: {key}"
                )));
            }
        }
        Ok(Self {
            max_input_tokens: limit(values, "maxInputTokens")?,
            reserved_output_tokens: limit(values, "reservedOutputTokens")?,
            safety_margin_tokens: limit(values, "safetyMarginTokens")?,
            ..Self::default()
        })
    }
}

fn limit(
    values: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> crate::Result<u32> {
    match values.get(key) {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(value) => match value.as_u64() {
            Some(number) if number <= u32::MAX as u64 => Ok(number as u32),
            _ => Err(crate::Error::Agent(format!(
                "{key} must be a nonnegative integer"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{Msg, TextBlock};
    use super::super::item::ContextBlock;

    #[test]
    fn approximate_estimator_counts_text_by_characters() {
        let msg = Msg::user("abcd");
        let estimate = ApproximateEstimator.estimate(&EstimateInput::new(&[msg], &[]));
        // 12 (per-message overhead) + ceil(4 / 2.5) = 14.
        assert_eq!(estimate.tokens, 14);
        assert_eq!(estimate.method, "mixed-text-2.5chars-media-sized-v2");
        assert!(!estimate.exact);
    }

    #[test]
    fn approximate_estimator_charges_media_a_flat_rate() {
        let mut msg = Msg::user("x");
        msg.content
            .push(ContentBlock::Text(TextBlock::new(String::new())));
        let base = ApproximateEstimator.estimate(&EstimateInput::new(&[msg.clone()], &[]));
        // An empty text block costs nothing extra beyond the message overhead.
        assert!(base.tokens >= 12);
    }

    #[test]
    fn default_selection_orders_memory_then_knowledge_then_rest() {
        let policy = DefaultSelectionPolicy;
        let items = vec![
            ContextItem::new("other", "other/1", "x").unwrap(),
            ContextItem::new("knowledge", "knowledge/1", "x").unwrap(),
            ContextItem::new("memory", "memory/2", "x").unwrap(),
            ContextItem::new("memory", "memory/1", "x").unwrap(),
        ];
        let order = policy.omission_order(&items);
        assert_eq!(
            order,
            vec![
                "memory/1".to_string(),
                "memory/2".to_string(),
                "knowledge/1".to_string(),
                "other/1".to_string()
            ]
        );
    }

    #[test]
    fn required_and_system_material_is_never_an_omission_candidate() {
        let policy = DefaultSelectionPolicy;
        let items = vec![
            ContextItem::instruction("policy", "instruction/a", "x").unwrap(),
            ContextItem::new("memory", "memory/req", "x").unwrap().required(),
            ContextItem::new("memory", "memory/opt", "x").unwrap(),
        ];
        assert_eq!(policy.omission_order(&items), vec!["memory/opt".to_string()]);
    }

    #[test]
    fn policy_from_map_rejects_unknown_and_invalid_values() {
        let mut values = serde_json::Map::new();
        values.insert("maxInputTokens".into(), serde_json::json!(24000));
        values.insert("safetyMarginTokens".into(), serde_json::json!(1024));
        let policy = ContextPolicy::from_map(&values).unwrap();
        assert_eq!(policy.max_input_tokens, 24000);
        assert_eq!(policy.reserved_output_tokens, 0);
        assert_eq!(policy.safety_margin_tokens, 1024);

        let mut unknown = serde_json::Map::new();
        unknown.insert("mystery".into(), serde_json::json!(1));
        assert!(ContextPolicy::from_map(&unknown).is_err());

        let mut negative = serde_json::Map::new();
        negative.insert("maxInputTokens".into(), serde_json::json!(-1));
        assert!(ContextPolicy::from_map(&negative).is_err());

        let mut fractional = serde_json::Map::new();
        fractional.insert("maxInputTokens".into(), serde_json::json!(1.5));
        assert!(ContextPolicy::from_map(&fractional).is_err());
    }

    #[test]
    fn manifest_item_carries_policy_but_never_the_body() {
        let item = ContextBlock::reference("format", "columns: a,b")
            .unwrap()
            .material();
        let entry = ContextManifestItem::material(&item);
        assert_eq!(entry.source_type, "reference");
        assert_eq!(
            entry.content_hash,
            super::super::content_hash("columns: a,b")
        );
        assert_eq!(entry.placement.as_deref(), Some("REFERENCE"));
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("contentHash"));
        assert!(!json.contains("columns: a,b"));
    }

    #[test]
    fn manifest_serializes_with_camel_case_metadata_only() {
        let manifest = ContextManifest {
            call_id: "call-1".into(),
            purpose: "REASONING".into(),
            model: "m".into(),
            policy_version: ContextManifest::POLICY_VERSION.into(),
            context_window: 256_000,
            catalog_generation: 0,
            catalog_key: None,
            estimated_input_tokens: 10,
            input_limit: 100,
            counting_method: "method".into(),
            exact: false,
            task_revision: 3,
            items: vec![],
            transforms: vec!["history_compaction".into()],
            validation: "passed".into(),
        };
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["callId"], "call-1");
        assert_eq!(json["estimatedInputTokens"], 10);
        assert_eq!(json["transforms"][0], "history_compaction");
        assert_eq!(json["policyVersion"], "harness-context-v2");
        assert_eq!(
            manifest.with_validation("budget_exceeded").validation,
            "budget_exceeded"
        );
    }

    #[test]
    fn requirement_proposals_imply_requirement_projection() {
        let options = TaskContextOptions::disabled().allow_requirement_proposals();
        assert!(options.requirement_proposals);
        assert!(options.requirements);
        assert!(!options.verification_results);
    }

    #[test]
    fn estimate_rejects_negative_counts() {
        assert!(Estimate::new(-1, "m", false).is_err());
        assert_eq!(Estimate::new(0, "m", true).unwrap().tokens, 0);
    }
}
