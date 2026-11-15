//! Human-readable byte and rate formatting.
//!
//! Uses decimal (SI) units to match how download sizes are presented in the interface:
//! `1.4 GB of 2.0 GB`.

use std::fmt;

/// A byte count rendered with a decimal unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteFormat(u64);

impl ByteFormat {
    /// Wraps a byte count.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Returns the raw byte count.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Renders the count together with a per-second suffix.
    #[must_use]
    pub fn per_second(self) -> String {
        format!("{self}/s")
    }
}

impl From<u64> for ByteFormat {
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ByteFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let bytes = self.0;
        if bytes < 1000 {
            return write!(f, "{bytes} B");
        }
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1000.0 && unit < UNITS.len() - 1 {
            value /= 1000.0;
            unit += 1;
        }
        write!(f, "{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_use_bytes_without_decimals() {
        assert_eq!(ByteFormat::new(0).to_string(), "0 B");
        assert_eq!(ByteFormat::new(999).to_string(), "999 B");
    }

    #[test]
    fn decimal_units_keep_one_decimal() {
        assert_eq!(ByteFormat::new(1000).to_string(), "1.0 KB");
        assert_eq!(ByteFormat::new(1_500_000).to_string(), "1.5 MB");
        assert_eq!(ByteFormat::new(2_000_000_000).to_string(), "2.0 GB");
        assert_eq!(ByteFormat::new(1_400_000_000).to_string(), "1.4 GB");
        assert_eq!(ByteFormat::new(3_000_000_000_000).to_string(), "3.0 TB");
    }

    #[test]
    fn rates_add_a_suffix() {
        assert_eq!(ByteFormat::new(4_200_000).per_second(), "4.2 MB/s");
    }

    #[test]
    fn huge_values_stay_in_the_last_unit() {
        assert_eq!(ByteFormat::new(u64::MAX).to_string(), "18446744.1 TB");
    }
}
