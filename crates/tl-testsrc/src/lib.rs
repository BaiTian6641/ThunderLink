//! Portable synthetic test-pattern source (SPEC §10): moving block grid +
//! frame-counter digits + white border, BGRA bytes, no platform deps, no
//! permissions. Shared by the macOS (CVPixelBuffer wrapper) and Linux
//! (plain buffer) capture crates.
//!
//! The frame-invariant part (gradient, dim blocks, border) is rendered
//! once and cached; each frame is one row-wise copy plus the moving
//! highlight and counter digits — cheap enough to sustain 60 fps at 5K
//! on one core.

use anyhow::{bail, Result};

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

const WHITE: u32 = 0xFFFF_FFFF; // 0xAARRGGBB (32BGRA in LE memory)
const DIM: u32 = 0xFF40_4048;
const BRIGHT: u32 = 0xFFFF_F080;

/// Animated test pattern generator. One instance per stream; not `Copy`.
pub struct Pattern {
    width: usize,
    height: usize,
    fps: u32,
    frame: u64,
    /// Tightly packed width-major frame-invariant background.
    background: Vec<u32>,
}

impl Pattern {
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

    pub fn width(&self) -> u32 {
        self.width as u32
    }

    pub fn height(&self) -> u32 {
        self.height as u32
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Frame counter of the frame [`Pattern::draw_into`] produced last.
    pub fn frame_index(&self) -> u64 {
        self.frame
    }

    /// Zero-based presentation timestamp (µs) for the next frame at the
    /// configured cadence. Source-local domain; the engine stamps
    /// wall-clock pts over the wire units anyway.
    pub fn pts_us(&self) -> i64 {
        self.frame as i64 * 1_000_000 / self.fps as i64
    }

    /// Draw the next frame into `bgra`, which must hold `width*height`
    /// little-endian 32BGRA pixels (stride == width). Returns the frame
    /// index drawn.
    pub fn draw_into(&mut self, bgra: &mut [u8]) -> Result<u64> {
        if bgra.len() != self.width * self.height * 4 {
            bail!(
                "buffer is {} bytes; need {} ({}x{}x4, tightly packed)",
                bgra.len(),
                self.width * self.height * 4,
                self.width,
                self.height
            );
        }
        let frame = self.frame;
        // SAFETY-free path: view the buffer as u32 rows.
        let px: &mut [u32] = bytemuck_like_cast(bgra);
        for y in 0..self.height {
            let dst = &mut px[y * self.width..(y + 1) * self.width];
            dst.copy_from_slice(&self.background[y * self.width..(y + 1) * self.width]);
        }
        overlay(px, self.width, self.height, frame);
        self.frame += 1;
        Ok(frame)
    }
}

impl Pattern {
    /// Draw ONE row of the current frame into `row` (little-endian
    /// 32BGRA, at least `width*4` bytes) — for stride-padded
    /// destinations. Call for y in 0..height, then [`Pattern::advance`].
    pub fn draw_row(&self, y: usize, row: &mut [u8]) -> Result<()> {
        let w = self.width;
        if y >= self.height || row.len() < w * 4 {
            bail!("draw_row: y={y} out of range or row too short ({} < {})", row.len(), w * 4);
        }
        let px: &mut [u32] = bytemuck_like_cast(&mut row[..w * 4]);
        px.copy_from_slice(&self.background[y * w..(y + 1) * w]);
        overlay_row(px, y, w, self.height, self.frame);
        Ok(())
    }

    /// Finish the frame after `draw_row` over all rows; returns its
    /// zero-based pts (µs) and advances the animation.
    pub fn advance(&mut self) -> i64 {
        let pts = self.pts_us();
        self.frame += 1;
        pts
    }
}

/// Row-slice of the per-frame overlay (see `overlay`).
fn overlay_row(row: &mut [u32], y: usize, width: usize, height: usize, frame: u64) {
    let bs = (width.min(height) / 12).max(16);
    let cols = (width / bs).max(1);
    let rows = (height / bs).max(1);
    let highlight = (frame as usize) % (cols * rows);
    let (bx, by) = (highlight % cols, highlight / cols);
    let x0 = bx * bs + 2;
    let y0 = by * bs + 2;
    if y >= y0 && y < (y0 + bs - 4).min(height) {
        let hi = (x0 + bs - 4).min(width);
        row[x0..hi].fill(BRIGHT);
    }

    let shown = frame % 1_000_000;
    let scale = (height / 60).max(2);
    let digit_w = 4 * scale;
    let total_w = 6 * digit_w;
    let x_start = width.saturating_sub(total_w) / 2;
    let y_start = height.saturating_sub(5 * scale) / 2;
    let rel = y.saturating_sub(y_start);
    if y >= y_start && rel < 5 * scale {
        for (i, d) in (0..6)
            .map(|p| (shown / 10u64.pow(5 - p) % 10) as usize)
            .enumerate()
        {
            let glyph = &FONT[d];
            let grow = rel / scale; // glyph row 0..5
            let bits = glyph[grow];
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    let x = x_start + i * digit_w + col * scale;
                    if x + scale <= width {
                        row[x..x + scale].fill(WHITE);
                    }
                }
            }
        }
    }
}
/// Reinterpret a mutable byte slice as u32 pixels (little-endian hosts;
/// every platform we ship on — aarch64/x86_64 — is LE).
fn bytemuck_like_cast(bgra: &mut [u8]) -> &mut [u32] {
    debug_assert_eq!(bgra.len() % 4, 0);
    // SAFETY: u32 alignment 4 == slice split into 4-byte chunks; every
    // element is a valid u32 bit pattern (no validity invariant beyond bits).
    unsafe { std::slice::from_raw_parts_mut(bgra.as_mut_ptr().cast::<u32>(), bgra.len() / 4) }
}

/// Per-frame overlay: moving bright block + counter digits.
fn overlay(px: &mut [u32], width: usize, height: usize, frame: u64) {
    let bs = (width.min(height) / 12).max(16);
    let cols = (width / bs).max(1);
    let rows = (height / bs).max(1);
    let highlight = (frame as usize) % (cols * rows);
    let (bx, by) = (highlight % cols, highlight / cols);
    let x0 = bx * bs + 2;
    let y0 = by * bs + 2;
    for y in y0..(y0 + bs - 4).min(height) {
        for x in x0..(x0 + bs - 4).min(width) {
            px[y * width + x] = BRIGHT;
        }
    }

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
                                px[y * width + x] = WHITE;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Frame-invariant background: dark diagonal gradient, dim slate block
/// grid, white 1px border. Tightly packed, width-major.
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
                px[y * width + x0..y * width + x_hi].fill(DIM);
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

    fn draw_n(width: u32, height: u32, n: u64) -> (Vec<u8>, Pattern) {
        let mut p = Pattern::new(width, height, 60);
        let mut buf = vec![0u8; width as usize * height as usize * 4];
        for _ in 0..n {
            p.draw_into(&mut buf).unwrap();
        }
        (buf, p)
    }

    #[test]
    fn consecutive_frames_differ() {
        let (a, pa) = draw_n(320, 240, 2);
        let (b, pb) = draw_n(320, 240, 3);
        assert_ne!(a, b, "animation must change the frame");
        assert_eq!(pa.pts_us(), 2_000_000 / 60); // 2 frames @60fps = 33_333µs
        assert_eq!(pb.frame_index(), 3);
    }

    #[test]
    fn border_is_white() {
        let (buf, _) = draw_n(320, 240, 1);
        let px: &[u32] =
            unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u32>(), buf.len() / 4) };
        assert_eq!(px[0], 0xFFFF_FFFF);
        assert_eq!(px[319], 0xFFFF_FFFF);
        assert_eq!(px[239 * 320], 0xFFFF_FFFF);
    }

    #[test]
    fn bad_buffer_rejected() {
        let mut p = Pattern::new(64, 48, 60);
        let mut short = vec![0u8; 100];
        assert!(p.draw_into(&mut short).is_err());
    }

    #[test]
    fn background_cached_identical_except_overlay() {
        // Frame N and N+1 differ ONLY in the highlight/digit cells; the
        // rest matches — proves the background template is stable.
        let (a, _) = draw_n(320, 240, 5);
        let (b, _) = draw_n(320, 240, 6);
        let pa: &[u32] = unsafe { std::slice::from_raw_parts(a.as_ptr().cast::<u32>(), a.len() / 4) };
        let pb: &[u32] = unsafe { std::slice::from_raw_parts(b.as_ptr().cast::<u32>(), b.len() / 4) };
        let differing: Vec<usize> =
            (0..pa.len()).filter(|&i| pa[i] != pb[i]).collect();
        assert!(!differing.is_empty(), "animation present");
        assert!(
            differing.len() < pa.len() / 50,
            "only a small overlay region changes, {} pixels differed",
            differing.len()
        );
    }
#[test]
    fn row_wise_equals_full_frame() {
        let mut a = Pattern::new(320, 240, 60);
        let mut full = vec![0u8; 320 * 240 * 4];
        a.draw_into(&mut full).unwrap();
        let mut b = Pattern::new(320, 240, 60);
        let mut rows = vec![0u8; 320 * 240 * 4];
        for y in 0..240 {
            b.draw_row(y, &mut rows[y * 320 * 4..]).unwrap();
        }
        // draw_into advanced `a` internally; the row API advances on
        // `advance()`. Content equality is the contract.
        b.advance();
        assert_eq!(a.frame_index(), b.frame_index());
        assert_eq!(full, rows, "row API and full-frame API must agree");
    }
}
