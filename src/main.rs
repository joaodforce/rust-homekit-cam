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
mod onvif;
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

/// An ONVIF camera: where to find it and how to log in. It can supply the RTSP URLs (when
/// [rtsp] is not set), the snapshot URL (when snapshot_url is not set) and motion/ring events.
#[derive(Deserialize)]
struct OnvifCfg {
    /// http://<camera>[:port]
    url: String,
    user: String,
    password: String,
    /// events whose topic contains one of these turn the Motion Sensor on/off
    #[serde(default = "motion_topics")]
    motion_topics: Vec<String>,
    /// events whose topic contains one of these (with a true value) ring the doorbell
    #[serde(default)]
    ring_topics: Vec<String>,
    /// use the camera's ONVIF snapshot URI (false when it is broken: set snapshot_url instead)
    #[serde(default = "yes")]
    snapshot: bool,
}

fn motion_topics() -> Vec<String> {
    vec!["Motion".into()]
}

/// Protect integration API: API key, camera id; the console's address (default: the RTSP
/// host) and its certificate's SHA-256 fingerprint ("AB:CD:…").
#[derive(Deserialize)]
struct ProtectCfg {
    /// which events turn the Motion Sensor on: "smart" (person, vehicle…) or "any"
    #[serde(default = "smart")]
    motion: protect::MotionKind,
    token: String,
    /// `rust-homekit-cam protect-cameras` lists them
    #[serde(default)]
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
    /// Pairing and recording state. Default: systemd's $STATE_DIRECTORY when run as a service,
    /// else /var/lib/rust-homekit-cam/<config file name> (so `code` / `reset` find it too).
    state_dir: Option<PathBuf>,
    /// Two-way audio; `false` turns it off (the Speaker stays, nothing is sent to the camera)
    talkback: Option<bool>,
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
    /// Optional ONVIF backend
    onvif: Option<OnvifCfg>,
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

fn load(path: &str) -> Config {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("{path}: {e}")));
    let mut cfg: Config = toml::from_str(&text).unwrap_or_else(|e| die(format!("{path}: {e}")));
    if cfg.state_dir.is_none() {
        let instance = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("default").to_string();
        cfg.state_dir = Some(match std::env::var_os("STATE_DIRECTORY") {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from("/var/lib/rust-homekit-cam").join(instance),
        });
    }
    cfg
}

fn die(msg: String) -> ! {
    log(msg);
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("onvif-discover") {
        for (url, scopes) in onvif::discover(std::time::Duration::from_secs(3)) {
            let pick = |k: &str| scopes.split_whitespace().find_map(|s| s.split_once(&format!("/{k}/")).map(|(_, v)| v.replace("%20", " "))).unwrap_or_default();
            println!("{url}  {} {}", pick("hardware"), pick("name"));
        }
        return;
    }
    let (cmd, path) = match args.as_slice() {
        [_, c, p] | [_, c, p, _] => (c.as_str(), p.as_str()),
        _ => die("usage: rust-homekit-cam serve|code|reset|probe|record-test|protect-cameras|onvif-info <config.toml>, or onvif-discover".into()),
    };
    let mut cfg = load(path);
    match cmd {
        "code" => {
            let (code, paired) = hap::code(cfg.state_dir.clone().unwrap());
            println!("{code} ({})", if paired { "paired" } else { "not paired" });
        }
        "probe" => {
            if let Some(oc) = &cfg.onvif {
                connect_onvif(oc, &mut cfg.rtsp);
            }
            probe(&cfg.rtsp)
        }
        "onvif-info" => onvif_info(&cfg),
        "snapshot" => snapshot_test(&cfg, args.get(3).map(String::as_str).unwrap_or("snapshot.jpg")),
        "protect-cameras" => protect_cameras(&cfg),
        "record-test" => record_test(&cfg.rtsp, args.get(3).map(String::as_str).unwrap_or("record-test.mp4")),
        "reset" =>println!("removed {} pairing(s); restart the service", hap::reset(cfg.state_dir.clone().unwrap())),
        "serve" => {
            let onvif = cfg.onvif.as_ref().map(|o| connect_onvif(o, &mut cfg.rtsp));
            if cfg.protect.as_ref().is_some_and(|p| p.camera_id.is_empty()) {
                die("[protect] camera_id is missing (rust-homekit-cam protect-cameras <config> lists them)".into());
            }
            let protect = cfg.protect.as_ref().map(|p| {
                let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host?".into()));
                Arc::new(protect::Protect::new(&host, &p.token, &p.camera_id, p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e)))
            });
            if protect.is_none() {
                log("no [protect]: two-way audio off".into());
            } else if cfg.talkback == Some(false) {
                log("talkback = false: two-way audio off".into());
            }
            let onvif_snapshot = onvif.as_ref().and_then(|(_, snap)| snap.clone());
            let snapshots = match (&cfg.snapshot_url, &protect, onvif_snapshot) {
                (Some(u), _, _) => Some(snapshot::Source::Url(u.clone())),
                (None, Some(p), _) => Some(snapshot::Source::Protect(p.clone())),
                (None, None, Some(u)) => Some(snapshot::Source::Url(u)),
                _ => None,
            }
            .map(|s| Arc::new(snapshot::Snapshots::new(s)));
            if snapshots.is_none() {
                log("no snapshot_url or [protect]: no snapshots (blank tiles)".into());
            }
            let motion_state = cfg.motion.unwrap_or(protect.is_some() || onvif.is_some()).then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let hksv = (cfg.hksv.unwrap_or(motion_state.is_some()) && motion_state.is_some())
                .then(|| cfg.rtsp.pick(2000).map(|(_, u)| u.to_string()))
                .flatten()
                .map(|url| hksv::Hksv::new(cfg.doorbell, cfg.state_dir.clone().unwrap(), url).unwrap_or_else(|e| die(e)));
            let cam = Arc::new(
                camera::Camera::new(&cfg.name, cfg.doorbell, cfg.rtsp.clone(), protect.clone().filter(|_| cfg.talkback.unwrap_or(true)))
                    .with_info(cfg.manufacturer.clone(), cfg.model.clone())
                    .with_snapshots(snapshots.clone())
                    .with_motion(motion_state.clone())
                    .with_hksv(hksv.clone()),
            );
            let server = hap::Server::start(cam, cfg.hap_port, cfg.state_dir.clone().unwrap()).unwrap_or_else(|e| die(e));
            let motion = motion_state.map(|st| events::Motion::new(camera::MOTION_DETECTED, st, std::time::Duration::from_secs(cfg.motion_hold)));
            let ev = Arc::new(events::Events::new(server.clone(), cfg.doorbell.then_some(camera::SWITCH_EVENT), snapshots.clone(), motion));
            if let (Some(p), Some(pc)) = (&protect, &cfg.protect) {
                follow_protect(p.clone(), pc.motion, ev.clone());
            }
            if let (Some((o, _)), Some(oc)) = (&onvif, &cfg.onvif) {
                let (motion_keys, ring_keys, ev) = (oc.motion_topics.clone(), oc.ring_topics.clone(), ev.clone());
                o.clone().follow(move |topic, _name, value| {
                    if onvif::motion_value(topic, value, &ring_keys) == Some(true) {
                        ev.ring("onvif");
                    } else if let Some(on) = onvif::motion_value(topic, value, &motion_keys) {
                        if on { ev.motion_start("onvif") } else { ev.motion_end("onvif") }
                    }
                });
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

/// Log in to the ONVIF camera (retrying until it answers), fill in [rtsp] from its H.264
/// profiles when not set, and find its snapshot URL.
fn connect_onvif(oc: &OnvifCfg, rtsp: &mut stream::Channels) -> (Arc<onvif::Onvif>, Option<String>) {
    let o = loop {
        match onvif::Onvif::connect(&oc.url, &oc.user, &oc.password) {
            Ok(o) => break o,
            Err(e) => {
                log(format!("onvif: {e} (retrying in 10 s)"));
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        }
    };
    let mut profiles: Vec<onvif::Profile> = o.profiles().unwrap_or_else(|e| die(e)).into_iter().filter(|p| p.encoding.eq_ignore_ascii_case("H264")).collect();
    profiles.sort_by_key(|p| std::cmp::Reverse(p.width * p.height));
    if rtsp.high.is_none() && rtsp.medium.is_none() && rtsp.low.is_none() {
        if profiles.is_empty() {
            die("onvif: no H.264 profile (switch the camera's streams to H.264)".into());
        }
        let uri = |p: &onvif::Profile| o.stream_uri(&p.token).map(|u| onvif::with_credentials(&u, &oc.user, &oc.password)).unwrap_or_else(|e| die(e));
        rtsp.high = Some(uri(&profiles[0]));
        if profiles.len() > 1 {
            rtsp.low = Some(uri(profiles.last().unwrap()));
        }
        rtsp.medium = Some(if profiles.len() > 2 { uri(&profiles[1]) } else { rtsp.high.clone().unwrap() });
        log(format!("onvif: streams from profiles {}", profiles.iter().map(|p| format!("{} {}x{}", p.name, p.width, p.height)).collect::<Vec<_>>().join(", ")));
    }
    let snapshot = if oc.snapshot {
        profiles.first().and_then(|p| o.snapshot_uri(&p.token).ok()).map(|u| onvif::with_credentials(&u, &oc.user, &oc.password))
    } else {
        None
    };
    (Arc::new(o), snapshot)
}

/// `snapshot <config> <out.jpg>`: fetch the picture the Home app would get.
fn snapshot_test(cfg: &Config, out: &str) {
    let source = if let Some(u) = &cfg.snapshot_url {
        snapshot::Source::Url(u.clone())
    } else if let Some(p) = &cfg.protect {
        let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host?".into()));
        snapshot::Source::Protect(Arc::new(protect::Protect::new(&host, &p.token, &p.camera_id, p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e))))
    } else if let Some(oc) = &cfg.onvif {
        let mut ch = cfg.rtsp.clone();
        match connect_onvif(oc, &mut ch).1 {
            Some(u) => snapshot::Source::Url(u),
            None => die("the camera has no ONVIF snapshot URI: set snapshot_url".into()),
        }
    } else {
        die("no snapshot source: set snapshot_url".into())
    };
    let img = snapshot::Snapshots::new(source).get().unwrap_or_else(|e| die(e));
    std::fs::write(out, img.as_slice()).unwrap_or_else(|e| die(e.to_string()));
    println!("{out}: {} bytes", img.len());
}

/// `onvif-info <config>`: what the [onvif] camera offers, and its events for 30 s.
fn onvif_info(cfg: &Config) {
    let oc = cfg.onvif.as_ref().unwrap_or_else(|| die("needs an [onvif] section (url, user, password)".into()));
    let o = onvif::Onvif::connect(&oc.url, &oc.user, &oc.password).unwrap_or_else(|e| die(e));
    let (make, model) = o.device_info().unwrap_or_default();
    println!("device: {make} {model}");
    let mask = |u: &str| match u.split_once("://") {
        Some((s, rest)) => format!("{s}://{}", rest.split_once('@').map(|(_, h)| h).unwrap_or(rest)),
        None => u.to_string(),
    };
    for p in o.profiles().unwrap_or_else(|e| die(e)) {
        println!("profile {} \"{}\": {} {}x{} {} kbps", p.token, p.name, p.encoding, p.width, p.height, p.bitrate_kbps);
        if let Ok(u) = o.stream_uri(&p.token) {
            println!("  rtsp:     {}", mask(&u));
        }
        if let Ok(u) = o.snapshot_uri(&p.token) {
            println!("  snapshot: {}", mask(&u));
        }
    }
    println!("events for 30 s (move in front of the camera):");
    let o = Arc::new(o);
    o.follow(|topic, name, value| println!("  {topic}  {name}={value}"));
    std::thread::sleep(std::time::Duration::from_secs(30));
}

/// The console's host, from any RTSP URL.
fn rtsp_host(ch: &stream::Channels) -> Option<String> {
    let url = ch.low.as_ref().or(ch.medium.as_ref()).or(ch.high.as_ref())?;
    Some(url.strip_prefix("rtsp://")?.split(['/', ':']).next()?.to_string())
}

/// `protect-cameras <config>`: every Protect camera with a config block to paste (needs only
/// [protect] token, and host when there is no [rtsp] yet). Prints RTSP aliases: they are secrets.
fn protect_cameras(cfg: &Config) {
    let p = cfg.protect.as_ref().unwrap_or_else(|| die("needs a [protect] section with token (and host)".into()));
    let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host = \"<console address>\" is needed".into()));
    let api = protect::Protect::new(&host, &p.token, "", p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e));
    for (id, name, speaker) in api.cameras().unwrap_or_else(|e| die(e)) {
        println!("# ---- {name}{}", if speaker { " (has a speaker: two-way audio)" } else { "" });
        println!("name = \"{name}\"\ndoorbell = false   # true for a doorbell");
        println!("[rtsp]");
        match api.rtsp_urls(&id) {
            Ok(urls) if !urls.is_empty() => urls.iter().for_each(|(ch, u)| println!("{ch} = \"{u}\"")),
            _ => println!("# RTSP is off for this camera: Protect → camera → Settings → Advanced → RTSP"),
        }
        println!("[protect]\ntoken = \"…\"\ncamera_id = \"{id}\"\nhost = \"{host}\"\n");
    }
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
