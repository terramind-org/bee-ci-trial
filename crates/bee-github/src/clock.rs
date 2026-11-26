//! Time, injected. Rate-limit waits and token expiry read the clock; nothing that affects an
//! id or a report does. Tests use [`ManualClock`], whose `sleep` advances time instantly and
//! records how long the client asked to wait.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

/// The clock the client waits on.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now_secs(&self) -> u64;
    /// Waits `d`.
    async fn sleep(&self, d: Duration);
}

/// The system clock and tokio's timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// A clock that only moves when slept on (tests).
#[derive(Debug, Default)]
pub struct ManualClock {
    state: Mutex<(u64, Vec<Duration>)>,
}

impl ManualClock {
    /// A clock at `now` seconds.
    pub fn new(now: u64) -> ManualClock {
        ManualClock {
            state: Mutex::new((now, Vec::new())),
        }
    }

    /// Every wait requested so far.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state.lock().map(|s| s.1.clone()).unwrap_or_default()
    }

    /// Moves the clock forward.
    pub fn advance(&self, secs: u64) {
        if let Ok(mut s) = self.state.lock() {
            s.0 += secs;
        }
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now_secs(&self) -> u64 {
        self.state.lock().map(|s| s.0).unwrap_or(0)
    }

    async fn sleep(&self, d: Duration) {
        if let Ok(mut s) = self.state.lock() {
            s.0 += d.as_secs().max(u64::from(d.subsec_nanos() > 0));
            s.1.push(d);
        }
        tokio::task::yield_now().await;
    }
}

/// Seconds since the epoch of an RFC 3339 UTC time (`2026-10-18T12:00:00Z`, fractions and
/// offsets accepted); `None` when it does not parse.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let oh: i64 = o.get(1..3)?.parse().ok()?;
            let om: i64 = o.get(4..6)?.parse().ok()?;
            let v = oh * 3600 + om * 60;
            if o.starts_with('+') { v } else { -v }
        }
        _ => return None,
    };
    // Days from the civil date (proleptic Gregorian), after Howard Hinnant's algorithm.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = days * 86_400 + h * 3600 + mi * 60 + se - offset;
    u64::try_from(t).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339("2026-10-18T12:34:56Z"), Some(1_792_326_896));
        assert_eq!(
            parse_rfc3339("2026-10-18T14:34:56.123+02:00"),
            Some(1_792_326_896)
        );
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[tokio::test]
    async fn manual_clock_records_sleeps() {
        let c = ManualClock::new(100);
        c.sleep(Duration::from_secs(5)).await;
        assert_eq!(c.now_secs(), 105);
        assert_eq!(c.sleeps(), vec![Duration::from_secs(5)]);
    }
}
