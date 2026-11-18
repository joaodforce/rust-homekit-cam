//! Fragmented MP4 for HomeKit Secure Video: an init segment (ftyp + moov) and fragments
//! (moof + mdat). Samples are the camera's own H.264 access units (AVCC: 4-byte lengths) and
//! raw AAC frames; nothing is decoded.

/// One sample: its data, duration (track timescale) and whether it is a sync sample.
pub struct Sample {
    pub data: Vec<u8>,
    pub duration: u32,
    pub key: bool,
}

pub struct VideoInfo {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

pub struct AudioInfo {
    /// AudioSpecificConfig (e.g. 0x14 0x08: AAC-LC, 16 kHz, mono)
    pub asc: Vec<u8>,
    pub rate: u32,
    pub channels: u16,
}

pub const VIDEO_TIMESCALE: u32 = 90000;

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

/// Full box: version + flags in front of the body.
fn fbx(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut v = vec![version];
    v.extend_from_slice(&flags.to_be_bytes()[1..]);
    v.extend_from_slice(body);
    bx(kind, &v)
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn tkhd(track: u32, width: u16, height: u16, audio: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0; 8]); // creation, modification
    b.extend_from_slice(&track.to_be_bytes());
    b.extend_from_slice(&[0; 4]); // reserved
    b.extend_from_slice(&[0; 4]); // duration
    b.extend_from_slice(&[0; 8]); // reserved
    b.extend_from_slice(&[0; 4]); // layer, alternate group
    b.extend_from_slice(&(if audio { 0x0100u16 } else { 0 }).to_be_bytes()); // volume
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&matrix());
    b.extend_from_slice(&((width as u32) << 16).to_be_bytes());
    b.extend_from_slice(&((height as u32) << 16).to_be_bytes());
    fbx(b"tkhd", 0, 3, &b) // enabled, in movie
}

fn mdhd(timescale: u32) -> Vec<u8> {
    let mut b = vec![0; 8];
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&[0; 4]); // duration
    b.extend_from_slice(&0x55c4u16.to_be_bytes()); // language "und"
    b.extend_from_slice(&[0; 2]);
    fbx(b"mdhd", 0, 0, &b)
}

fn hdlr(kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = vec![0; 4];
    b.extend_from_slice(kind);
    b.extend_from_slice(&[0; 12]);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    fbx(b"hdlr", 0, 0, &b)
}

fn dinf() -> Vec<u8> {
    let url = fbx(b"url ", 0, 1, &[]);
    bx(b"dinf", &fbx(b"dref", 0, 0, &cat(&[&1u32.to_be_bytes(), &url])))
}

fn empty_tables() -> Vec<u8> {
    cat(&[
        &fbx(b"stts", 0, 0, &[0; 4]),
        &fbx(b"stsc", 0, 0, &[0; 4]),
        &fbx(b"stsz", 0, 0, &[0; 8]),
        &fbx(b"stco", 0, 0, &[0; 4]),
    ])
}

fn avc1(v: &VideoInfo) -> Vec<u8> {
    let mut avcc = vec![1, v.sps[1], v.sps[2], v.sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(v.sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&v.sps);
    avcc.push(1);
    avcc.extend_from_slice(&(v.pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&v.pps);
    let mut b = vec![0; 6];
    b.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(&v.width.to_be_bytes());
    b.extend_from_slice(&v.height.to_be_bytes());
    b.extend_from_slice(&0x00480000u32.to_be_bytes()); // 72 dpi
    b.extend_from_slice(&0x00480000u32.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&1u16.to_be_bytes()); // frame count
    b.extend_from_slice(&[0; 32]); // compressor name
    b.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    b.extend_from_slice(&0xffffu16.to_be_bytes());
    b.extend_from_slice(&bx(b"avcC", &avcc));
    bx(b"avc1", &b)
}

/// MPEG-4 descriptor: tag, length (4-byte form), body.
fn desc(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len() as u32;
    let mut d = vec![tag, 0x80 | (n >> 21) as u8 & 0x7f, 0x80 | (n >> 14) as u8 & 0x7f, 0x80 | (n >> 7) as u8 & 0x7f, n as u8 & 0x7f];
    d.extend_from_slice(body);
    d
}

fn mp4a(a: &AudioInfo) -> Vec<u8> {
    let dsi = desc(0x05, &a.asc);
    let mut dcd = vec![0x40, 0x15, 0, 0, 0]; // AAC, audio stream, buffer size
    dcd.extend_from_slice(&64000u32.to_be_bytes()); // max bitrate
    dcd.extend_from_slice(&0u32.to_be_bytes()); // avg bitrate
    dcd.extend_from_slice(&dsi);
    let es = desc(0x03, &cat(&[&[0, 2, 0], &desc(0x04, &dcd), &desc(0x06, &[2])]));
    let mut b = vec![0; 6];
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&a.channels.to_be_bytes());
    b.extend_from_slice(&16u16.to_be_bytes()); // sample size
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&(a.rate << 16).to_be_bytes());
    b.extend_from_slice(&fbx(b"esds", 0, 0, &es));
    bx(b"mp4a", &b)
}

fn trak(track: u32, timescale: u32, entry: Vec<u8>, w: u16, h: u16, audio: bool) -> Vec<u8> {
    let media_header = if audio { fbx(b"smhd", 0, 0, &[0; 4]) } else { fbx(b"vmhd", 0, 1, &[0; 8]) };
    let stsd = fbx(b"stsd", 0, 0, &cat(&[&1u32.to_be_bytes(), &entry]));
    let stbl = bx(b"stbl", &cat(&[&stsd, &empty_tables()]));
    let minf = bx(b"minf", &cat(&[&media_header, &dinf(), &stbl]));
    let (kind, name) = if audio { (b"soun", "SoundHandler") } else { (b"vide", "VideoHandler") };
    let mdia = bx(b"mdia", &cat(&[&mdhd(timescale), &hdlr(kind, name), &minf]));
    bx(b"trak", &cat(&[&tkhd(track, w, h, audio), &mdia]))
}

fn trex(track: u32) -> Vec<u8> {
    let mut b = track.to_be_bytes().to_vec();
    b.extend_from_slice(&1u32.to_be_bytes()); // sample description index
    b.extend_from_slice(&[0; 12]);
    fbx(b"trex", 0, 0, &b)
}

/// ftyp + moov. Track 1 = video, track 2 = audio (if any).
pub fn init(v: &VideoInfo, a: Option<&AudioInfo>) -> Vec<u8> {
    let ftyp = bx(b"ftyp", &cat(&[b"isom", &0x200u32.to_be_bytes(), b"isom", b"iso5", b"iso6", b"mp41"]));
    let mut mvhd = vec![0; 8];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&[0; 4]); // duration
    mvhd.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    mvhd.extend_from_slice(&[0; 10]);
    mvhd.extend_from_slice(&matrix());
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&(if a.is_some() { 3u32 } else { 2 }).to_be_bytes()); // next track id
    let mut moov = fbx(b"mvhd", 0, 0, &mvhd);
    moov.extend(trak(1, VIDEO_TIMESCALE, avc1(v), v.width, v.height, false));
    let mut mvex = trex(1);
    if let Some(a) = a {
        moov.extend(trak(2, a.rate, mp4a(a), 0, 0, true));
        mvex.extend(trex(2));
    }
    moov.extend(bx(b"mvex", &mvex));
    cat(&[&ftyp, &bx(b"moov", &moov)])
}

/// One track's samples in a fragment, starting at `decode_time` (its timescale).
pub struct Run<'a> {
    pub track: u32,
    pub decode_time: u64,
    pub samples: &'a [Sample],
}

fn traf(run: &Run, data_offset: u32) -> Vec<u8> {
    let tfhd = fbx(b"tfhd", 0, 0x020000, &run.track.to_be_bytes()); // default-base-is-moof
    let tfdt = fbx(b"tfdt", 1, 0, &run.decode_time.to_be_bytes());
    let mut t = (run.samples.len() as u32).to_be_bytes().to_vec();
    t.extend_from_slice(&data_offset.to_be_bytes());
    for s in run.samples {
        t.extend_from_slice(&s.duration.to_be_bytes());
        t.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        t.extend_from_slice(&(if s.key { 0x02000000u32 } else { 0x01010000 }).to_be_bytes());
    }
    let trun = fbx(b"trun", 0, 0x000701, &t); // data offset, duration, size, flags per sample
    bx(b"traf", &cat(&[&tfhd, &tfdt, &trun]))
}

/// moof + mdat for one fragment.
pub fn fragment(sequence: u32, runs: &[Run]) -> Vec<u8> {
    let runs: Vec<&Run> = runs.iter().filter(|r| !r.samples.is_empty()).collect();
    let mfhd = fbx(b"mfhd", 0, 0, &sequence.to_be_bytes());
    // the moof's size does not depend on the offsets: build once to measure
    let size_of = |offsets: &[u32]| {
        let trafs: Vec<u8> = runs.iter().zip(offsets).flat_map(|(r, o)| traf(r, *o)).collect();
        bx(b"moof", &cat(&[&mfhd, &trafs]))
    };
    let moof_len = size_of(&vec![0; runs.len()]).len() as u32;
    let mut offsets = Vec::new();
    let mut at = moof_len + 8;
    for r in &runs {
        offsets.push(at);
        at += r.samples.iter().map(|s| s.data.len() as u32).sum::<u32>();
    }
    let moof = size_of(&offsets);
    let data: Vec<u8> = runs.iter().flat_map(|r| r.samples.iter().flat_map(|s| s.data.iter().copied())).collect();
    cat(&[&moof, &bx(b"mdat", &data)])
}

// --------------------------------------------------------------------------------- SPS size

struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.pos / 8)?;
        let v = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |a, _| Some(a << 1 | self.bit()?))
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// Width and height from an SPS NAL (with its header byte).
pub fn sps_size(sps: &[u8]) -> Option<(u16, u16)> {
    // remove emulation prevention bytes
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in &sps[1..] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { b: &rbsp, pos: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraints, level
    r.ue()?; // sps id
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max ref frames
    r.bit()?;
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        (cl, cr, ct, cb) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
    }
    let (sub_w, sub_h) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_x = if chroma == 0 { 1 } else { sub_w };
    let crop_y = (if chroma == 0 { 1 } else { sub_h }) * (2 - frame_mbs_only);
    let w = w_mbs * 16 - crop_x * (cl + cr);
    let h = h_units * 16 * (2 - frame_mbs_only) - crop_y * (ct + cb);
    Some((w as u16, h as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk top-level boxes: (type, size).
    fn boxes(b: &[u8]) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= b.len() {
            let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            out.push((String::from_utf8_lossy(&b[i + 4..i + 8]).into_owned(), n));
            assert!(n >= 8, "bad box size");
            i += n;
        }
        assert_eq!(i, b.len(), "boxes don't add up");
        out
    }

    #[test]
    fn sps_of_the_doorbell_medium_channel() {
        // Protect Doorbell Lite, Medium channel (sprop-parameter-sets)
        let sps = crate::tlv::unb64("Z01AH6aAqBxpuAgICgAAB9IAAXdhO0EQjUA=").unwrap();
        assert_eq!(sps_size(&sps), Some((672, 896)));
    }

    #[test]
    fn init_and_fragment_are_well_formed() {
        let v = VideoInfo { sps: vec![0x67, 0x4d, 0x40, 0x1f, 1], pps: vec![0x68, 2], width: 672, height: 896 };
        let a = AudioInfo { asc: vec![0x14, 0x08], rate: 16000, channels: 1 };
        let init = init(&v, Some(&a));
        assert_eq!(boxes(&init).iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["ftyp", "moov"]);
        let vs = [Sample { data: vec![0, 0, 0, 2, 0x65, 1], duration: 3750, key: true }, Sample { data: vec![0, 0, 0, 1, 0x41], duration: 3750, key: false }];
        let aus = [Sample { data: vec![9; 10], duration: 1024, key: true }];
        let f = fragment(1, &[Run { track: 1, decode_time: 0, samples: &vs }, Run { track: 2, decode_time: 0, samples: &aus }]);
        let top = boxes(&f);
        assert_eq!((top[0].0.as_str(), top[1].0.as_str()), ("moof", "mdat"));
        // the video run's data offset points at the start of mdat's payload
        let moof_len = top[0].1;
        let trun = f.windows(4).position(|w| w == b"trun").unwrap();
        let off = u32::from_be_bytes(f[trun + 12..trun + 16].try_into().unwrap()) as usize;
        assert_eq!(off, moof_len + 8);
        assert_eq!(&f[off..off + 6], &[0, 0, 0, 2, 0x65, 1]);
        // the audio run starts after the video bytes
        let trun2 = trun + 4 + f[trun + 4..].windows(4).position(|w| w == b"trun").unwrap();
        let off2 = u32::from_be_bytes(f[trun2 + 12..trun2 + 16].try_into().unwrap()) as usize;
        assert_eq!(&f[off2..off2 + 10], &[9; 10]);
    }
}
