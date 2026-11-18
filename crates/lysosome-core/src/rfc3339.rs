//! RFC 3339 UTC timestamps (`2026-07-01T10:00:00Z`), whole seconds.

use std::time::SystemTime;

use jiff::{civil::DateTime, tz::TimeZone, Timestamp};

/// Format as `YYYY-MM-DDTHH:MM:SSZ` (sub-second part truncated).
pub(crate) fn format(t: SystemTime) -> String {
    // Times beyond year 9999 can't be represented; clamp.
    let ts = Timestamp::try_from(t).unwrap_or(Timestamp::MAX);
    let ts = Timestamp::from_second(ts.as_second()).unwrap_or(Timestamp::MAX);
    let dt = TimeZone::UTC.to_datetime(ts);
    format!("{}Z", dt.strftime("%Y-%m-%dT%H:%M:%S"))
}

/// Parse an RFC 3339 timestamp. Like the old `humantime` "weak" parser it
/// accepts a space instead of `T` and a missing `Z` (treated as UTC), plus
/// optional fractional seconds. Numeric offsets are not accepted.
pub(crate) fn parse(s: &str) -> Result<SystemTime, String> {
    let body = s.strip_suffix(['Z', 'z']).unwrap_or(s);
    let normalized = match body.get(10..11) {
        Some(" " | "t") => format!("{}T{}", &body[..10], &body[11..]),
        _ => body.to_string(),
    };
    let err = |e: jiff::Error| format!("invalid timestamp {s:?}: {e}");
    let dt: DateTime = normalized.parse().map_err(err)?;
    let ts = TimeZone::UTC.to_timestamp(dt).map_err(err)?;
    Ok(SystemTime::from(ts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn formats_whole_seconds_utc() {
        let t = UNIX_EPOCH + Duration::new(1_782_900_000, 999_000_000);
        assert_eq!(format(t), "2026-07-01T10:00:00Z");
        assert_eq!(format(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parses_strict_and_weak_forms() {
        let want = UNIX_EPOCH + Duration::from_secs(1_782_900_000);
        assert_eq!(parse("2026-07-01T10:00:00Z").unwrap(), want);
        assert_eq!(parse("2026-07-01 10:00:00").unwrap(), want);
        assert_eq!(parse("2026-07-01 10:00:00Z").unwrap(), want);
        assert_eq!(
            parse("2026-07-01T10:00:00.5Z").unwrap(),
            want + Duration::from_millis(500)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("not a date").is_err());
        assert!(parse("").is_err());
        assert!(parse("2026-13-01T10:00:00Z").is_err());
    }
}
