//! Video channel (UDP): fragmentation, retransmit ring, NACK reassembly
//! with the SPEC §5 drop policy.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use tl_proto::packet::{VideoHeader, FLAG_HAS_CONFIG, FLAG_KEYFRAME, VIDEO_HEADER_LEN};
use tl_proto::{EncodedUnit, Feedback};

/// Hard cap on fragments per frame; guards against garbage `frag_count`.
const MAX_FRAGS_PER_FRAME: u16 = 8192;
/// Hard cap on one reassembled frame (SPEC: ~64 MiB garbage guard).
const MAX_FRAME_BYTES: usize = 64 << 20;
/// Incomplete frames are dropped+NACKed this long after their first
/// fragment (SPEC §5).
const FRAME_TIMEOUT: Duration = Duration::from_millis(33);
/// A NACKed frame stays reassemblable this long so retransmitted fragments
/// can still complete it (it is already counted as dropped either way).
const RETRANSMIT_GRACE: Duration = Duration::from_millis(500);
/// Periodic receiver report cadence (SPEC §5/§6).
const REPORT_INTERVAL: Duration = Duration::from_millis(500);
/// `IdrRequest` is emitted after this many drops ...
const IDR_DROP_THRESHOLD: usize = 3;
/// ... inside this sliding window (SPEC §5).
const IDR_DROP_WINDOW: Duration = Duration::from_millis(500);
/// Grace re-NACKs with zero progress before a frame is declared
/// unrecoverable (sender ring evicted / sender gone) → discard + IDR.
const MAX_GRACE_RENACKS: u8 = 3;

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[derive(Clone, Debug)]
pub struct VideoTxConfig {
    pub peer: SocketAddr,
    /// Total datagram budget incl. the 24-byte header (SPEC §5).
    pub datagram_payload: usize,
    /// Retransmit ring capacity in bytes (0 disables retransmit).
    pub ring_bytes: usize,
}

impl Default for VideoTxConfig {
    fn default() -> Self {
        Self {
            peer: SocketAddr::from(([0, 0, 0, 0], 0)),
            datagram_payload: tl_proto::DEFAULT_DATAGRAM_PAYLOAD,
            ring_bytes: 16 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TxStats {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub retransmits: u64,
}

pub struct VideoTx {
    sock: UdpSocket,
    cfg: VideoTxConfig,
    frame_seq: u32,
    /// Complete datagrams keyed (frame_seq, frag_index).
    ring: HashMap<(u32, u16), Vec<u8>>,
    /// FIFO insertion order for byte-budget eviction.
    ring_order: VecDeque<(u32, u16)>,
    ring_used: usize,
    idr_latched: bool,
    stats: TxStats,
}

impl VideoTx {
    pub fn bind(local: SocketAddr, cfg: VideoTxConfig) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        bump_buffer(&sock, BufferKind::Send);
        Ok(Self {
            sock,
            cfg,
            frame_seq: 0,
            ring: HashMap::new(),
            ring_order: VecDeque::new(),
            ring_used: 0,
            idr_latched: false,
            stats: TxStats::default(),
        })
    }

    /// Fragment one access unit per SPEC §5, store in the retransmit
    /// ring, send all fragments.
    pub fn send_unit(&mut self, unit: &EncodedUnit) -> io::Result<()> {
        if unit.data.is_empty() {
            // 0-length units are not valid on the wire (frag_count 0 is
            // rejected by VideoHeader::decode); skip them.
            log::warn!("send_unit: skipping empty access unit");
            return Ok(());
        }
        let per = self
            .cfg
            .datagram_payload
            .saturating_sub(VIDEO_HEADER_LEN)
            .max(1);
        let needed = unit.data.len().div_ceil(per);
        if needed > MAX_FRAGS_PER_FRAME as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unit of {} bytes needs {needed} fragments (cap {MAX_FRAGS_PER_FRAME})",
                    unit.data.len()
                ),
            ));
        }
        let frag_count = VideoHeader::frag_count_for(unit.data.len(), self.cfg.datagram_payload);
        // Parameter sets are required on every keyframe (SPEC §5), so a
        // keyframe unit always carries codec config.
        let flags = if unit.keyframe {
            FLAG_KEYFRAME | FLAG_HAS_CONFIG
        } else {
            0
        };
        let frame_seq = self.frame_seq;
        for (idx, chunk) in unit.data.chunks(per).enumerate() {
            let header = VideoHeader {
                frame_seq,
                frag_index: idx as u16,
                frag_count,
                flags,
                pts_us: unit.pts_us,
            };
            let mut datagram = Vec::with_capacity(VIDEO_HEADER_LEN + chunk.len());
            datagram.extend_from_slice(&header.encode());
            datagram.extend_from_slice(chunk);
            self.ring_insert(frame_seq, idx as u16, datagram.clone());
            self.sock.send_to(&datagram, self.cfg.peer)?;
            self.stats.bytes_sent += datagram.len() as u64;
        }
        self.stats.frames_sent += 1;
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Ok(())
    }

    /// NACK → retransmit ranges still in the ring; IdrRequest latches
    /// `take_idr_request`.
    pub fn handle_feedback(&mut self, fb: &Feedback) -> io::Result<()> {
        match fb {
            Feedback::Nack { frame_seq, ranges } => {
                for &(lo, hi) in ranges {
                    if hi < lo {
                        continue;
                    }
                    for idx in lo as u32..=hi as u32 {
                        let key = (*frame_seq, idx as u16);
                        if let Some(datagram) = self.ring.get(&key) {
                            self.sock.send_to(datagram, self.cfg.peer)?;
                            self.stats.retransmits += 1;
                            self.stats.bytes_sent += datagram.len() as u64;
                        } else {
                            log::debug!(
                                "NACK for evicted fragment seq={frame_seq} frag={idx}; ignored"
                            );
                        }
                    }
                }
            }
            Feedback::IdrRequest => {
                log::debug!("receiver requested IDR");
                self.idr_latched = true;
            }
            Feedback::Report { .. } => {
                log::trace!("receiver report: {fb:?}");
            }
        }
        Ok(())
    }

    pub fn take_idr_request(&mut self) -> bool {
        std::mem::take(&mut self.idr_latched)
    }

    pub fn stats(&self) -> TxStats {
        self.stats
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    fn ring_insert(&mut self, frame_seq: u32, frag_index: u16, datagram: Vec<u8>) {
        if self.cfg.ring_bytes == 0 {
            return;
        }
        let bytes = datagram.len();
        while self.ring_used + bytes > self.cfg.ring_bytes {
            let Some(oldest) = self.ring_order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.ring.remove(&oldest) {
                self.ring_used -= evicted.len();
            }
        }
        if bytes > self.cfg.ring_bytes {
            return; // single datagram larger than the whole ring
        }
        self.ring.insert((frame_seq, frag_index), datagram);
        self.ring_order.push_back((frame_seq, frag_index));
        self.ring_used += bytes;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RxStats {
    pub frames_complete: u64,
    pub frames_dropped: u64,
    pub packets_lost: u64,
    pub bytes_received: u64,
}

/// Reassembly state for one in-flight frame.
struct FrameBuf {
    frag_count: u16,
    frags: Vec<Option<Vec<u8>>>,
    received: u16,
    total_bytes: usize,
    /// Last time a *new* fragment landed (frame creation counts). The 33 ms
    /// drop timer runs from here so slow userspace drains of an already
    /// queued burst are not mistaken for loss.
    last_progress: Instant,
    pts_us: i64,
    flags: u16,
    /// Drop policy applied at least once; buffer may still be completed
    /// by retransmitted fragments within the grace window. Fresh fragment
    /// arrivals revive the frame (new 33 ms window, re-NACK on expiry) so
    /// retransmit bursts that themselves suffer loss still converge.
    nacked: bool,
    /// Drop statistics/IDR bookkeeping were recorded for this frame
    /// (counted once per frame even across revival re-drops).
    drop_counted: bool,
    /// Grace-period re-NACKs sent while receiving nothing. Exhaustion
    /// means the sender can no longer recover the frame (ring evicted or
    /// dead) → discard + IdrRequest.
    renacks: u8,
}

impl FrameBuf {
    fn new(h: &VideoHeader) -> Self {
        Self {
            frag_count: h.frag_count,
            frags: vec![None; h.frag_count as usize],
            received: 0,
            total_bytes: 0,
            last_progress: Instant::now(),
            pts_us: h.pts_us,
            flags: h.flags,
            nacked: false,
            drop_counted: false,
            renacks: 0,
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.frag_count
    }

    fn into_unit(self) -> EncodedUnit {
        let mut data = Vec::with_capacity(self.total_bytes);
        for frag in self.frags.into_iter().flatten() {
            data.extend_from_slice(&frag);
        }
        EncodedUnit {
            pts_us: self.pts_us,
            keyframe: self.flags & FLAG_KEYFRAME != 0,
            data,
        }
    }

    /// Inclusive missing-fragment ranges + missing count.
    fn missing_ranges(&self) -> (Vec<(u16, u16)>, u64) {
        let mut ranges = Vec::new();
        let mut count = 0u64;
        let mut start: Option<u16> = None;
        for i in 0..self.frag_count {
            if self.frags[i as usize].is_none() {
                count += 1;
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start.take() {
                ranges.push((s, i - 1));
            }
        }
        if let Some(s) = start {
            ranges.push((s, self.frag_count - 1));
        }
        (ranges, count)
    }
}

pub struct VideoRx {
    sock: UdpSocket,
    frames: HashMap<u32, FrameBuf>,
    /// Newest frame_seq ever accepted; older unknown seqs are stale.
    max_seq: Option<u32>,
    pending: Vec<Feedback>,
    drop_times: VecDeque<Instant>,
    last_report: Instant,
    last_arrival: Option<Instant>,
    prev_interval_us: Option<f64>,
    jitter_us: f64,
    period_frames: u64,
    period_lost: u64,
    stats: RxStats,
}

impl VideoRx {
    pub fn bind(local: SocketAddr) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        bump_buffer(&sock, BufferKind::Recv);
        Ok(Self {
            sock,
            frames: HashMap::new(),
            max_seq: None,
            pending: Vec::new(),
            drop_times: VecDeque::new(),
            last_report: Instant::now(),
            last_arrival: None,
            prev_interval_us: None,
            jitter_us: 0.0,
            period_frames: 0,
            period_lost: 0,
            stats: RxStats::default(),
        })
    }

    /// Wait up to `timeout`; returns a fully reassembled unit when one
    /// completes. Implements SPEC §5 drop/NACK policy internally.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<EncodedUnit>> {
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 65_535];
        loop {
            self.maintenance();
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let mut wait = deadline - now;
            if let Some(t) = self.next_maintenance_in(now) {
                wait = wait.min(t);
            }
            if wait.is_zero() {
                continue;
            }
            self.sock.set_read_timeout(Some(wait))?;
            match self.sock.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    self.stats.bytes_received += n as u64;
                    self.note_arrival();
                    if let Some(unit) = self.handle_datagram(&buf[..n]) {
                        return Ok(Some(unit));
                    }
                }
                Err(e) if timed_out(&e) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Drain pending feedback (NACKs, IdrRequest, periodic Report).
    pub fn take_feedback(&mut self) -> Vec<Feedback> {
        std::mem::take(&mut self.pending)
    }

    pub fn stats(&self) -> RxStats {
        self.stats
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    fn note_arrival(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_arrival.replace(now) {
            let interval_us = now.duration_since(prev).as_secs_f64() * 1e6;
            if let Some(pi) = self.prev_interval_us.replace(interval_us) {
                // RFC 3550-style EMA of inter-arrival deviation.
                let d = (interval_us - pi).abs();
                self.jitter_us += (d - self.jitter_us) / 16.0;
            }
        }
    }

    fn handle_datagram(&mut self, datagram: &[u8]) -> Option<EncodedUnit> {
        let h = VideoHeader::decode(datagram)?; // garbage → ignore
        if h.frag_count > MAX_FRAGS_PER_FRAME {
            log::debug!("ignoring datagram with absurd frag_count {}", h.frag_count);
            return None;
        }
        let payload = &datagram[VIDEO_HEADER_LEN..];

        if let Some(f) = self.frames.get_mut(&h.frame_seq) {
            if f.frag_count != h.frag_count {
                log::debug!(
                    "inconsistent frag_count for frame {}; ignoring",
                    h.frame_seq
                );
                return None;
            }
            let slot = &mut f.frags[h.frag_index as usize];
            if slot.is_none() {
                if f.nacked {
                    // The peer answered our NACK: give the frame a fresh
                    // reassembly window.
                    f.nacked = false;
                    f.renacks = 0;
                }
                f.last_progress = Instant::now();
                f.total_bytes += payload.len();
                if f.total_bytes > MAX_FRAME_BYTES {
                    self.frames.remove(&h.frame_seq);
                    log::warn!(
                        "frame {} exceeded {} bytes; discarded as garbage",
                        h.frame_seq,
                        MAX_FRAME_BYTES
                    );
                    return None;
                }
                *slot = Some(payload.to_vec());
                f.received += 1;
            }
            if f.is_complete() {
                let f = self.frames.remove(&h.frame_seq).expect("present");
                return Some(self.complete(f));
            }
            return None;
        }

        // New frame_seq: reject anything not strictly newer than the newest
        // seen (late fragments of already-dropped/completed frames).
        if let Some(max) = self.max_seq {
            if (h.frame_seq.wrapping_sub(max) as i32) <= 0 {
                return None;
            }
        }
        // A newer frame_seq arrived: every incomplete older frame is
        // dropped and NACKed once (SPEC §5).
        let older: Vec<u32> = self.frames.keys().copied().collect();
        for seq in older {
            self.drop_frame(seq, true);
        }
        self.max_seq = Some(h.frame_seq);

        let mut f = FrameBuf::new(&h);
        f.total_bytes = payload.len();
        if f.total_bytes > MAX_FRAME_BYTES {
            log::warn!("single fragment exceeds {} bytes; discarded", MAX_FRAME_BYTES);
            return None;
        }
        f.frags[h.frag_index as usize] = Some(payload.to_vec());
        f.received = 1;
        if f.is_complete() {
            return Some(self.complete(f));
        }
        self.frames.insert(h.frame_seq, f);
        None
    }

    fn complete(&mut self, f: FrameBuf) -> EncodedUnit {
        self.stats.frames_complete += 1;
        self.period_frames += 1;
        f.into_unit()
    }

    /// Apply the SPEC §5 drop policy to `seq` (NACK once, count loss,
    /// IDR threshold). `discard` removes the buffer (newer frame arrived);
    /// otherwise the buffer stays reassemblable for the grace window.
    fn drop_frame(&mut self, seq: u32, discard: bool) {
        let Some(f) = self.frames.get_mut(&seq) else {
            return;
        };
        if f.nacked {
            // Already dropped+NACKed; only honor the discard request so a
            // grace-retained frame is never completed after a newer one
            // (latest-wins, SPEC §5).
            if discard {
                self.frames.remove(&seq);
            }
            return;
        }
        f.nacked = true;
        let (ranges, missing) = f.missing_ranges();
        let first_drop = !f.drop_counted;
        if first_drop {
            f.drop_counted = true;
            self.stats.frames_dropped += 1;
            self.stats.packets_lost += missing;
            self.period_lost += missing;
        }
        if !ranges.is_empty() {
            log::debug!("frame {seq} dropped; NACKing {} fragment(s)", missing);
            self.pending.push(Feedback::Nack {
                frame_seq: seq,
                ranges,
            });
        }
        if discard {
            self.frames.remove(&seq);
        }
        if !first_drop {
            // Revival re-drop: re-NACK only; stats and the IDR threshold
            // count each frame once.
            return;
        }

        let now = Instant::now();
        self.drop_times.push_back(now);
        while let Some(&t) = self.drop_times.front() {
            if now.duration_since(t) > IDR_DROP_WINDOW {
                self.drop_times.pop_front();
            } else {
                break;
            }
        }
        if self.drop_times.len() >= IDR_DROP_THRESHOLD {
            self.drop_times.clear();
            log::info!("{IDR_DROP_THRESHOLD}+ frames dropped within 500ms; requesting IDR");
            self.pending.push(Feedback::IdrRequest);
        }
    }

    /// A NACKed frame got no retransmitted fragments for a full grace
    /// window: re-NACK what is still missing. After MAX_GRACE_RENACKS
    /// consecutive silent windows the frame is unrecoverable — discard it
    /// and ask for a fresh IDR instead of spinning forever.
    fn grace_renack(&mut self, seq: u32) {
        let Some(f) = self.frames.get_mut(&seq) else {
            return;
        };
        if !f.nacked {
            return;
        }
        f.renacks += 1;
        if f.renacks > MAX_GRACE_RENACKS {
            self.frames.remove(&seq);
            log::info!("frame {seq} unrecoverable after {MAX_GRACE_RENACKS} re-NACKs; requesting IDR");
            self.pending.push(Feedback::IdrRequest);
            return;
        }
        let (ranges, missing) = f.missing_ranges();
        f.last_progress = Instant::now();
        if !ranges.is_empty() {
            log::debug!("frame {seq} re-NACKing {} fragment(s) after silent grace window", missing);
            self.pending.push(Feedback::Nack {
                frame_seq: seq,
                ranges,
            });
        }
    }

    fn maintenance(&mut self) {
        let now = Instant::now();
        let timeout_drops: Vec<u32> = self
            .frames
            .iter()
            .filter(|(_, f)| !f.nacked && now.duration_since(f.last_progress) >= FRAME_TIMEOUT)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in timeout_drops {
            self.drop_frame(seq, false);
        }
        let grace_expiries: Vec<u32> = self
            .frames
            .iter()
            .filter(|(_, f)| f.nacked && now.duration_since(f.last_progress) >= RETRANSMIT_GRACE)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in grace_expiries {
            self.grace_renack(seq);
        }
        if now.duration_since(self.last_report) >= REPORT_INTERVAL {
            self.last_report = now;
            let report = Feedback::Report {
                received_frames: self.period_frames,
                lost_packets: self.period_lost,
                rtt_us: 0, // v1: no RTT probe on the video channel
                jitter_us: self.jitter_us.round() as u32,
            };
            self.period_frames = 0;
            self.period_lost = 0;
            self.pending.push(report);
        }
    }

    /// Time until the next scheduled maintenance event (frame timeout,
    /// grace purge, or periodic report).
    fn next_maintenance_in(&self, now: Instant) -> Option<Duration> {
        let mut next = self
            .last_report
            .checked_add(REPORT_INTERVAL)
            .map(|t| t.saturating_duration_since(now));
        for f in self.frames.values() {
            let due = if f.nacked {
                f.last_progress.checked_add(RETRANSMIT_GRACE)
            } else {
                f.last_progress.checked_add(FRAME_TIMEOUT)
            };
            if let Some(t) = due.map(|t| t.saturating_duration_since(now)) {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        next
    }
}

enum BufferKind {
    Send,
    Recv,
}

/// Best-effort kernel buffer bump; failure is non-fatal (loopback tests and
/// small sysctls). Video bursts are multi-MiB at 20 Gbps-class rates.
fn bump_buffer(sock: &UdpSocket, kind: BufferKind) {
    let sr = socket2::SockRef::from(sock);
    for size in [16 << 20, 8 << 20, 4 << 20, 1 << 20] {
        let r = match kind {
            BufferKind::Send => sr.set_send_buffer_size(size),
            BufferKind::Recv => sr.set_recv_buffer_size(size),
        };
        if r.is_ok() {
            log::debug!("socket buffer set to {size} bytes");
            return;
        }
    }
    log::debug!("could not enlarge socket buffer; using OS default");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    /// Deterministic pseudo-random bytes (xorshift64*).
    fn fill(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn loopback_3mb_unit_reassembles() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let mut tx = VideoTx::bind(
            loopback(),
            VideoTxConfig {
                peer: rx_addr,
                datagram_payload: tl_proto::DEFAULT_DATAGRAM_PAYLOAD,
                ring_bytes: 16 << 20,
            },
        )
        .unwrap();

        let data = fill(3 << 20, 0x1234_5678_9abc_def0);
        let unit = EncodedUnit {
            pts_us: 1_234_567,
            keyframe: true,
            data: data.clone(),
        };
        tx.send_unit(&unit).unwrap();
        assert_eq!(tx.stats().frames_sent, 1);

        let mut got = None;
        for _ in 0..400 {
            if let Some(u) = rx.poll(Duration::from_millis(50)).unwrap() {
                got = Some(u);
                break;
            }
            for fb in rx.take_feedback() {
                tx.handle_feedback(&fb).unwrap();
            }
        }
        let u = got.expect("unit must reassemble on loopback");
        assert_eq!(u.data, data);
        assert_eq!(u.pts_us, 1_234_567);
        assert!(u.keyframe);
        assert!(rx.stats().frames_complete >= 1);
    }

    #[test]
    fn empty_unit_is_skipped() {
        let probe = UdpSocket::bind(loopback()).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut tx = VideoTx::bind(
            loopback(),
            VideoTxConfig {
                peer: probe.local_addr().unwrap(),
                ..VideoTxConfig::default()
            },
        )
        .unwrap();
        tx.send_unit(&EncodedUnit {
            pts_us: 0,
            keyframe: false,
            data: Vec::new(),
        })
        .unwrap();
        assert_eq!(tx.stats().frames_sent, 0);
        let mut buf = [0u8; 2048];
        let err = probe.recv(&mut buf).unwrap_err();
        assert!(timed_out(&err));
    }

    #[test]
    fn tx_retransmits_nacked_ranges_and_latches_idr() {
        let probe = UdpSocket::bind(loopback()).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut tx = VideoTx::bind(
            loopback(),
            VideoTxConfig {
                peer: probe.local_addr().unwrap(),
                datagram_payload: tl_proto::DEFAULT_DATAGRAM_PAYLOAD,
                ring_bytes: 1 << 20,
            },
        )
        .unwrap();

        let data = fill(5_000, 7); // 4 fragments at 1376 B/frag
        tx.send_unit(&EncodedUnit {
            pts_us: 7,
            keyframe: false,
            data,
        })
        .unwrap();
        let mut buf = [0u8; 2048];
        let mut count = 0;
        while probe.recv(&mut buf).is_ok() {
            count += 1;
        }
        assert_eq!(count, 4);

        tx.handle_feedback(&Feedback::Nack {
            frame_seq: 0,
            ranges: vec![(1, 2)],
        })
        .unwrap();
        assert_eq!(tx.stats().retransmits, 2);
        let mut resent = Vec::new();
        for _ in 0..2 {
            let n = probe.recv(&mut buf).unwrap();
            let h = VideoHeader::decode(&buf[..n]).unwrap();
            assert_eq!(h.frame_seq, 0);
            resent.push(h.frag_index);
        }
        resent.sort_unstable();
        assert_eq!(resent, vec![1, 2]);

        assert!(!tx.take_idr_request());
        tx.handle_feedback(&Feedback::IdrRequest).unwrap();
        assert!(tx.take_idr_request());
        assert!(!tx.take_idr_request());
    }

    #[test]
    fn disabled_ring_drops_retransmit() {
        let probe = UdpSocket::bind(loopback()).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut tx = VideoTx::bind(
            loopback(),
            VideoTxConfig {
                peer: probe.local_addr().unwrap(),
                datagram_payload: tl_proto::DEFAULT_DATAGRAM_PAYLOAD,
                ring_bytes: 0,
            },
        )
        .unwrap();
        tx.send_unit(&EncodedUnit {
            pts_us: 0,
            keyframe: false,
            data: fill(3_000, 9),
        })
        .unwrap();
        let mut buf = [0u8; 2048];
        while probe.recv(&mut buf).is_ok() {}
        tx.handle_feedback(&Feedback::Nack {
            frame_seq: 0,
            ranges: vec![(0, 2)],
        })
        .unwrap();
        assert_eq!(tx.stats().retransmits, 0);
    }

    #[test]
    fn rx_nacks_missing_on_newer_seq_and_requests_idr() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();
        let send_frag = |seq: u32, idx: u16, count: u16| {
            let h = VideoHeader {
                frame_seq: seq,
                frag_index: idx,
                frag_count: count,
                flags: 0,
                pts_us: seq as i64,
            };
            let mut dg = h.encode().to_vec();
            dg.extend_from_slice(&[0xAB; 32]);
            raw.send_to(&dg, rx_addr).unwrap();
        };

        // Frame 1 missing fragment 2; frame 2 (complete) arrives → drop+NACK.
        send_frag(1, 0, 4);
        send_frag(1, 1, 4);
        send_frag(1, 3, 4);
        send_frag(2, 0, 1);
        let u = rx
            .poll(Duration::from_millis(500))
            .unwrap()
            .expect("frame 2 completes");
        assert_eq!(u.data, vec![0xAB; 32]);
        assert_eq!(u.pts_us, 2);
        let fbs = rx.take_feedback();
        assert!(
            fbs.iter().any(|f| matches!(
                f,
                Feedback::Nack { frame_seq: 1, ranges } if ranges == &vec![(2u16, 2u16)]
            )),
            "expected Nack for frame 1 frag 2, got {fbs:?}"
        );
        assert_eq!(rx.stats().frames_dropped, 1);
        assert_eq!(rx.stats().packets_lost, 1);

        // Two more drops inside the window → IdrRequest.
        send_frag(3, 0, 2); // missing idx 1
        send_frag(4, 0, 1);
        rx.poll(Duration::from_millis(500)).unwrap().unwrap();
        send_frag(5, 0, 2);
        send_frag(6, 0, 1);
        rx.poll(Duration::from_millis(500)).unwrap().unwrap();
        let fbs = rx.take_feedback();
        assert_eq!(rx.stats().frames_dropped, 3);
        assert!(
            fbs.iter().any(|f| matches!(f, Feedback::IdrRequest)),
            "expected IdrRequest after 3 drops, got {fbs:?}"
        );
        assert!(
            fbs.iter()
                .any(|f| matches!(f, Feedback::Nack { frame_seq: 3, .. }))
        );
        assert!(
            fbs.iter()
                .any(|f| matches!(f, Feedback::Nack { frame_seq: 5, .. }))
        );
    }

    #[test]
    fn rx_times_out_incomplete_frame_after_33ms() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();
        let h = VideoHeader {
            frame_seq: 10,
            frag_index: 0,
            frag_count: 2,
            flags: 0,
            pts_us: 0,
        };
        let mut dg = h.encode().to_vec();
        dg.extend_from_slice(&[0xCD; 16]);
        raw.send_to(&dg, rx_addr).unwrap();

        // Nothing completes within 200 ms; the 33 ms rule must fire.
        assert!(rx.poll(Duration::from_millis(200)).unwrap().is_none());
        let fbs = rx.take_feedback();
        assert!(
            fbs.iter().any(|f| matches!(
                f,
                Feedback::Nack { frame_seq: 10, ranges } if ranges == &vec![(1u16, 1u16)]
            )),
            "expected timeout Nack for frame 10, got {fbs:?}"
        );
        assert_eq!(rx.stats().frames_dropped, 1);
    }

    #[test]
    fn rx_emits_periodic_report_when_idle() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        assert!(rx.poll(Duration::from_millis(600)).unwrap().is_none());
        let fbs = rx.take_feedback();
        assert!(
            fbs.iter()
                .any(|f| matches!(f, Feedback::Report { rtt_us: 0, .. })),
            "expected periodic Report, got {fbs:?}"
        );
    }

    #[test]
    fn rx_ignores_garbage_and_absurd_frag_count() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();
        raw.send_to(b"not a video packet at all", rx_addr).unwrap();
        let h = VideoHeader {
            frame_seq: 1,
            frag_index: 0,
            frag_count: 9000, // > MAX_FRAGS_PER_FRAME
            flags: 0,
            pts_us: 0,
        };
        let mut dg = h.encode().to_vec();
        dg.extend_from_slice(&[0u8; 8]);
        raw.send_to(&dg, rx_addr).unwrap();
        assert!(rx.poll(Duration::from_millis(50)).unwrap().is_none());
        assert!(rx.take_feedback().is_empty());
        assert_eq!(rx.stats().frames_dropped, 0);
        // bytes_received still counts the raw UDP traffic
        assert!(rx.stats().bytes_received > 0);
    }

    fn send_frag_to(raw: &UdpSocket, rx_addr: SocketAddr, seq: u32, idx: u16, count: u16, byte: u8) {
        let h = VideoHeader {
            frame_seq: seq,
            frag_index: idx,
            frag_count: count,
            flags: 0,
            pts_us: seq as i64,
        };
        let mut dg = h.encode().to_vec();
        dg.extend_from_slice(&[byte; 16]);
        raw.send_to(&dg, rx_addr).unwrap();
    }

    #[test]
    fn nacked_frame_still_completes_via_retransmit() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();

        send_frag_to(&raw, rx_addr, 1, 0, 2, 0x11);
        // 33 ms rule fires → drop + NACK, buffer retained.
        assert!(rx.poll(Duration::from_millis(150)).unwrap().is_none());
        assert_eq!(rx.stats().frames_dropped, 1);
        assert!(rx
            .take_feedback()
            .iter()
            .any(|f| matches!(f, Feedback::Nack { frame_seq: 1, .. })));

        // The retransmitted missing fragment completes the frame anyway.
        send_frag_to(&raw, rx_addr, 1, 1, 2, 0x22);
        let u = rx
            .poll(Duration::from_millis(200))
            .unwrap()
            .expect("retained frame completes from retransmit");
        let mut want = vec![0x11; 16];
        want.extend_from_slice(&[0x22; 16]);
        assert_eq!(u.data, want);
        assert_eq!(rx.stats().frames_complete, 1);
    }

    #[test]
    fn nacked_frame_discarded_when_superseded() {
        // Regression: a grace-retained (NACKed) frame must be removed when
        // a newer frame_seq arrives; its late retransmit must never complete
        // a stale frame after the newer one (latest-wins, SPEC §5).
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();

        send_frag_to(&raw, rx_addr, 5, 0, 2, 0x55);
        // 33 ms rule → dropped + NACKed, buffer retained for grace.
        assert!(rx.poll(Duration::from_millis(150)).unwrap().is_none());
        assert_eq!(rx.stats().frames_dropped, 1);

        // Newer complete frame arrives → frame 5 must be discarded now.
        send_frag_to(&raw, rx_addr, 6, 0, 1, 0x66);
        let u = rx
            .poll(Duration::from_millis(300))
            .unwrap()
            .expect("newer frame completes");
        assert_eq!(u.data, vec![0x66; 16]);

        // Late retransmit for the superseded frame completes nothing.
        send_frag_to(&raw, rx_addr, 5, 1, 2, 0x77);
        assert!(rx.poll(Duration::from_millis(150)).unwrap().is_none());
        assert_eq!(rx.stats().frames_complete, 1);
    }

    #[test]
    fn silent_grace_windows_renack_then_request_idr() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();

        send_frag_to(&raw, rx_addr, 9, 0, 2, 0x33);
        assert!(rx.poll(Duration::from_millis(100)).unwrap().is_none());
        let fbs = rx.take_feedback();
        assert!(fbs
            .iter()
            .any(|f| matches!(f, Feedback::Nack { frame_seq: 9, .. })));

        // No retransmits ever arrive: grace windows re-NACK and finally
        // give up with an IdrRequest.
        let mut nacks = 0;
        let mut idr = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !idr {
            assert!(rx.poll(Duration::from_millis(300)).unwrap().is_none());
            for fb in rx.take_feedback() {
                match fb {
                    Feedback::Nack { frame_seq: 9, .. } => nacks += 1,
                    Feedback::IdrRequest => idr = true,
                    _ => {}
                }
            }
        }
        assert!(nacks >= 2, "expected grace re-NACKs, got {nacks}");
        assert!(idr, "expected IdrRequest after exhausting grace re-NACKs");
        // Exactly one drop accounted for the frame.
        assert_eq!(rx.stats().frames_dropped, 1);
    }

    #[test]
    fn late_fragment_of_dropped_frame_is_ignored() {
        let mut rx = VideoRx::bind(loopback()).unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let raw = UdpSocket::bind(loopback()).unwrap();
        let send_frag = |seq: u32, idx: u16, count: u16| {
            let h = VideoHeader {
                frame_seq: seq,
                frag_index: idx,
                frag_count: count,
                flags: 0,
                pts_us: 0,
            };
            let mut dg = h.encode().to_vec();
            dg.extend_from_slice(&[0xEF; 8]);
            raw.send_to(&dg, rx_addr).unwrap();
        };
        send_frag(1, 0, 2); // incomplete
        send_frag(2, 0, 1); // newer → frame 1 dropped+NACKed, completes frame 2
        rx.poll(Duration::from_millis(500)).unwrap().unwrap();
        let _ = rx.take_feedback();
        // Retransmitted fragment of frame 1 arrives after discard → ignored,
        // must NOT create a fresh buffer that would NACK again.
        send_frag(1, 1, 2);
        assert!(rx.poll(Duration::from_millis(100)).unwrap().is_none());
        let fbs = rx.take_feedback();
        assert!(
            !fbs.iter()
                .any(|f| matches!(f, Feedback::Nack { frame_seq: 1, .. })),
            "late fragment must not re-trigger NACK, got {fbs:?}"
        );
    }
}
