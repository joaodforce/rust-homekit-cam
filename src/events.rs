//! Rings and motion. Sources are pluggable and all end in `Events::ring()` / `Events::motion()`:
//! - HTTP: `POST /ring`, `POST /motion` with a bearer token (Home Assistant, Node-RED, webhooks…)
//! - UniFi Protect: the integration API's event stream (`crate::protect::events`)
//! - ONVIF: pull-point events (`crate::onvif`)
//!
//! Out (`motion_webhook`): each change of the Motion Sensor (on / cleared) and each ring is
//! POSTed as JSON to a URL, e.g. a Home Assistant webhook trigger, so other systems get the
//! cameras' motion without talking to the cameras.

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
    webhooks: Vec<Arc<Webhook>>,
}

/// Where state changes go: `{"camera": name, "event": "motion"|"ring", "state": …, "source": …}`.
pub struct Webhook {
    pub url: String,
    pub camera: String,
}

impl Webhook {
    /// POST in the background: a slow or absent receiver never holds up HomeKit.
    pub fn send(self: &Arc<Self>, event: &str, state: &str, source: &str) {
        let (w, body) = (self.clone(), webhook_body(&self.camera, event, state, source));
        std::thread::spawn(move || {
            match crate::httpc::request("POST", &w.url, Some("application/json"), body.as_bytes(), Duration::from_secs(5)) {
                Ok(r) if (200..300).contains(&r.status) => {}
                Ok(r) => crate::log(format!("motion_webhook {}: HTTP {}", w.shown(), r.status)),
                Err(e) => crate::log(format!("motion_webhook {}: {e}", w.shown())),
            }
        });
    }
}

impl Webhook {
    /// The URL for logs: without the query (it may carry a token).
    pub fn shown(&self) -> &str {
        self.url.split('?').next().unwrap_or(&self.url)
    }
}

fn webhook_body(camera: &str, event: &str, state: &str, source: &str) -> String {
    serde_json::json!({ "camera": camera, "event": event, "state": state, "source": source }).to_string()
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
        Events { server, ring_iid, snapshots, motion, last_ring: Mutex::new(None), webhooks: Vec::new() }
    }

    pub fn with_webhooks(mut self, w: Vec<Webhook>) -> Self {
        self.webhooks = w.into_iter().map(Arc::new).collect();
        self
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
        for w in &self.webhooks {
            w.send("ring", "pressed", source);
        }
        if let Some(s) = &self.snapshots {
            s.invalidate();
        }
        self.server.notify(iid, serde_json::json!(0)); // single press
    }

    /// Motion seen (again): the sensor turns on, and off `hold` after the last trigger.
    pub fn motion(&self, source: &str) {
        if let Some(g) = self.motion_on(source) {
            self.motion_off_after(g, None);
        }
    }

    /// Motion started and lasts until `motion_end` (at most 10 minutes, in case the end is lost).
    pub fn motion_start(&self, source: &str) {
        if let Some(g) = self.motion_on(source) {
            self.motion_off_after(g, Some(Duration::from_secs(600)));
        }
    }

    /// Motion ended: the sensor goes off `hold` from now (unless motion comes back).
    pub fn motion_end(&self, _source: &str) {
        let Some(m) = &self.motion else { return };
        if m.state.load(Ordering::SeqCst) {
            let g = m.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.motion_off_after(g, None);
        }
    }

    /// Sensor on (notifying on the change); returns the trigger's generation.
    fn motion_on(&self, source: &str) -> Option<u64> {
        let m = self.motion.as_ref()?;
        let generation = m.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !m.state.swap(true, Ordering::SeqCst) {
            crate::log(format!("motion ({source})"));
            for w in &self.webhooks {
                w.send("motion", "start", source);
            }
            if let Some(s) = &self.snapshots {
                s.invalidate();
            }
            self.server.notify(m.iid, serde_json::json!(true));
        }
        Some(generation)
    }

    /// Off after `after` (default: the hold time), unless a later trigger came in meanwhile.
    fn motion_off_after(&self, generation: u64, after: Option<Duration>) {
        let Some(m) = &self.motion else { return };
        let (state, gen_now, server, iid, webhook) = (m.state.clone(), m.generation.clone(), self.server.clone(), m.iid, self.webhooks.clone());
        let wait = after.unwrap_or(m.hold);
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            if gen_now.load(Ordering::SeqCst) == generation && state.swap(false, Ordering::SeqCst) {
                crate::log("motion cleared".into());
                server.notify(iid, serde_json::json!(false));
                for w in webhook {
                    w.send("motion", "end", "hold");
                }
            }
        });
    }
}

/// `POST /ring`, `POST /motion` (a pulse: on for `motion_hold`), `POST /motion/start` and
/// `POST /motion/end`, with `Authorization: Bearer <token>` (or `?token=`).
pub fn http(listen: &str, token: String, events: Arc<Events>) -> Result<(), String> {
    let l = TcpListener::bind(listen).map_err(|e| format!("event listener {listen}: {e}"))?;
    crate::log(format!("events: POST http://{listen}/ring, /motion, /motion/start, /motion/end"));
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
                ("POST", "/motion" | "/motion/start" | "/motion/end", true) if events.motion.is_some() => {
                    match path {
                        "/motion/start" => events.motion_start("http"),
                        "/motion/end" => events.motion_end("http"),
                        _ => events.motion("http"),
                    }
                    "204 No Content"
                }
                ("POST", "/ring" | "/motion" | "/motion/start" | "/motion/end", false) => "401 Unauthorized",
                _ => "404 Not Found",
            };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_body_shape() {
        let b: serde_json::Value = serde_json::from_str(&webhook_body("Garagem", "motion", "start", "onvif")).unwrap();
        assert_eq!(b, serde_json::json!({"camera": "Garagem", "event": "motion", "state": "start", "source": "onvif"}));
    }

    #[test]
    fn webhook_posts_json() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/webhook/cam-motion", l.local_addr().unwrap());
        Arc::new(Webhook { url, camera: "Garagem".into() }).send("motion", "end", "hold");
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut raw = Vec::new();
        crate::httpc::read_request_for_test(&mut s, &mut raw);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("POST /api/webhook/cam-motion HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Content-Type: application/json"));
        assert!(text.ends_with(r#"{"camera":"Garagem","event":"motion","source":"hold","state":"end"}"#), "{text}");
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
    }
}
