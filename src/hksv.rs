//! HomeKit Secure Video: the services a home hub needs to record, and their state.
//!
//! Services (instance ids):
//! - Camera Operating Mode [60]: EventSnapshotsActive, HomeKitCameraActive, PeriodicSnapshotsActive
//! - Camera Event Recording Management [70]: Active, Supported{Camera,Video,Audio}Recording-
//!   Configuration, SelectedCameraRecordingConfiguration, RecordingAudioActive; linked to the
//!   Motion Sensor [50] (and the Doorbell [40]) as triggers
//! - Data Stream Transport Management [80]: SupportedDataStreamTransportConfiguration,
//!   SetupDataStreamTransport, Version
//!
//! The hub's choices are kept in `<state_dir>/hksv.json`, since it writes them only once.

use crate::hap::{BAD_VALUE, Conn, NO_SUCH, READ_ONLY};
use crate::hds;
use crate::recording::Recorder;
use crate::tlv::{self, Tlv};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const PREBUFFER_MS: u32 = 4000;
const FRAGMENT_MS: u32 = 4000;

#[derive(Serialize, Deserialize, Clone)]
struct Saved {
    active: u8,
    camera_active: u8,
    event_snapshots: u8,
    periodic_snapshots: u8,
    audio_active: u8,
    /// SelectedCameraRecordingConfiguration as written (base64)
    selected: String,
}

impl Default for Saved {
    fn default() -> Self {
        Saved { active: 0, camera_active: 1, event_snapshots: 1, periodic_snapshots: 1, audio_active: 1, selected: String::new() }
    }
}

pub struct Hksv {
    doorbell: bool,
    path: PathBuf,
    saved: Mutex<Saved>,
    recorder: Arc<Recorder>,
    hds: Arc<hds::Server>,
}

struct Source(Arc<Recorder>);

impl hds::RecordingSource for Source {
    fn start(&self) -> Result<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>), String> {
        self.0.start()
    }
}

const PR_EV: &[&str] = &["pr", "ev"];
const PR_PW_EV: &[&str] = &["pr", "pw", "ev"];

fn ch(iid: u64, ty: &str, format: &str, perms: &[&str], value: Value) -> Value {
    let mut o = json!({"iid": iid, "type": ty, "format": format, "perms": perms});
    if !value.is_null() {
        o["value"] = value;
    }
    o
}

impl Hksv {
    /// `url`: the RTSP stream to record (the Medium channel by default).
    pub fn new(doorbell: bool, state_dir: PathBuf, url: String) -> Result<Arc<Hksv>, String> {
        let path = state_dir.join("hksv.json");
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let recorder = Recorder::new(url);
        let hds = hds::Server::start(Arc::new(Source(recorder.clone())))?;
        crate::log(format!("hksv: data stream on port {}", hds.port()));
        let h = Arc::new(Hksv { doorbell, path, saved: Mutex::new(saved), recorder, hds });
        h.apply();
        Ok(h)
    }

    /// Recorder on/off and settings from the saved state.
    fn apply(&self) {
        let s = self.saved.lock().unwrap().clone();
        let on = s.active == 1 && s.camera_active == 1 && !s.selected.is_empty();
        if on != self.recorder.enabled.load(Ordering::Relaxed) {
            crate::log(format!("hksv: recording {}", if on { "enabled" } else { "disabled" }));
        }
        self.recorder.audio_enabled.store(s.audio_active == 1, Ordering::Relaxed);
        if let Some(sel) = tlv::unb64(&s.selected) {
            let items = tlv::parse(&sel);
            if let Some(rec) = tlv::get(&items, 1).map(tlv::parse) {
                if let Some(pre) = tlv::get_uint(&rec, 1) {
                    self.recorder.prebuffer_ms.store(pre.clamp(1000, 8000) as u32, Ordering::Relaxed);
                }
            }
        }
        self.recorder.enabled.store(on, Ordering::Relaxed);
    }

    fn save(&self) {
        let s = self.saved.lock().unwrap().clone();
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&s).unwrap()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn supported_recording(&self) -> Tlv {
        let container = Tlv::new().u8(1, 0).tlv(2, Tlv::new().u32(1, FRAGMENT_MS)); // fragmented MP4
        let triggers: u64 = 0x01 | if self.doorbell { 0x02 } else { 0 }; // motion, doorbell
        Tlv::new().u32(1, PREBUFFER_MS).bytes(2, &triggers.to_le_bytes()).tlv(3, container)
    }

    fn supported_video() -> Tlv {
        let params = Tlv::new().u8(1, 0).u8(1, 1).u8(1, 2).u8(2, 0).u8(2, 1).u8(2, 2);
        let sizes: [(u16, u16); 6] = [(1920, 1080), (1280, 720), (640, 360), (1280, 960), (640, 480), (1080, 1440)];
        let mut cfg = Tlv::new().u8(1, 0).tlv(2, params);
        for (i, (w, h)) in sizes.iter().enumerate() {
            if i > 0 {
                cfg = cfg.sep();
            }
            cfg = cfg.tlv(3, Tlv::new().u16(1, *w).u16(2, *h).u8(3, 24));
        }
        Tlv::new().tlv(1, cfg)
    }

    fn supported_audio() -> Tlv {
        // AAC-LC, mono, variable bitrate, 16 kHz: what Protect's AAC track is (copied, not encoded)
        Tlv::new().tlv(1, Tlv::new().u8(1, 0).tlv(2, Tlv::new().u8(1, 1).u8(2, 0).u8(3, 1)))
    }

    fn supported_transport() -> Tlv {
        Tlv::new().tlv(1, Tlv::new().u8(1, 0)) // HomeKit Data Stream over TCP
    }

    pub fn services(&self) -> Vec<Value> {
        let s = self.saved.lock().unwrap().clone();
        let mut linked = vec![50];
        if self.doorbell {
            linked.push(40);
        }
        vec![
            json!({"iid": 60, "type": "21A", "characteristics": [
                ch(61, "223", "uint8", PR_PW_EV, json!(s.event_snapshots)),
                ch(62, "21B", "uint8", PR_PW_EV, json!(s.camera_active)),
                ch(63, "225", "uint8", PR_PW_EV, json!(s.periodic_snapshots)),
            ]}),
            json!({"iid": 70, "type": "204", "linked": linked, "characteristics": [
                ch(71, "B0", "uint8", PR_PW_EV, json!(s.active)),
                ch(72, "205", "tlv8", PR_EV, json!(self.supported_recording().base64())),
                ch(73, "206", "tlv8", PR_EV, json!(Self::supported_video().base64())),
                ch(74, "207", "tlv8", PR_EV, json!(Self::supported_audio().base64())),
                ch(75, "209", "tlv8", PR_PW_EV, json!(s.selected)),
                ch(76, "226", "uint8", PR_PW_EV, json!(s.audio_active)),
            ]}),
            json!({"iid": 80, "type": "129", "characteristics": [
                ch(81, "130", "tlv8", &["pr"], json!(Self::supported_transport().base64())),
                ch(82, "131", "tlv8", &["pr", "pw", "wr"], json!("")),
                ch(83, "37", "string", &["pr"], json!("1.0")),
            ]}),
        ]
    }

    pub fn handles(iid: u64) -> bool {
        (60..90).contains(&iid)
    }

    pub fn read(&self, iid: u64) -> Result<Value, i64> {
        let s = self.saved.lock().unwrap();
        Ok(match iid {
            61 => json!(s.event_snapshots),
            62 => json!(s.camera_active),
            63 => json!(s.periodic_snapshots),
            71 => json!(s.active),
            72 => json!(self.supported_recording().base64()),
            73 => json!(Self::supported_video().base64()),
            74 => json!(Self::supported_audio().base64()),
            75 => json!(s.selected),
            76 => json!(s.audio_active),
            81 => json!(Self::supported_transport().base64()),
            82 => json!(""),
            83 => json!("1.0"),
            _ => return Err(NO_SUCH),
        })
    }

    pub fn write(&self, conn: Conn, iid: u64, value: &Value) -> Result<Option<Value>, i64> {
        let n = || value.as_u64().or_else(|| value.as_bool().map(u64::from)).map(|v| v.min(1) as u8).ok_or(BAD_VALUE);
        {
            let mut s = self.saved.lock().unwrap();
            match iid {
                61 => s.event_snapshots = n()?,
                62 => s.camera_active = n()?,
                63 => s.periodic_snapshots = n()?,
                71 => s.active = n()?,
                76 => s.audio_active = n()?,
                75 => {
                    let v = value.as_str().ok_or(BAD_VALUE)?;
                    tlv::unb64(v).ok_or(BAD_VALUE)?;
                    s.selected = v.to_string();
                }
                82 => {
                    drop(s);
                    return self.setup_transport(conn, value).map(Some);
                }
                _ => return Err(READ_ONLY),
            }
        }
        crate::log(format!("hksv: {} = {}", iid, if iid == 75 { "(recording configuration)".to_string() } else { value.to_string() }));
        self.save();
        self.apply();
        Ok(None)
    }

    /// SetupDataStreamTransport: start a session, answer with our port and salt.
    fn setup_transport(&self, conn: Conn, value: &Value) -> Result<Value, i64> {
        let req = tlv::parse(&value.as_str().and_then(tlv::unb64).ok_or(BAD_VALUE)?);
        let command = tlv::get_uint(&req, 1).ok_or(BAD_VALUE)?;
        let controller_salt = tlv::get(&req, 3).ok_or(BAD_VALUE)?;
        if command != 0 || conn.secret == [0; 32] {
            return Ok(json!(Tlv::new().u8(1, 1).base64())); // generic error
        }
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        self.hds.prepare(hds::keys(&conn.secret, controller_salt, &salt));
        crate::log("hksv: data stream session prepared".into());
        let reply = Tlv::new().u8(1, 0).tlv(2, Tlv::new().u16(1, self.hds.port())).bytes(3, &salt);
        Ok(json!(reply.base64()))
    }
}
