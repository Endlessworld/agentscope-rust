//! Tests for `TeamTool`: operation dispatch, role clipping, error mapping.
use std::sync::Arc;

use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::agent::team::{
    LocalTeamClient, TeamClient, TeamCreateSpec, TeamMemberSpec, TeamRole,
};
use agentscope_harness::tool::TeamTool;

fn setup() -> (Arc<LocalTeamClient>, JsonMap) {
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
    let mut base = JsonMap::new();
    base.insert("team_id".to_string(), serde_json::json!("t1"));
    (client, base)
}

async fn run(tool: &TeamTool, args: JsonMap) -> String {
    tool.call(args, ToolExecutionContext::default())
        .await
        .expect("tool call infallible")
        .text_content()
}

#[tokio::test]
async fn create_list_claim_complete_flow() {
    let (client, a) = setup();
    let tool = TeamTool::new(client);

    let mut create = a.clone();
    create.insert("operation".into(), serde_json::json!("create_task"));
    create.insert("subject".into(), serde_json::json!("build it"));
    let out = run(&tool, create).await;
    assert!(out.contains("TODO"), "{out}");

    let mut list = a.clone();
    list.insert("operation".into(), serde_json::json!("list_tasks"));
    let out = run(&tool, list).await;
    let task_id = out
        .lines()
        .find(|l| l.starts_with("- "))
        .expect("task row")
        .split_whitespace()
        .nth(1)
        .expect("task id")
        .to_string();

    for op in ["claim_task", "complete_task"] {
        let mut m = a.clone();
        m.insert("operation".into(), serde_json::json!(op));
        m.insert("task_id".into(), serde_json::json!(task_id.clone()));
        m.insert("agent".into(), serde_json::json!("dev"));
        let out = run(&tool, m).await;
        assert!(!out.contains("error"), "{op}: {out}");
    }

    let mut list = a;
    list.insert("operation".into(), serde_json::json!("list_tasks"));
    let out = run(&tool, list).await;
    assert!(out.contains("[DONE]"), "{out}");
}

#[tokio::test]
async fn member_role_cannot_spawn_member() {
    let (client, a) = setup();
    let tool = TeamTool::new(client);

    let mut spawn = a.clone();
    spawn.insert("operation".into(), serde_json::json!("spawn_member"));
    spawn.insert("name".into(), serde_json::json!("helper"));
    // MEMBER role → clipped.
    let out = run(&tool, spawn).await;
    assert!(out.contains("may not perform"), "{out}");

    // LEAD role → allowed.
    let mut spawn = a;
    spawn.insert("operation".into(), serde_json::json!("spawn_member"));
    spawn.insert("name".into(), serde_json::json!("helper"));
    spawn.insert("role".into(), serde_json::json!("LEAD"));
    let out = run(&tool, spawn).await;
    assert!(out.contains("spawned"), "{out}");
}

#[tokio::test]
async fn messaging_and_broadcast() {
    let (client, a) = setup();
    let tool = TeamTool::new(client);

    let mut send = a.clone();
    send.insert("operation".into(), serde_json::json!("send_message"));
    send.insert("to".into(), serde_json::json!("dev"));
    send.insert("content".into(), serde_json::json!("hello"));
    let out = run(&tool, send).await;
    assert!(out.contains("delivered to dev"), "{out}");

    let mut bc = a;
    bc.insert("operation".into(), serde_json::json!("broadcast_message"));
    bc.insert("agent".into(), serde_json::json!("lead"));
    bc.insert("content".into(), serde_json::json!("hi all"));
    let out = run(&tool, bc).await;
    assert!(out.contains("1 members"), "{out}"); // only dev besides lead
}

#[tokio::test]
async fn unknown_operation_and_missing_args_error() {
    let (client, a) = setup();
    let tool = TeamTool::new(client);

    let mut m = a.clone();
    m.insert("operation".into(), serde_json::json!("fly_to_moon"));
    let out = run(&tool, m).await;
    assert!(out.contains("not a supported team operation"), "{out}");

    let mut m = a;
    m.insert("operation".into(), serde_json::json!("create_task"));
    let out = run(&tool, m).await;
    assert!(out.contains("`subject`"), "{out}");
}
