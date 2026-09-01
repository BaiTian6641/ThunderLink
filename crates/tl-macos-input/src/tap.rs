//! Input capture: a global CGEventTap producing normalized `InputEvent`s.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoop, CFRunLoopRef, CFRunLoopTimer, CFRunLoopTimerContext,
    CFRunLoopTimerRef,
};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};
use tl_proto::{InputEvent, Mods, MouseButton, COORD_MAX};

use crate::keys;

/// Rect (global display coordinates, points) being streamed; pointer
/// events are normalized over it (SPEC §7).
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// macOS CGKeyCode → USB HID usage ID (inverse of
/// `inject::hid_usage_to_keycode`; share one table).
pub fn keycode_to_hid_usage(keycode: u16) -> Option<u16> {
    keys::keycode_to_usage(keycode)
}

// NX device-dependent modifier bits, present in the low half of the raw
// CGEventFlags word of `flagsChanged` events (IOLLEvent.h). Index i
// corresponds to HID usage 0xE0 + i.
const MOD_DEVICE_BITS: [u64; 8] = [
    0x01, // 0xE0 LeftControl  (NX_DEVICELCTLKEYMASK)
    0x02, // 0xE1 LeftShift    (NX_DEVICELSHIFTKEYMASK)
    0x20, // 0xE2 LeftAlt      (NX_DEVICELALTKEYMASK)
    0x08, // 0xE3 LeftGUI      (NX_DEVICELCMDKEYMASK)
    0x80, // 0xE4 RightControl (NX_DEVICERCTLKEYMASK)
    0x04, // 0xE5 RightShift   (NX_DEVICERSHIFTKEYMASK)
    0x40, // 0xE6 RightAlt     (NX_DEVICERALTKEYMASK)
    0x10, // 0xE7 RightGUI     (NX_DEVICERCMDKEYMASK)
];

const HID_CAPS_LOCK: u16 = 0x39;

/// Move-coalescing flush cadence; SPEC §7 caps input batches at 500 Hz.
const FLUSH_INTERVAL_SECS: f64 = 0.002;

const EVENT_TYPES: &[CGEventType] = &[
    CGEventType::LeftMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseDown,
    CGEventType::RightMouseUp,
    CGEventType::OtherMouseDown,
    CGEventType::OtherMouseUp,
    CGEventType::MouseMoved,
    CGEventType::LeftMouseDragged,
    CGEventType::RightMouseDragged,
    CGEventType::OtherMouseDragged,
    CGEventType::ScrollWheel,
    CGEventType::KeyDown,
    CGEventType::KeyUp,
    CGEventType::FlagsChanged,
];

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRunLoopWakeUp(rl: CFRunLoopRef);
}

/// Global CGEventTap. Error message MUST contain "permission" when the
/// Input Monitoring / Accessibility TCC grant is missing (SPEC §9).
pub struct EventTap {
    thread: Option<JoinHandle<()>>,
    runloop: Option<CFRunLoop>,
}

impl EventTap {
    /// Events arrive on a dedicated thread. Move events coalesced;
    /// emit `InputEvent::Leave` when the pointer exits `bounds`.
    pub fn start(bounds: Rect, cb: Box<dyn FnMut(InputEvent) + Send>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Result<CFRunLoop>>();
        let thread = thread::Builder::new()
            .name("tl-input-tap".to_string())
            .spawn(move || run_tap_thread(bounds, cb, tx))
            .context("failed to spawn event-tap thread")?;
        let runloop = match rx.recv() {
            Ok(Ok(runloop)) => runloop,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(anyhow!("event-tap thread exited before reporting readiness"));
            }
        };
        Ok(Self {
            thread: Some(thread),
            runloop: Some(runloop),
        })
    }

    pub fn stop(&mut self) {
        if let Some(runloop) = self.runloop.take() {
            runloop.stop();
            // SAFETY: `runloop` is a valid CFRunLoop owned by this handle;
            // waking guarantees CFRunLoopRun returns promptly even with no
            // sources pending.
            unsafe { CFRunLoopWakeUp(runloop.as_concrete_TypeRef()) };
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!("event-tap thread panicked");
            }
        }
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Global-pointer → normalized-coordinate state machine (pure logic; unit
/// tested headlessly).
struct Normalizer {
    bounds: Rect,
    inside: bool,
}

impl Normalizer {
    fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            inside: false,
        }
    }

    fn contains(&self, px: f64, py: f64) -> bool {
        self.bounds.w > 0.0
            && self.bounds.h > 0.0
            && px >= self.bounds.x
            && px < self.bounds.x + self.bounds.w
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.h
    }

    /// Global point → normalized `0..=COORD_MAX`, clamped. Only meaningful
    /// while `contains` is true.
    fn normalize(&self, px: f64, py: f64) -> (u16, u16) {
        let norm = |v: f64, origin: f64, span: f64| -> u16 {
            let unit = if span > 0.0 { (v - origin) / span } else { 0.0 };
            let unit = if unit.is_nan() { 0.0 } else { unit.clamp(0.0, 1.0) };
            (unit * COORD_MAX as f64).round() as u16
        };
        (
            norm(px, self.bounds.x, self.bounds.w),
            norm(py, self.bounds.y, self.bounds.h),
        )
    }

    /// Feed a pointer-location event. Returns `MouseMove` while inside the
    /// bounds, `Leave` exactly once on the inside→outside transition, and
    /// `None` while outside.
    fn feed_pointer(&mut self, px: f64, py: f64) -> Option<InputEvent> {
        if self.contains(px, py) {
            self.inside = true;
            let (x, y) = self.normalize(px, py);
            Some(InputEvent::MouseMove { x, y })
        } else if std::mem::replace(&mut self.inside, false) {
            Some(InputEvent::Leave)
        } else {
            None
        }
    }
}

/// Mutable tap state. Lives exclusively on the tap's CFRunLoop thread; the
/// tap callback and the flush timer reach it through an `Rc<RefCell<_>>`.
struct TapState {
    norm: Normalizer,
    cb: Box<dyn FnMut(InputEvent) + Send>,
    /// Folded modifier mask applied to subsequent `Key` events.
    mods: Mods,
    /// Per-modifier-key down state, indexed by usage − 0xE0.
    mod_down: [bool; 8],
    caps_down: bool,
    /// Coalescing slot for consecutive pointer moves (latest wins).
    pending_move: Option<(u16, u16)>,
    /// Live tap mach port, for re-enabling disabled taps (null until set).
    tap_port: CFMachPortRef,
}

impl TapState {
    fn new(bounds: Rect, cb: Box<dyn FnMut(InputEvent) + Send>) -> Self {
        Self {
            norm: Normalizer::new(bounds),
            cb,
            mods: Mods::default(),
            mod_down: [false; 8],
            caps_down: false,
            pending_move: None,
            tap_port: std::ptr::null_mut(),
        }
    }

    fn emit(&mut self, ev: InputEvent) {
        (self.cb)(ev);
    }

    /// Deliver any coalesced move before a non-move event so ordering and
    /// final pointer position stay correct.
    fn flush_pending(&mut self) {
        if let Some((x, y)) = self.pending_move.take() {
            self.emit(InputEvent::MouseMove { x, y });
        }
    }

    fn feed_pointer(&mut self, px: f64, py: f64) {
        match self.norm.feed_pointer(px, py) {
            Some(InputEvent::MouseMove { x, y }) => self.pending_move = Some((x, y)),
            Some(ev) => {
                self.flush_pending();
                self.emit(ev);
            }
            None => {}
        }
    }

    fn feed_button(&mut self, button: MouseButton, down: bool, px: f64, py: f64) {
        if !self.norm.contains(px, py) {
            return;
        }
        self.flush_pending();
        self.emit(InputEvent::MouseButton { button, down });
    }

    fn feed_scroll(&mut self, dx: i64, dy: i64, px: f64, py: f64) {
        if !self.norm.contains(px, py) {
            return;
        }
        self.flush_pending();
        let clamp = |v: i64| v.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        self.emit(InputEvent::Scroll {
            dx: clamp(dx),
            dy: clamp(dy),
        });
    }

    fn feed_key(&mut self, usage: u16, down: bool) {
        self.flush_pending();
        self.emit(InputEvent::Key {
            usage,
            down,
            mods: self.mods,
        });
    }

    /// `flagsChanged`: refresh the folded modifier mask and emit `Key`
    /// events for the modifier keys themselves (HID 0xE0–0xE7 + CapsLock)
    /// whose state actually changed.
    fn feed_flags_changed(&mut self, flags: u64) {
        self.mods = mods_from_flags(flags);
        for (i, &bit) in MOD_DEVICE_BITS.iter().enumerate() {
            let down = flags & bit != 0;
            if down != self.mod_down[i] {
                self.mod_down[i] = down;
                self.flush_pending();
                self.emit(InputEvent::Key {
                    usage: 0xE0 + i as u16,
                    down,
                    mods: self.mods,
                });
            }
        }
        let caps = flags & CGEventFlags::CGEventFlagAlphaShift.bits() != 0;
        if caps != self.caps_down {
            self.caps_down = caps;
            self.flush_pending();
            self.emit(InputEvent::Key {
                usage: HID_CAPS_LOCK,
                down: caps,
                mods: self.mods,
            });
        }
    }
}

/// Raw `CGEventFlags` word → `tl_proto::Mods` (device-independent bits).
fn mods_from_flags(flags: u64) -> Mods {
    Mods {
        shift: flags & CGEventFlags::CGEventFlagShift.bits() != 0,
        ctrl: flags & CGEventFlags::CGEventFlagControl.bits() != 0,
        alt: flags & CGEventFlags::CGEventFlagAlternate.bits() != 0,
        meta: flags & CGEventFlags::CGEventFlagCommand.bits() != 0,
    }
}

fn handle_event(state: &mut TapState, etype: CGEventType, event: &CGEvent) {
    match etype {
        // The system can disable a tap on timeout/user input; re-enable it.
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            if !state.tap_port.is_null() {
                log::warn!("event tap disabled by system ({etype:?}); re-enabling");
                // SAFETY: `tap_port` is the live CFMachPortRef of this tap,
                // valid until the runloop thread invalidates it on shutdown.
                unsafe { CGEventTapEnable(state.tap_port, true) };
            }
        }
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let p = event.location();
            state.feed_pointer(p.x, p.y);
        }
        CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => {
            let p = event.location();
            state.feed_button(MouseButton::Left, matches!(etype, CGEventType::LeftMouseDown), p.x, p.y);
        }
        CGEventType::RightMouseDown | CGEventType::RightMouseUp => {
            let p = event.location();
            state.feed_button(MouseButton::Right, matches!(etype, CGEventType::RightMouseDown), p.x, p.y);
        }
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let p = event.location();
            let n = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match n {
                2 => MouseButton::Middle,
                3 => MouseButton::Back,
                4 => MouseButton::Forward,
                n => MouseButton::Other(n.clamp(0, 255) as u8),
            };
            state.feed_button(button, matches!(etype, CGEventType::OtherMouseDown), p.x, p.y);
        }
        CGEventType::ScrollWheel => {
            let p = event.location();
            let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            state.feed_scroll(dx, dy, p.x, p.y);
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let keycode =
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if let Some(usage) = keycode_to_hid_usage(keycode) {
                state.feed_key(usage, matches!(etype, CGEventType::KeyDown));
            }
        }
        CGEventType::FlagsChanged => {
            state.feed_flags_changed(event.get_flags().bits());
        }
        _ => {}
    }
}

/// CFRunLoop timer callback: flushes the coalesced move slot at 500 Hz.
extern "C" fn flush_timer(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `info` points at the `RefCell<TapState>` inside the `Rc` owned
    // by the tap-thread stack frame, which outlives the runloop run; the
    // timer is removed before that frame returns. Timer callbacks only ever
    // fire on the runloop thread, and `try_borrow_mut` guards against
    // re-entrancy from the tap callback.
    let state = unsafe { &*(info as *const RefCell<TapState>) };
    if let Ok(mut s) = state.try_borrow_mut() {
        s.flush_pending();
    }
}

fn run_tap_thread(
    bounds: Rect,
    cb: Box<dyn FnMut(InputEvent) + Send>,
    ready: mpsc::Sender<Result<CFRunLoop>>,
) {
    let state = Rc::new(RefCell::new(TapState::new(bounds, cb)));
    let cb_state = Rc::clone(&state);
    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        EVENT_TYPES.to_vec(),
        move |_proxy, etype, event| {
            if let Ok(mut s) = cb_state.try_borrow_mut() {
                handle_event(&mut s, etype, event);
            }
            None // listen-only: never consume
        },
    ) {
        Ok(tap) => tap,
        Err(()) => {
            let _ = ready.send(Err(anyhow!(
                "could not create CGEventTap: Input Monitoring / Accessibility \
                 permission missing (TCC denied)"
            )));
            return;
        }
    };

    let runloop = CFRunLoop::get_current();
    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            let _ = ready.send(Err(anyhow!(
                "could not create CFRunLoopSource for event tap"
            )));
            return;
        }
    };
    // SAFETY: `kCFRunLoopCommonModes` is a valid, immutable CFStringRef
    // constant exported by CoreFoundation.
    let mode = unsafe { kCFRunLoopCommonModes };
    runloop.add_source(&source, mode);

    // Move-coalescing flush timer (SPEC §7: batches at up to 500 Hz).
    let mut timer_ctx = CFRunLoopTimerContext {
        version: 0,
        info: Rc::as_ptr(&state) as *mut c_void,
        retain: None,
        release: None,
        copyDescription: None,
    };
    let timer = CFRunLoopTimer::new(
        // SAFETY: trivial getter for the current absolute time.
        unsafe { CFAbsoluteTimeGetCurrent() } + FLUSH_INTERVAL_SECS,
        FLUSH_INTERVAL_SECS,
        0,
        0,
        flush_timer,
        &mut timer_ctx,
    );
    runloop.add_timer(&timer, mode);

    state.borrow_mut().tap_port = tap.mach_port.as_concrete_TypeRef();
    tap.enable();

    if ready.send(Ok(runloop.clone())).is_err() {
        log::warn!("event-tap owner went away before startup completed");
        return;
    }
    log::debug!("event tap running");
    CFRunLoop::run_current();

    // Shutdown: detach everything from the (now stopped) runloop and kill
    // the tap port so no further callbacks can fire.
    runloop.remove_timer(&timer, mode);
    runloop.remove_source(&source, mode);
    // SAFETY: the port is owned by `tap` and invalidated exactly once, here,
    // after the runloop — and therefore all callbacks — has stopped.
    unsafe { CFMachPortInvalidate(tap.mach_port.as_concrete_TypeRef()) };
    log::debug!("event tap stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 100.0,
        }
    }

    fn collect(bounds: Rect) -> (TapState, std::sync::Arc<parking_lot::Mutex<Vec<InputEvent>>>) {
        let out = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let out2 = std::sync::Arc::clone(&out);
        let state = TapState::new(
            bounds,
            Box::new(move |ev| out2.lock().push(ev)),
        );
        (state, out)
    }

    #[test]
    fn normalizer_clamps_and_maps() {
        let n = Normalizer::new(rect());
        assert_eq!(n.normalize(100.0, 50.0), (0, 0));
        // Just inside the far edges.
        let (x, y) = n.normalize(299.999, 149.999);
        assert!(x >= COORD_MAX - 1, "x {x}");
        assert!(y >= COORD_MAX - 1, "y {y}");
        // Center of the rect.
        let (x, y) = n.normalize(200.0, 100.0);
        assert!((x as i32 - 32768).abs() <= 1, "center x {x}");
        assert!((y as i32 - 32768).abs() <= 1, "center y {y}");
    }

    #[test]
    fn normalizer_leave_transitions() {
        let mut n = Normalizer::new(rect());
        // Outside before ever entering: no Leave.
        assert_eq!(n.feed_pointer(0.0, 0.0), None);
        // Enter: a move, no Leave.
        assert!(matches!(n.feed_pointer(150.0, 75.0), Some(InputEvent::MouseMove { .. })));
        // Exit edge: exactly one Leave.
        assert_eq!(n.feed_pointer(500.0, 500.0), Some(InputEvent::Leave));
        assert_eq!(n.feed_pointer(500.0, 500.0), None);
        // Re-enter resumes moves without a Leave.
        assert!(matches!(n.feed_pointer(150.0, 75.0), Some(InputEvent::MouseMove { .. })));
        // Edges: top-left corner inside, bottom-right edge outside.
        assert!(matches!(n.feed_pointer(100.0, 50.0), Some(InputEvent::MouseMove { .. })));
        assert_eq!(n.feed_pointer(300.0, 150.0), Some(InputEvent::Leave));
    }

    #[test]
    fn normalizer_degenerate_rect_never_inside() {
        let mut n = Normalizer::new(Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
        assert_eq!(n.feed_pointer(0.0, 0.0), None);
    }

    #[test]
    fn moves_are_coalesced_latest_wins() {
        let (mut s, out) = collect(rect());
        s.feed_pointer(110.0, 60.0);
        s.feed_pointer(120.0, 70.0);
        s.feed_pointer(130.0, 80.0);
        assert!(out.lock().is_empty(), "moves buffered until flush");
        s.flush_pending();
        let got = out.lock().clone();
        assert_eq!(got.len(), 1, "three moves coalesce into one");
        let (want_x, want_y) = s.norm.normalize(130.0, 80.0);
        assert_eq!(got[0], InputEvent::MouseMove { x: want_x, y: want_y });
    }

    #[test]
    fn pending_move_flushes_before_other_events() {
        let (mut s, out) = collect(rect());
        s.feed_pointer(150.0, 75.0);
        s.feed_button(MouseButton::Left, true, 150.0, 75.0);
        let got = out.lock().clone();
        assert!(matches!(got[0], InputEvent::MouseMove { .. }));
        assert_eq!(
            got[1],
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true
            }
        );
    }

    #[test]
    fn events_outside_bounds_are_dropped_except_leave() {
        let (mut s, out) = collect(rect());
        s.feed_button(MouseButton::Left, true, 0.0, 0.0);
        s.feed_scroll(0, 3, 999.0, 999.0);
        s.feed_pointer(0.0, 0.0); // never inside → no Leave
        assert!(out.lock().is_empty());
        // Enter, then scroll outside is still dropped but exit emits Leave.
        s.feed_pointer(150.0, 75.0);
        s.flush_pending();
        s.feed_pointer(999.0, 999.0);
        assert_eq!(out.lock().last(), Some(&InputEvent::Leave));
    }

    #[test]
    fn flags_changed_emits_modifier_keys_and_folds_mods() {
        let (mut s, out) = collect(rect());
        let shift = CGEventFlags::CGEventFlagShift.bits();
        let cmd = CGEventFlags::CGEventFlagCommand.bits();

        // Left shift down.
        s.feed_flags_changed(shift | 0x02);
        // Same flags again: no duplicate event.
        s.feed_flags_changed(shift | 0x02);
        // Left command added while shift held.
        s.feed_flags_changed(shift | cmd | 0x02 | 0x08);
        {
            let got = out.lock();
            assert_eq!(
                got.as_slice(),
                &[
                    InputEvent::Key {
                        usage: 0xE1,
                        down: true,
                        mods: Mods { shift: true, ctrl: false, alt: false, meta: false },
                    },
                    InputEvent::Key {
                        usage: 0xE3,
                        down: true,
                        mods: Mods { shift: true, ctrl: false, alt: false, meta: true },
                    },
                ]
            );
        }
        // A regular key while modifiers held folds the current mods in.
        s.feed_key(0x04, true);
        assert_eq!(
            out.lock().last(),
            Some(&InputEvent::Key {
                usage: 0x04,
                down: true,
                mods: Mods { shift: true, ctrl: false, alt: false, meta: true },
            })
        );
        // Right shift down additionally, then all released.
        s.feed_flags_changed(shift | cmd | 0x02 | 0x04 | 0x08);
        s.feed_flags_changed(0);
        let got = out.lock();
        assert!(got.contains(&InputEvent::Key {
            usage: 0xE5,
            down: true,
            mods: Mods { shift: true, ctrl: false, alt: false, meta: true },
        }));
        // Everything released: all four modifiers go up, mods cleared.
        for usage in [0xE1u16, 0xE3, 0xE5] {
            assert!(
                got.contains(&InputEvent::Key {
                    usage,
                    down: false,
                    mods: Mods::default(),
                }),
                "missing key-up for usage {usage:#x}"
            );
        }
    }

    #[test]
    fn flags_changed_caps_lock_toggles() {
        let (mut s, out) = collect(rect());
        let caps = CGEventFlags::CGEventFlagAlphaShift.bits();
        s.feed_flags_changed(caps);
        s.feed_flags_changed(0);
        let got = out.lock().clone();
        assert_eq!(
            got,
            vec![
                InputEvent::Key { usage: 0x39, down: true, mods: Mods::default() },
                InputEvent::Key { usage: 0x39, down: false, mods: Mods::default() },
            ]
        );
    }

    /// Environment-agnostic contract check: when TCC denies the tap, the
    /// error message must mention "permission" (SPEC §9).
    #[test]
    fn tap_start_error_mentions_permission() {
        let bounds = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        match EventTap::start(bounds, Box::new(|_| {})) {
            Err(e) => {
                let msg = format!("{e:#}").to_lowercase();
                assert!(msg.contains("permission"), "error must mention permission: {msg}");
            }
            Ok(mut tap) => tap.stop(),
        }
    }

    /// Requires Input Monitoring + Accessibility permissions; `TL_E2E=1`.
    /// Injects a key via `Injector` and expects the tap to observe it.
    #[test]
    fn tap_observes_injected_key_e2e() {
        if std::env::var("TL_E2E").as_deref() != Ok("1") {
            return;
        }
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let seen = Arc::new(AtomicBool::new(false));
        let seen2 = Arc::clone(&seen);
        // Whole-screen bounds so the injected pointer position is inside.
        let bounds = Rect { x: -10000.0, y: -10000.0, w: 20000.0, h: 20000.0 };
        let mut tap = EventTap::start(
            bounds,
            Box::new(move |ev| {
                if matches!(ev, InputEvent::Key { usage: 0x06, down: true, .. }) {
                    seen2.store(true, Ordering::SeqCst);
                }
            }),
        )
        .expect("event tap requires Input Monitoring permission");

        let mut inj = crate::inject::Injector::new().expect("injector");
        let map = crate::inject::Mapping {
            origin_x: -10000.0,
            origin_y: -10000.0,
            width: 20000.0,
            height: 20000.0,
        };
        inj.inject(
            &InputEvent::Key { usage: 0x06, down: true, mods: Mods::default() },
            &map,
        )
        .expect("inject key down");
        inj.inject(
            &InputEvent::Key { usage: 0x06, down: false, mods: Mods::default() },
            &map,
        )
        .expect("inject key up");

        let deadline = Instant::now() + Duration::from_secs(3);
        while !seen.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        tap.stop();
        assert!(seen.load(Ordering::SeqCst), "tap did not observe injected key");
    }
}
