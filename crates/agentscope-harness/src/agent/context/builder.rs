//! The single final request compiler.
//!
//! Stateless and safe to share between sessions: every preparation recomputes from the
//! committed state and the registered sources. The compiler collects material, fits it into
//! the model budget by evicting optional sources in a deterministic order, renders the result
//! into the stable layout and records a metadata-only manifest.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, Msg, MsgRole, ToolResultBlock};
use agentscope_core::model::{GenerateOptions, Model, ToolSchema};

use super::item::{ContextItem, ContextItemPlacement, ContextPurpose};
use super::policy::{
    ContextManifest, ContextManifestItem, ContextPolicy, ContextSelectionPolicy,
    DefaultSelectionPolicy, EstimateInput,
};
use super::render::{
    layout_messages, material_message, remove_synthetic, render_context_blocks, MessageKind,
};
use super::source::{CollectedMaterials, ContextRequest, ContextSources};

/// Runtime-context key prefix carrying the manifest of one prepared call.
pub const MANIFEST_ATTRIBUTE_PREFIX: &str = "agentscope.model_request_manifest.";

/// Configuration of one pipeline.
#[derive(Clone)]
pub struct ContextPipelineConfig {
    /// Budget and accounting policy.
    pub policy: ContextPolicy,
    /// Registered dynamic sources, instructions and task projection options.
    pub sources: ContextSources,
    /// The order optional material is dropped to fit the budget.
    pub selection: Arc<dyn ContextSelectionPolicy>,
    /// Project task state (PLAN/BUILD) into the request.
    pub project_plan_state: bool,
}

impl std::fmt::Debug for ContextPipelineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextPipelineConfig")
            .field("policy", &self.policy)
            .field("sources", &self.sources)
            .field("project_plan_state", &self.project_plan_state)
            .finish_non_exhaustive()
    }
}

impl Default for ContextPipelineConfig {
    fn default() -> Self {
        Self {
            policy: ContextPolicy::default(),
            sources: ContextSources::empty(),
            selection: Arc::new(DefaultSelectionPolicy),
            project_plan_state: true,
        }
    }
}

/// The compiled request plus its manifest and the material that produced it.
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub messages: Vec<Msg>,
    pub tools: Vec<ToolSchema>,
    pub options: GenerateOptions,
    pub manifest: ContextManifest,
}

/// The single final request compiler.
#[derive(Debug, Clone, Default)]
pub struct ContextPipeline {
    config: ContextPipelineConfig,
}

impl ContextPipeline {
    /// Creates a pipeline from explicit configuration.
    pub fn new(config: ContextPipelineConfig) -> Self {
        Self { config }
    }

    /// Creates a pipeline that only enforces the budget (no sources, no instructions).
    pub fn with_policy(policy: ContextPolicy) -> Self {
        Self {
            config: ContextPipelineConfig {
                policy,
                ..ContextPipelineConfig::default()
            },
        }
    }

    /// The configured sources.
    pub fn sources(&self) -> &ContextSources {
        &self.config.sources
    }

    /// The configured policy.
    pub fn policy(&self) -> &ContextPolicy {
        &self.config.policy
    }

    /// Compiles one model request.
    ///
    /// `history` is the committed conversation; `messages` are the messages the caller already
    /// assembled (typically the same history). Sources are loaded once and reused by both the
    /// budget pass and the rendering pass.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        model: Arc<dyn Model>,
        request: ContextRequest,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> crate::Result<PreparedRequest> {
        let purpose = request.purpose;
        let mut transforms: Vec<String> = Vec::new();

        // 1. Collect material. Only a reasoning preparation loads dynamic sources.
        let collected: CollectedMaterials = self.config.sources.collect(&request).await?;
        transforms.extend(collected.transforms.clone());
        let mut selected = collected.items;

        // An auxiliary call sees no non-System material at all.
        if purpose != ContextPurpose::Reasoning {
            selected.retain(|item| item.placement == ContextItemPlacement::System);
        }

        // 2. A duplicate source id would silently overwrite another provider's material.
        let mut seen: HashSet<String> = HashSet::new();
        for item in &selected {
            if !seen.insert(item.source_id.clone()) {
                return Err(crate::Error::Agent(format!(
                    "Duplicate context source: {}",
                    item.source_id
                )));
            }
        }

        // 3. Build the request with the material and honour the budget.
        let mut current = self.with_materials(messages.clone(), &selected);
        let limit = self.input_limit(model.as_ref(), &options);
        let mut estimate = self
            .config
            .policy
            .estimator
            .estimate(&EstimateInput::new(&current, &tools));

        if purpose == ContextPurpose::Reasoning && estimate.tokens > limit {
            let order = self.config.selection.omission_order(&selected);
            // Validate every candidate before touching the list: an eviction order that names
            // required/System material is a policy bug, not something to silently skip.
            {
                let by_id: HashMap<String, &ContextItem> = selected
                    .iter()
                    .map(|item| (item.source_id.clone(), item))
                    .collect();
                let mut seen: HashSet<&str> = HashSet::new();
                for id in &order {
                    let item = by_id.get(id.as_str()).ok_or_else(|| {
                        crate::Error::Agent(format!(
                            "Invalid context omission candidate: {id}"
                        ))
                    })?;
                    if item.required
                        || item.placement == ContextItemPlacement::System
                        || !seen.insert(id.as_str())
                    {
                        return Err(crate::Error::Agent(format!(
                            "Invalid context omission candidate: {id}"
                        )));
                    }
                }
            }
            for id in order {
                if estimate.tokens <= limit {
                    break;
                }
                selected.retain(|candidate| candidate.source_id != id);
                transforms.push(format!("omitted_for_budget:{id}"));
                current = self.with_materials(messages.clone(), &selected);
                estimate = self
                    .config
                    .policy
                    .estimator
                    .estimate(&EstimateInput::new(&current, &tools));
            }
        }

        // 4. Stable layout: one System message, conversation, state, references.
        let mut prepared = layout_messages(current);
        if self.config.project_plan_state && purpose == ContextPurpose::Reasoning {
            prepared = remove_synthetic(prepared, &[MessageKind::PlanState]);
        }
        let estimate = self
            .config
            .policy
            .estimator
            .estimate(&EstimateInput::new(&prepared, &tools));

        // 5. Validation never repairs: it reports what is wrong with the request.
        let mut validation = "passed".to_string();
        if let Err(error) = validate_tool_pairs(&prepared) {
            validation = "invalid_tool_pairs".to_string();
            let manifest = self.manifest(
                &request,
                model.as_ref(),
                &estimate,
                limit,
                &prepared,
                &tools,
                &selected,
                &transforms,
                &validation,
            );
            self.observe(&manifest);
            return Err(crate::Error::Agent(error));
        }
        if estimate.tokens > limit {
            validation = "budget_exceeded".to_string();
            let manifest = self.manifest(
                &request,
                model.as_ref(),
                &estimate,
                limit,
                &prepared,
                &tools,
                &selected,
                &transforms,
                &validation,
            );
            self.observe(&manifest);
            return Err(crate::Error::Agent(format!(
                "Prepared model input exceeds budget: estimated={}, limit={limit}",
                estimate.tokens
            )));
        }

        let manifest = self.manifest(
            &request,
            model.as_ref(),
            &estimate,
            limit,
            &prepared,
            &tools,
            &selected,
            &transforms,
            &validation,
        );
        self.observe(&manifest);

        Ok(PreparedRequest {
            messages: prepared,
            tools,
            options,
            manifest,
        })
    }

    /// Notifies the observer, swallowing a panic.
    ///
    /// Telemetry is not authority to reject an otherwise valid request: a broken observer
    /// must never fail the model call it is describing.
    fn observe(&self, manifest: &ContextManifest) {
        let observer = Arc::clone(&self.config.policy.observer);
        let payload = manifest.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            observer(&payload)
        }));
        if result.is_err() {
            tracing::warn!(
                call_id = %manifest.call_id,
                "Context observer panicked; the prepared request is unaffected"
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn manifest(
        &self,
        request: &ContextRequest,
        model: &dyn Model,
        estimate: &super::policy::Estimate,
        limit: i64,
        messages: &[Msg],
        tools: &[ToolSchema],
        selected: &[ContextItem],
        transforms: &[String],
        validation: &str,
    ) -> ContextManifest {
        let mut items = Vec::new();
        for message in messages {
            let kind = if message.role == MsgRole::System {
                "system_input"
            } else if super::render::synthetic(message) {
                "runtime_projection"
            } else {
                "message"
            };
            let content = serde_json::to_string(&message.content).unwrap_or_default();
            items.push(ContextManifestItem::new(
                kind,
                message.id.clone(),
                super::item::content_hash(&content),
            ));
        }
        for tool in tools {
            let json = serde_json::to_string(tool).unwrap_or_default();
            items.push(ContextManifestItem::new(
                "tool_schema",
                tool.name.clone(),
                super::item::content_hash(&json),
            ));
        }
        for item in selected {
            if item.content.trim().is_empty() {
                continue;
            }
            items.push(ContextManifestItem::material(item));
        }
        ContextManifest {
            call_id: request.call_id.clone(),
            purpose: request.purpose.as_str().to_string(),
            model: model.model_name().to_string(),
            policy_version: ContextManifest::POLICY_VERSION.to_string(),
            estimated_input_tokens: estimate.tokens,
            input_limit: limit,
            counting_method: estimate.method.clone(),
            exact: estimate.exact,
            task_revision: 0,
            items,
            transforms: transforms.to_vec(),
            validation: validation.to_string(),
        }
    }

    /// Replaces the previous harness projections with the current material.
    ///
    /// This is what keeps the request a projection rather than an ever-growing transcript:
    /// old runtime/reference messages are dropped and regenerated from the current material.
    fn with_materials(&self, messages: Vec<Msg>, items: &[ContextItem]) -> Vec<Msg> {
        let mut expanded: Vec<Msg> = Vec::with_capacity(messages.len());
        for message in messages {
            if super::render::has_kind(&message, MessageKind::CompiledSystem) {
                // Re-expand a previously compiled System message into its sources.
                match message.metadata.get(super::render::ORIGINAL_SYSTEM_INPUTS) {
                    Some(serde_json::Value::Array(originals)) => {
                        for original in originals {
                            if let Ok(msg) = serde_json::from_value::<Msg>(original.clone()) {
                                expanded.push(msg);
                            }
                        }
                    }
                    _ => expanded.push(message),
                }
            } else {
                expanded.push(message);
            }
        }
        let mut kept: Vec<Msg> = expanded
            .into_iter()
            .filter(|message| {
                ![
                    MessageKind::WorkspaceMaterials,
                    MessageKind::HarnessInstructions,
                    MessageKind::RuntimeMaterials,
                ]
                .iter()
                .any(|kind| super::render::has_kind(message, *kind))
            })
            .collect();

        for placement in [
            ContextItemPlacement::System,
            ContextItemPlacement::Runtime,
            ContextItemPlacement::Reference,
        ] {
            let subset: Vec<ContextItem> = items
                .iter()
                .filter(|item| item.placement == placement)
                .cloned()
                .collect();
            let rendered = render_context_blocks(&subset);
            if rendered.trim().is_empty() {
                continue;
            }
            kept.push(material_message(placement, rendered));
        }
        layout_messages(kept)
    }

    /// The input allowance for one request.
    ///
    /// Zero policy limits derive from the model window; an unknown window falls back to a
    /// conservative constant so the budget stays meaningful.
    fn input_limit(&self, model: &dyn Model, options: &GenerateOptions) -> i64 {
        let window = model.context_window_size() as i64;
        let mut output = self.config.policy.reserved_output_tokens as i64;
        if output == 0 {
            output = if window > 0 {
                std::cmp::min(4096, std::cmp::max(1, window / 8))
            } else {
                4096
            };
        }
        if let Some(max_tokens) = options.max_tokens {
            output = std::cmp::max(output, max_tokens as i64);
        }
        if let Some(max_completion) = options.max_completion_tokens {
            output = std::cmp::max(output, max_completion as i64);
        }
        let safety = if self.config.policy.safety_margin_tokens > 0 {
            self.config.policy.safety_margin_tokens as i64
        } else if window > 0 {
            std::cmp::max(1, window / 50)
        } else {
            1024
        };
        let limit = if window > 0 {
            std::cmp::max(0, window - output - safety)
        } else {
            64000
        };
        if self.config.policy.max_input_tokens > 0 {
            std::cmp::min(limit, self.config.policy.max_input_tokens as i64)
        } else {
            limit
        }
    }
}

/// Preserves call/result identities; never manufactures results to repair invalid history.
pub fn validate_tool_pairs(messages: &[Msg]) -> std::result::Result<(), String> {
    let mut pending: HashSet<String> = HashSet::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse(call) => {
                    if call.id.trim().is_empty() || !pending.insert(call.id.clone()) {
                        return Err("Duplicate or missing tool call id".into());
                    }
                }
                ContentBlock::ToolResult(result) => {
                    let id = result.id.clone().unwrap_or_default();
                    if !pending.remove(&id) {
                        return Err(format!("Tool result without matching call: {id}"));
                    }
                }
                _ => {}
            }
        }
    }
    if !pending.is_empty() {
        return Err("Unresolved tool calls in model input".into());
    }
    Ok(())
}

/// Convenience: a result block list check used by callers validating recovered history.
#[allow(dead_code)]
pub(crate) fn has_suspended_result(results: &[ToolResultBlock]) -> bool {
    results.iter().any(|result| result.is_suspended())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentscope_core::message::ToolUseBlock;
    use futures::stream::BoxStream;

    use super::*;
    use crate::agent::context::item::ContextBlock;
    use crate::agent::context::source::{ContextSourceOptions, SourceFailurePolicy};

    struct StubModel {
        window: usize,
    }

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<agentscope_core::model::ChatResponse>> {
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "stub"
        }

        fn context_window_size(&self) -> usize {
            self.window
        }
    }

    struct FixedSource(Vec<ContextBlock>);

    #[async_trait::async_trait]
    impl super::super::source::ContextSource for FixedSource {
        async fn load(
            &self,
            _request: &ContextRequest,
        ) -> crate::Result<Vec<ContextBlock>> {
            Ok(self.0.clone())
        }
    }

    fn request(purpose: ContextPurpose) -> ContextRequest {
        ContextRequest {
            call_id: "call-1".into(),
            purpose,
            ..ContextRequest::default()
        }
    }

    fn model(window: usize) -> Arc<dyn Model> {
        Arc::new(StubModel { window })
    }

    #[tokio::test]
    async fn material_is_rendered_into_the_stable_layout() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "preserve the public API")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::runtime("status", "Change 42").unwrap(),
                    ContextBlock::reference("format", "columns: a,b").unwrap(),
                ])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            ..ContextPipelineConfig::default()
        });
        let prepared = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("investigate")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();

        // System first, then conversation, then runtime state, then references.
        assert_eq!(prepared.messages[0].role, MsgRole::System);
        assert!(prepared.messages[0].text_content().contains("preserve the public API"));
        assert_eq!(prepared.messages[1].text_content(), "investigate");
        let runtime_index = prepared
            .messages
            .iter()
            .position(|m| super::super::render::has_kind(m, MessageKind::RuntimeMaterials))
            .expect("runtime material present");
        let reference_index = prepared
            .messages
            .iter()
            .position(|m| super::super::render::has_kind(m, MessageKind::WorkspaceMaterials))
            .expect("reference material present");
        assert!(runtime_index < reference_index);
        assert_eq!(prepared.manifest.validation, "passed");
        assert_eq!(prepared.manifest.policy_version, "harness-context-v2");
    }

    #[tokio::test]
    async fn optional_material_is_omitted_to_fit_the_budget() {
        // A small window forces the reference material out while the required instruction
        // (System) survives.
        let big = "x".repeat(4000);
        let sources = ContextSources::empty()
            .with_instruction("policy", "keep me")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::reference("memory/a", big.clone())
                        .unwrap()
                        .with_priority(0),
                    ContextBlock::reference("knowledge/b", big).unwrap(),
                ])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            ..ContextPipelineConfig::default()
        });
        let prepared = pipeline
            .prepare(
                model(2000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();

        // Memory is evicted before knowledge.
        assert!(
            prepared
                .manifest
                .transforms
                .iter()
                .any(|t| t.starts_with("omitted_for_budget:source/biz/memory/a")),
            "{:?}",
            prepared.manifest.transforms
        );
        let rendered: String = prepared
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("keep me"), "required instruction survives");
    }

    #[tokio::test]
    async fn an_over_budget_required_instruction_is_rejected_not_dropped() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "y".repeat(20_000))
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            ..ContextPipelineConfig::default()
        });
        let err = pipeline
            .prepare(
                model(1000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds budget"), "{err}");
    }

    #[tokio::test]
    async fn an_auxiliary_call_sees_only_system_material() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "rules")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::runtime("status", "live").unwrap(),
                ])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            ..ContextPipelineConfig::default()
        });
        let prepared = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Summary),
                vec![Msg::user("summarize")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        let rendered: String = prepared
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("rules"));
        assert!(!rendered.contains("live"));
        assert_eq!(prepared.manifest.purpose, "SUMMARY");
    }

    #[tokio::test]
    async fn tool_pairs_are_validated_without_repairing_history() {
        let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
        // A dangling tool call must be reported, never silently resolved.
        let dangling = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-1",
            "read_file",
            Default::default(),
        ))]);
        let err = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![dangling],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unresolved tool calls"), "{err}");

        // A paired call/result history passes.
        let call = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-2",
            "read_file",
            Default::default(),
        ))]);
        let result = Msg::tool_blocks(vec![ToolResultBlock::new("call-2", "read_file", vec![])]);
        pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![call, result],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_observer_receives_metadata_only() {
        let seen: Arc<Mutex<Vec<ContextManifest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let policy = ContextPolicy::new(
            0,
            0,
            0,
            Arc::new(super::super::policy::ApproximateEstimator),
            Arc::new(move |manifest: &ContextManifest| {
                sink.lock().unwrap().push(manifest.clone());
            }),
        );
        let pipeline = ContextPipeline::with_policy(policy);
        let prepared = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("secret body")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        let observed = seen.lock().unwrap().clone();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].call_id, "call-1");
        assert!(observed[0].estimated_input_tokens > 0);
        // The manifest never carries prompt bodies.
        let json = serde_json::to_string(&observed[0]).unwrap();
        assert!(!json.contains("secret body"));
        assert!(!prepared.manifest.items.is_empty());
    }

    #[tokio::test]
    async fn a_failing_observer_does_not_reject_a_valid_request() {
        let policy = ContextPolicy::new(
            0,
            0,
            0,
            Arc::new(super::super::policy::ApproximateEstimator),
            Arc::new(|_: &ContextManifest| panic!("telemetry must not reject the request")),
        );
        let pipeline = ContextPipeline::with_policy(policy);
        pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_omit_source_records_a_transform_in_the_manifest() {
        struct Failing;
        #[async_trait::async_trait]
        impl super::super::source::ContextSource for Failing {
            async fn load(
                &self,
                _request: &ContextRequest,
            ) -> crate::Result<Vec<ContextBlock>> {
                Err(crate::Error::Agent("boom".into()))
            }
        }
        let sources = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(Failing),
                ContextSourceOptions::new(
                    std::time::Duration::from_secs(1),
                    SourceFailurePolicy::Omit,
                )
                .unwrap(),
            )
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            ..ContextPipelineConfig::default()
        });
        let prepared = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        assert!(prepared
            .manifest
            .transforms
            .iter()
            .any(|t| t.starts_with("source_unavailable:source/biz/")));
    }

    #[tokio::test]
    async fn budget_derives_from_the_model_window_when_limits_are_zero() {
        let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
        let prepared = pipeline
            .prepare(
                model(100_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        // window - min(4096, window/8) - window/50 = 100000 - 4096 - 2000.
        assert_eq!(prepared.manifest.input_limit, 93_904);
    }
}
