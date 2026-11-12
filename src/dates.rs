//! Minimal UTC calendar maths, so the engine needs no external crates.

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Parses `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SSZ` into Unix seconds. Dates and
/// times that don't exist, such as `2026-02-30` or `25:00:00`, are refused
/// rather than rolled over into a different day.
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T').unwrap_or((s, "00:00:00"));
    let d: Vec<i64> = date
        .split('-')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let t: Vec<i64> = time
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    if d.len() != 3 || t.len() != 3 {
        return None;
    }
    let real_day = (1..=12).contains(&d[1])
        && (1..=31).contains(&d[2])
        && civil_from_days(days_from_civil(d[0], d[1], d[2])) == (d[0], d[1], d[2]);
    if !real_day || !(0..24).contains(&t[0]) || !(0..60).contains(&t[1]) || !(0..60).contains(&t[2])
    {
        return None;
    }
    Some(days_from_civil(d[0], d[1], d[2]) * 86400 + t[0] * 3600 + t[1] * 60 + t[2])
}

pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 11, 5)), (2026, 11, 5));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_ts("1970-01-02T00:00:10Z"), Some(86410));
        assert_eq!(parse_ts("1970-01-02"), Some(86400));
        assert_eq!(parse_ts("garbage"), None);
    }

    #[test]
    fn refuses_dates_and_times_that_do_not_exist() {
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-02-30",
            "2025-02-29",
            "2026-04-31",
            "2026-11-00",
            "2026-11-05T24:00:00Z",
            "2026-11-05T12:60:00Z",
            "2026-11-05T12:00:60Z",
            "2026-11-05T-1:00:00Z",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
        assert!(parse_ts("2024-02-29").is_some());
        assert!(parse_ts("2026-12-31T23:59:59Z").is_some());
    }
}
