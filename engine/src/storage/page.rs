//! Page layout. A page is [`PAGE_SIZE`] bytes and holds one node:
//!
//! ```text
//! meta      1 | magic(8) | page_size u32 | page_count u32
//! leaf      2 | n u16 | next u32 | n x (key_len varint, key, value_len varint, value)
//! interior  3 | n u16 | child0 u32 | n x (key_len varint, key, child u32)
//! ```
//!
//! Interior key i separates child i (keys < key i) from child i+1 (keys >= key i). Leaves are
//! chained left to right by `next` (0 = none). Nodes are kept decoded in the page cache and
//! encoded only when written to the log, so `size` is tracked as entries come and go.

use std::io;

pub const PAGE_SIZE: usize = 4096;
/// The largest key + value an entry may hold: a page then always fits at least three.
pub const MAX_ENTRY: usize = 1000;
const MAGIC: &[u8; 8] = b"LODESTN1";
const HEADER: usize = 7;

pub type PageId = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Meta { page_count: u32 },
    Leaf { keys: Vec<Vec<u8>>, values: Vec<Vec<u8>>, next: PageId },
    Interior { keys: Vec<Vec<u8>>, children: Vec<PageId> },
}

pub fn varint_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

fn put_varint(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt page: {what}"))
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.pos + n > self.buf.len() {
            return Err(corrupt("entry past the end"));
        }
        self.pos += n;
        Ok(&self.buf[self.pos - n..self.pos])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn varint(&mut self) -> io::Result<usize> {
        let mut n = 0usize;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            n |= ((b & 0x7f) as usize) << shift;
            if b < 0x80 {
                return Ok(n);
            }
        }
        Err(corrupt("varint"))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.varint()?;
        Ok(self.take(n)?.to_vec())
    }
}

pub fn leaf_entry_size(key: &[u8], value: &[u8]) -> usize {
    varint_len(key.len()) + key.len() + varint_len(value.len()) + value.len()
}

pub fn interior_entry_size(key: &[u8]) -> usize {
    varint_len(key.len()) + key.len() + 4
}

impl Node {
    pub fn empty_leaf() -> Node {
        Node::Leaf { keys: Vec::new(), values: Vec::new(), next: 0 }
    }

    /// Encoded size in bytes.
    pub fn size(&self) -> usize {
        match self {
            Node::Meta { .. } => 1 + 8 + 8,
            Node::Leaf { keys, values, .. } => HEADER + keys.iter().zip(values).map(|(k, v)| leaf_entry_size(k, v)).sum::<usize>(),
            Node::Interior { keys, .. } => HEADER + keys.iter().map(|k| interior_entry_size(k)).sum::<usize>(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PAGE_SIZE);
        match self {
            Node::Meta { page_count } => {
                out.push(1);
                out.extend_from_slice(MAGIC);
                out.extend_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
                out.extend_from_slice(&page_count.to_le_bytes());
            }
            Node::Leaf { keys, values, next } => {
                out.push(2);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                out.extend_from_slice(&next.to_le_bytes());
                for (k, v) in keys.iter().zip(values) {
                    put_varint(&mut out, k.len());
                    out.extend_from_slice(k);
                    put_varint(&mut out, v.len());
                    out.extend_from_slice(v);
                }
            }
            Node::Interior { keys, children } => {
                out.push(3);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                out.extend_from_slice(&children[0].to_le_bytes());
                for (k, c) in keys.iter().zip(&children[1..]) {
                    put_varint(&mut out, k.len());
                    out.extend_from_slice(k);
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
        }
        assert!(out.len() <= PAGE_SIZE, "node of {} bytes does not fit a page", out.len());
        out.resize(PAGE_SIZE, 0);
        out
    }

    pub fn decode(buf: &[u8]) -> io::Result<Node> {
        let mut r = Reader { buf, pos: 1 };
        match buf.first() {
            Some(1) => {
                if r.take(8)? != MAGIC {
                    return Err(corrupt("bad magic: not a Lodestone database"));
                }
                if r.u32()? as usize != PAGE_SIZE {
                    return Err(corrupt("page size"));
                }
                Ok(Node::Meta { page_count: r.u32()? })
            }
            Some(2) => {
                let n = r.u16()? as usize;
                let next = r.u32()?;
                let (mut keys, mut values) = (Vec::with_capacity(n), Vec::with_capacity(n));
                for _ in 0..n {
                    keys.push(r.bytes()?);
                    values.push(r.bytes()?);
                }
                Ok(Node::Leaf { keys, values, next })
            }
            Some(3) => {
                let n = r.u16()? as usize;
                let mut children = Vec::with_capacity(n + 1);
                children.push(r.u32()?);
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    keys.push(r.bytes()?);
                    children.push(r.u32()?);
                }
                Ok(Node::Interior { keys, children })
            }
            _ => Err(corrupt("unknown page type")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let nodes = [
            Node::Meta { page_count: 42 },
            Node::Leaf { keys: vec![b"a".to_vec(), vec![0; 300]], values: vec![vec![], vec![9; 200]], next: 7 },
            Node::Interior { keys: vec![b"m".to_vec(), b"t".to_vec()], children: vec![2, 3, 4] },
        ];
        for n in nodes {
            let buf = n.encode();
            assert_eq!(buf.len(), PAGE_SIZE);
            assert_eq!(Node::decode(&buf).unwrap(), n);
            let used = buf.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
            assert!(used <= n.size());
        }
        assert!(Node::decode(&[9; PAGE_SIZE]).is_err());
        assert!(Node::decode(&[2, 5, 0, 0, 0, 0, 0, 200]).is_err());
    }
}
