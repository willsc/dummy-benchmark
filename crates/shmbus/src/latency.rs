//! Tiny fixed-bucket latency histogram.
//!
//! Stores samples in log-linear buckets covering 500ns .. 100ms plus an
//! overflow bin. Cheap to record (one branch + one increment) and produces
//! `min / p50 / p90 / p99 / p99.9 / max / mean` on demand.

/// Upper bound (inclusive) of each bucket, in nanoseconds. Must be sorted.
const BUCKET_BOUNDS_NS: [u64; 21] = [
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    u64::MAX,
];

pub struct LatencyHistogram {
    buckets: [u64; BUCKET_BOUNDS_NS.len()],
    min_ns: u64,
    max_ns: u64,
    sum_ns: u128,
    count: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: [0; BUCKET_BOUNDS_NS.len()],
            min_ns: u64::MAX,
            max_ns: 0,
            sum_ns: 0,
            count: 0,
        }
    }

    #[inline]
    pub fn record(&mut self, latency_ns: u64) {
        // Binary search would be a bit faster but linear over 21 entries is
        // already a couple of cache lines and avoids branch mispredicts.
        let idx = BUCKET_BOUNDS_NS
            .iter()
            .position(|&b| latency_ns <= b)
            .unwrap_or(BUCKET_BOUNDS_NS.len() - 1);
        self.buckets[idx] += 1;
        if latency_ns < self.min_ns {
            self.min_ns = latency_ns;
        }
        if latency_ns > self.max_ns {
            self.max_ns = latency_ns;
        }
        self.sum_ns += latency_ns as u128;
        self.count += 1;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Returns the upper bound of the bucket containing the given quantile.
    /// `q` is in [0.0, 1.0]. Returns 0 if no samples have been recorded.
    fn quantile(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * q).ceil() as u64;
        let target = target.max(1);
        let mut cum: u64 = 0;
        for (i, &b) in self.buckets.iter().enumerate() {
            cum += b;
            if cum >= target {
                return BUCKET_BOUNDS_NS[i];
            }
        }
        self.max_ns
    }

    pub fn summary(&self) -> Summary {
        Summary {
            count: self.count,
            min_ns: if self.count == 0 { 0 } else { self.min_ns },
            max_ns: self.max_ns,
            mean_ns: if self.count == 0 {
                0
            } else {
                (self.sum_ns / self.count as u128) as u64
            },
            p50_ns: self.quantile(0.50),
            p90_ns: self.quantile(0.90),
            p99_ns: self.quantile(0.99),
            p999_ns: self.quantile(0.999),
        }
    }
}

#[derive(Copy, Clone, Debug, serde::Serialize)]
pub struct Summary {
    pub count: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} min={} p50={} p90={} p99={} p99.9={} max={} mean={}",
            self.count,
            fmt_ns(self.min_ns),
            fmt_ns(self.p50_ns),
            fmt_ns(self.p90_ns),
            fmt_ns(self.p99_ns),
            fmt_ns(self.p999_ns),
            fmt_ns(self.max_ns),
            fmt_ns(self.mean_ns),
        )
    }
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.2}µs", ns as f64 / 1e3)
    } else {
        format!("{}ns", ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_min_max_count() {
        let mut h = LatencyHistogram::new();
        h.record(100);
        h.record(2_000);
        h.record(800);
        let s = h.summary();
        assert_eq!(s.count, 3);
        assert_eq!(s.min_ns, 100);
        assert_eq!(s.max_ns, 2_000);
    }

    #[test]
    fn quantiles_land_in_correct_bucket() {
        let mut h = LatencyHistogram::new();
        for _ in 0..99 {
            h.record(900); // bucket: 1µs
        }
        h.record(40_000); // bucket: 50µs — this becomes the p99+ tail
        let s = h.summary();
        assert_eq!(s.p50_ns, 1_000);
        assert_eq!(s.p90_ns, 1_000);
        assert_eq!(s.p99_ns, 1_000);
        assert_eq!(s.p999_ns, 50_000);
    }
}
