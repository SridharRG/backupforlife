//! Human-friendly durations: `90s`, `15m`, `6h`, `7d`, `2w`, `1h30m`, or bare seconds.

use std::fmt::{self, Write as _};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const UNITS: [(&str, u64); 5] = [
    ("w", 604_800),
    ("d", 86_400),
    ("h", 3_600),
    ("m", 60),
    ("s", 1),
];

/// Parses `1h30m`-style durations. A bare integer is a number of seconds.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("invalid duration `{input}`: number too large"));
    }

    let mut total: u64 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(format!(
                "invalid duration `{input}`: expected a number before `{rest}`"
            ));
        }
        let (number, tail) = rest.split_at(digits);
        let unit_len = tail.bytes().take_while(u8::is_ascii_alphabetic).count();
        let (unit, tail) = tail.split_at(unit_len);
        let factor = match UNITS.iter().find(|(name, _)| *name == unit) {
            Some((_, factor)) => *factor,
            None if unit.is_empty() => {
                return Err(format!(
                    "invalid duration `{input}`: missing unit after `{number}` (use s, m, h, d or w)"
                ));
            }
            None => {
                return Err(format!(
                    "invalid duration `{input}`: unknown unit `{unit}` (use s, m, h, d or w)"
                ));
            }
        };
        let n: u64 = number
            .parse()
            .map_err(|_| format!("invalid duration `{input}`: number too large"))?;
        total = n
            .checked_mul(factor)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(|| format!("invalid duration `{input}`: too large"))?;
        rest = tail;
    }
    Ok(Duration::from_secs(total))
}

/// Formats a duration compactly (`1d2h`, `4m12s`). Sub-second precision is dropped.
pub fn format_duration(d: Duration) -> String {
    let mut secs = d.as_secs();
    if secs == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    // Weeks are skipped on output: "14d" reads better than "2w" in tables.
    for (unit, size) in UNITS.iter().skip(1) {
        if secs >= *size {
            let _ = write!(out, "{}{unit}", secs / size);
            secs %= size;
        }
    }
    out
}

/// A duration that (de)serializes as a human string, or accepts integer seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// `None` for a zero duration, which config uses to mean "disabled".
    pub fn non_zero(self) -> Option<Duration> {
        (!self.0.is_zero()).then_some(self.0)
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration(self.0))
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(self.0))
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Secs(secs) => Ok(Self::from_secs(secs)),
            Raw::Text(text) => parse_duration(&text)
                .map(Self)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_combinations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(1_209_600)
        );
        assert_eq!(
            parse_duration(" 1h30m ").unwrap(),
            Duration::from_secs(5_400)
        );
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "h",
            "10x",
            "1h30",
            "-5m",
            "1.5h",
            "99999999999999999999w",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn formats_round_trip() {
        for secs in [0, 1, 59, 60, 61, 3_599, 3_600, 86_399, 86_400, 694_861] {
            let d = Duration::from_secs(secs);
            assert_eq!(parse_duration(&format_duration(d)).unwrap(), d, "{secs}");
        }
        assert_eq!(format_duration(Duration::from_secs(252)), "4m12s");
        assert_eq!(format_duration(Duration::from_secs(1_209_600)), "14d");
    }

    #[test]
    fn human_duration_accepts_strings_and_integers() {
        let d: HumanDuration = serde_json::from_str("\"6h\"").unwrap();
        assert_eq!(d.0, Duration::from_secs(21_600));
        let d: HumanDuration = serde_json::from_str("30").unwrap();
        assert_eq!(d.0, Duration::from_secs(30));
        assert!(serde_json::from_str::<HumanDuration>("\"soon\"").is_err());
        assert_eq!(HumanDuration::from_secs(0).non_zero(), None);
    }
}
