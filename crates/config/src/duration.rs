//! Human-readable durations in config, such as `"90d"`, `"720h"` or `"1h30m"`.

use std::fmt;
use std::time::Duration;

use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::{InstanceType, Metadata, Schema, SchemaObject, StringValidation};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Regex used in the JSON Schema for duration strings.
const DURATION_PATTERN: &str = r"^([0-9]+(ms|s|m|h|d|w))+$";

/// A duration written as a sequence of `<integer><unit>` parts, with units
/// `ms`, `s`, `m`, `h`, `d` (24h) and `w` (7d). Example: `"1d12h"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigDuration(pub Duration);

/// Error returned when a duration string is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid duration {0:?}: expected e.g. \"90d\", \"720h\" or \"1h30m\"")]
pub struct ParseDurationError(pub String);

impl ConfigDuration {
    /// Creates a duration from whole seconds.
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// Returns the underlying [`Duration`].
    pub const fn get(self) -> Duration {
        self.0
    }
}

impl std::str::FromStr for ConfigDuration {
    type Err = ParseDurationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseDurationError(s.to_string());
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return Err(err());
        }
        let mut total_ms: u64 = 0;
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                return Err(err());
            }
            let n: u64 = s[start..i].parse().map_err(|_| err())?;
            let unit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let ms_per_unit: u64 = match &s[unit_start..i] {
                "ms" => 1,
                "s" => 1_000,
                "m" => 60_000,
                "h" => 3_600_000,
                "d" => 86_400_000,
                "w" => 604_800_000,
                _ => return Err(err()),
            };
            total_ms = n
                .checked_mul(ms_per_unit)
                .and_then(|v| total_ms.checked_add(v))
                .ok_or_else(err)?;
        }
        Ok(Self(Duration::from_millis(total_ms)))
    }
}

impl fmt::Display for ConfigDuration {
    /// Formats using the largest unit that represents the value exactly.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        for (unit, size) in [
            ("w", 604_800_000u128),
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1_000),
        ] {
            if ms != 0 && ms % size == 0 {
                return write!(f, "{}{unit}", ms / size);
            }
        }
        if ms == 0 {
            return f.write_str("0s");
        }
        write!(f, "{ms}ms")
    }
}

impl Serialize for ConfigDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ConfigDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ConfigDuration {
    fn schema_name() -> String {
        "Duration".to_string()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            string: Some(Box::new(StringValidation {
                pattern: Some(DURATION_PATTERN.to_string()),
                ..Default::default()
            })),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "A duration such as \"90d\", \"720h\" or \"1h30m\" (units: ms, s, m, h, d, w)."
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        let d: ConfigDuration = "90d".parse().unwrap();
        assert_eq!(d.get(), Duration::from_secs(90 * 86_400));
        let d: ConfigDuration = "1h30m".parse().unwrap();
        assert_eq!(d.get(), Duration::from_secs(5_400));
        assert_eq!(d.to_string(), "90m");
        assert_eq!(ConfigDuration::from_secs(720 * 3600).to_string(), "30d");
        assert_eq!(
            "250ms".parse::<ConfigDuration>().unwrap().to_string(),
            "250ms"
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "90",
            "d",
            "1x",
            "1h 30m",
            "-1h",
            "99999999999999999999w",
        ] {
            assert!(s.parse::<ConfigDuration>().is_err(), "{s}");
        }
    }
}
