//! The HKSV recorder: while recording is enabled, one RTSP connection keeps the last few
//! seconds (from a keyframe) of H.264 access units and AAC frames in memory. A recording
//! starts with that prebuffer and continues live; fragments are cut at keyframes.

use crate::h264::Depacketizer;
use crate::mp4::{self, AudioInfo, Run, Sample, VideoInfo};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One video access unit (AVCC) or AAC frame, timestamps unwrapped in track units.
pub struct Media {
    pub video: bool,
    pub ts: i64,
    pub data: Vec<u8>,
    pub key: bool,
    pub arrival: Instant,
}

#[derive(Default)]
struct State {
    buffer: VecDeque<Arc<Media>>,
    live: Vec<Sender<Arc<Media>>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    audio: Option<AudioInfo>,
}

pub struct Recorder {
    url: String,
    pub enabled: AtomicBool,
    pub audio_enabled: AtomicBool,
    pub prebuffer_ms: AtomicU32,
    state: Mutex<State>,
}

/// Unwraps 32-bit RTP timestamps into a continuous i64.
#[derive(Default)]
struct Unwrap {
    last: Option<u32>,
    acc: i64,
}

impl Unwrap {
    fn push(&mut self, ts: u32) -> i64 {
        if let Some(l) = self.last {
            self.acc += ts.wrapping_sub(l) as i32 as i64;
        }
        self.last = Some(ts);
        self.acc
    }
}

/// RFC 3640 AAC-hbr: AU-headers-length (bits), 16-bit headers (13-bit size, 3-bit index), AUs.
pub fn aac_frames(payload: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if payload.len() < 2 {
        return out;
    }
    let header_bits = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let n = header_bits / 16;
    let mut data = 2 + header_bits.div_ceil(8);
    for i in 0..n {
        let h = u16::from_be_bytes([payload[2 + 2 * i], payload[3 + 2 * i]]);
        let size = (h >> 3) as usize;
        if data + size > payload.len() {
            break;
        }
        out.push(&payload[data..data + size]);
        data += size;
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

impl Recorder {
    pub fn new(url: String) -> Arc<Recorder> {
        let r = Arc::new(Recorder {
            url,
            enabled: AtomicBool::new(false),
            audio_enabled: AtomicBool::new(true),
            prebuffer_ms: AtomicU32::new(4000),
            state: Mutex::default(),
        });
        let r2 = r.clone();
        std::thread::spawn(move || r2.run());
        r
    }

    fn run(&self) {
        loop {
            if !self.enabled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            crate::log("recording: prebuffering".into());
            if let Err(e) = self.pull() {
                crate::log(format!("recording: {e}"));
            }
            *self.state.lock().unwrap() = State::default();
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    fn pull(&self) -> Result<(), String> {
        let (mut c, tracks) = crate::rtsp::Client::connect(&self.url)?;
        let v = tracks.iter().find(|t| t.codec == "h264").ok_or("no H.264 track")?;
        c.setup(v, 0)?;
        let aac = tracks.iter().find(|t| t.codec == "mpeg4-generic" && t.fmtp.iter().any(|(k, v)| k == "mode" && v.eq_ignore_ascii_case("AAC-hbr")));
        {
            let mut st = self.state.lock().unwrap();
            for p in &v.params {
                match p.first().map(|b| b & 0x1f) {
                    Some(7) => st.sps = Some(p.clone()),
                    Some(8) => st.pps = Some(p.clone()),
                    _ => {}
                }
            }
            if let Some(a) = aac {
                let asc = a.fmtp.iter().find(|(k, _)| k == "config").and_then(|(_, v)| unhex(v));
                st.audio = asc.map(|asc| AudioInfo { asc, rate: a.clock, channels: a.channels as u16 });
            }
        }
        if aac.is_some() {
            c.setup(aac.unwrap(), 2)?;
        }
        c.play()?;
        let mut depack = Depacketizer::default();
        let (mut nals, mut au, mut au_key, mut au_ts) = (Vec::new(), Vec::<u8>::new(), false, 0i64);
        let (mut vts, mut ats) = (Unwrap::default(), Unwrap::default());
        while self.enabled.load(Ordering::Relaxed) {
            let (ch, data) = c.next()?;
            if ch == 2 && data.len() > 12 {
                let ts = ats.push(u32::from_be_bytes(data[4..8].try_into().unwrap()));
                for (i, f) in aac_frames(&data[12..]).into_iter().enumerate() {
                    self.push(Media { video: false, ts: ts + 1024 * i as i64, data: f.to_vec(), key: true, arrival: Instant::now() });
                }
                continue;
            }
            if ch != 0 {
                continue;
            }
            depack.push(&data, &mut nals);
            for n in nals.drain(..) {
                let ty = n.data[0] & 0x1f;
                au_ts = vts.push(n.ts);
                match ty {
                    7 => self.state.lock().unwrap().sps = Some(n.data.clone()),
                    8 => self.state.lock().unwrap().pps = Some(n.data.clone()),
                    9 => {}
                    _ => {
                        au_key |= ty == 5;
                        au.extend_from_slice(&(n.data.len() as u32).to_be_bytes());
                        au.extend_from_slice(&n.data);
                    }
                }
                if n.last && !au.is_empty() {
                    self.push(Media { video: true, ts: au_ts, data: std::mem::take(&mut au), key: au_key, arrival: Instant::now() });
                    au_key = false;
                }
            }
        }
        Ok(())
    }

    fn push(&self, m: Media) {
        let m = Arc::new(m);
        let mut st = self.state.lock().unwrap();
        st.live.retain(|tx| tx.send(m.clone()).is_ok());
        st.buffer.push_back(m);
        // keep from the last keyframe that is at least `prebuffer` old
        let horizon = Duration::from_millis(self.prebuffer_ms.load(Ordering::Relaxed) as u64);
        let cut = st.buffer.iter().rposition(|x| x.video && x.key && x.arrival.elapsed() >= horizon);
        if let Some(i) = cut {
            st.buffer.drain(..i);
        }
        while st.buffer.len() > 4000 {
            st.buffer.pop_front();
        }
    }

    /// The init segment and a channel of fragments (prebuffer first, then live).
    pub fn start(&self) -> Result<(Vec<u8>, Receiver<Vec<u8>>), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (prebuffer, video, audio) = {
            let mut st = self.state.lock().unwrap();
            let (sps, pps) = (st.sps.clone().ok_or("no video yet")?, st.pps.clone().ok_or("no video yet")?);
            let first_key = st.buffer.iter().position(|m| m.video && m.key).ok_or("no keyframe buffered yet")?;
            let prebuffer: Vec<Arc<Media>> = st.buffer.iter().skip(first_key).cloned().collect();
            st.live.push(tx);
            let (width, height) = mp4::sps_size(&sps).unwrap_or((1920, 1080));
            let audio = if self.audio_enabled.load(Ordering::Relaxed) {
                st.audio.as_ref().map(|a| AudioInfo { asc: a.asc.clone(), rate: a.rate, channels: a.channels })
            } else {
                None
            };
            (prebuffer, VideoInfo { sps, pps, width, height }, audio)
        };
        let init = mp4::init(&video, audio.as_ref());
        let (ftx, frx) = sync_channel(8);
        let rate = audio.as_ref().map(|a| a.rate);
        std::thread::spawn(move || mux(prebuffer, rx, rate, ftx));
        Ok((init, frx))
    }
}

/// Times that tie the fragments together: the first video timestamp, and where audio starts.
struct Base {
    v0: i64,
    a0: Option<i64>,
    a_offset: i64,
}

/// One fragment from the buffered access units (the open one ends where `next_ts` starts).
fn fragment(seq: u32, base: &Base, video: &[Arc<Media>], audio: &[Arc<Media>], next_ts: i64, with_audio: bool) -> Vec<u8> {
    let vs: Vec<Sample> = video
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let next = video.get(i + 1).map(|n| n.ts).unwrap_or(next_ts);
            Sample { data: m.data.clone(), duration: (next - m.ts).clamp(1, 90000) as u32, key: m.key }
        })
        .collect();
    let aus: Vec<Sample> = audio.iter().map(|m| Sample { data: m.data.clone(), duration: 1024, key: true }).collect();
    let a_dt = audio.first().map(|m| (m.ts - base.a0.unwrap_or(m.ts) + base.a_offset).max(0) as u64).unwrap_or(0);
    let mut runs = vec![Run { track: 1, decode_time: (video[0].ts - base.v0).max(0) as u64, samples: &vs }];
    if with_audio {
        runs.push(Run { track: 2, decode_time: a_dt, samples: &aus });
    }
    mp4::fragment(seq, &runs)
}

/// Media → fragments, cut at each keyframe after the first.
fn mux(prebuffer: Vec<Arc<Media>>, live: Receiver<Arc<Media>>, audio_rate: Option<u32>, out: SyncSender<Vec<u8>>) {
    let mut seq = 1u32;
    let (mut video, mut audio): (Vec<Arc<Media>>, Vec<Arc<Media>>) = (Vec::new(), Vec::new());
    let mut base: Option<(Base, Instant)> = None;
    let items = prebuffer.into_iter().chain(std::iter::from_fn(|| live.recv_timeout(Duration::from_secs(10)).ok()));
    for m in items {
        if m.video {
            if base.is_none() {
                if !m.key {
                    continue;
                }
                base = Some((Base { v0: m.ts, a0: None, a_offset: 0 }, m.arrival));
            }
            if m.key && !video.is_empty() {
                let f = fragment(seq, &base.as_ref().unwrap().0, &video, &audio, m.ts, audio_rate.is_some());
                seq += 1;
                video.clear();
                audio.clear();
                if out.send(f).is_err() {
                    return;
                }
            }
            video.push(m);
        } else if let (Some(rate), Some((b, v_arrival))) = (audio_rate, base.as_mut()) {
            if m.arrival < *v_arrival {
                continue;
            }
            if b.a0.is_none() {
                b.a0 = Some(m.ts);
                b.a_offset = ((m.arrival - *v_arrival).as_secs_f64() * rate as f64) as i64;
            }
            audio.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_hbr_frames() {
        // two AUs of 3 and 2 bytes: headers-length 32 bits, sizes << 3
        let p = [0x00, 0x20, 0x00, 0x18, 0x00, 0x10, 1, 2, 3, 4, 5];
        assert_eq!(aac_frames(&p), vec![&[1u8, 2, 3][..], &[4, 5][..]]);
    }

    #[test]
    fn unwraps_rtp_time() {
        let mut u = Unwrap::default();
        assert_eq!(u.push(u32::MAX - 10), 0);
        assert_eq!(u.push(5), 16);
    }

    #[test]
    fn fragments_cut_at_keyframes() {
        let now = Instant::now();
        let m = |ts: i64, key: bool| Arc::new(Media { video: true, ts, data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }], key, arrival: now });
        let pre = vec![m(0, true), m(3000, false), m(6000, true), m(9000, false)];
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        let (ftx, frx) = sync_channel(8);
        mux(pre, rx, None, ftx);
        let frags: Vec<Vec<u8>> = frx.try_iter().collect();
        assert_eq!(frags.len(), 1, "the open fragment waits for the next keyframe");
        assert!(frags[0].windows(4).any(|w| w == b"moof"));
    }
}
