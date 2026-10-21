//! Budget-only auxiliary model calls, mirroring `ContextModelCalls`.
//!
//! An auxiliary call (memory-flush extraction, conversation summarization) is compiled by the
//! same final request compiler as a reasoning call, but it is prepared with
//! [`ContextPurpose::Summary`]: it sees System instructions only, never projects task state and
//! never re-enters [`ContextMiddleware`](super::ContextMiddleware) — so it cannot recursively
//! compact the conversation or inject task state.
//!
//! The request is built from an empty source set with the default policy: an auxiliary call has
//! no dynamic sources and no instructions of its own, yet it still honours the model's input
//! budget and emits the same metadata-only construction manifest.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model};
use futures::StreamExt;

use super::builder::ContextPipeline;
use super::item::ContextPurpose;
use super::policy::ContextPolicy;
use super::source::ContextRequest;

/// Runs one auxiliary (summary) model call and returns every streamed response chunk.
///
/// The call is compiled with [`ContextPipeline::with_policy`] using [`ContextPolicy::default`]
/// and no sources or instructions, then streamed with the *prepared* messages, tools and
/// options. The construction manifest is emitted by the pipeline itself, so callers never
/// observe it twice. Identities come from `ctx` when one is available; they are absent for
/// callers (compaction) that hold none.
///
/// Errors from preparation (budget, invalid tool pairs) and from the model stream are returned
/// unchanged: it is the caller's decision whether an auxiliary failure is fatal.
pub async fn auxiliary(
    model: Arc<dyn Model>,
    messages: Vec<Msg>,
    ctx: Option<&RuntimeContext>,
) -> crate::Result<Vec<ChatResponse>> {
    let request = ContextRequest {
        agent_id: ctx.and_then(|ctx| ctx.agent_id().map(str::to_string)),
        user_id: ctx.and_then(|ctx| ctx.user_id().map(str::to_string)),
        session_id: ctx.and_then(|ctx| ctx.session_id().map(str::to_string)),
        call_id: uuid::Uuid::new_v4().to_string(),
        purpose: ContextPurpose::Summary,
    };

    let pipeline = ContextPipeline::with_policy(ContextPolicy::default());
    let prepared = pipeline
        .prepare(
            Arc::clone(&model),
            request,
            messages,
            Vec::new(),
            GenerateOptions::default(),
        )
        .await?;

    // Journal the auxiliary boundary exactly like a reasoning dispatch: the wrapper only
    // records when the caller supplied a context that carries a session recorder.
    let model = match ctx {
        Some(ctx) => agentscope_core::session::SessionModels::wrap(Arc::clone(&model), ctx, "SUMMARY"),
        None => model,
    };
    let mut stream = model.stream(prepared.messages, prepared.tools, prepared.options);
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

/// Runs one auxiliary (summary) model call and concatenates the text blocks of every chunk.
///
/// This is the convenience seam for callers that only need the produced text (memory flush,
/// conversation summarization). Non-text blocks are ignored, exactly like the Java callers that
/// reduce over `TextBlock`s. Errors are propagated unchanged.
pub async fn auxiliary_text(
    model: Arc<dyn Model>,
    messages: Vec<Msg>,
    ctx: Option<&RuntimeContext>,
) -> crate::Result<String> {
    let mut out = String::new();
    for chunk in auxiliary(model, messages, ctx).await? {
        for block in chunk.content {
            if let ContentBlock::Text(text) = block {
                out.push_str(&text.text);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentscope_core::message::{ChatUsage, MsgRole, TextBlock};
    use agentscope_core::model::ToolSchema;
    use futures::stream::{self, BoxStream};

    use super::super::builder::ContextPipelineConfig;
    use super::super::item::{ContextBlock, ContextItemPlacement};
    use super::super::policy::{ApproximateEstimator, ContextManifest};
    use super::super::source::{ContextSource, ContextSourceOptions, ContextSources};
    use super::*;

    /// Records the request the model actually received and replays fixed text chunks.
    struct RecordingModel {
        chunks: Vec<String>,
        window: usize,
        fail: bool,
        seen_messages: Arc<Mutex<Vec<String>>>,
        seen_tools: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingModel {
        fn new(chunks: Vec<&str>, window: usize) -> Self {
            Self {
                chunks: chunks.into_iter().map(str::to_string).collect(),
                window,
                fail: false,
                seen_messages: Arc::new(Mutex::new(Vec::new())),
                seen_tools: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::new(Vec::new(), 0)
            }
        }
    }

    impl Model for RecordingModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            self.seen_messages
                .lock()
                .unwrap()
                .extend(messages.iter().map(Msg::text_content));
            self.seen_tools
                .lock()
                .unwrap()
                .extend(tools.iter().map(|tool| tool.name.clone()));
            let items: Vec<crate::Result<ChatResponse>> = if self.fail {
                vec![Err(crate::Error::Agent("model down".into()))]
            } else {
                self.chunks
                    .iter()
                    .map(|text| {
                        Ok(ChatResponse::new(
                            "r1",
                            vec![ContentBlock::Text(TextBlock::new(text.clone()))],
                            ChatUsage::default(),
                            None,
                            None,
                        ))
                    })
                    .collect()
            };
            Box::pin(stream::iter(items))
        }

        fn model_name(&self) -> &str {
            "recording"
        }

        fn context_window_size(&self) -> usize {
            self.window
        }
    }

    #[tokio::test]
    async fn an_auxiliary_call_streams_the_compiled_request() {
        let model = Arc::new(RecordingModel::new(
            vec!["- user likes Rust", "\n- project uses tokio"],
            128_000,
        ));
        let seen_messages = Arc::clone(&model.seen_messages);
        let seen_tools = Arc::clone(&model.seen_tools);
        let messages = vec![
            Msg::builder()
                .role(MsgRole::System)
                .content(vec![ContentBlock::Text(TextBlock::new("extract memories"))])
                .build(),
            Msg::user("I like Rust"),
        ];

        let text = auxiliary_text(model, messages, None).await.unwrap();
        assert_eq!(text, "- user likes Rust\n- project uses tokio");

        let rendered = seen_messages.lock().unwrap().join("\n");
        assert!(rendered.contains("extract memories"), "{rendered}");
        assert!(rendered.contains("I like Rust"), "{rendered}");
        // An auxiliary call never carries tool schemas.
        assert!(seen_tools.lock().unwrap().is_empty());
    }

    struct FixedSource(Vec<ContextBlock>);

    #[async_trait::async_trait]
    impl ContextSource for FixedSource {
        async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
            Ok(self.0.clone())
        }
    }

    /// The contract `auxiliary` relies on: a Summary preparation keeps System material only and
    /// records the wire purpose `SUMMARY` in the manifest.
    #[tokio::test]
    async fn a_summary_preparation_drops_non_system_material() {
        let manifests = Arc::new(Mutex::new(Vec::<ContextManifest>::new()));
        let captured = Arc::clone(&manifests);
        let sources = ContextSources::empty()
            .with_instruction("policy", "preserve the public API")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::runtime("status", "Change 42 under review").unwrap(),
                ])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let pipeline = ContextPipeline::new(ContextPipelineConfig {
            sources,
            policy: ContextPolicy::new(
                0,
                0,
                0,
                Arc::new(ApproximateEstimator),
                Arc::new(move |manifest: &ContextManifest| {
                    captured.lock().unwrap().push(manifest.clone());
                }),
            ),
            ..ContextPipelineConfig::default()
        });
        let model: Arc<dyn Model> = Arc::new(RecordingModel::new(Vec::new(), 128_000));
        let prepared = pipeline
            .prepare(
                model,
                ContextRequest {
                    call_id: "call-1".into(),
                    purpose: ContextPurpose::Summary,
                    ..ContextRequest::default()
                },
                vec![Msg::user("summarize me")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .await
            .unwrap();

        let rendered = prepared
            .messages
            .iter()
            .map(Msg::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("preserve the public API"), "{rendered}");
        assert!(!rendered.contains("Change 42 under review"), "{rendered}");

        let manifests = manifests.lock().unwrap();
        let manifest = manifests.last().expect("prepare must emit a manifest");
        assert_eq!(manifest.purpose, "SUMMARY");
        assert_eq!(manifest.purpose, ContextPurpose::Summary.as_str());
        assert!(manifest
            .items
            .iter()
            .any(|item| item.source_id == "instruction/policy"));
        assert!(manifest.items.iter().all(|item| {
            item.placement.as_deref() != Some(ContextItemPlacement::Runtime.as_str())
        }));
    }

    #[tokio::test]
    async fn an_auxiliary_call_surfaces_model_errors() {
        let model: Arc<dyn Model> = Arc::new(RecordingModel::failing());
        let error = auxiliary(model, vec![Msg::user("hi")], None)
            .await
            .unwrap_err();
        assert!(matches!(error, crate::Error::Agent(_)), "got {error:?}");
    }
}
