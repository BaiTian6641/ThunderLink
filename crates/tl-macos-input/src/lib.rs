//! macOS input injection (CGEventPost) + capture (CGEventTap). SPEC.md §10.
//!
//! - [`inject`]: turns normalized `tl_proto::InputEvent`s into Quartz events
//!   posted at the HID level, through a desktop-coordinate [`inject::Mapping`].
//! - [`tap`]: a global `CGEventTap` on a dedicated CFRunLoop thread producing
//!   normalized `InputEvent`s for the streamed display [`tap::Rect`].
//!
//! Both sides share one USB HID usage ⇄ macOS keycode table (`keys`).

mod keys;
pub mod inject;
pub mod tap;
