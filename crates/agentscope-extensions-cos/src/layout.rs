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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_prefixes_use_the_default() {
        assert_eq!(
            normalize_prefix(None, DEFAULT_STATE_PREFIX),
            DEFAULT_STATE_PREFIX
        );
        assert_eq!(
            normalize_prefix(Some(""), DEFAULT_STATE_PREFIX),
            DEFAULT_STATE_PREFIX
        );
        assert_eq!(
            normalize_prefix(Some("   "), DEFAULT_STATE_PREFIX),
            DEFAULT_STATE_PREFIX
        );
        // A prefix that is nothing but separators normalizes to empty and therefore
        // also falls back to the default.
        assert_eq!(
            normalize_prefix(Some("/"), DEFAULT_STATE_PREFIX),
            DEFAULT_STATE_PREFIX
        );
        assert_eq!(
            normalize_prefix(Some("\\"), DEFAULT_STATE_PREFIX),
            DEFAULT_STATE_PREFIX
        );
    }

    #[test]
    fn prefixes_strip_leading_slashes_and_gain_a_trailing_one() {
        assert_eq!(normalize_prefix(Some("a/b"), "d"), "a/b/");
        assert_eq!(normalize_prefix(Some("/a/b"), "d"), "a/b/");
        assert_eq!(normalize_prefix(Some("///a/b/"), "d"), "a/b/");
        assert_eq!(normalize_prefix(Some("a\\b"), "d"), "a/b/");
        // Java only validates `isBlank()`; surrounding spaces inside a non-blank
        // prefix are preserved verbatim.
        assert_eq!(normalize_prefix(Some(" agentscope "), "d"), " agentscope /");
        // The empty default keeps the prefix empty (snapshot layout).
        assert_eq!(normalize_optional_prefix(None), "");
        assert_eq!(normalize_optional_prefix(Some("/snap/")), "snap/");
    }

    #[test]
    fn keys_and_users_normalize_like_java() {
        assert_eq!(strip_leading_slashes("/a/b"), "a/b");
        assert_eq!(strip_leading_slashes("///a"), "a");
        assert_eq!(strip_leading_slashes("a/b"), "a/b");
        assert_eq!(strip_leading_slashes(""), "");

        assert_eq!(normalize_user(None), ANON_USER);
        assert_eq!(normalize_user(Some("")), ANON_USER);
        assert_eq!(normalize_user(Some("  ")), ANON_USER);
        assert_eq!(normalize_user(Some("u1")), "u1");
    }

    #[test]
    fn session_and_namespace_prefixes() {
        assert_eq!(
            session_prefix("agentscope/state/", Some("u1"), "s1").unwrap(),
            "agentscope/state/u1/s1/"
        );
        assert_eq!(
            session_prefix("agentscope/state/", None, "s1").unwrap(),
            "agentscope/state/__anon__/s1/"
        );
        assert!(session_prefix("p/", Some("u"), "").is_err());
        assert!(session_prefix("p/", Some("u"), "   ").is_err());

        assert_eq!(
            namespace_prefix("agentscope/store/", &[]),
            "agentscope/store/"
        );
        assert_eq!(
            namespace_prefix("agentscope/store/", &[String::from("a"), String::from("b")]),
            "agentscope/store/a/b/"
        );
    }

    #[test]
    fn snapshot_ids_reject_unsafe_segments() {
        assert!(validate_snapshot_id("snap-1").is_ok());
        for id in ["", "   ", "a/b", "a\\b", "..", "a/../b", "a\0b"] {
            assert!(validate_snapshot_id(id).is_err(), "should reject {id:?}");
        }
    }
}
