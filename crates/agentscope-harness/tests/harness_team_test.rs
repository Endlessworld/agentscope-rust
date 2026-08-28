//! Tests for the `agent/team` module: LocalTeamClient task board, mailboxes,
//! wakeups, CAS conflicts and role clipping.
use agentscope_harness::agent::team::{
    LocalTeamClient, TeamClient, TeamContext, TeamCreateSpec, TeamError, TeamMemberSpec,
    TeamRole, TeamTaskStatus,
};

fn team() -> (LocalTeamClient, &'static str) {
    let client = LocalTeamClient::new();
    client
        .create_team(TeamCreateSpec {
            id: "t1".into(),
            name: "alpha".into(),
            members: vec![
                TeamMemberSpec::new("lead", TeamRole::Lead),
                TeamMemberSpec::new("dev", TeamRole::Member),
                TeamMemberSpec::new("qa", TeamRole::Member),
            ],
        })
        .expect("team created");
    (client, "t1")
}

#[test]
fn create_task_and_claim_flow() {
    let (c, t) = team();
    let task = c.create_task(t, "implement x", "details", vec![]).unwrap();
    assert_eq!(task.status, TeamTaskStatus::Todo);
    assert!(task.assignee.is_none());
    assert_eq!(task.version, 1);

    let claimed = c.claim_task(t, &task.id, "dev").unwrap();
    assert_eq!(claimed.status, TeamTaskStatus::InProgress);
    assert_eq!(claimed.assignee.as_deref(), Some("dev"));
    assert_eq!(claimed.version, 2);

    let done = c.complete_task(t, &task.id, "dev").unwrap();
    assert_eq!(done.status, TeamTaskStatus::Done);
}

#[test]
fn claim_respects_blocked_by_dependencies() {
    let (c, t) = team();
    let dep = c.create_task(t, "first", "", vec![]).unwrap();
    let child = c
        .create_task(t, "second", "", vec![dep.id.clone()])
        .unwrap();

    let err = c.claim_task(t, &child.id, "dev").unwrap_err();
    match err {
        TeamError::Blocked(id, deps) => {
            assert_eq!(id, child.id);
            assert_eq!(deps, vec![dep.id.clone()]);
        }
        other => panic!("expected Blocked, got {other:?}"),
    }

    // Complete the dependency → claim succeeds.
    let v = c.list_tasks(t).unwrap().iter().find(|x| x.id == dep.id).unwrap().version;
    let _ = c.complete_task(t, &dep.id, "lead").unwrap();
    drop(v);
    let claimed = c.claim_task(t, &child.id, "qa").unwrap();
    assert_eq!(claimed.status, TeamTaskStatus::InProgress);
}

#[test]
fn assign_task_cas_conflict_and_unknown_member() {
    let (c, t) = team();
    let task = c.create_task(t, "x", "", vec![]).unwrap();

    // Unknown member.
    let err = c.assign_task(t, &task.id, "ghost", 1).unwrap_err();
    assert!(matches!(err, TeamError::MemberNotFound(_)));

    // Stale version → Conflict.
    let err = c.assign_task(t, &task.id, "dev", 99).unwrap_err();
    assert!(matches!(err, TeamError::Conflict { .. }), "{err:?}");

    // Fresh version wins.
    let assigned = c.assign_task(t, &task.id, "dev", task.version).unwrap();
    assert_eq!(assigned.assignee.as_deref(), Some("dev"));
}

#[test]
fn mailbox_direct_vs_broadcast_and_drain() {
    let (c, t) = team();
    c.send_message(t, "lead", "dev", "please review").unwrap();

    // Broadcast skips sender; lead is the sender here.
    let sent = c.broadcast_message(t, "lead", "standup in 5").unwrap();
    assert_eq!(sent.len(), 2, "dev + qa, not lead");

    let dev_mail = c.drain_mailbox(t, "dev");
    assert_eq!(dev_mail.len(), 2, "direct + broadcast: {dev_mail:?}");
    // Draining empties the box.
    assert!(c.drain_mailbox(t, "dev").is_empty());

    // qa got only broadcast.
    assert_eq!(c.drain_mailbox(t, "qa").len(), 1);
}

#[tokio::test]
async fn wakeup_delivery_on_message() {
    let (c, t) = team();
    let wakeups = c.subscribe_wakeups(t, "dev");

    c.send_message(t, "lead", "dev", "ping").unwrap();
    let msg = tokio::time::timeout(std::time::Duration::from_secs(1), wakeups.recv())
        .await
        .expect("wakeup within timeout")
        .expect("message");
    assert_eq!(msg.content, "ping");
}

#[test]
fn member_lifecycle_and_role_contexts() {
    let (c, t) = team();

    // Lead context allows spawn/shutdown.
    let lead_ctx = TeamContext::for_role(t, TeamRole::Lead);
    assert!(lead_ctx.allows("spawn_member") && lead_ctx.allows("claim_task"));

    // Member context does not.
    let member_ctx = TeamContext::for_role(t, TeamRole::Member);
    assert!(!member_ctx.allows("spawn_member"));
    assert!(member_ctx.allows("claim_task"));

    // Runtime spawn then shutdown.
    c.spawn_member(t, TeamMemberSpec::new("helper", TeamRole::Member)).unwrap();
    let info = c.team_info(t).unwrap();
    assert!(info.members.iter().any(|m| m.name == "helper"));

    c.shutdown_member(t, "helper").unwrap();
    let info = c.team_info(t).unwrap();
    let helper = info.members.iter().find(|m| m.name == "helper").unwrap();
    assert!(!helper.active);

    // Messaging a deactivated member still lands in their mailbox (preserved),
    // but shutdown of an unknown member errors.
    let err = c.shutdown_member(t, "ghost").unwrap_err();
    assert!(matches!(err, TeamError::MemberNotFound(_)));
}

#[test]
fn unknown_team_surfaces_not_found() {
    let (c, _t) = team();
    let err = c.create_task("nope", "x", "", vec![]).unwrap_err();
    assert!(matches!(err, TeamError::TeamNotFound(_)));
}
