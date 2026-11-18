//! SRTP / SRTCP sender, AES_CM_128_HMAC_SHA1_80 only (RFC 3711), key derivation rate 0.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type Ctr = ctr::Ctr128BE<Aes128>;
type HmacSha1 = Hmac<Sha1>;

/// AES-CM keystream XOR with a 14-byte salt-derived IV (`iv` = 112 bits, counter in the low 16).
fn aes_cm(key: &[u8; 16], iv14: &[u8; 14], data: &mut [u8]) {
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(iv14);
    Ctr::new(key.into(), &iv.into()).apply_keystream(data);
}

/// Session key derivation (RFC 3711 4.3.1) with kdr = 0.
fn derive(master_key: &[u8; 16], master_salt: &[u8; 14], label: u8, out: &mut [u8]) {
    let mut x = *master_salt;
    x[7] ^= label;
    out.iter_mut().for_each(|b| *b = 0);
    aes_cm(master_key, &x, out);
}

struct Keys {
    enc: [u8; 16],
    auth: [u8; 20],
    salt: [u8; 14],
}

impl Keys {
    fn new(master: &[u8], base: u8) -> Keys {
        let mk: [u8; 16] = master[..16].try_into().unwrap();
        let ms: [u8; 14] = master[16..30].try_into().unwrap();
        let mut k = Keys { enc: [0; 16], auth: [0; 20], salt: [0; 14] };
        derive(&mk, &ms, base, &mut k.enc);
        derive(&mk, &ms, base + 1, &mut k.auth);
        derive(&mk, &ms, base + 2, &mut k.salt);
        k
    }

    fn iv(&self, ssrc: u32, index: u64) -> [u8; 14] {
        let mut iv = self.salt;
        for (i, b) in ssrc.to_be_bytes().iter().enumerate() {
            iv[4 + i] ^= b;
        }
        for (i, b) in index.to_be_bytes()[2..].iter().enumerate() {
            iv[8 + i] ^= b;
        }
        iv
    }

    fn tag(&self, parts: &[&[u8]]) -> [u8; 10] {
        let mut m = <HmacSha1 as Mac>::new_from_slice(&self.auth).unwrap();
        for p in parts {
            m.update(p);
        }
        m.finalize().into_bytes()[..10].try_into().unwrap()
    }
}

/// Protects one sender's RTP and RTCP (a single SSRC).
pub struct Sender {
    rtp: Keys,
    rtcp: Keys,
    roc: u32,
    last_seq: Option<u16>,
    rtcp_index: u32,
}

impl Sender {
    /// `master` = 16-byte master key + 14-byte master salt.
    pub fn new(master: &[u8]) -> Sender {
        Sender { rtp: Keys::new(master, 0), rtcp: Keys::new(master, 3), roc: 0, last_seq: None, rtcp_index: 0 }
    }

    /// RTP packet (12-byte header, no CSRCs/extension) → SRTP packet.
    pub fn protect_rtp(&mut self, pkt: &[u8]) -> Vec<u8> {
        let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
        let ssrc = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
        if self.last_seq.is_some_and(|l| seq < l && l - seq > 0x8000) {
            self.roc = self.roc.wrapping_add(1);
        }
        self.last_seq = Some(seq);
        let index = (self.roc as u64) << 16 | seq as u64;
        let mut out = pkt.to_vec();
        aes_cm(&self.rtp.enc, &self.rtp.iv(ssrc, index), &mut out[12..]);
        let tag = self.rtp.tag(&[&out, &self.roc.to_be_bytes()]);
        out.extend_from_slice(&tag);
        out
    }

    /// RTCP compound packet → SRTCP (encrypted, E flag set).
    pub fn protect_rtcp(&mut self, pkt: &[u8]) -> Vec<u8> {
        let ssrc = u32::from_be_bytes(pkt[4..8].try_into().unwrap());
        let index = self.rtcp_index;
        self.rtcp_index = (self.rtcp_index + 1) & 0x7fff_ffff;
        let mut out = pkt.to_vec();
        aes_cm(&self.rtcp.enc, &self.rtcp.iv(ssrc, index as u64), &mut out[8..]);
        out.extend_from_slice(&(0x8000_0000 | index).to_be_bytes());
        let tag = self.rtcp.tag(&[&out]);
        out.extend_from_slice(&tag);
        out
    }
}

/// Unprotects one remote sender's SRTP (the controller's microphone).
pub struct Receiver {
    rtp: Keys,
    rtcp: Keys,
    roc: u32,
    last_seq: Option<u16>,
}

impl Receiver {
    pub fn new(master: &[u8]) -> Receiver {
        Receiver { rtp: Keys::new(master, 0), rtcp: Keys::new(master, 3), roc: 0, last_seq: None }
    }

    /// SRTCP packet → RTCP compound packet; None if too short or the tag is wrong.
    pub fn unprotect_rtcp(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < 8 + 4 + 10 {
            return None;
        }
        let (body, tag) = pkt.split_at(pkt.len() - 10);
        if self.rtcp.tag(&[body]) != tag {
            return None;
        }
        let (rtcp, e_index) = body.split_at(body.len() - 4);
        let e_index = u32::from_be_bytes(e_index.try_into().ok()?);
        let mut out = rtcp.to_vec();
        if e_index & 0x8000_0000 != 0 {
            let ssrc = u32::from_be_bytes(out[4..8].try_into().ok()?);
            aes_cm(&self.rtcp.enc, &self.rtcp.iv(ssrc, (e_index & 0x7fff_ffff) as u64), &mut out[8..]);
        }
        Some(out)
    }

    /// SRTP packet → RTP packet (header + plain payload); None if too short or the tag is wrong.
    pub fn unprotect_rtp(&mut self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < 12 + 10 {
            return None;
        }
        let (body, tag) = pkt.split_at(pkt.len() - 10);
        let seq = u16::from_be_bytes([body[2], body[3]]);
        let ssrc = u32::from_be_bytes(body[8..12].try_into().ok()?);
        // guess the rollover counter from the sequence number (RFC 3711 3.3.1, simplified)
        let roc = match self.last_seq {
            Some(l) if seq < l && l - seq > 0x8000 => self.roc.wrapping_add(1),
            Some(l) if seq > l && seq - l > 0x8000 => self.roc.wrapping_sub(1),
            _ => self.roc,
        };
        if self.rtp.tag(&[body, &roc.to_be_bytes()]) != tag {
            return None;
        }
        if roc != self.roc.wrapping_sub(1) {
            self.roc = roc;
            self.last_seq = Some(seq);
        }
        let mut out = body.to_vec();
        aes_cm(&self.rtp.enc, &self.rtp.iv(ssrc, (roc as u64) << 16 | seq as u64), &mut out[12..]);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rtcp_roundtrip() {
        let master = [7u8; 30];
        let rr = [0x81, 201, 0, 7, 1, 2, 3, 4, 9, 9, 9, 9, 0x10, 0, 0, 5, 0, 0, 1, 0, 0, 0, 0, 90, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut tx = Sender::new(&master);
        let rx = Receiver::new(&master);
        let p = tx.protect_rtcp(&rr);
        assert_ne!(&p[8..32], &rr[8..]);
        assert_eq!(rx.unprotect_rtcp(&p).unwrap(), rr);
        let mut bad = p.clone();
        bad[10] ^= 1;
        assert!(rx.unprotect_rtcp(&bad).is_none());
    }

    /// RFC 3711 appendix B.3.
    #[test]
    fn key_derivation_vectors() {
        let mut master = hex("E1F97A0D3E018BE0D64FA32C06DE4139");
        master.extend(hex("0EC675AD498AFEEBB6960B3AABE6"));
        let k = Keys::new(&master, 0);
        assert_eq!(k.enc.to_vec(), hex("C61E7A93744F39EE10734AFE3FF7A087"));
        assert_eq!(k.salt.to_vec(), hex("30CBBC08863D8C85D49DB34A9AE1"));
        assert_eq!(k.auth.to_vec(), hex("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4"));
    }

    /// RFC 3711 appendix B.2: AES-CM keystream.
    #[test]
    fn aes_cm_vector() {
        let key: [u8; 16] = hex("2B7E151628AED2A6ABF7158809CF4F3C").try_into().unwrap();
        let iv: [u8; 14] = hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD").try_into().unwrap();
        let mut ks = [0u8; 32];
        aes_cm(&key, &iv, &mut ks);
        assert_eq!(ks.to_vec(), hex("E03EAD0935C95E80E166B16DD92B4EB4D23513162B02D0F72A43A2FE4A5F97AB"));
    }

    #[test]
    fn rtp_grows_by_tag_and_rolls_over() {
        let mut s = Sender::new(&[7u8; 30]);
        let mut p = vec![0x80, 96, 0xff, 0xff, 0, 0, 0, 1, 1, 2, 3, 4, 9, 9, 9];
        assert_eq!(s.protect_rtp(&p).len(), p.len() + 10);
        p[2] = 0;
        p[3] = 0;
        s.protect_rtp(&p);
        assert_eq!(s.roc, 1);
        let sr = [0x80, 200, 0, 6, 1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(s.protect_rtcp(&sr).len(), sr.len() + 14);
    }

    #[test]
    fn receiver_roundtrip_and_rejects_tampering() {
        let key = [5u8; 30];
        let (mut tx, mut rx) = (Sender::new(&key), Receiver::new(&key));
        for seq in [0xfffeu16, 0xffff, 0, 1] {
            let mut p = vec![0x80, 110, 0, 0, 0, 0, 0, 1, 9, 8, 7, 6, 1, 2, 3, 4, 5];
            p[2..4].copy_from_slice(&seq.to_be_bytes());
            let mut wire = tx.protect_rtp(&p);
            assert_eq!(rx.unprotect_rtp(&wire).unwrap(), p);
            wire[13] ^= 1;
            assert!(rx.unprotect_rtp(&wire).is_none());
        }
    }
}
