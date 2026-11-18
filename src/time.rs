//! Stable time formats at the API and storage boundaries.
//!
//! Jiff owns time arithmetic and time zones. These printers preserve the
//! historical RFC 3339 fraction widths (0/3/6/9) and explicit UTC offsets.
use jiff::Timestamp;

pub trait TimestampExt {
    fn to_rfc3339(self) -> String;
    fn to_rfc3339_millis(self) -> String;
    fn to_rfc3339_z(self) -> String;
}

fn precision(time: Timestamp) -> usize {
    let nanos = time.subsec_nanosecond();
    if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    }
}

impl TimestampExt for Timestamp {
    fn to_rfc3339(self) -> String {
        format!(
            "{:.*}",
            precision(self),
            self.display_with_offset(jiff::tz::Offset::UTC)
        )
    }

    fn to_rfc3339_millis(self) -> String {
        format!("{self:.3}")
    }

    fn to_rfc3339_z(self) -> String {
        format!("{:.*}", precision(self), self)
    }
}

/// Floor to the legacy Unix microsecond unit, including before 1970.
pub fn unix_micros(time: Timestamp) -> i64 {
    time.as_nanosecond().div_euclid(1000) as i64
}

pub mod serde_unix_micros {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(time: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(unix_micros(*time))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        Timestamp::from_microsecond(i64::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// UTC timestamps in manifests and event payloads use `Z`, with fractions in
/// groups of three digits. Reading remains Jiff's RFC 3339 timestamp parser.
pub mod serde_timestamp {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(time: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{:.*}", precision(*time), time))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        Timestamp::deserialize(deserializer)
    }
}

pub mod serde_optional_timestamp {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        time: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match time {
            Some(time) => serializer.serialize_some(&format!("{:.*}", precision(*time), time)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        Option::<Timestamp>::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Times {
        #[serde(with = "serde_timestamp")]
        at: Timestamp,
        #[serde(default, with = "serde_optional_timestamp")]
        optional: Option<Timestamp>,
    }

    #[test]
    fn wire_formats_preserve_grouped_precision_and_utc_suffixes() {
        for fraction in ["", ".100", ".123400", ".123456789"] {
            let text = format!("2026-10-01T08:00:00{fraction}Z");
            let at: Timestamp = text.parse().unwrap();
            let value = serde_json::to_value(Times {
                at,
                optional: Some(at),
            })
            .unwrap();
            assert_eq!(value["at"], text);
            assert_eq!(value["optional"], text);
            assert_eq!(at.to_rfc3339(), text.replace('Z', "+00:00"));
            let old: Times = serde_json::from_value(value).unwrap();
            assert_eq!(old.at, at);
            assert_eq!(old.optional, Some(at));
        }
        let at: Timestamp = "1969-12-31T23:59:59.123400Z".parse().unwrap();
        assert_eq!(at.to_rfc3339(), "1969-12-31T23:59:59.123400+00:00");
        assert_eq!(at.to_rfc3339_millis(), "1969-12-31T23:59:59.123Z");
    }
}
