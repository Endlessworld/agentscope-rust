//! Routing context for inbound turns, mirroring `MsgContext` and `SessionIdUtils`.
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Routing context for inbound turns (direct API, channel adapter, group/room/thread).
/// The gateway maps stable conversation keys (`canonical_key`) to session ids.
#[derive(Debug, Clone, Default)]
pub struct MsgContext {
    /// Logical channel name (e.g. slack, discord, web).
    pub channel: Option<String>,
    /// Optional group / team / workspace id.
    pub group: Option<String>,
    /// Optional room / channel id.
    pub room: Option<String>,
    /// Optional thread / topic id.
    pub thread_id: Option<String>,
    /// Optional provider-specific thread timestamp / anchor.
    pub thread_ts: Option<String>,
    /// Additional adapter key/value pairs.
    pub extra: BTreeMap<String, String>,
    /// Optional authenticated user identity for HarnessAgent namespace isolation.
    pub user_id: Option<String>,
}

impl MsgContext {
    /// Default single-conversation context (no channel metadata, no userId).
    pub fn default_context() -> Self {
        Self {
            channel: Some("default".into()),
            ..Default::default()
        }
    }

    /// Returns a copy with the given `userId` set.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Stable key for session routing: the same logical conversation always maps
    /// to the same gateway session id.
    pub fn canonical_key(&self) -> String {
        let mut sb = String::with_capacity(64);
        sb.push_str(self.channel.as_deref().unwrap_or("default"));
        if let Some(g) = self.group.as_deref().filter(|s| !s.trim().is_empty()) {
            sb.push_str(&format!("|g:{}", g.trim()));
        }
        if let Some(r) = self.room.as_deref().filter(|s| !s.trim().is_empty()) {
            sb.push_str(&format!("|r:{}", r.trim()));
        }
        if let Some(t) = self.thread_id.as_deref().filter(|s| !s.trim().is_empty()) {
            sb.push_str(&format!("|t:{}", t.trim()));
        }
        if let Some(ts) = self.thread_ts.as_deref().filter(|s| !s.trim().is_empty()) {
            sb.push_str(&format!("|ts:{}", ts.trim()));
        }
        // `extra` is a BTreeMap, so entries iterate in sorted key order.
        for (k, v) in &self.extra {
            sb.push_str(&format!("|x:{k}={v}"));
        }
        sb
    }
}

/// Deterministic session-ID derivation: 12-character lowercase hex hash of parts
/// joined by `/`, using SHA-256. Filename-safe and stable across machines.
pub fn deterministic_hash(parts: &[&str]) -> String {
    let seed = parts.join("/");
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    hex_lower(&digest[..6])
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_includes_channel_and_segments() {
        let ctx = MsgContext {
            channel: Some("slack".into()),
            group: Some("g1".into()),
            room: Some("r1".into()),
            thread_id: Some("t1".into()),
            thread_ts: Some("123".into()),
            extra: [("k2".into(), "v2".into()), ("k1".into(), "v1".into())]
                .into_iter()
                .collect(),
            user_id: Some("alice".into()),
        };
        let key = ctx.canonical_key();
        // user_id does not participate.
        assert!(!key.contains("alice"));
        assert!(key.contains("|g:g1"));
        assert!(key.contains("|r:r1"));
        assert!(key.contains("|t:t1"));
        assert!(key.contains("|ts:123"));
        // extra is sorted by key.
        let k1 = key.find("|x:k1=").unwrap();
        let k2 = key.find("|x:k2=").unwrap();
        assert!(k1 < k2);
    }

    #[test]
    fn default_context_key_is_stable() {
        let a = MsgContext::default_context().canonical_key();
        let b = MsgContext::default_context().canonical_key();
        assert_eq!(a, b);
        assert!(a.starts_with("default"));
    }

    #[test]
    fn deterministic_hash_is_12_hex_and_reproducible() {
        let h = deterministic_hash(&["channel", "room"]);
        assert_eq!(h.len(), 12);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, deterministic_hash(&["channel", "room"]));
        assert_ne!(h, deterministic_hash(&["channel", "other"]));
    }
}
