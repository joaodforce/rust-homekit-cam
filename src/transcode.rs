//! Optional camera audio → Opus (`transcode_audio = true`), for cameras whose RTSP audio is
//! G.711 (PCMU/PCMA) or AAC-LC: HomeKit's live view only plays Opus (or AAC-ELD). Video is never
//! touched. Done in-process: G.711 by table, AAC-LC with Symphonia (pure Rust), Opus with the
//! bundled libopus; mono speech at 16/24 kHz costs well under 1% of a core.

use crate::rtsp::Track;
use rand_core::{OsRng, RngCore};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

enum Decoder {
    Ulaw,
    Alaw,
    Aac(Box<symphonia_codec_aac::AacDecoder>),
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
    channels: usize,
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
        let (dec, in_rate, channels) = match t.codec.as_str() {
            "pcmu" => (Decoder::Ulaw, t.clock.max(8000), 1),
            "pcma" => (Decoder::Alaw, t.clock.max(8000), 1),
            _ if is_aac_hbr(t) => {
                let asc = t.fmtp.iter().find(|(k, _)| k == "config").and_then(|(_, v)| crate::recording::unhex(v)).ok_or("AAC track without config")?;
                let mut p = AudioCodecParameters::new();
                p.for_codec(CODEC_ID_AAC).with_extra_data(asc.into_boxed_slice());
                let d = symphonia_codec_aac::AacDecoder::try_new(&p, &AudioDecoderOptions::default()).map_err(|e| format!("AAC: {e}"))?;
                (Decoder::Aac(Box::new(d)), t.clock, t.channels.max(1) as usize)
            }
            c => return Err(format!("can't transcode {c}")),
        };
        let rate = if matches!(rate, 8000 | 12000 | 16000 | 24000 | 48000) { rate } else { 16000 };
        // Opus frames are 10/20/40/60 ms; HomeKit asks 20, 30, 40 or 60
        let ms = if matches!(ptime, 20 | 40 | 60) { ptime } else { 20 };
        let mut enc = opus::Encoder::new(rate, opus::Channels::Mono, opus::Application::Voip).map_err(|e| format!("opus: {e}"))?;
        let _ = enc.set_bitrate(opus::Bitrate::Bits(24_000));
        Ok(Transcoder {
            dec,
            in_rate,
            channels,
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
        let c = match self.dec {
            Decoder::Ulaw => "PCMU",
            Decoder::Alaw => "PCMA",
            Decoder::Aac(_) => "AAC-LC",
        };
        format!("{c} {} Hz → Opus {} Hz, {} ms", self.in_rate, self.rs.to, self.frame as u32 * 1000 / self.rs.to)
    }

    /// One incoming RTP packet → zero or more Opus RTP packets.
    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Vec<u8>>) {
        let Some(payload) = rtp_payload(pkt) else { return };
        self.scratch.clear();
        match &mut self.dec {
            Decoder::Ulaw => self.scratch.extend(payload.iter().map(|&b| ulaw(b))),
            Decoder::Alaw => self.scratch.extend(payload.iter().map(|&b| alaw(b))),
            Decoder::Aac(d) => {
                for f in crate::recording::aac_frames(payload) {
                    if let Ok(buf) = d.decode_ref(&PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, f)) {
                        let mut s: Vec<i16> = Vec::new();
                        buf.copy_to_vec_interleaved(&mut s);
                        // downmix to mono
                        let ch = self.channels;
                        self.scratch.extend(s.chunks(ch).map(|c| (c.iter().map(|&x| x as i32).sum::<i32>() / c.len() as i32) as i16));
                    }
                }
            }
        }
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
}
