//! H.264 over RTP (RFC 6184), no decoding: incoming packets → NAL units → packets that fit the
//! controller's MTU, with our payload type, SSRC, sequence numbers and timestamps.

/// Incoming side: reassembles NAL units from single-NAL, STAP-A and FU-A packets.
#[derive(Default)]
pub struct Depacketizer {
    fu: Option<Vec<u8>>,
}

/// One NAL unit, its RTP timestamp and whether it ends the access unit (marker).
pub struct Nal {
    pub data: Vec<u8>,
    pub ts: u32,
    pub last: bool,
}

impl Depacketizer {
    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Nal>) {
        if pkt.len() < 13 || pkt[0] >> 6 != 2 {
            return;
        }
        let cc = (pkt[0] & 0x0f) as usize;
        let mut off = 12 + 4 * cc;
        if pkt[0] & 0x10 != 0 && pkt.len() >= off + 4 {
            off += 4 + 4 * u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
        }
        let mut end = pkt.len();
        if pkt[0] & 0x20 != 0 {
            end = end.saturating_sub(*pkt.last().unwrap() as usize);
        }
        if off >= end {
            return;
        }
        let marker = pkt[1] & 0x80 != 0;
        let ts = u32::from_be_bytes(pkt[4..8].try_into().unwrap());
        let p = &pkt[off..end];
        match p[0] & 0x1f {
            1..=23 => out.push(Nal { data: p.to_vec(), ts, last: marker }),
            24 => {
                // STAP-A: [len16][nal]...
                let mut i = 1;
                let start = out.len();
                while i + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                    i += 2;
                    if n == 0 || i + n > p.len() {
                        break;
                    }
                    out.push(Nal { data: p[i..i + n].to_vec(), ts, last: false });
                    i += n;
                }
                if out.len() > start {
                    out.last_mut().unwrap().last = marker;
                }
            }
            28 if p.len() > 2 => {
                let (s, e) = (p[1] & 0x80 != 0, p[1] & 0x40 != 0);
                if s {
                    let mut n = vec![(p[0] & 0xe0) | (p[1] & 0x1f)];
                    n.extend_from_slice(&p[2..]);
                    self.fu = Some(n);
                } else if let Some(n) = &mut self.fu {
                    n.extend_from_slice(&p[2..]);
                }
                if e {
                    if let Some(data) = self.fu.take() {
                        out.push(Nal { data, ts, last: marker });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Outgoing side.
pub struct Packetizer {
    pub pt: u8,
    pub ssrc: u32,
    pub seq: u16,
    /// max RTP packet size (header included)
    pub mtu: usize,
    ts_offset: u32,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// sent SPS/PPS since the last IDR
    params_sent: bool,
    started: bool,
    pub last_ts: u32,
}

impl Packetizer {
    pub fn new(pt: u8, ssrc: u32, mtu: usize, params: &[Vec<u8>], ts_offset: u32) -> Self {
        let mut p = Packetizer {
            pt,
            ssrc,
            seq: (ssrc >> 16) as u16,
            mtu: mtu.clamp(200, 1500),
            ts_offset,
            sps: None,
            pps: None,
            params_sent: false,
            started: false,
            last_ts: 0,
        };
        for n in params {
            p.remember(n);
        }
        p
    }

    fn remember(&mut self, n: &[u8]) {
        match n.first().map(|b| b & 0x1f) {
            Some(7) => self.sps = Some(n.to_vec()),
            Some(8) => self.pps = Some(n.to_vec()),
            _ => {}
        }
    }

    fn header(&mut self, ts: u32, marker: bool) -> Vec<u8> {
        let mut h = Vec::with_capacity(self.mtu);
        h.extend_from_slice(&[0x80, self.pt | if marker { 0x80 } else { 0 }]);
        h.extend_from_slice(&self.seq.to_be_bytes());
        h.extend_from_slice(&ts.to_be_bytes());
        h.extend_from_slice(&self.ssrc.to_be_bytes());
        self.seq = self.seq.wrapping_add(1);
        h
    }

    /// Packets for one NAL. Waits for the first IDR; puts SPS/PPS in front of every IDR that
    /// doesn't come with them.
    pub fn push(&mut self, nal: &Nal, out: &mut Vec<Vec<u8>>) {
        let ty = nal.data.first().map(|b| b & 0x1f).unwrap_or(0);
        self.remember(&nal.data);
        match ty {
            7 | 8 => {
                if !self.started {
                    return; // sent with the IDR
                }
                self.params_sent = true;
            }
            5 => {
                if !self.params_sent {
                    if let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) {
                        self.packetize(&sps, nal.ts, false, out);
                        self.packetize(&pps, nal.ts, false, out);
                    }
                }
                self.started = true;
                self.params_sent = false;
            }
            1 => {
                self.params_sent = false;
                if !self.started {
                    return;
                }
            }
            _ if !self.started => return,
            _ => {}
        }
        self.packetize(&nal.data, nal.ts, nal.last, out);
    }

    fn packetize(&mut self, nal: &[u8], ts: u32, last: bool, out: &mut Vec<Vec<u8>>) {
        let ts = ts.wrapping_add(self.ts_offset);
        self.last_ts = ts;
        if 12 + nal.len() <= self.mtu {
            let mut p = self.header(ts, last);
            p.extend_from_slice(nal);
            out.push(p);
            return;
        }
        let (hdr, body) = (nal[0], &nal[1..]);
        let chunk = self.mtu - 12 - 2;
        let n = body.len().div_ceil(chunk);
        for (i, c) in body.chunks(chunk).enumerate() {
            let end = i + 1 == n;
            let mut p = self.header(ts, last && end);
            p.push((hdr & 0xe0) | 28);
            p.push((if i == 0 { 0x80 } else { 0 }) | (if end { 0x40 } else { 0 }) | (hdr & 0x1f));
            p.extend_from_slice(c);
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(payload: &[u8], ts: u32, marker: bool) -> Vec<u8> {
        let mut p = vec![0x80, 97 | if marker { 0x80 } else { 0 }, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9];
        p[4..8].copy_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn fu_a_reassembly_and_refragmentation() {
        let idr: Vec<u8> = std::iter::once(0x65).chain((0..3000).map(|i| i as u8)).collect();
        let mut d = Depacketizer::default();
        let mut nals = Vec::new();
        // incoming: FU-A in 1000-byte pieces
        let body = &idr[1..];
        let pieces: Vec<&[u8]> = body.chunks(1000).collect();
        for (i, c) in pieces.iter().enumerate() {
            let mut p = vec![0x7c, (if i == 0 { 0x80 } else { 0 }) | (if i + 1 == pieces.len() { 0x40 } else { 0 }) | 5];
            p.extend_from_slice(c);
            d.push(&rtp(&p, 1000, i + 1 == pieces.len()), &mut nals);
        }
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].data, idr);
        assert!(nals[0].last);

        let sps = vec![0x67, 1, 2, 3];
        let pps = vec![0x68, 4];
        let mut pk = Packetizer::new(99, 0xabcd0000, 1378, &[sps.clone(), pps.clone()], 10);
        let mut out = Vec::new();
        pk.push(&Nal { data: vec![0x41, 1, 2], ts: 900, last: true }, &mut out);
        assert!(out.is_empty()); // waits for an IDR
        pk.push(&nals[0], &mut out);
        assert_eq!(&out[0][12..], &sps[..]);
        assert_eq!(&out[1][12..], &pps[..]);
        assert!(out.iter().all(|p| p.len() <= 1378));
        assert_eq!(u32::from_be_bytes(out[2][4..8].try_into().unwrap()), 1010);
        let last = out.last().unwrap();
        assert!(last[1] & 0x80 != 0 && last[13] & 0x40 != 0);
        // reassembling our own output gives the IDR back
        let mut again = Vec::new();
        let mut d2 = Depacketizer::default();
        for p in &out[2..] {
            d2.push(p, &mut again);
        }
        assert_eq!(again[0].data, idr);
        let seqs: Vec<u16> = out.iter().map(|p| u16::from_be_bytes([p[2], p[3]])).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)));
    }

    #[test]
    fn stap_a_split() {
        let mut d = Depacketizer::default();
        let mut nals = Vec::new();
        d.push(&rtp(&[0x18, 0, 2, 0x67, 1, 0, 2, 0x68, 2], 5, false), &mut nals);
        assert_eq!(nals.iter().map(|n| n.data[0]).collect::<Vec<_>>(), [0x67, 0x68]);
    }
}
