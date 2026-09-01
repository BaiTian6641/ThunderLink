//! Input forwarding types (target → initiator).

use serde::{Deserialize, Serialize};

/// Normalized pointer coordinate ceiling within the streamed display.
/// Coordinates are 0..=COORD_MAX on each axis, aspect-preserving.
pub const COORD_MAX: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u8),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer position, normalized over the streamed display rect.
    MouseMove { x: u16, y: u16 },
    MouseButton { button: MouseButton, down: bool },
    /// Scroll deltas in "lines" (signed; positive = up/right).
    Scroll { dx: i16, dy: i16 },
    /// `usage` = USB HID usage ID of the key (platform-neutral).
    Key { usage: u16, down: bool, mods: Mods },
    /// Pointer left the streamed area / capture released: all buttons up.
    Leave,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputBatch {
    pub seq: u32,
    pub events: Vec<InputEvent>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedState {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}
