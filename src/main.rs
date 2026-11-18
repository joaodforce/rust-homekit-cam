//! rust-homekit-cam: a UniFi Protect camera / doorbell as a native HomeKit accessory.
//!
//! `rust-homekit-cam serve <config.toml>` | `code <config.toml>` | `reset <config.toml>`

mod camera;
mod hap;
mod ring;
mod tlv;

use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

pub fn log(msg: String) {
    eprintln!("homekit-cam: {msg}");
}

#[derive(Deserialize)]
struct Config {
    /// Name in the Home app.
    name: String,
    #[serde(default = "yes")]
    doorbell: bool,
    #[serde(default = "default_port")]
    hap_port: u16,
    #[serde(default = "default_state")]
    state_dir: PathBuf,
    /// `POST /ring` listener, e.g. "0.0.0.0:8780"; needs `ring_token`.
    ring_listen: Option<String>,
    #[serde(default)]
    ring_token: String,
    /// Protect RTSP URLs (rtsp://<nvr>:7447/<alias>) per channel; used for live view.
    #[serde(default)]
    #[allow(dead_code)]
    rtsp: Rtsp,
}

#[derive(Deserialize, Default)]
#[allow(dead_code)]
struct Rtsp {
    high: Option<String>,
    medium: Option<String>,
    low: Option<String>,
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
        "reset" => println!("removed {} pairing(s); restart the service", hap::reset(cfg.state_dir)),
        "serve" => {
            let cam = Arc::new(camera::Camera::new(&cfg.name, cfg.doorbell));
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
