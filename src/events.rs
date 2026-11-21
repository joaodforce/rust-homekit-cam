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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

pub struct Events {
    pub server: Server,
    pub ring_iid: Option<u64>,
    /// dropped on a ring so the notification's picture is fresh
    pub snapshots: Option<Arc<crate::snapshot::Snapshots>>,
    pub motion: Option<Motion>,
    last_ring: Mutex<Option<Instant>>,
    webhooks: Vec<Arc<Webhook>>,
    webhook_snapshot: Option<WebhookSnapshot>,
}

/// Where state changes go: `{"camera": name, "event": "motion"|"ring", "state": …, "source": …}`.
pub struct Webhook {
    pub url: String,
    pub camera: String,
}

impl Webhook {
    /// POST in the background: a slow or absent receiver never holds up HomeKit.
    pub fn send(self: &Arc<Self>, event: &str, state: &str, source: &str) {
        let (w, body) = (self.clone(), webhook_body(&self.camera, event, state, source, None));
        std::thread::spawn(move || w.post(&body));
    }

    fn post(&self, body: &str) {
        match crate::httpc::request("POST", &self.url, Some("application/json"), body.as_bytes(), Duration::from_secs(5)) {
            Ok(r) if (200..300).contains(&r.status) => {}
            Ok(r) => crate::log(format!("motion_webhook {}: HTTP {}", self.shown(), r.status)),
            Err(e) => crate::log(format!("motion_webhook {}: {e}", self.shown())),
        }
    }
}

impl Webhook {
    /// The URL for logs: without the query (it may carry a token).
    pub fn shown(&self) -> &str {
        self.url.split('?').next().unwrap_or(&self.url)
    }
}

fn webhook_body(camera: &str, event: &str, state: &str, source: &str, snapshot: Option<&str>) -> String {
    let mut b = serde_json::json!({ "camera": camera, "event": event, "state": state, "source": source });
    if let Some(s) = snapshot {
        b["snapshot"] = s.into(); // base64 JPEG
    }
    b.to_string()
}

/// `motion_webhook_snapshot`: a small JPEG with each motion start.
#[derive(Clone, Copy, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSnapshot {
    /// pixels, the height follows the aspect
    #[serde(default = "thumb_width")]
    pub width: u32,
    /// JPEG quality 1–100
    #[serde(default = "thumb_quality")]
    pub quality: u8,
}

fn thumb_width() -> u32 {
    320
}
fn thumb_quality() -> u8 {
    60
}

/// The Motion Sensor's state: on with each trigger, off `hold` after the last one.
pub struct Motion {
    pub iid: u64,
    pub state: Arc<AtomicBool>,
    pub hold: Duration,
    /// when the sensor goes off; one timer thread per camera waits for it (each trigger moves it)
    timer: Arc<Deadline>,
    started: Once,
}

/// A deadline one thread waits for: `set` moves it (the latest call wins), `run` calls `expired`
/// each time it passes. Replaces a sleeping thread per trigger, which piled up on busy cameras
/// (a motion start waits up to 10 minutes for its end).
#[derive(Default)]
pub struct Deadline {
    at: Mutex<Option<Instant>>,
    changed: Condvar,
}

impl Deadline {
    pub fn set(&self, after: Duration) {
        *self.at.lock().unwrap() = Some(Instant::now() + after);
        self.changed.notify_one();
    }

    /// Wait for deadlines for ever, calling `expired` as each one passes.
    pub fn run(&self, expired: impl Fn()) {
        let mut at = self.at.lock().unwrap();
        loop {
            match *at {
                None => at = self.changed.wait(at).unwrap(),
                Some(t) if Instant::now() >= t => {
                    *at = None;
                    drop(at);
                    expired();
                    at = self.at.lock().unwrap();
                }
                Some(t) => at = self.changed.wait_timeout(at, t - Instant::now()).unwrap().0,
            }
        }
    }
}

impl Motion {
    pub fn new(iid: u64, state: Arc<AtomicBool>, hold: Duration) -> Self {
        Motion { iid, state, hold, timer: Arc::new(Deadline::default()), started: Once::new() }
    }
}

impl Events {
    pub fn new(server: Server, ring_iid: Option<u64>, snapshots: Option<Arc<crate::snapshot::Snapshots>>, motion: Option<Motion>) -> Self {
        Events { server, ring_iid, snapshots, motion, last_ring: Mutex::new(None), webhooks: Vec::new(), webhook_snapshot: None }
    }

    pub fn with_webhooks(mut self, w: Vec<Webhook>, snapshot: Option<WebhookSnapshot>) -> Self {
        self.webhooks = w.into_iter().map(Arc::new).collect();
        self.webhook_snapshot = snapshot;
        self
    }

    /// Motion start to the webhooks, with a fresh small snapshot when configured (fetched once,
    /// in the background, then sent to each).
    fn webhooks_motion_start(&self, source: &str) {
        let (Some(cfg), Some(snaps)) = (self.webhook_snapshot, self.snapshots.clone()) else {
            for w in &self.webhooks {
                w.send("motion", "start", source);
            }
            return;
        };
        let (webhooks, source) = (self.webhooks.clone(), source.to_string());
        std::thread::spawn(move || {
            let thumb = snaps.get().and_then(|j| crate::snapshot::shrink_jpeg(&j, cfg.width, cfg.quality));
            let b64 = match thumb {
                Ok(t) => Some(crate::tlv::b64(&t)),
                Err(e) => {
                    crate::log(format!("motion_webhook: no snapshot ({e}), sending the event without one"));
                    None
                }
            };
            for w in &webhooks {
                w.post(&webhook_body(&w.camera, "motion", "start", &source, b64.as_deref()));
            }
        });
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
        if self.motion_on(source) {
            self.motion_off_after(None);
        }
    }

    /// Motion started and lasts until `motion_end` (at most 10 minutes, in case the end is lost).
    pub fn motion_start(&self, source: &str) {
        if self.motion_on(source) {
            self.motion_off_after(Some(Duration::from_secs(600)));
        }
    }

    /// Motion ended: the sensor goes off `hold` from now (unless motion comes back).
    pub fn motion_end(&self, _source: &str) {
        let Some(m) = &self.motion else { return };
        if m.state.load(Ordering::SeqCst) {
            self.motion_off_after(None);
        }
    }

    /// Sensor on (notifying on the change); false without a Motion Sensor.
    fn motion_on(&self, source: &str) -> bool {
        let Some(m) = self.motion.as_ref() else { return false };
        if !m.state.swap(true, Ordering::SeqCst) {
            crate::log(format!("motion ({source})"));
            if let Some(s) = &self.snapshots {
                s.invalidate(); // the next picture (notifications, the webhook's) is from now
            }
            self.webhooks_motion_start(source);
            self.server.notify(m.iid, serde_json::json!(true));
        }
        true
    }

    /// Off after `after` (default: the hold time), unless a later trigger moves it again.
    fn motion_off_after(&self, after: Option<Duration>) {
        let Some(m) = &self.motion else { return };
        m.started.call_once(|| {
            let (timer, state, server, iid, webhooks) = (m.timer.clone(), m.state.clone(), self.server.clone(), m.iid, self.webhooks.clone());
            std::thread::spawn(move || {
                timer.run(|| {
                    if state.swap(false, Ordering::SeqCst) {
                        crate::log("motion cleared".into());
                        server.notify(iid, serde_json::json!(false));
                        for w in &webhooks {
                            w.send("motion", "end", "hold");
                        }
                    }
                })
            });
        });
        m.timer.set(after.unwrap_or(m.hold));
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
        let b: serde_json::Value = serde_json::from_str(&webhook_body("Garagem", "motion", "start", "onvif", None)).unwrap();
        assert_eq!(b, serde_json::json!({"camera": "Garagem", "event": "motion", "state": "start", "source": "onvif"}));
        let b: serde_json::Value = serde_json::from_str(&webhook_body("Garagem", "motion", "start", "onvif", Some("/9j/"))).unwrap();
        assert_eq!(b["snapshot"], "/9j/");
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

    #[test]
    fn deadline_latest_set_wins_and_one_thread_serves_all() {
        let d = Arc::new(Deadline::default());
        let fired = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let (d2, f2) = (d.clone(), fired.clone());
        std::thread::spawn(move || d2.run(|| f2.lock().unwrap().push(Instant::now())));
        let t0 = Instant::now();
        d.set(Duration::from_secs(600)); // a start: off in 10 min unless something else comes
        d.set(Duration::from_millis(80)); // an end: off after the hold instead
        std::thread::sleep(Duration::from_millis(40));
        d.set(Duration::from_millis(80)); // motion again: pushed back
        std::thread::sleep(Duration::from_millis(250));
        let f = fired.lock().unwrap().clone();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0] - t0 >= Duration::from_millis(115), "fired after {:?}", f[0] - t0);
        drop(f);
        d.set(Duration::from_millis(10)); // and it keeps serving later events
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(fired.lock().unwrap().len(), 2);
    }
}
