//! Minimal RTSP client: DESCRIBE, SETUP (TCP interleaved), PLAY, then RTP packets.
//! Plain `rtsp://` on the LAN. Credentials in the URL (`rtsp://user:pass@host/…`) are used for
//! Basic or Digest auth (most cameras); Protect's URLs carry their own alias instead.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub struct Track {
    pub control: String,
    /// lower-case codec from a=rtpmap ("h264", "opus", "mpeg4-generic")
    pub codec: String,
    pub payload_type: u8,
    pub clock: u32,
    /// H.264: SPS and PPS from `sprop-parameter-sets`
    pub params: Vec<Vec<u8>>,
    /// channels from a=rtpmap (audio)
    pub channels: u8,
    /// the a=fmtp parameters, lower-case keys ("config", "sizelength", …)
    pub fmtp: Vec<(String, String)>,
}

pub struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
    url: String,
    cseq: u32,
    session: Option<String>,
    last_keepalive: Instant,
    /// (user, password) from the URL
    creds: Option<(String, String)>,
    /// the server's WWW-Authenticate challenge, once seen
    challenge: Option<String>,
}

/// Split `rtsp://user:pass@host:port/path` into the URL without credentials and the credentials.
fn split_creds(url: &str) -> (String, Option<(String, String)>) {
    let Some(rest) = url.strip_prefix("rtsp://") else { return (url.to_string(), None) };
    let (auth, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    match auth.rsplit_once('@') {
        Some((userinfo, host)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            let dec = |s: &str| percent_decode(s);
            (format!("rtsp://{host}{tail}"), Some((dec(u), dec(p))))
        }
        None => (url.to_string(), None),
    }
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn md5_hex(s: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The Authorization header for a challenge (Digest, RFC 2617 without qop, as cameras use;
/// or Basic).
pub(crate) fn authorization(challenge: &str, user: &str, pass: &str, method: &str, uri: &str) -> String {
    let param = |name: &str| {
        let key = format!("{name}=\"");
        challenge.find(&key).map(|i| &challenge[i + key.len()..]).and_then(|r| r.split('"').next()).unwrap_or("").to_string()
    };
    if challenge.trim_start().to_ascii_lowercase().starts_with("digest") {
        let (realm, nonce) = (param("realm"), param("nonce"));
        let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        format!("Authorization: Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"")
    } else {
        format!("Authorization: Basic {}", crate::tlv::b64(format!("{user}:{pass}").as_bytes()))
    }
}

/// One interleaved packet: channel (even = RTP, odd = RTCP) and bytes.
pub type Packet = (u8, Vec<u8>);

impl Client {
    pub fn connect(url: &str) -> Result<(Client, Vec<Track>), String> {
        let (url, creds) = split_creds(url);
        let url = url.as_str();
        let hostport = url.strip_prefix("rtsp://").ok_or("not an rtsp:// URL")?.split('/').next().unwrap_or("");
        let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:554") };
        let s = TcpStream::connect(&addr).map_err(|e| format!("rtsp connect: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        s.set_nodelay(true).ok();
        let r = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        let mut c = Client { w: s, r, url: url.to_string(), cseq: 0, session: None, last_keepalive: Instant::now(), creds, challenge: None };
        let (headers, body) = c.request("DESCRIBE", url, &["Accept: application/sdp"])?;
        let base = header(&headers, "content-base").unwrap_or(url).trim_end_matches('/').to_string();
        let tracks = parse_sdp(&String::from_utf8_lossy(&body), &base);
        Ok((c, tracks))
    }

    /// SETUP a track on interleaved channels (`ch`, `ch+1`).
    pub fn setup(&mut self, track: &Track, ch: u8) -> Result<(), String> {
        let transport = format!("Transport: RTP/AVP/TCP;unicast;interleaved={}-{}", ch, ch + 1);
        let sess = self.session.clone().map(|s| format!("Session: {s}"));
        let mut extra = vec![transport.as_str()];
        if let Some(s) = &sess {
            extra.push(s);
        }
        let (headers, _) = self.request("SETUP", &track.control.clone(), &extra)?;
        if self.session.is_none() {
            self.session = header(&headers, "session").map(|s| s.split(';').next().unwrap_or("").trim().to_string());
        }
        Ok(())
    }

    pub fn play(&mut self) -> Result<(), String> {
        let s = format!("Session: {}", self.session.clone().unwrap_or_default());
        let url = self.url.clone();
        self.request("PLAY", &url, &[&s, "Range: npt=0.000-"])?;
        Ok(())
    }

    fn send(&mut self, method: &str, url: &str, extra: &[&str]) -> Result<(), String> {
        self.cseq += 1;
        let mut msg = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: rust-homekit-cam\r\n", self.cseq);
        if let (Some(ch), Some((u, p))) = (&self.challenge, &self.creds) {
            msg.push_str(&authorization(ch, u, p, method, url));
            msg.push_str("\r\n");
        }
        for h in extra {
            msg.push_str(h);
            msg.push_str("\r\n");
        }
        msg.push_str("\r\n");
        self.w.write_all(msg.as_bytes()).map_err(|e| format!("rtsp write: {e}"))
    }

    fn request(&mut self, method: &str, url: &str, extra: &[&str]) -> Result<(Vec<(String, String)>, Vec<u8>), String> {
        let mut retried = false;
        self.send(method, url, extra)?;
        loop {
            match self.read_message()? {
                Message::Response(200, h, b) => return Ok((h, b)),
                Message::Response(401, h, _) if !retried && self.creds.is_some() => {
                    // prefer Digest when several challenges are offered
                    let chs: Vec<&str> = h.iter().filter(|(k, _)| k == "www-authenticate").map(|(_, v)| v.as_str()).collect();
                    let ch = chs.iter().find(|c| c.to_ascii_lowercase().starts_with("digest")).or(chs.first()).ok_or("rtsp: 401 without a challenge")?;
                    self.challenge = Some(ch.to_string());
                    retried = true;
                    self.send(method, url, extra)?;
                }
                Message::Response(401, _, _) => return Err(format!("rtsp {method}: 401 (wrong or missing user:password in the URL)")),
                Message::Response(code, _, _) => return Err(format!("rtsp {method}: status {code}")),
                Message::Data(_) => {}
            }
        }
    }

    fn read_message(&mut self) -> Result<Message, String> {
        let io = |e: std::io::Error| format!("rtsp read: {e}");
        let mut first = [0u8; 1];
        self.r.read_exact(&mut first).map_err(io)?;
        if first[0] == b'$' {
            let mut h = [0u8; 3];
            self.r.read_exact(&mut h).map_err(io)?;
            let mut data = vec![0u8; u16::from_be_bytes([h[1], h[2]]) as usize];
            self.r.read_exact(&mut data).map_err(io)?;
            return Ok(Message::Data((h[0], data)));
        }
        let mut line = String::from(first[0] as char);
        self.r.read_line(&mut line).map_err(io)?;
        let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            let mut l = String::new();
            if self.r.read_line(&mut l).map_err(io)? == 0 || l.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let len: usize = header(&headers, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut body = vec![0u8; len];
        self.r.read_exact(&mut body).map_err(io)?;
        Ok(Message::Response(code, headers, body))
    }

    /// The next interleaved packet; sends a keep-alive every 20 s.
    pub fn next(&mut self) -> Result<Packet, String> {
        if self.last_keepalive.elapsed() > Duration::from_secs(20) {
            self.last_keepalive = Instant::now();
            let s = format!("Session: {}", self.session.clone().unwrap_or_default());
            let url = self.url.clone();
            self.send("GET_PARAMETER", &url, &[&s])?;
        }
        loop {
            if let Message::Data(p) = self.read_message()? {
                return Ok(p);
            }
        }
    }

    /// For stopping from another thread.
    pub fn stream(&self) -> Option<TcpStream> {
        self.w.try_clone().ok()
    }
}

enum Message {
    Response(u16, Vec<(String, String)>, Vec<u8>),
    Data(Packet),
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn parse_sdp(sdp: &str, base: &str) -> Vec<Track> {
    let mut tracks: Vec<Track> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            let pt = m.split_whitespace().nth(3).and_then(|p| p.parse().ok()).unwrap_or(0);
            tracks.push(Track { control: base.to_string(), codec: String::new(), payload_type: pt, clock: 90000, params: vec![], channels: 1, fmtp: vec![] });
        }
        let Some(t) = tracks.last_mut() else { continue };
        if let Some(c) = line.strip_prefix("a=control:") {
            t.control = if c.starts_with("rtsp://") { c.to_string() } else { format!("{base}/{c}") };
        } else if let Some(r) = line.strip_prefix("a=rtpmap:") {
            if let Some((_, enc)) = r.split_once(' ') {
                let mut p = enc.split('/');
                t.codec = p.next().unwrap_or("").to_ascii_lowercase();
                t.clock = p.next().and_then(|c| c.parse().ok()).unwrap_or(90000);
                t.channels = p.next().and_then(|c| c.parse().ok()).unwrap_or(1);
            }
        } else if let Some(f) = line.strip_prefix("a=fmtp:") {
            let f = f.split_once(' ').map(|(_, rest)| rest).unwrap_or(f);
            for kv in f.split(';') {
                if let Some((k, v)) = kv.trim().split_once('=') {
                    t.fmtp.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
                if let Some(v) = kv.trim().strip_prefix("sprop-parameter-sets=") {
                    t.params = v.split(',').filter_map(crate::tlv::unb64).filter(|p| !p.is_empty()).collect();
                }
            }
        }
    }
    tracks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_and_digest() {
        let (u, c) = split_creds("rtsp://admin:p%40ss@192.0.2.5:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(u, "rtsp://192.0.2.5:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(c, Some(("admin".into(), "p@ss".into())));
        assert_eq!(split_creds("rtsp://h:7447/abc").1, None);
        // RFC 2617 section 3.5 example (no qop)
        let ch = r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#;
        let a = authorization(ch, "Mufasa", "Circle Of Life", "GET", "/dir/index.html");
        assert!(a.contains(r#"response="670fd8c2df070c60b045671b8b24ff02""#), "{a}");
        assert_eq!(authorization("Basic realm=\"x\"", "a", "b", "DESCRIBE", "/"), "Authorization: Basic YTpi");
    }

    #[test]
    fn sdp_tracks() {
        let sdp = "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 opus/48000/2\r\na=control:trackID=1\r\n\
                   m=video 0 RTP/AVP 97\r\na=rtpmap:97 H264/90000\r\n\
                   a=fmtp:97 packetization-mode=1;sprop-parameter-sets=Z0IAH5WoFAFuQA==,aM48gA==\r\na=control:trackID=2\r\n";
        let t = parse_sdp(sdp, "rtsp://h:7447/abc");
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].codec.as_str(), t[0].clock), ("opus", 48000));
        assert_eq!((t[1].codec.as_str(), t[1].payload_type), ("h264", 97));
        assert_eq!(t[1].control, "rtsp://h:7447/abc/trackID=2");
        assert_eq!(t[1].params.len(), 2);
        assert_eq!(t[1].params[0][0] & 0x1f, 7);
    }
}
