//! Rings and motion. Sources are pluggable and all end in `Events::ring()` / `Events::motion()`:
//! - HTTP: `POST /ring`, `POST /motion` with a bearer token (Home Assistant, Node-RED, webhooks…)
//! - UniFi Protect: the integration API's event stream (`crate::protect::events`)

use crate::hap::Server;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Events {
    pub server: Server,
    pub ring_iid: Option<u64>,
    /// dropped on a ring so the notification's picture is fresh
    pub snapshots: Option<Arc<crate::snapshot::Snapshots>>,
    pub motion: Option<Motion>,
    last_ring: Mutex<Option<Instant>>,
}

/// The Motion Sensor's state: on with each trigger, off `hold` after the last one.
pub struct Motion {
    pub iid: u64,
    pub state: Arc<AtomicBool>,
    pub hold: Duration,
    generation: Arc<AtomicU64>,
}

impl Motion {
    pub fn new(iid: u64, state: Arc<AtomicBool>, hold: Duration) -> Self {
        Motion { iid, state, hold, generation: Arc::new(AtomicU64::new(0)) }
    }
}

impl Events {
    pub fn new(server: Server, ring_iid: Option<u64>, snapshots: Option<Arc<crate::snapshot::Snapshots>>, motion: Option<Motion>) -> Self {
        Events { server, ring_iid, snapshots, motion, last_ring: Mutex::new(None) }
    }

    /// A doorbell press. Repeats within 3 s (two sources, double presses) count once.
    pub fn ring(&self, source: &str) {
        let Some(iid) = self.ring_iid else { return };
        {
            let mut last = self.last_ring.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
                return;
            }
            *last = Some(Instant::now());
        }
        crate::log(format!("ring ({source})"));
        if let Some(s) = &self.snapshots {
            s.invalidate();
        }
        self.server.notify(iid, serde_json::json!(0)); // single press
    }

    /// Motion seen (again): the sensor turns on, and off `hold` after the last trigger.
    pub fn motion(&self, source: &str) {
        let Some(m) = &self.motion else { return };
        let generation = m.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !m.state.swap(true, Ordering::SeqCst) {
            crate::log(format!("motion ({source})"));
            if let Some(s) = &self.snapshots {
                s.invalidate();
            }
            self.server.notify(m.iid, serde_json::json!(true));
        }
        let (state, gen_now, hold, server, iid) = (m.state.clone(), m.generation.clone(), m.hold, self.server.clone(), m.iid);
        std::thread::spawn(move || {
            std::thread::sleep(hold);
            if gen_now.load(Ordering::SeqCst) == generation && state.swap(false, Ordering::SeqCst) {
                crate::log("motion cleared".into());
                server.notify(iid, serde_json::json!(false));
            }
        });
    }
}

/// `POST /ring`, `POST /motion` with `Authorization: Bearer <token>` (or `?token=`).
pub fn http(listen: &str, token: String, events: Arc<Events>) -> Result<(), String> {
    let l = TcpListener::bind(listen).map_err(|e| format!("event listener {listen}: {e}"))?;
    crate::log(format!("events: POST http://{listen}/ring, /motion"));
    std::thread::spawn(move || {
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
            let status = match (method, path, ok_token) {
                ("POST", "/ring", true) => {
                    events.ring("http");
                    "204 No Content"
                }
                ("POST", "/motion", true) if events.motion.is_some() => {
                    events.motion("http");
                    "204 No Content"
                }
                ("POST", "/ring" | "/motion", false) => "401 Unauthorized",
                _ => "404 Not Found",
            };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    Ok(())
}
