//! Private `CGVirtualDisplay` virtual display via Objective-C *runtime* class
//! lookup only (SPEC §10). No link-time private symbols: every
//! `CGVirtualDisplay*` class is resolved with `AnyClass::get`, so the binary
//! still launches on macOS versions where Apple removes the API — creation
//! then fails cleanly with an error instead of a missing-symbol abort.
//!
//! API shape (CoreGraphics class-dumped headers; BetterDummy, Chromium's
//! `virtual_display_mac_util.mm`, DeskPad, SideScreen):
//! - `CGVirtualDisplayDescriptor`: `name`, `maxPixelsWide/High` (u32),
//!   `sizeInMillimeters` (CGSize), `productID`/`vendorID`/`serialNum` (u32),
//!   `queue` (dispatch queue), `terminationHandler` (block).
//! - `CGVirtualDisplayMode`: `initWithWidth:height:refreshRate:` (u32,u32,f64).
//! - `CGVirtualDisplaySettings`: `hiDPI` (u32), `modes` (NSArray).
//! - `CGVirtualDisplay`: `initWithDescriptor:` (nil when unavailable),
//!   `applySettings:` -> BOOL, readonly `displayID` (u32).
//!
//! Empirically verified on macOS 26 (Apple Silicon):
//! - With `hiDPI = 1`, mode dimensions are interpreted as *points* and the
//!   backing is 2× — so a Retina display with pixel mode W×H is created with
//!   a (W/2)×(H/2) mode (point size = half the pixels).
//! - Releasing the CGVirtualDisplay object removes the display from the
//!   system ~1 s later (windows migrate back), but only reliably when the
//!   descriptor carried a dispatch `queue` (BetterDummy/Chromium/DeskPad all
//!   set one; without it the window-server teardown handshake can wedge).
//! - All messaging happens inside an autorelease pool: the private classes
//!   autorelease internally, and on std threads (no run loop) a missing pool
//!   leaks those objects and can stall the teardown path.

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQoS, GlobalQueueIdentifier};
use log::{debug, info};
use objc2::msg_send;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};

#[derive(Clone, Debug)]
pub struct VirtualDisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_millihertz: u32,
    /// When true, expose a Retina (2× point) mode at half the pixel size.
    pub hidpi: bool,
    pub name: String,
}

/// Private-API virtual display. All `CGVirtualDisplay*` classes are resolved
/// through the Objective-C runtime (class lookup / dynamic messaging) — NO
/// link-time private symbols, so the binary still launches if Apple removes
/// the API (creation then fails cleanly).
///
/// Threading: `!Send`/`!Sync` (inherited from `Retained<AnyObject>`).
/// `CGVirtualDisplay`'s thread-safety is undocumented by Apple, so the
/// handle conservatively stays on the thread that created it.
pub struct VirtualDisplay {
    display: Option<Retained<AnyObject>>,
    display_id: u32,
}

/// Derive a per-create serial so repeated create/drop cycles never collide
/// with a still-lingering sibling display (duplicate serials make
/// `initWithDescriptor:` return nil).
fn unique_serial(cfg: &VirtualDisplayConfig) -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut s = nanos ^ (cfg.width << 16) ^ cfg.height;
    if s == 0 {
        s = 1;
    }
    s
}

/// Resolve a private CoreGraphics class at runtime; clean error when absent.
fn runtime_class(name: &'static std::ffi::CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| {
        anyhow!(
            "private class {} not found; CGVirtualDisplay unavailable on this macOS",
            name.to_string_lossy()
        )
    })
}

/// `+alloc`/`-init` a runtime-resolved class, nil-checked.
fn alloc_init(cls: &AnyClass) -> Result<Retained<AnyObject>> {
    // SAFETY: `alloc`/`init` are valid messages for every NSObject subclass;
    // the returned pointer is +1 retained and wrapped in `Retained`, or nil
    // (checked via `from_raw` returning None).
    let obj = unsafe {
        let ptr: *mut AnyObject = msg_send![cls, alloc];
        let ptr: *mut AnyObject = msg_send![ptr, init];
        Retained::from_raw(ptr)
    };
    obj.ok_or_else(|| anyhow!("{:?} -init returned nil", cls.name()))
}

fn create_inner(cfg: &VirtualDisplayConfig) -> Result<(Retained<AnyObject>, u32)> {
    if cfg.width == 0 || cfg.height == 0 {
        bail!("virtual display dimensions must be non-zero");
    }
    let refresh_hz = if cfg.refresh_millihertz == 0 {
        60.0
    } else {
        cfg.refresh_millihertz as f64 / 1000.0
    };

    let desc_cls = runtime_class(c"CGVirtualDisplayDescriptor")?;
    let mode_cls = runtime_class(c"CGVirtualDisplayMode")?;
    let settings_cls = runtime_class(c"CGVirtualDisplaySettings")?;
    let display_cls = runtime_class(c"CGVirtualDisplay")?;

    // --- descriptor -----------------------------------------------------
    // A dispatch queue is REQUIRED for reliable teardown: without it the
    // window server's disconnect handshake can wedge and the display leaks
    // (all known consumers — BetterDummy, Chromium, DeskPad — set one).
    let queue =
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInteractive));
    let desc = alloc_init(desc_cls)?;
    let name = NSString::from_str(&cfg.name);
    // Approximate physical size: ~110 DPI at the point size (≈220 DPI Retina
    // at the full pixel size, above Apple's Retina PPI threshold).
    let (point_w, point_h) = if cfg.hidpi {
        (cfg.width as f64 / 2.0, cfg.height as f64 / 2.0)
    } else {
        (cfg.width as f64, cfg.height as f64)
    };
    let mm = CGSize::new(point_w * 25.4 / 110.0, point_h * 25.4 / 110.0);
    // Log-only termination handler: fires when the system tears the display
    // down (e.g. user removes it in System Settings).
    let display_name = cfg.name.clone();
    let termination_handler = RcBlock::new(move || {
        info!("virtual display '{display_name}' terminated by the system");
    });
    // SAFETY: `desc` is a live CGVirtualDisplayDescriptor; all selectors are
    // its declared @property setters with matching C types (NSString*,
    // OS_dispatch_queue*, block, u32, CGSize by value).
    unsafe {
        let _: () = msg_send![&*desc, setName: &*name];
        let _: () = msg_send![&*desc, setQueue: &*queue];
        let _: () = msg_send![&*desc, setTerminationHandler: &*termination_handler];
        let _: () = msg_send![&*desc, setMaxPixelsWide: cfg.width];
        let _: () = msg_send![&*desc, setMaxPixelsHigh: cfg.height];
        let _: () = msg_send![&*desc, setSizeInMillimeters: mm];
        // Arbitrary vendor/product ("TL" = 0x544C); serial must be unique
        // per live display.
        let _: () = msg_send![&*desc, setVendorID: 0x544C_u32];
        let _: () = msg_send![&*desc, setProductID: 0x4C54_u32];
        let _: () = msg_send![&*desc, setSerialNum: unique_serial(cfg)];
    }

    // --- mode -----------------------------------------------------------
    // With hiDPI=1 the mode dimensions are *points* (backing = 2×), so a
    // Retina display at pixel size W×H takes a (W/2)×(H/2) mode. Verified
    // empirically: current mode then reports pixelWidth=W with bounds
    // (points) W/2×H/2. Chromium's virtual_display_mac_util.mm halves the
    // same way.
    let (mode_w, mode_h) = if cfg.hidpi {
        (cfg.width / 2, cfg.height / 2)
    } else {
        (cfg.width, cfg.height)
    };
    // SAFETY: `mode_alloc` comes from +alloc of CGVirtualDisplayMode and
    // initWithWidth:height:refreshRate: is its declared designated
    // initializer (u32, u32, f64). +1 retained; nil checked below.
    let mode = unsafe {
        let mode_alloc: *mut AnyObject = msg_send![mode_cls, alloc];
        let ptr: *mut AnyObject =
            msg_send![mode_alloc, initWithWidth: mode_w, height: mode_h, refreshRate: refresh_hz];
        Retained::from_raw(ptr)
    }
    .ok_or_else(|| anyhow!("CGVirtualDisplayMode init returned nil"))?;

    // --- settings ---------------------------------------------------------
    let settings = alloc_init(settings_cls)?;
    let modes = NSArray::<AnyObject>::from_slice(&[&*mode]);
    // SAFETY: `settings` is a live CGVirtualDisplaySettings; setHiDPI:
    // takes u32 and setModes: takes NSArray* per the class dump.
    unsafe {
        let _: () = msg_send![&*settings, setHiDPI: cfg.hidpi as u32];
        let _: () = msg_send![&*settings, setModes: &*modes];
    }

    // --- display ----------------------------------------------------------
    // SAFETY: `display_alloc` comes from +alloc of CGVirtualDisplay and
    // initWithDescriptor: is its declared initializer. The returned pointer
    // is +1 retained or nil (private API unavailable/failing), which
    // `from_raw` turns into None.
    let display = unsafe {
        let display_alloc: *mut AnyObject = msg_send![display_cls, alloc];
        let ptr: *mut AnyObject = msg_send![display_alloc, initWithDescriptor: &*desc];
        Retained::from_raw(ptr)
    }
    .ok_or_else(|| {
        anyhow!("CGVirtualDisplay initWithDescriptor: returned nil (private API unavailable?)")
    })?;

    // SAFETY: `display` is a live CGVirtualDisplay; applySettings: takes
    // CGVirtualDisplaySettings* and returns BOOL.
    let applied: Bool = unsafe { msg_send![&*display, applySettings: &*settings] };
    if !applied.as_bool() {
        // `display` drops here, tearing down whatever was created.
        bail!("CGVirtualDisplay applySettings: failed");
    }

    // SAFETY: `displayID` is a readonly u32 property of CGVirtualDisplay.
    let display_id: u32 = unsafe { msg_send![&*display, displayID] };
    if display_id == 0 {
        bail!("CGVirtualDisplay reported displayID 0 after applySettings:");
    }

    info!(
        "virtual display '{}' created: id={} {}x{}@{:.2}Hz hidpi={}",
        cfg.name, display_id, cfg.width, cfg.height, refresh_hz, cfg.hidpi
    );
    Ok((display, display_id))
}

impl VirtualDisplay {
    /// Creates and applies the display; the desktop extends onto it.
    /// Fails cleanly (no panic) when the private API is unavailable.
    pub fn create(cfg: VirtualDisplayConfig) -> Result<Self> {
        // The private CGVirtualDisplay classes autorelease internally; a
        // pool keeps std threads (no run loop) from leaking those objects.
        let (display, display_id) = autoreleasepool(|_pool| create_inner(&cfg))?;
        Ok(Self {
            display: Some(display),
            display_id,
        })
    }

    /// CGDirectDisplayID for ScreenCaptureKit / arrangement queries.
    pub fn display_id(&self) -> u32 {
        self.display_id
    }
}

impl Drop for VirtualDisplay {
    /// Releasing the `CGVirtualDisplay` object removes the virtual display
    /// from the system (its `-dealloc` disconnects it; windows migrate back
    /// to real displays). Verified empirically on macOS 26: the display
    /// disappears from `CGGetOnlineDisplayList` ~1 s after release. The
    /// release happens inside an autorelease pool so autoreleased teardown
    /// objects drain promptly even on bare std threads.
    fn drop(&mut self) {
        let id = self.display_id;
        info!("destroying virtual display id={id} (releasing CGVirtualDisplay)");
        if let Some(display) = self.display.take() {
            autoreleasepool(move |_pool| {
                debug!(
                    "releasing CGVirtualDisplay object {:?}",
                    Retained::as_ptr(&display)
                );
                drop(display);
            });
        }
    }
}
