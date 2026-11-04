//! The page cache. Pages are held decoded; changed pages stay in memory until a commit writes
//! them to the log (no-steal: an uncommitted change never reaches disk, so recovery never has to
//! undo a page). Clean pages are evicted, least recently used first, when the cache is full.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::page::{Node, PAGE_SIZE, PageId};
use super::vfs::{File, Vfs};
use super::wal::Wal;

pub const META: PageId = 0;
pub const ROOT: PageId = 1;

pub struct Options {
    /// Pages kept in memory (dirty pages are always kept).
    pub cache_pages: usize,
    /// Checkpoint when the log grows past this many bytes.
    pub checkpoint_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options { cache_pages: 16_384, checkpoint_bytes: 32 << 20 }
    }
}

struct Slot {
    node: Node,
    dirty: bool,
    used: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub reads: u64,
    pub commits: u64,
    pub frames: u64,
    pub checkpoints: u64,
}

pub struct Pager {
    db: Box<dyn File>,
    wal: Wal,
    cache: HashMap<PageId, Slot>,
    tick: u64,
    page_count: u32,
    opts: Options,
    pub stats: Stats,
}

pub fn wal_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push("-wal");
    PathBuf::from(p)
}

impl Pager {
    pub fn open(vfs: &dyn Vfs, path: &Path, opts: Options) -> io::Result<Pager> {
        let db = vfs.open(path)?;
        let wal = Wal::open(vfs.open(&wal_path(path))?)?;
        let mut pager = Pager { db, wal, cache: HashMap::new(), tick: 0, page_count: 2, opts, stats: Stats::default() };
        let fresh = pager.wal.page_count == 0 && pager.db.len()? < PAGE_SIZE as u64;
        if fresh {
            pager.cache.insert(META, Slot { node: Node::Meta { page_count: 2 }, dirty: true, used: 0 });
            pager.cache.insert(ROOT, Slot { node: Node::empty_leaf(), dirty: true, used: 0 });
            pager.commit()?;
        } else {
            pager.page_count = u32::MAX; // so the meta page can be read
            match pager.get(META)? {
                Node::Meta { page_count } => pager.page_count = *page_count,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "page 0 is not the meta page")),
            }
        }
        Ok(pager)
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    fn load(&mut self, id: PageId) -> io::Result<()> {
        if self.cache.contains_key(&id) {
            return Ok(());
        }
        if id >= self.page_count {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("page {id} beyond the end")));
        }
        self.stats.reads += 1;
        let buf = match self.wal.read(id)? {
            Some(b) => b,
            None => {
                let mut b = vec![0u8; PAGE_SIZE];
                if self.db.read_at(&mut b, id as u64 * PAGE_SIZE as u64)? != PAGE_SIZE {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("page {id} missing")));
                }
                b
            }
        };
        let node = Node::decode(&buf)?;
        self.cache.insert(id, Slot { node, dirty: false, used: 0 });
        Ok(())
    }

    pub fn get(&mut self, id: PageId) -> io::Result<&Node> {
        self.load(id)?;
        self.tick += 1;
        let slot = self.cache.get_mut(&id).unwrap();
        slot.used = self.tick;
        Ok(&slot.node)
    }

    pub fn get_mut(&mut self, id: PageId) -> io::Result<&mut Node> {
        self.load(id)?;
        self.tick += 1;
        let slot = self.cache.get_mut(&id).unwrap();
        slot.used = self.tick;
        slot.dirty = true;
        Ok(&mut slot.node)
    }

    pub fn allocate(&mut self, node: Node) -> io::Result<PageId> {
        let id = self.page_count;
        self.page_count += 1;
        let count = self.page_count;
        *self.get_mut(META)? = Node::Meta { page_count: count };
        self.cache.insert(id, Slot { node, dirty: true, used: self.tick });
        Ok(id)
    }

    pub fn dirty_pages(&self) -> usize {
        self.cache.values().filter(|s| s.dirty).count()
    }

    /// Writes every changed page to the log as one durable commit.
    pub fn commit(&mut self) -> io::Result<()> {
        let mut pages: Vec<(PageId, Vec<u8>)> =
            self.cache.iter().filter(|(_, s)| s.dirty).map(|(&id, s)| (id, s.node.encode())).collect();
        if pages.is_empty() {
            return Ok(());
        }
        pages.sort_unstable_by_key(|p| p.0);
        self.wal.commit(&pages, self.page_count)?;
        for (id, _) in &pages {
            self.cache.get_mut(id).unwrap().dirty = false;
        }
        self.stats.commits += 1;
        self.stats.frames += pages.len() as u64;
        if self.wal.size() > self.opts.checkpoint_bytes {
            self.checkpoint()?;
        }
        self.evict();
        Ok(())
    }

    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.wal.checkpoint(self.db.as_mut())?;
        self.stats.checkpoints += 1;
        Ok(())
    }

    pub fn wal_frames(&self) -> u64 {
        self.wal.frames()
    }

    fn evict(&mut self) {
        if self.cache.len() <= self.opts.cache_pages {
            return;
        }
        let target = self.opts.cache_pages * 9 / 10;
        let mut clean: Vec<(u64, PageId)> =
            self.cache.iter().filter(|(id, s)| !s.dirty && **id > ROOT).map(|(&id, s)| (s.used, id)).collect();
        clean.sort_unstable();
        let excess = self.cache.len().saturating_sub(target);
        for (_, id) in clean.into_iter().take(excess) {
            self.cache.remove(&id);
        }
    }
}
