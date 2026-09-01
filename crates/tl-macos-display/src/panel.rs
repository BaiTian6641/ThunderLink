//! Main-panel info: native pixel resolution, backing scale, refresh rate,
//! and EDID via IOKit when readable (SPEC §10).

use anyhow::{anyhow, bail, Result};
use core_foundation::base::{CFAllocatorRef, CFType, CFTypeRef, TCFType};
use core_foundation::data::{CFData, CFDataRef};
use core_foundation::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use log::{debug, warn};
use std::ffi::c_char;
use std::ptr;
use tl_proto::PanelInfo;

// --- IOKit FFI (EDID lookup) ----------------------------------------------
// io_object_t / io_iterator_t are u32; kern_return_t is i32.
type IoIterator = u32;
const KERN_SUCCESS: i32 = 0;
/// kIOMainPortDefault (NULL mach port).
const IO_MAIN_PORT_DEFAULT: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: CFDictionaryRef,
        existing: *mut IoIterator,
    ) -> i32;
    fn IOIteratorNext(iterator: IoIterator) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
}

/// Native pixel resolution, backing scale (×100), refresh, and EDID
/// (via IOKit `IODisplayConnect` entries when available) of the
/// main panel. `edid: None` when unreadable (SPEC §10).
pub fn main_panel() -> Result<PanelInfo> {
    let display = CGDisplay::main();
    let mode = display
        .display_mode()
        .ok_or_else(|| anyhow!("CGDisplayCopyDisplayMode returned no mode for main display"))?;
    let width = mode.pixel_width() as u32;
    let height = mode.pixel_height() as u32;
    if width == 0 || height == 0 {
        bail!("main display mode reports zero pixels");
    }

    // CGDisplayModeGetRefreshRate; 0.0 means "unknown" → fall back to 60 Hz.
    let hz = mode.refresh_rate();
    let hz = if hz > 0.0 { hz } else { 60.0 };

    let points_w = display.bounds().size.width;
    let scale_x100 = if points_w > 0.0 {
        (width as f64 / points_w * 100.0).round() as u32
    } else {
        warn!("CGDisplayBounds width non-positive; assuming scale 1x");
        100
    };

    let edid = main_display_edid(&display);
    if edid.is_none() {
        debug!("main display EDID unreadable via IOKit; reporting None");
    }

    Ok(PanelInfo {
        width,
        height,
        refresh_millihertz: (hz * 1000.0).round() as u32,
        scale_x100,
        edid,
    })
}

/// Size in points of the main display's frame in global coordinates;
/// used as the input-capture rect for mirror sessions.
pub fn main_display_points() -> Result<(f64, f64)> {
    let size = CGDisplay::main().bounds().size;
    if size.width <= 0.0 || size.height <= 0.0 {
        bail!("CGDisplayBounds of main display has non-positive size");
    }
    Ok((size.width, size.height))
}

/// Frame (origin x/y + size, points, global display coordinates) of any
/// display. Used to aim input injection at a virtual display, which
/// WindowServer places at an offset in the desktop layout.
pub fn display_frame(display_id: u32) -> Result<(f64, f64, f64, f64)> {
    let bounds = CGDisplay::new(display_id).bounds();
    if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
        bail!("CGDisplayBounds of display {display_id} has non-positive size");
    }
    Ok((bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height))
}

/// EDID of the main display: iterate `IODisplayConnect` registry services
/// and match on DisplayVendorID/DisplayProductID (plus DisplaySerialNumber
/// when both sides report a non-zero serial) against the main display.
fn main_display_edid(display: &CGDisplay) -> Option<Vec<u8>> {
    let vendor = display.vendor_number();
    let product = display.model_number();
    let serial = display.serial_number();

    let mut iter: IoIterator = 0;
    // SAFETY: IOServiceMatching returns a +1 matching dictionary that
    // IOServiceGetMatchingServices always consumes. `iter` is written on
    // success.
    let kr = unsafe {
        let matching = IOServiceMatching(c"IODisplayConnect".as_ptr());
        if matching.is_null() {
            return None;
        }
        IOServiceGetMatchingServices(IO_MAIN_PORT_DEFAULT, matching, &mut iter)
    };
    if kr != KERN_SUCCESS {
        warn!("IOServiceGetMatchingServices failed: kern_return_t {kr}");
        return None;
    }
    if iter == 0 {
        return None;
    }

    let mut found: Option<Vec<u8>> = None;
    loop {
        // SAFETY: `iter` is a live iterator owned by this function.
        let entry = unsafe { IOIteratorNext(iter) };
        if entry == 0 {
            break;
        }
        if found.is_none() {
            found = edid_from_entry(entry, vendor, product, serial);
        }
        // SAFETY: `entry` is a live io_object_t reference we own.
        let kr = unsafe { IOObjectRelease(entry) };
        if kr != KERN_SUCCESS {
            warn!("IOObjectRelease(entry) failed: kern_return_t {kr}");
        }
    }
    // SAFETY: `iter` is a live io_object_t reference we own.
    let kr = unsafe { IOObjectRelease(iter) };
    if kr != KERN_SUCCESS {
        warn!("IOObjectRelease(iterator) failed: kern_return_t {kr}");
    }
    found
}

/// Read one +1-retained CF property of a registry entry.
fn entry_prop(entry: u32, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    // SAFETY: `entry` is a live registry entry, `key` a valid CFString,
    // NULL allocator selects the default. Returns +1 retained or NULL.
    let raw = unsafe { IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), ptr::null(), 0) };
    if raw.is_null() {
        None
    } else {
        // SAFETY: `raw` is +1 retained per the Create rule; wrap takes it.
        Some(unsafe { CFType::wrap_under_create_rule(raw) })
    }
}

fn entry_prop_u32(entry: u32, key: &str) -> Option<u32> {
    let value = entry_prop(entry, key)?;
    if !value.instance_of::<CFNumber>() {
        return None;
    }
    // SAFETY: type verified via instance_of::<CFNumber>() above.
    let number = unsafe { CFNumber::wrap_under_get_rule(value.as_concrete_TypeRef() as CFNumberRef) };
    number.to_i64().and_then(|v| u32::try_from(v).ok())
}

fn edid_from_entry(entry: u32, vendor: u32, product: u32, serial: u32) -> Option<Vec<u8>> {
    if entry_prop_u32(entry, "DisplayVendorID")? != vendor {
        return None;
    }
    if entry_prop_u32(entry, "DisplayProductID")? != product {
        return None;
    }
    // Serial is frequently 0 (unset) on either side; only enforce a match
    // when both report a non-zero serial.
    if serial != 0 {
        if let Some(s) = entry_prop_u32(entry, "DisplaySerialNumber") {
            if s != 0 && s != serial {
                return None;
            }
        }
    }
    // kIODisplayEDIDKey
    let value = entry_prop(entry, "IODisplayEDID")?;
    if !value.instance_of::<CFData>() {
        return None;
    }
    // SAFETY: type verified via instance_of::<CFData>() above.
    let data = unsafe { CFData::wrap_under_get_rule(value.as_concrete_TypeRef() as CFDataRef) };
    Some(data.bytes().to_vec())
}
