use chrono::{DateTime, SecondsFormat, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stable metadata timestamp format used by every filesystem-facing result.
pub(crate) fn system_time_to_rfc3339(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    DateTime::<Utc>::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(crate) fn unix_nanos_to_rfc3339(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000);
    let subsec = nanos.rem_euclid(1_000_000_000) as u32;
    DateTime::<Utc>::from_timestamp(seconds, subsec)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_known_subsecond_precision_stably() {
        let time = UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        assert_eq!(system_time_to_rfc3339(time), "2023-11-14T22:13:20.123456Z");
        assert_eq!(
            unix_nanos_to_rfc3339(1_700_000_000_123_456_789),
            "2023-11-14T22:13:20.123456Z"
        );
    }
}
