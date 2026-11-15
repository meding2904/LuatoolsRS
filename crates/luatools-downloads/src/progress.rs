//! Byte progress values and emission throttling.

use std::time::{Duration, Instant};

/// One progress reading from a transfer.
///
/// `total_bytes` is `None` when the source does not advertise a length (chunked responses),
/// in which case the interface draws an indeterminate bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferProgress {
    /// Bytes written so far.
    pub bytes_read: u64,
    /// Total expected bytes, when the source declared one.
    pub total_bytes: Option<u64>,
}

impl TransferProgress {
    /// Creates a reading.
    #[must_use]
    pub const fn new(bytes_read: u64, total_bytes: Option<u64>) -> Self {
        Self {
            bytes_read,
            total_bytes,
        }
    }

    /// Completion in `0.0..=1.0`, or `None` while the total length is unknown.
    ///
    /// A declared total of zero is treated as unknown, matching
    /// [`luatools_core::DownloadSnapshot::fraction`].
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        match self.total_bytes {
            Some(total) if total > 0 => {
                Some((self.bytes_read as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

/// Rate limiter for progress callbacks.
///
/// Network loops report once per 80 KiB chunk, which is far more often than an interface can
/// usefully redraw. The queue wraps its event emission in this limiter so a job produces at
/// most ten `DownloadUpdated` events per second.
#[derive(Debug, Clone)]
pub struct ProgressThrottle {
    interval: Duration,
    last_emit: Option<Instant>,
}

impl ProgressThrottle {
    /// Default interval: 100 ms, i.e. ten events per second.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

    /// Creates a throttle with an explicit minimum interval.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_emit: None,
        }
    }

    /// Returns true when a reading may be emitted now, and records the emission.
    ///
    /// The first call always passes. Monotonicity is assumed but not required: a clock that
    /// moves backwards simply suppresses the reading.
    pub fn allow(&mut self, now: Instant) -> bool {
        match self.last_emit {
            Some(last) if now.saturating_duration_since(last) < self.interval => false,
            _ => {
                self.last_emit = Some(now);
                true
            }
        }
    }

    /// Forgets the previous emission, so the next reading is always forwarded.
    pub fn reset(&mut self) {
        self.last_emit = None;
    }
}

impl Default for ProgressThrottle {
    fn default() -> Self {
        Self::new(Self::DEFAULT_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_totals_have_no_fraction() {
        assert_eq!(TransferProgress::new(0, None).fraction(), None);
        assert_eq!(TransferProgress::new(500, None).fraction(), None);
        assert_eq!(TransferProgress::new(500, Some(0)).fraction(), None);
    }

    #[test]
    fn known_totals_produce_a_clamped_fraction() {
        assert_eq!(TransferProgress::new(0, Some(200)).fraction(), Some(0.0));
        assert_eq!(TransferProgress::new(50, Some(200)).fraction(), Some(0.25));
        assert_eq!(TransferProgress::new(200, Some(200)).fraction(), Some(1.0));
        assert_eq!(
            TransferProgress::new(400, Some(200)).fraction(),
            Some(1.0),
            "overshoot is clamped instead of exceeding the bar"
        );
    }

    #[test]
    fn throttle_emits_at_most_ten_events_per_second() {
        let start = Instant::now();
        let mut throttle = ProgressThrottle::default();
        let mut emitted = 0_usize;

        // One thousand readings spread over one second (1 ms apart).
        for millis in 0..1000_u32 {
            let now = start + Duration::from_millis(u64::from(millis));
            if throttle.allow(now) {
                emitted += 1;
            }
        }

        assert!(
            (1..=11).contains(&emitted),
            "expected about ten emissions, got {emitted}"
        );
    }

    #[test]
    fn first_reading_is_always_emitted_and_reset_forgets_history() {
        let start = Instant::now();
        let mut throttle = ProgressThrottle::new(Duration::from_secs(1));
        assert!(throttle.allow(start));
        assert!(!throttle.allow(start + Duration::from_millis(10)));
        assert!(throttle.allow(start + Duration::from_secs(1)));
        throttle.reset();
        assert!(throttle.allow(start + Duration::from_millis(20)));
    }
}
