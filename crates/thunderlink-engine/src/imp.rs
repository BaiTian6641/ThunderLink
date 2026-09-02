//! macOS implementation of both engine roles. Moved verbatim from the
//! former `thunderlink` binary platform module (behavior-identical logs),
//! with [`EventSink`] emissions woven in at state milestones.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tl_macos_capture::capture::{primary_display_id, CaptureConfig, Capturer};
use tl_macos_capture::encode::Encoder;
use tl_macos_capture::testsrc::TestPattern;
use tl_macos_display::panel;
use tl_macos_display::virt::{VirtualDisplay, VirtualDisplayConfig};
use tl_macos_input::inject::{Injector, Mapping};
use tl_macos_input::tap::{EventTap, Rect};
use tl_macos_render::decode::{decoder_caps, Decoder};
use tl_macos_render::present::{Mode, PresentEvent};
use tl_net::feedback::FeedbackChannel;
use tl_net::input_chan::{InputRx, InputTx};
use tl_net::video::{VideoRx, VideoTx, VideoTxConfig};
use tl_proto::{
    default_bitrate_kbps, Chroma, InputBatch, InputEvent, Msg, PanelInfo, StreamConfig,
    TargetCaps, DEFAULT_DATAGRAM_PAYLOAD, FEEDBACK_PORT, INPUT_PORT, VIDEO_PORT,
};
use tl_session::{InitiatorSession, TargetSession};

use super::{EventSink, InitiatorConfig, Source, TargetConfig};

/// Schedules a closure onto the embedder's AppKit main thread.
pub type OnMain = StdArc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

use std::sync::Arc as StdArc;
use tl_macos_render::present::Presenter;

/// A presenter created on the embedder's AppKit MAIN thread (e.g. a sync
/// Tauri command) and driven by the engine through thread-safe handles.
/// `on_main` schedules a closure onto that same main thread (window
/// show/hide are AppKit main-thread operations; the engine worker cannot
/// call them directly).
pub struct EmbeddedPresenter {
    presenter: StdArc<Presenter>,
    on_main: OnMain,
}

impl EmbeddedPresenter {
    /// MUST be called on the AppKit main thread.
    pub fn new(
        windowed: bool,
        on_main: OnMain,
    ) -> Result<Self> {
        let mode = if windowed {
            tl_macos_render::present::Mode::Windowed
        } else {
            tl_macos_render::present::Mode::Fullscreen
        };
        Ok(Self { presenter: StdArc::new(Presenter::new(mode)?), on_main })
    }
}

impl EmbeddedPresenter {
    fn show_on_main(&self) {
        let p = self.presenter.clone();
        (self.on_main)(Box::new(move || {
            if let Err(e) = p.show() {
                log::error!("presenter show failed: {e:#}");
            }
        }));
    }

    fn hide_on_main(&self) {
        let p = self.presenter.clone();
        (self.on_main)(Box::new(move || p.hide()));
    }
}

const CONTROL_PORT: u16 = tl_proto::CONTROL_PORT;

fn any() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// Shared "why did the session end" slot: control workers write the first
/// reason; the role function emits it as `EngineEvent::Ended`.
fn reason_slot() -> Arc<Mutex<Option<String>>> {
    Arc::new(Mutex::new(None))
}

fn set_reason(slot: &Arc<Mutex<Option<String>>>, reason: impl Into<String>) {
    let mut g = slot.lock();
    if g.is_none() {
        *g = Some(reason.into());
    }
}

// ------------------------------ target ------------------------------

pub fn run_target(
    cfg: TargetConfig,
    presenter: Option<EmbeddedPresenter>,
    ev: &EventSink,
) -> Result<()> {
    let TargetConfig { bind, windowed, no_input, cancel } = cfg;
    let stop = cancel.0.clone();
    let end_reason = reason_slot();

    let panel_info = panel::main_panel().unwrap_or_else(|e| {
        log::warn!("panel info failed ({e}); using 1440x900 fallback");
        PanelInfo {
            width: 1440,
            height: 900,
            refresh_millihertz: 60_000,
            scale_x100: 200,
            edid: None,
        }
    });
    let caps = TargetCaps {
        name: "thunderlink-target".into(),
        panel: panel_info,
        decoders: decoder_caps(),
        accepts_input: !no_input,
    };
    log::info!(
        "target panel: {}x{}@{:.2}Hz scale {}%; decoders: {:?}",
        caps.panel.width,
        caps.panel.height,
        caps.panel.refresh_millihertz as f64 / 1000.0,
        caps.panel.scale_x100,
        caps.decoders.iter().map(|d| d.codec).collect::<Vec<_>>()
    );

    let listener = TcpListener::bind(SocketAddr::new(bind, CONTROL_PORT))
        .with_context(|| format!("bind control port {CONTROL_PORT}"))?;
    log::info!("listening for initiator on {bind}:{CONTROL_PORT}");

    // Stray TCP connections (port scans, discovery probes, peers that
    // hang up mid-handshake) must not kill the target — keep listening.
    // A listener-level failure only aborts after repeated attempts.
    let mut sess = {
        let mut failures = 0u32;
        loop {
            match TargetSession::accept(&listener, &caps.name, &caps) {
                Ok(sess) => break sess,
                Err(e) => {
                    failures += 1;
                    if failures >= 16 {
                        return Err(anyhow::Error::new(e).context(
                            "control listener failing repeatedly",
                        ));
                    }
                    log::warn!("inbound connection failed ({e}); listening on");
                }
            }
        }
    };
    let cfg = sess.await_config(&caps)?;
    log::info!("negotiated {cfg:?}");
    ev.emit(super::EngineEvent::Negotiated(cfg.clone()));
    let peer_ip = sess.peer_addr().ip();

    // Video path up before ack-ing Start (SPEC §4 step 4).
    let mut rx = VideoRx::bind(SocketAddr::new(bind, VIDEO_PORT))?;
    let fb = FeedbackChannel::bind(
        SocketAddr::new(bind, 0),
        SocketAddr::new(peer_ip, FEEDBACK_PORT),
    )?;
    let mut decoder = Decoder::new()?;

    // Presenter: either created by the embedder on ITS main thread
    // (EmbeddedPresenter; engine drives thread-safe handles) or created
    // here on the caller's thread — which the CLI guarantees is the
    // process main thread (AppKit contract, SPEC §9).
    let presenter_embedded = presenter;
    let submit;
    let closer_decode;
    let closer_control;
    let presenter_owned: Option<Presenter> = match presenter_embedded {
        Some(ref ep) => {
            let s = ep.presenter.submit_handle();
            submit = s.clone();
            closer_decode = s.clone();
            closer_control = s;
            None
        }
        None => {
            let mode = if windowed { Mode::Windowed } else { Mode::Fullscreen };
            let p = Presenter::new(mode)?; // caller is main thread (CLI)
            let s = p.submit_handle();
            submit = s.clone();
            closer_decode = s.clone();
            closer_control = s;
            Some(p)
        }
    };

    let counters = Arc::new(tl_video::Counters::default());

    let start = sess.await_start()?;
    start.ack_ready()?;
    log::info!("streaming started");
    ev.emit(super::EngineEvent::Streaming);

    // Decode worker: UDP -> VT decode -> presenter (latest-wins).
    {
        let stop = stop.clone();
        let counters = counters.clone();
        let ev = ev.clone();
        std::thread::Builder::new().name("decode".into()).spawn(move || {
            let mut n = 0u64;
            let r = tl_video::run_target(&mut rx, &fb, |unit| {
                for f in decoder.decode(unit)? {
                    n += 1;
                    if n.is_multiple_of(120) {
                        let lat_ms = (tl_proto::time::now_us() - f.pts_us()) as f64 / 1000.0;
                        log::info!("frame {n}: encode-to-decode ~{lat_ms:.1} ms");
                        ev.emit(super::EngineEvent::LatencyMs(lat_ms));
                    }
                    submit.submit(f);
                }
                Ok(())
            }, &stop, &counters);
            if let Err(e) = r {
                log::error!("decode loop ended: {e}");
            }
            stop.store(true, Ordering::SeqCst);
            closer_decode.request_close();
        })?;
    }

    // Control worker: heartbeats/stats out, Stop/Bye in.
    {
        let stop = stop.clone();
        let counters = counters.clone();
        let ev = ev.clone();
        let end_reason = end_reason.clone();
        std::thread::Builder::new().name("control".into()).spawn(move || {
            let chan = sess.channel();
            let _ = chan.set_read_timeout(Some(Duration::from_millis(500)));
            let mut last_bytes = 0u64;
            let mut last_frames = 0u64;
            // 1 s send cadence by deadline, NOT iteration count: echoed
            // heartbeats make some iterations near-instant, so a toggle
            // under-counts elapsed time (halves reported fps).
            let mut last_send = std::time::Instant::now();
            let mut last_recv = std::time::Instant::now();
            let mut rtt_us = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let dt = last_send.elapsed();
                if dt >= Duration::from_secs(1) {
                    let _ = chan.send(&Msg::Heartbeat { ts_us: tl_proto::time::now_us() });
                    let bytes = counters.bytes.load(Ordering::Relaxed);
                    let frames = counters.frames_in.load(Ordering::Relaxed);
                    let secs = dt.as_secs_f64();
                    let report = tl_proto::StatsReport {
                        decoded_fps: ((frames - last_frames) as f64 / secs) as u32,
                        bitrate_kbps: (((bytes - last_bytes) * 8) as f64 / secs / 1000.0) as u32,
                        rtt_us,
                        ..Default::default()
                    };
                    let _ = chan.send(&Msg::Stats(report.clone()));
                    ev.emit(super::EngineEvent::Stats(report));
                    last_send = std::time::Instant::now();
                    last_bytes = bytes;
                    last_frames = frames;
                }
                match chan.recv() {
                    Ok(Msg::Heartbeat { ts_us }) => {
                        // Echo of our own heartbeat: RTT probe result.
                        rtt_us = (tl_proto::time::now_us() - ts_us).max(0) as u32;
                        last_recv = std::time::Instant::now();
                    }
                    Ok(Msg::Stop) | Ok(Msg::Bye) => {
                        log::info!("session ended by initiator");
                        set_reason(&end_reason, "ended by initiator");
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
            closer_control.request_close();
        })?;
    }

    // Input forwarding: event tap -> 500 Hz UDP batches.
    if !no_input {
        let input_tx =
            InputTx::bind(SocketAddr::new(bind, 0), SocketAddr::new(peer_ip, INPUT_PORT))?;
        let (w, h) = panel::main_display_points().unwrap_or((1440.0, 900.0));
        let bounds = Rect { x: 0.0, y: 0.0, w, h };
        let (evt_tx, mut evt_rx) = tl_video::chan::latest_wins::<InputEvent>();
        match EventTap::start(bounds, Box::new(move |ev| evt_tx.send(ev))) {
            Ok(tap) => {
                let stop = stop.clone();
                std::thread::Builder::new().name("input".into()).spawn(move || {
                    // The tap must outlive the loop; the system
                    // removes it when dropped.
                    let _tap = tap;
                    let mut seq = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let Some(ev) = evt_rx.recv_timeout(Duration::from_millis(2)) else {
                            if evt_rx.is_closed() {
                                break;
                            }
                            continue;
                        };
                        seq = seq.wrapping_add(1);
                        if let Err(e) = input_tx.send(&InputBatch { seq, events: vec![ev] })
                        {
                            log::warn!("input send: {e}");
                        }
                    }
                })?;
            }
            Err(e) => {
                log::warn!("event tap unavailable ({e}); input forwarding disabled");
                ev.warn(format!("input forwarding disabled: {e}"));
            }
        }
    }

    // Present until the window closes or the session stops.
    match (presenter_owned, presenter_embedded) {
        (Some(p), _) => {
            // CLI path: the caller's thread IS the AppKit main thread; run()
            // owns the event loop until close.
            let stop_flag = stop.clone();
            p.run(move |ev| {
                if ev == PresentEvent::CloseRequested {
                    stop_flag.store(true, Ordering::SeqCst);
                }
            })?;
        }
        (None, Some(ep)) => {
            // Embedded path: the embedder's runloop pumps AppKit events;
            // show/hide on its main thread, render loop from here.
            ep.show_on_main();
            ep.presenter.start_render()?;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let closed = ep
                    .presenter
                    .poll_events()
                    .iter()
                    .any(|e| matches!(e, PresentEvent::CloseRequested));
                if closed {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            ep.presenter.stop_render();
            ep.hide_on_main();
        }
        (None, None) => unreachable!("one presenter path is always taken"),
    }
    stop.store(true, Ordering::SeqCst);
    // Let worker threads observe the stop flag before process exit.
    std::thread::sleep(Duration::from_millis(200));
    let reason = end_reason.lock().take().unwrap_or_else(|| "window closed".into());
    log::info!("target exiting");
    ev.emit(super::EngineEvent::Ended(reason));
    Ok(())
}

// ---------------------------- initiator ------------------------------

pub fn run_initiator(cfg: InitiatorConfig, ev: &EventSink) -> Result<()> {
    let InitiatorConfig {
        addr,
        source,
        codec,
        bitrate_kbps,
        fps,
        res,
        virtual_display,
        max_frames,
        cancel,
    } = cfg;
    let stop = cancel.0.clone();
    let end_reason = reason_slot();

    let mut sess = InitiatorSession::connect(addr, "thunderlink-initiator")?;
    let caps = sess.caps().clone();

    // Default: stream at the target panel's NATIVE resolution (SPEC §1).
    let (width, height) = res.unwrap_or((caps.panel.width, caps.panel.height));
    let fps_milli = fps.map(|f| f * 1000).unwrap_or(caps.panel.refresh_millihertz);
    let codec = codec.unwrap_or(tl_proto::Codec::Hevc);
    let bitrate =
        bitrate_kbps.unwrap_or_else(|| default_bitrate_kbps(width, height, codec));
    let stream_cfg = StreamConfig {
        codec,
        width,
        height,
        fps_millihertz: fps_milli,
        bitrate_kbps: bitrate,
        chroma: Chroma::Yuv420,
        hdr: false,
    };
    log::info!(
        "requesting {width}x{height}@{:.2}Hz {codec:?} {bitrate} kbps",
        fps_milli as f64 / 1000.0
    );
    sess.configure(&stream_cfg)?;
    ev.emit(super::EngineEvent::Negotiated(stream_cfg.clone()));
    // Extended-desktop mode: create a private-API virtual display that
    // the OS renders onto (PLAN §4.1). It carries the target panel's
    // native resolution; HiDPI when the target is Retina-class.
    let mut vdisp: Option<VirtualDisplay> = None;
    if virtual_display {
        let vd = VirtualDisplay::create(VirtualDisplayConfig {
            width,
            height,
            refresh_millihertz: fps_milli,
            hidpi: caps.panel.scale_x100 >= 150,
            name: "ThunderLink".into(),
        })
        .context("create virtual display (CGVirtualDisplay)")?;
        log::info!("virtual display created (CGDirectDisplayID {})", vd.display_id());
        vdisp = Some(vd);
        // WindowServer placement is async; let it settle before reading
        // the display's global-coordinate frame for the input mapping.
        std::thread::sleep(Duration::from_millis(150));
    }

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

    // Frame source: callback-driven (screen) or paced feeder (pattern).
    let (frame_tx, mut frame_rx) = tl_video::chan::latest_wins();
    let stop = stop.clone();

    let mut capturer: Option<Capturer> = None;
    match source {
        Source::Screen => {
            let mut c = Capturer::new(CaptureConfig {
                display_id: match &vdisp {
                    Some(vd) => vd.display_id(),
                    None => primary_display_id()?,
                },
                fps: (fps_milli / 1000).max(1),
                queue_depth: 2,
                show_cursor: true,
            })?;
            c.start(Box::new(move |f| frame_tx.send(f)))
                .context("start screen capture")?;
            capturer = Some(c);
        }
        Source::TestPattern => {
            let fps_whole = (fps_milli / 1000).max(1);
            let mut tp = TestPattern::new(width, height, fps_whole);
            let stop = stop.clone();
            std::thread::Builder::new().name("testsrc".into()).spawn(move || {
                let frame_dur = Duration::from_secs_f64(1.0 / fps_whole as f64);
                // Absolute-tick pacing: drawing + encoding time must not
                // accumulate into frame delay (SPEC §11 60 fps).
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
    }

    let mut encoder = Encoder::new(&stream_cfg).context("create VideoToolbox encoder")?;

    // Everything is ready; open the stream.
    sess.start()?;
    log::info!("streaming started");
    ev.emit(super::EngineEvent::Streaming);

    // Feedback worker: NACK retransmits / IDR requests from target.
    {
        let stop = stop.clone();
        let tx = tx.clone();
        std::thread::Builder::new().name("feedback".into()).spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match fb.poll(Duration::from_millis(100)) {
                    Ok(list) => {
                        let mut g = tx.lock();
                        for f in &list {
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

    // Input mapping: the streamed display's rect in this machine's
    // global coordinates — the virtual display's frame when extended,
    // else the main display (mirror).
    let input_map: Option<Mapping> = match &vdisp {
        Some(vd) => match panel::display_frame(vd.display_id()) {
            Ok((x, y, w, h)) => Some(Mapping { origin_x: x, origin_y: y, width: w, height: h }),
            Err(e) => {
                log::warn!("virtual display frame unavailable ({e}); input injection disabled");
                ev.warn("input injection disabled: virtual display frame unavailable");
                None
            }
        },
        None => match panel::main_display_points() {
            Ok((w, h)) => Some(Mapping { origin_x: 0.0, origin_y: 0.0, width: w, height: h }),
            Err(e) => {
                log::warn!("display geometry unavailable ({e}); input injection disabled");
                ev.warn("input injection disabled: display geometry unavailable");
                None
            }
        },
    };

    // Input inject worker: UDP batches -> CGEventPost on this machine.
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("input-inject".into()).spawn(move || {
            let mut inj = match Injector::new() {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("injector unavailable ({e}); input injection disabled");
                    return;
                }
            };
            let Some(map) = input_map else { return }; // failure already logged
            while !stop.load(Ordering::Relaxed) {
                match input_rx.poll(Duration::from_millis(100)) {
                    Ok(batches) => {
                        for b in batches {
                            for ev in &b.events {
                                let r = if matches!(ev, InputEvent::Leave) {
                                    inj.release_all()
                                } else {
                                    inj.inject(ev, &map)
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

    // Control worker: echo heartbeats, log target stats, watch Stop/Bye.
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<()>();
    {
        let stop = stop.clone();
        let ev = ev.clone();
        let end_reason = end_reason.clone();
        std::thread::Builder::new().name("control".into()).spawn(move || {
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
                        ev.emit(super::EngineEvent::Stats(s));
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
    }

    // Encode worker (this thread): frames -> VT encode -> UDP.
    let counters = tl_video::Counters::default();
    let frames_sent = AtomicU64::new(0);
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
            // Wall-clock stamp BEFORE encode (source pts domains differ:
            // test-pattern is zero-based, SCK is host-clock). pts flows
            // untouched through VT decode, so the target's log measures
            // encode-to-decode latency in one comparable domain.
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

    // Clean teardown: encoder/capturer drop here, control sends Stop/Bye.
    stop.store(true, Ordering::SeqCst);
    drop(encoder);
    drop(capturer.take());
    // After the capturer: it references the virtual display's id.
    drop(vdisp.take());
    let _ = ctrl_rx.recv_timeout(Duration::from_secs(2));
    let reason = end_reason
        .lock()
        .take()
        .unwrap_or_else(|| "frames complete".into());
    log::info!("initiator exiting");
    ev.emit(super::EngineEvent::Ended(reason));
    Ok(())
}
