//! Transport layer, mirroring `io.agentscope.extensions.aistio.transport`.

mod collaboration_client;
mod contract_http_server;
mod contract_provider;
mod control_plane_http_client;
mod http_self_registration;
mod orchestration_client;

pub use collaboration_client::{
    await_runtime_tool_approval, CollaborationClient, RuntimeApprovalDecision, RuntimeApprovalSource,
    RUNTIME_APPROVAL_POLL_INTERVAL, TASK_TOKEN_HEADER,
};
pub use orchestration_client::OrchestrationClient;
pub use contract_http_server::{
    ContractHttpServer, COMMAND_ID_HEADER, DEFAULT_MESSAGE_LIMIT, MAX_SESSION_PAGE,
};
pub use contract_provider::ContractProvider;
pub use control_plane_http_client::{
    encode_path_segment, encode_query, require_ok, trim_slash, BytesResponse,
    ControlPlaneHttpClient, Response, HTTP_TIMEOUT, INTERNAL_TOKEN_HEADER,
};
pub use http_self_registration::{
    parse_registered_identity, HttpSelfRegistration, IdentityListener, RegisteredIdentity,
    RegistrationOptions, DEFAULT_HEARTBEAT_INTERVAL, REGISTRATION_CREDENTIAL_HEADER,
};
