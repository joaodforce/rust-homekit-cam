//! One live-view stream: Protect RTSP (H.264) → repacketized → SRTP to the controller.
//! Stops when told to, when the RTSP connection ends, or when the controller has sent nothing
//! (RTCP receiver reports) for 30 s.

use crate::camera::Endpoint;
use crate::h264::{Depacketizer, Packetizer};
use crate::rtsp;
use crate::protect::Protect;
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
    pub audio: Option<Audio>,
}

/// Camera audio for the controller (Opus, passed through).
pub struct Audio {
    pub pt: u8,
    /// requested packet time, ms (informational: Protect's own framing is sent as is)
    pub ptime: u32,
    /// the controller's Opus sample rate (its microphone, for talk-back)
    pub rate: u32,
}

/// Rewrites an incoming RTP packet's header for our stream: PT, SSRC, own seq, offset ts.
struct Rewriter {
    pt: u8,
    ssrc: u32,
    seq: u16,
    ts_offset: u32,
    pub last_ts: u32,
}

impl Rewriter {
    fn new(pt: u8, ssrc: u32) -> Self {
        Rewriter { pt, ssrc, seq: OsRng.next_u32() as u16, ts_offset: OsRng.next_u32(), last_ts: 0 }
    }

    fn rewrite(&mut self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < 13 || pkt[0] >> 6 != 2 {
            return None;
        }
        let mut off = 12 + 4 * (pkt[0] & 0x0f) as usize;
        if pkt[0] & 0x10 != 0 && pkt.len() >= off + 4 {
            off += 4 + 4 * u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
        }
        let mut end = pkt.len();
        if pkt[0] & 0x20 != 0 {
            end = end.saturating_sub(*pkt.last()? as usize);
        }
        if off >= end {
            return None;
        }
        let ts = u32::from_be_bytes(pkt[4..8].try_into().ok()?).wrapping_add(self.ts_offset);
        self.last_ts = ts;
        let mut out = vec![0x80, self.pt | (pkt[1] & 0x80)];
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&pkt[off..end]);
        self.seq = self.seq.wrapping_add(1);
        Some(out)
    }
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

pub fn start(ep: Endpoint, req: Request, channels: &Channels, protect: Option<Arc<Protect>>) -> Result<Handle, String> {
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

    // two-way audio: a Protect talk-back session, then the controller's Opus RTP straight to it
    if let (Some(_), Some(p)) = (&req.audio, protect) {
        let (sock, key, stop3) = (ep.audio_sock.clone(), ep.audio_srtp_in.clone(), stop.clone());
        std::thread::spawn(move || talkback(sock, key, &p, stop3));
    }

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
    // camera audio: Protect's Opus track on channel 2
    let opus = req.audio.as_ref().and(tracks.iter().find(|t| t.codec == "opus"));
    if let Some(t) = opus {
        client.setup(t, 2)?;
    } else if req.audio.is_some() {
        crate::log("stream: no Opus track from Protect, video only".into());
    }
    client.play()?;
    let audio_dest = SocketAddr::new(ep.controller, ep.audio_port);
    let mut audio = opus.and(req.audio.as_ref()).map(|a| (Rewriter::new(a.pt, ep.audio_ssrc), srtp::Sender::new(&ep.audio_srtp_out)));
    let (mut a_count, mut a_octets, mut a_logged) = (0u32, 0u32, false);

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
        if ch == 2 {
            if let Some((rw, sr)) = &mut audio {
                if let Some(p) = rw.rewrite(&data) {
                    if !a_logged {
                        a_logged = true;
                        let toc = p[12];
                        crate::log(format!("stream: audio flowing, Opus config {} stereo {} ({} bytes)", toc >> 3, toc >> 2 & 1, p.len() - 12));
                    }
                    a_count = a_count.wrapping_add(1);
                    a_octets = a_octets.wrapping_add((p.len() - 12) as u32);
                    let _ = ep.audio_sock.send_to(&sr.protect_rtp(&p), audio_dest);
                }
            }
            continue;
        }
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
            if let Some((rw, sr)) = &mut audio {
                if a_count > 0 {
                    let _ = ep.audio_sock.send_to(&sr.protect_rtcp(&sender_report(ep.audio_ssrc, rw.last_ts, a_count, a_octets)), audio_dest);
                }
            }
        }
        if count > 0 && count % 2000 == 0 {
            crate::log(format!("stream: {count} packets in {:.0} s", started.elapsed().as_secs_f32()));
        }
    }
    Ok(())
}

/// The controller's microphone → camera speaker, until the stream stops. No transcoding: the
/// camera takes Opus over RTP; we decrypt, give the packets our own header and forward them.
fn talkback(sock: Arc<std::net::UdpSocket>, key: Vec<u8>, protect: &Protect, stop: Arc<AtomicBool>) {
    let tb = match protect.talkback() {
        Ok(t) if t.codec == "opus" => t,
        Ok(t) => return crate::log(format!("talkback: camera wants {} (only Opus is supported)", t.codec)),
        Err(e) => return crate::log(format!("talkback: {e}")),
    };
    crate::log(format!("talkback: session to {} ({} {} Hz)", tb.addr, tb.codec, tb.rate));
    let Ok(out) = std::net::UdpSocket::bind(("0.0.0.0", 0)) else { return };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
    let mut srtp = srtp::Receiver::new(&key);
    let mut rw = Rewriter::new(96, OsRng.next_u32());
    let (mut buf, mut got, mut bad) = ([0u8; 2048], 0u64, 0u64);
    while !stop.load(Ordering::Relaxed) {
        let Ok(n) = sock.recv(&mut buf) else { continue };
        let p = &buf[..n];
        if n < 12 || (200..=204).contains(&p[1]) {
            continue; // RTCP
        }
        let Some(rtp) = srtp.unprotect_rtp(p) else {
            bad += 1;
            continue;
        };
        let Some(pkt) = rw.rewrite(&rtp) else { continue };
        got += 1;
        if got == 1 {
            crate::log(format!("talkback: first packet from the controller ({} bytes Opus)", pkt.len() - 12));
        }
        let _ = out.send_to(&pkt, tb.addr);
    }
    if got > 0 || bad > 0 {
        crate::log(format!("talkback: {got} packets forwarded, {bad} rejected"));
    }
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
        let c = Channels { high: Some("h".into()), medium: Some("m".into()), low: Some("l".into()), ..Default::default() };
        assert_eq!(c.pick(164).unwrap().0, "low");
        assert_eq!(c.pick(2000).unwrap().0, "medium");
        assert_eq!(c.pick(8000).unwrap().0, "medium");
        let c2 = Channels { allow_high: true, ..c.clone() };
        assert_eq!(c2.pick(8000).unwrap().0, "high");
        let only_high = Channels { high: Some("h".into()), ..Default::default() };
        assert_eq!(only_high.pick(100).unwrap().0, "high");
        assert_eq!(sender_report(1, 2, 3, 4).len(), 28);
        let mut rw = Rewriter::new(110, 0x11223344);
        let out = rw.rewrite(&[0x80, 0x80 | 96, 0, 5, 0, 0, 0, 10, 9, 9, 9, 9, 0xfc, 1, 2]).unwrap();
        assert_eq!((out[1], &out[8..12], &out[12..]), (0x80 | 110, &[0x11, 0x22, 0x33, 0x44][..], &[0xfc, 1, 2][..]));
    }
}
