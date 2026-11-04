//! Order-preserving key encoding: comparing encoded bytes gives the order of the values.
//!
//! Byte strings are escaped (0x00 -> 0x00 0xff) and end in 0x00 0x00, so one key can follow
//! another without the first's bytes running into the second's; integers are big-endian with
//! the sign bit flipped; floats are flipped so negatives sort below positives.

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_bytes_open(out, b);
    out.extend_from_slice(&[0, 0]);
}

/// The escaped bytes without the terminator: a prefix of the encoding of every key starting with `b`.
pub fn put_bytes_open(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        if c == 0 {
            out.extend_from_slice(&[0, 0xff]);
        } else {
            out.push(c);
        }
    }
}

/// Decodes an escaped byte string; returns it and the rest of the input.
pub fn take_bytes(input: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0 {
            match input.get(i + 1)? {
                0 => return Some((out, &input[i + 2..])),
                0xff => out.push(0),
                _ => return None,
            }
            i += 2;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    None
}

pub fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_be_bytes());
}

pub fn take_u64(input: &[u8]) -> Option<(u64, &[u8])> {
    if input.len() < 8 {
        return None;
    }
    Some((u64::from_be_bytes(input[..8].try_into().unwrap()), &input[8..]))
}

pub fn put_i64(out: &mut Vec<u8>, n: i64) {
    put_u64(out, (n as u64) ^ (1 << 63));
}

pub fn take_i64(input: &[u8]) -> Option<(i64, &[u8])> {
    take_u64(input).map(|(n, rest)| ((n ^ (1 << 63)) as i64, rest))
}

pub fn put_f64(out: &mut Vec<u8>, f: f64) {
    let f = if f == 0.0 { 0.0 } else { f }; // -0.0 and 0.0 are one key
    let bits = f.to_bits();
    let ordered = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
    put_u64(out, ordered);
}

pub fn take_f64(input: &[u8]) -> Option<(f64, &[u8])> {
    take_u64(input).map(|(n, rest)| {
        let bits = if n >> 63 == 1 { n ^ (1 << 63) } else { !n };
        (f64::from_bits(bits), rest)
    })
}

/// The smallest key greater than every key starting with `prefix`, if there is one.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc<T>(f: fn(&mut Vec<u8>, T), v: T) -> Vec<u8> {
        let mut out = Vec::new();
        f(&mut out, v);
        out
    }

    #[test]
    fn order_is_preserved() {
        let ints = [i64::MIN, -5, -1, 0, 1, 7, i64::MAX];
        for w in ints.windows(2) {
            assert!(enc(put_i64, w[0]) < enc(put_i64, w[1]));
        }
        let floats = [f64::NEG_INFINITY, -1e10, -0.5, 0.0, 1e-9, 2.5, f64::INFINITY];
        for w in floats.windows(2) {
            assert!(enc(put_f64, w[0]) < enc(put_f64, w[1]), "{} {}", w[0], w[1]);
        }
        let strs: [&[u8]; 5] = [b"", b"\x00", b"\x00\x01", b"a", b"ab"];
        for w in strs.windows(2) {
            assert!(enc(put_bytes, w[0]) < enc(put_bytes, w[1]));
        }
        // A key followed by more data still sorts by the key first.
        let mut a = enc(put_bytes, b"a".as_slice());
        a.extend_from_slice(&[0xff; 4]);
        assert!(a < enc(put_bytes, b"a\x00".as_slice()));
    }

    #[test]
    fn round_trips() {
        for s in [b"".as_slice(), b"\x00\x00x\xff", b"hello"] {
            let mut e = enc(put_bytes, s);
            e.push(42);
            let (d, rest) = take_bytes(&e).unwrap();
            assert_eq!((d.as_slice(), rest), (s, [42].as_slice()));
        }
        for n in [i64::MIN, -3, 0, 99, i64::MAX] {
            assert_eq!(take_i64(&enc(put_i64, n)).unwrap().0, n);
        }
        for f in [-2.5, 0.0, 1e300, f64::NEG_INFINITY] {
            assert_eq!(take_f64(&enc(put_f64, f)).unwrap().0, f);
        }
        assert_eq!(prefix_end(&[1, 0xff, 0xff]), Some(vec![2]));
        assert_eq!(prefix_end(&[0xff]), None);
    }
}
