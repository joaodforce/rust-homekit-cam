//! Minimal HTTP/1.1 (and RTSP/1.0) request reader and response writer (from crtd).

use std::io::{self, BufRead, Read, Write};

const MAX_HEADERS: usize = 32;
const MAX_LINE: usize = 4096;
const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub protocol: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_line(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if !buf.ends_with(b"\n") {
        return Err(bad("header line too long"));
    }
    let s = String::from_utf8(buf).map_err(|_| bad("non-UTF-8 header"))?;
    Ok(Some(s.trim_end_matches(['\r', '\n']).to_string()))
}

/// Read one request. `Ok(None)` means the peer closed the connection cleanly.
pub fn read_request(r: &mut impl BufRead) -> io::Result<Option<Request>> {
    let line = loop {
        match read_line(r)? {
            None => return Ok(None),
            Some(l) if l.is_empty() => continue, // tolerate stray CRLFs between requests
            Some(l) => break l,
        }
    };
    let mut parts = line.splitn(3, ' ');
    let (Some(method), Some(path), Some(protocol)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad("malformed request line"));
    };
    let mut headers = Vec::new();
    loop {
        let l = read_line(r)?.ok_or_else(|| bad("eof in headers"))?;
        if l.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(bad("too many headers"));
        }
        let (k, v) = l.split_once(':').ok_or_else(|| bad("malformed header"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let mut req = Request {
        method: method.to_string(),
        path: path.to_string(),
        protocol: protocol.to_string(),
        headers,
        body: Vec::new(),
    };
    if let Some(len) = req.header("Content-Length") {
        let len: usize = len.parse().map_err(|_| bad("bad Content-Length"))?;
        if len > MAX_BODY {
            return Err(bad("body too large"));
        }
        req.body = vec![0; len];
        r.read_exact(&mut req.body)?;
    }
    Ok(Some(req))
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Close the connection after sending.
    pub close: bool,
}

impl Response {
    pub fn ok() -> Self {
        Self::status(200, "OK")
    }

    pub fn status(status: u16, reason: &'static str) -> Self {
        Self { status, reason, headers: Vec::new(), body: Vec::new(), close: false }
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.body = body;
        self.header("Content-Type", content_type)
    }

    pub fn write(&self, w: &mut impl Write, protocol: &str) -> io::Result<()> {
        let mut out = format!("{protocol} {} {}\r\n", self.status, self.reason);
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        w.write_all(&bytes)?;
        w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn parses_two_requests() {
        let raw = b"POST /fp-setup RTSP/1.0\r\nCSeq: 3\r\ncontent-length: 4\r\n\r\nabcdGET /info RTSP/1.0\r\nCSeq: 4\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let a = read_request(&mut r).unwrap().unwrap();
        assert_eq!((a.method.as_str(), a.path.as_str(), a.body.as_slice()), ("POST", "/fp-setup", &b"abcd"[..]));
        assert_eq!(a.header("cseq"), Some("3"));
        let b = read_request(&mut r).unwrap().unwrap();
        assert_eq!(b.path, "/info");
        assert!(read_request(&mut r).unwrap().is_none());
    }
}
