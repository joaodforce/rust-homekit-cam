//! One mDNS responder per host, shared by every camera instance.
//!
//! Each instance (a separate process) used to run its own responder on UDP 5353. Multicast
//! queries reach all of them, but Linux hands a *unicast* query to port 5353 to just one of the
//! sockets, which then can't answer for the other cameras. So the instances elect one leader:
//!
//! - whoever binds the abstract Unix socket `@rust-homekit-cam-mdns` first is the leader and runs
//!   the only responder: its own advert plus every follower's;
//! - the others connect to it and send their advert as a JSON line (again on every change);
//! - a follower's connection closing (the process stopped or died) withdraws its advert;
//! - when the leader goes away the followers reconnect, one of them taking over.
//!
//! Abstract sockets need no files or permissions and work under systemd's sandboxing. Off
//! Linux every process is its own responder (fine for a single dev instance).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::Duration;

/// One `_hap._tcp` service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Advert {
    pub name: String,
    /// `<host>.local.`, resolved to this machine's addresses
    pub host: String,
    pub port: u16,
    pub txt: HashMap<String, String>,
}

const SERVICE: &str = "_hap._tcp.local.";
#[cfg(target_os = "linux")]
const SOCKET: &[u8] = b"rust-homekit-cam-mdns";

/// This process's handle: publish (or update) our advert.
pub struct Mdns {
    current: Arc<Mutex<Option<Advert>>>,
    tx: Sender<Ev>,
}

enum Ev {
    /// our advert changed
    Update,
    /// the leader closed connection number n (follower side)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    LeaderGone(u64),
}

impl Mdns {
    pub fn start() -> Mdns {
        let (tx, rx) = channel();
        let current = Arc::new(Mutex::new(None));
        let (c, t) = (current.clone(), tx.clone());
        std::thread::spawn(move || run(c, t, rx));
        Mdns { current, tx }
    }

    pub fn publish(&self, a: Advert) {
        *self.current.lock().unwrap() = Some(a);
        let _ = self.tx.send(Ev::Update);
    }
}

fn daemon() -> Result<mdns_sd::ServiceDaemon, String> {
    let d = mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?;
    // IPv4 only: an IPv6 address in the advert can hang "Adding to Home…"
    d.disable_interface(mdns_sd::IfKind::IPv6).map_err(|e| e.to_string())?;
    Ok(d)
}

/// Register (or update in place: same name, no goodbye packets) one advert; its full name.
fn register(d: &mdns_sd::ServiceDaemon, a: &Advert) -> Result<String, String> {
    if a.name.is_empty() || a.name.len() > 63 || a.host.len() > 255 {
        return Err("bad advert".into());
    }
    let info = mdns_sd::ServiceInfo::new(SERVICE, &a.name, &a.host, "", a.port, a.txt.clone())
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
    let full = info.get_fullname().to_string();
    d.register(info).map_err(|e| e.to_string())?;
    Ok(full)
}

fn run(current: Arc<Mutex<Option<Advert>>>, tx: Sender<Ev>, rx: Receiver<Ev>) {
    #[cfg(target_os = "linux")]
    for n in 0.. {
        match leader_socket() {
            Some(listener) => return lead(listener, current, rx),
            None => match follow(n, &current, &tx, &rx) {
                Ok(()) => return, // our handle is gone: the process is exiting
                Err(e) => {
                    crate::log(format!("mdns: {e}; electing a new responder"));
                    // spread the followers out so one of them wins the socket
                    std::thread::sleep(Duration::from_millis(100 + (std::process::id() % 400) as u64));
                }
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = tx;
        standalone(current, rx)
    }
}

#[cfg(not(target_os = "linux"))]
fn standalone(current: Arc<Mutex<Option<Advert>>>, rx: Receiver<Ev>) {
    let d = match daemon() {
        Ok(d) => d,
        Err(e) => return crate::log(format!("mdns: {e}")),
    };
    while let Ok(ev) = rx.recv() {
        if let (Ev::Update, Some(a)) = (ev, current.lock().unwrap().clone()) {
            if let Err(e) = register(&d, &a) {
                crate::log(format!("mdns: {e}"));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn leader_socket() -> Option<std::os::unix::net::UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(SOCKET).ok()?;
    std::os::unix::net::UnixListener::bind_addr(&addr).ok()
}

/// The responder: our advert from `rx`, followers' from their connections.
#[cfg(target_os = "linux")]
fn lead(listener: std::os::unix::net::UnixListener, current: Arc<Mutex<Option<Advert>>>, rx: Receiver<Ev>) {
    let d = match daemon() {
        Ok(d) => Arc::new(d),
        Err(e) => return crate::log(format!("mdns: {e}")),
    };
    crate::log("mdns: this instance answers mDNS for every camera on the host".into());
    let d2 = d.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let d = d2.clone();
            std::thread::spawn(move || serve_follower(&d, conn));
        }
    });
    let mut ours: Option<Advert> = None;
    loop {
        let a = current.lock().unwrap().clone();
        if a.is_some() && a != ours {
            match register(&d, a.as_ref().unwrap()) {
                Ok(_) => ours = a,
                Err(e) => crate::log(format!("mdns: {e}")),
            }
        }
        if rx.recv().is_err() {
            return;
        }
    }
}

/// One follower: register each advert line; withdraw it when the connection closes.
#[cfg(target_os = "linux")]
fn serve_follower(d: &mdns_sd::ServiceDaemon, conn: std::os::unix::net::UnixStream) {
    let mut registered: Option<(String, String)> = None; // (full name, display name)
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        let Ok(a) = serde_json::from_str::<Advert>(&line) else { continue };
        match register(d, &a) {
            Ok(full) => {
                if let Some((old, _)) = registered.as_ref().filter(|(old, _)| *old != full) {
                    let _ = d.unregister(old);
                }
                if registered.as_ref().map(|(f, _)| f) != Some(&full) {
                    crate::log(format!("mdns: answering for \"{}\" (port {})", a.name, a.port));
                }
                registered = Some((full, a.name));
            }
            Err(e) => crate::log(format!("mdns: advert for {}: {e}", a.name)),
        }
    }
    if let Some((full, name)) = registered {
        crate::log(format!("mdns: \"{name}\" went away, withdrawing it"));
        let _ = d.unregister(&full);
    }
}

/// Send our advert to the leader, again on each update. Ok when our handle is dropped, Err when
/// the leader goes away.
#[cfg(target_os = "linux")]
fn follow(n: u64, current: &Arc<Mutex<Option<Advert>>>, tx: &Sender<Ev>, rx: &Receiver<Ev>) -> Result<(), String> {
    use std::io::Read;
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(SOCKET).map_err(|e| e.to_string())?;
    let mut s = std::os::unix::net::UnixStream::connect_addr(&addr).map_err(|e| format!("can't reach the responder: {e}"))?;
    crate::log("mdns: another instance answers mDNS; sending it our advert".into());
    // the leader never writes: a read returning means it closed
    let mut watch = s.try_clone().map_err(|e| e.to_string())?;
    let t = tx.clone();
    std::thread::spawn(move || {
        let _ = watch.read(&mut [0u8; 1]);
        let _ = t.send(Ev::LeaderGone(n));
    });
    let send = |s: &mut std::os::unix::net::UnixStream| -> Result<(), String> {
        if let Some(a) = current.lock().unwrap().clone() {
            let mut line = serde_json::to_string(&a).map_err(|e| e.to_string())?;
            line.push('\n');
            s.write_all(line.as_bytes()).map_err(|e| format!("responder: {e}"))?;
        }
        Ok(())
    };
    send(&mut s)?;
    loop {
        match rx.recv() {
            Ok(Ev::Update) => send(&mut s)?,
            Ok(Ev::LeaderGone(m)) if m == n => return Err("the responder went away".into()),
            Ok(Ev::LeaderGone(_)) => {} // an earlier connection's watcher
            Err(_) => return Ok(()),
        }
    }
}
