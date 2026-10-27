//! Integration test for the Docker sandbox backend.
//!
//! Skipped (with a message) when no Docker daemon is reachable, so the suite stays green on
//! machines without Docker. Override the image with `AGENTSCOPE_TEST_DOCKER_IMAGE`.
use std::process::Stdio;

use agentscope_harness::agent::{
    DockerSandboxClient, SandboxClient, SandboxClientOptions, WorkspaceSpec,
};

const IMAGE_ENV: &str = "AGENTSCOPE_TEST_DOCKER_IMAGE";

async fn docker_available() -> bool {
    tokio::process::Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Covers the container lifecycle, `exec`, workspace-spec materialization, the branch-A
/// preserved resume, and the persist/hydrate archive round trip.
#[tokio::test]
async fn docker_sandbox_lifecycle_roundtrip() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon reachable");
        return;
    }

    let image = std::env::var(IMAGE_ENV).unwrap_or_else(|_| "ubuntu:22.04".to_string());
    let client = DockerSandboxClient::new();
    let options = SandboxClientOptions {
        docker_image: Some(image),
        ..Default::default()
    };
    let spec = WorkspaceSpec::new("/workspace")
        .with_directory("src")
        .with_file("README.md", "# hello\n");

    let mut sandbox = client
        .create(&spec, None, Some(&options))
        .await
        .expect("create sandbox");
    sandbox.start().await.expect("start sandbox");
    assert!(sandbox.is_running());

    // Workspace spec materialized inside the container.
    let listing = sandbox
        .exec(None, "cat README.md", Some(30))
        .await
        .expect("read spec file");
    assert_eq!(listing.exit_code, 0, "stderr: {}", listing.stderr);
    assert_eq!(listing.stdout.trim(), "# hello");

    let echo = sandbox.exec(None, "echo hi", Some(30)).await.expect("echo");
    assert_eq!(echo.exit_code, 0);
    assert_eq!(echo.stdout.trim(), "hi");

    let failure = sandbox
        .exec(None, "exit 3", Some(30))
        .await
        .expect("failing command still returns a result");
    assert_eq!(failure.exit_code, 3);

    let archive = sandbox
        .persist_workspace()
        .await
        .expect("persist workspace");
    assert!(!archive.is_empty(), "workspace archive should not be empty");

    // stop + start reuses the live container: branch A keeps the workspace as-is.
    sandbox.stop().await.expect("stop sandbox");
    sandbox.start().await.expect("restart sandbox");
    let preserved = sandbox
        .exec(None, "cat README.md", Some(30))
        .await
        .expect("read preserved file");
    assert_eq!(preserved.stdout.trim(), "# hello");

    // Wipe the workspace, then restore it from the archive.
    sandbox
        .exec(None, "rm -rf /workspace/src /workspace/README.md", Some(30))
        .await
        .expect("wipe workspace");
    sandbox
        .hydrate_workspace(&archive)
        .await
        .expect("hydrate workspace");
    let restored = sandbox
        .exec(None, "cat README.md", Some(30))
        .await
        .expect("read restored file");
    assert_eq!(restored.stdout.trim(), "# hello");

    sandbox.shutdown().await.expect("shutdown sandbox");
}
