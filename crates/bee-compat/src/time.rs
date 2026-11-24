//! Timestamps in GitHub's form: ISO 8601 UTC with second precision (`2024-05-01T12:00:00Z`).

use pbjson_types::Timestamp;

/// Days from 1970-01-01 to the civil date `(y, m, d)` (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// A protobuf timestamp in GitHub's form, or JSON `null` when absent or zero.
pub fn iso(ts: &Option<Timestamp>) -> serde_json::Value {
    match ts {
        Some(t) if t.seconds != 0 || t.nanos != 0 => serde_json::Value::String(iso_secs(t.seconds)),
        _ => serde_json::Value::Null,
    }
}

/// A protobuf timestamp in GitHub's form, falling back to `fallback` when absent.
pub fn iso_or(ts: &Option<Timestamp>, fallback: &Option<Timestamp>) -> serde_json::Value {
    match iso(ts) {
        serde_json::Value::Null => iso(fallback),
        v => v,
    }
}

/// Milliseconds since the epoch in GitHub's form.
pub fn iso_ms(ms: u64) -> String {
    iso_secs((ms / 1000) as i64)
}

/// Parses `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ` or with a `±HH:MM` offset into seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    if s.len() < 10 || s.as_bytes().get(4) != Some(&b'-') || s.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m as u32, d as u32) * 86_400;
    if s.len() == 10 {
        return Some(secs);
    }
    if !matches!(s.as_bytes().get(10), Some(b'T' | b't' | b' ')) || s.len() < 19 {
        return None;
    }
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    secs += h * 3600 + mi * 60 + se;
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    match rest {
        "" | "Z" | "z" => Some(secs),
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let oh: i64 = r.get(1..3)?.parse().ok()?;
            let om: i64 = r.get(4..6)?.parse().ok()?;
            let off = oh * 3600 + om * 60;
            Some(if r.starts_with('+') {
                secs - off
            } else {
                secs + off
            })
        }
        _ => None,
    }
}

/// A protobuf timestamp from seconds.
pub fn ts_secs(secs: i64) -> Timestamp {
    Timestamp {
        seconds: secs,
        nanos: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_round_trip() {
        assert_eq!(iso_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_secs(1_714_564_800), "2024-05-01T12:00:00Z");
        assert_eq!(iso_secs(951_782_400), "2000-02-29T00:00:00Z");
        for s in [0i64, 1_714_564_800, 951_782_400, 4_102_444_799, 86_399] {
            assert_eq!(parse_iso(&iso_secs(s)), Some(s));
        }
        assert_eq!(parse_iso("2024-05-01"), Some(1_714_521_600));
        assert_eq!(parse_iso("2024-05-01T14:00:00+02:00"), Some(1_714_564_800));
        assert_eq!(parse_iso("2024-05-01T12:00:00.123Z"), Some(1_714_564_800));
        assert_eq!(parse_iso("yesterday"), None);
    }
}
