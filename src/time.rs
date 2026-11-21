//! Timestamps recorded in the index.

use jiff::Timestamp;

/// The current time in UTC as an RFC 3339 timestamp with second precision,
/// such as `2026-01-02T03:04:05Z`.
pub(crate) fn now() -> String {
    format_rfc3339(Timestamp::now())
}

fn format_rfc3339(timestamp: Timestamp) -> String {
    timestamp.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_with_second_precision() {
        let timestamp = Timestamp::new(1_700_000_000, 999_999_999).unwrap();
        assert_eq!(format_rfc3339(timestamp), "2023-11-14T22:13:20Z");
        assert_eq!(
            format_rfc3339(Timestamp::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
    }
}
