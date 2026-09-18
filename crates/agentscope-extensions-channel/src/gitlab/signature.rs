//! GitLab webhook token verification (constant-time `X-Gitlab-Token` match).

use crate::common::constant_time_eq;

/// Constant-time comparison of the configured webhook token against the `X-Gitlab-Token` header.
///
/// Mirrors the `GitLabWebhookController` header check; a missing or unequal token is rejected.
pub fn verify_webhook_token(expected: &str, provided: Option<&str>) -> bool {
    match provided {
        Some(provided) => constant_time_eq(expected, provided),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_identical_tokens() {
        assert!(verify_webhook_token("s3cret", Some("s3cret")));
        assert!(!verify_webhook_token("s3cret", Some("s3cres")));
        assert!(!verify_webhook_token("s3cret", Some("short")));
        assert!(!verify_webhook_token("s3cret", None));
    }
}
