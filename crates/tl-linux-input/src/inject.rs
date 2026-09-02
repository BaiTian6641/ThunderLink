//! Input injection: feed `tl_proto::InputEvent`s into a `/dev/uinput`
//! virtual HID device (SPEC §7; docs/LINUX-PORT.md).

use std::collections::HashSet;

use anyhow::Result;
use tl_proto::InputEvent;
use tl_proto::MouseButton;

use crate::keys;
use crate::uinput::{
    RawEvent, UinputDevice, ABS_X, ABS_Y, BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT,
    EV_ABS, EV_KEY, EV_REL, EV_SYN, REL_HWHEEL, REL_WHEEL, SYN_REPORT,
};

/// Destination for raw kernel input events. The production sink
/// ([`UinputDevice`]) writes into the real `/dev/uinput`; tests record the
/// stream instead, keeping the mapping logic verifiable headlessly without
/// a kernel device (SPEC §9).
pub(crate) trait EventSink: Send {
    /// Append one event to the current report.
    fn emit(&mut self, ev: RawEvent) -> Result<()>;

    /// Terminate the report: `EV_SYN`/`SYN_REPORT`. Consumers act on a
    /// report only once it is synced.
    fn sync(&mut self) -> Result<()> {
        self.emit(RawEvent::new(EV_SYN, SYN_REPORT, 0))
    }
}

impl EventSink for UinputDevice {
    fn emit(&mut self, ev: RawEvent) -> Result<()> {
        self.write_event(ev)
    }
}

/// USB HID usage ID → Linux `KEY_*` code. Covers the keyboard page
/// (0x04–0x38 letters/digits/punctuation, 0x39–0x63 function/nav/numpad,
/// 0xE0–0xE7 modifiers) — the same coverage as the macOS table.
pub fn hid_usage_to_keycode(usage: u16) -> Option<u16> {
    keys::usage_to_keycode(usage)
}

/// `tl_proto::MouseButton` → `BTN_*` code, for the five buttons the
/// virtual device advertises.
fn button_to_btn(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(BTN_LEFT),
        MouseButton::Right => Some(BTN_RIGHT),
        MouseButton::Middle => Some(BTN_MIDDLE),
        MouseButton::Back => Some(BTN_BACK),
        MouseButton::Forward => Some(BTN_FORWARD),
        MouseButton::Other(_) => None, // not advertised by the device
    }
}

/// Injects `InputEvent`s into a `/dev/uinput` virtual device.
///
/// Tracks every pressed key/button so [`Injector::release_all`] can
/// unwind partial input state (called on `InputEvent::Leave` and session
/// teardown).
///
/// There is no desktop-coordinate `Mapping` (unlike the macOS injector):
/// the virtual pointer is absolute over `0..=COORD_MAX`, which the
/// compositor scales onto the (virtual) screen.
pub struct Injector {
    sink: Box<dyn EventSink + Send>,
    /// Pressed keyboard keys, as USB HID usage IDs.
    pressed_keys: HashSet<u16>,
    /// Pressed mouse buttons (tiny set; `MouseButton` has no `Hash`).
    pressed_buttons: Vec<MouseButton>,
}

impl Injector {
    /// Open `/dev/uinput` and create the virtual device. Fails with an
    /// error mentioning "permission" when the device is unavailable to
    /// this session (module not loaded, not exposed to the container, or
    /// access denied) — SPEC §9.
    pub fn new() -> Result<Self> {
        let device = UinputDevice::open()?;
        Ok(Self::with_sink(Box::new(device)))
    }

    /// Build an injector over a caller-supplied sink (tests).
    pub(crate) fn with_sink(sink: Box<dyn EventSink + Send>) -> Self {
        Self {
            sink,
            pressed_keys: HashSet::new(),
            pressed_buttons: Vec::new(),
        }
    }

    /// Inject one event. Key events are translated through the HID usage
    /// table; usages with no `KEY_*` mapping (and `MouseButton::Other`)
    /// are dropped with a warning instead of failing the batch.
    ///
    /// `InputEvent::Key`'s `mods` field is advisory here: uinput has no
    /// per-event modifier flags — modifier state comes from the modifier
    /// keys' own down/up events (usages 0xE0–0xE7), which targets forward
    /// like any other key.
    pub fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                self.sink.emit(RawEvent::new(EV_ABS, ABS_X, i32::from(x)))?;
                self.sink.emit(RawEvent::new(EV_ABS, ABS_Y, i32::from(y)))?;
                self.sink.sync()?;
            }
            InputEvent::MouseButton { button, down } => {
                let Some(btn) = button_to_btn(button) else {
                    log::warn!("no BTN_* code for {button:?}; dropping button event");
                    return Ok(());
                };
                self.sink.emit(RawEvent::new(EV_KEY, btn, down as i32))?;
                self.sink.sync()?;
                if down {
                    if !self.pressed_buttons.contains(&button) {
                        self.pressed_buttons.push(button);
                    }
                } else {
                    self.pressed_buttons.retain(|&b| b != button);
                }
            }
            InputEvent::Scroll { dx, dy } => {
                // SPEC §7: line deltas, positive = up/right — exactly the
                // REL_WHEEL / REL_HWHEEL sign convention.
                self.sink.emit(RawEvent::new(EV_REL, REL_WHEEL, i32::from(dy)))?;
                self.sink.emit(RawEvent::new(EV_REL, REL_HWHEEL, i32::from(dx)))?;
                self.sink.sync()?;
            }
            InputEvent::Key { usage, down, mods: _ } => {
                let Some(code) = keys::usage_to_keycode(usage) else {
                    log::warn!("no KEY_* code for HID usage {usage:#06x}; dropping key event");
                    return Ok(());
                };
                self.sink.emit(RawEvent::new(EV_KEY, code, down as i32))?;
                self.sink.sync()?;
                if down {
                    self.pressed_keys.insert(usage);
                } else {
                    self.pressed_keys.remove(&usage);
                }
            }
            InputEvent::Leave => self.release_all()?,
        }
        Ok(())
    }

    /// Release every pressed key/button (stuck-input safety; called on
    /// `Leave` and session teardown). All ups go out as one atomic report
    /// (single `SYN_REPORT`), so consumers never observe a half-unwound
    /// chord.
    pub fn release_all(&mut self) -> Result<()> {
        let mut first_err: Option<anyhow::Error> = None;

        let usages: Vec<u16> = self.pressed_keys.drain().collect();
        let buttons = std::mem::take(&mut self.pressed_buttons);
        let dirty = !usages.is_empty() || !buttons.is_empty();

        for usage in usages {
            let Some(code) = keys::usage_to_keycode(usage) else {
                continue;
            };
            if let Err(e) = self.sink.emit(RawEvent::new(EV_KEY, code, 0)) {
                first_err.get_or_insert(e);
            }
        }
        for button in buttons {
            let Some(btn) = button_to_btn(button) else {
                continue;
            };
            if let Err(e) = self.sink.emit(RawEvent::new(EV_KEY, btn, 0)) {
                first_err.get_or_insert(e);
            }
        }
        if dirty {
            if let Err(e) = self.sink.sync() {
                first_err.get_or_insert(e);
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // Best-effort stuck-input safety; destroying the device (which
        // follows when the sink drops) also force-releases everything.
        let _ = self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tl_proto::Mods;

    use super::*;

    /// Records every emitted raw event (no /dev/uinput needed; SPEC §9).
    #[derive(Clone, Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<RawEvent>>>,
    }

    impl EventSink for Recorder {
        fn emit(&mut self, ev: RawEvent) -> Result<()> {
            self.events.lock().push(ev);
            Ok(())
        }
    }

    fn recorder() -> (Injector, Recorder) {
        let rec = Recorder::default();
        let inj = Injector::with_sink(Box::new(rec.clone()));
        (inj, rec)
    }

    fn events(rec: &Recorder) -> Vec<RawEvent> {
        rec.events.lock().clone()
    }

    fn key(usage: u16, down: bool) -> InputEvent {
        InputEvent::Key {
            usage,
            down,
            mods: Mods::default(),
        }
    }

    #[test]
    fn mouse_move_emits_abs_pair_then_sync() {
        let (mut inj, rec) = recorder();
        inj.inject(&InputEvent::MouseMove { x: 1234, y: 5678 }).unwrap();
        assert_eq!(
            events(&rec),
            vec![
                RawEvent::new(EV_ABS, ABS_X, 1234),
                RawEvent::new(EV_ABS, ABS_Y, 5678),
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
            ]
        );
        // Extremes of the normalized coordinate space stay in range.
        inj.inject(&InputEvent::MouseMove { x: 0, y: u16::MAX }).unwrap();
        assert_eq!(
            events(&rec)[3..],
            [
                RawEvent::new(EV_ABS, ABS_X, 0),
                RawEvent::new(EV_ABS, ABS_Y, i32::from(u16::MAX)),
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
            ][..]
        );
    }

    #[test]
    fn mouse_buttons_map_to_advertised_btn_codes() {
        let (mut inj, rec) = recorder();
        for (button, btn) in [
            (MouseButton::Left, BTN_LEFT),
            (MouseButton::Right, BTN_RIGHT),
            (MouseButton::Middle, BTN_MIDDLE),
            (MouseButton::Back, BTN_BACK),
            (MouseButton::Forward, BTN_FORWARD),
        ] {
            inj.inject(&InputEvent::MouseButton { button, down: true })
                .unwrap();
            inj.inject(&InputEvent::MouseButton { button, down: false })
                .unwrap();
            let evs = events(&rec);
            let tail: &[RawEvent] = &evs[evs.len() - 4..];
            assert_eq!(
                tail,
                &[
                    RawEvent::new(EV_KEY, btn, 1),
                    RawEvent::new(EV_SYN, SYN_REPORT, 0),
                    RawEvent::new(EV_KEY, btn, 0),
                    RawEvent::new(EV_SYN, SYN_REPORT, 0),
                ][..],
                "for {button:?}"
            );
        }
    }

    #[test]
    fn unmapped_other_button_is_skipped() {
        let (mut inj, rec) = recorder();
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Other(9),
            down: true,
        })
        .unwrap();
        assert!(events(&rec).is_empty());
    }

    #[test]
    fn scroll_maps_deltas_to_rel_wheels() {
        let (mut inj, rec) = recorder();
        inj.inject(&InputEvent::Scroll { dx: -2, dy: 3 }).unwrap();
        assert_eq!(
            events(&rec),
            vec![
                RawEvent::new(EV_REL, REL_WHEEL, 3),   // positive dy = up
                RawEvent::new(EV_REL, REL_HWHEEL, -2), // positive dx = right
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
            ]
        );
    }

    #[test]
    fn key_events_map_through_the_usage_table() {
        let (mut inj, rec) = recorder();
        let a = hid_usage_to_keycode(0x04).unwrap(); // KEY_A
        let lmeta = hid_usage_to_keycode(0xE3).unwrap(); // KEY_LEFTMETA
        inj.inject(&key(0x04, true)).unwrap();
        inj.inject(&key(0xE3, true)).unwrap();
        inj.inject(&key(0x04, false)).unwrap();
        assert_eq!(
            events(&rec),
            vec![
                RawEvent::new(EV_KEY, a, 1),
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
                RawEvent::new(EV_KEY, lmeta, 1),
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
                RawEvent::new(EV_KEY, a, 0),
                RawEvent::new(EV_SYN, SYN_REPORT, 0),
            ]
        );
    }

    #[test]
    fn unmapped_usage_is_skipped() {
        let (mut inj, rec) = recorder();
        for usage in [0x00u16, 0x65, 0xE8, 0xFFFF] {
            inj.inject(&key(usage, true)).unwrap();
        }
        assert!(events(&rec).is_empty());
    }

    #[test]
    fn leave_releases_all_tracked_state() {
        let (mut inj, rec) = recorder();
        let a = hid_usage_to_keycode(0x04).unwrap();
        let lshift = hid_usage_to_keycode(0xE1).unwrap();
        inj.inject(&key(0x04, true)).unwrap();
        inj.inject(&key(0xE1, true)).unwrap();
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        })
        .unwrap();
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Middle,
            down: true,
        })
        .unwrap();

        rec.events.lock().clear();
        inj.inject(&InputEvent::Leave).unwrap();
        let evs = events(&rec);
        // One atomic report: ups for a, LShift, Left, Middle (HashSet
        // drain order is unspecified) followed by a single sync.
        let (sync, ups) = evs.split_last().unwrap();
        assert_eq!(*sync, RawEvent::new(EV_SYN, SYN_REPORT, 0));
        assert_eq!(ups.len(), 4);
        assert!(ups.iter().all(|e| e.kind == EV_KEY && e.value == 0));
        let mut codes: Vec<u16> = ups.iter().map(|e| e.code).collect();
        codes.sort_unstable();
        assert_eq!(codes, vec![a, lshift, BTN_LEFT, BTN_MIDDLE]);

        // Fully unwound: a second Leave emits nothing.
        rec.events.lock().clear();
        inj.inject(&InputEvent::Leave).unwrap();
        assert!(events(&rec).is_empty());
    }

    #[test]
    fn release_all_unwinds_and_is_idempotent() {
        let (mut inj, rec) = recorder();
        inj.inject(&key(0x2C, true)).unwrap(); // space
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Forward,
            down: true,
        })
        .unwrap();

        rec.events.lock().clear();
        inj.release_all().unwrap();
        let evs = events(&rec);
        assert_eq!(evs.len(), 3, "two ups plus one sync: {evs:?}");
        let mut codes: Vec<u16> = evs[..2].iter().map(|e| e.code).collect();
        codes.sort_unstable();
        assert_eq!(codes, vec![hid_usage_to_keycode(0x2C).unwrap(), BTN_FORWARD]);

        inj.release_all().unwrap(); // nothing tracked anymore
        assert_eq!(events(&rec).len(), 3);
    }

    #[test]
    fn new_reports_permission_error_when_uinput_is_unavailable() {
        // The validation container has no /dev/uinput; on hosts that do
        // expose a writable one this just exercises the happy path.
        match Injector::new() {
            Err(e) => assert!(
                e.to_string().contains("permission"),
                "error must mention permission, got: {e:#}"
            ),
            Ok(_) => {}
        }
    }

    /// Requires a real /dev/uinput with write permission; `TL_E2E=1`.
    #[test]
    fn live_device_accepts_every_event_kind_e2e() {
        if std::env::var("TL_E2E").as_deref() != Ok("1") {
            eprintln!("skipped: set TL_E2E=1 to run the live uinput e2e test");
            return;
        }
        let mut inj = Injector::new().expect("uinput injector");
        inj.inject(&InputEvent::MouseMove { x: 100, y: 100 }).unwrap();
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        })
        .unwrap();
        inj.inject(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
        })
        .unwrap();
        inj.inject(&InputEvent::Scroll { dx: 1, dy: -1 }).unwrap();
        inj.inject(&key(0x2C, true)).unwrap();
        inj.inject(&key(0x2C, false)).unwrap();
        inj.release_all().unwrap();
    }
}
