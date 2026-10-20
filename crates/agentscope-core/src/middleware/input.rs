//! Input types for middleware interception points.
use std::sync::{Arc, Mutex};

use crate::message::{ContentBlock, Msg, TextBlock, ToolResultBlock, ToolResultState, ToolUseBlock};
use crate::model::{GenerateOptions, Model, ToolSchema};

/// Input for the agent invocation interception point.
#[derive(Debug, Clone)]
pub struct AgentInput {
    /// The input messages.
    pub messages: Vec<Msg>,
}

impl AgentInput {
    pub fn new(messages: Vec<Msg>) -> Self {
        Self { messages }
    }
}

/// Input for the reasoning (model-call) interception point.
#[derive(Debug, Clone)]
pub struct ReasoningInput {
    /// The messages sent to the model.
    pub messages: Vec<Msg>,
    /// The tool schemas available to the model.
    pub tools: Vec<ToolSchema>,
    /// The generation options.
    pub options: GenerateOptions,
}

impl ReasoningInput {
    pub fn new(messages: Vec<Msg>, tools: Vec<ToolSchema>, options: GenerateOptions) -> Self {
        Self { messages, tools, options }
    }
}

/// Input for the tool-call execution interception point.
#[derive(Debug, Clone)]
pub struct ActingInput {
    /// The tool call blocks to execute.
    pub tool_calls: Vec<ToolUseBlock>,
    /// Where the acting phase's tool results are collected.
    ///
    /// Java acting middlewares resolve the mutable `AgentState` from
    /// `RuntimeContext` and write their own placeholder results into the context
    /// (e.g. `AsyncToolMiddleware.emitPlaceholderAndComplete`). Rust's
    /// `on_acting` receives `&RuntimeContext` only, so the equivalent write goes
    /// through this handle: the agent persists whatever it holds once the stream
    /// ends.
    pub results: ActingResultsHandle,
}

impl ActingInput {
    pub fn new(tool_calls: Vec<ToolUseBlock>) -> Self {
        Self { tool_calls, results: ActingResultsHandle::new() }
    }

    /// Creates an input bound to an existing results handle.
    pub fn with_results(tool_calls: Vec<ToolUseBlock>, results: ActingResultsHandle) -> Self {
        Self { tool_calls, results }
    }
}

/// Shared result sink for one acting phase.
///
/// Serves the same purpose as Java's `AtomicReference<List<Entry<ToolUseBlock,
/// ToolResultBlock>>> resultHolder` in `ReActAgent.acting`, with one addition: it
/// also records the phase flags (`executed` / `suspended` / `returnDirect`
/// eligibility) so the agent can decide the short-circuit after the stream ends.
///
/// Results are keyed by tool-call id and **the first writer wins**: when a
/// middleware offloads a call and writes a placeholder, the still-running
/// background execution cannot overwrite what the agent already persisted.
#[derive(Debug, Clone, Default)]
pub struct ActingResultsHandle(Arc<Mutex<ActingResults>>);

#[derive(Debug)]
struct ActingResults {
    entries: Vec<(String, ToolResultBlock)>,
    executed: usize,
    suspended: bool,
    return_direct_eligible: bool,
    finished: bool,
    error: Option<String>,
}

impl Default for ActingResults {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            executed: 0,
            suspended: false,
            // Optimistic: the phase short-circuits only when every executed result
            // qualifies. `executed == 0` already disqualifies it.
            return_direct_eligible: true,
            finished: false,
            error: None,
        }
    }
}

/// A point-in-time view of an [`ActingResultsHandle`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActingResultsSnapshot {
    /// Results in first-write order.
    pub results: Vec<ToolResultBlock>,
    /// Number of calls that actually ran (placeholders do not count).
    pub executed: usize,
    /// Whether any executed call suspended itself.
    pub suspended: bool,
    /// Whether every executed result qualifies for a `returnDirect` short-circuit.
    pub return_direct_eligible: bool,
    /// Whether the acting core ran to completion.
    pub finished: bool,
    /// A run-level failure raised inside the core (a middleware never sees it as an
    /// event, so it is carried here instead of being dropped by the spawned task).
    pub error: Option<String>,
}

impl ActingResultsHandle {
    /// Creates an empty handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the result of a call the core actually executed.
    pub fn record(&self, block: ToolResultBlock, return_direct: bool) {
        let mut inner = self.0.lock().unwrap();
        inner.executed += 1;
        inner.suspended |= block.is_suspended();
        inner.return_direct_eligible &= return_direct;
        inner.push_first(&block);
    }

    /// Records a placeholder result for a call a middleware offloaded (or otherwise
    /// answered itself) without executing it.
    ///
    /// The block is marked `Success`, mirroring Java
    /// `AsyncToolMiddleware.emitPlaceholderAndComplete`, which emits
    /// `ToolResultEndEvent(..., ToolResultState.SUCCESS)` for its placeholder.
    pub fn record_placeholder(&self, call: &ToolUseBlock, text: &str) {
        let block = ToolResultBlock {
            id: Some(call.id.clone()),
            name: Some(call.name.clone()),
            output: vec![ContentBlock::Text(TextBlock::new(text))],
            metadata: Default::default(),
            state: ToolResultState::Success,
        };
        let mut inner = self.0.lock().unwrap();
        inner.push_first(&block);
    }

    /// Marks the acting core as having run to completion.
    pub fn finish(&self) {
        self.0.lock().unwrap().finished = true;
    }

    /// Records a run-level failure raised by the acting core.
    pub fn fail(&self, message: String) {
        let mut inner = self.0.lock().unwrap();
        if inner.error.is_none() {
            inner.error = Some(message);
        }
    }

    /// Returns a snapshot of the current state.
    pub fn snapshot(&self) -> ActingResultsSnapshot {
        let inner = self.0.lock().unwrap();
        ActingResultsSnapshot {
            results: inner.entries.iter().map(|(_, b)| b.clone()).collect(),
            executed: inner.executed,
            suspended: inner.suspended,
            return_direct_eligible: inner.return_direct_eligible,
            finished: inner.finished,
            error: inner.error.clone(),
        }
    }
}

impl ActingResults {
    fn push_first(&mut self, block: &ToolResultBlock) {
        match block.id.as_deref() {
            Some(id) if self.entries.iter().any(|(existing, _)| existing == id) => {}
            Some(id) => self.entries.push((id.to_string(), block.clone())),
            // A result without an id cannot be matched against a call; keep it so it
            // is still fed back rather than silently dropped.
            None => self.entries.push((String::new(), block.clone())),
        }
    }
}

/// Input for the raw model API call interception point.
#[derive(Clone)]
pub struct ModelCallInput {
    /// The messages sent to the model.
    pub messages: Vec<Msg>,
    /// The tool schemas available to the model.
    pub tools: Vec<ToolSchema>,
    /// The generation options.
    pub options: GenerateOptions,
    /// The model being invoked.
    pub model: Arc<dyn Model>,
}

impl ModelCallInput {
    pub fn new(
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
        model: Arc<dyn Model>,
    ) -> Self {
        Self { messages, tools, options, model }
    }
}

/// A boxed event stream produced by a middleware chain step.
pub type EventStream = futures::stream::BoxStream<'static, crate::error::Result<crate::event::AgentEvent>>;
