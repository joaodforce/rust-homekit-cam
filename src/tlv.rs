//! TLV8 for the camera characteristics (nested, base64 in JSON).

use crate::hap::crypto::tlv;

/// Builds a TLV8 value item by item.
#[derive(Default)]
pub struct Tlv(Vec<(u8, Vec<u8>)>);

impl Tlv {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn bytes(mut self, t: u8, v: &[u8]) -> Self {
        self.0.push((t, v.to_vec()));
        self
    }
    pub fn u8(self, t: u8, v: u8) -> Self {
        self.bytes(t, &[v])
    }
    pub fn u16(self, t: u8, v: u16) -> Self {
        self.bytes(t, &v.to_le_bytes())
    }
    pub fn u32(self, t: u8, v: u32) -> Self {
        self.bytes(t, &v.to_le_bytes())
    }
    pub fn tlv(self, t: u8, v: Tlv) -> Self {
        self.bytes(t, &v.encode())
    }
    /// The empty separator between repeated items of the same type.
    pub fn sep(self) -> Self {
        self.bytes(0, &[])
    }
    pub fn encode(&self) -> Vec<u8> {
        let items: Vec<(u8, &[u8])> = self.0.iter().map(|(t, v)| (*t, v.as_slice())).collect();
        tlv::encode(&items)
    }
    pub fn base64(&self) -> String {
        b64(&self.encode())
    }
}

/// Parsed items (fragments joined), in order, repeats kept.
pub fn parse(b: &[u8]) -> Vec<(u8, Vec<u8>)> {
    tlv::parse(b)
}

pub fn get(items: &[(u8, Vec<u8>)], t: u8) -> Option<&[u8]> {
    items.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice())
}

pub fn get_uint(items: &[(u8, Vec<u8>)], t: u8) -> Option<u64> {
    let v = get(items, t)?;
    (v.len() <= 8).then(|| v.iter().rev().fold(0u64, |a, b| (a << 8) | *b as u64))
}

pub fn b64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s {
        acc = acc << 6 | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for v in [&b""[..], b"f", b"fo", b"foo", b"foob", &[0xff, 0, 7, 9, 200]] {
            assert_eq!(unb64(&b64(v)).unwrap(), v);
        }
        assert_eq!(b64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn nested_and_uint() {
        let t = Tlv::new().u8(1, 0).tlv(2, Tlv::new().u16(1, 1920).u16(2, 1080)).sep().u32(6, 0x01020304).encode();
        let items = parse(&t);
        assert_eq!(get_uint(&items, 6), Some(0x01020304));
        assert_eq!(get_uint(&parse(get(&items, 2).unwrap()), 1), Some(1920));
    }
}
