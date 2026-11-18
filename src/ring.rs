//! Where rings come from. Every source ends in `Ringer::ring()`; today that's an HTTP POST
//! (from Home Assistant, or a Protect alarm-manager webhook), later possibly Protect's own
//! event websocket.

use crate::hap::Server;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Ringer {
    pub server: Server,
    pub iid: u64,
    /// dropped on a ring so the notification's picture is fresh
    pub snapshots: Option<std::sync::Arc<crate::snapshot::Snapshots>>,
}

impl Ringer {
    pub fn ring(&self) {
        crate::log("ring".into());
        if let Some(s) = &self.snapshots {
            s.invalidate();
        }
        self.server.notify(self.iid, serde_json::json!(0)); // single press
    }
}

/// `POST /ring` with `Authorization: Bearer <token>` (or `?token=`). Ignores repeats within 3 s.
pub fn http(listen: &str, token: String, ringer: Ringer) -> Result<(), String> {
    let l = TcpListener::bind(listen).map_err(|e| format!("ring listener {listen}: {e}"))?;
    crate::log(format!("ring: POST http://{listen}/ring"));
    std::thread::spawn(move || {
        let mut last: Option<Instant> = None;
        for mut s in l.incoming().flatten() {
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut r = BufReader::new(match s.try_clone() {
                Ok(c) => c,
                Err(_) => continue,
            });
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let (mut auth, mut len) = (String::new(), 0usize);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "authorization" => auth = v.trim().trim_start_matches("Bearer ").to_string(),
                        "content-length" => len = v.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            let _ = r.by_ref().take(len.min(4096) as u64).read_to_end(&mut Vec::new());
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let (path, query) = path.split_once('?').unwrap_or((path, ""));
            let qtoken = query.split('&').find_map(|kv| kv.strip_prefix("token=")).unwrap_or("");
            let ok_token = !token.is_empty() && (auth == token || qtoken == token);
            let status = match (method, path) {
                ("POST", "/ring") if ok_token => {
                    if last.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
                        last = Some(Instant::now());
                        ringer.ring();
                    }
                    "204 No Content"
                }
                ("POST", "/ring") => "401 Unauthorized",
                _ => "404 Not Found",
            };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    Ok(())
}
