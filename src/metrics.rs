//! Rendering of [`StatsSnapshot`] and device state in the formats we expose.
//!
//! Three shapes, all derived from the same snapshot so they never disagree:
//!
//! * `text`       - what `rcache status` prints for a human.
//! * `json`       - for scripts and for the HTTP status endpoint.
//! * `prometheus` - the text exposition format, for `/metrics`.

use crate::stats::StatsSnapshot;
use std::fmt::Write as _;

/// Device-level facts that are not counters.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: i32,
    pub device: String,
    pub origin: String,
    pub cache: String,
    pub origin_bytes: u64,
    pub journal_bytes: u64,
    pub journal_used: u64,
    pub live_blocks: u64,
    pub write_back: WriteBackInfo,
}

/// Write-back tuning, echoed back so `status` shows the live policy.
#[derive(Debug, Clone, Copy)]
pub struct WriteBackInfo {
    pub high_pct: u64,
    pub low_pct: u64,
    pub max_pct: u64,
    pub step_blocks: usize,
    pub idle_secs: u64,
}

/// Everything we can report about one running device.
#[derive(Debug, Clone)]
pub struct Status {
    pub info: DeviceInfo,
    pub stats: StatsSnapshot,
}

impl Status {
    pub fn journal_used_pct(&self) -> f64 {
        if self.info.journal_bytes == 0 {
            0.0
        } else {
            self.info.journal_used as f64 * 100.0 / self.info.journal_bytes as f64
        }
    }

    /// Simple health verdict: are we serving, and is anything failing?
    pub fn healthy(&self) -> bool {
        self.stats.failed_ops == 0 && self.stats.write_back_errors == 0
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn percent(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// Human readable report.
pub fn render_text(s: &Status) -> String {
    let mut o = String::new();
    let i = &s.info;
    let st = &s.stats;

    let _ = writeln!(o, "device            {}", i.device);
    let _ = writeln!(o, "  id              {}", i.id);
    let _ = writeln!(
        o,
        "  origin          {} ({})",
        i.origin,
        human_bytes(i.origin_bytes)
    );
    let _ = writeln!(
        o,
        "  cache           {} ({})",
        i.cache,
        human_bytes(i.journal_bytes)
    );
    let _ = writeln!(
        o,
        "  journal in use  {} ({:.1}%)  live blocks {}",
        human_bytes(i.journal_used),
        s.journal_used_pct(),
        i.live_blocks
    );
    let _ = writeln!(o, "  uptime          {}s", st.uptime_secs);
    let _ = writeln!(o);
    let _ = writeln!(o, "io");
    let _ = writeln!(
        o,
        "  reads           {} blocks  (hit {} / miss {})  hit ratio {}",
        st.read_blocks,
        st.read_hits,
        st.read_misses,
        percent(st.hit_ratio())
    );
    let _ = writeln!(o, "  writes          {} blocks", st.write_blocks);
    let _ = writeln!(
        o,
        "  cache written   {}",
        human_bytes(st.cache_bytes_written)
    );
    let _ = writeln!(o, "  total ops       {}", st.total_ops());
    let _ = writeln!(o, "  flushes         {}", st.flush_ops);
    let _ = writeln!(o, "  discards        {}", st.discard_ops);
    let _ = writeln!(
        o,
        "  failures        {} (write-back {})",
        st.failed_ops, st.write_back_errors
    );
    let _ = writeln!(o);
    let _ = writeln!(o, "write-back");
    let _ = writeln!(
        o,
        "  blocks written  {}  ({} superseded, {} bytes)",
        st.write_back_blocks,
        st.write_back_superseded,
        human_bytes(st.write_back_bytes)
    );
    let _ = writeln!(o, "  records drained {}", st.write_back_records);
    let _ = writeln!(
        o,
        "  pressure        {} space waits, {} journal-full drains",
        st.space_waits, st.journal_full_drains
    );
    let _ = writeln!(
        o,
        "  policy          high {}% / low {}% / max {}%, step {} blocks, idle {}s",
        i.write_back.high_pct,
        i.write_back.low_pct,
        i.write_back.max_pct,
        i.write_back.step_blocks,
        i.write_back.idle_secs
    );
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "latency (us)      p50      p90      p99      max     mean"
    );
    for (name, h) in [
        ("read", &st.read_latency),
        ("write", &st.write_latency),
        ("flush", &st.flush_latency),
        ("write-back", &st.write_back_latency),
    ] {
        let _ = writeln!(
            o,
            "{:<16} {:>7}  {:>7}  {:>7}  {:>7}  {:>7.0}",
            name,
            h.quantile_us(0.50),
            h.quantile_us(0.90),
            h.quantile_us(0.99),
            h.max_us,
            h.mean_us()
        );
    }
    // Lock contention: when the wait dominates the hold the bottleneck is
    // contention (shard the lock or use async IO); when the hold dominates it
    // is a long critical section (do less under the lock).
    if st.lock_wait.count > 0 {
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "lock (us)         p50      p90      p99      max     mean"
        );
        for (name, h) in [("wait", &st.lock_wait), ("hold", &st.lock_hold)] {
            let _ = writeln!(
                o,
                "{:<16} {:>7}  {:>7}  {:>7}  {:>7}  {:>7.0}",
                name,
                h.quantile_us(0.50),
                h.quantile_us(0.90),
                h.quantile_us(0.99),
                h.max_us,
                h.mean_us()
            );
        }
    }
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "health            {}",
        if s.healthy() { "ok" } else { "degraded" }
    );
    o
}

fn json_hist(h: &crate::stats::HistogramSnapshot) -> String {
    let mut parts = vec![
        format!("\"count\":{}", h.count),
        format!("\"mean_us\":{:.1}", h.mean_us()),
        format!("\"max_us\":{}", h.max_us),
        format!("\"p50_us\":{}", h.quantile_us(0.50)),
        format!("\"p90_us\":{}", h.quantile_us(0.90)),
        format!("\"p99_us\":{}", h.quantile_us(0.99)),
    ];
    // Cumulative buckets, matching the Prometheus histogram convention.
    let mut cumulative = 0u64;
    let mut buckets = Vec::with_capacity(h.buckets.len());
    for (i, n) in h.buckets.iter().enumerate() {
        cumulative += n;
        match h.edges_us.get(i) {
            Some(le) => buckets.push(format!("{{\"le_us\":{le},\"count\":{cumulative}}}")),
            None => buckets.push(format!("{{\"le_us\":null,\"count\":{cumulative}}}")),
        }
    }
    parts.push(format!("\"buckets\":[{}]", buckets.join(",")));
    format!("{{{}}}", parts.join(","))
}

/// Machine readable report.
pub fn render_json(s: &Status) -> String {
    let i = &s.info;
    let st = &s.stats;
    let mut o = String::with_capacity(1024);
    let _ = write!(
        o,
        "{{\"device\":\"{dev}\",\"id\":{id},\"origin\":\"{origin}\",\"origin_bytes\":{oob},\
\"cache\":\"{cache}\",\"journal_bytes\":{jb},\"journal_used\":{ju},\
\"journal_used_pct\":{jp:.2},\"live_blocks\":{lb},\"uptime_secs\":{up},\"healthy\":{healthy},",
        dev = i.device,
        id = i.id,
        origin = i.origin,
        oob = i.origin_bytes,
        cache = i.cache,
        jb = i.journal_bytes,
        ju = i.journal_used,
        jp = s.journal_used_pct(),
        lb = i.live_blocks,
        up = st.uptime_secs,
        healthy = s.healthy(),
    );
    let _ = write!(
        o,
        "\"write_back\":{{\"high_pct\":{},\"low_pct\":{},\"max_pct\":{},\"step_blocks\":{},\"idle_secs\":{}}},",
        i.write_back.high_pct,
        i.write_back.low_pct,
        i.write_back.max_pct,
        i.write_back.step_blocks,
        i.write_back.idle_secs
    );
    let _ = write!(
        o,
        "\"io\":{{\"read_blocks\":{},\"read_hits\":{},\"read_misses\":{},\"hit_ratio\":{:.6},\
\"write_blocks\":{},\"flush_ops\":{},\"discard_ops\":{},\"failed_ops\":{},\"unsupported_ops\":{}}},",
        st.read_blocks,
        st.read_hits,
        st.read_misses,
        st.hit_ratio(),
        st.write_blocks,
        st.flush_ops,
        st.discard_ops,
        st.failed_ops,
        st.unsupported_ops
    );
    let _ = write!(
        o,
        "\"write_back_stats\":{{\"records\":{},\"blocks\":{},\"superseded\":{},\"bytes\":{},\
\"errors\":{},\"space_waits\":{},\"journal_full_drains\":{}}},",
        st.write_back_records,
        st.write_back_blocks,
        st.write_back_superseded,
        st.write_back_bytes,
        st.write_back_errors,
        st.space_waits,
        st.journal_full_drains
    );
    let _ = write!(
        o,
        "\"latency\":{{\"read\":{},\"write\":{},\"flush\":{},\"write_back\":{}}}}}",
        json_hist(&st.read_latency),
        json_hist(&st.write_latency),
        json_hist(&st.flush_latency),
        json_hist(&st.write_back_latency)
    );
    o
}

/// Prometheus text exposition.
///
/// Counters use the `_total` suffix and are monotonic for the process
/// lifetime; gauges (journal usage) fluctuate.
pub fn render_prometheus(devices: &[Status]) -> String {
    let mut o = String::new();
    let header = |o: &mut String, name: &str, kind: &str, help: &str| {
        let _ = writeln!(o, "# HELP {name} {help}");
        let _ = writeln!(o, "# TYPE {name} {kind}");
    };

    header(
        &mut o,
        "rcache_up",
        "gauge",
        "1 if the device is being served",
    );
    header(
        &mut o,
        "rcache_healthy",
        "gauge",
        "1 if no IO or write-back errors occurred",
    );
    header(
        &mut o,
        "rcache_uptime_seconds",
        "gauge",
        "Seconds since the device was created",
    );
    header(
        &mut o,
        "rcache_origin_bytes",
        "gauge",
        "Size of the origin device",
    );
    header(
        &mut o,
        "rcache_journal_bytes",
        "gauge",
        "Size of the journal ring",
    );
    header(
        &mut o,
        "rcache_journal_used_bytes",
        "gauge",
        "Journal bytes currently holding records",
    );
    header(
        &mut o,
        "rcache_journal_used_ratio",
        "gauge",
        "Journal usage as a fraction of the ring",
    );
    header(
        &mut o,
        "rcache_live_blocks",
        "gauge",
        "Blocks whose newest version lives in the journal",
    );
    header(
        &mut o,
        "rcache_read_blocks_total",
        "counter",
        "Blocks read from the cache device",
    );
    header(
        &mut o,
        "rcache_read_hits_total",
        "counter",
        "Reads served from the journal",
    );
    header(
        &mut o,
        "rcache_read_misses_total",
        "counter",
        "Reads served from the origin",
    );
    header(
        &mut o,
        "rcache_write_blocks_total",
        "counter",
        "Blocks written into the journal",
    );
    header(
        &mut o,
        "rcache_flush_total",
        "counter",
        "FLUSH requests handled",
    );
    header(
        &mut o,
        "rcache_discard_total",
        "counter",
        "DISCARD requests handled",
    );
    header(
        &mut o,
        "rcache_failed_ops_total",
        "counter",
        "IO requests that failed",
    );
    header(
        &mut o,
        "rcache_unsupported_ops_total",
        "counter",
        "IO requests with an unsupported op",
    );
    header(
        &mut o,
        "rcache_write_back_records_total",
        "counter",
        "Journal records written back",
    );
    header(
        &mut o,
        "rcache_write_back_blocks_total",
        "counter",
        "Blocks written to the origin",
    );
    header(
        &mut o,
        "rcache_write_back_superseded_total",
        "counter",
        "Blocks skipped as stale during write-back",
    );
    header(
        &mut o,
        "rcache_write_back_bytes_total",
        "counter",
        "Bytes written to the origin",
    );
    header(
        &mut o,
        "rcache_write_back_errors_total",
        "counter",
        "Origin write failures",
    );
    header(
        &mut o,
        "rcache_space_waits_total",
        "counter",
        "Times a write waited for journal space",
    );
    header(
        &mut o,
        "rcache_journal_full_drains_total",
        "counter",
        "Times the journal was drained inline",
    );
    header(
        &mut o,
        "rcache_latency_us",
        "histogram",
        "Operation latency in microseconds",
    );

    for s in devices {
        let i = &s.info;
        let st = &s.stats;
        let l = format!("device=\"{}\",id=\"{}\"", i.device, i.id);
        let _ = writeln!(o, "rcache_up{{{l}}} 1");
        let _ = writeln!(
            o,
            "rcache_healthy{{{l}}} {}",
            if s.healthy() { 1 } else { 0 }
        );
        let _ = writeln!(o, "rcache_uptime_seconds{{{l}}} {}", st.uptime_secs);
        let _ = writeln!(o, "rcache_origin_bytes{{{l}}} {}", i.origin_bytes);
        let _ = writeln!(o, "rcache_journal_bytes{{{l}}} {}", i.journal_bytes);
        let _ = writeln!(o, "rcache_journal_used_bytes{{{l}}} {}", i.journal_used);
        let _ = writeln!(
            o,
            "rcache_journal_used_ratio{{{l}}} {:.6}",
            s.journal_used_pct() / 100.0
        );
        let _ = writeln!(o, "rcache_live_blocks{{{l}}} {}", i.live_blocks);
        let _ = writeln!(o, "rcache_read_blocks_total{{{l}}} {}", st.read_blocks);
        let _ = writeln!(o, "rcache_read_hits_total{{{l}}} {}", st.read_hits);
        let _ = writeln!(o, "rcache_read_misses_total{{{l}}} {}", st.read_misses);
        let _ = writeln!(o, "rcache_write_blocks_total{{{l}}} {}", st.write_blocks);
        let _ = writeln!(o, "rcache_flush_total{{{l}}} {}", st.flush_ops);
        let _ = writeln!(o, "rcache_discard_total{{{l}}} {}", st.discard_ops);
        let _ = writeln!(o, "rcache_failed_ops_total{{{l}}} {}", st.failed_ops);
        let _ = writeln!(
            o,
            "rcache_unsupported_ops_total{{{l}}} {}",
            st.unsupported_ops
        );
        let _ = writeln!(
            o,
            "rcache_write_back_records_total{{{l}}} {}",
            st.write_back_records
        );
        let _ = writeln!(
            o,
            "rcache_write_back_blocks_total{{{l}}} {}",
            st.write_back_blocks
        );
        let _ = writeln!(
            o,
            "rcache_write_back_superseded_total{{{l}}} {}",
            st.write_back_superseded
        );
        let _ = writeln!(
            o,
            "rcache_write_back_bytes_total{{{l}}} {}",
            st.write_back_bytes
        );
        let _ = writeln!(
            o,
            "rcache_write_back_errors_total{{{l}}} {}",
            st.write_back_errors
        );
        let _ = writeln!(o, "rcache_space_waits_total{{{l}}} {}", st.space_waits);
        let _ = writeln!(
            o,
            "rcache_journal_full_drains_total{{{l}}} {}",
            st.journal_full_drains
        );

        for (op, h) in [
            ("read", &st.read_latency),
            ("write", &st.write_latency),
            ("flush", &st.flush_latency),
            ("write_back", &st.write_back_latency),
        ] {
            let mut cumulative = 0u64;
            for (i, n) in h.buckets.iter().enumerate() {
                cumulative += n;
                let le = h.edges_us.get(i).copied();
                let le_s = match le {
                    Some(e) => e.to_string(),
                    None => "+Inf".to_string(),
                };
                let _ = writeln!(
                    o,
                    "rcache_latency_us_bucket{{{l},op=\"{op}\",le=\"{le_s}\"}} {cumulative}"
                );
            }
            let _ = writeln!(o, "rcache_latency_us_sum{{{l},op=\"{op}\"}} {}", h.sum_us);
            let _ = writeln!(o, "rcache_latency_us_count{{{l},op=\"{op}\"}} {}", h.count);
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> Status {
        let stats = crate::stats::Stats::new();
        stats
            .read_blocks
            .fetch_add(100, std::sync::atomic::Ordering::Relaxed);
        stats
            .read_hits
            .fetch_add(90, std::sync::atomic::Ordering::Relaxed);
        stats
            .read_misses
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        stats
            .write_blocks
            .fetch_add(50, std::sync::atomic::Ordering::Relaxed);
        stats.read_latency.observe(Duration::from_micros(40));
        Status {
            info: DeviceInfo {
                id: 0,
                device: "/dev/ublkb0".into(),
                origin: "/dev/sdb".into(),
                cache: "/mnt/nvme/c.journal".into(),
                origin_bytes: 2 << 30,
                journal_bytes: 64 << 20,
                journal_used: 16 << 20,
                live_blocks: 4096,
                write_back: WriteBackInfo {
                    high_pct: 50,
                    low_pct: 20,
                    max_pct: 90,
                    step_blocks: 1024,
                    idle_secs: 5,
                },
            },
            stats: stats.snapshot(),
        }
    }

    #[test]
    fn text_mentions_key_fields() {
        let t = render_text(&fixture());
        assert!(t.contains("/dev/ublkb0"));
        assert!(t.contains("hit ratio 90.0%"));
        assert!(t.contains("25.0%"));
        assert!(t.contains("health            ok"));
    }

    #[test]
    fn json_is_parsable_shape() {
        let j = render_json(&fixture());
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert!(j.contains("\"device\":\"/dev/ublkb0\""));
        assert!(j.contains("\"hit_ratio\":0.900000"));
        assert!(j.contains("\"healthy\":true"));
        // Balanced braces is a cheap structural sanity check.
        let opens = j.matches('{').count();
        let closes = j.matches('}').count();
        assert_eq!(opens, closes, "unbalanced json braces");
    }

    #[test]
    fn prometheus_has_types_and_buckets() {
        let p = render_prometheus(&[fixture()]);
        assert!(p.contains("# TYPE rcache_read_blocks_total counter"));
        assert!(p.contains("rcache_read_blocks_total{device=\"/dev/ublkb0\",id=\"0\"} 100"));
        assert!(p.contains("le=\"+Inf\""));
        assert!(
            p.contains("rcache_latency_us_count{device=\"/dev/ublkb0\",id=\"0\",op=\"read\"} 1")
        );
        assert!(p.contains("rcache_up{device=\"/dev/ublkb0\",id=\"0\"} 1"));
    }

    #[test]
    fn healthy_tracks_failures() {
        let mut s = fixture();
        assert!(s.healthy());
        s.stats.failed_ops = 1;
        assert!(!s.healthy());
        assert!(render_text(&s).contains("degraded"));
    }
}
