//! A tiny HTTP/1.1 client for one-shot requests (`Connection: close`): response parsing shared
//! with the Protect client, and plain-HTTP GET with Basic/Digest auth for camera snapshot URLs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Parse a complete response (read until close).
pub fn parse(raw: &[u8]) -> Result<Resp, String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("no HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let mut body = raw[end + 4..].to_vec();
    if headers.iter().any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body);
    } else if let Some(n) = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()) {
        body.truncate(n);
    }
    Ok(Resp { status, headers, body })
}

pub fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let n = std::str::from_utf8(&b[..i]).ok().and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok()).unwrap_or(0);
        let rest = &b[i + 2..];
        if n == 0 || rest.len() < n {
            break;
        }
        out.extend_from_slice(&rest[..n]);
        b = rest[n..].strip_prefix(b"\r\n").unwrap_or(&rest[n..]);
    }
    out
}

/// GET a plain `http://[user:pass@]host[:port]/path` URL; retries once with Basic/Digest on 401.
pub fn get(url: &str) -> Result<Resp, String> {
    request("GET", url, None, &[], Duration::from_secs(10))
}

/// One request to a plain `http://[user:pass@]host[:port]/path` URL, with an optional body
/// (content type, bytes); on 401 retried once with Basic or Digest (qop=auth too).
pub fn request(method: &str, url: &str, content_type: Option<&str>, body: &[u8], timeout: Duration) -> Result<Resp, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// URLs are supported")?;
    let (auth_host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (creds, host) = match auth_host.rsplit_once('@') {
        Some((ui, h)) => {
            let (u, p) = ui.split_once(':').unwrap_or((ui, ""));
            (Some((crate::rtsp::percent_decode(u), crate::rtsp::percent_decode(p))), h)
        }
        None => (None, auth_host),
    };
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let once = |auth: Option<String>| -> Result<Resp, String> {
        let mut s = TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
        s.set_read_timeout(Some(timeout)).ok();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: rust-homekit-cam\r\nConnection: close\r\n");
        if let Some(ct) = content_type {
            req.push_str(&format!("Content-Type: {ct}\r\nContent-Length: {}\r\n", body.len()));
        }
        if let Some(a) = auth {
            req.push_str(&a);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        let mut bytes = req.into_bytes();
        bytes.extend_from_slice(body);
        s.write_all(&bytes).map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        parse(&raw)
    };
    let r = once(None)?;
    match (&r.status, &creds) {
        (401, Some((u, p))) => {
            let chs: Vec<&str> = r.headers.iter().filter(|(k, _)| k == "www-authenticate").map(|(_, v)| v.as_str()).collect();
            let ch = chs.iter().find(|c| c.to_ascii_lowercase().starts_with("digest")).or(chs.first()).ok_or("401 without a challenge")?;
            once(Some(crate::rtsp::authorization(ch, u, p, method, path)))
        }
        _ => Ok(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_and_chunked() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 3\r\n\r\n\xff\xd8\xffjunk").unwrap();
        assert_eq!((r.status, r.body.as_slice(), r.header("content-type")), (200, &b"\xff\xd8\xff"[..], Some("image/jpeg")));
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n1\r\nc\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"abc");
    }
}
