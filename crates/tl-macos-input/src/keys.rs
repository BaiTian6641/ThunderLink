//! USB HID keyboard page (0x07) usage ID ⇄ macOS virtual keycode table.
//!
//! The keycode space is the Carbon/HIToolbox `kVK_*` ANSI set (the same
//! numeric space as `CGKeyCode`). The table is strictly 1:1 so both lookup
//! directions are exact inverses over the covered range; anything outside it
//! returns `None`.

/// `(USB HID usage ID, macOS kVK_* keycode)` pairs.
///
/// Sources: USB HID Usage Tables §10 (keyboard/keypad page 0x07) and
/// `HIToolbox/Events.h` (`kVK_*`).
pub(crate) const TABLE: &[(u16, u16)] = &[
    // 0x04–0x1D: letters a–z → kVK_ANSI_*
    (0x04, 0x00), // a → kVK_ANSI_A
    (0x05, 0x0B), // b → kVK_ANSI_B
    (0x06, 0x08), // c → kVK_ANSI_C
    (0x07, 0x02), // d → kVK_ANSI_D
    (0x08, 0x0E), // e → kVK_ANSI_E
    (0x09, 0x03), // f → kVK_ANSI_F
    (0x0A, 0x05), // g → kVK_ANSI_G
    (0x0B, 0x04), // h → kVK_ANSI_H
    (0x0C, 0x22), // i → kVK_ANSI_I
    (0x0D, 0x26), // j → kVK_ANSI_J
    (0x0E, 0x28), // k → kVK_ANSI_K
    (0x0F, 0x25), // l → kVK_ANSI_L
    (0x10, 0x2E), // m → kVK_ANSI_M
    (0x11, 0x2D), // n → kVK_ANSI_N
    (0x12, 0x1F), // o → kVK_ANSI_O
    (0x13, 0x23), // p → kVK_ANSI_P
    (0x14, 0x0C), // q → kVK_ANSI_Q
    (0x15, 0x0F), // r → kVK_ANSI_R
    (0x16, 0x01), // s → kVK_ANSI_S
    (0x17, 0x11), // t → kVK_ANSI_T
    (0x18, 0x20), // u → kVK_ANSI_U
    (0x19, 0x09), // v → kVK_ANSI_V
    (0x1A, 0x0D), // w → kVK_ANSI_W
    (0x1B, 0x07), // x → kVK_ANSI_X
    (0x1C, 0x10), // y → kVK_ANSI_Y
    (0x1D, 0x06), // z → kVK_ANSI_Z
    // 0x1E–0x27: digits 1–0 → kVK_ANSI_1 … kVK_ANSI_0
    (0x1E, 0x12), // 1
    (0x1F, 0x13), // 2
    (0x20, 0x14), // 3
    (0x21, 0x15), // 4
    (0x22, 0x16), // 5
    (0x23, 0x17), // 6
    (0x24, 0x1A), // 7
    (0x25, 0x1C), // 8
    (0x26, 0x19), // 9
    (0x27, 0x1D), // 0
    // 0x28–0x2C: control/whitespace keys
    (0x28, 0x24), // Return → kVK_Return
    (0x29, 0x35), // Escape → kVK_Escape
    (0x2A, 0x33), // Backspace → kVK_Delete
    (0x2B, 0x30), // Tab → kVK_Tab
    (0x2C, 0x31), // Space → kVK_Space
    // 0x2D–0x38: punctuation
    (0x2D, 0x1B), // - _ → kVK_ANSI_Minus
    (0x2E, 0x18), // = + → kVK_ANSI_Equal
    (0x2F, 0x21), // [ { → kVK_ANSI_LeftBracket
    (0x30, 0x1E), // ] } → kVK_ANSI_RightBracket
    (0x31, 0x2A), // \ | → kVK_ANSI_Backslash
    (0x32, 0x0A), // non-US # ~ → kVK_ISO_Section
    (0x33, 0x29), // ; : → kVK_ANSI_Semicolon
    (0x34, 0x27), // ' " → kVK_ANSI_Quote
    (0x35, 0x32), // ` ~ → kVK_ANSI_Grave
    (0x36, 0x2B), // , < → kVK_ANSI_Comma
    (0x37, 0x2F), // . > → kVK_ANSI_Period
    (0x38, 0x2C), // / ? → kVK_ANSI_Slash
    (0x39, 0x39), // CapsLock → kVK_CapsLock
    // 0x3A–0x45: F1–F12
    (0x3A, 0x7A), // F1
    (0x3B, 0x78), // F2
    (0x3C, 0x63), // F3
    (0x3D, 0x76), // F4
    (0x3E, 0x60), // F5
    (0x3F, 0x61), // F6
    (0x40, 0x62), // F7
    (0x41, 0x64), // F8
    (0x42, 0x65), // F9
    (0x43, 0x6D), // F10
    (0x44, 0x67), // F11
    (0x45, 0x6F), // F12
    // 0x46–0x4E: nav/editing cluster (PrintScreen/ScrollLock/Pause have no
    // dedicated kVK; F13–F15 are the conventional macOS seats).
    (0x46, 0x69), // PrintScreen → kVK_F13
    (0x47, 0x6B), // ScrollLock → kVK_F14
    (0x48, 0x71), // Pause → kVK_F15
    (0x49, 0x72), // Insert → kVK_Help
    (0x4A, 0x73), // Home → kVK_Home
    (0x4B, 0x74), // PageUp → kVK_PageUp
    (0x4C, 0x75), // Delete forward → kVK_ForwardDelete
    (0x4D, 0x77), // End → kVK_End
    (0x4E, 0x79), // PageDown → kVK_PageDown
    // 0x4F–0x52: arrow keys
    (0x4F, 0x7C), // Right → kVK_RightArrow
    (0x50, 0x7B), // Left → kVK_LeftArrow
    (0x51, 0x7D), // Down → kVK_DownArrow
    (0x52, 0x7E), // Up → kVK_UpArrow
    // 0x53–0x63: numpad
    (0x53, 0x47), // NumLock/Clear → kVK_ANSI_KeypadClear
    (0x54, 0x4B), // KP / → kVK_ANSI_KeypadDivide
    (0x55, 0x43), // KP * → kVK_ANSI_KeypadMultiply
    (0x56, 0x4E), // KP - → kVK_ANSI_KeypadMinus
    (0x57, 0x45), // KP + → kVK_ANSI_KeypadPlus
    (0x58, 0x4C), // KP Enter → kVK_ANSI_KeypadEnter
    (0x59, 0x53), // KP 1 → kVK_ANSI_Keypad1
    (0x5A, 0x54), // KP 2
    (0x5B, 0x55), // KP 3
    (0x5C, 0x56), // KP 4
    (0x5D, 0x57), // KP 5
    (0x5E, 0x58), // KP 6
    (0x5F, 0x59), // KP 7
    (0x60, 0x5C), // KP 8
    (0x61, 0x5D), // KP 9
    (0x62, 0x52), // KP 0
    (0x63, 0x41), // KP . → kVK_ANSI_KeypadDecimal
    // 0xE0–0xE7: modifiers
    (0xE0, 0x3B), // LeftControl → kVK_Control
    (0xE1, 0x38), // LeftShift → kVK_Shift
    (0xE2, 0x3A), // LeftAlt → kVK_Option
    (0xE3, 0x37), // LeftGUI → kVK_Command
    (0xE4, 0x3E), // RightControl → kVK_RightControl
    (0xE5, 0x3C), // RightShift → kVK_RightShift
    (0xE6, 0x3D), // RightAlt → kVK_RightOption
    (0xE7, 0x36), // RightGUI → kVK_RightCommand
];

/// USB HID usage ID → macOS keycode, if covered by [`TABLE`].
pub(crate) fn usage_to_keycode(usage: u16) -> Option<u16> {
    TABLE.iter().find(|&&(u, _)| u == usage).map(|&(_, k)| k)
}

/// macOS keycode → USB HID usage ID, if covered by [`TABLE`].
pub(crate) fn keycode_to_usage(keycode: u16) -> Option<u16> {
    TABLE.iter().find(|&&(_, k)| k == keycode).map(|&(u, _)| u)
}
