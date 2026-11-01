//! Training selection strategies.
//!
//! Rust port of `io.agentscope.core.training.strategy`: a unified
//! [`TrainingSelectionStrategy`] decides which agent calls are worth using for
//! training, with [`SamplingRateStrategy`] (probability based) and
//! [`ExplicitMarkingStrategy`] (caller-marked) as built-ins.
//!
//! Java propagates the training marker through Reactor's `ContextView`. Rust has
//! no equivalent implicit call-scoped context, so the marker is passed explicitly
//! as `Option<&TrainingAnnotation>` to [`TrainingSelectionStrategy::should_select`].

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A training annotation attached to an agent call, mirroring `TrainingAnnotation`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingAnnotation {
    /// Whether this call is marked for training.
    pub enabled: bool,
    /// Optional caller-specified task id (falls back to auto-generation).
    pub task_id: Option<String>,
    /// Caller-defined labels categorising the training sample.
    pub labels: Vec<String>,
    /// Additional metadata (e.g. `user_id`, `session_id`).
    pub metadata: HashMap<String, Value>,
    /// Creation timestamp in milliseconds since the Unix epoch.
    pub timestamp: u64,
}

impl Default for TrainingAnnotation {
    fn default() -> Self {
        Self {
            enabled: false,
            task_id: None,
            labels: Vec::new(),
            metadata: HashMap::new(),
            timestamp: now_millis(),
        }
    }
}

impl TrainingAnnotation {
    /// Creates an enabled annotation without labels or metadata.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Creates an enabled annotation carrying the given labels.
    pub fn with_labels<I, S>(labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            enabled: true,
            labels: labels.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Creates an enabled annotation with labels and metadata.
    pub fn with_labels_and_metadata<L, S, M, K, V>(labels: L, metadata: M) -> Self
    where
        L: IntoIterator<Item = S>,
        S: Into<String>,
        M: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Value>,
    {
        Self {
            enabled: true,
            labels: labels.into_iter().map(Into::into).collect(),
            metadata: metadata
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            ..Self::default()
        }
    }

    /// Returns a new builder.
    pub fn builder() -> TrainingAnnotationBuilder {
        TrainingAnnotationBuilder::default()
    }

    /// Whether this annotation has exceeded the given time-to-live (milliseconds).
    pub fn is_expired(&self, ttl_millis: u64) -> bool {
        now_millis().saturating_sub(self.timestamp) > ttl_millis
    }
}

/// Builder for [`TrainingAnnotation`], mirroring `TrainingAnnotation.Builder`.
#[derive(Debug, Clone)]
pub struct TrainingAnnotationBuilder {
    annotation: TrainingAnnotation,
}

impl Default for TrainingAnnotationBuilder {
    fn default() -> Self {
        Self {
            annotation: TrainingAnnotation::default(),
        }
    }
}

impl TrainingAnnotationBuilder {
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.annotation.enabled = enabled;
        self
    }
    pub fn task_id(mut self, task_id: impl Into<String>) -> Self {
        self.annotation.task_id = Some(task_id.into());
        self
    }
    pub fn labels<I, S>(mut self, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.annotation.labels = labels.into_iter().map(Into::into).collect();
        self
    }
    pub fn metadata<M, K, V>(mut self, metadata: M) -> Self
    where
        M: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Value>,
    {
        self.annotation.metadata = metadata
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }
    pub fn timestamp(mut self, timestamp: u64) -> Self {
        self.annotation.timestamp = timestamp;
        self
    }
    pub fn build(self) -> TrainingAnnotation {
        self.annotation
    }
}

/// Lightweight carrier for a training marker, mirroring `TrainingContext`.
///
/// Java's `TrainingContext` propagates the annotation through Reactor's
/// `ContextView`; Rust callers instead construct a `TrainingContext` (or a bare
/// [`TrainingAnnotation`]) and hand it to the strategy explicitly.
#[derive(Debug, Clone, Default)]
pub struct TrainingContext {
    annotation: Option<TrainingAnnotation>,
}

impl TrainingContext {
    /// Creates a context marking the call for training (no labels/metadata).
    pub fn mark() -> Self {
        Self {
            annotation: Some(TrainingAnnotation::enabled()),
        }
    }

    /// Creates a context marking the call with labels.
    pub fn mark_with_labels<I, S>(labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            annotation: Some(TrainingAnnotation::with_labels(labels)),
        }
    }

    /// Creates a context from an explicit annotation.
    pub fn with_annotation(annotation: TrainingAnnotation) -> Self {
        Self {
            annotation: Some(annotation),
        }
    }

    /// Creates a context marking the call with metadata only (no labels),
    /// mirroring Java's `TrainingContext.mark(Map<String, Object>)`.
    pub fn mark_with_metadata<M, K, V>(metadata: M) -> Self
    where
        M: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Value>,
    {
        Self {
            annotation: Some(TrainingAnnotation::with_labels_and_metadata(
                std::iter::empty::<String>(),
                metadata,
            )),
        }
    }

    /// Returns the current annotation, if any.
    pub fn current(&self) -> Option<&TrainingAnnotation> {
        self.annotation.as_ref()
    }
}

/// The result of a training-selection decision, mirroring `SelectionDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionDecision {
    /// Whether training should occur.
    pub should_train: bool,
    /// Human-readable reason (for logging/debugging).
    pub reason: String,
    /// Optional training labels.
    pub labels: Vec<String>,
    /// Optional additional metadata.
    pub metadata: HashMap<String, Value>,
}

impl SelectionDecision {
    /// Accepts for training with a reason and no extra data.
    pub fn accept(reason: impl Into<String>) -> Self {
        Self {
            should_train: true,
            reason: reason.into(),
            labels: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Accepts for training with a reason and labels.
    pub fn accept_with_labels<I, S>(reason: impl Into<String>, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            should_train: true,
            reason: reason.into(),
            labels: labels.into_iter().map(Into::into).collect(),
            metadata: HashMap::new(),
        }
    }

    /// Accepts for training with a reason, labels and metadata.
    pub fn accept_full<L, S, M, K, V>(reason: impl Into<String>, labels: L, metadata: M) -> Self
    where
        L: IntoIterator<Item = S>,
        S: Into<String>,
        M: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Value>,
    {
        Self {
            should_train: true,
            reason: reason.into(),
            labels: labels.into_iter().map(Into::into).collect(),
            metadata: metadata
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Rejects the call from training with a reason.
    pub fn reject(reason: impl Into<String>) -> Self {
        Self {
            should_train: false,
            reason: reason.into(),
            labels: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Whether training should occur.
    pub fn should_train(&self) -> bool {
        self.should_train
    }

    /// Returns the decision reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns the decision labels.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Returns the decision metadata.
    pub fn metadata(&self) -> &HashMap<String, Value> {
        &self.metadata
    }
}

/// Training request filtering strategy, mirroring `TrainingSelectionStrategy`.
pub trait TrainingSelectionStrategy: Send + Sync {
    /// Decides whether an agent call should be used for training.
    ///
    /// `annotation` carries the caller's explicit marker (if any). Java likewise
    /// receives the Reactor context here.
    fn should_select(
        &self,
        agent_name: &str,
        input_messages: &[agentscope_core::message::Msg],
        output_message: &agentscope_core::message::Msg,
        annotation: Option<&TrainingAnnotation>,
    ) -> SelectionDecision;

    /// Strategy priority (lower runs first). Defaults to 100.
    fn priority(&self) -> i32 {
        100
    }

    /// Strategy name for logging/debugging.
    fn name(&self) -> String {
        "TrainingSelectionStrategy".to_string()
    }

    /// Enables downcasting to a concrete strategy (used e.g. by
    /// `TrainingConfig::get_sample_rate`).
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Random-sampling strategy, mirroring `SamplingRateStrategy`.
pub struct SamplingRateStrategy {
    sample_rate: f64,
    rng: AtomicU64,
}

impl SamplingRateStrategy {
    /// Creates a sampling strategy for the given rate.
    ///
    /// # Panics
    ///
    /// Panics when `sample_rate` is outside `[0, 1]` (Java `IllegalArgumentException`).
    /// Use [`SamplingRateStrategy::try_of`] where a panic would abort a host thread.
    pub fn of(sample_rate: f64) -> Self {
        Self::try_of(sample_rate).expect("Sample rate must be between 0 and 1")
    }

    /// Fallible [`SamplingRateStrategy::of`]: returns an error instead of panicking when
    /// `sample_rate` is outside `[0, 1]`.
    pub fn try_of(sample_rate: f64) -> agentscope_core::error::Result<Self> {
        if !(0.0..=1.0).contains(&sample_rate) {
            return Err(agentscope_core::error::Error::Agent(
                "Sample rate must be between 0 and 1".to_string(),
            ));
        }
        let seed = now_millis() ^ (u64::from(std::process::id()) << 17) ^ 0x9E37_79B9_7F4A_7C15;
        Ok(Self {
            sample_rate,
            rng: AtomicU64::new(seed | 1),
        })
    }

    /// The configured sample rate.
    pub fn get_sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Returns a pseudo-random f64 in `[0, 1)` using an xorshift PRNG.
    fn next_unit(&self) -> f64 {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        // 53-bit mantissa in [0, 1).
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl TrainingSelectionStrategy for SamplingRateStrategy {
    fn should_select(
        &self,
        _agent_name: &str,
        _input_messages: &[agentscope_core::message::Msg],
        _output_message: &agentscope_core::message::Msg,
        _annotation: Option<&TrainingAnnotation>,
    ) -> SelectionDecision {
        if self.next_unit() < self.sample_rate {
            return SelectionDecision::accept_with_labels("sampling-rate", ["sampled"]);
        }
        SelectionDecision::reject("not-sampled")
    }

    fn priority(&self) -> i32 {
        200
    }

    fn name(&self) -> String {
        format!("SamplingRate({})", self.sample_rate)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Explicit-marking strategy, mirroring `ExplicitMarkingStrategy`.
pub struct ExplicitMarkingStrategy {
    ttl_ms: u64,
}

impl Default for ExplicitMarkingStrategy {
    fn default() -> Self {
        Self::create()
    }
}

impl ExplicitMarkingStrategy {
    /// Creates a strategy with the default one-minute TTL.
    pub fn create() -> Self {
        Self { ttl_ms: 60_000 }
    }

    /// Creates a strategy with a custom TTL in milliseconds.
    pub fn with_ttl_millis(ttl_ms: u64) -> Self {
        Self { ttl_ms }
    }

    /// Creates a strategy with a custom `std::time::Duration` TTL.
    pub fn with_ttl(ttl: std::time::Duration) -> Self {
        Self {
            ttl_ms: ttl.as_millis() as u64,
        }
    }

    /// The configured TTL in milliseconds.
    pub fn ttl_millis(&self) -> u64 {
        self.ttl_ms
    }
}

impl TrainingSelectionStrategy for ExplicitMarkingStrategy {
    fn should_select(
        &self,
        _agent_name: &str,
        _input_messages: &[agentscope_core::message::Msg],
        _output_message: &agentscope_core::message::Msg,
        annotation: Option<&TrainingAnnotation>,
    ) -> SelectionDecision {
        let Some(annotation) = annotation else {
            return SelectionDecision::reject("no-explicit-marking");
        };

        if !annotation.enabled {
            return SelectionDecision::reject("no-explicit-marking");
        }

        if annotation.is_expired(self.ttl_ms) {
            return SelectionDecision::reject("explicit-marking-expired");
        }

        let mut metadata: HashMap<String, Value> = annotation.metadata.clone();
        if let Some(task_id) = &annotation.task_id {
            metadata.insert("taskId".to_string(), Value::String(task_id.clone()));
        }

        SelectionDecision::accept_full("explicit-marking", annotation.labels.clone(), metadata)
    }

    fn priority(&self) -> i32 {
        10
    }

    fn name(&self) -> String {
        "ExplicitMarking".to_string()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Helper mirroring `TrainingContext.isExpired`.
pub fn is_expired(annotation: Option<&TrainingAnnotation>, ttl_ms: u64) -> bool {
    match annotation {
        Some(a) => a.is_expired(ttl_ms),
        None => true,
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl fmt::Display for SelectionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SelectionDecision{{shouldTrain={}, reason='{}', labels={:?}, metadata={:?}}}",
            self.should_train, self.reason, self.labels, self.metadata
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::Msg;

    fn msg() -> Msg {
        Msg::assistant("ok")
    }

    #[test]
    fn annotation_enabled_and_expiry() {
        let a = TrainingAnnotation::enabled();
        assert!(a.enabled);
        assert!(!a.is_expired(60_000));
        let old = TrainingAnnotation::builder()
            .enabled(true)
            .timestamp(now_millis().saturating_sub(5_000))
            .build();
        assert!(old.is_expired(1_000));
    }

    #[test]
    fn annotation_with_labels_and_metadata() {
        let a = TrainingAnnotation::with_labels_and_metadata(
            ["high-quality"],
            [("userId", "user-123")],
        );
        assert_eq!(a.labels, vec!["high-quality"]);
        assert_eq!(a.metadata.get("userId").unwrap(), "user-123");
    }

    #[test]
    fn sampling_bounds_accept_or_reject_deterministically() {
        let always = SamplingRateStrategy::of(1.0);
        let d = always.should_select("a", &[msg()], &msg(), None);
        assert!(d.should_train());
        assert_eq!(d.reason(), "sampling-rate");
        assert!(d.labels().contains(&"sampled".to_string()));

        let never = SamplingRateStrategy::of(0.0);
        let d = never.should_select("a", &[msg()], &msg(), None);
        assert!(!d.should_train());
        assert_eq!(d.reason(), "not-sampled");
    }

    #[test]
    #[should_panic]
    fn sampling_rejects_out_of_range() {
        let _ = SamplingRateStrategy::of(1.1);
    }

    #[test]
    fn sampling_priority_and_name() {
        let s = SamplingRateStrategy::of(0.5);
        assert_eq!(s.priority(), 200);
        assert_eq!(s.name(), "SamplingRate(0.5)");
        assert_eq!(s.get_sample_rate(), 0.5);
    }

    #[test]
    fn explicit_rejects_without_annotation() {
        let s = ExplicitMarkingStrategy::create();
        let d = s.should_select("a", &[msg()], &msg(), None);
        assert!(!d.should_train());
        assert_eq!(d.reason(), "no-explicit-marking");
    }

    #[test]
    fn explicit_accepts_enabled_annotation() {
        let s = ExplicitMarkingStrategy::create();
        let a = TrainingAnnotation::with_labels(["x"]);
        let d = s.should_select("a", &[msg()], &msg(), Some(&a));
        assert!(d.should_train());
        assert_eq!(d.reason(), "explicit-marking");
    }

    #[test]
    fn explicit_passes_through_task_id() {
        let s = ExplicitMarkingStrategy::create();
        let a = TrainingAnnotation::builder()
            .enabled(true)
            .task_id("custom-task-id")
            .build();
        let d = s.should_select("a", &[msg()], &msg(), Some(&a));
        assert!(d.should_train());
        assert_eq!(d.metadata().get("taskId").unwrap(), "custom-task-id");
    }

    #[test]
    fn explicit_rejects_expired() {
        let s = ExplicitMarkingStrategy::with_ttl_millis(1);
        let a = TrainingAnnotation::builder()
            .enabled(true)
            .timestamp(now_millis().saturating_sub(1_000))
            .build();
        let d = s.should_select("a", &[msg()], &msg(), Some(&a));
        assert!(!d.should_train());
        assert_eq!(d.reason(), "explicit-marking-expired");
    }

    #[test]
    fn explicit_priority_and_name() {
        let s = ExplicitMarkingStrategy::create();
        assert_eq!(s.priority(), 10);
        assert_eq!(s.name(), "ExplicitMarking");
    }

    #[test]
    fn training_context_mark() {
        let ctx = TrainingContext::mark();
        assert!(ctx.current().unwrap().enabled);
        let ctx = TrainingContext::mark_with_labels(["prod"]);
        assert_eq!(ctx.current().unwrap().labels, vec!["prod"]);
    }

    // --- Additional offline coverage -------------------------------------------------

    #[test]
    fn training_context_mark_with_metadata_and_annotation() {
        // Mirrors Java `TrainingContext.mark(Map)`: enabled, no labels, metadata set.
        let ctx = TrainingContext::mark_with_metadata([("user_id", "u-1")]);
        let a = ctx.current().unwrap();
        assert!(a.enabled);
        assert!(a.labels.is_empty());
        assert_eq!(a.metadata.get("user_id").unwrap(), "u-1");

        let explicit = TrainingAnnotation::builder()
            .enabled(false)
            .task_id("t-9")
            .build();
        let ctx = TrainingContext::with_annotation(explicit.clone());
        assert_eq!(ctx.current().unwrap(), &explicit);
        assert!(TrainingContext::default().current().is_none());
    }

    #[test]
    fn training_annotation_full_builder_and_serde() {
        let a = TrainingAnnotation::builder()
            .enabled(true)
            .task_id("t-1")
            .labels(["l1", "l2"])
            .metadata([("k", "v")])
            .timestamp(1234)
            .build();
        assert!(a.enabled);
        assert_eq!(a.task_id.as_deref(), Some("t-1"));
        assert_eq!(a.labels, vec!["l1", "l2"]);
        assert_eq!(a.metadata.get("k").unwrap(), "v");
        assert_eq!(a.timestamp, 1234);
        // Round-trips through serde (used by the annotation-carrying paths).
        let json = serde_json::to_string(&a).unwrap();
        let back: TrainingAnnotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn is_expired_helper_handles_absent_annotation() {
        // Mirrors Java `TrainingContext.isExpired(null, ttl) == true`.
        assert!(is_expired(None, 60_000));
        let fresh = TrainingAnnotation::enabled();
        assert!(!is_expired(Some(&fresh), 60_000));
        let stale = TrainingAnnotation::builder()
            .enabled(true)
            .timestamp(now_millis().saturating_sub(5_000))
            .build();
        assert!(is_expired(Some(&stale), 1_000));
    }

    #[test]
    fn selection_decision_factories_and_display() {
        let d = SelectionDecision::accept("r");
        assert!(d.should_train());
        assert!(d.labels().is_empty());
        assert!(d.metadata().is_empty());

        let d = SelectionDecision::accept_with_labels("r", ["a"]);
        assert_eq!(d.labels(), ["a".to_string()]);

        let d = SelectionDecision::accept_full("r", ["a"], [("k", "v")]);
        assert_eq!(d.metadata().get("k").unwrap(), "v");

        let d = SelectionDecision::reject("nope");
        assert!(!d.should_train());
        assert_eq!(d.reason(), "nope");
        assert!(format!("{d}").contains("shouldTrain=false"));
    }

    #[test]
    fn sampling_rejects_when_rate_partial_boundary() {
        // rate 1.0 always accepts, rate 0.0 always rejects (deterministic bounds).
        for _ in 0..8 {
            assert!(SamplingRateStrategy::of(1.0)
                .should_select("a", &[], &msg(), None)
                .should_train());
            assert!(!SamplingRateStrategy::of(0.0)
                .should_select("a", &[], &msg(), None)
                .should_train());
        }
    }

    #[test]
    fn explicit_rejects_disabled_annotation() {
        let s = ExplicitMarkingStrategy::with_ttl(std::time::Duration::from_secs(60));
        assert_eq!(s.ttl_millis(), 60_000);
        let a = TrainingAnnotation::builder().enabled(false).build();
        let d = s.should_select("a", &[], &msg(), Some(&a));
        assert!(!d.should_train());
        assert_eq!(d.reason(), "no-explicit-marking");
    }

    #[test]
    fn explicit_merges_annotation_metadata() {
        let s = ExplicitMarkingStrategy::create();
        let a = TrainingAnnotation::builder()
            .enabled(true)
            .labels(["hi"])
            .metadata([("user_id", "u-2")])
            .task_id("t-2")
            .build();
        let d = s.should_select("a", &[], &msg(), Some(&a));
        assert!(d.should_train());
        assert_eq!(d.labels(), ["hi".to_string()]);
        assert_eq!(d.metadata().get("user_id").unwrap(), "u-2");
        assert_eq!(d.metadata().get("taskId").unwrap(), "t-2");
    }
}
