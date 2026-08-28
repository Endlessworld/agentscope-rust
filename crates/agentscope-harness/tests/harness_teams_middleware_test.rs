//! Tests for `TeamsMiddleware`: system-prompt injection and mailbox drain.
use std::sync::Arc;

use futures::StreamExt;

use agentscope_core::middleware::{Middleware, Next, ReasoningInput};
use agentscope_core::message::{ContentBlock, Msg, MsgRole};

use agentscope_harness::agent::middleware::TeamsMiddleware;
use agentscope_harness::agent::team::{
    LocalTeamClient, TeamClient, TeamContext, TeamCreateSpec, TeamMemberSpec, TeamRole,
};

type Slot = Arc<std::sync::Mutex<Option<ReasoningInput>>>;

fn capture_pair() -> (Slot, Next<ReasoningInput>) {
    let slot: Slot = Arc::new(std::sync::Mutex::new(None));
    let slot2 = slot.clone();
    let next: Next<ReasoningInput> = Box::new(move |input| {
        *slot2.lock().unwrap() = Some(input);
        let events: Vec<agentscope_core::error::Result<agentscope_core::event::AgentEvent>> =
            Vec::new();
        futures::stream::iter(events).boxed()
    });
    (slot, next)
}

fn reasoning_input() -> ReasoningInput {
    ReasoningInput::new(
        vec![Msg::builder()
            .role(MsgRole::System)
            .content(vec![ContentBlock::Text(agentscope_core::message::TextBlock::new(
                "base prompt",
            ))])
            .build()],
        Vec::new(),
        agentscope_core::model::GenerateOptions::default(),
    )
}

fn setup(role: TeamRole) -> (Arc<LocalTeamClient>, TeamsMiddleware, &'static str) {
    let client = Arc::new(LocalTeamClient::new());
    client
        .create_team(TeamCreateSpec {
            id: "t1".into(),
            name: "alpha".into(),
            members: vec![
                TeamMemberSpec::new("lead", TeamRole::Lead),
                TeamMemberSpec::new("dev", TeamRole::Member),
            ],
        })
        .unwrap();
    let mw = TeamsMiddleware::new(client.clone(), "t1", "dev").with_role(role);
    (client, mw, "t1")
}

fn text_of(msg: &Msg) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_turn(mw: &TeamsMiddleware, slot: Slot, next: Next<ReasoningInput>) -> ReasoningInput {
    let stream = mw.on_reasoning(&Default::default(), reasoning_input(), next);
    let _ = futures::StreamExt::collect::<Vec<_>>(stream).await;
    slot.lock().unwrap().take().unwrap()
}

#[tokio::test]
async fn injects_team_context_into_system_prompt() {
    let (_c, mw, _) = setup(TeamRole::Member);
    let (slot, next) = capture_pair();
    let out = run_turn(&mw, slot, next).await;

    let sys = text_of(&out.messages[0]);
    assert!(sys.contains("<agent-teams>"), "{sys}");
    assert!(sys.contains("team `t1`"), "{sys}");
    assert!(sys.contains("- lead [LEAD]"), "{sys}");
    assert!(sys.contains("Available operations"), "{sys}");
    // Base prompt preserved after injection.
    assert!(sys.contains("base prompt"), "{sys}");
}

#[tokio::test]
async fn drains_mailbox_into_team_messages() {
    let (client, mw, t) = setup(TeamRole::Member);
    client.send_message(t, "lead", "dev", "please check the board").unwrap();

    let (slot, next) = capture_pair();
    let out = run_turn(&mw, slot, next).await;

    let last = out.messages.last().unwrap();
    assert_eq!(last.role, MsgRole::Assistant);
    let text = text_of(last);
    assert!(text.contains("<team-message from=\"lead\""), "{text}");
    assert!(text.contains("please check the board"), "{text}");

    // Second turn: mailbox already drained → no extra message.
    let (slot2, next2) = capture_pair();
    let out2 = run_turn(&mw, slot2, next2).await;
    assert!(!text_of(out2.messages.last().unwrap()).contains("<team-message"));
}

#[tokio::test]
async fn no_mailbox_no_extra_message() {
    let (_c, mw, _) = setup(TeamRole::Member);
    let (slot, next) = capture_pair();
    let out = run_turn(&mw, slot, next).await;
    // Only the system message remains (no assistant tail).
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.messages[0].role, MsgRole::System);
}

#[test]
fn role_clipping_via_context() {
    let lead = TeamContext::for_role("t", TeamRole::Lead);
    let member = TeamContext::for_role("t", TeamRole::Member);
    assert!(lead.allows("spawn_member"));
    assert!(!member.allows("spawn_member"));
}
