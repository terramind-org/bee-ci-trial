//! Dates, numbers and ranges in forge qualifiers (`created:>=2024-01-01`, `stars:10..50`).
//!
//! No clock is read: `@today` resolves against a date the caller supplies.

use std::ops::Bound;

/// A civil (proleptic Gregorian) date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CivilDate {
    /// Year.
    pub year: i32,
    /// Month, 1-12.
    pub month: u8,
    /// Day, 1-31.
    pub day: u8,
}

impl CivilDate {
    /// Validates a date.
    pub fn new(year: i32, month: u8, day: u8) -> Option<CivilDate> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(CivilDate { year, month, day })
    }

    /// Days since 1970-01-01.
    pub fn days_since_epoch(self) -> i64 {
        // Shift the year so it starts in March; leap days fall at the end.
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The date `days` days since 1970-01-01.
    pub fn from_days(days: i64) -> CivilDate {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        CivilDate { year, month, day }
    }

    /// Adds days.
    pub fn add_days(self, days: i64) -> CivilDate {
        CivilDate::from_days(self.days_since_epoch() + days)
    }
}

impl std::fmt::Display for CivilDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// A date or instant as written in a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DateValue {
    /// A whole day, UTC.
    Day(CivilDate),
    /// `@today` plus a day offset (`@today-7d` is -7).
    Today(i32),
    /// An instant: unix milliseconds.
    Instant(i64),
}

impl std::fmt::Display for DateValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DateValue::Day(d) => write!(f, "{d}"),
            DateValue::Today(0) => write!(f, "@today"),
            DateValue::Today(n) => write!(f, "@today{n:+}d"),
            DateValue::Instant(ms) => {
                let days = ms.div_euclid(86_400_000);
                let rem = ms.rem_euclid(86_400_000) / 1000;
                write!(
                    f,
                    "{}T{:02}:{:02}:{:02}Z",
                    CivilDate::from_days(days),
                    rem / 3600,
                    rem / 60 % 60,
                    rem % 60
                )
            }
        }
    }
}

fn num<T: std::str::FromStr>(s: &str, len: usize) -> Option<T> {
    if s.len() == len && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// Parses `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS][Z|±HH:MM]` or `@today[±Nd]`.
pub fn parse_date(s: &str) -> Option<DateValue> {
    if let Some(rest) = s.strip_prefix("@today") {
        if rest.is_empty() {
            return Some(DateValue::Today(0));
        }
        let n = rest.strip_suffix('d')?;
        let (sign, digits) = match n.as_bytes().first()? {
            b'-' => (-1, &n[1..]),
            b'+' => (1, &n[1..]),
            _ => return None,
        };
        if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(DateValue::Today(sign * digits.parse::<i32>().ok()?));
    }
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y: i32 = num(parts.next()?, 4)?;
    let m: u8 = num(parts.next()?, 2)?;
    let d: u8 = num(parts.next()?, 2)?;
    if parts.next().is_some() {
        return None;
    }
    let day = CivilDate::new(y, m, d)?;
    let Some(time) = time else {
        return Some(DateValue::Day(day));
    };
    let (clock, offset_min) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0i64)
    } else if let Some(pos) = time.rfind(['+', '-']) {
        let (c, off) = time.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (hh, mm) = off[1..].split_once(':')?;
        let h: i64 = num(hh, 2)?;
        let mi: i64 = num(mm, 2)?;
        if h > 23 || mi > 59 {
            return None;
        }
        (c, sign * (h * 60 + mi))
    } else {
        (time, 0)
    };
    let mut cp = clock.split(':');
    let h: i64 = num(cp.next()?, 2)?;
    let mi: i64 = num(cp.next()?, 2)?;
    let sec: i64 = match cp.next() {
        Some(x) => num(x, 2)?,
        None => 0,
    };
    if cp.next().is_some() || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let ms = (day.days_since_epoch() * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60) * 1000;
    Some(DateValue::Instant(ms))
}

/// A comparison as written: `X`, `>X`, `>=X`, `<X`, `<=X`, `X..Y` (`*` for open ends).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cmp<T> {
    /// Equal to (a whole day for dates).
    Eq(T),
    /// Greater than.
    Gt(T),
    /// Greater than or equal.
    Ge(T),
    /// Less than.
    Lt(T),
    /// Less than or equal.
    Le(T),
    /// Inclusive range; `None` is an open end.
    Between(Option<T>, Option<T>),
}

impl<T: std::fmt::Display> std::fmt::Display for Cmp<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmp::Eq(v) => write!(f, "{v}"),
            Cmp::Gt(v) => write!(f, ">{v}"),
            Cmp::Ge(v) => write!(f, ">={v}"),
            Cmp::Lt(v) => write!(f, "<{v}"),
            Cmp::Le(v) => write!(f, "<={v}"),
            Cmp::Between(a, b) => {
                let s = |x: &Option<T>| x.as_ref().map_or("*".to_string(), ToString::to_string);
                write!(f, "{}..{}", s(a), s(b))
            }
        }
    }
}

/// Parses a comparison with a value parser.
pub fn parse_cmp<T>(s: &str, value: &dyn Fn(&str) -> Option<T>) -> Option<Cmp<T>> {
    if let Some(r) = s.strip_prefix(">=") {
        return value(r).map(Cmp::Ge);
    }
    if let Some(r) = s.strip_prefix("<=") {
        return value(r).map(Cmp::Le);
    }
    if let Some(r) = s.strip_prefix('>') {
        return value(r).map(Cmp::Gt);
    }
    if let Some(r) = s.strip_prefix('<') {
        return value(r).map(Cmp::Lt);
    }
    if let Some((a, b)) = s.split_once("..") {
        let side = |x: &str| -> Option<Option<T>> {
            if x == "*" {
                Some(None)
            } else {
                value(x).map(Some)
            }
        };
        let (a, b) = (side(a)?, side(b)?);
        if a.is_none() && b.is_none() {
            return None;
        }
        return Some(Cmp::Between(a, b));
    }
    value(s).map(Cmp::Eq)
}

/// Parses a non-negative integer.
pub fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Integer bounds for a numeric comparison.
pub fn num_bounds(c: &Cmp<u64>) -> (Bound<i64>, Bound<i64>) {
    let v = |x: &u64| i64::try_from(*x).unwrap_or(i64::MAX);
    match c {
        Cmp::Eq(x) => (Bound::Included(v(x)), Bound::Included(v(x))),
        Cmp::Gt(x) => (Bound::Excluded(v(x)), Bound::Unbounded),
        Cmp::Ge(x) => (Bound::Included(v(x)), Bound::Unbounded),
        Cmp::Lt(x) => (Bound::Unbounded, Bound::Excluded(v(x))),
        Cmp::Le(x) => (Bound::Unbounded, Bound::Included(v(x))),
        Cmp::Between(a, b) => (
            a.as_ref()
                .map_or(Bound::Unbounded, |x| Bound::Included(v(x))),
            b.as_ref()
                .map_or(Bound::Unbounded, |x| Bound::Included(v(x))),
        ),
    }
}

const DAY_MS: i64 = 86_400_000;

/// `[start, end)` in unix ms of a date value; `None` when it needs `today` and none is
/// given.
fn span_of(d: &DateValue, today: Option<CivilDate>) -> Option<(i64, i64)> {
    match d {
        DateValue::Day(c) => {
            let s = c.days_since_epoch() * DAY_MS;
            Some((s, s + DAY_MS))
        }
        DateValue::Today(off) => {
            let s = today?.add_days(i64::from(*off)).days_since_epoch() * DAY_MS;
            Some((s, s + DAY_MS))
        }
        DateValue::Instant(ms) => Some((*ms, *ms + 1)),
    }
}

/// Unix-millisecond bounds of a date comparison. Whole days are UTC days: `>D` starts after
/// the day ends, `<=D` includes the whole day. `None` when `@today` is used without `today`.
pub fn date_bounds(
    c: &Cmp<DateValue>,
    today: Option<CivilDate>,
) -> Option<(Bound<i64>, Bound<i64>)> {
    Some(match c {
        Cmp::Eq(d) => {
            let (s, e) = span_of(d, today)?;
            (Bound::Included(s), Bound::Excluded(e))
        }
        Cmp::Gt(d) => (Bound::Included(span_of(d, today)?.1), Bound::Unbounded),
        Cmp::Ge(d) => (Bound::Included(span_of(d, today)?.0), Bound::Unbounded),
        Cmp::Lt(d) => (Bound::Unbounded, Bound::Excluded(span_of(d, today)?.0)),
        Cmp::Le(d) => (Bound::Unbounded, Bound::Excluded(span_of(d, today)?.1)),
        Cmp::Between(a, b) => (
            match a {
                Some(d) => Bound::Included(span_of(d, today)?.0),
                None => Bound::Unbounded,
            },
            match b {
                Some(d) => Bound::Excluded(span_of(d, today)?.1),
                None => Bound::Unbounded,
            },
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        let d = CivilDate::new(2024, 2, 29).unwrap();
        assert_eq!(CivilDate::from_days(d.days_since_epoch()), d);
        assert_eq!(CivilDate::new(1970, 1, 1).unwrap().days_since_epoch(), 0);
        assert_eq!(
            CivilDate::new(2000, 3, 1).unwrap().days_since_epoch(),
            11_017
        );
        assert!(CivilDate::new(2023, 2, 29).is_none());
        assert!(CivilDate::new(2023, 13, 1).is_none());
        for n in [-800_000i64, -1, 0, 1, 59, 60, 365, 20_000, 2_000_000] {
            assert_eq!(CivilDate::from_days(n).days_since_epoch(), n);
        }
        assert_eq!(d.add_days(1).to_string(), "2024-03-01");
    }

    #[test]
    fn dates() {
        assert_eq!(
            parse_date("2024-01-02"),
            Some(DateValue::Day(CivilDate::new(2024, 1, 2).unwrap()))
        );
        assert_eq!(parse_date("@today"), Some(DateValue::Today(0)));
        assert_eq!(parse_date("@today-7d"), Some(DateValue::Today(-7)));
        assert_eq!(
            parse_date("1970-01-01T00:00:01Z"),
            Some(DateValue::Instant(1000))
        );
        assert_eq!(
            parse_date("1970-01-01T01:00+01:00"),
            Some(DateValue::Instant(0))
        );
        assert_eq!(parse_date("2024-1-2"), None);
        assert_eq!(parse_date("2024-01-02T25:00"), None);
        assert_eq!(parse_date("@yesterday"), None);
        assert_eq!(DateValue::Instant(1000).to_string(), "1970-01-01T00:00:01Z");
        assert_eq!(DateValue::Today(-7).to_string(), "@today-7d");
    }

    #[test]
    fn comparisons() {
        assert_eq!(parse_cmp("10", &parse_u64), Some(Cmp::Eq(10)));
        assert_eq!(parse_cmp(">=10", &parse_u64), Some(Cmp::Ge(10)));
        assert_eq!(
            parse_cmp("10..*", &parse_u64),
            Some(Cmp::Between(Some(10), None))
        );
        assert_eq!(parse_cmp("*..*", &parse_u64), None);
        assert_eq!(parse_cmp("x", &parse_u64), None);
        assert_eq!(Cmp::Between(None, Some(3u64)).to_string(), "*..3");
        assert_eq!(
            num_bounds(&Cmp::Gt(3)),
            (Bound::Excluded(3), Bound::Unbounded)
        );
        let day = DateValue::Day(CivilDate::new(1970, 1, 2).unwrap());
        assert_eq!(
            date_bounds(&Cmp::Gt(day), None),
            Some((Bound::Included(2 * DAY_MS), Bound::Unbounded))
        );
        assert_eq!(
            date_bounds(&Cmp::Le(day), None),
            Some((Bound::Unbounded, Bound::Excluded(2 * DAY_MS)))
        );
        assert_eq!(date_bounds(&Cmp::Eq(DateValue::Today(0)), None), None);
        assert_eq!(
            date_bounds(&Cmp::Eq(DateValue::Today(-1)), CivilDate::new(1970, 1, 3)),
            Some((Bound::Included(DAY_MS), Bound::Excluded(2 * DAY_MS)))
        );
    }
}
