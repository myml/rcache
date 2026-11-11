//! Runtime statistics for an rcache instance.
//!
//! Everything here is a plain atomic counter: the IO path updates them without
//! taking a lock, and a reader (`rcache status`) can snapshot them at any time
//! without disturbing serving.
//!
//! IO is counted in *blocks* (the cache's unit) so hit ratios are meaningful.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Latency buckets in microseconds. Bucket `i` counts operations that took
/// more than `edges[i-1]` and at most `edges[i]`.
pub const LATENCY_EDGES_US: [u64; 16] = [
    10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000,
    500_000, 1_000_000,
];

/// A fixed-bucket latency histogram, cheap enough for the IO path.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}

impl Histogram {
    pub fn new() -> Self {
        Histogram {
            buckets: (0..=LATENCY_EDGES_US.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// Record one observation.
    pub fn observe(&self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        let idx = LATENCY_EDGES_US
            .iter()
            .position(|&edge| us <= edge)
            .unwrap_or(LATENCY_EDGES_US.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            edges_us: LATENCY_EDGES_US.to_vec(),
            buckets: self
                .buckets
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect(),
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time copy of one histogram.
#[derive(Debug, Clone)]
pub struct HistogramSnapshot {
    pub edges_us: Vec<u64>,
    pub buckets: Vec<u64>,
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
}

impl HistogramSnapshot {
    pub fn mean_us(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum_us as f64 / self.count as f64
        }
    }

    /// Approximate quantile in microseconds from the bucket bounds.
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count as f64 * q).ceil() as u64;
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= target {
                // The overflow bucket only says "at least the last edge", so
                // report the true maximum rather than a made-up upper bound.
                if i >= self.edges_us.len() {
                    return self.max_us;
                }
                return self.edges_us[i];
            }
        }
        self.max_us
    }
}

/// All counters for one device.
#[derive(Debug)]
pub struct Stats {
    started: Instant,

    // IO as seen by the block layer (blocks of BLOCK bytes).
    pub read_blocks: AtomicU64,
    pub read_hits: AtomicU64,
    /// Reads that had to be answered by the origin.
    pub read_misses: AtomicU64,
    pub write_blocks: AtomicU64,
    pub flush_ops: AtomicU64,
    pub discard_ops: AtomicU64,
    pub failed_ops: AtomicU64,
    pub unsupported_ops: AtomicU64,

    // Write-back behaviour.
    pub write_back_records: AtomicU64,
    pub write_back_blocks: AtomicU64,
    /// Blocks skipped because a newer version in the same write-back window
    /// superseded them.
    pub write_back_superseded: AtomicU64,
    pub write_back_bytes: AtomicU64,
    pub write_back_errors: AtomicU64,
    /// Times a writer had to wait for the ring to make room.
    pub space_waits: AtomicU64,
    /// Times the journal was full and drained inline.
    pub journal_full_drains: AtomicU64,

    /// Bytes read from / written to the cache file.
    pub cache_bytes_written: AtomicU64,

    pub read_latency: Histogram,
    pub write_latency: Histogram,
    pub flush_latency: Histogram,
    pub write_back_latency: Histogram,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Stats {
            started: Instant::now(),
            read_blocks: AtomicU64::new(0),
            read_hits: AtomicU64::new(0),
            read_misses: AtomicU64::new(0),
            write_blocks: AtomicU64::new(0),
            flush_ops: AtomicU64::new(0),
            discard_ops: AtomicU64::new(0),
            failed_ops: AtomicU64::new(0),
            unsupported_ops: AtomicU64::new(0),
            write_back_records: AtomicU64::new(0),
            write_back_blocks: AtomicU64::new(0),
            write_back_superseded: AtomicU64::new(0),
            write_back_bytes: AtomicU64::new(0),
            write_back_errors: AtomicU64::new(0),
            space_waits: AtomicU64::new(0),
            journal_full_drains: AtomicU64::new(0),
            cache_bytes_written: AtomicU64::new(0),
            read_latency: Histogram::new(),
            write_latency: Histogram::new(),
            flush_latency: Histogram::new(),
            write_back_latency: Histogram::new(),
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Everything a status command needs, captured atomically enough.
    pub fn snapshot(&self) -> StatsSnapshot {
        let read_blocks = self.read_blocks.load(Ordering::Relaxed);
        let read_hits = self.read_hits.load(Ordering::Relaxed);
        StatsSnapshot {
            uptime_secs: self.uptime().as_secs(),
            read_blocks,
            read_hits,
            read_misses: self.read_misses.load(Ordering::Relaxed),
            write_blocks: self.write_blocks.load(Ordering::Relaxed),
            flush_ops: self.flush_ops.load(Ordering::Relaxed),
            discard_ops: self.discard_ops.load(Ordering::Relaxed),
            failed_ops: self.failed_ops.load(Ordering::Relaxed),
            unsupported_ops: self.unsupported_ops.load(Ordering::Relaxed),
            write_back_records: self.write_back_records.load(Ordering::Relaxed),
            write_back_blocks: self.write_back_blocks.load(Ordering::Relaxed),
            write_back_superseded: self.write_back_superseded.load(Ordering::Relaxed),
            write_back_bytes: self.write_back_bytes.load(Ordering::Relaxed),
            write_back_errors: self.write_back_errors.load(Ordering::Relaxed),
            space_waits: self.space_waits.load(Ordering::Relaxed),
            journal_full_drains: self.journal_full_drains.load(Ordering::Relaxed),
            cache_bytes_written: self.cache_bytes_written.load(Ordering::Relaxed),
            read_latency: self.read_latency.snapshot(),
            write_latency: self.write_latency.snapshot(),
            flush_latency: self.flush_latency.snapshot(),
            write_back_latency: self.write_back_latency.snapshot(),
        }
    }
}

/// A consistent-enough copy of [`Stats`] for reporting.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub uptime_secs: u64,
    pub read_blocks: u64,
    pub read_hits: u64,
    pub read_misses: u64,
    pub write_blocks: u64,
    pub flush_ops: u64,
    pub discard_ops: u64,
    pub failed_ops: u64,
    pub unsupported_ops: u64,
    pub write_back_records: u64,
    pub write_back_blocks: u64,
    pub write_back_superseded: u64,
    pub write_back_bytes: u64,
    pub write_back_errors: u64,
    pub space_waits: u64,
    pub journal_full_drains: u64,
    pub cache_bytes_written: u64,
    pub read_latency: HistogramSnapshot,
    pub write_latency: HistogramSnapshot,
    pub flush_latency: HistogramSnapshot,
    pub write_back_latency: HistogramSnapshot,
}

impl StatsSnapshot {
    /// Fraction of reads answered from the journal (0.0 when no reads).
    pub fn hit_ratio(&self) -> f64 {
        if self.read_blocks == 0 {
            0.0
        } else {
            self.read_hits as f64 / self.read_blocks as f64
        }
    }

    pub fn total_ops(&self) -> u64 {
        self.read_blocks + self.write_blocks + self.flush_ops + self.discard_ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_ratio() {
        let s = Stats::new();
        s.read_blocks.fetch_add(10, Ordering::Relaxed);
        s.read_hits.fetch_add(7, Ordering::Relaxed);
        s.read_misses.fetch_add(3, Ordering::Relaxed);
        s.write_blocks.fetch_add(4, Ordering::Relaxed);
        let snap = s.snapshot();
        assert_eq!(snap.read_blocks, 10);
        assert!((snap.hit_ratio() - 0.7).abs() < 1e-9);
        assert_eq!(snap.total_ops(), 14);
    }

    #[test]
    fn empty_ratio_is_zero() {
        let s = Stats::new();
        assert_eq!(s.snapshot().hit_ratio(), 0.0);
    }

    #[test]
    fn histogram_buckets() {
        let h = Histogram::new();
        h.observe(Duration::from_micros(5));
        h.observe(Duration::from_micros(5));
        h.observe(Duration::from_micros(120));
        h.observe(Duration::from_micros(900_000));
        let snap = h.snapshot();
        assert_eq!(snap.count, 4);
        assert_eq!(snap.sum_us, 5 + 5 + 120 + 900_000);
        assert_eq!(snap.max_us, 900_000);
        // 50% of 4 = 2, which falls in the first bucket (<=10us).
        assert_eq!(snap.quantile_us(0.5), 10);
        // 75% of 4 = 3 -> 120us falls in the <=250 bucket.
        assert_eq!(snap.quantile_us(0.75), 250);
        // 100% lands in the (500ms, 1s] bucket: quantiles are bucket bounds,
        // so the answer is the bucket edge, not the exact 900ms sample.
        assert_eq!(snap.quantile_us(1.0), 1_000_000);
        assert!((snap.mean_us() - 225_032.5).abs() < 0.01);
    }

    /// A sample beyond the largest edge must report the real maximum.
    #[test]
    fn histogram_overflow_bucket() {
        let h = Histogram::new();
        h.observe(Duration::from_micros(5_000_000));
        let snap = h.snapshot();
        assert_eq!(snap.quantile_us(1.0), 5_000_000);
        assert_eq!(snap.max_us, 5_000_000);
    }

    #[test]
    fn empty_histogram() {
        let h = Histogram::new();
        let snap = h.snapshot();
        assert_eq!(snap.count, 0);
        assert_eq!(snap.quantile_us(0.9), 0);
        assert_eq!(snap.mean_us(), 0.0);
    }
}
