//! A spawned subagent must inherit the parent session's permission mode and the persisted
//! "always allow / always reject" rules *before* its first turn.
//!
//! Regression: the bridge installed only the confirm handler, so every child ran with the
//! trivial context (mode `Default`, no rules). Under `accept_edits` a shell command issued by a
//! subagent therefore executed without ever raising `session/request_permission` — the user was
//! asked for the parent's tools and not for the child's.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use agent_client_protocol::v1::schema::SessionId;
use agentscope_core::agent::CallableAgent;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::permission::{PermissionBehavior, PermissionMode, PermissionRule};
use agentscope_extensions_acp::config::{permission_context_for_mode, MODE_ACCEPT_EDITS};
use agentscope_extensions_acp::permission_store::PermissionRuleStore;
use agentscope_extensions_acp::session::SubagentPermissionHook;
use agentscope_harness::agent::HarnessAgent;
use agentscope_harness::tool::SubagentRuntimeHook;

/// A model that answers "done" without calling a tool: these tests never run a turn.
struct StubModel;

#[async_trait::async_trait]
impl Model for StubModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "stub"
    }
}

fn fresh_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_acp_permission_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn build_child(root: &PathBuf) -> HarnessAgent {
    HarnessAgent::builder()
        .name("acp-subagent-permission-test")
        .model(Arc::new(StubModel))
        .session_id("sub-x")
        .cwd(root.clone())
        .workspace(root.join(".agentscope").join("workspace"))
        .disable_memory_hooks()
        .max_iters(1)
        .build()
}

fn hook_for(
    root: &PathBuf,
    mode: &str,
) -> (
    SubagentPermissionHook,
    Arc<Mutex<HashMap<String, Weak<dyn CallableAgent>>>>,
) {
    let agents: Arc<Mutex<HashMap<String, Weak<dyn CallableAgent>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let hook = SubagentPermissionHook::new(
        Arc::new(Mutex::new(None)),
        Arc::new(PermissionRuleStore::at_dir(root)),
        SessionId::new("parent-session"),
        Arc::new(Mutex::new(mode.to_string())),
        Arc::clone(&agents),
    );
    (hook, agents)
}

#[test]
fn a_child_inherits_the_session_mode_and_the_persisted_rules() {
    let root = fresh_root("inherits");
    let store = PermissionRuleStore::at_dir(&root);
    let user_rule = PermissionRule::new(
        "execute",
        Some("cargo test".to_string()),
        PermissionBehavior::Allow,
        "user",
    );
    store.add(user_rule.clone()).unwrap();

    let (hook, agents) = hook_for(&root, MODE_ACCEPT_EDITS);
    let child = Arc::new(build_child(&root));
    let dyn_child: Arc<dyn CallableAgent> = child.clone();
    hook.on_child_ready(None, "sub-x", &dyn_child);

    let ctx = child
        .agent()
        .expect("the harness agent wraps a ReActAgent")
        .permission_context();
    // Without the inherited context this is `PermissionMode::Default` and every tool runs
    // silently: exactly the bug the user reported.
    assert_eq!(
        ctx.mode,
        PermissionMode::AcceptEdits,
        "the child must run under the mode the session was in"
    );

    let expected = permission_context_for_mode(MODE_ACCEPT_EDITS);
    assert_eq!(
        ctx.allow_rules.get("write_file"),
        expected.allow_rules.get("write_file"),
        "the mode's own auto-allow rules must survive on the child"
    );
    assert_eq!(
        ctx.allow_rules
            .get("execute")
            .and_then(|rules| rules.first()),
        Some(&user_rule),
        "an `always allow` answer must apply to children too"
    );

    assert_eq!(
        agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len(),
        1,
        "the child is registered so a cancel addressed to it can be routed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_finished_child_is_not_pinned_by_the_registry() {
    let root = fresh_root("weak");
    let (hook, agents) = hook_for(&root, MODE_ACCEPT_EDITS);
    let child = Arc::new(build_child(&root));
    let dyn_child: Arc<dyn CallableAgent> = child.clone();
    hook.on_child_ready(None, "sub-x", &dyn_child);

    let registered = agents
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get("sub-x")
        .cloned()
        .expect("the child is registered");
    assert!(registered.upgrade().is_some());

    drop(dyn_child);
    drop(child);
    assert!(
        registered.upgrade().is_none(),
        "the registry must not keep a finished subagent (and its toolkit) alive"
    );
    let _ = std::fs::remove_dir_all(&root);
}
