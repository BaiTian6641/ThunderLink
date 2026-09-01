//! AppKit + Metal presenter (SPEC §10).
//!
//! Window on the main thread, rendering on a CVDisplayLink thread with a
//! CVMetalTextureCache zero-copy path (BGRA direct; NV12/P010 two-plane
//! YUV→RGB in the fragment shader). Latest-wins submit, one frame per vsync.
#![allow(non_upper_case_globals)] // Apple-style k* constant patterns

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};

use anyhow::{anyhow, bail, Context, Result};
use core_foundation::base::CFRelease;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSScreen,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSNotification, NSPoint, NSRect, NSSize, NSString};
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLStoreAction,
    MTLTexture,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use parking_lot::Mutex;

use super::decode::DecodedFrame;
use crate::vt::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Windowed,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentEvent {
    CloseRequested,
    Resized { width: u32, height: u32 },
}

// ---------------------------------------------------------------------------
// Window delegate (main thread)
// ---------------------------------------------------------------------------

struct DelegateIvars {
    events: Arc<Mutex<Vec<PresentEvent>>>,
    size: Arc<AtomicU64>,
    window: Retained<NSWindow>,
    layer: Retained<CAMetalLayer>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ThunderLinkWindowDelegate"]
    #[ivars = DelegateIvars]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.ivars().events.lock().push(PresentEvent::CloseRequested);
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            let ivars = self.ivars();
            let (w, h) = drawable_size(&ivars.window);
            ivars.layer.setContentsScale(ivars.window.backingScaleFactor());
            ivars.layer.setDrawableSize(NSSize { width: w as f64, height: h as f64 });
            ivars.size.store(pack_size(w, h), Ordering::Release);
            ivars.events.lock().push(PresentEvent::Resized { width: w, height: h });
        }
    }
);

impl WindowDelegate {
    fn new(mtm: MainThreadMarker, ivars: DelegateIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        // SAFETY: `this` is freshly allocated; plain NSObject init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Drawable size in physical pixels (bounds × backing scale factor).
fn drawable_size(window: &NSWindow) -> (u32, u32) {
    let scale = window.backingScaleFactor();
    match window.contentView() {
        Some(view) => {
            let b = view.bounds();
            (
                (b.size.width * scale).max(1.0).round() as u32,
                (b.size.height * scale).max(1.0).round() as u32,
            )
        }
        None => (1, 1),
    }
}

fn pack_size(w: u32, h: u32) -> u64 {
    (w as u64) << 32 | h as u64
}

// ---------------------------------------------------------------------------
// Metal shaders
// ---------------------------------------------------------------------------

const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut {
    float4 pos [[position]];
    float2 uv;
};

// Fullscreen triangle; uv (0,0) = top-left, matching CVPixelBuffer row order.
vertex VOut tl_vs(uint vid [[vertex_id]]) {
    float2 p = float2((float)(vid & 1u) * 2.0, (float)(vid >> 1u) * 2.0);
    VOut o;
    o.pos = float4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    o.uv = p;
    return o;
}

fragment float4 tl_fs_bgra(VOut in [[stage_in]], texture2d<float> tex [[texture(0)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    return tex.sample(s, in.uv);
}

// BT.709, video (limited) range — what VT emits for SDR ('420v').
fragment float4 tl_fs_nv12_video(VOut in [[stage_in]],
                                 texture2d<float> yTex [[texture(0)]],
                                 texture2d<float> uvTex [[texture(1)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float y = yTex.sample(s, in.uv).r;
    float2 c = uvTex.sample(s, in.uv).rg;
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float u = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    float v = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    float r = y + 1.5748 * v;
    float g = y - 0.1873 * u - 0.4681 * v;
    float b = y + 1.8556 * u;
    return float4(r, g, b, 1.0);
}

// BT.709, full range ('420f').
fragment float4 tl_fs_nv12_full(VOut in [[stage_in]],
                                texture2d<float> yTex [[texture(0)]],
                                texture2d<float> uvTex [[texture(1)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float y = yTex.sample(s, in.uv).r;
    float2 c = uvTex.sample(s, in.uv).rg - 0.5;
    float r = y + 1.5748 * c.y;
    float g = y - 0.1873 * c.x - 0.4681 * c.y;
    float b = y + 1.8556 * c.x;
    return float4(r, g, b, 1.0);
}
"#;

struct Pipelines {
    bgra: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    nv12_video: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    nv12_full: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

fn build_pipelines(device: &ProtocolObject<dyn MTLDevice>) -> Result<Pipelines> {
    let src = NSString::from_str(SHADERS);
    let lib = device
        .newLibraryWithSource_options_error(&src, None)
        .map_err(|e| anyhow!("Metal shader library compile failed: {e:?}"))?;
    let vs = lib
        .newFunctionWithName(&NSString::from_str("tl_vs"))
        .context("shader missing tl_vs")?;
    let mk = |frag: &str| -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
        let fs = lib
            .newFunctionWithName(&NSString::from_str(frag))
            .with_context(|| format!("shader missing {frag}"))?;
        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(&vs));
        desc.setFragmentFunction(Some(&fs));
        // SAFETY: color attachment arrays always have ≥1 slot (index 0 valid).
        unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) }
            .setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        device
            .newRenderPipelineStateWithDescriptor_error(&desc)
            .map_err(|e| anyhow!("render pipeline {frag}: {e:?}"))
    };
    Ok(Pipelines {
        bgra: mk("tl_fs_bgra")?,
        nv12_video: mk("tl_fs_nv12_video")?,
        nv12_full: mk("tl_fs_nv12_full")?,
    })
}

// ---------------------------------------------------------------------------
// Render context (display-link thread)
// ---------------------------------------------------------------------------

struct RenderCtx {
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    tex_cache: CVMetalTextureCacheRef,
    pending: Arc<Mutex<Option<DecodedFrame>>>,
    pipelines: Pipelines,
    warned_format: bool,
}

/// Zero-copy texture view of one CVPixelBuffer plane via CVMetalTextureCache.
fn cv_metal_texture(
    cache: CVMetalTextureCacheRef,
    image: CVPixelBufferRef,
    format: MTLPixelFormat,
    width: usize,
    height: usize,
    plane: usize,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    let mut tex: CVMetalTextureRef = ptr::null_mut();
    // SAFETY: image is retained by the DecodedFrame for the call's duration;
    // out param checked for status + null.
    let status = unsafe {
        CVMetalTextureCacheCreateTextureFromImage(
            ptr::null(),
            cache,
            image,
            ptr::null(),
            format.0 as u64,
            width,
            height,
            plane,
            &mut tex,
        )
    };
    if status != kCVReturnSuccess || tex.is_null() {
        bail!("CVMetalTextureCacheCreateTextureFromImage failed: {status}");
    }
    // SAFETY: tex is a live CVMetalTexture; GetTexture returns its (get-rule)
    // MTLTexture which we retain, then release the CVMetalTexture wrapper.
    let raw = unsafe { CVMetalTextureGetTexture(tex) };
    let mtl = unsafe { Retained::retain(raw.cast::<ProtocolObject<dyn MTLTexture>>()) };
    // SAFETY: tex is a live owned object.
    unsafe { CFRelease(tex) };
    mtl.ok_or_else(|| anyhow!("CVMetalTextureGetTexture returned null"))
}

fn render_latest(ctx: &mut RenderCtx) -> Result<()> {
    let frame = ctx.pending.lock().take();
    let Some(frame) = frame else { return Ok(()) }; // no new frame this vsync
    let pb = frame.cv_pixel_buffer();
    let (w, h) = (frame.width() as usize, frame.height() as usize);

    let (tex0, tex1, pipeline) = match frame.pixel_format() {
        kCVPixelFormatType_32BGRA => (
            cv_metal_texture(ctx.tex_cache, pb, MTLPixelFormat::BGRA8Unorm, w, h, 0)?,
            None,
            &ctx.pipelines.bgra,
        ),
        fmt @ (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        | kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) => {
            let y = cv_metal_texture(ctx.tex_cache, pb, MTLPixelFormat::R8Unorm, w, h, 0)?;
            let uv = cv_metal_texture(ctx.tex_cache, pb, MTLPixelFormat::RG8Unorm, w / 2, h / 2, 1)?;
            let pipe = if fmt == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
                &ctx.pipelines.nv12_full
            } else {
                &ctx.pipelines.nv12_video
            };
            (y, Some(uv), pipe)
        }
        // P010: 10-bit in the high bits of 16-bit; *16Unorm sampling
        // normalizes it exactly like the 8-bit NV12 path (SDR downconvert).
        kCVPixelFormatType_OneComponent10 => {
            let y = cv_metal_texture(ctx.tex_cache, pb, MTLPixelFormat::R16Unorm, w, h, 0)?;
            let uv =
                cv_metal_texture(ctx.tex_cache, pb, MTLPixelFormat::RG16Unorm, w / 2, h / 2, 1)?;
            (y, Some(uv), &ctx.pipelines.nv12_video)
        }
        other => {
            if !ctx.warned_format {
                ctx.warned_format = true;
                log::error!("present: unsupported pixel buffer format {other:#010x}; dropping frames");
            }
            return Ok(());
        }
    };

    let Some(drawable) = ctx.layer.nextDrawable() else {
        return Ok(()); // minimized / zero-size drawable
    };
    let drawable_tex = drawable.texture();
    let pass = MTLRenderPassDescriptor::renderPassDescriptor();
    // SAFETY: color attachment arrays always have ≥1 slot (index 0 valid).
    let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
    color.setTexture(Some(&drawable_tex));
    color.setLoadAction(MTLLoadAction::Clear);
    color.setStoreAction(MTLStoreAction::Store);
    color.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 });

    let cmd = ctx.queue.commandBuffer().context("commandBuffer failed")?;
    let enc = cmd
        .renderCommandEncoderWithDescriptor(&pass)
        .context("renderCommandEncoder failed")?;
    enc.setRenderPipelineState(pipeline);
    // SAFETY: indices match the shader's [[texture(n)]] bindings; textures are
    // live Retained objects outliving the encoder.
    unsafe {
        enc.setFragmentTexture_atIndex(Some(&tex0), 0);
        if let Some(t1) = &tex1 {
            enc.setFragmentTexture_atIndex(Some(t1), 1);
        }
    }
    // SAFETY: fullscreen triangle — 3 vertices, no buffers bound.
    unsafe { enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3) };
    enc.endEncoding();
    cmd.presentDrawable(ProtocolObject::from_ref(&*drawable));
    cmd.commit();
    // SAFETY: cache is live; flush only reaps textures not retained by
    // in-flight command buffers, keeping IOSurface memory bounded.
    unsafe { CVMetalTextureCacheFlush(ctx.tex_cache, 0) };
    Ok(())
}

extern "C" fn display_link_callback(
    _link: CVDisplayLinkRef,
    _now: *const c_void,
    _output_time: *const c_void,
    _flags_in: u64,
    _flags_out: *mut u64,
    ctx: *mut c_void,
) -> i32 {
    // SAFETY: ctx points at a Box<RenderCtx> owned by run(); CVDisplayLinkStop
    // blocks until any in-flight callback returns, and the box is reclaimed
    // only after the link is stopped, so the pointer is always valid here.
    let ctx = unsafe { &mut *ctx.cast::<RenderCtx>() };
    autoreleasepool(|_| {
        if let Err(e) = render_latest(ctx) {
            log::error!("present: render failed: {e:#}");
        }
    });
    0
}

// ---------------------------------------------------------------------------
// SubmitHandle
// ---------------------------------------------------------------------------

/// Thread-safe handle for the decode worker (integration contract):
/// shares the Presenter's latest-wins pending slot and close flag.
/// `Clone + Send + Sync`.
#[derive(Clone)]
pub struct SubmitHandle {
    pending: Arc<Mutex<Option<DecodedFrame>>>,
    close_requested: Arc<AtomicBool>,
}

impl SubmitHandle {
    /// Any thread. Non-blocking; overwrites the pending frame (latest-wins).
    pub fn submit(&self, frame: DecodedFrame) {
        *self.pending.lock() = Some(frame);
    }

    /// Any thread. Ask the presenter to close the window and end `run()`
    /// (the close still flows through `windowWillClose` on the main thread,
    /// so `on_event(CloseRequested)` is delivered exactly once).
    pub fn request_close(&self) {
        self.close_requested.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Presenter
// ---------------------------------------------------------------------------

/// AppKit + Metal presenter.
///
/// THREAD CONTRACT (SPEC §9): `new` and `run` MUST be called on the
/// process main thread; `submit`/`submit_handle`/`request_close` may be
/// called from any thread. Presentation is latest-wins: a new `submit`
/// replaces any not-yet-presented frame (never queues).
pub struct Presenter {
    window: Retained<NSWindow>,
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    tex_cache: CVMetalTextureCacheRef,
    pipelines: Pipelines,
    pending: Arc<Mutex<Option<DecodedFrame>>>,
    events: Arc<Mutex<Vec<PresentEvent>>>,
    close_requested: Arc<AtomicBool>,
    size: Arc<AtomicU64>,
    mode: Mode,
}

// SAFETY: the only methods callable through a shared reference (`submit`,
// `submit_handle`, `request_close`, `content_size`) touch exclusively the
// Mutex/atomic fields. All AppKit/QuartzCore objects are only used from
// `new`/`run`, which are main-thread-guarded via MainThreadMarker.
unsafe impl Send for Presenter {}
// SAFETY: see Send; shared references never reach AppKit state.
unsafe impl Sync for Presenter {}

impl Presenter {
    /// MAIN THREAD ONLY (AppKit).
    pub fn new(mode: Mode) -> Result<Self> {
        let mtm = MainThreadMarker::new().context("Presenter::new must run on the main thread")?;
        ensure_app(mtm);

        let device = MTLCreateSystemDefaultDevice().context("no Metal device available")?;
        let queue = device.newCommandQueue().context("newCommandQueue failed")?;
        let pipelines = build_pipelines(&device)?;

        let mut tex_cache: CVMetalTextureCacheRef = ptr::null_mut();
        // SAFETY: device is a live id<MTLDevice>; out param checked.
        let status = unsafe {
            CVMetalTextureCacheCreate(
                ptr::null(),
                ptr::null(),
                // Fat *const dyn → thin *mut: `as *const c_void` drops the
                // (ObjC-less) metadata; the C API only wants the object ptr.
                (Retained::as_ptr(&device) as *const c_void).cast_mut(),
                ptr::null(),
                &mut tex_cache,
            )
        };
        if status != kCVReturnSuccess || tex_cache.is_null() {
            bail!("CVMetalTextureCacheCreate failed: {status}");
        }

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);

        let (frame_rect, style) = match mode {
            Mode::Fullscreen => {
                let screen = NSScreen::mainScreen(mtm).context("no main screen")?;
                (screen.frame(), NSWindowStyleMask::Borderless)
            }
            Mode::Windowed => (
                NSRect {
                    origin: NSPoint { x: 100.0, y: 100.0 },
                    size: NSSize { width: 1280.0, height: 720.0 },
                },
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
            ),
        };

        // SAFETY: main thread (mtm proof); all arguments are valid values.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame_rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("ThunderLink"));
        if mode == Mode::Fullscreen {
            // Cover the menu bar: just above NSMainMenuWindowLevel (24).
            window.setLevel(25);
        } else {
            window.center();
        }

        let view = NSView::initWithFrame(mtm.alloc(), frame_rect);
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));
        window.setContentView(Some(&view));

        let (w, h) = drawable_size(&window);
        layer.setContentsScale(window.backingScaleFactor());
        layer.setDrawableSize(NSSize { width: w as f64, height: h as f64 });

        Ok(Self {
            window,
            layer,
            queue,
            tex_cache,
            pipelines,
            pending: Arc::new(Mutex::new(None)),
            events: Arc::new(Mutex::new(Vec::new())),
            close_requested: Arc::new(AtomicBool::new(false)),
            size: Arc::new(AtomicU64::new(pack_size(w, h))),
            mode,
        })
    }

    /// Any thread. Non-blocking; overwrites the pending frame.
    pub fn submit(&self, frame: DecodedFrame) {
        *self.pending.lock() = Some(frame);
    }

    /// Any thread. Returns a cloneable handle sharing this presenter's
    /// pending slot and close flag — the way a decode worker submits while
    /// `run()` occupies the main thread.
    pub fn submit_handle(&self) -> SubmitHandle {
        SubmitHandle {
            pending: self.pending.clone(),
            close_requested: self.close_requested.clone(),
        }
    }

    /// Any thread. Ask `run()` to close the window and return (session Stop).
    /// Same semantics as `SubmitHandle::request_close`.
    pub fn request_close(&self) {
        self.close_requested.store(true, Ordering::Release);
    }

    /// Drawable size in pixels.
    pub fn content_size(&self) -> (u32, u32) {
        let packed = self.size.load(Ordering::Acquire);
        ((packed >> 32) as u32, packed as u32)
    }

    /// MAIN THREAD ONLY. Presents the newest submitted frame per vsync
    /// (CVDisplayLink); returns when the window closes after delivering
    /// `CloseRequested`.
    pub fn run(self, mut on_event: impl FnMut(PresentEvent) + 'static) -> Result<()> {
        let mtm = MainThreadMarker::new().context("Presenter::run must run on the main thread")?;
        let _ = self.mode;

        let delegate = WindowDelegate::new(
            mtm,
            DelegateIvars {
                events: self.events.clone(),
                size: self.size.clone(),
                window: self.window.clone(),
                layer: self.layer.clone(),
            },
        );
        self.window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        self.window.makeKeyAndOrderFront(None);

        let app = NSApplication::sharedApplication(mtm);
        app.activate();

        let mut ctx = Box::new(RenderCtx {
            layer: self.layer.clone(),
            queue: self.queue.clone(),
            tex_cache: self.tex_cache,
            pending: self.pending.clone(),
            pipelines: Pipelines {
                bgra: self.pipelines.bgra.clone(),
                nv12_video: self.pipelines.nv12_video.clone(),
                nv12_full: self.pipelines.nv12_full.clone(),
            },
            warned_format: false,
        });
        let ctx_ptr: *mut RenderCtx = &mut *ctx;

        let mut link: CVDisplayLinkRef = ptr::null_mut();
        // SAFETY: out param checked for status + null.
        let status = unsafe { CVDisplayLinkCreateWithCGDisplay(CGMainDisplayID(), &mut link) };
        if status != kCVReturnSuccess || link.is_null() {
            bail!("CVDisplayLinkCreateWithCGDisplay failed: {status}");
        }
        // SAFETY: link is live; ctx_ptr stays valid until after
        // CVDisplayLinkStop below.
        unsafe { CVDisplayLinkSetOutputCallback(link, display_link_callback, ctx_ptr.cast()) };
        // SAFETY: link is live and has a callback.
        unsafe { CVDisplayLinkStart(link) };
        log::info!("present: display link started ({:?})", self.mode);

        // Event loop on the main thread until the window closes.
        let mut closed = false;
        while !closed {
            autoreleasepool(|_| {
                // Blocks up to ~1/60 s for an event (this also runs the main
                // run loop mode, draining GCD main-queue blocks), then drains
                // every queued event.
                let deadline = NSDate::dateWithTimeIntervalSinceNow(1.0 / 60.0);
                while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&deadline),
                    // SAFETY: immutable framework global string constant.
                    unsafe { NSDefaultRunLoopMode },
                    true,
                ) {
                    app.sendEvent(&event);
                }
                app.updateWindows();
            });

            // Cross-thread close request (control thread on session Stop):
            // perform the actual close here, on the main thread, so the
            // delegate path (and CloseRequested delivery) is identical to a
            // user close.
            if self.close_requested.swap(false, Ordering::AcqRel) {
                log::info!("present: close requested programmatically");
                self.window.close();
            }

            let drained: Vec<PresentEvent> = self.events.lock().drain(..).collect();
            for event in drained {
                if matches!(event, PresentEvent::CloseRequested) {
                    closed = true;
                }
                on_event(event);
            }
        }

        // SAFETY: stop blocks until any in-flight callback returns; only then
        // release the link and reclaim the render context.
        unsafe {
            CVDisplayLinkStop(link);
            CFRelease(link.cast());
        }
        drop(ctx);
        self.window.setDelegate(None);
        self.window.orderOut(None);
        Ok(())
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // SAFETY: tex_cache was created in new() and is released exactly once
        // here; the display link (which borrowed it) is stopped before run()
        // returns, so no user remains.
        unsafe { CFRelease(self.tex_cache) };
    }
}

/// One-time NSApplication setup for a non-bundled binary (tests/examples).
fn ensure_app(mtm: MainThreadMarker) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let app = NSApplication::sharedApplication(mtm);
        if !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
            log::warn!("present: setActivationPolicy(Regular) failed (headless session?)");
        }
        app.finishLaunching();
    });
}
