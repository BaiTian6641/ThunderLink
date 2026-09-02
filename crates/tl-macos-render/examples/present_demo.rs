//! Windowed presenter runtime demo / E2E driver (SPEC §11 smoke surface).
//!
//! Runs TestPattern → HEVC Encoder → Decoder → Presenter for ~5 s on the
//! main thread (AppKit contract), then closes the window programmatically
//! and verifies `CloseRequested` was delivered. Exits non-zero on failure.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use tl_macos_capture::encode::Encoder;
use tl_macos_capture::testsrc::TestPattern;
use tl_macos_render::decode::Decoder;
use tl_macos_render::present::{Mode, PresentEvent, Presenter};
use tl_proto::{Chroma, Codec, StreamConfig};

type DispatchTimeT = u64;
const DISPATCH_TIME_NOW: DispatchTimeT = 0;

extern "C" {
    // dispatch_get_main_queue() is header-inline over this exported global.
    #[link_name = "_dispatch_main_q"]
    static DISPATCH_MAIN_Q: std::ffi::c_void;
    fn dispatch_time(when: DispatchTimeT, delta: i64) -> DispatchTimeT;
    fn dispatch_after(when: DispatchTimeT, queue: *mut std::ffi::c_void, block: &block2::Block<dyn Fn()>);
}

fn main() -> Result<()> {
    let _ = env_logger::try_init();
    let cfg = StreamConfig {
        codec: Codec::Hevc,
        width: 640,
        height: 480,
        fps_millihertz: 60_000,
        bitrate_kbps: 8_000,
        chroma: Chroma::Yuv420,
        hdr: false,
        audio: false,
        audio_bitrate_kbps: None,
    };

    let presenter = Presenter::new(Mode::Windowed)?;
    eprintln!("present_demo: content_size = {:?}", presenter.content_size());

    // Feeder: decode worker on its own thread, submitting via the handle —
    // exactly how thunderlink-target drives it.
    let handle = presenter.submit_handle();
    let feeder_failed = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicUsize::new(0));
    let events_seen = Arc::new(AtomicUsize::new(0));
    let close_seen = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        let failed = feeder_failed.clone();
        let submitted = submitted.clone();
        scope.spawn(move || {
            let r: Result<()> = (|| {
                let mut src = TestPattern::new(cfg.width, cfg.height, 60);
                let mut enc = Encoder::new(&cfg)?;
                let mut dec = Decoder::new()?;
                for _ in 0..240 {
                    let frame = src.next()?;
                    for unit in enc.encode(&frame)? {
                        for df in dec.decode(&unit)? {
                            handle.submit(df);
                            submitted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(16));
                }
                Ok(())
            })();
            if let Err(e) = r {
                eprintln!("present_demo: feeder failed: {e:#}");
                failed.store(true, Ordering::Relaxed);
                handle.request_close();
            }
        });

        // Auto-close after 5 s on the main thread (GCD main-queue blocks are
        // drained by the presenter's event pump).
        let close_block = RcBlock::new(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let app = NSApplication::sharedApplication(mtm);
            for window in app.windows().iter() {
                window.performClose(None);
            }
        });
        // SAFETY: libdispatch symbols; dispatch_after copies the block, so the
        // stack reference is only needed for the call's duration.
        unsafe {
            let when = dispatch_time(DISPATCH_TIME_NOW, 5_000_000_000);
            let queue = &raw const DISPATCH_MAIN_Q as *mut std::ffi::c_void;
            dispatch_after(when, queue, &close_block);
        }

        let close_seen = close_seen.clone();
        let events_seen = events_seen.clone();
        let r = presenter.run(move |event| {
            events_seen.fetch_add(1, Ordering::Relaxed);
            if matches!(event, PresentEvent::CloseRequested) {
                close_seen.fetch_add(1, Ordering::Relaxed);
            }
            eprintln!("present_demo: event {event:?}");
        });
        if let Err(e) = r {
            eprintln!("present_demo: presenter.run failed: {e:#}");
            feeder_failed.store(true, Ordering::Relaxed);
        }
    });

    let n_submitted = submitted.load(Ordering::Relaxed);
    eprintln!(
        "present_demo: submitted={} events={} close={}",
        n_submitted,
        events_seen.load(Ordering::Relaxed),
        close_seen.load(Ordering::Relaxed)
    );
    if feeder_failed.load(Ordering::Relaxed) {
        anyhow::bail!("feeder/presenter failed");
    }
    anyhow::ensure!(n_submitted >= 200, "too few frames submitted: {n_submitted}");
    anyhow::ensure!(close_seen.load(Ordering::Relaxed) == 1, "CloseRequested not delivered");
    eprintln!("present_demo: OK");
    Ok(())
}
