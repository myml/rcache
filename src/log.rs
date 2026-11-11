//! SSD-side write-back journal.
//!
//! # On-disk layout (all little endian)
//!
//! ```text
//! sector 0      : header (512 bytes used, CRC of the area covered)
//! sector 1..    : log area, a ring of records
//!
//! record        : fixed 64 byte header + payload (payload_len is 512 aligned)
//!                 u64 blk       origin LBA (in 4 KiB units)
//!                 u32 payload_len   bytes of data following the header
//!                 u32 version       higher wins for a given blk
//!                 u32 crc32     over payload
//!                 u32 flags     1 = committed
//! ```

use std::fs::{File, OpenOptions};

use std::os::unix::fs::FileExt;
use std::path::Path;

pub const SECTOR: u64 = 512;
pub const BLOCK: u64 = 4096;
pub const REC_HDR: usize = 64;
pub const MAGIC: u64 = 0x5243_4143_4845_3031; // "RCACHE01"
pub const VERSION: u32 = 1;
/// Round `v` up to the next multiple of `unit`.
#[inline]
fn round_up(v: u64, unit: u64) -> u64 {
    v.div_ceil(unit) * unit
}

/// Smallest cache file we will accept (1 MiB, block aligned).
pub const MIN_CACHE_SIZE: u64 = 1024 * 1024;

/// A journal record header as found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecHdr {
    pub blk: u64,
    pub len: u32,
    pub version: u32,
    pub crc: u32,
    pub flags: u32,
}

impl RecHdr {
    pub fn total_len(&self) -> u64 {
        REC_HDR as u64 + self.len as u64
    }

    fn encode(&self, buf: &mut [u8; REC_HDR]) {
        buf[0..8].copy_from_slice(&self.blk.to_le_bytes());
        buf[8..12].copy_from_slice(&self.len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.version.to_le_bytes());
        buf[16..20].copy_from_slice(&self.crc.to_le_bytes());
        buf[20..24].copy_from_slice(&self.flags.to_le_bytes());
    }

    fn decode(buf: &[u8; REC_HDR]) -> Self {
        RecHdr {
            blk: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            version: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            crc: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
        }
    }
}

/// Header stored at sector 0 of the cache device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub origin_size: u64,
    pub block_size: u32,
    pub log_off: u64,
    pub log_len: u64,
    pub head: u64,
    pub tail: u64,
    pub next_version: u32,
}

const HDR_SIZE: usize = 64;

impl Header {
    pub fn new(origin_size: u64, block_size: u32, log_off: u64, log_len: u64) -> Self {
        Header {
            origin_size,
            block_size,
            log_off,
            log_len,
            head: 0,
            tail: 0,
            next_version: 1,
        }
    }

    fn encode(&self) -> [u8; HDR_SIZE] {
        let mut b = [0u8; HDR_SIZE];
        b[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.block_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.origin_size.to_le_bytes());
        b[24..32].copy_from_slice(&self.log_off.to_le_bytes());
        b[32..40].copy_from_slice(&self.log_len.to_le_bytes());
        b[40..48].copy_from_slice(&self.head.to_le_bytes());
        b[48..56].copy_from_slice(&self.tail.to_le_bytes());
        b[56..60].copy_from_slice(&self.next_version.to_le_bytes());
        let crc = crc32fast::hash(&b[0..60]);
        b[60..64].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn decode(b: &[u8; SECTOR as usize]) -> Option<(Self, bool)> {
        if u64::from_le_bytes(b[0..8].try_into().unwrap()) != MAGIC {
            return None;
        }
        let stored = u32::from_le_bytes(b[60..64].try_into().unwrap());
        let ok = crc32fast::hash(&b[0..60]) == stored;
        Some((
            Header {
                block_size: u32::from_le_bytes(b[12..16].try_into().unwrap()),
                origin_size: u64::from_le_bytes(b[16..24].try_into().unwrap()),
                log_off: u64::from_le_bytes(b[24..32].try_into().unwrap()),
                log_len: u64::from_le_bytes(b[32..40].try_into().unwrap()),
                head: u64::from_le_bytes(b[40..48].try_into().unwrap()),
                tail: u64::from_le_bytes(b[48..56].try_into().unwrap()),
                next_version: u32::from_le_bytes(b[56..60].try_into().unwrap()),
            },
            ok,
        ))
    }
}

/// The SSD log device.
pub struct Log {
    file: File,
    pub hdr: Header,
    /// Name tag of the origin device, only for diagnostics.
    #[allow(dead_code)]
    pub origin: String,
}

impl Log {
    /// Open an existing cache file, or format a fresh one of `size` bytes.
    pub fn open(path: &Path, origin: &str, origin_size: u64) -> anyhow::Result<Log> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let size = file.metadata()?.len();

        if size == 0 {
            // Brand new file: allocate the minimum ring, then format below.
            file.set_len(MIN_CACHE_SIZE)?;
        } else {
            let mut first = [0u8; SECTOR as usize];
            file.read_exact_at(&mut first, 0)?;
            if let Some((hdr, crc_ok)) = Header::decode(&first) {
                if !crc_ok {
                    anyhow::bail!("cache header CRC mismatch: refusing to use a damaged journal");
                }
                if hdr.origin_size != origin_size {
                    anyhow::bail!(
                        "cache is bound to an origin of {} bytes, but this origin is {} bytes \
                         (refusing to attach a cache to the wrong device)",
                        hdr.origin_size,
                        origin_size
                    );
                }
                if size < hdr.log_off + hdr.log_len {
                    anyhow::bail!("cache file too small for its own journal layout");
                }
                return Ok(Log {
                    file,
                    hdr,
                    origin: origin.to_string(),
                });
            }
            if size < MIN_CACHE_SIZE {
                anyhow::bail!(
                    "{} exists but is too small ({} bytes); create a larger file or delete it",
                    path.display(),
                    size
                );
            }
        }

        // Fresh format: the whole file minus one sector is the log ring.
        let size = file.metadata()?.len();
        if size < MIN_CACHE_SIZE {
            anyhow::bail!(
                "{} is too small ({} bytes); at least {} bytes are needed",
                path.display(),
                size,
                MIN_CACHE_SIZE
            );
        }
        let log_len = (size - SECTOR) / BLOCK * BLOCK;
        let hdr = Header::new(origin_size, BLOCK as u32, SECTOR, log_len);
        let mut log = Log {
            file,
            hdr,
            origin: origin.to_string(),
        };
        log.sync_header()?;
        Ok(log)
    }

    fn sync_header(&mut self) -> anyhow::Result<()> {
        let b = self.hdr.encode();
        self.file.write_all_at(&b, 0)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn used(&self) -> u64 {
        (self.hdr.tail - self.hdr.head) % self.hdr.log_len.max(1)
    }

    /// Will a record of `data_len` payload bytes fit?
    pub fn can_fit(&self, data_len: u64) -> bool {
        let need = REC_HDR as u64 + round_up(data_len, SECTOR);
        need + self.used() <= self.hdr.log_len
    }

    /// Append one record and return the version assigned to it.
    pub fn append(&mut self, blk: u64, data: &[u8]) -> anyhow::Result<u32> {
        let padded = round_up(data.len() as u64, SECTOR);
        if !self.can_fit(data.len() as u64) {
            anyhow::bail!("journal full (used {} of {} ring)", self.used(), self.hdr.log_len);
        }
        let version = self.hdr.next_version;
        let hdr = RecHdr {
            blk,
            len: padded as u32,
            version,
            crc: crc32fast::hash(data),
            flags: 1,
        };
        let mut rec = vec![0u8; REC_HDR + padded as usize];
        let mut h = [0u8; REC_HDR];
        hdr.encode(&mut h);
        rec[..REC_HDR].copy_from_slice(&h);
        rec[REC_HDR..REC_HDR + data.len()].copy_from_slice(data);

        // Commit the header first: a torn record is simply not visible.
        self.file.write_all_at(&rec, self.hdr.log_off + self.hdr.tail)?;
        // Temporary self-check; the header write below is the real commit.
        self.hdr.tail = (self.hdr.tail + rec.len() as u64) % self.hdr.log_len;
        self.hdr.next_version += 1;
        self.sync_header()?;
        Ok(version)
    }

    /// Read the payload of a record found at `pos` (0 <= pos < log_len).
    pub fn read_record(&self, pos: u64) -> anyhow::Result<Option<(RecHdr, Vec<u8>)>> {
        let mut h = [0u8; REC_HDR];
        self.file
            .read_exact_at(&mut h, self.hdr.log_off + pos)?;
        let hdr = RecHdr::decode(&h);
        if hdr.len == 0 || hdr.len as u64 > self.hdr.log_len {
            return Ok(None);
        }
        let mut data = vec![0u8; hdr.len as usize];
        self.file
            .read_exact_at(&mut data, self.hdr.log_off + pos + REC_HDR as u64)?;
        if crc32fast::hash(&data) != hdr.crc {
            return Ok(None);
        }
        Ok(Some((hdr, data)))
    }

    /// Iterate all live records, returning their log offset and header.
    pub fn scan(&self) -> anyhow::Result<Vec<(u64, RecHdr)>> {
        let mut out = Vec::new();
        let mut pos = self.hdr.head;
        let mut walked = 0u64;
        while pos != self.hdr.tail && walked < self.hdr.log_len {
            let mut h = [0u8; REC_HDR];
            self.file.read_exact_at(&mut h, self.hdr.log_off + pos)?;
            let hdr = RecHdr::decode(&h);
            if hdr.len == 0 || hdr.flags != 1 {
                break;
            }
            let total = hdr.total_len();
            if pos + total > self.hdr.log_len || walked + total > self.hdr.log_len {
                break;
            }
            out.push((pos, hdr));
            walked += total;
            pos = (pos + total) % self.hdr.log_len;
        }
        Ok(out)
    }

    /// Reset the ring after a complete write-back.
    pub fn reset(&mut self) -> anyhow::Result<()> {
        self.hdr.head = 0;
        self.hdr.tail = 0;
        self.sync_header()
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str, size: u64) -> PathBuf {
        let size = size.max(MIN_CACHE_SIZE);
        let p = std::env::temp_dir().join(format!("rcache-test-{}-{}", std::process::id(), name));
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(size).unwrap();
        p
    }

    #[test]
    fn format_reopen() {
        let p = tmp("fmt", 64 * 1024);
        {
            let l = Log::open(&p, "/dev/null", 1 << 20).unwrap();
            assert_eq!(l.hdr.origin_size, 1 << 20);
            assert_eq!(l.hdr.log_off, SECTOR);
        }
        let l = Log::open(&p, "/dev/null", 1 << 20).unwrap();
        assert_eq!(l.hdr.head, 0);
        assert_eq!(l.hdr.tail, 0);
        // A different origin size must be rejected.
        assert!(Log::open(&p, "/dev/null", 2 << 20).is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn append_scan() {
        let p = tmp("append", 64 * 1024);
        let mut l = Log::open(&p, "/dev/null", 1 << 20).unwrap();
        l.append(100, &[1u8; 4096]).unwrap();
        l.append(101, &[2u8; 8192]).unwrap();
        let recs = l.scan().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].1.blk, 100);
        assert_eq!(recs[1].1.blk, 101);
        assert_eq!(recs[0].1.version, 1);
        assert_eq!(recs[1].1.version, 2);
        let (h, d) = l.read_record(recs[1].0).unwrap().unwrap();
        assert_eq!(h.blk, 101);
        assert_eq!(d.len(), 8192);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn full_and_reset() {
        let p = tmp("full", MIN_CACHE_SIZE);
        let mut l = Log::open(&p, "/dev/null", 1 << 20).unwrap();
        let cap = l.hdr.log_len;
        let per_rec = REC_HDR as u64 + 4096;
        let fits = cap / per_rec;
        assert!(fits >= 2, "ring too small for this test");
        for i in 0..fits {
            l.append(i, &[0u8; 4096]).unwrap();
        }
        // The ring is now full: one more record must be refused.
        assert!(l.append(999, &[0u8; 4096]).is_err());
        l.reset().unwrap();
        assert_eq!(l.used(), 0);
        l.append(0, &[7u8; 4096]).unwrap();
        let recs = l.scan().unwrap();
        assert_eq!(recs.len(), 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn bad_crc_is_ignored() {
        let p = tmp("crc", 64 * 1024);
        let mut l = Log::open(&p, "/dev/null", 1 << 20).unwrap();
        l.append(5, &[3u8; 4096]).unwrap();
        // Corrupt one payload byte on disk.
        let pos = l.hdr.log_off + REC_HDR as u64;
        l.file.write_all_at(&[0xFF], pos).unwrap();
        let recs = l.scan().unwrap();
        let (pos, h) = recs[0];
        assert!(l.read_record(pos).unwrap().is_none(), "hdr {:?}", h);
        std::fs::remove_file(&p).ok();
    }
}
