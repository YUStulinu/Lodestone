//! Storage: pages, the write-ahead log and a B+tree, giving an ordered key-value store whose
//! commits are atomic and durable.

pub mod btree;
pub mod page;
pub mod pager;
pub mod vfs;
pub mod wal;

use std::io;
use std::ops::Bound;
use std::path::Path;

pub use pager::{Options, Stats};
use pager::Pager;
use vfs::Vfs;

pub struct Storage {
    pager: Pager,
}

impl Storage {
    pub fn open(vfs: &dyn Vfs, path: &Path, opts: Options) -> io::Result<Storage> {
        Ok(Storage { pager: Pager::open(vfs, path, opts)? })
    }
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        btree::get(&mut self.pager, key)
    }
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        btree::insert(&mut self.pager, key, value)
    }
    pub fn delete(&mut self, key: &[u8]) -> io::Result<bool> {
        btree::delete(&mut self.pager, key)
    }
    pub fn scan(&mut self, from: Bound<&[u8]>, to: Bound<&[u8]>, f: impl FnMut(&[u8], &[u8]) -> bool) -> io::Result<()> {
        btree::scan(&mut self.pager, from, to, f)
    }
    /// Makes every change so far durable, atomically.
    pub fn commit(&mut self) -> io::Result<()> {
        self.pager.commit()
    }
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.pager.checkpoint()
    }
    pub fn check(&mut self) -> Result<btree::Shape, String> {
        btree::check(&mut self.pager)
    }
    pub fn stats(&self) -> Stats {
        self.pager.stats
    }
    pub fn page_count(&self) -> u32 {
        self.pager.page_count()
    }
    pub fn wal_frames(&self) -> u64 {
        self.pager.wal_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use vfs::SimVfs;

    fn contents(st: &mut Storage) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut out = BTreeMap::new();
        st.scan(Bound::Unbounded, Bound::Unbounded, |k, v| {
            out.insert(k.to_vec(), v.to_vec());
            true
        })
        .unwrap();
        out
    }

    /// The disk dies after a random number of writes and syncs -- during a commit, a checkpoint,
    /// or between them -- then loses a random part of what was not synced, tearing some writes.
    /// Reopened, the store must hold exactly the last commit that returned, or the one that was
    /// under way (its frames may all have reached the disk before the sync failed), and its tree
    /// must be whole.
    #[test]
    fn power_loss_leaves_the_last_commit_or_the_interrupted_one() {
        let path = Path::new("db");
        let opts = || Options { cache_pages: 24, checkpoint_bytes: 48 * 1024 };
        let (mut interrupted, mut torn_or_lost) = (0, 0);
        for seed in 0..300 {
            let mut rng = fastrand::Rng::with_seed(seed);
            let vfs = SimVfs::new();
            let mut st = Storage::open(&vfs, path, opts()).unwrap();
            let mut committed = BTreeMap::new();
            let mut working = BTreeMap::new();
            let mut in_flight = None;
            vfs.fail_after(Some(rng.u64(0..300)));
            'run: for _ in 0..60 {
                for _ in 0..rng.usize(1..40) {
                    let k = rng.u32(0..400).to_be_bytes().to_vec();
                    if rng.u8(0..5) == 0 {
                        st.delete(&k).unwrap();
                        working.remove(&k);
                    } else {
                        let v = vec![rng.u8(..); rng.usize(1..300)];
                        st.set(&k, &v).unwrap();
                        working.insert(k, v);
                    }
                }
                match st.commit() {
                    Ok(()) => committed = working.clone(),
                    Err(_) => {
                        in_flight = Some(working.clone());
                        break 'run;
                    }
                }
            }
            drop(st);
            vfs.crash(&mut rng);
            vfs.fail_after(None);
            let mut st = Storage::open(&vfs, path, opts()).unwrap();
            st.check().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            let got = contents(&mut st);
            if got != committed {
                assert_eq!(Some(&got), in_flight.as_ref(), "seed {seed}: neither the last commit nor the interrupted one");
                interrupted += 1;
            } else if in_flight.is_some() {
                torn_or_lost += 1;
            }
            // And it keeps working.
            st.set(b"after", b"crash").unwrap();
            st.commit().unwrap();
        }
        // Both outcomes of an interrupted commit occurred.
        assert!(interrupted > 0 && torn_or_lost > 0, "{interrupted} {torn_or_lost}");
    }
}
