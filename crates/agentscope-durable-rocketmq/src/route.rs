//! Physical destination of one session's event stream.
//!
//! Mirrors the Java topology rule ("one naming convention shared by Brain and Worker; physical
//! topics stay infrastructure details"): the namespace decides the topic, the session decides the
//! tag, and the session storage key is the message key hosts use for tracing.

use agentscope_durable::SessionKey;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// Prefix of every topic this adapter creates.
pub const TOPIC_PREFIX: &str = "durable";

/// Prefix of every session tag.
pub const TAG_PREFIX: &str = "s-";

/// Characters of the namespace slug kept in the topic; the digest suffix keeps it unique.
const SLUG_MAX: usize = 40;

/// Hex characters of the namespace digest kept in the topic.
const TOPIC_DIGEST_LEN: usize = 8;

/// Hex characters of the session digest used as the tag.
const TAG_DIGEST_LEN: usize = 16;

/// Where one session's events are published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Message queue topic (shared by every session of the namespace).
    pub topic: String,
    /// Message queue tag (one per session, used for subscription filtering).
    pub tag: String,
    /// Message key: the durable session storage key, so a broker trace maps back to a session.
    pub key: String,
}

/// Derive the destination for `session`. Pure: the same key always yields the same route.
pub fn route(session: &SessionKey) -> Route {
    Route {
        topic: topic_for_namespace(&session.namespace),
        tag: tag_for_session(session),
        key: session.storage_key(),
    }
}

/// Topic for a namespace.
///
/// The readable slug is a convenience only; the digest suffix is what makes two namespaces that
/// sanitize to the same slug ("acme corp" and "acme-corp") land on different topics. A namespace
/// made entirely of non-ASCII characters therefore still gets a stable, distinct topic.
pub fn topic_for_namespace(namespace: &str) -> String {
    let digest = digest_hex(namespace);
    format!(
        "{TOPIC_PREFIX}-{}-{}",
        slugify(namespace),
        &digest[..TOPIC_DIGEST_LEN]
    )
}

fn tag_for_session(session: &SessionKey) -> String {
    let digest = digest_hex(&session.storage_key());
    format!("{TAG_PREFIX}{}", &digest[..TAG_DIGEST_LEN])
}

/// Lowercase ASCII slug: letters/digits survive, `-`/`_` survive, everything else becomes a
/// single `-`, runs are collapsed and the result is bounded.
fn slugify(namespace: &str) -> String {
    let mut slug = String::with_capacity(namespace.len().min(SLUG_MAX));
    for character in namespace.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else if character == '-' || character == '_' {
            character
        } else {
            '-'
        };
        if mapped == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(mapped);
        if slug.len() >= SLUG_MAX {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "default".to_string()
    } else {
        slug.to_string()
    }
}

fn digest_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing into a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(namespace: &str, session_id: &str) -> SessionKey {
        SessionKey::new(namespace, session_id).expect("valid session key")
    }

    #[test]
    fn a_route_is_stable_and_namespaced() {
        let key = session("tenant", "session-a");
        let first = route(&key);
        assert_eq!(first, route(&key), "derivation must be pure");
        assert_eq!(first.key, key.storage_key());
        assert!(first.topic.starts_with("durable-tenant-"));
        assert!(first.tag.starts_with(TAG_PREFIX));
        assert!(
            first
                .topic
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "topic must stay in the RocketMQ name alphabet: {}",
            first.topic
        );

        let other_session = route(&session("tenant", "session-b"));
        assert_eq!(other_session.topic, first.topic, "one topic per namespace");
        assert_ne!(other_session.tag, first.tag, "one tag per session");

        let other_namespace = route(&session("other", "session-a"));
        assert_ne!(other_namespace.topic, first.topic);
    }

    #[test]
    fn namespaces_that_sanitize_alike_still_get_distinct_topics() {
        let spaced = topic_for_namespace("acme corp");
        let dashed = topic_for_namespace("acme-corp");
        assert!(spaced.starts_with("durable-acme-corp-"));
        assert!(dashed.starts_with("durable-acme-corp-"));
        assert_ne!(spaced, dashed);
    }

    #[test]
    fn hostile_and_empty_namespaces_stay_in_the_name_alphabet() {
        for namespace in ["", "租户/一", "...", "UPPER.Case"] {
            let topic = topic_for_namespace(namespace);
            assert!(
                topic
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{namespace:?} produced {topic:?}"
            );
            assert!(topic.len() <= 64, "topic must stay short: {topic:?}");
        }
        assert!(topic_for_namespace("").starts_with("durable-default-"));
    }

    #[test]
    fn a_long_namespace_is_bounded() {
        let topic = topic_for_namespace(&"n".repeat(300));
        assert!(topic.len() <= 64, "topic must stay short: {} chars", topic.len());
    }
}
