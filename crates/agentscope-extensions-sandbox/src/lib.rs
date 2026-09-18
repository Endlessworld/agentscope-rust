//! `agentscope-extensions-sandbox` — remote sandbox backends for AgentScope Rust.
//!
//! Mirrors the Java `agentscope-extensions-sandbox` aggregator module, which contains four
//! backend submodules: `agentrun`, `daytona`, `e2b` and `kubernetes`.
//!
//! # Architecture
//!
//! Every backend is adapted onto the harness [`agentscope_harness::agent::sandbox::Sandbox`] and
//! [`agentscope_harness::agent::sandbox::SandboxClient`] traits through the shared glue in
//! [`remote`]. Because the harness `SandboxState` is a closed enum, backend state is projected
//! into its `Remote` variant with backend-specific fields carried in `metadata`.
//!
//! | Java submodule | Rust module | Client constructor |
//! |---|---|---|
//! | `agentrun` | [`agentrun`] | [`agentrun::agentrun_client`] |
//! | `daytona` | [`daytona`] | [`daytona::daytona_client`] |
//! | `e2b` | [`e2b`] | [`e2b::e2b_client`] |
//! | `kubernetes` | [`kubernetes`] | [`kubernetes::kubernetes_client`] |
//!
//! Shared HTTP / retry / workspace-projection plumbing lives in [`http`], [`retry`], [`error`],
//! [`workspace`] and [`remote`]. HTTP uses [`reqwest`]; all request-body construction, response
//! parsing, retry/backoff and error mapping are covered by offline unit tests.

pub mod agentrun;
pub mod daytona;
pub mod e2b;
pub mod error;
pub mod fs_spec;
pub mod http;
pub mod kubernetes;
pub mod kubernetes_files;
pub mod remote;
pub mod retry;
pub mod workspace;

pub use error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
pub use http::{build_client, join_url, HttpClient, HttpOptions};
pub use remote::{
    ensure_output_success, workspace_projection_hash, RemoteBackend, RemoteExecOutput,
    RemoteSandbox, RemoteSandboxClient, RemoteSandboxCore, RemoteSandboxHandle,
    RemoteSandboxStateData,
};
pub use retry::{with_retries, RetryPolicy};
pub use workspace::{
    build_workspace_setup_script, ensure_success, parent_dir, resolve_remote_path, shell_quote,
    short_hash,
};
