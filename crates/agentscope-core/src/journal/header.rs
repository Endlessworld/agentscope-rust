//! Session format marker, mirroring `SessionHeader` / `SessionHistoryMode`.

use serde::{Deserialize, Serialize};

use super::session_error;

/// Exactly one authority: standalone state-store persistence or native committed history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHistoryMode {
    /// State-store persistence owns history.
    Legacy,
    /// The native event log owns history.
    EventLog,
}

/// Immutable coverage/format marker. A checkpoint baseline does not pretend to contain
/// earlier history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub format_version: i32,
    pub authority: String,
    pub replay_coverage: String,
    pub created_at: i64,
}

impl SessionHeader {
    /// The header written for a new native session.
    pub fn current() -> Self {
        Self {
            format_version: 1,
            authority: "event_log".to_string(),
            replay_coverage: "adapter_chunks".to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Rejects an unsupported format/authority instead of silently rewriting history.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.format_version != 1 || self.authority != "event_log" {
            return Err(session_error("Unsupported session format/authority"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_header_round_trips_camel_case() {
        let header = SessionHeader::current();
        let json = serde_json::to_value(&header).unwrap();
        assert_eq!(json["formatVersion"], 1);
        assert_eq!(json["authority"], "event_log");
        assert_eq!(json["replayCoverage"], "adapter_chunks");
        let back: SessionHeader = serde_json::from_value(json).unwrap();
        assert_eq!(back, header);
        back.validate().unwrap();
    }

    #[test]
    fn unsupported_format_or_authority_is_rejected() {
        let mut header = SessionHeader::current();
        header.format_version = 2;
        assert!(header.validate().is_err());
        let mut header = SessionHeader::current();
        header.authority = "state_store".into();
        assert!(header.validate().is_err());
    }
}
