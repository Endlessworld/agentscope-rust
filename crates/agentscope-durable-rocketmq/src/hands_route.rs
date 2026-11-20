//! Physical destinations of the Brain–Hands channels.
//!
//! One namespace owns three topics: requests (Brain → Hands), replies (Hands → Brain) and
//! wakeups (any host → any host). The readable slug is bounded so the broker's 64-character
//! topic limit holds even for a long namespace.

use crate::route::{digest_hex, slugify};

/// Characters of the namespace slug kept in a hands topic.
const SLUG_MAX: usize = 24;
/// Hex characters of the namespace digest kept in a hands topic.
const DIGEST_LEN: usize = 8;

/// Topic a Brain publishes Requests to.
pub fn request_topic(namespace: &str) -> String {
    topic(namespace, "req")
}

/// Topic a Hands host publishes Replies to.
pub fn reply_topic(namespace: &str) -> String {
    topic(namespace, "reply")
}

/// Topic a host publishes wakeups to.
pub fn wakeup_topic(namespace: &str) -> String {
    topic(namespace, "wakeup")
}

fn topic(namespace: &str, suffix: &str) -> String {
    let digest = digest_hex(namespace);
    let mut slug = slugify(namespace);
    slug.truncate(SLUG_MAX.min(slug.len()));
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "default" } else { slug };
    format!("durable-{slug}-hands-{}-{suffix}", &digest[..DIGEST_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_channels_stay_apart_and_in_the_name_alphabet() {
        let request = request_topic("tenant");
        let reply = reply_topic("tenant");
        let wakeup = wakeup_topic("tenant");
        assert_ne!(request, reply);
        assert_ne!(reply, wakeup);
        assert_ne!(request, wakeup);
        for topic in [&request, &reply, &wakeup] {
            assert!(
                topic
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-'),
                "{topic:?}"
            );
            assert!(topic.len() <= 64, "{topic:?} is {} chars", topic.len());
        }
        assert!(request.starts_with("durable-tenant-hands-"));
        assert!(request.ends_with("-req"));
    }

    #[test]
    fn a_long_or_hostile_namespace_stays_bounded_and_distinct() {
        let long = request_topic(&"n".repeat(300));
        assert!(long.len() <= 64);
        let spaced = request_topic("acme corp");
        let dashed = request_topic("acme-corp");
        assert_ne!(spaced, dashed, "the digest keeps namespaces apart");
    }
}
