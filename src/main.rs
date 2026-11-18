//! rust-homekit-cam: an RTSP camera / doorbell (UniFi Protect or any H.264 camera) as a native
//! HomeKit accessory.
//!
//! `rust-homekit-cam serve <config.toml>` | `code <config.toml>` | `reset <config.toml>`

mod camera;
mod h264;
mod hap;
mod hds;
mod hksv;
mod mp4;
mod httpc;
mod protect;
mod recording;
mod events;
mod rtsp;
mod snapshot;
mod srtp;
mod stream;
mod tlv;

use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

pub fn log(msg: String) {
    eprintln!("homekit-cam: {msg}");
}

/// Protect integration API: API key, camera id; the console's address (default: the RTSP
/// host) and its certificate's SHA-256 fingerprint ("AB:CD:…").
#[derive(Deserialize)]
struct ProtectCfg {
    /// which events turn the Motion Sensor on: "smart" (person, vehicle…) or "any"
    #[serde(default = "smart")]
    motion: protect::MotionKind,
    token: String,
    camera_id: String,
    host: Option<String>,
    cert_sha256: Option<String>,
}

#[derive(Deserialize)]
struct Config {
    /// Name in the Home app.
    name: String,
    #[serde(default = "yes")]
    doorbell: bool,
    /// Shown in the Home app's accessory details.
    manufacturer: Option<String>,
    model: Option<String>,
    #[serde(default = "default_port")]
    hap_port: u16,
    #[serde(default = "default_state")]
    state_dir: PathBuf,
    /// HomeKit Secure Video: on by default with the Motion Sensor (its trigger)
    hksv: Option<bool>,
    /// Motion Sensor: on by default when there is a Protect backend; `POST /motion` feeds it too.
    motion: Option<bool>,
    /// seconds the sensor stays on after the last motion
    #[serde(default = "motion_hold")]
    motion_hold: u64,
    /// `POST /ring`, `POST /motion` listener, e.g. "0.0.0.0:8780"; needs `ring_token`.
    ring_listen: Option<String>,
    #[serde(default)]
    ring_token: String,
    /// A JPEG snapshot URL (http://[user:pass@]host/path); default with [protect]: Protect's.
    snapshot_url: Option<String>,
    /// Optional UniFi Protect backend (two-way audio, snapshots).
    protect: Option<ProtectCfg>,
    /// Protect RTSP URLs (rtsp://<nvr>:7447/<alias>) per channel, for live view.
    #[serde(default)]
    rtsp: stream::Channels,
}

fn yes() -> bool {
    true
}
fn smart() -> protect::MotionKind {
    protect::MotionKind::Smart
}
fn motion_hold() -> u64 {
    10
}
fn default_port() -> u16 {
    51830
}
fn default_state() -> PathBuf {
    "/var/lib/rust-homekit-cam".into()
}

fn load(path: &str) -> Config {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("{path}: {e}")));
    toml::from_str(&text).unwrap_or_else(|e| die(format!("{path}: {e}")))
}

fn die(msg: String) -> ! {
    log(msg);
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (cmd, path) = match args.as_slice() {
        [_, c, p] | [_, c, p, _] => (c.as_str(), p.as_str()),
        _ => die("usage: rust-homekit-cam serve|code|reset <config.toml>".into()),
    };
    let cfg = load(path);
    match cmd {
        "code" => {
            let (code, paired) = hap::code(cfg.state_dir);
            println!("{code} ({})", if paired { "paired" } else { "not paired" });
        }
        "probe" => probe(&cfg.rtsp),
        "record-test" => record_test(&cfg.rtsp, args.get(3).map(String::as_str).unwrap_or("record-test.mp4")),
        "reset" =>println!("removed {} pairing(s); restart the service", hap::reset(cfg.state_dir)),
        "serve" => {
            let protect = cfg.protect.as_ref().map(|p| {
                let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host?".into()));
                Arc::new(protect::Protect::new(&host, &p.token, &p.camera_id, p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e)))
            });
            if protect.is_none() {
                log("no [protect]: two-way audio off".into());
            }
            let snapshots = match (&cfg.snapshot_url, &protect) {
                (Some(u), _) => Some(snapshot::Source::Url(u.clone())),
                (None, Some(p)) => Some(snapshot::Source::Protect(p.clone())),
                _ => None,
            }
            .map(|s| Arc::new(snapshot::Snapshots::new(s)));
            if snapshots.is_none() {
                log("no snapshot_url or [protect]: no snapshots (blank tiles)".into());
            }
            let motion_state = cfg.motion.unwrap_or(protect.is_some()).then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let hksv = (cfg.hksv.unwrap_or(motion_state.is_some()) && motion_state.is_some())
                .then(|| cfg.rtsp.pick(2000).map(|(_, u)| u.to_string()))
                .flatten()
                .map(|url| hksv::Hksv::new(cfg.doorbell, cfg.state_dir.clone(), url).unwrap_or_else(|e| die(e)));
            let cam = Arc::new(
                camera::Camera::new(&cfg.name, cfg.doorbell, cfg.rtsp.clone(), protect.clone())
                    .with_info(cfg.manufacturer.clone(), cfg.model.clone())
                    .with_snapshots(snapshots.clone())
                    .with_motion(motion_state.clone())
                    .with_hksv(hksv.clone()),
            );
            let server = hap::Server::start(cam, cfg.hap_port, cfg.state_dir).unwrap_or_else(|e| die(e));
            let motion = motion_state.map(|st| events::Motion::new(camera::MOTION_DETECTED, st, std::time::Duration::from_secs(cfg.motion_hold)));
            let ev = Arc::new(events::Events::new(server.clone(), cfg.doorbell.then_some(camera::SWITCH_EVENT), snapshots.clone(), motion));
            if let (Some(p), Some(pc)) = (&protect, &cfg.protect) {
                follow_protect(p.clone(), pc.motion, ev.clone());
            }
            if let Some(listen) = &cfg.ring_listen {
                if cfg.ring_token.is_empty() {
                    die("ring_listen needs ring_token".into());
                }
                events::http(listen, cfg.ring_token.clone(), ev.clone()).unwrap_or_else(|e| die(e));
            }
            loop {
                std::thread::park();
            }
        }
        c => die(format!("unknown command {c}")),
    }
}

/// Rings and motion from Protect's event stream. A motion event keeps the sensor on until it
/// ends (re-triggered every few seconds), then `motion_hold` more.
fn follow_protect(p: Arc<protect::Protect>, kind: protect::MotionKind, ev: Arc<events::Events>) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    let open: Arc<Mutex<HashSet<String>>> = Arc::default();
    let (open2, ev2) = (open.clone(), ev.clone());
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(4));
        let mut o = open2.lock().unwrap();
        if !o.is_empty() {
            ev2.motion("protect");
        }
        o.shrink_to_fit();
    });
    p.events(kind, move |e| match e {
        protect::Event::Ring => ev.ring("protect"),
        protect::Event::Motion(id) => {
            open.lock().unwrap().insert(id);
            ev.motion("protect");
        }
        protect::Event::MotionEnd(id) => {
            if open.lock().unwrap().remove(&id) {
                ev.motion("protect"); // the hold starts now
            }
        }
    });
}

/// The console's host, from any RTSP URL.
fn rtsp_host(ch: &stream::Channels) -> Option<String> {
    let url = ch.low.as_ref().or(ch.medium.as_ref()).or(ch.high.as_ref())?;
    Some(url.strip_prefix("rtsp://")?.split(['/', ':']).next()?.to_string())
}

/// `record-test <config> <out.mp4>`: what HKSV would get: 4 s prebuffer + ~8 s live, as fMP4.
fn record_test(ch: &stream::Channels, out: &str) {
    let url = ch.pick(2000).map(|(_, u)| u.to_string()).unwrap_or_else(|| die("no RTSP URL".into()));
    let r = recording::Recorder::new(url);
    r.enabled.store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_secs(6));
    let (init, frags) = r.start().unwrap_or_else(|e| die(e));
    let mut file = init;
    let t0 = std::time::Instant::now();
    let mut n = 0;
    while t0.elapsed().as_secs() < 8 {
        if let Ok(f) = frags.recv_timeout(std::time::Duration::from_secs(5)) {
            file.extend(f);
            n += 1;
        }
    }
    std::fs::write(out, &file).unwrap_or_else(|e| die(e.to_string()));
    println!("{out}: {n} fragments, {} bytes", file.len());
}

/// `probe`: pull ~3 s from each configured channel and count what arrives (no secrets printed).
fn probe(ch: &stream::Channels) {
    for (name, url) in [("high", &ch.high), ("medium", &ch.medium), ("low", &ch.low)] {
        let Some(url) = url else { continue };
        let r = (|| -> Result<String, String> {
            let (mut c, tracks) = rtsp::Client::connect(url)?;
            let codecs: Vec<String> = tracks.iter().map(|t| format!("{}/{}", t.codec, t.clock)).collect();
            let v = tracks.iter().find(|t| t.codec == "h264").ok_or("no h264")?;
            c.setup(v, 0)?;
            c.play()?;
            let mut d = h264::Depacketizer::default();
            let (mut nals, mut types, mut max) = (Vec::new(), [0u32; 32], 0usize);
            let t0 = std::time::Instant::now();
            while t0.elapsed().as_secs() < 3 {
                let (chn, p) = c.next()?;
                if chn == 0 {
                    max = max.max(p.len());
                    d.push(&p, &mut nals);
                }
            }
            for n in nals.drain(..) {
                types[(n.data[0] & 0x1f) as usize] += 1;
            }
            Ok(format!("tracks {codecs:?}, sprop {}, max pkt {max}, NALs: idr {} non-idr {} sps {} pps {}", v.params.len(), types[5], types[1], types[7], types[8]))
        })();
        println!("{name}: {}", r.unwrap_or_else(|e| format!("error: {e}")));
    }
}
