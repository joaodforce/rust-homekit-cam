//! Optional audio transcoding (`transcode_audio = true`); video is never touched.
//!
//! - Live view plays only Opus (or AAC-ELD): G.711 (PCMU/PCMA) or AAC-LC camera audio → Opus.
//! - Recordings (HKSV) take only AAC: G.711 or Opus camera audio → AAC-LC 16 kHz mono.
//!
//! All in-process: G.711 by table, AAC-LC decoding with Symphonia and encoding with oxideav-aac
//! (pure Rust), Opus with the bundled libopus. Mono speech costs well under 1% of a core.

use crate::rtsp::Track;
use rand_core::{OsRng, RngCore};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Camera audio (one RTP payload at a time) → mono PCM at `rate`.
enum Decoder {
    Ulaw,
    Alaw,
    Aac(Box<symphonia_codec_aac::AacDecoder>, usize),
    Opus(opus::Decoder),
}

impl Decoder {
    /// The decoder for a track and the sample rate of its output.
    fn for_track(t: &Track) -> Result<(Decoder, u32), String> {
        Ok(match t.codec.as_str() {
            "pcmu" => (Decoder::Ulaw, t.clock.max(8000)),
            "pcma" => (Decoder::Alaw, t.clock.max(8000)),
            "opus" => (Decoder::Opus(opus::Decoder::new(16000, opus::Channels::Mono).map_err(|e| format!("opus: {e}"))?), 16000),
            _ if is_aac_hbr(t) => {
                let asc = t.fmtp.iter().find(|(k, _)| k == "config").and_then(|(_, v)| crate::recording::unhex(v)).ok_or("AAC track without config")?;
                let mut p = AudioCodecParameters::new();
                p.for_codec(CODEC_ID_AAC).with_extra_data(asc.into_boxed_slice());
                let d = symphonia_codec_aac::AacDecoder::try_new(&p, &AudioDecoderOptions::default()).map_err(|e| format!("AAC: {e}"))?;
                (Decoder::Aac(Box::new(d), t.channels.max(1) as usize), t.clock)
            }
            c => return Err(format!("can't decode {c}")),
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Decoder::Ulaw => "PCMU",
            Decoder::Alaw => "PCMA",
            Decoder::Aac(..) => "AAC-LC",
            Decoder::Opus(_) => "Opus",
        }
    }

    fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) {
        match self {
            Decoder::Ulaw => out.extend(payload.iter().map(|&b| ulaw(b))),
            Decoder::Alaw => out.extend(payload.iter().map(|&b| alaw(b))),
            Decoder::Opus(d) => {
                let mut pcm = [0i16; 1920]; // 120 ms at 16 kHz
                if let Ok(n) = d.decode(payload, &mut pcm, false) {
                    out.extend_from_slice(&pcm[..n]);
                }
            }
            Decoder::Aac(d, ch) => {
                for f in crate::recording::aac_frames(payload) {
                    if let Ok(buf) = d.decode_ref(&PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, f)) {
                        let mut s: Vec<i16> = Vec::new();
                        buf.copy_to_vec_interleaved(&mut s);
                        // downmix to mono
                        out.extend(s.chunks(*ch).map(|c| (c.iter().map(|&x| x as i32).sum::<i32>() / c.len() as i32) as i16));
                    }
                }
            }
        }
    }
}

/// Codecs we can transcode, as `Track::codec` names them.
pub fn supported(t: &Track) -> bool {
    matches!(t.codec.as_str(), "pcmu" | "pcma") || is_aac_hbr(t)
}

fn is_aac_hbr(t: &Track) -> bool {
    t.codec == "mpeg4-generic" && t.fmtp.iter().any(|(k, v)| k == "mode" && v.eq_ignore_ascii_case("AAC-hbr"))
}

pub struct Transcoder {
    dec: Decoder,
    in_rate: u32,
    rs: Resampler,
    enc: opus::Encoder,
    /// samples per Opus frame at the output rate
    frame: usize,
    pcm: Vec<i16>,
    scratch: Vec<i16>,
    out: Vec<u8>,
    pt: u8,
    ssrc: u32,
    seq: u16,
    ts: u32,
    pub last_ts: u32,
}

impl Transcoder {
    /// `rate`: the controller's Opus sample rate (also our RTP clock), `ptime` its packet time.
    pub fn new(t: &Track, pt: u8, ssrc: u32, ptime: u32, rate: u32) -> Result<Self, String> {
        if !supported(t) {
            return Err(format!("can't transcode {} to Opus", t.codec));
        }
        let (dec, in_rate) = Decoder::for_track(t)?;
        let rate = if matches!(rate, 8000 | 12000 | 16000 | 24000 | 48000) { rate } else { 16000 };
        // Opus frames are 10/20/40/60 ms; HomeKit asks 20, 30, 40 or 60
        let ms = if matches!(ptime, 20 | 40 | 60) { ptime } else { 20 };
        let mut enc = opus::Encoder::new(rate, opus::Channels::Mono, opus::Application::Voip).map_err(|e| format!("opus: {e}"))?;
        let _ = enc.set_bitrate(opus::Bitrate::Bits(24_000));
        Ok(Transcoder {
            dec,
            in_rate,
            rs: Resampler::new(in_rate, rate),
            enc,
            frame: (rate * ms / 1000) as usize,
            pcm: Vec::new(),
            scratch: Vec::new(),
            out: vec![0; 1500],
            pt,
            ssrc,
            seq: OsRng.next_u32() as u16,
            ts: OsRng.next_u32(),
            last_ts: 0,
        })
    }

    pub fn describe(&self) -> String {
        format!("{} {} Hz → Opus {} Hz, {} ms", self.dec.name(), self.in_rate, self.rs.to, self.frame as u32 * 1000 / self.rs.to)
    }

    /// One incoming RTP packet → zero or more Opus RTP packets.
    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Vec<u8>>) {
        let Some(payload) = rtp_payload(pkt) else { return };
        self.scratch.clear();
        self.dec.decode(payload, &mut self.scratch);
        self.rs.run(&self.scratch, &mut self.pcm);
        let mut used = 0;
        while self.pcm.len() - used >= self.frame {
            let Ok(n) = self.enc.encode(&self.pcm[used..used + self.frame], &mut self.out) else { break };
            used += self.frame;
            let mut p = vec![0x80, self.pt];
            p.extend_from_slice(&self.seq.to_be_bytes());
            p.extend_from_slice(&self.ts.to_be_bytes());
            p.extend_from_slice(&self.ssrc.to_be_bytes());
            p.extend_from_slice(&self.out[..n]);
            out.push(p);
            self.last_ts = self.ts;
            self.seq = self.seq.wrapping_add(1);
            self.ts = self.ts.wrapping_add(self.frame as u32);
        }
        self.pcm.drain(..used);
    }
}

/// G.711 or Opus camera audio → AAC-LC 16 kHz mono frames (raw, no ADTS) for recordings.
pub struct ToAac {
    dec: Decoder,
    rs: Resampler,
    enc: oxideav_aac::encoder::StreamEncoder,
    pcm: Vec<i16>,
    scratch: Vec<i16>,
}

/// The AudioSpecificConfig of what `ToAac` makes: AAC-LC (2), 16 kHz (index 8), mono.
pub const TO_AAC_ASC: [u8; 2] = [0x14, 0x08];
pub const TO_AAC_RATE: u32 = 16000;

impl ToAac {
    /// Tracks worth encoding for recordings (AAC itself is copied as is).
    pub fn wants(t: &Track) -> bool {
        matches!(t.codec.as_str(), "pcmu" | "pcma" | "opus")
    }

    pub fn new(t: &Track) -> Result<Self, String> {
        let (dec, rate) = Decoder::for_track(t)?;
        let cfg = oxideav_aac::encoder::EncoderConfig { sample_rate: TO_AAC_RATE, channels: 1, bitrate: 32_000 };
        let enc = oxideav_aac::encoder::StreamEncoder::new(cfg).map_err(|e| format!("AAC encoder: {e}"))?;
        Ok(ToAac { dec, rs: Resampler::new(rate, TO_AAC_RATE), enc, pcm: Vec::new(), scratch: Vec::new() })
    }

    pub fn describe(&self) -> String {
        format!("{} {} Hz → AAC-LC {TO_AAC_RATE} Hz", self.dec.name(), self.rs.from)
    }

    /// One incoming RTP packet → zero or more AAC frames of 1024 samples.
    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Vec<u8>>) {
        let Some(payload) = rtp_payload(pkt) else { return };
        self.scratch.clear();
        self.dec.decode(payload, &mut self.scratch);
        self.rs.run(&self.scratch, &mut self.pcm);
        let mut used = 0;
        while self.pcm.len() - used >= 1024 {
            if let Ok(adts) = self.enc.encode_frame(&self.pcm[used..used + 1024]) {
                // strip the ADTS header: 7 bytes, 9 with a CRC
                let hdr = if adts.len() > 1 && adts[1] & 1 == 0 { 9 } else { 7 };
                if adts.len() > hdr {
                    out.push(adts[hdr..].to_vec());
                }
            }
            used += 1024;
        }
        self.pcm.drain(..used);
    }
}

/// Pull `url` for `secs`, transcode its audio as a live view would, decode the Opus back: a
/// summary (packets, timestamp steps, level) for `rust-homekit-cam audio-test`.
pub fn selftest(url: &str, rate: u32, ptime: u32, secs: u64) -> Result<String, String> {
    let (mut c, tracks) = crate::rtsp::Client::connect(url)?;
    let t = tracks.iter().find(|t| supported(t)).ok_or_else(|| {
        format!("no G.711/AAC track (tracks: {:?})", tracks.iter().map(|t| t.codec.as_str()).collect::<Vec<_>>())
    })?;
    let mut tc = Transcoder::new(t, 110, 1, ptime, rate)?;
    let desc = tc.describe();
    c.setup(t, 2)?;
    c.play()?;
    let mut dec = opus::Decoder::new(tc.rs.to, opus::Channels::Mono).map_err(|e| e.to_string())?;
    let (mut out, mut pcm) = (Vec::new(), vec![0i16; 5760]);
    let (mut n, mut bytes, mut peak, mut sq, mut samples) = (0u32, 0usize, 0i32, 0f64, 0u64);
    let mut steps = std::collections::BTreeMap::<u32, u32>::new();
    let mut last: Option<u32> = None;
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < secs {
        let (ch, data) = c.next()?;
        if ch != 2 {
            continue;
        }
        tc.push(&data, &mut out);
        for p in out.drain(..) {
            let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
            if let Some(l) = last {
                *steps.entry(ts.wrapping_sub(l)).or_default() += 1;
            }
            last = Some(ts);
            n += 1;
            bytes += p.len() - 12;
            let k = dec.decode(&p[12..], &mut pcm, false).map_err(|e| format!("decoding our Opus: {e}"))?;
            for &s in &pcm[..k] {
                peak = peak.max((s as i32).abs());
                sq += (s as f64) * (s as f64);
            }
            samples += k as u64;
        }
    }
    let secs_f = start.elapsed().as_secs_f64();
    let rms = if samples > 0 { (sq / samples as f64).sqrt() } else { 0.0 };
    Ok(format!(
        "{desc}: {n} packets ({:.1}/s, expected {:.1}), {:.1} s of audio in {:.1} s, {:.0} kbps, ts steps {steps:?}, peak {peak}, rms {rms:.0}",
        n as f64 / secs_f,
        1000.0 / ptime as f64,
        samples as f64 / tc.rs.to as f64,
        secs_f,
        bytes as f64 * 8.0 / secs_f / 1000.0
    ))
}

fn rtp_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 13 || pkt[0] >> 6 != 2 {
        return None;
    }
    let mut off = 12 + 4 * (pkt[0] & 0x0f) as usize;
    if pkt[0] & 0x10 != 0 && pkt.len() >= off + 4 {
        off += 4 + 4 * u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
    }
    let mut end = pkt.len();
    if pkt[0] & 0x20 != 0 {
        end = end.saturating_sub(pkt[pkt.len() - 1] as usize);
    }
    (off < end).then(|| &pkt[off..end])
}

/// G.711 μ-law → 16-bit linear.
fn ulaw(u: u8) -> i16 {
    let u = !u;
    let t = ((((u & 0x0f) as i32) << 3) + 0x84) << ((u & 0x70) >> 4);
    (if u & 0x80 != 0 { 0x84 - t } else { t - 0x84 }) as i16
}

/// G.711 A-law → 16-bit linear.
fn alaw(a: u8) -> i16 {
    let a = a ^ 0x55;
    let seg = (a & 0x70) >> 4;
    let mut t = ((a & 0x0f) as i32) << 4;
    t = match seg {
        0 => t + 8,
        _ => (t + 0x108) << (seg - 1),
    };
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// Linear-interpolation resampler, continuous across calls (fine for speech).
struct Resampler {
    from: u32,
    to: u32,
    /// position of the next output sample, in input samples, relative to `prev`
    pos: f64,
    prev: i16,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler { from, to, pos: 1.0, prev: 0 }
    }

    fn run(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.from == self.to {
            return out.extend_from_slice(input);
        }
        if input.is_empty() {
            return;
        }
        let step = self.from as f64 / self.to as f64;
        // sample i of the block is at position i + 1 (0 = `prev`, the last of the previous block)
        let at = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        while self.pos <= input.len() as f64 {
            let i = self.pos.floor() as usize;
            let f = self.pos - i as f64;
            let a = at(i) as f64;
            let b = if i < input.len() { input[i] as f64 } else { a };
            out.push((a + (b - a) * f) as i16);
            self.pos += step;
        }
        self.pos -= input.len() as f64;
        self.prev = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_tables() {
        assert_eq!(ulaw(0xff), 0);
        assert_eq!(ulaw(0x00), -32124);
        assert_eq!(ulaw(0x80), 32124);
        assert_eq!(alaw(0xd5), 8);
        assert_eq!(alaw(0x55), -8);
        assert_eq!(alaw(0xaa), 32256);
    }

    #[test]
    fn resampler_ratio_holds_across_blocks() {
        let mut r = Resampler::new(8000, 16000);
        let mut out = Vec::new();
        for _ in 0..50 {
            r.run(&[100; 160], &mut out);
        }
        assert!((out.len() as i32 - 16000).abs() <= 2, "{}", out.len());
        assert!(out[10..].iter().all(|&s| s == 100));
    }

    fn track(codec: &str, clock: u32) -> Track {
        Track { control: String::new(), codec: codec.into(), payload_type: 0, clock, params: vec![], channels: 1, fmtp: vec![] }
    }

    #[test]
    fn pcmu_8k_to_opus_16k_20ms() {
        let mut t = Transcoder::new(&track("pcmu", 8000), 110, 7, 20, 16000).unwrap();
        let mut out = Vec::new();
        // 1 s of a 440 Hz tone in 20 ms packets
        for k in 0..50u32 {
            let mut p = vec![0x80, 0, 0, k as u8, 0, 0, 0, 0, 0, 0, 0, 1];
            p.extend((0..160).map(|i| {
                let s = ((k * 160 + i) as f32 * 440.0 * std::f32::consts::TAU / 8000.0).sin() * 8000.0;
                linear_to_ulaw(s as i16)
            }));
            t.push(&p, &mut out);
        }
        assert!((49..=50).contains(&out.len()), "{}", out.len());
        let ts: Vec<u32> = out.iter().map(|p| u32::from_be_bytes(p[4..8].try_into().unwrap())).collect();
        assert!(ts.windows(2).all(|w| w[1].wrapping_sub(w[0]) == 320));
        assert!(out.iter().all(|p| p[1] == 110 && p.len() > 13));
        // and it decodes back to a tone
        let mut d = opus::Decoder::new(16000, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0i16; 960];
        let n = d.decode(&out[30][12..], &mut pcm, false).unwrap();
        assert_eq!(n, 320);
        assert!(pcm[..n].iter().map(|&s| s.unsigned_abs() as u32).max().unwrap() > 3000);
    }

    pub fn linear_to_ulaw_pub(s: i16) -> u8 {
        linear_to_ulaw(s)
    }

    fn linear_to_ulaw(s: i16) -> u8 {
        (0..=255u8).min_by_key(|&u| (ulaw(u) as i32 - s as i32).abs()).unwrap()
    }

    #[test]
    fn aac_track_needs_config() {
        let mut t = track("mpeg4-generic", 16000);
        t.fmtp = vec![("mode".into(), "AAC-hbr".into())];
        assert!(supported(&t));
        assert!(Transcoder::new(&t, 110, 1, 20, 16000).is_err());
        t.fmtp.push(("config".into(), "1408".into()));
        assert!(Transcoder::new(&t, 110, 1, 20, 16000).is_ok());
    }

    #[test]
    fn opus_and_pcmu_to_aac_roundtrip() {
        // 2 s of a 440 Hz tone as PCMU, encoded to AAC and decoded back with Symphonia
        let mut t = ToAac::new(&track("pcmu", 8000)).unwrap();
        let mut frames = Vec::new();
        for k in 0..100u32 {
            let mut p = vec![0x80, 0, 0, k as u8, 0, 0, 0, 0, 0, 0, 0, 1];
            p.extend((0..160).map(|i| linear_to_ulaw((((k * 160 + i) as f32 * 440.0 * std::f32::consts::TAU / 8000.0).sin() * 8000.0) as i16)));
            t.push(&p, &mut frames);
        }
        assert_eq!(frames.len(), 31); // 32000 samples at 16 kHz / 1024
        let mut aac = track("mpeg4-generic", 16000);
        aac.fmtp = vec![("mode".into(), "AAC-hbr".into()), ("config".into(), "1408".into())];
        let (mut d, _) = Decoder::for_track(&aac).unwrap();
        let mut pcm = Vec::new();
        for f in &frames {
            // wrap as an AAC-hbr RTP payload: one AU header
            let mut p = vec![0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x00, 0x10];
            p.extend_from_slice(&((f.len() as u16) << 3).to_be_bytes());
            p.extend_from_slice(f);
            d.decode(rtp_payload(&p).unwrap(), &mut pcm);
        }
        assert_eq!(pcm.len(), 31 * 1024);
        let peak = pcm[4096..].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 5000 && peak < 12000, "peak {peak}");
        // zero crossings ≈ 2 × 440 per second
        let z = pcm[4096..20480].windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!((z as i32 - 880).abs() < 30, "{z} crossings");

        // Opus in: encode a tone with libopus first
        let mut enc = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut t = ToAac::new(&track("opus", 48000)).unwrap();
        let mut frames = Vec::new();
        for k in 0..100u32 {
            let pcm: Vec<i16> = (0..960).map(|i| (((k * 960 + i) as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 8000.0) as i16).collect();
            let mut p = vec![0x80, 0, 0, k as u8, 0, 0, 0, 0, 0, 0, 0, 1];
            p.extend(enc.encode_vec(&pcm, 400).unwrap());
            t.push(&p, &mut frames);
        }
        assert_eq!(frames.len(), 31);
    }
}

#[cfg(test)]
mod bench {
    #[test]
    #[ignore]
    fn to_aac_speed() {
        for (name, tns_pns) in [("defaults", true), ("no TNS/PNS", false)] {
            for (sig, f) in [("noise-ish", 0u32), ("tone", 1), ("silence", 2)] {
                let t = super::tests_track("pcmu");
                let mut e = super::ToAac::new(&t).unwrap();
                e.enc.set_tns(tns_pns);
                e.enc.set_pns(tns_pns);
                let mut out = Vec::new();
                let start = std::time::Instant::now();
                for k in 0..3000u32 {
                    let mut p = vec![0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
                    p.extend((0..160).map(|i| match f {
                        0 => ((k * 160 + i).wrapping_mul(2654435761) >> 24) as u8,
                        1 => super::tests::linear_to_ulaw_pub((((k * 160 + i) as f32 * 300.0 * std::f32::consts::TAU / 8000.0).sin() * 6000.0) as i16),
                        _ => 0xff,
                    }));
                    e.push(&p, &mut out);
                }
                eprintln!("{name}, {sig}: 60 s → AAC in {:?}", start.elapsed());
            }
        }
    }
}
#[cfg(test)]
fn tests_track(codec: &str) -> Track {
    Track { control: String::new(), codec: codec.into(), payload_type: 0, clock: 8000, params: vec![], channels: 1, fmtp: vec![] }
}
