//! Civil calendar conversions shared by the slt renderer and the DuckDB adapter, so both
//! sides of the differential format dates and timestamps identically.

/// Howard Hinnant's civil-from-days: a day count since 1970-01-01 to (year, month, day).
/// http://howardhinnant.github.io/date_algorithms.html#civil_from_days
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats a day count since the epoch (UTC) as `YYYY-MM-DD`.
pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats nanoseconds since the epoch (UTC) as `YYYY-MM-DD HH:MM:SS`, adding a `.` plus 6
/// fractional digits when the remainder is a whole microsecond, else 9. `div_euclid`/
/// `rem_euclid` keep pre-1970 values correct.
pub fn format_timestamp_ns(ns: i64) -> String {
    const NS_PER_DAY: i64 = 86_400_000_000_000;
    let days = ns.div_euclid(NS_PER_DAY);
    let of_day = ns.rem_euclid(NS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    let secs = of_day / 1_000_000_000;
    let frac = of_day % 1_000_000_000;
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let base = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if frac == 0 {
        base
    } else if frac % 1000 == 0 {
        format!("{base}.{:06}", frac / 1000)
    } else {
        format!("{base}.{frac:09}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_date_at_and_around_the_epoch() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_date(19724), "2024-01-02");
    }

    #[test]
    fn format_timestamp_ns_whole_seconds_has_no_fraction() {
        assert_eq!(
            format_timestamp_ns(1_704_164_645_000_000_000),
            "2024-01-02 03:04:05"
        );
    }

    #[test]
    fn format_timestamp_ns_microsecond_fraction_is_six_digits() {
        assert_eq!(
            format_timestamp_ns(1_704_164_645_500_000_000),
            "2024-01-02 03:04:05.500000"
        );
    }

    #[test]
    fn format_timestamp_ns_sub_microsecond_fraction_is_nine_digits() {
        assert_eq!(
            format_timestamp_ns(1_704_164_645_500_000_001),
            "2024-01-02 03:04:05.500000001"
        );
    }

    #[test]
    fn format_timestamp_ns_negative_one_is_the_last_ns_of_1969() {
        assert_eq!(format_timestamp_ns(-1), "1969-12-31 23:59:59.999999999");
    }
}
