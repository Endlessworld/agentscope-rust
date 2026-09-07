//! Repro: git command timeout through SandboxBackedFilesystem::execute.
use std::sync::Arc;
use std::time::Instant;

use agentscope_core::agent::RuntimeContext;
use agentscope_harness::filesystem::{LocalFilesystem, SandboxBackedFilesystem, SandboxFilesystem};

#[tokio::main]
async fn main() {
    let cwd = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let inner: Arc<dyn agentscope_harness::filesystem::Filesystem> = Arc::new(LocalFilesystem::new(cwd.clone()));
    let sandbox = SandboxBackedFilesystem::new(inner, cwd.clone());

    let cmds = vec![
        ("plain git status".to_string(), "git status --short".to_string()),
        (
            "with cd /d prefix (cmd syntax)".to_string(),
            format!("cd /d \"{}\" && git status --short", cwd),
        ),
        ("git --version".to_string(), "git --version".to_string()),
    ];

    for (label, cmd) in cmds {
        let t = Instant::now();
        let rt = RuntimeContext::default();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(40),
            sandbox.execute(&rt, &cmd, 30),
        )
            .await;
        match r {
            Ok(Ok(resp)) => println!(
                "[{label}] {t:?} exit={} out={:.200}",
                resp.exit_code,
                resp.output.replace('\n', " | ")
            ),
            Ok(Err(e)) => println!("[{label}] {t:?} ERROR: {e}"),
            Err(_) => println!("[{label}] {t:?} OUTER TIMEOUT (40s)"),
        }
    }
}
