//! HomeKit pairing (from crtd), on top of the pieces AirPlay 2 already uses
//! (`raop::hap`: TLV8, SRP-6a, ChaCha20-Poly1305, HKDF).
//!
//! - pair-setup (M1..M6): SRP with our setup code proves the iPhone knows it; then both sides
//!   swap long-term Ed25519 public keys (M5/M6), each signed and encrypted. The iPhone becomes
//!   our first (admin) controller. Only possible while unpaired.
//! - pair-verify (M1..M4): a fresh X25519 exchange per connection, each side signing with its
//!   long-term key; we check the iPhone's signature against the key it gave us at pair-setup.
//!   The X25519 secret then keys the encrypted session.
//! - /pairings: admins add, remove and list controllers (the Home app shares the TV with the
//!   rest of the home this way).

use super::crypto::{self as hap, ERR_AUTH, Srp, error_reply, hkdf32, nonce_label, tlv};
use super::identity::Identity;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{EphemeralSecret, PublicKey};

const ERR_UNAVAILABLE: u8 = 6;
const ERR_UNKNOWN: u8 = 1;

/// A controller (an iPhone, or the home hub on behalf of the home's members).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pairing {
    pub id: String,
    /// Long-term Ed25519 public key, hex.
    pub ltpk: String,
    pub admin: bool,
}

/// What the accessory remembers (`<state_dir>/hap.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Store {
    /// Our accessory pairing id, MAC-style ("AB:CD:…").
    pub device_id: String,
    /// "123-45-678", entered in the Home app when adding the accessory.
    pub setup_code: String,
    /// 4 characters [0-9A-Z] that tie the setup QR code to our advert (`sh`).
    #[serde(default = "new_setup_id")]
    pub setup_id: String,
    pub pairings: Vec<Pairing>,
}

fn new_setup_id() -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    hap::random_bytes::<4>().iter().map(|b| CHARS[*b as usize % 36] as char).collect()
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Store {
    /// A fresh accessory: random id and setup code.
    pub fn new() -> Self {
        let r = hap::random_bytes::<10>();
        let device_id = r[..6].iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":");
        Self { device_id, setup_code: setup_code(&r[6..]), setup_id: new_setup_id(), pairings: Vec::new() }
    }

    /// What the setup QR code holds: "X-HM://" + 9 base-36 digits + the setup id. The number packs
    /// version 0, category 18 (video doorbell), flag 2 (IP) and the setup code's 8 digits.
    pub fn qr_payload(&self) -> String {
        let code: u64 = self.setup_code.replace('-', "").parse().unwrap_or(0);
        let mut n = (crate::hap::CATEGORY << 31) | (2 << 27) | (code & 0x7ff_ffff);
        let mut digits = Vec::new();
        for _ in 0..9 {
            digits.push(b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"[(n % 36) as usize]);
            n /= 36;
        }
        digits.reverse();
        format!("X-HM://{}{}", String::from_utf8(digits).unwrap(), self.setup_id)
    }

    /// The advert's `sh`: first 4 bytes of SHA-512(setup id + accessory id), base64.
    pub fn setup_hash(&self) -> String {
        base64(&hap::sha512(&[self.setup_id.as_bytes(), self.device_id.as_bytes()])[..4])
    }

    pub fn paired(&self) -> bool {
        !self.pairings.is_empty()
    }

    fn key_of(&self, id: &str) -> Option<VerifyingKey> {
        let p = self.pairings.iter().find(|p| p.id == id)?;
        VerifyingKey::from_bytes(&unhex(&p.ltpk)?.try_into().ok()?).ok()
    }

    pub fn is_admin(&self, id: &str) -> bool {
        self.pairings.iter().any(|p| p.id == id && p.admin)
    }
}

/// 8 digits as "XXX-XX-XXX", avoiding the codes HomeKit rejects (all the same, 12345678, 87654321).
fn setup_code(random: &[u8]) -> String {
    let mut n = u32::from_le_bytes([random[0], random[1], random[2], random[3]]) % 100_000_000;
    loop {
        let d = format!("{n:08}");
        let same = d.chars().all(|c| c == d.chars().next().unwrap());
        if !same && d != "12345678" && d != "87654321" {
            return format!("{}-{}-{}", &d[..3], &d[3..5], &d[5..]);
        }
        n = (n + 1) % 100_000_000;
    }
}

fn verify_sig(key: &VerifyingKey, msg: &[u8], sig: &[u8]) -> bool {
    Signature::from_slice(sig).is_ok_and(|s| key.verify(msg, &s).is_ok())
}

/// Reply body, and a change to the store if pairing finished.
pub type SetupStep = (Vec<u8>, Option<Pairing>);

#[derive(Default)]
pub struct PairSetup {
    srp: Option<Srp>,
    key: Option<[u8; 64]>,
}

impl PairSetup {
    pub fn step(&mut self, store: &Store, id: &Identity, body: &[u8]) -> Result<SetupStep, String> {
        let items = tlv::parse(body);
        let state = tlv::get(&items, tlv::STATE).and_then(|s| s.first().copied()).unwrap_or(0);
        match state {
            1 => {
                if store.paired() {
                    return Ok((error_reply(2, ERR_UNAVAILABLE), None)); // already someone's
                }
                let srp = Srp::new(&store.setup_code);
                let reply = tlv::encode(&[(tlv::STATE, &[2]), (tlv::SALT, &srp.salt), (tlv::PUBLIC_KEY, &srp.pub_b.to_bytes_be())]);
                self.srp = Some(srp);
                Ok((reply, None))
            }
            3 => {
                let srp = self.srp.as_ref().ok_or("pair-setup M3 before M1")?;
                let a = tlv::get(&items, tlv::PUBLIC_KEY).ok_or("pair-setup M3: no public key")?;
                let proof = tlv::get(&items, tlv::PROOF).ok_or("pair-setup M3: no proof")?;
                match srp.verify(a, proof) {
                    Ok((ours, key)) => {
                        self.key = Some(key);
                        Ok((tlv::encode(&[(tlv::STATE, &[4]), (tlv::PROOF, &ours)]), None))
                    }
                    Err(e) => {
                        crate::log(format!("pair-setup: {e}"));
                        Ok((error_reply(4, ERR_AUTH), None))
                    }
                }
            }
            5 => {
                let k = self.key.ok_or("pair-setup M5 before M3")?;
                let enc = tlv::get(&items, tlv::ENCRYPTED_DATA).ok_or("pair-setup M5: no data")?;
                let session = hkdf32(&k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
                let Ok(plain) = hap::open(&session, &nonce_label(b"PS-Msg05"), &[], enc) else {
                    return Ok((error_reply(6, ERR_AUTH), None));
                };
                let sub = tlv::parse(&plain);
                let (Some(ctrl_id), Some(ltpk), Some(sig)) =
                    (tlv::get(&sub, tlv::IDENTIFIER), tlv::get(&sub, tlv::PUBLIC_KEY), tlv::get(&sub, tlv::SIGNATURE))
                else {
                    return Ok((error_reply(6, ERR_AUTH), None));
                };
                // the iPhone signs (derived key | its pairing id | its long-term key)
                let x = hkdf32(&k, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info");
                let key = <[u8; 32]>::try_from(ltpk).ok().and_then(|b| VerifyingKey::from_bytes(&b).ok());
                if !key.is_some_and(|key| verify_sig(&key, &[&x[..], ctrl_id, ltpk].concat(), sig)) {
                    return Ok((error_reply(6, ERR_AUTH), None));
                }
                // our side: sign (derived key | our id | our long-term key), send it encrypted
                let x = hkdf32(&k, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info");
                let our_ltpk = id.public_key();
                let our_sig = id.sign(&[&x[..], store.device_id.as_bytes(), &our_ltpk].concat());
                let mut reply_sub = tlv::encode(&[
                    (tlv::IDENTIFIER, store.device_id.as_bytes()),
                    (tlv::PUBLIC_KEY, &our_ltpk),
                    (tlv::SIGNATURE, &our_sig),
                ]);
                hap::seal(&session, &nonce_label(b"PS-Msg06"), &[], &mut reply_sub);
                let pairing = Pairing { id: String::from_utf8_lossy(ctrl_id).into_owned(), ltpk: hex(ltpk), admin: true };
                Ok((tlv::encode(&[(tlv::STATE, &[6]), (tlv::ENCRYPTED_DATA, &reply_sub)]), Some(pairing)))
            }
            s => Err(format!("pair-setup: unexpected state {s}")),
        }
    }
}

/// Reply body, and when verified: (the session's shared secret, the controller's id).
pub type VerifyStep = (Vec<u8>, Option<([u8; 32], String)>);

#[derive(Default)]
pub struct PairVerify {
    state: Option<([u8; 32], [u8; 32], [u8; 32])>, // (shared, ours, theirs)
}

impl PairVerify {
    pub fn step(&mut self, store: &Store, id: &Identity, body: &[u8]) -> Result<VerifyStep, String> {
        let items = tlv::parse(body);
        let state = tlv::get(&items, tlv::STATE).and_then(|s| s.first().copied()).unwrap_or(0);
        match state {
            1 => {
                let theirs: [u8; 32] = tlv::get(&items, tlv::PUBLIC_KEY).and_then(|k| k.try_into().ok()).ok_or("pair-verify M1: bad key")?;
                let eph = EphemeralSecret::random_from_rng(OsRng);
                let ours = PublicKey::from(&eph).to_bytes();
                let shared = eph.diffie_hellman(&PublicKey::from(theirs)).to_bytes();
                let sig = id.sign(&[&ours[..], store.device_id.as_bytes(), &theirs[..]].concat());
                let mut sub = tlv::encode(&[(tlv::IDENTIFIER, store.device_id.as_bytes()), (tlv::SIGNATURE, &sig)]);
                let key = hkdf32(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
                hap::seal(&key, &nonce_label(b"PV-Msg02"), &[], &mut sub);
                self.state = Some((shared, ours, theirs));
                Ok((tlv::encode(&[(tlv::STATE, &[2]), (tlv::PUBLIC_KEY, &ours), (tlv::ENCRYPTED_DATA, &sub)]), None))
            }
            3 => {
                let (shared, ours, theirs) = self.state.ok_or("pair-verify M3 before M1")?;
                let enc = tlv::get(&items, tlv::ENCRYPTED_DATA).ok_or("pair-verify M3: no data")?;
                let key = hkdf32(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
                let Ok(plain) = hap::open(&key, &nonce_label(b"PV-Msg03"), &[], enc) else {
                    return Ok((error_reply(4, ERR_AUTH), None));
                };
                let sub = tlv::parse(&plain);
                let (Some(ctrl), Some(sig)) = (tlv::get(&sub, tlv::IDENTIFIER), tlv::get(&sub, tlv::SIGNATURE)) else {
                    return Ok((error_reply(4, ERR_AUTH), None));
                };
                let ctrl = String::from_utf8_lossy(ctrl).into_owned();
                // only controllers we paired with; they sign (their eph. key | their id | ours)
                let ok = store.key_of(&ctrl).is_some_and(|k| verify_sig(&k, &[&theirs[..], ctrl.as_bytes(), &ours[..]].concat(), sig));
                if !ok {
                    crate::log(format!("pair-verify: unknown controller or bad signature ({ctrl})"));
                    return Ok((error_reply(4, ERR_AUTH), None));
                }
                Ok((tlv::encode(&[(tlv::STATE, &[4])]), Some((shared, ctrl))))
            }
            s => Err(format!("pair-verify: unexpected state {s}")),
        }
    }
}

/// /pairings (admins only): add (3), remove (4), list (5). Returns the reply and the new
/// pairing list if it changed.
pub fn pairings(store: &Store, controller: &str, body: &[u8]) -> (Vec<u8>, Option<Vec<Pairing>>) {
    let items = tlv::parse(body);
    let method = tlv::get(&items, tlv::METHOD).and_then(|m| m.first().copied()).unwrap_or(0);
    if !store.is_admin(controller) {
        return (error_reply(2, ERR_AUTH), None);
    }
    let id = tlv::get(&items, tlv::IDENTIFIER).map(|i| String::from_utf8_lossy(i).into_owned());
    match method {
        3 => {
            let (Some(id), Some(ltpk)) = (id, tlv::get(&items, tlv::PUBLIC_KEY)) else { return (error_reply(2, ERR_UNKNOWN), None) };
            let admin = tlv::get(&items, PERMISSIONS).is_some_and(|p| p.first() == Some(&1));
            let mut list = store.pairings.clone();
            match list.iter_mut().find(|p| p.id == id) {
                Some(p) if p.ltpk != hex(ltpk) => return (error_reply(2, ERR_UNKNOWN), None),
                Some(p) => p.admin = admin,
                None => list.push(Pairing { id, ltpk: hex(ltpk), admin }),
            }
            (tlv::encode(&[(tlv::STATE, &[2])]), Some(list))
        }
        4 => {
            let Some(id) = id else { return (error_reply(2, ERR_UNKNOWN), None) };
            let mut list: Vec<Pairing> = store.pairings.iter().filter(|p| p.id != id).cloned().collect();
            if !list.iter().any(|p| p.admin) {
                list.clear(); // no admin left: back to unpaired
            }
            (tlv::encode(&[(tlv::STATE, &[2])]), Some(list))
        }
        5 => {
            let mut out = tlv::encode(&[(tlv::STATE, &[2])]);
            for (i, p) in store.pairings.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(&[SEPARATOR, 0]);
                }
                let ltpk = unhex(&p.ltpk).unwrap_or_default();
                out.extend(tlv::encode(&[(tlv::IDENTIFIER, p.id.as_bytes()), (tlv::PUBLIC_KEY, &ltpk), (PERMISSIONS, &[p.admin as u8])]));
            }
            (out, None)
        }
        _ => (error_reply(2, ERR_UNKNOWN), None),
    }
}

const PERMISSIONS: u8 = 0x0B;
const SEPARATOR: u8 = 0xFF;

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn qr_payload_packs_category_flags_and_code() {
        let s = Store { device_id: "AA:BB:CC:DD:EE:FF".into(), setup_code: "123-45-679".into(), setup_id: "ABCD".into(), pairings: vec![] };
        let p = s.qr_payload();
        assert!(p.starts_with("X-HM://") && p.ends_with("ABCD") && p.len() == 7 + 9 + 4, "{p}");
        let n = u64::from_str_radix(&p[7..16], 36).unwrap();
        assert_eq!((n >> 31 & 0xff, n >> 27 & 0xf, n & 0x7ff_ffff), (18, 2, 12345679));
        assert_eq!(base64(&[1, 2, 3, 4]), "AQIDBA==");
        assert_eq!(s.setup_hash().len(), 8);
    }

    #[test]
    fn setup_codes_are_valid() {
        assert_eq!(setup_code(&[0, 0, 0, 0]), "000-00-001"); // 00000000 is all the same
        let c = setup_code(&[78, 97, 188, 0]); // 12345678
        assert_eq!(c, "123-45-679");
        let s = Store::new();
        assert_eq!((s.device_id.len(), s.setup_code.len()), (17, 10));
    }

    /// Controller side of pair-verify against our accessory side.
    #[test]
    fn verify_accepts_only_known_controllers() {
        let acc = Identity::random();
        let ctrl = SigningKey::generate(&mut OsRng);
        let mut store = Store::new();
        let pairing = Pairing { id: "phone".into(), ltpk: hex(&ctrl.verifying_key().to_bytes()), admin: true };
        let run = |store: &Store, name: &str| {
            let eph = EphemeralSecret::random_from_rng(OsRng);
            let theirs = PublicKey::from(&eph).to_bytes();
            let mut v = PairVerify::default();
            let (m2, _) = v.step(store, &acc, &tlv::encode(&[(tlv::STATE, &[1]), (tlv::PUBLIC_KEY, &theirs)])).unwrap();
            let m2 = tlv::parse(&m2);
            let ours: [u8; 32] = tlv::get(&m2, tlv::PUBLIC_KEY).unwrap().try_into().unwrap();
            let shared = eph.diffie_hellman(&PublicKey::from(ours)).to_bytes();
            let key = hkdf32(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
            let sig = ctrl.sign(&[&theirs[..], name.as_bytes(), &ours[..]].concat()).to_bytes();
            let mut sub = tlv::encode(&[(tlv::IDENTIFIER, name.as_bytes()), (tlv::SIGNATURE, &sig)]);
            hap::seal(&key, &nonce_label(b"PV-Msg03"), &[], &mut sub);
            let (_, done) = v.step(store, &acc, &tlv::encode(&[(tlv::STATE, &[3]), (tlv::ENCRYPTED_DATA, &sub)])).unwrap();
            done.map(|(s, c)| (s == shared, c))
        };
        assert_eq!(run(&store, "phone"), None); // not paired yet
        store.pairings.push(pairing);
        assert_eq!(run(&store, "phone"), Some((true, "phone".into())));
        assert_eq!(run(&store, "stranger"), None);
    }

    #[test]
    fn pairings_admin_rules() {
        let mut store = Store::new();
        store.pairings.push(Pairing { id: "admin".into(), ltpk: "00".repeat(32), admin: true });
        let add = tlv::encode(&[(tlv::METHOD, &[3]), (tlv::IDENTIFIER, b"guest"), (tlv::PUBLIC_KEY, &[1; 32]), (PERMISSIONS, &[0])]);
        assert!(pairings(&store, "nobody", &add).1.is_none());
        store.pairings = pairings(&store, "admin", &add).1.unwrap();
        assert_eq!(store.pairings.len(), 2);
        let list = tlv::parse(&pairings(&store, "admin", &tlv::encode(&[(tlv::METHOD, &[5])])).0);
        assert_eq!(list.iter().filter(|(t, _)| *t == tlv::IDENTIFIER).count(), 2);
        // removing the only admin unpairs everything
        let rm = tlv::encode(&[(tlv::METHOD, &[4]), (tlv::IDENTIFIER, b"admin")]);
        assert!(pairings(&store, "admin", &rm).1.unwrap().is_empty());
    }
}
