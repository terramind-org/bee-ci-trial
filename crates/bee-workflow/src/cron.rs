//! POSIX cron schedules for `on.schedule`.
//!
//! Five fields, in UTC: minute (0-59), hour (0-23), day of month (1-31), month (1-12 or
//! `JAN`-`DEC`) and day of week (0-6 or `SUN`-`SAT`; 7 is also Sunday). Each field accepts `*`,
//! lists (`,`), ranges (`-`) and steps (`/`). As in POSIX cron, when both day fields are
//! restricted a time matches if either one does.

use std::fmt;

/// A minute in UTC, the unit the scheduler ticks in. Supplied by the caller, never read from a
/// clock here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcMinute {
    /// Year, e.g. 2026.
    pub year: i32,
    /// Month, 1-12.
    pub month: u32,
    /// Day of month, 1-31.
    pub day: u32,
    /// Hour, 0-23.
    pub hour: u32,
    /// Minute, 0-59.
    pub minute: u32,
}

impl UtcMinute {
    /// Day of week, 0 = Sunday.
    pub fn weekday(&self) -> u32 {
        // Sakamoto's method.
        const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
        let m = self.month.clamp(1, 12);
        let y = if m < 3 { self.year - 1 } else { self.year };
        let d = y + y.div_euclid(4) - y.div_euclid(100)
            + y.div_euclid(400)
            + T[(m - 1) as usize]
            + self.day as i32;
        d.rem_euclid(7) as u32
    }
}

/// A cron parse error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronError(pub String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

/// A parsed cron schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    source: String,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    day_restricted: bool,
    weekday_restricted: bool,
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn field(
    text: &str,
    name: &str,
    min: u32,
    max: u32,
    names: &[&str],
    names_base: u32,
) -> Result<u64, CronError> {
    let bad = |why: &str| CronError(format!("invalid cron {name} field '{text}': {why}"));
    let value = |s: &str| -> Result<u32, CronError> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + names_base);
        }
        let v: u32 = s.parse().map_err(|_| bad("not a number"))?;
        if v < min || v > max {
            return Err(bad(&format!("{v} is outside {min}-{max}")));
        }
        Ok(v)
    };
    let mut bits = 0u64;
    for part in text.split(',') {
        if part.is_empty() {
            return Err(bad("empty list item"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let s: u32 = s.parse().map_err(|_| bad("bad step"))?;
                if s == 0 {
                    return Err(bad("step must be positive"));
                }
                (r, s)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (value(a)?, value(b)?);
            if a > b {
                return Err(bad("range start is after its end"));
            }
            (a, b)
        } else {
            let a = value(range)?;
            // `5/15` means 5, 20, 35, 50.
            if part.contains('/') { (a, max) } else { (a, a) }
        };
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl Cron {
    /// Parses a five-field cron expression.
    pub fn parse(source: &str) -> Result<Cron, CronError> {
        let fields: Vec<&str> = source.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError(format!(
                "invalid cron expression '{source}': expected 5 fields, found {}",
                fields.len()
            )));
        }
        let mut weekdays = field(fields[4], "day-of-week", 0, 7, &DAYS, 0)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Cron {
            source: source.to_owned(),
            minutes: field(fields[0], "minute", 0, 59, &[], 0)?,
            hours: field(fields[1], "hour", 0, 23, &[], 0)?,
            days: field(fields[2], "day-of-month", 1, 31, &[], 0)?,
            months: field(fields[3], "month", 1, 12, &MONTHS, 1)?,
            weekdays,
            day_restricted: !fields[2].starts_with('*'),
            weekday_restricted: !fields[4].starts_with('*'),
        })
    }

    /// The expression as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the schedule fires at this minute.
    pub fn matches(&self, t: &UtcMinute) -> bool {
        let has = |bits: u64, v: u32| v < 64 && bits & (1 << v) != 0;
        if !has(self.minutes, t.minute) || !has(self.hours, t.hour) || !has(self.months, t.month) {
            return false;
        }
        let dom = has(self.days, t.day);
        let dow = has(self.weekdays, t.weekday());
        match (self.day_restricted, self.weekday_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> UtcMinute {
        UtcMinute {
            year,
            month,
            day,
            hour,
            minute,
        }
    }

    #[test]
    fn weekday_is_correct() {
        assert_eq!(t(2026, 10, 18, 0, 0).weekday(), 0); // a Sunday
        assert_eq!(t(2024, 2, 29, 0, 0).weekday(), 4); // a Thursday
        assert_eq!(t(2000, 1, 1, 0, 0).weekday(), 6); // a Saturday
    }

    #[test]
    fn documented_examples() {
        // "30 5 * * 1,3": 5:30 UTC on Monday and Wednesday (GitHub's schedule docs).
        let c = Cron::parse("30 5 * * 1,3").unwrap();
        assert!(c.matches(&t(2026, 10, 19, 5, 30)));
        assert!(c.matches(&t(2026, 10, 21, 5, 30)));
        assert!(!c.matches(&t(2026, 10, 20, 5, 30)));
        // "30 5,17 * * *": 5:30 and 17:30 every day.
        let c = Cron::parse("30 5,17 * * *").unwrap();
        assert!(c.matches(&t(2026, 1, 1, 17, 30)));
        assert!(!c.matches(&t(2026, 1, 1, 17, 31)));
        // "*/15 * * * *": every 15 minutes.
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert!(c.matches(&t(2026, 1, 1, 3, 45)));
        assert!(!c.matches(&t(2026, 1, 1, 3, 46)));
    }

    #[test]
    fn steps_ranges_names_and_day_or_rule() {
        let c = Cron::parse("5/20 0 * JAN-MAR MON-FRI").unwrap();
        assert!(c.matches(&t(2026, 2, 2, 0, 45)));
        assert!(!c.matches(&t(2026, 4, 6, 0, 45)));
        // Both day fields restricted: either matches.
        let c = Cron::parse("0 0 1 * 0").unwrap();
        assert!(c.matches(&t(2026, 10, 1, 0, 0)));
        assert!(c.matches(&t(2026, 10, 18, 0, 0)));
        assert!(!c.matches(&t(2026, 10, 19, 0, 0)));
        assert!(
            Cron::parse("0 0 * * 7")
                .unwrap()
                .matches(&t(2026, 10, 18, 0, 0))
        );
    }

    #[test]
    fn invalid_expressions() {
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "* * * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }
}
