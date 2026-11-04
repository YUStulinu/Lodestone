//! Files, behind a trait so the storage engine can run on a simulated disk.
//!
//! [`OsVfs`] is the real file system. [`SimVfs`] keeps every file in memory and remembers which
//! writes have been made durable by `sync`; [`SimVfs::crash`] then throws away a random part of
//! the rest -- whole writes dropped, or only some of their 512-byte sectors kept -- as a power cut
//! would. Recovery must cope with whatever survives.

use std::collections::HashMap;
use std::fs::{File as StdFile, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[allow(clippy::len_without_is_empty)]
pub trait File: Send {
    /// Reads up to `buf.len()` bytes at `offset`; returns how many were read (fewer at the end).
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
    fn len(&mut self) -> io::Result<u64>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
}

pub trait Vfs: Send + Sync {
    /// Opens a file for reading and writing, creating it if it does not exist.
    fn open(&self, path: &Path) -> io::Result<Box<dyn File>>;
}

pub struct OsVfs;

struct OsFile(StdFile);

impl Vfs for OsVfs {
    fn open(&self, path: &Path) -> io::Result<Box<dyn File>> {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        Ok(Box::new(OsFile(f)))
    }
}

#[cfg(unix)]
impl File for OsFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self.0.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.0.write_all_at(buf, offset)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}

#[cfg(windows)]
impl File for OsFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self.0.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            done += self.0.seek_write(&buf[done..], offset + done as u64)?;
        }
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}

const SECTOR: usize = 512;

#[derive(Clone)]
enum Op {
    Write(u64, Vec<u8>),
    SetLen(u64),
}

#[derive(Default)]
struct SimFile {
    durable: Vec<u8>,
    current: Vec<u8>,
    pending: Vec<Op>,
}

fn apply(data: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(off, buf) => {
            let end = *off as usize + buf.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[*off as usize..end].copy_from_slice(buf);
        }
        Op::SetLen(len) => data.resize(*len as usize, 0),
    }
}

/// An in-memory disk that can lose what was not synced.
#[derive(Clone, Default)]
pub struct SimVfs {
    files: Arc<Mutex<HashMap<PathBuf, SimFile>>>,
    /// Writes, syncs and truncations left before the disk "dies" (None: never).
    budget: Arc<Mutex<Option<u64>>>,
}

impl SimVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// A power cut: every file falls back to its synced state plus a random subset of the writes
    /// made since, some of them torn at sector boundaries.
    pub fn crash(&self, rng: &mut fastrand::Rng) {
        let mut files = self.files.lock().unwrap();
        for f in files.values_mut() {
            let mut data = f.durable.clone();
            for op in f.pending.drain(..) {
                match rng.u32(0..3) {
                    0 => {} // lost
                    1 => apply(&mut data, &op),
                    _ => match &op {
                        // torn: some sectors made it, some did not
                        Op::Write(off, buf) => {
                            for (i, chunk) in buf.chunks(SECTOR).enumerate() {
                                if rng.bool() {
                                    apply(&mut data, &Op::Write(off + (i * SECTOR) as u64, chunk.to_vec()));
                                }
                            }
                        }
                        Op::SetLen(_) => {
                            if rng.bool() {
                                apply(&mut data, &op)
                            }
                        }
                    },
                }
            }
            f.durable = data.clone();
            f.current = data;
        }
    }

    /// After `ops` more writes, syncs or truncations every one fails, as if the process had died
    /// there; `crash` then decides what reached the disk.
    pub fn fail_after(&self, ops: Option<u64>) {
        *self.budget.lock().unwrap() = ops;
    }

    /// Bytes currently in a file (for tests).
    pub fn contents(&self, path: &Path) -> Vec<u8> {
        self.files.lock().unwrap().get(path).map(|f| f.current.clone()).unwrap_or_default()
    }

    /// Overwrites a file's bytes, durably (for corruption tests).
    pub fn set_contents(&self, path: &Path, data: Vec<u8>) {
        let mut files = self.files.lock().unwrap();
        let f = files.entry(path.to_path_buf()).or_default();
        f.durable = data.clone();
        f.current = data;
        f.pending.clear();
    }
}

struct SimHandle {
    path: PathBuf,
    files: Arc<Mutex<HashMap<PathBuf, SimFile>>>,
    budget: Arc<Mutex<Option<u64>>>,
}

impl Vfs for SimVfs {
    fn open(&self, path: &Path) -> io::Result<Box<dyn File>> {
        self.files.lock().unwrap().entry(path.to_path_buf()).or_default();
        Ok(Box::new(SimHandle { path: path.to_path_buf(), files: self.files.clone(), budget: self.budget.clone() }))
    }
}

impl SimHandle {
    fn spend(&self) -> io::Result<()> {
        let mut b = self.budget.lock().unwrap();
        match *b {
            Some(0) => Err(io::Error::other("simulated crash")),
            Some(n) => {
                *b = Some(n - 1);
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut SimFile) -> T) -> T {
        let mut files = self.files.lock().unwrap();
        f(files.get_mut(&self.path).expect("open file"))
    }
}

impl File for SimHandle {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.with(|f| {
            let off = offset as usize;
            if off >= f.current.len() {
                return Ok(0);
            }
            let n = buf.len().min(f.current.len() - off);
            buf[..n].copy_from_slice(&f.current[off..off + n]);
            Ok(n)
        })
    }
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.spend()?;
        self.with(|f| {
            let op = Op::Write(offset, buf.to_vec());
            apply(&mut f.current, &op);
            f.pending.push(op);
            Ok(())
        })
    }
    fn sync(&mut self) -> io::Result<()> {
        self.spend()?;
        self.with(|f| {
            for op in f.pending.drain(..) {
                apply(&mut f.durable, &op);
            }
            Ok(())
        })
    }
    fn len(&mut self) -> io::Result<u64> {
        self.with(|f| Ok(f.current.len() as u64))
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.spend()?;
        self.with(|f| {
            let op = Op::SetLen(len);
            apply(&mut f.current, &op);
            f.pending.push(op);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_writes_survive_a_crash() {
        let vfs = SimVfs::new();
        let p = Path::new("f");
        let mut f = vfs.open(p).unwrap();
        f.write_at(b"hello", 0).unwrap();
        f.sync().unwrap();
        f.write_at(&vec![7u8; 4096], 5).unwrap();
        for seed in 0..20 {
            let v2 = vfs.clone();
            let mut rng = fastrand::Rng::with_seed(seed);
            v2.crash(&mut rng);
            assert_eq!(&vfs.contents(p)[..5], b"hello");
            // Whatever else survived is either the old bytes (none) or the new ones.
            assert!(vfs.contents(p)[5..].iter().all(|&b| b == 0 || b == 7));
        }
    }
}
