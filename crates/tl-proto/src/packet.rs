//! Video-packet framing and feedback messages.

use serde::{Deserialize, Serialize};

pub const VIDEO_MAGIC: u32 = u32::from_le_bytes(*b"TLV1");
pub const VIDEO_HEADER_LEN: usize = 24;

/// `VideoHeader.flags`: unit starts with an IDR/keyframe.
pub const FLAG_KEYFRAME: u16 = 1 << 0;
/// `VideoHeader.flags`: unit carries codec parameter sets (VPS/SPS/PPS).
pub const FLAG_HAS_CONFIG: u16 = 1 << 1;

/// Fixed 24-byte little-endian datagram header (SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHeader {
    pub frame_seq: u32,
    pub frag_index: u16,
    pub frag_count: u16,
    pub flags: u16,
    pub pts_us: i64,
}

impl VideoHeader {
    pub fn encode(&self) -> [u8; VIDEO_HEADER_LEN] {
        let mut b = [0u8; VIDEO_HEADER_LEN];
        b[0..4].copy_from_slice(&VIDEO_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.frame_seq.to_le_bytes());
        b[8..10].copy_from_slice(&self.frag_index.to_le_bytes());
        b[10..12].copy_from_slice(&self.frag_count.to_le_bytes());
        b[12..14].copy_from_slice(&self.flags.to_le_bytes());
        // 14..16 reserved
        b[16..24].copy_from_slice(&self.pts_us.to_le_bytes());
        b
    }

    /// Parse and validate. None on truncation, bad magic, or invalid
    /// fragmentation fields (`frag_count == 0` or index out of range).
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < VIDEO_HEADER_LEN {
            return None;
        }
        if u32::from_le_bytes(buf[0..4].try_into().ok()?) != VIDEO_MAGIC {
            return None;
        }
        let frag_index = u16::from_le_bytes(buf[8..10].try_into().ok()?);
        let frag_count = u16::from_le_bytes(buf[10..12].try_into().ok()?);
        if frag_count == 0 || frag_index >= frag_count {
            return None;
        }
        Some(Self {
            frame_seq: u32::from_le_bytes(buf[4..8].try_into().ok()?),
            frag_index,
            frag_count,
            flags: u16::from_le_bytes(buf[12..14].try_into().ok()?),
            pts_us: i64::from_le_bytes(buf[16..24].try_into().ok()?),
        })
    }

    /// Number of fragments needed to carry `payload_len` bytes with a given total
    /// datagram budget (header included), per SPEC §5.
    pub fn frag_count_for(payload_len: usize, datagram_payload: usize) -> u16 {
        let per = datagram_payload.saturating_sub(VIDEO_HEADER_LEN).max(1);
        payload_len.div_ceil(per) as u16
    }
}

/// One encoded video frame (access unit) as an Annex B bytestream.
/// For HEVC/H.264, parameter sets MUST precede slice NALs on keyframes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncodedUnit {
    pub pts_us: i64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Target → initiator feedback (SPEC §6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Feedback {
    /// Retransmit request: inclusive fragment ranges per frame.
    Nack { frame_seq: u32, ranges: Vec<(u16, u16)> },
    /// Unrecoverable loss: request a fresh IDR.
    IdrRequest,
    /// Periodic receiver report (~every 500 ms).
    Report {
        received_frames: u64,
        lost_packets: u64,
        rtt_us: u32,
        jitter_us: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = VideoHeader {
            frame_seq: 42,
            frag_index: 3,
            frag_count: 7,
            flags: FLAG_KEYFRAME | FLAG_HAS_CONFIG,
            pts_us: -123_456,
        };
        let enc = h.encode();
        assert_eq!(enc.len(), VIDEO_HEADER_LEN);
        assert_eq!(VideoHeader::decode(&enc), Some(h));
    }

    #[test]
    fn header_rejects_garbage() {
        assert_eq!(VideoHeader::decode(&[]), None);
        let mut good = VideoHeader {
            frame_seq: 0,
            frag_index: 0,
            frag_count: 1,
            flags: 0,
            pts_us: 0,
        }
        .encode();
        good[0] ^= 0xFF; // break magic
        assert_eq!(VideoHeader::decode(&good), None);

        let mut bad = VideoHeader {
            frame_seq: 0,
            frag_index: 1, // >= frag_count
            frag_count: 1,
            flags: 0,
            pts_us: 0,
        }
        .encode();
        assert_eq!(VideoHeader::decode(&bad), None);
        bad = VideoHeader {
            frame_seq: 0,
            frag_index: 0,
            frag_count: 0,
            flags: 0,
            pts_us: 0,
        }
        .encode();
        assert_eq!(VideoHeader::decode(&bad), None);
    }

    #[test]
    fn frag_count_math() {
        assert_eq!(VideoHeader::frag_count_for(0, 1400), 0);
        assert_eq!(VideoHeader::frag_count_for(1, 1400), 1);
        // 1400 - 24 = 1376 bytes per fragment
        assert_eq!(VideoHeader::frag_count_for(1376, 1400), 1);
        assert_eq!(VideoHeader::frag_count_for(1377, 1400), 2);
    }
}
