//! UTC timestamps as the package index writes them: `YYYY-MM-DDTHH:MM:SSZ`.

/// Format milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(millis: u64) -> String {
    let secs = millis / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm), in unsigned arithmetic:
    // shifting the epoch to 0000-03-01 keeps every intermediate non-negative for dates after 1970.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z"); // a leap day
        assert_eq!(rfc3339(1_791_032_431_999), "2026-10-03T13:00:31Z"); // milliseconds truncate
        assert_eq!(rfc3339(4_102_444_799_000), "2099-12-31T23:59:59Z");
        assert!(godspeed_package::is_timestamp(&rfc3339(1_791_032_431_000)));
    }
}
