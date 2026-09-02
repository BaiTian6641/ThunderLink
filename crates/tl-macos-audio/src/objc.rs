//! Minimal Objective-C runtime access, used only to construct the
//! `CATapDescription` object (an ObjC *class*, per
//! `CoreAudio.framework/Headers/CATapDescription.h`) that
//! `AudioHardwareCreateProcessTap` consumes.
//!
//! `objc_msgSend` is transmuted to the exact typed signature of each call
//! site — the standard ABI-safe pattern on both x86_64 and arm64 Apple
//! targets (all our arguments are integer/pointer class, no float regs).
use std::ffi::{c_char, c_void, CStr};

use anyhow::{anyhow, Result};

pub(crate) type ObjcId = *mut c_void;
pub(crate) type Sel = *const c_void;

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> ObjcId;
    fn sel_registerName(name: *const c_char) -> Sel;
    /// Declared as a unit function and transmuted per call site; see module docs.
    fn objc_msgSend();
    fn objc_release(object: ObjcId);
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// Look up a class by name; errors (rather than returning nil) so callers
/// fail loudly on wrong SDKs.
pub(crate) fn get_class(name: &CStr) -> Result<ObjcId> {
    // SAFETY: `name` is a valid NUL-terminated C string; objc_getClass has no
    // preconditions beyond that.
    let cls = unsafe { objc_getClass(name.as_ptr()) };
    if cls.is_null() {
        return Err(anyhow!(
            "Objective-C class {} not found (requires the macOS 14.2+ SDK/runtime)",
            name.to_string_lossy()
        ));
    }
    Ok(cls)
}

/// Register (or fetch) a selector.
pub(crate) fn sel(name: &CStr) -> Sel {
    // SAFETY: `name` is a valid NUL-terminated C string.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Send `receiver sel` (no args), returning an object reference.
///
/// # Safety
/// `receiver` must be a live ObjC object and `selector` must name a method
/// taking no arguments and returning an object (`id`/`instancetype`).
pub(crate) unsafe fn msg_send_id0(receiver: ObjcId, selector: Sel) -> Result<ObjcId> {
    let imp: unsafe extern "C" fn(ObjcId, Sel) -> ObjcId =
        // SAFETY: transmuting objc_msgSend to the exact static signature of
        // this call site is ABI-correct on Apple targets (see module docs).
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: caller guarantees receiver/selector validity for this shape.
    let ret = unsafe { imp(receiver, selector) };
    if ret.is_null() {
        Err(anyhow!(
            "Objective-C message returned nil (receiver {:?}, selector {:?})",
            receiver,
            selector
        ))
    } else {
        Ok(ret)
    }
}

/// Send `receiver selector: arg` where `arg` is an object reference and the
/// method returns an object (`id`/`instancetype`).
///
/// # Safety
/// `receiver` must be a live ObjC object; `selector` must name a method with
/// exactly one object argument returning an object.
pub(crate) unsafe fn msg_send_id1(receiver: ObjcId, selector: Sel, arg: ObjcId) -> Result<ObjcId> {
    let imp: unsafe extern "C" fn(ObjcId, Sel, ObjcId) -> ObjcId =
        // SAFETY: see msg_send_id0.
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: caller guarantees receiver/selector validity for this shape.
    let ret = unsafe { imp(receiver, selector, arg) };
    if ret.is_null() {
        Err(anyhow!(
            "Objective-C message returned nil (receiver {:?}, selector {:?})",
            receiver,
            selector
        ))
    } else {
        Ok(ret)
    }
}

/// Send `receiver selector: arg` with an `NSInteger` argument and void return
/// (used for `setMuteBehavior:`; `CATapMuteBehavior` is an `NS_ENUM(NSInteger…)`).
///
/// # Safety
/// `receiver` must be a live ObjC object; `selector` must name a method with
/// exactly one `NSInteger` argument returning void.
pub(crate) unsafe fn msg_send_long1_void(receiver: ObjcId, selector: Sel, arg: i64) {
    let imp: unsafe extern "C" fn(ObjcId, Sel, i64) =
        // SAFETY: see msg_send_id0.
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: caller guarantees receiver/selector validity for this shape.
    unsafe { imp(receiver, selector, arg) };
}

/// Release an owned (alloc/new/retain) object reference.
///
/// # Safety
/// `object` must be non-null and must not be released again afterwards.
pub(crate) unsafe fn release(object: ObjcId) {
    // SAFETY: caller guarantees single release of a live owned object.
    unsafe { objc_release(object) };
}

/// RAII autorelease pool — framework internals may autorelease while we talk
/// to them; without a pool those would leak for the thread's lifetime.
pub(crate) struct AutoreleasePool {
    token: *mut c_void,
}

impl AutoreleasePool {
    pub(crate) fn new() -> Self {
        // SAFETY: objc_autoreleasePoolPush has no preconditions.
        Self { token: unsafe { objc_autoreleasePoolPush() } }
    }
}

impl Default for AutoreleasePool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        // SAFETY: token came from a matching push in `new` and is popped once.
        unsafe { objc_autoreleasePoolPop(self.token) };
    }
}
