//! Raw `/dev/uinput` plumbing: hand-defined kernel ABI structs and ioctl
//! request numbers (`libc` does not ship `<linux/uinput.h>`), plus the
//! [`UinputDevice`] wrapper that creates one virtual HID device and writes
//! `struct input_event`s into it.

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::unix::io::AsRawFd;

use anyhow::{anyhow, Result};
use tl_proto::COORD_MAX;

use crate::keys;

// ---- Event types and codes (`<linux/input-event-codes.h>`) ----

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_REL: u16 = 0x02;
pub(crate) const EV_ABS: u16 = 0x03;

pub(crate) const SYN_REPORT: u16 = 0x00;

pub(crate) const ABS_X: u16 = 0x00;
pub(crate) const ABS_Y: u16 = 0x01;

pub(crate) const REL_HWHEEL: u16 = 0x06;
pub(crate) const REL_WHEEL: u16 = 0x08;

pub(crate) const BTN_LEFT: u16 = 0x110;
pub(crate) const BTN_RIGHT: u16 = 0x111;
pub(crate) const BTN_MIDDLE: u16 = 0x112;
pub(crate) const BTN_FORWARD: u16 = 0x115;
pub(crate) const BTN_BACK: u16 = 0x116;

const BUS_USB: u16 = 0x03;

// ---- Kernel ABI structs (`<linux/uinput.h>`, `<linux/input.h>`) ----

/// Kernel `struct input_event`. The timestamp is ignored on the uinput
/// write path (the kernel stamps its own), so it stays zeroed.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawEvent {
    /// `timeval.tv_sec`.
    tv_sec: libc::time_t,
    /// `timeval.tv_usec`.
    tv_usec: libc::suseconds_t,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl RawEvent {
    pub(crate) fn new(kind: u16, code: u16, value: i32) -> Self {
        Self {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        }
    }
}

/// `struct input_id`.
#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `struct uinput_setup` (kernel ≥ 4.4; `UI_DEV_SETUP`).
#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

/// `struct uinput_abs_setup` (kernel ≥ 4.4; `UI_ABS_SETUP`). The unnamed
/// `__u16 filler` after `code` is explicit here as `_pad`.
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

// ---- ioctl request numbers (`<linux/uinput.h>`) ----
//
// Encoded with the asm-generic `_IOC` scheme used by x86-64/aarch64/
// riscv64/arm (dir bits: NONE=0, WRITE=1, READ=2).

const UINPUT_IOCTL_BASE: u32 = b'U' as u32;

const fn io(ty: u32, nr: u32) -> libc::c_ulong {
    ((ty << 8) | nr) as libc::c_ulong
}

const fn iow(ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((1u32 << 30) | (size << 16) | (ty << 8) | nr) as libc::c_ulong
}

const UI_DEV_CREATE: libc::c_ulong = io(UINPUT_IOCTL_BASE, 1);
const UI_DEV_DESTROY: libc::c_ulong = io(UINPUT_IOCTL_BASE, 2);
const UI_DEV_SETUP: libc::c_ulong = iow(UINPUT_IOCTL_BASE, 3, size_of::<UinputSetup>() as u32);
const UI_ABS_SETUP: libc::c_ulong = iow(UINPUT_IOCTL_BASE, 4, size_of::<UinputAbsSetup>() as u32);
const UI_SET_EVBIT: libc::c_ulong = iow(
    UINPUT_IOCTL_BASE,
    100,
    size_of::<libc::c_int>() as u32,
);
const UI_SET_KEYBIT: libc::c_ulong = iow(
    UINPUT_IOCTL_BASE,
    101,
    size_of::<libc::c_int>() as u32,
);
const UI_SET_RELBIT: libc::c_ulong = iow(
    UINPUT_IOCTL_BASE,
    102,
    size_of::<libc::c_int>() as u32,
);

/// One virtual HID device created through `/dev/uinput`.
///
/// Drop unregisters the device; the kernel force-releases anything still
/// pressed at that point, and the `File` close follows.
pub(crate) struct UinputDevice {
    file: File,
}

impl UinputDevice {
    /// Open `/dev/uinput` and create the ThunderLink virtual device: an
    /// absolute pointer (`ABS_X`/`ABS_Y` over `0..=COORD_MAX`), the five
    /// mouse buttons, `REL_WHEEL`/`REL_HWHEEL`, and every key from the HID
    /// usage table.
    pub(crate) fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(open_err)?;
        let dev = Self { file };
        dev.setup_device()?;
        Ok(dev)
    }

    /// Program and register the device (modern `UI_DEV_SETUP` path,
    /// kernel ≥ 4.4; `UI_ABS_SETUP` enables the abs bits itself).
    fn setup_device(&self) -> Result<()> {
        for ty in [EV_KEY, EV_REL, EV_ABS] {
            self.ioctl(UI_SET_EVBIT, i32::from(ty) as libc::c_ulong)?;
        }
        for &(_, code) in keys::TABLE {
            self.ioctl(UI_SET_KEYBIT, i32::from(code) as libc::c_ulong)?;
        }
        for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_BACK, BTN_FORWARD] {
            self.ioctl(UI_SET_KEYBIT, i32::from(btn) as libc::c_ulong)?;
        }
        for rel in [REL_WHEEL, REL_HWHEEL] {
            self.ioctl(UI_SET_RELBIT, i32::from(rel) as libc::c_ulong)?;
        }
        for axis in [ABS_X, ABS_Y] {
            let abs = UinputAbsSetup {
                code: axis,
                _pad: 0,
                minimum: 0,
                maximum: i32::from(COORD_MAX),
                fuzz: 0,
                flat: 0,
                resolution: 0,
            };
            self.ioctl_ptr(UI_ABS_SETUP, &abs)?;
        }

        let mut name = [0u8; UINPUT_MAX_NAME_SIZE];
        let device_name = b"ThunderLink Virtual Input";
        name[..device_name.len()].copy_from_slice(device_name);
        let setup = UinputSetup {
            id: InputId {
                bustype: BUS_USB,
                vendor: 0x544C, // "TL"
                product: 0x0001,
                version: 1,
            },
            name,
            ff_effects_max: 0,
        };
        self.ioctl_ptr(UI_DEV_SETUP, &setup)?;
        self.ioctl(UI_DEV_CREATE, 0)
    }

    /// `ioctl` whose argument is an integer bit/code passed by value.
    fn ioctl(&self, req: libc::c_ulong, arg: libc::c_ulong) -> Result<()> {
        // SAFETY: `req` is a valid uinput request number for this owned,
        // write-open fd, and `arg` is an integer bit/code — no pointer is
        // involved, and the kernel does not retain anything.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req, arg) };
        check_ioctl(r, req)
    }

    /// `ioctl` whose argument is a pointer to a `repr(C)` struct laid out
    /// exactly like the kernel's own definition.
    fn ioctl_ptr<T>(&self, req: libc::c_ulong, arg: &T) -> Result<()> {
        // SAFETY: `req` is a valid uinput request number for this owned,
        // write-open fd; `arg` points to a `repr(C)` struct matching the
        // kernel ABI that is alive for the whole synchronous call. The
        // kernel copies it out and never retains the pointer.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), req, arg as *const T) };
        check_ioctl(r, req)
    }

    /// Write one kernel `struct input_event` to the device.
    pub(crate) fn write_event(&mut self, ev: RawEvent) -> Result<()> {
        loop {
            // SAFETY: `ev` is a `repr(C)` struct matching the kernel's
            // `struct input_event` for this platform's word size; the fd is
            // owned and write-open; the kernel copies exactly
            // `size_of::<RawEvent>()` bytes synchronously and retains
            // nothing.
            let n = unsafe {
                libc::write(
                    self.file.as_raw_fd(),
                    &ev as *const RawEvent as *const libc::c_void,
                    size_of::<RawEvent>(),
                )
            };
            if n == size_of::<RawEvent>() as libc::ssize_t {
                return Ok(());
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue; // EINTR: nothing was written, retry
                }
                return Err(anyhow!("failed to write input event to /dev/uinput: {e}"));
            }
            return Err(anyhow!(
                "short write to /dev/uinput: {n} of {} bytes",
                size_of::<RawEvent>()
            ));
        }
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // Best-effort unregister; `File`'s close(2) follows. Ignoring the
        // return: there is nothing sane to do with a failure while
        // unwinding, and the fd close tears the device down regardless.
        // SAFETY: UI_DEV_DESTROY on our own, still-open fd; takes no
        // argument.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}

fn check_ioctl(r: libc::c_int, req: libc::c_ulong) -> Result<()> {
    if r < 0 {
        Err(anyhow!(
            "uinput ioctl {req:#x} failed: {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

/// Map `/dev/uinput` open failures to actionable messages; access-denied
/// paths must mention "permission" (SPEC §9).
fn open_err(e: io::Error) -> anyhow::Error {
    match e.kind() {
        io::ErrorKind::NotFound => anyhow!(
            "/dev/uinput not found — the uinput kernel module is not loaded \
             or this session/container has no permission grant to a uinput \
             device: {e}"
        ),
        io::ErrorKind::PermissionDenied => anyhow!(
            "permission denied opening /dev/uinput — grant this user access \
             (udev rule / uinput group membership) or adjust the sandbox \
             permission settings: {e}"
        ),
        _ => anyhow!("failed to open /dev/uinput: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_event_layout_matches_native_kernel_input_event() {
        // timeval (2 × long) + u16 + u16 + i32.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<RawEvent>(), 24);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(size_of::<RawEvent>(), 16);
    }

    #[test]
    fn setup_struct_layouts_match_kernel_abi() {
        // input_id (4 × u16) + name[80] + ff_effects_max.
        assert_eq!(size_of::<UinputSetup>(), 92);
        // u16 + filler + 5 × i32.
        assert_eq!(size_of::<UinputAbsSetup>(), 24);
        assert_eq!(size_of::<InputId>(), 8);
    }

    // Literal request numbers from <linux/uinput.h> for the asm-generic
    // ioctl encoding (x86-64, aarch64, riscv64, 32-bit arm, …) — the
    // architectures this crate targets. Other encodings (mips/ppc/…)
    // skip the check but still use the computed constants.
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "x86",
        target_arch = "arm",
    ))]
    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_DEV_SETUP, 0x405C5503);
        assert_eq!(UI_ABS_SETUP, 0x40185504);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_KEYBIT, 0x40045565);
        assert_eq!(UI_SET_RELBIT, 0x40045566);
    }

    #[test]
    fn open_failure_mentions_permission_without_dev_uinput() {
        // Headless environments (the validation container) have no
        // /dev/uinput; when one IS present and permitted there is no
        // error path to check.
        if let Err(e) = UinputDevice::open() {
            assert!(
                e.to_string().contains("permission"),
                "error must mention permission, got: {e:#}"
            );
        }
    }
}
