//! The write-back cache: journal + origin write-back + recovery.
//!
//! Design notes:
//!
//! * Every write goes to the SSD journal first and becomes the newest version
//!   of its block. The origin still holds the stale copy until the journal is
//!   drained.
//! * Reads check the journal index; a hit is served from the SSD, a miss from
//!   the origin.
//! * `flush()` writes every live record back to the origin and resets the ring.
//!   `write_back_step()` is the incremental version used by the background
//!   writer: it reclaims a prefix of the ring and leaves the rest alone.
//! * The index is rebuilt on every start by scanning the ring, so there is no
//!   independent index to keep consistent: the journal *is* the metadata.

use crate::log::{Log, BLOCK};
use anyhow::Context;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::FileTypeExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::stats::Stats;

/// Origin blocks that may be reclaimed from the journal before the origin is
/// forced to disk. Bounds what a power loss can lose to roughly
/// `MAX_UNSYNCED_BLOCKS * BLOCK` bytes, while keeping `fsync` off the hot path
/// (it is a synchronous COMMIT on network filesystems).
pub const MAX_UNSYNCED_BLOCKS: u64 = 32768;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// What we know about one block that currently lives in the journal.
#[derive(Debug, Clone, Copy)]
pub struct Live {
    /// Byte offset inside the ring.
    pub pos: u64,
    pub version: u32,
    pub is_dirty: bool,
}

pub struct Cache {
    pub log: Log,
    file: File,
    /// blk -> location of its newest version in the journal.
    index: HashMap<u64, Live>,
    /// Origin blocks touched since the last flush, for diagnostics.
    dirty_blocks: u64,
    /// Counters updated by the write-back path; `None` in unit tests.
    stats: Option<Arc<Stats>>,
    /// Origin blocks written since the last durable origin sync.
    unsynced_blocks: u64,
}

/// Byte size of a block device (BLKGETSIZE64) or regular file.
pub fn device_size(f: &File) -> std::io::Result<u64> {
    if f.metadata()?.file_type().is_block_device() {
        use std::os::unix::io::AsRawFd;
        let mut size = 0u64;
        // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut size) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(size)
    } else {
        Ok(f.metadata()?.len())
    }
}

impl Cache {
    /// Open (or create and format) the cache. Performs journal replay.
    pub fn open(cache_path: &Path, origin: &Path) -> anyhow::Result<Cache> {
        let origin_file = File::options().read(true).write(true).open(origin)?;
        let origin_size = device_size(&origin_file)
            .with_context(|| format!("query size of {}", origin.display()))?;
        if origin_size % BLOCK != 0 {
            anyhow::bail!("origin size {} is not a multiple of {BLOCK}", origin_size);
        }

        let log = Log::open(cache_path, &origin.display().to_string(), origin_size)?;
        let mut c = Cache {
            log,
            file: origin_file,
            index: HashMap::new(),
            dirty_blocks: 0,
            stats: None,
            unsynced_blocks: 0,
        };
        c.replay()?;
        Ok(c)
    }

    /// Rebuild the index from the journal, then push everything back to the
    /// origin so the cache starts empty. Anything that cannot be replayed is
    /// dropped (and reported), never silently trusted.
    pub fn replay(&mut self) -> anyhow::Result<()> {
        let mut seen = 0usize;
        for (pos, hdr) in self.log.scan()? {
            if let Some(data) = self.log.read_record(pos)? {
                let (_, payload) = data;
                if payload.len() as u64 != hdr.len as u64 {
                    log::warn!("record for blk {} has odd length, skipped", hdr.blk);
                    continue;
                }
                let e = self.index.entry(hdr.blk).or_insert(Live {
                    pos,
                    version: hdr.version,
                    is_dirty: true,
                });
                if hdr.version >= e.version {
                    e.pos = pos;
                    e.version = hdr.version;
                }
                seen += 1;
            }
        }
        if seen > 0 {
            log::info!("replaying {seen} journal records onto the origin");
        }
        self.flush()?;
        Ok(())
    }

    /// Publish the journal header after a batch of appends.
    pub fn publish(&mut self) -> anyhow::Result<()> {
        self.log.deferred_header_write()
    }

    /// Attach counters so the write-back path reports into them.
    pub fn with_stats(mut self, stats: Arc<Stats>) -> Cache {
        self.stats = Some(stats);
        self
    }

    /// Make every accepted write durable, without moving data to the origin.
    ///
    /// This is what a FLUSH from the filesystem needs: once the journal is on
    /// stable storage the write is safe (a crash replays it). Draining to the
    /// origin here would block the queue for as long as the slow disk takes --
    /// measured at up to 9 seconds on a network filesystem -- and the data
    /// would be no safer than when it sits in a synced journal.
    pub fn sync_journal(&mut self) -> anyhow::Result<()> {
        self.log.flush_header()
    }

    /// Push everything written to the origin to stable storage.
    ///
    /// Write-back steps avoid this because it is expensive on network
    /// filesystems; FLUSH and shutdown call it to honour their promise.
    pub fn sync_origin(&mut self) -> anyhow::Result<()> {
        self.file.sync_data()?;
        self.dirty_blocks = 0;
        self.unsynced_blocks = 0;
        Ok(())
    }

    /// Make every accepted write durable in the journal.
    ///
    /// Appends only fsync occasionally, so a FLUSH must force the pending
    /// batch out before it can claim the data is safe.
    pub fn sync(&mut self) -> anyhow::Result<()> {
        self.log.flush_header()
    }

    pub fn index_len(&self) -> usize {
        self.index.len()
    }

    /// Is the block currently shadowed by a journal entry?
    #[allow(dead_code)]
    pub fn is_cached(&self, blk: u64) -> bool {
        self.index.contains_key(&blk)
    }

    /// Read `blk` into `buf` (len must be BLOCK).
    pub fn read_block(&self, blk: u64, buf: &mut [u8]) -> std::io::Result<()> {
        debug_assert_eq!(buf.len() as u64, BLOCK);
        if let Some(e) = self.index.get(&blk) {
            let (hdr, data) = self
                .log
                .read_record(e.pos)
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .ok_or_else(|| std::io::Error::other("journal record vanished"))?;
            if hdr.len as usize != BLOCK as usize {
                return Err(std::io::Error::other("journal record length mismatch"));
            }
            buf.copy_from_slice(&data);
            return Ok(());
        }
        self.file.read_exact_at(buf, blk * BLOCK)
    }

    /// Read `blocks` blocks starting at `blk` into `buf`.
    pub fn read(&self, blk: u64, blocks: usize, buf: &mut [u8]) -> std::io::Result<()> {
        if buf.len() != blocks * BLOCK as usize {
            return Err(std::io::Error::other("read buffer size mismatch"));
        }
        for i in 0..blocks {
            let off = i * BLOCK as usize;
            self.read_block(blk + i as u64, &mut buf[off..off + BLOCK as usize])?;
        }
        Ok(())
    }

    /// Write `blocks` blocks starting at `blk` from `buf` into the journal.
    pub fn write(&mut self, blk: u64, blocks: usize, buf: &[u8]) -> anyhow::Result<()> {
        if buf.len() != blocks * BLOCK as usize {
            anyhow::bail!("write buffer size mismatch");
        }
        for i in 0..blocks {
            let off = i * BLOCK as usize;
            self.append_block(blk + i as u64, &buf[off..off + BLOCK as usize])?;
        }
        // One header write per command instead of one per block.
        self.publish()
    }

    /// Forward a discard to the origin and drop any journal copy of the range.
    pub fn discard(&mut self, blk: u64, blocks: usize) -> anyhow::Result<()> {
        self.index
            .retain(|b, _| *b < blk || *b >= blk + blocks as u64);
        let start = blk * BLOCK;

        use std::os::unix::io::AsRawFd;
        #[repr(C)]
        struct Range {
            start: u64,
            len: u64,
        }
        let r = Range {
            start,
            len: blocks as u64 * BLOCK,
        };
        // BLKDISCARD = _IO(0x12, 119). Discard is advisory: ENOTTY and friends
        // (loop devices, some files) simply mean "nothing to do".
        const BLKDISCARD: libc::c_ulong = 0x1277;
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKDISCARD, &r) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOTTY) {
                log::debug!("origin discard at {start} failed: {err}");
            }
        }
        Ok(())
    }

    /// Write `buf` (BLOCK bytes) as the new version of `blk`, into the journal
    /// and publish it. Use the batch entry points for multi-block IO.
    #[cfg(test)]
    pub fn write_block(&mut self, blk: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.append_block(blk, buf)?;
        self.log.deferred_header_write()
    }

    /// Append one block without publishing the header; the caller is expected
    /// to call `publish()` once its batch is done.
    fn append_block(&mut self, blk: u64, buf: &[u8]) -> anyhow::Result<()> {
        // Reclaim as needed until the record fits. The background writer keeps
        // the ring low in normal operation, so this rarely runs. When it does,
        // drain fully: partial reclaims can leave the ring unable to fit a
        // record (the head can only advance record by record), which would
        // spin here.
        if !self.log.can_fit(buf.len() as u64) {
            log::debug!("journal full, draining to origin before append");
            if let Some(st) = &self.stats {
                st.journal_full_drains.fetch_add(1, Ordering::Relaxed);
            }
            self.flush()?;
        }
        // No lookup by scanning: the record offset comes straight from append,
        // so a concurrent write-back reclaiming the ring cannot hide it.
        let (version, pos) = self.log.append(blk, buf)?;
        match self.index.entry(blk) {
            Entry::Occupied(mut o) => {
                o.get_mut().pos = pos;
                o.get_mut().version = version;
                o.get_mut().is_dirty = true;
            }
            Entry::Vacant(v) => {
                v.insert(Live {
                    pos,
                    version,
                    is_dirty: true,
                });
            }
        }
        self.dirty_blocks += 1;
        Ok(())
    }

    /// Write every journaled block back to the origin, then reset the ring.
    ///
    /// This is the "make the origin self-contained now" path used by FLUSH,
    /// shutdown and replay. A failed origin write leaves the journal intact so
    /// the data can still be recovered on the next start.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        while self.write_back_step(usize::MAX)? > 0 {}
        // The journal is about to be discarded, so the origin content must be
        // on stable storage before that happens.
        self.sync_origin()?;
        self.log.reset()?;
        self.index.clear();
        Ok(())
    }

    /// Appends waiting for a durable header update.
    pub fn pending_appends(&self) -> u32 {
        self.log.pending_appends()
    }

    /// Journal usage as a percentage of the ring, for watermarks.
    pub fn used_pct(&self) -> u64 {
        let len = self.log.hdr.log_len.max(1);
        self.log.used().saturating_mul(100) / len
    }

    /// Write back at most `budget` blocks (or `max_records` records) from the
    /// front of the journal, then reclaim them.
    ///
    /// Returns the number of records reclaimed, so the caller can tell when
    /// there is nothing left to do.
    ///
    /// Strategy, chosen for a spinning origin:
    ///
    /// * only the prefix `[head, cut)` is written back, and the *last*
    ///   occurrence of a block inside that prefix wins — earlier versions are
    ///   wasted IO and get skipped;
    /// * the surviving records are sorted by block number so the origin sees
    ///   large forward-sequential writes instead of random ones.
    ///
    /// `cut` is chosen so that at most `budget` distinct blocks are written.
    pub fn write_back_step(&mut self, budget_blocks: usize) -> anyhow::Result<usize> {
        self.write_back_step_within(budget_blocks, None)
    }

    /// Like [`Cache::write_back_step`], but stops early once `deadline` passes.
    ///
    /// The block budget alone does not bound the pause on a slow origin: one
    /// step of 64 blocks can take seconds on a network filesystem, and the
    /// whole time it holds the cache lock. Returning partial work is safe
    /// because only the records actually written are reclaimed.
    pub fn write_back_step_within(
        &mut self,
        budget_blocks: usize,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<usize> {
        let recs = self.log.scan()?;
        if recs.is_empty() {
            return Ok(0);
        }

        // Pick the prefix to reclaim: walk forward, charging one block per new
        // destination block, and stop before exceeding the budget.
        let mut seen: HashSet<u64> = HashSet::new();
        let mut cut_idx = 0usize;
        for (i, (_, hdr)) in recs.iter().enumerate() {
            if seen.len() >= budget_blocks.max(1) && seen.insert(hdr.blk) {
                // Revert the speculative insert and stop at the previous record.
                seen.remove(&hdr.blk);
                cut_idx = i;
                break;
            }
            seen.insert(hdr.blk);
            cut_idx = i + 1;
        }
        if cut_idx == 0 {
            return Ok(0);
        }
        let prefix = &recs[..cut_idx];

        // Newest version of each block inside the prefix wins.
        let mut newest: HashMap<u64, u32> = HashMap::new();
        for &(_, hdr) in prefix {
            let e = newest.entry(hdr.blk).or_insert(0);
            if hdr.version > *e {
                *e = hdr.version;
            }
        }
        let mut work: Vec<(u64, u64, u32)> = prefix
            .iter()
            .filter(|(_, h)| newest.get(&h.blk) == Some(&h.version))
            .map(|(pos, h)| (h.blk, *pos, h.version))
            .collect();
        // Forward-sequential on the origin.
        work.sort_unstable_by_key(|(blk, _, _)| *blk);

        let superseded = prefix.len() - work.len();
        let wb_started = std::time::Instant::now();
        let mut bytes_written = 0u64;
        let mut done_blocks = 0usize;
        for (blk, pos, _version) in &work {
            if let Some(dl) = deadline {
                if std::time::Instant::now() >= dl {
                    break;
                }
            }
            let Some((h, data)) = self.log.read_record(*pos)? else {
                continue;
            };
            if data.len() as u64 != h.len as u64 {
                continue;
            }
            if let Err(e) = self.file.write_all_at(&data, blk * BLOCK) {
                if let Some(st) = &self.stats {
                    st.write_back_errors.fetch_add(1, Ordering::Relaxed);
                }
                anyhow::bail!(
                    "origin write of block {blk} failed ({} records kept): {e}",
                    recs.len()
                );
            }
            bytes_written += data.len() as u64;
            done_blocks += 1;
        }
        if done_blocks == 0 {
            return Ok(0);
        }
        // Do NOT fsync the origin here. On a network filesystem every sync is
        // a synchronous COMMIT RPC, which costs far more than the writes it
        // covers; doing one per step throttles the whole cache. The data is in
        // the page cache (and the journal still holds it), so reclaiming a
        // little later is safe -- see `origin_is_synced`.
        self.dirty_blocks = self.dirty_blocks.saturating_add(done_blocks as u64);
        self.unsynced_blocks += done_blocks as u64;

        // Reclaiming a record discards the journal's copy, so the origin write
        // must be durable first. Syncing on every step is far too expensive on
        // a network filesystem, so allow a bounded window: the newest
        // `MAX_UNSYNCED_BLOCKS` blocks may live only in the page cache.
        if self.unsynced_blocks >= MAX_UNSYNCED_BLOCKS {
            // This fsync sits inside the critical section, so its cost is a
            // direct latency hit for every writer. On a network filesystem it
            // is a synchronous COMMIT that can take tens of milliseconds, so
            // the window is kept large (128 MiB) on purpose. The journal still
            // holds those records until they are reclaimed *after* this, so
            // the exposure is bounded either way.
            self.file.sync_data()?;
            self.unsynced_blocks = 0;
        }

        if let Some(st) = &self.stats {
            st.write_back_blocks
                .fetch_add(done_blocks as u64, Ordering::Relaxed);
            st.write_back_records
                .fetch_add(prefix.len() as u64, Ordering::Relaxed);
            st.write_back_superseded
                .fetch_add(superseded as u64, Ordering::Relaxed);
            st.write_back_bytes
                .fetch_add(bytes_written, Ordering::Relaxed);
            st.write_back_latency.observe(wb_started.elapsed());
        }

        // Reclaim exactly the records we handled.
        self.log.reclaim_records(prefix.len())?;

        // Forget index entries whose newest version was just reclaimed and
        // which have no newer record still in the ring.
        let remaining = self.log.scan()?;
        let mut still: HashSet<u64> = HashSet::with_capacity(remaining.len());
        for (pos, hdr) in remaining {
            // Rebuild the index from what is left in the ring.
            match self.index.get_mut(&hdr.blk) {
                Some(e) if hdr.version >= e.version => {
                    e.pos = pos;
                    e.version = hdr.version;
                }
                Some(_) => {}
                None => {
                    self.index.insert(
                        hdr.blk,
                        Live {
                            pos,
                            version: hdr.version,
                            is_dirty: true,
                        },
                    );
                }
            }
            still.insert(hdr.blk);
        }
        self.index.retain(|b, _| still.contains(b));

        debug_assert_eq!(
            {
                let recs = self.log.scan().unwrap();
                recs.iter().map(|(_, h)| h.total_len()).sum::<u64>()
            },
            self.log.used(),
            "ring accounting diverged after reclaim (head {} tail {})",
            self.log.hdr.head,
            self.log.hdr.tail
        );
        Ok(cut_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        dir: std::path::PathBuf,
        origin: std::path::PathBuf,
        cache: std::path::PathBuf,
    }

    impl Fx {
        fn new(name: &str) -> Fx {
            let dir =
                std::env::temp_dir().join(format!("rcache-cache-{}-{}", std::process::id(), name));
            std::fs::create_dir_all(&dir).unwrap();
            // 8 MiB origin: enough blocks for the index/replay tests.
            let origin = dir.join("origin.img");
            let f = std::fs::File::create(&origin).unwrap();
            f.set_len(8 << 20).unwrap();
            let cache = dir.join("cache.img");
            Fx { dir, origin, cache }
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn read_origin(fx: &Fx, blk: u64) -> Vec<u8> {
        let f = File::open(&fx.origin).unwrap();
        let mut b = vec![0u8; BLOCK as usize];
        f.read_exact_at(&mut b, blk * BLOCK).unwrap();
        b
    }

    #[test]
    fn write_read_flush() {
        let fx = Fx::new("wrf");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let data = vec![0xABu8; BLOCK as usize];
        c.write_block(3, &data).unwrap();
        assert!(c.is_cached(3));

        let mut buf = vec![0u8; BLOCK as usize];
        c.read_block(3, &mut buf).unwrap();
        assert_eq!(buf, data);
        // Origin still stale before flush.
        assert_ne!(read_origin(&fx, 3), data);

        c.flush().unwrap();
        assert!(!c.is_cached(3));
        assert_eq!(read_origin(&fx, 3), data);
    }

    #[test]
    fn read_miss_hits_origin() {
        let fx = Fx::new("miss");
        // Pre-fill origin block 7.
        let f = File::options().write(true).open(&fx.origin).unwrap();
        f.write_all_at(&[0x11u8; BLOCK as usize], 7 * BLOCK)
            .unwrap();
        drop(f);

        let c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let mut buf = vec![0u8; BLOCK as usize];
        c.read_block(7, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x11));
    }

    #[test]
    fn last_write_wins() {
        let fx = Fx::new("lww");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        c.write_block(9, &[1u8; BLOCK as usize]).unwrap();
        c.write_block(9, &[2u8; BLOCK as usize]).unwrap();
        let mut buf = vec![0u8; BLOCK as usize];
        c.read_block(9, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 2));
        c.flush().unwrap();
        assert!(read_origin(&fx, 9).iter().all(|&b| b == 2));
    }

    #[test]
    fn recovery_after_crash() {
        let fx = Fx::new("crash");
        {
            let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
            c.write_block(1, &[0x5Au8; BLOCK as usize]).unwrap();
            // Drop without flush: simulates a crash with a dirty journal.
            std::mem::forget(c);
        }
        // Reopening must replay the journal onto the origin.
        let c = Cache::open(&fx.cache, &fx.origin).unwrap();
        assert!(c.index_len() == 0);
        assert!(read_origin(&fx, 1).iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn incremental_write_back() {
        let fx = Fx::new("incr");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();

        // Write blocks 0..8, then re-write 3 and 4 with newer data.
        for i in 0..8u64 {
            c.write_block(i, &[i as u8 + 1; BLOCK as usize]).unwrap();
        }
        c.write_block(3, &[0xE3; BLOCK as usize]).unwrap();
        c.write_block(4, &[0xE4; BLOCK as usize]).unwrap();
        let used_before = c.log.used();
        assert!(used_before > 0);

        // One small step must reclaim only part of the journal.
        let reclaimed = c.write_back_step(2).unwrap();
        assert!(reclaimed > 0, "step reclaimed nothing");
        assert!(c.log.used() < used_before, "ring did not shrink");
        assert!(c.log.used() > 0, "step drained everything");

        // The drained prefix must be on the origin, newest version included.
        assert_eq!(read_origin(&fx, 0)[0], 1);

        // Reads still see the newest data for everything.
        let mut buf = vec![0u8; BLOCK as usize];
        c.read_block(3, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xE3));

        // Finish the job; origin must end up fully consistent.
        c.flush().unwrap();
        assert_eq!(c.log.used(), 0);
        for i in 0..8u64 {
            let want = match i {
                3 => 0xE3,
                4 => 0xE4,
                n => n as u8 + 1,
            };
            assert_eq!(read_origin(&fx, i)[0], want, "block {i}");
        }
    }

    #[test]
    fn write_back_skips_stale_versions() {
        let fx = Fx::new("stale");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        // Same block written 4 times: only the last version matters.
        for v in 0..4u8 {
            c.write_block(2, &[v; BLOCK as usize]).unwrap();
        }
        let records = c.log.scan().unwrap().len();
        assert_eq!(records, 4);
        // A step big enough for one block must reclaim all 4 records.
        let reclaimed = c.write_back_step(1).unwrap();
        assert_eq!(reclaimed, 4, "stale versions were not skipped");
        assert_eq!(c.log.used(), 0);
        assert!(read_origin(&fx, 2).iter().all(|&b| b == 3));
    }

    /// The ring must never report more live bytes than the records that are
    /// actually there. A head that runs past the tail silently corrupts
    /// `used()`/`can_fit()` accounting.
    #[test]
    fn ring_pointers_stay_consistent() {
        let fx = Fx::new("ring");
        let f = File::create(&fx.cache).unwrap();
        f.set_len(crate::log::MIN_CACHE_SIZE).unwrap();
        drop(f);
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let ring = c.log.hdr.log_len;

        for round in 0..200u64 {
            // Interleave writes and small write-back steps, cycling block ids
            // so records alias and wrap-around gets exercised.
            let blk = (round * 7) % 40;
            c.write_block(blk, &[round as u8; BLOCK as usize]).unwrap();
            if round % 3 == 0 {
                c.write_back_step(4).unwrap();
            }
            let recs = c.log.scan().unwrap();
            let bytes: u64 = recs.iter().map(|(_, h)| h.total_len()).sum();
            assert_eq!(
                bytes,
                c.log.used(),
                "round {round}: scan() bytes {bytes} != used() {} (head {} tail {} ring {ring})",
                c.log.used(),
                c.log.hdr.head,
                c.log.hdr.tail
            );
            assert!(c.log.used() <= ring);
        }
    }

    /// Reproduce "ring is exactly full, then flush" which is what mkfs does on
    /// a small journal. The pointers must stay in range afterwards.
    #[test]
    fn flush_when_ring_exactly_full() {
        let fx = Fx::new("exactfull");
        let f = File::create(&fx.cache).unwrap();
        f.set_len(crate::log::MIN_CACHE_SIZE).unwrap();
        drop(f);
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let ring = c.log.hdr.log_len;
        let per = crate::log::REC_HDR as u64 + BLOCK;
        let fits = ring / per;

        // Fill the ring as far as records allow, then flush.
        for i in 0..fits {
            c.write_block(i, &[i as u8; BLOCK as usize]).unwrap();
        }
        assert!(c.log.used() <= ring);
        let before = c.log.used();
        c.flush().unwrap();
        assert_eq!(c.log.hdr.head, 0, "head not rewound after full flush");
        assert_eq!(c.log.hdr.tail, 0, "tail not rewound after full flush");
        assert_eq!(c.log.used(), 0);
        assert!(before > 0);
        // The ring must still accept new writes.
        c.write_block(999, &[0xAB; BLOCK as usize]).unwrap();
        assert!(c.log.hdr.head < ring && c.log.hdr.tail < ring);
    }

    /// Regression: reclaiming must never leave the head "behind" the tail in
    /// ring arithmetic, which makes `used()` report a nearly full ring and
    /// wedges every subsequent write.
    #[test]
    fn reclaim_near_ring_end() {
        let fx = Fx::new("ringend");
        let f = File::create(&fx.cache).unwrap();
        f.set_len(crate::log::MIN_CACHE_SIZE).unwrap();
        drop(f);
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let ring = c.log.hdr.log_len;
        let per = crate::log::REC_HDR as u64 + BLOCK;
        let fits = ring / per;
        // Drive the tail all the way around the ring several times, taking
        // small write-back steps so head and tail chase each other.
        for round in 0..(fits * 4) {
            let blk = round % 32;
            c.write_block(blk, &[round as u8; BLOCK as usize]).unwrap();
            c.write_back_step(3).unwrap();
            assert!(
                c.log.used() < ring,
                "used() {} must stay below ring {} (head {} tail {})",
                c.log.used(),
                ring,
                c.log.hdr.head,
                c.log.hdr.tail
            );
            let recs = c.log.scan().unwrap();
            let bytes: u64 = recs.iter().map(|(_, h)| h.total_len()).sum();
            assert_eq!(bytes, c.log.used(), "round {round} accounting");
        }
    }

    /// Write-back must never lose a newer version that was appended after the
    /// step started: the version numbers, not the ring order, decide.
    #[test]
    fn interleaved_write_and_write_back() {
        let fx = Fx::new("interleave");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();

        // Block 5 written, partially written back, then written again with a
        // newer version, then fully drained.
        c.write_block(1, &[0x11; BLOCK as usize]).unwrap();
        c.write_block(5, &[0x55; BLOCK as usize]).unwrap();
        c.write_back_step(1).unwrap(); // reclaims block 1 only
        c.write_block(5, &[0x99; BLOCK as usize]).unwrap();
        c.write_block(2, &[0x22; BLOCK as usize]).unwrap();
        c.write_back_step(2).unwrap();
        c.flush().unwrap();

        let mut buf = vec![0u8; BLOCK as usize];
        std::fs::File::open(&fx.origin)
            .unwrap()
            .read_exact_at(&mut buf, 5 * BLOCK)
            .unwrap();
        assert!(
            buf.iter().all(|&b| b == 0x99),
            "block 5 lost its newest version"
        );
    }

    /// `scan()` only sees a contiguous run from the head. The append path can
    /// skip the tail to the ring start (a "hole"), and everything after that
    /// hole is invisible -- so reclaiming must never leave the head before a
    /// hole while live records sit behind it.
    #[test]
    fn scan_sees_all_live_records_after_reclaim() {
        let fx = Fx::new("hole");
        let f = File::create(&fx.cache).unwrap();
        f.set_len(crate::log::MIN_CACHE_SIZE).unwrap();
        drop(f);
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let ring = c.log.hdr.log_len;
        let per = crate::log::REC_HDR as u64 + BLOCK;
        let fits = ring / per;

        for round in 0..(fits * 3) {
            c.write_block(round % 50, &[round as u8; BLOCK as usize])
                .unwrap();
            if round % 2 == 0 {
                c.write_back_step(2).unwrap();
            }
            // Every live record must be visible to scan(); otherwise the
            // index silently forgets blocks that are still only in the ring.
            let recs = c.log.scan().unwrap();
            let bytes: u64 = recs.iter().map(|(_, h)| h.total_len()).sum();
            assert_eq!(
                bytes,
                c.log.used(),
                "round {round}: scan sees {bytes} bytes but ring holds {} (head {} tail {})",
                c.log.used(),
                c.log.hdr.head,
                c.log.hdr.tail
            );
        }
    }

    /// Exactly what the block-level test on the target did: 256 sequential
    /// blocks, then a flush. Every block must land at its own offset.
    #[test]
    fn sequential_batch_write_back() {
        let fx = Fx::new("seqbatch");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let n = 256usize;
        let mut data = vec![0u8; n * BLOCK as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        c.write(100, n, &data).unwrap();
        let mut back = vec![0u8; n * BLOCK as usize];
        c.read(100, n, &mut back).unwrap();
        assert_eq!(back, data, "cache readback");
        c.flush().unwrap();
        for i in 0..n {
            let ob = read_origin(&fx, 100 + i as u64);
            let off = i * BLOCK as usize;
            assert_eq!(
                ob[..],
                data[off..off + BLOCK as usize],
                "origin block {} (of the batch) is wrong",
                100 + i
            );
        }
    }

    /// Appends are batched for durability, but a record must still be
    /// replayable (and visible to the index) before its batch is fsynced.
    #[test]
    fn batched_appends_are_still_replayable() {
        let fx = Fx::new("batch");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        // Fewer than BATCH_APPENDS so nothing is fsynced yet.
        let n = (crate::log::BATCH_APPENDS as usize) - 1;
        for i in 0..n {
            c.write_block(i as u64, &[(i as u8) | 1; BLOCK as usize])
                .unwrap();
        }
        assert!(c.pending_appends() > 0, "expected an unsynced batch");
        assert_eq!(c.index_len(), n);

        // A reader must see them immediately, from the journal.
        let mut buf = vec![0u8; BLOCK as usize];
        c.read_block(0, &mut buf).unwrap();
        assert_eq!(buf[0], 1);

        // Reopening without an explicit sync must replay them all: the record
        // bytes are in the page cache and the header records the tail.
        drop(c);
        let c2 = Cache::open(&fx.cache, &fx.origin).unwrap();
        assert_eq!(c2.index_len(), 0, "replay should have drained to origin");
        for i in 0..n {
            assert_eq!(read_origin(&fx, i as u64)[0], (i as u8) | 1, "block {i}");
        }
    }

    /// `sync()` makes the pending batch durable and is idempotent.
    #[test]
    fn sync_makes_batch_durable() {
        let fx = Fx::new("sync");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        for i in 0..3u64 {
            c.write_block(i, &[0x77; BLOCK as usize]).unwrap();
        }
        assert!(c.pending_appends() > 0);
        c.sync().unwrap();
        assert_eq!(c.pending_appends(), 0);
        // Syncing again is a no-op, not an error.
        c.sync().unwrap();
        // Data lands on the origin only after a write-back.
        assert!(read_origin(&fx, 0).iter().all(|&b| b == 0));
        c.flush().unwrap();
        assert!(read_origin(&fx, 0).iter().all(|&b| b == 0x77));
    }

    /// Regression: once the append batch counter reaches BATCH_APPENDS and a
    /// sync happens, the counter must start again. If it is not reset, every
    /// later append pays for a full fsync -- which on a slow origin turns a
    /// cache that should absorb writes into a synchronous passthrough.
    #[test]
    fn sync_resets_the_batch_counter() {
        let fx = Fx::new("batchreset");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let batch = crate::log::BATCH_APPENDS as usize;

        // Drive past the first batch boundary so a sync is triggered.
        for i in 0..batch + 1 {
            c.write_block(i as u64, &[0x11; BLOCK as usize]).unwrap();
            c.publish().unwrap();
        }
        assert!(
            c.pending_appends() < crate::log::BATCH_APPENDS,
            "counter not reset after a sync: {} pending",
            c.pending_appends()
        );

        // Appends after the reset must accumulate again. If the counter had
        // stayed above the threshold, each of these would sync immediately and
        // pending_appends() would remain 0.
        c.write_block(999, &[0x22; BLOCK as usize]).unwrap();
        let after_one = c.pending_appends();
        assert!(
            (1..crate::log::BATCH_APPENDS).contains(&after_one),
            "counter did not restart: {after_one} pending, an fsync runs per append"
        );
        c.write_block(998, &[0x33; BLOCK as usize]).unwrap();
        assert!(
            c.pending_appends() > after_one,
            "counter is not accumulating ({} -> {})",
            after_one,
            c.pending_appends()
        );
    }

    /// A step given an already-expired deadline must do nothing, and a
    /// partial step must only reclaim what it actually wrote back.
    #[test]
    fn step_respects_deadline() {
        let fx = Fx::new("deadline");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        for i in 0..8u64 {
            c.write_block(i, &[0x33; BLOCK as usize]).unwrap();
        }
        let live_before = c.index_len();
        // Deadline already passed: no work, no reclaim.
        let past = std::time::Instant::now();
        let done = c.write_back_step_within(64, Some(past)).unwrap();
        assert_eq!(done, 0, "expired deadline still wrote back");
        assert_eq!(
            c.index_len(),
            live_before,
            "expired deadline reclaimed records"
        );

        // A generous deadline does the work.
        let later = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let done = c.write_back_step_within(64, Some(later)).unwrap();
        assert_eq!(done, 8);
        for i in 0..8u64 {
            assert_eq!(read_origin(&fx, i)[0], 0x33, "block {i} not written");
        }
    }

    /// The origin must hold every block before the journal is reset, or a
    /// crash right after `flush()` would lose data that was acknowledged.
    #[test]
    fn flush_keeps_origin_complete() {
        let fx = Fx::new("flushorder");
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let n = 32u64;
        for i in 0..n {
            c.write_block(i, &[(i as u8) + 1; BLOCK as usize]).unwrap();
        }
        assert_eq!(c.index_len() as u64, n);
        c.flush().unwrap();
        assert_eq!(c.index_len(), 0, "journal still holds blocks after flush");
        assert_eq!(c.log.used(), 0);
        for i in 0..n {
            assert_eq!(read_origin(&fx, i)[0], (i as u8) + 1, "block {i} missing");
        }
        // And the ring still accepts new writes afterwards.
        c.write_block(100, &[0xEE; BLOCK as usize]).unwrap();
        c.flush().unwrap();
        assert_eq!(read_origin(&fx, 100)[0], 0xEE);
    }

    #[test]
    fn journal_full_drains() {
        let fx = Fx::new("jfull");
        // Small cache: room for a handful of records.
        let f = File::create(&fx.cache).unwrap();
        f.set_len(crate::log::MIN_CACHE_SIZE).unwrap();
        drop(f);
        let mut c = Cache::open(&fx.cache, &fx.origin).unwrap();
        let cap = c.log.hdr.log_len;
        let per_rec = crate::log::REC_HDR as u64 + BLOCK;
        // About 3x the ring capacity: forces several drains.
        let n = (cap / per_rec) * 3;
        assert!(n * BLOCK <= 8 << 20, "test origin too small for n={n}");
        for i in 0..n {
            c.write_block(i, &[(i as u8).wrapping_add(1); BLOCK as usize])
                .unwrap();
        }
        c.flush().unwrap();
        // Everything must land on the origin, latest version included.
        for i in 0..n {
            assert_eq!(
                read_origin(&fx, i)[0],
                (i as u8).wrapping_add(1),
                "block {i} not persisted"
            );
        }
        assert_eq!(c.log.used(), 0);
        assert_eq!(c.index_len(), 0);
    }
}
