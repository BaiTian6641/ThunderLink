//! Synthetic animated test pattern (SPEC §10), macOS wrapper: draws via
//! the portable `tl-testsrc` crate into a 32BGRA CVPixelBuffer. Needs NO
//! TCC permission — used by unit tests and the smoke run.

use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};

use super::capture::CapturedFrame;

/// Synthetic animated frames (moving gradient/blocks + frame counter).
/// Needs NO TCC permission — used by unit tests and the smoke run.
pub struct TestPattern {
    pattern: tl_testsrc::Pattern,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { pattern: tl_testsrc::Pattern::new(width, height, fps) }
    }

    // Name pinned by the crate's public contract (not an Iterator impl).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<CapturedFrame> {
        let width = self.pattern.width() as usize;
        let height = self.pattern.height() as usize;
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; on success it holds a
        // newly created (+1) pixel buffer we take ownership of.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                self.pattern.width() as usize,
                self.pattern.height() as usize,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(anyhow!("CVPixelBufferCreate failed: {status}"));
        }
        // SAFETY: CVPixelBufferCreate follows the create rule; we own `raw`.
        let buf = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("checked above")) };

        // SAFETY: buffer valid; locking for write access (flags 0).
        let status = unsafe { CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags(0)) };
        if status != 0 {
            return Err(anyhow!("CVPixelBufferLockBaseAddress failed: {status}"));
        }
        // SAFETY: buffer is locked; base address/stride describe its memory.
        unsafe {
            let base = CVPixelBufferGetBaseAddress(&buf);
            let stride = CVPixelBufferGetBytesPerRow(&buf);
            if base.is_null() || stride < width * 4 {
                CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
                return Err(anyhow!("CVPixelBuffer has invalid layout"));
            }
            let plane =
                std::slice::from_raw_parts_mut(base as *mut u8, height * stride);
            for y in 0..height {
                self.pattern
                    .draw_row(y, &mut plane[y * stride..y * stride + width * 4])?;
            }
            CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
        }

        let pts_us = self.pattern.advance();
        Ok(CapturedFrame::from_parts(buf, pts_us))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::{
        CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow,
    };

    /// Read back a copy of the frame's pixels (32BGRA, tightly packed).
    fn pixels_of(frame: &CapturedFrame) -> Vec<u8> {
        let buf = frame.pixel_buffer();
        // SAFETY: valid buffer; read-only lock paired with matching unlock.
        unsafe {
            assert_eq!(
                CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly),
                0
            );
            let base = CVPixelBufferGetBaseAddress(buf) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(buf);
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), w * 4));
            }
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly),
                0
            );
            out
        }
    }

    #[test]
    fn produces_distinct_sequential_frames() {
        let mut src = TestPattern::new(640, 480, 60);
        let f0 = src.next().unwrap();
        let f1 = src.next().unwrap();
        let f2 = src.next().unwrap();

        assert_eq!((f0.width(), f0.height()), (640, 480));
        assert_eq!((f1.width(), f1.height()), (640, 480));
        let _ = (CVPixelBufferGetWidth, CVPixelBufferGetHeight);

        // pts follows the fps cadence.
        assert_eq!(f0.pts_us(), 0);
        assert_eq!(f1.pts_us(), 1_000_000 / 60);
        assert_eq!(f2.pts_us(), 2 * 1_000_000 / 60);

        // Animation: consecutive frames differ.
        let p0 = pixels_of(&f0);
        let p1 = pixels_of(&f1);
        assert_eq!(p0.len(), 640 * 480 * 4);
        assert_ne!(p0, p1, "consecutive frames must differ");

        // White 1px border: corners are opaque white.
        let stride = 640 * 4;
        for px in [&p0[0..4], &p0[stride - 4..stride]] {
            assert_eq!(px, &[0xFF, 0xFF, 0xFF, 0xFF], "border must be white");
        }
    }
}
