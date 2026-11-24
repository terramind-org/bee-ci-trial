//! Latency samples and their summaries (nearest-rank percentiles).

use std::time::Duration;

/// Summary of a set of latency samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    /// Number of samples.
    pub count: usize,
    /// Median.
    pub p50: Duration,
    /// 90th percentile.
    pub p90: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// Largest sample.
    pub max: Duration,
    /// Arithmetic mean.
    pub mean: Duration,
}

/// The nearest-rank percentile `p` (0 < p ≤ 1) of sorted samples; zero when empty.
pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() as f64 * p).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Summarizes samples (in any order).
pub fn summarize(samples: &[Duration]) -> Summary {
    let mut v = samples.to_vec();
    v.sort();
    let total: Duration = v.iter().sum();
    Summary {
        count: v.len(),
        p50: percentile(&v, 0.50),
        p90: percentile(&v, 0.90),
        p99: percentile(&v, 0.99),
        max: v.last().copied().unwrap_or_default(),
        mean: if v.is_empty() {
            Duration::ZERO
        } else {
            total / v.len() as u32
        },
    }
}

/// Milliseconds with two decimals, e.g. `12.34 ms`.
pub fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let s = summarize(&v);
        assert_eq!(s.count, 100);
        assert_eq!(s.p50, Duration::from_millis(50));
        assert_eq!(s.p90, Duration::from_millis(90));
        assert_eq!(s.p99, Duration::from_millis(99));
        assert_eq!(s.max, Duration::from_millis(100));
        assert_eq!(s.mean, Duration::from_micros(50_500));
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
        assert_eq!(
            percentile(&[Duration::from_millis(3)], 0.99),
            Duration::from_millis(3)
        );
        assert_eq!(summarize(&[]), Summary::default());
        assert_eq!(ms(Duration::from_micros(12_346)), "12.35 ms");
    }
}
