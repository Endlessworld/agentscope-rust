//! Integration tests for the background subagent task repository.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_harness::{
    Filesystem, LocalFilesystem, TaskRepository, TaskRunSpec, TaskStatus, WorkspaceTaskRepository,
};

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_task_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

fn new_repo(dir: &std::path::Path) -> WorkspaceTaskRepository {
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
    WorkspaceTaskRepository::new(fs, "parent")
}

#[tokio::test(flavor = "multi_thread")]
async fn local_spec_completes_and_persists() {
    let dir = temp_dir("complete").await;
    let repo = new_repo(&dir);
    let bg = repo
        .put_task(
            &ctx(),
            "t1",
            "sub1",
            "sess1",
            TaskRunSpec::local(|| Ok("result-1".to_string())),
        )
        .await;
    assert!(bg.wait_for_completion(5000));
    assert_eq!(bg.get_status_string(), "Completed");
    assert_eq!(bg.get_result().as_deref(), Some("result-1"));

    // Workspace persisted: re-opening the repo sees the record.
    let repo2 = new_repo(&dir);
    let got = repo2.get_task(&ctx(), "sess1", "t1").await.unwrap();
    assert_eq!(got.get_task_status(), TaskStatus::Completed);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn local_spec_failure_yields_failed() {
    let dir = temp_dir("fail").await;
    let repo = new_repo(&dir);
    let bg = repo
        .put_task(
            &ctx(),
            "t1",
            "sub1",
            "sess1",
            TaskRunSpec::local(|| Err(std::io::Error::other("boom").into())),
        )
        .await;
    assert!(bg.wait_for_completion(5000));
    assert_eq!(bg.get_task_status(), TaskStatus::Failed);
    assert!(bg.get_error().is_some());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_task_marks_cancelled() {
    let dir = temp_dir("cancel").await;
    let repo = new_repo(&dir);
    // A local task that blocks until cancelled.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let bg = repo
        .put_task(
            &ctx(),
            "t1",
            "sub1",
            "sess1",
            TaskRunSpec::local(move || {
                let _ = rx.lock().unwrap().recv();
                Ok("late".to_string())
            }),
        )
        .await;
    // Task is still running before cancelling.
    assert!(!bg.is_completed());
    let cancelled = repo.cancel_task(&ctx(), "sess1", "t1").await;
    assert!(cancelled);
    assert_eq!(bg.get_task_status(), TaskStatus::Cancelled);
    let _ = tx.send(());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_tasks_filters_by_status() {
    let dir = temp_dir("list").await;
    let repo = new_repo(&dir);
    repo.put_task(
        &ctx(),
        "t-ok",
        "sub1",
        "sess",
        TaskRunSpec::local(|| Ok("a".to_string())),
    )
    .await
    .wait_for_completion(5000);
    repo.put_task(
        &ctx(),
        "t-fail",
        "sub1",
        "sess",
        TaskRunSpec::local(|| Err(std::io::Error::other("nope").into())),
    )
    .await
    .wait_for_completion(5000);

    let all = repo.list_tasks(&ctx(), "sess", None).await;
    assert_eq!(all.len(), 2);
    let completed = repo
        .list_tasks(&ctx(), "sess", Some(TaskStatus::Completed))
        .await;
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].task_id(), "t-ok");
    let failed = repo
        .list_tasks(&ctx(), "sess", Some(TaskStatus::Failed))
        .await;
    assert_eq!(failed.len(), 1);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_deliveries_and_mark_delivered() {
    let dir = temp_dir("delivery").await;
    let repo = new_repo(&dir);
    repo.put_task(
        &ctx(),
        "t1",
        "sub1",
        "sess",
        TaskRunSpec::local(|| Ok("done".to_string())),
    )
    .await
    .wait_for_completion(5000);

    assert!(!repo.is_delivered(&ctx(), "sess", "t1").await);
    let pending = repo.find_pending_deliveries(&ctx(), "sess").await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_id, "t1");
    assert_eq!(pending[0].result.as_deref(), Some("done"));

    repo.mark_delivered(&ctx(), "sess", "t1").await;
    assert!(repo.is_delivered(&ctx(), "sess", "t1").await);
    assert!(repo
        .find_pending_deliveries(&ctx(), "sess")
        .await
        .is_empty());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// An adopted run (the shape `agent_spawn` / `agent_send` submit) must stop when the task is
/// cancelled: `cancel_task` aborts the wrapper task, which has to propagate into the adopted
/// execution instead of leaving it running detached.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_task_aborts_an_adopted_run() {
    struct StoppedOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for StoppedOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let dir = temp_dir("cancel-adopted").await;
    let repo = new_repo(&dir);
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let inner = {
        // The guard is built *outside* the async block and moved in, so it also fires when
        // the task is aborted before it is ever polled.
        let guard = StoppedOnDrop(Arc::clone(&stopped));
        tokio::spawn(async move {
            let _guard = guard;
            futures::future::pending::<agentscope_core::error::Result<String>>().await
        })
    };
    let bg = repo
        .put_task(
            &ctx(),
            "t1",
            "sub1",
            "sess",
            TaskRunSpec::Adopted { future: inner },
        )
        .await;
    assert_eq!(bg.get_task_status(), TaskStatus::Running);

    assert!(repo.cancel_task(&ctx(), "sess", "t1").await);
    // Give the abort a moment to unwind the adopted task.
    for _ in 0..50 {
        if stopped.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        stopped.load(std::sync::atomic::Ordering::SeqCst),
        "the cancelled adopted run kept executing"
    );
    assert_eq!(bg.get_task_status(), TaskStatus::Cancelled);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}
