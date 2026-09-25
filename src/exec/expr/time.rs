//! Date/time scalar kernels (adelie-1st.1): date_trunc, time_bucket, extract. TIMESTAMP is i64
//! ns UTC, DATE is i32 days; every division floors (`div_euclid`) so pre-1970 values are exact.

use crate::exec::{Column, ColumnBuilder, ExecError};
use crate::types::{DataType, Value, civil_from_days, days_from_civil};

use super::{DatePart, TruncUnit};

const NS_PER_DAY: i64 = 86_400_000_000_000;

/// 0 = Monday .. 6 = Sunday (day 0, the Unix epoch, was a Thursday).
fn weekday_mon0(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn trunc_ns(ns: i64, unit: &TruncUnit) -> i64 {
    let floor_to = |width: i64| ns.div_euclid(width) * width;
    match unit {
        TruncUnit::Microsecond => floor_to(1_000),
        TruncUnit::Millisecond => floor_to(1_000_000),
        TruncUnit::Second => floor_to(1_000_000_000),
        TruncUnit::Minute => floor_to(60_000_000_000),
        TruncUnit::Hour => floor_to(3_600_000_000_000),
        TruncUnit::Day => floor_to(NS_PER_DAY),
        TruncUnit::Week => {
            let days = ns.div_euclid(NS_PER_DAY);
            (days - weekday_mon0(days)) * NS_PER_DAY
        }
        TruncUnit::Month | TruncUnit::Quarter | TruncUnit::Year => {
            let days = ns.div_euclid(NS_PER_DAY);
            let (y, m, _) = civil_from_days(days);
            let (y, m) = match unit {
                TruncUnit::Month => (y, m),
                TruncUnit::Quarter => (y, (m - 1) / 3 * 3 + 1),
                TruncUnit::Year => (y, 1),
                _ => unreachable!("matched above"),
            };
            days_from_civil(y, m, 1) * NS_PER_DAY
        }
    }
}

pub(crate) fn date_trunc(col: &Column, unit: &TruncUnit) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::Timestamp, col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            b.push_null();
            continue;
        }
        let Value::Timestamp(ns) = col.get(i) else {
            unreachable!("func_type checked this column is TIMESTAMP")
        };
        b.push(&Value::Timestamp(trunc_ns(ns, unit)))?;
    }
    Ok(b.finish())
}

pub(crate) fn time_bucket(
    col: &Column,
    width_ns: i64,
    origin_ns: i64,
) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::Timestamp, col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            b.push_null();
            continue;
        }
        let Value::Timestamp(ns) = col.get(i) else {
            unreachable!("func_type checked this column is TIMESTAMP")
        };
        let bucket = origin_ns + (ns - origin_ns).div_euclid(width_ns) * width_ns;
        b.push(&Value::Timestamp(bucket))?;
    }
    Ok(b.finish())
}

fn day_of_year(days: i64, year: i64) -> i64 {
    days - days_from_civil(year, 1, 1) + 1
}

/// ISO 8601 week number: week 1 contains the year's first Thursday. `p(y)` is the ISO
/// "long year" test (Gregorian): year `y` has 53 weeks iff `p(y) == 4 || p(y-1) == 3`.
fn iso_week(days: i64, year: i64) -> i64 {
    fn p(y: i64) -> i64 {
        (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7)
    }
    fn weeks_in_year(y: i64) -> i64 {
        if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
    }
    let ordinal = day_of_year(days, year);
    let iso_weekday = weekday_mon0(days) + 1; // 1 = Monday .. 7 = Sunday
    let week = (ordinal - iso_weekday + 10).div_euclid(7);
    if week < 1 {
        weeks_in_year(year - 1)
    } else if week > weeks_in_year(year) {
        1
    } else {
        week
    }
}

fn extract_ns(ns: i64, part: &DatePart) -> i64 {
    let days = ns.div_euclid(NS_PER_DAY);
    let of_day = ns.rem_euclid(NS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    match part {
        DatePart::Year => year,
        DatePart::Quarter => (month as i64 - 1) / 3 + 1,
        DatePart::Month => month as i64,
        DatePart::Week => iso_week(days, year),
        DatePart::Day => day as i64,
        // Sunday = 0 (DuckDB `dow`): weekday_mon0 is 0 = Monday .. 6 = Sunday.
        DatePart::DayOfWeek => (weekday_mon0(days) + 1) % 7,
        DatePart::DayOfYear => day_of_year(days, year),
        DatePart::Hour => of_day / 3_600_000_000_000,
        DatePart::Minute => of_day / 60_000_000_000 % 60,
        DatePart::Second => of_day / 1_000_000_000 % 60,
        DatePart::Millisecond | DatePart::Microsecond => {
            let sec_of_minute = of_day / 1_000_000_000 % 60;
            let frac_ns = of_day % 1_000_000_000;
            match part {
                DatePart::Millisecond => sec_of_minute * 1_000 + frac_ns / 1_000_000,
                DatePart::Microsecond => sec_of_minute * 1_000_000 + frac_ns / 1_000,
                _ => unreachable!("matched above"),
            }
        }
        DatePart::Epoch => ns.div_euclid(1_000_000_000),
    }
}

/// TIMESTAMP or DATE (`func_type` checks this); a DATE is midnight, so its time parts are 0.
pub(crate) fn extract(col: &Column, part: &DatePart) -> Result<Column, ExecError> {
    let mut b = ColumnBuilder::with_capacity(DataType::Int64, col.len());
    for i in 0..col.len() {
        if col.is_null(i) {
            b.push_null();
            continue;
        }
        let ns = match col.get(i) {
            Value::Timestamp(ns) => ns,
            Value::Date(d) => i64::from(d) * NS_PER_DAY,
            other => unreachable!("func_type checked TIMESTAMP or DATE, found {other:?}"),
        };
        b.push(&Value::Int64(extract_ns(ns, part)))?;
    }
    Ok(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_col(vals: &[i64]) -> Column {
        Column::from_values(
            &DataType::Timestamp,
            &vals
                .iter()
                .map(|&n| Value::Timestamp(n))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn trunc(unit: TruncUnit, ns: i64) -> i64 {
        let col = ts_col(&[ns]);
        let Value::Timestamp(out) = date_trunc(&col, &unit).unwrap().get(0) else {
            unreachable!()
        };
        out
    }

    #[test]
    fn month_truncates_across_a_leap_day() {
        // 2024-02-29T13:00:00Z
        let ns = (19_782i64 * NS_PER_DAY) + 13 * 3_600_000_000_000;
        assert_eq!(trunc(TruncUnit::Month, ns), 19_754 * NS_PER_DAY); // 2024-02-01
    }

    #[test]
    fn week_truncates_a_sunday_to_the_prior_monday() {
        // 2024-01-07 is a Sunday; day 19729.
        let sunday = 19_729i64 * NS_PER_DAY;
        assert_eq!(trunc(TruncUnit::Week, sunday), 19_723 * NS_PER_DAY); // 2024-01-01 Monday
    }

    /// Fails under truncating (toward-zero) division: `-1 / NS_PER_DAY == 0`, which would
    /// truncate to 1970-01-01, not 1969-01-01.
    #[test]
    fn year_truncates_a_negative_timestamp_correctly() {
        assert_eq!(trunc(TruncUnit::Year, -1), -365 * NS_PER_DAY); // 1969-01-01
    }

    #[test]
    fn time_bucket_15_minutes_with_the_duckdb_origin_pre_origin() {
        let origin = 946_857_600_000_000_000; // 2000-01-03 UTC
        let width = 15 * 60 * 1_000_000_000i64;
        let ts = origin - width - 1; // just before the bucket 2 back from origin
        let col = ts_col(&[ts]);
        let out = time_bucket(&col, width, origin).unwrap();
        let Value::Timestamp(bucket) = out.get(0) else {
            unreachable!()
        };
        assert_eq!(bucket, origin - 2 * width);
    }

    fn extract_one(part: DatePart, ns: i64) -> i64 {
        let col = ts_col(&[ns]);
        let Value::Int64(out) = extract(&col, &part).unwrap().get(0) else {
            unreachable!()
        };
        out
    }

    #[test]
    fn extract_every_part_of_one_known_timestamp() {
        // 2024-03-04T05:06:07.123456789Z (Monday), day 19786.
        let ns = 19_786i64 * NS_PER_DAY
            + 5 * 3_600_000_000_000
            + 6 * 60_000_000_000
            + 7 * 1_000_000_000
            + 123_456_789;
        assert_eq!(extract_one(DatePart::Year, ns), 2024);
        assert_eq!(extract_one(DatePart::Quarter, ns), 1);
        assert_eq!(extract_one(DatePart::Month, ns), 3);
        assert_eq!(extract_one(DatePart::Day, ns), 4);
        assert_eq!(extract_one(DatePart::DayOfWeek, ns), 1); // Monday
        assert_eq!(extract_one(DatePart::Hour, ns), 5);
        assert_eq!(extract_one(DatePart::Minute, ns), 6);
        assert_eq!(extract_one(DatePart::Second, ns), 7);
        assert_eq!(extract_one(DatePart::Millisecond, ns), 7_123);
        assert_eq!(extract_one(DatePart::Microsecond, ns), 7_123_456);
        assert_eq!(extract_one(DatePart::Epoch, ns), 1_709_528_767);
    }

    #[test]
    fn extract_date_treats_time_parts_as_zero() {
        let col = Column::from_values(&DataType::Date, &[Value::Date(19_786)]).unwrap();
        let hour = extract(&col, &DatePart::Hour).unwrap();
        assert_eq!(hour.get(0), Value::Int64(0));
        let day = extract(&col, &DatePart::Day).unwrap();
        assert_eq!(day.get(0), Value::Int64(4));
    }

    #[test]
    fn iso_week_1_and_53_edges() {
        // 2021-01-03 is a Sunday belonging to ISO week 53 of 2020.
        let days_2021_01_03 = 18_630i64;
        assert_eq!(
            extract_one(DatePart::Week, days_2021_01_03 * NS_PER_DAY),
            53
        );
        // 2021-01-04 (Monday) is ISO week 1 of 2021.
        assert_eq!(
            extract_one(DatePart::Week, (days_2021_01_03 + 1) * NS_PER_DAY),
            1
        );
    }
}
