//! Resolution of `ScheduleConfig::zone_id` to a UTC offset.
//!
//! Java hands the id to `CronScheduleBuilder#inTimeZone(TimeZone.getTimeZone(zoneId))`,
//! i.e. the cron expression is evaluated against that zone's **wall clock**.
//! `chrono` does not bundle an IANA database (that is the `chrono-tz` crate), so
//! this module maps the id onto a [`FixedOffset`]:
//!
//! * absent / `null`  → UTC (Quartz's default);
//! * fixed offsets     → `"+08:00"`, `"-05:30"`, `"+0800"`, `"UTC+8"`, `"GMT-4"`;
//! * UTC aliases       → `"UTC"`, `"Z"`, `"UT"`, `"GMT"`, `"Etc/UTC"`;
//! * a small table of DST-free IANA ids (`Asia/Shanghai`, `Asia/Tokyo`, ...).
//!
//! Anything else — including zones that observe daylight saving — is rejected
//! with an explicit error. Java's `TimeZone.getTimeZone` silently falls back to
//! GMT for unknown ids, which hides typos; failing fast is intentional here.

use chrono::FixedOffset;
use thiserror::Error;

/// Error raised when a `zone_id` cannot be resolved to a fixed offset.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "unsupported time zone id '{zone_id}': expected \"UTC\", a fixed offset such as \"+08:00\", \
     or a DST-free zone such as \"Asia/Shanghai\""
)]
pub struct ScheduleZoneError {
    /// The rejected id, as supplied by the caller.
    pub zone_id: String,
}

/// IANA ids that never observe daylight saving, with their constant offset in
/// seconds east of UTC. Lookup is case-insensitive.
const FIXED_ZONES: &[(&str, i32)] = &[
    ("UTC", 0),
    ("UT", 0),
    ("GMT", 0),
    ("Z", 0),
    ("ETC/UTC", 0),
    ("ETC/GMT", 0),
    ("ASIA/SHANGHAI", 8 * 3600),
    ("ASIA/CHONGQING", 8 * 3600),
    ("ASIA/HONG_KONG", 8 * 3600),
    ("ASIA/TAIPEI", 8 * 3600),
    ("ASIA/SINGAPORE", 8 * 3600),
    ("ASIA/TOKYO", 9 * 3600),
    ("ASIA/SEOUL", 9 * 3600),
    ("ASIA/KOLKATA", 5 * 3600 + 1800),
    ("ASIA/DUBAI", 4 * 3600),
];

/// Resolves `zone_id` (as stored on a [`super::ScheduleConfig`]) to a fixed
/// offset. `None` means UTC.
pub fn resolve_zone(zone_id: Option<&str>) -> Result<FixedOffset, ScheduleZoneError> {
    let raw = match zone_id {
        None => return Ok(utc()),
        Some(raw) => raw,
    };
    parse_zone_id(raw).ok_or_else(|| ScheduleZoneError { zone_id: raw.to_string() })
}

fn utc() -> FixedOffset {
    // 0 is always a valid offset.
    FixedOffset::east_opt(0).expect("zero offset is valid")
}

fn parse_zone_id(raw: &str) -> Option<FixedOffset> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    if let Some((_, seconds)) = FIXED_ZONES.iter().find(|(name, _)| *name == upper.as_str()) {
        return FixedOffset::east_opt(*seconds);
    }
    // `UTC+8` / `GMT-4:00` / `+08:00` / `+0800` / `-5`.
    let body = upper
        .strip_prefix("UTC")
        .or_else(|| upper.strip_prefix("GMT"))
        .unwrap_or(upper.as_str());
    parse_offset(body)
}

fn parse_offset(body: &str) -> Option<FixedOffset> {
    let (sign, rest) = match body.as_bytes().first()? {
        b'+' => (1, &body[1..]),
        b'-' => (-1, &body[1..]),
        _ => return None,
    };
    let (hour_part, minute_part) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None => match rest.len() {
            1 | 2 => (rest, "0"),
            3 => (&rest[0..1], &rest[1..3]),
            4 => (&rest[0..2], &rest[2..4]),
            _ => return None,
        },
    };
    let hours: i32 = hour_part.parse().ok()?;
    let minutes: i32 = minute_part.parse().ok()?;
    if hours > 18 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_zone_defaults_to_utc() {
        assert_eq!(resolve_zone(None).unwrap(), FixedOffset::east_opt(0).unwrap());
    }

    #[test]
    fn resolves_utc_aliases() {
        for id in ["UTC", "utc", "Z", "UT", "GMT", "Etc/UTC", "Etc/GMT", "+00:00", "UTC+0"] {
            assert_eq!(
                resolve_zone(Some(id)).unwrap(),
                FixedOffset::east_opt(0).unwrap(),
                "zone id {id}"
            );
        }
    }

    #[test]
    fn resolves_iana_aliases_and_fixed_offsets() {
        let eight = 8 * 3600;
        for id in ["+08:00", "+0800", "UTC+8", "GMT+08:00", "Asia/Shanghai", "asia/shanghai"] {
            assert_eq!(
                resolve_zone(Some(id)).unwrap(),
                FixedOffset::east_opt(eight).unwrap(),
                "zone id {id}"
            );
        }
        assert_eq!(
            resolve_zone(Some("-05:30")).unwrap(),
            FixedOffset::east_opt(-(5 * 3600 + 1800)).unwrap()
        );
        assert_eq!(
            resolve_zone(Some("Asia/Kolkata")).unwrap(),
            FixedOffset::east_opt(5 * 3600 + 1800).unwrap()
        );
    }

    #[test]
    fn rejects_unknown_and_dst_zones() {
        for id in ["", "   ", "Mars/Olympus", "Europe/London", "America/New_York", "+19:00", "+08:99", "8"] {
            let err = resolve_zone(Some(id)).unwrap_err();
            assert_eq!(err.zone_id, id);
            assert!(err.to_string().contains("unsupported time zone id"), "err was {err}");
        }
    }
}
