//! Time zones for reminders and user settings.
//!
//! There is no tz database in the dependency set, so zones are a built-in table of common
//! IANA names with their standard offset and one of the daylight-saving rules in use today
//! (none, US, EU, Australia, New Zealand), plus fixed offsets written `UTC+5`, `+05:30` or
//! `Z`. A name outside the table is refused (`unknown_time_zone`) instead of guessed.

use crate::error::{ForgeError, Result};

const DAY_MS: i64 = 86_400_000;
const MIN_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dst {
    None,
    Us,
    Eu,
    Au,
    Nz,
}

/// A resolved zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    std_minutes: i32,
    dst: Dst,
}

const TABLE: &[(&str, i32, Dst)] = &[
    ("UTC", 0, Dst::None),
    ("GMT", 0, Dst::None),
    ("Etc/UTC", 0, Dst::None),
    ("Europe/London", 0, Dst::Eu),
    ("Europe/Dublin", 0, Dst::Eu),
    ("Europe/Lisbon", 0, Dst::Eu),
    ("Europe/Paris", 60, Dst::Eu),
    ("Europe/Berlin", 60, Dst::Eu),
    ("Europe/Madrid", 60, Dst::Eu),
    ("Europe/Rome", 60, Dst::Eu),
    ("Europe/Amsterdam", 60, Dst::Eu),
    ("Europe/Brussels", 60, Dst::Eu),
    ("Europe/Vienna", 60, Dst::Eu),
    ("Europe/Zurich", 60, Dst::Eu),
    ("Europe/Stockholm", 60, Dst::Eu),
    ("Europe/Oslo", 60, Dst::Eu),
    ("Europe/Copenhagen", 60, Dst::Eu),
    ("Europe/Prague", 60, Dst::Eu),
    ("Europe/Warsaw", 60, Dst::Eu),
    ("Europe/Athens", 120, Dst::Eu),
    ("Europe/Helsinki", 120, Dst::Eu),
    ("Europe/Kyiv", 120, Dst::Eu),
    ("Europe/Bucharest", 120, Dst::Eu),
    ("Europe/Istanbul", 180, Dst::None),
    ("Europe/Moscow", 180, Dst::None),
    ("America/New_York", -300, Dst::Us),
    ("America/Toronto", -300, Dst::Us),
    ("America/Detroit", -300, Dst::Us),
    ("America/Chicago", -360, Dst::Us),
    ("America/Denver", -420, Dst::Us),
    ("America/Phoenix", -420, Dst::None),
    ("America/Los_Angeles", -480, Dst::Us),
    ("America/Vancouver", -480, Dst::Us),
    ("America/Anchorage", -540, Dst::Us),
    ("Pacific/Honolulu", -600, Dst::None),
    ("America/Mexico_City", -360, Dst::None),
    ("America/Sao_Paulo", -180, Dst::None),
    ("America/Argentina/Buenos_Aires", -180, Dst::None),
    ("Africa/Johannesburg", 120, Dst::None),
    ("Africa/Lagos", 60, Dst::None),
    ("Africa/Nairobi", 180, Dst::None),
    ("Asia/Dubai", 240, Dst::None),
    ("Asia/Karachi", 300, Dst::None),
    ("Asia/Kolkata", 330, Dst::None),
    ("Asia/Calcutta", 330, Dst::None),
    ("Asia/Dhaka", 360, Dst::None),
    ("Asia/Bangkok", 420, Dst::None),
    ("Asia/Jakarta", 420, Dst::None),
    ("Asia/Shanghai", 480, Dst::None),
    ("Asia/Hong_Kong", 480, Dst::None),
    ("Asia/Singapore", 480, Dst::None),
    ("Asia/Taipei", 480, Dst::None),
    ("Asia/Manila", 480, Dst::None),
    ("Asia/Tokyo", 540, Dst::None),
    ("Asia/Seoul", 540, Dst::None),
    ("Australia/Perth", 480, Dst::None),
    ("Australia/Brisbane", 600, Dst::None),
    ("Australia/Sydney", 600, Dst::Au),
    ("Australia/Melbourne", 600, Dst::Au),
    ("Pacific/Auckland", 720, Dst::Nz),
];

impl Zone {
    /// Resolves a zone name or fixed offset.
    pub fn parse(name: &str) -> Result<Zone> {
        let bad = || {
            ForgeError::invalid(
                "unknown_time_zone",
                format!("time zone {name:?} is not in the built-in table or a fixed offset"),
            )
        };
        if let Some((_, m, d)) = TABLE.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name)) {
            return Ok(Zone {
                std_minutes: *m,
                dst: *d,
            });
        }
        let t = name
            .strip_prefix("UTC")
            .or_else(|| name.strip_prefix("GMT"))
            .unwrap_or(name);
        if t == "Z" {
            return Ok(Zone {
                std_minutes: 0,
                dst: Dst::None,
            });
        }
        let sign = match t.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return Err(bad()),
        };
        let body = &t[1..];
        let (h, m) = match body.split_once(':') {
            Some((h, m)) => (h, m),
            None if body.len() == 4 => body.split_at(2),
            None => (body, "0"),
        };
        let (h, m): (i32, i32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
        if h > 14 || m > 59 {
            return Err(bad());
        }
        Ok(Zone {
            std_minutes: sign * (h * 60 + m),
            dst: Dst::None,
        })
    }

    /// The UTC offset in minutes at UTC time `utc_ms`.
    pub fn offset_minutes(&self, utc_ms: i64) -> i32 {
        if self.dst == Dst::None {
            return self.std_minutes;
        }
        let year = civil(utc_ms.div_euclid(DAY_MS)).0;
        let std = i64::from(self.std_minutes) * MIN_MS;
        // Transition instants in UTC.
        let at = |y: i64, m: i64, which: i64, local_h: i64, local_offset: i64| {
            let day = nth_weekday(y, m, 0, which);
            days(y, m, day) * DAY_MS + local_h * 3_600_000 - local_offset
        };
        let on = match self.dst {
            Dst::Us => {
                let start = at(year, 3, 2, 2, std);
                let end = at(year, 11, 1, 2, std + 60 * MIN_MS);
                utc_ms >= start && utc_ms < end
            }
            Dst::Eu => {
                let start = at(year, 3, -1, 1, 0);
                let end = at(year, 10, -1, 1, 0);
                utc_ms >= start && utc_ms < end
            }
            Dst::Au => {
                // Southern hemisphere: DST spans the new year.
                let start = at(year, 10, 1, 2, std);
                let end = at(year, 4, 1, 2, std);
                utc_ms >= start || utc_ms < end
            }
            Dst::Nz => {
                let start = at(year, 9, -1, 2, std);
                let end = at(year, 4, 1, 2, std);
                utc_ms >= start || utc_ms < end
            }
            Dst::None => false,
        };
        self.std_minutes + if on { 60 } else { 0 }
    }

    /// Local wall-clock milliseconds (as if UTC) at `utc_ms`.
    pub fn to_local(&self, utc_ms: i64) -> i64 {
        utc_ms + i64::from(self.offset_minutes(utc_ms)) * MIN_MS
    }

    /// The UTC instant of a local wall-clock time (as if UTC), resolving gaps forward.
    pub fn to_utc(&self, local_ms: i64) -> i64 {
        let guess = local_ms - i64::from(self.std_minutes) * MIN_MS;
        let off = i64::from(self.offset_minutes(guess)) * MIN_MS;
        let utc = local_ms - off;
        let off2 = i64::from(self.offset_minutes(utc)) * MIN_MS;
        local_ms - off2
    }
}

/// `(year, month, day)` of a day number since 1970-01-01.
pub fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Day number since 1970-01-01 of a civil date.
pub fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Weekday of a day number, 0 = Sunday.
pub fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

/// Day of month of the `n`th (1-based) or last (`-1`) `wd` (0 = Sunday) of a month.
fn nth_weekday(y: i64, m: i64, wd: i64, n: i64) -> i64 {
    if n > 0 {
        let first = days(y, m, 1);
        1 + (wd - weekday(first)).rem_euclid(7) + 7 * (n - 1)
    } else {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let last = days(ny, nm, 1) - 1;
        let (_, _, ld) = civil(last);
        ld - (weekday(last) - wd).rem_euclid(7)
    }
}

/// The date `YYYY-MM-DD` of a day number.
pub fn date_string(day: i64) -> String {
    let (y, m, d) = civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses `YYYY-MM-DD` into a day number.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut p = s.split('-');
    let (y, m, d): (i64, i64, i64) = (
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    );
    if p.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days(y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i64, m: i64, d: i64, h: i64) -> i64 {
        days(y, m, d) * DAY_MS + h * 3_600_000
    }

    #[test]
    fn civil_round_trip_and_weekdays() {
        assert_eq!(civil(days(2026, 10, 18)), (2026, 10, 18));
        assert_eq!(weekday(days(2026, 10, 18)), 0); // a Sunday
        assert_eq!(nth_weekday(2026, 3, 0, 2), 8);
        assert_eq!(nth_weekday(2026, 10, 0, -1), 25);
    }

    #[test]
    fn daylight_saving_rules() {
        let ny = Zone::parse("America/New_York").unwrap();
        assert_eq!(ny.offset_minutes(ms(2026, 1, 15, 12)), -300);
        assert_eq!(ny.offset_minutes(ms(2026, 7, 15, 12)), -240);
        let berlin = Zone::parse("Europe/Berlin").unwrap();
        assert_eq!(berlin.offset_minutes(ms(2026, 7, 1, 0)), 120);
        assert_eq!(berlin.offset_minutes(ms(2026, 12, 1, 0)), 60);
        let sydney = Zone::parse("Australia/Sydney").unwrap();
        assert_eq!(sydney.offset_minutes(ms(2026, 1, 1, 0)), 660);
        assert_eq!(sydney.offset_minutes(ms(2026, 7, 1, 0)), 600);
        assert_eq!(Zone::parse("UTC+5:30").unwrap().offset_minutes(0), 330);
        assert!(Zone::parse("Mars/Olympus").is_err());
    }

    #[test]
    fn local_time_round_trips() {
        let ny = Zone::parse("America/New_York").unwrap();
        let utc = ms(2026, 7, 15, 12);
        assert_eq!(ny.to_utc(ny.to_local(utc)), utc);
        assert_eq!(parse_date("2026-02-30"), Some(days(2026, 2, 30)));
        assert_eq!(date_string(days(2026, 10, 18)), "2026-10-18");
    }
}
