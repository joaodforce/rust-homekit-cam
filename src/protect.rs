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
}
