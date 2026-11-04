//! A B+tree of byte keys and values over the pager. The root stays on page 1: when it splits,
//! its contents move to two new pages and it becomes their parent. Leaves are chained for scans.
//!
//! Deletion removes the entry; a leaf left empty is unlinked from its parent and from the leaf
//! chain (and a parent left with no children goes the same way), so scans never wade through
//! empty pages. Underfull leaves are not merged, and unlinked pages are not reused: the file
//! does not shrink.

use std::io;
use std::ops::Bound;

use super::page::{MAX_ENTRY, Node, PAGE_SIZE, PageId};
use super::pager::{Pager, ROOT};

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("entry larger than {MAX_ENTRY} bytes"))
}

/// The child of an interior node to follow for `key`.
fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}

/// Path from the root to the leaf that holds (or would hold) `key`: (page, child index taken).
fn descend(pager: &mut Pager, key: &[u8]) -> io::Result<(Vec<(PageId, usize)>, PageId)> {
    let mut path = Vec::new();
    let mut id = ROOT;
    loop {
        match pager.get(id)? {
            Node::Interior { keys, children } => {
                let i = child_index(keys, key);
                path.push((id, i));
                id = children[i];
            }
            Node::Leaf { .. } => return Ok((path, id)),
            Node::Meta { .. } => return Err(io::Error::new(io::ErrorKind::InvalidData, "meta page in the tree")),
        }
    }
}

pub fn get(pager: &mut Pager, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let (_, leaf) = descend(pager, key)?;
    let Node::Leaf { keys, values, .. } = pager.get(leaf)? else { unreachable!() };
    Ok(keys.binary_search_by(|k| k.as_slice().cmp(key)).ok().map(|i| values[i].clone()))
}

/// Inserts or replaces.
pub fn insert(pager: &mut Pager, key: &[u8], value: &[u8]) -> io::Result<()> {
    if key.len() + value.len() > MAX_ENTRY {
        return Err(too_large());
    }
    let (path, leaf) = descend(pager, key)?;
    {
        let Node::Leaf { keys, values, .. } = pager.get_mut(leaf)? else { unreachable!() };
        match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
            Ok(i) => values[i] = value.to_vec(),
            Err(i) => {
                keys.insert(i, key.to_vec());
                values.insert(i, value.to_vec());
            }
        }
    }
    if pager.get(leaf)?.size() > PAGE_SIZE {
        split(pager, path, leaf)?;
    }
    Ok(())
}

/// Splits an overfull node and pushes the separator up, splitting parents as needed.
fn split(pager: &mut Pager, mut path: Vec<(PageId, usize)>, mut id: PageId) -> io::Result<()> {
    loop {
        if id == ROOT {
            // Move the root's contents to a new page; the root becomes its parent.
            let node = std::mem::replace(pager.get_mut(ROOT)?, Node::Interior { keys: vec![], children: vec![] });
            let moved = pager.allocate(node)?;
            *pager.get_mut(ROOT)? = Node::Interior { keys: vec![], children: vec![moved] };
            path.push((ROOT, 0));
            id = moved;
        }
        let new_id = pager.allocate(Node::empty_leaf())?;
        let (sep, right) = match pager.get_mut(id)? {
            Node::Leaf { keys, values, next } => {
                let at = split_point(keys.iter().zip(values.iter()).map(|(k, v)| super::page::leaf_entry_size(k, v)));
                let rk = keys.split_off(at);
                let rv = values.split_off(at);
                let right = Node::Leaf { keys: rk, values: rv, next: *next };
                *next = new_id;
                let Node::Leaf { keys: rk, .. } = &right else { unreachable!() };
                (rk[0].clone(), right)
            }
            Node::Interior { keys, children } => {
                let at = split_point(keys.iter().map(|k| super::page::interior_entry_size(k)));
                let at = at.clamp(1, keys.len() - 1);
                let mut rk = keys.split_off(at);
                let sep = rk.remove(0);
                let rc = children.split_off(at + 1);
                (sep, Node::Interior { keys: rk, children: rc })
            }
            Node::Meta { .. } => unreachable!(),
        };
        *pager.get_mut(new_id)? = right;
        let (parent, i) = path.pop().expect("a non-root node has a parent");
        let Node::Interior { keys, children } = pager.get_mut(parent)? else { unreachable!() };
        keys.insert(i, sep);
        children.insert(i + 1, new_id);
        if pager.get(parent)?.size() <= PAGE_SIZE {
            return Ok(());
        }
        id = parent;
    }
}

/// The index that splits entries of these sizes into two halves of about equal bytes.
fn split_point(sizes: impl Iterator<Item = usize>) -> usize {
    let sizes: Vec<usize> = sizes.collect();
    let total: usize = sizes.iter().sum();
    let mut acc = 0;
    for (i, s) in sizes.iter().enumerate() {
        acc += s;
        if acc * 2 >= total {
            return (i + 1).clamp(1, sizes.len() - 1);
        }
    }
    sizes.len() / 2
}

pub fn delete(pager: &mut Pager, key: &[u8]) -> io::Result<bool> {
    let (path, leaf) = descend(pager, key)?;
    let Node::Leaf { keys, .. } = pager.get(leaf)? else { unreachable!() };
    let Ok(i) = keys.binary_search_by(|k| k.as_slice().cmp(key)) else { return Ok(false) };
    let Node::Leaf { keys, values, .. } = pager.get_mut(leaf)? else { unreachable!() };
    keys.remove(i);
    values.remove(i);
    if keys.is_empty() && leaf != ROOT {
        unlink(pager, path, leaf)?;
    }
    Ok(true)
}

/// The leaf just before the one reached by `path`, if any.
fn previous_leaf(pager: &mut Pager, path: &[(PageId, usize)]) -> io::Result<Option<PageId>> {
    for &(parent, i) in path.iter().rev() {
        if i == 0 {
            continue;
        }
        let Node::Interior { children, .. } = pager.get(parent)? else { unreachable!() };
        let mut id = children[i - 1];
        loop {
            match pager.get(id)? {
                Node::Interior { children, .. } => id = *children.last().unwrap(),
                _ => return Ok(Some(id)),
            }
        }
    }
    Ok(None)
}

/// Takes an empty leaf out of the tree.
fn unlink(pager: &mut Pager, mut path: Vec<(PageId, usize)>, leaf: PageId) -> io::Result<()> {
    let Node::Leaf { next, .. } = pager.get(leaf)? else { unreachable!() };
    let next = *next;
    if let Some(prev) = previous_leaf(pager, &path)?
        && let Node::Leaf { next: n, .. } = pager.get_mut(prev)?
    {
        *n = next;
    }
    while let Some((parent, i)) = path.pop() {
        let Node::Interior { keys, children } = pager.get_mut(parent)? else { unreachable!() };
        children.remove(i);
        if !keys.is_empty() {
            keys.remove(i.saturating_sub(1));
        }
        if !children.is_empty() {
            return Ok(());
        }
        // The parent is empty too. The root becomes an empty leaf; any other parent is removed
        // from its own parent, its page left behind as an (unreachable) empty leaf so that it
        // still encodes.
        *pager.get_mut(parent)? = Node::empty_leaf();
        if parent == ROOT {
            return Ok(());
        }
    }
    Ok(())
}

/// Calls `f` on every entry in the range, in key order, until it returns false.
pub fn scan(pager: &mut Pager, from: Bound<&[u8]>, to: Bound<&[u8]>, mut f: impl FnMut(&[u8], &[u8]) -> bool) -> io::Result<()> {
    let start: &[u8] = match from {
        Bound::Included(k) | Bound::Excluded(k) => k,
        Bound::Unbounded => &[],
    };
    let (_, mut leaf) = descend(pager, start)?;
    let mut first = true;
    loop {
        let Node::Leaf { keys, values, next } = pager.get(leaf)? else { unreachable!() };
        let begin = if first {
            match from {
                Bound::Included(k) => keys.partition_point(|x| x.as_slice() < k),
                Bound::Excluded(k) => keys.partition_point(|x| x.as_slice() <= k),
                Bound::Unbounded => 0,
            }
        } else {
            0
        };
        first = false;
        for i in begin..keys.len() {
            let k = keys[i].as_slice();
            let past = match to {
                Bound::Included(e) => k > e,
                Bound::Excluded(e) => k >= e,
                Bound::Unbounded => false,
            };
            if past || !f(k, &values[i]) {
                return Ok(());
            }
        }
        if *next == 0 {
            return Ok(());
        }
        leaf = *next;
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Shape {
    pub depth: usize,
    pub leaves: usize,
    pub interior: usize,
    pub entries: usize,
}

/// Checks the tree's invariants: keys sorted within and between nodes, every key inside the
/// bounds its parents set, all leaves at one depth, the leaf chain visiting the leaves in order,
/// and every node fitting its page.
pub fn check(pager: &mut Pager) -> Result<Shape, String> {
    let mut shape = Shape::default();
    let mut leaves = Vec::new();
    walk(pager, ROOT, None, None, 1, &mut shape, &mut leaves)?;
    for w in leaves.windows(2) {
        let Ok(Node::Leaf { next, .. }) = pager.get(w[0]) else { return Err("leaf vanished".into()) };
        if *next != w[1] {
            return Err(format!("leaf {} links to {}, not {}", w[0], next, w[1]));
        }
    }
    if let Some(&last) = leaves.last() {
        let Ok(Node::Leaf { next, .. }) = pager.get(last) else { return Err("leaf vanished".into()) };
        if *next != 0 {
            return Err(format!("last leaf {last} links to {next}"));
        }
    }
    Ok(shape)
}

fn walk(
    pager: &mut Pager,
    id: PageId,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    depth: usize,
    shape: &mut Shape,
    leaves: &mut Vec<PageId>,
) -> Result<(), String> {
    let node = pager.get(id).map_err(|e| format!("page {id}: {e}"))?.clone();
    if node.size() > PAGE_SIZE {
        return Err(format!("page {id} overfull"));
    }
    let in_bounds = |k: &[u8]| lo.as_deref().is_none_or(|l| k >= l) && hi.as_deref().is_none_or(|h| k < h);
    match node {
        Node::Leaf { keys, .. } => {
            if shape.depth == 0 {
                shape.depth = depth;
            } else if shape.depth != depth {
                return Err(format!("leaf {id} at depth {depth}, others at {}", shape.depth));
            }
            if keys.windows(2).any(|w| w[0] >= w[1]) {
                return Err(format!("leaf {id} unsorted"));
            }
            if let Some(k) = keys.iter().find(|k| !in_bounds(k)) {
                return Err(format!("leaf {id} key {k:?} outside its parent's bounds"));
            }
            shape.leaves += 1;
            shape.entries += keys.len();
            leaves.push(id);
        }
        Node::Interior { keys, children } => {
            if children.len() != keys.len() + 1 || keys.windows(2).any(|w| w[0] >= w[1]) {
                return Err(format!("interior {id} malformed"));
            }
            if keys.iter().any(|k| !in_bounds(k)) {
                return Err(format!("interior {id} separator outside its parent's bounds"));
            }
            shape.interior += 1;
            for (i, &c) in children.iter().enumerate() {
                let clo = if i == 0 { lo.clone() } else { Some(keys[i - 1].clone()) };
                let chi = if i == keys.len() { hi.clone() } else { Some(keys[i].clone()) };
                walk(pager, c, clo, chi, depth + 1, shape, leaves)?;
            }
        }
        Node::Meta { .. } => return Err(format!("meta node at page {id}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pager::Options;
    use crate::storage::vfs::SimVfs;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn open(vfs: &SimVfs) -> Pager {
        Pager::open(vfs, Path::new("db"), Options { cache_pages: 64, checkpoint_bytes: 1 << 20 }).unwrap()
    }

    #[test]
    fn matches_a_btreemap_under_random_operations() {
        let vfs = SimVfs::new();
        let mut p = open(&vfs);
        let mut model = BTreeMap::new();
        let mut rng = fastrand::Rng::with_seed(7);
        for round in 0..20_000 {
            let k = format!("{:06}", rng.u32(0..5000)).into_bytes();
            if rng.u8(0..4) == 0 {
                assert_eq!(delete(&mut p, &k).unwrap(), model.remove(&k).is_some());
            } else {
                let v = vec![rng.u8(..); rng.usize(0..120)];
                insert(&mut p, &k, &v).unwrap();
                model.insert(k, v);
            }
            if round % 997 == 0 {
                p.commit().unwrap();
            }
        }
        check(&mut p).unwrap();
        let mut all = Vec::new();
        scan(&mut p, Bound::Unbounded, Bound::Unbounded, |k, v| {
            all.push((k.to_vec(), v.to_vec()));
            true
        })
        .unwrap();
        assert_eq!(all, model.clone().into_iter().collect::<Vec<_>>());
        for (k, v) in model.iter().take(300) {
            assert_eq!(get(&mut p, k).unwrap().as_ref(), Some(v));
        }
        // Range bounds.
        let (lo, hi) = (b"001000".as_slice(), b"002000".as_slice());
        let mut got = Vec::new();
        scan(&mut p, Bound::Excluded(lo), Bound::Included(hi), |k, _| {
            got.push(k.to_vec());
            true
        })
        .unwrap();
        let want: Vec<_> = model.range::<[u8], _>((Bound::Excluded(lo), Bound::Included(hi))).map(|(k, _)| k.clone()).collect();
        assert_eq!(got, want);
        // Reopen: everything committed is back, through the log.
        p.commit().unwrap();
        drop(p);
        let mut p = open(&vfs);
        check(&mut p).unwrap();
        let mut n = 0;
        scan(&mut p, Bound::Unbounded, Bound::Unbounded, |_, _| {
            n += 1;
            true
        })
        .unwrap();
        assert_eq!(n, model.len());
    }

    #[test]
    fn emptied_leaves_leave_the_tree() {
        let vfs = SimVfs::new();
        let mut p = open(&vfs);
        for i in 0..3000u32 {
            insert(&mut p, &i.to_be_bytes(), &[7; 100]).unwrap();
        }
        let full = check(&mut p).unwrap();
        // Delete a block in the middle, then the start, then everything.
        for i in 1000..2500u32 {
            delete(&mut p, &i.to_be_bytes()).unwrap();
        }
        for i in 0..300u32 {
            delete(&mut p, &i.to_be_bytes()).unwrap();
        }
        let shape = check(&mut p).unwrap();
        assert_eq!(shape.entries, 1200);
        assert!(shape.leaves < full.leaves / 2, "{shape:?} vs {full:?}");
        let mut n = 0;
        scan(&mut p, Bound::Included(&999u32.to_be_bytes()[..]), Bound::Unbounded, |_, _| {
            n += 1;
            true
        })
        .unwrap();
        assert_eq!(n, 1 + 500);
        for i in (300..1000u32).chain(2500..3000) {
            delete(&mut p, &i.to_be_bytes()).unwrap();
        }
        assert_eq!(check(&mut p).unwrap(), Shape { depth: 1, leaves: 1, interior: 0, entries: 0 });
        insert(&mut p, b"again", b"1").unwrap();
        assert_eq!(get(&mut p, b"again").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn big_entries_split_and_oversized_ones_are_refused() {
        let vfs = SimVfs::new();
        let mut p = open(&vfs);
        for i in 0..500u32 {
            insert(&mut p, &i.to_be_bytes(), &[i as u8; 900]).unwrap();
        }
        let shape = check(&mut p).unwrap();
        assert_eq!(shape.entries, 500);
        assert_eq!(shape.depth, 2); // four ~900-byte entries a leaf, the 4-byte separators fit one interior level
        assert!(shape.leaves >= 125);
        assert!(insert(&mut p, b"k", &[0; MAX_ENTRY]).is_err());
    }
}
