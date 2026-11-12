//! ublk target: expose the write-back cache as `/dev/ublkbN`.
//!
//! IO comes in as ublk commands on a queue; every block-sized command is
//! served by [`Cache`]:
//!
//! * READ  -> journal hit? reads are served from the SSD copy, else origin.
//! * WRITE -> appended to the SSD journal, origin updated later.
//! * FLUSH -> journal drained to the origin, then the command completes.
//!
//! Flow control: `ublk` hands us one command per tag at a time (sync style
//! `wait_and_handle_io`). A command is processed inline; the only thing that
//! ever happens asynchronously is our own journal/origin IO, which uses the
//! queue's io_uring directly.

use anyhow::{Context, Result};
use libublk::ctrl::UblkCtrl;
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crate::cache::Cache;
use crate::log::BLOCK;
use crate::shared::SharedStats;
use crate::stats::Stats;

/// How eagerly the journal is written back to the origin.
#[derive(Debug, Clone, Copy)]
pub struct WriteBack {
    /// Journal usage above which write-back runs regardless of idleness, as a
    /// percentage of the ring.
    pub high_watermark: u64,
    /// Stop writing back once usage drops below this percentage.
    pub low_watermark: u64,
    /// Block a write when usage is above this percentage, waiting for the
    /// background writer to make room.
    pub max_watermark: u64,
    /// Blocks written back per step. Kept modest so IO stays responsive.
    pub step_blocks: usize,
    /// How long the journal may sit dirty before an idle write-back kicks in.
    pub idle: std::time::Duration,
}

/// Upper bound on how long one write-back step may hold the cache lock.
///
/// The block count alone does not bound the pause: a spinning or network
/// origin can take milliseconds per block. Time-boxing keeps writer latency
/// predictable regardless of how slow the origin is.
pub const MAX_STEP_TIME: std::time::Duration = std::time::Duration::from_millis(20);

impl Default for WriteBack {
    fn default() -> Self {
        WriteBack {
            high_watermark: 50,
            low_watermark: 20,
            max_watermark: 90,
            // A slow origin can take milliseconds per block, and every
            // write-back step holds the cache lock. Small bursts keep the
            // queue responsive: measured on an NFS-backed origin, 1024
            // blocks/step gave ~485 IOPS while 64 gave ~5000.
            step_blocks: 64, // 256 KiB at BLOCK = 4096
            idle: std::time::Duration::from_secs(5),
        }
    }
}

/// Everything a queue thread needs.
struct Shared {
    cache: Arc<Mutex<Cache>>,
    stats: Arc<Stats>,
    /// Set once a write had to wait for space, to wake the background writer.
    wake: Arc<SpaceSignal>,
    /// Journal usage above which a write waits instead of draining inline.
    max_watermark: u64,
}

fn usage_pct(c: &Cache) -> u64 {
    c.used_pct()
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Wraps a cache guard so the wait for the lock and the time spent holding it
/// are both recorded. Knowing which of the two dominates decides whether the
/// right fix is sharding the lock or shortening a critical section.
struct TimedGuard<'a> {
    guard: Option<MutexGuard<'a, Cache>>,
    stats: &'a Stats,
    acquired: std::time::Instant,
}

impl TimedGuard<'_> {
    fn get(&mut self) -> &mut Cache {
        self.guard.as_mut().expect("guard taken before drop")
    }
}

impl Drop for TimedGuard<'_> {
    fn drop(&mut self) {
        self.stats.lock_hold.observe(self.acquired.elapsed());
        // Dropping the guard releases the lock right after this.
        self.guard.take();
    }
}

fn lock_timed<'a>(cache: &'a Mutex<Cache>, stats: &'a Stats) -> TimedGuard<'a> {
    let t0 = std::time::Instant::now();
    let guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    let acquired = std::time::Instant::now();
    stats.lock_wait.observe(acquired - t0);
    TimedGuard {
        guard: Some(guard),
        stats,
        acquired,
    }
}

/// Signals a writer that is waiting for journal space.
type SpaceSignal = (Mutex<bool>, std::sync::Condvar);

/// Handle for stopping the background writer.
struct WriteBackThread {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WriteBackThread {
    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Background write-back.
///
/// Two triggers, checked every tick:
///
/// * **high watermark / a waiting writer** — usage above `high_watermark`, or
///   a write blocked at `max_watermark`; write back until `low_watermark`.
/// * **idle** — the journal has not changed for `idle`, so dirty data from a
///   quiet workload still reaches the origin without waiting for a flush.
///
/// Each step writes at most `step_blocks` blocks (and at most `MAX_STEP_TIME`), so a big batch of dirty data
/// does not monopolise the origin.
fn spawn_write_back(
    cache: Arc<Mutex<Cache>>,
    wake: Arc<SpaceSignal>,
    wb: WriteBack,
) -> WriteBackThread {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        let mut last_used = u64::MAX;
        let mut last_change = std::time::Instant::now();
        // Set while an idle write-back is running; it keeps going until the
        // journal is empty so a quiet workload does not leave dirty data
        // sitting in the ring for one step at a time.
        let mut draining = false;

        while !stop2.load(Ordering::SeqCst) {
            let (used, pct) = {
                let c = cache.lock().unwrap_or_else(|p| p.into_inner());
                (c.log.used(), usage_pct(&c))
            };
            let forced = *wake.0.lock().unwrap_or_else(|p| p.into_inner());
            if used != last_used {
                last_used = used;
                last_change = std::time::Instant::now();
            }

            if used == 0 {
                draining = false;
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            // Start an idle drain after a quiet period, or when the ring is
            // filling up, or because a writer is blocked at the max watermark.
            let start_drain =
                forced || pct >= wb.high_watermark || last_change.elapsed() >= wb.idle;
            if !draining && start_drain {
                draining = true;
                log::debug!("write-back: start drain at {pct}% (forced={forced})");
            }
            if !draining {
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            // Consume the wake flag and make room for exactly one step.
            {
                let mut guard = wake.0.lock().unwrap_or_else(|p| p.into_inner());
                *guard = false;
            }
            let res = {
                let mut c = cache.lock().unwrap_or_else(|p| p.into_inner());
                // Durability before reclaiming: the records we are about to
                // write back must not be forgotten from the journal first.
                // Bound the lock hold: a slow origin makes a block-count
                // budget alone insufficient.
                let deadline = std::time::Instant::now() + MAX_STEP_TIME;
                c.sync()
                    .and_then(|()| c.write_back_step_within(wb.step_blocks, Some(deadline)))
            };
            match res {
                Ok(0) => {
                    draining = false;
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                Ok(_) => {
                    // Release writers waiting at the max watermark.
                    {
                        let _g = wake.0.lock().unwrap_or_else(|p| p.into_inner());
                        wake.1.notify_all();
                    }
                    // An idle drain runs until the journal is (nearly) empty;
                    // the low watermark only decides when a drain *starts*
                    // being optional. Writers waiting at the max watermark
                    // keep it going regardless.
                    let (remaining, log_len) = {
                        let c = cache.lock().unwrap_or_else(|p| p.into_inner());
                        (c.log.used(), c.log.hdr.log_len)
                    };
                    let waiting = *wake.0.lock().unwrap_or_else(|p| p.into_inner());
                    let low_water = wb.low_watermark.saturating_mul(log_len) / 100;
                    if remaining <= low_water && !waiting {
                        draining = false;
                    }
                }
                Err(e) => {
                    log::error!("write-back step failed: {e:#}");
                    draining = false;
                    thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    });
    WriteBackThread {
        stop,
        handle: Some(handle),
    }
}

/// Open (formatting if needed) the cache journal for `origin`.
fn open_cache(cache: &Path, origin: &Path) -> Result<Cache> {
    Cache::open(cache, origin)
        .with_context(|| format!("open cache {} for {}", cache.display(), origin.display()))
}

pub struct Target {
    pub id: i32,
    pub cache: PathBuf,
    pub origin: PathBuf,
    pub queues: u16,
    pub depth: u16,
    pub write_back: WriteBack,
}

impl Target {
    /// Create the device and serve requests until it is stopped or `stop()`
    /// is triggered by a signal.
    pub fn run(self, drain: Arc<AtomicBool>, metrics_listen: Option<String>) -> Result<()> {
        // A ublk id can be held by the kernel after a crashed process, in
        // which case creation fails with EOPNOTSUPP. Retry once with the next
        // free id instead of leaving the operator to clean up by hand.
        let mut target = self;
        let first_id = resolve_id(target.id)?;
        target.id = first_id;

        // Keep a copy of what a retry needs before `run_once` consumes the
        // target, so a stale id does not force the operator to intervene.
        let retry_spec = (
            target.cache.clone(),
            target.origin.clone(),
            target.queues,
            target.depth,
            target.write_back,
        );
        match target.run_once(drain, metrics_listen) {
            Err(e) if is_stale_id_error(&e) => {
                log::warn!("device id {first_id} is unusable ({e:#}); retrying with another id");
                let next = next_free_id_excluding(first_id)?;
                log::info!("retrying with device id {next}");
                Target {
                    id: next,
                    cache: retry_spec.0,
                    origin: retry_spec.1,
                    queues: retry_spec.2,
                    depth: retry_spec.3,
                    write_back: retry_spec.4,
                }
                .run_once(Arc::new(AtomicBool::new(false)), None)
            }
            other => other,
        }
    }

    fn run_once(self, drain: Arc<AtomicBool>, metrics_listen: Option<String>) -> Result<()> {
        let cache = open_cache(&self.cache, &self.origin)?;
        let dev_size = cache.log.hdr.origin_size;
        log::info!(
            "rcache: origin {} ({} MiB), journal {} MiB, {} live blocks",
            self.origin.display(),
            dev_size >> 20,
            cache.log.hdr.log_len >> 20,
            cache.index_len()
        );
        drop(cache);

        let wake = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let wb = self.write_back;
        let max_io = BLOCK as usize * 64;
        // The status file is published only once the ublk device exists, so a
        // failed start leaves nothing behind that `status` could mistake for
        // a running device.
        let stats = Arc::new(Stats::new());
        let opened = open_cache(&self.cache, &self.origin)?;
        let journal_bytes = opened.log.hdr.log_len;
        let cache = Arc::new(Mutex::new(opened.with_stats(stats.clone())));
        // The status file is created from `device_fn`, once the kernel has
        // told us the id it actually assigned (which need not be the id we
        // asked for). Until then nothing is published.
        let mut publisher = SharedStats::create_pending(stats.clone());
        let info = crate::metrics::DeviceInfo {
            id: self.id,
            device: format!("/dev/ublkb{}", self.id),
            origin: self.origin.display().to_string(),
            cache: self.cache.display().to_string(),
            origin_bytes: dev_size,
            journal_bytes,
            journal_used: 0,
            live_blocks: 0,
            write_back: crate::metrics::WriteBackInfo {
                high_pct: wb.high_watermark,
                low_pct: wb.low_watermark,
                max_pct: wb.max_watermark,
                step_blocks: wb.step_blocks,
                idle_secs: wb.idle.as_secs(),
            },
        };
        publisher.set_info(info);
        let publisher = Arc::new(Mutex::new(publisher));
        let shared = Arc::new(Shared {
            cache: cache.clone(),
            stats: stats.clone(),
            wake: wake.clone(),
            max_watermark: wb.max_watermark,
        });
        let writer = spawn_write_back(cache.clone(), wake, wb);

        let ctrl = libublk::ctrl::UblkCtrlBuilder::default()
            .name("rcache")
            .id(self.id)
            .depth(self.depth)
            .nr_queues(self.queues)
            .io_buf_bytes(max_io as u32)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
            .build()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create ublk device id={}: {e:?}\n                     hint: another /dev/ublkb*/ublkc* may still be in use; \
                     check `ls /sys/class/ublk-char` and stop leftover devices \
                     with `rcache stop --id N`",
                    self.id
                )
            })?;

        let tgt_init = move |dev: &mut UblkDev| -> Result<(), UblkError> {
            dev.set_default_params(dev_size);
            // The backing block size is BLOCK; the block layer must not
            // send us anything that is not a multiple of it.
            // Declare a *stable* write cache plus FUA support.
            //
            // With UBLK_ATTR_VOLATILE_CACHE the block layer serialises every
            // fsync through an explicit FLUSH request, and the userspace
            // round-trip for that request does not complete reliably under
            // the synchronous queue model. Declaring FUA instead makes the
            // block layer attach UBLK_IO_F_FUA to writes that need to be
            // durable, which we honour with a journal drain.
            dev.tgt.params.basic.attrs =
                libublk::sys::UBLK_ATTR_VOLATILE_CACHE | libublk::sys::UBLK_ATTR_FUA;
            dev.tgt.params.basic.logical_bs_shift = 12;
            dev.tgt.params.basic.physical_bs_shift = 12;
            dev.tgt.params.basic.dev_sectors = dev_size >> 9;
            Ok(())
        };

        let s = shared.clone();
        let q_fn = move |qid: u16, dev: &UblkDev| {
            let bufs = Rc::new(dev.alloc_queue_io_bufs());
            let queue = match UblkQueue::new(qid, dev)
                .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
            {
                Ok(q) => q,
                Err(e) => {
                    log::error!("queue {qid}: setup failed: {e:?}");
                    return;
                }
            };
            let s2 = s.clone();
            let bufs2 = bufs.clone();
            queue.wait_and_handle_io(move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
                let b = &bufs2[tag as usize];
                // The per-tag buffer is `max_io_buf_bytes` wide; the command
                // itself may be shorter, so hand the op only its own bytes.
                // Always hand over the whole tag buffer: a command may be
                // larger than the buffer, and handle_command serves it in
                // windows of this same memory.
                //
                // SAFETY: one in-flight command per tag per queue thread, so no
                // other handle touches this buffer while we serve `tag`.
                let buf = unsafe { std::slice::from_raw_parts_mut(b.as_ptr() as *mut u8, b.len()) };
                let res = handle_command(&s2, q, tag, io, buf, b.as_ptr() as *mut u8);
                if let Err(UblkError::QueueIsDown) = res {
                    log::debug!("queue {qid}: device went down");
                }
            });
        };

        // Publisher: refresh the shared status file so `rcache status` and
        // `/metrics` see live numbers without talking to the queue threads.
        {
            let sp = publisher.clone();
            let sc = cache.clone();
            thread::spawn(move || loop {
                thread::sleep(std::time::Duration::from_millis(200));
                let (used, live) = {
                    let c = sc.lock().unwrap_or_else(|p| p.into_inner());
                    (c.log.used(), c.index_len() as u64)
                };
                let mut p = sp.lock().unwrap_or_else(|p| p.into_inner());
                p.set_gauges(used, live);
                p.publish();
            });
        }

        // Optional HTTP monitoring endpoint.
        let http_stop = Arc::new(AtomicBool::new(false));
        if let Some(listen) = metrics_listen {
            let addr: std::net::SocketAddr = listen
                .parse()
                .with_context(|| format!("invalid --metrics-listen `{listen}`"))?;
            let stop = http_stop.clone();
            thread::spawn(move || {
                if let Err(e) = crate::httpd::serve(addr, stop) {
                    log::error!("metrics endpoint failed: {e:#}");
                }
            });
        }

        let s = shared.clone();
        let pub_for_device = publisher.clone();
        let device_fn = move |c: &UblkCtrl| {
            // The device exists now: bind the status file to the id the kernel
            // assigned and publish the first sample.
            let real_id = c.dev_info().dev_id as i32;
            let mut p = pub_for_device.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = p.bind(real_id) {
                log::warn!("cannot create status file for id {real_id}: {e}");
            }
            p.publish();
            drop(p);
            log::info!("rcache: created /dev/ublkb{real_id}");
            // Drain watcher: a signal asks for the journal to be pushed to the
            // origin, nothing more. Stopping the device is left to `rcache
            // stop` / device removal, because a mounted filesystem still
            // needs this device to answer IO.
            let cache = s.cache.clone();
            let d = drain.clone();
            thread::spawn(move || loop {
                while !d.swap(false, Ordering::SeqCst) {
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                match cache.lock().unwrap_or_else(|p| p.into_inner()).flush() {
                    Ok(()) => log::info!("rcache: journal drained to origin"),
                    Err(e) => log::error!("rcache: drain failed: {e:#}"),
                }
            });
        };

        let _ = ctrl.run_target(tgt_init, q_fn, device_fn);

        // Stop the background writer, then do the final drain.
        writer.stop();
        http_stop.store(true, Ordering::SeqCst);
        {
            let p = publisher.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(id) = p.bound_id() {
                crate::shared::remove_status(id);
            }
        }
        let mut c = shared.lock();
        if c.pending_appends() > 0 {
            log::debug!(
                "{} appends pending at shutdown, syncing",
                c.pending_appends()
            );
        }
        c.flush().context("final journal drain failed")?;
        log::info!("rcache: stopped, journal drained");
        Ok(())
    }
}

/// Handle one ublk command.
fn handle_command(
    s: &Shared,
    q: &UblkQueue,
    tag: u16,
    io: &UblkIOCtx,
    buf: &mut [u8],
    tag_buf_addr: *mut u8,
) -> Result<(), UblkError> {
    // A CQE that is our own target IO (journal/origin read or write) arriving
    // is not a new command: the sync submit path already waited for it.
    if io.is_tgt_io() {
        return Ok(());
    }

    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    // A FLUSH carries a negative/neutral "sector" (the driver does not define
    // one), so compute the byte offset without overflowing.
    let sector = iod.start_sector.wrapping_mul(512);
    let bytes = (iod.nr_sectors as usize) * 512;
    let space = (s.wake.as_ref(), s.max_watermark);

    // One ublk command may be larger than this tag's buffer. Serve it in
    // windows of the tag buffer: the data lives there, so each window is
    // processed in place (no copy) before moving on.
    let buf_len = buf.len();
    let res = if buf_len > 0 && bytes > buf_len && op != libublk::sys::UBLK_IO_OP_FLUSH {
        let mut res: i32 = 0;
        let mut off = 0usize;
        while off < bytes {
            let n = (bytes - off).min(buf_len);
            let piece = &mut buf[..n];
            let r = process_op_shared(
                &s.cache,
                &s.stats,
                Some(space),
                op,
                sector + off as u64,
                n,
                piece,
            );
            if r < 0 {
                res = r;
                break;
            }
            res += r;
            off += n;
        }
        res
    } else {
        // The command fits in the tag buffer: hand over exactly its bytes.
        let n = bytes.min(buf_len);
        process_op_shared(
            &s.cache,
            &s.stats,
            Some(space),
            op,
            sector,
            n,
            &mut buf[..n],
        )
    };

    // COMMIT must carry the same buffer address the FETCH used. For FLUSH
    // (`bytes == 0`) the buffer we were handed is zero length, so pass the
    // tag's real buffer rather than an empty slice (which would send NULL and
    // wedge the tag).
    let commit = if bytes == 0 || buf_len == 0 {
        BufDesc::RawAddress(tag_buf_addr as *const u8)
    } else {
        // Never hand libublk a slice longer than the tag buffer.
        BufDesc::Slice(&buf[..bytes.min(buf_len)])
    };
    q.complete_io_cmd_unified(tag, commit, Ok(UblkIORes::Result(res)))?;
    Ok(())
}

/// Serve one block command against the cache. Returns the ublk result code
/// (bytes transferred, 0 for flush, or a negative errno).
///
/// Kept independent of ublk so the same path can be exercised from tests.
#[cfg(test)]
pub fn process_op(cache: &Mutex<Cache>, op: u32, sector: u64, bytes: usize, buf: &mut [u8]) -> i32 {
    let stats = Stats::new();
    process_op_shared(cache, &stats, None, op, sector, bytes, buf)
}

/// Same as [`process_op`], but with the background-writer hook: a write that
/// finds the journal past the max watermark waits for it to catch up instead
/// of draining inline (which would stall the queue for the whole ring).
pub fn process_op_shared(
    cache: &Mutex<Cache>,
    stats: &Stats,
    space: Option<(&SpaceSignal, u64)>,
    op: u32,
    sector: u64,
    bytes: usize,
    buf: &mut [u8],
) -> i32 {
    use std::sync::atomic::Ordering::Relaxed;
    let lock = || lock_timed(cache, stats);
    let (blk, nblocks) = byte_range(sector, bytes);
    let aligned = is_aligned(sector, BLOCK) && is_aligned(bytes as u64, BLOCK);
    let started = std::time::Instant::now();

    match op {
        libublk::sys::UBLK_IO_OP_READ => {
            let hits = {
                let mut c = lock();
                (0..nblocks)
                    .filter(|i| c.get().is_cached(blk + *i as u64))
                    .count()
            };
            let res = if aligned {
                match lock().get().read(blk, nblocks, buf) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("read at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            } else {
                // Sub-block read: fetch the covering blocks, copy the slice.
                let off = (sector % BLOCK) as usize;
                match read_slice(
                    lock().get(),
                    blk,
                    nblocks.min(bytes / BLOCK as usize + 1),
                    off,
                    bytes,
                ) {
                    Ok(v) => {
                        buf[..bytes].copy_from_slice(&v);
                        bytes as i32
                    }
                    Err(e) => {
                        log::error!("unaligned read at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            };
            stats.read_blocks.fetch_add(nblocks as u64, Relaxed);
            stats.read_hits.fetch_add(hits as u64, Relaxed);
            stats
                .read_misses
                .fetch_add((nblocks - hits) as u64, Relaxed);
            if res < 0 {
                stats.failed_ops.fetch_add(1, Relaxed);
            }
            stats.read_latency.observe(started.elapsed());
            res
        }
        libublk::sys::UBLK_IO_OP_WRITE => {
            if let Some((wake, max_pct)) = space {
                if wait_for_space(cache, stats, wake, max_pct) {
                    stats.space_waits.fetch_add(1, Relaxed);
                }
            }
            let res = if aligned {
                match lock().get().write(blk, nblocks, buf) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("write at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            } else {
                // Sub-block write: read-modify-write the covering blocks.
                match write_slice(lock().get(), blk, nblocks, sector, &buf[..bytes]) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("unaligned write at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            };
            stats.write_blocks.fetch_add(nblocks as u64, Relaxed);
            stats
                .cache_bytes_written
                .fetch_add(nblocks as u64 * BLOCK, Relaxed);
            if res < 0 {
                stats.failed_ops.fetch_add(1, Relaxed);
            }
            stats.write_latency.observe(started.elapsed());
            res
        }
        libublk::sys::UBLK_IO_OP_FLUSH => {
            // Everything accepted so far must be on stable storage before the
            // flush reports success. Syncing the journal is enough for that;
            // draining to the origin here would hold the lock for as long as
            // the slow disk needs (see `Cache::sync_journal`).
            let mut c = lock();
            let res = match c.get().sync_journal() {
                Ok(()) => 0,
                Err(e) => {
                    log::error!("flush: {e}");
                    -libc::EIO
                }
            };
            stats.flush_ops.fetch_add(1, Relaxed);
            if res < 0 {
                stats.failed_ops.fetch_add(1, Relaxed);
            }
            stats.flush_latency.observe(started.elapsed());
            res
        }
        libublk::sys::UBLK_IO_OP_DISCARD => {
            let res = match lock().get().discard(blk, nblocks) {
                Ok(()) => 0,
                Err(e) => {
                    log::error!("discard at sector {sector}: {e}");
                    -libc::EIO
                }
            };
            stats.discard_ops.fetch_add(1, Relaxed);
            if res < 0 {
                stats.failed_ops.fetch_add(1, Relaxed);
            }
            res
        }
        other => {
            log::warn!("unsupported ublk op {other} at sector {sector}");
            stats.unsupported_ops.fetch_add(1, Relaxed);
            -libc::EOPNOTSUPP
        }
    }
}

/// Read `len` bytes starting at `off` inside the block range [blk, blk+nblocks).
fn read_slice(
    c: &mut Cache,
    blk: u64,
    nblocks: usize,
    off: usize,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    let mut whole = vec![0u8; nblocks * BLOCK as usize];
    c.read(blk, nblocks, &mut whole)?;
    Ok(whole[off..off + len].to_vec())
}

/// Overlay `data` at `sector` onto the covering blocks and journal them.
fn write_slice(
    c: &mut Cache,
    blk: u64,
    nblocks: usize,
    sector: u64,
    data: &[u8],
) -> std::io::Result<()> {
    let mut whole = vec![0u8; nblocks * BLOCK as usize];
    c.read(blk, nblocks, &mut whole)?;
    let off = (sector % BLOCK) as usize;
    whole[off..off + data.len()].copy_from_slice(data);
    c.write(blk, nblocks, &whole)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Block until the journal is below `max_pct`, asking the background writer to
/// make room. Returns immediately when there is already space.
/// Returns true if the caller actually had to wait.
/// Make room in the journal for one more write.
///
/// Returns true if the caller had to wait. Waiting alone is not enough: the
/// background writer is deliberately time-boxed, so on a slow origin it can
/// fall behind a burst of writes. If a short wait does not clear the pressure,
/// the caller does a bounded drain itself instead of blocking indefinitely
/// (which would look like a hung device).
fn wait_for_space(cache: &Mutex<Cache>, stats: &Stats, wake: &SpaceSignal, max_pct: u64) -> bool {
    let mut waited = false;
    let mut asked = wake.0.lock().unwrap_or_else(|p| p.into_inner());
    let mut rounds = 0u32;
    loop {
        let pct = {
            let c = cache.lock().unwrap_or_else(|p| p.into_inner());
            usage_pct(&c)
        };
        if pct < max_pct {
            return waited;
        }
        waited = true;
        rounds += 1;

        // Ask the background writer first...
        *asked = true;
        wake.1.notify_all();

        // ...but after a couple of rounds, help instead of waiting forever.
        //
        // The help must be *bounded*: a full `flush()` here blocks the queue
        // thread for as long as the whole journal takes to reach the origin
        // (seconds to minutes on a slow disk), which looks like a hung
        // device. A few time-boxed steps per round keep other writers moving
        // while still making progress.
        if rounds >= 3 {
            let mut c = cache.lock().unwrap_or_else(|p| p.into_inner());
            stats
                .journal_full_drains
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Give the drain a slightly larger slice than the background
            // writer: this caller cannot make progress otherwise. Still
            // bounded, so other queue threads are not starved.
            let deadline = std::time::Instant::now() + MAX_STEP_TIME * 4;
            let _ = c
                .sync()
                .and_then(|()| c.write_back_step_within(256, Some(deadline)));
            drop(c);
            continue;
        }

        let (guard, _) = wake
            .1
            .wait_timeout(asked, std::time::Duration::from_millis(50))
            .unwrap_or_else(|p| p.into_inner());
        asked = guard;
    }
}

/// `value` is a whole multiple of `unit`.
#[inline]
fn is_aligned(value: u64, unit: u64) -> bool {
    value % unit == 0
}

/// Turn a byte range into (first block touched, number of blocks covered).
/// Never panics: sub-block offsets are rounded down/up to whole blocks.
fn byte_range(byte_off: u64, bytes: usize) -> (u64, usize) {
    let first = byte_off / BLOCK;
    let last = byte_off.saturating_add(bytes as u64).div_ceil(BLOCK);
    (first, (last - first) as usize)
}

/// Turn a negative id into a free one.
fn resolve_id(id: i32) -> Result<i32> {
    if id < 0 {
        next_free_id()
    } else {
        Ok(id)
    }
}

/// ublk reports a stale id as `EOPNOTSUPP`; our error carries the message.
fn is_stale_id_error(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("failed to create ublk device") && (s.contains("-95") || s.contains("EOPNOTSUPP"))
}

/// Next free id, pretending `used` is taken (used when a retry is needed).
fn next_free_id_excluding(used: i32) -> Result<i32> {
    let mut used_set: std::collections::HashSet<i32> = std::collections::HashSet::new();
    used_set.insert(used);
    if let Ok(rd) = std::fs::read_dir("/sys/class/ublk-char") {
        for e in rd.flatten() {
            if let Some(n) = e
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("ublkc"))
                .and_then(|s| s.parse::<i32>().ok())
            {
                used_set.insert(n);
            }
        }
    }
    (0..1024)
        .find(|id| !used_set.contains(id))
        .context("no free ublk id")
}

/// Allocate the lowest unused ublk device id.
///
/// ublk reports a stale id as busy, so pick one that is neither in sysfs nor
/// still publishing a status file.
pub fn next_free_id() -> Result<i32> {
    let mut used: std::collections::HashSet<i32> = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/ublk-char") {
        for e in rd.flatten() {
            if let Some(n) = e
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("ublkc"))
                .and_then(|s| s.parse::<i32>().ok())
            {
                used.insert(n);
            }
        }
    }
    used.extend(crate::shared::list_ids());
    (0..1024)
        .find(|id| !used.contains(id))
        .context("no free ublk id")
}

/// Stop an existing device by id.
///
/// Uses the asynchronous delete command: it does not wait for remaining
/// openers or in-flight IO, so a stuck device can still be torn down.
pub fn remove(id: i32) -> Result<()> {
    let ctrl = libublk::ctrl::UblkCtrl::new_simple(id)
        .map_err(|e| anyhow::anyhow!("open ublk ctrl {id}: {e:?}"))?;
    ctrl.del_dev_async()
        .map_err(|e| anyhow::anyhow!("delete ublk device {id}: {e:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    fn origin_block(path: &Path, blk: u64) -> Vec<u8> {
        let mut f = std::fs::File::open(path).unwrap();
        f.seek(SeekFrom::Start(blk * BLOCK)).unwrap();
        let mut b = vec![0u8; BLOCK as usize];
        f.read_exact(&mut b).unwrap();
        b
    }

    /// Drive the exact same op path the ublk queue thread uses, with a plain
    /// file standing in for the origin block device.
    #[test]
    fn end_to_end_writeback() {
        let dir = std::env::temp_dir().join(format!("rcache-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("origin.img");
        std::fs::File::create(&origin)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        let cache_path = dir.join("cache.img");
        std::fs::File::create(&cache_path)
            .unwrap()
            .set_len(crate::log::MIN_CACHE_SIZE)
            .unwrap();

        let cache = Mutex::new(Cache::open(&cache_path, &origin).unwrap());

        // 1. Write 3 blocks at block 100.
        let mut buf = vec![0u8; 3 * BLOCK as usize];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        let sector = 100 * BLOCK;
        assert_eq!(
            process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_WRITE,
                sector,
                buf.len(),
                &mut buf
            ),
            buf.len() as i32
        );

        // 2. Read them back through the cache (journal hit).
        let mut got = vec![0u8; 3 * BLOCK as usize];
        assert_eq!(
            process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_READ,
                sector,
                got.len(),
                &mut got
            ),
            got.len() as i32
        );
        assert_eq!(got, buf);
        // Origin is still stale for the whole range before the flush.
        let stale = origin_block(&origin, 100);
        assert!(stale.iter().all(|&b| b == 0));

        // 3. Read an untouched block: comes from the origin, not the journal.
        let mut zero = vec![0xFFu8; BLOCK as usize];
        assert_eq!(
            process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_READ,
                200 * BLOCK,
                zero.len(),
                &mut zero
            ),
            zero.len() as i32
        );
        assert!(zero.iter().all(|&b| b == 0));

        // 4. FLUSH makes the journal durable but does not drain it: the data
        //    is safe once the journal is synced, and draining here would hold
        //    the lock for as long as the origin takes.
        assert_eq!(
            process_op(&cache, libublk::sys::UBLK_IO_OP_FLUSH, 0, 0, &mut []),
            0
        );
        assert_eq!(cache.lock().unwrap().pending_appends(), 0);
        assert_eq!(
            cache.lock().unwrap().index_len(),
            3,
            "FLUSH must keep the journal contents"
        );
        // The data is still served correctly from the journal.
        let mut again = vec![0u8; buf.len()];
        assert_eq!(
            process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_READ,
                100 * BLOCK,
                buf.len(),
                &mut again
            ),
            buf.len() as i32
        );
        assert_eq!(again, buf);
        // A full drain (what shutdown does) does reach the origin.
        cache.lock().unwrap().flush().unwrap();
        for i in 0..3u64 {
            let ob = origin_block(&origin, 100 + i);
            let off = i as usize * BLOCK as usize;
            assert_eq!(ob[..], buf[off..off + BLOCK as usize], "block {}", 100 + i);
        }
        assert_eq!(cache.lock().unwrap().index_len(), 0);

        // 5. Sub-block requests round to whole blocks instead of panicking.
        assert_eq!(byte_range(3584, 512), (0, 1));
        assert_eq!(byte_range(4096, 4096), (1, 1));
        assert_eq!(byte_range(4096, 8192), (1, 2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unaligned_write_read() {
        let dir = std::env::temp_dir().join(format!("rcache-e2e3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("origin.img");
        std::fs::File::create(&origin)
            .unwrap()
            .set_len(4 << 20)
            .unwrap();
        let cache_path = dir.join("cache.img");
        let cache = Mutex::new(Cache::open(&cache_path, &origin).unwrap());

        // Pre-fill block 1 on the origin with 0xAA so read-modify-write has a
        // base to preserve.
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::File::options().write(true).open(&origin).unwrap();
            f.write_all_at(&[0xAAu8; BLOCK as usize], BLOCK).unwrap();
        }
        let c = Cache::open(&cache_path, &origin).unwrap();
        drop(std::mem::replace(&mut *cache.lock().unwrap(), c));

        // Write 512 bytes at block 1 + 512 (sub-block, like mkfs does).
        let mut data = vec![0x5Cu8; 512];
        let r = process_op(
            &cache,
            libublk::sys::UBLK_IO_OP_WRITE,
            BLOCK + 512,
            data.len(),
            &mut data,
        );
        assert_eq!(r, 512);

        // Read it back with a sub-block read.
        let mut got = vec![0u8; 512];
        let r = process_op(
            &cache,
            libublk::sys::UBLK_IO_OP_READ,
            BLOCK + 512,
            got.len(),
            &mut got,
        );
        assert_eq!(r, 512);
        assert_eq!(got, vec![0x5Cu8; 512]);

        // The rest of the block must be preserved.
        let mut rest = vec![0u8; BLOCK as usize];
        let r = process_op(
            &cache,
            libublk::sys::UBLK_IO_OP_READ,
            BLOCK,
            rest.len(),
            &mut rest,
        );
        assert_eq!(r, BLOCK as i32);
        assert!(rest[..512].iter().all(|&b| b == 0xAA));
        assert!(rest[512..1024].iter().all(|&b| b == 0x5C));
        assert!(rest[1024..].iter().all(|&b| b == 0xAA));

        // The merged block must be readable through the cache right away: a
        // sub-block write is a read-modify-write, so the untouched bytes keep
        // their old value. FLUSH only makes the journal durable, so read back
        // through the device rather than from the origin.
        let mut o = vec![0u8; BLOCK as usize];
        assert_eq!(
            process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_READ,
                BLOCK,
                o.len(),
                &mut o
            ),
            BLOCK as i32
        );
        assert_eq!(o[512], 0x5C, "the written byte is missing");
        assert_eq!(o[0], 0xAA, "bytes before the patch were lost");
        assert_eq!(o[2048], 0xAA, "bytes after the patch were lost");

        // Draining reaches the origin with the same merged content.
        cache.lock().unwrap().flush().unwrap();
        {
            use std::os::unix::fs::FileExt;
            std::fs::File::open(&origin)
                .unwrap()
                .read_exact_at(&mut o, BLOCK)
                .unwrap();
        }
        assert_eq!(o[512], 0x5C);
        assert_eq!(o[0], 0xAA);
        assert_eq!(o[2048], 0xAA);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command larger than the tag buffer must be served in windows and land
    /// completely on the origin; truncating it silently would corrupt data.
    #[test]
    fn windowed_command_larger_than_buffer() {
        let dir = std::env::temp_dir().join(format!("rcache-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("origin.img");
        std::fs::File::create(&origin)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        let cache_path = dir.join("cache.img");
        let cache = Mutex::new(Cache::open(&cache_path, &origin).unwrap());

        let chunk = 64 * BLOCK as usize; // the tag buffer size
        let blocks = 512usize; // 2 MiB, far bigger than `chunk`
        let mut data = vec![0u8; blocks * BLOCK as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i * 7 % 253) as u8;
        }

        // Windowed write, exactly like handle_command does it.
        let mut off = 0usize;
        while off < data.len() {
            let n = (data.len() - off).min(chunk);
            let mut piece = data[off..off + n].to_vec();
            let r = process_op(
                &cache,
                libublk::sys::UBLK_IO_OP_WRITE,
                (100 * BLOCK) + off as u64,
                n,
                &mut piece,
            );
            assert_eq!(r, n as i32);
            off += n;
        }
        cache.lock().unwrap().flush().unwrap();

        for i in 0..blocks {
            let ob = origin_block(&origin, 100 + i as u64);
            let o = i * BLOCK as usize;
            assert_eq!(
                ob[..],
                data[o..o + BLOCK as usize],
                "block {} wrong",
                100 + i
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_op_reports_eopnotsupp() {
        let dir = std::env::temp_dir().join(format!("rcache-e2e2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("origin.img");
        std::fs::File::create(&origin)
            .unwrap()
            .set_len(4 << 20)
            .unwrap();
        let cache_path = dir.join("cache.img");
        let cache = Mutex::new(Cache::open(&cache_path, &origin).unwrap());
        let mut buf = [0u8; 8];
        let r = process_op(&cache, 0xee, 0, 0, &mut buf);
        assert_eq!(r, -libc::EOPNOTSUPP);
        std::fs::remove_dir_all(&dir).ok();
    }
}
