//! Linux input injection through a `/dev/uinput` virtual HID device.
//!
//! Mirrors the injection half of `tl-macos-input` (SPEC §7/§10;
//! docs/LINUX-PORT.md): [`inject::Injector`] turns normalized
//! `tl_proto::InputEvent`s into kernel `struct input_event`s — an absolute
//! pointer (`ABS_X`/`ABS_Y` over `0..=COORD_MAX`), the five mouse buttons,
//! `REL_WHEEL`/`REL_HWHEEL` scroll, and a full keyboard translated through
//! the USB HID usage ⇄ `KEY_*` table (`keys`).
//!
//! Unlike the macOS injector there is no desktop-coordinate `Mapping`: the
//! virtual pointer is absolute over its advertised axis range, which the
//! compositor scales onto the (virtual) screen.
//!
//! The device plumbing lives in `uinput` — hand-defined kernel ABI, since
//! `libc` does not ship `<linux/uinput.h>`. All event writes flow through
//! the [`inject::EventSink`] indirection so the mapping logic is testable
//! headlessly without a real `/dev/uinput` (SPEC §9).
//!
//! The whole crate is Linux-only: on any other OS it compiles to nothing.

#![cfg(target_os = "linux")]

mod keys;
pub mod inject;
mod uinput;
