//! HomeKit Accessory Protocol over IP: one accessory per `Server`, from crtd's `src/homekit`.
//!
//! - advertised as `_hap._tcp` (`ci` = category, `sf=1` while unpaired), IPv4-only host name
//!   (crtd lesson: an IPv6 address in the advert hangs "Adding to Home…")
//! - HTTP/1.1; after pair-verify every message is ChaCha20-Poly1305 framed (`crypto`)
//! - `GET /accessories`, `GET`/`PUT /characteristics` (values, write-responses, event
//!   subscriptions), `POST /pairings`, `/identify`; `Server::notify` pushes `EVENT/1.0`
//!
//! State (setup code, accessory id, controllers): `<state_dir>/hap.json`, long-term key in
//! `hap-ed25519.key`, both 0600.

pub mod crypto;
mod http;
mod identity;
mod pairing;

use crypto::{SecureReader, SecureWriter, Slot};
use http::{Request, Response};
use identity::Identity;
use pairing::{PairSetup, PairVerify, Store};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Video doorbell. (Cameras are 17.)
pub const CATEGORY: u64 = 18;

/// HAP status codes.
pub const OK: i64 = 0;
pub const NOT_READABLE: i64 = -70405;
pub const READ_ONLY: i64 = -70404;
pub const NO_SUCH: i64 = -70409;
pub const BAD_VALUE: i64 = -70410;

/// What a connection is: its id (for per-session state like streams) and the peer's address.
#[derive(Clone, Copy, Debug)]
pub struct Conn {
    pub id: u64,
    pub peer: IpAddr,
    pub local: IpAddr,
}

/// The attribute database and its behaviour. Instance ids are per accessory (aid 1).
pub trait Accessory: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn category(&self) -> u64;
    /// `GET /accessories`, services of aid 1.
    fn services(&self, serial: &str) -> Value;
    fn read(&self, conn: Conn, iid: u64) -> Result<Value, i64>;
    /// A write; `Some(value)` is returned to the controller when it asked for a write-response.
    fn write(&self, conn: Conn, iid: u64, value: &Value) -> Result<Option<Value>, i64>;
    /// The connection closed (stop its streams).
    fn closed(&self, _conn: Conn) {}
}

struct Session {
    controller: String,
    writer: Arc<Mutex<SecureWriter>>,
    events: HashSet<u64>,
    stream: TcpStream,
}

struct Inner {
    acc: Arc<dyn Accessory>,
    port: u16,
    dir: PathBuf,
    id: Identity,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<u64, Session>>,
    advert: Mutex<Option<mdns_sd::ServiceDaemon>>,
}

#[derive(Clone)]
pub struct Server(Arc<Inner>);

fn log(msg: String) {
    crate::log(format!("hap: {msg}"));
}

impl Server {
    pub fn start(acc: Arc<dyn Accessory>, port: u16, dir: PathBuf) -> Result<Server, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let id = Identity::load_or_create(&dir.join("hap-ed25519.key")).map_err(|e| e.to_string())?;
        let store = load_store(&dir);
        if !store.paired() {
            log(format!("not paired: add \"{}\" in the Home app, setup code {}", acc.name(), store.setup_code));
        }
        let listener = TcpListener::bind(("::", port))
            .or_else(|_| TcpListener::bind(("0.0.0.0", port)))
            .map_err(|e| format!("port {port}: {e}"))?;
        let s = Server(Arc::new(Inner {
            acc,
            port,
            dir,
            id,
            store: Mutex::new(store),
            sessions: Mutex::new(HashMap::new()),
            advert: Mutex::new(None),
        }));
        s.advertise();
        let srv = s.clone();
        std::thread::spawn(move || {
            let mut next = 0u64;
            for stream in listener.incoming().flatten() {
                next += 1;
                let (n, srv) = (next, srv.clone());
                std::thread::spawn(move || srv.connection(n, stream));
            }
        });
        Ok(s)
    }

    /// The setup code and whether a controller is paired.
    pub fn setup_code(&self) -> (String, bool) {
        let st = self.0.store.lock().unwrap();
        (st.setup_code.clone(), st.paired())
    }

    /// Push a characteristic's new value to every session that subscribed to it.
    pub fn notify(&self, iid: u64, value: Value) {
        let body = serde_json::to_vec(&json!({"characteristics": [{"aid": 1, "iid": iid, "value": value}]})).unwrap();
        let mut sent = 0;
        for s in self.0.sessions.lock().unwrap().values() {
            if s.events.contains(&iid) {
                let msg = Response::ok().body("application/hap+json", body.clone());
                if msg.write(&mut *s.writer.lock().unwrap(), "EVENT/1.0").is_ok() {
                    sent += 1;
                }
            }
        }
        log(format!("event {iid} = {value} to {sent} session(s)"));
    }

    fn save(&self, s: &Store) {
        save_store(&self.0.dir, s);
    }

    /// (Re-)announce `_hap._tcp` with the current pairing state and config number.
    fn advertise(&self) {
        let st = self.0.store.lock().unwrap().clone();
        let name = self.0.acc.name().to_string();
        let shape = self.0.acc.services(&st.device_id).to_string();
        let cn = 1 + shape.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193)) % 65534;
        let txt: HashMap<String, String> = [
            ("c#", cn.to_string()),
            ("ff", "0".into()),
            ("id", st.device_id.clone()),
            ("md", name.clone()),
            ("pv", "1.1".into()),
            ("s#", "1".into()),
            ("sf", if st.paired() { "0" } else { "1" }.into()),
            ("ci", self.0.acc.category().to_string()),
            ("sh", st.setup_hash()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut adv = self.0.advert.lock().unwrap();
        if let Some(old) = adv.take() {
            let _ = old.shutdown();
            std::thread::sleep(Duration::from_millis(200));
        }
        let port = self.0.port;
        let host = format!("{}.local.", st.device_id.replace(':', ""));
        let result = (|| -> Result<mdns_sd::ServiceDaemon, String> {
            let d = mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?;
            d.disable_interface(mdns_sd::IfKind::IPv6).map_err(|e| e.to_string())?;
            let info = mdns_sd::ServiceInfo::new("_hap._tcp.local.", &name, &host, "", port, txt)
                .map_err(|e| e.to_string())?
                .enable_addr_auto();
            d.register(info).map_err(|e| e.to_string())?;
            Ok(d)
        })();
        match result {
            Ok(d) => *adv = Some(d),
            Err(e) => log(format!("mdns: {e}")),
        }
    }

    fn connection(&self, n: u64, stream: TcpStream) {
        let hk = &self.0;
        let unmap = |ip: IpAddr| match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
            v4 => v4,
        };
        let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else { return };
        let conn = Conn { id: n, peer: unmap(peer.ip()), local: unmap(local.ip()) };
        let (Ok(read_half), Ok(spare)) = (stream.try_clone(), stream.try_clone()) else { return };
        let (read_slot, write_slot): (Slot, Slot) = Default::default();
        let mut reader = BufReader::new(SecureReader::new(read_half, read_slot.clone()));
        let writer = Arc::new(Mutex::new(SecureWriter::new(stream, write_slot.clone())));
        let (mut setup, mut verify) = (PairSetup::default(), PairVerify::default());
        let mut controller: Option<String> = None;

        log(format!("connection {n} from {peer}"));
        while let Ok(Some(req)) = http::read_request(&mut reader) {
            log(format!("connection {n}: {} {} ({} bytes)", req.method, req.path, req.body.len()));
            let mut encrypt_with = None;
            let resp = match (req.method.as_str(), req.path.split('?').next().unwrap_or("")) {
                ("POST", "/pair-setup") => {
                    let st = hk.store.lock().unwrap().clone();
                    match setup.step(&st, &hk.id, &req.body) {
                        Ok((body, Some(p))) => {
                            log(format!("paired with controller {} ({peer})", p.id));
                            let mut s = hk.store.lock().unwrap();
                            s.pairings = vec![p];
                            self.save(&s);
                            drop(s);
                            self.advertise();
                            tlv_reply(body)
                        }
                        Ok((body, None)) => tlv_reply(body),
                        Err(e) => {
                            log(format!("pair-setup: {e}"));
                            Response::status(400, "Bad Request")
                        }
                    }
                }
                ("POST", "/pair-verify") => {
                    let st = hk.store.lock().unwrap().clone();
                    match verify.step(&st, &hk.id, &req.body) {
                        Ok((body, Some((shared, ctrl)))) => {
                            controller = Some(ctrl);
                            encrypt_with = Some(shared);
                            tlv_reply(body)
                        }
                        Ok((body, None)) => tlv_reply(body),
                        Err(e) => {
                            log(format!("pair-verify: {e}"));
                            Response::status(400, "Bad Request")
                        }
                    }
                }
                ("POST", "/identify") if !hk.store.lock().unwrap().paired() => {
                    log("identify".into());
                    Response::status(204, "No Content")
                }
                _ if controller.is_none() => hap_json(470, "Connection Authorization Required", &json!({"status": -70401})),
                _ => {
                    let ctrl = controller.clone().unwrap();
                    let r = self.handle(conn, &ctrl, &req);
                    if hk.store.lock().unwrap().pairings.iter().all(|p| p.id != ctrl) {
                        let _ = r.write(&mut *writer.lock().unwrap(), "HTTP/1.1");
                        break;
                    }
                    r
                }
            };
            if resp.write(&mut *writer.lock().unwrap(), "HTTP/1.1").is_err() {
                break;
            }
            if let Some(shared) = encrypt_with {
                let (r, w) = crypto::control_ciphers(&shared);
                *read_slot.lock().unwrap() = Some(r);
                *write_slot.lock().unwrap() = Some(w);
                let ctrl = controller.clone().unwrap();
                log(format!("session {n}: controller {ctrl} ({peer})"));
                let Ok(stream) = spare.try_clone() else { break };
                hk.sessions.lock().unwrap().insert(n, Session { controller: ctrl, writer: writer.clone(), events: HashSet::new(), stream });
            }
        }
        hk.sessions.lock().unwrap().remove(&n);
        hk.acc.closed(conn);
        log(format!("connection {n} closed"));
    }

    /// The encrypted endpoints, for a verified controller.
    fn handle(&self, conn: Conn, ctrl: &str, req: &Request) -> Response {
        let hk = &self.0;
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        let serial = hk.store.lock().unwrap().device_id.clone();
        match (req.method.as_str(), path) {
            ("GET", "/accessories") => {
                hap_json(200, "OK", &json!({"accessories": [{"aid": 1, "services": hk.acc.services(&serial)}]}))
            }
            ("GET", "/characteristics") => {
                let ids = query.split('&').find_map(|kv| kv.strip_prefix("id=")).unwrap_or("");
                let mut any_error = false;
                let mut out: Vec<Value> = ids
                    .split(',')
                    .filter_map(|p| p.split_once('.'))
                    .map(|(a, i)| {
                        let iid: u64 = i.parse().unwrap_or(0);
                        let r = if a == "1" { hk.acc.read(conn, iid) } else { Err(NO_SUCH) };
                        match r {
                            Ok(v) => json!({"aid": 1, "iid": iid, "value": v}),
                            Err(code) => {
                                any_error = true;
                                json!({"aid": a.parse::<u64>().unwrap_or(0), "iid": iid, "status": code})
                            }
                        }
                    })
                    .collect();
                if any_error {
                    for o in &mut out {
                        o.as_object_mut().unwrap().entry("status").or_insert(json!(0));
                    }
                    hap_json(207, "Multi-Status", &json!({"characteristics": out}))
                } else {
                    hap_json(200, "OK", &json!({"characteristics": out}))
                }
            }
            ("PUT", "/characteristics") => {
                let Ok(body) = serde_json::from_slice::<Value>(&req.body) else { return Response::status(400, "Bad Request") };
                let mut results = Vec::new();
                let mut multi = false;
                for ch in body["characteristics"].as_array().cloned().unwrap_or_default() {
                    let iid = ch["iid"].as_u64().unwrap_or(0);
                    let mut r = json!({"aid": 1, "iid": iid, "status": OK});
                    if let Some(ev) = ch.get("ev").and_then(Value::as_bool) {
                        if let Some(s) = hk.sessions.lock().unwrap().get_mut(&conn.id) {
                            if ev { s.events.insert(iid) } else { s.events.remove(&iid) };
                        }
                    }
                    if let Some(v) = ch.get("value") {
                        match hk.acc.write(conn, iid, v) {
                            Ok(Some(back)) if ch["r"].as_bool() == Some(true) => {
                                r["value"] = back;
                                multi = true;
                            }
                            Ok(_) => {}
                            Err(code) => {
                                r["status"] = json!(code);
                                multi = true;
                            }
                        }
                    }
                    results.push(r);
                }
                if multi {
                    hap_json(207, "Multi-Status", &json!({"characteristics": results}))
                } else {
                    Response::status(204, "No Content")
                }
            }
            ("PUT", "/prepare") => hap_json(200, "OK", &json!({"status": 0})),
            ("POST", "/resource") => {
                // snapshots: not yet (milestone 5)
                Response::status(404, "Not Found")
            }
            ("POST", "/pairings") => {
                let st = hk.store.lock().unwrap().clone();
                let (body, changed) = pairing::pairings(&st, ctrl, &req.body);
                if let Some(list) = changed {
                    let unpaired = list.is_empty();
                    let mut s = hk.store.lock().unwrap();
                    s.pairings = list;
                    self.save(&s);
                    let remaining: Vec<String> = s.pairings.iter().map(|p| p.id.clone()).collect();
                    drop(s);
                    log(if unpaired { "all pairings removed: unpaired".into() } else { format!("pairings now: {remaining:?}") });
                    for (k, sess) in hk.sessions.lock().unwrap().iter() {
                        if *k != conn.id && !remaining.contains(&sess.controller) {
                            let _ = sess.stream.shutdown(std::net::Shutdown::Both);
                        }
                    }
                    self.advertise();
                }
                tlv_reply(body)
            }
            _ => Response::status(404, "Not Found"),
        }
    }
}

fn hap_json(status: u16, reason: &'static str, v: &Value) -> Response {
    Response::status(status, reason).body("application/hap+json", serde_json::to_vec(v).unwrap())
}

fn tlv_reply(body: Vec<u8>) -> Response {
    Response::ok().body("application/pairing+tlv8", body)
}

fn load_store(dir: &std::path::Path) -> Store {
    let path = dir.join("hap.json");
    if let Some(s) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Store>(&b).ok()) {
        return s;
    }
    let s = Store::new();
    let _ = std::fs::create_dir_all(dir);
    save_store(dir, &s);
    s
}

fn save_store(dir: &std::path::Path, s: &Store) {
    let path = dir.join("hap.json");
    let tmp = path.with_extension("tmp");
    let ok = std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap()).is_ok() && std::fs::rename(&tmp, &path).is_ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    if !ok {
        log(format!("can't save {}", path.display()));
    }
}

/// Forget all controllers (setup code and accessory id stay).
pub fn reset(dir: PathBuf) -> usize {
    let mut s = load_store(&dir);
    let n = s.pairings.len();
    s.pairings.clear();
    save_store(&dir, &s);
    n
}

/// The setup code, without starting a server.
pub fn code(dir: PathBuf) -> (String, bool) {
    let s = load_store(&dir);
    (s.setup_code.clone(), s.paired())
}
