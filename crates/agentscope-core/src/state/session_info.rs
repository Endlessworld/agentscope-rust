//! Session metadata, mirroring `io.agentscope.core.state.SessionInfo`.

/// Metadata about a stored session: storage size, last-modified time and the
/// number of state components persisted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub size: u64,
    pub last_modified: u64,
    pub component_count: u32,
}

impl SessionInfo {
    pub fn new(
        session_id: impl Into<String>,
        size: u64,
        last_modified: u64,
        component_count: u32,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            size,
            last_modified,
            component_count,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn last_modified(&self) -> u64 {
        self.last_modified
    }
    pub fn component_count(&self) -> u32 {
        self.component_count
    }
}

impl std::fmt::Display for SessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SessionInfo{{id='{}', size={}, lastModified={}, components={}}}",
            self.session_id, self.size, self.last_modified, self.component_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_and_display() {
        let info = SessionInfo::new("s1", 1024, 1_700_000_000_000, 3);
        assert_eq!(info.session_id(), "s1");
        assert_eq!(info.size(), 1024);
        assert_eq!(info.last_modified(), 1_700_000_000_000);
        assert_eq!(info.component_count(), 3);
        assert_eq!(
            info.to_string(),
            "SessionInfo{id='s1', size=1024, lastModified=1700000000000, components=3}"
        );
    }

    #[test]
    fn serde_roundtrip_camel_case() {
        let info = SessionInfo::new("s1", 10, 20, 1);
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("sessionId"));
        assert!(json.contains("componentCount"));
        let back: SessionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, info);
    }
}
