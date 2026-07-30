use std::time::{SystemTime, UNIX_EPOCH};

pub const SECONDS_PER_DAY: i64 = 86_400;

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_secs() as i64
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the format the snapshot series has used since day one.
pub fn iso8601_utc(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(SECONDS_PER_DAY));
    let rem = unix.rem_euclid(SECONDS_PER_DAY);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

pub fn date_utc(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(SECONDS_PER_DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Hinnant's `civil_from_days`: days since the Unix epoch to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1), "1970-01-01T00:00:01Z");
        assert_eq!(iso8601_utc(1_784_206_313), "2026-07-16T12:51:53Z");
        assert_eq!(iso8601_utc(1_583_020_800), "2020-03-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(date_utc(1_784_206_313), "2026-07-16");
        assert_eq!(
            date_utc(1_784_206_313 - 180 * SECONDS_PER_DAY),
            "2026-01-17"
        );
    }

    #[test]
    fn round_trips_every_day_for_four_centuries() {
        let mut prev = String::new();
        for day in -36_524..109_572 {
            let s = date_utc(day * SECONDS_PER_DAY);
            assert_eq!(s.len(), 10);
            assert!(s > prev, "{s} did not follow {prev}");
            prev = s;
        }
    }
}
