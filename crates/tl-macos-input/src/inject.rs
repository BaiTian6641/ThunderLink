//! Input injection: post `tl_proto::InputEvent`s into the Quartz event stream.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use tl_proto::{InputEvent, Mods, MouseButton, COORD_MAX};

use crate::keys;

/// Normalized → desktop coordinate mapping: the streamed display's rect
/// in the initiator's global display-coordinate space (points).
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    pub origin_x: f64,
    pub origin_y: f64,
    pub width: f64,
    pub height: f64,
}

impl Mapping {
    /// Normalized `0..=COORD_MAX` coordinates → global desktop points.
    /// Out-of-range fractions are clamped into the rect.
    pub fn denormalize(&self, x: u16, y: u16) -> (f64, f64) {
        (
            self.origin_x + denorm_unit(x as f64 / COORD_MAX as f64, self.width),
            self.origin_y + denorm_unit(y as f64 / COORD_MAX as f64, self.height),
        )
    }
}

/// Unit-interval fraction × span, clamped to `[0, span]`.
pub(crate) fn denorm_unit(unit: f64, span: f64) -> f64 {
    if unit.is_nan() {
        0.0
    } else {
        unit.clamp(0.0, 1.0) * span
    }
}

/// Posts `InputEvent`s via `CGEventPost` at the HID tap location.
///
/// Tracks every pressed key/button so [`Injector::release_all`] can unwind
/// partial input state (called on `InputEvent::Leave` and session teardown).
pub struct Injector {
    source: CGEventSource,
    /// Pressed keyboard keys, as USB HID usage IDs.
    pressed_keys: HashSet<u16>,
    /// Pressed mouse buttons (tiny set; `MouseButton` has no `Hash`).
    pressed_buttons: Vec<MouseButton>,
    /// Last injected pointer position (Quartz requires a point on
    /// button/scroll events).
    last_pos: CGPoint,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create CGEventSource (HID system state)"))?;
        // Seed the pointer position from the real cursor so a button event
        // injected before any MouseMove lands somewhere sensible.
        let last_pos = CGEvent::new(source.clone())
            .map(|e| e.location())
            .unwrap_or(CGPoint::new(0.0, 0.0));
        Ok(Self {
            source,
            pressed_keys: HashSet::new(),
            pressed_buttons: Vec::new(),
            last_pos,
        })
    }

    /// Post one event via CGEventPost. Keys use `hid_usage_to_keycode`.
    pub fn inject(&mut self, ev: &InputEvent, map: &Mapping) -> Result<()> {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = map.denormalize(x, y);
                self.last_pos = CGPoint::new(px, py);
                let e = CGEvent::new_mouse_event(
                    self.source.clone(),
                    CGEventType::MouseMoved,
                    self.last_pos,
                    CGMouseButton::Left,
                )
                .map_err(|_| anyhow!("failed to create mouse-move event"))?;
                e.post(CGEventTapLocation::HID);
            }
            InputEvent::MouseButton { button, down } => {
                self.post_button(button, down)?;
                if down {
                    if !self.pressed_buttons.contains(&button) {
                        self.pressed_buttons.push(button);
                    }
                } else {
                    self.pressed_buttons.retain(|&b| b != button);
                }
            }
            InputEvent::Scroll { dx, dy } => {
                // Line deltas (SPEC §7: "deltas in lines"; positive = up/right
                // matches the CG wheel1/wheel2 sign convention).
                let e = CGEvent::new_scroll_event(
                    self.source.clone(),
                    ScrollEventUnit::LINE,
                    2,
                    dy as i32,
                    dx as i32,
                    0,
                )
                .map_err(|_| anyhow!("failed to create scroll event"))?;
                e.post(CGEventTapLocation::HID);
            }
            InputEvent::Key { usage, down, mods } => {
                let Some(keycode) = hid_usage_to_keycode(usage) else {
                    log::warn!("no keycode for HID usage {usage:#06x}; dropping key event");
                    return Ok(());
                };
                let e = CGEvent::new_keyboard_event(self.source.clone(), keycode, down)
                    .map_err(|_| anyhow!("failed to create keyboard event (keycode {keycode})"))?;
                e.set_flags(mods_to_flags(mods));
                e.post(CGEventTapLocation::HID);
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
    /// `Leave` and session teardown).
    pub fn release_all(&mut self) -> Result<()> {
        let mut first_err: Option<anyhow::Error> = None;

        let keys: Vec<u16> = self.pressed_keys.drain().collect();
        for usage in keys {
            let Some(keycode) = hid_usage_to_keycode(usage) else {
                continue;
            };
            match CGEvent::new_keyboard_event(self.source.clone(), keycode, false) {
                Ok(e) => {
                    e.set_flags(CGEventFlags::empty());
                    e.post(CGEventTapLocation::HID);
                }
                Err(()) => {
                    first_err.get_or_insert_with(|| {
                        anyhow!("failed to create key-up event for keycode {keycode}")
                    });
                }
            }
        }

        let buttons = std::mem::take(&mut self.pressed_buttons);
        for button in buttons {
            if let Err(e) = self.post_button(button, false) {
                first_err.get_or_insert(e);
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn post_button(&self, button: MouseButton, down: bool) -> Result<()> {
        // macOS supports up to 32 numbered mouse buttons; 0 = left, 1 =
        // right, 2 = middle, 3+ = other ("USB device order"), posted as
        // OtherMouse events with the button-number field set.
        let (etype, cg_button, number) = match button {
            MouseButton::Left => (
                if down {
                    CGEventType::LeftMouseDown
                } else {
                    CGEventType::LeftMouseUp
                },
                CGMouseButton::Left,
                None,
            ),
            MouseButton::Right => (
                if down {
                    CGEventType::RightMouseDown
                } else {
                    CGEventType::RightMouseUp
                },
                CGMouseButton::Right,
                None,
            ),
            MouseButton::Middle => (
                other_type(down),
                CGMouseButton::Center,
                None, // Center already implies button 2
            ),
            MouseButton::Back => (other_type(down), CGMouseButton::Center, Some(3)),
            MouseButton::Forward => (other_type(down), CGMouseButton::Center, Some(4)),
            MouseButton::Other(n) => (other_type(down), CGMouseButton::Center, Some(n)),
        };
        let e = CGEvent::new_mouse_event(self.source.clone(), etype, self.last_pos, cg_button)
            .map_err(|_| anyhow!("failed to create mouse-button event"))?;
        if let Some(n) = number {
            e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, i64::from(n));
        }
        e.post(CGEventTapLocation::HID);
        Ok(())
    }
}

fn other_type(down: bool) -> CGEventType {
    if down {
        CGEventType::OtherMouseDown
    } else {
        CGEventType::OtherMouseUp
    }
}

/// `tl_proto::Mods` → Quartz `CGEventFlags` modifier mask.
fn mods_to_flags(mods: Mods) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if mods.shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if mods.ctrl {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if mods.alt {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if mods.meta {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

/// USB HID usage ID → macOS CGKeyCode. Covers the keyboard page
/// (0x04–0x38 letters/digits, 0x39–0x65 function/nav, 0xE0–0xE7 mods).
pub fn hid_usage_to_keycode(usage: u16) -> Option<u16> {
    keys::usage_to_keycode(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tap::keycode_to_hid_usage;

    #[test]
    fn keycode_table_roundtrip_both_directions() {
        for &(usage, keycode) in crate::keys::TABLE {
            assert_eq!(
                hid_usage_to_keycode(usage),
                Some(keycode),
                "usage {usage:#06x} must map to keycode {keycode:#06x}"
            );
            assert_eq!(
                keycode_to_hid_usage(keycode),
                Some(usage),
                "keycode {keycode:#06x} must map back to usage {usage:#06x}"
            );
        }
    }

    #[test]
    fn keycode_table_is_one_to_one() {
        let mut usages: Vec<u16> = crate::keys::TABLE.iter().map(|&(u, _)| u).collect();
        let mut keycodes: Vec<u16> = crate::keys::TABLE.iter().map(|&(_, k)| k).collect();
        usages.sort_unstable();
        keycodes.sort_unstable();
        usages.dedup();
        keycodes.dedup();
        assert_eq!(usages.len(), crate::keys::TABLE.len(), "duplicate usage");
        assert_eq!(keycodes.len(), crate::keys::TABLE.len(), "duplicate keycode");
    }

    #[test]
    fn keycode_table_spot_checks() {
        assert_eq!(hid_usage_to_keycode(0x04), Some(0x00)); // a → kVK_ANSI_A
        assert_eq!(hid_usage_to_keycode(0x1D), Some(0x06)); // z → kVK_ANSI_Z
        assert_eq!(hid_usage_to_keycode(0x28), Some(0x24)); // Return
        assert_eq!(hid_usage_to_keycode(0x29), Some(0x35)); // Escape
        assert_eq!(hid_usage_to_keycode(0x2C), Some(0x31)); // Space
        assert_eq!(hid_usage_to_keycode(0x3A), Some(0x7A)); // F1
        assert_eq!(hid_usage_to_keycode(0x45), Some(0x6F)); // F12
        assert_eq!(hid_usage_to_keycode(0x4F), Some(0x7C)); // Right arrow
        assert_eq!(hid_usage_to_keycode(0x58), Some(0x4C)); // KP Enter
        assert_eq!(hid_usage_to_keycode(0x63), Some(0x41)); // KP Decimal
        assert_eq!(hid_usage_to_keycode(0xE0), Some(0x3B)); // LeftControl
        assert_eq!(hid_usage_to_keycode(0xE3), Some(0x37)); // LeftGUI (Command)
        assert_eq!(hid_usage_to_keycode(0xE7), Some(0x36)); // RightGUI
        assert_eq!(keycode_to_hid_usage(0x37), Some(0xE3));
        // Unmapped usages / keycodes return None.
        assert_eq!(hid_usage_to_keycode(0x00), None);
        assert_eq!(hid_usage_to_keycode(0x65), None);
        assert_eq!(keycode_to_hid_usage(0x3F), None); // kVK_Function
    }

    #[test]
    fn denormalize_corners_center() {
        let map = Mapping {
            origin_x: 100.0,
            origin_y: -50.0,
            width: 800.0,
            height: 600.0,
        };
        let (x0, y0) = map.denormalize(0, 0);
        assert_eq!((x0, y0), (100.0, -50.0));

        let (x1, y1) = map.denormalize(COORD_MAX, COORD_MAX);
        assert_eq!((x1, y1), (900.0, 550.0));

        let (xc, yc) = map.denormalize(COORD_MAX / 2, COORD_MAX / 2);
        assert!((xc - 500.0).abs() < 0.01, "center x: {xc}");
        assert!((yc - 250.0).abs() < 0.01, "center y: {yc}");
    }

    #[test]
    fn denormalize_clamps_out_of_range() {
        assert_eq!(denorm_unit(-0.25, 100.0), 0.0);
        assert_eq!(denorm_unit(1.5, 100.0), 100.0);
        assert_eq!(denorm_unit(f64::NAN, 100.0), 0.0);
        // Valid fractions pass through unclamped.
        assert_eq!(denorm_unit(0.25, 200.0), 50.0);
    }

    /// Requires Accessibility permission; run with `TL_E2E=1`.
    #[test]
    fn injector_posts_events_e2e() {
        if std::env::var("TL_E2E").as_deref() != Ok("1") {
            return;
        }
        let mut inj = Injector::new().expect("injector");
        let map = Mapping {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 1000.0,
            height: 1000.0,
        };
        inj.inject(&InputEvent::MouseMove { x: 100, y: 100 }, &map)
            .expect("move");
        inj.inject(
            &InputEvent::Key {
                usage: 0x04,
                down: true,
                mods: Mods::default(),
            },
            &map,
        )
        .expect("key down");
        inj.inject(
            &InputEvent::Key {
                usage: 0x04,
                down: false,
                mods: Mods::default(),
            },
            &map,
        )
        .expect("key up");
        inj.inject(
            &InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
            &map,
        )
        .expect("button down");
        inj.inject(&InputEvent::Scroll { dx: 0, dy: 1 }, &map)
            .expect("scroll");
        inj.inject(&InputEvent::Leave, &map).expect("leave");
    }
}
