//! USB HID keyboard page (0x07) usage ID ⇄ Linux `KEY_*` code table.
//!
//! Same coverage as the macOS table (`tl-macos-input/src/keys.rs`): the
//! contiguous usage set `0x04..=0x63` plus the eight modifiers
//! `0xE0..=0xE7` (104 entries), mapped onto `KEY_*` codes from
//! `<linux/input-event-codes.h>`. Strictly 1:1, so both lookup directions
//! are exact inverses; anything outside the covered range returns `None`.

/// `(USB HID usage ID, Linux KEY_* code)` pairs.
///
/// Sources: USB HID Usage Tables §10 (keyboard/keypad page 0x07) and
/// `include/uapi/linux/input-event-codes.h`.
pub(crate) const TABLE: &[(u16, u16)] = &[
    // 0x04–0x1D: letters a–z → KEY_A … KEY_Z
    (0x04, 30),  // a → KEY_A
    (0x05, 48),  // b → KEY_B
    (0x06, 46),  // c → KEY_C
    (0x07, 32),  // d → KEY_D
    (0x08, 18),  // e → KEY_E
    (0x09, 33),  // f → KEY_F
    (0x0A, 34),  // g → KEY_G
    (0x0B, 35),  // h → KEY_H
    (0x0C, 23),  // i → KEY_I
    (0x0D, 36),  // j → KEY_J
    (0x0E, 37),  // k → KEY_K
    (0x0F, 38),  // l → KEY_L
    (0x10, 50),  // m → KEY_M
    (0x11, 49),  // n → KEY_N
    (0x12, 24),  // o → KEY_O
    (0x13, 25),  // p → KEY_P
    (0x14, 16),  // q → KEY_Q
    (0x15, 19),  // r → KEY_R
    (0x16, 31),  // s → KEY_S
    (0x17, 20),  // t → KEY_T
    (0x18, 22),  // u → KEY_U
    (0x19, 47),  // v → KEY_V
    (0x1A, 17),  // w → KEY_W
    (0x1B, 45),  // x → KEY_X
    (0x1C, 21),  // y → KEY_Y
    (0x1D, 44),  // z → KEY_Z
    // 0x1E–0x27: digits 1–0 → KEY_1 … KEY_0
    (0x1E, 2),   // 1 → KEY_1
    (0x1F, 3),   // 2 → KEY_2
    (0x20, 4),   // 3 → KEY_3
    (0x21, 5),   // 4 → KEY_4
    (0x22, 6),   // 5 → KEY_5
    (0x23, 7),   // 6 → KEY_6
    (0x24, 8),   // 7 → KEY_7
    (0x25, 9),   // 8 → KEY_8
    (0x26, 10),  // 9 → KEY_9
    (0x27, 11),  // 0 → KEY_0
    // 0x28–0x2C: control/whitespace keys
    (0x28, 28),  // Return → KEY_ENTER
    (0x29, 1),   // Escape → KEY_ESC
    (0x2A, 14),  // Backspace → KEY_BACKSPACE
    (0x2B, 15),  // Tab → KEY_TAB
    (0x2C, 57),  // Space → KEY_SPACE
    // 0x2D–0x38: punctuation
    (0x2D, 12),  // - _ → KEY_MINUS
    (0x2E, 13),  // = + → KEY_EQUAL
    (0x2F, 26),  // [ { → KEY_LEFTBRACE
    (0x30, 27),  // ] } → KEY_RIGHTBRACE
    (0x31, 43),  // \ | → KEY_BACKSLASH
    (0x32, 85),  // non-US # ~ → KEY_102ND
    (0x33, 39),  // ; : → KEY_SEMICOLON
    (0x34, 40),  // ' " → KEY_APOSTROPHE
    (0x35, 41),  // ` ~ → KEY_GRAVE
    (0x36, 51),  // , < → KEY_COMMA
    (0x37, 52),  // . > → KEY_DOT
    (0x38, 53),  // / ? → KEY_SLASH
    (0x39, 58),  // CapsLock → KEY_CAPSLOCK
    // 0x3A–0x45: F1–F12
    (0x3A, 59),  // F1 → KEY_F1
    (0x3B, 60),  // F2 → KEY_F2
    (0x3C, 61),  // F3 → KEY_F3
    (0x3D, 62),  // F4 → KEY_F4
    (0x3E, 63),  // F5 → KEY_F5
    (0x3F, 64),  // F6 → KEY_F6
    (0x40, 65),  // F7 → KEY_F7
    (0x41, 66),  // F8 → KEY_F8
    (0x42, 67),  // F9 → KEY_F9
    (0x43, 68),  // F10 → KEY_F10
    (0x44, 87),  // F11 → KEY_F11
    (0x45, 88),  // F12 → KEY_F12
    // 0x46–0x4E: nav/editing cluster. Linux has no dedicated keycode for
    // PrintScreen (it reports KEY_SYSRQ, like the kernel's own HID map).
    (0x46, 99),   // PrintScreen → KEY_SYSRQ
    (0x47, 70),   // ScrollLock → KEY_SCROLLLOCK
    (0x48, 119),  // Pause → KEY_PAUSE
    (0x49, 110),  // Insert → KEY_INSERT
    (0x4A, 102),  // Home → KEY_HOME
    (0x4B, 104),  // PageUp → KEY_PAGEUP
    (0x4C, 111),  // Delete forward → KEY_DELETE
    (0x4D, 108),  // End → KEY_END
    (0x4E, 109),  // PageDown → KEY_PAGEDOWN
    // 0x4F–0x52: arrow keys
    (0x4F, 106),  // Right → KEY_RIGHT
    (0x50, 105),  // Left → KEY_LEFT
    (0x51, 107),  // Down → KEY_DOWN
    (0x52, 103),  // Up → KEY_UP
    // 0x53–0x63: numpad
    (0x53, 69),   // NumLock/Clear → KEY_NUMLOCK
    (0x54, 98),   // KP / → KEY_KPSLASH
    (0x55, 55),   // KP * → KEY_KPASTERISK
    (0x56, 74),   // KP - → KEY_KPMINUS
    (0x57, 78),   // KP + → KEY_KPPLUS
    (0x58, 96),   // KP Enter → KEY_KPENTER
    (0x59, 79),   // KP 1 → KEY_KP1
    (0x5A, 80),   // KP 2 → KEY_KP2
    (0x5B, 81),   // KP 3 → KEY_KP3
    (0x5C, 75),   // KP 4 → KEY_KP4
    (0x5D, 76),   // KP 5 → KEY_KP5
    (0x5E, 77),   // KP 6 → KEY_KP6
    (0x5F, 71),   // KP 7 → KEY_KP7
    (0x60, 72),   // KP 8 → KEY_KP8
    (0x61, 73),   // KP 9 → KEY_KP9
    (0x62, 82),   // KP 0 → KEY_KP0
    (0x63, 83),   // KP . → KEY_KPDOT
    // 0xE0–0xE7: modifiers
    (0xE0, 29),    // LeftControl → KEY_LEFTCTRL
    (0xE1, 42),    // LeftShift → KEY_LEFTSHIFT
    (0xE2, 56),    // LeftAlt → KEY_LEFTALT
    (0xE3, 125),   // LeftGUI → KEY_LEFTMETA
    (0xE4, 97),    // RightControl → KEY_RIGHTCTRL
    (0xE5, 54),    // RightShift → KEY_RIGHTSHIFT
    (0xE6, 100),   // RightAlt → KEY_RIGHTALT
    (0xE7, 126),   // RightGUI → KEY_RIGHTMETA
];

/// USB HID usage ID → Linux `KEY_*` code, if covered by [`TABLE`].
pub(crate) fn usage_to_keycode(usage: u16) -> Option<u16> {
    TABLE.iter().find(|&&(u, _)| u == usage).map(|&(_, k)| k)
}

/// Linux `KEY_*` code → USB HID usage ID, if covered by [`TABLE`].
/// Test-only for now: the injection side maps usage → code only. The
/// evdev capture side (docs/LINUX-PORT.md) will consume this direction.
#[cfg(test)]
pub(crate) fn keycode_to_usage(keycode: u16) -> Option<u16> {
    TABLE.iter().find(|&&(_, k)| k == keycode).map(|&(u, _)| u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_strictly_bijective() {
        let mut usages: Vec<u16> = TABLE.iter().map(|&(u, _)| u).collect();
        usages.sort_unstable();
        usages.dedup();
        assert_eq!(usages.len(), TABLE.len(), "duplicate HID usages in table");

        let mut codes: Vec<u16> = TABLE.iter().map(|&(_, c)| c).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), TABLE.len(), "duplicate KEY_* codes in table");

        // Both lookups are exact inverses over the covered set.
        for &(usage, code) in TABLE {
            assert_eq!(usage_to_keycode(usage), Some(code));
            assert_eq!(keycode_to_usage(code), Some(usage));
        }
    }

    #[test]
    fn table_covers_the_macos_usage_set() {
        // 104 entries: contiguous 0x04..=0x63 plus the 8 modifiers.
        assert_eq!(TABLE.len(), 104);
        for usage in 0x04..=0x63 {
            assert!(
                usage_to_keycode(usage).is_some(),
                "usage {usage:#04x} must be covered"
            );
        }
        for usage in 0xE0..=0xE7 {
            assert!(
                usage_to_keycode(usage).is_some(),
                "modifier usage {usage:#04x} must be covered"
            );
        }
        // Everything outside the covered set is None (unmapped usages get
        // skipped by the injector).
        for usage in [0x00u16, 0x03, 0x64, 0x65, 0xDF, 0xE8, 0xFFFF] {
            assert!(
                usage_to_keycode(usage).is_none(),
                "usage {usage:#04x} must NOT be covered"
            );
        }
    }

    #[test]
    fn spot_checks_against_input_event_codes_h() {
        assert_eq!(usage_to_keycode(0x04), Some(30)); // a → KEY_A
        assert_eq!(usage_to_keycode(0x28), Some(28)); // Return → KEY_ENTER
        assert_eq!(usage_to_keycode(0x29), Some(1)); // Escape → KEY_ESC
        assert_eq!(usage_to_keycode(0x4C), Some(111)); // Delete → KEY_DELETE
        assert_eq!(usage_to_keycode(0x46), Some(99)); // PrintScreen → KEY_SYSRQ
        assert_eq!(usage_to_keycode(0x5F), Some(71)); // KP7 → KEY_KP7
        assert_eq!(usage_to_keycode(0xE3), Some(125)); // LeftGUI → KEY_LEFTMETA
        assert_eq!(keycode_to_usage(83), Some(0x63)); // KEY_KPDOT
        assert_eq!(keycode_to_usage(119), Some(0x48)); // KEY_PAUSE
    }
}
