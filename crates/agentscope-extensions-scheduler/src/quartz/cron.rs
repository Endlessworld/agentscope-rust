//! Cron expression parser reproducing the `org.quartz.CronExpression`
//! semantics the Java scheduler relies on.
//!
//! Field layout: `sec min hour dom month dow` (6 fields, Quartz). The 5-field
//! Unix form (`min hour dom month dow`, seconds default to `0`) is accepted as a
//! Rust-only convenience — Quartz itself requires 6 fields.
//!
//! Supported field tokens: `*`, `?`, `N`, `a-b`, `a-b/step`, `*/step`, `a,b,c`,
//! and month/weekday names (`JAN-DEC`, `SUN-SAT`).
//!
//! Quartz-compatible details enforced here:
//! * Weekdays are numbered **1 = SUN … 7 = SAT**; `0` is rejected. Unix cron's
//!   0-based numbering is deliberately *not* accepted, because it would shift
//!   every weekday by one day without any error.
//! * Quartz refuses to combine the two day selectors: exactly one of
//!   day-of-month / day-of-week must be `?` (`*` counts as "specified").
//!   Expressions that set both are rejected at parse time instead of silently
//!   firing on the union of both.
//!
//! Not implemented yet (see `docs/extensions/SCHEDULER__GAP_TRACKER.md` §6):
//! `L` / `W` / `LW` / `#` and the optional year field.

use chrono::{DateTime, Datelike, TimeZone, Timelike};
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
    /// Which of the two day fields governs the trigger. Quartz rejects
    /// expressions that specify both.
    day_selector: DaySelector,
}

/// Day-of-month / day-of-week selection mode, mirroring Quartz's
/// `dayOfMSpec` / `dayOfWSpec` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaySelector {
    /// Neither field restricts the day (5-field `* *`, i.e. every day).
    Any,
    /// Day-of-month governs; day-of-week was `?`.
    DayOfMonth,
    /// Day-of-week governs; day-of-month was `?`.
    DayOfWeek,
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
        let (sec, min, hour, dom, month, dow, six_field) = match parts.len() {
            5 => ("0", parts[0], parts[1], parts[2], parts[3], parts[4], false),
            6 => (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], true),
            n => {
                return Err(CronParseError(format!(
                    "expected 5 or 6 fields, got {n}"
                )))
            }
        };

        // Field values are parsed first so that range/step errors (which Quartz
        // reports while walking the fields) take precedence over the day-field
        // conflict check below.
        let seconds = Field { values: parse_field(sec, 0, 59, &[])? };
        let minutes = Field { values: parse_field(min, 0, 59, &[])? };
        let hours = Field { values: parse_field(hour, 0, 23, &[])? };
        let days_of_month = Field { values: parse_field(dom, 1, 31, &[])? };
        let months = Field { values: parse_field(month, 1, 12, &MONTH_NAMES)? };
        // Quartz weekday numbering: 1 = SUN … 7 = SAT (0 is out of range).
        let days_of_week = Field { values: parse_field(dow, 1, 7, &DOW_NAMES)? };
        let day_selector = resolve_day_selector(dom, dow, six_field)?;

        Ok(Self {
            seconds,
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            day_selector,
        })
    }

    /// Returns true when `now` satisfies this expression's field constraints.
    ///
    /// All fields are evaluated on the **wall clock of `now`'s time zone**, which
    /// is how Quartz applies `CronTrigger`'s `TimeZone`.
    pub fn matches<Tz: TimeZone>(&self, now: DateTime<Tz>) -> bool {
        if !self.seconds.matches(now.second()) || !self.minutes.matches(now.minute()) {
            return false;
        }
        if !self.hours.matches(now.hour()) {
            return false;
        }
        if !self.months.matches(now.month()) {
            return false;
        }

        match self.day_selector {
            DaySelector::Any => true,
            DaySelector::DayOfMonth => self.days_of_month.matches(now.day()),
            DaySelector::DayOfWeek => {
                // chrono: Sunday = 0; Quartz: Sunday = 1.
                let dow = now.weekday().num_days_from_sunday() + 1;
                self.days_of_week.matches(dow)
            }
        }
    }

    /// Computes the next fire time strictly after `from`, scanning second by
    /// second for up to 4 years (bounded work).
    pub fn next_after<Tz: TimeZone>(&self, from: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let mut candidate = from + chrono::Duration::seconds(1);
        // Align to the next whole second.
        candidate = candidate.with_nanosecond(0).unwrap_or(candidate);
        // Scan up to 4 years of minutes (a leap year plus slack).
        let limit = 4 * 366 * 24 * 60;
        for _ in 0..limit {
            if self.matches(candidate.clone()) {
                return Some(candidate);
            }
            candidate += chrono::Duration::seconds(1);
        }
        None
    }
}

fn is_unspecified(field: &str) -> bool {
    field.trim() == "?"
}

/// True when the field is a bare `*` (an unrestricted wildcard).
fn is_wildcard(field: &str) -> bool {
    field.trim() == "*"
}

/// Quartz `CronExpression#buildExpression` refuses to combine the day-of-month
/// and day-of-week fields:
///
/// ```text
/// Support for specifying both a day-of-week AND a day-of-month parameter is not implemented.
/// ```
///
/// It treats every token other than `?` (including `*`) as "specified", so
/// `0 0 9 * * MON-FRI` is rejected — the correct spelling is
/// `0 0 9 ? * MON-FRI`. Rust used to fire on the union of both fields instead.
fn resolve_day_selector(
    dom: &str,
    dow: &str,
    six_field: bool,
) -> Result<DaySelector, CronParseError> {
    let dom_specified = !is_unspecified(dom);
    let dow_specified = !is_unspecified(dow);

    if six_field {
        // Quartz form: exactly one of the two fields must be `?`.
        if dom_specified == dow_specified {
            return Err(day_field_conflict());
        }
        return Ok(if dom_specified {
            DaySelector::DayOfMonth
        } else {
            DaySelector::DayOfWeek
        });
    }

    // 5-field form is a Rust-only extension, where `* *` conventionally means
    // "every day"; only reject the ambiguous case of two concrete day fields.
    // (`*` is a wildcard here, not a "specified" value as in the Quartz form.)
    let dom_restricted = dom_specified && !is_wildcard(dom);
    let dow_restricted = dow_specified && !is_wildcard(dow);
    match (dom_restricted, dow_restricted) {
        (true, true) => Err(day_field_conflict()),
        (true, false) => Ok(DaySelector::DayOfMonth),
        (false, true) => Ok(DaySelector::DayOfWeek),
        (false, false) => Ok(DaySelector::Any),
    }
}

fn day_field_conflict() -> CronParseError {
    CronParseError(
        "support for specifying both a day-of-week AND a day-of-month parameter is not \
         implemented"
            .into(),
    )
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
    // Quartz numbers months and weekdays from 1 (`SUN`=1, `JAN`=1); the
    // 0-based branch only applies to a future 0-based field.
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
    use chrono::{TimeZone, Utc};

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn parses_six_field_quartz() {
        assert!(CronExpression::parse("0 0 8 * * ?").is_ok());
        assert!(CronExpression::parse("0 0/30 * * * ?").is_ok());
        assert!(CronExpression::parse("0 0 9 ? * MON-FRI").is_ok());
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
        // Quartz requires `?` in day-of-month when day-of-week is restrictive.
        let c = CronExpression::parse("0 0 9 ? * MON-FRI").unwrap();
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
        let c = CronExpression::parse("0 0 0 1 JAN ?").unwrap();
        assert!(c.matches(at(2026, 1, 1, 0, 0, 0)));
        assert!(!c.matches(at(2026, 2, 1, 0, 0, 0)));
    }

    #[test]
    fn weekday_numbers_follow_quartz() {
        // Quartz: 1 = SUN … 7 = SAT. 2026-09-20 is a Sunday, 09-19 a Saturday and
        // 09-21 a Monday.
        let sunday = CronExpression::parse("0 0 9 ? * 1").unwrap();
        assert!(sunday.matches(at(2026, 9, 20, 9, 0, 0)));
        assert!(!sunday.matches(at(2026, 9, 21, 9, 0, 0)));
        assert!(!sunday.matches(at(2026, 9, 19, 9, 0, 0)));

        let saturday = CronExpression::parse("0 0 9 ? * 7").unwrap();
        assert!(saturday.matches(at(2026, 9, 19, 9, 0, 0)));
        assert!(!saturday.matches(at(2026, 9, 20, 9, 0, 0)));
    }

    #[test]
    fn weekday_names_follow_quartz() {
        let c = CronExpression::parse("0 0 9 ? * SUN").unwrap();
        assert!(c.matches(at(2026, 9, 20, 9, 0, 0)));
        assert!(!c.matches(at(2026, 9, 21, 9, 0, 0)));
    }

    #[test]
    fn rejects_zero_based_weekday_numbers() {
        // Quartz throws `Day-of-Week values must be between 1 and 7` for 0; it
        // must not be silently reinterpreted as a weekday.
        assert!(CronExpression::parse("0 0 9 ? * 0").is_err());
        assert!(CronExpression::parse("0 0 9 ? * 0-6").is_err());
        assert!(CronExpression::parse("0 0 9 ? * 1-8").is_err());
    }

    #[test]
    fn rejects_both_day_of_month_and_day_of_week() {
        // Quartz: `*` counts as specified, so exactly one of the two day fields
        // must be `?`. Previously Rust fired on the union of both.
        let err = CronExpression::parse("0 0 9 * * MON-FRI").unwrap_err();
        assert!(err.to_string().contains("both a day-of-week"), "err was {err}");
        assert!(CronExpression::parse("0 0 0 1 * MON").is_err());
        assert!(CronExpression::parse("0 0 0 * * *").is_err());

        // The Quartz spellings are accepted.
        assert!(CronExpression::parse("0 0 9 ? * MON-FRI").is_ok());
        assert!(CronExpression::parse("0 0 0 1 * ?").is_ok());
    }
}
