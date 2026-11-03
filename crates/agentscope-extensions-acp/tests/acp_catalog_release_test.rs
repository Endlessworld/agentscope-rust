//! The parsed model catalog is ~15 MB for ~9k models and is only needed to *list* models and to
//! resolve a switch. Once a session has bound its model the host hands the table back; these tests
//! pin the session side of that contract: binding happens eagerly (so the release is safe) and the
//! per-turn lookup never asks the catalog again.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::SessionId;
use agentscope_core::message::Msg;
use agentscope_core::model::catalog::ModelCatalog;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::{release_catalog_if_unused, AcpAgentSession, AcpCodingAgent};
use agentscope_harness::agent::HarnessAgent;

/// Never called: the session is only built, never prompted.
struct IdleModel;

#[async_trait::async_trait]
impl Model for IdleModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::empty())
    }

    fn model_name(&self) -> &str {
        "idle"
    }
}

fn bound_session() -> AcpAgentSession {
    let cwd = std::env::temp_dir().join(format!("acp-catalog-release-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let agent = HarnessAgent::builder()
        .name("acp-catalog")
        .model(Arc::new(IdleModel))
        .build();
    AcpAgentSession::new(SessionId::new("s-catalog"), cwd, agent, vec![], vec![])
}

#[tokio::test]
async fn a_session_binds_its_model_before_the_catalog_is_released() {
    let mut session = bound_session();
    // Answered at construction time, even for a model the catalog has nothing for: nothing is
    // left for a later turn to look up.
    assert!(session.has_cached_descriptor());
    assert!(session.selected_descriptor().is_none());
    assert!(session.selected_catalog_key().is_none());

    // A switch re-binds immediately instead of deferring to the next prompt.
    session.set_model("not-in-any-catalog");
    assert!(session.has_cached_descriptor());
    assert!(session.selected_descriptor().is_none());
}

#[tokio::test]
async fn releasing_neither_loads_the_catalog_nor_needs_a_session() {
    let mut registry: HashMap<String, AcpAgentSession> = HashMap::new();
    registry.insert("s-catalog".to_string(), bound_session());
    release_catalog_if_unused(&Mutex::new(registry));
    // No sessions at all is the same situation: nothing may be parsed just to release it.
    release_catalog_if_unused(&Mutex::new(HashMap::<String, AcpAgentSession>::new()));
    assert!(!ModelCatalog::global().is_loaded());
}

#[test]
fn initialize_reads_the_cached_image_capability_instead_of_the_catalog() {
    // No models at all: answering from the catalog can only say "no image", so the cached value
    // is the only way the advertised capability can flip — which is what `initialize` reads.
    let agent = AcpCodingAgent::new(
        |_, _| unreachable!("capabilities never builds a session"),
        vec![],
        vec![],
    );
    assert!(agent.image_support().get().is_none());
    assert!(!agent.capabilities().prompt_capabilities.image);
    // The first `initialize` computed the answer and kept it, so the next one does not parse.
    assert_eq!(agent.image_support().get(), Some(false));

    // A host that refreshes the cache while the table is resident (startup, each refresh) decides
    // what `initialize` advertises from then on.
    agent.image_support().set(true);
    assert!(agent.capabilities().prompt_capabilities.image);
    assert_eq!(agent.image_support().get(), Some(true));
}
