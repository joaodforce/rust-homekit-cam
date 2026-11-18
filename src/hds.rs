//! HomeKit Data Stream (HDS): the channel a home hub pulls HomeKit Secure Video recordings over.
//!
//! - Setup: the controller writes SetupDataStreamTransport (its 32-byte salt); we answer with our
//!   TCP port and salt. Keys: HKDF-SHA512(HAP session secret, controller salt ‖ our salt,
//!   "HDS-Read-Encryption-Key" (we → controller) / "HDS-Write-Encryption-Key" (controller → us)).
//! - Frames: [type 1][u24 BE length][ChaCha20-Poly1305(payload), the 4-byte header as AAD][tag],
//!   nonce = 4 zero bytes + u64 LE counter per direction.
//! - Payload: [header length][header][message], both in Apple's "opack" encoding.
//! - Protocol: `control/hello` request, then `dataSend/open` (type "ipcamera.recording"),
//!   our `dataSend/data` events (fMP4 chunks), `dataSend/ack` and `dataSend/close` from the hub.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// -------------------------------------------------------------------------------- opack

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<V>),
    Dict(Vec<(V, V)>),
}

impl V {
    pub fn get(&self, key: &str) -> Option<&V> {
        match self {
            V::Dict(items) => items.iter().find(|(k, _)| matches!(k, V::Str(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        if let V::Str(s) = self { Some(s) } else { None }
    }
    pub fn int(&self) -> Option<i64> {
        if let V::Int(i) = self { Some(*i) } else { None }
    }
}

/// A dictionary with string keys.
pub fn dict(items: &[(&str, V)]) -> V {
    V::Dict(items.iter().map(|(k, v)| (V::Str(k.to_string()), v.clone())).collect())
}

pub fn encode(v: &V, out: &mut Vec<u8>) {
    match v {
        V::Null => out.push(0x04),
        V::Bool(b) => out.push(if *b { 0x01 } else { 0x02 }),
        V::Int(i) => match *i {
            -1 => out.push(0x07),
            0..=39 => out.push(0x08 + *i as u8),
            i if i8::try_from(i).is_ok() => out.extend([0x30, i as i8 as u8]),
            i if i16::try_from(i).is_ok() => {
                out.push(0x31);
                out.extend((i as i16).to_le_bytes())
            }
            i if i32::try_from(i).is_ok() => {
                out.push(0x32);
                out.extend((i as i32).to_le_bytes())
            }
            i => {
                out.push(0x33);
                out.extend(i.to_le_bytes())
            }
        },
        V::Float(f) => {
            out.push(0x36);
            out.extend(f.to_le_bytes())
        }
        V::Str(s) => {
            let n = s.len();
            if n <= 32 {
                out.push(0x40 + n as u8);
            } else if n <= 0xff {
                out.extend([0x61, n as u8]);
            } else {
                out.push(0x63);
                out.extend((n as u32).to_le_bytes());
            }
            out.extend(s.as_bytes());
        }
        V::Data(d) => {
            let n = d.len();
            if n <= 32 {
                out.push(0x70 + n as u8);
            } else if n <= 0xff {
                out.extend([0x91, n as u8]);
            } else if n <= 0xffff {
                out.push(0x92);
                out.extend((n as u16).to_le_bytes());
            } else {
                out.push(0x93);
                out.extend((n as u32).to_le_bytes());
            }
            out.extend(d);
        }
        V::Array(a) => {
            if a.len() < 15 {
                out.push(0xd0 + a.len() as u8);
                a.iter().for_each(|x| encode(x, out));
            } else {
                out.push(0xdf);
                a.iter().for_each(|x| encode(x, out));
                out.push(0x03);
            }
        }
        V::Dict(d) => {
            if d.len() < 15 {
                out.push(0xe0 + d.len() as u8);
                d.iter().for_each(|(k, v)| {
                    encode(k, out);
                    encode(v, out)
                });
            } else {
                out.push(0xef);
                d.iter().for_each(|(k, v)| {
                    encode(k, out);
                    encode(v, out)
                });
                out.push(0x03);
            }
        }
    }
}

struct Decoder<'a> {
    b: &'a [u8],
    pos: usize,
    /// values that later "compression" tags (0xa0…) refer back to
    tracked: Vec<V>,
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let s = self.b.get(self.pos..self.pos + n).ok_or("opack: truncated")?;
        self.pos += n;
        Ok(s)
    }
    fn le(&mut self, n: usize) -> Result<u64, String> {
        Ok(self.take(n)?.iter().rev().fold(0u64, |a, b| a << 8 | *b as u64))
    }
    /// `Ok(None)`: the terminator of an open-ended array or dictionary
    fn value(&mut self) -> Result<Option<V>, String> {
        let tag = self.take(1)?[0];
        let v = match tag {
            0x01 => V::Bool(true),
            0x02 => V::Bool(false),
            0x03 => return Ok(None),
            0x04 => V::Null,
            0x05 => V::Data(self.take(16)?.to_vec()),
            0x06 => V::Float(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x07 => V::Int(-1),
            0x08..=0x2f => V::Int((tag - 0x08) as i64),
            0x30 => V::Int(self.take(1)?[0] as i8 as i64),
            0x31 => V::Int(self.le(2)? as u16 as i16 as i64),
            0x32 => V::Int(self.le(4)? as u32 as i32 as i64),
            0x33 => V::Int(self.le(8)? as i64),
            0x35 => V::Float(f32::from_le_bytes(self.take(4)?.try_into().unwrap()) as f64),
            0x36 => V::Float(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x40..=0x60 => V::Str(String::from_utf8_lossy(self.take((tag - 0x40) as usize)?).into_owned()),
            0x61..=0x64 => {
                let n = self.le(1 << (tag - 0x61))? as usize;
                V::Str(String::from_utf8_lossy(self.take(n)?).into_owned())
            }
            0x6f => {
                let end = self.b[self.pos..].iter().position(|&c| c == 0).ok_or("opack: unterminated string")?;
                let s = String::from_utf8_lossy(self.take(end)?).into_owned();
                self.pos += 1;
                V::Str(s)
            }
            0x70..=0x90 => V::Data(self.take((tag - 0x70) as usize)?.to_vec()),
            0x91..=0x94 => {
                let n = self.le(1 << (tag - 0x91))? as usize;
                V::Data(self.take(n)?.to_vec())
            }
            0xa0..=0xcf => {
                let i = (tag - 0xa0) as usize;
                return self.tracked.get(i).cloned().map(Some).ok_or_else(|| format!("opack: bad back-reference {i}"));
            }
            0xd0..=0xdf => {
                let mut a = Vec::new();
                if tag == 0xdf {
                    while let Some(x) = self.value()? {
                        a.push(x);
                    }
                } else {
                    for _ in 0..tag - 0xd0 {
                        a.push(self.value()?.ok_or("opack: early terminator")?);
                    }
                }
                return Ok(Some(V::Array(a)));
            }
            0xe0..=0xef => {
                let mut d = Vec::new();
                if tag == 0xef {
                    while let Some(k) = self.value()? {
                        d.push((k, self.value()?.ok_or("opack: early terminator")?));
                    }
                } else {
                    for _ in 0..tag - 0xe0 {
                        let k = self.value()?.ok_or("opack: early terminator")?;
                        d.push((k, self.value()?.ok_or("opack: early terminator")?));
                    }
                }
                return Ok(Some(V::Dict(d)));
            }
            t => return Err(format!("opack: unknown tag 0x{t:02x}")),
        };
        // everything bigger than a one-byte value can be referenced again later
        if !matches!(v, V::Null | V::Bool(_)) && !matches!(tag, 0x07..=0x2f) {
            self.tracked.push(v.clone());
        }
        Ok(Some(v))
    }
}

pub fn decode(b: &[u8]) -> Result<V, String> {
    Decoder { b, pos: 0, tracked: Vec::new() }.value()?.ok_or_else(|| "opack: empty".into())
}

// -------------------------------------------------------------------------------- framing

pub struct Keys {
    /// we → controller
    pub read: [u8; 32],
    /// controller → us
    pub write: [u8; 32],
}

pub fn keys(secret: &[u8; 32], controller_salt: &[u8], accessory_salt: &[u8]) -> Keys {
    let salt = [controller_salt, accessory_salt].concat();
    let h = Hkdf::<Sha512>::new(Some(&salt), secret);
    let (mut read, mut write) = ([0u8; 32], [0u8; 32]);
    h.expand(b"HDS-Read-Encryption-Key", &mut read).unwrap();
    h.expand(b"HDS-Write-Encryption-Key", &mut write).unwrap();
    Keys { read, write }
}

fn nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// Seal one frame.
pub fn seal(key: &[u8; 32], counter: u64, plain: &[u8]) -> Vec<u8> {
    let n = plain.len() as u32;
    let header = [1, (n >> 16) as u8, (n >> 8) as u8, n as u8];
    let mut body = plain.to_vec();
    let tag = ChaCha20Poly1305::new(key.into()).encrypt_in_place_detached(Nonce::from_slice(&nonce(counter)), &header, &mut body).unwrap();
    [&header[..], &body, &tag].concat()
}

/// Open a frame body (`header` + ciphertext + tag).
pub fn open(key: &[u8; 32], counter: u64, header: &[u8; 4], body: &[u8]) -> Option<Vec<u8>> {
    let (ct, tag) = body.split_at(body.len().checked_sub(16)?);
    let mut buf = ct.to_vec();
    ChaCha20Poly1305::new(key.into())
        .decrypt_in_place_detached(Nonce::from_slice(&nonce(counter)), header, &mut buf, Tag::from_slice(tag))
        .ok()?;
    Some(buf)
}

/// header + message → payload
pub fn payload(header: &V, message: &V) -> Vec<u8> {
    let mut h = Vec::new();
    encode(header, &mut h);
    let mut p = vec![h.len() as u8];
    p.extend(h);
    encode(message, &mut p);
    p
}

pub fn parse_payload(p: &[u8]) -> Result<(V, V), String> {
    let n = *p.first().ok_or("hds: empty payload")? as usize;
    let header = decode(p.get(1..1 + n).ok_or("hds: short header")?)?;
    let message = decode(&p[1 + n..])?;
    Ok((header, message))
}

// -------------------------------------------------------------------------------- server

/// Hands a recording to the connection: init segment, then fragments until `None`.
pub trait RecordingSource: Send + Sync + 'static {
    /// Start a recording: the init segment and a receiver of fragments.
    fn start(&self) -> Result<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>), String>;
}

struct Prepared {
    keys: Keys,
    at: Instant,
}

pub struct Server {
    port: u16,
    prepared: Mutex<Vec<Prepared>>,
}

/// Largest chunk of one fragment per `data` event.
const CHUNK: usize = 0x40000;

impl Server {
    pub fn start(source: Arc<dyn RecordingSource>) -> Result<Arc<Server>, String> {
        let l = TcpListener::bind(("0.0.0.0", 0)).map_err(|e| format!("hds: {e}"))?;
        let port = l.local_addr().map_err(|e| e.to_string())?.port();
        let srv = Arc::new(Server { port, prepared: Mutex::new(Vec::new()) });
        let s2 = srv.clone();
        std::thread::spawn(move || {
            for stream in l.incoming().flatten() {
                let (srv, src) = (s2.clone(), source.clone());
                std::thread::spawn(move || {
                    if let Err(e) = srv.connection(stream, src) {
                        crate::log(format!("hds: {e}"));
                    }
                });
            }
        });
        Ok(srv)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// A controller set up a session: remember its keys for the next 10 s.
    pub fn prepare(&self, keys: Keys) {
        let mut p = self.prepared.lock().unwrap();
        p.retain(|x| x.at.elapsed() < Duration::from_secs(10));
        p.push(Prepared { keys, at: Instant::now() });
    }

    fn read_frame(s: &mut TcpStream) -> Result<([u8; 4], Vec<u8>), String> {
        let mut header = [0u8; 4];
        s.read_exact(&mut header).map_err(|e| e.to_string())?;
        let n = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if n > 1 << 20 {
            return Err("hds: frame too big".into());
        }
        let mut body = vec![0u8; n + 16];
        s.read_exact(&mut body).map_err(|e| e.to_string())?;
        Ok((header, body))
    }

    fn connection(&self, mut s: TcpStream, source: Arc<dyn RecordingSource>) -> Result<(), String> {
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let (header, body) = Self::read_frame(&mut s)?;
        // which prepared session is this? the one whose key opens the first frame
        let keys = {
            let mut p = self.prepared.lock().unwrap();
            let i = p.iter().position(|x| open(&x.keys.write, 0, &header, &body).is_some()).ok_or("hds: unknown connection")?;
            p.remove(i).keys
        };
        crate::log("hds: controller connected".into());
        let writer = Arc::new(Mutex::new((s.try_clone().map_err(|e| e.to_string())?, 0u64)));
        let send = {
            let w = writer.clone();
            let key = keys.read;
            move |h: V, m: V| -> Result<(), String> {
                let mut g = w.lock().unwrap();
                let frame = seal(&key, g.1, &payload(&h, &m));
                g.1 += 1;
                g.0.write_all(&frame).map_err(|e| e.to_string())
            }
        };
        let send = Arc::new(send);
        let mut counter = 0u64;
        let mut pending = Some((header, body));
        let streams: Arc<Mutex<BTreeMap<i64, Arc<std::sync::atomic::AtomicBool>>>> = Arc::default();
        loop {
            let (h, b) = match pending.take() {
                Some(f) => f,
                None => match Self::read_frame(&mut s) {
                    Ok(f) => f,
                    Err(_) => break,
                },
            };
            let plain = open(&keys.write, counter, &h, &b).ok_or("hds: decryption failed")?;
            counter += 1;
            let (hdr, msg) = parse_payload(&plain)?;
            let protocol = hdr.get("protocol").and_then(V::str).unwrap_or("").to_string();
            if let Some(topic) = hdr.get("request").and_then(V::str) {
                let id = hdr.get("id").and_then(V::int).unwrap_or(0);
                let reply = |status: i64, m: V| {
                    send(dict(&[("protocol", V::Str(protocol.clone())), ("response", V::Str(topic.into())), ("id", V::Int(id)), ("status", V::Int(status))]), m)
                };
                match (protocol.as_str(), topic) {
                    ("control", "hello") => reply(0, dict(&[]))?,
                    ("dataSend", "open") => {
                        let stream_id = msg.get("streamId").and_then(V::int).unwrap_or(0);
                        let kind = msg.get("type").and_then(V::str).unwrap_or("");
                        if kind != "ipcamera.recording" {
                            reply(0, dict(&[("status", V::Int(1))]))?; // unsupported
                            continue;
                        }
                        match source.start() {
                            Ok((init, fragments)) => {
                                reply(0, dict(&[("status", V::Int(0))]))?;
                                crate::log(format!("hds: recording stream {stream_id} open ({})", msg.get("reason").and_then(V::str).unwrap_or("")));
                                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                streams.lock().unwrap().insert(stream_id, stop.clone());
                                let send = send.clone();
                                std::thread::spawn(move || send_recording(stream_id, init, fragments, stop, &*send));
                            }
                            Err(e) => {
                                crate::log(format!("hds: recording: {e}"));
                                reply(0, dict(&[("status", V::Int(4))]))?; // unexpected failure
                            }
                        }
                    }
                    _ => reply(1, dict(&[]))?, // unsupported
                }
            } else if let Some(topic) = hdr.get("event").and_then(V::str) {
                if protocol == "dataSend" && (topic == "close" || topic == "ack") {
                    let stream_id = msg.get("streamId").and_then(V::int).unwrap_or(0);
                    if let Some(stop) = streams.lock().unwrap().remove(&stream_id) {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        crate::log(format!("hds: recording stream {stream_id} {topic}"));
                    }
                }
            }
        }
        for stop in streams.lock().unwrap().values() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        crate::log("hds: controller disconnected".into());
        Ok(())
    }
}

/// Send the init segment and then each fragment, chunked, until stopped or the source ends.
fn send_recording(
    stream_id: i64,
    init: Vec<u8>,
    fragments: std::sync::mpsc::Receiver<Vec<u8>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    send: &dyn Fn(V, V) -> Result<(), String>,
) {
    let event = || dict(&[("protocol", V::Str("dataSend".into())), ("event", V::Str("data".into()))]);
    let mut seq = 1i64;
    let mut send_object = |data: &[u8], kind: &str, seq: i64| -> Result<(), String> {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        for (i, c) in chunks.iter().enumerate() {
            let mut meta = vec![
                ("dataType", V::Str(kind.into())),
                ("dataSequenceNumber", V::Int(seq)),
                ("dataChunkSequenceNumber", V::Int(i as i64 + 1)),
                ("isLastDataChunk", V::Bool(i + 1 == chunks.len())),
            ];
            if i == 0 && chunks.len() > 1 {
                meta.push(("dataTotalSize", V::Int(data.len() as i64)));
            }
            let packet = dict(&[("data", V::Data(c.to_vec())), ("metadata", dict(&meta))]);
            send(event(), dict(&[("streamId", V::Int(stream_id)), ("packets", V::Array(vec![packet]))]))?;
        }
        Ok(())
    };
    let mut sent = 0;
    let result = (|| -> Result<(), String> {
        send_object(&init, "mediaInitialization", seq)?;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match fragments.recv_timeout(Duration::from_secs(1)) {
                Ok(f) => {
                    seq += 1;
                    send_object(&f, "mediaFragment", seq)?;
                    sent += 1;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        Ok(())
    })();
    crate::log(format!("hds: recording stream {stream_id}: {sent} fragments sent{}", result.err().map(|e| format!(" ({e})")).unwrap_or_default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opack_roundtrip() {
        let v = dict(&[
            ("protocol", V::Str("dataSend".into())),
            ("id", V::Int(1234)),
            ("small", V::Int(7)),
            ("neg", V::Int(-1)),
            ("big", V::Int(1 << 40)),
            ("ok", V::Bool(true)),
            ("data", V::Data(vec![1; 300])),
            ("list", V::Array(vec![V::Int(1), V::Str("x".repeat(40))])),
        ]);
        let mut b = Vec::new();
        encode(&v, &mut b);
        assert_eq!(decode(&b).unwrap(), v);
        assert_eq!(&b[..1], &[0xe8]); // dictionary of 8
    }

    #[test]
    fn opack_back_references() {
        // [ "streamId", "streamId" as a back-reference (0xa0 = the first tracked value) ]
        let b = [0xd2, 0x48, b's', b't', b'r', b'e', b'a', b'm', b'I', b'd', 0xa0];
        assert_eq!(decode(&b).unwrap(), V::Array(vec![V::Str("streamId".into()), V::Str("streamId".into())]));
    }

    #[test]
    fn frames_roundtrip() {
        let k = keys(&[7; 32], &[1; 32], &[2; 32]);
        assert_ne!(k.read, k.write);
        let p = payload(&dict(&[("protocol", V::Str("control".into())), ("request", V::Str("hello".into())), ("id", V::Int(1))]), &dict(&[]));
        let f = seal(&k.write, 0, &p);
        let header: [u8; 4] = f[..4].try_into().unwrap();
        assert_eq!(header[0], 1);
        let plain = open(&k.write, 0, &header, &f[4..]).unwrap();
        let (h, m) = parse_payload(&plain).unwrap();
        assert_eq!(h.get("request").and_then(V::str), Some("hello"));
        assert_eq!(m, dict(&[]));
        assert!(open(&k.write, 1, &header, &f[4..]).is_none());
    }
}
