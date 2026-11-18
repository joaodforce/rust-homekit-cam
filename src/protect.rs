//! The bits of UniFi Protect's integration API we use (`/proxy/protect/integration/v1`), with an
//! API key (`X-API-KEY`). The console's certificate is self-signed, so it is pinned by its
//! SHA-256 fingerprint instead of checked against a CA.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Protect {
    host: String,
    key: String,
    camera: String,
    tls: Arc<rustls::ClientConfig>,
    /// the fingerprint seen when none is pinned (logged once, to put in the config)
    seen: Arc<Mutex<Option<String>>>,
}

/// A talk-back session: where the camera wants the controller's audio.
#[derive(Debug, PartialEq)]
pub struct Talkback {
    pub addr: std::net::SocketAddr,
    pub codec: String,
    pub rate: u32,
}

#[derive(Debug)]
struct Pinned {
    sha256: Option<Vec<u8>>,
    seen: Arc<Mutex<Option<String>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":")
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(cert.as_ref()).to_vec();
        match &self.sha256 {
            Some(want) if *want == got => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(format!("certificate fingerprint changed: {}", hex(&got)))),
            None => {
                *self.seen.lock().unwrap() = Some(hex(&got));
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl Protect {
    /// `host`: the console's address; `cert_sha256`: "AB:CD:…" (None: accept and log it).
    pub fn new(host: &str, key: &str, camera: &str, cert_sha256: Option<&str>) -> Result<Protect, String> {
        let sha256 = match cert_sha256 {
            Some(s) => {
                let b: Vec<u8> = s.split(':').map(|h| u8::from_str_radix(h, 16)).collect::<Result<_, _>>().map_err(|_| "bad protect_cert_sha256")?;
                (b.len() == 32).then_some(b).ok_or("protect_cert_sha256: need 32 bytes")?.into()
            }
            None => None,
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let seen = Arc::new(Mutex::new(None));
        let verifier = Arc::new(Pinned { sha256, seen: seen.clone(), provider: provider.clone() });
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(Protect { host: host.to_string(), key: key.to_string(), camera: camera.to_string(), tls: Arc::new(tls), seen })
    }

    fn request(&self, method: &str, path: &str, body: &str, accept: &str) -> Result<Vec<u8>, String> {
        let addr = if self.host.contains(':') { self.host.clone() } else { format!("{}:443", self.host) };
        let tcp = TcpStream::connect(&addr).map_err(|e| format!("protect {addr}: {e}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let name = ServerName::try_from(self.host.split(':').next().unwrap_or("").to_string()).map_err(|e| e.to_string())?;
        let conn = rustls::ClientConnection::new(self.tls.clone(), name).map_err(|e| e.to_string())?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        let req = format!(
            "{method} /proxy/protect/integration/v1{path} HTTP/1.1\r\nHost: {}\r\nX-API-KEY: {}\r\nAccept: {accept}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.host,
            self.key,
            body.len()
        );
        s.write_all(req.as_bytes()).map_err(|e| format!("protect: {e}"))?;
        let mut raw = Vec::new();
        // the console closes without close_notify sometimes: keep what arrived
        let _ = s.read_to_end(&mut raw);
        if let Some(fp) = self.seen.lock().unwrap().take() {
            crate::log(format!("protect: certificate not pinned; to pin it set [protect] cert_sha256 = \"{fp}\""));
        }
        let r = crate::httpc::parse(&raw).map_err(|e| format!("protect: {e}"))?;
        if r.status != 200 {
            return Err(format!("protect {method} {path}: HTTP {}", r.status));
        }
        Ok(r.body)
    }

    fn request_json(&self, method: &str, path: &str, body: &str) -> Result<Value, String> {
        serde_json::from_slice(&self.request(method, path, body, "application/json")?).map_err(|e| format!("protect: {e}"))
    }

    /// All cameras: (id, name, has a speaker).
    pub fn cameras(&self) -> Result<Vec<(String, String, bool)>, String> {
        let v = self.request_json("GET", "/cameras", "")?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .map(|c| {
                        let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
                        (s("id"), s("name"), c["featureFlags"]["hasSpeaker"].as_bool().unwrap_or(false))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// A camera's RTSP streams as plain `rtsp://<console>:7447/<alias>` URLs, per channel
    /// ("high", "medium", "low"); channels without RTSP enabled are left out.
    pub fn rtsp_urls(&self, camera: &str) -> Result<Vec<(String, String)>, String> {
        let v = self.request_json("GET", &format!("/cameras/{camera}/rtsps-stream"), "")?;
        let mut out = Vec::new();
        for ch in ["high", "medium", "low"] {
            if let Some(u) = v[ch].as_str() {
                let alias = u.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or("");
                let host = self.host.split(':').next().unwrap_or("");
                out.push((ch.to_string(), format!("rtsp://{host}:7447/{alias}")));
            }
        }
        Ok(out)
    }

    /// The camera's current snapshot (JPEG).
    pub fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.request("GET", &format!("/cameras/{}/snapshot", self.camera), "", "image/jpeg")
    }

    /// Open a talk-back session on the camera.
    pub fn talkback(&self) -> Result<Talkback, String> {
        let v = self.request_json("POST", &format!("/cameras/{}/talkback-session", self.camera), "{}")?;
        parse_talkback(&v)
    }
}

// ----------------------------------------------------------------------------- event stream

/// Which Protect events count as motion: `Smart` = person / vehicle / animal / package
/// detections (no shadows and leaves), `Any` = every motion event too.
#[derive(Clone, Copy, PartialEq, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionKind {
    Smart,
    Any,
}

/// What one event-stream message means for our camera.
#[derive(Debug, PartialEq)]
pub enum Event {
    Ring,
    /// motion event `id` started or is still going
    Motion(String),
    /// motion event `id` ended
    MotionEnd(String),
}

pub fn parse_event(msg: &str, camera: &str, kind: MotionKind) -> Option<Event> {
    let v: Value = serde_json::from_str(msg).ok()?;
    let item = &v["item"];
    if item["device"] != camera || item["modelKey"] != "event" {
        return None;
    }
    let id = item["id"].as_str()?.to_string();
    let ended = item.get("end").is_some_and(|e| !e.is_null());
    let motion = match item["type"].as_str()? {
        "ring" if v["type"] == "add" => return Some(Event::Ring),
        "motion" => kind == MotionKind::Any,
        "smartDetectZone" | "smartDetectLine" => true,
        _ => false,
    };
    motion.then(|| if ended { Event::MotionEnd(id) } else { Event::Motion(id) })
}

/// Minimal WebSocket client (RFC 6455) over the pinned TLS connection: text frames in, pings
/// answered; our own ping every 30 s so a dead connection is noticed.
struct Ws {
    s: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

impl Ws {
    fn send(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        use rand_core::RngCore;
        let mut f = vec![0x80 | opcode];
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        rand_core::OsRng.fill_bytes(&mut mask);
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&f)
    }

    /// The next text message (empty for a pong: the connection is alive); `Ok(None)` on a read
    /// timeout.
    fn next(&mut self) -> Result<Option<String>, String> {
        let mut msg = Vec::new();
        loop {
            let mut h = [0u8; 2];
            match self.s.read_exact(&mut h) {
                Ok(()) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) && msg.is_empty() => {
                    return Ok(None);
                }
                Err(e) => return Err(e.to_string()),
            }
            let (fin, opcode) = (h[0] & 0x80 != 0, h[0] & 0x0f);
            let mut n = (h[1] & 0x7f) as u64;
            if n == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                n = u16::from_be_bytes(b) as u64;
            } else if n == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                n = u64::from_be_bytes(b);
            }
            if n > 1 << 20 {
                return Err("websocket frame too big".into());
            }
            let mut data = vec![0u8; n as usize];
            self.s.read_exact(&mut data).map_err(|e| e.to_string())?;
            match opcode {
                0x8 => return Err("websocket closed by the console".into()),
                0x9 => self.send(0xA, &data).map_err(|e| e.to_string())?,
                0xA => return Ok(Some(String::new())),
                0x0..=0x2 => {
                    msg.extend_from_slice(&data);
                    if fin {
                        return Ok(Some(String::from_utf8_lossy(&msg).into_owned()));
                    }
                }
                _ => {}
            }
        }
    }
}

impl Protect {
    fn websocket(&self, path: &str) -> Result<Ws, String> {
        use rand_core::RngCore;
        let addr = if self.host.contains(':') { self.host.clone() } else { format!("{}:443", self.host) };
        let tcp = TcpStream::connect(&addr).map_err(|e| format!("protect {addr}: {e}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let name = ServerName::try_from(self.host.split(':').next().unwrap_or("").to_string()).map_err(|e| e.to_string())?;
        let conn = rustls::ClientConnection::new(self.tls.clone(), name).map_err(|e| e.to_string())?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        let mut key = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut key);
        let req = format!(
            "GET /proxy/protect/integration/v1{path} HTTP/1.1\r\nHost: {}\r\nX-API-KEY: {}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            self.host,
            self.key,
            crate::tlv::b64(&key)
        );
        s.write_all(req.as_bytes()).map_err(|e| format!("protect ws: {e}"))?;
        // the 101 response, up to the blank line (byte by byte: frames may follow at once)
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).map_err(|e| format!("protect ws: {e}"))?;
            head.push(b[0]);
            if head.len() > 8192 {
                return Err("protect ws: response too long".into());
            }
        }
        let status = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("").to_string();
        if status != "101" {
            return Err(format!("protect ws {path}: HTTP {status}"));
        }
        Ok(Ws { s })
    }

    /// Follow the camera's events for ever (reconnecting), calling `on` for each.
    pub fn events(self: Arc<Self>, kind: MotionKind, on: impl Fn(Event) + Send + 'static) {
        std::thread::spawn(move || {
            let mut backoff = 1;
            loop {
                match self.websocket("/subscribe/events") {
                    Ok(mut ws) => {
                        crate::log("protect: following events".into());
                        backoff = 1;
                        let mut idle = 0;
                        loop {
                            match ws.next() {
                                Ok(Some(m)) => {
                                    idle = 0;
                                    if m.is_empty() {
                                        continue;
                                    }
                                    if let Some(e) = parse_event(&m, &self.camera, kind) {
                                        on(e);
                                    }
                                }
                                Ok(None) => {
                                    idle += 1;
                                    // a quiet stream is normal: ping, and give up only when
                                    // two pings in a row go unanswered
                                    if idle > 2 || ws.send(0x9, b"").is_err() {
                                        crate::log("protect: event stream silent, reconnecting".into());
                                        break;
                                    }
                                }
                                Err(e) => {
                                    crate::log(format!("protect: event stream: {e}"));
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => crate::log(format!("protect: event stream: {e}")),
                }
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        });
    }
}

fn parse_talkback(v: &Value) -> Result<Talkback, String> {
    let url = v["url"].as_str().ok_or("talkback: no url")?;
    let addr = url.strip_prefix("rtp://").unwrap_or(url).parse().map_err(|_| format!("talkback: url {url}"))?;
    Ok(Talkback { addr, codec: v["codec"].as_str().unwrap_or("").to_string(), rate: v["samplingRate"].as_u64().unwrap_or(0) as u32 })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talkback_answer() {
        let v = serde_json::json!({"url": "rtp://192.0.2.10:7004", "codec": "opus", "samplingRate": 24000, "bitsPerSample": 16});
        let t = parse_talkback(&v).unwrap();
        assert_eq!((t.addr.to_string().as_str(), t.codec.as_str(), t.rate), ("192.0.2.10:7004", "opus", 24000));
        assert!(Protect::new("h", "k", "c", Some("00:11")).is_err());
    }

    #[test]
    fn events_from_the_stream() {
        let cam = "6944095800883303e4621af7";
        let p = |m: &str, k| parse_event(m, cam, k);
        let smart = r#"{"type":"add","item":{"id":"e1","modelKey":"event","type":"smartDetectZone","start":1,"device":"6944095800883303e4621af7","smartDetectTypes":["vehicle"]}}"#;
        assert_eq!(p(smart, MotionKind::Smart), Some(Event::Motion("e1".into())));
        let ended = r#"{"type":"update","item":{"id":"e1","type":"smartDetectZone","start":1,"end":2,"device":"6944095800883303e4621af7","smartDetectTypes":["person"],"modelKey":"event"}}"#;
        assert_eq!(p(ended, MotionKind::Smart), Some(Event::MotionEnd("e1".into())));
        let motion = r#"{"type":"add","item":{"id":"m","modelKey":"event","type":"motion","start":1,"device":"6944095800883303e4621af7"}}"#;
        assert_eq!(p(motion, MotionKind::Smart), None);
        assert_eq!(p(motion, MotionKind::Any), Some(Event::Motion("m".into())));
        let audio = r#"{"item":{"id":"a","modelKey":"event","type":"smartAudioDetect","start":1,"device":"6944095800883303e4621af7","smartDetectTypes":["alrmSpeak"]},"type":"update"}"#;
        assert_eq!(p(audio, MotionKind::Any), None);
        let other = smart.replace("6944095800883303e4621af7", "68da94080219e103e40cda72");
        assert_eq!(p(&other, MotionKind::Any), None);
        let ring = r#"{"type":"add","item":{"id":"r","modelKey":"event","type":"ring","start":1,"device":"6944095800883303e4621af7"}}"#;
        assert_eq!(p(ring, MotionKind::Smart), Some(Event::Ring));
    }
}
