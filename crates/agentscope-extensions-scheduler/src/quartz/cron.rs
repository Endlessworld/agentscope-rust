//! Cron expression parser reproducing the `org.quartz.CronExpression`
//! semantics the Java scheduler relies on.
//!
//! Field layout: `sec min hour dom month dow [year]` (Quartz). The 5-field Unix
//! form (`min hour dom month dow`, seconds default to `0`) is accepted as a
//! Rust-only convenience — Quartz itself requires 6 fields.
//!
//! Supported field tokens: `*`, `?`, `N`, `a-b`, `a-b/step`, `*/step`, `a,b,c`,
//! month/weekday names (`JAN-DEC`, `SUN-SAT`) and the Quartz day extensions
//! `L`, `L-n`, `LW`, `nW` (day-of-month) plus `L`, `nL`, `n#k` (day-of-week).
//!
//! Quartz-compatible details enforced here:
//! * Weekdays are numbered **1 = SUN … 7 = SAT**; `0` is rejected. Unix cron's
//!   0-based numbering is deliberately *not* accepted, because it would shift
//!   every weekday by one day without any error.
//! * `?` is only accepted in the two day fields (`'?' can only be specified for
//!   Day-of-Month or Day-of-Week`).
//! * Quartz refuses to combine the two day selectors: exactly one of
//!   day-of-month / day-of-week must be `?` (`*` counts as "specified").
//!   Expressions that set both are rejected at parse time instead of silently
//!   firing on the union of both.
//! * A bare `a/step` expands through the field maximum even when `step == 1`
//!   (Quartz `addToSet(val, end=-1, incr)`), so `0/1` really means *every* value.
//! * The optional seventh (`year`) field is supported, `1970-2199` as in the
//!   Quartz 2.3.2 javadoc.

use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike,
    Weekday,
};
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

/// Day-of-month rule, mirroring Quartz's `dayOfMSpec` plus the `L`/`W` flags.
#[derive(Debug, Clone)]
enum DayOfMonthRule {
    /// Plain value list (`1`, `1-15`, `*/2`, `1,15,30`).
    Values(Field),
    /// `L` — last day of the month.
    Last,
    /// `L-n` — `n` days before the last day of the month.
    LastOffset(u32),
    /// `LW` — last weekday (Mon-Fri) of the month.
    LastWeekday,
    /// `nW` — weekday nearest to the `n`-th, never leaving the month.
    NearestWeekday(u32),
}

impl DayOfMonthRule {
    fn matches(&self, date: NaiveDate) -> bool {
        let (year, month, day) = (date.year(), date.month(), date.day());
        match self {
            Self::Values(field) => field.matches(day),
            Self::Last => days_in_month(year, month) == Some(day),
            Self::LastOffset(offset) => {
                matches!(days_in_month(year, month), Some(last) if last.checked_sub(*offset) == Some(day))
            }
            Self::LastWeekday => last_weekday(year, month) == Some(day),
            Self::NearestWeekday(n) => match days_in_month(year, month) {
                // Quartz: a `W` day that does not exist in the month never fires.
                Some(last) if *n <= last => nearest_weekday(year, month, *n, last) == Some(day),
                _ => false,
            },
        }
    }
}

/// Day-of-week rule, mirroring Quartz's `dayOfWSpec` plus the `L`/`#` flags.
#[derive(Debug, Clone)]
enum DayOfWeekRule {
    /// Plain value list (`MON-FRI`, `1,3,5`).
    Values(Field),
    /// `nL` — last `n`-th weekday of the month (e.g. `6L` = last Friday).
    LastInMonth(u32),
    /// `n#k` — `k`-th `n`-th weekday of the month (e.g. `6#3`).
    NthInMonth(u32, u32),
}

impl DayOfWeekRule {
    fn matches(&self, date: NaiveDate) -> bool {
        // chrono: Sunday = 0; Quartz: Sunday = 1.
        let dow = date.weekday().num_days_from_sunday() + 1;
        match self {
            Self::Values(field) => field.matches(dow),
            Self::LastInMonth(day) => {
                if dow != *day {
                    return false;
                }
                matches!(days_in_month(date.year(), date.month()), Some(last) if date.day() + 7 > last)
            }
            Self::NthInMonth(day, nth) => dow == *day && (date.day() - 1) / 7 + 1 == *nth,
        }
    }
}

/// Which of the two day fields governs the trigger. Quartz rejects expressions
/// that specify both.
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

/// Quartz `CronExpression.MIN_YEAR` / `MAX_YEAR` (javadoc range).
const MIN_YEAR: u32 = 1970;
const MAX_YEAR: u32 = 2199;

/// A parsed cron expression.
#[derive(Debug, Clone)]
pub struct CronExpression {
    seconds: Field,
    minutes: Field,
    hours: Field,
    months: Field,
    days_of_month: DayOfMonthRule,
    days_of_week: DayOfWeekRule,
    day_selector: DaySelector,
    /// Optional seventh field (`year`); unrestricted when absent.
    years: Option<Field>,
}

impl CronExpression {
    /// Parses a cron expression.
    ///
    /// Accepts 6 fields (`sec min hour dom month dow`), 7 fields
    /// (`sec min hour dom month dow year`) or 5 fields
    /// (`min hour dom month dow`, seconds default to `0`).
    pub fn parse(expr: &str) -> Result<Self, CronParseError> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(CronParseError("expression is empty".into()));
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let (sec, min, hour, dom, month, dow, year, six_field) = match parts.len() {
            5 => (
                "0", parts[0], parts[1], parts[2], parts[3], parts[4], None, false,
            ),
            6 => (
                parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], None, true,
            ),
            7 => (
                parts[0],
                parts[1],
                parts[2],
                parts[3],
                parts[4],
                parts[5],
                Some(parts[6]),
                true,
            ),
            n => {
                return Err(CronParseError(format!(
                    "expected 5, 6 or 7 fields, got {n}"
                )))
            }
        };

        // Field values are parsed first so that range/step errors (which Quartz
        // reports while walking the fields) take precedence over the day-field
        // conflict check below.
        let seconds = Field {
            values: parse_field(sec, 0, 59, &[], false)?,
        };
        let minutes = Field {
            values: parse_field(min, 0, 59, &[], false)?,
        };
        let hours = Field {
            values: parse_field(hour, 0, 23, &[], false)?,
        };
        let months = Field {
            values: parse_field(month, 1, 12, &MONTH_NAMES, false)?,
        };
        let years = match year {
            Some(raw) => Some(Field {
                values: parse_field(raw, MIN_YEAR, MAX_YEAR, &[], false)?,
            }),
            None => None,
        };
        // Quartz weekday numbering: 1 = SUN … 7 = SAT (0 is out of range).
        let days_of_month = parse_day_of_month(dom)?;
        let days_of_week = parse_day_of_week(dow)?;
        let day_selector = resolve_day_selector(dom, dow, six_field)?;

        Ok(Self {
            seconds,
            minutes,
            hours,
            months,
            days_of_month,
            days_of_week,
            day_selector,
            years,
        })
    }

    /// Returns true when `now` satisfies this expression's field constraints.
    ///
    /// All fields are evaluated on the **wall clock of `now`'s time zone**, which
    /// is how Quartz applies `CronTrigger`'s `TimeZone`.
    pub fn matches<Tz: TimeZone>(&self, now: DateTime<Tz>) -> bool {
        self.matches_naive(now.naive_local())
    }

    fn matches_naive(&self, now: NaiveDateTime) -> bool {
        if !self.year_matches(now.year()) {
            return false;
        }
        if !self.seconds.matches(now.second()) || !self.minutes.matches(now.minute()) {
            return false;
        }
        if !self.hours.matches(now.hour()) || !self.months.matches(now.month()) {
            return false;
        }
        self.day_matches(now.date())
    }

    /// The year constraints, if the seventh field was supplied.
    pub fn years(&self) -> Option<&[u32]> {
        self.years.as_ref().map(|f| f.values.as_slice())
    }

    fn year_matches(&self, year: i32) -> bool {
        match &self.years {
            None => true,
            Some(field) => {
                year >= MIN_YEAR as i32 && year <= MAX_YEAR as i32 && field.matches(year as u32)
            }
        }
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        match self.day_selector {
            DaySelector::Any => true,
            DaySelector::DayOfMonth => self.days_of_month.matches(date),
            DaySelector::DayOfWeek => self.days_of_week.matches(date),
        }
    }

    /// Computes the next fire time strictly after `from`.
    ///
    /// This is the equivalent of Quartz `CronExpression#getNextValidTimeAfter`.
    /// Field-wise skipping keeps the search cheap (at most a few thousand
    /// iterations, dominated by day-level jumps) while remaining exact for the
    /// `L` / `W` / `#` day rules.
    pub fn next_after<Tz: TimeZone>(&self, from: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = from.timezone();
        let start = from.naive_local();
        let start = start.with_nanosecond(0).unwrap_or(start);
        let mut cursor = start + Duration::seconds(1);
        // Generous bound: day-level skipping means one iteration per skipped day
        // plus a handful per candidate minute.
        let budget: u32 = 4 * 366 * 24 * 60;
        for _ in 0..budget {
            let year = cursor.year();
            if year > MAX_YEAR as i32 {
                return None;
            }
            if !self.year_matches(year) {
                cursor = NaiveDate::from_ymd_opt(year + 1, 1, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.months.matches(cursor.month()) {
                cursor = next_month_start(cursor)?;
                continue;
            }
            if !self.day_matches(cursor.date()) {
                cursor = cursor.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hours.matches(cursor.hour()) {
                cursor = next_hour_start(cursor)?;
                continue;
            }
            if !self.minutes.matches(cursor.minute()) {
                cursor = next_minute_start(cursor)?;
                continue;
            }
            if !self.seconds.matches(cursor.second()) {
                cursor += Duration::seconds(1);
                continue;
            }
            match tz.from_local_datetime(&cursor) {
                LocalResult::Single(dt) => return Some(dt),
                // Ambiguous wall clock (DST fall-back): take the earlier instant.
                LocalResult::Ambiguous(earliest, _) => return Some(earliest),
                // Non-existent wall clock (DST spring-forward): keep searching.
                LocalResult::None => {
                    cursor += Duration::seconds(1);
                }
            }
        }
        None
    }
}

fn next_month_start(dt: NaiveDateTime) -> Option<NaiveDateTime> {
    let (year, month) = (dt.year(), dt.month());
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)?.and_hms_opt(0, 0, 0)
}

fn next_hour_start(dt: NaiveDateTime) -> Option<NaiveDateTime> {
    (dt + Duration::hours(1))
        .with_minute(0)?
        .with_second(0)?
        .with_nanosecond(0)
}

fn next_minute_start(dt: NaiveDateTime) -> Option<NaiveDateTime> {
    (dt + Duration::minutes(1))
        .with_second(0)?
        .with_nanosecond(0)
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(first_of_next.pred_opt()?.day())
}

fn weekday_of(year: i32, month: u32, day: u32) -> Option<Weekday> {
    NaiveDate::from_ymd_opt(year, month, day).map(|date| date.weekday())
}

/// Quartz `nW`: the weekday nearest to the `n`-th, never crossing the month
/// boundary (`1W` on a Saturday fires on the 3rd, `31W` on a Sunday fires on the
/// 29th).
fn nearest_weekday(year: i32, month: u32, day: u32, last_day: u32) -> Option<u32> {
    match weekday_of(year, month, day)? {
        Weekday::Sat => Some(if day == 1 { day + 2 } else { day - 1 }),
        Weekday::Sun => Some(if day == last_day {
            day.saturating_sub(2)
        } else {
            day + 1
        }),
        _ => Some(day),
    }
}

/// Quartz `LW`: the last Mon-Fri day of the month.
fn last_weekday(year: i32, month: u32) -> Option<u32> {
    let last_day = days_in_month(year, month)?;
    match weekday_of(year, month, last_day)? {
        Weekday::Sat => Some(last_day - 1),
        Weekday::Sun => Some(last_day - 2),
        _ => Some(last_day),
    }
}

fn parse_day_of_month(raw: &str) -> Result<DayOfMonthRule, CronParseError> {
    let field = raw.trim();
    let upper = field.to_ascii_uppercase();
    // `?` means "not specified"; the rule is then ignored by `DaySelector`. Keep
    // an unrestricted placeholder so the struct stays total.
    if upper == "?" {
        return Ok(DayOfMonthRule::Values(Field {
            values: parse_field("*", 1, 31, &[], false)?,
        }));
    }
    if upper.contains('L') || upper.contains('W') {
        if upper.contains(',') {
            return Err(CronParseError(
                "'L' and 'W' cannot be combined with other day-of-month values".into(),
            ));
        }
        return parse_day_of_month_special(&upper).map(Ok::<_, CronParseError>)?;
    }
    Ok(DayOfMonthRule::Values(Field {
        values: parse_field(field, 1, 31, &[], false)?,
    }))
}

fn parse_day_of_month_special(upper: &str) -> Result<DayOfMonthRule, CronParseError> {
    if upper == "L" {
        return Ok(DayOfMonthRule::Last);
    }
    if upper == "LW" || upper == "WL" {
        return Ok(DayOfMonthRule::LastWeekday);
    }
    if let Some(rest) = upper.strip_prefix("L-") {
        let offset: u32 = rest
            .parse()
            .map_err(|_| CronParseError(format!("invalid last-day offset in '{upper}'")))?;
        if offset == 0 || offset > 30 {
            return Err(CronParseError(format!(
                "last-day offset {offset} out of range [1,30]"
            )));
        }
        return Ok(DayOfMonthRule::LastOffset(offset));
    }
    if let Some(rest) = upper.strip_suffix('W') {
        let day: u32 = rest
            .parse()
            .map_err(|_| CronParseError(format!("invalid nearest-weekday token '{upper}'")))?;
        if !(1..=31).contains(&day) {
            return Err(CronParseError(format!(
                "nearest-weekday day {day} out of range [1,31]"
            )));
        }
        return Ok(DayOfMonthRule::NearestWeekday(day));
    }
    Err(CronParseError(format!(
        "unrecognised day-of-month token '{upper}'"
    )))
}

fn parse_day_of_week(raw: &str) -> Result<DayOfWeekRule, CronParseError> {
    let field = raw.trim();
    let upper = field.to_ascii_uppercase();
    // `?` means "not specified"; ignored by `DaySelector`.
    if upper == "?" {
        return Ok(DayOfWeekRule::Values(Field {
            values: parse_field("*", 1, 7, &DOW_NAMES, false)?,
        }));
    }
    if upper.contains('#') {
        if upper.contains(',') {
            return Err(CronParseError(
                "'#' cannot be combined with other day-of-week values".into(),
            ));
        }
        let (day_part, nth_part) = upper
            .split_once('#')
            .ok_or_else(|| CronParseError(format!("invalid nth-weekday token '{upper}'")))?;
        let day = resolve(day_part, &DOW_NAMES, 1, 7)?;
        let nth: u32 = nth_part
            .parse()
            .map_err(|_| CronParseError(format!("invalid '#' ordinal in '{upper}'")))?;
        if !(1..=5).contains(&nth) {
            return Err(CronParseError(format!(
                "the '#' day-of-week ordinal {nth} is out of range [1,5]"
            )));
        }
        return Ok(DayOfWeekRule::NthInMonth(day, nth));
    }
    if upper.contains('L') {
        // Quartz: a bare `L` in day-of-week simply means 7 (= SAT).
        if upper == "L" {
            return Ok(DayOfWeekRule::Values(Field { values: vec![7] }));
        }
        if upper.contains(',') {
            return Err(CronParseError(
                "'L' cannot be combined with other day-of-week values".into(),
            ));
        }
        let day_part = upper
            .strip_suffix('L')
            .ok_or_else(|| CronParseError(format!("invalid last-weekday token '{upper}'")))?;
        let day = resolve(day_part, &DOW_NAMES, 1, 7)?;
        return Ok(DayOfWeekRule::LastInMonth(day));
    }
    Ok(DayOfWeekRule::Values(Field {
        values: parse_field(field, 1, 7, &DOW_NAMES, false)?,
    }))
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
    allow_question: bool,
) -> Result<Vec<u32>, CronParseError> {
    let mut out: Vec<u32> = Vec::new();
    for token in field.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err(CronParseError("empty field segment".into()));
        }
        if token.contains('?') && !allow_question {
            return Err(CronParseError(
                "'?' can only be specified for Day-of-Month or Day-of-Week".into(),
            ));
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
                (r.trim(), Some(step))
            }
            None => (token, None),
        };

        let (start, end) = if range_part == "*" || range_part == "?" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (resolve(a, names, min, max)?, resolve(b, names, min, max)?)
        } else {
            let v = resolve(range_part, names, min, max)?;
            // Quartz `addToSet(val, end=-1, incr)`: a bare `a/step` runs from `a`
            // through the field maximum, *including* `step == 1` (`0/1` is every
            // minute). Without `/` it is the single value `a`.
            if step.is_some() {
                (v, max)
            } else {
                (v, v)
            }
        };

        if start > end {
            return Err(CronParseError(format!("range start {start} > end {end}")));
        }

        let step = step.unwrap_or(1);
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
        assert!(
            err.to_string().contains("both a day-of-week"),
            "err was {err}"
        );
        assert!(CronExpression::parse("0 0 0 1 * MON").is_err());
        assert!(CronExpression::parse("0 0 0 * * *").is_err());

        // The Quartz spellings are accepted.
        assert!(CronExpression::parse("0 0 9 ? * MON-FRI").is_ok());
        assert!(CronExpression::parse("0 0 0 1 * ?").is_ok());
    }

    // ---- Quartz day extensions: L / W / LW / nW / nL / n#k ----------------

    #[test]
    fn last_day_of_month() {
        // `L` in day-of-month (hour must be a real hour: `0 0 0 L * ?`).
        let c = CronExpression::parse("0 0 0 L * ?").unwrap();
        // September 2026 has 30 days, October 31.
        assert!(c.matches(at(2026, 9, 30, 0, 0, 0)));
        assert!(!c.matches(at(2026, 9, 29, 0, 0, 0)));
        assert!(c.matches(at(2026, 10, 31, 0, 0, 0)));
        // Leap February fires on the 29th; a non-leap February on the 28th.
        assert!(c.matches(at(2028, 2, 29, 0, 0, 0)));
        assert!(!c.matches(at(2028, 2, 28, 0, 0, 0)));
        assert!(c.matches(at(2027, 2, 28, 0, 0, 0)));

        // `getNextValidTimeAfter` analogue.
        assert_eq!(
            c.next_after(at(2026, 9, 15, 12, 0, 0)).unwrap(),
            at(2026, 9, 30, 0, 0, 0)
        );
        assert_eq!(
            c.next_after(at(2026, 9, 30, 0, 0, 0)).unwrap(),
            at(2026, 10, 31, 0, 0, 0)
        );
    }

    #[test]
    fn last_day_offset() {
        // `L-3` in September 2026 (30 days) is the 27th.
        let c = CronExpression::parse("0 0 0 L-3 * ?").unwrap();
        assert!(c.matches(at(2026, 9, 27, 0, 0, 0)));
        assert!(!c.matches(at(2026, 9, 26, 0, 0, 0)));
        assert!(!c.matches(at(2026, 9, 30, 0, 0, 0)));
        // August 2026 has 31 days -> the 28th.
        assert!(c.matches(at(2026, 8, 28, 0, 0, 0)));
    }

    #[test]
    fn last_weekday_of_month() {
        // 2026-10-31 is a Saturday, so `LW` fires on Friday the 30th.
        let c = CronExpression::parse("0 0 0 LW * ?").unwrap();
        assert!(c.matches(at(2026, 10, 30, 0, 0, 0)));
        assert!(!c.matches(at(2026, 10, 31, 0, 0, 0)));
        assert!(!c.matches(at(2026, 10, 29, 0, 0, 0)));
        // 2026-09-30 is a Wednesday: the last day is already a weekday.
        assert!(c.matches(at(2026, 9, 30, 0, 0, 0)));
        // 2026-05-31 is a Sunday -> Friday the 29th.
        assert!(c.matches(at(2026, 5, 29, 0, 0, 0)));
        assert!(!c.matches(at(2026, 5, 31, 0, 0, 0)));
    }

    #[test]
    fn nearest_weekday_of_month() {
        // 2026-10-15 is a Thursday.
        let c = CronExpression::parse("0 0 0 15W * ?").unwrap();
        assert!(c.matches(at(2026, 10, 15, 0, 0, 0)));
        assert!(!c.matches(at(2026, 10, 14, 0, 0, 0)));

        // 2026-08-15 is a Saturday -> Friday the 14th.
        assert!(c.matches(at(2026, 8, 14, 0, 0, 0)));
        assert!(!c.matches(at(2026, 8, 15, 0, 0, 0)));

        // 2026-11-15 is a Sunday -> Monday the 16th.
        assert!(c.matches(at(2026, 11, 16, 0, 0, 0)));
        assert!(!c.matches(at(2026, 11, 15, 0, 0, 0)));
        assert!(!c.matches(at(2026, 11, 13, 0, 0, 0)));
    }

    #[test]
    fn nearest_weekday_does_not_cross_month_boundary() {
        // 2026-08-01 is a Saturday: `1W` must fire on Monday the 3rd, not on the
        // previous month's Friday.
        let c = CronExpression::parse("0 0 0 1W * ?").unwrap();
        assert!(c.matches(at(2026, 8, 3, 0, 0, 0)));
        assert!(!c.matches(at(2026, 8, 1, 0, 0, 0)));
        assert!(!c.matches(at(2026, 7, 31, 0, 0, 0)));

        // 2026-05-31 is a Sunday: `31W` fires on Friday the 29th.
        let c = CronExpression::parse("0 0 0 31W * ?").unwrap();
        assert!(c.matches(at(2026, 5, 29, 0, 0, 0)));
        assert!(!c.matches(at(2026, 5, 31, 0, 0, 0)));
    }

    #[test]
    fn nearest_weekday_never_fires_when_day_is_missing() {
        // April 2026 only has 30 days, so `31W` never fires that month.
        let c = CronExpression::parse("0 0 0 31W * ?").unwrap();
        assert!(!c.matches(at(2026, 4, 30, 0, 0, 0)));
        assert!(!c.matches(at(2026, 4, 29, 0, 0, 0)));
        // February in a non-leap year.
        assert!(!c.matches(at(2027, 2, 28, 0, 0, 0)));
    }

    #[test]
    fn last_day_of_week_in_month() {
        // `6L` = last Friday. September 2026 Fridays: 4, 11, 18, 25.
        let c = CronExpression::parse("0 0 12 ? * 6L").unwrap();
        assert!(c.matches(at(2026, 9, 25, 12, 0, 0)));
        assert!(!c.matches(at(2026, 9, 18, 12, 0, 0)));
        assert!(!c.matches(at(2026, 9, 26, 12, 0, 0)));
        // Named form.
        let named = CronExpression::parse("0 0 12 ? * FRIL").unwrap();
        assert!(named.matches(at(2026, 9, 25, 12, 0, 0)));
    }

    #[test]
    fn nth_day_of_week_in_month() {
        // `6#3` = third Friday. September 2026 Fridays: 4, 11, 18, 25.
        let c = CronExpression::parse("0 0 12 ? * 6#3").unwrap();
        assert!(c.matches(at(2026, 9, 18, 12, 0, 0)));
        assert!(!c.matches(at(2026, 9, 11, 12, 0, 0)));
        assert!(!c.matches(at(2026, 9, 25, 12, 0, 0)));

        // Named weekday with an ordinal (`FRI#2`) is accepted.
        assert!(CronExpression::parse("0 0 12 ? * FRI#2").is_ok());
        assert!(CronExpression::parse("0 0 12 ? * 6#6").is_err());
        assert!(CronExpression::parse("0 0 12 ? * 6#0").is_err());
    }

    #[test]
    fn nth_day_of_week_skips_months_without_it() {
        // September 2026 has only four Fridays, so `6#5` never fires.
        let c = CronExpression::parse("0 0 12 ? * 6#5").unwrap();
        assert!(!c.matches(at(2026, 9, 25, 12, 0, 0)));
        // October 2026 has five Fridays: 2, 9, 16, 23, 30.
        assert!(c.matches(at(2026, 10, 30, 12, 0, 0)));
    }

    #[test]
    fn bare_last_day_of_week_means_saturday() {
        // Quartz: `L` in day-of-week = 7 = SAT.
        let c = CronExpression::parse("0 0 9 ? * L").unwrap();
        assert!(c.matches(at(2026, 9, 19, 9, 0, 0)));
        assert!(!c.matches(at(2026, 9, 20, 9, 0, 0)));
    }

    #[test]
    fn rejects_malformed_day_extensions() {
        assert!(CronExpression::parse("0 0 0 L-0 * ?").is_err());
        assert!(CronExpression::parse("0 0 0 L-31 * ?").is_err());
        assert!(CronExpression::parse("0 0 0 32W * ?").is_err());
        assert!(CronExpression::parse("0 0 0 W * ?").is_err());
        assert!(CronExpression::parse("0 0 0 1,L * ?").is_err());
        assert!(CronExpression::parse("0 0 0 ? * 6#3,1").is_err());
        // `L`/`W` in day-of-month still requires `?` in day-of-week (Quartz rule).
        assert!(CronExpression::parse("0 0 0 L * MON").is_err());
        assert!(CronExpression::parse("0 0 0 L * ?").is_ok());
        assert!(CronExpression::parse("0 0 0 ? * 6#3").is_ok());
    }

    // ---- year field -------------------------------------------------------

    #[test]
    fn year_field_limits_firing() {
        let c = CronExpression::parse("0 0 0 1 1 ? 2027").unwrap();
        assert_eq!(c.years(), Some([2027u32].as_slice()));
        assert!(c.matches(at(2027, 1, 1, 0, 0, 0)));
        assert!(!c.matches(at(2026, 1, 1, 0, 0, 0)));
        assert!(!c.matches(at(2028, 1, 1, 0, 0, 0)));
        assert_eq!(
            c.next_after(at(2026, 10, 28, 0, 0, 0)).unwrap(),
            at(2027, 1, 1, 0, 0, 0)
        );
        // No further occurrence after the expression's last year.
        assert_eq!(c.next_after(at(2027, 6, 1, 0, 0, 0)), None);
    }

    #[test]
    fn year_field_range_matches_quartz() {
        assert!(CronExpression::parse("0 0 0 1 1 ? 1970").is_ok());
        assert!(CronExpression::parse("0 0 0 1 1 ? 2199").is_ok());
        assert!(CronExpression::parse("0 0 0 1 1 ? 1969").is_err());
        assert!(CronExpression::parse("0 0 0 1 1 ? 2200").is_err());
        assert!(CronExpression::parse("0 0 0 1 1 ? 2026-2028").is_ok());
        assert!(CronExpression::parse("0 0 0 1 1 ? */2").is_ok());
    }

    #[test]
    fn year_field_next_after_skips_years() {
        let c = CronExpression::parse("0 0 0 1 1 ? 2030").unwrap();
        assert_eq!(
            c.next_after(at(2026, 10, 28, 0, 0, 0)).unwrap(),
            at(2030, 1, 1, 0, 0, 0)
        );
    }

    // ---- other Quartz parsing details -------------------------------------

    #[test]
    fn step_without_range_expands_even_for_step_one() {
        // Quartz `addToSet(val, end=-1, incr=1)` -> 0/1 is every minute.
        let c = CronExpression::parse("0 0/1 * * * ?").unwrap();
        assert!(c.matches(at(2026, 9, 18, 10, 5, 0)));
        assert!(c.matches(at(2026, 9, 18, 10, 59, 0)));
        // `*/1` behaves the same.
        let c = CronExpression::parse("0 */1 * * * ?").unwrap();
        assert!(c.matches(at(2026, 9, 18, 10, 37, 0)));
        // A bare value stays a single value.
        let c = CronExpression::parse("0 5 * * * ?").unwrap();
        assert!(c.matches(at(2026, 9, 18, 10, 5, 0)));
        assert!(!c.matches(at(2026, 9, 18, 10, 6, 0)));
    }

    #[test]
    fn question_mark_only_allowed_in_day_fields() {
        assert!(CronExpression::parse("0 ? 8 * * ?").is_err());
        assert!(CronExpression::parse("? 0 8 * * ?").is_err());
        assert!(CronExpression::parse("0 0 ? * * ?").is_err());
        // The Quartz spellings that use `?` in a day field are accepted.
        assert!(CronExpression::parse("0 0 8 1 * ?").is_ok());
        assert!(CronExpression::parse("0 0 8 ? * MON").is_ok());
        // Quartz also refuses `? ?` (neither day field specified).
        assert!(CronExpression::parse("0 0 8 ? * ?").is_err());
    }

    #[test]
    fn next_after_is_strictly_after() {
        let c = CronExpression::parse("0 0 8 * * ?").unwrap();
        assert_eq!(
            c.next_after(at(2026, 9, 18, 8, 0, 0)).unwrap(),
            at(2026, 9, 19, 8, 0, 0)
        );
    }
}
