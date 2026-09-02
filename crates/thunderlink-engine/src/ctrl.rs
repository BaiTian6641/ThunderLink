//! Platform-neutral initiator control worker: echo heartbeats for RTT,
//! surface target stats, watch Stop/Bye, enforce the 5 s silence teardown
//! (SPEC §4). Shared by the macOS and Linux imps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tl_proto::Msg;
use tl_session::InitiatorSession;

use super::EngineEvent;

pub(crate) type EndReason = Arc<Mutex<Option<String>>>;

/// Spawn the control worker; returns a receiver signaled when it exits
/// (the caller waits on it during clean teardown).
pub(crate) fn spawn_initiator_control_worker(
    sess: InitiatorSession,
    stop: Arc<AtomicBool>,
    ev: super::EventSink,
    end_reason: EndReason,
) -> std::io::Result<std::sync::mpsc::Receiver<()>> {
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
        let mut sess = sess;
        let chan = sess.channel();
        let _ = chan.set_read_timeout(Some(Duration::from_millis(500)));
        let mut last_hb = 0i64;
        let mut last_recv = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            match chan.recv() {
                Ok(Msg::Heartbeat { ts_us }) => {
                    last_recv = std::time::Instant::now();
                    if ts_us != last_hb {
                        // Echo for RTT measurement on the target.
                        let _ = chan.send(&Msg::Heartbeat { ts_us });
                        last_hb = ts_us;
                    }
                }
                Ok(Msg::Stats(s)) => {
                    last_recv = std::time::Instant::now();
                    log::info!(
                        "target: {} fps decoded, {} kbps, rtt {} us",
                        s.decoded_fps,
                        s.bitrate_kbps,
                        s.rtt_us
                    );
                    ev.emit(EngineEvent::Stats(s));
                }
                Ok(Msg::Stop) | Ok(Msg::Bye) => {
                    set_reason(&end_reason, "ended by target");
                    break;
                }
                Ok(_) => last_recv = std::time::Instant::now(),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    log::warn!("control channel: {e}");
                    set_reason(&end_reason, format!("control channel: {e}"));
                    break;
                }
            }
            // Liveness: tear down on 5 s of silence (SPEC §4).
            if last_recv.elapsed() > Duration::from_secs(5) {
                log::warn!("control channel silent for 5 s; tearing down");
                set_reason(&end_reason, "control channel silent for 5 s");
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);
        let _ = chan.send(&Msg::Stop);
        let _ = chan.send(&Msg::Bye);
        let _ = ctrl_tx.send(());
    })?;
    Ok(ctrl_rx)
}

pub(crate) fn set_reason(slot: &EndReason, reason: impl Into<String>) {
    let mut g = slot.lock();
    if g.is_none() {
        *g = Some(reason.into());
    }
}
