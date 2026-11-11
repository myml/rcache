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
use std::sync::atomic::{AtomicBool, Ordering};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crate::cache::Cache;
use crate::log::BLOCK;

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

impl Default for WriteBack {
    fn default() -> Self {
        WriteBack {
            high_watermark: 50,
            low_watermark: 20,
            max_watermark: 90,
            step_blocks: 1024, // 4 MiB at BLOCK = 4096
            idle: std::time::Duration::from_secs(5),
        }
    }
}

/// Everything a queue thread needs.
struct Shared {
    cache: Arc<Mutex<Cache>>,
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
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
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
/// Each step writes at most `step_blocks` blocks, so a big batch of dirty data
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
            let start_drain = forced
                || pct >= wb.high_watermark
                || last_change.elapsed() >= wb.idle;
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
                c.sync().and_then(|()| c.write_back_step(wb.step_blocks))
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
    pub fn run(self, drain: Arc<AtomicBool>) -> Result<()> {
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

        let cache = Arc::new(Mutex::new(open_cache(&self.cache, &self.origin)?));
        let wake = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let wb = self.write_back;
        let max_io = BLOCK as usize * 64;
        let shared = Arc::new(Shared {
            cache: cache.clone(),
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
            dev.tgt.params.basic.attrs = libublk::sys::UBLK_ATTR_VOLATILE_CACHE
                | libublk::sys::UBLK_ATTR_FUA;
            dev.tgt.params.basic.logical_bs_shift = 12;
            dev.tgt.params.basic.physical_bs_shift = 12;
            dev.tgt.params.basic.dev_sectors = dev_size >> 9;
            Ok(())
        };

        let s = shared.clone();
        let q_fn = move |qid: u16, dev: &UblkDev| {
            let bufs = Rc::new(dev.alloc_queue_io_bufs());
            let queue = match UblkQueue::new(qid, dev).and_then(|q| {
                q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs)))
            }) {
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
                let buf = unsafe {
                    std::slice::from_raw_parts_mut(b.as_ptr() as *mut u8, b.len())
                };
                let res = handle_command(&s2, q, tag, io, buf, b.as_ptr() as *mut u8);
                if let Err(UblkError::QueueIsDown) = res {
                    log::debug!("queue {qid}: device went down");
                }
            });
        };

        let s = shared.clone();
        let device_fn = move |c: &UblkCtrl| {
            log::info!("rcache: created /dev/ublkb{}", c.dev_info().dev_id);
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
        let mut c = shared.lock();
        if c.pending_appends() > 0 {
            log::debug!("{} appends pending at shutdown, syncing", c.pending_appends());
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
            let r = process_op_shared(&s.cache, Some(space), op, sector + off as u64, n, piece);
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
        process_op_shared(&s.cache, Some(space), op, sector, n, &mut buf[..n])
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
pub fn process_op(
    cache: &Mutex<Cache>,
    op: u32,
    sector: u64,
    bytes: usize,
    buf: &mut [u8],
) -> i32 {
    process_op_shared(cache, None, op, sector, bytes, buf)
}

/// Same as [`process_op`], but with the background-writer hook: a write that
/// finds the journal past the max watermark waits for it to catch up instead
/// of draining inline (which would stall the queue for the whole ring).
pub fn process_op_shared(
    cache: &Mutex<Cache>,
    space: Option<(&SpaceSignal, u64)>,
    op: u32,
    sector: u64,
    bytes: usize,
    buf: &mut [u8],
) -> i32 {
    let lock = || cache.lock().unwrap_or_else(|p| p.into_inner());
    let (blk, nblocks) = byte_range(sector, bytes);
    let aligned = is_aligned(sector, BLOCK) && is_aligned(bytes as u64, BLOCK);

    match op {
        libublk::sys::UBLK_IO_OP_READ => {
            if aligned {
                match lock().read(blk, nblocks, buf) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("read at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            } else {
                // Sub-block read: fetch the covering blocks, copy the slice.
                let off = (sector % BLOCK) as usize;
                match read_slice(&mut lock(), blk, nblocks.min(bytes / BLOCK as usize + 1), off, bytes) {
                    Ok(v) => {
                        buf[..bytes].copy_from_slice(&v);
                        bytes as i32
                    }
                    Err(e) => {
                        log::error!("unaligned read at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            }
        }
        libublk::sys::UBLK_IO_OP_WRITE => {
            if let Some((wake, max_pct)) = space {
                wait_for_space(cache, wake, max_pct);
            }
            if aligned {
                match lock().write(blk, nblocks, buf) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("write at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            } else {
                // Sub-block write: read-modify-write the covering blocks.
                match write_slice(&mut lock(), blk, nblocks, sector, &buf[..bytes]) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("unaligned write at sector {sector}: {e}");
                        -libc::EIO
                    }
                }
            }
        }
        libublk::sys::UBLK_IO_OP_FLUSH => {
            // Everything accepted so far must be on stable storage before the
            // flush reports success.
            let mut c = lock();
            match c.sync().and_then(|()| c.flush()) {
                Ok(()) => 0,
                Err(e) => {
                    log::error!("flush: {e}");
                    -libc::EIO
                }
            }
        }
        libublk::sys::UBLK_IO_OP_DISCARD => match lock().discard(blk, nblocks) {
            Ok(()) => 0,
            Err(e) => {
                log::error!("discard at sector {sector}: {e}");
                -libc::EIO
            }
        },
        other => {
            log::warn!("unsupported ublk op {other} at sector {sector}");
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
fn wait_for_space(
    cache: &Mutex<Cache>,
    wake: &SpaceSignal,
    max_pct: u64,
) {
    let mut asked = wake.0.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        let pct = {
            let c = cache.lock().unwrap_or_else(|p| p.into_inner());
            usage_pct(&c)
        };
        if pct < max_pct {
            return;
        }
        *asked = true;
        wake.1.notify_all();
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
        assert_eq!(process_op(&cache, libublk::sys::UBLK_IO_OP_WRITE, sector, buf.len(), &mut buf), buf.len() as i32);

        // 2. Read them back through the cache (journal hit).
        let mut got = vec![0u8; 3 * BLOCK as usize];
        assert_eq!(process_op(&cache, libublk::sys::UBLK_IO_OP_READ, sector, got.len(), &mut got), got.len() as i32);
        assert_eq!(got, buf);
        // Origin is still stale for the whole range before the flush.
        let stale = origin_block(&origin, 100);
        assert!(stale.iter().all(|&b| b == 0));

        // 3. Read an untouched block: comes from the origin, not the journal.
        let mut zero = vec![0xFFu8; BLOCK as usize];
        assert_eq!(
            process_op(&cache, libublk::sys::UBLK_IO_OP_READ, 200 * BLOCK, zero.len(), &mut zero),
            zero.len() as i32
        );
        assert!(zero.iter().all(|&b| b == 0));

        // 4. FLUSH drains the journal to the origin.
        assert_eq!(process_op(&cache, libublk::sys::UBLK_IO_OP_FLUSH, 0, 0, &mut []), 0);
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
        std::fs::File::create(&origin).unwrap().set_len(4 << 20).unwrap();
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
        let r = process_op(&cache, libublk::sys::UBLK_IO_OP_WRITE, BLOCK + 512, data.len(), &mut data);
        assert_eq!(r, 512);

        // Read it back with a sub-block read.
        let mut got = vec![0u8; 512];
        let r = process_op(&cache, libublk::sys::UBLK_IO_OP_READ, BLOCK + 512, got.len(), &mut got);
        assert_eq!(r, 512);
        assert_eq!(got, vec![0x5Cu8; 512]);

        // The rest of the block must be preserved.
        let mut rest = vec![0u8; BLOCK as usize];
        let r = process_op(&cache, libublk::sys::UBLK_IO_OP_READ, BLOCK, rest.len(), &mut rest);
        assert_eq!(r, BLOCK as i32);
        assert!(rest[..512].iter().all(|&b| b == 0xAA));
        assert!(rest[512..1024].iter().all(|&b| b == 0x5C));
        assert!(rest[1024..].iter().all(|&b| b == 0xAA));

        // Flush and verify the origin got the merged block.
        assert_eq!(process_op(&cache, libublk::sys::UBLK_IO_OP_FLUSH, 0, 0, &mut []), 0);
        let mut o = vec![0u8; BLOCK as usize];
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
        std::fs::File::create(&origin).unwrap().set_len(8 << 20).unwrap();
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
            assert_eq!(ob[..], data[o..o + BLOCK as usize], "block {} wrong", 100 + i);
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
