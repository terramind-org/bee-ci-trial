//! UTC calendar arithmetic for billing periods. No wall-clock access: every function is a pure
//! function of the milliseconds it is given.

const MS_PER_DAY: u64 = 86_400_000;

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `(year, month 1-12, day 1-31)` of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Index of the UTC calendar month containing `ms`: `year * 12 + (month - 1)`.
pub fn month_of(ms: u64) -> i64 {
    let (y, m, _) = civil_from_days((ms / MS_PER_DAY) as i64);
    y * 12 + i64::from(m) - 1
}

/// First millisecond of month `mi`.
pub fn month_start_ms(mi: i64) -> u64 {
    let (y, m) = (mi.div_euclid(12), mi.rem_euclid(12) as u32 + 1);
    (days_from_civil(y, m, 1).max(0) as u64) * MS_PER_DAY
}

/// `YYYY-MM` of month `mi`.
pub fn month_label(mi: i64) -> String {
    format!("{:04}-{:02}", mi.div_euclid(12), mi.rem_euclid(12) + 1)
}

/// `YYYYMM` of month `mi`.
pub fn yyyymm(mi: i64) -> String {
    format!("{:04}{:02}", mi.div_euclid(12), mi.rem_euclid(12) + 1)
}

/// `YYYY-MM-DD` of the day containing `ms`.
pub fn day_label(ms: u64) -> String {
    let (y, m, d) = civil_from_days((ms / MS_PER_DAY) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_round_trip() {
        // 2026-10-18T00:00:00Z
        let ms = 1_792_281_600_000;
        assert_eq!(day_label(ms), "2026-10-18");
        let mi = month_of(ms);
        assert_eq!(month_label(mi), "2026-10");
        assert_eq!(day_label(month_start_ms(mi)), "2026-10-01");
        assert_eq!(day_label(month_start_ms(mi + 3)), "2027-01-01");
        assert_eq!(month_of(month_start_ms(mi + 3) - 1), mi + 2);
        assert_eq!(yyyymm(mi), "202610");
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(951_782_400_000), "2000-02-29");
    }
}
