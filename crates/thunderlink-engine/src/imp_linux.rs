//! Linux implementation of the initiator role (docs/LINUX-PORT.md).
//!
//! Mirror of the macOS imp with Linux primitives: x11 screen capture or
//! the shared test-pattern source, x264 software encode (Annex B,
//! param sets on IDR, SPEC §5), uinput injection for the input
//! backchannel. Extended-display mode (EVDI) is not implemented yet.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tl_linux_capture::{Encoder as X264Encoder, RawFrame, ScreenCapturer, TestPattern};
use tl_linux_input::inject::Injector;
use tl_net::feedback::FeedbackChannel;
use tl_net::input_chan::InputRx;
use tl_net::video::{VideoTx, VideoTxConfig};
use tl_proto::{
    default_bitrate_kbps, InputEvent, StreamConfig, DEFAULT_DATAGRAM_PAYLOAD, FEEDBACK_PORT,
    INPUT_PORT, VIDEO_PORT,
};
use tl_session::InitiatorSession;

use super::ctrl::{spawn_initiator_control_worker, EndReason};
use super::{EventSink, InitiatorConfig, Source};

enum EncoderChoice {
    FFmpeg(tl_linux_capture::ffmpeg::FFmpegEncoder),
    X264(X264Encoder),
}

impl EncoderChoice {
    fn request_idr(&mut self) {
        match self {
            Self::FFmpeg(e) => e.request_idr(),
            Self::X264(e) => e.request_idr(),
        }
    }
    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        match self {
            Self::FFmpeg(e) => e.set_bitrate(kbps),
            Self::X264(e) => e.set_bitrate(kbps),
        }
    }
    fn encode(&mut self, frame: &RawFrame) -> Result<Vec<tl_proto::EncodedUnit>> {
        match self {
            Self::FFmpeg(e) => e.encode(frame),
            Self::X264(e) => e.encode(frame),
        }
    }
}

fn any() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

pub fn run_initiator(cfg: InitiatorConfig, ev: &EventSink) -> Result<()> {
    let InitiatorConfig {
        addr,
        source,
        codec,
        bitrate_kbps,
        fps,
        res,
        virtual_display: _,
        max_frames,
        audio,
        cancel,
    } = cfg;
    let stop = cancel.0.clone();
    let end_reason: EndReason = Arc::new(Mutex::new(None));

    // Linux codec selection: prefer HEVC via the ffmpeg subprocess encoder
    // (libx265) when available; fall back to x264 H.264 (SPEC §8 fallback).
    let use_ffmpeg_hevc = codec != Some(tl_proto::Codec::H264)
        && tl_linux_capture::ffmpeg::FFmpegEncoder::available();
    let codec = if use_ffmpeg_hevc {
        log::info!("HEVC via ffmpeg/libx265 available; using HEVC");
        tl_proto::Codec::Hevc
    } else {
        if codec != Some(tl_proto::Codec::H264) {
            log::warn!(
                "HEVC unavailable (no ffmpeg/libx265); using H.264 fallback (SPEC §8)"
            );
        }
        tl_proto::Codec::H264
    };

    let mut sess = InitiatorSession::connect(addr, "thunderlink-initiator")?;
    let caps = sess.caps().clone();
    if matches!(audio, Some(super::AudioSource::System)) {
        anyhow::bail!("system audio capture is macOS-only in v1 (PipeWire planned)");
    }

    // Default: stream at the target panel's NATIVE resolution (SPEC §1),
    // clamped to the target's decoder capability for the chosen codec.
    let (mut width, mut height) = res.unwrap_or((caps.panel.width, caps.panel.height));
    if let Some(d) = caps.decoders.iter().find(|d| d.codec == codec) {
        if width > d.max_width || height > d.max_height {
            let scale = (d.max_width as f64 / width as f64)
                .min(d.max_height as f64 / height as f64)
                .min(1.0);
            width = (width as f64 * scale) as u32 & !1; // even dimensions
            height = (height as f64 * scale) as u32 & !1;
            log::warn!(
                "target decoder caps {d:?}; clamped to {width}x{height}"
            );
        }
    }
    let fps_milli = fps.map(|f| f * 1000).unwrap_or(caps.panel.refresh_millihertz);
    let bitrate = bitrate_kbps.unwrap_or_else(|| default_bitrate_kbps(width, height, codec));
    let stream_cfg = StreamConfig {
        codec,
        width,
        height,
        fps_millihertz: fps_milli,
        bitrate_kbps: bitrate,
        chroma: tl_proto::Chroma::Yuv420,
        hdr: false,
        audio: audio.is_some(),
        audio_bitrate_kbps: audio.map(|_| 192),
    };
    log::info!(
        "requesting {width}x{height}@{:.2}Hz {codec:?} {bitrate} kbps",
        fps_milli as f64 / 1000.0
    );
    sess.configure(&stream_cfg)?;
    ev.emit(super::EngineEvent::Negotiated(stream_cfg.clone()));

    let peer_ip = sess.peer_addr().ip();
    let tx = Arc::new(Mutex::new(VideoTx::bind(
        SocketAddr::new(any(), 0),
        VideoTxConfig {
            peer: SocketAddr::new(peer_ip, VIDEO_PORT),
            datagram_payload: DEFAULT_DATAGRAM_PAYLOAD,
            ring_bytes: 16 << 20,
        },
    )?));
    let fb = FeedbackChannel::bind(
        SocketAddr::new(any(), FEEDBACK_PORT),
        SocketAddr::new(peer_ip, FEEDBACK_PORT),
    )?;
    let input_rx = InputRx::bind(SocketAddr::new(any(), INPUT_PORT))?;

    // Frame source: paced feeder (pattern) or X11 grab.
    let (frame_tx, mut frame_rx) = tl_video::chan::latest_wins::<RawFrame>();
    let stop = stop.clone();
    match source {
        Source::TestPattern => {
            let fps_whole = (fps_milli / 1000).max(1);
            let mut tp = TestPattern::new(width, height, fps_whole);
            let stop = stop.clone();
            let max_frames = max_frames;
            std::thread::Builder::new().name("testsrc".into()).spawn(move || {
                let frame_dur = Duration::from_secs_f64(1.0 / fps_whole as f64);
                let mut next_tick = std::time::Instant::now() + frame_dur;
                let mut sent = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(max) = max_frames {
                        if sent >= max {
                            break;
                        }
                    }
                    match tp.next() {
                        Ok(f) => frame_tx.send(f),
                        Err(e) => {
                            log::error!("test pattern: {e}");
                            break;
                        }
                    }
                    sent += 1;
                    let now = std::time::Instant::now();
                    if next_tick > now {
                        std::thread::sleep(next_tick - now);
                    }
                    next_tick += frame_dur;
                }
                frame_tx.close();
            })?;
        }
        Source::Screen => {
            // The screen path needs a live X server (Wayland portal
            // capture is a follow-up; see docs/LINUX-PORT.md).
            let fps_whole = (fps_milli / 1000).max(1);
            let mut cap = ScreenCapturer::new(fps_whole)
                .context("open X11 screen capture (is $DISPLAY set?)")?;
            log::info!(
                "capturing X11 root window {}x{}",
                cap.width(),
                cap.height()
            );
            let stop = stop.clone();
            let max_frames = max_frames;
            std::thread::Builder::new().name("x11grab".into()).spawn(move || {
                let frame_dur = Duration::from_secs_f64(1.0 / fps_whole as f64);
                let mut next_tick = std::time::Instant::now() + frame_dur;
                let mut sent = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(max) = max_frames {
                        if sent >= max {
                            break;
                        }
                    }
                    match cap.next_frame() {
                        Ok(f) => frame_tx.send(f),
                        Err(e) => {
                            log::error!("x11 grab: {e}");
                            break;
                        }
                    }
                    sent += 1;
                    let now = std::time::Instant::now();
                    if next_tick > now {
                        std::thread::sleep(next_tick - now);
                    }
                    next_tick += frame_dur;
                }
                frame_tx.close();
            })?;
        }
    }

    let mut encoder: EncoderChoice = if codec == tl_proto::Codec::Hevc {
        EncoderChoice::FFmpeg(
            tl_linux_capture::ffmpeg::FFmpegEncoder::new(&stream_cfg)
                .context("create ffmpeg HEVC encoder")?,
        )
    } else {
        EncoderChoice::X264(
            X264Encoder::new(&stream_cfg).context("create x264 encoder")?,
        )
    };

    sess.start()?;
    log::info!("streaming started");
    ev.emit(super::EngineEvent::Streaming);

    // Audio feeder (SPEC §12): sine source only on Linux v1.
    if let Some(source) = audio {
        let peer = SocketAddr::new(peer_ip, tl_proto::AUDIO_PORT);
        let local = SocketAddr::new(any(), 0);
        super::audio::spawn_audio_feeder(local, peer, source, 192, stop.clone())?;
    }

    // Adaptive-bitrate target shared with the encode closure (SPEC §8).
    let bitrate_target = Arc::new(AtomicU64::new(0));
    let base_bitrate = stream_cfg.bitrate_kbps;

    // Feedback worker: NACK retransmits, IDR requests, and the adaptive
    // bitrate ladder (SPEC §8) driven by receiver Reports.
    {
        let stop = stop.clone();
        let tx = tx.clone();
        let bitrate_target = bitrate_target.clone();
        std::thread::Builder::new().name("feedback".into()).spawn(move || {
            let mut ladder = super::ladder::BitrateLadder::new(base_bitrate);
            while !stop.load(Ordering::Relaxed) {
                match fb.poll(Duration::from_millis(100)) {
                    Ok(list) => {
                        let mut g = tx.lock();
                        for f in &list {
                            if let tl_proto::Feedback::Report { lost_packets, received_frames, jitter_us, .. } = f {
                                if let super::ladder::LadderAction::Set(kbps) =
                                    ladder.report(*lost_packets, *received_frames, *jitter_us)
                                {
                                    log::info!("adaptive bitrate: {kbps} kbps");
                                    bitrate_target.store(kbps as u64, Ordering::SeqCst);
                                }
                            }
                            if let Err(e) = g.handle_feedback(f) {
                                log::warn!("feedback handling: {e}");
                            }
                        }
                    }
                    Err(e) => log::warn!("feedback poll: {e}"),
                }
            }
        })?;
    }

    // Input inject worker: UDP batches -> uinput virtual devices.
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("input-inject".into()).spawn(move || {
            let mut inj = match Injector::new() {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("uinput injector unavailable ({e}); input injection disabled");
                    return;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                match input_rx.poll(Duration::from_millis(100)) {
                    Ok(batches) => {
                        for b in batches {
                            for ev in &b.events {
                                let r = if matches!(ev, InputEvent::Leave) {
                                    inj.release_all()
                                } else {
                                    inj.inject(ev)
                                };
                                if let Err(e) = r {
                                    log::warn!("inject: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => log::warn!("input poll: {e}"),
                }
            }
        })?;
    }

    // Control worker (shared, platform-neutral).
    let ctrl_rx = spawn_initiator_control_worker(sess, stop.clone(), ev.clone(), end_reason.clone())?;

    // Encode worker (this thread): frames -> x264 -> UDP.
    let counters = tl_video::Counters::default();
    let frames_sent = AtomicU64::new(0);
    let mut last_bitrate = stream_cfg.bitrate_kbps;
    let bitrate_target = bitrate_target.clone();
    let stop_enc = stop.clone();
    tl_video::run_initiator(
        &mut frame_rx,
        |frame, force_idr| {
            if let Some(max) = max_frames {
                if frames_sent.load(Ordering::Relaxed) >= max {
                    stop_enc.store(true, Ordering::Relaxed);
                }
            }
            if force_idr {
                encoder.request_idr();
            }
            let want_kbps = bitrate_target.load(Ordering::SeqCst) as u32;
            if want_kbps != 0 && want_kbps != last_bitrate {
                match encoder.set_bitrate(want_kbps) {
                    Ok(()) => {
                        last_bitrate = want_kbps;
                        log::info!("encoder bitrate now {want_kbps} kbps");
                    }
                    Err(e) => log::warn!("set_bitrate({want_kbps}): {e}"),
                }
            }
            // Wall-clock stamp BEFORE encode (source pts domains differ);
            // pts flows untouched through decode, so the target's log
            // measures encode-to-decode latency in one domain.
            let t0 = tl_proto::time::now_us();
            let mut units = encoder.encode(frame)?;
            for u in &mut units {
                u.pts_us = t0;
            }
            frames_sent.fetch_add(1, Ordering::Relaxed);
            Ok(units)
        },
        &tx,
        &stop,
        &counters,
    )?;

    stop.store(true, Ordering::SeqCst);
    let _ = ctrl_rx.recv_timeout(Duration::from_secs(2));
    let reason = end_reason
        .lock()
        .take()
        .unwrap_or_else(|| "frames complete".into());
    log::info!("initiator exiting");
    ev.emit(super::EngineEvent::Ended(reason));
    Ok(())
}
