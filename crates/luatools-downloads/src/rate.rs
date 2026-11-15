//! Throughput and ETA accounting from progress samples.
//!
//! Transfers only know about one chunk at a time; the queue folds the readings into a short
//! sliding window so the interface can show a stable rate instead of a per-chunk spike.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Sliding window over which the rate is measured.
const RATE_WINDOW: Duration = Duration::from_secs(3);

/// Shortest span a rate is divided by. Progress can arrive in bursts milliseconds apart;
/// dividing by such a span reports absurd speeds.
const MIN_SPAN: Duration = Duration::from_millis(500);

/// Retains enough samples to outlast [`RATE_WINDOW`] at the queue's 10 Hz throttle.
const MAX_SAMPLES: usize = 64;

/// Rolling byte-rate meter.
#[derive(Debug, Default)]
pub(crate) struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl RateMeter {
    /// Creates an empty meter.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drops every sample, e.g. when a job is resumed or retried.
    pub(crate) fn reset(&mut self) {
        self.samples.clear();
    }

    /// Folds one reading in and returns the current rate and ETA.
    ///
    /// Returns `None` while no honest measurement is possible yet; callers keep the previous
    /// displayed figures in that case. `eta` is omitted when the total is unknown or already
    /// reached.
    pub(crate) fn record(
        &mut self,
        now: Instant,
        bytes_read: u64,
        total_bytes: Option<u64>,
    ) -> Option<(u64, Option<u64>)> {
        // A restarted transfer reports fewer bytes than the previous attempt. Start over rather
        // than computing a negative delta.
        if self
            .samples
            .back()
            .is_some_and(|&(_, previous)| bytes_read < previous)
        {
            self.samples.clear();
        }

        self.samples.push_back((now, bytes_read));
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }

        // Trim stale samples, but always keep two so the span remains measurable.
        while self.samples.len() > 2 {
            let Some(&(oldest_time, _)) = self.samples.front() else {
                break;
            };
            let Some(&(second_time, _)) = self.samples.get(1) else {
                break;
            };
            let oldest_age = now.saturating_duration_since(oldest_time);
            let second_age = now.saturating_duration_since(second_time);
            if oldest_age > RATE_WINDOW && second_age >= MIN_SPAN {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        if self.samples.len() < 2 {
            return None;
        }

        let (oldest_time, oldest_bytes) = *self.samples.front()?;
        let span = now.saturating_duration_since(oldest_time).as_secs_f64();
        if span < MIN_SPAN.as_secs_f64() {
            return None;
        }

        let delta = bytes_read.saturating_sub(oldest_bytes);
        #[allow(clippy::cast_precision_loss)]
        let rate = (delta as f64 / span).round() as u64;

        let eta = match total_bytes {
            Some(total) if total > bytes_read && rate > 0 => {
                #[allow(clippy::cast_precision_loss)]
                let seconds = ((total - bytes_read) as f64 / rate as f64).ceil() as u64;
                Some(seconds)
            }
            _ => None,
        };

        Some((rate, eta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_reading_cannot_be_measured() {
        let start = Instant::now();
        let mut meter = RateMeter::new();
        assert_eq!(meter.record(start, 0, Some(1000)), None);
    }

    #[test]
    fn a_second_reading_after_the_minimum_span_reports_a_rate() {
        let start = Instant::now();
        let mut meter = RateMeter::new();
        assert_eq!(meter.record(start, 0, Some(3_000_000)), None);

        let measured = meter.record(start + Duration::from_secs(1), 1_000_000, Some(3_000_000));
        assert_eq!(measured, Some((1_000_000, Some(2))));
    }

    #[test]
    fn bursts_closer_than_the_minimum_span_are_ignored() {
        let start = Instant::now();
        let mut meter = RateMeter::new();
        assert_eq!(meter.record(start, 0, Some(100)), None);
        assert_eq!(
            meter.record(start + Duration::from_millis(10), 50, Some(100)),
            None
        );
    }

    #[test]
    fn unknown_totals_report_a_rate_without_an_eta() {
        let start = Instant::now();
        let mut meter = RateMeter::new();
        assert_eq!(meter.record(start, 0, None), None);
        assert_eq!(
            meter.record(start + Duration::from_secs(1), 2_048, None),
            Some((2_048, None))
        );
    }

    #[test]
    fn a_restart_resets_the_window() {
        let start = Instant::now();
        let mut meter = RateMeter::new();
        let _ = meter.record(start, 10_000, Some(20_000));
        assert_eq!(
            meter.record(start + Duration::from_secs(1), 5, Some(20_000)),
            None,
            "a shrinking byte count starts a new attempt"
        );
    }
}
