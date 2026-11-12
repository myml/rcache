//! Cross-process view of a running device.
//!
//! `rcache run` publishes its counters into a small mmap'd file under
//! `/run/rcache/<id>.stats` so that `rcache status` / `rcache metrics` can
//! report on a live device without IPC.
//!
//! Layout: a fixed-size, naturally aligned struct of `u64` slots plus a small
//! header. Everything is plain integers (no pointers), so the file stays
//! meaningful even if read by a different build of rcache.

use crate::metrics::{DeviceInfo, Status, WriteBackInfo};
use crate::stats::{HistogramSnapshot, Stats, StatsSnapshot, LATENCY_EDGES_US};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const MAGIC: u64 = 0x5243_5354_4154_3031; // "RCSTAT01"
pub const VERSION: u64 = 1;

/// Directory holding the live status files.
pub fn run_dir() -> PathBuf {
    std::env::var_os("RCACHE_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/rcache"))
}

pub fn status_path(id: i32) -> PathBuf {
    run_dir().join(format!("{id}.stats"))
}

/// Fixed slots, one per counter: the wire format shared with `status`.
mod slot {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 1;
    pub const DEV_ID: usize = 2;
    pub const UPTIME: usize = 3;
    // Identity (hashed / truncated to u64 where not numeric).
    pub const ORIGIN_BYTES: usize = 4;
    pub const JOURNAL_BYTES: usize = 5;
    pub const JOURNAL_USED: usize = 6;
    pub const LIVE_BLOCKS: usize = 7;
    pub const WB_HIGH: usize = 8;
    pub const WB_LOW: usize = 9;
    pub const WB_MAX: usize = 10;
    pub const WB_STEP: usize = 11;
    pub const WB_IDLE: usize = 12;
    // IO counters.
    pub const READ_BLOCKS: usize = 16;
    pub const READ_HITS: usize = 17;
    pub const READ_MISSES: usize = 18;
    pub const WRITE_BLOCKS: usize = 19;
    pub const FLUSH_OPS: usize = 20;
    pub const DISCARD_OPS: usize = 21;
    pub const FAILED_OPS: usize = 22;
    pub const UNSUPPORTED_OPS: usize = 23;
    // Write-back counters.
    pub const WB_RECORDS: usize = 24;
    pub const WB_BLOCKS: usize = 25;
    pub const WB_SUPERSEDED: usize = 26;
    pub const WB_BYTES: usize = 27;
    pub const WB_ERRORS: usize = 28;
    pub const SPACE_WAITS: usize = 29;
    pub const JOURNAL_FULL_DRAINS: usize = 30;
    pub const CACHE_BYTES_WRITTEN: usize = 31;
    /// Owning process id; readers use it to detect leftovers from a crash.
    pub const PID: usize = 13;
    // Latency histograms: (count, sum_us, max_us) then bucket counts.
    pub const LAT_BASE: usize = 32;
    pub const LAT_STRIDE: usize = 4 + crate::stats::LATENCY_EDGES_US.len() + 1;
    /// read, write, flush, write_back, lock_wait, lock_hold.
    pub const N_LAT: usize = 6;
    pub const TOTAL: usize = LAT_BASE + LAT_STRIDE * N_LAT;
}

/// Shared counters a running device publishes.
pub struct SharedStats {
    map: memmap2::MmapMut,
    path: PathBuf,
    /// Local atomics; the mmap is refreshed from these periodically so the
    /// IO path never touches the mapped page directly.
    stats: Arc<Stats>,
    live_journal_used: AtomicU64,
    live_live_blocks: AtomicU64,
    /// Device description, published once `bind` has chosen a file.
    info: Option<DeviceInfo>,
    bound: bool,
}

impl SharedStats {
    /// A publisher that is not yet bound to an id or a file. The device id is
    /// only known after the kernel creates the device, so the status file is
    /// mapped later by [`SharedStats::bind`].
    pub fn create_pending(stats: Arc<Stats>) -> SharedStats {
        SharedStats {
            map: memmap2::MmapMut::map_anon(0).expect("zero-length map"),
            path: PathBuf::new(),
            stats,
            live_journal_used: AtomicU64::new(0),
            live_live_blocks: AtomicU64::new(0),
            info: None,
            bound: false,
        }
    }

    /// Remember the device description; published on the next `publish()`.
    pub fn set_info(&mut self, info: DeviceInfo) {
        self.info = Some(info);
    }

    /// The id this publisher is bound to, if any.
    pub fn bound_id(&self) -> Option<i32> {
        if self.bound {
            self.info.as_ref().map(|i| i.id)
        } else {
            None
        }
    }

    /// Map the status file for the id the kernel assigned and publish it.
    pub fn bind(&mut self, id: i32) -> std::io::Result<()> {
        if let Some(info) = self.info.as_mut() {
            info.id = id;
            info.device = format!("/dev/ublkb{id}");
        }
        let created = SharedStats::create(id)?;
        self.map = created.map;
        self.path = created.path;
        self.bound = true;
        if let Some(info) = self.info.clone() {
            self.publish_info(&info);
        }
        Ok(())
    }

    /// Create and map the status file for `id`.
    pub fn create(id: i32) -> std::io::Result<SharedStats> {
        let dir = run_dir();
        std::fs::create_dir_all(&dir)?;
        let path = status_path(id);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len((slot::TOTAL * 8) as u64)?;
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        Ok(SharedStats {
            map,
            path,
            stats: Arc::new(Stats::new()),
            live_journal_used: AtomicU64::new(0),
            live_live_blocks: AtomicU64::new(0),
            info: None,
            bound: true,
        })
    }

    /// Describe the device in the file so `status` can show it.
    pub fn publish_info(&mut self, info: &DeviceInfo) {
        let mut w = |i: usize, v: u64| {
            self.map[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        };
        w(slot::MAGIC, MAGIC);
        w(slot::VERSION, VERSION);
        w(slot::DEV_ID, info.id as u64);
        w(slot::ORIGIN_BYTES, info.origin_bytes);
        w(slot::JOURNAL_BYTES, info.journal_bytes);
        w(slot::WB_HIGH, info.write_back.high_pct);
        w(slot::WB_LOW, info.write_back.low_pct);
        w(slot::WB_MAX, info.write_back.max_pct);
        w(slot::WB_STEP, info.write_back.step_blocks as u64);
        w(slot::WB_IDLE, info.write_back.idle_secs);
        w(slot::PID, std::process::id() as u64);
        // Identity strings go to a sidecar so the fixed struct stays simple.
        let side = self.path.with_extension("info");
        let text = format!("{}\n{}\n{}\n", info.device, info.origin, info.cache);
        let _ = std::fs::write(side, text);
    }

    /// Copy the live counters into the mapping. Cheap enough to call on a
    /// timer (once every couple of hundred milliseconds).
    pub fn publish(&mut self) {
        use Ordering::Relaxed;
        if !self.bound {
            return;
        }
        let s = &self.stats;
        let snap = s.snapshot();
        let read = |a: &AtomicU64| a.load(Relaxed);
        let w = |map: &mut memmap2::MmapMut, i: usize, v: u64| {
            map[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        };
        w(&mut self.map, slot::UPTIME, snap.uptime_secs);
        w(
            &mut self.map,
            slot::JOURNAL_USED,
            read(&self.live_journal_used),
        );
        w(
            &mut self.map,
            slot::LIVE_BLOCKS,
            read(&self.live_live_blocks),
        );
        w(&mut self.map, slot::READ_BLOCKS, read(&s.read_blocks));
        w(&mut self.map, slot::READ_HITS, read(&s.read_hits));
        w(&mut self.map, slot::READ_MISSES, read(&s.read_misses));
        w(&mut self.map, slot::WRITE_BLOCKS, read(&s.write_blocks));
        w(&mut self.map, slot::FLUSH_OPS, read(&s.flush_ops));
        w(&mut self.map, slot::DISCARD_OPS, read(&s.discard_ops));
        w(&mut self.map, slot::FAILED_OPS, read(&s.failed_ops));
        w(
            &mut self.map,
            slot::UNSUPPORTED_OPS,
            read(&s.unsupported_ops),
        );
        w(&mut self.map, slot::WB_RECORDS, read(&s.write_back_records));
        w(&mut self.map, slot::WB_BLOCKS, read(&s.write_back_blocks));
        w(
            &mut self.map,
            slot::WB_SUPERSEDED,
            read(&s.write_back_superseded),
        );
        w(&mut self.map, slot::WB_BYTES, read(&s.write_back_bytes));
        w(&mut self.map, slot::WB_ERRORS, read(&s.write_back_errors));
        w(&mut self.map, slot::SPACE_WAITS, read(&s.space_waits));
        w(
            &mut self.map,
            slot::JOURNAL_FULL_DRAINS,
            read(&s.journal_full_drains),
        );
        w(
            &mut self.map,
            slot::CACHE_BYTES_WRITTEN,
            read(&s.cache_bytes_written),
        );

        for (n, h) in [
            snap.read_latency,
            snap.write_latency,
            snap.flush_latency,
            snap.write_back_latency,
            snap.lock_wait,
            snap.lock_hold,
        ]
        .iter()
        .enumerate()
        {
            let base = slot::LAT_BASE + n * slot::LAT_STRIDE;
            w(&mut self.map, base, h.count);
            w(&mut self.map, base + 1, h.sum_us);
            w(&mut self.map, base + 2, h.max_us);
            for (b, cnt) in h.buckets.iter().enumerate() {
                w(&mut self.map, base + 4 + b, *cnt);
            }
        }
        let _ = self.map.flush();
    }

    /// Gauges the writer refreshes before publishing.
    pub fn set_gauges(&mut self, journal_used: u64, live_blocks: u64) {
        self.live_journal_used
            .store(journal_used, Ordering::Relaxed);
        self.live_live_blocks.store(live_blocks, Ordering::Relaxed);
    }
}

/// Read a published status file back. Returns `None` when no device with that
/// id is publishing (stale or missing file).
pub fn read_status(id: i32) -> anyhow::Result<Option<Status>> {
    let path = status_path(id);
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if buf.len() < slot::TOTAL * 8 {
        return Ok(None);
    }
    let r = |i: usize| -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[i * 8..i * 8 + 8]);
        u64::from_le_bytes(b)
    };
    if r(slot::MAGIC) != MAGIC {
        return Ok(None);
    }
    // A file left behind by a crashed process must not look like a live
    // device: check that the publisher is still running.
    let pid = r(slot::PID);
    if pid == 0 || !Path::new(&format!("/proc/{pid}")).exists() {
        return Ok(None);
    }

    let side = path.with_extension("info");
    let text = std::fs::read_to_string(&side).unwrap_or_default();
    let mut lines = text.lines();
    let device = lines.next().unwrap_or("").to_string();
    let origin = lines.next().unwrap_or("").to_string();
    let cache = lines.next().unwrap_or("").to_string();

    let read_hist = |n: usize| {
        let base = slot::LAT_BASE + n * slot::LAT_STRIDE;
        let nbuckets = LATENCY_EDGES_US.len() + 1;
        let buckets: Vec<u64> = (0..nbuckets).map(|b| r(base + 4 + b)).collect();
        let count: u64 = buckets.iter().sum();
        HistogramSnapshot {
            edges_us: LATENCY_EDGES_US.to_vec(),
            buckets,
            count,
            sum_us: r(base + 1),
            max_us: r(base + 2),
        }
    };
    Ok(Some(Status {
        info: DeviceInfo {
            id: r(slot::DEV_ID) as i32,
            device,
            origin,
            cache,
            origin_bytes: r(slot::ORIGIN_BYTES),
            journal_bytes: r(slot::JOURNAL_BYTES),
            journal_used: r(slot::JOURNAL_USED),
            live_blocks: r(slot::LIVE_BLOCKS),
            write_back: WriteBackInfo {
                high_pct: r(slot::WB_HIGH),
                low_pct: r(slot::WB_LOW),
                max_pct: r(slot::WB_MAX),
                step_blocks: r(slot::WB_STEP) as usize,
                idle_secs: r(slot::WB_IDLE),
            },
        },
        stats: StatsSnapshot {
            uptime_secs: r(slot::UPTIME),
            read_blocks: r(slot::READ_BLOCKS),
            read_hits: r(slot::READ_HITS),
            read_misses: r(slot::READ_MISSES),
            write_blocks: r(slot::WRITE_BLOCKS),
            flush_ops: r(slot::FLUSH_OPS),
            discard_ops: r(slot::DISCARD_OPS),
            failed_ops: r(slot::FAILED_OPS),
            unsupported_ops: r(slot::UNSUPPORTED_OPS),
            write_back_records: r(slot::WB_RECORDS),
            write_back_blocks: r(slot::WB_BLOCKS),
            write_back_superseded: r(slot::WB_SUPERSEDED),
            write_back_bytes: r(slot::WB_BYTES),
            write_back_errors: r(slot::WB_ERRORS),
            space_waits: r(slot::SPACE_WAITS),
            journal_full_drains: r(slot::JOURNAL_FULL_DRAINS),
            cache_bytes_written: r(slot::CACHE_BYTES_WRITTEN),
            lock_wait: read_hist(4),
            lock_hold: read_hist(5),
            read_latency: read_hist(0),
            write_latency: read_hist(1),
            flush_latency: read_hist(2),
            write_back_latency: read_hist(3),
        },
    }))
}

/// Ids of every device currently publishing a status file.
pub fn list_ids() -> Vec<i32> {
    let mut ids = Vec::new();
    let Ok(rd) = std::fs::read_dir(run_dir()) else {
        return ids;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(stem) = name.strip_suffix(".stats") {
            if let Ok(id) = stem.parse::<i32>() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    ids
}

/// Remove the status files for `id` (on clean shutdown).
pub fn remove_status(id: i32) {
    let p = status_path(id);
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("info"));
}
