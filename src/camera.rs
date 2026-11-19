//! A camera (optionally a video doorbell) as a HomeKit accessory.
//!
//! Services (instance ids):
//! - Accessory Information [1]
//! - Protocol Information [8]
//! - Camera RTP Stream Management [10]: SupportedVideo/Audio/RTPConfiguration, SetupEndpoints,
//!   SelectedRTPStreamConfiguration, StreamingStatus, Active
//! - Microphone [20] (Mute), Speaker [30] (Mute, Volume)
//! - Doorbell [40] (ProgrammableSwitchEvent), only when `doorbell` is set; primary then
//!
//! Live streaming is `crate::stream`'s job; this module speaks the TLVs.

use crate::hap::{self, Accessory, BAD_VALUE, Conn, NO_SUCH, NOT_READABLE, READ_ONLY};
use crate::tlv::{self, Tlv};
use rand_core::{OsRng, RngCore};
use serde_json::{Value, json};
use std::collections::HashMap;
use crate::stream::{self, Channels};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

pub const SWITCH_EVENT: u64 = 41;
pub const MOTION_DETECTED: u64 = 51;
const SETUP_ENDPOINTS: u64 = 14;
const SELECTED_CONFIG: u64 = 15;
const STREAMING_STATUS: u64 = 16;
const STREAM_ACTIVE: u64 = 17;

/// Codec numbers in the camera TLVs.
const H264: u8 = 0;
const OPUS: u8 = 3;


/// One controller's negotiated stream (from SetupEndpoints), keyed by its session id.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub session_id: Vec<u8>,
    pub conn: u64,
    pub controller: std::net::IpAddr,
    pub video_port: u16,
    pub audio_port: u16,
    /// controller's SRTP key+salt: what it encrypts with (its talk-back audio)
    pub video_srtp_in: Vec<u8>,
    pub audio_srtp_in: Vec<u8>,
    /// ours: what we encrypt the camera's video/audio with
    pub video_srtp_out: Vec<u8>,
    pub audio_srtp_out: Vec<u8>,
    pub video_ssrc: u32,
    pub audio_ssrc: u32,
    pub local: std::net::IpAddr,
    /// our UDP sockets (their ports go in the SetupEndpoints answer)
    pub video_sock: Arc<UdpSocket>,
    pub audio_sock: Arc<UdpSocket>,
}

pub struct Camera {
    name: String,
    manufacturer: String,
    model: String,
    doorbell: bool,
    /// per connection: the last SetupEndpoints answer (read back by controllers that don't use
    /// write-responses)
    setup_reply: Mutex<HashMap<u64, Vec<u8>>>,
    endpoints: Mutex<HashMap<Vec<u8>, Endpoint>>,
    muted: Mutex<(bool, bool, u8)>, // microphone, speaker, volume
    channels: Channels,
    protect: Option<Arc<crate::protect::Protect>>,
    snapshots: Option<Arc<crate::snapshot::Snapshots>>,
    /// Motion Sensor service, when there is a motion source
    motion: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// HomeKit Secure Video services, when enabled
    hksv: Option<Arc<crate::hksv::Hksv>>,
    streams: Mutex<HashMap<Vec<u8>, stream::Handle>>,
}

impl Camera {
    pub fn new(name: &str, doorbell: bool, channels: Channels, protect: Option<Arc<crate::protect::Protect>>) -> Self {
        Camera {
            name: name.into(),
            manufacturer: "rust-homekit-cam".into(),
            model: if doorbell { "Video Doorbell" } else { "Camera" }.into(),
            doorbell,
            setup_reply: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            muted: Mutex::new((false, false, 100)),
            channels,
            protect,
            snapshots: None,
            motion: None,
            hksv: None,
            streams: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_hksv(mut self, h: Option<Arc<crate::hksv::Hksv>>) -> Self {
        self.hksv = h;
        self
    }

    pub fn with_motion(mut self, state: Option<Arc<std::sync::atomic::AtomicBool>>) -> Self {
        self.motion = state;
        self
    }

    pub fn with_snapshots(mut self, s: Option<Arc<crate::snapshot::Snapshots>>) -> Self {
        self.snapshots = s;
        self
    }

    /// Manufacturer and model shown in the Home app (accessory details).
    pub fn with_info(mut self, manufacturer: Option<String>, model: Option<String>) -> Self {
        self.manufacturer = manufacturer.unwrap_or(self.manufacturer);
        self.model = model.unwrap_or(self.model);
        self
    }

    fn supported_video() -> Tlv {
        // Advertised sizes: what the iPhone may ask for. We send the Protect channel that fits
        // (the real size is in the SPS; iOS decodes whatever comes). Portrait sizes for the
        // 3:4 doorbell are included.
        let sizes: [(u16, u16); 12] = [
            (1920, 1080), (1280, 720), (640, 360), (480, 270), (320, 180),
            (1280, 960), (1024, 768), (640, 480), (480, 360), (320, 240),
            (1080, 1440), (480, 640),
        ];
        let params = Tlv::new().u8(1, 0).u8(1, 1).u8(1, 2).u8(2, 0).u8(2, 1).u8(2, 2).u8(3, 0);
        // attributes: repeated type-3 items, separated
        let mut cfg = Tlv::new().u8(1, H264).tlv(2, params);
        for (i, (w, h)) in sizes.iter().enumerate() {
            if i > 0 {
                cfg = cfg.sep();
            }
            cfg = cfg.tlv(3, Tlv::new().u16(1, *w).u16(2, *h).u8(3, 30));
        }
        Tlv::new().tlv(1, cfg)
    }

    fn supported_audio() -> Tlv {
        // Opus only (16/24 kHz mono, variable bitrate): Protect's own Opus track passes through.
        // AAC-ELD is not offered: iPhones prefer it when listed, and it would need a transcode.
        let opus = |rate: u8| Tlv::new().u8(1, OPUS).tlv(2, Tlv::new().u8(1, 1).u8(2, 0).u8(3, rate));
        Tlv::new().tlv(1, opus(2)).sep().tlv(1, opus(1)).u8(2, 0) // no comfort noise
    }

    fn supported_rtp() -> Tlv {
        Tlv::new().u8(2, 0) // AES_CM_128_HMAC_SHA1_80
    }

    /// SetupEndpoints write: remember the controller's side, answer with ours.
    fn setup_endpoints(&self, conn: Conn, req: &[u8]) -> Result<Vec<u8>, i64> {
        let items = tlv::parse(req);
        let sid = tlv::get(&items, 1).ok_or(BAD_VALUE)?.to_vec();
        let addr = tlv::parse(tlv::get(&items, 3).ok_or(BAD_VALUE)?);
        let ip: std::net::IpAddr = std::str::from_utf8(tlv::get(&addr, 2).ok_or(BAD_VALUE)?)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(BAD_VALUE)?;
        let srtp = |t| -> Result<Vec<u8>, i64> {
            let p = tlv::parse(tlv::get(&items, t).ok_or(BAD_VALUE)?);
            let mut k = tlv::get(&p, 2).ok_or(BAD_VALUE)?.to_vec();
            k.extend_from_slice(tlv::get(&p, 3).ok_or(BAD_VALUE)?);
            Ok(k)
        };
        let key = || {
            let mut k = vec![0u8; 30];
            OsRng.fill_bytes(&mut k);
            k
        };
        let bind = |ip: std::net::IpAddr| UdpSocket::bind((ip, 0)).map(Arc::new).map_err(|_| BAD_VALUE);
        let (video_sock, audio_sock) = (bind(conn.local)?, bind(conn.local)?);
        let port = |s: &UdpSocket| s.local_addr().map(|a| a.port()).unwrap_or(0);
        let (our_video, our_audio) = (port(&video_sock), port(&audio_sock));
        let ep = Endpoint {
            session_id: sid.clone(),
            conn: conn.id,
            controller: ip,
            video_port: tlv::get_uint(&addr, 3).ok_or(BAD_VALUE)? as u16,
            audio_port: tlv::get_uint(&addr, 4).ok_or(BAD_VALUE)? as u16,
            video_srtp_in: srtp(4)?,
            audio_srtp_in: srtp(5)?,
            video_srtp_out: key(),
            audio_srtp_out: key(),
            video_ssrc: OsRng.next_u32(),
            audio_ssrc: OsRng.next_u32(),
            local: conn.local,
            video_sock,
            audio_sock,
        };
        crate::log(format!("stream setup from {}: video :{}, audio :{}", ep.controller, ep.video_port, ep.audio_port));
        let srtp_out = |k: &[u8]| Tlv::new().u8(1, 0).bytes(2, &k[..16]).bytes(3, &k[16..]);
        let ours = Tlv::new()
            .u8(1, if ep.local.is_ipv4() { 0 } else { 1 })
            .bytes(2, ep.local.to_string().as_bytes())
            .u16(3, our_video)
            .u16(4, our_audio);
        let reply = Tlv::new()
            .bytes(1, &sid)
            .u8(2, 0)
            .tlv(3, ours)
            .tlv(4, srtp_out(&ep.video_srtp_out))
            .tlv(5, srtp_out(&ep.audio_srtp_out))
            .u32(6, ep.video_ssrc)
            .u32(7, ep.audio_ssrc)
            .encode();
        self.endpoints.lock().unwrap().insert(sid, ep);
        Ok(reply)
    }

    /// The audio half of SelectedRTPStreamConfiguration; None unless Opus (and not muted).
    fn selected_audio(items: &[(u8, Vec<u8>)], muted: bool) -> Option<stream::Audio> {
        let raw = tlv::get(items, 3)?;
        let a = tlv::parse(raw);
        let codec = tlv::get_uint(&a, 1)?;
        let params = tlv::get(&a, 2).map(tlv::parse).unwrap_or_default();
        let rtp = tlv::get(&a, 3).map(tlv::parse).unwrap_or_default();
        let rate = [8, 16, 24].get(tlv::get_uint(&params, 3).unwrap_or(9) as usize).copied().unwrap_or(0);
        let ptime = tlv::get_uint(&params, 4).unwrap_or(0);
        let pt = tlv::get_uint(&rtp, 1).unwrap_or(110) as u8;
        crate::log(format!("audio selected: codec {codec} {rate} kHz, {ptime} ms packets, pt {pt}{}", if muted { " (muted)" } else { "" }));
        (codec == OPUS as u64 && !muted).then_some(stream::Audio { pt, ptime: ptime as u32, rate: rate * 1000 })
    }

    /// SelectedRTPStreamConfiguration write: start / stop / reconfigure.
    fn selected(&self, req: &[u8]) -> Result<(), i64> {
        let items = tlv::parse(req);
        let ctl = tlv::parse(tlv::get(&items, 1).ok_or(BAD_VALUE)?);
        let sid = tlv::get(&ctl, 1).ok_or(BAD_VALUE)?.to_vec();
        let command = tlv::get_uint(&ctl, 2).ok_or(BAD_VALUE)?;
        let video = tlv::get(&items, 2).map(tlv::parse).unwrap_or_default();
        let attrs = tlv::get(&video, 3).map(tlv::parse).unwrap_or_default();
        let rtp = tlv::get(&video, 4).map(tlv::parse).unwrap_or_default();
        let (w, h) = (tlv::get_uint(&attrs, 1).unwrap_or(0), tlv::get_uint(&attrs, 2).unwrap_or(0));
        let (kbps, mtu) = (tlv::get_uint(&rtp, 3).unwrap_or(0), tlv::get_uint(&rtp, 5).unwrap_or(1378));
        let pt = tlv::get_uint(&rtp, 1).unwrap_or(99) as u8;
        match command {
            1 => {
                crate::log(format!("stream start: {w}x{h} ≤{kbps} kbps mtu {mtu} pt {pt}"));
                let ep = self.endpoints.lock().unwrap().get(&sid).cloned().ok_or(BAD_VALUE)?;
                let audio = Self::selected_audio(&items, self.muted.lock().unwrap().0);
                let req = stream::Request { pt, kbps, mtu: mtu as usize, audio };
                match stream::start(ep, req, &self.channels, self.protect.clone()) {
                    Ok(h) => {
                        self.streams.lock().unwrap().insert(sid, h);
                    }
                    Err(e) => crate::log(format!("stream start: {e}")),
                }
            }
            4 => crate::log(format!("stream reconfigure: {w}x{h} ≤{kbps} kbps (kept the current channel)")),
            0 => {
                crate::log("stream end".into());
                self.endpoints.lock().unwrap().remove(&sid);
                self.streams.lock().unwrap().remove(&sid);
            }
            c => crate::log(format!("stream command {c}")),
        }
        Ok(())
    }
}

const PR: &[&str] = &["pr"];
const PW: &[&str] = &["pw"];
const PR_EV: &[&str] = &["pr", "ev"];
const PR_PW: &[&str] = &["pr", "pw"];
const PR_PW_EV: &[&str] = &["pr", "pw", "ev"];

fn ch(iid: u64, ty: &str, format: &str, perms: &[&str], extra: Value) -> Value {
    let mut o = json!({"iid": iid, "type": ty, "format": format, "perms": perms});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            o[k] = v;
        }
    }
    o
}

impl Accessory for Camera {
    fn name(&self) -> &str {
        &self.name
    }

    fn category(&self) -> u64 {
        if self.doorbell { hap::CATEGORY } else { 17 }
    }

    fn services(&self, serial: &str) -> Value {
        let mut list = vec![
            json!({"iid": 1, "type": "3E", "characteristics": [
                ch(2, "14", "bool", PW, json!({})),
                ch(3, "20", "string", PR, json!({"value": self.manufacturer})),
                ch(4, "21", "string", PR, json!({"value": self.model})),
                ch(5, "23", "string", PR, json!({"value": self.name})),
                ch(6, "30", "string", PR, json!({"value": serial})),
                ch(7, "52", "string", PR, json!({"value": env!("CARGO_PKG_VERSION")})),
            ]}),
            json!({"iid": 8, "type": "A2", "characteristics": [ch(9, "37", "string", PR, json!({"value": "1.1.0"}))]}),
            json!({"iid": 10, "type": "110", "characteristics": [
                ch(11, "114", "tlv8", PR, json!({"value": Self::supported_video().base64()})),
                ch(12, "115", "tlv8", PR, json!({"value": Self::supported_audio().base64()})),
                ch(13, "116", "tlv8", PR, json!({"value": Self::supported_rtp().base64()})),
                ch(SETUP_ENDPOINTS, "118", "tlv8", PR_PW, json!({})),
                ch(SELECTED_CONFIG, "117", "tlv8", PR_PW, json!({})),
                ch(STREAMING_STATUS, "120", "tlv8", PR_EV, json!({})),
                ch(STREAM_ACTIVE, "B0", "uint8", PR_PW_EV, json!({"minValue": 0, "maxValue": 1, "minStep": 1})),
            ]}),
            json!({"iid": 20, "type": "112", "characteristics": [ch(21, "11A", "bool", PR_PW_EV, json!({}))]}),
        ];
        // the Speaker service is what makes the Home app show the talk button: only with a
        // talk-back backend (changes c#, so controllers refetch without a re-add)
        if self.protect.is_some() {
            list.push(json!({"iid": 30, "type": "113", "characteristics": [
                ch(31, "11A", "bool", PR_PW_EV, json!({})),
                ch(32, "119", "uint8", PR_PW_EV, json!({"minValue": 0, "maxValue": 100, "minStep": 1, "unit": "percentage"})),
            ]}));
        }
        if self.doorbell {
            list.push(json!({"iid": 40, "type": "121", "primary": true, "characteristics": [
                ch(SWITCH_EVENT, "73", "uint8", PR_EV, json!({"minValue": 0, "maxValue": 0, "validValues": [0]})),
                ch(42, "23", "string", PR, json!({"value": self.name})),
            ]}));
        }
        if let Some(h) = &self.hksv {
            list.extend(h.services());
        }
        if self.motion.is_some() {
            list.push(json!({"iid": 50, "type": "85", "characteristics": [
                ch(MOTION_DETECTED, "22", "bool", PR_EV, json!({})),
                ch(52, "23", "string", PR, json!({"value": format!("{} Motion", self.name)})),
            ]}));
        }
        // fill in readable values
        for s in &mut list {
            for c in s["characteristics"].as_array_mut().unwrap() {
                let iid = c["iid"].as_u64().unwrap();
                if c.get("value").is_none() && c["perms"].as_array().unwrap().iter().any(|p| p == "pr") {
                    if let Ok(v) = self.read(Conn { id: 0, peer: [0, 0, 0, 0].into(), local: [0, 0, 0, 0].into(), secret: [0; 32] }, iid) {
                        c["value"] = v;
                    }
                }
            }
        }
        Value::Array(list)
    }

    fn read(&self, conn: Conn, iid: u64) -> Result<Value, i64> {
        if let (Some(h), true) = (&self.hksv, crate::hksv::Hksv::handles(iid)) {
            return h.read(iid);
        }
        let m = *self.muted.lock().unwrap();
        Ok(match iid {
            11 => json!(Self::supported_video().base64()),
            12 => json!(Self::supported_audio().base64()),
            13 => json!(Self::supported_rtp().base64()),
            SETUP_ENDPOINTS => json!(tlv::b64(self.setup_reply.lock().unwrap().get(&conn.id).map(Vec::as_slice).unwrap_or(&[]))),
            SELECTED_CONFIG => json!(""),
            STREAMING_STATUS => json!(Tlv::new().u8(1, 0).base64()), // available
            STREAM_ACTIVE => json!(1),
            21 => json!(m.0),
            31 => json!(m.1),
            32 => json!(m.2),
            SWITCH_EVENT if self.doorbell => Value::Null, // only meaningful as an event
            MOTION_DETECTED if self.motion.is_some() => {
                json!(self.motion.as_ref().unwrap().load(std::sync::atomic::Ordering::SeqCst))
            }
            3..=7 | 9 | 42 | 52 => return Err(NOT_READABLE), // static: served from the database
            _ => return Err(NO_SUCH),
        })
    }

    fn write(&self, conn: Conn, iid: u64, value: &Value) -> Result<Option<Value>, i64> {
        if let (Some(h), true) = (&self.hksv, crate::hksv::Hksv::handles(iid)) {
            return h.write(conn, iid, value);
        }
        let tlv_in = || value.as_str().and_then(tlv::unb64).ok_or(BAD_VALUE);
        match iid {
            2 => {
                crate::log("identify".into());
                Ok(None)
            }
            SETUP_ENDPOINTS => {
                let reply = self.setup_endpoints(conn, &tlv_in()?)?;
                let v = json!(tlv::b64(&reply));
                self.setup_reply.lock().unwrap().insert(conn.id, reply);
                Ok(Some(v))
            }
            SELECTED_CONFIG => self.selected(&tlv_in()?).map(|_| None),
            STREAM_ACTIVE => Ok(None),
            21 | 31 => {
                let b = value.as_bool().or(value.as_u64().map(|v| v == 1)).ok_or(BAD_VALUE)?;
                let mut m = self.muted.lock().unwrap();
                if iid == 21 { m.0 = b } else { m.1 = b }
                Ok(None)
            }
            32 => {
                self.muted.lock().unwrap().2 = value.as_u64().ok_or(BAD_VALUE)?.min(100) as u8;
                Ok(None)
            }
            _ => Err(READ_ONLY),
        }
    }

    fn snapshot(&self, _w: u32, _h: u32) -> Result<Arc<Vec<u8>>, String> {
        self.snapshots.as_ref().ok_or("no snapshot source configured")?.get()
    }

    fn closed(&self, conn: Conn) {
        self.setup_reply.lock().unwrap().remove(&conn.id);
        let gone: Vec<Vec<u8>> =
            self.endpoints.lock().unwrap().iter().filter(|(_, e)| e.conn == conn.id).map(|(k, _)| k.clone()).collect();
        for k in gone {
            self.endpoints.lock().unwrap().remove(&k);
            self.streams.lock().unwrap().remove(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Conn {
        Conn { id: 1, peer: [127, 0, 0, 1].into(), local: [127, 0, 0, 1].into(), secret: [0; 32] }
    }

    #[test]
    fn database_has_doorbell_and_camera() {
        let cam = Camera::new("Doorbell", true, Channels::default(), None);
        let s = cam.services("AA:BB");
        let types: Vec<&str> = s.as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
        // no talk-back backend: no Speaker (113), so no talk button in the Home app
        assert_eq!(types, ["3E", "A2", "110", "112", "121"]);
        assert_eq!(cam.read(conn(), SWITCH_EVENT), Ok(Value::Null));
        assert!(Camera::new("Cam", false, Channels::default(), None).services("x").as_array().unwrap().len() == 4);
    }

    #[test]
    fn setup_endpoints_answers_with_our_side() {
        let cam = Camera::new("Doorbell", true, Channels::default(), None);
        let srtp = Tlv::new().u8(1, 0).bytes(2, &[1; 16]).bytes(3, &[2; 14]);
        let req = Tlv::new()
            .bytes(1, &[9; 16])
            .tlv(3, Tlv::new().u8(1, 0).bytes(2, b"10.0.0.5").u16(3, 50000).u16(4, 50002))
            .tlv(4, srtp)
            .tlv(5, Tlv::new().u8(1, 0).bytes(2, &[3; 16]).bytes(3, &[4; 14]))
            .base64();
        let out = cam.write(conn(), SETUP_ENDPOINTS, &json!(req)).unwrap().unwrap();
        let items = tlv::parse(&tlv::unb64(out.as_str().unwrap()).unwrap());
        assert_eq!(tlv::get(&items, 1), Some(&[9u8; 16][..]));
        assert_eq!(tlv::get_uint(&items, 2), Some(0));
        let addr = tlv::parse(tlv::get(&items, 3).unwrap());
        assert_eq!(tlv::get(&addr, 2), Some(&b"127.0.0.1"[..]));
        assert_ne!(tlv::get_uint(&addr, 3), Some(50000));
        let ep = cam.endpoints.lock().unwrap()[&vec![9u8; 16]].clone();
        assert_eq!((ep.video_port, ep.audio_port, ep.video_srtp_in[0], ep.video_srtp_in[29]), (50000, 50002, 1, 2));
        assert_eq!(cam.read(conn(), SETUP_ENDPOINTS).unwrap(), out);
    }
}
