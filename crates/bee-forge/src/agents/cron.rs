//! Five-field cron schedules (minute, hour, day of month, month, day of week; UTC) for agent
//! schedules and scheduled prebuilds. Fields take `*`, numbers, ranges `a-b`, lists `a,b`
//! and steps `*/n` or `a-b/n`. Day of week is 0–6 with 0 = Sunday (7 is accepted as Sunday).
//! When both day fields are restricted, a time matches when either does (classic cron).

use crate::error::{ForgeError, Result};

/// A parsed schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

fn field(s: &str, lo: u32, hi: u32, name: &str) -> Result<(u64, bool)> {
    let bad = || ForgeError::invalid("invalid_cron", format!("bad {name} field {s:?}"));
    let mut bits = 0u64;
    let any = s == "*";
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().map_err(|_| bad())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(bad());
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse::<u32>().map_err(|_| bad())?,
                b.parse::<u32>().map_err(|_| bad())?,
            )
        } else {
            let v = range.parse::<u32>().map_err(|_| bad())?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(bad());
        }
        let mut v = a;
        while v <= b {
            bits |= 1u64 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

impl Cron {
    /// Parses an expression.
    pub fn parse(s: &str) -> Result<Cron> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return Err(ForgeError::invalid(
                "invalid_cron",
                format!("{s:?} is not a five-field cron expression"),
            ));
        }
        let (minutes, _) = field(f[0], 0, 59, "minute")?;
        let (hours, _) = field(f[1], 0, 23, "hour")?;
        let (days, any_day) = field(f[2], 1, 31, "day")?;
        let (months, _) = field(f[3], 1, 12, "month")?;
        let (mut weekdays, any_weekday) = field(f[4], 0, 7, "weekday")?;
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1;
        }
        Ok(Cron {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: (weekdays & 0x7f) as u8,
            any_day,
            any_weekday,
        })
    }

    /// Whether the minute starting at `ms` matches.
    pub fn matches(&self, ms: u64) -> bool {
        let mins = ms / 60_000;
        let minute = (mins % 60) as u32;
        let hour = ((mins / 60) % 24) as u32;
        let days = mins / 1440;
        let (_, month, day) = super::civil(days);
        let weekday = ((days + 4) % 7) as u32; // 1970-01-01 was a Thursday
        let day_ok = match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (false, true) => self.days & (1 << day) != 0,
            (true, false) => self.weekdays & (1 << weekday) != 0,
            (false, false) => self.days & (1 << day) != 0 || self.weekdays & (1 << weekday) != 0,
        };
        self.minutes & (1 << minute) != 0
            && self.hours & (1 << hour) != 0
            && self.months & (1 << month) != 0
            && day_ok
    }

    /// The first matching minute strictly after `after_ms` and at or before `until_ms`
    /// (looking at most 366 days ahead).
    pub fn next_between(&self, after_ms: u64, until_ms: u64) -> Option<u64> {
        let mut t = (after_ms / 60_000 + 1) * 60_000;
        let end = until_ms.min(after_ms.saturating_add(366 * 86_400_000));
        while t <= end {
            if self.matches(t) {
                return Some(t);
            }
            t += 60_000;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let c = Cron::parse("30 2 * * *").unwrap();
        // 1970-01-01 02:30 UTC
        assert!(c.matches(2 * 3_600_000 + 30 * 60_000));
        assert!(!c.matches(2 * 3_600_000 + 31 * 60_000));
        let every15 = Cron::parse("*/15 * * * *").unwrap();
        assert!(every15.matches(45 * 60_000));
        assert!(!every15.matches(46 * 60_000));
        assert_eq!(every15.next_between(0, 3_600_000), Some(15 * 60_000));
        // Thursday 1970-01-01 is weekday 4.
        assert!(Cron::parse("0 0 * * 4").unwrap().matches(0));
        assert!(!Cron::parse("0 0 * * 1-3").unwrap().matches(0));
        assert!(Cron::parse("0 0 1 1 *").unwrap().matches(0));
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("* * *").is_err());
        assert!(Cron::parse("*/0 * * * *").is_err());
    }
}
