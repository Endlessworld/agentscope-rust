//! Docker sandbox backend, mirroring
//! `io.agentscope.harness.agent.sandbox.impl.docker`.
//!
//! Commands run through the `docker` CLI, like the Java reference: comparable line by
//! line and dependency-free (see `history/adr.md`, B1).

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use super::snapshot::SandboxSnapshot;
use super::state::{DockerSandboxState, SandboxState};
use super::workspace_spec::{ExecResult, WorkspaceSpec};
use super::{Sandbox, SandboxClient, SandboxClientOptions, SandboxSnapshotSpec};

const DEFAULT_IMAGE: &str = "ubuntu:22.04";
const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";
/// Per-stream output cap, matching the Java backend.
const OUTPUT_TRUNCATE_BYTES: usize = 512 * 1024;
/// Cap for CLI plumbing output (container ids, error text).
const CLI_OUTPUT_BYTES: usize = 64 * 1024;
const CONTAINER_START_TIMEOUT_SECONDS: u64 = 60;
const CONTAINER_STOP_TIMEOUT_SECONDS: u64 = 30;
const TAR_TIMEOUT_SECONDS: u64 = 120;
const PROBE_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_EXEC_TIMEOUT_SECONDS: u64 = 120;
/// Keeps the container alive; traps SIGTERM so `docker stop` is prompt (Java parity).
const KEEP_ALIVE_COMMAND: &str = "trap 'exit 0' TERM; while :; do sleep 3600 & wait $!; done";

/// Factory for Docker-backed sandboxes.
#[derive(Debug, Default, Clone, Copy)]
pub struct DockerSandboxClient;

impl DockerSandboxClient {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SandboxClient for DockerSandboxClient {
    async fn create(
        &self,
        workspace_spec: &WorkspaceSpec,
        snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>> {
        let options = options.cloned().unwrap_or_default();
        let session_id = uuid::Uuid::new_v4().to_string();
        let state = DockerSandboxState {
            session_id: session_id.clone(),
            container_id: None,
            container_name: None,
            workspace_root: options
                .workspace_root
                .clone()
                .unwrap_or_else(|| DEFAULT_WORKSPACE_ROOT.to_string()),
            image: options
                .docker_image
                .clone()
                .unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
            container_owned: true,
            workspace_root_ready: false,
            memory_size_bytes: options.resource_limits.memory_limit,
            cpu_count: options.resource_limits.cpu_limit.map(|cpus| cpus as u64),
            exposed_ports: options.exposed_ports.clone(),
            network: options.network_mode.clone(),
            additional_run_args: options.additional_run_args.clone(),
            volumes: options.volumes.clone(),
            workspace_projection_hash: None,
            workspace_spec: Some(workspace_spec.clone()),
            metadata: std::collections::HashMap::new(),
        };
        debug!(
            "[sandbox-docker] Creating new sandbox: id={}, image={}",
            state.session_id, state.image
        );
        let sandbox = DockerSandbox::new(SandboxState::Docker(state));
        let sandbox = match snapshot_spec {
            Some(spec) => sandbox.with_snapshot(spec.build(&session_id)?),
            None => sandbox,
        };
        Ok(Box::new(sandbox))
    }

    async fn resume(&self, state: &SandboxState) -> Result<Box<dyn Sandbox>> {
        match state {
            SandboxState::Docker(docker) => {
                debug!(
                    "[sandbox-docker] Resuming sandbox: id={}, containerId={:?}",
                    docker.session_id, docker.container_id
                );
                Ok(Box::new(DockerSandbox::new(state.clone())))
            }
            other => Err(Error::Other(format!(
                "Expected DockerSandboxState but got: {other:?}"
            ))),
        }
    }

    /// No-op: cleanup is handled by `DockerSandbox::shutdown()` (Java parity).
    async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
        Ok(())
    }

    fn serialize_state(&self, state: &SandboxState) -> Result<String> {
        serde_json::to_string(state).map_err(Error::from)
    }

    fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
        serde_json::from_str(json).map_err(Error::from)
    }
}

/// Docker-backed sandbox: runs shell commands inside a container through `docker exec`.
#[derive(Debug)]
pub struct DockerSandbox {
    state: SandboxState,
    running: AtomicBool,
    /// Workspace snapshot bound at create time (Java re-injects it from the builder on
    /// resume; the Rust `resume(state)` path has no spec, so it stays `None`).
    snapshot: Option<Arc<dyn SandboxSnapshot>>,
}

impl DockerSandbox {
    pub fn new(state: SandboxState) -> Self {
        Self {
            state,
            running: AtomicBool::new(false),
            snapshot: None,
        }
    }

    /// Binds a workspace snapshot used by `start`/`stop` for persistence.
    pub fn with_snapshot(mut self, snapshot: Arc<dyn SandboxSnapshot>) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    fn docker(&self) -> &DockerSandboxState {
        match &self.state {
            SandboxState::Docker(state) => state,
            other => unreachable!("DockerSandbox always holds a Docker state, found {other:?}"),
        }
    }

    fn docker_mut(&mut self) -> &mut DockerSandboxState {
        match &mut self.state {
            SandboxState::Docker(state) => state,
            other => unreachable!("DockerSandbox always holds a Docker state, found {other:?}"),
        }
    }

    /// Returns the live container id, or an error when the container is unknown.
    fn container_id(&self) -> Result<String> {
        self.docker()
            .container_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::Other("Docker container is unavailable".to_string()))
    }

    /// Creates the workspace root directory inside the container.
    async fn setup_workspace(&self) -> Result<()> {
        let container_id = self.container_id()?;
        let root = self.docker().workspace_root.clone();
        run_docker_cli(30, &["exec", &container_id, "mkdir", "-p", &root]).await
    }

    /// Materializes the spec's directories and files inside the container workspace.
    ///
    /// The Java backend writes these with host `Files` calls, which cannot reach a container
    /// path; here they are materialized through `docker exec`. Materialization is idempotent,
    /// so it serves both the ephemeral-only (branch A) and the full (branch D) modes.
    async fn apply_workspace_spec(&self, spec: &WorkspaceSpec) -> Result<()> {
        let root = self.docker().workspace_root.clone();
        let mut dirs: Vec<String> = spec
            .directories
            .iter()
            .filter(|dir| !dir.trim().is_empty())
            .map(|dir| workspace_path(&root, dir))
            .collect();
        if let Some(cwd) = spec
            .working_directory
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            dirs.push(workspace_path(&root, cwd));
        }
        if !dirs.is_empty() {
            let mut command = String::from("mkdir -p");
            for dir in &dirs {
                command.push(' ');
                command.push_str(&shell_quote(dir));
            }
            self.exec_raw(&command, PROBE_TIMEOUT_SECONDS).await?;
        }

        for (path, content) in &spec.files {
            let target = workspace_path(&root, path);
            let parent = target
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_else(|| root.clone());
            let encoded = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                content.as_bytes(),
            );
            let command = format!(
                "mkdir -p {} && printf %s {} | base64 -d > {}",
                shell_quote(&parent),
                shell_quote(&encoded),
                shell_quote(&target)
            );
            self.exec_raw(&command, PROBE_TIMEOUT_SECONDS).await?;
        }

        if !spec.copy_in.is_empty() {
            warn!(
                "[sandbox-docker] skipping {} copy_in entries: host->container transfer is not implemented",
                spec.copy_in.len()
            );
        }
        Ok(())
    }

    /// Runs a command inside the workspace, failing the call when it exits non-zero.
    async fn exec_raw(&self, command: &str, timeout_seconds: u64) -> Result<()> {
        let result = self.exec(None, command, Some(timeout_seconds)).await?;
        if !result.is_success() {
            return Err(Error::Other(format!(
                "command failed (exit={}): {}",
                result.exit_code,
                result.stderr.trim()
            )));
        }
        Ok(())
    }

    /// Probes whether the workspace root still exists (`test -d`), like the Java backend.
    async fn probe_workspace_root(&self) -> bool {
        let root = self.docker().workspace_root.clone();
        let command = format!("test -d {}", shell_quote(&root));
        match self.exec(None, &command, Some(PROBE_TIMEOUT_SECONDS)).await {
            Ok(result) => result.is_success(),
            Err(e) => {
                warn!("[sandbox-docker] Probe for workspace root failed, assuming lost: {e}");
                false
            }
        }
    }

    /// Ensures the backing container is running (Java parity): a running container is reused,
    /// a stopped one is restarted, and a missing/unknown one is replaced by a new container
    /// that forces a full workspace re-initialization.
    async fn ensure_container_running(&mut self) -> Result<()> {
        let container_id = self
            .docker()
            .container_id
            .clone()
            .filter(|id| !id.is_empty());
        if let Some(container_id) = container_id {
            match inspect_container_state(&container_id).await {
                ContainerState::Running => {
                    debug!("[sandbox-docker] Container already running: {container_id}");
                    return Ok(());
                }
                ContainerState::Stopped => {
                    debug!("[sandbox-docker] Restarting stopped container: {container_id}");
                    return run_docker_cli(
                        CONTAINER_START_TIMEOUT_SECONDS,
                        &["start", &container_id],
                    )
                    .await;
                }
                ContainerState::Unknown => {
                    warn!(
                        "[sandbox-docker] Container {container_id} not found, creating a new one"
                    );
                    self.docker_mut().workspace_root_ready = false;
                }
            }
        }
        self.create_and_start_container().await
    }

    /// Runs a new detached container and records its id.
    async fn create_and_start_container(&mut self) -> Result<()> {
        let container_name = format!("agentscope-sandbox-{}", self.docker().session_id);
        let args = build_docker_run_command(self.docker(), &container_name);
        debug!(
            "[sandbox-docker] Creating container: image={}, name={}",
            self.docker().image,
            container_name
        );

        let mut command = docker_command();
        command.args(&args);
        let outcome =
            run_command(command, CONTAINER_START_TIMEOUT_SECONDS, CLI_OUTPUT_BYTES).await?;
        let stdout = String::from_utf8_lossy(&outcome.stdout).trim().to_string();
        let stderr = outcome.stderr.trim();
        match outcome.exit_code {
            None => {
                return Err(Error::Other(format!(
                    "docker run timed out for image: {}",
                    self.docker().image
                )))
            }
            Some(0) => {}
            Some(code) => {
                return Err(Error::Other(format!(
                    "docker run failed (exit={code}): {stderr}"
                )))
            }
        }
        if stdout.is_empty() {
            return Err(Error::Other(format!(
                "docker run produced no container ID. stderr: {stderr}"
            )));
        }

        let docker_state = self.docker_mut();
        docker_state.container_id = Some(stdout.clone());
        docker_state.container_name = Some(container_name);
        debug!("[sandbox-docker] Container started: id={stdout}");
        Ok(())
    }

    /// Attempts to restore the workspace from the bound snapshot. Returns `true` when an
    /// archive was hydrated; `false` when no snapshot is bound, persistence is disabled, the
    /// snapshot is not restorable, or restore failed (degrading to a fresh workspace).
    async fn try_restore_snapshot(&self) -> Result<bool> {
        let Some(snapshot) = self.snapshot.clone() else {
            return Ok(false);
        };
        if !snapshot.is_persistence_enabled() {
            return Ok(false);
        }
        match snapshot.is_restorable().await {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => {
                warn!("[sandbox-docker] Snapshot restorability check failed: {e}");
                return Ok(false);
            }
        }
        match snapshot.restore().await {
            Ok(bytes) => {
                self.setup_workspace().await?;
                self.hydrate_workspace(&bytes).await?;
                Ok(true)
            }
            Err(e) => {
                warn!("[sandbox-docker] Snapshot restore failed, falling back to fresh: {e}");
                Ok(false)
            }
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for DockerSandbox {
    async fn start(&mut self) -> Result<()> {
        self.ensure_container_running().await?;
        let spec = self.docker().workspace_spec.clone().unwrap_or_default();

        let workspace_ready = self.docker().workspace_root_ready;
        let root_present = workspace_ready && self.probe_workspace_root().await;

        if root_present {
            // Branch A: workspace preserved — re-materialize the spec entries.
            debug!("[sandbox-docker] Branch A: workspace preserved");
            self.apply_workspace_spec(&spec).await?;
        } else if self.try_restore_snapshot().await? {
            // Branch B/C: workspace lost (or fresh container) but a snapshot is restorable —
            // hydrate the archive, then re-apply the spec (materialization is idempotent, so
            // non-ephemeral entries are refreshed and ephemeral entries are always re-applied).
            debug!("[sandbox-docker] Branch B/C: restored from snapshot");
            self.apply_workspace_spec(&spec).await?;
        } else {
            // Branch D: fresh workspace initialization.
            debug!("[sandbox-docker] Branch D: fresh workspace initialization");
            self.setup_workspace().await?;
            self.apply_workspace_spec(&spec).await?;
        }

        self.docker_mut().workspace_root_ready = true;
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        let snapshot = self.snapshot.clone();
        if let Some(snapshot) = snapshot {
            // Noop snapshots report persistence disabled; skip the expensive archive step.
            if snapshot.is_persistence_enabled() {
                match self.persist_workspace().await {
                    Ok(bytes) => match snapshot.persist(&bytes).await {
                        Ok(()) => debug!(
                            "[sandbox-docker] Persisted workspace snapshot ({} bytes)",
                            bytes.len()
                        ),
                        Err(e) => warn!("[sandbox-docker] Snapshot persist failed: {e}"),
                    },
                    Err(e) => warn!("[sandbox-docker] Workspace archive failed: {e}"),
                }
            }
        }
        self.docker_mut().workspace_root_ready = true;
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        let (container_id, owned) = {
            let state = self.docker();
            (
                state.container_id.clone().filter(|id| !id.is_empty()),
                state.container_owned,
            )
        };
        let Some(container_id) = container_id else {
            return Ok(());
        };
        if !owned {
            debug!("[sandbox-docker] Skipping shutdown: container is user-managed: {container_id}");
            return Ok(());
        }
        let stop_arg = format!("--time={CONTAINER_STOP_TIMEOUT_SECONDS}");
        if let Err(e) = run_docker_cli(
            CONTAINER_STOP_TIMEOUT_SECONDS * 2,
            &["stop", &stop_arg, &container_id],
        )
        .await
        {
            warn!("[sandbox-docker] Failed to stop container {container_id}: {e}");
        }
        if let Err(e) = run_docker_cli(30, &["rm", "--force", &container_id]).await {
            warn!("[sandbox-docker] Failed to remove container {container_id}: {e}");
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn state(&self) -> &SandboxState {
        &self.state
    }

    async fn exec(
        &self,
        _runtime_context: Option<&RuntimeContext>,
        command: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<ExecResult> {
        let container_id = self.container_id()?;
        let root = self.docker().workspace_root.clone();
        let timeout = timeout_seconds.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECONDS);
        let mut cli = docker_command();
        cli.args(["exec", "-w", &root, &container_id, "sh", "-c", command]);
        let outcome = run_command(cli, timeout, OUTPUT_TRUNCATE_BYTES).await?;
        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            stderr: outcome.stderr,
            // A timed-out command reports `-1`, the crate-wide convention (`ExecuteResponse`).
            exit_code: outcome.exit_code.unwrap_or(-1),
            truncated: outcome.truncated,
        })
    }

    async fn persist_workspace(&self) -> Result<Vec<u8>> {
        let container_id = self.container_id()?;
        let root = self.docker().workspace_root.clone();
        let mut cli = docker_command();
        cli.args(["exec", &container_id, "tar", "-cf", "-", "-C", &root, "."]);
        let outcome = run_command(cli, TAR_TIMEOUT_SECONDS, usize::MAX).await?;
        match outcome.exit_code {
            Some(0) => Ok(outcome.stdout),
            Some(code) => Err(Error::Other(format!(
                "docker tar command failed (exit={code}): {}",
                outcome.stderr.trim()
            ))),
            None => Err(Error::Other(format!(
                "docker tar command timed out for container: {container_id}"
            ))),
        }
    }

    async fn hydrate_workspace(&self, archive: &[u8]) -> Result<()> {
        let container_id = self.container_id()?;
        let root = self.docker().workspace_root.clone();
        run_docker_cli(30, &["exec", &container_id, "mkdir", "-p", &root]).await?;

        let mut cli = docker_command();
        cli.args(["exec", "-i", &container_id, "tar", "-xf", "-", "-C", &root])
            .stdin(Stdio::piped());
        let mut child = cli.spawn().map_err(Error::Io)?;
        let pid = child.id().unwrap_or(0);
        let mut stdout_stream = child.stdout.take();
        let mut stderr_stream = child.stderr.take();
        // Drain both pipes while the archive is written, otherwise a full pipe stalls `tar`.
        let drain = tokio::spawn(async move {
            let mut stdout = Vec::new();
            if let Some(stream) = stdout_stream.as_mut() {
                let _ = stream.read_to_end(&mut stdout).await;
            }
            let mut stderr = Vec::new();
            if let Some(stream) = stderr_stream.as_mut() {
                let _ = stream.read_to_end(&mut stderr).await;
            }
            stderr
        });

        let timed_out = format!("docker tar extract timed out for container: {container_id}");
        if let Some(mut stdin) = child.stdin.take() {
            let write = async {
                let _ = stdin.write_all(archive).await;
                let _ = stdin.shutdown().await;
            };
            if tokio::time::timeout(Duration::from_secs(TAR_TIMEOUT_SECONDS), write)
                .await
                .is_err()
            {
                crate::filesystem::sandbox::kill_process_tree(pid);
                let _ = child.wait().await;
                return Err(Error::Other(timed_out));
            }
        }

        let status = match tokio::time::timeout(
            Duration::from_secs(TAR_TIMEOUT_SECONDS),
            child.wait(),
        )
        .await
        {
            Ok(status) => status.map_err(Error::Io)?,
            Err(_) => {
                crate::filesystem::sandbox::kill_process_tree(pid);
                let _ = child.wait().await;
                return Err(Error::Other(timed_out));
            }
        };
        let stderr = drain.await.unwrap_or_default();
        if !status.success() {
            return Err(Error::Other(format!(
                "docker tar extract failed (exit={:?}): {}",
                status.code(),
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        Ok(())
    }
}

/// Builds the `docker run` argv for a new container (mirrors Java's `buildDockerRunCommand`).
fn build_docker_run_command(state: &DockerSandboxState, container_name: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container_name.to_string(),
    ];

    if let Some(spec) = &state.workspace_spec {
        // Sorted so the argv stays deterministic (the spec uses a HashMap).
        let mut env: Vec<(&String, &String)> = spec.environment.iter().collect();
        env.sort();
        for (key, value) in env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
    }
    if let Some(memory) = state.memory_size_bytes {
        args.push(format!("--memory={memory}"));
    }
    if let Some(cpus) = state.cpu_count {
        args.push(format!("--cpus={cpus}"));
    }
    for port in &state.exposed_ports {
        args.push("-p".into());
        args.push(format!("{port}:{port}"));
    }
    let network = state
        .network
        .as_deref()
        .map(str::trim)
        .filter(|network| !network.is_empty())
        .unwrap_or("none");
    args.push(format!("--network={network}"));
    for volume in &state.volumes {
        let mode = if volume.read_only { "ro" } else { "rw" };
        args.push("-v".into());
        args.push(format!(
            "{}:{}:{mode}",
            volume.host_path, volume.container_path
        ));
    }
    args.extend(state.additional_run_args.iter().cloned());

    args.push(state.image.clone());
    args.push("sh".into());
    args.push("-c".into());
    args.push(KEEP_ALIVE_COMMAND.into());
    args
}

/// Creates a `docker` CLI command with piped output and no inherited stdin.
fn docker_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("docker");
    // Never inherit the parent's stdin: under ACP stdio it is a pipe the client keeps open,
    // which makes the CLI wait for input instead of exiting.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Result of a `docker` CLI round trip. `exit_code == None` means the command timed out.
struct CliOutcome {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
    truncated: bool,
}

/// Reads a stream to EOF, keeping at most `cap` bytes but draining the rest so the child
/// process never blocks on a full pipe.
async fn read_capped<R>(mut stream: R, cap: usize) -> (Vec<u8>, bool)
where
    R: AsyncReadExt + Unpin,
{
    let mut collected = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if collected.len() < cap {
                    let room = cap - collected.len();
                    collected.extend_from_slice(&buf[..read.min(room)]);
                }
            }
        }
    }
    let truncated = collected.len() >= cap;
    (collected, truncated)
}

/// Runs a `docker` CLI command, draining both pipes concurrently and killing the process
/// tree on timeout (an orphaned child would otherwise hold the pipes open forever).
async fn run_command(
    mut command: tokio::process::Command,
    timeout_seconds: u64,
    cap: usize,
) -> Result<CliOutcome> {
    let mut child = command.spawn().map_err(Error::Io)?;
    let pid = child.id().unwrap_or(0);
    let stdout_stream = child.stdout.take();
    let stderr_stream = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        match stdout_stream {
            Some(stream) => read_capped(stream, cap).await,
            None => (Vec::new(), false),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr_stream {
            Some(stream) => {
                let (bytes, truncated) = read_capped(stream, cap).await;
                (String::from_utf8_lossy(&bytes).into_owned(), truncated)
            }
            None => (String::new(), false),
        }
    });

    let timeout = Duration::from_secs(timeout_seconds.max(1));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.map_err(Error::Io)?,
        Err(_) => {
            crate::filesystem::sandbox::kill_process_tree(pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            })
            .await;
            return Ok(CliOutcome {
                exit_code: None,
                stdout: Vec::new(),
                stderr: "Command timed out".to_string(),
                truncated: true,
            });
        }
    };

    let (stdout, stdout_truncated) = stdout_task.await.unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_task.await.unwrap_or_default();
    Ok(CliOutcome {
        exit_code: status.code(),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// Runs a `docker` CLI command that must succeed, discarding its output.
async fn run_docker_cli(timeout_seconds: u64, args: &[&str]) -> Result<()> {
    let mut command = docker_command();
    command.args(args);
    let subcommand = args.first().copied().unwrap_or_default();
    let outcome = run_command(command, timeout_seconds, CLI_OUTPUT_BYTES).await?;
    match outcome.exit_code {
        Some(0) => Ok(()),
        Some(code) => Err(Error::Other(format!(
            "docker {subcommand} failed (exit={code}): {}",
            outcome.stderr.trim()
        ))),
        None => Err(Error::Other(format!("docker {subcommand} timed out"))),
    }
}

/// Container state as reported by `docker inspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerState {
    Running,
    Stopped,
    Unknown,
}

/// Inspects a container through `docker inspect -f {{.State.Running}}`.
async fn inspect_container_state(container_id: &str) -> ContainerState {
    let mut command = docker_command();
    command.args(["inspect", "-f", "{{.State.Running}}", container_id]);
    let outcome = match run_command(command, 10, 1024).await {
        Ok(outcome) => outcome,
        Err(e) => {
            debug!("[sandbox-docker] Failed to inspect container {container_id}: {e}");
            return ContainerState::Unknown;
        }
    };
    match outcome.exit_code {
        Some(0) => match String::from_utf8_lossy(&outcome.stdout).trim() {
            "true" => ContainerState::Running,
            _ => ContainerState::Stopped,
        },
        _ => ContainerState::Unknown,
    }
}

/// Single-quotes a value for `sh -c`, escaping embedded single quotes.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Joins a container-absolute workspace root with a workspace-relative path.
fn workspace_path(root: &str, relative: &str) -> String {
    let relative = relative.replace('\\', "/");
    format!(
        "{}/{}",
        root.trim_end_matches('/'),
        relative.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sandbox::{LocalSandboxState, VolumeMount};

    fn sample_state() -> DockerSandboxState {
        DockerSandboxState {
            session_id: "session-1".to_string(),
            container_id: None,
            container_name: None,
            workspace_root: DEFAULT_WORKSPACE_ROOT.to_string(),
            image: DEFAULT_IMAGE.to_string(),
            container_owned: true,
            workspace_root_ready: false,
            memory_size_bytes: None,
            cpu_count: None,
            exposed_ports: Vec::new(),
            network: None,
            additional_run_args: Vec::new(),
            volumes: Vec::new(),
            workspace_projection_hash: None,
            workspace_spec: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn client_creates_state_from_options() {
        let client = DockerSandboxClient::new();
        let mut options = SandboxClientOptions {
            docker_image: Some("python:3.12-slim".to_string()),
            workspace_root: Some("/home/agent/workspace".to_string()),
            network_mode: Some("bridge".to_string()),
            exposed_ports: vec![8080],
            additional_run_args: vec!["--init".to_string()],
            ..Default::default()
        };
        options.resource_limits.memory_limit = Some(512 * 1024 * 1024);
        options.resource_limits.cpu_limit = Some(2.0);

        let spec = WorkspaceSpec::new("/ignored").with_directory("src");
        let sandbox = client.create(&spec, None, Some(&options)).await.unwrap();
        match sandbox.state() {
            SandboxState::Docker(state) => {
                assert_eq!(state.image, "python:3.12-slim");
                assert_eq!(state.workspace_root, "/home/agent/workspace");
                assert_eq!(state.network.as_deref(), Some("bridge"));
                assert_eq!(state.exposed_ports, vec![8080]);
                assert_eq!(state.memory_size_bytes, Some(512 * 1024 * 1024));
                assert_eq!(state.cpu_count, Some(2));
                assert!(state.container_owned);
                assert!(!state.workspace_root_ready);
                assert_eq!(
                    state.workspace_spec.as_ref().unwrap().directories,
                    vec!["src"]
                );
            }
            other => panic!("expected Docker state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_defaults_image_and_workspace_root() {
        let client = DockerSandboxClient::new();
        let sandbox = client
            .create(&WorkspaceSpec::default(), None, None)
            .await
            .unwrap();
        match sandbox.state() {
            SandboxState::Docker(state) => {
                assert_eq!(state.image, DEFAULT_IMAGE);
                assert_eq!(state.workspace_root, DEFAULT_WORKSPACE_ROOT);
                assert!(state.container_id.is_none());
            }
            other => panic!("expected Docker state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_resume_rejects_other_backends() {
        let client = DockerSandboxClient::new();
        let state = SandboxState::Local(LocalSandboxState {
            session_id: "s".to_string(),
            temp_dir: "/tmp/s".to_string(),
            workspace_root_ready: false,
            workspace_projection_hash: None,
            metadata: std::collections::HashMap::new(),
        });
        assert!(client.resume(&state).await.is_err());
        assert!(client
            .resume(&SandboxState::Docker(sample_state()))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn client_state_round_trips_through_json() {
        let client = DockerSandboxClient::new();
        let state = SandboxState::Docker(DockerSandboxState {
            container_id: Some("abc123".to_string()),
            ..sample_state()
        });
        let json = client.serialize_state(&state).unwrap();
        let restored = client.deserialize_state(&json).unwrap();
        assert_eq!(restored.session_id(), "session-1");
        match restored {
            SandboxState::Docker(restored) => {
                assert_eq!(restored.container_id.as_deref(), Some("abc123"));
                assert!(restored.container_owned);
            }
            other => panic!("expected Docker state, got {other:?}"),
        }
    }

    #[test]
    fn run_command_defaults_network_and_keepalive() {
        let args = build_docker_run_command(&sample_state(), "agentscope-sandbox-session-1");
        assert_eq!(
            &args[..4],
            ["run", "-d", "--name", "agentscope-sandbox-session-1"]
        );
        assert!(args.contains(&"--network=none".to_string()));
        assert_eq!(args[args.len() - 4], DEFAULT_IMAGE);
        assert_eq!(&args[args.len() - 3..], ["sh", "-c", KEEP_ALIVE_COMMAND]);
    }

    #[test]
    fn run_command_includes_limits_ports_volumes_and_env() {
        let mut state = sample_state();
        state.image = "alpine:3".to_string();
        state.network = Some(" bridge ".to_string());
        state.memory_size_bytes = Some(1024);
        state.cpu_count = Some(2);
        state.exposed_ports = vec![8080, 9090];
        state.additional_run_args = vec!["--init".to_string()];
        state.volumes = vec![VolumeMount {
            host_path: "/host/data".to_string(),
            container_path: "/workspace/data".to_string(),
            read_only: true,
        }];
        state.workspace_spec = Some(
            WorkspaceSpec::new("/workspace")
                .with_env("B", "2")
                .with_env("A", "1"),
        );

        let args = build_docker_run_command(&state, "name");
        assert_eq!(args.iter().filter(|arg| arg.as_str() == "-e").count(), 2);
        assert!(args.contains(&"--memory=1024".to_string()));
        assert!(args.contains(&"--cpus=2".to_string()));
        assert!(args.contains(&"--network=bridge".to_string()));
        assert!(args.contains(&"8080:8080".to_string()));
        assert!(args.contains(&"9090:9090".to_string()));
        assert!(args.contains(&"/host/data:/workspace/data:ro".to_string()));
        assert!(args.contains(&"--init".to_string()));
        let image_index = args.iter().position(|arg| arg == "alpine:3").unwrap();
        assert!(args.iter().position(|arg| arg == "A=1").unwrap() < image_index);
        assert!(args.iter().position(|arg| arg == "--init").unwrap() < image_index);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/workspace/it's"), "'/workspace/it'\\''s'");
    }

    #[test]
    fn workspace_path_joins_relative_paths() {
        assert_eq!(
            workspace_path("/workspace", "src/main.rs"),
            "/workspace/src/main.rs"
        );
        assert_eq!(workspace_path("/workspace/", "/src"), "/workspace/src");
        assert_eq!(workspace_path("/", "a"), "/a");
        assert_eq!(
            workspace_path("/workspace", "src\\main.rs"),
            "/workspace/src/main.rs"
        );
    }

    #[tokio::test]
    async fn commands_require_a_container() {
        let sandbox = DockerSandbox::new(SandboxState::Docker(sample_state()));
        assert!(sandbox.exec(None, "echo hi", Some(1)).await.is_err());
        assert!(sandbox.persist_workspace().await.is_err());
        assert!(sandbox.hydrate_workspace(b"not-a-tar").await.is_err());
    }

    #[tokio::test]
    async fn stop_marks_workspace_ready_and_shutdown_is_noop_without_container() {
        let mut sandbox = DockerSandbox::new(SandboxState::Docker(sample_state()));
        sandbox.stop().await.unwrap();
        assert!(sandbox.state().is_workspace_root_ready());
        assert!(!sandbox.is_running());
        sandbox.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_binds_snapshot_spec_and_restore_degrades_without_archive() {
        use super::super::snapshot::{LocalSnapshotSpec, NoopSandboxSnapshot};
        use std::sync::Arc;

        // No snapshot bound.
        let bare = DockerSandbox::new(SandboxState::Docker(sample_state()));
        assert!(!bare.try_restore_snapshot().await.unwrap());

        // Noop snapshot: persistence disabled → no restore attempt.
        let noop = DockerSandbox::new(SandboxState::Docker(sample_state()))
            .with_snapshot(Arc::new(NoopSandboxSnapshot));
        assert!(!noop.try_restore_snapshot().await.unwrap());

        // Local snapshot with no archive on disk → not restorable.
        let dir = std::env::temp_dir().join(format!("asr_docker_snap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let spec = LocalSnapshotSpec::new(dir.to_string_lossy().to_string());
        let local = DockerSandbox::new(SandboxState::Docker(sample_state()))
            .with_snapshot(spec.build("sess").unwrap());
        assert!(!local.try_restore_snapshot().await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_skips_archive_for_noop_snapshot_without_container() {
        use super::super::snapshot::NoopSandboxSnapshot;
        use std::sync::Arc;
        // Persistence disabled → stop must not attempt `persist_workspace` (which would fail
        // without a container), so it completes cleanly.
        let mut sandbox = DockerSandbox::new(SandboxState::Docker(sample_state()))
            .with_snapshot(Arc::new(NoopSandboxSnapshot));
        sandbox.stop().await.unwrap();
        assert!(!sandbox.is_running());
    }
}
