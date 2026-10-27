//! Redis key layout for the agent state store, mirroring
//! `io.agentscope.extensions.redis.state.RedisAgentStateKeyLayout`.
//!
//! Two layouts exist. **V0** (the default) keeps the original, backwards-compatible keys;
//! **V1** wraps the `userId/sessionId` session segment in a Redis **hash tag** so that every
//! key of one session hashes to the same Cluster slot:
//!
//! | data | V0 | V1 |
//! |---|---|---|
//! | single state | `{prefix}{user}/{session}:{key}` | `{prefix}{{user}/{session}}:{key}` |
//! | state version | `{prefix}{user}/{session}:{key}:ver` | `{prefix}{{user}/{session}}:{key}:ver` |
//! | list state | `{prefix}{user}/{session}:{key}:list` | `{prefix}{{user}/{session}}:{key}:list` |
//! | session index | `{prefix}{user}/{session}:_keys` | `{prefix}{{user}/{session}}:_keys` |
//!
//! ## Why V1 exists
//!
//! The optimistic-concurrency Lua script
//! ([`super::version_support::SAVE_SCRIPT`]) passes the payload key, the version key and the
//! session index to a single `EVAL`. Redis Cluster rejects multi-key scripts whose keys map to
//! different slots (`CROSSSLOT`), which is exactly what the V0 layout permits, because Cluster
//! hashes each complete key independently. V1 hashes only the text inside `{…}`, pinning all of
//! a session's keys to one slot. V0 and V1 are disjoint key namespaces: a store only ever reads
//! and writes the layout it was configured with.

use agentscope_core::error::{Error, Result};

/// Redis key suffix of the session key index set (`{prefix}{slot}:_keys`).
pub(crate) const KEYS_SUFFIX: &str = ":_keys";

/// Redis key suffix of a list state value (`{prefix}{slot}:{key}:list`).
pub(crate) const LIST_SUFFIX: &str = ":list";

/// Sentinel replacing an absent or blank user id (matches Java `ANON_USER`).
pub(crate) const ANON_USER: &str = "__anon__";

/// Redis key layout version, mirroring Java `RedisAgentStateStore.KeyLayoutVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyLayoutVersion {
    /// Original layout without a Redis hash tag.
    ///
    /// The default: preserves existing Redis keys, and is unsafe on Redis Cluster for the
    /// multi-key CAS script.
    #[default]
    V0,
    /// Cluster-safe layout with the session segment wrapped in a Redis hash tag.
    V1,
}

/// Builds the Redis keys of one agent-state session in a given layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedisAgentStateKeyLayout {
    key_prefix: String,
    /// Either the plain `userId/sessionId` segment (V0) or the hash-tagged
    /// `{userId/sessionId}` segment (V1).
    slot_id: String,
}

impl RedisAgentStateKeyLayout {
    /// Builds a **V1** layout, wrapping the normalized session segment in a Redis hash tag.
    ///
    /// Example: `agentscope:session:{alice/s1}:agent_state`.
    ///
    /// # Errors
    /// Returns an error when the prefix, user id or session id cannot be represented
    /// unambiguously inside the hash tag (a `{`, `}` or `/` in the wrong segment), or when the
    /// session id is blank.
    pub(crate) fn v1(key_prefix: &str, user_id: Option<&str>, session_id: &str) -> Result<Self> {
        validate_v1_key_prefix(key_prefix)?;
        validate_v1_user_id(&normalize_user(user_id))?;
        validate_v1_session_id(session_id)?;
        Ok(Self {
            key_prefix: key_prefix.to_string(),
            slot_id: format!("{{{}}}", session_segment(user_id, session_id)?),
        })
    }

    /// Builds a **V0** layout using the original, non-hash-tagged session segment.
    ///
    /// Example: `agentscope:session:alice/s1:agent_state`.
    ///
    /// # Errors
    /// Returns an error when the session id is blank.
    pub(crate) fn v0(key_prefix: &str, user_id: Option<&str>, session_id: &str) -> Result<Self> {
        Ok(Self {
            key_prefix: key_prefix.to_string(),
            slot_id: session_segment(user_id, session_id)?,
        })
    }

    /// The normalized session segment of this layout (`alice/s1` for V0, `{alice/s1}` for V1).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn slot_id(&self) -> &str {
        &self.slot_id
    }

    /// Redis key of a single state value.
    pub(crate) fn state_key(&self, key: &str) -> String {
        format!("{}{}:{key}", self.key_prefix, self.slot_id)
    }

    /// Redis key of a list state value.
    pub(crate) fn list_key(&self, key: &str) -> String {
        format!("{}{LIST_SUFFIX}", self.state_key(key))
    }

    /// Session-index member tracking a list state key.
    pub(crate) fn list_track_key(&self, key: &str) -> String {
        format!("{key}{LIST_SUFFIX}")
    }

    /// Redis key of the session index set.
    pub(crate) fn keys_key(&self) -> String {
        format!("{}{}{KEYS_SUFFIX}", self.key_prefix, self.slot_id)
    }

    /// Whether a session-index member represents a list state key.
    pub(crate) fn is_list_track_key(tracked_key: &str) -> bool {
        tracked_key.ends_with(LIST_SUFFIX)
    }

    /// The state key name represented by a list session-index member.
    pub(crate) fn base_key_from_list_track_key(tracked_key: &str) -> &str {
        &tracked_key[..tracked_key.len() - LIST_SUFFIX.len()]
    }
}

/// Normalizes an absent or blank user id to [`ANON_USER`].
pub(crate) fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Redis `SCAN MATCH` pattern selecting the V0 session index keys of one user segment.
pub(crate) fn v0_keys_pattern(key_prefix: &str, user_segment: &str) -> String {
    format!(
        "{}{}/*{KEYS_SUFFIX}",
        escape_redis_glob_literal(key_prefix),
        escape_redis_glob_literal(user_segment)
    )
}

/// Redis `SCAN MATCH` pattern selecting the V1 session index keys of one user segment.
pub(crate) fn v1_keys_pattern(key_prefix: &str, user_segment: &str) -> String {
    format!(
        "{}{{{}/*}}{KEYS_SUFFIX}",
        escape_redis_glob_literal(key_prefix),
        escape_redis_glob_literal(user_segment)
    )
}

/// Validates that a V1 prefix cannot alter the Redis hash tag added around a session segment.
pub(crate) fn validate_v1_key_prefix(key_prefix: &str) -> Result<()> {
    reject_characters(key_prefix, "keyPrefix", false)
}

/// Validates that a normalized V1 user id is unambiguous inside `{userId/sessionId}`.
pub(crate) fn validate_v1_user_id(user_id: &str) -> Result<()> {
    reject_characters(user_id, "userId", true)
}

/// Parses a V0 session index key, returning the session id when it matches the user segment.
pub(crate) fn parse_v0_session_id_from_keys_key(
    keys_key: &str,
    key_prefix: &str,
    user_segment: &str,
) -> Option<String> {
    let user_prefix = format!("{key_prefix}{user_segment}/");
    keys_key
        .strip_prefix(&user_prefix)?
        .strip_suffix(KEYS_SUFFIX)
        .map(str::to_string)
}

/// Parses a V1 session index key, returning the session id without the hash-tag braces.
pub(crate) fn parse_v1_session_id_from_keys_key(
    keys_key: &str,
    key_prefix: &str,
    user_segment: &str,
) -> Option<String> {
    let user_prefix = format!("{key_prefix}{{{user_segment}/");
    let suffix = format!("}}{KEYS_SUFFIX}");
    keys_key
        .strip_prefix(&user_prefix)?
        .strip_suffix(&suffix)
        .map(str::to_string)
}

/// Escapes Redis glob metacharacters so a value is matched **literally** by `SCAN MATCH`.
///
/// This transforms only the temporary scan pattern; it never modifies the caller's value or
/// the Redis key stored for that value.
fn escape_redis_glob_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn validate_v1_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(Error::Other("sessionId must not be blank".into()));
    }
    reject_characters(session_id, "sessionId", true)
}

fn reject_characters(value: &str, name: &str, reject_slash: bool) -> Result<()> {
    if value.contains('{') || value.contains('}') || (reject_slash && value.contains('/')) {
        return Err(Error::Other(format!(
            "{name}: {value} contains a character that is not supported by V1"
        )));
    }
    Ok(())
}

/// Builds the normalized `userId/sessionId` segment shared by the V0 and V1 layouts.
fn session_segment(user_id: Option<&str>, session_id: &str) -> Result<String> {
    if session_id.trim().is_empty() {
        return Err(Error::Other("sessionId must not be blank".into()));
    }
    Ok(format!("{}/{}", normalize_user(user_id), session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::version_support::version_key;

    /// Extracts the Redis hash tag (`{...}`) of a key, when present.
    fn hash_tag(key: &str) -> Option<&str> {
        let start = key.find('{')? + 1;
        let end = start + key[start..].find('}')?;
        Some(&key[start..end])
    }

    #[test]
    fn v0_keeps_the_original_layout() {
        let layout =
            RedisAgentStateKeyLayout::v0("agentscope:session:", Some("alice"), "s1").unwrap();
        assert_eq!(layout.slot_id(), "alice/s1");
        assert_eq!(
            layout.state_key("agent_state"),
            "agentscope:session:alice/s1:agent_state"
        );
        assert_eq!(
            layout.list_key("messages"),
            "agentscope:session:alice/s1:messages:list"
        );
        assert_eq!(layout.keys_key(), "agentscope:session:alice/s1:_keys");
        assert_eq!(hash_tag(&layout.state_key("k")), None, "V0 has no hash tag");
    }

    #[test]
    fn v0_normalizes_missing_user_to_anon() {
        let layout = RedisAgentStateKeyLayout::v0("agentscope:session:", None, "s1").unwrap();
        assert_eq!(layout.state_key("k"), "agentscope:session:__anon__/s1:k");
        assert_eq!(ANON_USER, "__anon__");
    }

    #[test]
    fn v1_wraps_the_session_segment_in_a_hash_tag() {
        let layout = RedisAgentStateKeyLayout::v1("custom:session:", None, "s1").unwrap();
        assert_eq!(layout.slot_id(), "{__anon__/s1}");
        assert_eq!(
            layout.state_key("agent_state"),
            "custom:session:{__anon__/s1}:agent_state"
        );
        assert_eq!(layout.keys_key(), "custom:session:{__anon__/s1}:_keys");
    }

    #[test]
    fn v1_session_keys_share_one_hash_tag() {
        // Regression for the Java `fix(agent): agent state redis key without same hashslot`:
        // every key the CAS script touches must carry the same hash tag, otherwise Redis
        // Cluster rejects the script with CROSSSLOT.
        let layout =
            RedisAgentStateKeyLayout::v1("agentscope:session:", Some("alice"), "s1").unwrap();
        let state = layout.state_key("agent_state");
        let list = layout.list_key("messages");
        let index = layout.keys_key();
        let version = version_key(&state);

        assert_eq!(hash_tag(&state), Some("alice/s1"));
        assert_eq!(
            hash_tag(&list),
            hash_tag(&state),
            "list key shares the session slot"
        );
        assert_eq!(
            hash_tag(&index),
            hash_tag(&state),
            "session index shares the slot"
        );
        assert_eq!(
            hash_tag(&version),
            hash_tag(&state),
            "version key shares the slot"
        );
        assert_eq!(version, "agentscope:session:{alice/s1}:agent_state:ver");
    }

    #[test]
    fn v1_rejects_identifiers_that_would_break_the_hash_tag() {
        for prefix in ["scope{bad:", "scope}bad:"] {
            let err = RedisAgentStateKeyLayout::v1(prefix, Some("alice"), "s1").unwrap_err();
            assert!(err.to_string().contains("not supported by V1"), "{err}");
        }
        for user in ["bad/user", "bad{user", "bad}user"] {
            let err = RedisAgentStateKeyLayout::v1("p:", Some(user), "s1").unwrap_err();
            assert!(err.to_string().contains("not supported by V1"), "{err}");
        }
        for session in ["bad/session", "bad{session", "bad}session"] {
            assert!(RedisAgentStateKeyLayout::v1("p:", Some("alice"), session).is_err());
        }
    }

    #[test]
    fn v0_keeps_previously_valid_identifiers_and_rejects_blank_sessions() {
        let layout = RedisAgentStateKeyLayout::v0("p:", Some("u/{x}"), "s/{y}").unwrap();
        assert_eq!(layout.state_key("k"), "p:u/{x}/s/{y}:k");
        assert!(RedisAgentStateKeyLayout::v0("p:", Some("u"), "  ").is_err());
        assert!(RedisAgentStateKeyLayout::v1("p:", Some("u"), "  ").is_err());
    }

    #[test]
    fn list_track_key_helpers_are_symmetric() {
        let layout = RedisAgentStateKeyLayout::v0("p:", Some("u"), "s").unwrap();
        assert_eq!(layout.list_track_key("messages"), "messages:list");
        assert!(RedisAgentStateKeyLayout::is_list_track_key("messages:list"));
        assert!(!RedisAgentStateKeyLayout::is_list_track_key("messages"));
        assert_eq!(
            RedisAgentStateKeyLayout::base_key_from_list_track_key("messages:list"),
            "messages"
        );
    }

    #[test]
    fn listing_patterns_escape_glob_literals() {
        // Literal values as they arrive from the caller (raw strings: backslashes verbatim).
        let prefix = r"tenant*?:[x]\:";
        let user = r"u*?[x]\";
        assert_eq!(
            v0_keys_pattern(prefix, user),
            r"tenant\*\?:\[x\]\\:u\*\?\[x\]\\/*:_keys"
        );
        assert_eq!(
            v1_keys_pattern(prefix, user),
            r"tenant\*\?:\[x\]\\:{u\*\?\[x\]\\/*}:_keys"
        );
    }

    #[test]
    fn listed_session_ids_round_trip_through_the_patterns() {
        let v0 = RedisAgentStateKeyLayout::v0("tenant:", Some("u"), "wanted").unwrap();
        assert_eq!(
            parse_v0_session_id_from_keys_key(&v0.keys_key(), "tenant:", "u").as_deref(),
            Some("wanted")
        );
        assert_eq!(
            parse_v0_session_id_from_keys_key("tenant:neighbor/other:_keys", "tenant:", "u"),
            None
        );
        assert_eq!(
            parse_v0_session_id_from_keys_key("other:u/foreign:_keys", "tenant:", "u"),
            None
        );

        let v1 = RedisAgentStateKeyLayout::v1("tenant:", Some("u"), "wanted").unwrap();
        assert_eq!(
            parse_v1_session_id_from_keys_key(&v1.keys_key(), "tenant:", "u").as_deref(),
            Some("wanted")
        );
        assert_eq!(
            parse_v1_session_id_from_keys_key("tenant:{neighbor/other}:_keys", "tenant:", "u"),
            None
        );
        assert_eq!(
            parse_v1_session_id_from_keys_key("other:{u/foreign}:_keys", "tenant:", "u"),
            None
        );

        // V0 and V1 are disjoint namespaces: neither parser accepts the other's keys.
        assert_eq!(
            parse_v1_session_id_from_keys_key(&v0.keys_key(), "tenant:", "u"),
            None
        );
        assert_eq!(
            parse_v0_session_id_from_keys_key(&v1.keys_key(), "tenant:", "u"),
            None
        );
    }
}
