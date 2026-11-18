//! One live-view stream: Protect RTSP (H.264) → repacketized → SRTP to the controller.
//! Stops when told to, when the RTSP connection ends, or when the controller has sent nothing
//! (RTCP receiver reports) for 30 s.

use crate::camera::Endpoint;
use crate::h264::{Depacketizer, Packetizer};
use crate::rtsp;
use crate::srtp;
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Protect's RTSP URLs per channel.
#[derive(Deserialize, Default, Clone)]
pub struct Channels {
    pub high: Option<String>,
    pub medium: Option<String>,
    pub low: Option<String>,
    /// allow High for big requests (10 Mbps on a Doorbell Lite)
    #[serde(default)]
    pub allow_high: bool,
}

impl Channels {
    /// The channel for a requested max bitrate (kbps).
    pub fn pick(&self, kbps: u64) -> Option<(&'static str, &str)> {
        let order: &[(&'static str, &Option<String>)] = if kbps <= 500 {
            &[("low", &self.low), ("medium", &self.medium), ("high", &self.high)]
        } else if kbps >= 4000 && self.allow_high {
            &[("high", &self.high), ("medium", &self.medium), ("low", &self.low)]
        } else {
            &[("medium", &self.medium), ("low", &self.low), ("high", &self.high)]
        };
        order.iter().find_map(|(n, u)| u.as_deref().map(|u| (*n, u)))
    }
}

pub struct Request {
    pub pt: u8,
    pub kbps: u64,
    pub mtu: usize,
}

pub struct Handle {
    stop: Arc<AtomicBool>,
    rtsp: Arc<Mutex<Option<TcpStream>>>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.rtsp.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(ep: Endpoint, req: Request, channels: &Channels) -> Result<Handle, String> {
    let (name, url) = channels.pick(req.kbps).ok_or("no RTSP URL configured")?;
    let url = url.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let rtsp_conn = Arc::new(Mutex::new(None));
    let h = Handle { stop: stop.clone(), rtsp: rtsp_conn.clone() };
    crate::log(format!("stream: {} kbps requested → Protect channel {name}", req.kbps));

    // watchdog: the controller's RTCP keeps us going
    let (sock, stop2, rtsp2) = (ep.video_sock.clone(), stop.clone(), rtsp_conn.clone());
    std::thread::spawn(move || {
        let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
        let mut last = Instant::now();
        let mut buf = [0u8; 2048];
        while !stop2.load(Ordering::Relaxed) {
            if sock.recv(&mut buf).is_ok() {
                last = Instant::now();
            }
            if last.elapsed() > Duration::from_secs(30) {
                crate::log("stream: no RTCP from the controller for 30 s, stopping".into());
                stop2.store(true, Ordering::Relaxed);
                if let Some(s) = rtsp2.lock().unwrap().take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    });

    std::thread::spawn(move || {
        if let Err(e) = run(&ep, &req, &url, &stop, &rtsp_conn) {
            if !stop.load(Ordering::Relaxed) {
                crate::log(format!("stream: {e}"));
            }
        }
        stop.store(true, Ordering::Relaxed);
        crate::log("stream: ended".into());
    });
    Ok(h)
}

fn run(ep: &Endpoint, req: &Request, url: &str, stop: &AtomicBool, conn: &Mutex<Option<TcpStream>>) -> Result<(), String> {
    let (mut client, tracks) = rtsp::Client::connect(url)?;
    *conn.lock().unwrap() = client.stream();
    let video = tracks.iter().find(|t| t.codec == "h264").ok_or("no H.264 track (is the camera on H.265?)")?;
    client.setup(video, 0)?;
    client.play()?;

    let dest = SocketAddr::new(ep.controller, ep.video_port);
    let mut srtp = srtp::Sender::new(&ep.video_srtp_out);
    let mut depack = Depacketizer::default();
    let mut pack = Packetizer::new(req.pt, ep.video_ssrc, req.mtu, &video.params, OsRng.next_u32());
    let (mut nals, mut pkts) = (Vec::new(), Vec::new());
    let (mut count, mut octets) = (0u32, 0u32);
    let mut last_sr = Instant::now();
    let started = Instant::now();
    let mut first_ts: Option<u32> = None;

    while !stop.load(Ordering::Relaxed) {
        let (ch, data) = client.next()?;
        if ch != 0 {
            continue;
        }
        depack.push(&data, &mut nals);
        for n in nals.drain(..) {
            pack.push(&n, &mut pkts);
        }
        for p in pkts.drain(..) {
            first_ts.get_or_insert(pack.last_ts);
            count = count.wrapping_add(1);
            octets = octets.wrapping_add((p.len() - 12) as u32);
            let _ = ep.video_sock.send_to(&srtp.protect_rtp(&p), dest);
        }
        if first_ts.is_some() && last_sr.elapsed() > Duration::from_millis(500) {
            last_sr = Instant::now();
            let _ = ep.video_sock.send_to(&srtp.protect_rtcp(&sender_report(ep.video_ssrc, pack.last_ts, count, octets)), dest);
        }
        if count > 0 && count % 2000 == 0 {
            crate::log(format!("stream: {count} packets in {:.0} s", started.elapsed().as_secs_f32()));
        }
    }
    Ok(())
}

fn sender_report(ssrc: u32, ts: u32, count: u32, octets: u32) -> Vec<u8> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ntp_s = now.as_secs() + 2_208_988_800;
    let ntp_f = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&(ntp_s as u32).to_be_bytes());
    p.extend_from_slice(&(ntp_f as u32).to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    p.extend_from_slice(&octets.to_be_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_choice() {
        let c = Channels { high: Some("h".into()), medium: Some("m".into()), low: Some("l".into()), allow_high: false };
        assert_eq!(c.pick(164).unwrap().0, "low");
        assert_eq!(c.pick(2000).unwrap().0, "medium");
        assert_eq!(c.pick(8000).unwrap().0, "medium");
        let c2 = Channels { allow_high: true, ..c.clone() };
        assert_eq!(c2.pick(8000).unwrap().0, "high");
        let only_high = Channels { high: Some("h".into()), ..Default::default() };
        assert_eq!(only_high.pick(100).unwrap().0, "high");
        assert_eq!(sender_report(1, 2, 3, 4).len(), 28);
    }
}
