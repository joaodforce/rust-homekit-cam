//! rust-homekit-cam: an RTSP camera / doorbell (UniFi Protect or any H.264 camera) as a native
//! HomeKit accessory.
//!
//! `rust-homekit-cam serve <config.toml>` | `code <config.toml>` | `reset <config.toml>`

mod camera;
mod h264;
mod hap;
mod hds;
mod hksv;
mod mdns;
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
mod transcode;

use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

pub fn log(msg: String) {
    eprintln!("homekit-cam: {msg}");
}

/// An ONVIF camera: where to find it and how to log in. It can supply the RTSP URLs (when
/// [rtsp] is not set), the snapshot URL (when snapshot_url is not set) and motion/ring events.
#[derive(Deserialize, Clone)]
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
    /// HomeKit Secure Video (on by default). The Motion Sensor, its trigger, is always there; fed by
    /// Protect, ONVIF events or `POST /motion`. (An old `motion = …` setting is ignored.)
    hksv: Option<bool>,
    /// seconds the sensor stays on after the last motion
    #[serde(default = "motion_hold")]
    motion_hold: u64,
    /// `POST /ring`, `POST /motion` listener, e.g. "0.0.0.0:8780"; needs `ring_token`.
    ring_listen: Option<String>,
    #[serde(default)]
    ring_token: String,
    /// POST each Motion Sensor change and ring as JSON to this URL, or each of a list of URLs
    /// (plain http://), e.g. a Home Assistant webhook:
    /// `{"camera", "event": "motion"|"ring", "state": "start"|"end"|"pressed", "source"}`
    #[serde(default)]
    motion_webhook: OneOrMore,
    /// Send a small JPEG (base64, `"snapshot"`) with each motion start: `{ width = 320, quality = 60 }`
    motion_webhook_snapshot: Option<events::WebhookSnapshot>,
    /// A JPEG snapshot URL (http://[user:pass@]host/path); default with [protect]: Protect's.
    snapshot_url: Option<String>,
    /// Grab snapshots from the RTSP stream (sub stream first) with this ffmpeg ("ffmpeg" =
    /// from PATH). Used when there is no snapshot_url; wins over the [protect]/[onvif] snapshots.
    snapshot_ffmpeg: Option<String>,
    /// Shrink snapshots when the controller asks for a much smaller image (Apple Watch: ~320x240
    /// through a home hub). Decoded at reduced scale and re-encoded: a few ms per snapshot.
    #[serde(default)]
    snapshot_resize: bool,
    /// Camera audio → what HomeKit takes: G.711/AAC-LC → Opus for live view, G.711/Opus → AAC-LC
    /// for recordings. Off by default: without it those get video only. Video is never transcoded.
    #[serde(default)]
    transcode_audio: bool,
    /// Optional ONVIF backend
    onvif: Option<OnvifCfg>,
    /// Optional UniFi Protect backend (two-way audio, snapshots).
    protect: Option<ProtectCfg>,
    /// Protect RTSP URLs (rtsp://<nvr>:7447/<alias>) per channel, for live view.
    #[serde(default)]
    rtsp: stream::Channels,
}

/// A setting that takes one value or a list.
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(untagged)]
enum OneOrMore {
    #[default]
    None,
    One(String),
    More(Vec<String>),
}

impl OneOrMore {
    fn list(&self) -> Vec<String> {
        match self {
            OneOrMore::None => vec![],
            OneOrMore::One(s) => vec![s.clone()],
            OneOrMore::More(v) => v.clone(),
        }
    }
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
    cfg.rtsp.transcode_audio = cfg.transcode_audio;
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
    if matches!(args.get(1).map(String::as_str), Some("--version" | "-V" | "version")) {
        return println!("rust-homekit-cam {}", env!("CARGO_PKG_VERSION"));
    }
    if args.get(1).map(String::as_str) == Some("onvif-discover") {
        for (url, scopes) in onvif::discover(std::time::Duration::from_secs(3)) {
            let pick = |k: &str| scopes.split_whitespace().find_map(|s| s.split_once(&format!("/{k}/")).map(|(_, v)| v.replace("%20", " "))).unwrap_or_default();
            println!("{url}  {} {}", pick("hardware"), pick("name"));
        }
        return;
    }
    let (cmd, path) = match args.as_slice() {
        [_, c, p] | [_, c, p, _] => (c.as_str(), p.as_str()),
        _ => die("usage: rust-homekit-cam serve|code|reset|probe|audio-test|snapshot|record-test|protect-cameras|onvif-info <config.toml>, onvif-discover, or --version".into()),
    };
    let mut cfg = load(path);
    match cmd {
        "code" => {
            let (code, paired) = hap::code(cfg.state_dir.clone().unwrap());
            println!("{code} ({})", if paired { "paired" } else { "not paired" });
        }
        "probe" => {
            if let Some(oc) = &cfg.onvif {
                try_onvif(oc, &mut cfg.rtsp).unwrap_or_else(|e| die(e));
            }
            probe(&cfg.rtsp)
        }
        "audio-test" => {
            if let Some(oc) = &cfg.onvif {
                try_onvif(oc, &mut cfg.rtsp).unwrap_or_else(|e| die(e));
            }
            for (name, url) in [("medium", &cfg.rtsp.medium), ("low", &cfg.rtsp.low), ("high", &cfg.rtsp.high)] {
                if let Some(u) = url {
                    for (rate, ptime) in [(16000, 20), (24000, 60)] {
                        match transcode::selftest(u, rate, ptime, 6) {
                            Ok(s) => println!("{name} → Opus {rate} Hz {ptime} ms: {s}"),
                            Err(e) => println!("{name} → Opus {rate} Hz {ptime} ms: error: {e}"),
                        }
                    }
                }
            }
        }
        "onvif-info" => onvif_info(&cfg),
        "snapshot" => snapshot_test(&cfg, args.get(3).map(String::as_str).unwrap_or("snapshot.jpg")),
        "protect-cameras" => protect_cameras(&cfg),
        "record-test" => record_test(&cfg.rtsp, args.get(3).map(String::as_str).unwrap_or("record-test.mp4")),
        "reset" =>println!("removed {} pairing(s); restart the service", hap::reset(cfg.state_dir.clone().unwrap())),
        "serve" => {
            // With [rtsp] URLs, ONVIF is extra (events, maybe snapshots): one try now, and on failure
            // keep serving and retry in the background. Without them it supplies the streams: wait.
            let has_rtsp = cfg.rtsp.high.is_some() || cfg.rtsp.medium.is_some() || cfg.rtsp.low.is_some();
            let onvif = match &cfg.onvif {
                Some(oc) if has_rtsp => try_onvif(oc, &mut cfg.rtsp).map_err(|e| log(onvif_failure(&e, true))).ok(),
                Some(oc) => Some(connect_onvif(oc, &mut cfg.rtsp)),
                None => None,
            };
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
                (None, _, _) if cfg.snapshot_ffmpeg.is_some() => Some(ffmpeg_source(cfg.snapshot_ffmpeg.as_deref(), &cfg.rtsp)),
                (None, Some(p), _) => Some(snapshot::Source::Protect(p.clone())),
                (None, None, Some(u)) => Some(snapshot::Source::Url(u)),
                _ => None,
            }
            .map(|s| Arc::new(snapshot::Snapshots::new(s).with_resize(cfg.snapshot_resize)));
            if snapshots.is_none() {
                log("no snapshot source (snapshot_url, snapshot_ffmpeg, [protect] or [onvif]): blank tiles".into());
            }
            let motion_state = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
            if protect.is_none() && onvif.is_none() && cfg.ring_listen.is_none() {
                log("no motion source ([protect], [onvif] or ring_listen for POST /motion): the Motion Sensor never fires, so HKSV won't record".into());
            }
            let hksv = cfg.hksv.unwrap_or(true)
                .then(|| cfg.rtsp.pick(2000).map(|(_, u)| u.to_string()))
                .flatten()
                .map(|url| hksv::Hksv::new(cfg.doorbell, cfg.state_dir.clone().unwrap(), url, cfg.transcode_audio).unwrap_or_else(|e| die(e)));
            let cam = Arc::new(
                camera::Camera::new(&cfg.name, cfg.doorbell, cfg.rtsp.clone(), protect.clone().filter(|_| cfg.talkback.unwrap_or(true)))
                    .with_info(cfg.manufacturer.clone(), cfg.model.clone())
                    .with_size(video_size(&cfg.rtsp))
                    .with_snapshots(snapshots.clone())
                    .with_motion(motion_state.clone())
                    .with_hksv(hksv.clone()),
            );
            let server = hap::Server::start(cam, cfg.hap_port, cfg.state_dir.clone().unwrap()).unwrap_or_else(|e| die(e));
            let motion = motion_state.map(|st| events::Motion::new(camera::MOTION_DETECTED, st, std::time::Duration::from_secs(cfg.motion_hold)));
            let webhooks: Vec<events::Webhook> = cfg.motion_webhook.list().into_iter().map(|url| events::Webhook { url, camera: cfg.name.clone() }).collect();
            for w in &webhooks {
                if !w.url.starts_with("http://") {
                    die("motion_webhook: only plain http:// URLs are supported".into());
                }
                log(format!("events: motion and rings also go to {}", w.shown()));
            }
            let ev = Arc::new(events::Events::new(server.clone(), cfg.doorbell.then_some(camera::SWITCH_EVENT), snapshots.clone(), motion).with_webhooks(webhooks, cfg.motion_webhook_snapshot));
            if let (Some(p), Some(pc)) = (&protect, &cfg.protect) {
                follow_protect(p.clone(), pc.motion, ev.clone());
            }
            if let Some(oc) = &cfg.onvif {
                let (motion_keys, ring_keys, ev) = (oc.motion_topics.clone(), oc.ring_topics.clone(), ev.clone());
                let on_event = move |topic: &str, _name: &str, value: &str| {
                    if onvif::motion_value(topic, value, &ring_keys) == Some(true) {
                        ev.ring("onvif");
                    } else if let Some(on) = onvif::motion_value(topic, value, &motion_keys) {
                        if on { ev.motion_start("onvif") } else { ev.motion_end("onvif") }
                    }
                };
                match &onvif {
                    Some((o, _)) => o.clone().follow(on_event),
                    None => {
                        // the first try failed: keep retrying (gently, see onvif_failure) for events
                        let (oc, mut ch) = (oc.clone(), cfg.rtsp.clone());
                        std::thread::spawn(move || connect_onvif(&oc, &mut ch).0.follow(on_event));
                    }
                }
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
/// A rejected login (wrong password, or an ONVIF user apart from the web login): retrying fast
/// would lock the account (Intelbras: 30 attempts), so these wait much longer.
fn onvif_auth_error(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    ["not authorized", "notauthorized", "http 401", "invalid username or password", "unauthorized"].iter().any(|k| l.contains(k))
}

/// The log line for a failed ONVIF connection, and how long to wait before the next try.
fn onvif_failure(e: &str, background: bool) -> String {
    let wait = if onvif_auth_error(e) { 600 } else { 10 };
    let hint = if onvif_auth_error(e) {
        ": the camera rejected the ONVIF login (check user/password; some cameras keep ONVIF users apart from the web login); retrying slowly so the account isn't locked"
    } else {
        ""
    };
    let mode = if background { ", serving the [rtsp] streams meanwhile" } else { "" };
    format!("onvif: {e}{hint}{mode} (next try in {wait} s)")
}

/// Connect, retrying for ever (slowly after a rejected login).
fn connect_onvif(oc: &OnvifCfg, rtsp: &mut stream::Channels) -> (Arc<onvif::Onvif>, Option<String>) {
    loop {
        match try_onvif(oc, rtsp) {
            Ok(r) => return r,
            Err(e) => {
                log(onvif_failure(&e, false));
                std::thread::sleep(std::time::Duration::from_secs(if onvif_auth_error(&e) { 600 } else { 10 }));
            }
        }
    }
}

/// One connection attempt: the camera's services and profiles; streams into `rtsp` when it has
/// none; the snapshot URL (when `snapshot` is on).
fn try_onvif(oc: &OnvifCfg, rtsp: &mut stream::Channels) -> Result<(Arc<onvif::Onvif>, Option<String>), String> {
    let o = onvif::Onvif::connect(&oc.url, &oc.user, &oc.password)?;
    let mut profiles: Vec<onvif::Profile> = o.profiles()?.into_iter().filter(|p| p.encoding.eq_ignore_ascii_case("H264")).collect();
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
    Ok((Arc::new(o), snapshot))
}

/// Stream snapshots through ffmpeg: the smallest stream decodes fastest.
fn ffmpeg_source(bin: Option<&str>, r: &stream::Channels) -> snapshot::Source {
    let bin = bin.unwrap_or("ffmpeg").to_string();
    let url = r.low.clone().or_else(|| r.medium.clone()).or_else(|| r.high.clone()).unwrap_or_else(|| die("snapshot_ffmpeg needs an RTSP stream".into()));
    snapshot::Source::Ffmpeg(bin, url)
}

/// `snapshot <config> <out.jpg>`: fetch the picture the Home app would get.
fn snapshot_test(cfg: &Config, out: &str) {
    let source = if let Some(u) = &cfg.snapshot_url {
        snapshot::Source::Url(u.clone())
    } else if cfg.snapshot_ffmpeg.is_some() {
        let mut ch = cfg.rtsp.clone();
        if let Some(oc) = &cfg.onvif {
            try_onvif(oc, &mut ch).unwrap_or_else(|e| die(e));
        }
        ffmpeg_source(cfg.snapshot_ffmpeg.as_deref(), &ch)
    } else if let Some(p) = &cfg.protect {
        let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host?".into()));
        snapshot::Source::Protect(Arc::new(protect::Protect::new(&host, &p.token, &p.camera_id, p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e))))
    } else if let Some(oc) = &cfg.onvif {
        let mut ch = cfg.rtsp.clone();
        match try_onvif(oc, &mut ch).unwrap_or_else(|e| die(e)).1 {
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
        match o.snapshot_uri(&p.token) {
            Ok(u) => println!("  snapshot: {}", mask(&u)),
            Err(e) => println!("  snapshot: none ({e})"),
        }
    }
    println!("events for 30 s (move in front of the camera):");
    let o = Arc::new(o);
    o.follow(|topic, name, value| println!("  {topic}  {name}={value}"));
    std::thread::sleep(std::time::Duration::from_secs(30));
}

/// The video size from a stream's SPS (DESCRIBE only), to offer resolutions in the right
/// orientation. None if no stream answers.
fn video_size(ch: &stream::Channels) -> Option<(u16, u16)> {
    let url = ch.medium.as_ref().or(ch.high.as_ref()).or(ch.low.as_ref())?;
    let size = rtsp::Client::connect(url).ok().and_then(|(_, tracks)| {
        let v = tracks.iter().find(|t| t.codec == "h264")?;
        v.params.iter().find(|p| p.first().map(|b| b & 0x1f) == Some(7)).and_then(|sps| mp4::sps_size(sps))
    });
    match size {
        Some((w, h)) => log(format!("video: {w}x{h} ({}): offering {} sizes", if h > w { "portrait" } else { "landscape" }, if h > w { "portrait" } else { "landscape" })),
        None => log("video: size unknown (stream not reachable now): offering landscape and portrait sizes".into()),
    }
    size
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
            Ok(_) => println!("# RTSP is off for this camera: Protect → camera → Settings → Advanced → RTSP"),
            Err(e) => println!("# couldn't read the RTSP streams: {e}"),
        }
        println!("[protect]\ntoken = \"…\"\ncamera_id = \"{id}\"\nhost = \"{host}\"\n");
    }
}

/// `record-test <config> <out.mp4>`: what HKSV would get: 4 s prebuffer + ~8 s live, as fMP4.
fn record_test(ch: &stream::Channels, out: &str) {
    let url = ch.pick(2000).map(|(_, u)| u.to_string()).unwrap_or_else(|| die("no RTSP URL".into()));
    let r = recording::Recorder::new(url, ch.transcode_audio);
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
/// What `probe` learned about one stream, for the warnings after the per-stream lines.
struct Probed {
    name: &'static str,
    kbps: usize,
    /// most slices seen in one picture
    slices: u32,
    audio: Vec<String>,
}

fn probe(ch: &stream::Channels) {
    let mut found = Vec::new();
    for (name, url) in [("high", &ch.high), ("medium", &ch.medium), ("low", &ch.low)] {
        let Some(url) = url else { continue };
        let r = (|| -> Result<(String, Probed), String> {
            let t_connect = std::time::Instant::now();
            let (mut c, tracks) = rtsp::Client::connect(url)?;
            let codecs: Vec<String> = tracks.iter().map(|t| format!("{}/{}", t.codec, t.clock)).collect();
            let v = tracks.iter().find(|t| t.codec == "h264").ok_or_else(|| format!("no H.264 track ({}): switch the camera to H.264", codecs.join(", ")))?;
            c.setup(v, 0)?;
            let a = tracks.iter().find(|t| t.codec == "opus");
            if let Some(a) = a {
                c.setup(a, 2)?;
            }
            c.play()?;
            let mut d = h264::Depacketizer::default();
            let (mut nals, mut types, mut max) = (Vec::new(), [0u32; 32], 0usize);
            let (mut vj, mut aj) = (stream::Jitter::default(), stream::Jitter::default());
            let mut bytes = 0usize;
            let mut first_idr: Option<std::time::Duration> = None;
            let (mut slices, mut max_slices) = (0u32, 0u32);
            let mut size = String::new();
            if let Some(sps) = v.params.iter().find(|p| p.first().map(|b| b & 0x1f) == Some(7)) {
                if let Some((w, h)) = mp4::sps_size(sps) {
                    size = format!("{w}x{h}, ");
                }
            }
            let t0 = std::time::Instant::now();
            while t0.elapsed().as_secs() < 5 {
                let (chn, p) = c.next()?;
                if chn == 0 {
                    max = max.max(p.len());
                    bytes += p.len();
                    vj.push(&p, v.clock);
                    d.push(&p, &mut nals);
                    if first_idr.is_none() && nals.iter().any(|n| n.data[0] & 0x1f == 5) {
                        first_idr = Some(t_connect.elapsed());
                    }
                    for n in nals.drain(..) {
                        let ty = n.data[0] & 0x1f;
                        types[ty as usize] += 1;
                        if ty == 1 || ty == 5 {
                            slices += 1;
                        }
                        if n.last {
                            max_slices = max_slices.max(slices);
                            slices = 0;
                        }
                    }
                } else if chn == 2 {
                    aj.push(&p, a.map(|a| a.clock).unwrap_or(48_000));
                }
            }
            let audio = if a.is_some() { format!(", audio {:.0} ms (worst {:.0})", aj.j, aj.worst) } else { String::new() };
            let kbps = bytes * 8 / 5000;
            let line = format!(
                "{size}first keyframe after {} ms, tracks {codecs:?}, video {kbps} kbps, slices per picture up to {max_slices}, sprop {}, max pkt {max}, NALs: idr {} non-idr {} sps {} pps {}; arrival jitter: video {:.0} ms (worst {:.0}){audio}",
                first_idr.map(|d| d.as_millis()).unwrap_or(0), v.params.len(), types[5], types[1], types[7], types[8], vj.j, vj.worst
            );
            let audio = tracks.iter().filter(|t| t.codec != "h264" && t.codec != "h265").map(|t| t.codec.clone()).collect();
            Ok((line, Probed { name, kbps, slices: max_slices, audio }))
        })();
        match r {
            Ok((line, p)) => {
                println!("{name}: {line}");
                found.push(p);
            }
            Err(e) => println!("{name}: error: {e}"),
        }
    }
    for w in probe_warnings(&found, ch.transcode_audio, ch.pick(2000).map(|(n, _)| n), ch.pick(132).map(|(n, _)| n)) {
        println!("warning: {w}");
    }
}

/// Things that work but will disappoint: HomeKit limits learned the hard way (see docs).
fn probe_warnings(found: &[Probed], transcode: bool, rec: Option<&str>, remote: Option<&str>) -> Vec<String> {
    let mut w = Vec::new();
    let get = |n: Option<&str>| found.iter().find(|p| Some(p.name) == n);
    if let Some(p) = get(remote) {
        if p.kbps > 200 {
            w.push(format!(
                "remote viewers (via a home hub, which asks for ≤132 kbps) get the {} stream at {} kbps: expect freezes; set a sub stream to ~128 kbps as `low`",
                p.name, p.kbps
            ));
        }
    }
    for p in found {
        if p.slices > 2 {
            w.push(format!("{}: pictures come in up to {} slices; HomeKit may not decode them (one slice per frame is safest: check the camera's encoder settings)", p.name, p.slices));
        }
    }
    let has = |p: &Probed, c: &[&str]| p.audio.iter().any(|a| c.contains(&a.as_str()));
    for p in found {
        if !transcode && !has(p, &["opus"]) && has(p, &["pcmu", "pcma", "mpeg4-generic"]) {
            w.push(format!("{}: live view has no sound (audio {:?}, HomeKit plays Opus): set transcode_audio = true", p.name, p.audio));
        }
    }
    if let Some(p) = get(rec) {
        let aac = has(p, &["mpeg4-generic"]);
        if !aac && !transcode && has(p, &["pcmu", "pcma", "opus"]) {
            w.push(format!("recordings use the {} stream: its audio {:?} isn't AAC, so they'll be silent: set transcode_audio = true, or AAC in the camera", p.name, p.audio));
        }
        if !aac && has(p, &["pcmu", "pcma", "opus"]) {
            if let Some(o) = found.iter().find(|o| o.name != p.name && has(o, &["mpeg4-generic"])) {
                w.push(format!("the {} stream has AAC but recordings use {}: set AAC on {} too, and recordings copy it instead of encoding", o.name, p.name, p.name));
            }
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &'static str, kbps: usize, slices: u32, audio: &[&str]) -> Probed {
        Probed { name, kbps, slices, audio: audio.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn motion_webhook_takes_one_url_or_a_list() {
        #[derive(Deserialize)]
        struct C {
            #[serde(default)]
            motion_webhook: OneOrMore,
        }
        let p = |s: &str| toml::from_str::<C>(s).unwrap().motion_webhook.list();
        assert!(p("").is_empty());
        assert_eq!(p(r#"motion_webhook = "http://a""#), vec!["http://a"]);
        assert_eq!(p(r#"motion_webhook = ["http://a", "http://b"]"#), vec!["http://a", "http://b"]);
    }

    #[test]
    fn onvif_login_failures_retry_slowly() {
        let intelbras = "ONVIF: HTTP 400 Sender not Authorized. Invalid username or password! You still have 29 attempt(s).";
        assert!(onvif_auth_error(intelbras));
        assert!(onvif_failure(intelbras, true).contains("next try in 600 s"));
        assert!(onvif_auth_error("ONVIF fault: ter:NotAuthorized"));
        assert!(!onvif_auth_error("192.168.10.40:80: Connection refused"));
        assert!(onvif_failure("192.168.10.40:80: Connection refused", false).ends_with("(next try in 10 s)"));
    }

    #[test]
    fn probe_warns_about_what_homekit_dislikes() {
        // a typical cheap camera: main 2 Mbps PCMA, sub 512 kbps in 6 slices
        let found = [p("medium", 2000, 1, &["pcma"]), p("low", 512, 6, &["pcma"])];
        let w = probe_warnings(&found, false, Some("medium"), Some("low"));
        assert!(w.iter().any(|x| x.contains("remote viewers") && x.contains("512 kbps")));
        assert!(w.iter().any(|x| x.starts_with("low: pictures come in up to 6 slices")));
        assert_eq!(w.iter().filter(|x| x.contains("live view has no sound")).count(), 2);
        assert!(w.iter().any(|x| x.contains("recordings use the medium stream")));
        // fixed up: sub at 128 kbps, one slice, AAC on the sub only, transcoding on
        let found = [p("medium", 2000, 1, &["pcma"]), p("low", 128, 1, &["mpeg4-generic"])];
        let w = probe_warnings(&found, true, Some("medium"), Some("low"));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("the low stream has AAC but recordings use medium"));
        // Protect-like: Opus + AAC, small low stream
        let found = [p("medium", 500, 1, &["opus", "mpeg4-generic"]), p("low", 120, 1, &["opus", "mpeg4-generic"])];
        assert!(probe_warnings(&found, false, Some("medium"), Some("low")).is_empty());
    }
}
