//! Runtime context shared across agent execution.
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::formatter::ResponseFormat;
use crate::message::{JsonMap, Msg};
use crate::state::AgentState;
use crate::tool::Toolkit;

/// A copy-on-write bag of typed, keyed execution-scoped values.
///
/// Mirrors the Java `RuntimeContext.put(key, value)` / `get(key)` surface used to carry
/// execution services (session recorder, inbox command, action observer) without widening
/// [`RuntimeContext`] with one field per subsystem.
///
/// Cloning is cheap and **value-semantic**: a clone shares storage until either side
/// inserts, at which point the writer takes its own copy. Mutations therefore never leak
/// into the context a caller cloned from (Java copies its attribute map the same way).
#[derive(Clone, Default)]
pub struct Extensions {
    values: Arc<HashMap<&'static str, Arc<dyn Any + Send + Sync>>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.values.len())
            .finish()
    }
}

impl Extensions {
    /// Creates an empty bag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a value, replacing any value stored under the same key.
    pub fn insert<T: Any + Send + Sync>(&mut self, key: &'static str, value: T) {
        self.insert_arc(key, Arc::new(value));
    }

    /// Inserts an already-shared value, so callers that hold an `Arc` keep ownership.
    pub fn insert_arc<T: Any + Send + Sync>(&mut self, key: &'static str, value: Arc<T>) {
        Arc::make_mut(&mut self.values).insert(key, value as Arc<dyn Any + Send + Sync>);
    }

    /// Returns the value stored under `key` when it has type `T`.
    pub fn get<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        self.values
            .get(key)
            .and_then(|value| Arc::clone(value).downcast::<T>().ok())
    }

    /// Whether a value is stored under `key` (of any type).
    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Overlays `other` onto this bag; keys present in `other` win.
    ///
    /// Used to layer per-run services (a session recorder) on top of the services an agent
    /// was built with, without dropping the build-time ones.
    pub fn extend(&mut self, other: &Extensions) {
        if other.values.is_empty() {
            return;
        }
        let merged = Arc::make_mut(&mut self.values);
        for (key, value) in other.values.iter() {
            merged.insert(key, Arc::clone(value));
        }
    }

    /// Removes and returns a value when it has type `T`.
    pub fn remove<T: Any + Send + Sync>(&mut self, key: &str) -> Option<Arc<T>> {
        let removed = Arc::make_mut(&mut self.values).remove(key)?;
        removed.downcast::<T>().ok()
    }

    /// Number of stored values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Maximum length, in characters, of a normalized agent id.
///
/// The id is interpolated into namespace/session path segments (`agents/{agent}/…`), so it is
/// capped well below the 255-byte filesystem component limit: backends append prefixes and
/// suffixes of their own to the segment. Java uses 255 (`AgentBase#MAX_AGENT_ID_LENGTH`); the
/// tighter budget here keeps room for those decorations.
pub const MAX_AGENT_ID_LENGTH: usize = 128;

/// Placeholder returned by [`normalize_agent_id`] when an id carries nothing usable.
pub const DEFAULT_AGENT_ID: &str = "agent";

/// Whether `ch` may appear in a normalized agent id.
fn is_agent_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// Whether `ch` is meaningful in an id (`.` alone is not: `..` is a traversal segment).
fn is_usable_agent_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

/// Normalizes an agent id into a single path/namespace-safe segment.
///
/// Unlike the Java `AgentBase#normalizeAgentId`, which rejects a malformed id with
/// `IllegalArgumentException`, this **coerces** instead of failing: the id reaches
/// [`RuntimeContext::with_agent_id`], whose `-> Self` builder contract cannot report an error,
/// and losing a whole run over a cosmetic id would be worse than sanitizing it.
///
/// Rules, applied in order:
/// 1. surrounding whitespace is trimmed;
/// 2. every character outside `[A-Za-z0-9._-]` — path separators (`/`, `\\`), NUL, control
///    characters, interior whitespace, non-ASCII — becomes `_`; a raw separator therefore can
///    never introduce a new path level;
/// 3. the result is truncated to [`MAX_AGENT_ID_LENGTH`] characters;
/// 4. an id with nothing usable left (empty, blank, all separators, all dots — `..` would escape
///    the namespace) collapses to [`DEFAULT_AGENT_ID`], so the result is never empty.
///
/// Step 4 inspects the *source* characters of the retained prefix rather than the substituted
/// ones, so an all-separator id (`"///"`) falls back instead of degrading to `"___"`. Reading only
/// the retained prefix also keeps the rule stable under truncation: `".".repeat(200) + "a"` drops
/// the trailing `a`, sees nothing usable, and falls back on the first pass — so a second pass
/// cannot change the value again.
///
/// The mapping is idempotent: `normalize(normalize(x)) == normalize(x)`.
pub fn normalize_agent_id(agent_id: impl AsRef<str>) -> String {
    let raw = agent_id.as_ref().trim();
    // Exactly the characters that can end up in the result.
    let prefix: String = raw.chars().take(MAX_AGENT_ID_LENGTH).collect();
    if !prefix.chars().any(is_usable_agent_id_char) {
        return DEFAULT_AGENT_ID.to_string();
    }
    prefix
        .chars()
        .map(|ch| if is_agent_id_char(ch) { ch } else { '_' })
        .collect()
}

/// Runtime context shared across agent execution, mirroring `RuntimeContext`.
#[derive(Debug, Clone, Default)]
pub struct RuntimeContext {
    /// Agent name for this execution.
    pub agent_name: Option<String>,
    /// Raw user input.
    pub user_input: Option<String>,
    /// The current conversation memory.
    pub memory: Vec<Msg>,
    /// The live toolkit (may be empty).
    pub toolkit: Option<Toolkit>,
    /// Runtime metadata.
    pub metadata: JsonMap,
    /// Working directory for this execution.
    pub cwd: Option<String>,
    /// Optional structured-output schema for this call's reasoning phase.
    pub response_format: Option<ResponseFormat>,
    /// Session id for this execution (used by harness middlewares to scope inbox/async tools).
    pub session_id: Option<String>,
    /// User id for this execution.
    pub user_id: Option<String>,
    /// Agent id (as opposed to agent name) for this execution.
    ///
    /// Set it through [`RuntimeContext::with_agent_id`], which applies [`normalize_agent_id`];
    /// writing the field directly bypasses that normalization.
    pub agent_id: Option<String>,
    /// Correlation id for this single execution, aligned with [`crate::agent::AgentRun::run_id`].
    pub run_id: Option<String>,
    /// Optional sender used to forward fine-grained delta events (text/thinking)
    /// from deep inside the model call up to the agent-level event stream, enabling
    /// incremental (token-by-token) rendering.
    pub delta_tx: Option<
        futures::channel::mpsc::UnboundedSender<crate::error::Result<crate::event::AgentEvent>>,
    >,
    /// Shared per-agent state handle, exposed so context-managing middlewares
    /// (e.g. conversation compaction) can read and rewrite the accumulated
    /// history across calls. `None` for ad-hoc contexts (tool calls, tests).
    pub state: Option<Arc<Mutex<AgentState>>>,
    /// Typed execution-scoped services (session recorder, inbox command, action
    /// observer, …), mirroring the Java `RuntimeContext` attribute map.
    pub extensions: Extensions,
}

impl RuntimeContext {
    /// Sets the session id.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the user id.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the agent id, normalized by [`normalize_agent_id`].
    ///
    /// Normalization happens here rather than at the call sites so no caller can smuggle a
    /// separator or an over-long id into the namespace paths built from [`RuntimeContext::agent_id`].
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(normalize_agent_id(agent_id.into()));
        self
    }

    /// Returns the session id, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the user id, if any.
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Returns the agent id, if any.
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Sets the run id, adopting `run_id` when non-blank and generating one otherwise.
    pub fn with_run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = Some(
            run_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(super::run::generate_run_id),
        );
        self
    }

    /// Returns the run id, or `None` for an ad-hoc context (tool calls, tests).
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }
}

impl RuntimeContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts an execution-scoped service, replacing any value under the same key.
    pub fn set_extension<T: Any + Send + Sync>(&mut self, key: &'static str, value: T) {
        self.extensions.insert(key, value);
    }

    /// Returns the execution-scoped service stored under `key`, when present.
    pub fn extension<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        self.extensions.get::<T>(key)
    }

    /// Builder-style [`RuntimeContext::set_extension`].
    pub fn with_extension<T: Any + Send + Sync>(mut self, key: &'static str, value: T) -> Self {
        self.set_extension(key, value);
        self
    }

    /// Inserts an already-shared execution-scoped service.
    pub fn set_extension_arc<T: Any + Send + Sync>(&mut self, key: &'static str, value: Arc<T>) {
        self.extensions.insert_arc(key, value);
    }

    /// Builder-style [`RuntimeContext::set_extension_arc`].
    pub fn with_extension_arc<T: Any + Send + Sync>(
        mut self,
        key: &'static str,
        value: Arc<T>,
    ) -> Self {
        self.set_extension_arc(key, value);
        self
    }

    /// Returns the accumulated text content of the memory.
    pub fn text_content(&self) -> String {
        self.memory
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (a) A path separator never survives, so the id cannot add a path level.
    #[test]
    fn normalize_replaces_path_separators() {
        assert_eq!(normalize_agent_id("a/b"), "a_b");
        assert!(!normalize_agent_id("a/b").contains('/'));
        assert_eq!(normalize_agent_id("a\\b"), "a_b");
        assert_eq!(normalize_agent_id("../../etc/passwd"), ".._.._etc_passwd");
        // Non-`/` hazards too: NUL, interior whitespace, control characters.
        assert_eq!(normalize_agent_id("a\0b"), "a_b");
        assert_eq!(normalize_agent_id("a b"), "a_b");
        assert_eq!(normalize_agent_id("a\tb\nc"), "a_b_c");
        assert_eq!(normalize_agent_id("  padded  "), "padded");
        assert_eq!(normalize_agent_id("agént"), "ag_nt");
        // Already-safe ids are untouched.
        assert_eq!(normalize_agent_id("agent-1.v2_x"), "agent-1.v2_x");
    }

    // (b) Over-long ids are truncated to the exported cap.
    #[test]
    fn normalize_truncates_over_long_ids() {
        let long = "a".repeat(MAX_AGENT_ID_LENGTH + 50);
        let normalized = normalize_agent_id(&long);
        assert_eq!(normalized.chars().count(), MAX_AGENT_ID_LENGTH);
        assert_eq!(normalized, "a".repeat(MAX_AGENT_ID_LENGTH));

        // At the limit exactly nothing is dropped.
        let at_limit = "b".repeat(MAX_AGENT_ID_LENGTH);
        assert_eq!(normalize_agent_id(&at_limit), at_limit);

        // Truncation counts characters, not bytes: a multibyte-heavy id still
        // fills the cap and stays on a char boundary.
        let multibyte = "aé".repeat(MAX_AGENT_ID_LENGTH);
        let normalized = normalize_agent_id(&multibyte);
        assert_eq!(normalized.chars().count(), MAX_AGENT_ID_LENGTH);
        assert_eq!(normalized, "a_".repeat(MAX_AGENT_ID_LENGTH / 2));
    }

    // (c) Normalization is idempotent.
    #[test]
    fn normalize_is_idempotent() {
        let long = "z".repeat(MAX_AGENT_ID_LENGTH + 20);
        // The trailing `a` is truncation fodder: once cut, the remainder is all dots and must
        // already have fallen back, otherwise a second pass would change the value again.
        let dots_then_char = format!("{}a", ".".repeat(MAX_AGENT_ID_LENGTH + 10));
        let inputs = [
            "a/b",
            "a\\b",
            "  spaced out  ",
            "...",
            "",
            "   ",
            "///",
            "ok-1",
            "agént/../x",
            long.as_str(),
            dots_then_char.as_str(),
        ];
        for input in inputs {
            let once = normalize_agent_id(input);
            let twice = normalize_agent_id(&once);
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }

    // (d) Empty / all-invalid input still yields a usable, non-empty id.
    #[test]
    fn normalize_never_yields_empty_id() {
        for input in ["", "   ", "\t\n", "///", "\\\\", "\0", "...", "..../...."] {
            let normalized = normalize_agent_id(input);
            assert!(!normalized.is_empty(), "empty result for {input:?}");
            assert_eq!(
                normalized, DEFAULT_AGENT_ID,
                "expected fallback for {input:?}"
            );
            assert!(!normalized.contains('/'), "separator leaked for {input:?}");
        }
        // A dot-only id would be a traversal segment, so it falls back as well.
        assert_eq!(normalize_agent_id(".."), DEFAULT_AGENT_ID);
        // Punctuation-only ids are already safe and stay as-is (non-empty).
        assert_eq!(normalize_agent_id("-_"), "-_");
    }

    // (e) `with_agent_id` applies the normalization; this is the wiring assertion.
    #[test]
    fn with_agent_id_normalizes() {
        let ctx = RuntimeContext::new().with_agent_id("a/b");
        assert_eq!(ctx.agent_id(), Some("a_b"));

        let ctx = RuntimeContext::new().with_agent_id("  spaced id  ");
        assert_eq!(ctx.agent_id(), Some("spaced_id"));

        let ctx = RuntimeContext::new().with_agent_id("");
        assert_eq!(ctx.agent_id(), Some(DEFAULT_AGENT_ID));

        let ctx = RuntimeContext::new().with_agent_id("x".repeat(MAX_AGENT_ID_LENGTH + 1));
        assert_eq!(ctx.agent_id().map(str::len), Some(MAX_AGENT_ID_LENGTH));

        // An already-valid id is preserved verbatim.
        let ctx = RuntimeContext::new().with_agent_id("agent-1");
        assert_eq!(ctx.agent_id(), Some("agent-1"));
    }
}
