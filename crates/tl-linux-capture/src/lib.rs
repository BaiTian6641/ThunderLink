//! Linux screen capture (X11 root window) + x264 software encode for the
//! Linux initiator (docs/LINUX-PORT.md; SPEC §5/§9/§10). Mirrors the
//! macOS capture crate's contract with plain BGRA buffers instead of
//! CVPixelBuffer.
//!
//! The crate is Linux-only: every item is behind `cfg(target_os =
//! "linux")`, leaving an empty crate on other targets so the
//! cross-platform workspace stays green.

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod encode;
#[cfg(target_os = "linux")]
pub mod frame;
#[cfg(target_os = "linux")]
pub mod testsrc;

#[cfg(target_os = "linux")]
pub use capture::ScreenCapturer;
#[cfg(target_os = "linux")]
pub use encode::Encoder;
#[cfg(target_os = "linux")]
pub use frame::RawFrame;
#[cfg(target_os = "linux")]
pub use testsrc::TestPattern;
