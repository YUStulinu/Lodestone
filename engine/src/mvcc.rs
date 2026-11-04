//! Multi-version concurrency control: snapshot isolation over the key-value store.
//!
//! Every write is stored as a new version of its key, tagged with the writing transaction's
//! version number. A transaction sees the versions written by itself and by transactions that
//! had committed when it began -- those numbered below its own and not in its snapshot of active
//! transactions. Writing a key that has a version the writer cannot see (from a concurrent
//! transaction, committed or not) is a write-write conflict: the writer gets a serialization
//! failure and must retry (first committer wins, roughly; here the first writer wins).
//!
//! Everything lives in the one B+tree, under these key prefixes:
//!
//! ```text
//! 0x01                         next version number
//! 0x02 version                 an active transaction
//! 0x03 version key             a key the transaction wrote (to undo it)
//! 0x04 key version             a version of a key: 0x00 deleted, 0x01 + value
//! 0x05 key                     unversioned data (counters that must not conflict)
//! ```
//!
//! Commit deletes the transaction's active marker and write records and makes the store durable.
//! Rollback also deletes its versions. Because a commit flushes all changed pages, other
//! transactions' uncommitted versions can reach the disk; after a crash, recovery rolls back
//! every transaction still marked active, exactly like a rollback.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::keycode::{prefix_end, put_bytes, put_bytes_open, put_u64, take_bytes, take_u64};
use crate::storage::vfs::Vfs;
use crate::storage::{Options, Storage};

const NEXT_VERSION: u8 = 0x01;
const TXN_ACTIVE: u8 = 0x02;
const TXN_WRITE: u8 = 0x03;
const VERSION: u8 = 0x04;
const UNVERSIONED: u8 = 0x05;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A concurrent transaction wrote the same key: retry.
    Serialization,
    ReadOnly,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Serialization => write!(f, "could not serialize access due to concurrent update"),
            Error::ReadOnly => write!(f, "cannot write in a read-only transaction"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn version_key(key: &[u8], version: u64) -> Vec<u8> {
    let mut k = vec![VERSION];
    put_bytes(&mut k, key);
    put_u64(&mut k, version);
    k
}

fn decode_version_key(k: &[u8]) -> Option<(Vec<u8>, u64)> {
    let (key, rest) = take_bytes(k.get(1..)?)?;
    let (v, _) = take_u64(rest)?;
    Some((key, v))
}

fn active_key(version: u64) -> Vec<u8> {
    let mut k = vec![TXN_ACTIVE];
    put_u64(&mut k, version);
    k
}

fn write_key(version: u64, key: &[u8]) -> Vec<u8> {
    let mut k = vec![TXN_WRITE];
    put_u64(&mut k, version);
    put_bytes(&mut k, key);
    k
}

fn unversioned_key(key: &[u8]) -> Vec<u8> {
    let mut k = vec![UNVERSIONED];
    put_bytes(&mut k, key);
    k
}

/// Live transactions by the oldest version each might read ("xmin"), so vacuum knows what to keep.
type Live = Arc<Mutex<BTreeMap<u64, usize>>>;

#[derive(Clone)]
pub struct Mvcc {
    store: Arc<Mutex<Storage>>,
    live: Live,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub next_version: u64,
    pub active: usize,
    pub pages: u32,
    pub wal_frames: u64,
    pub commits: u64,
    pub checkpoints: u64,
}

fn scan_keys(st: &mut Storage, prefix: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let end = prefix_end(prefix);
    let mut out = Vec::new();
    let to = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    st.scan(Bound::Included(prefix), to, |k, _| {
        out.push(k.to_vec());
        true
    })?;
    Ok(out)
}

fn active_set(st: &mut Storage) -> io::Result<HashSet<u64>> {
    Ok(scan_keys(st, &[TXN_ACTIVE])?.iter().filter_map(|k| take_u64(&k[1..]).map(|(v, _)| v)).collect())
}

fn next_version(st: &mut Storage) -> io::Result<u64> {
    Ok(st.get(&[NEXT_VERSION])?.map(|b| u64::from_be_bytes(b.try_into().unwrap())).unwrap_or(1))
}

/// Deletes a transaction's versions, write records and active marker.
fn undo(st: &mut Storage, version: u64) -> io::Result<()> {
    let mut prefix = vec![TXN_WRITE];
    put_u64(&mut prefix, version);
    for wk in scan_keys(st, &prefix)? {
        if let Some((key, _)) = take_bytes(&wk[9..]) {
            st.delete(&version_key(&key, version))?;
        }
        st.delete(&wk)?;
    }
    st.delete(&active_key(version))?;
    Ok(())
}

impl Mvcc {
    /// Opens a database, rolling back the transactions a crash left unfinished.
    pub fn open(vfs: &dyn Vfs, path: &Path, opts: Options) -> Result<Mvcc> {
        let mut st = Storage::open(vfs, path, opts)?;
        let stale = active_set(&mut st)?;
        for v in &stale {
            undo(&mut st, *v)?;
        }
        if !stale.is_empty() {
            st.commit()?;
        }
        Ok(Mvcc { store: Arc::new(Mutex::new(st)), live: Live::default() })
    }

    fn lock(&self) -> MutexGuard<'_, Storage> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn begin(&self) -> Result<Transaction> {
        let mut st = self.lock();
        let version = next_version(&mut st)?;
        st.set(&[NEXT_VERSION], &(version + 1).to_be_bytes())?;
        let active = active_set(&mut st)?;
        st.set(&active_key(version), &[])?;
        Ok(self.track(version, false, active))
    }

    fn track(&self, version: u64, read_only: bool, active: HashSet<u64>) -> Transaction {
        let xmin = active.iter().copied().min().unwrap_or(version).min(version);
        *self.live.lock().unwrap().entry(xmin).or_default() += 1;
        Transaction { store: self.store.clone(), live: self.live.clone(), xmin, version, read_only, active, done: false }
    }

    /// A transaction that only reads: it sees what was committed when it began and writes nothing.
    pub fn begin_read_only(&self) -> Result<Transaction> {
        let mut st = self.lock();
        let version = next_version(&mut st)?;
        let active = active_set(&mut st)?;
        Ok(self.track(version, true, active))
    }

    /// A counter outside the versioning: concurrent transactions can bump it without
    /// conflicting, and a rollback does not undo it (like a sequence).
    pub fn bump(&self, key: &[u8], by: i64) -> Result<i64> {
        let mut st = self.lock();
        let k = unversioned_key(key);
        let n = st.get(&k)?.map(|b| i64::from_be_bytes(b.try_into().unwrap())).unwrap_or(0) + by;
        st.set(&k, &n.to_be_bytes())?;
        Ok(n)
    }

    pub fn counter(&self, key: &[u8]) -> Result<i64> {
        let mut st = self.lock();
        Ok(st.get(&unversioned_key(key))?.map(|b| i64::from_be_bytes(b.try_into().unwrap())).unwrap_or(0))
    }

    pub fn status(&self) -> Result<Status> {
        let mut st = self.lock();
        let s = st.stats();
        Ok(Status {
            next_version: next_version(&mut st)?,
            active: active_set(&mut st)?.len(),
            pages: st.page_count(),
            wal_frames: st.wal_frames(),
            commits: s.commits,
            checkpoints: s.checkpoints,
        })
    }

    /// Checks the B+tree's invariants.
    pub fn check(&self) -> std::result::Result<crate::storage::btree::Shape, String> {
        self.lock().check()
    }

    pub fn checkpoint(&self) -> Result<()> {
        let mut st = self.lock();
        st.commit()?;
        st.checkpoint()?;
        Ok(())
    }

    /// Removes versions no transaction can see any more: those shadowed by a newer version that
    /// every running transaction sees, and deletions every transaction sees. Returns how many.
    pub fn vacuum(&self) -> Result<usize> {
        let mut st = self.lock();
        // Every live transaction sees everything below its xmin that it would see at all.
        let oldest = self.live.lock().unwrap().keys().next().copied();
        let horizon = oldest.unwrap_or(next_version(&mut st)?);
        let mut doomed = Vec::new();
        let mut current: Option<Vec<u8>> = None;
        // Versions of the current key below the horizon: (storage key, is deletion).
        let mut old: Vec<(Vec<u8>, bool)> = Vec::new();
        let flush = |old: &mut Vec<(Vec<u8>, bool)>, doomed: &mut Vec<Vec<u8>>| {
            if let Some((last, deleted)) = old.pop() {
                doomed.extend(old.drain(..).map(|(k, _)| k));
                if deleted {
                    doomed.push(last);
                }
            }
            old.clear();
        };
        let prefix = [VERSION];
        let end = prefix_end(&prefix).unwrap();
        st.scan(Bound::Included(&prefix), Bound::Excluded(&end), |k, v| {
            if let Some((key, version)) = decode_version_key(k) {
                if current.as_deref() != Some(key.as_slice()) {
                    flush(&mut old, &mut doomed);
                    current = Some(key);
                }
                if version < horizon {
                    old.push((k.to_vec(), v.first() == Some(&0)));
                }
            }
            true
        })?;
        flush(&mut old, &mut doomed);
        for k in &doomed {
            st.delete(k)?;
        }
        st.commit()?;
        Ok(doomed.len())
    }
}

pub struct Transaction {
    store: Arc<Mutex<Storage>>,
    live: Live,
    xmin: u64,
    version: u64,
    read_only: bool,
    active: HashSet<u64>,
    done: bool,
}

/// Bounds on the keys of a scan, within a prefix.
pub struct Range<'a> {
    pub prefix: &'a [u8],
    pub from: Bound<&'a [u8]>,
    pub to: Bound<&'a [u8]>,
}

impl Transaction {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    fn lock(&self) -> MutexGuard<'_, Storage> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn visible(&self, version: u64) -> bool {
        if self.active.contains(&version) {
            return false;
        }
        if self.read_only { version < self.version } else { version <= self.version }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut st = self.lock();
        let from = version_key(key, 0);
        let to = version_key(key, self.version);
        let mut found = None;
        st.scan(Bound::Included(&from), Bound::Included(&to), |_k, v| {
            // Versions come oldest first; remember the newest visible one.
            if let Some((_, ver)) = decode_version_key(_k) {
                if self.visible(ver) {
                    found = Some(v.to_vec());
                }
            }
            true
        })?;
        Ok(found.and_then(|v| if v.first() == Some(&1) { Some(v[1..].to_vec()) } else { None }))
    }

    pub fn set(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(key, Some(value))
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.write(key, None)
    }

    fn write(&self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut st = self.lock();
        // Any version we cannot see, from the oldest transaction we might not see on, is a conflict.
        let from = self.active.iter().copied().min().unwrap_or(self.version + 1).min(self.version + 1);
        let mut conflict = false;
        st.scan(
            Bound::Included(&version_key(key, from)),
            Bound::Included(&version_key(key, u64::MAX)),
            |k, _| {
                if let Some((_, v)) = decode_version_key(k) {
                    if !self.visible(v) {
                        conflict = true;
                        return false;
                    }
                }
                true
            },
        )?;
        if conflict {
            return Err(Error::Serialization);
        }
        st.set(&write_key(self.version, key), &[])?;
        let stored = match value {
            Some(v) => {
                let mut b = Vec::with_capacity(v.len() + 1);
                b.push(1);
                b.extend_from_slice(v);
                b
            }
            None => vec![0],
        };
        st.set(&version_key(key, self.version), &stored)?;
        Ok(())
    }

    /// The visible keys and values in a range, in key order, at most `limit` of them.
    pub fn scan(&self, range: &Range, limit: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut base = vec![VERSION];
        put_bytes_open(&mut base, range.prefix);
        let full = |k: &[u8]| {
            let mut out = vec![VERSION];
            let mut whole = range.prefix.to_vec();
            whole.extend_from_slice(k);
            put_bytes(&mut out, &whole);
            out
        };
        let lo: Vec<u8>;
        let from = match range.from {
            Bound::Included(k) => {
                lo = full(k);
                Bound::Included(lo.as_slice())
            }
            Bound::Excluded(k) => {
                lo = [full(k), vec![0xff; 8]].concat();
                Bound::Excluded(lo.as_slice())
            }
            Bound::Unbounded => Bound::Included(base.as_slice()),
        };
        let hi: Vec<u8>;
        let to = match range.to {
            Bound::Included(k) => {
                hi = [full(k), vec![0xff; 8]].concat();
                Bound::Included(hi.as_slice())
            }
            Bound::Excluded(k) => {
                hi = full(k);
                Bound::Excluded(hi.as_slice())
            }
            Bound::Unbounded => match prefix_end(&base) {
                Some(e) => {
                    hi = e;
                    Bound::Excluded(hi.as_slice())
                }
                None => Bound::Unbounded,
            },
        };
        let mut out = Vec::new();
        let mut current: Option<(Vec<u8>, Option<Vec<u8>>)> = None;
        let plen = range.prefix.len();
        let mut st = self.lock();
        st.scan(from, to, |k, v| {
            let Some((key, ver)) = decode_version_key(k) else { return true };
            if current.as_ref().is_some_and(|(c, _)| *c != key) {
                if let Some((ck, Some(cv))) = current.take() {
                    out.push((ck[plen..].to_vec(), cv));
                    if out.len() >= limit {
                        return false;
                    }
                }
            }
            if current.is_none() {
                current = Some((key, None));
            }
            if self.visible(ver) {
                let c = current.as_mut().unwrap();
                c.1 = if v.first() == Some(&1) { Some(v[1..].to_vec()) } else { None };
            }
            true
        })?;
        if out.len() < limit {
            if let Some((ck, Some(cv))) = current {
                out.push((ck[plen..].to_vec(), cv));
            }
        }
        Ok(out)
    }

    /// Makes the transaction's writes visible to transactions that begin afterwards, durably.
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        if self.read_only {
            return Ok(());
        }
        let mut st = self.lock();
        let mut prefix = vec![TXN_WRITE];
        put_u64(&mut prefix, self.version);
        for wk in scan_keys(&mut st, &prefix)? {
            st.delete(&wk)?;
        }
        st.delete(&active_key(self.version))?;
        st.commit()?;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<()> {
        self.done = true;
        if self.read_only {
            return Ok(());
        }
        let mut st = self.lock();
        undo(&mut st, self.version)?;
        st.commit()?;
        Ok(())
    }
}

impl Drop for Transaction {
    /// A transaction dropped without commit is rolled back.
    fn drop(&mut self) {
        if !self.done && !self.read_only {
            let mut st = self.lock();
            let _ = undo(&mut st, self.version);
        }
        let mut live = self.live.lock().unwrap();
        if let Some(n) = live.get_mut(&self.xmin) {
            *n -= 1;
            if *n == 0 {
                live.remove(&self.xmin);
            }
        }
    }
}

/// The versions of every key (for tests and debugging).
pub fn dump(mvcc: &Mvcc) -> Vec<(Vec<u8>, u64)> {
    let mut st = mvcc.lock();
    let mut out = BTreeSet::new();
    let _ = st.scan(Bound::Included(&[VERSION][..]), Bound::Excluded(&[VERSION + 1][..]), |k, _| {
        if let Some(kv) = decode_version_key(k) {
            out.insert(kv);
        }
        true
    });
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vfs::SimVfs;

    fn open(vfs: &SimVfs) -> Mvcc {
        Mvcc::open(vfs, Path::new("db"), Options::default()).unwrap()
    }

    fn all(t: &Transaction) -> Vec<(Vec<u8>, Vec<u8>)> {
        t.scan(&Range { prefix: b"", from: Bound::Unbounded, to: Bound::Unbounded }, usize::MAX).unwrap()
    }

    #[test]
    fn snapshots_see_what_had_committed_when_they_began() {
        let db = open(&SimVfs::new());
        let t1 = db.begin().unwrap();
        t1.set(b"a", b"1").unwrap();
        t1.set(b"b", b"1").unwrap();
        t1.commit().unwrap();

        let reader = db.begin_read_only().unwrap();
        let t2 = db.begin().unwrap();
        t2.set(b"a", b"2").unwrap();
        t2.delete(b"b").unwrap();
        t2.set(b"c", b"2").unwrap();
        assert_eq!(t2.get(b"a").unwrap(), Some(b"2".to_vec())); // own writes
        assert_eq!(reader.get(b"a").unwrap(), Some(b"1".to_vec())); // not t2's
        let t3 = db.begin().unwrap(); // began while t2 was running
        t2.commit().unwrap();
        assert_eq!(all(&reader), vec![(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"1".to_vec())]);
        assert_eq!(t3.get(b"a").unwrap(), Some(b"1".to_vec()));
        let t4 = db.begin_read_only().unwrap();
        assert_eq!(all(&t4), vec![(b"a".to_vec(), b"2".to_vec()), (b"c".to_vec(), b"2".to_vec())]);
    }

    #[test]
    fn write_write_conflicts_fail_the_second_writer() {
        let db = open(&SimVfs::new());
        let t1 = db.begin().unwrap();
        let t2 = db.begin().unwrap();
        t1.set(b"k", b"1").unwrap();
        assert!(matches!(t2.set(b"k", b"2"), Err(Error::Serialization)));
        t2.set(b"other", b"2").unwrap(); // other keys are fine
        t1.commit().unwrap();
        // Still a conflict after t1 committed: t2 cannot see t1's version.
        assert!(matches!(t2.set(b"k", b"2"), Err(Error::Serialization)));
        t2.rollback().unwrap();
        let t3 = db.begin().unwrap();
        t3.set(b"k", b"3").unwrap();
        t3.commit().unwrap();
        assert!(matches!(db.begin_read_only().unwrap().set(b"x", b""), Err(Error::ReadOnly)));
    }

    #[test]
    fn rollback_and_drop_undo_and_recovery_finishes_the_job() {
        let vfs = SimVfs::new();
        let db = open(&vfs);
        let t = db.begin().unwrap();
        t.set(b"keep", b"1").unwrap();
        t.commit().unwrap();
        let t = db.begin().unwrap();
        t.set(b"gone", b"1").unwrap();
        t.rollback().unwrap();
        {
            let t = db.begin().unwrap();
            t.set(b"dropped", b"1").unwrap();
        }
        // A transaction left running, its writes flushed by another commit, then a "crash".
        let open_txn = db.begin().unwrap();
        open_txn.set(b"in-flight", b"1").unwrap();
        let other = db.begin().unwrap();
        other.set(b"other", b"1").unwrap();
        other.commit().unwrap();
        std::mem::forget(open_txn);
        drop(db);
        let db = open(&vfs);
        let r = db.begin_read_only().unwrap();
        assert_eq!(all(&r).into_iter().map(|(k, _)| k).collect::<Vec<_>>(), vec![b"keep".to_vec(), b"other".to_vec()]);
        assert_eq!(db.status().unwrap().active, 0);
        assert!(dump(&db).iter().all(|(k, _)| k == b"keep" || k == b"other"));
    }

    #[test]
    fn scans_with_bounds_prefix_and_limit() {
        let db = open(&SimVfs::new());
        let t = db.begin().unwrap();
        for i in 0..50u8 {
            t.set(&[b't', 0, i], &[i]).unwrap();
        }
        t.set(b"u", b"x").unwrap();
        t.delete(&[b't', 0, 10]).unwrap();
        let r = Range { prefix: &[b't', 0], from: Bound::Excluded(&[5]), to: Bound::Included(&[12]) };
        let keys: Vec<u8> = t.scan(&r, 100).unwrap().iter().map(|(k, _)| k[0]).collect();
        assert_eq!(keys, vec![6, 7, 8, 9, 11, 12]);
        let r = Range { prefix: &[b't', 0], from: Bound::Unbounded, to: Bound::Unbounded };
        assert_eq!(t.scan(&r, 3).unwrap().len(), 3);
        assert_eq!(t.scan(&r, 1000).unwrap().len(), 49);
        t.commit().unwrap();
    }

    #[test]
    fn vacuum_drops_only_invisible_versions() {
        let db = open(&SimVfs::new());
        for i in 0..5u8 {
            let t = db.begin().unwrap();
            t.set(b"k", &[i]).unwrap();
            t.set(b"tmp", &[i]).unwrap();
            if i == 4 {
                t.delete(b"tmp").unwrap();
            }
            t.commit().unwrap();
        }
        let old_reader = db.begin_read_only().unwrap();
        let t = db.begin().unwrap();
        t.set(b"k", b"new").unwrap();
        t.commit().unwrap();
        let removed = db.vacuum().unwrap();
        // k: versions 1-4 go (5 remains for the old reader, 6 is newest); tmp: all 5 go.
        assert_eq!(removed, 4 + 5);
        assert_eq!(old_reader.get(b"k").unwrap(), Some(vec![4]));
        assert_eq!(db.begin_read_only().unwrap().get(b"k").unwrap(), Some(b"new".to_vec()));
        assert_eq!(db.begin_read_only().unwrap().get(b"tmp").unwrap(), None);
    }
}
