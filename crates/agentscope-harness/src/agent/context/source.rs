//! Dynamic context sources and their collection rules.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::item::{ContextBlock, ContextItem, ContextPurpose};

/// What happens when a context source fails to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFailurePolicy {
    /// Fail the whole request.
    Fail,
    /// Omit the source and record a transform; required blocks are rejected.
    Omit,
}

/// Immutable loading settings. Defaults: five-second timeout, fail the request on error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSourceOptions {
    pub timeout: Duration,
    pub on_failure: SourceFailurePolicy,
}

impl Default for ContextSourceOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            on_failure: SourceFailurePolicy::Fail,
        }
    }
}

impl ContextSourceOptions {
    /// Rejects a non-positive timeout.
    pub fn new(timeout: Duration, on_failure: SourceFailurePolicy) -> crate::Result<Self> {
        if timeout.is_zero() {
            return Err(crate::Error::Agent(
                "Positive source timeout required".into(),
            ));
        }
        Ok(Self {
            timeout,
            on_failure,
        })
    }
}

/// The immutable invocation identity handed to a source.
///
/// It deliberately exposes no mutable agent state; the identities may be absent and do not
/// establish authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRequest {
    pub agent_id: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub call_id: String,
    pub purpose: ContextPurpose,
}

impl Default for ContextRequest {
    fn default() -> Self {
        Self {
            agent_id: None,
            user_id: None,
            session_id: None,
            call_id: String::new(),
            purpose: ContextPurpose::Reasoning,
        }
    }
}

/// Read-only dynamic context. Shared instances must be thread-safe and authorize each
/// request.
#[async_trait::async_trait]
pub trait ContextSource: Send + Sync {
    /// Loads the blocks for one request.
    async fn load(&self, request: &ContextRequest) -> crate::Result<Vec<ContextBlock>>;
}

/// A boxed source so registrations can be stored uniformly.
pub type BoxedContextSource = Arc<dyn ContextSource>;

/// One registered source with its loading options.
#[derive(Clone)]
pub struct ContextSourceRegistration {
    pub id: String,
    pub source: BoxedContextSource,
    pub options: ContextSourceOptions,
}

impl std::fmt::Debug for ContextSourceRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextSourceRegistration")
            .field("id", &self.id)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// The collected material of one preparation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectedMaterials {
    pub items: Vec<ContextItem>,
    pub transforms: Vec<String>,
}

/// Internal immutable registration snapshot used by the pipeline.
#[derive(Clone, Default)]
pub struct ContextSources {
    sources: Vec<ContextSourceRegistration>,
    instructions: Vec<ContextItem>,
    task_context: super::policy::TaskContextOptions,
}

impl std::fmt::Debug for ContextSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextSources")
            .field("sources", &self.sources.len())
            .field("instructions", &self.instructions.len())
            .field("task_context", &self.task_context)
            .finish()
    }
}

impl ContextSources {
    /// No sources, no instructions.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Adds a source; the id must be unique and well formed.
    pub fn with_source(
        mut self,
        id: impl Into<String>,
        source: BoxedContextSource,
        options: ContextSourceOptions,
    ) -> crate::Result<Self> {
        let id = id.into();
        valid_id(&id)?;
        if self.sources.iter().any(|s| s.id == id) {
            return Err(crate::Error::Agent(format!(
                "Duplicate context source: {id}"
            )));
        }
        self.sources.push(ContextSourceRegistration {
            id,
            source,
            options,
        });
        Ok(self)
    }

    /// Adds a trusted instruction (System placement, required).
    pub fn with_instruction(
        mut self,
        id: impl Into<String>,
        content: impl Into<String>,
    ) -> crate::Result<Self> {
        let id = id.into();
        valid_id(&id)?;
        let source_id = format!("instruction/{id}");
        if self.instructions.iter().any(|i| i.source_id == source_id) {
            return Err(crate::Error::Agent(format!("Duplicate instruction: {id}")));
        }
        self.instructions.push(ContextItem::instruction(
            "business_instruction",
            source_id,
            content,
        )?);
        Ok(self)
    }

    /// Replaces the task projection options.
    pub fn with_task_context(mut self, options: super::policy::TaskContextOptions) -> Self {
        self.task_context = options;
        self
    }

    /// The task projection options.
    pub fn task_context(&self) -> super::policy::TaskContextOptions {
        self.task_context
    }

    /// The registered instructions (System material).
    pub fn instructions(&self) -> &[ContextItem] {
        &self.instructions
    }

    /// Whether any source or instruction is registered.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.instructions.is_empty()
    }

    /// Loads every source (in registration order) plus the registered instructions.
    ///
    /// Only a reasoning preparation loads dynamic sources; an auxiliary call sees just the
    /// System instructions.
    pub async fn collect(&self, request: &ContextRequest) -> crate::Result<CollectedMaterials> {
        if request.purpose != ContextPurpose::Reasoning {
            return Ok(CollectedMaterials {
                items: self.instructions.clone(),
                transforms: Vec::new(),
            });
        }
        let mut items = self.instructions.clone();
        let mut transforms = Vec::new();
        for registration in &self.sources {
            let collected = self.collect_source(registration, request).await?;
            items.extend(collected.items);
            transforms.extend(collected.transforms);
        }
        Ok(CollectedMaterials { items, transforms })
    }

    async fn collect_source(
        &self,
        registration: &ContextSourceRegistration,
        request: &ContextRequest,
    ) -> crate::Result<CollectedMaterials> {
        let prefix = format!("source/{}/", registration.id);
        let loaded = match tokio::time::timeout(
            registration.options.timeout,
            registration.source.load(request),
        )
        .await
        {
            Ok(Ok(blocks)) => Ok(blocks),
            Ok(Err(error)) => Err(error),
            Err(_elapsed) => Err(crate::Error::Agent("Context source timed out".into())),
        };
        let blocks = match loaded {
            Ok(blocks) => blocks,
            Err(error) => {
                if registration.options.on_failure != SourceFailurePolicy::Omit {
                    return Err(error);
                }
                return Ok(CollectedMaterials {
                    items: Vec::new(),
                    transforms: vec![format!(
                        "source_unavailable:{prefix}:{}",
                        error_kind(&error)
                    )],
                });
            }
        };

        // Structural violations are never hidden by the optional-source failure policy.
        let mut ids = HashSet::new();
        let mut items = Vec::with_capacity(blocks.len());
        for block in blocks {
            let mut item = block.material();
            if item.source_id.trim().is_empty() {
                return Err(crate::Error::Agent("Null context block".into()));
            }
            if !ids.insert(item.source_id.clone()) {
                return Err(crate::Error::Agent(format!(
                    "Duplicate block: {prefix}{}",
                    item.source_id
                )));
            }
            if item.required && registration.options.on_failure == SourceFailurePolicy::Omit {
                return Err(crate::Error::Agent(
                    "Required blocks cannot come from an OMIT source".into(),
                ));
            }
            item.source_id = format!("{prefix}{}", item.source_id);
            items.push(item);
        }
        Ok(CollectedMaterials {
            items,
            transforms: Vec::new(),
        })
    }
}

fn valid_id(id: &str) -> crate::Result<()> {
    let mut chars = id.chars();
    let first_ok = chars
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false);
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return Err(crate::Error::Agent(format!(
            "Invalid context source/instruction ID: {id}"
        )));
    }
    Ok(())
}

fn error_kind(error: &crate::Error) -> String {
    match error {
        crate::Error::Agent(_) => "Agent".into(),
        crate::Error::Model(_) => "Model".into(),
        crate::Error::Tool(_) => "Tool".into(),
        crate::Error::Http(_) => "Http".into(),
        crate::Error::HttpStatus { .. } => "HttpStatus".into(),
        crate::Error::Io(_) => "Io".into(),
        _ => "Other".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource(Vec<ContextBlock>);

    #[async_trait::async_trait]
    impl ContextSource for FixedSource {
        async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
            Ok(self.0.clone())
        }
    }

    struct FailingSource;

    #[async_trait::async_trait]
    impl ContextSource for FailingSource {
        async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
            Err(crate::Error::Http("upstream down".into()))
        }
    }

    fn request(purpose: ContextPurpose) -> ContextRequest {
        ContextRequest {
            call_id: "call-1".into(),
            purpose,
            ..ContextRequest::default()
        }
    }

    #[tokio::test]
    async fn instructions_and_sources_are_collected_in_order() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "preserve API")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![ContextBlock::runtime(
                    "status", "running",
                )
                .unwrap()])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let collected = sources
            .collect(&request(ContextPurpose::Reasoning))
            .await
            .unwrap();
        assert_eq!(collected.items.len(), 2);
        assert_eq!(collected.items[0].source_id, "instruction/policy");
        assert_eq!(collected.items[1].source_id, "source/biz/status");
    }

    #[tokio::test]
    async fn auxiliary_purposes_load_only_instructions() {
        let sources = ContextSources::empty()
            .with_instruction("policy", "x")
            .unwrap()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![ContextBlock::runtime(
                    "status", "running",
                )
                .unwrap()])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let collected = sources
            .collect(&request(ContextPurpose::Summary))
            .await
            .unwrap();
        assert_eq!(collected.items.len(), 1);
        assert_eq!(collected.items[0].source_id, "instruction/policy");
    }

    #[tokio::test]
    async fn a_failing_source_fails_the_request_by_default() {
        let sources = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(FailingSource),
                ContextSourceOptions::default(),
            )
            .unwrap();
        assert!(sources
            .collect(&request(ContextPurpose::Reasoning))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_omit_source_records_a_transform_instead() {
        let sources = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(FailingSource),
                ContextSourceOptions::new(Duration::from_secs(1), SourceFailurePolicy::Omit)
                    .unwrap(),
            )
            .unwrap();
        let collected = sources
            .collect(&request(ContextPurpose::Reasoning))
            .await
            .unwrap();
        assert!(collected.items.is_empty());
        assert_eq!(collected.transforms.len(), 1);
        assert!(collected.transforms[0].starts_with("source_unavailable:source/biz/:"));
        assert!(collected.transforms[0].ends_with("Http"));
    }

    #[tokio::test]
    async fn required_blocks_cannot_come_from_an_omit_source() {
        let sources = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![ContextBlock::runtime("status", "x")
                    .unwrap()
                    .required()])),
                ContextSourceOptions::new(Duration::from_secs(1), SourceFailurePolicy::Omit)
                    .unwrap(),
            )
            .unwrap();
        let err = sources
            .collect(&request(ContextPurpose::Reasoning))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Required blocks"), "{err}");
    }

    #[tokio::test]
    async fn duplicate_source_ids_and_blocks_are_rejected() {
        let sources = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        assert!(sources
            .clone()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![])),
                ContextSourceOptions::default()
            )
            .is_err());

        let dup = ContextSources::empty()
            .with_source(
                "biz",
                Arc::new(FixedSource(vec![
                    ContextBlock::runtime("status", "a").unwrap(),
                    ContextBlock::runtime("status", "b").unwrap(),
                ])),
                ContextSourceOptions::default(),
            )
            .unwrap();
        let err = dup
            .collect(&request(ContextPurpose::Reasoning))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Duplicate block"), "{err}");
    }

    #[test]
    fn ids_are_validated() {
        assert!(ContextSources::empty()
            .with_instruction("bad id", "x")
            .is_err());
        assert!(ContextSources::empty()
            .with_instruction("ok.id-1", "x")
            .is_ok());
        let sources = ContextSources::empty()
            .with_instruction("dup", "x")
            .unwrap();
        assert!(sources.with_instruction("dup", "y").is_err());
    }

    #[test]
    fn options_reject_a_zero_timeout() {
        assert!(ContextSourceOptions::new(Duration::ZERO, SourceFailurePolicy::Fail).is_err());
        assert_eq!(
            ContextSourceOptions::default().timeout,
            Duration::from_secs(5)
        );
        assert_eq!(
            ContextSourceOptions::default().on_failure,
            SourceFailurePolicy::Fail
        );
    }
}
