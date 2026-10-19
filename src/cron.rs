//! Dependency-free five-field cron expressions with Vixie-cron semantics.
//!
//! Supports `*`, lists, ranges, steps (`*/15`, `1-5/2`, `10/5`), month and weekday
//! names, `7` as Sunday, and the `@hourly` / `@daily` / `@weekly` / `@monthly` /
//! `@yearly` macros. When both day-of-month and day-of-week are restricted a day
//! matches if *either* does, as in classic cron.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, LocalResult, NaiveTime, TimeDelta, TimeZone, Timelike};

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// How far ahead `next_after` searches. Covers leap days on a given weekday.
const SEARCH_DAYS: u32 = 366 * 30;

#[derive(Clone, PartialEq, Eq)]
pub struct Schedule {
    source: String,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    days_star: bool,
    weekdays_star: bool,
}

impl Schedule {
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The first matching minute strictly after `after`, in `after`'s time zone.
    ///
    /// Wall-clock times skipped by a DST jump are skipped; times repeated by a
    /// DST fall-back fire once, on their first occurrence.
    pub fn next_after<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = after.timezone();
        let start = after.naive_local().with_second(0)?.with_nanosecond(0)? + TimeDelta::minutes(1);
        let first_day = start.date();
        let mut day = first_day;

        for _ in 0..SEARCH_DAYS {
            if self.day_matches(day) {
                let floor = if day == first_day {
                    start.time()
                } else {
                    NaiveTime::MIN
                };
                for hour in floor.hour()..24 {
                    if !bit(self.hours, hour) {
                        continue;
                    }
                    let first_minute = if day == first_day && hour == floor.hour() {
                        floor.minute()
                    } else {
                        0
                    };
                    for minute in first_minute..60 {
                        if !bit(self.minutes, minute) {
                            continue;
                        }
                        let naive = day.and_hms_opt(hour, minute, 0)?;
                        match tz.from_local_datetime(&naive) {
                            LocalResult::Single(dt) if dt > *after => return Some(dt),
                            LocalResult::Ambiguous(earliest, _) if earliest > *after => {
                                return Some(earliest);
                            }
                            _ => {}
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }

    fn day_matches(&self, day: chrono::NaiveDate) -> bool {
        if !bit(self.months, day.month()) {
            return false;
        }
        let dom = bit(self.days, day.day());
        let dow = bit(self.weekdays, day.weekday().num_days_from_sunday());
        if self.days_star || self.weekdays_star {
            dom && dow
        } else {
            dom || dow
        }
    }
}

fn bit(mask: u64, n: u32) -> bool {
    mask & (1 << n) != 0
}

impl FromStr for Schedule {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let source = input.trim();
        let expr = match source {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other if other.starts_with('@') => {
                return Err(format!("unsupported cron macro `{other}`"));
            }
            other => other,
        };
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [min, hour, dom, month, dow] = fields[..] else {
            return Err(format!(
                "cron expression `{source}` must have 5 fields (minute hour day-of-month month day-of-week), found {}",
                fields.len()
            ));
        };
        let field = |name: &str, raw: &str, lo: u32, hi: u32, names: &[&str], base: u32| {
            parse_field(raw, lo, hi, names, base)
                .map_err(|e| format!("cron `{source}`: {name} field: {e}"))
        };

        let mut weekdays = field("day-of-week", dow, 0, 7, &WEEKDAY_NAMES, 0)?;
        if bit(weekdays, 7) {
            weekdays = (weekdays | 1) & !(1 << 7);
        }
        Ok(Self {
            source: source.to_string(),
            minutes: field("minute", min, 0, 59, &[], 0)?,
            hours: field("hour", hour, 0, 23, &[], 0)?,
            days: field("day-of-month", dom, 1, 31, &[], 0)?,
            months: field("month", month, 1, 12, &MONTH_NAMES, 1)?,
            weekdays,
            days_star: dom.starts_with('*'),
            weekdays_star: dow.starts_with('*'),
        })
    }
}

fn parse_field(raw: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Result<u64, String> {
    let value = |token: &str| -> Result<u32, String> {
        if let Ok(n) = token.parse::<u32>() {
            return Ok(n);
        }
        names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(token))
            .map(|i| i as u32 + base)
            .ok_or_else(|| format!("invalid value `{token}`"))
    };

    let mut mask = 0u64;
    for part in raw.split(',') {
        if part.is_empty() {
            return Err(format!("empty list element in `{raw}`"));
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| format!("invalid step `{step}`"))?;
                if step == 0 {
                    return Err("step must be at least 1".into());
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // `10/5` means "from 10 to the end, every 5".
            if part.contains('/') { (v, hi) } else { (v, v) }
        };
        if start < lo || end > hi {
            return Err(format!("`{part}` is outside {lo}-{hi}"));
        }
        if start > end {
            return Err(format!("range `{range}` is reversed"));
        }
        let mut v = start;
        while v <= end {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok(mask)
}

impl<'de> serde::Deserialize<'de> for Schedule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl fmt::Debug for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Schedule({:?})", self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn next(expr: &str, after: &str) -> String {
        let schedule: Schedule = expr.parse().unwrap();
        schedule.next_after(&at(after)).unwrap().to_rfc3339()
    }

    #[test]
    fn daily_at_two() {
        assert_eq!(
            next("0 2 * * *", "2026-01-01T01:30:00Z"),
            "2026-01-01T02:00:00+00:00"
        );
        assert_eq!(
            next("0 2 * * *", "2026-01-01T02:00:00Z"),
            "2026-01-02T02:00:00+00:00"
        );
        assert_eq!(
            next("0 2 * * *", "2026-01-01T02:00:30Z"),
            "2026-01-02T02:00:00+00:00"
        );
    }

    #[test]
    fn steps_lists_and_ranges() {
        assert_eq!(
            next("*/15 * * * *", "2026-01-01T00:07:00Z"),
            "2026-01-01T00:15:00+00:00"
        );
        assert_eq!(
            next("5,35 9-17 * * *", "2026-01-01T17:36:00Z"),
            "2026-01-02T09:05:00+00:00"
        );
        assert_eq!(
            next("10/20 * * * *", "2026-01-01T00:31:00Z"),
            "2026-01-01T00:50:00+00:00"
        );
        assert_eq!(
            next("0 0-23/6 * * *", "2026-01-01T06:00:00Z"),
            "2026-01-01T12:00:00+00:00"
        );
    }

    #[test]
    fn weekdays_and_names() {
        // 2026-01-01 is a Thursday.
        assert_eq!(
            next("0 2 * * 0", "2026-01-01T00:00:00Z"),
            "2026-01-04T02:00:00+00:00"
        );
        assert_eq!(
            next("0 2 * * 7", "2026-01-01T00:00:00Z"),
            "2026-01-04T02:00:00+00:00"
        );
        assert_eq!(
            next("0 2 * * SUN", "2026-01-01T00:00:00Z"),
            "2026-01-04T02:00:00+00:00"
        );
        assert_eq!(
            next("0 0 1 jun *", "2026-01-01T00:00:00Z"),
            "2026-06-01T00:00:00+00:00"
        );
        assert_eq!(
            next("0 3 * * mon-fri", "2026-01-02T04:00:00Z"),
            "2026-01-05T03:00:00+00:00"
        );
    }

    #[test]
    fn day_of_month_or_day_of_week() {
        // Restricted in both fields: the 15th OR any Monday, whichever comes first.
        assert_eq!(
            next("0 0 15 * 1", "2026-01-01T00:00:00Z"),
            "2026-01-05T00:00:00+00:00"
        );
        assert_eq!(
            next("0 0 15 * 1", "2026-01-13T00:00:00Z"),
            "2026-01-15T00:00:00+00:00"
        );
        // A `*`-prefixed field makes both conditions required.
        // Days 1, 11, 21, 31 that are also Mondays: 2026-05-11 is the first.
        assert_eq!(
            next("0 0 */10 * 1", "2026-01-01T00:00:00Z"),
            "2026-05-11T00:00:00+00:00"
        );
    }

    #[test]
    fn leap_day_and_macros() {
        assert_eq!(
            next("0 0 29 2 *", "2026-03-01T00:00:00Z"),
            "2028-02-29T00:00:00+00:00"
        );
        assert_eq!(
            next("@hourly", "2026-01-01T00:59:59Z"),
            "2026-01-01T01:00:00+00:00"
        );
        assert_eq!(
            next("@weekly", "2026-01-01T00:00:00Z"),
            "2026-01-04T00:00:00+00:00"
        );
        assert_eq!(
            next("@monthly", "2026-01-31T12:00:00Z"),
            "2026-02-01T00:00:00+00:00"
        );
    }

    #[test]
    fn impossible_schedule_has_no_next() {
        let schedule: Schedule = "0 0 31 2 *".parse().unwrap();
        assert!(schedule.next_after(&at("2026-01-01T00:00:00Z")).is_none());
    }

    #[test]
    fn rejects_bad_expressions() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "1,,2 * * * *",
            "@reboot",
        ] {
            assert!(
                bad.parse::<Schedule>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }
}
