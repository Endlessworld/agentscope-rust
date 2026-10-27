//! Object key layout helpers shared by the COS-backed components.
//!
//! Faithful to the Java layout in `CosAgentStateStore` / `CosBaseStore` /
//! `CosRemoteSnapshotClient`.

/// Anonymous user namespace segment used when no `userId` is supplied.
pub const ANON_USER: &str = "__anon__";

/// Default prefix for the agent state store (`CosAgentStateStore`).
pub const DEFAULT_STATE_PREFIX: &str = "agentscope/state/";

/// Default prefix for the base store (`CosBaseStore`).
pub const DEFAULT_STORE_PREFIX: &str = "agentscope/store/";

/// Default top-level prefix used by the distributed store.
pub const DEFAULT_DISTRIBUTED_PREFIX: &str = "agentscope/";

pub const JSON_SUFFIX: &str = ".json";
pub const LIST_SUFFIX: &str = ".list.json";
pub const HASH_SUFFIX: &str = ".list.hash";
pub const VERSION_SUFFIX: &str = ".version";
pub const SNAPSHOT_SUFFIX: &str = ".tar";

/// Normalizes a user id, mapping `None`/blank to [`ANON_USER`].
pub fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Normalizes a key prefix: backslashes to slashes, strip leading slashes,
/// ensure a single trailing slash. Returns `default` when blank.
pub fn normalize_prefix(prefix: Option<&str>, default: &str) -> String {
    match prefix {
        Some(p) if !p.trim().is_empty() => {
            let mut p = p.replace('\\', "/");
            while p.starts_with('/') {
                p.remove(0);
            }
            if !p.is_empty() && !p.ends_with('/') {
                p.push('/');
            }
            if p.is_empty() {
                default.to_string()
            } else {
                p
            }
        }
        _ => default.to_string(),
    }
}

/// Like [`normalize_prefix`] but the default is the empty string (snapshot client).
pub fn normalize_optional_prefix(prefix: Option<&str>) -> String {
    normalize_prefix(prefix, "")
}

/// Strips leading `/` characters from an item key.
pub fn strip_leading_slashes(s: &str) -> &str {
    s.trim_start_matches('/')
}

/// Validates that a snapshot id is a safe single path segment.
pub fn validate_snapshot_id(id: &str) -> crate::Result<()> {
    if id.trim().is_empty() {
        return Err(crate::CosError::Config(
            "snapshotId must not be blank".to_string(),
        ));
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") || id.contains('\0') {
        return Err(crate::CosError::Config(format!(
            "snapshotId contains unsafe characters: {id}"
        )));
    }
    Ok(())
}

/// Builds the `{prefix}{user}/{session}/` prefix for a session's state objects.
pub fn session_prefix(
    prefix: &str,
    user_id: Option<&str>,
    session_id: &str,
) -> crate::Result<String> {
    if session_id.trim().is_empty() {
        return Err(crate::CosError::Config(
            "sessionId must not be blank".to_string(),
        ));
    }
    Ok(format!("{prefix}{}/{session_id}/", normalize_user(user_id)))
}

/// Builds `{prefix}{ns0}/{ns1}/.../` for the base store namespace layout.
pub fn namespace_prefix(key_prefix: &str, namespace: &[String]) -> String {
    let mut sb = String::from(key_prefix);
    for component in namespace {
        sb.push_str(component);
        sb.push('/');
    }
    sb
}
