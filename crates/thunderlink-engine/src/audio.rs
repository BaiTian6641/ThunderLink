//! Platform-neutral audio worker glue (SPEC §12): initiator feeder and
//! target playback loop over `tl-audio` primitives. Shared by imps.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use tl_audio::{AudioRx, AudioTx, JitterBuffer, OpusDecoder, OpusEncoder, PopResult, SineSource};


use super::{AudioSource, AudioStats, EventSink};

const FRAME: Duration = Duration::from_millis(10);

/// Spawn the initiator audio feeder: source → opus → UDP at 100 pps.
/// Wall-clock pts (SPEC §12.4); absolute-tick pacing like video.
pub(crate) fn spawn_audio_feeder(
    bind: SocketAddr,
    peer: SocketAddr,
    source: AudioSource,
    bitrate_kbps: u32,
    stop: std::sync::Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    let tx = AudioTx::bind(bind, peer).context("bind audio channel")?;
    let mut seq: u32 = 0;
    let encoder = OpusEncoder::new(bitrate_kbps).context("create opus encoder")?;
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut enc = encoder;
            match source {
                AudioSource::Sine { freq_hz } => {
                    let mut src = SineSource::new(freq_hz);
                    let mut next_tick = Instant::now() + FRAME;
                    while !stop.load(Ordering::Relaxed) {
                        let pcm = src.next_frame();
                        let pts = tl_proto::time::now_us();
                        match enc.encode(&pcm) {
                            Ok(packet) => {
                                // DTX may produce empty packets; still seq-
                                // advance so the receiver's jitter math
                                // holds, but skip the wire write.
                                if !packet.is_empty() {
                                    if let Err(e) = tx.send(seq, pts, &packet) {
                                        log::warn!("audio send: {e}");
                                    }
                                }
                            }
                            Err(e) => log::warn!("audio encode: {e}"),
                        }
                        seq = seq.wrapping_add(1);
                        let now = Instant::now();
                        if next_tick > now {
                            std::thread::sleep(next_tick - now);
                        }
                        next_tick += FRAME;
                    }
                }
                AudioSource::System => {
                    log::warn!("system audio capture is macOS-only; audio feeder idle");
                }
            }
        })
        .context("spawn audio feeder")
}

/// Run the target audio playback loop until `stop`: UDP → jitter buffer →
/// opus decode → `write` callback (interleaved stereo f32). Emits ~1 Hz
/// AudioStats on `ev` (SPEC §12.5). macOS only today (the Linux target
/// role has no presenter/output yet).
#[cfg(target_os = "macos")]
pub(crate) fn run_audio_sink(
    bind: SocketAddr,
    stop: &std::sync::Arc<AtomicBool>,
    ev: &EventSink,
    mut write: impl FnMut(&[f32]),
) -> Result<()> {
    let mut rx = AudioRx::bind(bind).context("bind audio rx")?;
    let mut jb = JitterBuffer::new(Duration::from_millis(40));
    let mut dec = OpusDecoder::new().context("create opus decoder")?;
    let mut pcm_out: Vec<f32> = Vec::with_capacity(480 * 2);
    let mut last_report = Instant::now();
    let mut next_tick = Instant::now() + FRAME;
    // Drift: wall clock vs the pts of the newest packet we ever played
    // (positive = playback head lags the source clock; SPEC §12.5).
    let mut newest_played_pts: Option<i64> = None;
    while !stop.load(Ordering::Relaxed) {
        for pkt in rx.poll(FRAME).context("audio poll")? {
            jb.push(pkt);
        }
        // Real-time pacing: pop EXACTLY one 10 ms frame per tick (Play or
        // Conceal); the 40 ms jitter depth absorbs arrival variance. The
        // first draft popped up to 4 frames/tick — a 4x drain that
        // late-dropped ~70% of packets.
        {
            match jb.pop() {
                PopResult::Play(pkt) => {
                    newest_played_pts = Some(match newest_played_pts {
                        Some(p) if pkt.pts_us <= p => p,
                        _ => pkt.pts_us,
                    });
                    let decoded = dec.decode(Some(&pkt.payload)).unwrap_or_default();
                    pcm_out.clear();
                    pcm_out.extend(decoded.iter().map(|&s| s as f32 / 32768.0));
                    write(&pcm_out);
                }
                PopResult::Conceal => {
                    let decoded = dec.decode(None).unwrap_or_default();
                    pcm_out.clear();
                    pcm_out.extend(decoded.iter().map(|&s| s as f32 / 32768.0));
                    write(&pcm_out);
                }
                PopResult::Empty => {} // below depth / drained: wait for the next tick
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let s = jb.stats();
            let drift_ms = newest_played_pts
                .map(|p| (tl_proto::time::now_us() - p) as f64 / 1000.0)
                .unwrap_or(0.0);
            ev.emit(super::EngineEvent::Audio(AudioStats {
                played: s.played,
                concealed: s.concealed,
                dropped: s.dropped_late + s.dropped_gap,
                drift_ms,
            }));
            log::info!(
                "audio: {} played, {} concealed, {} dropped, drift {drift_ms:+.1} ms",
                s.played,
                s.concealed,
                s.dropped_late + s.dropped_gap
            );
        }
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep((next_tick - now).min(FRAME));
        }
        next_tick += FRAME;
    }
    Ok(())
}

/// Sanity-check the channel constants the wire format relies on.
#[cfg(test)]
mod tests {
    #[test]
    fn audio_constants() {
        use tl_proto::{AUDIO_MAGIC, AUDIO_PORT};
        assert_eq!(AUDIO_PORT, 47780);
        assert_eq!(&AUDIO_MAGIC.to_le_bytes(), b"TLA1");
    }
}
