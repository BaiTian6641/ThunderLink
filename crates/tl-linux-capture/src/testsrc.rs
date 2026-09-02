//! Synthetic animated test pattern (SPEC §10), Linux wrapper: draws via
//! the portable `tl-testsrc` crate into a plain 32BGRA buffer. Needs NO
//! permissions — used by unit tests and the smoke run.

use anyhow::Result;

use super::frame::RawFrame;

/// Synthetic animated frames (moving gradient/blocks + frame counter).
/// No display server or permissions required.
pub struct TestPattern {
    pattern: tl_testsrc::Pattern,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { pattern: tl_testsrc::Pattern::new(width, height, fps) }
    }

    pub fn width(&self) -> u32 {
        self.pattern.width()
    }

    pub fn height(&self) -> u32 {
        self.pattern.height()
    }

    pub fn fps(&self) -> u32 {
        self.pattern.fps()
    }

    // Name pinned by the crate's public contract (not an Iterator impl).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<RawFrame> {
        let (width, height) = (self.pattern.width(), self.pattern.height());
        // `pts_us()` is the pts of the frame `draw_into` is about to
        // produce; `draw_into` advances the animation itself (advance()
        // is only for the draw_row path — calling both would skip frames).
        let pts_us = self.pattern.pts_us();
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        self.pattern.draw_into(&mut bgra)?;
        Ok(RawFrame { width, height, pts_us, bgra })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(frame: &RawFrame, x: usize, y: usize) -> [u8; 4] {
        let i = (y * frame.width as usize + x) * 4;
        [frame.bgra[i], frame.bgra[i + 1], frame.bgra[i + 2], frame.bgra[i + 3]]
    }

    #[test]
    fn produces_distinct_sequential_frames_with_white_border() {
        let mut src = TestPattern::new(320, 180, 60);
        assert_eq!((src.width(), src.height(), src.fps()), (320, 180, 60));

        let f0 = src.next().unwrap();
        assert_eq!(f0.pts_us, 0);
        assert_eq!(f0.bgra.len(), 320 * 180 * 4);

        let f1 = src.next().unwrap();
        assert_eq!(f1.pts_us, 1_000_000 / 60);

        // The moving highlight/counter make consecutive frames differ.
        assert_ne!(f0.bgra, f1.bgra, "frames 0 and 1 must differ");

        // White 1px border (0xFFFFFFFF 32BGRA), corners included.
        for (x, y) in [(0, 0), (319, 0), (0, 179), (319, 179)] {
            assert_eq!(px(&f1, x, y), [255, 255, 255, 255], "border at {x},{y}");
        }
    }

    #[test]
    fn pts_follows_configured_cadence() {
        let mut src = TestPattern::new(64, 64, 30);
        let pts: Vec<i64> = (0..5).map(|_| src.next().unwrap().pts_us).collect();
        assert_eq!(pts, vec![0, 33_333, 66_666, 100_000, 133_333]);
    }
}
