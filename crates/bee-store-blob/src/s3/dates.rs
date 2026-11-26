//! The few date conversions S3 needs: SigV4 timestamps and the two formats S3 answers with.

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(yyyymmddThhmmssZ, yyyymmdd)` for a Unix time in seconds.
pub(crate) fn amz_date(unix_secs: u64) -> (String, String) {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let date = format!("{y:04}{m:02}{d:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    (stamp, date)
}

fn num(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// Parses `2024-05-01T12:00:00.000Z` (ListObjectsV2) into Unix milliseconds.
pub(crate) fn parse_iso8601_ms(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let (clock, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = clock.split(':');
    let (h, mi, s) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    let mut ms = frac.chars().take(3).collect::<String>();
    while ms.len() < 3 {
        ms.push('0');
    }
    let secs = days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(secs * 1000 + num(&ms)?).ok()
}

/// Parses `Wed, 21 Oct 2015 07:28:00 GMT` (the `Last-Modified` header) into Unix milliseconds.
pub(crate) fn parse_http_date_ms(text: &str) -> Option<u64> {
    let (_, rest) = text.trim().split_once(", ")?;
    let mut parts = rest.split(' ');
    let day = num(parts.next()?)?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = num(parts.next()?)?;
    let mut t = parts.next()?.split(':');
    let (h, mi, s) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(secs * 1000).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amz_dates() {
        assert_eq!(
            amz_date(1_369_353_600),
            ("20130524T000000Z".to_string(), "20130524".to_string())
        );
        assert_eq!(amz_date(0).0, "19700101T000000Z");
        assert_eq!(amz_date(951_782_400 + 3661).0, "20000229T010101Z");
    }

    #[test]
    fn parses_both_formats() {
        assert_eq!(
            parse_iso8601_ms("2013-05-24T00:00:00.000Z"),
            Some(1_369_353_600_000)
        );
        assert_eq!(
            parse_iso8601_ms("2013-05-24T00:00:01.250Z"),
            Some(1_369_353_601_250)
        );
        assert_eq!(
            parse_iso8601_ms("2013-05-24T00:00:00Z"),
            Some(1_369_353_600_000)
        );
        assert_eq!(
            parse_http_date_ms("Fri, 24 May 2013 00:00:00 GMT"),
            Some(1_369_353_600_000)
        );
        assert_eq!(parse_http_date_ms("nonsense"), None);
        assert_eq!(parse_iso8601_ms("nonsense"), None);
    }
}
