//! HAP building blocks, taken from crtd (`src/raop/hap.rs`): TLV8, SRP-6a (SHA-512, 3072-bit
//! group, user "Pair-Setup"), HKDF, ChaCha20-Poly1305, and the encrypted session framing:
//! blocks of [u16 LE length][ChaCha20-Poly1305(<= 1024 bytes)][16-byte tag], length as AAD,
//! nonce = 4 zero bytes + u64 LE block counter, keys HKDF-SHA512(secret, "Control-Salt",
//! "Control-{Read,Write}-Encryption-Key").

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use hkdf::Hkdf;
use num_bigint::BigUint;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha512};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};



// ---------------------------------------------------------------------------------------- TLV8

#[allow(dead_code)] // the full set of HomeKit TLV types, some unused so far
pub mod tlv {
    pub const METHOD: u8 = 0;
    pub const IDENTIFIER: u8 = 1;
    pub const SALT: u8 = 2;
    pub const PUBLIC_KEY: u8 = 3;
    pub const PROOF: u8 = 4;
    pub const ENCRYPTED_DATA: u8 = 5;
    pub const STATE: u8 = 6;
    pub const ERROR: u8 = 7;
    pub const SIGNATURE: u8 = 10;
    pub const FLAGS: u8 = 19;

    /// Items in order; consecutive fragments of one type are joined.
    pub fn parse(mut data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut last_full = false;
        while data.len() >= 2 {
            let (t, n) = (data[0], data[1] as usize);
            let v = &data[2..(2 + n).min(data.len())];
            match out.last_mut() {
                Some((lt, lv)) if *lt == t && last_full => lv.extend_from_slice(v),
                _ => out.push((t, v.to_vec())),
            }
            last_full = n == 255;
            data = &data[(2 + n).min(data.len())..];
        }
        out
    }

    pub fn get(items: &[(u8, Vec<u8>)], t: u8) -> Option<&[u8]> {
        items.iter().find(|(it, _)| *it == t).map(|(_, v)| v.as_slice())
    }

    pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (t, v) in items {
            if v.is_empty() {
                out.extend_from_slice(&[*t, 0]);
            }
            for chunk in v.chunks(255) {
                out.push(*t);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out
    }
}

/// HomeKit error codes (TLV type 7).
pub(crate) const ERR_AUTH: u8 = 2;

pub(crate) fn error_reply(state: u8, code: u8) -> Vec<u8> {
    tlv::encode(&[(tlv::STATE, &[state]), (tlv::ERROR, &[code])])
}

// ----------------------------------------------------------------------------------------- SRP

const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";
const USER: &str = "Pair-Setup";

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap()
}

pub(crate) fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Left-pad to the size of N (384 bytes).
fn pad(x: &BigUint) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; 384usize.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

pub(crate) fn random_bytes<const L: usize>() -> [u8; L] {
    let mut b = [0u8; L];
    OsRng.fill_bytes(&mut b);
    b
}

pub(crate) struct Srp {
    pub(crate) salt: Vec<u8>,
    v: BigUint,
    b: BigUint,
    pub(crate) pub_b: BigUint,
}

impl Srp {
    /// SRP verifier for `pin` (AirPlay's transient pairing: "3939"; HomeKit: the setup code).
    pub(crate) fn new(pin: &str) -> Self {
        let (nn, g) = (n(), BigUint::from(5u32));
        let mut salt = random_bytes::<16>();
        salt[0] |= 0x80; // no leading zero: the sender hashes the salt as a number
        let x = BigUint::from_bytes_be(&sha512(&[&salt, &sha512(&[format!("{USER}:{pin}").as_bytes()])]));
        let v = g.modpow(&x, &nn);
        let b = BigUint::from_bytes_be(&random_bytes::<32>());
        let k = BigUint::from_bytes_be(&sha512(&[&pad(&nn), &pad(&g)]));
        let pub_b = (&k * &v + g.modpow(&b, &nn)) % &nn;
        Self { salt: salt.to_vec(), v, b, pub_b }
    }

    /// Check the sender's proof; returns (our proof, session key K).
    pub(crate) fn verify(&self, pub_a: &[u8], proof: &[u8]) -> Result<([u8; 64], [u8; 64]), String> {
        let nn = n();
        let a = BigUint::from_bytes_be(pub_a);
        if (&a % &nn) == BigUint::from(0u32) {
            return Err("SRP: A is zero".into());
        }
        let u = BigUint::from_bytes_be(&sha512(&[&pad(&a), &pad(&self.pub_b)]));
        let s = (&a * self.v.modpow(&u, &nn)).modpow(&self.b, &nn);
        let key = sha512(&[&s.to_bytes_be()]);
        let (hn, hg) = (sha512(&[&nn.to_bytes_be()]), sha512(&[&[5u8]]));
        let xor: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        let a_bytes = a.to_bytes_be();
        let m1 = sha512(&[&xor, &sha512(&[USER.as_bytes()]), &self.salt, &a_bytes, &self.pub_b.to_bytes_be(), &key]);
        if m1.as_slice() != proof {
            return Err("SRP: sender's proof does not match (wrong PIN?)".into());
        }
        Ok((sha512(&[&a_bytes, &m1, &key]), key))
    }
}

pub fn hkdf32(secret: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha512>::new(Some(salt.as_bytes()), secret).expand(info.as_bytes(), &mut out).unwrap();
    out
}

pub(crate) fn nonce_label(label: &[u8; 8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(label);
    n
}

pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], data: &mut Vec<u8>) {
    let tag = ChaCha20Poly1305::new(key.into()).encrypt_in_place_detached(Nonce::from_slice(nonce), aad, data).unwrap();
    data.extend_from_slice(&tag);
}

pub fn open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 16 {
        return Err("encrypted data too short".into());
    }
    let (ct, tag) = data.split_at(data.len() - 16);
    let mut buf = ct.to_vec();
    ChaCha20Poly1305::new(key.into())
        .decrypt_in_place_detached(Nonce::from_slice(nonce), aad, &mut buf, Tag::from_slice(tag))
        .map_err(|_| "decryption failed".to_string())?;
    Ok(buf)
}

// -------------------------------------------------------------------- encrypted control channel

const BLOCK: usize = 1024;

pub struct Cipher {
    key: [u8; 32],
    counter: u64,
}

impl Cipher {
    fn nonce(&mut self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        n
    }

    pub fn seal_all(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + plain.len() / BLOCK * 18 + 18);
        for chunk in plain.chunks(BLOCK) {
            let len = (chunk.len() as u16).to_le_bytes();
            let mut data = chunk.to_vec();
            let nonce = self.nonce();
            seal(&self.key, &nonce, &len, &mut data);
            out.extend_from_slice(&len);
            out.extend_from_slice(&data);
        }
        out
    }

    pub fn open_block(&mut self, len: [u8; 2], data: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = self.nonce();
        open(&self.key, &nonce, &len, data)
    }
}

/// (what we read, what we write) for the control channel, from the server's side.
pub fn control_ciphers(secret: &[u8]) -> (Cipher, Cipher) {
    let read = hkdf32(secret, "Control-Salt", "Control-Write-Encryption-Key");
    let write = hkdf32(secret, "Control-Salt", "Control-Read-Encryption-Key");
    (Cipher { key: read, counter: 0 }, Cipher { key: write, counter: 0 })
}

pub type Slot = Arc<Mutex<Option<Cipher>>>;

/// Read side of a connection; transparent until a cipher is put in its slot.
pub struct SecureReader {
    inner: TcpStream,
    slot: Slot,
    buf: Vec<u8>,
    pos: usize,
}

impl SecureReader {
    pub fn new(inner: TcpStream, slot: Slot) -> Self {
        Self { inner, slot, buf: Vec::new(), pos: 0 }
    }
}

impl Read for SecureReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        let mut slot = self.slot.lock().unwrap();
        let Some(cipher) = slot.as_mut() else {
            drop(slot);
            return self.inner.read(out);
        };
        let mut len = [0u8; 2];
        match self.inner.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(e),
        }
        let n = u16::from_le_bytes(len) as usize;
        if n > BLOCK {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "encrypted block too large"));
        }
        let mut data = vec![0u8; n + 16];
        self.inner.read_exact(&mut data)?;
        self.buf = cipher.open_block(len, &data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.pos = 0;
        drop(slot);
        self.read(out)
    }
}

/// Write side of a connection; transparent until a cipher is put in its slot.
pub struct SecureWriter {
    inner: TcpStream,
    slot: Slot,
}

impl SecureWriter {
    pub fn new(inner: TcpStream, slot: Slot) -> Self {
        Self { inner, slot }
    }

    pub fn raw(&self) -> io::Result<TcpStream> {
        self.inner.try_clone()
    }
}

impl Write for SecureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut slot = self.slot.lock().unwrap();
        match slot.as_mut() {
            None => self.inner.write(buf),
            Some(c) => {
                let sealed = c.seal_all(buf);
                self.inner.write_all(&sealed)?;
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlv_fragments_roundtrip() {
        let big = vec![7u8; 600];
        let enc = tlv::encode(&[(tlv::STATE, &[2]), (tlv::PUBLIC_KEY, &big), (tlv::PROOF, &[1, 2])]);
        let items = tlv::parse(&enc);
        assert_eq!(items.len(), 3);
        assert_eq!(tlv::get(&items, tlv::PUBLIC_KEY).unwrap(), &big[..]);
        assert_eq!(tlv::get(&items, tlv::PROOF).unwrap(), &[1, 2]);
    }

}
