//! RTP packetization of H.264 (RFC 6184, packetization-mode 1) and RTCP SR.

use std::time::{SystemTime, UNIX_EPOCH};

pub struct RtpPacketizer {
    pub ssrc: u32,
    payload_type: u8,
    max_payload: usize,
    /// Extended sequence number: (ROC << 16) | seq.
    index: u64,
    pub packet_count: u32,
    pub octet_count: u32,
    pub last_timestamp: u32,
}

impl RtpPacketizer {
    pub fn new(ssrc: u32, payload_type: u8, mtu: usize, initial_seq: u16) -> Self {
        // RTP header (12) + SRTP auth tag (10) must fit in the MTU.
        let max_payload = mtu.clamp(400, 1500) - 12 - 10;
        RtpPacketizer {
            ssrc,
            payload_type,
            max_payload,
            index: initial_seq as u64,
            packet_count: 0,
            octet_count: 0,
            last_timestamp: 0,
        }
    }

    fn packet(&mut self, marker: bool, ts: u32, payload_parts: &[&[u8]]) -> (Vec<u8>, u64) {
        let seq = (self.index & 0xffff) as u16;
        let mut p = Vec::with_capacity(12 + self.max_payload + 10);
        p.push(0x80);
        p.push(((marker as u8) << 7) | (self.payload_type & 0x7f));
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        let mut len = 0;
        for part in payload_parts {
            p.extend_from_slice(part);
            len += part.len();
        }
        let index = self.index;
        self.index = (self.index + 1) & 0xffff_ffff_ffff;
        self.packet_count = self.packet_count.wrapping_add(1);
        self.octet_count = self.octet_count.wrapping_add(len as u32);
        self.last_timestamp = ts;
        (p, index)
    }

    /// Packetizes one access unit (a list of NAL units without start codes).
    /// Returns (packet, srtp index) pairs.
    pub fn packetize_au(&mut self, nals: &[&[u8]], ts: u32) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        for (n, nal) in nals.iter().enumerate() {
            let last_nal = n + 1 == nals.len();
            if nal.len() <= self.max_payload {
                out.push(self.packet(last_nal, ts, &[nal]));
                continue;
            }
            // FU-A fragmentation.
            let hdr = nal[0];
            let fu_indicator = (hdr & 0xe0) | 28;
            let nal_type = hdr & 0x1f;
            let body = &nal[1..];
            let chunk = self.max_payload - 2;
            let count = body.len().div_ceil(chunk);
            for (i, part) in body.chunks(chunk).enumerate() {
                let start = i == 0;
                let end = i + 1 == count;
                let fu_header = ((start as u8) << 7) | ((end as u8) << 6) | nal_type;
                out.push(self.packet(last_nal && end, ts, &[&[fu_indicator, fu_header], part]));
            }
        }
        out
    }

    /// RTCP sender report (no report blocks), unencrypted.
    pub fn sender_report(&self) -> Vec<u8> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let ntp_sec = (now.as_secs() + 2_208_988_800) as u32;
        let ntp_frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
        let mut p = vec![0x80, 200, 0x00, 0x06];
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        p.extend_from_slice(&ntp_sec.to_be_bytes());
        p.extend_from_slice(&(ntp_frac as u32).to_be_bytes());
        p.extend_from_slice(&self.last_timestamp.to_be_bytes());
        p.extend_from_slice(&self.packet_count.to_be_bytes());
        p.extend_from_slice(&self.octet_count.to_be_bytes());
        p
    }
}

/// Splits an Annex-B byte stream into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &(_, payload_start)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|&(s, _)| s).unwrap_or(data.len());
        // A 4-byte start code leaves a trailing zero on the previous NAL.
        while end > payload_start && data[end - 1] == 0 && k + 1 < starts.len() {
            end -= 1;
        }
        if end > payload_start {
            nals.push(&data[payload_start..end]);
        }
    }
    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let nals = split_annexb(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 9, 9]]);
    }

    #[test]
    fn fragments_large_nal() {
        let mut p = RtpPacketizer::new(1, 99, 400, 65535);
        let nal: Vec<u8> = std::iter::once(0x65).chain(std::iter::repeat_n(7, 1000)).collect();
        let pkts = p.packetize_au(&[&[0x67, 1], &nal], 1234);
        assert_eq!(pkts.len(), 1 + 1000usize.div_ceil(400 - 22 - 2));
        // Sequence wraps and the index (ROC) keeps counting.
        assert_eq!(pkts[0].1, 65535);
        assert_eq!(pkts[1].1, 65536);
        assert_eq!(&pkts[1].0[2..4], &[0, 0]);
        // Marker only on the last packet; FU-A start/end bits set correctly.
        assert!(pkts.iter().take(pkts.len() - 1).all(|(b, _)| b[1] & 0x80 == 0));
        assert_eq!(pkts.last().unwrap().0[1] & 0x80, 0x80);
        assert_eq!(pkts[1].0[12..14], [0x60 | 28, 0x80 | 5]);
        assert_eq!(pkts.last().unwrap().0[13], 0x40 | 5);
    }
}
