//! Synthetic animated test pattern (SPEC §10): moving bright block grid,
//! large frame-counter digits, white 1px border. CPU-drawn into a 32BGRA
//! CVPixelBuffer; needs NO TCC permission.

use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};

use super::capture::CapturedFrame;

/// 3x5 bitmap font for digits 0-9 (3 bits per row, MSB = left).
const FONT: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

const WHITE: u32 = 0xFFFF_FFFF; // 0xAARRGGBB (32BGRA in memory)

/// Synthetic animated frames (moving gradient/blocks + frame counter).
/// Needs NO TCC permission — used by unit tests and the smoke run.
///
/// The frame-invariant part (gradient, dim block grid, border) is rendered
/// once into `background`; each frame copies it and paints only the moving
/// highlight and counter digits. Per-frame cost is one row-wise memcpy plus
/// O(block) writes, which sustains 60 fps even at 5K.
pub struct TestPattern {
    width: usize,
    height: usize,
    fps: u32,
    frame: u64,
    /// Tightly packed (width-major) frame-invariant background.
    background: Vec<u32>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        Self {
            width,
            height,
            fps: fps.max(1),
            frame: 0,
            background: build_background(width, height),
        }
    }

    // Name pinned by the crate's public contract (not an Iterator impl).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<CapturedFrame> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; on success it holds a
        // newly created (+1) pixel buffer we take ownership of.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                self.width,
                self.height,
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
            if base.is_null() || stride < self.width * 4 {
                CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
                return Err(anyhow!("CVPixelBuffer has invalid layout"));
            }
            draw_frame(
                base as *mut u32,
                self.width,
                self.height,
                stride / 4,
                &self.background,
                self.frame,
            );
            CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
        }

        let pts_us = self.frame as i64 * 1_000_000 / self.fps as i64;
        self.frame += 1;
        Ok(CapturedFrame::from_parts(buf, pts_us))
    }
}

/// SAFETY: `pixels` must point at `height` rows of `stride` u32 pixels with
/// `stride >= width`.
unsafe fn draw_frame(
    pixels: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    background: &[u32],
    frame: u64,
) {
    debug_assert_eq!(background.len(), width * height);
    // Frame-invariant background: one row-wise copy.
    for y in 0..height {
        // SAFETY: row `y` holds `width` valid u32 slots (stride >= width).
        unsafe {
            let dst = std::slice::from_raw_parts_mut(pixels.add(y * stride), width);
            dst.copy_from_slice(&background[y * width..(y + 1) * width]);
        }
    }

    // Moving bright block grid; one highlighted block advances each frame.
    let bs = (width.min(height) / 12).max(16);
    let cols = (width / bs).max(1);
    let rows = (height / bs).max(1);
    let highlight = (frame as usize) % (cols * rows);
    let (bx, by) = (highlight % cols, highlight / cols);
    let x0 = bx * bs + 2;
    let y0 = by * bs + 2;
    for y in y0..(y0 + bs - 4).min(height) {
        for x in x0..(x0 + bs - 4).min(width) {
            // SAFETY: coordinates clamped to the frame rectangle.
            unsafe { *pixels.add(y * stride + x) = 0xFFFF_F080; }
        }
    }

    // Large frame-counter digits (6 decimal digits, centered).
    let shown = frame % 1_000_000;
    let scale = (height / 60).max(2);
    let digit_w = 4 * scale; // 3 glyph columns + 1 spacing
    let total_w = 6 * digit_w;
    let x_start = width.saturating_sub(total_w) / 2;
    let y_start = height.saturating_sub(5 * scale) / 2;
    for (i, d) in (0..6)
        .map(|p| (shown / 10u64.pow(5 - p) % 10) as usize)
        .enumerate()
    {
        let glyph = &FONT[d];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = x_start + i * digit_w + col * scale + dx;
                            let y = y_start + row * scale + dy;
                            if x < width && y < height {
                                // SAFETY: coordinates clamped to the frame.
                                unsafe {
                                    *pixels.add(y * stride + x) = WHITE;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Render the frame-invariant background: dark diagonal gradient, dim slate
/// block grid, white 1px border. Tightly packed, width-major.
fn build_background(width: usize, height: usize) -> Vec<u32> {
    let mut px = vec![0u32; width * height];

    // Gradient: red varies per column, green per row.
    let r_col: Vec<u32> = (0..width).map(|x| (x * 96 / width.max(1)) as u32).collect();
    for y in 0..height {
        let g = (y * 96 / height.max(1)) as u32;
        let base = 0xFF00_0000 | (g << 8) | 0x30;
        let row = &mut px[y * width..(y + 1) * width];
        for (dst, &r) in row.iter_mut().zip(&r_col) {
            *dst = base | (r << 16);
        }
    }

    // Dim slate block grid with 2px gaps; the moving highlight is per-frame.
    let bs = (width.min(height) / 12).max(16);
    let cols = (width / bs).max(1);
    let rows = (height / bs).max(1);
    for by in 0..rows {
        for bx in 0..cols {
            let x0 = bx * bs + 2;
            let y0 = by * bs + 2;
            let x_hi = (x0 + bs - 4).min(width);
            let y_hi = (y0 + bs - 4).min(height);
            for y in y0..y_hi {
                px[y * width + x0..y * width + x_hi].fill(0xFF40_4048);
            }
        }
    }

    // White 1px border.
    for x in 0..width {
        px[x] = WHITE;
        px[(height - 1) * width + x] = WHITE;
    }
    for y in 0..height {
        px[y * width] = WHITE;
        px[y * width + width - 1] = WHITE;
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

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
