//! ScreenCaptureKit capture (SPEC §10): SCShareableContent → SCContentFilter
//! → SCStream, delivering zero-copy `CapturedFrame`s on a dispatch queue.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::define_class;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMTime, CMSampleBuffer};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamErrorDomain,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};
use parking_lot::{Condvar, Mutex};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// True when the process already holds the Screen Recording TCC grant.
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
}

/// CGDirectDisplayID of the primary display.
pub fn primary_display_id() -> Result<u32> {
    // SAFETY: CGMainDisplayID is a pure query with no preconditions.
    Ok(unsafe { CGMainDisplayID() })
}

fn screen_recording_permitted() -> bool {
    // SAFETY: CGPreflightScreenCaptureAccess is a pure query.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Convert a CMTime to microseconds (i128 intermediate: value can be
/// nanoseconds since boot, which overflows i64 microseconds math).
pub(crate) fn cmtime_to_us(t: CMTime) -> i64 {
    if t.timescale <= 0 {
        return 0;
    }
    ((t.value as i128 * 1_000_000) / t.timescale as i128) as i64
}

/// Zero-copy captured frame (retained CVPixelBuffer + timestamp).
/// Must be `Send` (move across the pipeline thread boundary).
pub struct CapturedFrame {
    buf: CFRetained<CVPixelBuffer>,
    pts_us: i64,
}

// SAFETY: CVPixelBuffer is a reference-counted, thread-safe CoreFoundation
// object; `CFRetained` holds a strong reference. The frame's pixel contents
// are treated as immutable after capture/drawing completes, so moving the
// owning handle across threads cannot race.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    pub fn pts_us(&self) -> i64 {
        self.pts_us
    }
    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.buf) as u32
    }
    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.buf) as u32
    }

    pub(crate) fn from_parts(buf: CFRetained<CVPixelBuffer>, pts_us: i64) -> Self {
        Self { buf, pts_us }
    }

    /// Borrow the underlying pixel buffer (for VideoToolbox encode).
    pub(crate) fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buf
    }
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub display_id: u32,
    pub fps: u32,
    /// SCStream queue depth; 2 recommended for latency.
    pub queue_depth: u32,
    pub show_cursor: bool,
}

/// Map an SCK/CoreMedia error to an anyhow error, ensuring TCC denial is
/// reported with the word "permission" (SPEC §9).
fn map_capture_error(ctx: &str, e: &NSError) -> anyhow::Error {
    let domain = e.domain();
    let code = e.code();
    let desc = e.localizedDescription();
    // SAFETY: SCStreamErrorDomain is an immutable framework constant.
    let sc_domain = unsafe { SCStreamErrorDomain };
    let tcc_denied = !screen_recording_permitted()
        || (domain.to_string() == sc_domain.to_string() && matches!(code, -3801 | -3803)); // UserDeclined | MissingEntitlements
    if tcc_denied {
        anyhow!(
            "{ctx}: Screen Recording permission denied — grant permission in \
             System Settings > Privacy & Security > Screen Recording ({desc})"
        )
    } else {
        anyhow!("{ctx}: {desc} ({domain}:{code})")
    }
}

/// Result slot used to bridge SCK completion handlers (async) to the
/// synchronous public API.
struct Completion<T> {
    state: Mutex<Option<Result<T, String>>>,
    cond: Condvar,
}

impl<T> Completion<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            cond: Condvar::new(),
        }
    }

    fn fulfill(&self, res: Result<T, String>) {
        *self.state.lock() = Some(res);
        self.cond.notify_one();
    }

    fn wait(&self, timeout: Duration, what: &str) -> Result<T> {
        let mut guard = self.state.lock();
        if guard.is_none() && self.cond.wait_for(&mut guard, timeout).timed_out() {
            return Err(anyhow!("{what}: timed out after {timeout:?}"));
        }
        guard
            .take()
            .expect("completion slot filled after wait")
            .map_err(|e| anyhow!("{what}: {e}"))
    }
}

fn get_shareable_content() -> Result<Retained<SCShareableContent>> {
    // The completion fires on a system dispatch queue while we block
    // here; one-way handoff of an Apple snapshot object (thread-safe per
    // Apple's general rule), so the non-Send payload crossing is sound.
    #[allow(clippy::arc_with_non_send_sync)]
    let slot = Arc::new(Completion::<Retained<SCShareableContent>>::new());
    let slot2 = Arc::clone(&slot);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let res = if !error.is_null() {
                // SAFETY: `error` is a valid NSError pointer supplied by SCK.
                let e = unsafe { &*error };
                Err(format!("{} (code {})", e.localizedDescription(), e.code()))
            } else if !content.is_null() {
                // SAFETY: `content` is a valid SCShareableContent supplied by
                // SCK; `retain` takes our own strong reference to it.
                Ok(unsafe { Retained::retain(content) }.expect("non-null content"))
            } else {
                Err("ScreenCaptureKit returned neither content nor error".to_string())
            };
            slot2.fulfill(res);
        },
    );
    // SAFETY: block outlives the async call (we wait for completion below);
    // the block matches the documented handler signature.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    slot.wait(Duration::from_secs(15), "SCShareableContent query")
}

type FrameCallback = Box<dyn FnMut(CapturedFrame) + Send>;

pub struct Ivars {
    on_frame: Mutex<Option<FrameCallback>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TLCaptureStreamOutput"]
    #[ivars = Ivars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)] // name pinned by the SCStreamOutput protocol trait
        fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            // Never let a panic cross the FFI/ObjC boundary.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: sample_buffer is a valid, ready video sample buffer
                // delivered by SCStream on our handler queue.
                let buf = unsafe { sample_buffer.image_buffer() };
                let Some(buf) = buf else {
                    log::warn!("SCK sample without image buffer; dropped");
                    return;
                };
                // SAFETY: valid sample buffer; pts query is read-only.
                let pts_us = cmtime_to_us(unsafe { sample_buffer.presentation_time_stamp() });
                let frame = CapturedFrame::from_parts(buf, pts_us);
                let mut guard = self.ivars().on_frame.lock();
                if let Some(cb) = guard.as_mut() {
                    cb(frame);
                }
            }));
        }
    }
);

impl StreamOutput {
    fn new(on_frame: Box<dyn FnMut(CapturedFrame) + Send>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            on_frame: Mutex::new(Some(on_frame)),
        });
        // SAFETY: standard NSObject init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Capturer {
    stream: Retained<SCStream>,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    output: Option<Retained<ProtocolObject<dyn SCStreamOutput>>>,
    running: bool,
}

impl Capturer {
    /// Error message MUST contain "permission" when Screen Recording
    /// TCC is denied (SPEC §9).
    pub fn new(cfg: CaptureConfig) -> Result<Self> {
        if !screen_recording_permitted() {
            return Err(anyhow!(
                "Screen Recording permission denied — grant permission in \
                 System Settings > Privacy & Security > Screen Recording, then restart"
            ));
        }
        if cfg.fps == 0 {
            return Err(anyhow!("capture fps must be > 0"));
        }

        let content = get_shareable_content().context("failed to query SCShareableContent")?;

        // SAFETY: `content` is valid; displays() is a read-only query.
        let displays = unsafe { content.displays() };
        let display = displays
            .iter()
            // SAFETY: SCDisplay objects from the array are valid.
            .find(|d| unsafe { d.displayID() } == cfg.display_id)
            .ok_or_else(|| {
                anyhow!("display id {} not found in shareable content", cfg.display_id)
            })?;

        // SAFETY: valid objects; empty exclusion list = capture whole display.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::<SCWindow>::from_slice(&[]),
            )
        };

        let scfg = unsafe { SCStreamConfiguration::new() };
        // SAFETY: all setters are plain property writes on a valid object.
        unsafe {
            scfg.setWidth(display.width() as usize);
            scfg.setHeight(display.height() as usize);
            scfg.setMinimumFrameInterval(CMTime::new(1, cfg.fps as i32));
            scfg.setQueueDepth(cfg.queue_depth as isize);
            scfg.setShowsCursor(cfg.show_cursor);
            scfg.setPixelFormat(objc2_core_video::kCVPixelFormatType_32BGRA);
            scfg.setCapturesAudio(false);
            scfg.setScalesToFit(false);
        }

        let queue = DispatchQueue::new("dev.thunderlink.capture", None);
        // SAFETY: filter/config valid; delegate is optional (None).
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &scfg, None)
        };

        Ok(Self {
            stream,
            queue,
            output: None,
            running: false,
        })
    }

    /// `on_frame` runs on a dedicated capture thread; must not block.
    pub fn start(&mut self, on_frame: Box<dyn FnMut(CapturedFrame) + Send>) -> Result<()> {
        if self.running {
            return Err(anyhow!("capturer already running"));
        }
        let handler = StreamOutput::new(on_frame);
        let output = ProtocolObject::from_retained(handler);
        // SAFETY: stream/output valid; queue is our own serial queue.
        unsafe {
            self.stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    &output,
                    SCStreamOutputType::Screen,
                    Some(&self.queue),
                )
        }
        .map_err(|e| map_capture_error("add stream output", &e))?;

        let slot = Arc::new(Completion::<()>::new());
        let slot2 = Arc::clone(&slot);
        let block = RcBlock::new(move |error: *mut NSError| {
            let res = if error.is_null() {
                Ok(())
            } else {
                // SAFETY: `error` is a valid NSError pointer supplied by SCK.
                let e = unsafe { &*error };
                Err(map_capture_error("start capture", e).to_string())
            };
            slot2.fulfill(res);
        });
        // SAFETY: block matches the documented signature and outlives the wait.
        unsafe { self.stream.startCaptureWithCompletionHandler(Some(&block)) };
        slot.wait(Duration::from_secs(15), "start capture")?;

        self.output = Some(output);
        self.running = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        let slot = Arc::new(Completion::<()>::new());
        let slot2 = Arc::clone(&slot);
        let block = RcBlock::new(move |error: *mut NSError| {
            let res = if error.is_null() {
                Ok(())
            } else {
                // SAFETY: `error` is a valid NSError pointer supplied by SCK.
                let e = unsafe { &*error };
                Err(e.localizedDescription().to_string())
            };
            slot2.fulfill(res);
        });
        // SAFETY: block matches the documented signature and outlives the wait.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        if let Err(e) = slot.wait(Duration::from_secs(5), "stop capture") {
            log::warn!("stop capture: {e:#}");
        }
        self.output = None;
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time proof that CapturedFrame can cross the pipeline boundary.
    #[test]
    fn captured_frame_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CapturedFrame>();
    }

    #[test]
    fn primary_display_id_nonzero() {
        // Headless CI still exposes the main display object.
        let id = primary_display_id().expect("main display id");
        assert_ne!(id, 0);
    }

    /// Real screen capture needs the Screen Recording TCC grant (and a
    /// window server session): only run in the manual e2e harness.
    #[test]
    fn capture_frames_e2e() {
        if std::env::var("TL_E2E").ok().as_deref() != Some("1") {
            return;
        }
        let display_id = primary_display_id().unwrap();
        let cfg = CaptureConfig {
            display_id,
            fps: 60,
            queue_depth: 2,
            show_cursor: true,
        };
        let mut capturer = match Capturer::new(cfg) {
            Ok(c) => c,
            Err(e) => {
                assert!(
                    format!("{e:#}").contains("permission"),
                    "unexpected error (must mention permission on TCC denial): {e:#}"
                );
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<CapturedFrame>();
        capturer
            .start(Box::new(move |f| {
                let _ = tx.send(f);
            }))
            .unwrap();
        let f1 = rx.recv_timeout(Duration::from_secs(5)).expect("first frame");
        let f2 = rx.recv_timeout(Duration::from_secs(5)).expect("second frame");
        assert!(f1.width() > 0 && f1.height() > 0);
        assert!(f2.pts_us() > f1.pts_us(), "pts must advance");
        capturer.stop();
    }
}
