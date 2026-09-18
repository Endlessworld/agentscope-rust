//! Minimal cron expression parser supporting the 6-field Unix/Quartz-style
//! syntax used by the Java scheduler examples (`sec min hour dom month dow`),
//! plus the standard 5-field form.
//!
//! Supported field tokens: `*`, `?` (alias of `*`), `N`, `a-b`, `a-b/step`,
//! `*/step`, `a,b,c`, and month/weekday names (`JAN-DEC`, `SUN-SAT`).

use chrono::{DateTime, Datelike, Timelike, Utc};
use thiserror::Error;

/// Error raised when parsing an invalid cron expression.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("Invalid cron expression: {0}")]
pub struct CronParseError(pub String);

#[derive(Debug, Clone)]
struct Field {
    /// Allowed values, sorted and de-duplicated.
    values: Vec<u32>,
}

impl Field {
    fn matches(&self, v: u32) -> bool {
        self.values.binary_search(&v).is_ok()
    }
}

/// A parsed cron expression.
#[derive(Debug, Clone)]
pub struct CronExpression {
    seconds: Field,
    minutes: Field,
    hours: Field,
    days_of_month: Field,
    months: Field,
    days_of_week: Field,
    /// True when the day-of-month field was `*`/`?`, in which case the
    /// day-of-week field governs (standard cron OR-semantics).
    dom_wildcard: bool,
    /// True when the day-of-week field was `*`/`?`.
    dow_wildcard: bool,
}

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DOW_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

impl CronExpression {
    /// Parses a cron expression.
    ///
    /// Accepts 6 fields (`sec min hour dom month dow`) or 5 fields
    /// (`min hour dom month dow`, seconds default to `0`).
    pub fn parse(expr: &str) -> Result<Self, CronParseError> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(CronParseError("expression is empty".into()));
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let (sec, min, hour, dom, month, dow) = match parts.len() {
            5 => ("0", parts[0], parts[1], parts[2], parts[3], parts[4]),
            6 => (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]),
            n => {
                return Err(CronParseError(format!(
                    "expected 5 or 6 fields, got {n}"
                )))
            }
        };

        let dom_wildcard = is_wildcard(dom);
        let dow_wildcard = is_wildcard(dow);

        Ok(Self {
            seconds: Field { values: parse_field(sec, 0, 59, &[])? },
            minutes: Field { values: parse_field(min, 0, 59, &[])? },
            hours: Field { values: parse_field(hour, 0, 23, &[])? },
            days_of_month: Field { values: parse_field(dom, 1, 31, &[])? },
            months: Field { values: parse_field(month, 1, 12, &MONTH_NAMES)? },
            days_of_week: Field { values: parse_field(dow, 0, 7, &DOW_NAMES)? },
            dom_wildcard,
            dow_wildcard,
        })
    }

    /// Returns true when `now` satisfies this expression's field constraints.
    pub fn matches(&self, now: DateTime<Utc>) -> bool {
        if !self.seconds.matches(now.second()) || !self.minutes.matches(now.minute()) {
            return false;
        }
        if !self.hours.matches(now.hour()) {
            return false;
        }
        if !self.months.matches(now.month()) {
            return false;
        }

        let dom_ok = self.days_of_month.matches(now.day());
        // chrono: Monday=0..Sunday=6; normalise cron Sunday(0 or 7) instead.
        let dow = now.weekday().num_days_from_sunday();
        let dow_ok = self.days_of_week.matches(dow) || (dow == 0 && self.days_of_week.matches(7));

        match (self.dom_wildcard, self.dow_wildcard) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// Computes the next fire time strictly after `from`, scanning minute by
    /// minute for up to 4 years (bounded work).
    pub fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut candidate = from + chrono::Duration::seconds(1);
        // Align to the next whole second.
        candidate = candidate.with_nanosecond(0).unwrap_or(candidate);
        // Scan up to 4 years of minutes (a leap year plus slack).
        let limit = 4 * 366 * 24 * 60;
        for _ in 0..limit {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate += chrono::Duration::seconds(1);
        }
        None
    }
}

fn is_wildcard(field: &str) -> bool {
    let f = field.trim();
    f == "*" || f == "?"
}

fn parse_field(
    field: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<Vec<u32>, CronParseError> {
    let mut out: Vec<u32> = Vec::new();
    for token in field.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err(CronParseError("empty field segment".into()));
        }
        let (range_part, step) = match token.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .trim()
                    .parse()
                    .map_err(|_| CronParseError(format!("invalid step '{s}'")))?;
                if step == 0 {
                    return Err(CronParseError("step must be > 0".into()));
                }
                (r.trim(), step)
            }
            None => (token, 1),
        };

        let (start, end) = if range_part == "*" || range_part == "?" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (resolve(a, names, min, max)?, resolve(b, names, min, max)?)
        } else {
            let v = resolve(range_part, names, min, max)?;
            // A bare `a/step` means "from a through the field maximum"
            // (cron/Quartz semantics); a bare `a` is the single value `a`.
            if step > 1 {
                (v, max)
            } else {
                (v, v)
            }
        };

        if start > end {
            return Err(CronParseError(format!(
                "range start {start} > end {end}"
            )));
        }

        let mut v = start;
        while v <= end {
            out.push(v);
            v += step;
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err(CronParseError("field resolved to no values".into()));
    }
    Ok(out)
}

fn resolve(token: &str, names: &[&str], min: u32, max: u32) -> Result<u32, CronParseError> {
    let t = token.trim();
    let numeric = t.parse::<u32>().ok().or_else(|| {
        let upper = t.to_ascii_uppercase();
        names
            .iter()
            .position(|n| *n == upper)
            .map(|idx| names_index_to_value(idx, names, min))
    });
    match numeric {
        Some(v) if v >= min && v <= max => Ok(v),
        Some(v) => Err(CronParseError(format!(
            "value {v} out of range [{min},{max}]"
        ))),
        None => Err(CronParseError(format!("unrecognised token '{t}'"))),
    }
}

fn names_index_to_value(idx: usize, _names: &[&str], min: u32) -> u32 {
    // Month names start at 1, weekday names at 0.
    if min == 1 {
        idx as u32 + 1
    } else {
        idx as u32
    }
    .min(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn parses_six_field_quartz() {
        assert!(CronExpression::parse("0 0 8 * * ?").is_ok());
        assert!(CronExpression::parse("0 0/30 * * * ?").is_ok());
        assert!(CronExpression::parse("0 0 9 * * MON-FRI").is_ok());
        assert!(CronExpression::parse("0 0 0 1 * ?").is_ok());
    }

    #[test]
    fn five_field_defaults_seconds_to_zero() {
        let c = CronExpression::parse("0 8 * * *").unwrap();
        assert!(c.matches(at(2026, 9, 18, 8, 0, 0)));
        assert!(!c.matches(at(2026, 9, 18, 8, 0, 1)));
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(CronExpression::parse("").is_err());
        assert!(CronExpression::parse("* *").is_err());
        assert!(CronExpression::parse("0 0 99 * * ?").is_err());
        assert!(CronExpression::parse("0 0 8 * * */0").is_err());
    }

    #[test]
    fn daily_at_eight() {
        let c = CronExpression::parse("0 0 8 * * ?").unwrap();
        assert!(c.matches(at(2026, 9, 18, 8, 0, 0)));
        assert!(!c.matches(at(2026, 9, 18, 9, 0, 0)));
        let next = c.next_after(at(2026, 9, 18, 9, 0, 0)).unwrap();
        assert_eq!(next, at(2026, 9, 19, 8, 0, 0));
    }

    #[test]
    fn every_thirty_minutes() {
        let c = CronExpression::parse("0 0/30 * * * ?").unwrap();
        assert!(c.matches(at(2026, 9, 18, 10, 0, 0)));
        assert!(c.matches(at(2026, 9, 18, 10, 30, 0)));
        assert!(!c.matches(at(2026, 9, 18, 10, 15, 0)));
    }

    #[test]
    fn weekday_range() {
        let c = CronExpression::parse("0 0 9 * * MON-FRI").unwrap();
        // 2026-09-18 is a Friday.
        assert!(c.matches(at(2026, 9, 18, 9, 0, 0)));
        // 2026-09-19 is a Saturday.
        assert!(!c.matches(at(2026, 9, 19, 9, 0, 0)));
    }

    #[test]
    fn first_of_month() {
        let c = CronExpression::parse("0 0 0 1 * ?").unwrap();
        assert!(c.matches(at(2026, 10, 1, 0, 0, 0)));
        assert!(!c.matches(at(2026, 10, 2, 0, 0, 0)));
    }

    #[test]
    fn named_months() {
        let c = CronExpression::parse("0 0 0 1 JAN *").unwrap();
        assert!(c.matches(at(2026, 1, 1, 0, 0, 0)));
        assert!(!c.matches(at(2026, 2, 1, 0, 0, 0)));
    }
}
