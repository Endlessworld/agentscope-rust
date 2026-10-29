//! The single final request compiler.
//!
//! Stateless and safe to share between sessions: every preparation recomputes from the
//! committed state and the registered sources. The compiler collects material, fits it into
//! the model budget by evicting optional sources in a deterministic order, renders the result
//! into the stable layout and records a metadata-only manifest.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock};
use agentscope_core::model::catalog::{DEFAULT_RESERVED_OUTPUT, MAX_SAFETY_MARGIN};
use agentscope_core::model::{GenerateOptions, Model, ToolSchema, DEFAULT_CONTEXT_WINDOW};
use agentscope_core::state::TaskContextState;

use super::item::{ContextItem, ContextItemPlacement, ContextPurpose};
use super::policy::{
    ContextManifest, ContextManifestItem, ContextPolicy, ContextSelectionPolicy,
    DefaultSelectionPolicy, Estimate, EstimateInput,
};
use super::render::{
    layout_messages, material_message, remove_synthetic, render_context_blocks, MessageKind,
};
use super::source::{CollectedMaterials, ContextRequest, ContextSources};
use super::usage::context_usage;
use crate::agent::middleware::tool_result_eviction::{evict_messages, ToolResultFingerprint};

/// Filesystem identity for one preparation.
///
/// Evicted results are written through the workspace filesystem, which scopes paths by the
/// call identity. The compiler therefore rebuilds that identity from [`ContextRequest`]
/// instead of reading mutable agent state.
fn call_context(request: &ContextRequest) -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    if let Some(agent_id) = &request.agent_id {
        ctx = ctx.with_agent_id(agent_id.clone());
    }
    if let Some(user_id) = &request.user_id {
        ctx = ctx.with_user_id(user_id.clone());
    }
    if let Some(session_id) = &request.session_id {
        ctx = ctx.with_session_id(session_id.clone());
    }
    ctx
}

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
    /// Per-tool-result eviction run in the budget pass.
    ///
    /// Eviction belongs to the single compiler: an oversized historical tool result must be
    /// offloaded *before* the request is measured and layouted, otherwise the budget decision
    /// is made on a view the provider never receives.
    pub eviction: Option<ContextEviction>,
}

/// Per-tool-result eviction wired into the compiler's budget pass.
#[derive(Clone)]
pub struct ContextEviction {
    /// Workspace the offloaded results are written to.
    pub fs: Arc<dyn crate::filesystem::Filesystem>,
    /// Thresholds and exclusions, shared with the eviction middleware.
    pub config: crate::agent::memory::ToolResultEvictionConfig,
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
            eviction: None,
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

/// The result of compiling one request.
///
/// A reasoning request that cannot fit the model budget is reported as
/// [`PrepareOutcome::BudgetExceeded`] instead of an error, so the caller can
/// compact the conversation and retry. Callers that cannot compact keep using
/// [`ContextPipeline::prepare`], which converts the outcome into an error.
#[derive(Debug)]
pub enum PrepareOutcome {
    /// The request fits the model input budget.
    Prepared(PreparedRequest),
    /// The request exceeds the local input budget and must be compacted.
    BudgetExceeded(BudgetExceeded),
}

/// The local budget failure of one prepared request.
///
/// The fields mirror the retired `Prepared model input exceeds budget` error and
/// are kept for structured logging (`.log` warning) and for the compaction
/// fallback. This is a local accounting failure, never a gateway rejection.
#[derive(Debug, Clone)]
pub struct BudgetExceeded {
    pub model: String,
    pub catalog_key: String,
    pub estimated: i64,
    /// The local estimator's count, retained for diagnostics.
    pub local_estimated: i64,
    pub limit: i64,
    pub window: i64,
    pub reserved_output: i64,
    pub safety_margin: i64,
    pub manifest: ContextManifest,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Prepared model input exceeds budget (local): model={}, catalogKey={}, \
             estimated={}, localEstimated={}, limit={}, window={}, reserved_output={}, \
             safety_margin={}",
            self.model,
            self.catalog_key,
            self.estimated,
            self.local_estimated,
            self.limit,
            self.window,
            self.reserved_output,
            self.safety_margin
        )
    }
}

impl BudgetExceeded {
    /// Converts the failure into the error surfaced by callers that cannot compact.
    pub fn into_error(self) -> crate::Error {
        crate::Error::Agent(self.to_string())
    }
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

    /// Wires per-tool-result eviction into the budget pass.
    pub fn with_eviction(mut self, eviction: ContextEviction) -> Self {
        self.config.eviction = Some(eviction);
        self
    }

    /// Projects the committed task state into a request view.
    ///
    /// Pure and transient: the caller's history is never rewritten, only the view handed to the
    /// provider. Requirement and verification sections follow the configured options.
    fn project_task(&self, messages: Vec<Msg>, task_state: Option<&TaskContextState>) -> Vec<Msg> {
        let Some(state) = task_state else {
            return messages;
        };
        let options = self.config.sources.task_context();
        agentscope_core::middleware::project(
            messages,
            state,
            options.requirements,
            options.verification_results,
        )
    }

    /// Compiles one model request.
    ///
    /// `history` is the committed conversation; `messages` are the messages the caller already
    /// assembled (typically the same history). Sources are loaded once and reused by both the
    /// budget pass and the rendering pass. Task state is not projected.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        model: Arc<dyn Model>,
        request: ContextRequest,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> crate::Result<PreparedRequest> {
        self.prepare_with_task(model, request, messages, tools, options, None)
            .await
    }

    /// Compiles one model request, projecting the committed task state.
    ///
    /// The projection is transient: it rewrites historical `todo_write` receipts and appends
    /// one synthetic `<TASK_STATE>` message for this request only, never to durable history.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_with_task(
        &self,
        model: Arc<dyn Model>,
        request: ContextRequest,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
        task_state: Option<&TaskContextState>,
    ) -> crate::Result<PreparedRequest> {
        match self
            .prepare_with_task_outcome(model, request, messages, tools, options, task_state)
            .await?
        {
            PrepareOutcome::Prepared(prepared) => Ok(prepared),
            PrepareOutcome::BudgetExceeded(overflow) => Err(overflow.into_error()),
        }
    }

    /// Compiles one model request, reporting an over-budget reasoning request as an
    /// outcome instead of an error.
    ///
    /// The caller owns the recovery policy: the real model-call path compacts the
    /// conversation and retries, while auxiliary (summary) calls are truncated by the
    /// compiler itself because they cannot compact.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_with_task_outcome(
        &self,
        model: Arc<dyn Model>,
        request: ContextRequest,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
        task_state: Option<&TaskContextState>,
    ) -> crate::Result<PrepareOutcome> {
        let purpose = request.purpose;
        let task_revision = task_state.map(|state| state.revision()).unwrap_or(0);
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

        // 3. Tool-result eviction. Java runs this inside the compiler as well: offloading an
        // oversized historical result changes the view that is measured and sent, so doing it
        // only in a sibling middleware pass would budget a request nobody ever sees.
        let mut current = self.with_materials(messages.clone(), &selected);
        if purpose == ContextPurpose::Reasoning {
            if let Some(eviction) = &self.config.eviction {
                let fs_ctx = call_context(&request);
                let agent_name = request.agent_id.clone().unwrap_or_default();
                let mut replacements: HashMap<ToolResultFingerprint, String> = HashMap::new();
                let (rebuilt, changed) = evict_messages(
                    &eviction.fs,
                    &eviction.config,
                    &agent_name,
                    &fs_ctx,
                    current,
                    &mut replacements,
                )
                .await;
                if changed {
                    transforms.push("tool_result_offload".to_string());
                }
                current = rebuilt;
            }
        }
        // 3.5 Task projection. Java projects inside the compiler before measuring, so the
        // budget counts the synthetic state message the provider will actually receive.
        if purpose == ContextPurpose::Reasoning && task_state.is_some() {
            current = self.project_task(current, task_state);
            transforms.push("task_state_projection".to_string());
        }
        let limit = self.input_limit(model.as_ref(), &options);
        let mut local_estimate = self
            .config
            .policy
            .estimator
            .estimate(&EstimateInput::new(&current, &tools));
        // Omission decisions use the same provider-anchored count as the final budget
        // check, so optional material is dropped before a conversation is compacted.
        let mut observed_tokens = context_usage(&current, &tools, Some(model.model_name())).tokens;

        if purpose == ContextPurpose::Reasoning && observed_tokens > limit {
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
                        crate::Error::Agent(format!("Invalid context omission candidate: {id}"))
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
                if observed_tokens <= limit {
                    break;
                }
                selected.retain(|candidate| candidate.source_id != id);
                transforms.push(format!("omitted_for_budget:{id}"));
                current =
                    self.project_task(self.with_materials(messages.clone(), &selected), task_state);
                local_estimate = self
                    .config
                    .policy
                    .estimator
                    .estimate(&EstimateInput::new(&current, &tools));
                observed_tokens = context_usage(&current, &tools, Some(model.model_name())).tokens;
            }
        }

        // Auxiliary requests cannot compact a conversation. Shrink the request itself so a
        // summary prompt never fails on the local input budget: drop the oldest chat turns
        // first, then trim the largest remaining text block (the prompt body).
        let current = if purpose != ContextPurpose::Reasoning && local_estimate.tokens > limit {
            let shrunk = self.shrink_auxiliary(current, &tools, limit);
            transforms.push("auxiliary_truncated_for_budget".to_string());
            shrunk
        } else {
            current
        };

        // 4. Stable layout: one System message, conversation, state, references.
        let mut prepared = layout_messages(current);
        if self.config.project_plan_state && purpose == ContextPurpose::Reasoning {
            prepared = remove_synthetic(prepared, &[MessageKind::PlanState]);
        }
        // The provider's latest `usage.input_tokens` is the authority for everything up
        // to the assistant message that carried it; only the suffix added afterwards is
        // locally estimated. Without any provider usage (first call, or just after a
        // compaction invalidated the anchor) the local estimator is the fallback.
        let local_estimate = self
            .config
            .policy
            .estimator
            .estimate(&EstimateInput::new(&prepared, &tools));
        let usage = context_usage(&prepared, &tools, Some(model.model_name()));
        if usage.is_provider_anchored() {
            tracing::debug!(
                model = %model.model_name(),
                provider_input_tokens = usage.provider_input_tokens.unwrap_or(0),
                suffix_tokens = usage.suffix_tokens,
                local_tokens = usage.local_tokens,
                anchored_tokens = usage.tokens,
                "context size anchored on provider usage"
            );
        }
        let estimate = Estimate::new(usage.tokens, usage.method(), false)?;

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
                task_revision,
            );
            self.observe(&manifest);
            return Err(crate::Error::Agent(error));
        }
        if estimate.tokens > limit {
            if purpose == ContextPurpose::Reasoning {
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
                    task_revision,
                );
                self.observe(&manifest);
                let (window, reserved, safety, budget_limit) =
                    self.budget_breakdown(model.as_ref(), &options);
                let catalog_key = model
                    .descriptor()
                    .map(|descriptor| descriptor.key.as_str())
                    .unwrap_or("-")
                    .to_string();
                let overflow = BudgetExceeded {
                    model: model.model_name().to_string(),
                    catalog_key,
                    estimated: estimate.tokens,
                    local_estimated: local_estimate.tokens,
                    limit: budget_limit,
                    window,
                    reserved_output: reserved,
                    safety_margin: safety,
                    manifest,
                };
                // A local budget miss is a recoverable condition on the reasoning path:
                // the caller compacts the conversation and retries. Log it as a warning
                // (the ACP log file), never as the agent's terminal error.
                tracing::warn!(
                    model = %overflow.model,
                    catalog_key = %overflow.catalog_key,
                    estimated = overflow.estimated,
                    local_estimated = overflow.local_estimated,
                    limit = overflow.limit,
                    window = overflow.window,
                    reserved_output = overflow.reserved_output,
                    safety_margin = overflow.safety_margin,
                    "model input exceeds the local budget; conversation compaction is required"
                );
                return Ok(PrepareOutcome::BudgetExceeded(overflow));
            }
            // A summary call has no conversation to compact. Send the best-effort,
            // truncated request rather than failing the compaction it is serving.
            validation = "budget_exceeded_truncated".to_string();
            tracing::warn!(
                model = %model.model_name(),
                estimated = estimate.tokens,
                limit,
                "auxiliary request remains above the local budget after truncation; sending best effort"
            );
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
            task_revision,
        );
        self.observe(&manifest);

        Ok(PrepareOutcome::Prepared(PreparedRequest {
            messages: prepared,
            tools,
            options,
            manifest,
        }))
    }

    /// Shrinks an auxiliary request that cannot fit the local budget.
    ///
    /// Auxiliary calls (conversation summarization, memory extraction) have no
    /// conversation to compact, so the compiler trims the request itself: it drops
    /// the oldest chat turns while keeping the last message, then truncates the
    /// largest remaining text block (head + tail) until the estimate fits.
    fn shrink_auxiliary(
        &self,
        mut messages: Vec<Msg>,
        tools: &[ToolSchema],
        limit: i64,
    ) -> Vec<Msg> {
        let estimate = |messages: &[Msg]| {
            self.config
                .policy
                .estimator
                .estimate(&EstimateInput::new(messages, tools))
                .tokens
        };

        // Drop the oldest non-system turn, never the last message: the last message is
        // the prompt the auxiliary call exists to send.
        let mut guard = 0usize;
        while estimate(&messages) > limit && guard < messages.len() {
            guard += 1;
            let Some(index) = messages
                .iter()
                .enumerate()
                .find(|(i, message)| message.role != MsgRole::System && i + 1 < messages.len())
                .map(|(i, _)| i)
            else {
                break;
            };
            messages.remove(index);
        }

        // Trim the largest text block from both ends. The middle of a summarization
        // prompt carries the oldest conversation, which is exactly what can be lost
        // when the budget is exhausted.
        const MARKER: &str = "\n\n... truncated to fit the context budget ...\n\n";
        let mut guard = 0usize;
        while estimate(&messages) > limit && guard < messages.len().saturating_mul(32).max(32) {
            guard += 1;
            let target = messages
                .iter()
                .enumerate()
                .flat_map(|(mi, message)| {
                    message
                        .content
                        .iter()
                        .enumerate()
                        .filter_map(move |(bi, block)| match block {
                            ContentBlock::Text(text) => Some((mi, bi, text.text.chars().count())),
                            _ => None,
                        })
                })
                .max_by_key(|(_, _, chars)| *chars);
            let Some((message_index, block_index, total_chars)) = target else {
                break;
            };
            if total_chars == 0 {
                break;
            }
            let over = (estimate(&messages) - limit).max(1);
            let drop_chars = ((over as f64 * 2.5).ceil() as usize).max(64) + MARKER.chars().count();
            if drop_chars >= total_chars {
                messages[message_index].content[block_index] =
                    ContentBlock::Text(TextBlock::new(""));
                continue;
            }
            let ContentBlock::Text(text) = &messages[message_index].content[block_index] else {
                break;
            };
            let kept = total_chars - drop_chars;
            let head_len = kept / 2;
            let tail_len = kept - head_len;
            let chars: Vec<char> = text.text.chars().collect();
            let head: String = chars[..head_len].iter().collect();
            let tail: String = chars[total_chars - tail_len..].iter().collect();
            messages[message_index].content[block_index] =
                ContentBlock::Text(TextBlock::new(format!("{head}{MARKER}{tail}")));
        }
        messages
    }

    /// Notifies the observer, swallowing a panic.
    ///
    /// Telemetry is not authority to reject an otherwise valid request: a broken observer
    /// must never fail the model call it is describing.
    fn observe(&self, manifest: &ContextManifest) {
        let observer = Arc::clone(&self.config.policy.observer);
        let payload = manifest.clone();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || observer(&payload)));
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
        task_revision: u64,
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
            context_window: model.context_window_size() as i64,
            catalog_generation: if model.descriptor().is_some() {
                agentscope_core::model::catalog::ModelCatalog::global().generation()
            } else {
                0
            },
            catalog_key: model.descriptor().map(|descriptor| descriptor.key.clone()),
            estimated_input_tokens: estimate.tokens,
            input_limit: limit,
            counting_method: estimate.method.clone(),
            exact: estimate.exact,
            task_revision,
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
        self.budget_breakdown(model, options).3
    }

    /// Returns `(window, reserved_output, safety_margin, final_limit)`.
    fn budget_breakdown(
        &self,
        model: &dyn Model,
        options: &GenerateOptions,
    ) -> (i64, i64, i64, i64) {
        let window = model.context_window_size() as i64;
        let mut output = self.config.policy.reserved_output_tokens as i64;
        if output == 0 {
            output = if window > 0 {
                std::cmp::min(DEFAULT_RESERVED_OUTPUT as i64, std::cmp::max(1, window / 8))
            } else {
                DEFAULT_RESERVED_OUTPUT as i64
            };
        }
        if let Some(max_tokens) = options.max_tokens {
            output = std::cmp::max(output, max_tokens as i64);
        }
        if let Some(max_completion) = options.max_completion_tokens {
            output = std::cmp::max(output, max_completion as i64);
        }
        let model_max_output = model.max_output_tokens() as i64;
        if model_max_output > 0 {
            output = std::cmp::min(output, model_max_output);
        }
        let safety = if self.config.policy.safety_margin_tokens > 0 {
            self.config.policy.safety_margin_tokens as i64
        } else if window > 0 {
            std::cmp::min(MAX_SAFETY_MARGIN as i64, std::cmp::max(1, window / 50))
        } else {
            1024
        };
        let limit = if window > 0 {
            std::cmp::max(0, window - output - safety)
        } else {
            DEFAULT_CONTEXT_WINDOW as i64
        };
        let limit = if self.config.policy.max_input_tokens > 0 {
            std::cmp::min(limit, self.config.policy.max_input_tokens as i64)
        } else {
            limit
        };
        (window, output, safety, limit)
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

    use agentscope_core::message::{ChatUsage, ToolUseBlock};
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
        async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
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
        assert!(prepared.messages[0]
            .text_content()
            .contains("preserve the public API"));
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
        assert!(
            rendered.contains("keep me"),
            "required instruction survives"
        );
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
    async fn an_over_budget_reasoning_request_is_reported_as_an_outcome() {
        let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
        let outcome = pipeline
            .prepare_with_task_outcome(
                model(1000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("x".repeat(20_000))],
                Vec::new(),
                GenerateOptions::default(),
                None,
            )
            .await
            .unwrap();
        match outcome {
            PrepareOutcome::BudgetExceeded(info) => {
                assert!(info.estimated > info.limit);
                assert_eq!(info.window, 1_000);
                assert!(info.to_string().contains("exceeds budget"), "{info}");
            }
            PrepareOutcome::Prepared(_) => panic!("must report a budget outcome"),
        }
    }

    #[tokio::test]
    async fn an_auxiliary_request_is_truncated_instead_of_rejected() {
        let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
        let prepared = pipeline
            .prepare(
                model(1000),
                request(ContextPurpose::Summary),
                vec![Msg::user("x".repeat(50_000))],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        assert!(
            prepared.manifest.estimated_input_tokens <= prepared.manifest.input_limit,
            "{:?}",
            prepared.manifest
        );
        assert!(prepared
            .manifest
            .transforms
            .iter()
            .any(|t| t == "auxiliary_truncated_for_budget"));
    }

    #[tokio::test]
    async fn an_auxiliary_call_sees_only_system_material() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "rules")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::runtime("status", "live").unwrap()
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
            async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
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
    async fn the_manifest_uses_provider_usage_when_a_previous_call_reported_it() {
        let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
        let mut assistant = Msg::assistant("previous reply");
        assistant.usage = Some(ChatUsage {
            input_tokens: 50_000,
            output_tokens: 10,
            ..ChatUsage::default()
        });
        let prepared = pipeline
            .prepare(
                model(128_000),
                request(ContextPurpose::Reasoning),
                vec![Msg::user("hello"), assistant],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            prepared.manifest.counting_method,
            super::super::usage::PROVIDER_METHOD
        );
        assert!(
            prepared.manifest.estimated_input_tokens > 50_000,
            "{:?}",
            prepared.manifest
        );
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
        // window - min(8192, window/8) - window/50 = 100000 - 8192 - 2000.
        assert_eq!(prepared.manifest.input_limit, 89_808);
    }
}
