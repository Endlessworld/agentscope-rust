//! Active-point resolution: an explicit [`Middleware::active_points`] declaration decides
//! where a middleware is invoked, and the grouping is frozen when the chain is built.
//!
//! Mirrors Java `core/middleware/ActivePointsResolverTest` and the `activePoints()` half of
//! `core/middleware/MiddlewareActivePointsTest` (commit `6b9d5840f`). Java additionally infers
//! an undeclared (`null`) middleware's points from the hook methods its class implements; Rust
//! cannot inspect which hook methods a type overrides, so the trait default (every point) is
//! the undeclared state and that tier is covered as such below.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{ReActAgent, RuntimeContext};
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::middleware::{
    ActivePointsResolver, AgentInput, EventStream, ExtensionPoint, Middleware, MiddlewareChain,
    Next, ReasoningInput,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

/// A model that always answers with plain text, so one reasoning iteration finishes the turn.
struct TextModel;

impl Model for TextModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new("ok"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "stub"
    }
}

/// Implements both the agent and the reasoning hook whatever it declares, and records every
/// hook it is actually invoked at — the declaration alone decides.
struct TraceMiddleware {
    label: &'static str,
    points: Vec<ExtensionPoint>,
    trace: Arc<Mutex<Vec<String>>>,
}

impl TraceMiddleware {
    fn new(
        label: &'static str,
        points: Vec<ExtensionPoint>,
        trace: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            label,
            points,
            trace: Arc::clone(trace),
        }
    }
}

impl Middleware for TraceMiddleware {
    fn active_points(&self) -> Vec<ExtensionPoint> {
        self.points.clone()
    }

    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:agent", self.label));
        next(input)
    }

    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:reasoning", self.label));
        next(input)
    }
}

/// Implements only the agent hook and declares nothing.
struct UndeclaredAgentHook;

impl Middleware for UndeclaredAgentHook {
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        next(input)
    }
}

/// Flips its declaration while the agent is already built.
struct FlippingDeclaration {
    participates: Arc<AtomicBool>,
    trace: Arc<Mutex<Vec<String>>>,
}

impl Middleware for FlippingDeclaration {
    fn active_points(&self) -> Vec<ExtensionPoint> {
        if self.participates.load(Ordering::SeqCst) {
            vec![ExtensionPoint::OnAgent]
        } else {
            Vec::new()
        }
    }

    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        self.trace.lock().unwrap().push("dynamic:agent".to_string());
        next(input)
    }
}

fn agent_with(chain: MiddlewareChain) -> ReActAgent {
    ReActAgent::builder()
        .name("active-points")
        .model(Arc::new(TextModel))
        .middleware_chain(chain)
        .build()
        .unwrap()
}

#[tokio::test]
async fn an_explicit_declaration_decides_where_the_middleware_runs() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let middleware = TraceMiddleware::new("B", vec![ExtensionPoint::OnReasoning], &trace);
    assert_eq!(
        ActivePointsResolver::resolve(&middleware),
        vec![ExtensionPoint::OnReasoning],
        "the declaration is authoritative, not the implemented hooks"
    );

    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(middleware));
    agent_with(chain).call(vec![Msg::user("hi")]).await.unwrap();

    assert_eq!(*trace.lock().unwrap(), vec!["B:reasoning"]);
}

#[tokio::test]
async fn an_undeclared_middleware_keeps_the_default_participation() {
    let middleware = UndeclaredAgentHook;
    assert_eq!(
        ActivePointsResolver::resolve(&middleware),
        ExtensionPoint::ALL.to_vec(),
        "an undeclared middleware participates everywhere (Java omits the declaration only \
         by inferring the implemented hooks)"
    );

    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(UndeclaredAgentHook));
    assert_eq!(chain.participants(ExtensionPoint::OnAgent).len(), 1);
    assert_eq!(chain.participants(ExtensionPoint::OnActing).len(), 1);
    agent_with(chain).call(vec![Msg::user("hi")]).await.unwrap();
}

#[tokio::test]
async fn an_empty_declaration_keeps_registration_but_never_runs() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(TraceMiddleware::new("off", Vec::new(), &trace)));

    assert_eq!(chain.len(), 1, "the registration is retained");
    assert!(chain.participants(ExtensionPoint::OnAgent).is_empty());

    let _ = agent_with(chain).call(vec![Msg::user("hi")]).await.unwrap();
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_grouping_is_frozen_when_the_chain_is_built() {
    let participates = Arc::new(AtomicBool::new(true));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(FlippingDeclaration {
        participates: Arc::clone(&participates),
        trace: Arc::clone(&trace),
    }));
    let agent = agent_with(chain);

    // A later declaration change must not reach an already-built agent: activation is
    // decided once at construction (Java `ReActAgent.groupedMiddlewares`).
    participates.store(false, Ordering::SeqCst);
    agent.call(vec![Msg::user("hi")]).await.unwrap();
    assert_eq!(*trace.lock().unwrap(), vec!["dynamic:agent"]);
}

#[tokio::test]
async fn repeated_points_collapse_to_one_invocation() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let middleware = TraceMiddleware::new(
        "dup",
        vec![ExtensionPoint::OnAgent, ExtensionPoint::OnAgent],
        &trace,
    );
    assert_eq!(
        ActivePointsResolver::resolve(&middleware),
        vec![ExtensionPoint::OnAgent],
        "Java returns a set"
    );

    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(middleware));
    agent_with(chain).call(vec![Msg::user("hi")]).await.unwrap();
    assert_eq!(*trace.lock().unwrap(), vec!["dup:agent"]);
}

#[test]
fn group_by_point_buckets_only_participants() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let middlewares: Vec<Arc<dyn Middleware>> = vec![
        Arc::new(TraceMiddleware::new(
            "reasoning",
            vec![ExtensionPoint::OnReasoning],
            &trace,
        )),
        Arc::new(TraceMiddleware::new(
            "both",
            vec![ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning],
            &trace,
        )),
        Arc::new(TraceMiddleware::new("off", Vec::new(), &trace)),
    ];

    let grouped = ActivePointsResolver::group_by_point(&middlewares);

    assert_eq!(grouped[ExtensionPoint::OnReasoning.index()].len(), 2);
    assert_eq!(grouped[ExtensionPoint::OnAgent.index()].len(), 1);
    assert_eq!(
        grouped[ExtensionPoint::OnActing.index()].len(),
        0,
        "points without participants keep an empty bucket"
    );
}
