//! rust-homekit-cam: an RTSP camera / doorbell (UniFi Protect or any H.264 camera) as a native
//! HomeKit accessory.
//!
//! `rust-homekit-cam serve <config.toml>` | `code <config.toml>` | `reset <config.toml>`

mod camera;
mod h264;
mod hap;
mod protect;
mod ring;
mod rtsp;
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
    /// `POST /ring` listener, e.g. "0.0.0.0:8780"; needs `ring_token`.
    ring_listen: Option<String>,
    #[serde(default)]
    ring_token: String,
    /// Optional UniFi Protect backend (two-way audio).
    protect: Option<ProtectCfg>,
    /// Protect RTSP URLs (rtsp://<nvr>:7447/<alias>) per channel, for live view.
    #[serde(default)]
    rtsp: stream::Channels,
}

fn yes() -> bool {
    true
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
        [_, c, p] => (c.as_str(), p.as_str()),
        _ => die("usage: rust-homekit-cam serve|code|reset <config.toml>".into()),
    };
    let cfg = load(path);
    match cmd {
        "code" => {
            let (code, paired) = hap::code(cfg.state_dir);
            println!("{code} ({})", if paired { "paired" } else { "not paired" });
        }
        "probe" => probe(&cfg.rtsp),
        "reset" =>println!("removed {} pairing(s); restart the service", hap::reset(cfg.state_dir)),
        "serve" => {
            let protect = cfg.protect.as_ref().map(|p| {
                let host = p.host.clone().or_else(|| rtsp_host(&cfg.rtsp)).unwrap_or_else(|| die("[protect] host?".into()));
                Arc::new(protect::Protect::new(&host, &p.token, &p.camera_id, p.cert_sha256.as_deref()).unwrap_or_else(|e| die(e)))
            });
            if protect.is_none() {
                log("no [protect]: two-way audio off".into());
            }
            let cam = Arc::new(camera::Camera::new(&cfg.name, cfg.doorbell, cfg.rtsp.clone(), protect).with_info(cfg.manufacturer.clone(), cfg.model.clone()));
            let server = hap::Server::start(cam, cfg.hap_port, cfg.state_dir).unwrap_or_else(|e| die(e));
            if let Some(listen) = &cfg.ring_listen {
                if cfg.ring_token.is_empty() {
                    die("ring_listen needs ring_token".into());
                }
                let ringer = ring::Ringer { server: server.clone(), iid: camera::SWITCH_EVENT };
                ring::http(listen, cfg.ring_token.clone(), ringer).unwrap_or_else(|e| die(e));
            }
            loop {
                std::thread::park();
            }
        }
        c => die(format!("unknown command {c}")),
    }
}

/// The console's host, from any RTSP URL.
fn rtsp_host(ch: &stream::Channels) -> Option<String> {
    let url = ch.low.as_ref().or(ch.medium.as_ref()).or(ch.high.as_ref())?;
    Some(url.strip_prefix("rtsp://")?.split(['/', ':']).next()?.to_string())
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
