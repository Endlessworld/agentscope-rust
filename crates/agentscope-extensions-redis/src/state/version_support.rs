//! Shared optimistic-concurrency helpers for the Redis-backed [`super::RedisAgentStateStore`],
//! mirroring `io.agentscope.extensions.redis.state.RedisStateVersionSupport`.
//!
//! The Redis key layout for a single versioned state value is:
//!
//! * payload key `{prefix}{slot}:{key}` — JSON string value
//! * version key `{prefix}{slot}:{key}:ver` — monotonic counter string
//! * session keys set `{prefix}{slot}:_keys` — tracks every state key in the session

/// Redis key suffix for the monotonic version counter of a single state value.
pub const VERSION_SUFFIX: &str = ":ver";

/// Lua `ARGV[2]` sentinel for an unconditional overwrite inside [`SAVE_SCRIPT`].
pub const UNCONDITIONAL: &str = "-1";

/// Atomically compare-and-set or unconditionally bump a versioned payload.
///
/// * `KEYS`: payload key, version key, session keys set.
/// * `ARGV`: JSON payload, expected version ([`UNCONDITIONAL`] for bump), state-key member.
///
/// Returns the new version on success, or `-1` on conflict.
///
/// This is a byte-for-byte port of the Java `SAVE_SCRIPT`; the CAS semantics
/// (`expected == -1` means write-always, otherwise optimistic-lock) match the harness
/// `AgentStateStore` contract.
pub const SAVE_SCRIPT: &str = r#"
local function current_version()
  if redis.call('EXISTS', KEYS[2]) == 1 then
    return tonumber(redis.call('GET', KEYS[2]))
  end
  return 0
end

local expected = tonumber(ARGV[2])
if expected == -1 then
  local newVersion = 1
  if redis.call('EXISTS', KEYS[2]) == 1 then
    newVersion = tonumber(redis.call('GET', KEYS[2])) + 1
  end
  redis.call('SET', KEYS[1], ARGV[1])
  redis.call('SET', KEYS[2], newVersion)
  redis.call('SADD', KEYS[3], ARGV[3])
  return newVersion
end

local current = current_version()
if current ~= expected then
  return -1
end

local newVersion = expected + 1
redis.call('SET', KEYS[1], ARGV[1])
redis.call('SET', KEYS[2], newVersion)
redis.call('SADD', KEYS[3], ARGV[3])
return newVersion
"#;

/// Returns the version key for a payload key.
pub fn version_key(payload_key: &str) -> String {
    format!("{payload_key}{VERSION_SUFFIX}")
}

/// Parses a version, treating a missing payload or blank version as `0`.
pub fn parse_version(payload_present: bool, version_value: Option<&str>) -> u64 {
    if !payload_present {
        return 0;
    }
    match version_value {
        None => 0,
        Some(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                0
            } else {
                trimmed.parse::<u64>().unwrap_or(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_key_appends_suffix() {
        assert_eq!(
            version_key("agentscope:session:u/s:k"),
            "agentscope:session:u/s:k:ver"
        );
    }

    #[test]
    fn parse_version_edges() {
        assert_eq!(parse_version(false, Some("5")), 0);
        assert_eq!(parse_version(true, None), 0);
        assert_eq!(parse_version(true, Some("  ")), 0);
        assert_eq!(parse_version(true, Some("42")), 42);
    }
}
