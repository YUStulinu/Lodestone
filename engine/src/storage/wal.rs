//! The write-ahead log: whole pages, appended at commit, copied into the database file at
//! checkpoints (the scheme of SQLite's WAL mode).
//!
//! ```text
//! header  magic u32 | version u32 | page_size u32 | pad u32 | salt u64 | checksum u32 | pad u32
//! frame   page u32 | commit u32 | salt u64 | checksum u32 | pad u32 | page bytes
//! ```
//!
//! A commit is a run of frames whose last one has `commit` set to the database size in pages;
//! it is durable once the log is synced. Each frame's checksum is a CRC-32 over the previous
//! frame's checksum, its own header and its page, so a frame left over from an earlier log (a
//! different salt) or torn by a crash breaks the chain. Recovery replays frames up to the last
//! commit whose chain is whole and drops the rest. A checkpoint writes the latest copy of every
//! page into the database file, syncs it, and only then starts a new log with a new salt; a crash
//! in between replays the same pages again, which is harmless.

use std::collections::HashMap;
use std::io;

use super::page::{PAGE_SIZE, PageId};
use super::vfs::File;

const MAGIC: u32 = 0x4c44_574c; // "LDWL"
const VERSION: u32 = 1;
pub const HEADER: u64 = 32;
const FRAME_HEADER: usize = 24;
pub const FRAME: u64 = (FRAME_HEADER + PAGE_SIZE) as u64;

pub struct Wal {
    file: Box<dyn File>,
    salt: u64,
    /// Where the next frame goes: just after the last committed one.
    end: u64,
    /// Checksum of the last committed frame (or of the header).
    chain: u32,
    /// Page -> offset of the page bytes in its newest committed frame.
    index: HashMap<PageId, u64>,
    /// Database size in pages at the last commit in the log (0: no commit).
    pub page_count: u32,
}

fn checksum(prev: u32, header: &[u8], page: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&prev.to_le_bytes());
    h.update(header);
    h.update(page);
    h.finalize()
}

impl Wal {
    /// Opens a log and recovers its committed frames.
    pub fn open(mut file: Box<dyn File>) -> io::Result<Wal> {
        let len = file.len()?;
        let mut header = [0u8; HEADER as usize];
        let valid = len >= HEADER && file.read_at(&mut header, 0)? == HEADER as usize && {
            let sum = u32::from_le_bytes(header[24..28].try_into().unwrap());
            u32::from_le_bytes(header[0..4].try_into().unwrap()) == MAGIC
                && u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize == PAGE_SIZE
                && sum == checksum(0, &header[..24], &[])
        };
        if !valid {
            let mut wal = Wal { file, salt: 0, end: HEADER, chain: 0, index: HashMap::new(), page_count: 0 };
            wal.reset()?;
            return Ok(wal);
        }
        let salt = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let mut chain = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let mut wal = Wal { file, salt, end: HEADER, chain, index: HashMap::new(), page_count: 0 };
        let mut pending = HashMap::new();
        let mut frame = vec![0u8; FRAME as usize];
        let mut off = HEADER;
        while off + FRAME <= len {
            if wal.file.read_at(&mut frame, off)? < FRAME as usize {
                break;
            }
            let (fh, page) = frame.split_at(FRAME_HEADER);
            let page_no = u32::from_le_bytes(fh[0..4].try_into().unwrap());
            let commit = u32::from_le_bytes(fh[4..8].try_into().unwrap());
            let fsalt = u64::from_le_bytes(fh[8..16].try_into().unwrap());
            let sum = u32::from_le_bytes(fh[16..20].try_into().unwrap());
            if fsalt != salt || sum != checksum(chain, &fh[..16], page) {
                break;
            }
            chain = sum;
            pending.insert(page_no, off + FRAME_HEADER as u64);
            off += FRAME;
            if commit != 0 {
                wal.index.extend(pending.drain());
                wal.end = off;
                wal.chain = chain;
                wal.page_count = commit;
            }
        }
        if len > wal.end {
            // Uncommitted or torn frames: cut them off so new frames follow the last commit.
            wal.file.set_len(wal.end)?;
            wal.file.sync()?;
        }
        Ok(wal)
    }

    /// Starts an empty log with a new salt.
    fn reset(&mut self) -> io::Result<()> {
        self.salt = fastrand::u64(..);
        let mut header = [0u8; HEADER as usize];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        header[16..24].copy_from_slice(&self.salt.to_le_bytes());
        let sum = checksum(0, &header[..24], &[]);
        header[24..28].copy_from_slice(&sum.to_le_bytes());
        self.file.write_at(&header, 0)?;
        self.file.set_len(HEADER)?;
        self.file.sync()?;
        self.chain = sum;
        self.end = HEADER;
        self.index.clear();
        self.page_count = 0;
        Ok(())
    }

    /// The newest committed copy of a page, if the log has one.
    pub fn read(&mut self, page: PageId) -> io::Result<Option<Vec<u8>>> {
        let Some(&off) = self.index.get(&page) else { return Ok(None) };
        let mut buf = vec![0u8; PAGE_SIZE];
        if self.file.read_at(&mut buf, off)? != PAGE_SIZE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "log frame cut short"));
        }
        Ok(Some(buf))
    }

    /// Appends pages as one commit and syncs: when this returns, the commit is durable.
    pub fn commit(&mut self, pages: &[(PageId, Vec<u8>)], page_count: u32) -> io::Result<()> {
        assert!(!pages.is_empty() && page_count > 0);
        let mut buf = Vec::with_capacity(pages.len() * FRAME as usize);
        let mut chain = self.chain;
        let mut offsets = Vec::with_capacity(pages.len());
        for (i, (page_no, data)) in pages.iter().enumerate() {
            debug_assert_eq!(data.len(), PAGE_SIZE);
            let mut fh = [0u8; FRAME_HEADER];
            fh[0..4].copy_from_slice(&page_no.to_le_bytes());
            let commit = if i == pages.len() - 1 { page_count } else { 0 };
            fh[4..8].copy_from_slice(&commit.to_le_bytes());
            fh[8..16].copy_from_slice(&self.salt.to_le_bytes());
            chain = checksum(chain, &fh[..16], data);
            fh[16..20].copy_from_slice(&chain.to_le_bytes());
            offsets.push((*page_no, self.end + buf.len() as u64 + FRAME_HEADER as u64));
            buf.extend_from_slice(&fh);
            buf.extend_from_slice(data);
        }
        self.file.write_at(&buf, self.end)?;
        self.file.sync()?;
        self.end += buf.len() as u64;
        self.chain = chain;
        self.index.extend(offsets);
        self.page_count = page_count;
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.end
    }

    pub fn frames(&self) -> u64 {
        (self.end - HEADER) / FRAME
    }

    /// Copies the newest version of every logged page into the database file, syncs it, then
    /// empties the log.
    pub fn checkpoint(&mut self, db: &mut dyn File) -> io::Result<()> {
        if self.index.is_empty() {
            return Ok(());
        }
        let mut pages: Vec<_> = self.index.iter().map(|(&p, &o)| (p, o)).collect();
        pages.sort_unstable();
        let mut buf = vec![0u8; PAGE_SIZE];
        for (page, off) in pages {
            if self.file.read_at(&mut buf, off)? != PAGE_SIZE {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "log frame cut short"));
            }
            db.write_at(&buf, page as u64 * PAGE_SIZE as u64)?;
        }
        db.sync()?;
        self.reset()
    }
}
