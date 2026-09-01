//! Pipeline glue: latest-wins frame channel + the two streaming loops.
//!
//! Platform crates supply capture/encode/decode/present primitives; these
//! loops own pacing, IDR latching, feedback flushing, and stop handling.
//! Threading model: one loop per thread, driven by the caller (SPEC §1).

#![forbid(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use parking_lot::{Condvar, Mutex};
use std::time::Duration;

use anyhow::Result;
use tl_net::feedback::FeedbackChannel;
use tl_net::video::{VideoRx, VideoTx};
use tl_proto::EncodedUnit;

pub mod chan {
    use super::*;

    struct Shared<T> {
        slot: Mutex<(u64, Option<T>)>,
        cv: Condvar,
        closed: AtomicBool,
        /// Live `Sender` clones; the last drop closes the channel.
        senders: AtomicU64,
    }

    /// Latest-wins channel endpoint: sending replaces any pending value;
    /// the receiver always sees the newest item exactly once.
    pub struct Sender<T> {
        shared: std::sync::Arc<Shared<T>>,
    }

    pub struct Receiver<T> {
        shared: std::sync::Arc<Shared<T>>,
        seen: u64,
    }

    pub fn latest_wins<T>() -> (Sender<T>, Receiver<T>) {
        let shared = std::sync::Arc::new(Shared {
            slot: Mutex::new((0, None)),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            senders: AtomicU64::new(1),
        });
        (Sender { shared: shared.clone() }, Receiver { shared, seen: 0 })
    }

    impl<T> Sender<T> {
        /// Publish `v`, replacing any not-yet-received value (drop-oldest).
        pub fn send(&self, v: T) {
            let mut g = self.shared.slot.lock();
            g.0 = g.0.wrapping_add(1);
            g.1 = Some(v);
            self.shared.cv.notify_one();
        }

        /// Wake the receiver permanently; it will drain the pending value
        /// then observe closure as `None`.
        pub fn close(&self) {
            self.shared.closed.store(true, Ordering::SeqCst);
            self.shared.cv.notify_all();
        }
    }

    impl<T> Receiver<T> {
        /// True once the sender closed the channel. Note the pending
        /// value (if any) is still delivered before `recv_timeout`
        /// reports closure.
        pub fn is_closed(&self) -> bool {
            self.shared.closed.load(Ordering::SeqCst)
        }

        /// Wait up to `d` for a value newer than anything previously
        /// returned. `None` on timeout or once closed and drained.
        pub fn recv_timeout(&mut self, d: Duration) -> Option<T> {
            let mut g = self.shared.slot.lock();
            let deadline = std::time::Instant::now() + d;
            loop {
                if g.0 != self.seen && g.1.is_some() {
                    self.seen = g.0;
                    return g.1.take();
                }
                if self.shared.closed.load(Ordering::SeqCst) {
                    return None;
                }
                let remain = deadline.checked_duration_since(std::time::Instant::now())?;
                self.shared.cv.wait_for(&mut g, remain);
            }
        }
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            self.shared.senders.fetch_add(1, Ordering::SeqCst);
            Self { shared: self.shared.clone() }
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            // Last producer gone (e.g. screen-capture callback released at
            // teardown): close so the receiver's `is_closed()` fires instead
            // of idling until an external stop flag.
            if self.shared.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.close();
            }
        }
    }
}

/// Shared statistics for both loops; read deltas for rates.
#[derive(Default)]
pub struct Counters {
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
    pub bytes: AtomicU64,
    pub drops: AtomicU64,
}

/// Initiator worker loop (one thread): pull newest frame, encode, send.
///
/// - `encode(frame, force_idr)` returns the access units for one frame.
/// - IDR is forced when the target requested one (latched inside `VideoTx`
///   by `handle_feedback`).
/// - Exits cleanly when `stop` is set or the frame channel closes.
pub fn run_initiator<F>(
    frames: &mut chan::Receiver<F>,
    mut encode: impl FnMut(&F, bool) -> Result<Vec<EncodedUnit>>,
    tx: &Mutex<VideoTx>,
    stop: &AtomicBool,
    counters: &Counters,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = frames.recv_timeout(Duration::from_millis(50)) else {
            if frames.is_closed() {
                break;
            }
            continue;
        };
        counters.frames_in.fetch_add(1, Ordering::Relaxed);
        let force_idr = {
            let mut g = tx.lock();
            g.take_idr_request()
        };
        let units = encode(&frame, force_idr)?;
        let mut g = tx.lock();
        for u in &units {
            counters.bytes.fetch_add(u.data.len() as u64, Ordering::Relaxed);
            g.send_unit(u)?;
            counters.frames_out.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Target worker loop (one thread): poll datagrams, hand reassembled units to `on_unit`
/// (decode + present inside), flush feedback (SPEC §5/§6).
pub fn run_target(
    rx: &mut VideoRx,
    fb: &FeedbackChannel,
    mut on_unit: impl FnMut(&EncodedUnit) -> Result<()>,
    stop: &AtomicBool,
    counters: &Counters,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        match rx.poll(Duration::from_millis(10)) {
            Ok(Some(unit)) => {
                counters.frames_in.fetch_add(1, Ordering::Relaxed);
                counters.bytes.fetch_add(unit.data.len() as u64, Ordering::Relaxed);
                on_unit(&unit)?;
            }
            Ok(None) => {}
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
        for fb_msg in rx.take_feedback() {
            if let Err(e) = fb.send(&fb_msg) {
                log::warn!("feedback send failed: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::chan;
    use std::time::Duration;

    #[test]
    fn latest_wins_replaces_pending() {
        let (tx, mut rx) = chan::latest_wins::<u32>();
        tx.send(1);
        tx.send(2);
        tx.send(3);
        assert_eq!(rx.recv_timeout(Duration::from_millis(50)), Some(3));
        assert_eq!(rx.recv_timeout(Duration::from_millis(10)), None);
        tx.send(4);
        assert_eq!(rx.recv_timeout(Duration::from_millis(50)), Some(4));
    }

    #[test]
    fn close_terminates_receiver() {
        let (tx, mut rx) = chan::latest_wins::<u32>();
        tx.send(9);
        tx.close();
        assert_eq!(rx.recv_timeout(Duration::from_millis(50)), Some(9));
        assert_eq!(rx.recv_timeout(Duration::from_millis(50)), None);
    }
}
