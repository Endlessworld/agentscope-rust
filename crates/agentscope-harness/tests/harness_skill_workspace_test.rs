//! Integration tests for the harness skill runtime and workspace plan-mode
//! modules delivered in migration stage 6.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::state::AgentState;
use agentscope_harness::{
    AgentSkill, Filesystem, LocalFilesystem, PlanModeManager, ShellPathPolicy, SkillCatalog,
    SkillFilter, SkillPromptBuilder, SkillRuntime,
};

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("asr_skill_{tag}_{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

#[tokio::test]
async fn plan_mode_manager_enter_write_exit() {
    let dir = temp_dir("plan").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let mgr = PlanModeManager::new(fs.clone(), None);

    let mut state = AgentState::default();
    assert!(!mgr.is_plan_active(&state));

    let path = mgr.enter(&mut state);
    assert_eq!(path, "plans/PLAN.md");
    assert!(mgr.is_plan_active(&state));

    let written = mgr
        .write_plan(
            &RuntimeContext::default(),
            &mut state,
            "# Plan\n1. do things",
        )
        .await
        .unwrap();
    assert_eq!(written, "plans/PLAN.md");
    let data = fs
        .read(&RuntimeContext::default(), "plans/PLAN.md", 0, 0)
        .await
        .unwrap();
    assert!(data.content.contains("do things"));

    mgr.exit(&mut state);
    assert!(!mgr.is_plan_active(&state));
    // Plan file path is retained after exit.
    assert_eq!(mgr.plan_file_path(&state), "plans/PLAN.md");

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[test]
fn plan_mode_manager_custom_plan_dir_normalized() {
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(std::env::temp_dir()));
    let mgr = PlanModeManager::new(fs, Some(" /custom/dir/ "));
    assert_eq!(mgr.plan_dir(), "custom/dir");
    let mut state = AgentState::default();
    assert_eq!(mgr.enter(&mut state), "custom/dir/PLAN.md");
}

#[test]
fn skill_runtime_renders_catalog_with_filter() {
    let rt = SkillRuntime::new();
    let cat = SkillCatalog::of(vec![harness_entry("data"), harness_entry("secret")]);
    let out = rt.render_prompt(&cat, &SkillFilter::only(&["data"]));
    assert!(out.contains("data_workspace"));
    assert!(!out.contains("secret_workspace"));
}

#[test]
fn skill_prompt_code_execution_only_when_files_root_present() {
    let b = SkillPromptBuilder::new();
    let cat = SkillCatalog::of(vec![
        harness_entry("plain"),
        harness_entry_with_root("scripted", "/workspace/skills/scripted"),
    ]);
    let out = b.render_all(&cat);
    assert!(out.contains("<code_execution>"));
    assert!(out.contains("<files-root>/workspace/skills/scripted</files-root>"));
}

#[test]
fn shell_path_policy_resolves_sandbox_roots() {
    let p = ShellPathPolicy::sandbox();
    let roots = p
        .resolve(
            "data",
            Some(&agentscope_harness::StageResult::WorkspaceNative),
        )
        .unwrap();
    assert_eq!(roots, "/workspace/skills/data");
}

fn harness_entry(name: &str) -> agentscope_harness::HarnessSkillEntry {
    agentscope_harness::HarnessSkillEntry::new(
        AgentSkill::builder()
            .name(name)
            .source("workspace")
            .description(format!("desc {name}"))
            .build(),
    )
}

fn harness_entry_with_root(name: &str, root: &str) -> agentscope_harness::HarnessSkillEntry {
    harness_entry(name).with_files_root(root)
}
