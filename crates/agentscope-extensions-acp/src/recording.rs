//! Whether the ACP agent journals each model call's full request payload.
//!
//! `request/prepared` is the only session event carrying the whole request
//! (`messages` + `tools`); nothing reads it back, since recovery uses
//! `state/checkpoint` plus `model/dispatch`. It is pure forensics, and on a
//! long coding session it is the single largest journal payload, so the ACP
//! agent leaves it off by default.
//!
//! | env | default | effect |
//! | --- | --- | --- |
//! | `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS` | off | `1`/`true`/`yes`/`on` restores full payload recording |

const RECORD_MODEL_REQUESTS_ENV: &str = "AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS";

/// Whether `request/prepared` events should be journalled.
///
/// Turning this back on restores the previous behaviour exactly: the payload is
/// written in the same commit as `model/dispatch`, and no recovery path changes.
pub fn record_model_requests() -> bool {
    match std::env::var(RECORD_MODEL_REQUESTS_ENV) {
        Ok(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}
