//! Receiver jitter buffer (SPEC §12.5): reorders in-flight packets by
//! `seq`, conceals gaps (caller runs Opus PLC), and skips ahead after
//! sustained loss. Wraparound-safe on the u32 seq ring.

use std::collections::HashMap;
use std::time::Duration;

use crate::chan::AudioPacket;

/// After this many consecutive conceals, skip ahead to the oldest
/// buffered packet instead of concealing further.
const MAX_CONSECUTIVE_CONCEALS: u32 = 3;

/// `true` iff `a` is older than `b` on the wrapping u32 seq ring
/// (serial-number arithmetic; valid while the reorder window stays
/// below 2^31 packets).
fn seq_older(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) > 0x8000_0000
}

/// Result of one playout tick ([`JitterBuffer::pop`]).
#[derive(Debug, PartialEq, Eq)]
pub enum PopResult {
    /// Deliver this packet — strictly the next in seq order.
    Play(AudioPacket),
    /// The expected packet is missing; conceal one frame with Opus PLC.
    Conceal,
    /// Nothing deliverable: pre-buffering (below depth) or drained dry
    /// after sustained loss. Target-level pacing is the caller's.
    Empty,
}

/// Gap-free PLC accounting (SPEC §12.5/§12.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// Packets delivered in order.
    pub played: u64,
    /// PLC frames emitted for missing packets.
    pub concealed: u64,
    /// Pushed packets at or older than the newest played seq.
    pub dropped_late: u64,
    /// Seqs declared lost when skipping ahead after sustained concealment.
    pub dropped_gap: u64,
}

/// Fixed-depth jitter buffer over 10 ms audio frames. `push` from the
/// network path, `pop` from the 10 ms playout clock.
pub struct JitterBuffer {
    /// Configured depth quantized to whole 10 ms frames (min 1).
    depth_frames: usize,
    /// Latched once the buffer has held `depth_frames` packets; delivery
    /// starts then (SPEC §12.5 "start 40 ms").
    started: bool,
    /// Seq of the first packet ever pushed; anchors ring ordering near
    /// the u32 wraparound point.
    anchor: Option<u32>,
    /// Next seq to deliver.
    next_seq: Option<u32>,
    slots: HashMap<u32, AudioPacket>,
    consecutive_conceals: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(depth: Duration) -> Self {
        let depth_frames = ((depth.as_millis() / 10) as usize).max(1);
        Self {
            depth_frames,
            started: false,
            anchor: None,
            next_seq: None,
            slots: HashMap::new(),
            consecutive_conceals: 0,
            stats: JitterStats::default(),
        }
    }

    /// Accept a packet. Packets at or older than the newest played seq
    /// are stale and counted in [`JitterStats::dropped_late`]; duplicates
    /// of still-buffered seqs replace silently.
    pub fn push(&mut self, pkt: AudioPacket) {
        if self.anchor.is_none() {
            self.anchor = Some(pkt.seq);
        }
        if let Some(next) = self.next_seq {
            let newest_played = next.wrapping_sub(1);
            if pkt.seq == newest_played || seq_older(pkt.seq, newest_played) {
                self.stats.dropped_late += 1;
                log::debug!("jitter: dropped late packet seq={}", pkt.seq);
                return;
            }
        }
        self.slots.insert(pkt.seq, pkt);
        if !self.started && self.slots.len() >= self.depth_frames {
            self.started = true;
            self.next_seq = Some(self.lowest_seq().expect("slots just filled"));
        }
    }

    /// One playout tick. In-order delivery once the buffer has filled
    /// past the depth; a missing seq conceals (PLC), and after
    /// [`MAX_CONSECUTIVE_CONCEALS`] consecutive misses the buffer skips
    /// ahead to the oldest buffered packet, counting the skipped seqs in
    /// [`JitterStats::dropped_gap`].
    pub fn pop(&mut self) -> PopResult {
        let Some(next) = self.next_seq else {
            return PopResult::Empty;
        };
        if let Some(pkt) = self.slots.remove(&next) {
            self.next_seq = Some(next.wrapping_add(1));
            self.stats.played += 1;
            self.consecutive_conceals = 0;
            return PopResult::Play(pkt);
        }
        if self.consecutive_conceals >= MAX_CONSECUTIVE_CONCEALS {
            // Sustained loss: jump to the oldest buffered packet. Every
            // slot is newer than `next` (push drops anything older), so
            // `lowest_seq` is strictly ahead on the ring.
            if let Some(target) = self.lowest_seq() {
                let mut s = next;
                while s != target {
                    self.stats.dropped_gap += 1;
                    s = s.wrapping_add(1);
                }
                log::debug!(
                    "jitter: skip-ahead to seq={target}, {} lost",
                    target.wrapping_sub(next)
                );
                self.next_seq = Some(target);
                self.consecutive_conceals = 0;
                return self.pop(); // `target` is buffered: plays now
            }
            // Drained dry mid-stream; the caller decides whether to wait.
            return PopResult::Empty;
        }
        self.consecutive_conceals += 1;
        self.stats.concealed += 1;
        self.next_seq = Some(next.wrapping_add(1));
        PopResult::Conceal
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Oldest buffered seq, by signed ring distance from the anchor.
    /// Signed (not unsigned) distance so reorders that straddle the
    /// first-received seq still order correctly; wrap-safe while the
    /// reorder window stays below 2^31 packets.
    fn lowest_seq(&self) -> Option<u32> {
        let anchor = self.anchor?;
        self.slots
            .keys()
            .copied()
            .min_by_key(|&s| (s.wrapping_sub(anchor) as i32) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32) -> AudioPacket {
        AudioPacket {
            seq,
            pts_us: seq as i64 * 10_000,
            payload: vec![seq as u8],
        }
    }

    #[test]
    fn in_order_play() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        assert_eq!(jb.pop(), PopResult::Empty); // below depth
        for seq in 0..4u32 {
            jb.push(pkt(seq));
        }
        for seq in 0..4u32 {
            assert_eq!(jb.pop(), PopResult::Play(pkt(seq)));
        }
        assert_eq!(jb.stats().played, 4);
        // Everything played, nothing buffered, next seq missing → PLC.
        assert_eq!(jb.pop(), PopResult::Conceal);
    }

    #[test]
    fn reorder_within_depth() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        for seq in [2u32, 3, 0, 1] {
            jb.push(pkt(seq));
        }
        for seq in 0..4u32 {
            assert_eq!(jb.pop(), PopResult::Play(pkt(seq)), "seq {seq}");
        }
        assert_eq!(jb.stats().played, 4);
        assert_eq!(jb.stats().dropped_late, 0);
    }

    #[test]
    fn conceal_on_short_loss_then_resume() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        for seq in 0..4u32 {
            jb.push(pkt(seq));
        }
        for seq in 0..4u32 {
            assert_eq!(jb.pop(), PopResult::Play(pkt(seq)));
        }
        // Lose seq 4 only.
        jb.push(pkt(5));
        assert_eq!(jb.pop(), PopResult::Conceal);
        assert_eq!(jb.pop(), PopResult::Play(pkt(5)));
        // Lose seqs 6 and 7.
        for seq in 8..10u32 {
            jb.push(pkt(seq));
        }
        assert_eq!(jb.pop(), PopResult::Conceal);
        assert_eq!(jb.pop(), PopResult::Conceal);
        assert_eq!(jb.pop(), PopResult::Play(pkt(8)));
        assert_eq!(jb.pop(), PopResult::Play(pkt(9)));
        let s = jb.stats();
        assert_eq!((s.played, s.concealed, s.dropped_gap), (7, 3, 0));
    }

    #[test]
    fn three_miss_then_skip_ahead() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        for seq in 0..4u32 {
            jb.push(pkt(seq));
        }
        for seq in 0..4u32 {
            assert_eq!(jb.pop(), PopResult::Play(pkt(seq)));
        }
        // Lose 4..=8; 9 and 10 are buffered.
        for seq in 9..11u32 {
            jb.push(pkt(seq));
        }
        assert_eq!(jb.pop(), PopResult::Conceal); // 4
        assert_eq!(jb.pop(), PopResult::Conceal); // 5
        assert_eq!(jb.pop(), PopResult::Conceal); // 6
        assert_eq!(jb.pop(), PopResult::Play(pkt(9))); // 7, 8 skipped
        assert_eq!(jb.pop(), PopResult::Play(pkt(10)));
        let s = jb.stats();
        assert_eq!(
            (s.played, s.concealed, s.dropped_late, s.dropped_gap),
            (6, 3, 0, 2)
        );
        // Drained and still missing: conceals resume until the cap
        // re-fills, then Empty (nothing left to skip to).
        assert_eq!(jb.pop(), PopResult::Conceal); // 11
        assert_eq!(jb.pop(), PopResult::Conceal); // 12
        assert_eq!(jb.pop(), PopResult::Conceal); // 13
        assert_eq!(jb.pop(), PopResult::Empty);
    }

    #[test]
    fn late_drops_counted() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        for seq in 0..4u32 {
            jb.push(pkt(seq));
        }
        assert_eq!(jb.pop(), PopResult::Play(pkt(0)));
        assert_eq!(jb.pop(), PopResult::Play(pkt(1))); // newest played = 1
        jb.push(pkt(0)); // replay of played
        jb.push(pkt(1)); // replay of played
        assert_eq!(jb.stats().dropped_late, 2);
        jb.push(pkt(4)); // fresh seq still accepted
        for seq in 2..4u32 {
            assert_eq!(jb.pop(), PopResult::Play(pkt(seq)));
        }
        assert_eq!(jb.pop(), PopResult::Play(pkt(4)));
        let s = jb.stats();
        assert_eq!((s.played, s.dropped_late, s.concealed), (5, 2, 0));
    }

    #[test]
    fn seq_wraparound_at_u32_max() {
        let mut jb = JitterBuffer::new(Duration::from_millis(40));
        let base = u32::MAX - 1;
        for seq in [base, base.wrapping_add(1), base.wrapping_add(2), base.wrapping_add(3)] {
            jb.push(pkt(seq));
        }
        assert_eq!(jb.pop(), PopResult::Play(pkt(base)));
        assert_eq!(jb.pop(), PopResult::Play(pkt(u32::MAX)));
        assert_eq!(jb.pop(), PopResult::Play(pkt(0)));
        assert_eq!(jb.pop(), PopResult::Play(pkt(1)));
        // Gap across the wrap: conceal 2, resume on 3.
        assert_eq!(jb.pop(), PopResult::Conceal);
        jb.push(pkt(3));
        assert_eq!(jb.pop(), PopResult::Play(pkt(3)));
        // Seq 0 now lies at/older than newest played (3) across the ring.
        jb.push(pkt(0));
        assert_eq!(jb.stats().dropped_late, 1);
        assert_eq!(jb.stats().played, 5);
    }
}
