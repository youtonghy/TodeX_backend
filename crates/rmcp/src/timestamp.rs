use jiff::{Timestamp, fmt::temporal::DateTimePrinter, tz::Offset};

pub(crate) fn rfc3339(timestamp: Timestamp) -> String {
    // Preserve the previous RFC 3339 spelling: +00:00 and 0, 3, 6, or 9 fractional digits.
    let nanos = timestamp.subsec_nanosecond();
    let precision = if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    };
    DateTimePrinter::new()
        .precision(Some(precision))
        .timestamp_with_offset_to_string(&timestamp, Offset::UTC)
}
