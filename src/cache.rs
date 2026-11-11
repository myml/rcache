//! The write-back cache: journal + origin write-back + recovery.
//!
//! Design notes:
//!
//! * Every write goes to the SSD journal first and becomes the newest version
//!   of its block. The origin still holds the stale copy until the journal is
//!   drained.
//! * Reads check the journal index; a hit is served from the SSD, a miss from
//!   the origin.
//! * `flush()` writes every live record back to the origin, oldest first, and
//!   resets the ring. That is also what makes the origin self-contained again.
//! * The index is rebuilt on every start by scanning the ring, so there is no
//!   independent index to keep consistent: the journal *is* the metadata.

use anyhow::Context;
use crate::log::{Log, BLOCK};
use std::collections::hash_map::Entry;
use std::os::unix::fs::FileTypeExt;
use std::collections::HashMap;
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
            self.write_block(blk + i as u64, &buf[off..off + BLOCK as usize])?;
        }
        Ok(())
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

    /// Write `buf` (BLOCK bytes) as the new version of `blk`, into the journal.
    pub fn write_block(&mut self, blk: u64, buf: &[u8]) -> anyhow::Result<()> {
        if !self.log.can_fit(buf.len() as u64) {
            // Journal full: drain it first. This keeps the origin correct at
            // all times and bounds how long a write can block.
            log::debug!("journal full, draining before append");
            self.flush()?;
        }
        let version = self.log.append(blk, buf)?;
        let pos = self.find_pos(blk, version)?;
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

    fn find_pos(&self, blk: u64, version: u32) -> anyhow::Result<u64> {
        for (pos, hdr) in self.log.scan()? {
            if hdr.blk == blk && hdr.version == version {
                return Ok(pos);
            }
        }
        anyhow::bail!("just-appended record for blk {blk} not found")
    }

    /// Write every journaled block back to the origin, oldest record first
    /// (so later versions win) then reset the ring.
    ///
    /// A failed origin write leaves the journal fully intact so the data can
    /// be recovered on the next start; only a successful drain resets it.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        let recs = self.log.scan()?;
        // Order by log position == order by age, but wrap-around means a
        // higher position can be older; versions decide, so sort by version.
        let live = recs.len();
        let mut ordered: Vec<_> = recs.into_iter().collect();
        ordered.sort_by_key(|(_, h)| h.version);
        for (pos, hdr) in ordered {
            if let Some((h, data)) = self.log.read_record(pos)? {
                if data.len() as u64 != hdr.len as u64 {
                    continue;
                }
                if let Err(e) = self.file.write_all_at(&data, h.blk * BLOCK) {
                    anyhow::bail!(
                        "origin write of block {} failed ({} live records kept): {e}",
                        h.blk,
                        live
                    );
                }
            }
        }
        self.file.sync_data()?;
        self.log.reset()?;
        self.index.clear();
        self.dirty_blocks = 0;
        Ok(())
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
            let dir = std::env::temp_dir().join(format!(
                "rcache-cache-{}-{}",
                std::process::id(),
                name
            ));
            std::fs::create_dir_all(&dir).unwrap();
            // 8 MiB origin: enough blocks for the index/replay tests.
            let origin = dir.join("origin.img");
            let f = std::fs::File::create(&origin).unwrap();
            f.set_len(8 << 20).unwrap();
            let cache = dir.join("cache.img");
            Fx {
                dir,
                origin,
                cache,
            }
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
        f.write_all_at(&[0x11u8; BLOCK as usize], 7 * BLOCK).unwrap();
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
